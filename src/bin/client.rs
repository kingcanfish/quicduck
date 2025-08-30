use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;
use quiche::{Connection, ConnectionId};
use ring::rand::SecureRandom;
use std::io::{self, Write};
use tokio::io::{stdin, AsyncBufReadExt, BufReader};
use tokio::net::UdpSocket;

use log::{debug, error, info};

use quicduck::{config, create_simple_config};

#[derive(Parser)]
#[command(name = "quicduck-client")]
#[command(about = "A simple QUIC client supporting custom server address")]
pub struct Args {
    /// Server address to connect to (IP:PORT or hostname:PORT)
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    pub server: String,
}

/// 简单的 QUIC 客户端
pub struct SimpleQuicClient {
    socket: UdpSocket,
    conn: Connection,
    server_addr: SocketAddr,
    next_stream_id: u64, // 追踪下一个可用的流ID
    // 存储每个流的部分数据缓冲区
    stream_buffers: HashMap<u64, Vec<u8>>,
}

impl SimpleQuicClient {
    async fn show_prompt(&self) -> Result<()> {
        std::io::stdout()
            .write_all(b"->")
            .and_then(|_| std::io::stdout().flush())?;
        Ok(())
    }
}

impl SimpleQuicClient {
    /// 创建新的 QUIC 客户端
    pub async fn new(server_addr_str: &str) -> Result<Self> {
        let server_addr: SocketAddr = server_addr_str.parse()?;
        // 绑定本地 UDP 套接字
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let local_addr = socket.local_addr()?;
        info!("🔗 客户端本地地址: {local_addr}");

        // 生成连接 ID
        let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        ring::rand::SystemRandom::new().fill(&mut scid).unwrap();
        let scid = ConnectionId::from_ref(&scid);

        // 创建客户端配置
        let mut config = create_simple_config()?;
        config.verify_peer(false); // 关闭证书验证（仅用于测试）

        // 建立连接
        let conn = quiche::connect(None, &scid, local_addr, server_addr, &mut config)?;
        info!("📡 正在连接到服务器 {server_addr}");

        Ok(Self {
            socket,
            conn,
            server_addr,
            next_stream_id: 4, // 从流ID 4开始（客户端发起的双向流）
            stream_buffers: HashMap::new(),
        })
    }

    /// 完成握手过程
    pub async fn handshake(&mut self) -> Result<()> {
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        // 发送初始数据包
        let (write, send_info) = self.conn.send(&mut out)?;
        self.socket.send_to(&out[..write], send_info.to).await?;
        debug!("📤 发送初始握手包");

        // 等待握手完成
        let mut attempts = 0;
        while !self.conn.is_established() && attempts < 10 {
            attempts += 1;

            // 接收响应
            match tokio::time::timeout(Duration::from_secs(1), self.socket.recv_from(&mut buf))
                .await
            {
                Ok(Ok((len, from))) => {
                    if from != self.server_addr {
                        continue;
                    }

                    // 处理接收到的数据包
                    self.conn.recv(
                        &mut buf[..len],
                        quiche::RecvInfo {
                            to: self.socket.local_addr()?,
                            from,
                        },
                    )?;

                    // 发送待发送的数据包
                    self.send_pending_packets(&mut out).await?;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    // 超时，重试发送
                    self.send_pending_packets(&mut out).await?;
                }
            }
        }

        if !self.conn.is_established() {
            return Err(anyhow!("握手失败"));
        }

        info!("✅ 连接已建立!");
        Ok(())
    }

    /// 发送消息
    pub async fn send_message(&mut self, message: &str) -> Result<()> {
        if !self.conn.is_established() {
            return Err(anyhow!("连接未建立"));
        }

        // 使用新的流ID发送消息，每个消息使用独立的流
        let stream_id = self.next_stream_id;
        self.next_stream_id += 4; // 下一个客户端发起的双向流ID（间隔4）

        self.conn.stream_send(stream_id, message.as_bytes(), true)?;
        debug!(
            "📤 发送消息到流 {stream_id} ({} 字节，fin=true): \"{message}\"",
            message.len()
        );

        // 发送数据包
        let mut out = [0; config::MAX_DATAGRAM_SIZE];
        self.send_pending_packets(&mut out).await?;

        Ok(())
    }

    /// 运行客户端主循环，支持终端输入和实时接收消息
    pub async fn run_interactive(&mut self) -> Result<()> {
        info!("🎯 进入交互模式，输入消息后按回车发送，输入 'quit' 退出");

        self.show_prompt().await?;

        let mut stdin_reader = BufReader::new(stdin());
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            let mut line = String::new();

            tokio::select! {
                // 处理终端输入
                result = stdin_reader.read_line(&mut line) => {
                    match result {
                        Ok(_) => {
                            let message = line.trim();

                            if message == "quit" {
                                info!("👋 再见!");
                                break;
                            }

                            if !message.is_empty() {
                                if let Err(e) = self.send_message(message).await {
                                    error!("❌ 发送消息失败: {e}");
                                }
                            }
                            // 输入处理完后显示新的提示符
                            self.show_prompt().await?;
                        }
                        Err(e) => {
                             error!("❌ 读取输入失败: {e}");
                             self.show_prompt().await?;
                         }
                    }
                }

                // 处理网络接收
                result = self.socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, from)) => {
                            if from == self.server_addr {
                                // 处理数据包
                                if let Err(e) = self.conn.recv(&mut buf[..len], quiche::RecvInfo {
                                    to: self.socket.local_addr()?,
                                    from,
                                }) {
                                    error!("❌ 处理数据包失败: {e}");
                                    continue;
                                }

                                // 检查可读的流并立即打印
                                for stream_id in self.conn.readable() {
                                    if let Ok(response) = self.read_stream_data(stream_id) {
                                        if !response.is_empty() {
                                            // 清除当前行，显示消息，然后重新显示提示符
                                            std::io::stdout().write_all(b"\r").and_then(|_| std::io::stdout().flush())?;
                                            info!("📨 收到消息: {response}");
                                            self.show_prompt().await?;
                                        }
                                    }
                                }

                                // 发送待发送的数据包
                                let _ = self.send_pending_packets(&mut out).await;
                            }
                        }
                        Err(e) => {
                             error!("❌ 网络接收错误: {e}");
                         }
                    }
                }
            }
        }

        Ok(())
    }

    /// 从指定流读取数据
    fn read_stream_data(&mut self, stream_id: u64) -> Result<String> {
        // 获取或创建该流的缓冲区
        let stream_buffer = self.stream_buffers.entry(stream_id).or_default();

        loop {
            let mut stream_buf = vec![0; 1024];
            match self.conn.stream_recv(stream_id, &mut stream_buf) {
                Ok((len, fin)) => {
                    if len > 0 {
                        stream_buffer.extend_from_slice(&stream_buf[..len]);
                    }

                    if fin {
                        // 流结束，返回完整数据并清理缓冲区
                        let complete_data = stream_buffer.clone();
                        self.stream_buffers.remove(&stream_id);

                        if !complete_data.is_empty() {
                            return Ok(String::from_utf8_lossy(&complete_data).to_string());
                        } else {
                            return Ok(String::new());
                        }
                    }

                    if len == 0 {
                        // 没有更多数据但流未结束，保留缓冲区数据，不返回任何内容
                        debug!("⚠️ 流{stream_id}未结束，等待后续数据");
                        break;
                    }
                }
                Err(quiche::Error::Done) => {
                    // 当前没有更多数据可读，保留已读数据等待后续数据，不返回任何内容
                    debug!("⚠️ 流{stream_id}未结束，等待后续数据");
                    break;
                }
                Err(e) => return Err(anyhow!("读取流失败: {e}")),
            }
        }

        // 流未结束，返回空字符串等待后续数据
        Ok(String::new())
    }
    pub async fn receive_response(&mut self) -> Result<String> {
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        // 等待响应
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.socket.recv_from(&mut buf))
                .await
            {
                Ok(Ok((len, from))) => {
                    if from != self.server_addr {
                        continue;
                    }

                    // 处理数据包
                    self.conn.recv(
                        &mut buf[..len],
                        quiche::RecvInfo {
                            to: self.socket.local_addr()?,
                            from,
                        },
                    )?;

                    // 检查可读的流
                    for stream_id in self.conn.readable() {
                        // 完整读取流数据，不截断
                        let mut complete_response = Vec::new();
                        let mut total_len = 0;

                        loop {
                            let mut stream_buf = vec![0; 1024];
                            match self.conn.stream_recv(stream_id, &mut stream_buf) {
                                Ok((len, fin)) => {
                                    if len > 0 {
                                        complete_response.extend_from_slice(&stream_buf[..len]);
                                        total_len += len;
                                        debug!("📥 读取了 {len} 字节，fin: {fin}, 总计: {total_len} 字节");
                                    }

                                    // 如果收到 fin 标志，说明数据传输完成
                                    if fin {
                                        let response =
                                            String::from_utf8_lossy(&complete_response).to_string();
                                        info!("📨 收到完整响应 ({total_len} 字节): \"{response}\"");
                                        return Ok(response);
                                    }

                                    // 如果没有数据且没有 fin，继续等待
                                    if len == 0 {
                                        break;
                                    }
                                }
                                Err(quiche::Error::Done) => break,
                                Err(e) => return Err(anyhow!("读取流失败: {e}")),
                            }
                        }

                        // 如果读取到了数据但没有fin标志，也返回当前数据
                        if !complete_response.is_empty() {
                            let response = String::from_utf8_lossy(&complete_response).to_string();
                            info!("📨 收到部分响应 ({total_len} 字节): \"{response}\"");
                            return Ok(response);
                        }
                    }

                    // 发送待发送的数据包
                    self.send_pending_packets(&mut out).await?;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(anyhow!("接收响应超时")),
            }
        }
    }

    /// 发送待发送的数据包
    async fn send_pending_packets(&mut self, out: &mut [u8]) -> Result<()> {
        loop {
            let (write, send_info) = match self.conn.send(out) {
                Ok(v) => v,
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(anyhow!("发送失败: {e}")),
            };

            self.socket.send_to(&out[..write], send_info.to).await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统，使用环境变量RUST_LOG控制日志级别
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    // 解析命令行参数
    let args = Args::parse();

    info!("🦆 QUIC Duck 客户端启动中...");
    info!("🏠 连接到服务器: {}", args.server);

    let mut client = SimpleQuicClient::new(&args.server).await?;

    // 完成握手
    client.handshake().await?;

    // 启动交互模式
    client.run_interactive().await?;
    Ok(())
}

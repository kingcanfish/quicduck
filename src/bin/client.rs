use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, warn};
use quiche::{Connection, ConnectionId};
use ring::rand::SecureRandom;
use std::io::Write;
use tokio::io::{stdin, AsyncBufReadExt, BufReader};
use tokio::net::UdpSocket;

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
    server_addr_str: String, // 保存服务器地址字符串用于重连
    next_stream_id: u64,     // 追踪下一个可用的流ID
    // 存储每个流的部分数据缓冲区
    stream_buffers: HashMap<u64, Vec<u8>>,
    last_activity: std::time::Instant, // 最后活动时间
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
            server_addr_str: server_addr_str.to_string(),
            next_stream_id: 4, // 从流ID 4开始（客户端发起的双向流）
            stream_buffers: HashMap::new(),
            last_activity: std::time::Instant::now(),
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

    /// 检查连接是否仍然有效
    fn is_connection_alive(&self) -> bool {
        self.conn.is_established() && !self.conn.is_closed()
    }

    /// 重新连接到服务器
    pub async fn reconnect(&mut self) -> Result<()> {
        info!("🔄 尝试重新连接到服务器 {}", self.server_addr);

        // 重新解析服务器地址（防止DNS变化）
        self.server_addr = self.server_addr_str.parse()?;

        // 重新绑定socket（可选，如果当前socket有问题）
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let local_addr = socket.local_addr()?;
        self.socket = socket;

        // 生成新的连接ID
        let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        ring::rand::SystemRandom::new().fill(&mut scid).unwrap();
        let scid = ConnectionId::from_ref(&scid);

        // 创建新的连接配置
        let mut config = create_simple_config()?;
        config.verify_peer(false);

        // 建立新连接
        self.conn = quiche::connect(None, &scid, local_addr, self.server_addr, &mut config)?;

        // 重置状态
        self.next_stream_id = 4;
        self.stream_buffers.clear();
        self.last_activity = std::time::Instant::now();

        // 完成握手
        self.handshake().await?;

        info!("✅ 重连成功!");
        Ok(())
    }

    /// 更新最后活动时间
    fn update_activity(&mut self) {
        self.last_activity = std::time::Instant::now();
    }

    /// 检查是否需要发送PING帧保活
    fn should_send_ping(&self) -> bool {
        // 在空闲超时时间的1/3后开始发送PING帧 (即100秒后)
        self.last_activity.elapsed() > Duration::from_secs(100)
    }

    /// 发送QUIC标准的PING帧
    async fn send_ping(&mut self) -> Result<()> {
        if !self.is_connection_alive() {
            return Err(anyhow!("连接已关闭"));
        }

        // 使用QUIC标准的ACK-eliciting包（包含PING帧）
        match self.conn.send_ack_eliciting() {
            Ok(_) => {
                debug!("💓 发送QUIC PING帧");
                let mut out = [0; config::MAX_DATAGRAM_SIZE];
                self.send_pending_packets(&mut out).await?;
                self.update_activity();
                Ok(())
            }
            Err(e) => Err(anyhow!("PING发送失败: {e}")),
        }
    }

    /// 发送消息
    pub async fn send_message(&mut self, message: &str) -> Result<()> {
        // 检查连接状态，如果连接已关闭则尝试重连
        if !self.is_connection_alive() {
            warn!("⚠️ 检测到连接已关闭，尝试重连...");
            if let Err(e) = self.reconnect().await {
                return Err(anyhow!("重连失败: {e}"));
            }
        }

        if !self.conn.is_established() {
            return Err(anyhow!("连接未建立"));
        }

        // 使用新的流ID发送消息，每个消息使用独立的流
        let stream_id = self.next_stream_id;
        self.next_stream_id += 4; // 下一个客户端发起的双向流ID（间隔4）

        let message_bytes = message.as_bytes();
        let chunk_size = 8192; // 8KB块大小
        let mut sent = 0;

        debug!(
            "📤 开始发送消息到流 {stream_id} (总计 {} 字节): \"{message}\"",
            message_bytes.len()
        );

        while sent < message_bytes.len() {
            let remaining = message_bytes.len() - sent;
            let chunk_len = std::cmp::min(chunk_size, remaining);
            let chunk = &message_bytes[sent..sent + chunk_len];
            let is_last = sent + chunk_len >= message_bytes.len();

            // 循环发送直到成功或出错
            loop {
                match self.conn.stream_send(stream_id, chunk, is_last) {
                    Ok(written) => {
                        if written == chunk.len() {
                            debug!(
                                "📤 成功发送块 {}/{} 字节到流 {stream_id} (fin={})",
                                sent + written,
                                message_bytes.len(),
                                is_last
                            );
                            break; // 成功发送完整块
                        } else {
                            // 部分发送，等待流控制窗口
                            debug!(
                                "⚠️ 部分发送 {}/{} 字节，等待流控制窗口",
                                written,
                                chunk.len()
                            );
                            let mut out = [0; config::MAX_DATAGRAM_SIZE];
                            self.send_pending_packets(&mut out).await?;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                    Err(quiche::Error::Done) => {
                        // 流控制限制，等待并重试
                        debug!("⚠️ 流控制限制，等待重试");
                        let mut out = [0; config::MAX_DATAGRAM_SIZE];
                        self.send_pending_packets(&mut out).await?;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => return Err(anyhow!("发送失败: {e}")),
                }
            }

            sent += chunk_len;

            // 发送数据包
            let mut out = [0; config::MAX_DATAGRAM_SIZE];
            self.send_pending_packets(&mut out).await?;
        }

        debug!(
            "✅ 完整消息发送完成到流 {stream_id} ({} 字节)",
            message_bytes.len()
        );
        self.update_activity(); // 更新最后活动时间
        Ok(())
    }

    /// 运行客户端主循环，支持终端输入和实时接收消息
    pub async fn run_interactive(&mut self) -> Result<()> {
        info!("🎯 进入交互模式，输入消息后按回车发送，输入 'quit' 退出");

        self.show_prompt().await?;

        let mut stdin_reader = BufReader::new(stdin());
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        // QUIC内部定时器，用于处理超时和保活
        let mut quic_timer = tokio::time::interval(Duration::from_millis(100));

        loop {
            let mut line = String::new();

            tokio::select! {
                // QUIC内部定时器 - 处理超时、重传、保活等
                _ = quic_timer.tick() => {
                    // 调用QUIC的超时处理
                    self.conn.on_timeout();

                    // 检查连接状态
                    if !self.is_connection_alive() {
                        if self.conn.is_draining() {
                            warn!("⚠️ 连接正在关闭，尝试重连...");
                        } else if self.conn.is_closed() {
                            warn!("⚠️ 连接已关闭，尝试重连...");
                        }
                        if let Err(e) = self.reconnect().await {
                            error!("❌ 重连失败: {e}");
                            continue;
                        }
                    } else if self.should_send_ping() {
                        // 发送QUIC标准的PING帧保活
                        if let Err(e) = self.send_ping().await {
                            debug!("💔 PING发送失败: {e}");
                        }
                    }

                    // 发送待发送的数据包（包括PING、ACK等）
                    let _ = self.send_pending_packets(&mut out).await;
                }

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

                                // 更新活动时间（收到服务端数据）
                                self.update_activity();

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
    // 初始化日志系统，使用环境变量RUST_LOG控制日志级别，默认Info
    env_logger::init_from_env(Env::default().default_filter_or("info"));

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

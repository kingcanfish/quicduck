use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, warn};
use quiche::{Connection, ConnectionId};
use ring::rand::SecureRandom;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::interval;

use quicduck::{config, create_simple_config, generate_cert_and_key_for_domain};

#[derive(Parser)]
#[command(name = "quicduck-server")]
#[command(about = "A simple QUIC echo server supporting custom address and certificate domain")]
pub struct Args {
    /// Listen address (IP:PORT)
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    pub addr: String,

    /// Certificate domain name
    #[arg(short = 'd', long, default_value = "localhost")]
    pub domain: String,
}

/// UDP数据包结构
#[derive(Debug)]
struct UdpPacket {
    data: Vec<u8>,
    from: SocketAddr,
}

/// 连接处理器
struct ConnectionHandler {
    conn: Connection,
    client_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    packet_rx: mpsc::Receiver<UdpPacket>,
    stream_buffers: HashMap<u64, Vec<u8>>, // 流ID -> 缓冲区数据
    migration_count: u32,                  // 连接迁移次数统计
}

impl ConnectionHandler {
    pub async fn run(mut self) -> Result<()> {
        let mut timer_interval = interval(Duration::from_millis(25)); // QUIC内部定时器

        loop {
            tokio::select! {
                // 处理收到的UDP数据包
                packet = self.packet_rx.recv() => {
                    match packet {
                        Some(packet) => {
                             if let Err(e) = self.handle_packet(packet).await {
                                 error!("❌ 处理数据包失败: {e}");
                             }
                         },
                        None => {
                             debug!("🚪 连接通道关闭，退出连接处理器");
                             break;
                         }
                    }
                }

                // QUIC内部定时器
                _ = timer_interval.tick() => {
                    self.conn.on_timeout();
                    if let Err(e) = self.send_pending_packets().await {
                        error!("❌ 发送定时器数据包失败: {e}");
                    }

                    // 检查连接是否关闭
                    if self.conn.is_closed() {
                        debug!("🚪 连接已关闭，退出处理器");
                        break;
                    }
                }

                // 连接超时检查
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    if !self.conn.is_established() {
                        debug!("⏰ 连接建立超时，关闭连接");
                        break;
                    }
                }
            }
        }

        debug!(
            "✅ 连接处理器退出: {} (总迁移次数: {})",
            self.client_addr, self.migration_count
        );
        Ok(())
    }

    async fn handle_packet(&mut self, packet: UdpPacket) -> Result<()> {
        // 检测连接迁移
        if self.client_addr != packet.from {
            debug!("🚀 检测到连接迁移: {} -> {}", self.client_addr, packet.from);

            // 在生产环境中，这里应该进行路径验证
            // 为了简化演示，我们直接接受迁移

            // 更新客户端地址
            self.client_addr = packet.from;
            self.migration_count += 1;

            debug!(
                "✅ 连接迁移完成，新地址: {} (迁移次数: {})",
                self.client_addr, self.migration_count
            );
        }

        // 接收数据包
        let mut packet_data = packet.data;
        self.conn.recv(
            &mut packet_data,
            quiche::RecvInfo {
                to: self.socket.local_addr()?,
                from: packet.from, // 使用数据包的实际源地址
            },
        )?;

        // 处理可读的流
        if self.conn.is_established() {
            self.process_readable_streams().await?;
        }

        // 发送待发送的数据包
        self.send_pending_packets().await?;

        Ok(())
    }

    async fn process_readable_streams(&mut self) -> Result<()> {
        for stream_id in self.conn.readable() {
            // 获取或创建该流的缓冲区
            let stream_buffer = self.stream_buffers.entry(stream_id).or_default();

            loop {
                let mut stream_buf = vec![0; 1024];
                match self.conn.stream_recv(stream_id, &mut stream_buf) {
                    Ok((len, fin)) => {
                        if len > 0 {
                            stream_buffer.extend_from_slice(&stream_buf[..len]);
                            debug!("📥 从流 {stream_id} 读取了 {len} 字节，fin: {fin}, 缓冲区总计: {} 字节", stream_buffer.len());
                        }

                        // 如果收到 fin 标志，处理完整消息
                        if fin {
                            if !stream_buffer.is_empty() {
                                let msg = String::from_utf8_lossy(stream_buffer);
                                info!("📨 收到完整消息 ({} 字节): \"{msg}\"", stream_buffer.len());

                                // 发送回应，设置 fin=true 表示响应发送完毕
                                let response = format!("Echo: {msg}");
                                self.conn
                                    .stream_send(stream_id, response.as_bytes(), true)?;
                                debug!(
                                    "📤 发送回应 ({} 字节，fin=true): \"{response}\"",
                                    response.len()
                                );

                                // 清理该流的缓冲区
                                self.stream_buffers.remove(&stream_id);
                            }
                            break;
                        }

                        if len == 0 {
                            // 没有更多数据但流未结束，保留缓冲区数据
                            // 当前没有更多数据可读，保留已读数据等待后续数据，不返回任何内容
                            debug!("⚠️ 流{stream_id}未结束，等待后续数据");
                            break;
                        }
                    }
                    Err(quiche::Error::Done) => {
                        // 当前没有更多数据可读，保留已读数据等待后续数据
                        // 当前没有更多数据可读，保留已读数据等待后续数据，不返回任何内容
                        debug!("⚠️ 流{stream_id}未结束，等待后续数据");
                        break;
                    }
                    Err(e) => {
                        error!("读取流失败: {e}");
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn send_pending_packets(&mut self) -> Result<()> {
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            let (write, send_info) = match self.conn.send(&mut out) {
                Ok(v) => v,
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(anyhow!("发送失败: {e}")),
            };

            self.socket.send_to(&out[..write], send_info.to).await?;
        }

        Ok(())
    }
}

/// 简单的 QUIC 服务器
pub struct SimpleQuicServer {
    socket: Arc<UdpSocket>,
    // 存储连接的发送通道：ConnectionID -> 数据包发送通道
    connection_senders: Arc<Mutex<HashMap<Vec<u8>, mpsc::Sender<UdpPacket>>>>,
}

impl SimpleQuicServer {
    /// 创建新的 QUIC 服务器
    pub async fn new(addr: &str) -> Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        info!("🦆 QUIC 服务器启动在: {addr}");

        Ok(Self {
            socket: Arc::new(socket),
            connection_senders: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 运行服务器
    pub async fn run(&self) -> Result<()> {
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            // 接收数据包
            let (len, from) = self.socket.recv_from(&mut buf).await?;
            let packet_data = buf[..len].to_vec();

            // 解析数据包头获取连接ID
            let hdr =
                match quiche::Header::from_slice(&mut packet_data.clone(), quiche::MAX_CONN_ID_LEN)
                {
                    Ok(hdr) => hdr,
                    Err(e) => {
                        error!("❌ 解析数据包头失败: {e}");
                        continue;
                    }
                };

            debug!(
                "📦 收到数据包类型: {:?}, scid: {:?}, dcid: {:?}, 来自: {from}",
                hdr.ty, hdr.scid, hdr.dcid
            );

            // 使用dcid作为连接标识符
            let conn_id = hdr.dcid.to_vec();

            let mut senders = self.connection_senders.lock().await;

            if let Some(sender) = senders.get(&conn_id) {
                // 现有连接，转发数据包
                let packet = UdpPacket {
                    data: packet_data,
                    from,
                };

                if sender.send(packet).await.is_err() {
                    debug!("🚪 连接处理器已关闭，清理连接: {:?}", hdr.dcid);
                    senders.remove(&conn_id);
                }
            } else {
                // 新连接，只处理Initial类型的数据包
                match hdr.ty {
                    quiche::Type::Initial => {
                        if let Err(e) = self
                            .handle_new_connection(hdr, packet_data, from, &mut senders)
                            .await
                        {
                            error!("❌ 创建新连接失败: {e}");
                        }
                    }
                    _ => {
                        warn!("⚠️ 忽略非 Initial 类型的新连接数据包: {:?}", hdr.ty);
                    }
                }
            }
        }
    }

    async fn handle_new_connection(
        &self,
        hdr: quiche::Header<'_>,
        packet_data: Vec<u8>,
        from: SocketAddr,
        senders: &mut HashMap<Vec<u8>, mpsc::Sender<UdpPacket>>,
    ) -> Result<()> {
        info!(
            "🆕 处理新的 Initial 连接, dcid: {:?}, 来自: {from}",
            hdr.dcid
        );

        // 创建服务器配置
        let mut config = create_simple_config()?;

        // 生成并保存临时证书
        ensure_test_cert_exists("localhost")?; // 默认使用localhost，因为这是一个运行时调用
        config.load_cert_chain_from_pem_file("cert.pem")?;
        config.load_priv_key_from_pem_file("key.pem")?;

        // 生成连接 ID
        let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        ring::rand::SystemRandom::new().fill(&mut scid).unwrap();
        let scid = ConnectionId::from_ref(&scid);

        // 创建新连接
        let conn = quiche::accept(&scid, None, self.socket.local_addr()?, from, &mut config)?;

        // 创建数据包通道
        let (packet_tx, packet_rx) = mpsc::channel::<UdpPacket>(100);

        // 创建连接处理器
        let handler = ConnectionHandler {
            conn,
            client_addr: from,
            socket: self.socket.clone(),
            packet_rx,
            stream_buffers: HashMap::new(),
            migration_count: 0, // 初始化迁移计数器
        };

        // 存储发送通道
        let conn_id = scid.clone().to_vec();
        senders.insert(conn_id, packet_tx.clone());

        // 发送初始数据包给处理器
        let initial_packet = UdpPacket {
            data: packet_data,
            from,
        };

        if packet_tx.send(initial_packet).await.is_err() {
            return Err(anyhow!("发送初始数据包失败"));
        }

        // 启动连接处理器协程
        let connection_senders = self.connection_senders.clone();
        let conn_id_for_cleanup = hdr.dcid.to_vec();
        let dcid_for_log = hdr.dcid.into_owned(); // 复制dcid用于日志

        tokio::spawn(async move {
            info!("🔗 新连接建立: dcid={dcid_for_log:?} <- {from}");

            if let Err(e) = handler.run().await {
                error!("❌ 连接处理器错误: {e}");
            }

            // 清理连接
            debug!("🧹 清理连接: dcid={dcid_for_log:?}");
            let mut senders = connection_senders.lock().await;
            senders.remove(&conn_id_for_cleanup);
        });

        Ok(())
    }
}

/// 确保测试证书文件存在
fn ensure_test_cert_exists(domain: &str) -> Result<()> {
    if std::path::Path::new("cert.pem").exists() && std::path::Path::new("key.pem").exists() {
        return Ok(());
    }
    generate_test_cert_for_domain(domain)
}

/// 生成测试用的自签名证书
fn generate_test_cert_for_domain(domain: &str) -> Result<()> {
    use std::fs::File;
    use std::io::Write;

    info!("🔐 正在生成测试证书针对域名: {domain}...");

    // 使用 rcgen 库生成证书和私钥
    let (cert_pem, key_pem) = generate_cert_and_key_for_domain(domain)?;

    // 写入证书文件
    let mut cert_file = File::create("cert.pem")?;
    cert_file.write_all(cert_pem.as_bytes())?;

    // 写入私钥文件
    let mut key_file = File::create("key.pem")?;
    key_file.write_all(key_pem.as_bytes())?;

    info!("✅ 已生成测试证书文件 cert.pem 和 key.pem");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统，使用环境变量RUST_LOG控制日志级别，默认Info
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // 解析命令行参数
    let args = Args::parse();

    info!("🦆 QUIC Duck 服务器启动中...");
    info!("🔗 监听地址: {}", args.addr);
    info!("🔐 证书域名: {}", args.domain);

    // 确保测试证书存在
    ensure_test_cert_exists(&args.domain)?;

    let server = SimpleQuicServer::new(&args.addr).await?;
    server.run().await
}

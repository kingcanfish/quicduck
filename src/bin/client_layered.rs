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
use tokio::sync::mpsc;

use quicduck::{config, create_simple_config};

#[derive(Parser)]
#[command(name = "quicduck-client-layered")]
#[command(about = "A layered QUIC client with app/quic/udp layers")]
pub struct Args {
    /// Server address to connect to (IP:PORT or hostname:PORT)
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    pub server: String,
}

// ==================== 层间消息定义 ====================

/// 应用层到QUIC层的消息
#[derive(Debug)]
pub enum AppToQuicMessage {
    /// 发送消息到服务器
    SendMessage { content: String },
    /// 关闭连接
    Shutdown,
}

/// QUIC层到应用层的消息
#[derive(Debug)]
pub enum QuicToAppMessage {
    /// 收到来自服务器的消息
    MessageReceived { content: String },
    /// 连接状态变化
    ConnectionStatus { connected: bool },
    /// 错误信息
    Error { message: String },
}

/// QUIC层到UDP层的消息
#[derive(Debug)]
pub struct QuicToUdpMessage {
    /// 要发送的数据包
    pub data: Vec<u8>,
    /// 目标地址
    pub to: SocketAddr,
}

/// UDP层到QUIC层的消息
#[derive(Debug)]
pub struct UdpToQuicMessage {
    /// 接收到的数据包
    pub data: Vec<u8>,
    /// 来源地址
    pub from: SocketAddr,
    /// 本地地址
    pub to: SocketAddr,
}

// ==================== 应用层 ====================

pub struct AppLayer {
    /// 向QUIC层发送消息的通道
    to_quic_tx: mpsc::UnboundedSender<AppToQuicMessage>,
    /// 从QUIC层接收消息的通道
    from_quic_rx: mpsc::UnboundedReceiver<QuicToAppMessage>,
}

impl AppLayer {
    pub fn new(
        to_quic_tx: mpsc::UnboundedSender<AppToQuicMessage>,
        from_quic_rx: mpsc::UnboundedReceiver<QuicToAppMessage>,
    ) -> Self {
        Self {
            to_quic_tx,
            from_quic_rx,
        }
    }

    /// 运行应用层主循环
    pub async fn run(&mut self) -> Result<()> {
        info!("🎯 应用层启动，进入交互模式");
        info!("🎯 输入消息后按回车发送，输入 'quit' 退出");

        self.show_prompt().await?;

        let mut stdin_reader = BufReader::new(stdin());

        loop {
            let mut line = String::new();

            tokio::select! {
                // 处理用户输入
                result = stdin_reader.read_line(&mut line) => {
                    match result {
                        Ok(_) => {
                            let message = line.trim();

                            if message == "quit" {
                                info!("👋 应用层准备关闭");
                                let _ = self.to_quic_tx.send(AppToQuicMessage::Shutdown);
                                break;
                            }

                            if !message.is_empty() {
                                // 发送消息到QUIC层
                                if let Err(e) = self.to_quic_tx.send(AppToQuicMessage::SendMessage {
                                    content: message.to_string(),
                                }) {
                                    error!("❌ 发送消息到QUIC层失败: {e}");
                                }
                            }
                            self.show_prompt().await?;
                        }
                        Err(e) => {
                            error!("❌ 读取输入失败: {e}");
                            self.show_prompt().await?;
                        }
                    }
                }

                // 处理来自QUIC层的消息
                msg = self.from_quic_rx.recv() => {
                    match msg {
                        Some(QuicToAppMessage::MessageReceived { content }) => {
                            // 清除当前行，显示消息，然后重新显示提示符
                            std::io::stdout().write_all(b"\r").and_then(|_| std::io::stdout().flush())?;
                            info!("📨 收到消息: {content}");
                            self.show_prompt().await?;
                        }
                        Some(QuicToAppMessage::ConnectionStatus { connected }) => {
                            if connected {
                                info!("✅ 连接已建立");
                            } else {
                                warn!("⚠️ 连接已断开");
                            }
                        }
                        Some(QuicToAppMessage::Error { message }) => {
                            error!("❌ QUIC层错误: {message}");
                        }
                        None => {
                            info!("📡 QUIC层已关闭，应用层退出");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn show_prompt(&self) -> Result<()> {
        print!("-> ");
        std::io::stdout().flush()?;
        Ok(())
    }
}

// ==================== QUIC层 ====================

pub struct QuicLayer {
    /// QUIC连接
    conn: Connection,
    /// 服务器地址
    server_addr: SocketAddr,
    /// 下一个流ID
    next_stream_id: u64,
    /// 流缓冲区
    stream_buffers: HashMap<u64, Vec<u8>>,
    /// 最后活动时间
    last_activity: std::time::Instant,

    /// 从应用层接收消息的通道
    from_app_rx: mpsc::UnboundedReceiver<AppToQuicMessage>,
    /// 向应用层发送消息的通道
    to_app_tx: mpsc::UnboundedSender<QuicToAppMessage>,
    /// 向UDP层发送消息的通道
    to_udp_tx: mpsc::UnboundedSender<QuicToUdpMessage>,
    /// 从UDP层接收消息的通道
    from_udp_rx: mpsc::UnboundedReceiver<UdpToQuicMessage>,
}

impl QuicLayer {
    pub fn new(
        conn: Connection,
        server_addr: SocketAddr,
        from_app_rx: mpsc::UnboundedReceiver<AppToQuicMessage>,
        to_app_tx: mpsc::UnboundedSender<QuicToAppMessage>,
        to_udp_tx: mpsc::UnboundedSender<QuicToUdpMessage>,
        from_udp_rx: mpsc::UnboundedReceiver<UdpToQuicMessage>,
    ) -> Self {
        Self {
            conn,
            server_addr,
            next_stream_id: 4,
            stream_buffers: HashMap::new(),
            last_activity: std::time::Instant::now(),
            from_app_rx,
            to_app_tx,
            to_udp_tx,
            from_udp_rx,
        }
    }

    /// 运行QUIC层主循环
    pub async fn run(&mut self) -> Result<()> {
        info!("🔗 QUIC层启动");

        let mut quic_timer = tokio::time::interval(Duration::from_millis(100));
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            tokio::select! {
                // QUIC内部定时器
                _ = quic_timer.tick() => {
                    self.conn.on_timeout();
                    self.send_pending_packets(&mut out).await?;
                    
                    // 检查是否需要发送保活包
                    if self.should_send_ping() {
                        if let Err(e) = self.send_ping(&mut out).await {
                            debug!("💔 PING发送失败: {e}");
                        }
                    }
                }

                // 处理来自应用层的消息
                msg = self.from_app_rx.recv() => {
                    match msg {
                        Some(AppToQuicMessage::SendMessage { content }) => {
                            if let Err(e) = self.send_message(&content, &mut out).await {
                                let _ = self.to_app_tx.send(QuicToAppMessage::Error {
                                    message: format!("发送消息失败: {e}"),
                                });
                            }
                        }
                        Some(AppToQuicMessage::Shutdown) => {
                            info!("🔗 QUIC层收到关闭信号");
                            break;
                        }
                        None => {
                            info!("📡 应用层已关闭，QUIC层退出");
                            break;
                        }
                    }
                }

                // 处理来自UDP层的消息
                msg = self.from_udp_rx.recv() => {
                    match msg {
                        Some(UdpToQuicMessage { data, from, to }) => {
                            if from == self.server_addr {
                                if let Err(e) = self.handle_incoming_packet(&data, from, to).await {
                                    debug!("❌ 处理数据包失败: {e}");
                                }
                                self.send_pending_packets(&mut out).await?;
                            }
                        }
                        None => {
                            info!("📡 UDP层已关闭，QUIC层退出");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// 处理接收到的数据包
    async fn handle_incoming_packet(&mut self, data: &[u8], from: SocketAddr, to: SocketAddr) -> Result<()> {
        // 处理QUIC数据包，使用正确的本地地址
        let mut data_copy = data.to_vec();
        self.conn.recv(&mut data_copy, quiche::RecvInfo { to, from })?;
        self.last_activity = std::time::Instant::now();

        // 检查可读的流
        for stream_id in self.conn.readable() {
            if let Ok(message) = self.read_stream_data(stream_id) {
                if !message.is_empty() {
                    let _ = self.to_app_tx.send(QuicToAppMessage::MessageReceived {
                        content: message,
                    });
                }
            }
        }

        Ok(())
    }

    /// 发送消息
    async fn send_message(&mut self, message: &str, out: &mut [u8]) -> Result<()> {
        if !self.conn.is_established() {
            return Err(anyhow!("连接未建立"));
        }

        let stream_id = self.next_stream_id;
        self.next_stream_id += 4;

        let message_bytes = message.as_bytes();
        debug!("📤 QUIC层发送消息到流 {stream_id}: \"{message}\"");

        // 发送数据到流
        match self.conn.stream_send(stream_id, message_bytes, true) {
            Ok(_) => {
                debug!("✅ 消息发送到流 {stream_id} 成功");
                self.send_pending_packets(out).await?;
                self.last_activity = std::time::Instant::now();
            }
            Err(e) => return Err(anyhow!("发送失败: {e}")),
        }

        Ok(())
    }

    /// 从流读取数据
    fn read_stream_data(&mut self, stream_id: u64) -> Result<String> {
        let mut stream_finished = false;

        // 确保流缓冲区存在
        if !self.stream_buffers.contains_key(&stream_id) {
            self.stream_buffers.insert(stream_id, Vec::new());
        }

        loop {
            let mut stream_buf = vec![0; 1024];
            match self.conn.stream_recv(stream_id, &mut stream_buf) {
                Ok((len, fin)) => {
                    if len > 0 {
                        // 添加到流缓冲区
                        if let Some(buffer) = self.stream_buffers.get_mut(&stream_id) {
                            buffer.extend_from_slice(&stream_buf[..len]);
                        }
                    }

                    if fin {
                        stream_finished = true;
                        break;
                    }

                    if len == 0 {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(anyhow!("读取流失败: {e}")),
            }
        }

        // 如果流结束，返回完整数据并清理缓冲区
        if stream_finished {
            if let Some(buffer) = self.stream_buffers.remove(&stream_id) {
                if !buffer.is_empty() {
                    return Ok(String::from_utf8_lossy(&buffer).to_string());
                }
            }
        }

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

            // 发送到UDP层
            let _ = self.to_udp_tx.send(QuicToUdpMessage {
                data: out[..write].to_vec(),
                to: send_info.to,
            });
        }
        Ok(())
    }

    /// 检查是否需要发送PING
    fn should_send_ping(&self) -> bool {
        self.last_activity.elapsed() > Duration::from_secs(30)
    }

    /// 发送PING包
    async fn send_ping(&mut self, out: &mut [u8]) -> Result<()> {
        match self.conn.send_ack_eliciting() {
            Ok(_) => {
                debug!("💓 QUIC层发送PING帧");
                self.send_pending_packets(out).await?;
                self.last_activity = std::time::Instant::now();
                Ok(())
            }
            Err(e) => Err(anyhow!("PING发送失败: {e}")),
        }
    }
}

// ==================== UDP层 ====================

pub struct UdpLayer {
    /// UDP套接字
    socket: UdpSocket,
    /// 向QUIC层发送消息的通道
    to_quic_tx: mpsc::UnboundedSender<UdpToQuicMessage>,
    /// 从QUIC层接收消息的通道
    from_quic_rx: mpsc::UnboundedReceiver<QuicToUdpMessage>,
}

impl UdpLayer {
    pub fn new(
        socket: UdpSocket,
        to_quic_tx: mpsc::UnboundedSender<UdpToQuicMessage>,
        from_quic_rx: mpsc::UnboundedReceiver<QuicToUdpMessage>,
    ) -> Self {
        Self {
            socket,
            to_quic_tx,
            from_quic_rx,
        }
    }

    /// 运行UDP层主循环
    pub async fn run(&mut self) -> Result<()> {
        info!("🌐 UDP层启动，监听地址: {}", self.socket.local_addr()?);

        let mut buf = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            tokio::select! {
                // 处理网络接收
                result = self.socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, from)) => {
                            debug!("🌐 UDP层收到 {} 字节数据包，来自 {}", len, from);
                            
                            // 发送到QUIC层，包含正确的本地地址
                            let local_addr = self.socket.local_addr().unwrap_or_else(|_| {
                                SocketAddr::from(([127, 0, 0, 1], 0))
                            });
                            let _ = self.to_quic_tx.send(UdpToQuicMessage {
                                data: buf[..len].to_vec(),
                                from,
                                to: local_addr,
                            });
                        }
                        Err(e) => {
                            error!("❌ UDP接收错误: {e}");
                        }
                    }
                }

                // 处理来自QUIC层的发送请求
                msg = self.from_quic_rx.recv() => {
                    match msg {
                        Some(QuicToUdpMessage { data, to }) => {
                            debug!("🌐 UDP层发送 {} 字节数据包到 {}", data.len(), to);
                            
                            if let Err(e) = self.socket.send_to(&data, to).await {
                                error!("❌ UDP发送失败: {e}");
                            }
                        }
                        None => {
                            info!("📡 QUIC层已关闭，UDP层退出");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ==================== 主函数和初始化 ====================

/// 创建QUIC连接并完成握手
async fn create_quic_connection(server_addr_str: &str, socket: &UdpSocket) -> Result<Connection> {
    let server_addr: SocketAddr = server_addr_str.parse()?;
    let local_addr = socket.local_addr()?;

    // 生成连接ID
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    ring::rand::SystemRandom::new().fill(&mut scid).unwrap();
    let scid = ConnectionId::from_ref(&scid);

    // 创建配置
    let mut config = create_simple_config()?;
    config.verify_peer(false);

    // 建立连接
    let conn = quiche::connect(None, &scid, local_addr, server_addr, &mut config)?;
    info!("📡 QUIC连接创建完成");

    Ok(conn)
}

/// 完成QUIC握手
async fn complete_handshake(
    conn: &mut Connection,
    socket: &UdpSocket,
    server_addr: SocketAddr,
) -> Result<()> {
    let mut buf = [0; config::MAX_DATAGRAM_SIZE];
    let mut out = [0; config::MAX_DATAGRAM_SIZE];

    // 发送初始包
    let (write, send_info) = conn.send(&mut out)?;
    socket.send_to(&out[..write], send_info.to).await?;
    debug!("📤 发送初始握手包");

    // 等待握手完成
    let mut attempts = 0;
    while !conn.is_established() && attempts < 10 {
        attempts += 1;

        match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, from))) => {
                if from != server_addr {
                    continue;
                }

                conn.recv(
                    &mut buf[..len],
                    quiche::RecvInfo {
                        to: socket.local_addr()?,
                        from,
                    },
                )?;

                // 发送待发送的包
                loop {
                    let (write, send_info) = match conn.send(&mut out) {
                        Ok(v) => v,
                        Err(quiche::Error::Done) => break,
                        Err(e) => return Err(anyhow!("发送失败: {e}")),
                    };
                    socket.send_to(&out[..write], send_info.to).await?;
                }
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                // 超时，重试
                loop {
                    let (write, send_info) = match conn.send(&mut out) {
                        Ok(v) => v,
                        Err(quiche::Error::Done) => break,
                        Err(e) => return Err(anyhow!("发送失败: {e}")),
                    };
                    socket.send_to(&out[..write], send_info.to).await?;
                }
            }
        }
    }

    if !conn.is_established() {
        return Err(anyhow!("握手失败"));
    }

    info!("✅ QUIC握手完成!");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // 解析参数
    let args = Args::parse();
    let server_addr: SocketAddr = args.server.parse()?;

    info!("🦆 分层QUIC客户端启动中...");
    info!("🏠 连接到服务器: {}", args.server);

    // 创建UDP套接字
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    info!("🔗 客户端本地地址: {}", socket.local_addr()?);

    // 创建QUIC连接并完成握手
    let mut conn = create_quic_connection(&args.server, &socket).await?;
    complete_handshake(&mut conn, &socket, server_addr).await?;

    // 创建通道
    let (app_to_quic_tx, app_to_quic_rx) = mpsc::unbounded_channel();
    let (quic_to_app_tx, quic_to_app_rx) = mpsc::unbounded_channel();
    let (quic_to_udp_tx, quic_to_udp_rx) = mpsc::unbounded_channel();
    let (udp_to_quic_tx, udp_to_quic_rx) = mpsc::unbounded_channel();

    // 创建各层
    let mut app_layer = AppLayer::new(app_to_quic_tx, quic_to_app_rx);
    let mut quic_layer = QuicLayer::new(
        conn,
        server_addr,
        app_to_quic_rx,
        quic_to_app_tx,
        quic_to_udp_tx,
        udp_to_quic_rx,
    );
    let mut udp_layer = UdpLayer::new(socket, udp_to_quic_tx, quic_to_udp_rx);

    // 启动各层
    let app_handle = tokio::spawn(async move {
        if let Err(e) = app_layer.run().await {
            error!("❌ 应用层错误: {e}");
        }
    });

    let quic_handle = tokio::spawn(async move {
        if let Err(e) = quic_layer.run().await {
            error!("❌ QUIC层错误: {e}");
        }
    });

    let udp_handle = tokio::spawn(async move {
        if let Err(e) = udp_layer.run().await {
            error!("❌ UDP层错误: {e}");
        }
    });

    info!("🚀 所有层启动完成，开始运行");

    // 等待任意一个层结束
    tokio::select! {
        _ = app_handle => info!("📱 应用层已退出"),
        _ = quic_handle => info!("🔗 QUIC层已退出"),
        _ = udp_handle => info!("🌐 UDP层已退出"),
    }

    info!("👋 分层客户端关闭");
    Ok(())
}

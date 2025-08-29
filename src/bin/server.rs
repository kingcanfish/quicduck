use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use quiche::Connection;
use ring::rand::SecureRandom;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use quiche::ConnectionId;

use quicduck::{config, create_simple_config, generate_cert_and_key};

/// 简单的 QUIC 服务器
pub struct SimpleQuicServer {
    socket: UdpSocket,
    // 使用ConnectionID作为主键，支持连接迁移
    connections: Arc<Mutex<HashMap<Vec<u8>, (Connection, SocketAddr)>>>, // (连接对象, 当前客户端地址)
    // ConnectionID映射表：从scid映射到当前dcid
    conn_id_mapping: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
    // 存储每个连接的流数据缓冲区：连接ID -> (流ID -> 缓冲区数据)
    stream_buffers: Arc<Mutex<HashMap<Vec<u8>, HashMap<u64, Vec<u8>>>>>,
}

impl SimpleQuicServer {
    /// 创建新的 QUIC 服务器
    pub async fn new(addr: &str) -> Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        println!("🦆 QUIC 服务器启动在: {addr}");

        Ok(Self {
            socket,
            connections: Arc::new(Mutex::new(HashMap::new())),
            conn_id_mapping: Arc::new(Mutex::new(HashMap::new())),
            stream_buffers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 运行服务器
    pub async fn run(&self) -> Result<()> {
        let mut buf = [0; config::MAX_DATAGRAM_SIZE];
        let mut out = [0; config::MAX_DATAGRAM_SIZE];

        loop {
            // 接收数据包
            let (len, from) = self.socket.recv_from(&mut buf).await?;
            println!("📦 收到来自 {from} 的 {len} 字节数据");

            // 处理数据包
            if let Err(e) = self.handle_packet(&buf[..len], from, &mut out).await {
                eprintln!("❌ 处理数据包失败: {e}");
            }
        }
    }

    /// 处理单个数据包
    async fn handle_packet(&self, pkt: &[u8], from: SocketAddr, out: &mut [u8]) -> Result<()> {
        // 解析数据包头获取连接ID
        let hdr = quiche::Header::from_slice(&mut pkt.to_vec(), quiche::MAX_CONN_ID_LEN)
            .map_err(|e| anyhow!("解析数据包头失败: {e}"))?;

        println!(
            "📦 收到数据包类型: {:?}, scid: {:?}, dcid: {:?}, 来自: {from}",
            hdr.ty, hdr.scid, hdr.dcid
        );

        // 查找连接：首先尝试用dcid，如果找不到则检查映射表
        let mut connections = self.connections.lock().await;

        let mut conn_key = hdr.dcid.to_vec();

        // 检查连接是否存在
        let connection_exists = connections.contains_key(&conn_key);

        if !connection_exists {
            // 连接不存在 - 只有Initial类型才能创建新连接
            match hdr.ty {
                quiche::Type::Initial => {
                    println!(
                        "🆕 处理新的 Initial 连接,scid: {:?} dcid: {:?}, 来自: {from}",
                        hdr.scid, hdr.dcid
                    );

                    // 创建服务器配置
                    let mut config = create_simple_config()?;

                    // 生成并保存临时证书
                    ensure_test_cert_exists()?;
                    config.load_cert_chain_from_pem_file("cert.pem")?;
                    config.load_priv_key_from_pem_file("key.pem")?;

                    // 创建新连接

                    // 生成连接 ID
                    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
                    ring::rand::SystemRandom::new().fill(&mut scid).unwrap();
                    let scid = ConnectionId::from_ref(&scid);
                    let mut conn =
                        quiche::accept(&scid, None, self.socket.local_addr()?, from, &mut config)?;

                    // 发送待发送的数据包
                    loop {
                        let (write, send_info) = match conn.send(out) {
                            Ok(v) => v,
                            Err(quiche::Error::Done) => break,
                            Err(e) => return Err(anyhow!("发送失败: {e}")),
                        };

                        self.socket.send_to(&out[..write], send_info.to).await?;
                    }
                    conn_key = scid.to_vec();
                    connections.insert(scid.to_vec(), (conn, from));
                }
                _ => {
                    println!(
                        "⚠️ 找不到连接且非Initial数据包: {:?}, dcid: {:?}",
                        hdr.ty, hdr.dcid
                    );
                    return Ok(());
                }
            }
        }

        // 获取连接并更新客户端地址（支持连接迁移）
        let (conn, _stored_addr) = match connections.get_mut(&conn_key) {
            Some((conn, addr)) => {
                if *addr != from {
                    println!("🚀 检测到连接迁移: {addr} -> {from}");
                    *addr = from;
                }
                (conn, *addr)
            }
            None => {
                println!("⚠️ 找不到连接: dcid={conn_key:?}，可能连接已关闭");
                return Ok(());
            }
        };

        // 接收数据包
        conn.recv(
            &mut pkt.to_vec(),
            quiche::RecvInfo {
                to: self.socket.local_addr()?,
                from,
            },
        )?;

        // 处理可读的流
        if conn.is_established() {
            let mut stream_buffers = self.stream_buffers.lock().await;
            let conn_stream_buffers = stream_buffers.entry(conn_key.clone()).or_insert_with(HashMap::new);
            
            for stream_id in conn.readable() {
                // 获取或创建该流的缓冲区
                let stream_buffer = conn_stream_buffers.entry(stream_id).or_insert_with(Vec::new);
                
                loop {
                    let mut stream_buf = vec![0; 1024];
                    match conn.stream_recv(stream_id, &mut stream_buf) {
                        Ok((len, fin)) => {
                            if len > 0 {
                                stream_buffer.extend_from_slice(&stream_buf[..len]);
                                println!("📥 从流 {stream_id} 读取了 {len} 字节，fin: {fin}, 缓冲区总计: {} 字节", stream_buffer.len());
                            }
                            
                            // 如果收到 fin 标志，处理完整消息
                            if fin {
                                if !stream_buffer.is_empty() {
                                    let msg = String::from_utf8_lossy(stream_buffer);
                                    println!("📨 收到完整消息 ({} 字节): \"{msg}\"", stream_buffer.len());
                                    
                                    // 发送回应，设置 fin=true 表示响应发送完毕
                                    let response = format!("Echo: {msg}");
                                    conn.stream_send(stream_id, response.as_bytes(), true)?;
                                    println!("📤 发送回应 ({} 字节，fin=true): \"{response}\"", response.len());
                                    
                                    // 清理该流的缓冲区
                                    conn_stream_buffers.remove(&stream_id);
                                }
                                break;
                            }
                            
                            if len == 0 {
                                // 没有更多数据但流未结束，保留缓冲区数据
                                println!("⚠️ 流{stream_id}未结束，等待后续数据");
                                break;
                            }
                        }
                        Err(quiche::Error::Done) => {
                            // 当前没有更多数据可读，保留已读数据等待后续数据
                            println!("⚠️ 流{stream_id}暂无更多数据，等待后续数据");
                            break;
                        }
                        Err(e) => {
                            eprintln!("读取流失败: {e}");
                            break;
                        }
                    }
                }
            }
        }

        // 发送待发送的数据包
        loop {
            let (write, send_info) = match conn.send(out) {
                Ok(v) => v,
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(anyhow!("发送失败: {e}")),
            };

            self.socket.send_to(&out[..write], send_info.to).await?;
        }

        // 检查连接是否关闭，如果关闭则清理连接
        if conn.is_closed() {
            println!("🚪 连接已关闭，清理连接: dcid={conn_key:?}");
            connections.remove(&conn_key);
            // 同时清理映射表中相关的条目
            let mut conn_id_mapping = self.conn_id_mapping.lock().await;
            conn_id_mapping.retain(|_, dcid| dcid != &conn_key);
            // 清理流缓冲区
            let mut stream_buffers = self.stream_buffers.lock().await;
            stream_buffers.remove(&conn_key);
        }

        Ok(())
    }
}

/// 确保测试证书文件存在
fn ensure_test_cert_exists() -> Result<()> {
    if std::path::Path::new("cert.pem").exists() && std::path::Path::new("key.pem").exists() {
        return Ok(());
    }
    generate_test_cert()
}

/// 生成测试用的自签名证书
fn generate_test_cert() -> Result<()> {
    use std::fs::File;
    use std::io::Write;

    println!("🔐 正在生成测试证书...");

    // 使用 rcgen 库生成证书和私钥
    let (cert_pem, key_pem) = generate_cert_and_key()?;

    // 写入证书文件
    let mut cert_file = File::create("cert.pem")?;
    cert_file.write_all(cert_pem.as_bytes())?;

    // 写入私钥文件
    let mut key_file = File::create("key.pem")?;
    key_file.write_all(key_pem.as_bytes())?;

    println!("✅ 已生成测试证书文件 cert.pem 和 key.pem");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🦆 QUIC Duck 服务器启动中...");

    // 确保测试证书存在
    ensure_test_cert_exists()?;

    let server = SimpleQuicServer::new("127.0.0.1:8080").await?;
    server.run().await
}

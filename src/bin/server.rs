use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use quiche::Connection;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use quicduck::{config, create_simple_config, generate_cert_and_key};

/// 简单的 QUIC 服务器
pub struct SimpleQuicServer {
    socket: UdpSocket,
    connections: Arc<Mutex<HashMap<String, Connection>>>,
}

impl SimpleQuicServer {
    /// 创建新的 QUIC 服务器
    pub async fn new(addr: &str) -> Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        println!("🦆 QUIC 服务器启动在: {addr}");

        Ok(Self {
            socket,
            connections: Arc::new(Mutex::new(HashMap::new())),
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

        // 简化连接管理：使用客户端地址作为连接标识符
        // 这样避免了复杂的连接ID映射问题
        let conn_key = format!("{}:{}", from.ip(), from.port());

        println!("📦 收到数据包类型: {:?}, scid: {:?}, dcid: {:?}, 来自: {from}", 
                 hdr.ty, hdr.scid, hdr.dcid);

        // 获取或创建连接
        let mut connections = self.connections.lock().await;
        
        if !connections.contains_key(&conn_key) {
            // 新连接 - 只处理 Initial 类型的数据包
            match hdr.ty {
                quiche::Type::Initial => {
                    println!("🆕 处理新的 Initial 连接来自: {conn_key}");
                },
                _ => {
                    println!("⚠️ 忽略非 Initial 类型的新连接数据包: {:?}", hdr.ty);
                    return Ok(());  // 忽略而不是报错
                }
            }

            // 创建服务器配置
            let mut config = create_simple_config()?;
            
            // 生成并保存临时证书
            ensure_test_cert_exists()?;
            config.load_cert_chain_from_pem_file("cert.pem")?;
            config.load_priv_key_from_pem_file("key.pem")?;
            
            // 创建新连接
            let conn = quiche::accept(&hdr.scid, Some(&hdr.dcid), 
                                   self.socket.local_addr()?, from, &mut config)?;
            
            connections.insert(conn_key.clone(), conn);
            println!("🔗 新连接建立: {conn_key} <- {from}");
        }

        // 获取连接并处理数据包
        let conn = match connections.get_mut(&conn_key) {
            Some(conn) => conn,
            None => {
                println!("⚠️ 找不到连接: {conn_key}，可能连接已关闭");
                return Ok(());
            }
        };
        
        // 接收数据包
        conn.recv(&mut pkt.to_vec(), quiche::RecvInfo {
            to: self.socket.local_addr()?,
            from,
        })?;

        // 处理可读的流
        if conn.is_established() {
            for stream_id in conn.readable() {
                // 完整读取客户端消息，不截断
                let mut complete_message = Vec::new();
                let mut total_len = 0;
                
                loop {
                    let mut stream_buf = vec![0; 1024];
                    match conn.stream_recv(stream_id, &mut stream_buf) {
                        Ok((len, fin)) => {
                            if len > 0 {
                                complete_message.extend_from_slice(&stream_buf[..len]);
                                total_len += len;
                                println!("📥 从流 {stream_id} 读取了 {len} 字节，fin: {fin}, 总计: {total_len} 字节");
                            }
                            
                            // 如果收到 fin 标志或没有更多数据，处理完整消息
                            if fin || len == 0 {
                                if !complete_message.is_empty() {
                                    let msg = String::from_utf8_lossy(&complete_message);
                                    println!("📨 收到完整消息 ({total_len} 字节): \"{msg}\"");
                                    
                                    // 发送回应，设置 fin=true 表示响应发送完毕
                                    let response = format!("Echo: {msg}");
                                    conn.stream_send(stream_id, response.as_bytes(), true)?;
                                    println!("📤 发送回应 ({} 字节，fin=true): \"{response}\"", response.len());
                                }
                                break;
                            }
                        }
                        Err(quiche::Error::Done) => break,
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
            println!("🚪 连接已关闭，清理连接: {conn_key}");
            connections.remove(&conn_key);
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
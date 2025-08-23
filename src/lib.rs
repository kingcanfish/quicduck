/// QUIC配置常量
pub mod config {
    pub const MAX_DATAGRAM_SIZE: usize = 1350;
}


/// 生成自签名证书和私钥（用于测试）
pub fn generate_cert_and_key() -> anyhow::Result<(String, String)> {
    use std::time::{Duration, SystemTime};
    
    // 创建证书参数
    let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]);
    params.not_before = (SystemTime::now() - Duration::from_secs(24 * 3600)).into(); // 1天前
    params.not_after = (SystemTime::now() + Duration::from_secs(365 * 24 * 3600)).into(); // 365天后
    params.serial_number = Some(1_u64.into());
    
    // 生成证书
    let cert = rcgen::Certificate::from_params(params)?;
    let cert_pem = cert.serialize_pem()?;
    let key_pem = cert.serialize_private_key_pem();
    
    Ok((cert_pem, key_pem))
}

pub fn create_simple_config() -> anyhow::Result<quiche::Config> {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    
    // 设置应用协议
    config.set_application_protos(&[b"quic-demo"])?;
    
    // 设置基本的流控制参数
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(100_000);
    config.set_initial_max_stream_data_bidi_remote(100_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    
    // 设置超时时间
    config.set_max_idle_timeout(30_000); // 30 秒
    
    Ok(config)
}
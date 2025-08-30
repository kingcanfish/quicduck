# QUIC Duck - 极简 QUIC 通信 Demo 🦆

一个用 Rust 编写的极简 QUIC 协议演示项目，展示客户端和服务器之间的基本通信。

## 特性

✨ **极简设计** - 最少的代码，最清晰的逻辑
🚀 **快速启动** - 无需复杂配置，开箱即用
🔒 **自动证书** - 使用 rcgen 库动态生成测试用的自签名证书
📋 **结构化日志** - 使用结构化logging，可通过环境变量灵活配置
📨 **回声服务** - 服务器会回显客户端发送的所有消息

## 项目结构

```
quicduck/
├── src/
│   ├── lib.rs          # 共享配置和工具函数
│   └── bin/
│       ├── server.rs   # QUIC 服务器
│       └── client.rs   # QUIC 客户端
├── Cargo.toml          # 项目配置（仅7个依赖项！）
└── README.md           # 项目说明
```

## 快速开始

### 1. 编译项目

```bash
cargo build --release
```

### 2. 启动服务器

在一个终端窗口中：

```bash
# 生产环境（最小日志）
RUST_LOG=error cargo run --bin server

# 开发环境（详细调试）
RUST_LOG=debug cargo run --bin server

# 默认（信息级别）
cargo run --bin server
```

你会看到类似输出（RUST_LOG=info 或更高级别时）：
```
[INFO] 🦆 QUIC Duck 服务器启动中...
[INFO] 🔗 监听地址: 127.0.0.1:8080
[INFO] 🔐 证书域名: localhost
[INFO] 🦆 QUIC 服务器启动在: 127.0.0.1:8080
```

（RUST_LOG=debug 时会显示更多网络层调试信息）

### 3. 运行客户端

在另一个终端窗口中：

```bash
# 匹配开发环境日志级别
RUST_LOG=debug cargo run --bin client

# 或使用默认信息级别
cargo run --bin client
```

客户端会自动发送几条测试消息并显示服务器的回应：

（RUST_LOG=info 输出）
```
[INFO] 🦆 QUIC Duck 客户端启动中...
[INFO] 🔗 客户端本地地址: 127.0.0.1:54321
[INFO] 🏠 连接到服务器: 127.0.0.1:8080
[INFO] 📡 正在连接到服务器 127.0.0.1:8080
[INFO] ✅ 连接已建立!
```

（RUST_LOG=debug 时会显示更多网络层调试信息，包括数据包传输详情）

## 技术细节

### 依赖项
- `tokio` - 异步运行时
- `quiche` - QUIC 协议实现
- `ring` - 加密算法库
- `rcgen` - 自签名证书生成库
- `anyhow` - 错误处理
- `log` - 日志接口
- `env_logger` - 环境变量配置的日志后端

### QUIC 配置
- 协议版本: 最新的 QUIC 标准版本
- 应用协议: "quic-demo"
- 最大数据量: 1MB
- 双向流数量: 10
- 连接超时: 30秒

### 日志配置
项目使用结构化日志，可以通过 `RUST_LOG` 环境变量灵活控制日志输出级别：

- `RUST_LOG=error` - 只显示错误信息
- `RUST_LOG=warn` - 显示警告和错误
- `RUST_LOG=info` - 显示基本信息、警告和错误 (默认)
- `RUST_LOG=debug` - 显示详细调试信息
- `RUST_LOG=quicduck=debug` - 只对本项目启用调试级别
- `RUST_LOG=off` - 完全关闭日志

示例：
```bash
# 在生产环境使用最小日志输出
RUST_LOG=error cargo run --bin server --release

# 在开发环境使用详细调试信息
RUST_LOG=debug cargo run --bin client

# 关闭所有日志输出
RUST_LOG=off cargo run --bin server
```

### 安全说明
⚠️  **仅用于开发和测试**
- 使用自签名证书
- 关闭了证书验证
- 不适合生产环境使用

## 扩展建议

这个项目作为学习 QUIC 协议的起点，你可以继续扩展：

1. **配置化日志** - ⭐ 已完成：结构化日志适用于生产环境
2. **增加消息类型** - 支持不同类型的消息
3. **持久连接** - 保持长连接进行多轮通信
4. **多客户端支持** - 服务器同时处理多个客户端
5. **流控制** - 实现更复杂的数据流管理
6. **真实证书** - 使用有效的 TLS 证书

## 故障排除

### 常见问题

**Q: 编译失败，提示找不到 `quiche`？**  
A: 确保你使用的是最新版本的 Rust (1.70+)

**Q: 连接失败？**  
A: 检查防火墙设置，确保端口 8080 没有被占用

**Q: 证书错误？**
A: 删除 `cert.pem` 和 `key.pem` 文件，重新运行服务器

**Q: 我看不到任何输出或日志？**
A: 可能是日志级别设置问题。运行时设置 `RUST_LOG=info` 或 `RUST_LOG=debug` 来启用日志输出

## 协议版本

使用 QUIC 协议版本: 0x00000001 (RFC 9000)

## 许可证

MIT License

---

Made with ❤️ and 🦆 by QUIC enthusiasts
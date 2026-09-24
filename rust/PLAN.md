# Rust 重构计划（mini-ftpd）

> 目标：将 Slyar FTPserver（MFC / VC6，仅 Windows）用 Rust 重构，支持跨平台发布。
> 决策记录：**CLI 先行、GUI 后置**；功能范围**完全对齐原版**；代码放 `rust/` 子目录，原 MFC 代码保留对照。

## 一、现状分析（原版）

- 约 3700 行 MFC C++，依赖 WinSock、注册表配置、系统托盘，仅 Windows
- 网络模型：`CListenSocket` 监听 → `CConnectThread` 每连接一线程 → `CControlSocket` 命令通道 + `CDataSocket` 数据通道
- 21 条 FTP 命令：USER / PASS / QUIT / TYPE / PWD / CDUP / CWD / PORT / PASV / LIST / RETR / STOR / SIZE / DELE / RNFR / RNTO / RMD / MKD / ABOR / SYST / NOOP
- 功能：用户管理、目录级六项权限、最大连接数、主动/被动模式、托盘 GUI、注册表存配置

## 二、技术选型

| 用途 | 选择 | 理由 |
|---|---|---|
| 异步运行时 | `tokio` | 替代「每连接一线程」 |
| CLI 参数 | `clap` | 端口/根目录/配置文件覆盖 |
| 配置文件 | `serde` + `toml` | 替代注册表，跨平台 |
| 日志 | `tracing` + `tracing-subscriber` | 替代窗口日志 |
| 错误处理 | `thiserror` + `anyhow` | — |

## 三、项目结构（core 与 UI 解耦，为 GUI 留口）

```
rust/
├── Cargo.toml              # workspace
├── config.example.toml     # 配置样例（全字段注释）
├── crates/
│   ├── ftpd-core/          # 核心库
│   │   └── src/
│   │       ├── config.rs   # TOML 配置（替代注册表）
│   │       ├── auth.rs     # 用户认证 + 六项权限位
│   │       ├── fs.rs       # 虚拟路径映射、chroot 防护、LIST 格式化
│   │       ├── command.rs  # 21 条命令解析
│   │       ├── data.rs     # PORT/PASV 数据通道（替代 DataSocket）
│   │       ├── session.rs  # 控制连接状态机（替代 ControlSocket/ConnectThread）
│   │       └── server.rs   # 监听 + 连接数限制（替代 ListenSocket）
│   └── mini-ftpd/          # CLI 二进制
└── tests 在 ftpd-core/tests/
```

## 四、里程碑

| # | 内容 | 状态 |
|---|---|---|
| M1 | workspace 骨架、配置加载、CLI、日志 | ✅ 完成 |
| M2 | 协议核心：监听 → 会话状态机 → 命令分发（USER/PASS/PWD/CWD/QUIT 先行） | ✅ 完成 |
| M3 | 数据通道：PASV + LIST/RETR/STOR | ✅ 完成 |
| M4 | 主动模式 + 写操作：PORT、DELE/RNFR/RNTO/MKD/RMD/SIZE/ABOR/SYST/NOOP/TYPE | ✅ 完成 |
| M5 | 权限系统：六项权限、最大连接数 `421`、空闲超时 `426` | ✅ 完成 |
| M6 | 测试：23 项自动化（单元+端到端）+ ftplib/curl 真实客户端验证 | ✅ 完成 |
| M7 | 跨平台发布：GitHub Actions 矩阵构建 + tag 触发 Release | ✅ 完成（`.github/workflows/`） |

## 五、跨平台发布方案

- `rust-ci.yml`：push/PR 时在 Linux / Windows / macOS 跑 fmt + clippy + test
- `rust-release.yml`：打 `rust-v*` tag 触发
  - 目标：`x86_64-pc-windows-msvc`、`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`（cross）、`x86_64-apple-darwin`、`aarch64-apple-darwin`
  - 产物：zip（Windows）/ tar.gz（其余）+ SHA256SUMS，发布到 GitHub Releases
- 本地交叉编译 Windows 版：`rustup target add x86_64-pc-windows-gnu` + mingw-w64，产出见 `rust/dist/`

## 六、与原版的有意差异（安全/正确性修正，协议响应码不变）

1. chroot 强制防目录穿越（原版字符串前缀匹配可绕过；含符号链接逃逸）
2. STOR 正常截断（原版 `modeNoTruncate` 覆盖短文件残留尾部垃圾）
3. RMD 拒绝删除主目录本身
4. 空 PASS 不再无条件放行（anonymous 仍任意密码）
5. 容忍 `LIST -la` 等客户端 flags
6. PORT 格式错误回 `501`（原版恒 200）
7. 传输中除 ABOR/QUIT 外回 `503`（原版交错处理）
8. CWD 目标必须是目录

## 七、后续规划（Phase 4）

| 内容 | 状态 |
|---|---|
| egui + tray-icon 桌面 GUI / 系统托盘（`crates/mini-ftpd-gui`，复用 `ftpd-core`） | ✅ 完成：配置编辑、多用户权限、启停、连接数、日志面板、最小化到托盘（托盘不可用时优雅降级） |
| GUI 产物纳入 Release 打包 | ✅ 完成：Windows / macOS / Linux x64 包内含 CLI + GUI；aarch64 Linux 仅 CLI（cross 容器 arm64 GTK 依赖脆弱，且该平台以服务器场景为主） |
| 可选：配置热加载、TLS（FTPS）、日志滚动 | 待办 |

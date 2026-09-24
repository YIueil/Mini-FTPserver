# mini-ftpd

经典 Slyar FTPserver（MFC，仅 Windows）的 Rust 重写版：跨平台、异步、单二进制。

## 特性

- 与原版行为对齐的 21 条 FTP 命令：USER / PASS / QUIT / TYPE / PWD / CDUP / CWD / PORT / PASV / LIST / RETR / STOR / SIZE / DELE / RNFR / RNTO / RMD / MKD / ABOR / SYST / NOOP（其余一律 `502`）
- 主动（PORT）与被动（PASV）数据通道
- 多用户 + 目录级权限：下载 / 上传 / 重命名 / 删除 / 建目录（列表权限 = 下载或上传，同原版规则）
- 最大连接数限制、控制连接空闲超时
- 响应报文逐条对齐原版（见下「与原版的有意差异」）
- 系统要求：无（纯 Rust，静态链接 musl 亦可自行配置 target）

## 构建与运行

```bash
cd rust
cargo build --release
./target/release/mini-ftpd --config config.example.toml
```

无配置直接运行时使用内置默认值（与原版默认一致）：端口 21、用户 `anonymous`（任意密码可登录）、主目录 `./ftp_root`、只读（仅下载）。

命令行参数（覆盖配置文件）：

```
-c, --config <FILE>   TOML 配置文件路径
-p, --port <PORT>     监听端口
-l, --listen <ADDR>   监听地址
    --root <DIR>      覆盖首个用户的主目录
```

配置项见 [config.example.toml](config.example.toml)。日志由 `RUST_LOG` 控制，如 `RUST_LOG=debug`。

## 跨平台发布

打 tag 即触发 GitHub Actions 矩阵构建并发布 Release：

```bash
git tag rust-v1.0.0 && git push origin rust-v1.0.0
```

产物（含 SHA256 校验和）。除 aarch64 Linux 外的包同时包含 CLI（`mini-ftpd`）与 GUI（`mini-ftpd-gui`）；aarch64 Linux 面向服务器场景，仅含 CLI：

| 目标平台 | 产物 | 内容 |
|---|---|---|
| Linux x86_64 | `mini-ftpd-<ver>-<target>.tar.gz` | CLI + GUI |
| Linux aarch64 | `mini-ftpd-<ver>-<target>.tar.gz` | 仅 CLI |
| macOS x86_64 / Apple Silicon | `mini-ftpd-<ver>-<target>.tar.gz` | CLI + GUI |
| Windows x86_64 | `mini-ftpd-<ver>-<target>.zip` | CLI + GUI |

本地交叉编译 aarch64 Linux：`cargo install cross && cross build --release --target aarch64-unknown-linux-gnu`

## 架构

```
crates/ftpd-core        核心库（CLI 与 GUI 共用）
  src/config.rs         TOML 配置（替代原版的注册表）
  src/auth.rs           用户认证与权限
  src/fs.rs             虚拟路径映射、chroot 防护、Unix 风格 LIST 格式化
  src/command.rs        命令行解析
  src/data.rs           PORT/PASV 数据通道（10 秒连接等待，同原版）
  src/session.rs        控制连接状态机（替代 CControlSocket + CConnectThread）
  src/server.rs         监听与连接数限制（替代 CListenSocket）
crates/mini-ftpd        CLI 二进制
crates/mini-ftpd-gui    桌面 GUI（egui + 系统托盘）：配置编辑、用户权限、启停、日志面板
```

GUI 运行：`cargo run -p mini-ftpd-gui`（关闭窗口最小化到托盘，托盘不可用时直接退出；仅允许单实例运行；启动时自动加载系统中文字体；顶栏可切换亮色/暗黑主题、显示/隐藏日志面板，选择自动保存；启动服务器时自动保存配置并在日志中列出各网卡的 ftp:// 访问地址）。GUI 与 CLI 共用同一份 TOML 配置（默认 `./mini-ftpd.toml`）。

Linux 构建 GUI 需要系统依赖（托盘后端）：`sudo apt install libgtk-3-dev libappindicator3-dev libxdo-dev`（Arch：`pacman -S gtk3 libappindicator-gtk3 xdotool`；Windows / macOS 无需额外依赖）。

异步模型：Tokio。原版的「每连接一个 MFC 线程 + 定时器」由单任务 + `tokio::select!` 取代；传输在独立 task 中执行，支持 ABOR 中途取消。

## 测试

```bash
cargo test --workspace
```

- 单元测试：路径规范化与逃逸防护、权限映射、命令解析、LIST 格式
- 集成测试：真实起服务器，用原始协议客户端跑通 登录/PWD/CWD/MKD/STOR/LIST/SIZE/RETR/RNFR/RNTO/DELE/RMD（PASV + PORT 两种模式）、权限拒绝、`..` 逃逸拦截、超限 `421`、空闲超时 `426`、ABOR 取消
- 另已用真实客户端（Python `ftplib`、`curl`）人工验证上传/下载/列目录/重命名/删除

## 与原版的有意差异

均为安全或正确性修正，协议响应码保持一致：

1. **目录穿越**：原版 `GetLocalPath` 用字符串前缀匹配防逃逸，可被绕过；本版用组件规范化 + canonicalize 强制 chroot，符号链接逃逸同样拦截。
2. **STOR 截断**：原版以 `modeNoTruncate` 打开已存在文件，覆盖较短文件时残留尾部垃圾；本版正常截断。
3. **RMD 根目录**：原版允许删除用户主目录本身；本版拒绝。
4. **PASS 空密码绕过**：原版当客户端发送空 PASS 时无论配置如何都放行；本版要求密码匹配（anonymous 仍任意密码）。
5. **LIST 参数**：容忍客户端常见 flags（`LIST -la`），原版按字面路径处理导致 550。
6. **PORT 参数**：格式错误时回 `501`，原版无条件回 200。
7. **传输中命令**：传输期间除 ABOR/QUIT 外回 `503 Transfer in progress.`，原版会交错处理。
8. **CWD 目标必须是目录**，原版对文件也会返回成功。

原版缺陷亦未复刻：GBK 编码依赖（本版按 UTF-8/字节透传）、`226 Transfer complete`/`226 Transfer complete.` 句号不一致（本版保留两种原文以兼容各自场景）。

## 与原版的模块对照

| MFC（原版） | Rust |
|---|---|
| `CListenSocket` | `server.rs` accept 循环 |
| `CConnectThread` + 定时器 | `session.rs` + `tokio::time::timeout` |
| `CControlSocket` | `session.rs` 状态机 + `command.rs` |
| `CDataSocket` | `data.rs` + 传输 task |
| 注册表配置 | `config.rs`（TOML） |
| 对话框/托盘 GUI | 暂未实现（规划：egui，核心库已解耦） |

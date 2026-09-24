#![windows_subsystem = "windows"]

use eframe::egui;
use ftpd_core::auth::UserStore;
use ftpd_core::config::{ServerConfig, UserConfig};
use ftpd_core::server::FtpServer;
#[cfg(windows)]
use raw_window_handle::HasWindowHandle;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(windows)]
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

const DEFAULT_CONFIG_PATH: &str = "./mini-ftpd.toml";
const MAX_LOG_LINES: usize = 500;

fn main() -> eframe::Result<()> {
    // 托盘程序惯例：只允许一个实例，否则每次启动都会多一个托盘图标
    if !acquire_single_instance() {
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mini-ftpd")
            .with_inner_size([860.0, 620.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mini-ftpd",
        options,
        Box::new(|cc| {
            let app = GuiApp::new(cc);
            install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\mini-ftpd-gui-single-instance\0"
        .encode_utf16()
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return false;
        }
        // 有意泄漏句柄：进程存活期间持有互斥体
        true
    }
}

#[cfg(not(windows))]
fn acquire_single_instance() -> bool {
    use std::os::unix::ffi::OsStrExt;
    let path = std::env::temp_dir().join("mini-ftpd-gui.lock");
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    unsafe {
        let fd = libc::open(c_path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
        if fd < 0 {
            return true;
        }
        // 有意泄漏 fd：进程存活期间持有文件锁
        libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0
    }
}

// egui 内置字体不含 CJK 字形，从系统加载中文字体兜底
fn install_cjk_font(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/msyh.ttf",
            "C:/Windows/Fonts/simhei.ttf",
            "C:/Windows/Fonts/simsun.ttc",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        ]
    };
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return;
    }
    tracing::warn!("未找到系统中文字体，界面中文可能无法显示");
}

// ---------- log capture ----------

#[derive(Clone, Default)]
struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    fn push(&self, text: &str) {
        let mut lines = self.lines.lock().unwrap();
        for line in text.lines() {
            while lines.len() >= MAX_LOG_LINES {
                lines.pop_front();
            }
            lines.push_back(line.to_string());
        }
    }

    fn text(&self) -> String {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }
}

struct LogWriter(LogBuffer);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.push(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------- server handle ----------

struct RunningServer {
    token: CancellationToken,
    active: Arc<AtomicUsize>,
    addr: SocketAddr,
}

// ---------- tray ----------

#[cfg(target_os = "linux")]
fn gtk_ready() -> bool {
    gtk::init().is_ok()
}

#[cfg(not(target_os = "linux"))]
fn gtk_ready() -> bool {
    true
}

struct TrayState {
    _tray: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

fn make_icon() -> tray_icon::Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if dx * dx + dy * dy < 13.0 * 13.0 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 0x2b;
                rgba[i + 1] = 0x6c;
                rgba[i + 2] = 0xb5;
                rgba[i + 3] = 0xff;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("icon size is valid")
}

fn setup_tray() -> Option<TrayState> {
    let show = MenuItem::new("显示主窗口", true, None);
    let quit = MenuItem::new("退出", true, None);
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();
    let menu = Menu::with_items(&[&show, &quit]).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("mini-ftpd")
        .with_icon(make_icon())
        .build()
        .ok()?;
    Some(TrayState {
        _tray: tray,
        show_id,
        quit_id,
    })
}

// 窗口隐藏后 eframe 不再驱动帧（update 不会被调用），托盘事件必须在独立线程里处理，
// 并用原生 API 直接唤醒窗口，否则托盘菜单在最小化后完全失效
fn spawn_tray_menu_thread(
    tray: &TrayState,
    _ctx: egui::Context,
    #[cfg(windows)] hwnd: Arc<AtomicIsize>,
) {
    let show_id = tray.show_id.clone();
    let quit_id = tray.quit_id.clone();
    let spawn = std::thread::Builder::new()
        .name("tray-menu".to_owned())
        .spawn(move || {
            while let Ok(event) = MenuEvent::receiver().recv() {
                if event.id == quit_id {
                    // 隐藏状态下无法走 eframe 正常关闭流程；与原版一致，退出时由 OS 回收连接
                    std::process::exit(0);
                }
                if event.id == show_id {
                    #[cfg(windows)]
                    wake_main_window(&hwnd);
                    #[cfg(not(windows))]
                    wake_main_window(&_ctx);
                }
            }
        });
    if let Err(e) = spawn {
        tracing::warn!("托盘事件线程启动失败：{e}");
    }
}

#[cfg(windows)]
fn wake_main_window(hwnd: &AtomicIsize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let raw = hwnd.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    unsafe {
        ShowWindow(raw as _, SW_RESTORE);
        SetForegroundWindow(raw as _);
    }
}

// 其他平台缺少可靠的唤醒手段，尽力而为（部分平台隐藏窗口后不重绘，可能无效）
#[cfg(not(windows))]
fn wake_main_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

// ---------- app ----------

fn apply_theme(ctx: &egui::Context, dark_mode: bool) {
    ctx.set_visuals(if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
}

// 启动后列出本机各网卡的访问地址，方便直接复制给客户端
fn log_access_addrs(port: u16) {
    let mut ips: Vec<IpAddr> = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    match local_ip_address::list_afinet_netifas() {
        Ok(ifas) => {
            for (_, addr) in ifas {
                if let IpAddr::V4(v4) = addr {
                    if !v4.is_loopback() && !ips.contains(&addr) {
                        ips.push(addr);
                    }
                }
            }
        }
        Err(e) => tracing::warn!("获取网卡地址失败：{e}"),
    }
    let addrs = ips
        .iter()
        .map(|ip| format!("ftp://{ip}:{port}"))
        .collect::<Vec<_>>()
        .join("  ");
    tracing::info!("访问地址：{addrs}");
}

struct GuiApp {
    config: ServerConfig,
    listen_str: String,
    status_msg: String,
    logs: LogBuffer,
    rt: tokio::runtime::Runtime,
    server: Option<RunningServer>,
    tray: Option<TrayState>,
    dark_mode: bool,
    show_logs: bool,
    #[cfg(windows)]
    hwnd: Arc<AtomicIsize>,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(12.0, 5.0);
        });

        let dark_mode = cc
            .storage
            .and_then(|s| s.get_string("theme"))
            .map(|t| t != "light")
            .unwrap_or(true);
        apply_theme(ctx, dark_mode);
        let show_logs = cc
            .storage
            .and_then(|s| s.get_string("show_logs"))
            .map(|v| v != "0")
            .unwrap_or(true);

        // Route tracing events into the GUI log panel.
        let logs = LogBuffer::default();
        let writer = logs.clone();
        tracing_subscriber::fmt()
            .with_writer(move || LogWriter(writer.clone()))
            .with_ansi(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();

        let (config, status_msg) =
            match ServerConfig::load(std::path::Path::new(DEFAULT_CONFIG_PATH)) {
                Ok(cfg) => (cfg, format!("已加载配置 {DEFAULT_CONFIG_PATH}")),
                Err(_) => (
                    ServerConfig::default(),
                    format!("未找到 {DEFAULT_CONFIG_PATH}，使用内置默认配置"),
                ),
            };
        let listen_str = config.listen.to_string();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let gtk_ok = gtk_ready();
        let tray = if gtk_ok { setup_tray() } else { None };
        if tray.is_none() {
            tracing::warn!("系统托盘不可用（关闭窗口将直接退出）");
        }

        #[cfg(windows)]
        let hwnd = Arc::new(AtomicIsize::new(0));
        if let Some(tray) = &tray {
            spawn_tray_menu_thread(
                tray,
                ctx.clone(),
                #[cfg(windows)]
                Arc::clone(&hwnd),
            );
        }

        Self {
            config,
            listen_str,
            status_msg,
            logs,
            rt,
            server: None,
            tray,
            dark_mode,
            show_logs,
            #[cfg(windows)]
            hwnd,
        }
    }

    fn apply_listen(&mut self) -> Result<(), String> {
        self.config.listen = self
            .listen_str
            .trim()
            .parse()
            .map_err(|_| format!("监听地址无效：{}", self.listen_str))?;
        Ok(())
    }

    fn start_server(&mut self) {
        if self.server.is_some() {
            return;
        }
        if let Err(e) = self.apply_listen() {
            self.status_msg = e;
            return;
        }
        let config = self.config.clone();
        let result = config
            .validate()
            .map_err(|e| e.to_string())
            .and_then(|_| {
                for user in &config.users {
                    if !user.home.exists() {
                        std::fs::create_dir_all(&user.home)
                            .map_err(|e| format!("无法创建主目录 {}：{e}", user.home.display()))?;
                    }
                }
                UserStore::new(&config.users).map_err(|e| e.to_string())
            })
            .and_then(|users| {
                self.rt
                    .block_on(FtpServer::bind(Arc::new(config), Arc::new(users)))
                    .map_err(|e| e.to_string())
            });
        match result {
            Ok(server) => {
                let addr = server.local_addr().expect("bound listener");
                let active = server.active_connections();
                let token = CancellationToken::new();
                let shutdown = token.clone();
                self.rt.spawn(async move {
                    if let Err(e) = server.run_until(shutdown).await {
                        tracing::error!("服务器异常：{e}");
                    }
                });
                self.server = Some(RunningServer {
                    token,
                    active,
                    addr,
                });
                match self.save_config() {
                    Ok(_) => tracing::info!("配置已保存到 {DEFAULT_CONFIG_PATH}"),
                    Err(e) => tracing::warn!("{e}"),
                }
                self.status_msg = format!("服务器运行中：{addr}");
                tracing::info!("服务器已启动，监听 {addr}");
                log_access_addrs(addr.port());
            }
            Err(e) => self.status_msg = format!("启动失败：{e}"),
        }
    }

    fn stop_server(&mut self) {
        if let Some(server) = self.server.take() {
            server.token.cancel();
            self.status_msg = "已停止接受新连接（已有会话继续至结束）".to_string();
            tracing::info!("服务器已停止");
        }
    }

    fn save_config(&self) -> Result<(), String> {
        let text = toml::to_string_pretty(&self.config).map_err(|e| format!("序列化失败：{e}"))?;
        std::fs::write(DEFAULT_CONFIG_PATH, text).map_err(|e| format!("保存配置失败：{e}"))
    }

    fn server_settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(ui.available_width());
        egui::Grid::new("server_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("监听地址");
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [130.0, 22.0],
                        egui::TextEdit::singleline(&mut self.listen_str),
                    );
                    ui.label("端口");
                    ui.add_sized(
                        [56.0, 22.0],
                        egui::DragValue::new(&mut self.config.port).range(1..=65535),
                    );
                });
                ui.end_row();

                ui.label("最大连接数");
                ui.add_sized(
                    [64.0, 22.0],
                    egui::DragValue::new(&mut self.config.max_connections).range(1..=10000),
                );
                ui.end_row();

                ui.label("空闲超时 (秒)");
                ui.add_sized(
                    [64.0, 22.0],
                    egui::DragValue::new(&mut self.config.idle_timeout_secs).range(0..=86400),
                );
                ui.end_row();

                ui.label("PASV 端口");
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [64.0, 22.0],
                        egui::DragValue::new(&mut self.config.passive_ports.min).range(0..=65535),
                    );
                    ui.label("—");
                    ui.add_sized(
                        [64.0, 22.0],
                        egui::DragValue::new(&mut self.config.passive_ports.max).range(0..=65535),
                    );
                });
                ui.end_row();
            });
        ui.add_space(4.0);
        ui.label("欢迎消息");
        ui.add_sized(
            [ui.available_width(), 22.0],
            egui::TextEdit::singleline(&mut self.config.welcome_message),
        );
        ui.add_space(2.0);
        ui.label("告别消息");
        ui.add_sized(
            [ui.available_width(), 22.0],
            egui::TextEdit::singleline(&mut self.config.goodbye_message),
        );
        ui.add_space(4.0);
        ui.label(egui::RichText::new("PASV 端口 min=0 表示由系统自动分配（与原版一致）").weak());
    }

    fn users_ui(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(ui.available_width());
        let mut remove_idx = None;
        for (i, user) in self.config.users.iter_mut().enumerate() {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let title = if user.username.is_empty() {
                            format!("用户 #{}", i + 1)
                        } else {
                            user.username.clone()
                        };
                        ui.label(egui::RichText::new(title).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("删除此用户").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    egui::Grid::new(ui.id().with("grid"))
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("用户名");
                            ui.add_sized(
                                [130.0, 22.0],
                                egui::TextEdit::singleline(&mut user.username),
                            );
                            ui.end_row();

                            ui.label("密码");
                            ui.add_sized(
                                [130.0, 22.0],
                                egui::TextEdit::singleline(&mut user.password).password(true),
                            );
                            ui.end_row();
                        });
                    ui.label("主目录");
                    let mut home = user.home.to_string_lossy().into_owned();
                    if ui
                        .add_sized(
                            [ui.available_width(), 22.0],
                            egui::TextEdit::singleline(&mut home),
                        )
                        .changed()
                    {
                        user.home = home.into();
                    }
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("权限");
                        ui.checkbox(&mut user.permissions.download, "下载");
                        ui.checkbox(&mut user.permissions.upload, "上传");
                        ui.checkbox(&mut user.permissions.rename, "重命名");
                        ui.checkbox(&mut user.permissions.delete, "删除");
                        ui.checkbox(&mut user.permissions.mkdir, "建目录");
                    });
                });
            ui.add_space(6.0);
        }
        if let Some(i) = remove_idx {
            self.config.users.remove(i);
        }
        if ui
            .add_sized(
                [ui.available_width(), 28.0],
                egui::Button::new("+ 添加用户"),
            )
            .clicked()
        {
            self.config.users.push(UserConfig {
                username: format!("user{}", self.config.users.len() + 1),
                ..UserConfig::default()
            });
        }
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "列表权限 = 下载 或 上传（同原版规则）；覆盖已存在文件需要「删除」权限",
            )
            .weak(),
        );
    }
}

impl eframe::App for GuiApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(
            "theme",
            if self.dark_mode { "dark" } else { "light" }.to_owned(),
        );
        storage.set_string(
            "show_logs",
            if self.show_logs { "1" } else { "0" }.to_owned(),
        );
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Refresh logs / connection count periodically.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        #[cfg(windows)]
        if self.hwnd.load(Ordering::Relaxed) == 0 {
            if let Ok(handle) = _frame.window_handle() {
                if let raw_window_handle::RawWindowHandle::Win32(w) = handle.as_raw() {
                    self.hwnd.store(w.hwnd.get(), Ordering::Relaxed);
                }
            }
        }

        // Close button minimizes to tray when available.
        if ctx.input(|i| i.viewport().close_requested()) && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            tracing::info!("已最小化到系统托盘（托盘菜单可恢复或退出）");
        }

        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("mini-ftpd").size(17.0).strong());
                    ui.separator();
                    match &self.server {
                        Some(s) => {
                            ui.colored_label(egui::Color32::from_rgb(0x3f, 0xb9, 0x50), "●");
                            ui.label("运行中");
                            ui.label(egui::RichText::new(format!("{}", s.addr)).monospace());
                            ui.separator();
                            ui.label(format!("连接数 {}", s.active.load(Ordering::Relaxed)));
                        }
                        None => {
                            ui.colored_label(egui::Color32::GRAY, "●");
                            ui.label(egui::RichText::new("已停止").weak());
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let running = self.server.is_some();
                        let (text, fill) = if running {
                            ("停止服务器", egui::Color32::from_rgb(0x6b, 0x2d, 0x2d))
                        } else {
                            ("启动服务器", egui::Color32::from_rgb(0x24, 0x53, 0x31))
                        };
                        let button = egui::Button::new(
                            egui::RichText::new(text).color(egui::Color32::WHITE),
                        )
                        .fill(fill);
                        if ui.add_sized([96.0, 26.0], button).clicked() {
                            if running {
                                self.stop_server();
                            } else {
                                self.start_server();
                            }
                        }
                        ui.separator();
                        let logs_text = if self.show_logs {
                            "隐藏日志"
                        } else {
                            "显示日志"
                        };
                        if ui.button(logs_text).clicked() {
                            self.show_logs = !self.show_logs;
                        }
                        let theme_text = if self.dark_mode {
                            "亮色模式"
                        } else {
                            "暗黑模式"
                        };
                        if ui.button(theme_text).clicked() {
                            self.dark_mode = !self.dark_mode;
                            apply_theme(ctx, self.dark_mode);
                        }
                    });
                });
            });

        if self.show_logs {
            egui::TopBottomPanel::bottom("logs")
                .resizable(true)
                .default_height(170.0)
                .frame(
                    egui::Frame::side_top_panel(&ctx.style())
                        .inner_margin(egui::Margin::symmetric(14, 8)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("日志").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("清空").clicked() {
                                self.logs.clear();
                            }
                        });
                    });
                    ui.add_space(4.0);
                    egui::Frame::dark_canvas(ui.style())
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut self.logs.text())
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(f32::INFINITY)
                                            .interactive(false),
                                    );
                                });
                        });
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&self.status_msg).weak());
                        ui.add_space(6.0);

                        ui.columns(2, |cols| {
                            egui::Frame::group(cols[0].style())
                                .inner_margin(egui::Margin::same(12))
                                .show(&mut cols[0], |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "用户与权限（{}）",
                                            self.config.users.len()
                                        ))
                                        .size(15.0)
                                        .strong(),
                                    );
                                    ui.add_space(6.0);
                                    self.users_ui(ui);
                                });

                            egui::Frame::group(cols[1].style())
                                .inner_margin(egui::Margin::same(12))
                                .show(&mut cols[1], |ui| {
                                    ui.label(egui::RichText::new("服务器设置").size(15.0).strong());
                                    ui.add_space(6.0);
                                    self.server_settings_ui(ui);
                                });
                        });
                        ui.add_space(4.0);
                    });
            });
    }
}

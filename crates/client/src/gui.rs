//! SuperControl 图形界面：发现局域网里的电脑、配对、一键连接、设置。
//!
//! 远程画面窗口在独立进程里运行（`rc-client view <地址>`）：每个连接互不影响，
//! 画面窗口崩溃也不会带走主界面。

use crate::ui_kit::{self as kit, Toggle};
use crate::{awdl, config, net};
use anyhow::Result;
use eframe::egui::{self, Color32, Margin, RichText, Sense, Vec2};
use rc_protocol::{self as proto, DeviceId};
use std::collections::HashMap;
use std::io::BufRead;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ICON_PNG: &[u8] = include_bytes!("../../../packaging/icon/icon-256.png");

/// `background`：开机自启时只在托盘 / 菜单栏里启动，不显示主窗口。
pub fn run(background: bool) -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("SuperControl")
        // 自测截图时可用 RC_GUI_HEIGHT 把窗口拉高，看到整页
        .with_inner_size([900.0, std::env::var("RC_GUI_HEIGHT").ok().and_then(|h| h.parse().ok()).unwrap_or(620.0)])
        .with_min_inner_size([680.0, 480.0])
        // 开机自启：窗口一开始就是隐藏的（macOS 上 eframe 不会再把它显示出来）
        .with_visible(!background);
    // Windows 上 eframe 画完第一帧总会把窗口显示出来，先放到屏幕外，第二帧再隐藏
    if cfg!(windows) && background {
        viewport = viewport.with_position(OFFSCREEN);
    }
    if let Some((rgba, w, h)) = decode_png(ICON_PNG) {
        viewport = viewport.with_icon(Arc::new(egui::IconData {
            rgba,
            width: w,
            height: h,
        }));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "SuperControl",
        options,
        Box::new(move |cc| Ok(Box::new(Gui::new(&cc.egui_ctx, background)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// 后台启动时 Windows 主窗口先放在这里（看不见），隐藏后再挪回屏幕中间。
const OFFSCREEN: [f32; 2] = [-20000.0, -20000.0];

fn decode_png(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    (info.color_type == png::ColorType::Rgba).then_some((buf, info.width, info.height))
}

/// 本机在局域网里的 IPv4 地址（显示给用户，方便对方手动连接）。跳过代理软件的虚拟网卡（198.18.x.x）。
fn local_ip() -> Option<String> {
    let mut best: Option<(u8, std::net::Ipv4Addr)> = None;
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        let std::net::IpAddr::V4(ip) = iface.ip() else {
            continue;
        };
        let o = ip.octets();
        if ip.is_loopback() || ip.is_link_local() || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) {
            continue;
        }
        let rank = if o[0] == 192 && o[1] == 168 {
            3
        } else if ip.is_private() {
            2
        } else {
            1
        };
        if best.is_none_or(|(r, _)| rank > r) {
            best = Some((rank, ip));
        }
    }
    best.map(|(_, ip)| ip.to_string())
}

/// 后台扫描到的被控端（含最后一次出现的时间）。
struct Seen {
    host: net::Discovered,
    at: Instant,
}

/// 一次“连接”操作的进度（在后台线程里更新）。
enum Job {
    Connecting { name: String },
    Pairing { name: String, code: String },
    Failed { name: String, error: String },
    Started(Session),
}

/// 一个正在运行的远程画面窗口进程。
struct Session {
    name: String,
    /// 对方的设备 ID（手动输入地址首次配对时可能不知道）
    host_id: Option<DeviceId>,
    addr: SocketAddr,
    child: Child,
    /// 子进程 stderr 的最后几行（出错时显示）。
    stderr: Arc<Mutex<Vec<String>>>,
    /// 画面窗口已和对方完成握手（它在 stdout 打印“已连接…”时置位）。
    ready: Arc<AtomicBool>,
    /// 远端的显示器名称与当前下标（画面窗口在 stdout 打印 `RC-DISPLAYS` 时更新）
    displays: Arc<Mutex<(Vec<String>, u32)>>,
    /// 画面窗口的标准输入：发 `display <下标>` 切换远端显示器
    stdin: Option<std::process::ChildStdin>,
}

impl Session {
    /// 子进程 stderr 里最能说明问题的一行。
    fn last_error(&self) -> String {
        let lines = self.stderr.lock().unwrap();
        let line = lines.iter().rev().find(|l| l.starts_with("Error")).or(lines.last()).cloned().unwrap_or_default();
        line.trim_start_matches("Error: ").to_string()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Devices,
    Settings,
}

/// 设备卡片显示的内容。
struct CardInfo {
    name: String,
    os: String,
    addr: String,
    online: bool,
    paired: bool,
    compatible: bool,
    /// 点“连接”时用的参数
    target: Option<(Option<DeviceId>, SocketAddr, Option<String>)>,
    /// 点“忘记”时删除的配对记录
    forget_id: Option<String>,
    /// 已经在控制这台电脑（连接按钮不可点）
    connected: bool,
    /// 离线且知道网卡地址时可以唤醒：(设备 ID, 网卡地址, 上次的 IP)
    wake: Option<(String, Vec<[u8; 6]>, Option<std::net::IpAddr>)>,
    /// 刚发过唤醒包，正在等它上线
    waking: bool,
}

struct Gui {
    page: Page,
    logo: Option<egui::TextureHandle>,
    device_name: String,
    local_ip: Option<String>,
    ip_checked: Instant,
    seen: Arc<Mutex<HashMap<DeviceId, Seen>>>,
    manual_addr: String,
    settings: config::Settings,
    awdl_guard: bool,
    awdl_active: bool,
    /// AWDL 守护是旧版（本机被控制时不生效），需要重新安装
    awdl_outdated: bool,
    awdl_checked: Instant,
    awdl_job: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// 发过唤醒包、正在等待上线的设备：设备 ID → (名称, 开始时间)
    waking: HashMap<String, (String, Instant)>,
    /// 正在控制本机的设备名称（后台每秒向本机被控端查询）
    controller: Arc<Mutex<Option<String>>>,
    /// 本机是否装了虚拟声卡（后台检测，None 表示还没检测完）
    cable: Arc<Mutex<Option<bool>>>,
    cable_job: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// 被控端在后台线程里维护（窗口隐藏时界面不刷新，不能靠界面驱动）
    host: Arc<Mutex<HostRunner>>,
    /// “允许其他设备控制”开关；托盘菜单直接改它，界面每帧同步到 settings.host
    host_want: Arc<AtomicBool>,
    job: Option<Arc<Mutex<Job>>>,
    sessions: Vec<Session>,
    /// 底部提示：(内容, 是否错误, 出现时间)
    message: Option<(String, bool, Instant)>,
    screenshot: Option<(String, Instant)>,
    /// 托盘 / 菜单栏图标（创建失败时为 None，关闭窗口就直接退出）
    tray: Option<crate::tray::Tray>,
    tray_rx: std::sync::mpsc::Receiver<crate::tray::TrayCmd>,
    /// 主窗口当前隐藏在托盘里
    hidden: bool,
    /// 用户选择了退出（不再把关闭窗口当作隐藏）
    quitting: bool,
    /// Windows 后台启动：窗口还在屏幕外，等第二帧隐藏，下次显示时挪回屏幕中间
    parked: bool,
    frames: u32,
    /// 开机自动启动（读自系统的启动项）
    autostart: bool,
}

impl Gui {
    fn new(ctx: &egui::Context, background: bool) -> Self {
        kit::apply_theme(ctx);
        kit::load_system_font(ctx);
        let settings = config::settings();
        let (tray_tx, tray_rx) = std::sync::mpsc::channel();
        let host = Arc::new(Mutex::new(HostRunner::new()));
        let host_want = Arc::new(AtomicBool::new(settings.host));
        spawn_host_keeper(host.clone(), host_want.clone());
        let on_toggle = {
            let (host, want) = (host.clone(), host_want.clone());
            move || {
                // 菜单项已自己切换了勾选，这里跟着翻转并立刻保存、生效
                let on = !want.fetch_xor(true, Ordering::SeqCst);
                let mut s = config::settings();
                s.host = on;
                let _ = config::save_settings(&s);
                let mut h = host.lock().unwrap();
                h.error = None;
                h.next_try = Instant::now();
            }
        };
        let tray = decode_png(ICON_PNG).and_then(|(rgba, _, _)| {
            crate::tray::Tray::new(&rgba, settings.host, ctx.clone(), tray_tx, on_toggle)
                .map_err(|e| eprintln!("创建托盘图标失败: {e:#}"))
                .ok()
        });
        let seen = Arc::new(Mutex::new(HashMap::new()));
        spawn_scanner(seen.clone(), ctx.clone());
        if background {
            if tray.is_some() {
                crate::tray::set_dock_visible(false);
            } else {
                // 托盘图标没建起来就没有入口可以打开窗口，只好直接显示
                crate::tray::native_show();
            }
        }
        let logo = decode_png(ICON_PNG).map(|(rgba, w, h)| {
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            ctx.load_texture("logo", img, egui::TextureOptions::LINEAR)
        });
        Self {
            page: match std::env::var("RC_GUI_PAGE").as_deref() {
                Ok("settings") => Page::Settings,
                _ => Page::Devices,
            },
            logo,
            device_name: config::device_name(),
            local_ip: local_ip(),
            ip_checked: Instant::now(),
            seen,
            manual_addr: String::new(),
            settings,
            awdl_guard: awdl::enabled(),
            awdl_active: awdl::active(),
            awdl_outdated: awdl::guard_installed() && awdl::guard_outdated(),
            awdl_checked: Instant::now(),
            awdl_job: None,
            waking: HashMap::new(),
            cable: {
                let c: Arc<Mutex<Option<bool>>> = Arc::default();
                let c2 = c.clone();
                std::thread::spawn(move || *c2.lock().unwrap() = Some(rc_audio::virtual_cable().is_some()));
                c
            },
            cable_job: None,
            controller: {
                let c: Arc<Mutex<Option<String>>> = Arc::default();
                let (c2, ctx) = (c.clone(), ctx.clone());
                std::thread::spawn(move || loop {
                    let now = net::local_status().ok().flatten();
                    let mut cur = c2.lock().unwrap();
                    if *cur != now {
                        *cur = now;
                        ctx.request_repaint();
                    }
                    drop(cur);
                    std::thread::sleep(Duration::from_secs(1));
                });
                c
            },
            host,
            host_want,
            job: None,
            sessions: Vec::new(),
            message: None,
            // 自测：RC_GUI_SCREENSHOT=/路径.png 时，启动 2 秒后截取本窗口并退出
            screenshot: std::env::var("RC_GUI_SCREENSHOT")
                .ok()
                .map(|p| (p, Instant::now())),
            tray,
            tray_rx,
            hidden: background,
            parked: cfg!(windows) && background,
            frames: 0,
            quitting: false,
            autostart: crate::autostart::enabled(),
        }
    }

    /// 隐藏到托盘 / 菜单栏（被控端照常运行）。
    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        crate::tray::set_dock_visible(false);
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        if std::mem::take(&mut self.parked) {
            // 从屏幕外挪回主显示器中间
            let (monitor, size) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().outer_rect.map(|r| r.size())));
            if let (Some(m), Some(sz)) = (monitor, size) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(((m - sz) / 2.0).to_pos2()));
            }
        }
        crate::tray::set_dock_visible(true);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn set_host_enabled(&mut self, on: bool) {
        self.settings.host = on;
        self.host_want.store(on, Ordering::SeqCst);
        let _ = config::save_settings(&self.settings);
        {
            let mut h = self.host.lock().unwrap();
            h.error = None;
            h.next_try = Instant::now();
        }
        if let Some(t) = &self.tray {
            t.set_host(on);
        }
    }

    /// 托盘菜单命令、关闭按钮、后台启动。
    fn window_lifecycle(&mut self, ctx: &egui::Context) {
        self.frames = self.frames.saturating_add(1);
        if self.parked && self.frames == 2 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                crate::tray::TrayCmd::Show => self.show_window(ctx),
                // 托盘回调里已经切换并保存过了，这里只同步界面里的设置
                crate::tray::TrayCmd::ToggleHost => self.settings.host = self.host_want.load(Ordering::SeqCst),
                crate::tray::TrayCmd::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.settings.close_to_tray
            && self.tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }
    }

    /// 唤醒的设备上线了就提示成功；等太久没上线就提示失败。
    fn poll_waking(&mut self) {
        if self.waking.is_empty() {
            return;
        }
        let online: Vec<String> = {
            let now = Instant::now();
            let seen = self.seen.lock().unwrap();
            seen.values().filter(|s| now - s.at < Duration::from_secs(8)).map(|s| proto::hex(&s.host.host_id)).collect()
        };
        let mut done = Vec::new();
        for (id, (name, at)) in &self.waking {
            if online.contains(id) {
                done.push((id.clone(), format!("「{name}」已唤醒，可以连接了"), false));
            } else if at.elapsed() > WAKE_TIMEOUT {
                done.push((
                    id.clone(),
                    format!("「{name}」没有响应：可能已关机、断开了网络，或网卡不支持网络唤醒"),
                    true,
                ));
            }
        }
        for (id, text, error) in done {
            self.waking.remove(&id);
            self.say(text, error);
        }
    }

    /// 发网络唤醒包：睡眠中的网卡可能漏收，前 20 秒里多发几次。
    fn wake(&mut self, ctx: &egui::Context, name: String, id: String, macs: Vec<[u8; 6]>, ip: Option<std::net::IpAddr>) {
        self.waking.insert(id, (name.clone(), Instant::now()));
        self.say(format!("已向「{name}」发送唤醒信号，正在等待它上线…"), false);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for delay in [0, 2, 3, 5, 10] {
                std::thread::sleep(Duration::from_secs(delay));
                if let Err(e) = net::wake(&macs, ip) {
                    eprintln!("发送唤醒包失败: {e:#}");
                }
            }
            ctx.request_repaint();
        });
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error, Instant::now()));
    }

    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| {
            matches!(
                *j.lock().unwrap(),
                Job::Connecting { .. } | Job::Pairing { .. }
            )
        })
    }

    /// 发起连接：未配对时先在本界面完成配对（显示配对码），然后启动远程画面窗口。
    fn connect(
        &mut self,
        ctx: &egui::Context,
        name: String,
        host_id: Option<DeviceId>,
        addr: SocketAddr,
        os: Option<String>,
    ) {
        self.message = None;
        let job = Arc::new(Mutex::new(Job::Connecting { name: name.clone() }));
        self.job = Some(job.clone());
        let settings = self.settings.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let set = |j: Job| {
                *job.lock().unwrap() = j;
                ctx.request_repaint();
            };
            let paired = host_id.is_some_and(|id| config::key_for(&id).is_some());
            if !paired {
                // 先完成配对（不打开画面），再由画面窗口直接连接。
                let result = net::pair(addr, |ev| match ev {
                    net::HandshakeEvent::PairCode { host_name, code } => set(Job::Pairing {
                        name: host_name.to_string(),
                        code: code.to_string(),
                    }),
                });
                if let Err(e) = result {
                    return set(Job::Failed {
                        name,
                        error: format!("{e:#}"),
                    });
                }
            }
            let mut s = match spawn_viewer(&name, addr, &settings, os.as_deref()) {
                Ok(s) => Session { host_id, ..s },
                Err(e) => {
                    return set(Job::Failed {
                        name,
                        error: format!("{e:#}"),
                    })
                }
            };
            // 画面窗口真正连上对方之后才算成功；连不上时它会带着错误退出。
            // 等待上限覆盖握手的最长超时（TCP 连接 5 秒 + 握手 15 秒）。
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                if s.ready.load(Ordering::Relaxed) {
                    return set(Job::Started(s));
                }
                if let Ok(Some(_)) = s.child.try_wait() {
                    std::thread::sleep(Duration::from_millis(100)); // 让 stderr 读完
                    let error = match s.last_error() {
                        e if e.is_empty() => "画面窗口意外退出".to_string(),
                        e => e,
                    };
                    return set(Job::Failed { name, error });
                }
                if Instant::now() > deadline {
                    let _ = s.child.kill();
                    let _ = s.child.wait();
                    return set(Job::Failed {
                        name,
                        error: "连接超时：对方可能已关机、睡眠或不在同一局域网".into(),
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }

    /// 处理后台任务的结果、回收已退出的画面窗口。
    fn poll(&mut self) {
        self.poll_waking();
        if let Some(job) = &self.job {
            let mut guard = job.lock().unwrap();
            match std::mem::replace(
                &mut *guard,
                Job::Connecting {
                    name: String::new(),
                },
            ) {
                Job::Started(s) => {
                    let text = format!("已打开「{}」的远程画面", s.name);
                    self.sessions.push(s);
                    drop(guard);
                    self.job = None;
                    self.say(text, false);
                }
                Job::Failed { name, error } => {
                    drop(guard);
                    self.job = None;
                    self.say(format!("连接「{name}」失败：{error}"), true);
                }
                other => *guard = other,
            }
        }

        let mut ended = Vec::new();
        self.sessions.retain_mut(|s| match s.child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let lines = s.stderr.lock().unwrap();
                    let err = lines
                        .iter()
                        .rev()
                        .find(|l| l.starts_with("Error"))
                        .or(lines.last())
                        .cloned()
                        .unwrap_or_default();
                    ended.push(format!(
                        "「{}」的连接已断开：{}",
                        s.name,
                        err.trim_start_matches("Error: ")
                    ));
                }
                false
            }
            _ => true,
        });
        if let Some(e) = ended.pop() {
            self.say(e, true);
        }

        if let Some(rx) = &self.awdl_job {
            if let Ok(r) = rx.try_recv() {
                self.awdl_job = None;
                self.awdl_guard = awdl::enabled();
                self.awdl_outdated = awdl::guard_installed() && awdl::guard_outdated();
                if let Err(e) = r {
                    self.say(format!("设置 AWDL 自动处理失败：{e}"), true);
                }
            }
        }
        if let Some(rx) = &self.cable_job {
            if let Ok(r) = rx.try_recv() {
                self.cable_job = None;
                match r {
                    Ok(()) => {
                        *self.cable.lock().unwrap() = Some(true);
                        self.say(format!("虚拟声卡 {} 已安装", rc_audio::install::NAME), false);
                    }
                    Err(e) => self.say(format!("安装虚拟声卡失败：{e}"), true),
                }
            }
        }
        if self.awdl_checked.elapsed() > Duration::from_secs(2) {
            self.awdl_checked = Instant::now();
            self.awdl_active = awdl::active();
            self.awdl_outdated = awdl::guard_installed() && awdl::guard_outdated();
        }
        if self.ip_checked.elapsed() > Duration::from_secs(10) {
            self.ip_checked = Instant::now();
            self.local_ip = local_ip();
        }
        // 成功提示几秒后自动消失，错误提示保留到用户关闭
        if self
            .message
            .as_ref()
            .is_some_and(|(_, err, at)| !err && at.elapsed() > Duration::from_secs(5))
        {
            self.message = None;
        }
    }
}

/// 后台线程：每 300 毫秒让被控端的运行状态跟开关一致（不依赖界面刷新）。
fn spawn_host_keeper(host: Arc<Mutex<HostRunner>>, want: Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        host.lock().unwrap().tick(want.load(Ordering::SeqCst));
        std::thread::sleep(Duration::from_millis(300));
    });
}

/// 被控端：界面打开期间运行，界面关闭后停止。两台电脑都打开本程序才能互相控制。
struct HostRunner {
    error: Option<String>,
    next_try: Instant,
    #[cfg(target_os = "macos")]
    child: Option<Child>,
    /// 系统权限（录屏, 辅助功能），每 2 秒刷新
    #[cfg(target_os = "macos")]
    perms: (bool, bool),
    #[cfg(target_os = "macos")]
    perms_checked: Instant,
    #[cfg(windows)]
    state: Option<u32>,
    #[cfg(windows)]
    checked: Instant,
}

impl HostRunner {
    fn new() -> Self {
        #[cfg(target_os = "macos")]
        crate::hostagent::remove_legacy_agent();
        #[cfg(windows)]
        crate::winhost::hold_ui_mutex();
        Self {
            error: None,
            next_try: Instant::now(),
            #[cfg(target_os = "macos")]
            child: None,
            #[cfg(target_os = "macos")]
            perms: crate::hostagent::permissions(),
            #[cfg(target_os = "macos")]
            perms_checked: Instant::now(),
            #[cfg(windows)]
            state: crate::winhost::service_state(),
            #[cfg(windows)]
            checked: Instant::now(),
        }
    }

    fn retry_later(&mut self, e: anyhow::Error) {
        self.error = Some(format!("{e:#}"));
        self.next_try = Instant::now() + Duration::from_secs(10);
    }

    /// 让被控端的运行状态跟开关一致。
    #[cfg(target_os = "macos")]
    fn tick(&mut self, want: bool) {
        // 用户刚授予了权限：重启被控端让它生效，不用重新打开本程序
        if self.perms_checked.elapsed() > Duration::from_secs(2) {
            self.perms_checked = Instant::now();
            let now = crate::hostagent::permissions();
            let granted = (now.0 && !self.perms.0) || (now.1 && !self.perms.1);
            self.perms = now;
            if granted && self.child.is_some() {
                self.stop();
                self.next_try = Instant::now();
            }
        }
        if let Some(c) = &mut self.child {
            if !matches!(c.try_wait(), Ok(None)) {
                self.child = None;
                self.error = Some("被控端已退出，稍后自动重启（详见日志）".into());
                self.next_try = Instant::now() + Duration::from_secs(5);
            }
        }
        if want && self.child.is_none() && Instant::now() >= self.next_try {
            match crate::hostagent::spawn() {
                Ok(c) => {
                    self.child = Some(c);
                    self.error = None;
                }
                Err(e) => self.retry_later(e),
            }
        }
        if !want {
            self.stop();
        }
    }

    #[cfg(target_os = "macos")]
    fn running(&self) -> bool {
        self.child.is_some()
    }

    #[cfg(target_os = "macos")]
    fn installed(&self) -> bool {
        true
    }

    #[cfg(target_os = "macos")]
    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    #[cfg(windows)]
    fn tick(&mut self, want: bool) {
        use crate::winhost;
        if self.checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.checked = Instant::now();
        self.state = winhost::service_state();
        let Some(state) = self.state else { return };
        let running = matches!(state, 2 | 4);
        if want != running && state != 3 && Instant::now() >= self.next_try {
            match winhost::control(want) {
                Ok(()) => self.error = None,
                Err(e) => self.retry_later(e),
            }
        }
    }

    #[cfg(windows)]
    fn running(&self) -> bool {
        self.state == Some(4)
    }

    #[cfg(windows)]
    fn installed(&self) -> bool {
        self.state.is_some()
    }

    #[cfg(windows)]
    fn stop(&mut self) {
        if matches!(self.state, Some(2 | 4)) {
            let _ = crate::winhost::control(false);
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn tick(&mut self, _want: bool) {}
    #[cfg(not(any(windows, target_os = "macos")))]
    fn running(&self) -> bool {
        false
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    fn installed(&self) -> bool {
        false
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    fn stop(&mut self) {}
}

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

impl Gui {
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(
                    egui::Image::new(logo)
                        .fit_to_exact_size(Vec2::splat(34.0))
                        .corner_radius(8),
                );
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    RichText::new("SuperControl")
                        .size(17.0)
                        .strong()
                        .color(kit::TEXT),
                );
                ui.label(
                    RichText::new("局域网远程控制")
                        .size(11.5)
                        .color(kit::TEXT_WEAK),
                );
            });
        });
        ui.add_space(26.0);
        self.nav_item(ui, Page::Devices, "设备", kit::icon_monitor);
        self.nav_item(ui, Page::Settings, "设置", kit::icon_gear);
    }

    /// 侧边栏底部：本机信息。
    fn local_card(&mut self, ui: &mut egui::Ui) {
        let mut disconnected = None;
        {
            egui::Frame::new()
                .fill(Color32::WHITE)
                .corner_radius(12)
                .inner_margin(Margin::same(12))
                .stroke((1.0, kit::BORDER))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(RichText::new("本机").size(11.5).color(kit::TEXT_FAINT));
                        ui.add(
                            egui::Label::new(
                                RichText::new(&self.device_name)
                                    .size(14.0)
                                    .strong()
                                    .color(kit::TEXT),
                            )
                            .truncate(),
                        );
                        if let Some(ip) = &self.local_ip {
                            ui.label(RichText::new(ip).size(12.0).color(kit::TEXT_WEAK));
                        }
                        ui.add_space(4.0);
                        let controller = self.controller.lock().unwrap().clone();
                        match controller {
                            Some(name) => {
                                kit::chip(ui, "正在被控制", kit::ORANGE);
                                ui.add(egui::Label::new(RichText::new(&name).size(12.0).color(kit::TEXT)).truncate())
                                    .on_hover_text(format!("「{name}」正在控制这台电脑"));
                                ui.add_space(2.0);
                                if kit::secondary_button(ui, "断开").on_hover_text("断开对方的远程控制").clicked() {
                                    *self.controller.lock().unwrap() = None;
                                    std::thread::spawn(|| {
                                        if let Err(e) = net::local_disconnect() {
                                            eprintln!("断开失败: {e:#}");
                                        }
                                    });
                                    disconnected = Some(name);
                                }
                            }
                            None => {
                                let (text, dot) = self.host_status();
                                kit::chip(ui, text, dot);
                            }
                        }
                    });
                });
        }
        if let Some(name) = disconnected {
            self.say(format!("已断开「{name}」的远程控制"), false);
        }
    }

    /// 被控端状态：(文字, 圆点颜色)
    fn host_status(&self) -> (&'static str, Color32) {
        let h = self.host.lock().unwrap();
        if !self.settings.host {
            ("不允许被控制", kit::GRAY)
        } else if h.running() {
            ("可被控制", kit::GREEN)
        } else if h.error.is_some() || !h.installed() {
            ("被控端未就绪", kit::ORANGE)
        } else {
            ("正在启动…", kit::GRAY)
        }
    }

    fn nav_item(
        &mut self,
        ui: &mut egui::Ui,
        page: Page,
        label: &str,
        icon: fn(&egui::Painter, egui::Rect, Color32),
    ) {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
        let selected = self.page == page;
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, 10, Color32::WHITE);
            p.rect_stroke(rect, 10, (1.0, kit::BORDER), egui::StrokeKind::Inside);
        } else if resp.hovered() {
            p.rect_filled(rect, 10, Color32::from_rgb(0xE2, 0xE2, 0xE6));
        }
        let color = if selected {
            kit::TEXT
        } else {
            kit::TEXT_WEAK
        };
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 22.0, rect.center().y),
            Vec2::splat(18.0),
        );
        icon(p, icon_rect, color);
        p.text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.5),
            if selected { kit::TEXT } else { kit::TEXT_WEAK },
        );
        if resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            self.page = page;
        }
        ui.add_space(2.0);
    }

    fn page_header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
        ui.label(RichText::new(title).size(24.0).strong().color(kit::TEXT));
        ui.label(RichText::new(subtitle).size(13.0).color(kit::TEXT_WEAK));
    }

    fn section_label(ui: &mut egui::Ui, text: &str) {
        ui.add_space(6.0);
        ui.label(
            RichText::new(text)
                .size(13.0)
                .strong()
                .color(kit::TEXT_WEAK),
        );
        ui.add_space(2.0);
    }

    fn devices_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                Self::page_header(
                    ui,
                    "设备",
                    "同一局域网中打开了 SuperControl 的电脑会自动出现在这里",
                )
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let go = kit::secondary_button(ui, "连接").clicked();
                let edit = egui::TextEdit::singleline(&mut self.manual_addr)
                    .hint_text("输入对方 IP 地址")
                    .desired_width(160.0)
                    .margin(Margin::symmetric(10, 7));
                let resp = ui.add(edit);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (go || enter) && !self.busy() && !self.manual_addr.trim().is_empty() {
                    match net::parse_addr(self.manual_addr.trim()) {
                        Ok(addr) => {
                            let name = self.manual_addr.trim().to_string();
                            self.connect(&ctx, name, None, addr, None);
                        }
                        Err(e) => self.say(format!("地址无效：{e:#}"), true),
                    }
                }
            });
        });
        ui.add_space(14.0);

        // 正在控制的连接
        if !self.sessions.is_empty() {
            Self::section_label(ui, "正在控制");
            let mut kill = None;
            let mut select = None;
            for (i, s) in self.sessions.iter().enumerate() {
                kit::card()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            kit::chip(ui, "已连接", kit::GREEN);
                            ui.label(RichText::new(&s.name).size(14.5).strong().color(kit::TEXT));
                            ui.label(
                                RichText::new("远程画面已在独立窗口中打开")
                                    .size(12.0)
                                    .color(kit::TEXT_WEAK),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if kit::secondary_button(ui, "断开").clicked() {
                                        kill = Some(i);
                                    }
                                },
                            );
                        });
                        // 远端有多块显示器：在这里切换（画面窗口里也可以用快捷键）
                        let (names, current) = s.displays.lock().unwrap().clone();
                        if names.len() > 1 {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("显示器").size(12.5).color(kit::TEXT_WEAK));
                                for (d, name) in names.iter().enumerate() {
                                    if kit::segment(ui, name, d as u32 == current).clicked() && d as u32 != current {
                                        select = Some((i, d));
                                    }
                                }
                            });
                        }
                    });
            }
            if let Some((i, d)) = select {
                if let Some(stdin) = self.sessions[i].stdin.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(stdin, "display {d}").and_then(|_| stdin.flush());
                }
            }
            if let Some(i) = kill {
                let mut s = self.sessions.remove(i);
                let _ = s.child.kill();
                let _ = s.child.wait();
            }
            ui.add_space(8.0);
        }

        let cards = self.device_cards();
        let online = cards.iter().filter(|c| c.online).count();
        Self::section_label(ui, &format!("局域网中的设备 · {online} 台在线"));
        if cards.is_empty() {
            Self::empty_state(ui);
            return;
        }

        let gap = 14.0;
        let width = ui.available_width();
        let cols = (((width + gap) / (260.0 + gap)).floor() as usize).clamp(1, 3);
        let card_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
        let busy = self.busy();
        let mut action = None;
        let mut forget = None;
        let mut wake = None;
        for row in cards.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for c in row {
                    ui.allocate_ui_with_layout(
                        Vec2::new(card_w, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let (connect, forget_it, wake_it) = device_card(ui, c, card_w, busy);
                            if wake_it {
                                if let Some((id, macs, ip)) = &c.wake {
                                    wake = Some((c.name.clone(), id.clone(), macs.clone(), *ip));
                                }
                            }
                            if connect {
                                if let Some((id, addr, os)) = &c.target {
                                    action = Some((c.name.clone(), *id, *addr, os.clone()));
                                }
                            }
                            if forget_it {
                                forget = c.forget_id.clone();
                            }
                        },
                    );
                }
            });
            ui.add_space(gap - 8.0);
        }
        if let Some((name, id, addr, os)) = action {
            self.connect(&ctx, name, id, addr, os);
        }
        if let Some((name, id, macs, ip)) = wake {
            self.wake(&ctx, name, id, macs, ip);
        }
        if let Some(id) = forget {
            let _ = config::forget(&id);
            self.say("已删除配对信息，下次连接需要重新配对", false);
        }
    }

    /// 是否已经有画面窗口在控制这台设备：有设备 ID 时按 ID 比较，否则按 IP。
    fn is_connected(&self, id: Option<DeviceId>, addr: SocketAddr) -> bool {
        self.sessions.iter().any(|s| match (s.host_id, id) {
            (Some(a), Some(b)) => a == b,
            _ => s.addr.ip() == addr.ip(),
        })
    }

    /// 在线设备 + 配对过但当前不在线的设备。
    fn device_cards(&self) -> Vec<CardInfo> {
        let now = Instant::now();
        let mut online: Vec<net::Discovered> = {
            let seen = self.seen.lock().unwrap();
            seen.values()
                .filter(|s| now - s.at < Duration::from_secs(8))
                .map(|s| s.host.clone())
                .collect()
        };
        online.sort_by(|a, b| a.name.cmp(&b.name));
        let mut cards: Vec<CardInfo> = online
            .iter()
            .map(|d| CardInfo {
                name: d.name.clone(),
                os: d.os.clone(),
                addr: d.addr.ip().to_string(),
                online: true,
                paired: d.paired,
                compatible: d.version == proto::PROTOCOL_VERSION,
                target: Some((Some(d.host_id), d.addr, Some(d.os.clone()))),
                forget_id: d.paired.then(|| proto::hex(&d.host_id)),
                connected: self.is_connected(Some(d.host_id), d.addr),
                wake: None,
                waking: false,
            })
            .collect();
        for k in config::known_hosts() {
            if online.iter().any(|d| proto::hex(&d.host_id) == k.id) {
                continue;
            }
            let addr = k.last_addr.clone().unwrap_or_default();
            let parsed = addr.parse::<SocketAddr>().ok();
            let id = proto::parse_hex16(&k.id);
            cards.push(CardInfo {
                name: k.name.clone(),
                os: String::new(),
                addr: parsed.map(|a| a.ip().to_string()).unwrap_or(addr),
                online: false,
                paired: true,
                compatible: true,
                target: parsed.map(|a| (proto::parse_hex16(&k.id), a, None)),
                forget_id: Some(k.id.clone()),
                connected: parsed.is_some_and(|a| self.is_connected(id, a)),
                wake: {
                    let macs: Vec<[u8; 6]> = k.macs.iter().filter_map(|m| proto::discovery::parse_mac(m)).collect();
                    (!macs.is_empty()).then(|| (k.id.clone(), macs, parsed.map(|a| a.ip())))
                },
                waking: self.waking.contains_key(&k.id),
            });
        }
        cards
    }

    fn empty_state(ui: &mut egui::Ui) {
        kit::card()
            .inner_margin(Margin::symmetric(24, 32))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                    ui.painter().rect_filled(rect, 18, kit::FILL);
                    kit::icon_monitor(ui.painter(), rect.shrink(14.0), kit::TEXT);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let w = 220.0;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        ui.spinner();
                        ui.label(
                            RichText::new("正在寻找局域网中的设备…")
                                .size(15.0)
                                .strong()
                                .color(kit::TEXT),
                        );
                    });
                    ui.add_space(4.0);
                    for line in [
                        "在另一台电脑上也打开 SuperControl",
                        "两台电脑需要连接同一个路由器（局域网）",
                        "找不到时，可以在右上角直接输入对方的 IP 地址",
                    ] {
                        ui.label(RichText::new(line).size(12.5).color(kit::TEXT_WEAK));
                    }
                });
            });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "设置", "调整控制方式、被控制选项和网络");
        ui.add_space(14.0);

        // ---- 通用 ----
        Self::section_label(ui, "通用");
        kit::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut auto = self.autostart;
            kit::setting_row(ui, "开机时自动启动", "登录后在后台启动 SuperControl（不弹出窗口），保持可被控制", 60.0, |ui| {
                if ui.add(Toggle(&mut auto)).changed() {
                    match crate::autostart::set(auto) {
                        Ok(()) => self.autostart = auto,
                        Err(e) => self.message = Some((format!("{e:#}"), true, Instant::now())),
                    }
                }
            });
            kit::divider(ui);
            let place = if cfg!(target_os = "macos") { "菜单栏" } else { "任务栏托盘" };
            let mut to_tray = self.settings.close_to_tray;
            kit::setting_row(
                ui,
                &format!("关闭窗口时隐藏到{place}"),
                &format!("被控端继续运行，点{place}里的图标可以重新打开；从图标菜单里选“退出”才会完全退出"),
                60.0,
                |ui| {
                    if ui.add(Toggle(&mut to_tray)).changed() {
                        self.settings.close_to_tray = to_tray;
                        let _ = config::save_settings(&self.settings);
                    }
                },
            );
        });
        ui.add_space(10.0);

        // ---- 控制其他电脑 ----
        Self::section_label(ui, "控制其他电脑");
        let mut changed = false;
        kit::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (title, desc) = if cfg!(target_os = "macos") {
                ("Command 键当作 Ctrl", "⌘C / ⌘V 即可在 Windows 上复制粘贴")
            } else {
                (
                    "控制 Mac 时 Ctrl 当作 Command",
                    "Ctrl+C / Ctrl+V 即可在 Mac 上复制粘贴",
                )
            };
            kit::setting_row(ui, title, desc, 60.0, |ui| {
                changed |= ui.add(Toggle(&mut self.settings.cmd_as_ctrl)).changed()
            });
            kit::divider(ui);
            let (keys_desc, release) = if cfg!(target_os = "macos") {
                ("⌘Tab、⌃←/→ 切桌面、⌃↑ 调度中心、⌘空格 等在远程窗口里发给对方，并换成对方系统的对应快捷键", "⌃⌥⌘")
            } else {
                ("Alt+Tab、Ctrl+Win+←/→ 切桌面、Win+Tab、Win+D、Win 键 等在远程窗口里发给对方，并换成对方系统的对应快捷键", "Ctrl+Alt+Win")
            };
            kit::setting_row(
                ui,
                "把系统快捷键发给对方",
                &format!("{keys_desc}。临时交还本机：同时按住 {release}，点一下远程画面恢复"),
                60.0,
                |ui| changed |= ui.add(Toggle(&mut self.settings.system_keys)).changed(),
            );
            kit::divider(ui);
            kit::setting_row(
                ui,
                "播放远端的声音",
                "对方电脑的声音在本机播放",
                60.0,
                |ui| changed |= ui.add(Toggle(&mut self.settings.audio)).changed(),
            );
            kit::divider(ui);
            kit::setting_row(
                ui,
                "控制时让对方静音",
                "对方电脑的扬声器在连接期间静音，断开后恢复；声音照样传到本机",
                60.0,
                |ui| changed |= ui.add(Toggle(&mut self.settings.mute_host)).changed(),
            );
            kit::divider(ui);
            kit::setting_row(
                ui,
                "把本机麦克风传给对方",
                "对方的会议、语音软件可以把它当麦克风用。对方需要虚拟声卡，没有的话连接时会在对方电脑上询问是否一键安装",
                60.0,
                |ui| changed |= ui.add(Toggle(&mut self.settings.mic)).changed(),
            );
            kit::divider(ui);
            kit::setting_row(
                ui,
                "按原始分辨率传输",
                "默认按窗口大小编码：看起来一样清晰，延迟和带宽更低",
                60.0,
                |ui| changed |= ui.add(Toggle(&mut self.settings.native)).changed(),
            );
        });
        if changed {
            let _ = config::save_settings(&self.settings);
        }

        // ---- 被控制 ----
        ui.add_space(10.0);
        Self::section_label(ui, "被控制");
        kit::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut on = self.settings.host;
            let (text, dot) = self.host_status();
            kit::setting_row(ui, "允许其他设备控制这台电脑", "只在 SuperControl 打开期间可被控制；新设备第一次连接时，需要在这台电脑上确认配对码", 200.0, |ui| {
                if ui.add(Toggle(&mut on)).changed() {
                    self.set_host_enabled(on);
                }
                kit::chip(ui, text, dot);
            });
            self.host_platform_ui(ui);
            self.cable_row(ui);
            if let Some(e) = self.host.lock().unwrap().error.clone() {
                ui.add_space(4.0);
                ui.label(RichText::new(e).size(12.0).color(kit::ORANGE));
            }
        });

        // ---- 网络（macOS） ----
        if cfg!(target_os = "macos") {
            ui.add_space(10.0);
            Self::section_label(ui, "网络");
            kit::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut guard = self.awdl_guard;
                let pending = self.awdl_job.is_some();
                kit::setting_row(
                    ui,
                    "远程期间自动关闭 AWDL",
                    "AWDL 是隔空投送、接力使用的无线通道，会让 Wi-Fi 周期性卡顿几十毫秒。只在本机控制别人或被控制期间关闭；第一次开启需要输入一次管理员密码",
                    200.0,
                    |ui| {
                        let resp = ui.add_enabled(!pending, Toggle(&mut guard));
                        if pending {
                            ui.spinner();
                        }
                        if resp.changed() {
                            let (tx, rx) = std::sync::mpsc::channel();
                            self.awdl_job = Some(rx);
                            std::thread::spawn(move || {
                                let _ = tx.send(awdl::set_enabled(guard).map_err(|e| format!("{e:#}")));
                            });
                        }
                        if guard && self.awdl_outdated && !pending {
                            // 旧版守护只在本机控制别人时生效；更新后本机被控制时也会关闭 AWDL
                            if kit::secondary_button(ui, "更新")
                                .on_hover_text("当前版本只在本机控制别人时关闭 AWDL；更新后本机被控制时也会关闭（需要输入一次管理员密码）")
                                .clicked()
                            {
                                let (tx, rx) = std::sync::mpsc::channel();
                                self.awdl_job = Some(rx);
                                std::thread::spawn(move || {
                                    let _ = tx.send(awdl::set_enabled(true).map_err(|e| format!("{e:#}")));
                                });
                            }
                        }
                        if self.awdl_active {
                            kit::chip(ui, "AWDL 开启中", kit::ORANGE);
                        } else {
                            kit::chip(ui, "AWDL 已关闭", kit::GREEN);
                        }
                    },
                );
            });
        }

        ui.add_space(16.0);
        ui.label(
            RichText::new(format!(
                "SuperControl {} · 协议版本 {}",
                env!("CARGO_PKG_VERSION"),
                proto::PROTOCOL_VERSION
            ))
            .size(12.0)
            .color(kit::TEXT_FAINT),
        );
    }

    /// 虚拟声卡：对方把麦克风传过来时，本机软件从这里当麦克风收到声音。
    fn cable_row(&mut self, ui: &mut egui::Ui) {
        kit::divider(ui);
        let mic_name = if cfg!(windows) { "CABLE Output" } else { "BlackHole 2ch" };
        let desc = format!(
            "对方开启“把本机麦克风传给对方”后，这台电脑上的会议、语音软件把麦克风选成“{mic_name}”即可。没装的话，对方连上时也会在这里询问"
        );
        let installed = *self.cable.lock().unwrap();
        let busy = self.cable_job.is_some();
        kit::setting_row(ui, &format!("虚拟声卡 {}", rc_audio::install::NAME), &desc, 200.0, |ui| {
            if busy {
                ui.spinner();
                kit::chip(ui, "正在安装…", kit::GRAY);
            } else {
                match installed {
                    Some(true) => kit::chip(ui, "已安装", kit::GREEN),
                    Some(false) => {
                        if kit::primary_button(ui, "安装", true).clicked() {
                            let (tx, rx) = std::sync::mpsc::channel();
                            self.cable_job = Some(rx);
                            std::thread::spawn(move || {
                                let _ = tx.send(rc_audio::install::install(false).map_err(|e| format!("{e:#}")));
                            });
                        }
                        kit::chip(ui, "未安装", kit::GRAY)
                    }
                    None => kit::chip(ui, "检测中…", kit::GRAY),
                }
            }
        });
    }

    #[cfg(windows)]
    fn host_platform_ui(&mut self, ui: &mut egui::Ui) {
        use crate::winhost;
        kit::divider(ui);
        let state = self.host.lock().unwrap().state;
        let installed = state.is_some();
        let desc = if installed {
            "已安装，可以控制锁屏、登录界面和 UAC 弹窗"
        } else {
            "需要安装一次（会请求管理员权限）。安装后可以控制锁屏、登录界面和 UAC 弹窗"
        };
        let mut result = Ok(());
        kit::setting_row(ui, "被控端服务", desc, 240.0, |ui| {
            if installed {
                if kit::text_button(ui, "卸载", kit::TEXT_WEAK).clicked() {
                    result = winhost::run_elevated("uninstall");
                }
                if kit::secondary_button(ui, "重新安装").clicked() {
                    result = winhost::run_elevated("install");
                }
                kit::chip(ui, winhost::state_text(state), kit::GRAY);
            } else if kit::primary_button(ui, "安装", true).clicked() {
                result = winhost::run_elevated("install");
            }
        });
        if let Err(e) = result {
            self.say(format!("{e:#}"), true);
        }
    }

    #[cfg(target_os = "macos")]
    fn host_platform_ui(&mut self, ui: &mut egui::Ui) {
        if !self.settings.host {
            return;
        }
        let (screen, access) = self.host.lock().unwrap().perms;
        let rows = [
            ("辅助功能", "用来接收对方发来的键盘和鼠标操作", access, "Privacy_Accessibility"),
            ("录屏与系统录音", "用来把这台 Mac 的画面和声音传给对方", screen, "Privacy_ScreenCapture"),
        ];
        for (title, desc, granted, pane) in rows {
            kit::divider(ui);
            kit::setting_row(ui, title, desc, 220.0, |ui| {
                if !granted && kit::secondary_button(ui, "打开设置").clicked() {
                    let _ = Command::new("open")
                        .arg(format!("x-apple.systempreferences:com.apple.preference.security?{pane}"))
                        .spawn();
                }
                if granted {
                    kit::chip(ui, "已允许", kit::GREEN);
                } else {
                    kit::chip(ui, "未允许", kit::ORANGE);
                }
            });
        }
        if !(screen && access) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "在系统设置里打开 SuperControl 的开关，这里会自动更新。如果开关已经打开仍显示“未允许”，\
                     说明那是旧版本留下的授权：选中 SuperControl 点“−”删除，再重新打开本程序授权。",
                )
                .size(12.0)
                .color(kit::TEXT_WEAK),
            );
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn host_platform_ui(&mut self, _ui: &mut egui::Ui) {}

    /// 底部提示条。
    fn toast(&mut self, ctx: &egui::Context) {
        let Some((text, error, _)) = self.message.clone() else {
            return;
        };
        // 黑底白字；错误时文字前加红点
        let (fg, bg) = (Color32::WHITE, kit::BLACK);
        let mut close = false;
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [105.0, -20.0])
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(bg)
                    .corner_radius(12)
                    .inner_margin(Margin::symmetric(14, 10))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 16,
                        spread: 0,
                        color: Color32::from_black_alpha(25),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(520.0);
                        ui.horizontal(|ui| {
                            if error {
                                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                ui.painter().circle_filled(r.center(), 4.0, kit::RED);
                            }
                            ui.add(
                                egui::Label::new(RichText::new(&text).size(13.0).color(fg)).wrap(),
                            );
                            if kit::close_button(ui, Color32::from_gray(150)).clicked() {
                                close = true;
                            }
                        });
                    });
            });
        if close {
            self.message = None;
        }
    }

    /// 连接 / 配对进度对话框。
    fn job_dialog(&mut self, ctx: &egui::Context) {
        let status = self.job.as_ref().and_then(|j| match &*j.lock().unwrap() {
            Job::Connecting { name } => Some((name.clone(), None)),
            Job::Pairing { name, code } => Some((name.clone(), Some(code.clone()))),
            _ => None,
        });
        let Some((name, code)) = status else { return };
        egui::Modal::new(egui::Id::unique("connecting"))
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .corner_radius(16)
                    .inner_margin(Margin::same(24)),
            )
            .show(ctx, |ui| {
                ui.set_width(360.0);
                match code {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(format!("正在连接「{name}」…"))
                                    .size(15.0)
                                    .color(kit::TEXT),
                            );
                        });
                    }
                    Some(code) => {
                        ui.label(
                            RichText::new(format!("与「{name}」配对"))
                                .size(18.0)
                                .strong()
                                .color(kit::TEXT),
                        );
                        ui.label(
                            RichText::new(
                                "第一次连接需要配对。请在对方屏幕上核对配对码，一致后点“允许”。",
                            )
                            .size(13.0)
                            .color(kit::TEXT_WEAK),
                        );
                        ui.add_space(14.0);
                        let digits: Vec<char> = code.chars().collect();
                        let (box_w, gap) = (44.0, 8.0);
                        let total = digits.len() as f32 * box_w
                            + (digits.len().saturating_sub(1)) as f32 * gap;
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 56.0),
                            Sense::hover(),
                        );
                        let mut x = rect.center().x - total / 2.0;
                        for d in digits {
                            let r = egui::Rect::from_min_size(
                                egui::pos2(x, rect.top()),
                                Vec2::new(box_w, 56.0),
                            );
                            ui.painter().rect_filled(r, 10, kit::FILL);
                            ui.painter().text(
                                r.center(),
                                egui::Align2::CENTER_CENTER,
                                d,
                                egui::FontId::proportional(28.0),
                                kit::TEXT,
                            );
                            x += box_w + gap;
                        }
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("等待对方确认（60 秒内）…")
                                    .size(13.0)
                                    .color(kit::TEXT_WEAK),
                            );
                        });
                        ui.label(
                            RichText::new("配对一次后，以后可以直接连接。")
                                .size(12.0)
                                .color(kit::TEXT_FAINT),
                        );
                    }
                }
            });
    }
}

/// 唤醒后等它上线的最长时间
const WAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// 设备卡片。返回 (点了连接, 点了忘记, 点了唤醒)。
fn device_card(ui: &mut egui::Ui, c: &CardInfo, width: f32, busy: bool) -> (bool, bool, bool) {
    let (mut connect, mut forget, mut wake) = (false, false, false);
    kit::card().show(ui, |ui| {
        ui.set_width(width - 34.0);
        ui.horizontal(|ui| {
            kit::device_tile(ui, &c.os, 46.0, c.online);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(
                    egui::Label::new(RichText::new(&c.name).size(15.0).strong().color(kit::TEXT))
                        .truncate(),
                );
                let sub = if c.os.is_empty() {
                    c.addr.clone()
                } else {
                    format!("{} · {}", kit::os_label(&c.os), c.addr)
                };
                ui.label(RichText::new(sub).size(12.0).color(kit::TEXT_WEAK));
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if c.waking {
                kit::chip(ui, "正在唤醒…", kit::ORANGE);
            } else if !c.online {
                kit::chip(ui, "离线", kit::GRAY);
            } else if !c.compatible {
                kit::chip(ui, "版本不一致", kit::ORANGE);
            } else {
                kit::chip(ui, "在线", kit::GREEN);
            }
            if c.paired {
                kit::chip(ui, "已配对", kit::BLACK);
            } else {
                kit::chip(ui, "首次连接需配对", kit::GRAY);
            }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let enabled = !busy && c.compatible && c.target.is_some() && !c.connected;
            let hint = if c.connected {
                "已在控制这台电脑，远程画面在独立窗口中"
            } else if !c.compatible {
                "两台电脑的 SuperControl 版本不一致，请更新到同一版本"
            } else if !c.online {
                "对方当前不在线，将按上次的地址尝试连接"
            } else {
                "打开远程画面"
            };
            let label = if c.connected {
                "已连接"
            } else if c.online {
                "连接"
            } else {
                "尝试连接"
            };
            if kit::primary_button(ui, label, enabled)
                .on_hover_text(hint)
                .on_disabled_hover_text(hint)
                .clicked()
            {
                connect = true;
            }
            if c.wake.is_some()
                && !c.waking
                && kit::secondary_button(ui, "唤醒")
                    .on_hover_text("对方睡眠时发送网络唤醒信号（需要对方网卡支持网络唤醒）")
                    .clicked()
            {
                wake = true;
            }
            if c.forget_id.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kit::text_button(ui, "忘记", kit::TEXT_FAINT)
                        .on_hover_text("删除本机保存的配对信息")
                        .clicked()
                    {
                        forget = true;
                    }
                });
            }
        });
    });
    (connect, forget, wake)
}

impl eframe::App for Gui {
    fn on_exit(&mut self) {
        self.host_want.store(false, Ordering::SeqCst);
        self.host.lock().unwrap().stop();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        self.window_lifecycle(&ctx);
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::left("nav")
            .exact_size(210.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(kit::SIDEBAR).inner_margin(Margin {
                left: 14,
                right: 14,
                top: 20,
                bottom: 14,
            }))
            .show(ui, |ui| {
                egui::Panel::bottom("local").frame(egui::Frame::new()).resizable(false).show(ui, |ui| self.local_card(ui));
                self.sidebar(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(kit::BG).inner_margin(Margin {
                left: 28,
                right: 28,
                top: 24,
                bottom: 8,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        match self.page {
                            Page::Devices => self.devices_page(ui),
                            Page::Settings => self.settings_page(ui),
                        }
                        ui.add_space(60.0); // 给底部提示条留位置
                    });
            });

        self.toast(&ctx);
        self.job_dialog(&ctx);

        if let Some((path, at)) = &self.screenshot {
            if at.elapsed() > Duration::from_secs(2) {
                let path = path.clone();
                self.screenshot = None;
                let c = ctx.clone();
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::ScreenshotCallback::new(move |img| {
                        let _ = save_png(&path, &img);
                        c.send_viewport_cmd(egui::ViewportCommand::Close);
                    }),
                ));
            }
        }
    }
}

/// 后台每 2 秒扫描一次局域网。
fn spawn_scanner(seen: Arc<Mutex<HashMap<DeviceId, Seen>>>, ctx: egui::Context) {
    std::thread::spawn(move || loop {
        if let Ok(found) = net::discover(Duration::from_millis(700)) {
            let now = Instant::now();
            // 本机自己的被控端不显示
            let local: Vec<std::net::IpAddr> = if_addrs::get_if_addrs()
                .unwrap_or_default()
                .iter()
                .map(|i| i.ip())
                .collect();
            let mut map = seen.lock().unwrap();
            for host in found
                .into_iter()
                .filter(|h| !h.addr.ip().is_loopback() && !local.contains(&h.addr.ip()))
            {
                if host.paired {
                    let macs = if host.macs.is_empty() { net::neighbor_mac(host.addr.ip()).into_iter().collect() } else { host.macs.clone() };
                    if !macs.is_empty() {
                        let _ = config::remember_macs(&host.host_id, &macs);
                    }
                }
                map.insert(host.host_id, Seen { host, at: now });
            }
            map.retain(|_, s| now - s.at < Duration::from_secs(30));
            drop(map);
            ctx.request_repaint();
        }
        std::thread::sleep(Duration::from_millis(1300));
    });
}

/// 启动远程画面窗口进程。
fn spawn_viewer(
    name: &str,
    addr: SocketAddr,
    settings: &config::Settings,
    host_os: Option<&str>,
) -> Result<Session> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("view").arg(addr.to_string());
    // Mac 控制任何系统时都按习惯互换；Windows 只在控制 Mac 时互换（控制 Windows 时 Ctrl 就是 Ctrl）
    if let Some(os) = host_os.filter(|o| matches!(*o, "macos" | "windows")) {
        cmd.arg("--host-os").arg(os);
    }
    if !settings.system_keys {
        cmd.arg("--local-shortcuts");
    }
    if settings.cmd_as_ctrl && (cfg!(target_os = "macos") || host_os == Some("macos")) {
        cmd.arg("--cmd-as-ctrl");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // 不弹出控制台窗口
    }
    if settings.native {
        cmd.arg("--native");
    }
    if !settings.mute_host {
        cmd.arg("--no-mute-host");
    }
    if settings.mic {
        cmd.arg("--mic");
    }
    if !settings.audio {
        cmd.arg("--no-audio");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let ready = Arc::new(AtomicBool::new(false));
    let displays = Arc::new(Mutex::new((Vec::new(), 0)));
    if let Some(pipe) = child.stdout.take() {
        let (ready, displays) = (ready.clone(), displays.clone());
        // 一直读到进程退出，否则管道写满会卡住画面窗口
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                if line.starts_with("已连接") {
                    ready.store(true, Ordering::Relaxed);
                } else if let Some(rest) = line.strip_prefix("RC-DISPLAYS ") {
                    // RC-DISPLAYS <当前下标> <名称1>|<名称2>|…
                    if let Some((cur, names)) = rest.split_once(' ') {
                        let names = names.split('|').map(str::to_string).collect();
                        *displays.lock().unwrap() = (names, cur.parse().unwrap_or(0));
                    }
                }
            }
        });
    }
    let stdin = child.stdin.take();
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(pipe) = child.stderr.take() {
        let lines = stderr.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                let mut v = lines.lock().unwrap();
                v.push(line);
                if v.len() > 20 {
                    v.remove(0);
                }
            }
        });
    }
    Ok(Session {
        name: name.to_string(),
        host_id: None,
        addr,
        child,
        stderr,
        ready,
        displays,
        stdin,
    })
}

fn save_png(path: &str, img: &egui::ColorImage) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let rgba: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|p| p.to_srgba_unmultiplied())
        .collect();
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

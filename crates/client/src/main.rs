//! 控制端（macOS / Windows）：图形界面、远程画面窗口、剪贴板与文件、声音。

// Windows：作为窗口程序运行，双击打开时不带黑色的控制台窗口。命令行用法见 main 里的 attach_console。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("控制端只支持 macOS 和 Windows。");
    std::process::exit(1);
}

#[cfg(any(target_os = "macos", windows))]
mod audio;
#[cfg(target_os = "macos")]
mod awdl;
/// Windows 上没有 AWDL
#[cfg(windows)]
mod awdl {
    pub fn enabled() -> bool {
        false
    }
    pub fn active() -> bool {
        false
    }
    pub fn set_enabled(_: bool) -> anyhow::Result<()> {
        Ok(())
    }
}
#[cfg(any(target_os = "macos", windows))]
mod config;
#[cfg(any(target_os = "macos", windows))]
mod ui_kit;
#[cfg(any(target_os = "macos", windows))]
mod autostart;
#[cfg(any(target_os = "macos", windows))]
mod tray;
#[cfg(any(target_os = "macos", windows))]
mod datalink;
#[cfg(target_os = "macos")]
#[path = "display_mac.rs"]
mod display;
#[cfg(windows)]
#[path = "display_win.rs"]
mod display;
#[cfg(any(target_os = "macos", windows))]
mod gui;
#[cfg(target_os = "macos")]
mod hostagent;
#[cfg(any(target_os = "macos", windows))]
mod keymap;
#[cfg(any(target_os = "macos", windows))]
mod localclip;
#[cfg(any(target_os = "macos", windows))]
mod net;
#[cfg(windows)]
mod winhost;

#[cfg(any(target_os = "macos", windows))]
#[derive(clap::Parser, Debug)]
#[command(about = "局域网远程控制 - 控制端（不带参数运行时打开图形界面）")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// 后台启动（开机自启用）：不显示主窗口，只在托盘 / 菜单栏里
    #[arg(long, hide = true)]
    background: bool,
}

#[cfg(any(target_os = "macos", windows))]
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// 连接被控端并显示远程画面
    View(app::ViewArgs),
    /// 扫描局域网里的被控端
    Scan,
    /// 列出已配对的被控端
    Hosts,
    /// 删除已配对的被控端（按标识前缀或名称）
    Forget { query: String },
}

#[cfg(any(target_os = "macos", windows))]
fn main() -> anyhow::Result<()> {
    use clap::Parser;
    #[cfg(windows)]
    attach_console();
    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(cli.background),
        Some(Cmd::View(args)) => app::run(args),
        Some(Cmd::Scan) => {
            let found = net::discover(std::time::Duration::from_millis(800))?;
            if found.is_empty() {
                println!("没有发现被控端（确认对方已运行 rc-host，且在同一局域网）");
            }
            for d in found {
                let paired = if d.paired { "已配对" } else { "未配对" };
                println!("{}  {}  {}  {}", d.addr, d.name, d.os, paired);
            }
            Ok(())
        }
        Some(Cmd::Hosts) => {
            for h in config::known_hosts() {
                println!("{}  {}  {}", &h.id[..8], h.name, h.last_addr.unwrap_or_default());
            }
            Ok(())
        }
        Some(Cmd::Forget { query }) => {
            let hosts = config::known_hosts();
            let matched: Vec<_> = hosts.iter().filter(|h| h.name == query || (query.len() >= 4 && h.id.starts_with(&query))).collect();
            if matched.is_empty() {
                println!("没有匹配「{query}」的被控端");
            }
            for h in matched {
                config::forget(&h.id)?;
                println!("已删除：{}", h.name);
            }
            Ok(())
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
mod app {
    use crate::{display::VideoSink, keymap, net};
    use anyhow::{Context, Result};
    use rc_protocol::{ClientMsg, InputEvent};
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
    use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::keyboard::PhysicalKey;
    #[cfg(target_os = "macos")]
    use winit::platform::macos::EventLoopBuilderExtMacOS;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    #[derive(clap::Args, Debug)]
    pub struct ViewArgs {
        /// 被控端地址，如 192.168.1.20 或 192.168.1.20:47800
        host: String,
        /// 按对方系统的习惯映射修饰键：Mac 控制 Windows 时 Command → Ctrl；
        /// Windows 控制 Mac 时 Ctrl → Command（⌘C / Ctrl+C 都是复制）
        #[arg(long, alias = "swap-modifiers")]
        cmd_as_ctrl: bool,
        /// 每秒在终端打印一行统计信息
        #[arg(long)]
        stats: bool,
        /// 始终按远端原始分辨率传输（默认按本机窗口的实际像素尺寸缩小编码，延迟更低）
        #[arg(long)]
        native: bool,
        /// 连接后把这些文件 / 文件夹发送到远端“下载”（等同于拖进窗口）
        #[arg(long = "send", value_name = "路径")]
        send: Vec<std::path::PathBuf>,
        /// 不播放远端的声音
        #[arg(long)]
        no_audio: bool,
        /// 连接后取回远端剪贴板里的文件（等同于 ⌘⇧D）
        #[arg(long, hide = true)]
        fetch: bool,
        /// 指定视频格式（默认 HEVC 优先，本机或对方不支持时用 H.264）
        #[arg(long, value_parser = ["hevc", "h264"])]
        codec: Option<String>,
    }

    /// 取回远端复制的文件的快捷键（显示用）。
    const FETCH_KEY: &str = if cfg!(target_os = "macos") { "⌘⇧D" } else { "Ctrl+Alt+D" };

    enum UserEvent {
        Disconnected,
        Cursor(rc_protocol::CursorShape),
        /// 被控端的显示器列表与当前显示器
        Displays(Vec<rc_protocol::DisplayInfo>, u32),
        /// 切换到第几块显示器（来自主界面，经标准输入）
        SelectDisplay(u32),
    }

    /// 切换显示器的快捷键说明（显示用）。
    const DISPLAY_KEYS: &str = if cfg!(target_os = "macos") { "⌃⌥← / ⌃⌥→" } else { "Ctrl+Alt+← / →" };

    /// 被控端光标形状 → 本地系统光标；`None` 表示隐藏本地光标。
    fn cursor_icon(shape: rc_protocol::CursorShape) -> Option<winit::window::CursorIcon> {
        use rc_protocol::CursorShape as S;
        use winit::window::CursorIcon as C;
        Some(match shape {
            S::Arrow => C::Default,
            S::Text => C::Text,
            S::Hand => C::Pointer,
            S::Wait => C::Wait,
            S::Progress => C::Progress,
            S::Crosshair => C::Crosshair,
            S::ResizeNS => C::NsResize,
            S::ResizeEW => C::EwResize,
            S::ResizeNWSE => C::NwseResize,
            S::ResizeNESW => C::NeswResize,
            S::Move => C::Move,
            S::NotAllowed => C::NotAllowed,
            S::Help => C::Help,
            S::Hidden | S::InVideo => return None,
        })
    }

    struct App {
        args: ViewArgs,
        host_name: String,
        conn: Option<net::Connection>,
        proxy: winit::event_loop::EventLoopProxy<UserEvent>,
        window: Option<Arc<Window>>,
        control: Option<net::Control>,
        shared: Arc<net::Shared>,
        video_size: (u32, u32),
        keys_down: HashSet<(u16, bool)>,
        buttons_down: HashSet<u8>,
        /// 触控板像素滚动的小数累积（单位：1/120 格）。
        wheel_acc: (f64, f64),
        data: Option<crate::datalink::DataLink>,
        /// 声音播放流（丢弃即停止）
        audio: Option<cpal::Stream>,
        mods: winit::keyboard::ModifiersState,
        /// 被本地快捷键吃掉的按键：抬起时也不发给远端
        swallowed: HashSet<winit::keyboard::KeyCode>,
        /// 被控端的显示器列表与当前显示的那块
        displays: Vec<rc_protocol::DisplayInfo>,
        current_display: u32,
    }

    pub fn run(args: ViewArgs) -> Result<()> {
        // 告诉系统这是延迟敏感的前台任务：不做 App Nap、不合并定时器。
        #[cfg(target_os = "macos")]
        let _activity = begin_latency_critical_activity();
        let addr = net::parse_addr(&args.host)?;
        let mut codecs = crate::display::supported_codecs();
        if let Some(c) = &args.codec {
            let want = if c == "h264" { rc_protocol::Codec::H264 } else { rc_protocol::Codec::Hevc };
            anyhow::ensure!(codecs.contains(&want), "本机不支持 {} 解码", want.name());
            codecs = vec![want];
        }
        let conn = net::connect(addr, !args.no_audio, codecs, |ev| match ev {
            net::HandshakeEvent::PairCode { host_name, code } => {
                eprintln!("首次连接「{host_name}」需要配对：请在对方屏幕上确认配对码 {code} 并点“是”");
            }
        })?;
        println!("已连接「{}」，远端画面 {}x{} @ {}fps，{}", conn.host_name, conn.width, conn.height, conn.fps, conn.codec.name());

        let event_loop = EventLoop::<UserEvent>::with_user_event()
            // 不要默认菜单：否则 Cmd+Q / Cmd+W / Cmd+H 会被本地菜单吃掉，发不到远端。
            .with_default_menu_if_mac()
            .build()?;
        let mut app = App {
            video_size: (conn.width, conn.height),
            host_name: conn.host_name.clone(),
            args,
            conn: Some(conn),
            proxy: event_loop.create_proxy(),
            window: None,
            control: None,
            shared: Arc::new(net::Shared::default()),
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            wheel_acc: (0.0, 0.0),
            data: None,
            audio: None,
            mods: Default::default(),
            swallowed: HashSet::new(),
            displays: Vec::new(),
            current_display: 0,
        };
        // 主界面通过标准输入发命令：`display <下标>` 切换远端显示器
        {
            let proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
                    if let Some(i) = line.strip_prefix("display ").and_then(|v| v.trim().parse().ok()) {
                        let _ = proxy.send_event(UserEvent::SelectDisplay(i));
                    }
                }
            });
        }
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    impl App {
        fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
            let mut conn = self.conn.take().context("重复初始化")?;
            let (vw, vh) = self.video_size;
            // 初始窗口：逻辑宽 1280，保持远端画面比例。
            let size = LogicalSize::new(1280.0, 1280.0 * vh as f64 / vw as f64);
            let attrs = Window::default_attributes().with_title(format!("远程控制 - {}", self.host_name)).with_inner_size(size);
            let window = Arc::new(event_loop.create_window(attrs)?);

            let sink = match window.window_handle()?.as_raw() {
                #[cfg(target_os = "macos")]
                RawWindowHandle::AppKit(h) => unsafe { VideoSink::attach(h.ns_view.as_ptr(), conn.codec)? },
                #[cfg(windows)]
                RawWindowHandle::Win32(h) => unsafe { VideoSink::attach(h.hwnd.get() as *mut std::ffi::c_void, conn.codec)? },
                _ => anyhow::bail!("不支持的窗口类型"),
            };

            let control = net::Control::new(conn.tcp.try_clone()?);
            self.shared.alive.store(true, Ordering::Relaxed);
            if let Some(sock) = conn.audio.take() {
                match crate::audio::start(sock, Arc::new(std::sync::atomic::AtomicBool::new(true))) {
                    Ok(s) => self.audio = Some(s),
                    Err(e) => eprintln!("声音不可用: {e:#}"),
                }
            }
            // 数据通道失败不影响远程画面，只是没有剪贴板和文件传输
            match crate::datalink::DataLink::open(conn.addr, conn.data_token) {
                Ok(link) => {
                    if !self.args.send.is_empty() {
                        let (l, paths) = (link.clone(), self.args.send.clone());
                        std::thread::spawn(move || {
                            l.send_paths(paths);
                            eprintln!("{}", l.status.lock().unwrap().clone().unwrap_or_default());
                        });
                    }
                    if self.args.fetch {
                        let l = link.clone();
                        std::thread::spawn(move || {
                            // 等远端剪贴板里出现文件（最多 60 秒）
                            for _ in 0..600 {
                                if l.remote_files.lock().unwrap().is_some() {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            l.fetch_remote_files();
                        });
                    }
                    self.data = Some(link);
                }
                Err(e) => eprintln!("数据通道不可用（剪贴板与文件传输不可用）: {e:#}"),
            }
            let (proxy, cursor_proxy, displays_proxy) = (self.proxy.clone(), self.proxy.clone(), self.proxy.clone());
            net::spawn_control_reader(
                conn.tcp,
                self.shared.clone(),
                move |shape| {
                    let _ = cursor_proxy.send_event(UserEvent::Cursor(shape));
                },
                move |list, current| {
                    let _ = displays_proxy.send_event(UserEvent::Displays(list, current));
                },
                move || {
                    let _ = proxy.send_event(UserEvent::Disconnected);
                },
            );
            net::spawn_pinger(control.clone(), self.shared.clone());
            let (title_window, host, print_stats) = (window.clone(), self.host_name.clone(), self.args.stats);
            let shared = self.shared.clone();
            let link = self.data.clone();
            net::spawn_video_receiver(conn.udp, sink, control.clone(), self.shared.clone(), move |s| {
                let mut line = format!(
                    "{} fps · {:.0} Mbps · 延迟 {:.1} ms（被控端 {:.1} + 传输 {:.1}，最大 {:.1}）· RTT {:.1} ms",
                    s.fps,
                    s.mbps,
                    s.latency_ms,
                    s.host_ms,
                    (s.latency_ms - s.host_ms).max(0.0),
                    s.latency_max_ms,
                    s.rtt_ms
                );
                if s.recovered > 0 {
                    line += &format!(" · FEC 修复 {}", s.recovered);
                }
                if s.lost > 0 {
                    line += &format!(" · 丢帧 {}", s.lost);
                }
                if s.awdl_active {
                    line += " · ⚠️ AWDL 开启（会造成延迟尖峰）";
                }
                if let Some(link) = &link {
                    if let Some((names, bytes)) = link.remote_files.lock().unwrap().as_ref() {
                        let first = names.first().cloned().unwrap_or_default();
                        let more = if names.len() > 1 { format!(" 等 {} 项", names.len()) } else { String::new() };
                        line = format!("远端复制了「{first}」{more}（{:.1} MB），按 {FETCH_KEY} 保存到本机 · {line}", *bytes as f64 / 1e6);
                    }
                    if let Some(st) = link.status.lock().unwrap().as_ref() {
                        line = format!("{st} · {line}");
                    }
                }
                if print_stats {
                    println!("{line}");
                }
                let host = match shared.display_label.lock().unwrap().as_deref() {
                    Some(d) => format!("{host} · {d}"),
                    None => host.clone(),
                };
                let title = match shared.notice.lock().unwrap().as_deref() {
                    Some(n) => format!("远程控制 - {host} · ⚠️ {n}"),
                    None => format!("远程控制 - {host} · {line}"),
                };
                title_window.set_title(&title);
            });

            self.window = Some(window);
            self.control = Some(control);
            self.send_viewport();
            Ok(())
        }

        /// 告诉被控端本机窗口的实际像素尺寸，它会按此缩小编码分辨率。
        fn send_viewport(&self) {
            let (Some(window), Some(control)) = (&self.window, &self.control) else { return };
            let (width, height) = if self.args.native {
                (0, 0)
            } else {
                let size = window.inner_size();
                (size.width, size.height)
            };
            control.send(&ClientMsg::Viewport { width, height });
        }

        fn select_display(&mut self, index: u32) {
            if index as usize >= self.displays.len() || index == self.current_display {
                return;
            }
            // 松开按着的键，免得切换过程中远端卡键
            self.release_all();
            if let Some(c) = &self.control {
                c.send(&ClientMsg::SelectDisplay { index });
            }
        }

        fn send(&self, ev: InputEvent) {
            if let Some(c) = &self.control {
                c.send(&ClientMsg::Input(ev));
            }
        }

        /// 窗口内的物理像素坐标 → 远端屏幕归一化坐标（考虑等比缩放的黑边）。
        fn map_cursor(&self, pos: PhysicalPosition<f64>, win: PhysicalSize<u32>) -> (u16, u16) {
            normalize_cursor((pos.x, pos.y), (win.width as f64, win.height as f64), self.video_size)
        }

        /// 松开所有按下的键和鼠标键（失去焦点时调用，防止远端卡键）。
        fn release_all(&mut self) {
            for (scancode, extended) in std::mem::take(&mut self.keys_down) {
                self.send(InputEvent::Key { scancode, extended, down: false });
            }
            for button in std::mem::take(&mut self.buttons_down) {
                self.send(InputEvent::MouseButton { button, down: false });
            }
        }
    }

    /// 只在 macOS 上关闭默认菜单（否则 ⌘Q / ⌘W / ⌘H 会被本地菜单吃掉，发不到远端）。
    trait DefaultMenu {
        fn with_default_menu_if_mac(&mut self) -> &mut Self;
    }

    impl DefaultMenu for winit::event_loop::EventLoopBuilder<UserEvent> {
        fn with_default_menu_if_mac(&mut self) -> &mut Self {
            #[cfg(target_os = "macos")]
            self.with_default_menu(false);
            self
        }
    }

    #[cfg(target_os = "macos")]
    fn begin_latency_critical_activity() -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
        use objc2::runtime::{AnyClass, AnyObject};
        use objc2::msg_send;
        use objc2_foundation::NSString;
        const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF;
        const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0xFF_0000_0000;
        unsafe {
            let info: *mut AnyObject = msg_send![AnyClass::get(c"NSProcessInfo")?, processInfo];
            let reason = NSString::from_str("远程控制画面传输");
            msg_send![info, beginActivityWithOptions: NS_ACTIVITY_USER_INITIATED | NS_ACTIVITY_LATENCY_CRITICAL, reason: &*reason]
        }
    }

    pub(crate) fn normalize_cursor(pos: (f64, f64), win: (f64, f64), video: (u32, u32)) -> (u16, u16) {
        let (vw, vh) = (video.0.max(1) as f64, video.1.max(1) as f64);
        let scale = (win.0 / vw).min(win.1 / vh).max(f64::MIN_POSITIVE);
        let (dw, dh) = (vw * scale, vh * scale);
        let (ox, oy) = ((win.0 - dw) / 2.0, (win.1 - dh) / 2.0);
        let nx = ((pos.0 - ox) / dw).clamp(0.0, 1.0);
        let ny = ((pos.1 - oy) / dh).clamp(0.0, 1.0);
        ((nx * 65535.0).round() as u16, (ny * 65535.0).round() as u16)
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_none() {
                if let Err(e) = self.start(event_loop) {
                    eprintln!("初始化失败: {e:#}");
                    event_loop.exit();
                }
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::Disconnected => {
                    eprintln!("连接已断开");
                    event_loop.exit();
                }
                UserEvent::Displays(list, current) => {
                    if let Some(d) = list.get(current as usize) {
                        // 鼠标坐标按新显示器的宽高比换算（画面等比缩放，可能有黑边）
                        self.video_size = (d.width, d.height);
                        *self.shared.display_label.lock().unwrap() = (list.len() > 1)
                            .then(|| format!("{}（{}/{}，{DISPLAY_KEYS} 切换）", d.name, current + 1, list.len()));
                    }
                    // 告诉主界面（它读本进程的标准输出）：当前下标 + 各显示器名称
                    let names: Vec<String> = list.iter().map(|d| d.name.replace('|', " ")).collect();
                    println!("RC-DISPLAYS {current} {}", names.join("|"));
                    self.displays = list;
                    self.current_display = current;
                }
                UserEvent::SelectDisplay(index) => self.select_display(index),
                UserEvent::Cursor(shape) => {
                    if self.args.stats {
                        println!("光标: {shape:?}");
                    }
                    if let Some(window) = &self.window {
                        match cursor_icon(shape) {
                            Some(icon) => {
                                window.set_cursor(icon);
                                window.set_cursor_visible(true);
                            }
                            None => window.set_cursor_visible(false),
                        }
                    }
                }
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            let Some(window) = self.window.clone() else { return };
            match event {
                WindowEvent::CloseRequested => {
                    self.release_all();
                    if let Some(d) = &self.data {
                        d.close();
                    }
                    self.shared.alive.store(false, Ordering::Relaxed);
                    event_loop.exit();
                }
                WindowEvent::Focused(false) => self.release_all(),
                // 包括切换全屏、拖到不同缩放比例的显示器（被控端会去抖）
                WindowEvent::Resized(_) => self.send_viewport(),
                WindowEvent::CursorMoved { position, .. } => {
                    let (x, y) = self.map_cursor(position, window.inner_size());
                    self.send(InputEvent::MouseMoveAbs { x, y });
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let button = match button {
                        MouseButton::Left => 0,
                        MouseButton::Right => 1,
                        MouseButton::Middle => 2,
                        MouseButton::Back => 3,
                        MouseButton::Forward => 4,
                        MouseButton::Other(_) => return,
                    };
                    let down = state == ElementState::Pressed;
                    if down {
                        self.buttons_down.insert(button);
                    } else {
                        self.buttons_down.remove(&button);
                    }
                    self.send(InputEvent::MouseButton { button, down });
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    // 统一成 Windows 的 WHEEL_DELTA 单位（120 = 一格）。
                    // winit 中正值表示“内容向右/向下移动”：纵向与 Windows 同号，横向相反。
                    let (dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (-x as f64 * 120.0, y as f64 * 120.0),
                        MouseScrollDelta::PixelDelta(p) => (-p.x * 3.0, p.y * 3.0),
                    };
                    self.wheel_acc.0 += dx;
                    self.wheel_acc.1 += dy;
                    let (ix, iy) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
                    self.wheel_acc.0 -= ix;
                    self.wheel_acc.1 -= iy;
                    let clamp = |v: f64| v.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    if ix != 0.0 || iy != 0.0 {
                        self.send(InputEvent::Wheel { dx: clamp(ix), dy: clamp(iy) });
                    }
                }
                WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
                // 拖进窗口的文件发送到远端“下载”，并放进远端剪贴板
                WindowEvent::DroppedFile(path) => {
                    if let Some(d) = &self.data {
                        d.drop_file(path);
                    }
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    let PhysicalKey::Code(code) = event.physical_key else { return };
                    // ⌘⇧D（Windows 上 Ctrl+Alt+D）：取回远端复制的文件（本地快捷键，不发给远端）
                    let fetch_mods = if cfg!(target_os = "macos") {
                        self.mods.super_key() && self.mods.shift_key()
                    } else {
                        self.mods.control_key() && self.mods.alt_key()
                    };
                    if code == winit::keyboard::KeyCode::KeyD && fetch_mods {
                        if event.state == ElementState::Pressed && !event.repeat {
                            self.swallowed.insert(code);
                            if let Some(d) = &self.data {
                                d.fetch_remote_files();
                            }
                        }
                        return;
                    }
                    // ⌃⌥← / ⌃⌥→（Windows 上 Ctrl+Alt+← / →）：切换远端的显示器
                    use winit::keyboard::KeyCode::{ArrowLeft, ArrowRight};
                    if matches!(code, ArrowLeft | ArrowRight) && self.mods.control_key() && self.mods.alt_key() && self.displays.len() > 1 {
                        if event.state == ElementState::Pressed && !event.repeat {
                            self.swallowed.insert(code);
                            let n = self.displays.len() as u32;
                            let next = if code == ArrowRight { (self.current_display + 1) % n } else { (self.current_display + n - 1) % n };
                            self.select_display(next);
                        }
                        return;
                    }
                    if event.state == ElementState::Released && self.swallowed.remove(&code) {
                        return;
                    }
                    let Some((scancode, extended)) = keymap::to_scancode(code, self.args.cmd_as_ctrl) else { return };
                    let down = event.state == ElementState::Pressed;
                    if down {
                        self.keys_down.insert((scancode, extended));
                    } else if !self.keys_down.remove(&(scancode, extended)) {
                        return; // 没发过按下（例如焦点切换时已统一松开），不补发抬起
                    }
                    self.send(InputEvent::Key { scancode, extended, down });
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::normalize_cursor;

        #[test]
        fn cursor_exact_fit() {
            assert_eq!(normalize_cursor((0.0, 0.0), (1920.0, 1080.0), (1920, 1080)), (0, 0));
            assert_eq!(normalize_cursor((1920.0, 1080.0), (1920.0, 1080.0), (1920, 1080)), (65535, 65535));
            assert_eq!(normalize_cursor((960.0, 540.0), (1920.0, 1080.0), (1920, 1080)), (32768, 32768));
        }

        #[test]
        fn cursor_letterbox_and_pillarbox() {
            // 窗口比画面“高”：上下各 270 的黑边
            let win = (1920.0, 1620.0);
            assert_eq!(normalize_cursor((0.0, 100.0), win, (1920, 1080)), (0, 0)); // 黑边内钳到边缘
            assert_eq!(normalize_cursor((960.0, 270.0), win, (1920, 1080)).1, 0);
            assert_eq!(normalize_cursor((960.0, 810.0), win, (1920, 1080)), (32768, 32768));
            assert_eq!(normalize_cursor((960.0, 1350.0), win, (1920, 1080)).1, 65535);
            // 窗口比画面“宽”：左右各 480 的黑边，画面缩小一半也不影响结果
            let win = (1920.0, 540.0);
            assert_eq!(normalize_cursor((480.0, 0.0), win, (1920, 1080)), (0, 0));
            assert_eq!(normalize_cursor((1440.0, 540.0), win, (1920, 1080)), (65535, 65535));
        }

        #[test]
        fn cursor_degenerate_window() {
            let _ = normalize_cursor((5.0, 5.0), (0.0, 0.0), (1920, 1080));
            let _ = normalize_cursor((5.0, 5.0), (100.0, 100.0), (0, 0));
        }
    }
}

/// Windows 窗口程序没有自己的控制台。从命令行运行时借用父进程（cmd / PowerShell）的控制台，
/// 让命令行子命令的输出照常显示；输出已被重定向（例如界面启动画面窗口时接了管道）就不动。
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE};
    unsafe {
        if GetStdHandle(STD_ERROR_HANDLE).map_or(true, |h| h.is_invalid() || h.0.is_null()) {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

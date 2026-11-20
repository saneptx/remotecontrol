//! 控制端（macOS / Windows）：图形界面、远程画面窗口、剪贴板与文件、声音。

// Windows：作为窗口程序运行，双击打开时不带黑色的控制台窗口。命令行用法见 main 里的 attach_console。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("控制端只支持 macOS 和 Windows。");
    std::process::exit(1);
}

#[cfg(any(target_os = "macos", windows))]
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
    pub fn guard_installed() -> bool {
        false
    }
    pub fn guard_outdated() -> bool {
        false
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
#[cfg(any(target_os = "macos", windows))]
mod shortcuts;
#[cfg(any(target_os = "macos", windows))]
mod syskeys;
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
    use crate::shortcuts::{self as sc, Os};
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
    use winit::keyboard::{KeyCode, PhysicalKey};
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
        /// 连接期间不让对方的扬声器静音
        #[arg(long)]
        no_mute_host: bool,
        /// 把本机麦克风传给对方（对方需要装虚拟声卡：VB-CABLE / BlackHole）
        #[arg(long)]
        mic: bool,
        /// 对方的系统（macos / windows），用于翻译切应用、切桌面等快捷键；默认按与本机不同的系统处理
        #[arg(long, value_parser = ["macos", "windows"])]
        host_os: Option<String>,
        /// 不拦截本机的系统快捷键（⌘Tab / Alt+Tab、切桌面等留给本机）
        #[arg(long)]
        local_shortcuts: bool,
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
        /// 被控端刚装好虚拟声卡，可以开始传麦克风了
        MicPort(u16),
        /// 切换到第几块显示器（来自主界面，经标准输入）
        SelectDisplay(u32),
        /// 测试用：等同于在窗口里按粘贴键（本机复制了文件时发送并在远端粘贴）
        Paste,
        /// 被系统快捷键拦截（Windows 键盘钩子）拦下的按键
        HookKey(KeyCode, bool),
    }

    /// 测试命令里的键名。
    fn test_key(name: &str) -> Option<KeyCode> {
        use KeyCode::*;
        Some(match name {
            "SuperLeft" => SuperLeft,
            "ControlLeft" => ControlLeft,
            "AltLeft" => AltLeft,
            "ShiftLeft" => ShiftLeft,
            "Tab" => Tab,
            "ArrowLeft" => ArrowLeft,
            "ArrowRight" => ArrowRight,
            "ArrowUp" => ArrowUp,
            "Space" => Space,
            "F11" => F11,
            "KeyD" => KeyD,
            _ => return None,
        })
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
        audio: Option<rc_audio::Stream>,
        /// 正在发给对方的麦克风
        mic: Option<rc_audio::Stream>,
        /// 被控端地址（会话中途开始传麦克风时用）
        addr: Option<std::net::SocketAddr>,
        /// 被本地快捷键吃掉的按键：抬起时也不发给远端
        swallowed: HashSet<winit::keyboard::KeyCode>,
        /// 被控端的显示器列表与当前显示的那块
        displays: Vec<rc_protocol::DisplayInfo>,
        current_display: u32,
        /// 对方的系统（翻译快捷键用）
        remote_os: Os,
        /// 本机当前按住的物理键（Windows 上 Win 键被钩子拦下，窗口收不到它的修饰键状态，只能自己记）
        local_down: HashSet<KeyCode>,
        /// 远程窗口在前台时拦截本机系统快捷键
        syskeys: Option<crate::syskeys::SysKeys>,
        /// 用户按了“交还本机”组合键：在点回远程画面之前不拦截
        keys_released: bool,
        /// 正在对方那边切换应用：按住的对方修饰键（本机松开 ⌘ / Alt 时松开它）
        switching: Option<sc::Scan>,
        /// Windows：Win 键按下后还没有按别的键（松开时算“单按 Win”）
        win_tap: bool,
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
        let opts = net::Options { audio: !args.no_audio, codecs, mute_host: !args.no_mute_host, mic: args.mic };
        let conn = net::connect(addr, opts, |ev| match ev {
            net::HandshakeEvent::PairCode { host_name, code } => {
                eprintln!("首次连接「{host_name}」需要配对：请在对方屏幕上确认配对码 {code} 并点“是”");
            }
        })?;
        println!("已连接「{}」，远端画面 {}x{} @ {}fps，{}", conn.host_name, conn.width, conn.height, conn.fps, conn.codec.name());

        let event_loop = EventLoop::<UserEvent>::with_user_event()
            // 不要默认菜单：否则 Cmd+Q / Cmd+W / Cmd+H 会被本地菜单吃掉，发不到远端。
            .with_default_menu_if_mac()
            .build()?;
        let args_remote_os = args.host_os.as_deref().and_then(Os::parse).unwrap_or(match Os::LOCAL {
            Os::Mac => Os::Windows,
            Os::Windows => Os::Mac,
        });
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
            mic: None,
            addr: None,
            swallowed: HashSet::new(),
            displays: Vec::new(),
            current_display: 0,
            remote_os: args_remote_os,
            local_down: HashSet::new(),
            syskeys: None,
            keys_released: false,
            switching: None,
            win_tap: false,
        };
        // 主界面通过标准输入发命令：`display <下标>` 切换远端显示器
        {
            let proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
                    if let Some(i) = line.strip_prefix("display ").and_then(|v| v.trim().parse().ok()) {
                        let _ = proxy.send_event(UserEvent::SelectDisplay(i));
                    } else if line.trim() == "paste" {
                        let _ = proxy.send_event(UserEvent::Paste);
                    } else if let Some(combo) = line.trim().strip_prefix("key ") {
                        // 测试用：`key SuperLeft+Tab` 依次按下这些键、再倒序松开（等同于在窗口里按这组快捷键）
                        let keys: Vec<KeyCode> = combo.split('+').filter_map(test_key).collect();
                        for &k in &keys {
                            let _ = proxy.send_event(UserEvent::HookKey(k, true));
                        }
                        for &k in keys.iter().rev() {
                            let _ = proxy.send_event(UserEvent::HookKey(k, false));
                        }
                    }
                }
            });
        }
        event_loop.run_app(&mut app)?;
        // 以错误退出：主界面会把这句话显示给用户
        anyhow::ensure!(!app.shared.kicked.load(Ordering::Relaxed), "对方电脑上的用户断开了这次远程控制");
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
                match rc_audio::player::start(sock, Arc::new(std::sync::atomic::AtomicBool::new(true)), None) {
                    Ok(s) => self.audio = Some(s),
                    Err(e) => eprintln!("声音不可用: {e:#}"),
                }
            }
            self.addr = Some(conn.addr);
            if self.args.mic {
                self.start_mic(conn.addr, conn.mic_port);
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
            let (proxy, cursor_proxy, displays_proxy, mic_proxy) = (self.proxy.clone(), self.proxy.clone(), self.proxy.clone(), self.proxy.clone());
            net::spawn_control_reader(
                conn.tcp,
                self.shared.clone(),
                move |shape| {
                    let _ = cursor_proxy.send_event(UserEvent::Cursor(shape));
                },
                move |list, current| {
                    let _ = displays_proxy.send_event(UserEvent::Displays(list, current));
                },
                move |port| {
                    let _ = mic_proxy.send_event(UserEvent::MicPort(port));
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
                    if let Some((first, n)) = link.local_files_hint() {
                        let more = if n > 1 { format!(" 等 {n} 项") } else { String::new() };
                        line = format!("本机复制了「{first}」{more}，在远端粘贴即可发送 · {line}");
                    }
                    if let Some(st) = link.status.lock().unwrap().as_ref() {
                        line = format!("{st} · {line}");
                    }
                }
                if print_stats {
                    println!("{line}");
                }
                if let Some(n) = shared.audio_note.lock().unwrap().as_deref() {
                    line = format!("{n} · {line}");
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

            // 系统快捷键拦截：拦下的按键经事件循环交回窗口处理
            if !self.args.local_shortcuts {
                let proxy = self.proxy.clone();
                #[cfg(windows)]
                let hwnd = match window.window_handle()?.as_raw() {
                    RawWindowHandle::Win32(h) => h.hwnd.get(),
                    _ => 0,
                };
                #[cfg(not(windows))]
                let hwnd = 0;
                let on_key = Box::new(move |k, down| {
                    let _ = proxy.send_event(UserEvent::HookKey(k, down));
                });
                let mut keys = crate::syskeys::SysKeys::new(on_key, hwnd);
                keys.set_active(window.has_focus());
                self.syskeys = Some(keys);
            }
            self.window = Some(window);
            self.control = Some(control);
            self.send_viewport();
            Ok(())
        }

        /// 把本机麦克风发给对方（对方放进虚拟声卡）。
        fn start_mic(&mut self, addr: std::net::SocketAddr, port: u16) {
            let note = |s: &str| {
                eprintln!("{s}");
                *self.shared.audio_note.lock().unwrap() = Some(s.to_string());
            };
            if port == 0 {
                return note("麦克风暂未传送：对方电脑没有虚拟声卡，已在对方电脑上询问是否安装");
            }
            let started = std::net::UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| s.connect((addr.ip(), port)).map(|()| s))
                .map_err(anyhow::Error::from)
                .and_then(rc_audio::mic::start);
            match started {
                Ok((stream, name)) => {
                    println!("麦克风：正在把「{name}」传给对方");
                    self.mic = Some(stream);
                }
                Err(e) => note(&format!("麦克风不可用：{e:#}")),
            }
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

        /// 把本机复制的文件传到远端，传完后在远端按一次粘贴（用户当前按住的修饰键 + V）。
        fn paste_files(&self, files: Vec<std::path::PathBuf>) {
            let (Some(link), Some(control)) = (self.data.clone(), self.control.clone()) else { return };
            // 远端已按下的修饰键（例如 ⌘ 映射成的 Ctrl），粘贴时一并按下；
            // 没有按住任何键时（测试命令）按本机 Ctrl 键的映射来
            let mut mods: Vec<(u16, bool)> = self.keys_down.iter().copied().collect();
            if mods.is_empty() {
                mods.extend(keymap::to_scancode(winit::keyboard::KeyCode::ControlLeft, self.args.cmd_as_ctrl));
            }
            std::thread::spawn(move || {
                let Some(id) = link.send_paths(files) else { return };
                if !link.wait_saved(id, std::time::Duration::from_secs(30)) {
                    eprintln!("远端没有确认收到文件，未自动粘贴");
                    return; // 对方没确认（旧版本或出错）：不自动粘贴，用户可以手动再按一次
                }
                eprintln!("文件已存到远端，正在远端粘贴");
                let key = |scancode: u16, extended: bool, down: bool| {
                    control.send(&ClientMsg::Input(InputEvent::Key { scancode, extended, down }));
                };
                for &(s, e) in &mods {
                    key(s, e, true);
                }
                key(0x2f, false, true); // V
                key(0x2f, false, false);
                for &(s, e) in mods.iter().rev() {
                    key(s, e, false);
                }
            });
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

        /// 是否拦截本机系统快捷键（窗口在前台、没有交还本机时）。
        fn capture_keys(&mut self, on: bool) {
            let on = on && !self.keys_released;
            if let Some(k) = &mut self.syskeys {
                k.set_active(on);
            }
        }

        /// 在对方那边按一次组合键：先松开对方当前按着的键，按下组合键，再恢复原来按着的键。
        fn send_chord(&self, mods: &[sc::Scan], key: sc::Scan) {
            let key_ev = |(scancode, extended): sc::Scan, down| InputEvent::Key { scancode, extended, down };
            let held: Vec<sc::Scan> = self.keys_down.iter().copied().collect();
            for &k in &held {
                self.send(key_ev(k, false));
            }
            for &m in mods {
                self.send(key_ev(m, true));
            }
            self.send(key_ev(key, true));
            self.send(key_ev(key, false));
            for &m in mods.iter().rev() {
                self.send(key_ev(m, false));
            }
            for &k in &held {
                self.send(key_ev(k, true));
            }
        }

        /// 一个物理键的按下 / 抬起（来自窗口，或 Windows 键盘钩子）。
        fn handle_key(&mut self, code: KeyCode, pressed: bool, repeat: bool) {
            use KeyCode::*;
            if pressed {
                self.local_down.insert(code);
            } else {
                self.local_down.remove(&code);
            }
            let m = sc::Mods::from_keys(&self.local_down);
            let is_mod = matches!(code, ControlLeft | ControlRight | AltLeft | AltRight | ShiftLeft | ShiftRight | SuperLeft | SuperRight);
            let key_ev = |(scancode, extended): sc::Scan, down| InputEvent::Key { scancode, extended, down };

            // “交还本机”：⌃⌥⌘（Windows 上 Ctrl+Alt+Win）同时按住，本机快捷键恢复，点回画面再拦截
            if pressed && !repeat && is_mod && sc::is_release_combo(m) && self.syskeys.is_some() && !self.keys_released {
                self.keys_released = true;
                self.capture_keys(false);
                let click = "点一下远程画面恢复";
                *self.shared.audio_note.lock().unwrap() = Some(format!("已把系统快捷键交还本机（{click}）"));
            }

            // 单按 Win（Windows 本机）：对方是 Mac 时换成 ⌘空格（聚焦搜索）
            if matches!(code, SuperLeft | SuperRight) {
                if pressed && !repeat {
                    self.win_tap = true;
                }
            } else if pressed {
                self.win_tap = false;
            }

            // 切换应用结束：本机松开 ⌘ / Alt，对方松开对应的修饰键
            if !pressed && sc::is_switch_modifier(Os::LOCAL, code) {
                if let Some(target) = self.switching.take() {
                    self.send(key_ev(target, false));
                    if let Some(s) = keymap::to_scancode(code, self.args.cmd_as_ctrl) {
                        self.keys_down.remove(&s);
                    }
                    return;
                }
            }

            // ⌘⇧D（Windows 上 Ctrl+Alt+D）：取回远端复制的文件（本地快捷键，不发给远端）
            let fetch_mods = if cfg!(target_os = "macos") { m.meta && m.shift } else { m.ctrl && m.alt };
            if code == KeyD && fetch_mods {
                if pressed && !repeat {
                    self.swallowed.insert(code);
                    if let Some(d) = &self.data {
                        d.fetch_remote_files();
                    }
                }
                return;
            }
            // 粘贴键（Ctrl+V，Mac 上开启“⌘ 当作 Ctrl”时也包括 ⌘V）：本机刚复制了文件时，
            // 先把文件传到远端（存进“下载”并放进远端剪贴板），传完再在远端按下粘贴
            let paste_mods = (m.ctrl || (cfg!(target_os = "macos") && self.args.cmd_as_ctrl && m.meta)) && !m.shift && !m.alt;
            if code == KeyV && paste_mods && pressed && !repeat {
                if let Some(files) = self.data.as_ref().and_then(|d| d.take_local_files()) {
                    self.swallowed.insert(code);
                    self.paste_files(files);
                    return;
                }
            }
            // ⌃⌥← / ⌃⌥→（Windows 上 Ctrl+Alt+← / →）：切换远端的显示器
            if matches!(code, ArrowLeft | ArrowRight) && m.ctrl && m.alt && !m.meta && self.displays.len() > 1 {
                if pressed && !repeat {
                    self.swallowed.insert(code);
                    let n = self.displays.len() as u32;
                    let next = if code == ArrowRight { (self.current_display + 1) % n } else { (self.current_display + n - 1) % n };
                    self.select_display(next);
                }
                return;
            }

            // 系统快捷键按对方系统的习惯翻译（切应用、切桌面、总览、显示桌面、搜索）
            if pressed {
                if let Some(intent) = sc::detect(Os::LOCAL, code, m) {
                    self.swallowed.insert(code);
                    match intent {
                        sc::Intent::AppSwitch { reverse } => {
                            if self.switching.is_none() {
                                // 松开对方当前按着的修饰键（例如 ⌘ 对应的 Ctrl），改按对方切换应用用的修饰键
                                for k in std::mem::take(&mut self.keys_down) {
                                    self.send(key_ev(k, false));
                                }
                                let target = sc::switch_modifier(self.remote_os);
                                self.send(key_ev(target, true));
                                self.switching = Some(target);
                            }
                            if reverse {
                                self.send(key_ev(sc::SHIFT_SCAN, true));
                            }
                            self.send(key_ev(sc::TAB_SCAN, true));
                            self.send(key_ev(sc::TAB_SCAN, false));
                            if reverse {
                                self.send(key_ev(sc::SHIFT_SCAN, false));
                            }
                        }
                        other if !repeat => {
                            if let Some((mods, key)) = sc::chord(self.remote_os, other) {
                                self.send_chord(&mods, key);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
            }
            if !pressed && self.swallowed.remove(&code) {
                return;
            }
            let Some((scancode, extended)) = keymap::to_scancode(code, self.args.cmd_as_ctrl) else { return };
            if pressed {
                self.keys_down.insert((scancode, extended));
            } else if !self.keys_down.remove(&(scancode, extended)) {
                return; // 没发过按下（例如焦点切换时已统一松开），不补发抬起
            }
            self.send(InputEvent::Key { scancode, extended, down: pressed });

            // 单按 Win 后松开：对方是 Mac 时打开聚焦搜索（对方是 Windows 时上面已原样发过去，开始菜单会自己弹出）
            if !pressed && matches!(code, SuperLeft | SuperRight) && std::mem::take(&mut self.win_tap) && Os::LOCAL == Os::Windows && self.remote_os == Os::Mac {
                if let Some((mods, key)) = sc::chord(self.remote_os, sc::Intent::Search) {
                    self.send_chord(&mods, key);
                }
            }
        }

        /// 让记录的修饰键和系统报告的修饰键状态一致：少了就补按左侧键，多了就补抬起。
        #[cfg(target_os = "macos")]
        fn sync_modifiers(&mut self, state: winit::keyboard::ModifiersState) {
            use KeyCode::*;
            for (on, left, right) in [
                (state.super_key(), SuperLeft, SuperRight),
                (state.control_key(), ControlLeft, ControlRight),
                (state.alt_key(), AltLeft, AltRight),
                (state.shift_key(), ShiftLeft, ShiftRight),
            ] {
                let held: Vec<KeyCode> = [left, right].into_iter().filter(|k| self.local_down.contains(k)).collect();
                if on && held.is_empty() {
                    self.handle_key(left, true, false);
                } else if !on {
                    for k in held {
                        self.handle_key(k, false, false);
                    }
                }
            }
        }

        /// 松开所有按下的键和鼠标键（失去焦点时调用，防止远端卡键）。
        fn release_all(&mut self) {
            if let Some(target) = self.switching.take() {
                self.send(InputEvent::Key { scancode: target.0, extended: target.1, down: false });
            }
            self.local_down.clear();
            self.win_tap = false;
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
                UserEvent::MicPort(port) => {
                    if self.args.mic && self.mic.is_none() {
                        *self.shared.audio_note.lock().unwrap() = None;
                        if let Some(addr) = self.addr {
                            self.start_mic(addr, port);
                        }
                    }
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
                UserEvent::HookKey(code, down) => {
                    let repeat = down && self.local_down.contains(&code);
                    self.handle_key(code, down, repeat);
                }
                UserEvent::Paste => {
                    if let Some(files) = self.data.as_ref().and_then(|d| d.take_local_files()) {
                        self.paste_files(files);
                    }
                }
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
                WindowEvent::Focused(false) => {
                    self.capture_keys(false);
                    self.release_all();
                }
                WindowEvent::Focused(true) => self.capture_keys(true),
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
                    if down && self.keys_released {
                        // 点回远程画面：重新拦截系统快捷键
                        self.keys_released = false;
                        *self.shared.audio_note.lock().unwrap() = None;
                        self.capture_keys(true);
                    }
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
                // 拖进窗口的文件发送到远端“下载”，并放进远端剪贴板
                WindowEvent::DroppedFile(path) => {
                    if let Some(d) = &self.data {
                        d.drop_file(path);
                    }
                }
                // macOS：系统快捷键被关闭（见 syskeys）时，⌘ 等修饰键的 flagsChanged 不带键码，
                // winit 只报修饰键状态、不报按键，这里按状态补上修饰键的按下 / 抬起
                #[cfg(target_os = "macos")]
                WindowEvent::ModifiersChanged(m) => self.sync_modifiers(m.state()),
                WindowEvent::KeyboardInput { event, .. } => {
                    let PhysicalKey::Code(code) = event.physical_key else { return };
                    self.handle_key(code, event.state == ElementState::Pressed, event.repeat);
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

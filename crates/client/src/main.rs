//! 控制端（macOS）：显示远端画面并回传键鼠输入。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("控制端目前只支持 macOS。");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod keymap;
#[cfg(target_os = "macos")]
mod net;

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    app::run()
}

#[cfg(target_os = "macos")]
mod app {
    use crate::{display::VideoSink, keymap, net};
    use anyhow::{Context, Result};
    use clap::Parser;
    use rc_protocol::{ClientMsg, InputEvent};
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
    use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::keyboard::PhysicalKey;
    use winit::platform::macos::EventLoopBuilderExtMacOS;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    #[derive(Parser, Debug)]
    #[command(about = "局域网远程控制 - 控制端")]
    struct Args {
        /// 被控端地址，如 192.168.1.20 或 192.168.1.20:47800
        host: String,
        /// 连接密码（被控端启动时显示）
        #[arg(long, short, env = "RC_PASSWORD")]
        password: String,
        /// 把 Command 键当作 Ctrl 发送（默认当作 Win 键）
        #[arg(long)]
        cmd_as_ctrl: bool,
        /// 每秒在终端打印一行统计信息
        #[arg(long)]
        stats: bool,
        /// 始终按远端原始分辨率传输（默认按本机窗口的实际像素尺寸缩小编码，延迟更低）
        #[arg(long)]
        native: bool,
    }

    enum UserEvent {
        Disconnected,
    }

    struct App {
        args: Args,
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
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        // 告诉系统这是延迟敏感的前台任务：不做 App Nap、不合并定时器。
        let _activity = begin_latency_critical_activity();
        let conn = net::connect(&args.host, &args.password)?;
        println!("已连接，远端画面 {}x{} @ {}fps", conn.width, conn.height, conn.fps);

        let event_loop = EventLoop::<UserEvent>::with_user_event()
            // 不要默认菜单：否则 Cmd+Q / Cmd+W / Cmd+H 会被本地菜单吃掉，发不到远端。
            .with_default_menu(false)
            .build()?;
        let mut app = App {
            video_size: (conn.width, conn.height),
            args,
            conn: Some(conn),
            proxy: event_loop.create_proxy(),
            window: None,
            control: None,
            shared: Arc::new(net::Shared::default()),
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            wheel_acc: (0.0, 0.0),
        };
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    impl App {
        fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
            let conn = self.conn.take().context("重复初始化")?;
            let (vw, vh) = self.video_size;
            // 初始窗口：逻辑宽 1280，保持远端画面比例。
            let size = LogicalSize::new(1280.0, 1280.0 * vh as f64 / vw as f64);
            let attrs = Window::default_attributes().with_title(format!("远程控制 - {}", self.args.host)).with_inner_size(size);
            let window = Arc::new(event_loop.create_window(attrs)?);

            let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
                anyhow::bail!("不是 AppKit 窗口");
            };
            let sink = unsafe { VideoSink::attach(handle.ns_view.as_ptr())? };

            let control = net::Control::new(conn.tcp.try_clone()?);
            self.shared.alive.store(true, Ordering::Relaxed);
            let proxy = self.proxy.clone();
            net::spawn_control_reader(conn.tcp, self.shared.clone(), move || {
                let _ = proxy.send_event(UserEvent::Disconnected);
            });
            net::spawn_pinger(control.clone(), self.shared.clone());
            let (title_window, host, print_stats) = (window.clone(), self.args.host.clone(), self.args.stats);
            let shared = self.shared.clone();
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
                if print_stats {
                    println!("{line}");
                }
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
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            let Some(window) = self.window.clone() else { return };
            match event {
                WindowEvent::CloseRequested => {
                    self.release_all();
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
                WindowEvent::KeyboardInput { event, .. } => {
                    let PhysicalKey::Code(code) = event.physical_key else { return };
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

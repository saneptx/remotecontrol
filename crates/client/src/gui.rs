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

pub fn run() -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("SuperControl")
        .with_inner_size([900.0, 620.0])
        .with_min_inner_size([680.0, 480.0]);
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
        Box::new(|cc| Ok(Box::new(Gui::new(&cc.egui_ctx)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

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
    child: Child,
    /// 子进程 stderr 的最后几行（出错时显示）。
    stderr: Arc<Mutex<Vec<String>>>,
    /// 画面窗口已和对方完成握手（它在 stdout 打印“已连接…”时置位）。
    ready: Arc<AtomicBool>,
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
    awdl_checked: Instant,
    awdl_job: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    host: HostRunner,
    job: Option<Arc<Mutex<Job>>>,
    sessions: Vec<Session>,
    /// 底部提示：(内容, 是否错误, 出现时间)
    message: Option<(String, bool, Instant)>,
    screenshot: Option<(String, Instant)>,
}

impl Gui {
    fn new(ctx: &egui::Context) -> Self {
        kit::apply_theme(ctx);
        kit::load_system_font(ctx);
        let seen = Arc::new(Mutex::new(HashMap::new()));
        spawn_scanner(seen.clone(), ctx.clone());
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
            settings: config::settings(),
            awdl_guard: awdl::enabled(),
            awdl_active: awdl::active(),
            awdl_checked: Instant::now(),
            awdl_job: None,
            host: HostRunner::new(),
            job: None,
            sessions: Vec::new(),
            message: None,
            // 自测：RC_GUI_SCREENSHOT=/路径.png 时，启动 2 秒后截取本窗口并退出
            screenshot: std::env::var("RC_GUI_SCREENSHOT")
                .ok()
                .map(|p| (p, Instant::now())),
        }
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
                Ok(s) => s,
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
                if let Err(e) = r {
                    self.say(format!("设置 AWDL 自动处理失败：{e}"), true);
                }
            }
        }
        if self.awdl_checked.elapsed() > Duration::from_secs(2) {
            self.awdl_checked = Instant::now();
            self.awdl_active = awdl::active();
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
        self.host.tick(self.settings.host);
    }
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
    fn local_card(&self, ui: &mut egui::Ui) {
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
                        let (text, dot) = self.host_status();
                        kit::chip(ui, text, dot);
                    });
                });
        }
    }

    /// 被控端状态：(文字, 圆点颜色)
    fn host_status(&self) -> (&'static str, Color32) {
        if !self.settings.host {
            ("不允许被控制", kit::GRAY)
        } else if self.host.running() {
            ("可被控制", kit::GREEN)
        } else if self.host.error.is_some() || !self.host.installed() {
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
                    });
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
        for row in cards.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for c in row {
                    ui.allocate_ui_with_layout(
                        Vec2::new(card_w, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let (connect, forget_it) = device_card(ui, c, card_w, busy);
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
        if let Some(id) = forget {
            let _ = config::forget(&id);
            self.say("已删除配对信息，下次连接需要重新配对", false);
        }
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
            })
            .collect();
        for k in config::known_hosts() {
            if online.iter().any(|d| proto::hex(&d.host_id) == k.id) {
                continue;
            }
            let addr = k.last_addr.clone().unwrap_or_default();
            let parsed = addr.parse::<SocketAddr>().ok();
            cards.push(CardInfo {
                name: k.name.clone(),
                os: String::new(),
                addr: parsed.map(|a| a.ip().to_string()).unwrap_or(addr),
                online: false,
                paired: true,
                compatible: true,
                target: parsed.map(|a| (proto::parse_hex16(&k.id), a, None)),
                forget_id: Some(k.id.clone()),
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
                    self.settings.host = on;
                    let _ = config::save_settings(&self.settings);
                    self.host.error = None;
                    self.host.next_try = Instant::now();
                }
                kit::chip(ui, text, dot);
            });
            self.host_platform_ui(ui);
            if let Some(e) = &self.host.error {
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
                    "AWDL 是隔空投送、接力使用的无线通道，会让 Wi-Fi 周期性卡顿几十毫秒。只在远程画面打开期间关闭；第一次开启需要输入一次管理员密码",
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

    #[cfg(windows)]
    fn host_platform_ui(&mut self, ui: &mut egui::Ui) {
        use crate::winhost;
        kit::divider(ui);
        let installed = self.host.state.is_some();
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
                kit::chip(ui, winhost::state_text(self.host.state), kit::GRAY);
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
        let (screen, access) = self.host.perms;
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

/// 设备卡片。返回 (点了连接, 点了忘记)。
fn device_card(ui: &mut egui::Ui, c: &CardInfo, width: f32, busy: bool) -> (bool, bool) {
    let (mut connect, mut forget) = (false, false);
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
            if !c.online {
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
            let enabled = !busy && c.compatible && c.target.is_some();
            let hint = if !c.compatible {
                "两台电脑的 SuperControl 版本不一致，请更新到同一版本"
            } else if !c.online {
                "对方当前不在线，将按上次的地址尝试连接"
            } else {
                "打开远程画面"
            };
            let label = if c.online { "连接" } else { "尝试连接" };
            if kit::primary_button(ui, label, enabled)
                .on_hover_text(hint)
                .clicked()
            {
                connect = true;
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
    (connect, forget)
}

impl eframe::App for Gui {
    fn on_exit(&mut self) {
        self.host.stop();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
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
    if !settings.audio {
        cmd.arg("--no-audio");
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let ready = Arc::new(AtomicBool::new(false));
    if let Some(pipe) = child.stdout.take() {
        let ready = ready.clone();
        // 一直读到进程退出，否则管道写满会卡住画面窗口
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                if line.starts_with("已连接") {
                    ready.store(true, Ordering::Relaxed);
                }
            }
        });
    }
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
        child,
        stderr,
        ready,
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

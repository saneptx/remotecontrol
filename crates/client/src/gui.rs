//! 控制端图形界面：发现局域网里的被控端、配对、一键连接、设置。
//!
//! 远程画面窗口在独立进程里运行（`rc-client view <地址>`）：每个连接互不影响，
//! 画面窗口崩溃也不会带走主界面。

use crate::{awdl, config, net};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use rc_protocol::{self as proto, DeviceId};
use std::collections::HashMap;
use std::io::BufRead;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RemoteControl")
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([420.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("RemoteControl", options, Box::new(|cc| Ok(Box::new(Gui::new(&cc.egui_ctx)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
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
}

struct Gui {
    device_name: String,
    seen: Arc<Mutex<HashMap<DeviceId, Seen>>>,
    manual_addr: String,
    settings: config::Settings,
    awdl_guard: bool,
    awdl_active: bool,
    awdl_checked: Instant,
    awdl_job: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    host_enabled: bool,
    job: Option<Arc<Mutex<Job>>>,
    sessions: Vec<Session>,
    message: Option<(String, bool)>,
    screenshot: Option<(String, Instant)>,
}

impl Gui {
    fn new(ctx: &egui::Context) -> Self {
        let seen = Arc::new(Mutex::new(HashMap::new()));
        spawn_scanner(seen.clone(), ctx.clone());
        ctx.set_visuals(egui::Visuals::light());
        Self {
            device_name: config::device_name(),
            seen,
            manual_addr: String::new(),
            settings: config::settings(),
            awdl_guard: awdl::guard_installed(),
            awdl_active: awdl::active(),
            awdl_checked: Instant::now(),
            awdl_job: None,
            #[cfg(target_os = "macos")]
            host_enabled: crate::hostagent::enabled(),
            #[cfg(not(target_os = "macos"))]
            host_enabled: false,
            job: None,
            sessions: Vec::new(),
            message: None,
            // 自测：RC_GUI_SCREENSHOT=/路径.png 时，启动 2 秒后截取本窗口并退出
            screenshot: std::env::var("RC_GUI_SCREENSHOT").ok().map(|p| (p, Instant::now())),
        }
    }

    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| matches!(*j.lock().unwrap(), Job::Connecting { .. } | Job::Pairing { .. }))
    }

    /// 发起连接：未配对时先在本界面完成配对（显示配对码），然后启动远程画面窗口。
    fn connect(&mut self, ctx: &egui::Context, name: String, host_id: Option<DeviceId>, addr: SocketAddr, os: Option<String>) {
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
                    net::HandshakeEvent::PairCode { host_name, code } => {
                        set(Job::Pairing { name: host_name.to_string(), code: code.to_string() })
                    }
                });
                if let Err(e) = result {
                    return set(Job::Failed { name, error: format!("{e:#}") });
                }
            }
            match spawn_viewer(&name, addr, &settings, os.as_deref()) {
                Ok(s) => set(Job::Started(s)),
                Err(e) => set(Job::Failed { name, error: format!("{e:#}") }),
            }
        });
    }

    /// 处理后台任务的结果、回收已退出的画面窗口。
    fn poll(&mut self) {
        if let Some(job) = &self.job {
            let mut guard = job.lock().unwrap();
            match std::mem::replace(&mut *guard, Job::Connecting { name: String::new() }) {
                Job::Started(s) => {
                    self.message = Some((format!("已打开「{}」的远程画面", s.name), false));
                    self.sessions.push(s);
                    drop(guard);
                    self.job = None;
                }
                Job::Failed { name, error } => {
                    self.message = Some((format!("连接「{name}」失败：{error}"), true));
                    drop(guard);
                    self.job = None;
                }
                other => *guard = other,
            }
        }

        let mut ended = Vec::new();
        self.sessions.retain_mut(|s| match s.child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let lines = s.stderr.lock().unwrap();
                    let err = lines.iter().rev().find(|l| l.starts_with("Error")).or(lines.last()).cloned().unwrap_or_default();
                    ended.push(format!("「{}」的连接已断开：{}", s.name, err.trim_start_matches("Error: ")));
                }
                false
            }
            _ => true,
        });
        if let Some(e) = ended.pop() {
            self.message = Some((e, true));
        }

        if let Some(rx) = &self.awdl_job {
            if let Ok(r) = rx.try_recv() {
                self.awdl_job = None;
                self.awdl_guard = awdl::guard_installed();
                if let Err(e) = r {
                    self.message = Some((format!("设置 AWDL 自动处理失败：{e}"), true));
                }
            }
        }
        if self.awdl_checked.elapsed() > Duration::from_secs(2) {
            self.awdl_checked = Instant::now();
            self.awdl_active = awdl::active();
        }
    }

    fn device_list(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let online: Vec<net::Discovered> = {
            let seen = self.seen.lock().unwrap();
            let mut v: Vec<_> = seen.values().filter(|s| now - s.at < Duration::from_secs(8)).map(|s| s.host.clone()).collect();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        };
        // 配对过但当前没扫描到的设备，也列出来（可按上次的地址直接连）
        let offline: Vec<config::KnownHost> = config::known_hosts()
            .into_iter()
            .filter(|k| !online.iter().any(|d| proto::hex(&d.host_id) == k.id))
            .collect();

        if online.is_empty() && offline.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("正在扫描局域网…").weak());
            });
            ui.label(RichText::new("请确认对方已安装并运行被控端，且两台电脑连在同一个局域网。").weak().small());
        }

        let busy = self.busy();
        let mut action: Option<(String, Option<DeviceId>, SocketAddr, Option<String>)> = None;
        let mut forget: Option<String> = None;
        for d in &online {
            let compatible = d.version == proto::PROTOCOL_VERSION;
            let badge = if !compatible {
                "版本不一致，请把两端更新到同一版本"
            } else if d.paired {
                "已配对"
            } else {
                "未配对"
            };
            device_row(ui, &d.name, &d.os, &d.addr.to_string(), badge, d.paired && compatible, |ui| {
                if ui.add_enabled(!busy && compatible, egui::Button::new("连接")).clicked() {
                    action = Some((d.name.clone(), Some(d.host_id), d.addr, Some(d.os.clone())));
                }
                if d.paired && ui.small_button("忘记").on_hover_text("删除本机保存的配对信息").clicked() {
                    forget = Some(proto::hex(&d.host_id));
                }
            });
        }
        for k in &offline {
            let addr = k.last_addr.clone().unwrap_or_default();
            device_row(ui, &k.name, "", &addr, "离线", false, |ui| {
                let parsed = addr.parse::<SocketAddr>().ok();
                if ui.add_enabled(!busy && parsed.is_some(), egui::Button::new("连接")).on_hover_text("按上次的地址尝试连接").clicked() {
                    action = Some((k.name.clone(), proto::parse_hex16(&k.id), parsed.unwrap(), None));
                }
                if ui.small_button("忘记").clicked() {
                    forget = Some(k.id.clone());
                }
            });
        }
        if let Some((name, id, addr, os)) = action {
            self.connect(ui.ctx(), name, id, addr, os);
        }
        if let Some(id) = forget {
            let _ = config::forget(&id);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let swap_label = if cfg!(target_os = "macos") {
            "Command 键当作 Ctrl（⌘C / ⌘V 即复制粘贴）"
        } else {
            "控制 Mac 时 Ctrl 当作 Command（Ctrl+C / Ctrl+V 即复制粘贴）"
        };
        changed |= ui.checkbox(&mut self.settings.cmd_as_ctrl, swap_label).changed();
        changed |= ui.checkbox(&mut self.settings.audio, "播放远端的声音").changed();
        changed |= ui
            .checkbox(&mut self.settings.native, "按原始分辨率传输")
            .on_hover_text("默认按窗口的实际像素尺寸编码：显示效果相同，延迟和带宽更低")
            .changed();
        if changed {
            let _ = config::save_settings(&self.settings);
        }

        if !cfg!(target_os = "macos") {
            return;
        }
        let mut guard = self.awdl_guard;
        let pending = self.awdl_job.is_some();
        ui.horizontal(|ui| {
            let resp = ui
                .add_enabled(!pending, egui::Checkbox::new(&mut guard, "远程期间自动关闭 AWDL"))
                .on_hover_text("AWDL 是隔空投送、接力使用的无线通道，会让 Wi-Fi 周期性停顿几十毫秒。\n开启后只在远程画面打开期间关闭它，其余时间不受影响。\n首次开启需要输入管理员密码。");
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
        });
        if self.awdl_guard && !pending && awdl::guard_outdated() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("已安装的是旧版守护，主界面开着时也会关闭 AWDL。").small().color(Color32::from_rgb(200, 120, 0)));
                if ui.small_button("更新").clicked() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.awdl_job = Some(rx);
                    std::thread::spawn(move || {
                        let _ = tx.send(awdl::set_enabled(true).map_err(|e| format!("{e:#}")));
                    });
                }
            });
        }
        let (text, color) = if self.awdl_active {
            ("当前 AWDL：开启", Color32::from_rgb(200, 120, 0))
        } else {
            ("当前 AWDL：关闭", Color32::from_rgb(40, 140, 60))
        };
        ui.label(RichText::new(text).small().color(color));
    }
}

impl Gui {
    #[cfg(windows)]
    fn host_ui(&mut self, ui: &mut egui::Ui) {
        use crate::winhost;
        let state = winhost::service_state();
        ui.horizontal(|ui| {
            ui.label("允许其他设备控制这台电脑（系统服务）：");
            ui.label(RichText::new(state.unwrap_or("未安装")).strong());
        });
        ui.horizontal(|ui| {
            let (label, args) = if state.is_some() { ("卸载", "uninstall") } else { ("安装并启动", "install") };
            if ui.button(label).on_hover_text("需要管理员权限（会弹出确认）").clicked() {
                if let Err(e) = winhost::run_elevated(args) {
                    self.message = Some((format!("{e:#}"), true));
                }
            }
        });
    }

    #[cfg(target_os = "macos")]
    fn host_ui(&mut self, ui: &mut egui::Ui) {
        use crate::hostagent;
        let mut on = self.host_enabled;
        if ui
            .checkbox(&mut on, "允许其他设备控制这台 Mac")
            .on_hover_text("登录后自动在后台运行被控端。其他电脑首次连接时，这里会弹窗请你确认配对码。")
            .changed()
        {
            match hostagent::set_enabled(on) {
                Ok(()) => self.host_enabled = on,
                Err(e) => self.message = Some((format!("{e:#}"), true)),
            }
        }
        if self.host_enabled {
            ui.label(RichText::new("首次开启需要在系统设置中允许 rc-host 使用以下两项权限，授权后重新开启一次本开关：").small().weak());
            ui.horizontal(|ui| {
                if ui.small_button("录屏与系统录音…").clicked() {
                    let _ = Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture").spawn();
                }
                if ui.small_button("辅助功能…").clicked() {
                    let _ = Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
                }
            });
            // 被控端日志里最近的权限 / 错误提示
            if let Some(line) = hostagent::log_tail(30).into_iter().rev().find(|l| l.contains("权限") || l.contains("失败")) {
                ui.label(RichText::new(line).small().color(Color32::from_rgb(200, 120, 0)));
            }
        }
    }
}

fn device_row(ui: &mut egui::Ui, name: &str, os: &str, addr: &str, badge: &str, good: bool, buttons: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let icon = match os {
                "windows" => "🖥",
                "macos" => "💻",
                _ => "💻",
            };
            ui.label(RichText::new(icon).size(22.0));
            ui.vertical(|ui| {
                ui.label(RichText::new(name).strong().size(15.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(addr).weak().small());
                    let color = if good { Color32::from_rgb(40, 140, 60) } else { Color32::GRAY };
                    ui.label(RichText::new(badge).small().color(color));
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), buttons);
        });
    });
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(18)).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("RemoteControl");
                ui.label(RichText::new(format!("本机：{}", self.device_name)).weak());
                ui.add_space(12.0);

                ui.label(RichText::new("局域网中的设备").strong());
                ui.add_space(4.0);
                self.device_list(ui);

                ui.add_space(12.0);
                ui.label(RichText::new("手动连接").strong());
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.manual_addr).hint_text("IP 地址，如 192.168.1.14").desired_width(240.0);
                    let resp = ui.add(edit);
                    let go = ui.add_enabled(!self.busy() && !self.manual_addr.trim().is_empty(), egui::Button::new("连接")).clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go {
                        match net::parse_addr(self.manual_addr.trim()) {
                            Ok(addr) => {
                                let name = self.manual_addr.trim().to_string();
                                self.connect(&ctx, name, None, addr, None);
                            }
                            Err(e) => self.message = Some((format!("地址无效：{e:#}"), true)),
                        }
                    }
                });

                ui.add_space(12.0);
                ui.label(RichText::new("设置").strong());
                self.settings_ui(ui);

                ui.add_space(12.0);
                ui.label(RichText::new("被控制").strong());
                self.host_ui(ui);

                if !self.sessions.is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new("正在控制").strong());
                    let mut kill = None;
                    for (i, s) in self.sessions.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(&s.name);
                            if ui.small_button("断开").clicked() {
                                kill = Some(i);
                            }
                        });
                    }
                    if let Some(i) = kill {
                        let mut s = self.sessions.remove(i);
                        let _ = s.child.kill();
                        let _ = s.child.wait();
                    }
                }

                if let Some((text, error)) = &self.message {
                    ui.add_space(12.0);
                    let color = if *error { Color32::from_rgb(190, 40, 40) } else { Color32::from_rgb(40, 140, 60) };
                    ui.label(RichText::new(text).color(color));
                }
            });
        });

        // 连接 / 配对进度
        let status = self.job.as_ref().and_then(|j| match &*j.lock().unwrap() {
            Job::Connecting { name } => Some((name.clone(), None)),
            Job::Pairing { name, code } => Some((name.clone(), Some(code.clone()))),
            _ => None,
        });
        if let Some((name, code)) = status {
            egui::Modal::new(egui::Id::unique("connecting")).show(&ctx, |ui| {
                ui.set_width(340.0);
                match code {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("正在连接「{name}」…"));
                        });
                    }
                    Some(code) => {
                        ui.label(RichText::new(format!("与「{name}」配对")).strong().size(16.0));
                        ui.add_space(6.0);
                        ui.label("请在对方屏幕上核对配对码，一致后点“是”：");
                        ui.add_space(6.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(code).monospace().size(36.0).strong());
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("等待对方确认（60 秒内）…").weak());
                        });
                        ui.label(RichText::new("配对一次后，以后可直接连接。").weak().small());
                    }
                }
            });
        }

        if let Some((path, at)) = &self.screenshot {
            if at.elapsed() > Duration::from_secs(2) {
                let path = path.clone();
                self.screenshot = None;
                let c = ctx.clone();
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let _ = save_png(&path, &img);
                    c.send_viewport_cmd(egui::ViewportCommand::Close);
                })));
            }
        }
    }
}

/// 后台每 2 秒扫描一次局域网。
fn spawn_scanner(seen: Arc<Mutex<HashMap<DeviceId, Seen>>>, ctx: egui::Context) {
    std::thread::spawn(move || loop {
        if let Ok(found) = net::discover(Duration::from_millis(700)) {
            let now = Instant::now();
            let mut map = seen.lock().unwrap();
            for host in found {
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
fn spawn_viewer(name: &str, addr: SocketAddr, settings: &config::Settings, host_os: Option<&str>) -> Result<Session> {
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
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
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
    Ok(Session { name: name.to_string(), child, stderr })
}

fn save_png(path: &str, img: &egui::ColorImage) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

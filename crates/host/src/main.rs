//! 被控端：等待控制端连接，推送屏幕画面，注入键鼠输入。

mod source;
mod trust;
#[cfg(not(windows))]
mod testsrc;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(windows)]
mod win;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rc_protocol::{self as proto, video, ClientMsg, HostMsg};
use source::{InputSink, VideoSource};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(about = "局域网远程控制 - 被控端", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    args: Args,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 安装为 Windows 系统服务：开机自启，可控制锁屏、登录界面与 UAC 弹窗（需要管理员权限）
    Install(Args),
    /// 卸载系统服务（需要管理员权限）
    Uninstall,
    /// 检查服务、端口、网络与防火墙状态，排查连不上的问题
    Status,
    /// 列出已配对（可免确认直接连接）的控制端
    Devices,
    /// 删除已配对的控制端（按名称或标识前缀；`--all` 删除全部）
    Forget { query: String },
    /// 由 Windows 服务管理器调用，请勿手动运行
    #[command(hide = true)]
    Service(Args),
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// 监听端口（TCP 控制通道）
    #[arg(long, default_value_t = proto::DEFAULT_PORT)]
    pub port: u16,
    /// 本机在控制端设备列表里显示的名称（默认为计算机名）
    #[arg(long)]
    pub name: Option<String>,
    /// 目标码率（Mbps）。4:4:4 下 1080p 建议 50+，4K 建议 100+
    #[arg(long, default_value_t = 80)]
    pub bitrate: u32,
    /// 最高帧率；默认跟随显示器刷新率
    #[arg(long)]
    pub fps: Option<u32>,
    /// 前向纠错冗余比例（%）。Wi-Fi 下建议 20 以上，有线网络可降到 5～10，0 为关闭
    #[arg(long, default_value_t = 20)]
    pub fec: u32,
    /// 允许系统按电源设置进入睡眠（默认被控端运行期间阻止睡眠，否则睡眠后无法连接）
    #[arg(long)]
    pub allow_sleep: bool,
    /// 测试用：随机丢弃该比例（%）的视频包，模拟网络丢包
    #[arg(long, hide = true, default_value_t = 0.0)]
    pub sim_loss: f64,
}

/// 进程启动以来的微秒数；采集时间戳与 Pong 共用这一时钟。
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn validate(args: &Args) -> Result<()> {
    if args.fps == Some(0) || args.bitrate == 0 {
        bail!("--fps 和 --bitrate 必须大于 0");
    }
    if args.fec > 200 {
        bail!("--fec 不能超过 200");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            validate(&cli.args)?;
            run_host(cli.args)
        }
        #[cfg(windows)]
        Some(Cmd::Install(args)) => {
            validate(&args)?;
            win::service::install(&args)
        }
        #[cfg(windows)]
        Some(Cmd::Uninstall) => win::service::uninstall(),
        #[cfg(windows)]
        Some(Cmd::Status) => win::service::status(),
        #[cfg(windows)]
        Some(Cmd::Service(args)) => win::service::run(args),
        Some(Cmd::Devices) => list_devices(),
        Some(Cmd::Forget { query }) => {
            let store = trust::Store::open(None)?;
            let removed = store.forget(&query)?;
            if removed.is_empty() {
                println!("没有找到匹配「{query}」的设备。运行 rc-host devices 查看列表。");
            }
            for d in removed {
                println!("已删除：{}（{}）", d.name, &d.id[..8]);
            }
            Ok(())
        }
        #[cfg(not(windows))]
        Some(_) => bail!("系统服务只支持 Windows"),
    }
}

pub fn list_devices() -> Result<()> {
    let store = trust::Store::open(None)?;
    let list = store.list();
    println!("本机：{}（{}）", store.host_name(), &proto::hex(&store.host_id())[..8]);
    println!("配置：{}", store.path().display());
    if list.is_empty() {
        println!("还没有已配对的控制端。");
    }
    for d in list {
        println!("  {}  {}", &d.id[..8], d.name);
    }
    Ok(())
}

fn run_host(args: Args) -> Result<()> {
    now_us();
    #[cfg(windows)]
    win::power::init(!args.allow_sleep);
    let store = Arc::new(trust::Store::open(args.name.clone())?);

    let listener = TcpListener::bind(("0.0.0.0", args.port)).with_context(|| format!("监听端口 {} 失败（是否已有被控端或系统服务在运行？）", args.port))?;
    spawn_discovery(store.clone(), args.port);
    println!("被控端「{}」已启动，端口 {}，配置 {}", store.host_name(), args.port, store.path().display());
    println!("等待控制端连接…（新设备首次连接时会在本机弹窗确认）");

    // 每个连接一个线程；同一时间只允许一个控制端，其余的收到“正在被控制”的提示。
    let active: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept 失败: {e}");
                continue;
            }
        };
        let (args, store, active) = (args.clone(), store.clone(), active.clone());
        std::thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            match session(stream, &args, &store, &active) {
                Ok(true) => println!("会话结束 {peer:?}"),
                Ok(false) => {} // 端口探测、仅配对
                Err(e) => eprintln!("会话出错 {peer:?}: {e:#}"),
            }
        });
    }
    Ok(())
}

/// 占用“当前控制端”名额；析构时释放。
struct ActiveSlot(Arc<Mutex<Option<String>>>);

impl ActiveSlot {
    fn claim(active: &Arc<Mutex<Option<String>>>, name: &str) -> Result<Self, String> {
        let mut cur = active.lock().unwrap();
        if let Some(other) = cur.as_ref() {
            return Err(other.clone());
        }
        *cur = Some(name.to_string());
        Ok(Self(active.clone()))
    }
}

impl Drop for ActiveSlot {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

/// 返回 `Ok(false)` 表示对方只是连上就断开（端口探测，例如 `rc-host status`），不记日志。
fn session(mut tcp: TcpStream, args: &Args, store: &trust::Store, active: &Arc<Mutex<Option<String>>>) -> Result<bool> {
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    let peer = tcp.peer_addr()?;

    // ---- 握手 ----
    let hello = match proto::read_msg::<ClientMsg>(&mut tcp) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let ClientMsg::Hello { version, device_id, device_name, client_nonce } = hello else {
        // 旧版本控制端的 Hello 格式不同，解析会落到这里
        let _ = proto::write_msg(&mut tcp, &HostMsg::Reject { reason: "控制端版本过旧，请更新".into() });
        bail!("握手失败：期望 Hello");
    };
    if version != proto::PROTOCOL_VERSION {
        proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("协议版本不匹配（被控端 {}，控制端 {version}），请把两端更新到同一版本", proto::PROTOCOL_VERSION) })?;
        bail!("协议版本不匹配: {version}");
    }
    let device_name = trust::sanitize_name(&device_name);
    let (host_id, host_name) = (store.host_id(), store.host_name().to_string());
    let _slot = match ActiveSlot::claim(active, &device_name) {
        Ok(s) => s,
        Err(other) => {
            let _ = proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("「{other}」正在控制「{host_name}」，请稍后再试") });
            return Ok(false);
        }
    };

    // ---- 未配对：交换临时公钥，弹窗确认 ----
    let pair = |tcp: &mut TcpStream| -> Result<proto::pair::Key> {
        let eph = proto::pair::Ephemeral::generate();
        proto::write_msg(tcp, &HostMsg::PairRequired { host_id, host_name: host_name.clone(), public_key: eph.public })?;
        let ClientMsg::PairKey { public_key } = proto::read_msg(tcp)? else {
            bail!("配对失败：期望 PairKey");
        };
        let Some(key) = eph.derive(&public_key, &host_id, &device_id) else {
            bail!("配对失败：对方公钥无效");
        };
        let code = proto::pair::pair_code(&key);
        println!("配对请求：{device_name}（{}），配对码 {code}", peer.ip());
        if !trust::approve_pairing(&device_name, &peer.ip().to_string(), &code) {
            let _ = proto::write_msg(tcp, &HostMsg::Reject { reason: format!("「{host_name}」拒绝了配对请求（或 60 秒内无人确认）") });
            bail!("配对被拒绝：{device_name}");
        }
        store.add(&device_id, &device_name, &key)?;
        println!("已与「{device_name}」配对");
        Ok(key)
    };
    let mut key = match store.key_for(&device_id) {
        Some(k) => k,
        None => pair(&mut tcp)?,
    };

    // ---- 双向认证 ----
    let mut nonce = proto::random_nonce();
    proto::write_msg(&mut tcp, &HostMsg::Challenge { host_id, host_name: host_name.clone(), nonce })?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut auth = match proto::read_msg::<ClientMsg>(&mut tcp) {
        Ok(m) => m,
        // 控制端界面只做配对、随后断开（真正的画面连接紧接着另开）
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if auth == ClientMsg::RequestPairing {
        // 本机记得对方，但对方丢了配对信息（重装、删除了设备）：重新配对，仍需本机用户确认
        tcp.set_read_timeout(Some(Duration::from_secs(90)))?;
        key = pair(&mut tcp)?;
        nonce = proto::random_nonce();
        proto::write_msg(&mut tcp, &HostMsg::Challenge { host_id, host_name: host_name.clone(), nonce })?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        auth = match proto::read_msg::<ClientMsg>(&mut tcp) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e.into()),
        };
    }
    let ClientMsg::Auth { mac, udp_port } = auth else {
        bail!("握手失败：期望 Auth");
    };
    if !proto::pair::ct_eq(&mac, &proto::pair::client_mac(&key, &nonce, &client_nonce)) {
        std::thread::sleep(Duration::from_secs(1)); // 限制暴力尝试速度
        proto::write_msg(&mut tcp, &HostMsg::Reject { reason: "认证失败：配对信息不一致，请在控制端删除该设备后重新配对".into() })?;
        bail!("认证失败：{device_name}");
    }
    println!("控制端已连接: {device_name}（{peer}）");

    // ---- 打开采集/编码 ----
    let (mut video_src, mut input) = match open_platform(args) {
        Ok(v) => v,
        Err(e) => {
            proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("被控端初始化失败: {e:#}") })?;
            return Err(e);
        }
    };
    let (width, height) = video_src.size();
    let host_proof = proto::pair::host_proof(&key, &nonce, &client_nonce);
    proto::write_msg(&mut tcp, &HostMsg::Welcome { width, height, fps: video_src.fps(), host_proof })?;
    // 会话期间保持显示器常亮
    #[cfg(windows)]
    let _awake = win::power::SessionAwake::new();
    tcp.set_read_timeout(None)?;

    let udp = UdpSocket::bind("0.0.0.0:0")?;
    socket2::SockRef::from(&udp).set_send_buffer_size(4 << 20).ok();
    udp.connect(SocketAddr::new(peer.ip(), udp_port))?;

    // ---- 控制通道读线程：输入事件、关键帧请求、心跳 ----
    let alive = Arc::new(AtomicBool::new(true));
    // 控制端窗口尺寸，打包成 (w << 32 | h)；NO_VIEWPORT 表示还没收到。
    const NO_VIEWPORT: u64 = u64::MAX;
    let viewport = Arc::new(AtomicU64::new(NO_VIEWPORT));
    let want_key = Arc::new(AtomicBool::new(true));
    let writer = Arc::new(Mutex::new(tcp.try_clone()?));
    let reader = {
        let (alive, want_key, writer, viewport) = (alive.clone(), want_key.clone(), writer.clone(), viewport.clone());
        std::thread::spawn(move || {
            loop {
                match proto::read_msg::<ClientMsg>(&mut tcp) {
                    Ok(ClientMsg::Input(ev)) => input.inject(ev),
                    Ok(ClientMsg::Viewport { width, height }) => {
                        viewport.store(((width as u64) << 32) | height as u64, Ordering::Relaxed)
                    }
                    Ok(ClientMsg::RequestKeyframe) => want_key.store(true, Ordering::Relaxed),
                    Ok(ClientMsg::Ping { t_us }) => {
                        let pong = HostMsg::Pong { t_us, host_us: now_us() };
                        if proto::write_msg(&mut *writer.lock().unwrap(), &pong).is_err() {
                            break;
                        }
                    }
                    Ok(_) | Err(_) => break,
                }
            }
            alive.store(false, Ordering::Relaxed);
            // `input` 在这里析构，负责松开所有仍按下的键
        })
    };

    // ---- 视频主循环 ----
    let result = (|| -> Result<()> {
        let mut frame_id = 0u32;
        let mut notice: Option<String> = None;
        let mut loss = SimLoss::new(args.sim_loss);
        // 窗口拖动缩放时会连续收到很多尺寸：稳定 300ms 后才重建编码器。第一次立即生效。
        let (mut applied_vp, mut pending_vp) = (NO_VIEWPORT, NO_VIEWPORT);
        let mut pending_since = Instant::now();
        while alive.load(Ordering::Relaxed) {
            let vp = viewport.load(Ordering::Relaxed);
            if vp != pending_vp {
                pending_vp = vp;
                pending_since = Instant::now();
            }
            let settled = applied_vp == NO_VIEWPORT || pending_since.elapsed() >= Duration::from_millis(300);
            if pending_vp != applied_vp && pending_vp != NO_VIEWPORT && settled {
                video_src.set_viewport((pending_vp >> 32) as u32, pending_vp as u32)?;
                applied_vp = pending_vp;
            }

            let force_key = want_key.swap(false, Ordering::Relaxed);
            let frame = video_src.next_frame(force_key)?;

            let current = video_src.notice();
            if current != notice {
                match &current {
                    Some(t) => println!("提示: {t}"),
                    None => println!("画面已恢复"),
                }
                proto::write_msg(&mut *writer.lock().unwrap(), &HostMsg::Notice { text: current.clone() })?;
                notice = current;
            }

            let Some(frame) = frame else { continue };
            let meta = video::FrameMeta {
                frame_id,
                keyframe: frame.keyframe,
                capture_us: frame.capture_us,
                host_us: now_us().saturating_sub(frame.capture_us).min(u32::MAX as u64) as u32,
            };
            video::packetize(&meta, &frame.data, args.fec, |pkt| {
                if loss.drop() {
                    return Ok(());
                }
                send_datagram(&udp, pkt)
            })?;
            frame_id = frame_id.wrapping_add(1);
        }
        Ok(())
    })();

    // 关闭 TCP 让读线程退出
    let _ = writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    result.map(|()| true)
}

/// 局域网发现应答：收到控制端的广播探测，就单播回复本机信息。
fn spawn_discovery(store: Arc<trust::Store>, port: u16) {
    let os = if cfg!(windows) { "windows" } else { "macos" };
    std::thread::spawn(move || {
        let bind = || -> std::io::Result<UdpSocket> {
            let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
            sock.set_reuse_address(true)?;
            sock.set_broadcast(true)?;
            sock.bind(&SocketAddr::from(([0, 0, 0, 0], proto::DISCOVERY_PORT)).into())?;
            Ok(sock.into())
        };
        let sock = match bind() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("局域网发现不可用（UDP {} 绑定失败）: {e}", proto::DISCOVERY_PORT);
                return;
            }
        };
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            let Some(probe) = proto::discovery::decode_probe(&buf[..n]) else { continue };
            let paired = store.key_for(&probe.device_id).is_some();
            let reply = proto::discovery::Announce::new(store.host_id(), store.host_name().to_string(), os, port, paired);
            let _ = sock.send_to(&proto::discovery::encode(&reply), from);
        }
    });
}

/// 发送一个 UDP 包。发送缓冲区满时短暂退避重试；其余错误（例如对端端口暂不可达）忽略，
/// 会话存活由 TCP 通道判定。
fn send_datagram(udp: &UdpSocket, pkt: &[u8]) -> std::io::Result<()> {
    for _ in 0..50 {
        match udp.send(pkt) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(55) => {
                std::thread::sleep(Duration::from_micros(200));
            }
            _ => break,
        }
    }
    Ok(())
}

/// 测试用的随机丢包（xorshift）。
struct SimLoss {
    threshold: u64,
    state: u64,
}

impl SimLoss {
    fn new(pct: f64) -> Self {
        let seed = u64::from_le_bytes(proto::random_nonce()[..8].try_into().unwrap()) | 1;
        Self { threshold: ((pct.clamp(0.0, 100.0) / 100.0) * u64::MAX as f64) as u64, state: seed }
    }

    fn drop(&mut self) -> bool {
        if self.threshold == 0 {
            return false;
        }
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state < self.threshold
    }
}

#[cfg(windows)]
fn open_platform(args: &Args) -> Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    let src = win::WinSource::new(args)?;
    let input = win::input::Injector::new(src.desktop_rect());
    Ok((Box::new(src), Box::new(input)))
}

#[cfg(not(windows))]
fn open_platform(args: &Args) -> Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    eprintln!("注意：非 Windows 平台，仅提供测试图像源（ffmpeg testsrc2），输入事件只打印不注入。");
    Ok((Box::new(testsrc::TestSource::new(args)?), Box::new(testsrc::LogInput)))
}

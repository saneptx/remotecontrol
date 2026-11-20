//! 被控端：等待控制端连接，推送屏幕画面，注入键鼠输入。

mod audio;
mod clip;
mod mute;
mod netif;
mod source;
mod trust;
#[cfg(not(windows))]
mod testsrc;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(windows)]
mod win;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rc_protocol::{self as proto, video, ClientMsg, HostMsg};
use source::{InputSink, VideoSource};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
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
    /// 安装为 Windows 系统服务：随 SuperControl 界面启动，可控制锁屏、登录界面与 UAC 弹窗（需要管理员权限）
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
    /// 剪贴板助手（由被控端以登录用户身份启动），请勿手动运行
    #[command(hide = true)]
    ClipHelper,
    /// 调试：每半秒打印一次识别到的光标形状
    #[command(hide = true)]
    CursorTest { seconds: u64 },
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// 监听端口（TCP 控制通道）
    #[arg(long, default_value_t = proto::DEFAULT_PORT)]
    pub port: u16,
    /// 本机在控制端设备列表里显示的名称（默认为计算机名）
    #[arg(long)]
    pub name: Option<String>,
    /// 目标码率（Mbps）。默认 40，局域网有线可提高到 60～80 换取更清晰的快速运动画面
    #[arg(long, default_value_t = 40)]
    pub bitrate: u32,
    /// 最高帧率；默认跟随显示器刷新率
    #[arg(long)]
    pub fps: Option<u32>,
    /// 前向纠错冗余比例（%）。Wi-Fi 下建议 20 以上，有线网络可降到 5～10，0 为关闭
    #[arg(long, default_value_t = 20)]
    pub fec: u32,
    /// 被控端运行期间一直阻止系统睡眠。默认按系统电源设置睡眠：有人连接时保持唤醒，
    /// 睡眠后控制端可以用网络唤醒（Wake-on-LAN）叫醒本机
    #[arg(long)]
    pub prevent_sleep: bool,
    /// 旧版参数（现在默认就允许睡眠），保留以兼容旧的启动命令
    #[arg(long, hide = true)]
    pub allow_sleep: bool,
    /// 测试用：随机丢弃该比例（%）的视频包，模拟网络丢包
    #[arg(long, hide = true, default_value_t = 0.0)]
    pub sim_loss: f64,
    /// 由界面程序启动：父进程（界面）退出后本进程也退出
    #[arg(long, hide = true)]
    pub exit_with_parent: bool,
    /// Windows 视频编码器：auto 先试 NVENC，不行再用系统硬件编码器（Intel/AMD）
    #[arg(long, value_enum, default_value_t = EncoderChoice::Auto)]
    pub encoder: EncoderChoice,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EncoderChoice {
    Auto,
    Nvenc,
    /// Media Foundation 硬件编码器
    Mf,
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
        #[cfg(windows)]
        Some(Cmd::ClipHelper) => win::cliphelper::helper_main(),
        Some(Cmd::Devices) => list_devices(),
        #[cfg(target_os = "macos")]
        Some(Cmd::CursorTest { seconds }) => {
            mac::cursor::init();
            let mut c = mac::cursor::Cursors::new();
            for _ in 0..seconds * 2 {
                println!("{:?}", c.current());
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        Some(Cmd::CursorTest { .. }) => bail!("只在 macOS 上可用"),
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
    // 上次会话让扬声器静音后没来得及恢复（崩溃、断电）：先恢复
    mute::recover();
    #[cfg(unix)]
    if args.exit_with_parent {
        // 界面退出（包括崩溃）后父进程变成 launchd，被控端随之退出
        let parent = std::os::unix::process::parent_id();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if std::os::unix::process::parent_id() != parent {
                println!("界面程序已关闭，被控端退出");
                std::process::exit(0);
            }
        });
    }
    #[cfg(windows)]
    win::power::init(args.prevent_sleep);
    #[cfg(target_os = "macos")]
    if !test_source() {
        mac::power::init(args.prevent_sleep);
        mac::request_permissions();
        mac::cursor::init();
    }
    let store = Arc::new(trust::Store::open(args.name.clone())?);

    let listener = TcpListener::bind(("0.0.0.0", args.port)).with_context(|| format!("监听端口 {} 失败（是否已有被控端或系统服务在运行？）", args.port))?;
    spawn_discovery(store.clone(), args.port);
    println!("被控端「{}」已启动，端口 {}，配置 {}", store.host_name(), args.port, store.path().display());
    println!("等待控制端连接…（新设备首次连接时会在本机弹窗确认）");

    // 每个连接一个线程；同一时间只允许一个控制端，其余的收到“正在被控制”的提示。
    let active: ActiveState = Arc::new(Mutex::new(None));
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

/// 询问并安装虚拟声卡；装好后开始把控制端的麦克风放进去，并用 [`HostMsg::MicPort`] 通知控制端。
/// 播放流由本线程持有，会话结束（`alive` 变 false）时停止。
fn offer_cable_install(client: &str, alive: &Arc<AtomicBool>, writer: &Arc<Mutex<TcpStream>>) {
    if !trust::approve_cable_install(client) {
        println!("本机用户暂不安装虚拟声卡");
        return;
    }
    let notice = |text: Option<String>| {
        let _ = proto::write_msg(&mut *writer.lock().unwrap(), &HostMsg::Notice { text });
    };
    notice(Some(format!("对方电脑正在安装虚拟声卡 {}…", rc_audio::install::NAME)));
    // Windows 被控端以 SYSTEM 运行，可以直接安装；直接运行的被控端（以及 macOS）需要提权确认
    let elevated = cfg!(windows) && std::env::var("USERNAME").is_ok_and(|u| u.eq_ignore_ascii_case("SYSTEM") || u.ends_with('$'));
    let started = rc_audio::install::install(elevated).and_then(|()| {
        let (dev, name) = rc_audio::virtual_cable().context("没有找到虚拟声卡")?;
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        let port = sock.local_addr()?.port();
        let stream = rc_audio::player::start(sock, alive.clone(), Some(dev))?;
        println!("虚拟声卡已安装，控制端的麦克风将放进「{name}」");
        Ok((stream, port))
    });
    match started {
        Ok((stream, port)) => {
            notice(None);
            let _ = proto::write_msg(&mut *writer.lock().unwrap(), &HostMsg::MicPort { port });
            while alive.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
            }
            drop(stream);
        }
        Err(e) => {
            eprintln!("安装虚拟声卡失败: {e:#}");
            notice(Some(format!("对方电脑安装虚拟声卡失败：{e:#}")));
            std::thread::sleep(Duration::from_secs(10));
            notice(None);
        }
    }
}

/// 当前的控制端：名称，以及连上之后它的控制通道（写入端），用于本机用户主动断开。
struct Active {
    name: String,
    writer: Option<Arc<Mutex<TcpStream>>>,
}

type ActiveState = Arc<Mutex<Option<Active>>>;

/// 占用“当前控制端”名额；析构时释放。
struct ActiveSlot(ActiveState);

impl ActiveSlot {
    fn claim(active: &ActiveState, name: &str) -> Result<Self, String> {
        let mut cur = active.lock().unwrap();
        if let Some(other) = cur.as_ref() {
            return Err(other.name.clone());
        }
        *cur = Some(Active { name: name.to_string(), writer: None });
        Ok(Self(active.clone()))
    }
}

/// 本机界面发来的请求（只接受本机回环地址）：查询当前控制端，或断开它。
fn local_request(mut tcp: TcpStream, msg: ClientMsg, active: &ActiveState) -> Result<()> {
    match msg {
        ClientMsg::LocalStatus => {
            // 握手完成（已连上）才算在控制
            let controller = active.lock().unwrap().as_ref().filter(|a| a.writer.is_some()).map(|a| a.name.clone());
            proto::write_msg(&mut tcp, &HostMsg::LocalStatus { controller })?;
        }
        ClientMsg::LocalDisconnect => {
            let target = active.lock().unwrap().as_ref().and_then(|a| a.writer.clone().map(|w| (a.name.clone(), w)));
            if let Some((name, writer)) = target {
                let mut w = writer.lock().unwrap();
                let _ = proto::write_msg(&mut *w, &HostMsg::Kicked);
                // 关闭控制通道：会话的读线程随即出错，整个会话结束
                let _ = w.shutdown(std::net::Shutdown::Both);
                println!("本机用户断开了「{name}」的控制");
            }
            proto::write_msg(&mut tcp, &HostMsg::LocalStatus { controller: None })?;
        }
        _ => {}
    }
    Ok(())
}

impl Drop for ActiveSlot {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

/// 返回 `Ok(false)` 表示对方只是连上就断开（端口探测，例如 `rc-host status`），不记日志。
fn session(mut tcp: TcpStream, args: &Args, store: &trust::Store, active: &ActiveState) -> Result<bool> {
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    let peer = tcp.peer_addr()?;

    // ---- 握手 ----
    let hello = match proto::read_msg::<ClientMsg>(&mut tcp) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    // 数据通道：交给对应的会话
    if let ClientMsg::DataHello { token } = hello {
        if let Some(tx) = data_waiters().lock().unwrap().remove(&token) {
            let _ = tx.send(tcp);
        }
        return Ok(false);
    }
    if matches!(hello, ClientMsg::LocalStatus | ClientMsg::LocalDisconnect) {
        if peer.ip().is_loopback() {
            local_request(tcp, hello, active)?;
        }
        return Ok(false);
    }
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
    let ClientMsg::Auth { mac, udp_port, audio_port, codecs, mute_host, mic } = auth else {
        bail!("握手失败：期望 Auth");
    };
    if !proto::pair::ct_eq(&mac, &proto::pair::client_mac(&key, &nonce, &client_nonce)) {
        std::thread::sleep(Duration::from_secs(1)); // 限制暴力尝试速度
        proto::write_msg(&mut tcp, &HostMsg::Reject { reason: "认证失败：配对信息不一致，请在控制端删除该设备后重新配对".into() })?;
        bail!("认证失败：{device_name}");
    }
    println!("控制端已连接: {device_name}（{peer}）");

    // ---- 打开采集/编码 ----
    let (mut video_src, mut input) = match open_platform(args, &codecs) {
        Ok(v) => v,
        Err(e) => {
            proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("被控端初始化失败: {e:#}") })?;
            return Err(e);
        }
    };
    let (width, height) = video_src.size();
    let host_proof = proto::pair::host_proof(&key, &nonce, &client_nonce);
    // 数据通道（剪贴板、文件）：控制端拿 token 另开一条连接
    let data_token = proto::random_nonce();
    let (data_tx, data_rx) = mpsc::channel::<TcpStream>();
    data_waiters().lock().unwrap().insert(data_token, data_tx);
    let data_conn: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    {
        let data_conn = data_conn.clone();
        std::thread::spawn(move || {
            let stream = data_rx.recv_timeout(Duration::from_secs(15));
            data_waiters().lock().unwrap().remove(&data_token);
            if let Ok(stream) = stream {
                let _ = stream.set_read_timeout(None);
                *data_conn.lock().unwrap() = stream.try_clone().ok();
                clip::run(stream);
            }
        });
    }
    // 视频与声音的发送端口在 Welcome 里告诉控制端，供它打洞穿过本机防火墙
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    socket2::SockRef::from(&udp).set_send_buffer_size(4 << 20).ok();
    udp.connect(SocketAddr::new(peer.ip(), udp_port))?;
    let audio_udp = match audio_port {
        0 => None,
        p => {
            let s = UdpSocket::bind("0.0.0.0:0")?;
            s.connect(SocketAddr::new(peer.ip(), p))?;
            Some(s)
        }
    };
    // 控制端的麦克风：放进虚拟声卡（VB-CABLE / BlackHole），本机软件选它当麦克风即可
    let mic_sock = if mic {
        match rc_audio::virtual_cable() {
            Some((dev, name)) => {
                let s = UdpSocket::bind("0.0.0.0:0")?;
                println!("控制端的麦克风将放进「{name}」");
                Some((s, dev))
            }
            None => {
                println!("控制端要传麦克风，但本机没有虚拟声卡（Windows 装 VB-CABLE，Mac 装 BlackHole）");
                None
            }
        }
    } else {
        None
    };
    let welcome = HostMsg::Welcome {
        width,
        height,
        fps: video_src.fps(),
        host_proof,
        data_token,
        video_port: udp.local_addr()?.port(),
        audio_port: audio_udp.as_ref().and_then(|s| s.local_addr().ok()).map(|a| a.port()).unwrap_or(0),
        codec: video_src.codec(),
        mic_port: mic_sock.as_ref().and_then(|(s, _)| s.local_addr().ok()).map(|a| a.port()).unwrap_or(0),
    };
    proto::write_msg(&mut tcp, &welcome)?;
    // 会话期间保持系统和显示器唤醒
    #[cfg(windows)]
    let _awake = win::power::SessionAwake::new();
    #[cfg(target_os = "macos")]
    let _awake = mac::power::SessionAwake::new();
    tcp.set_read_timeout(None)?;

    // ---- 控制通道读线程：输入事件、关键帧请求、心跳 ----
    let alive = Arc::new(AtomicBool::new(true));
    // macOS：会话期间每秒刷新一个标记文件，AWDL 守护（界面里“远程期间自动关闭 AWDL”）据此在本机被控制时也关闭 AWDL，
    // 否则 Wi-Fi 每隔几秒卡顿上百毫秒。标记 5 秒不刷新即视为结束（被控端崩溃也不会一直压着 AWDL）
    #[cfg(target_os = "macos")]
    {
        let alive = alive.clone();
        std::thread::spawn(move || {
            let marker = trust::config_dir().join("host-session");
            while alive.load(Ordering::Relaxed) {
                let _ = std::fs::write(&marker, b"");
                std::thread::sleep(Duration::from_secs(1));
            }
            let _ = std::fs::remove_file(&marker);
        });
    }
    if let Some(a) = audio_udp {
        audio::spawn(a, alive.clone());
    }
    // 会话结束时（本函数返回）恢复扬声器、停止播放麦克风
    let _mute = if mute_host { mute::MuteGuard::engage() } else { None };
    let want_cable = mic && mic_sock.is_none();
    let _mic = mic_sock.and_then(|(s, dev)| match rc_audio::player::start(s, alive.clone(), Some(dev)) {
        Ok(stream) => Some(stream),
        Err(e) => {
            eprintln!("播放控制端麦克风失败: {e:#}");
            None
        }
    });
    // 控制端窗口尺寸，打包成 (w << 32 | h)；NO_VIEWPORT 表示还没收到。
    const NO_VIEWPORT: u64 = u64::MAX;
    let viewport = Arc::new(AtomicU64::new(NO_VIEWPORT));
    let want_key = Arc::new(AtomicBool::new(true));
    // 控制端要求切换的显示器：下标 + 1，0 表示没有请求
    let select = Arc::new(AtomicU32::new(0));
    let writer = Arc::new(Mutex::new(tcp.try_clone()?));
    // 已连上：本机界面可以看到是谁在控制，并能断开它
    if let Some(a) = active.lock().unwrap().as_mut() {
        a.writer = Some(writer.clone());
    }
    // 控制端要传麦克风、本机却没有虚拟声卡：问本机用户要不要装，装好后告诉控制端开始传
    if want_cable {
        let (alive, writer, name) = (alive.clone(), writer.clone(), device_name.clone());
        std::thread::spawn(move || offer_cable_install(&name, &alive, &writer));
    }
    let reader = {
        let (alive, want_key, writer, viewport, select) = (alive.clone(), want_key.clone(), writer.clone(), viewport.clone(), select.clone());
        std::thread::spawn(move || {
            loop {
                match proto::read_msg::<ClientMsg>(&mut tcp) {
                    Ok(ClientMsg::Input(ev)) => input.inject(ev),
                    Ok(ClientMsg::Viewport { width, height }) => {
                        viewport.store(((width as u64) << 32) | height as u64, Ordering::Relaxed)
                    }
                    Ok(ClientMsg::RequestKeyframe) => want_key.store(true, Ordering::Relaxed),
                    Ok(ClientMsg::SelectDisplay { index }) => select.store(index.saturating_add(1), Ordering::Relaxed),
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
        let mut cursor: Option<proto::CursorShape> = None;
        let mut loss = SimLoss::new(args.sim_loss);
        let send_displays = |src: &dyn VideoSource| -> Result<()> {
            let list = src.displays();
            if !list.is_empty() {
                proto::write_msg(&mut *writer.lock().unwrap(), &HostMsg::Displays { list, current: src.current_display() })?;
            }
            Ok(())
        };
        send_displays(&*video_src)?;
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

            let sel = select.swap(0, Ordering::Relaxed);
            if sel > 0 {
                match video_src.select_display(sel - 1) {
                    Ok(()) => {
                        let (w, h) = video_src.size();
                        println!("已切换到显示器 {}（{w}x{h}）", sel);
                        // 新显示器的尺寸可能不同：按控制端窗口重新确定编码分辨率
                        applied_vp = NO_VIEWPORT;
                        want_key.store(true, Ordering::Relaxed);
                    }
                    Err(e) => eprintln!("切换到显示器 {sel} 失败: {e:#}"),
                }
                // 不管成功与否都告诉控制端当前是哪一块，它的界面据此更新
                send_displays(&*video_src)?;
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

            let shape = video_src.cursor();
            if shape.is_some() && shape != cursor {
                proto::write_msg(&mut *writer.lock().unwrap(), &HostMsg::Cursor { shape: shape.unwrap() })?;
                cursor = shape;
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

    // 关闭 TCP 让读线程退出；数据通道一并关闭
    let _ = writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    if let Some(d) = data_conn.lock().unwrap().take() {
        let _ = d.shutdown(std::net::Shutdown::Both);
    }
    let _ = reader.join();
    result.map(|()| true)
}

/// 等待数据通道连接的会话：token → 交付通道。
fn data_waiters() -> &'static Mutex<HashMap<[u8; 16], mpsc::Sender<TcpStream>>> {
    static W: OnceLock<Mutex<HashMap<[u8; 16], mpsc::Sender<TcpStream>>>> = OnceLock::new();
    W.get_or_init(Default::default)
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
            let reply = proto::discovery::Announce::new(store.host_id(), store.host_name().to_string(), os, port, paired, netif::physical_macs());
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
fn open_platform(args: &Args, codecs: &[proto::Codec]) -> Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    let src = win::WinSource::new(args, codecs)?;
    let input = win::input::Injector::new(src.area());
    Ok((Box::new(src), Box::new(input)))
}

/// 测试图像源（ffmpeg testsrc2 + 440 Hz 测试音），用于没有真实屏幕权限时联调：RC_TEST_SOURCE=1。
pub fn test_source() -> bool {
    !cfg!(any(windows, target_os = "macos")) || std::env::var("RC_TEST_SOURCE").is_ok_and(|v| v == "1")
}

#[cfg(not(windows))]
fn open_platform(args: &Args, codecs: &[proto::Codec]) -> Result<(Box<dyn VideoSource>, Box<dyn InputSink>)> {
    #[cfg(target_os = "macos")]
    if !test_source() {
        let src = mac::MacSource::new(args, codecs)?;
        let input = mac::input::Injector::new(src.area());
        return Ok((Box::new(src), Box::new(input)));
    }
    if !codecs.contains(&proto::Codec::Hevc) {
        bail!("测试图像源只能输出 HEVC");
    }
    eprintln!("注意：使用测试图像源（ffmpeg testsrc2），输入事件只打印不注入。");
    Ok((Box::new(testsrc::TestSource::new(args)?), Box::new(testsrc::LogInput)))
}

//! 被控端：等待控制端连接，推送屏幕画面，注入键鼠输入。

mod source;
#[cfg(not(windows))]
mod testsrc;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(windows)]
mod win;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rc_protocol::{self as proto, video, ClientMsg, HostMsg};
use source::{InputSink, VideoSource};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Parser, Debug, Clone)]
#[command(about = "局域网远程控制 - 被控端")]
pub struct Args {
    /// 监听端口（TCP 控制通道）
    #[arg(long, default_value_t = proto::DEFAULT_PORT)]
    port: u16,
    /// 连接密码；不指定则每次启动随机生成
    #[arg(long, env = "RC_PASSWORD")]
    password: Option<String>,
    /// 目标码率（Mbps）。4:4:4 下 1080p 建议 50+，4K 建议 100+
    #[arg(long, default_value_t = 80)]
    pub bitrate: u32,
    /// 最高帧率；默认跟随显示器刷新率
    #[arg(long)]
    pub fps: Option<u32>,
    /// 前向纠错冗余比例（%）。Wi-Fi 下建议 20 以上，有线网络可降到 5～10，0 为关闭
    #[arg(long, default_value_t = 20)]
    fec: u32,
    /// 允许系统按电源设置进入睡眠（默认被控端运行期间阻止睡眠，否则睡眠后无法连接）
    #[arg(long)]
    allow_sleep: bool,
    /// 测试用：随机丢弃该比例（%）的视频包，模拟网络丢包
    #[arg(long, hide = true, default_value_t = 0.0)]
    sim_loss: f64,
}

/// 进程启动以来的微秒数；采集时间戳与 Pong 共用这一时钟。
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.fps == Some(0) || args.bitrate == 0 {
        bail!("--fps 和 --bitrate 必须大于 0");
    }
    if args.fec > 200 {
        bail!("--fec 不能超过 200");
    }
    now_us();
    #[cfg(windows)]
    win::power::init(!args.allow_sleep);
    let password = args.password.clone().unwrap_or_else(|| {
        // 去掉易混淆字符的 8 位随机口令
        const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
        proto::random_nonce()[..8].iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
    });

    let listener = TcpListener::bind(("0.0.0.0", args.port)).with_context(|| format!("监听端口 {} 失败", args.port))?;
    println!("被控端已启动，端口 {}，连接密码: {}", args.port, password);
    println!("等待控制端连接…");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept 失败: {e}");
                continue;
            }
        };
        let peer = stream.peer_addr().ok();
        match session(stream, &args, &password) {
            Ok(()) => println!("会话结束 {peer:?}"),
            Err(e) => eprintln!("会话出错 {peer:?}: {e:#}"),
        }
    }
    Ok(())
}

fn session(mut tcp: TcpStream, args: &Args, password: &str) -> Result<()> {
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    let peer = tcp.peer_addr()?;

    // ---- 握手 ----
    let ClientMsg::Hello { version, client_nonce } = proto::read_msg(&mut tcp)? else {
        bail!("握手失败：期望 Hello");
    };
    if version != proto::PROTOCOL_VERSION {
        proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("协议版本不匹配（被控端 {}）", proto::PROTOCOL_VERSION) })?;
        bail!("协议版本不匹配: {version}");
    }
    let nonce = proto::random_nonce();
    proto::write_msg(&mut tcp, &HostMsg::Challenge { nonce })?;
    let ClientMsg::Auth { mac, udp_port } = proto::read_msg(&mut tcp)? else {
        bail!("握手失败：期望 Auth");
    };
    if !proto::verify_mac(password, &nonce, &client_nonce, &mac) {
        std::thread::sleep(Duration::from_secs(1)); // 限制暴力尝试速度
        proto::write_msg(&mut tcp, &HostMsg::Reject { reason: "密码错误".into() })?;
        bail!("密码错误");
    }
    println!("控制端已连接: {peer}");

    // ---- 打开采集/编码 ----
    let (mut video_src, mut input) = match open_platform(args) {
        Ok(v) => v,
        Err(e) => {
            proto::write_msg(&mut tcp, &HostMsg::Reject { reason: format!("被控端初始化失败: {e:#}") })?;
            return Err(e);
        }
    };
    let (width, height) = video_src.size();
    proto::write_msg(&mut tcp, &HostMsg::Welcome { width, height, fps: video_src.fps() })?;
    // 会话期间保持显示器常亮
    #[cfg(windows)]
    let _awake = win::power::SessionAwake::new();
    tcp.set_read_timeout(None)?;

    let udp = UdpSocket::bind("0.0.0.0:0")?;
    socket2::SockRef::from(&udp).set_send_buffer_size(4 << 20).ok();
    udp.connect(SocketAddr::new(peer.ip(), udp_port))?;

    // ---- 控制通道读线程：输入事件、关键帧请求、心跳 ----
    let alive = Arc::new(AtomicBool::new(true));
    let want_key = Arc::new(AtomicBool::new(true));
    let writer = Arc::new(Mutex::new(tcp.try_clone()?));
    let reader = {
        let (alive, want_key, writer) = (alive.clone(), want_key.clone(), writer.clone());
        std::thread::spawn(move || {
            loop {
                match proto::read_msg::<ClientMsg>(&mut tcp) {
                    Ok(ClientMsg::Input(ev)) => input.inject(ev),
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
        while alive.load(Ordering::Relaxed) {
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
    result
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

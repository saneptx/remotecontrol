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
    /// 最高帧率
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
}

/// 进程启动以来的微秒数；采集时间戳与 Pong 共用这一时钟。
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.fps == 0 || args.bitrate == 0 {
        bail!("--fps 和 --bitrate 必须大于 0");
    }
    now_us();
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
    proto::write_msg(&mut tcp, &HostMsg::Welcome { width, height, fps: args.fps })?;
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
        while alive.load(Ordering::Relaxed) {
            let force_key = want_key.swap(false, Ordering::Relaxed);
            let Some(frame) = video_src.next_frame(force_key)? else { continue };
            video::packetize(frame_id, frame.keyframe, frame.capture_us, &frame.data, |pkt| send_datagram(&udp, pkt))?;
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

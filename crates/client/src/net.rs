//! 连接、握手，以及视频接收 / 心跳线程。

use crate::display::VideoSink;
use anyhow::{bail, Context, Result};
use rc_protocol::{self as proto, video, ClientMsg, HostMsg};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// 已完成握手的连接。
pub struct Connection {
    pub tcp: TcpStream,
    pub udp: UdpSocket,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub fn connect(host: &str, password: &str) -> Result<Connection> {
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:{}", proto::DEFAULT_PORT) };
    let addr = addr.to_socket_addrs()?.next().context("无法解析被控端地址")?;
    let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).with_context(|| format!("连接 {addr} 失败"))?;
    tcp.set_nodelay(true)?;
    // 被控端初始化编码器可能要一两秒
    tcp.set_read_timeout(Some(Duration::from_secs(15)))?;

    // 视频帧是突发到达的（一帧上百个包），接收缓冲区要足够大。
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    socket2::SockRef::from(&udp).set_recv_buffer_size(8 << 20).ok();
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;

    let client_nonce = proto::random_nonce();
    proto::write_msg(&mut tcp, &ClientMsg::Hello { version: proto::PROTOCOL_VERSION, client_nonce })?;
    let nonce = match proto::read_msg(&mut tcp)? {
        HostMsg::Challenge { nonce } => nonce,
        HostMsg::Reject { reason } => bail!("被控端拒绝连接: {reason}"),
        other => bail!("握手失败: {other:?}"),
    };
    let mac = proto::auth_mac(password, &nonce, &client_nonce);
    proto::write_msg(&mut tcp, &ClientMsg::Auth { mac, udp_port: udp.local_addr()?.port() })?;
    match proto::read_msg(&mut tcp)? {
        HostMsg::Welcome { width, height, fps } => {
            tcp.set_read_timeout(None)?;
            Ok(Connection { tcp, udp, width, height, fps })
        }
        HostMsg::Reject { reason } => bail!("被控端拒绝连接: {reason}"),
        other => bail!("握手失败: {other:?}"),
    }
}

/// 控制通道的写端，多线程共享。
#[derive(Clone)]
pub struct Control(Arc<Mutex<TcpStream>>);

impl Control {
    pub fn new(tcp: TcpStream) -> Self {
        Self(Arc::new(Mutex::new(tcp)))
    }

    pub fn send(&self, msg: &ClientMsg) -> bool {
        proto::write_msg(&mut *self.0.lock().unwrap(), msg).is_ok()
    }
}

/// 各线程共享的运行状态。
#[derive(Default)]
pub struct Shared {
    pub alive: AtomicBool,
    /// 被控端时钟 - 本地时钟（微秒），由心跳估算。
    clock_offset_us: AtomicI64,
    rtt_us: AtomicU64,
}

/// 读控制通道：处理 Pong 估算时钟差；连接断开时调用 `on_closed`。
pub fn spawn_control_reader(mut tcp: TcpStream, shared: Arc<Shared>, on_closed: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let mut best_rtt = u64::MAX;
        while let Ok(msg) = proto::read_msg::<HostMsg>(&mut tcp) {
            if let HostMsg::Pong { t_us, host_us } = msg {
                let rtt = now_us().saturating_sub(t_us);
                shared.rtt_us.store(rtt, Ordering::Relaxed);
                // 往返最短的那次样本最接近“对称延迟”假设，用它估算时钟差；
                // 让最优值缓慢老化，以跟上两端时钟的漂移。
                best_rtt = best_rtt.saturating_add(best_rtt / 50 + 1);
                if rtt <= best_rtt {
                    best_rtt = rtt;
                    let offset = host_us as i64 - (t_us + rtt / 2) as i64;
                    shared.clock_offset_us.store(offset, Ordering::Relaxed);
                }
            }
        }
        shared.alive.store(false, Ordering::Relaxed);
        on_closed();
    });
}

pub fn spawn_pinger(control: Control, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        while shared.alive.load(Ordering::Relaxed) && control.send(&ClientMsg::Ping { t_us: now_us() }) {
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

pub struct Stats {
    pub fps: u32,
    pub mbps: f64,
    /// 采集 → 提交解码 的平均耗时（毫秒）。
    pub latency_ms: f64,
    pub rtt_ms: f64,
    pub lost: u32,
}

/// 视频接收线程：UDP → 重组 → 解码显示。每秒回调一次统计信息。
pub fn spawn_video_receiver(
    udp: UdpSocket,
    mut sink: VideoSink,
    control: Control,
    shared: Arc<Shared>,
    mut on_stats: impl FnMut(Stats) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut reasm = video::Reassembler::new();
        let mut buf = [0u8; 2048];
        let mut last_key_request = Instant::now() - Duration::from_secs(1);
        let mut request_key = |force: bool| {
            // 节流：关键帧很大，重复请求只会加重拥塞。
            if force || last_key_request.elapsed() > Duration::from_millis(200) {
                last_key_request = Instant::now();
                control.send(&ClientMsg::RequestKeyframe);
            }
        };

        let (mut frames, mut bytes, mut lost, mut latency_sum) = (0u32, 0usize, 0u32, 0i64);
        let mut window_start = Instant::now();

        while shared.alive.load(Ordering::Relaxed) {
            match udp.recv(&mut buf) {
                Ok(n) => {
                    bytes += n;
                    match reasm.push(&buf[..n]) {
                        video::Output::Frame(frame) => {
                            if sink.push(&frame.data) {
                                frames += 1;
                                let host_now = now_us() as i64 + shared.clock_offset_us.load(Ordering::Relaxed);
                                latency_sum += host_now - frame.capture_us as i64;
                            } else {
                                request_key(false);
                            }
                        }
                        video::Output::Lost => {
                            lost += 1;
                            request_key(false);
                        }
                        video::Output::Pending => {}
                    }
                }
                // 读超时：若有凑不齐的帧，说明尾包丢了且后面没有新帧（画面静止），主动要关键帧。
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if reasm.on_timeout() {
                        lost += 1;
                        request_key(true);
                    }
                }
                Err(_) => {}
            }

            let elapsed = window_start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                on_stats(Stats {
                    fps: (frames as f64 / elapsed.as_secs_f64()).round() as u32,
                    mbps: bytes as f64 * 8.0 / 1e6 / elapsed.as_secs_f64(),
                    latency_ms: if frames > 0 { latency_sum as f64 / frames as f64 / 1000.0 } else { 0.0 },
                    rtt_ms: shared.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                    lost,
                });
                (frames, bytes, lost, latency_sum) = (0, 0, 0, 0);
                window_start = Instant::now();
            }
        }
    });
}

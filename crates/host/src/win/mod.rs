//! Windows 被控端：DXGI Desktop Duplication 采集 → NVENC HEVC 编码，全程显存内零拷贝。

mod capture;
mod cursor;
pub mod cliphelper;
mod desktop;
pub mod dialog;
pub mod input;
mod nvenc;
pub mod power;
mod scale;
pub mod service;

use crate::source::{encode_size, EncodedFrame, VideoSource};
use crate::{now_us, Args};
use anyhow::{bail, Result};
use capture::{Capturer, State};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{ID3D11Query, D3D11_QUERY_DESC, D3D11_QUERY_EVENT};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 采集 →（缩放）→ 编码管线。字段按声明顺序析构：编码器必须先于它引用的纹理/设备释放。
struct Pipeline {
    enc: nvenc::Encoder,
    scaler: Option<scale::Scaler>,
    /// GPU 栅栏：等拷贝/缩放真正执行完，用于分段计时。
    fence: ID3D11Query,
    cap: Capturer,
}

impl Pipeline {
    fn open(fps: Option<u32>, bitrate: u32, viewport: (u32, u32)) -> Result<(Self, u32)> {
        let cap = Capturer::new()?;
        let fps = fps.unwrap_or(cap.refresh_hz).clamp(1, 240);
        let (scaler, enc) = Self::make_encoder(&cap, fps, bitrate, viewport)?;
        let mut fence = None;
        unsafe { cap.device().CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut fence))? };
        Ok((Self { enc, scaler, fence: fence.unwrap(), cap }, fps))
    }

    /// 按控制端窗口尺寸创建编码器；需要缩小时在前面接一个显卡缩放器。
    fn make_encoder(cap: &Capturer, fps: u32, bitrate: u32, viewport: (u32, u32)) -> Result<(Option<scale::Scaler>, nvenc::Encoder)> {
        let src = (cap.width, cap.height);
        let (w, h) = encode_size(src, viewport);
        if (w, h) != src {
            match scale::Scaler::new(cap.device(), cap.context(), cap.target(), src, (w, h), fps) {
                Ok(scaler) => {
                    let enc = nvenc::Encoder::new(cap.device(), scaler.output(), w, h, fps, bitrate)?;
                    println!("编码分辨率 {w}x{h}（采集 {}x{}，按控制端窗口缩小）", src.0, src.1);
                    return Ok((Some(scaler), enc));
                }
                Err(e) => eprintln!("创建缩放器失败，改用原始分辨率: {e:#}"),
            }
        }
        println!("编码分辨率 {}x{}（原始）", src.0, src.1);
        Ok((None, nvenc::Encoder::new(cap.device(), cap.target(), src.0, src.1, fps, bitrate)?))
    }

    fn resize(&mut self, fps: u32, bitrate: u32, viewport: (u32, u32)) -> Result<()> {
        let (scaler, enc) = Self::make_encoder(&self.cap, fps, bitrate, viewport)?;
        self.enc = enc; // 旧编码器在这里析构，随后才替换它引用的旧缩放纹理
        self.scaler = scaler;
        Ok(())
    }

    fn encode(&mut self, force_idr: bool, prof: &mut Profile) -> Result<(Vec<u8>, bool)> {
        let t0 = Instant::now();
        if let Some(s) = &self.scaler {
            s.process()?;
        }
        // 等前面排队的 GPU 工作（采集拷贝 + 缩放）执行完。反正编码也要等它们，这里只是把时间分开统计。
        unsafe {
            let ctx = self.cap.context();
            ctx.End(&self.fence);
            ctx.Flush();
            let deadline = Instant::now() + Duration::from_millis(100);
            loop {
                let mut done = windows::core::BOOL(0);
                let ok = ctx.GetData(&self.fence, Some((&mut done as *mut windows::core::BOOL).cast()), 4, 0).is_ok();
                if (ok && done.as_bool()) || Instant::now() > deadline {
                    break;
                }
                std::thread::yield_now();
            }
        }
        let t1 = Instant::now();
        let out = self.enc.encode(force_idr)?;
        prof.scale += t1 - t0;
        prof.encode += t1.elapsed();
        Ok(out)
    }
}

pub struct WinSource {
    inner: Option<Pipeline>,
    fps_arg: Option<u32>,
    bitrate: u32,
    width: u32,
    height: u32,
    rect: (i32, i32, u32, u32),
    fps: u32,
    /// 两次编码之间的最小间隔（略小于 1/fps，避免和显示器刷新节奏“擦边”而多等一帧）。
    min_interval: Duration,
    last_encode: Instant,
    has_image: bool,
    /// 尚未编码的最早一次画面更新的呈现时刻（QPC）。
    dirty_since: Option<i64>,
    pending_key: bool,
    notice: Option<String>,
    last_rebuild: Instant,
    /// 控制端窗口像素尺寸；(0, 0) 表示原始分辨率。
    viewport: (u32, u32),
    prof: Profile,
    cursors: cursor::Cursors,
}

/// 被控端分段耗时，每 5 秒写一行日志，用于定位延迟。
struct Profile {
    since: Instant,
    frames: u32,
    /// 画面更新 → 开始编码
    wait: Duration,
    scale: Duration,
    encode: Duration,
}

impl Profile {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, wait: Duration::ZERO, scale: Duration::ZERO, encode: Duration::ZERO }
    }

    fn maybe_log(&mut self) {
        let elapsed = self.since.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        if self.frames > 0 {
            let avg = |d: Duration| d.as_secs_f64() * 1000.0 / self.frames as f64;
            println!(
                "性能: {:.0} fps，等待 {:.1} ms，GPU 拷贝+缩放 {:.1} ms，编码 {:.1} ms",
                self.frames as f64 / elapsed.as_secs_f64(),
                avg(self.wait),
                avg(self.scale),
                avg(self.encode)
            );
        }
        *self = Self::new();
    }
}

impl WinSource {
    pub fn new(args: &Args) -> Result<Self> {
        // 显示器可能已关闭（甚至从系统中断开）：先唤醒，再给它几秒钟重新出现。
        power::wake_display();
        let deadline = Instant::now() + Duration::from_secs(5);
        let (pipe, fps) = loop {
            match Pipeline::open(args.fps, args.bitrate, (0, 0)) {
                Ok(v) => break v,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("初始化采集失败，重试中: {e:#}");
                    std::thread::sleep(Duration::from_millis(300));
                    power::wake_display();
                }
                Err(e) => return Err(e),
            }
        };
        let cap = &pipe.cap;
        println!("采集 {}x{} @ {}Hz，NVENC HEVC 4:2:0，{} Mbps，上限 {} fps", cap.width, cap.height, cap.refresh_hz, args.bitrate, fps);
        let min_interval = Duration::from_secs_f64(0.9 / fps as f64);
        Ok(Self {
            width: cap.width,
            height: cap.height,
            rect: (cap.left, cap.top, cap.width, cap.height),
            inner: Some(pipe),
            fps_arg: args.fps,
            bitrate: args.bitrate,
            fps,
            min_interval,
            last_encode: Instant::now() - min_interval,
            has_image: false,
            dirty_since: None,
            pending_key: true,
            notice: None,
            last_rebuild: Instant::now(),
            viewport: (0, 0),
            prof: Profile::new(),
            cursors: cursor::Cursors::new(),
        })
    }

    /// 被捕获屏幕在虚拟桌面中的位置与尺寸 (left, top, width, height)。
    pub fn desktop_rect(&self) -> (i32, i32, u32, u32) {
        self.rect
    }

    /// 采集管线失效后定期尝试重建。
    fn try_rebuild(&mut self) -> Result<()> {
        if self.last_rebuild.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(50));
            return Ok(());
        }
        self.last_rebuild = Instant::now();
        power::wake_display();
        match Pipeline::open(self.fps_arg, self.bitrate, self.viewport) {
            Ok((p, _)) => {
                if (p.cap.width, p.cap.height) != (self.width, self.height) {
                    bail!("屏幕分辨率已变化（{}x{} → {}x{}），请重新连接", self.width, self.height, p.cap.width, p.cap.height);
                }
                println!("采集管线已重建");
                self.inner = Some(p);
                self.pending_key = true;
                self.has_image = false;
                self.notice = None;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.notice = Some(if msg.contains("锁屏") { msg } else { "远端显示器不可用（可能已关闭），正在重试…".into() });
            }
        }
        Ok(())
    }
}

/// QPC 时间点 → [`now_us`] 时钟。两者在 Windows 上同源，这里只换算“距今多久”。
fn qpc_to_us(qpc: i64) -> u64 {
    let (mut now, mut freq) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut now);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    let age_us = if freq > 0 { ((now - qpc).max(0) as i128 * 1_000_000 / freq as i128) as u64 } else { 0 };
    now_us().saturating_sub(age_us)
}

impl VideoSource for WinSource {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn fps(&self) -> u32 {
        self.fps
    }

    fn notice(&self) -> Option<String> {
        self.notice.clone()
    }

    fn cursor(&mut self) -> Option<rc_protocol::CursorShape> {
        Some(self.cursors.current())
    }

    fn set_viewport(&mut self, width: u32, height: u32) -> Result<()> {
        if self.viewport == (width, height) {
            return Ok(());
        }
        self.viewport = (width, height);
        let Some(p) = self.inner.as_mut() else { return Ok(()) };
        if let Err(e) = p.resize(self.fps, self.bitrate, self.viewport) {
            // 新编码器建不起来时整条管线重建（try_rebuild 会按新尺寸来）。
            eprintln!("调整编码分辨率失败: {e:#}");
            self.inner = None;
        }
        self.pending_key = true;
        Ok(())
    }

    fn next_frame(&mut self, force_key: bool) -> Result<Option<EncodedFrame>> {
        self.pending_key |= force_key;
        let Some(p) = self.inner.as_mut() else {
            self.try_rebuild()?;
            return Ok(None);
        };

        // 有待编码的内容时只等到允许编码的时刻；否则等下一次画面更新（至多一帧，
        // 以便及时响应关键帧请求）。新画面一到立即编码，不再先睡一整帧。
        let deadline = self.last_encode + self.min_interval;
        let has_work = self.has_image && (self.dirty_since.is_some() || self.pending_key);
        let wait = if has_work { deadline.saturating_duration_since(Instant::now()) } else { self.min_interval };
        let timeout_ms = wait.as_micros().div_ceil(1000) as u32;

        if let Some(present) = p.cap.acquire(timeout_ms)? {
            self.has_image = true;
            // 多次更新合并成一帧时，按最早那次计时：它等得最久，代表真实的响应延迟。
            self.dirty_since.get_or_insert(present);
        }

        self.notice = match p.cap.state() {
            State::Ok => None,
            State::SecureDesktop => Some("远端处于锁屏或 UAC 界面，当前权限无法采集和操作（用 rc-host install 安装为系统服务即可解决）".into()),
            State::Unavailable => Some("远端显示器不可用（可能已关闭），正在重试…".into()),
        };
        if p.cap.needs_rebuild() {
            eprintln!("采集中断，重建采集管线");
            self.inner = None; // 先析构编码器，再析构采集
            self.dirty_since = None;
            return Ok(None);
        }

        let has_work = self.has_image && (self.dirty_since.is_some() || self.pending_key);
        if !has_work || Instant::now() < deadline {
            return Ok(None);
        }

        let capture_us = self.dirty_since.take().map(qpc_to_us).unwrap_or_else(now_us);
        self.last_encode = Instant::now();
        self.prof.frames += 1;
        self.prof.wait += Duration::from_micros(now_us().saturating_sub(capture_us));
        let (data, keyframe) = p.encode(self.pending_key, &mut self.prof)?;
        self.prof.maybe_log();
        self.pending_key = false;
        Ok(Some(EncodedFrame { data, keyframe, capture_us }))
    }
}

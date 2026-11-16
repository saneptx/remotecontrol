//! macOS 被控端：ScreenCaptureKit 采集 → VideoToolbox HEVC 编码；CGEvent 注入输入。
//!
//! 需要两项系统权限：“屏幕录制”（采集画面和声音）与“辅助功能”（操作键鼠）。

pub mod clipboard;
pub mod input;
pub mod power;
mod vt;

use crate::source::{encode_size, Area, EncodedFrame, VideoSource};
use crate::{now_us, Args};
use anyhow::{bail, Context, Result};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::prelude::*;
use screencapturekit::CVPixelBuffer;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsBuiltin(id: u32) -> u32;
    fn CGDisplayCopyDisplayMode(id: u32) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: *mut c_void) -> f64;
    fn CGDisplayModeRelease(mode: *mut c_void);
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(a: *const c_void, k: *const *const c_void, v: *const *const c_void, n: isize, kcb: *const c_void, vcb: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const NEED_SCREEN: &str = "这台 Mac 没有授予“屏幕录制”权限：请在 系统设置 → 隐私与安全性 → 录屏与系统录音 中允许 SuperControl，然后重新连接";
const NEED_ACCESSIBILITY: &str = "这台 Mac 没有授予“辅助功能”权限，只能看不能操作：请在 系统设置 → 隐私与安全性 → 辅助功能 中允许 SuperControl";

/// 启动时检查权限，缺少时触发系统的授权提示。
pub fn request_permissions() {
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            eprintln!("{NEED_SCREEN}");
            CGRequestScreenCaptureAccess();
        }
        if !AXIsProcessTrusted() {
            eprintln!("{NEED_ACCESSIBILITY}");
            let (k, v) = (kAXTrustedCheckOptionPrompt, kCFBooleanTrue);
            let opts = CFDictionaryCreate(std::ptr::null(), &k, &v, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
            AXIsProcessTrustedWithOptions(opts);
            CFRelease(opts);
        }
    }
}

/// 显示器的像素尺寸与刷新率。
fn display_mode(id: u32) -> (u32, u32, u32) {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            return (1920, 1080, 60);
        }
        let r = (
            CGDisplayModeGetPixelWidth(mode) as u32,
            CGDisplayModeGetPixelHeight(mode) as u32,
            (CGDisplayModeGetRefreshRate(mode).round() as u32).clamp(30, 240),
        );
        CGDisplayModeRelease(mode);
        r
    }
}

/// 显示器在全局坐标中的位置（点）：(x, y, 宽, 高)。
fn display_bounds(id: u32) -> (f64, f64, f64, f64) {
    let r = unsafe { input::CGDisplayBounds(id) };
    (r.origin.x, r.origin.y, r.size.w, r.size.h)
}

/// 可采集的显示器：主显示器在前，其余按从左到右排。
fn display_ids(content: &SCShareableContent) -> Vec<u32> {
    let main = unsafe { CGMainDisplayID() };
    let mut ids: Vec<u32> = content.displays().iter().map(|d| d.display_id()).collect();
    ids.sort_by(|a, b| (*a != main).cmp(&(*b != main)).then(display_bounds(*a).0.total_cmp(&display_bounds(*b).0)));
    ids
}

fn shareable() -> Result<SCShareableContent> {
    SCShareableContent::get().map_err(|e| anyhow::anyhow!("{NEED_SCREEN}（{e:?}）"))
}

fn display_filter(content: &SCShareableContent, id: u32) -> Result<SCContentFilter> {
    let displays = content.displays();
    let display = displays.iter().find(|d| d.display_id() == id).context("显示器已断开")?;
    SCContentFilter::create().with_display(display).with_excluding_windows(&[]).build().map_err(|e| anyhow::anyhow!("{e:?}"))
}

type Latest = (Mutex<Option<(CVPixelBuffer, u64)>>, Condvar);

pub struct MacSource {
    stream: SCStream,
    filter: SCContentFilter,
    latest: Arc<Latest>,
    enc: vt::Encoder,
    last: Option<CVPixelBuffer>,
    size: (u32, u32),
    fps: u32,
    fps_arg: Option<u32>,
    bitrate: u32,
    /// 正在采集的显示器（CGDirectDisplayID）
    display: u32,
    /// 控制端窗口像素尺寸；(0, 0) 表示原始分辨率
    viewport: (u32, u32),
    /// 与输入注入共享的当前显示器位置
    area: Area,
    pending_key: bool,
    frame_interval: Duration,
}

fn stream_config(w: u32, h: u32, fps: u32) -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(w)
        .with_height(h)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_minimum_frame_interval(&CMTime::new(1, fps as i32))
        .with_shows_cursor(true)
        .with_queue_depth(4)
}

impl MacSource {
    pub fn new(args: &Args) -> Result<Self> {
        power::wake_display();
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            bail!("{NEED_SCREEN}");
        }
        let content = shareable()?;
        let display = *display_ids(&content).first().context("没有找到显示器")?;
        let (w, h, refresh) = display_mode(display);
        let fps = args.fps.unwrap_or(refresh).clamp(1, 240);
        let filter = display_filter(&content, display)?;

        let latest: Arc<Latest> = Arc::new((Mutex::new(None), Condvar::new()));
        let mut stream = SCStream::new(&filter, &stream_config(w, h, fps)).map_err(|e| anyhow::anyhow!("创建采集流失败: {e:?}"))?;
        {
            let latest = latest.clone();
            stream
                .add_output_handler(
                    move |sample: CMSampleBuffer, ty: SCStreamOutputType| {
                        if ty != SCStreamOutputType::Screen {
                            return;
                        }
                        // 画面没有变化（Idle）的帧不带新内容
                        if !matches!(sample.frame_status(), Some(SCFrameStatus::Complete) | None) {
                            return;
                        }
                        if let Some(pb) = sample.pixel_buffer() {
                            *latest.0.lock().unwrap() = Some((pb, now_us()));
                            latest.1.notify_one();
                        }
                    },
                    SCStreamOutputType::Screen,
                )
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        stream.start_capture().map_err(|e| anyhow::anyhow!("开始采集失败: {e:?}"))?;
        let enc = vt::Encoder::new(w, h, fps, args.bitrate)?;
        println!("采集 {w}x{h} @ {refresh}Hz，VideoToolbox HEVC，{} Mbps，上限 {fps} fps", args.bitrate);
        Ok(Self {
            stream,
            filter,
            latest,
            enc,
            last: None,
            size: (w, h),
            fps,
            fps_arg: args.fps,
            bitrate: args.bitrate,
            display,
            viewport: (0, 0),
            area: Arc::new(Mutex::new(display_bounds(display))),
            pending_key: true,
            frame_interval: Duration::from_secs(1) / fps,
        })
    }
}

impl MacSource {
    /// 当前显示器的位置，交给输入注入使用（切换显示器时随之更新）。
    pub fn area(&self) -> Area {
        self.area.clone()
    }
}

impl VideoSource for MacSource {
    fn displays(&self) -> Vec<rc_protocol::DisplayInfo> {
        let Ok(content) = shareable() else { return Vec::new() };
        let main = unsafe { CGMainDisplayID() };
        display_ids(&content)
            .into_iter()
            .enumerate()
            .map(|(i, id)| {
                let (width, height, _) = display_mode(id);
                let builtin = unsafe { CGDisplayIsBuiltin(id) } != 0;
                rc_protocol::DisplayInfo {
                    name: if builtin { "内建显示器".into() } else { format!("显示器 {}", i + 1) },
                    width,
                    height,
                    primary: id == main,
                }
            })
            .collect()
    }

    fn current_display(&self) -> u32 {
        shareable().map(|c| display_ids(&c).iter().position(|&id| id == self.display).unwrap_or(0) as u32).unwrap_or(0)
    }

    fn select_display(&mut self, index: u32) -> Result<()> {
        let content = shareable()?;
        let id = *display_ids(&content).get(index as usize).context("没有这块显示器")?;
        if id == self.display {
            return Ok(());
        }
        let filter = display_filter(&content, id)?;
        let (w, h, refresh) = display_mode(id);
        let fps = self.fps_arg.unwrap_or(refresh).clamp(1, 240);
        let (ew, eh) = encode_size((w, h), self.viewport);
        self.stream.update_content_filter(&filter).map_err(|e| anyhow::anyhow!("切换显示器失败: {e:?}"))?;
        self.stream.update_configuration(&stream_config(ew, eh, fps)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        self.enc = vt::Encoder::new(ew, eh, fps, self.bitrate)?;
        self.filter = filter;
        self.display = id;
        self.size = (w, h);
        self.fps = fps;
        self.frame_interval = Duration::from_secs(1) / fps;
        self.last = None;
        self.pending_key = true;
        *self.area.lock().unwrap() = display_bounds(id);
        println!("采集显示器 {id}：{w}x{h} @ {refresh}Hz，编码 {ew}x{eh}");
        Ok(())
    }

    // 读不到其他应用光标的可靠形状（系统箭头、I 形光标是动态生成的），所以光标直接画在画面里
    fn cursor(&mut self) -> Option<rc_protocol::CursorShape> {
        Some(rc_protocol::CursorShape::InVideo)
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn fps(&self) -> u32 {
        self.fps
    }

    fn notice(&self) -> Option<String> {
        (!unsafe { AXIsProcessTrusted() }).then(|| NEED_ACCESSIBILITY.to_string())
    }

    /// ScreenCaptureKit 直接按窗口尺寸输出（显卡上缩放，不额外消耗），编码器随之重建。
    fn set_viewport(&mut self, width: u32, height: u32) -> Result<()> {
        self.viewport = (width, height);
        let (w, h) = encode_size(self.size, (width, height));
        if (w, h) == (self.enc.width, self.enc.height) {
            return Ok(());
        }
        self.stream.update_configuration(&stream_config(w, h, self.fps)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        self.enc = vt::Encoder::new(w, h, self.fps, self.bitrate)?;
        self.last = None;
        self.pending_key = true;
        println!("编码分辨率 {w}x{h}（采集 {}x{}）", self.size.0, self.size.1);
        Ok(())
    }

    fn next_frame(&mut self, force_key: bool) -> Result<Option<EncodedFrame>> {
        self.pending_key |= force_key;
        let deadline = Instant::now() + self.frame_interval;
        let mut slot = self.latest.0.lock().unwrap();
        while slot.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            slot = self.latest.1.wait_timeout(slot, left).unwrap().0;
        }
        let fresh = slot.take();
        drop(slot);

        let (pb, capture_us) = match fresh {
            // 缩放切换期间可能还会收到旧尺寸的帧，丢掉
            Some((pb, t)) if pb.width() as u32 == self.enc.width && pb.height() as u32 == self.enc.height => (pb, t),
            Some(_) => return Ok(None),
            None if self.pending_key && self.last.is_some() => (self.last.clone().unwrap(), now_us()),
            None => return Ok(None),
        };
        let out = self.enc.encode(pb.as_ptr(), capture_us, self.pending_key)?;
        self.last = Some(pb);
        let Some((data, keyframe)) = out else { return Ok(None) };
        if keyframe {
            self.pending_key = false;
        }
        Ok(Some(EncodedFrame { data, keyframe, capture_us }))
    }
}

impl Drop for MacSource {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
        let _ = &self.filter;
    }
}

//! Windows 被控端：DXGI Desktop Duplication 采集 → NVENC HEVC 4:4:4 编码，全程显存内零拷贝。

mod capture;
pub mod input;
mod nvenc;

use crate::source::{EncodedFrame, VideoSource};
use crate::{now_us, Args};
use anyhow::Result;
use std::time::{Duration, Instant};

pub struct WinSource {
    // 字段按声明顺序析构：编码器必须先于它引用的纹理/设备释放。
    enc: nvenc::Encoder,
    cap: capture::Capturer,
    frame_interval: Duration,
    last_encode: Instant,
    has_image: bool,
    pending_key: bool,
}

impl WinSource {
    pub fn new(args: &Args) -> Result<Self> {
        let cap = capture::Capturer::new()?;
        let enc = nvenc::Encoder::new(cap.device(), cap.target(), cap.width, cap.height, args.fps, args.bitrate)?;
        println!("采集 {}x{}，NVENC HEVC 4:4:4，{} Mbps，{} fps", cap.width, cap.height, args.bitrate, args.fps);
        let frame_interval = Duration::from_secs(1) / args.fps;
        Ok(Self { enc, cap, frame_interval, last_encode: Instant::now() - frame_interval, has_image: false, pending_key: true })
    }

    /// 被捕获屏幕在虚拟桌面中的位置与尺寸 (left, top, width, height)。
    pub fn desktop_rect(&self) -> (i32, i32, u32, u32) {
        (self.cap.left, self.cap.top, self.cap.width, self.cap.height)
    }
}

impl VideoSource for WinSource {
    fn size(&self) -> (u32, u32) {
        (self.cap.width, self.cap.height)
    }

    fn next_frame(&mut self, force_key: bool) -> Result<Option<EncodedFrame>> {
        self.pending_key |= force_key;

        // 帧率上限：等待期间桌面的更新会被 DXGI 累积，下次取到的就是最新画面。
        let since = self.last_encode.elapsed();
        if since < self.frame_interval {
            std::thread::sleep(self.frame_interval - since);
        }

        let timeout_ms = self.frame_interval.as_millis().max(1) as u32;
        let updated = self.cap.acquire(timeout_ms)?;
        self.has_image |= updated;
        // 画面没变就不编码；但若有待发的关键帧请求，用上一帧的画面重新编码。
        if !self.has_image || !(updated || self.pending_key) {
            return Ok(None);
        }

        let capture_us = now_us();
        self.last_encode = Instant::now();
        let (data, keyframe) = self.enc.encode(self.pending_key)?;
        self.pending_key = false;
        Ok(Some(EncodedFrame { data, keyframe, capture_us }))
    }
}

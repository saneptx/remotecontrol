//! 声音播放：接收对方的 PCM，经抖动缓冲后从本机的输出设备播放。
//!
//! 缓冲目标约 30 ms；积压超过 90 ms 时丢掉最旧的部分，延迟不会越攒越大；
//! 缓冲见底时输出静音并重新攒够目标量再播，避免断断续续的爆音。

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rc_protocol::audio;
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Frame = [f32; 2];

struct Jitter {
    q: VecDeque<Frame>,
    target: usize,
    max: usize,
    primed: bool,
}

impl Jitter {
    fn push(&mut self, frames: &[Frame]) {
        self.q.extend(frames);
        if self.q.len() > self.max {
            let excess = self.q.len() - self.target;
            self.q.drain(..excess);
        }
    }

    fn pop(&mut self) -> Frame {
        if !self.primed {
            if self.q.len() < self.target {
                return [0.0; 2];
            }
            self.primed = true;
        }
        self.q.pop_front().unwrap_or_else(|| {
            self.primed = false;
            [0.0; 2]
        })
    }
}

/// 线性插值重采样（被控端与本机采样率不同时）。
struct Resampler {
    step: f64,
    pos: f64,
    prev: Frame,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: [0.0; 2] }
    }

    fn process(&mut self, input: &[Frame], out: &mut Vec<Frame>) {
        if input.is_empty() {
            return;
        }
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        // 序列：下标 0 为上一批的最后一帧，下标 i+1 为 input[i]
        let at = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        while self.pos + 1.0 <= input.len() as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        self.prev = *input.last().unwrap();
    }
}

/// 开始播放。`device` 为 None 时用默认输出设备。返回的 Stream 需要一直持有（丢弃即停止）。
pub fn start(sock: UdpSocket, alive: Arc<AtomicBool>, device: Option<cpal::Device>) -> Result<cpal::Stream> {
    let device = match device {
        Some(d) => d,
        None => cpal::default_host().default_output_device().context("没有默认的输出设备")?,
    };
    let cfg = device.default_output_config().context("读取输出设备格式")?;
    let (out_rate, out_ch) = (cfg.sample_rate(), cfg.channels() as usize);
    let jitter = Arc::new(Mutex::new(Jitter {
        q: VecDeque::new(),
        target: out_rate as usize * 30 / 1000,
        max: out_rate as usize * 90 / 1000,
        primed: false,
    }));

    {
        let jitter = jitter.clone();
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut resampler: Option<(u32, Resampler)> = None;
            let mut last_seq: Option<u32> = None;
            let mut out = Vec::new();
            // 调试：RC_AUDIO_DEBUG=1 时每秒打印收包数与峰值电平
            let debug = std::env::var("RC_AUDIO_DEBUG").is_ok_and(|v| v == "1");
            let (mut packets, mut peak, mut tick) = (0u32, 0i16, std::time::Instant::now());
            while alive.load(Ordering::Relaxed) {
                if debug && tick.elapsed() >= Duration::from_secs(1) {
                    eprintln!("声音：{packets} 包/秒，峰值 {:.0}%", peak as f32 / 327.67);
                    (packets, peak, tick) = (0, 0, std::time::Instant::now());
                }
                let Ok(n) = sock.recv(&mut buf) else { continue };
                let Some(p) = audio::decode(&buf[..n]) else { continue };
                packets += 1;
                peak = peak.max(p.samples.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
                // 迟到的旧包直接丢掉
                if last_seq.is_some_and(|l| p.seq.wrapping_sub(l) > u32::MAX / 2) {
                    continue;
                }
                last_seq = Some(p.seq);
                let ch = p.channels as usize;
                let frames: Vec<Frame> = p
                    .samples
                    .chunks_exact(ch)
                    .map(|f| {
                        let l = f[0] as f32 / 32768.0;
                        [l, if ch > 1 { f[1] as f32 / 32768.0 } else { l }]
                    })
                    .collect();
                if resampler.as_ref().is_none_or(|(r, _)| *r != p.sample_rate) {
                    resampler = Some((p.sample_rate, Resampler::new(p.sample_rate, out_rate)));
                }
                out.clear();
                resampler.as_mut().unwrap().1.process(&frames, &mut out);
                jitter.lock().unwrap().push(&out);
            }
        });
    }

    let make = |buffer_size| cpal::StreamConfig { channels: out_ch as u16, sample_rate: out_rate, buffer_size };
    let build = |config| {
        let jitter = jitter.clone();
        device.build_output_stream(
            config,
            move |data: &mut [f32], _: &_| {
                let mut j = jitter.lock().unwrap();
                for frame in data.chunks_mut(out_ch) {
                    let [l, r] = j.pop();
                    frame[0] = l;
                    if out_ch > 1 {
                        frame[1] = r;
                    }
                    for x in frame.iter_mut().skip(2) {
                        *x = 0.0;
                    }
                }
            },
            // 启动瞬间的欠载提示是正常现象，不打印
            |e| {
                if !e.to_string().contains("underrun") {
                    eprintln!("声音播放出错: {e}")
                }
            },
            None,
        )
    };
    // 尽量用小缓冲（约 5 ms）降低延迟，设备不支持时退回默认
    let stream = build(make(cpal::BufferSize::Fixed(256))).or_else(|_| build(make(cpal::BufferSize::Default)))?;
    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratio_and_continuity() {
        let mut r = Resampler::new(44_100, 48_000);
        let input: Vec<Frame> = (0..4410).map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 4800).abs() <= 2, "{}", out.len());
        // 单调递增、无跳变
        assert!(out.windows(2).all(|w| w[1][0] >= w[0][0] && w[1][0] - w[0][0] < 1.0));
        let mut same = Resampler::new(48_000, 48_000);
        let mut o2 = Vec::new();
        same.process(&input[..10], &mut o2);
        assert_eq!(o2, input[..10]);
    }

    #[test]
    fn jitter_caps_latency_and_reprimes() {
        let mut j = Jitter { q: VecDeque::new(), target: 10, max: 30, primed: false };
        j.push(&[[1.0; 2]; 5]);
        assert_eq!(j.pop(), [0.0; 2]); // 未攒够
        j.push(&[[1.0; 2]; 5]);
        assert_eq!(j.pop(), [1.0; 2]);
        j.push(&[[2.0; 2]; 100]);
        assert_eq!(j.q.len(), 10); // 超过上限后裁回目标
        for _ in 0..10 {
            j.pop();
        }
        assert_eq!(j.pop(), [0.0; 2]); // 见底后重新攒
        assert!(!j.primed);
    }
}

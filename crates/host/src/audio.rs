//! 声音采集（被控端）：Windows 上用 WASAPI 环回录下系统正在播放的声音，打包成 PCM 发给控制端。
//! 非 Windows 平台（测试图像源）发送 440 Hz 测试音。

use rc_protocol::audio::{self, MAX_FRAMES};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 把交错的立体声 i16 按包发出。
struct Sender {
    udp: UdpSocket,
    rate: u32,
    seq: u32,
    pending: Vec<i16>,
}

impl Sender {
    fn push(&mut self, stereo: impl Iterator<Item = i16>) {
        self.pending.extend(stereo);
        while self.pending.len() >= MAX_FRAMES * 2 {
            let chunk: Vec<i16> = self.pending.drain(..MAX_FRAMES * 2).collect();
            let _ = self.udp.send(&audio::encode(self.seq, self.rate, 2, &chunk));
            self.seq = self.seq.wrapping_add(1);
        }
    }
}

/// 在后台线程采集并发送，直到 `alive` 变为 false。
pub fn spawn(dest: SocketAddr, alive: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        if let Err(e) = run(dest, &alive) {
            eprintln!("声音不可用: {e:#}");
        }
    });
}

#[cfg(windows)]
fn run(dest: SocketAddr, alive: &AtomicBool) -> anyhow::Result<()> {
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::SampleFormat;

    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(dest)?;
    let host = cpal::default_host();
    let device = host.default_output_device().context("没有默认的播放设备")?;
    let cfg = device.default_output_config().context("读取播放设备格式")?;
    let (rate, channels) = (cfg.sample_rate(), cfg.channels() as usize);
    println!("声音：环回采集 {} Hz，{} 声道", rate, channels);
    let mut sender = Sender { udp, rate, seq: 0, pending: Vec::with_capacity(MAX_FRAMES * 4) };
    // 多声道只取前两个；单声道复制成立体声
    let pick = move |frame: &[f32]| -> [i16; 2] {
        let l = audio::f32_to_i16(frame[0]);
        [l, if channels > 1 { audio::f32_to_i16(frame[1]) } else { l }]
    };
    let config = cpal::StreamConfig { channels: channels as u16, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let err = |e: cpal::Error| {
        if !e.to_string().contains("underrun") {
            eprintln!("声音采集出错: {e}")
        }
    };
    // 在播放设备上建立输入流，cpal 会自动启用 WASAPI 环回模式
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &_| sender.push(data.chunks_exact(channels).flat_map(pick)),
            err,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &_| {
                sender.push(data.chunks_exact(channels).flat_map(|f| [f[0], if channels > 1 { f[1] } else { f[0] }]))
            },
            err,
            None,
        )?,
        other => anyhow::bail!("不支持的采样格式 {other:?}"),
    };
    stream.play()?;
    while alive.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run(dest: SocketAddr, alive: &AtomicBool) -> anyhow::Result<()> {
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(dest)?;
    let rate = 48_000u32;
    let mut sender = Sender { udp, rate, seq: 0, pending: Vec::new() };
    let mut phase = 0f32;
    let start = std::time::Instant::now();
    let mut sent_frames = 0u64;
    while alive.load(Ordering::Relaxed) {
        // 按真实时间节奏生成 440 Hz 测试音
        let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        let n = due.saturating_sub(sent_frames) as usize;
        let tone: Vec<i16> = (0..n)
            .flat_map(|_| {
                phase = (phase + 440.0 / rate as f32) % 1.0;
                let s = audio::f32_to_i16((phase * std::f32::consts::TAU).sin() * 0.2);
                [s, s]
            })
            .collect();
        sender.push(tone.into_iter());
        sent_frames += n as u64;
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

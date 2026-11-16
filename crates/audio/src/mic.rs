//! 麦克风采集：本机默认输入设备 → 单声道 16 位 PCM → UDP 包。

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rc_protocol::audio::{self, MAX_FRAMES};
use std::net::UdpSocket;

struct Sender {
    udp: UdpSocket,
    rate: u32,
    seq: u32,
    pending: Vec<i16>,
}

impl Sender {
    fn push(&mut self, mono: impl Iterator<Item = i16>) {
        self.pending.extend(mono);
        while self.pending.len() >= MAX_FRAMES {
            let chunk: Vec<i16> = self.pending.drain(..MAX_FRAMES).collect();
            let _ = self.udp.send(&audio::encode(self.seq, self.rate, 1, &chunk));
            self.seq = self.seq.wrapping_add(1);
        }
    }
}

/// 开始采集麦克风并发送（`udp` 已 connect 到对方的麦克风端口）。返回 (Stream, 设备名称)；Stream 丢弃即停止。
///
/// macOS 第一次使用时系统会询问是否允许使用麦克风。
pub fn start(udp: UdpSocket) -> Result<(cpal::Stream, String)> {
    let device = cpal::default_host().default_input_device().context("没有麦克风")?;
    let name = device.to_string();
    let cfg = device.default_input_config().context("读取麦克风格式")?;
    let (rate, channels) = (cfg.sample_rate(), cfg.channels() as usize);
    let mut sender = Sender { udp, rate, seq: 0, pending: Vec::with_capacity(MAX_FRAMES * 2) };
    let config = cpal::StreamConfig { channels: channels as u16, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let err = |e: cpal::Error| {
        if !e.to_string().contains("overrun") {
            eprintln!("麦克风采集出错: {e}")
        }
    };
    // 多声道麦克风取各声道平均
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &_| {
                sender.push(data.chunks_exact(channels).map(|f| audio::f32_to_i16(f.iter().sum::<f32>() / channels as f32)))
            },
            err,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &_| {
                sender.push(data.chunks_exact(channels).map(|f| (f.iter().map(|&x| x as i32).sum::<i32>() / channels as i32) as i16))
            },
            err,
            None,
        )?,
        other => anyhow::bail!("不支持的麦克风采样格式 {other:?}"),
    };
    stream.play().context("启动麦克风")?;
    Ok((stream, name))
}

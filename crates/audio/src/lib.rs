//! 声音（两端共用）：
//! - [`player`]：接收 PCM 包，经抖动缓冲后播放到某个输出设备（控制端放远端声音；被控端把麦克风声音放进虚拟声卡）。
//! - [`mic`]：采集本机麦克风，打包发给对方。

pub mod mic;
pub mod player;

use cpal::traits::HostTrait;

/// 播放 / 采集流；丢弃即停止。
pub type Stream = cpal::Stream;
pub type Device = cpal::Device;

/// 虚拟声卡的播放端名称（放进去的声音会出现在它的“麦克风”端）：
/// Windows 的 VB-CABLE 播放端叫 “CABLE Input (VB-Audio Virtual Cable)”；Mac 的 BlackHole 叫 “BlackHole 2ch”。
const VIRTUAL_CABLES: &[&str] = &["cable input", "blackhole", "vb-audio"];

/// 找一个虚拟声卡的播放端。返回 (设备, 名称)。
pub fn virtual_cable() -> Option<(cpal::Device, String)> {
    let host = cpal::default_host();
    host.output_devices().ok()?.find_map(|d| {
        let name = d.to_string();
        let lower = name.to_lowercase();
        VIRTUAL_CABLES.iter().any(|v| lower.contains(v)).then_some((d, name))
    })
}

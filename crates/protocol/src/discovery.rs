//! 局域网发现。
//!
//! 控制端向各网卡的子网广播地址发送 [`Probe`]，被控端在 [`crate::DISCOVERY_PORT`] 上监听，
//! 单播回复 [`Announce`]。回复包的源地址就是控制端能直接访问到的那个地址，
//! 不会被虚拟网卡（VMware、WSL、代理 TUN 等）的地址干扰。

use crate::DeviceId;
use serde::{Deserialize, Serialize};

const MAGIC: [u8; 4] = *b"RCD1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    magic: [u8; 4],
    /// 发起方的设备标识，被控端据此在回复里标明是否已配对。
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Announce {
    magic: [u8; 4],
    pub version: u32,
    pub host_id: DeviceId,
    pub name: String,
    /// "windows" / "macos" …
    pub os: String,
    /// TCP 控制端口。
    pub port: u16,
    /// 发起探测的设备是否已与本机配对。
    pub paired: bool,
    /// 本机物理网卡的硬件地址：控制端记下来，对方睡眠后用网络唤醒包（Wake-on-LAN）叫醒它。
    pub macs: Vec<[u8; 6]>,
}

/// 旧版被控端的应答（没有 `macs`）。
#[derive(Deserialize)]
struct AnnounceV1 {
    magic: [u8; 4],
    version: u32,
    host_id: DeviceId,
    name: String,
    os: String,
    port: u16,
    paired: bool,
}

impl Probe {
    pub fn new(device_id: DeviceId) -> Self {
        Self { magic: MAGIC, device_id }
    }
}

impl Announce {
    pub fn new(host_id: DeviceId, name: String, os: &str, port: u16, paired: bool, macs: Vec<[u8; 6]>) -> Self {
        Self { magic: MAGIC, version: crate::PROTOCOL_VERSION, host_id, name, os: os.into(), port, paired, macs }
    }
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serialize(msg).expect("serialize")
}

pub fn decode_probe(buf: &[u8]) -> Option<Probe> {
    bincode::deserialize::<Probe>(buf).ok().filter(|p| p.magic == MAGIC)
}

pub fn decode_announce(buf: &[u8]) -> Option<Announce> {
    let a = bincode::deserialize::<Announce>(buf).ok().or_else(|| {
        let v = bincode::deserialize::<AnnounceV1>(buf).ok()?;
        Some(Announce { magic: v.magic, version: v.version, host_id: v.host_id, name: v.name, os: v.os, port: v.port, paired: v.paired, macs: vec![] })
    })?;
    (a.magic == MAGIC).then_some(a)
}

/// Wake-on-LAN 唤醒包：6 个 0xFF，再把网卡地址重复 16 遍。
pub fn magic_packet(mac: &[u8; 6]) -> Vec<u8> {
    let mut p = vec![0xFF; 6];
    for _ in 0..16 {
        p.extend_from_slice(mac);
    }
    p
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s.split([':', '-']).map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().ok()?;
    parts.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_magic() {
        let p = Probe::new([3; 16]);
        assert_eq!(decode_probe(&encode(&p)), Some(p.clone()));
        let a = Announce::new([4; 16], "书房电脑".into(), "windows", 47800, true, vec![[1, 2, 3, 4, 5, 0xab]]);
        assert_eq!(decode_announce(&encode(&a)), Some(a));
        assert_eq!(decode_probe(b"garbage"), None);
        let mut bad = encode(&p);
        bad[0] = b'X';
        assert_eq!(decode_probe(&bad), None);
    }

    #[test]
    fn legacy_announce_still_decodes() {
        #[derive(Serialize)]
        struct Old {
            magic: [u8; 4],
            version: u32,
            host_id: DeviceId,
            name: String,
            os: String,
            port: u16,
            paired: bool,
        }
        let old = Old { magic: MAGIC, version: 11, host_id: [5; 16], name: "旧电脑".into(), os: "macos".into(), port: 47800, paired: false };
        let a = decode_announce(&encode(&old)).unwrap();
        assert_eq!((a.name.as_str(), a.port, a.macs.len()), ("旧电脑", 47800, 0));
    }

    #[test]
    fn wake_packet() {
        let mac = [0xc4, 0x03, 0xa8, 0xd1, 0xcf, 0x27];
        let p = magic_packet(&mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[96..], &mac);
        assert_eq!(parse_mac(&format_mac(&mac)), Some(mac));
        assert_eq!(parse_mac("C4-03-A8-D1-CF-27"), Some(mac));
        assert_eq!(parse_mac("c4:03"), None);
    }
}

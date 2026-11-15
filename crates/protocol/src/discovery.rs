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
}

impl Probe {
    pub fn new(device_id: DeviceId) -> Self {
        Self { magic: MAGIC, device_id }
    }
}

impl Announce {
    pub fn new(host_id: DeviceId, name: String, os: &str, port: u16, paired: bool) -> Self {
        Self { magic: MAGIC, version: crate::PROTOCOL_VERSION, host_id, name, os: os.into(), port, paired }
    }
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serialize(msg).expect("serialize")
}

pub fn decode_probe(buf: &[u8]) -> Option<Probe> {
    bincode::deserialize::<Probe>(buf).ok().filter(|p| p.magic == MAGIC)
}

pub fn decode_announce(buf: &[u8]) -> Option<Announce> {
    bincode::deserialize::<Announce>(buf).ok().filter(|a| a.magic == MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_magic() {
        let p = Probe::new([3; 16]);
        assert_eq!(decode_probe(&encode(&p)), Some(p.clone()));
        let a = Announce::new([4; 16], "书房电脑".into(), "windows", 47800, true);
        assert_eq!(decode_announce(&encode(&a)), Some(a));
        assert_eq!(decode_probe(b"garbage"), None);
        let mut bad = encode(&p);
        bad[0] = b'X';
        assert_eq!(decode_probe(&bad), None);
    }
}

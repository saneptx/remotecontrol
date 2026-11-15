//! 局域网远程控制协议。
//!
//! - 控制通道：TCP（NODELAY），长度前缀 + bincode。握手、输入事件、关键帧请求、心跳。
//! - 视频通道：UDP，每帧切成 ≤ [`MAX_PAYLOAD`] 字节的分片，丢包时不重传，
//!   由控制端请求关键帧恢复（局域网内重传的等待比重发一帧更贵）。

pub mod hevc;
pub mod video;

use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47800;
const MAX_MSG_LEN: u32 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientMsg {
    Hello { version: u32, client_nonce: [u8; 16] },
    /// `mac` = HMAC-SHA256(password, host_nonce || client_nonce)；`udp_port` 为控制端接收视频的端口。
    Auth { mac: [u8; 32], udp_port: u16 },
    Input(InputEvent),
    RequestKeyframe,
    Ping { t_us: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HostMsg {
    Challenge { nonce: [u8; 16] },
    Welcome { width: u32, height: u32, fps: u32 },
    Reject { reason: String },
    /// `t_us` 原样返回，`host_us` 为被控端时钟，用于估算两端时钟差。
    Pong { t_us: u64, host_us: u64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum InputEvent {
    /// 归一化到被捕获屏幕的绝对坐标，0..=65535。
    MouseMoveAbs { x: u16, y: u16 },
    /// 0=左 1=右 2=中 3=X1 4=X2
    MouseButton { button: u8, down: bool },
    /// 单位与 Windows WHEEL_DELTA 一致（120 = 一格），向上/向右为正。
    Wheel { dx: i16, dy: i16 },
    /// PC Set-1 扫描码；`extended` 表示 E0 前缀。
    Key { scancode: u16, extended: bool, down: bool },
}

pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = bincode::serialize(msg).map_err(invalid)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)
}

pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_MSG_LEN {
        return Err(invalid("message too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    bincode::deserialize(&body).map_err(invalid)
}

fn invalid(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

pub fn random_nonce() -> [u8; 16] {
    let mut n = [0u8; 16];
    getrandom::getrandom(&mut n).expect("os rng");
    n
}

fn mac_for(password: &str, host_nonce: &[u8; 16], client_nonce: &[u8; 16]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("hmac key");
    mac.update(host_nonce);
    mac.update(client_nonce);
    mac
}

pub fn auth_mac(password: &str, host_nonce: &[u8; 16], client_nonce: &[u8; 16]) -> [u8; 32] {
    mac_for(password, host_nonce, client_nonce).finalize().into_bytes().into()
}

/// 常数时间校验。
pub fn verify_mac(password: &str, host_nonce: &[u8; 16], client_nonce: &[u8; 16], mac: &[u8; 32]) -> bool {
    mac_for(password, host_nonce, client_nonce).verify_slice(mac).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_roundtrip() {
        let mut buf = Vec::new();
        let a = ClientMsg::Input(InputEvent::Key { scancode: 0x1d, extended: true, down: true });
        let b = ClientMsg::Ping { t_us: 42 };
        write_msg(&mut buf, &a).unwrap();
        write_msg(&mut buf, &b).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_msg::<ClientMsg>(&mut r).unwrap(), a);
        assert_eq!(read_msg::<ClientMsg>(&mut r).unwrap(), b);
        assert!(read_msg::<ClientMsg>(&mut r).is_err());
    }

    #[test]
    fn oversized_msg_rejected() {
        let buf = u32::MAX.to_le_bytes();
        assert!(read_msg::<ClientMsg>(&mut &buf[..]).is_err());
    }

    #[test]
    fn auth() {
        let (h, c) = ([1u8; 16], [2u8; 16]);
        let mac = auth_mac("secret", &h, &c);
        assert!(verify_mac("secret", &h, &c, &mac));
        assert!(!verify_mac("wrong", &h, &c, &mac));
        assert!(!verify_mac("secret", &c, &h, &mac));
    }
}

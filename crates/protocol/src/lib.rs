//! 局域网远程控制协议。
//!
//! - 控制通道：TCP（NODELAY），长度前缀 + bincode。握手与配对、输入事件、关键帧请求、心跳。
//! - 发现：UDP 广播，见 [`discovery`]。
//! - 视频通道：UDP，每帧切成 ≤ [`MAX_PAYLOAD`] 字节的分片，丢包时不重传，
//!   由控制端请求关键帧恢复（局域网内重传的等待比重发一帧更贵）。

pub mod audio;
pub mod data;
pub mod discovery;
pub mod h264;
pub mod hevc;
pub mod image;
pub mod pair;
pub mod transfer;
pub mod video;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u32 = 10;
pub const DEFAULT_PORT: u16 = 47800;
/// 局域网发现使用的 UDP 端口。
pub const DISCOVERY_PORT: u16 = 47801;
/// Windows：界面程序运行期间持有的全局互斥量。被控端服务发现它消失（界面已关闭）就自行停止。
pub const UI_MUTEX: &str = "Global\\RemoteControlUI";
const MAX_MSG_LEN: u32 = 64 * 1024;

/// 设备的持久随机标识（首次运行时生成）。
pub type DeviceId = [u8; 16];

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// 握手流程：
///
/// ```text
/// 控制端 Hello ─────────────────────────▶ 被控端
///        ◀── Challenge（已配对）  或  PairRequired（未配对，附被控端临时公钥）
/// 未配对： PairKey（控制端临时公钥）──▶   被控端弹窗确认（两边显示相同的 6 位配对码）
///        ◀── Challenge（确认后）  或  Reject
/// Auth（用配对密钥计算的 MAC）────────▶
///        ◀── Welcome（附被控端对配对密钥的证明，防止被冒充）
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientMsg {
    Hello { version: u32, device_id: DeviceId, device_name: String, client_nonce: [u8; 16] },
    /// 配对时控制端的 X25519 临时公钥。
    PairKey { public_key: [u8; 32] },
    /// 收到 Challenge 但本机没有该被控端的配对信息（被删除或重装）：请求重新配对。
    RequestPairing,
    /// 新连接的第一条消息：表示这是某个会话的数据通道（剪贴板、文件），`token` 来自 Welcome。
    DataHello { token: [u8; 16] },
    /// `mac` 见 [`pair::client_mac`]；`udp_port` 为控制端接收视频的端口；
    /// `audio_port` 为接收声音的端口（0 表示不要声音）。
    /// `codecs`：控制端能解码的视频格式，按优先顺序排列；被控端选第一个自己能编码的。
    Auth { mac: [u8; 32], udp_port: u16, audio_port: u16, codecs: Vec<Codec> },
    Input(InputEvent),
    RequestKeyframe,
    Ping { t_us: u64 },
    /// 控制端窗口的实际像素尺寸。被控端按它缩小编码分辨率（不放大），
    /// 避免编码最终看不到的像素。`0x0` 表示要求原始分辨率。
    Viewport { width: u32, height: u32 },
    /// 切换到被控端的第 `index` 块显示器（下标对应 [`HostMsg::Displays`] 的列表）。
    SelectDisplay { index: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HostMsg {
    /// 控制端已配对：用配对密钥回答挑战。
    Challenge { host_id: DeviceId, host_name: String, nonce: [u8; 16] },
    /// 控制端未配对：附被控端的 X25519 临时公钥，等待控制端的 `PairKey`。
    PairRequired { host_id: DeviceId, host_name: String, public_key: [u8; 32] },
    /// `host_proof` 见 [`pair::host_proof`]；`data_token` 用于打开数据通道。
    /// `video_port` / `audio_port` 是被控端发送视频、声音所用的 UDP 端口：控制端先从自己的接收端口
    /// 往这里发小包（UDP 打洞），本机防火墙就会把对方发来的数据当作回复放行，无需添加防火墙规则。
    /// `codec` 为被控端选定的视频格式（整个会话不变）。
    Welcome { width: u32, height: u32, fps: u32, host_proof: [u8; 32], data_token: [u8; 16], video_port: u16, audio_port: u16, codec: Codec },
    Reject { reason: String },
    /// `t_us` 原样返回，`host_us` 为被控端时钟，用于估算两端时钟差。
    Pong { t_us: u64, host_us: u64 },
    /// 被控端状态提示（例如处于锁屏、无法采集）；`None` 表示恢复正常。
    Notice { text: Option<String> },
    /// 被控端当前的光标形状，变化时发送；控制端把本地光标换成同样的形状。
    Cursor { shape: CursorShape },
    /// 被控端的显示器列表和当前显示的那块；连接后发送一次，切换显示器后再发送。
    Displays { list: Vec<DisplayInfo>, current: u32 },
}

/// 视频编码格式。两端都是 Annex-B 码流、4:2:0、无 B 帧、关键帧带参数集。
///
/// 默认 HEVC：同样画质码率更低。H.264 是备选：Windows 控制端没装“HEVC 视频扩展”、
/// 或被控端显卡只有 H.264 硬件编码时使用。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
        }
    }

    /// 访问单元是否可作为解码起点。
    pub fn is_keyframe(self, au: &[u8]) -> bool {
        match self {
            Codec::Hevc => hevc::contains_irap(au),
            Codec::H264 => h264::contains_idr(au),
        }
    }
}

/// 被控端的一块显示器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayInfo {
    pub name: String,
    /// 像素尺寸
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// 光标形状（系统标准光标）。应用自绘的光标按箭头处理。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CursorShape {
    Arrow,
    /// 文本输入（I 形）
    Text,
    /// 链接（手指）
    Hand,
    /// 忙（沙漏 / 转圈）
    Wait,
    /// 后台忙（箭头 + 转圈）
    Progress,
    Crosshair,
    /// 上下调整
    ResizeNS,
    /// 左右调整
    ResizeEW,
    /// 左上-右下调整
    ResizeNWSE,
    /// 右上-左下调整
    ResizeNESW,
    /// 移动（四向箭头）
    Move,
    NotAllowed,
    Help,
    /// 被控端隐藏了光标（例如全屏视频、打字时）
    Hidden,
    /// 光标已画在画面里（macOS 被控端），控制端隐藏本地光标
    InVideo,
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

pub fn random_32() -> [u8; 32] {
    let mut n = [0u8; 32];
    getrandom::getrandom(&mut n).expect("os rng");
    n
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
    fn hex_roundtrip() {
        let id = random_nonce();
        assert_eq!(parse_hex16(&hex(&id)), Some(id));
        assert_eq!(parse_hex16("zz"), None);
        assert_eq!(parse_hex16(&"g".repeat(32)), None);
    }
}

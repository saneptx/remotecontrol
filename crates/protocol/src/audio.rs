//! 声音：被控端把系统正在播放的声音以原始 PCM（16 位）通过 UDP 发给控制端。
//!
//! 局域网带宽充足（48 kHz 立体声约 1.5 Mbps），不做压缩，省去编解码延迟。
//! 包头（小端，12 字节）：`magic:u8 | channels:u8 | frames:u16 | seq:u32 | sample_rate:u32`

const MAGIC: u8 = 0xA5;
pub const HEADER_LEN: usize = 12;
/// 每包最多 240 帧（48 kHz 下 5 ms）：立体声 960 字节，不超过以太网 MTU。
pub const MAX_FRAMES: usize = 240;

pub fn encode(seq: u32, sample_rate: u32, channels: u8, samples: &[i16]) -> Vec<u8> {
    let frames = samples.len() / channels.max(1) as usize;
    let mut out = Vec::with_capacity(HEADER_LEN + samples.len() * 2);
    out.push(MAGIC);
    out.push(channels);
    out.extend_from_slice(&(frames as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub struct Packet {
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub samples: Vec<i16>,
}

pub fn decode(buf: &[u8]) -> Option<Packet> {
    if buf.len() < HEADER_LEN || buf[0] != MAGIC || buf[1] == 0 {
        return None;
    }
    let channels = buf[1];
    let frames = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    let seq = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let n = frames * channels as usize;
    let body = buf.get(HEADER_LEN..HEADER_LEN + n * 2)?;
    if !(8000..=192_000).contains(&sample_rate) {
        return None;
    }
    let samples = body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    Some(Packet { seq, sample_rate, channels, samples })
}

/// 浮点样本 → 16 位。
pub fn f32_to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s: Vec<i16> = (0..480).map(|i| (i * 37 - 9000) as i16).collect();
        let pkt = encode(9, 48000, 2, &s);
        assert!(pkt.len() <= HEADER_LEN + MAX_FRAMES * 4);
        let p = decode(&pkt).unwrap();
        assert_eq!((p.seq, p.sample_rate, p.channels, p.samples), (9, 48000, 2, s));
        assert!(decode(&pkt[..20]).is_none());
        assert!(decode(b"garbage packet!!").is_none());
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
    }
}

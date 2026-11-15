//! UDP 视频分片与重组。
//!
//! 包头（小端，18 字节）：
//! `frame_id:u32 | frag_idx:u16 | frag_count:u16 | flags:u8 | reserved:u8 | capture_us:u64`

use std::collections::BTreeMap;

pub const HEADER_LEN: usize = 18;
/// 保证整包不超过以太网 MTU（1500 - IP 20 - UDP 8），避免 IP 分片。
pub const MAX_PAYLOAD: usize = 1400;
pub const MAX_DATAGRAM: usize = HEADER_LEN + MAX_PAYLOAD;
pub const FLAG_KEYFRAME: u8 = 1;

/// 把一帧切片，逐包回调 `send`。
pub fn packetize(
    frame_id: u32,
    keyframe: bool,
    capture_us: u64,
    data: &[u8],
    mut send: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let count = data.len().div_ceil(MAX_PAYLOAD).max(1);
    assert!(count <= u16::MAX as usize, "frame too large");
    let mut buf = [0u8; MAX_DATAGRAM];
    for (idx, chunk) in data.chunks(MAX_PAYLOAD).enumerate() {
        buf[0..4].copy_from_slice(&frame_id.to_le_bytes());
        buf[4..6].copy_from_slice(&(idx as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&(count as u16).to_le_bytes());
        buf[8] = if keyframe { FLAG_KEYFRAME } else { 0 };
        buf[9] = 0;
        buf[10..18].copy_from_slice(&capture_us.to_le_bytes());
        buf[HEADER_LEN..HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        send(&buf[..HEADER_LEN + chunk.len()])?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum Output {
    /// 还没凑齐，或是无关紧要的包。
    Pending,
    /// 一帧完整且可解码（参考链未断）。
    Frame(Frame),
    /// 检测到丢帧，参考链已断：调用方应请求关键帧。之后的非关键帧会被丢弃。
    Lost,
}

struct Partial {
    keyframe: bool,
    capture_us: u64,
    count: u16,
    received: u16,
    frags: Vec<Option<Vec<u8>>>,
}

/// 帧重组器。只在更新的帧已经完整到达时才判定旧帧丢失，容忍帧内乱序。
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    /// 为 true 时只接受关键帧（刚启动或参考链已断）。
    waiting_key: bool,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self { partial: BTreeMap::new(), last_delivered: None, waiting_key: true }
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_partial(&self) -> bool {
        !self.partial.is_empty()
    }

    /// 接收超时时调用：丢掉凑不齐的帧，之后只等关键帧。返回是否需要请求关键帧。
    pub fn on_timeout(&mut self) -> bool {
        if self.partial.is_empty() {
            return false;
        }
        self.partial.clear();
        self.waiting_key = true;
        true
    }

    pub fn push(&mut self, pkt: &[u8]) -> Output {
        if pkt.len() < HEADER_LEN {
            return Output::Pending;
        }
        let id = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
        let idx = u16::from_le_bytes(pkt[4..6].try_into().unwrap());
        let count = u16::from_le_bytes(pkt[6..8].try_into().unwrap());
        let keyframe = pkt[8] & FLAG_KEYFRAME != 0;
        let capture_us = u64::from_le_bytes(pkt[10..18].try_into().unwrap());
        if count == 0 || idx >= count {
            return Output::Pending;
        }
        if self.last_delivered.is_some_and(|last| id <= last) {
            return Output::Pending; // 迟到的旧包
        }

        let p = self.partial.entry(id).or_insert_with(|| Partial {
            keyframe,
            capture_us,
            count,
            received: 0,
            frags: vec![None; count as usize],
        });
        if p.count != count {
            return Output::Pending;
        }
        let slot = &mut p.frags[idx as usize];
        if slot.is_none() {
            *slot = Some(pkt[HEADER_LEN..].to_vec());
            p.received += 1;
        }
        if p.received < p.count {
            return Output::Pending;
        }

        // 这一帧齐了；比它旧的未完成帧都算丢失。
        let p = self.partial.remove(&id).unwrap();
        let lost_older = self.partial.keys().next().is_some_and(|&k| k < id);
        self.partial.retain(|&k, _| k > id);
        let gap = lost_older || self.last_delivered.is_some_and(|last| id != last.wrapping_add(1));
        self.last_delivered = Some(id);

        if !p.keyframe && (gap || self.waiting_key) {
            let newly_lost = !self.waiting_key;
            self.waiting_key = true;
            // 已经在等关键帧时不重复上报，由调用方按时间节流重发请求。
            return if newly_lost || gap { Output::Lost } else { Output::Pending };
        }
        self.waiting_key = false;
        let mut data = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().unwrap().len()).sum());
        for f in &p.frags {
            data.extend_from_slice(f.as_ref().unwrap());
        }
        Output::Frame(Frame { id, keyframe: p.keyframe, capture_us: p.capture_us, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(id: u32, key: bool, data: &[u8]) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        packetize(id, key, id as u64 * 1000, data, |p| {
            v.push(p.to_vec());
            Ok(())
        })
        .unwrap();
        v
    }

    fn feed(r: &mut Reassembler, pkts: &[Vec<u8>]) -> Output {
        let mut last = Output::Pending;
        for p in pkts {
            last = r.push(p);
        }
        last
    }

    fn payload(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn roundtrip_multi_fragment() {
        let data = payload(MAX_PAYLOAD * 3 + 17, 1);
        let pkts = packets(0, true, &data);
        assert_eq!(pkts.len(), 4);
        assert!(pkts.iter().all(|p| p.len() <= MAX_DATAGRAM));
        let mut r = Reassembler::new();
        assert_eq!(feed(&mut r, &pkts), Output::Frame(Frame { id: 0, keyframe: true, capture_us: 0, data }));
    }

    #[test]
    fn exact_multiple_and_tiny() {
        let mut r = Reassembler::new();
        let d = payload(MAX_PAYLOAD * 2, 2);
        assert_eq!(packets(0, true, &d).len(), 2);
        assert!(matches!(feed(&mut r, &packets(0, true, &d)), Output::Frame(f) if f.data == d));
        assert!(matches!(feed(&mut r, &packets(1, false, &[9])), Output::Frame(f) if f.data == [9]));
    }

    #[test]
    fn reordered_within_frame() {
        let data = payload(MAX_PAYLOAD * 4, 3);
        let mut pkts = packets(0, true, &data);
        pkts.reverse();
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &pkts), Output::Frame(f) if f.data == data));
    }

    #[test]
    fn duplicate_packets_ignored() {
        let data = payload(MAX_PAYLOAD * 2, 4);
        let pkts = packets(0, true, &data);
        let mut r = Reassembler::new();
        assert_eq!(r.push(&pkts[0]), Output::Pending);
        assert_eq!(r.push(&pkts[0]), Output::Pending);
        assert!(matches!(r.push(&pkts[1]), Output::Frame(_)));
        assert_eq!(r.push(&pkts[1]), Output::Pending);
    }

    #[test]
    fn waits_for_first_keyframe() {
        let mut r = Reassembler::new();
        assert!(!matches!(feed(&mut r, &packets(5, false, &[1])), Output::Frame(_)));
        assert!(!matches!(feed(&mut r, &packets(6, false, &[1])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(7, true, &[1])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(8, false, &[1])), Output::Frame(_)));
    }

    #[test]
    fn lost_fragment_breaks_chain_until_keyframe() {
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &packets(0, true, &[0])), Output::Frame(_)));
        let mut f1 = packets(1, false, &payload(MAX_PAYLOAD * 2, 5));
        f1.pop(); // 丢一个分片
        assert_eq!(feed(&mut r, &f1), Output::Pending);
        assert_eq!(feed(&mut r, &packets(2, false, &[2])), Output::Lost);
        assert!(!matches!(feed(&mut r, &packets(3, false, &[3])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(4, true, &[4])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(5, false, &[5])), Output::Frame(_)));
        assert!(!r.has_partial());
    }

    #[test]
    fn whole_frame_lost_is_detected() {
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &packets(0, true, &[0])), Output::Frame(_)));
        assert_eq!(feed(&mut r, &packets(2, false, &[2])), Output::Lost);
    }

    #[test]
    fn keyframe_after_gap_is_accepted() {
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &packets(0, true, &[0])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(3, true, &[3])), Output::Frame(_)));
    }

    #[test]
    fn timeout_drops_partial() {
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &packets(0, true, &[0])), Output::Frame(_)));
        assert!(!r.on_timeout());
        let mut f1 = packets(1, false, &payload(MAX_PAYLOAD * 2, 6));
        f1.pop();
        feed(&mut r, &f1);
        assert!(r.on_timeout());
        assert!(!r.has_partial());
        assert!(!matches!(feed(&mut r, &packets(2, false, &[2])), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets(3, true, &[3])), Output::Frame(_)));
    }

    #[test]
    fn garbage_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&[1, 2, 3]), Output::Pending);
        let mut bad = packets(0, true, &[1])[0].clone();
        bad[4] = 9; // idx >= count
        assert_eq!(r.push(&bad), Output::Pending);
    }
}

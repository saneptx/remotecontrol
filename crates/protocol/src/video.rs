//! UDP 视频分片、前向纠错（FEC）与重组。
//!
//! 每帧切成等长的数据分片，再用 Reed-Solomon 生成若干冗余分片一起发出。
//! 只要收到的分片总数 ≥ 数据分片数，就能还原整帧，不需要重传或请求关键帧——
//! 这对 Wi-Fi 上零星丢包尤其关键。
//!
//! 包头（小端，28 字节）：
//! `frame_id:u32 | flags:u8 | rsv:u8 | data_count:u16 | parity_count:u16 | shard_idx:u16 |
//!  frame_len:u32 | capture_us:u64 | host_us:u32`
//!
//! `host_us` 是被控端从采集到开始发送所花的时间（采集等待 + 编码），用于拆分延迟。

use std::collections::BTreeMap;

pub const HEADER_LEN: usize = 28;
/// 保证整包不超过以太网 MTU（1500 - IP 20 - UDP 8），避免 IP 分片。必须是偶数（RS 编码要求）。
pub const MAX_PAYLOAD: usize = 1400;
pub const MAX_DATAGRAM: usize = HEADER_LEN + MAX_PAYLOAD;
pub const FLAG_KEYFRAME: u8 = 1;
/// RS 编码支持的单侧最大分片数。
const MAX_SHARDS: usize = 32768;

/// 冗余分片数：`ceil(data * pct / 100)`，至少 1（pct 为 0 时不加冗余）。
pub fn parity_count(data_count: usize, fec_pct: u32) -> usize {
    if fec_pct == 0 {
        return 0;
    }
    (data_count * fec_pct as usize).div_ceil(100).clamp(1, MAX_SHARDS)
}

pub struct FrameMeta {
    pub frame_id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub host_us: u32,
}

/// 把一帧切片（含冗余分片），逐包回调 `send`。数据分片先发，冗余分片后发。
pub fn packetize(
    meta: &FrameMeta,
    data: &[u8],
    fec_pct: u32,
    mut send: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let data_count = data.len().div_ceil(MAX_PAYLOAD).max(1);
    assert!(data_count <= MAX_SHARDS, "frame too large");
    // 所有分片等长；尽量少补零。
    let shard_size = (data.len().div_ceil(data_count).max(1) + 1) & !1;
    let parity = parity_count(data_count, fec_pct);

    // 只有最后一个分片可能需要补零。
    let last_start = (data_count - 1) * shard_size;
    let mut last = data[last_start.min(data.len())..].to_vec();
    last.resize(shard_size, 0);
    let shard = |i: usize| -> &[u8] { if i + 1 == data_count { &last } else { &data[i * shard_size..(i + 1) * shard_size] } };

    let recovery = if parity > 0 {
        reed_solomon_simd::encode(data_count, parity, (0..data_count).map(shard))
            .map_err(|e| std::io::Error::other(e.to_string()))?
    } else {
        Vec::new()
    };

    let mut buf = [0u8; MAX_DATAGRAM];
    buf[0..4].copy_from_slice(&meta.frame_id.to_le_bytes());
    buf[4] = if meta.keyframe { FLAG_KEYFRAME } else { 0 };
    buf[5] = 0;
    buf[6..8].copy_from_slice(&(data_count as u16).to_le_bytes());
    buf[8..10].copy_from_slice(&(parity as u16).to_le_bytes());
    buf[12..16].copy_from_slice(&(data.len() as u32).to_le_bytes());
    buf[16..24].copy_from_slice(&meta.capture_us.to_le_bytes());
    buf[24..28].copy_from_slice(&meta.host_us.to_le_bytes());
    let all = (0..data_count).map(shard).chain(recovery.iter().map(Vec::as_slice));
    for (idx, payload) in all.enumerate() {
        buf[10..12].copy_from_slice(&(idx as u16).to_le_bytes());
        buf[HEADER_LEN..HEADER_LEN + shard_size].copy_from_slice(payload);
        send(&buf[..HEADER_LEN + shard_size])?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub struct Frame {
    pub id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub host_us: u32,
    pub data: Vec<u8>,
    /// 有数据分片丢失、靠冗余分片还原。
    pub recovered: bool,
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
    host_us: u32,
    data_count: u16,
    parity_count: u16,
    frame_len: u32,
    shard_size: usize,
    shards: Vec<Option<Vec<u8>>>,
    received_data: u16,
    received_total: u16,
}

impl Partial {
    /// 数据分片齐了，或者总数够 RS 还原时，返回 (整帧, 是否经过还原)。
    fn try_complete(&mut self) -> Option<(Vec<u8>, bool)> {
        let d = self.data_count as usize;
        let recovered = self.received_data < self.data_count;
        if recovered {
            if self.received_total < self.data_count {
                return None;
            }
            let originals = self.shards[..d].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
            let recovery = self.shards[d..].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
            let restored = reed_solomon_simd::decode(d, self.parity_count as usize, originals, recovery).ok()?;
            for (i, s) in restored {
                self.shards[i] = Some(s);
            }
        }
        let mut data = Vec::with_capacity(d * self.shard_size);
        for s in &self.shards[..d] {
            data.extend_from_slice(s.as_ref()?);
        }
        data.truncate(self.frame_len as usize);
        Some((data, recovered))
    }
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
        if pkt.len() <= HEADER_LEN {
            return Output::Pending;
        }
        let u16_at = |i: usize| u16::from_le_bytes([pkt[i], pkt[i + 1]]);
        let id = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
        let keyframe = pkt[4] & FLAG_KEYFRAME != 0;
        let data_count = u16_at(6);
        let parity_count = u16_at(8);
        let idx = u16_at(10);
        let frame_len = u32::from_le_bytes(pkt[12..16].try_into().unwrap());
        let capture_us = u64::from_le_bytes(pkt[16..24].try_into().unwrap());
        let host_us = u32::from_le_bytes(pkt[24..28].try_into().unwrap());
        let payload = &pkt[HEADER_LEN..];
        let shard_size = payload.len();

        let total = data_count as usize + parity_count as usize;
        if data_count == 0
            || idx as usize >= total
            || !shard_size.is_multiple_of(2)
            || frame_len as usize > data_count as usize * shard_size
        {
            return Output::Pending;
        }
        if self.last_delivered.is_some_and(|last| id <= last) {
            return Output::Pending; // 迟到的旧包，或已还原帧的剩余分片
        }

        let p = self.partial.entry(id).or_insert_with(|| Partial {
            keyframe,
            capture_us,
            host_us,
            data_count,
            parity_count,
            frame_len,
            shard_size,
            shards: vec![None; total],
            received_data: 0,
            received_total: 0,
        });
        if (p.data_count, p.parity_count, p.frame_len, p.shard_size) != (data_count, parity_count, frame_len, shard_size) {
            return Output::Pending;
        }
        let slot = &mut p.shards[idx as usize];
        if slot.is_some() {
            return Output::Pending;
        }
        *slot = Some(payload.to_vec());
        p.received_total += 1;
        if idx < data_count {
            p.received_data += 1;
        }
        let Some((data, recovered)) = p.try_complete() else {
            return Output::Pending;
        };

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
        Output::Frame(Frame { id, keyframe: p.keyframe, capture_us: p.capture_us, host_us: p.host_us, data, recovered })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets_fec(id: u32, key: bool, data: &[u8], fec: u32) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        let meta = FrameMeta { frame_id: id, keyframe: key, capture_us: id as u64 * 1000, host_us: 7 };
        packetize(&meta, data, fec, |p| {
            v.push(p.to_vec());
            Ok(())
        })
        .unwrap();
        v
    }

    fn packets(id: u32, key: bool, data: &[u8]) -> Vec<Vec<u8>> {
        packets_fec(id, key, data, 0)
    }

    fn feed(r: &mut Reassembler, pkts: &[Vec<u8>]) -> Output {
        let mut last = Output::Pending;
        for p in pkts {
            let out = r.push(p);
            if !matches!(out, Output::Pending) {
                last = out;
            }
        }
        last
    }

    fn payload(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn frame_data(o: Output) -> Vec<u8> {
        match o {
            Output::Frame(f) => f.data,
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_multi_fragment() {
        let data = payload(MAX_PAYLOAD * 3 + 17, 1);
        let pkts = packets(0, true, &data);
        assert_eq!(pkts.len(), 4);
        assert!(pkts.iter().all(|p| p.len() <= MAX_DATAGRAM));
        let mut r = Reassembler::new();
        assert_eq!(
            feed(&mut r, &pkts),
            Output::Frame(Frame { id: 0, keyframe: true, capture_us: 0, host_us: 7, data, recovered: false })
        );
    }

    #[test]
    fn odd_and_tiny_sizes() {
        let mut r = Reassembler::new();
        for (id, len) in [1usize, 2, 3, 1399, 1400, 1401, 2800, 4321].into_iter().enumerate() {
            let d = payload(len, id as u8);
            let pkts = packets_fec(id as u32, true, &d, 20);
            assert!(pkts.iter().all(|p| p.len() <= MAX_DATAGRAM && (p.len() - HEADER_LEN) % 2 == 0));
            assert_eq!(frame_data(feed(&mut r, &pkts)), d, "len {len}");
        }
    }

    #[test]
    fn parity_counts() {
        assert_eq!(parity_count(1, 0), 0);
        assert_eq!(parity_count(1, 20), 1);
        assert_eq!(parity_count(10, 20), 2);
        assert_eq!(parity_count(11, 20), 3);
        assert_eq!(parity_count(100, 20), 20);
    }

    #[test]
    fn reordered_within_frame() {
        let data = payload(MAX_PAYLOAD * 4, 3);
        let mut pkts = packets_fec(0, true, &data, 20);
        pkts.reverse();
        let mut r = Reassembler::new();
        assert_eq!(frame_data(feed(&mut r, &pkts)), data);
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
    fn fec_recovers_lost_data_shards() {
        let data = payload(MAX_PAYLOAD * 50 + 123, 9);
        let pkts = packets_fec(0, true, &data, 20);
        assert_eq!(pkts.len(), 51 + 11);
        // 丢掉 11 个分片（等于冗余数），其中大多是数据分片，包括最后一个
        let drop = [0usize, 3, 7, 10, 20, 21, 22, 33, 49, 50, 55];
        let kept: Vec<_> = pkts.iter().enumerate().filter(|(i, _)| !drop.contains(i)).map(|(_, p)| p.clone()).collect();
        let mut r = Reassembler::new();
        match feed(&mut r, &kept) {
            Output::Frame(f) => {
                assert_eq!(f.data, data);
                assert!(f.recovered);
            }
            other => panic!("{other:?}"),
        }
        // 下一帧照常
        assert_eq!(frame_data(feed(&mut r, &packets_fec(1, false, &[1, 2, 3], 20))), [1, 2, 3]);
    }

    #[test]
    fn fec_completes_early_and_ignores_rest() {
        let data = payload(MAX_PAYLOAD * 5, 2);
        let pkts = packets_fec(0, true, &data, 20); // 5 数据 + 1 冗余
        let mut r = Reassembler::new();
        for p in &pkts[1..5] {
            assert_eq!(r.push(p), Output::Pending);
        }
        assert!(matches!(r.push(&pkts[5]), Output::Frame(f) if f.data == data && f.recovered));
        assert_eq!(r.push(&pkts[0]), Output::Pending);
        assert!(!r.has_partial());
    }

    #[test]
    fn loss_beyond_fec_breaks_chain_until_keyframe() {
        let mut r = Reassembler::new();
        assert!(matches!(feed(&mut r, &packets_fec(0, true, &[0], 20)), Output::Frame(_)));
        let mut f1 = packets_fec(1, false, &payload(MAX_PAYLOAD * 10, 5), 20);
        f1.drain(0..3); // 丢 3 个，冗余只有 2 个
        assert_eq!(feed(&mut r, &f1), Output::Pending);
        assert_eq!(feed(&mut r, &packets_fec(2, false, &[2], 20)), Output::Lost);
        assert!(!matches!(feed(&mut r, &packets_fec(3, false, &[3], 20)), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets_fec(4, true, &[4], 20)), Output::Frame(_)));
        assert!(matches!(feed(&mut r, &packets_fec(5, false, &[5], 20)), Output::Frame(_)));
        assert!(!r.has_partial());
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
        let good = packets(0, true, &[1, 2])[0].clone();
        let mut bad = good.clone();
        bad[10] = 9; // idx >= total
        assert_eq!(r.push(&bad), Output::Pending);
        let mut bad = good.clone();
        bad[6] = 0; // data_count = 0
        assert_eq!(r.push(&bad), Output::Pending);
        let mut bad = good.clone();
        bad[12] = 200; // frame_len 超过分片容量
        assert_eq!(r.push(&bad), Output::Pending);
        assert!(matches!(r.push(&good), Output::Frame(_)));
    }
}

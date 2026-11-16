//! 最小的 H.264 Annex-B 工具：识别 NAL 类型（切分 NAL 与 HEVC 共用 [`crate::hevc::nals`]）。

pub use crate::hevc::nals;

pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1f
}

/// VCL NAL（携带图像数据的切片）。
pub fn is_vcl(t: u8) -> bool {
    (1..=5).contains(&t)
}

pub fn contains_idr(au: &[u8]) -> bool {
    nals(au).any(|n| nal_type(n) == NAL_IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types() {
        // SPS, PPS, IDR 切片
        let data = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88];
        let v: Vec<u8> = nals(&data).map(nal_type).collect();
        assert_eq!(v, vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(contains_idr(&data));
        assert!(!contains_idr(&data[..11]));
    }
}

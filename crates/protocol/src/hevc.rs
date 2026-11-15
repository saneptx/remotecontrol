//! 最小的 HEVC Annex-B 工具：按起始码切 NAL、识别类型。

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;

pub fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}

/// VCL NAL（携带图像数据的切片）。
pub fn is_vcl(t: u8) -> bool {
    t < 32
}

/// IRAP（IDR/CRA/BLA），可作为解码起点。
pub fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}

/// 找下一个 `00 00 01` 起始码，返回其起始下标。
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        // 一次跳过尽量多的字节：起始码第三个字节必须 <= 1
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// 遍历 Annex-B 码流中的 NAL（不含起始码，去掉尾部补零）。
pub fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = find_start_code(data, 0).map(|p| p + 3);
    std::iter::from_fn(move || loop {
        let start = pos?;
        let next = find_start_code(data, start);
        pos = next.map(|p| p + 3);
        let mut end = next.unwrap_or(data.len());
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            return Some(&data[start..end]);
        }
    })
}

pub fn contains_irap(au: &[u8]) -> bool {
    nals(au).any(|n| is_irap(nal_type(n)))
}

/// 在 `data[from..]` 中找下一个 AUD NAL 的起始码位置（含可能的前导 0），用于切分访问单元。
pub fn find_aud(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(p) = find_start_code(data, i) {
        let hdr = *data.get(p + 3)?;
        if (hdr >> 1) & 0x3f == NAL_AUD {
            return Some(if p > 0 && data[p - 1] == 0 { p - 1 } else { p });
        }
        i = p + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nals() {
        let data = [0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 1, 0x42, 1, 0xbb, 0xcc, 0, 0, 0, 1, 0x26, 1, 0xdd];
        let v: Vec<&[u8]> = nals(&data).collect();
        assert_eq!(v, vec![&[0x40, 1, 0xaa][..], &[0x42, 1, 0xbb, 0xcc][..], &[0x26, 1, 0xdd][..]]);
        assert_eq!(v.iter().map(|n| nal_type(n)).collect::<Vec<_>>(), vec![NAL_VPS, NAL_SPS, 19]);
        assert!(contains_irap(&data));
        assert!(!contains_irap(&data[..13]));
    }

    #[test]
    fn empty_and_garbage() {
        assert_eq!(nals(&[]).count(), 0);
        assert_eq!(nals(&[1, 2, 3, 4, 5]).count(), 0);
        assert_eq!(nals(&[0, 0, 1]).count(), 0);
    }

    #[test]
    fn finds_aud() {
        let aud = [0, 0, 0, 1, 0x46, 1, 0x50];
        let slice = [0, 0, 1, 0x02, 1, 0xff];
        let mut s = Vec::new();
        s.extend_from_slice(&aud);
        s.extend_from_slice(&slice);
        s.extend_from_slice(&aud);
        assert_eq!(find_aud(&s, 0), Some(0));
        assert_eq!(find_aud(&s, 4), Some(aud.len() + slice.len()));
        assert_eq!(find_aud(&s, aud.len() + slice.len() + 2), None);
    }
}

use anyhow::Result;
use rc_protocol::InputEvent;

/// 一帧已编码的 HEVC Annex-B 访问单元。
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// 采集时刻（[`crate::now_us`] 时钟）。
    pub capture_us: u64,
}

pub trait VideoSource {
    fn size(&self) -> (u32, u32);
    fn fps(&self) -> u32;
    /// 需要告知控制端的状态（例如处于锁屏、无法采集）。
    fn notice(&self) -> Option<String> {
        None
    }
    /// 当前光标形状（每帧查询一次，变化时发给控制端）。`None` 表示不支持。
    fn cursor(&mut self) -> Option<rc_protocol::CursorShape> {
        None
    }
    /// 控制端窗口像素尺寸变化（已去抖）。`(0, 0)` 表示原始分辨率。
    fn set_viewport(&mut self, _width: u32, _height: u32) -> Result<()> {
        Ok(())
    }
    /// 阻塞至多约一帧时间。画面无变化时返回 `None`（不发送任何数据）。
    /// `force_key` 为 true 时必须尽快产出一个关键帧。
    fn next_frame(&mut self, force_key: bool) -> Result<Option<EncodedFrame>>;
}

/// 输入注入。析构时应松开所有仍按下的键/鼠标键。
pub trait InputSink: Send {
    fn inject(&mut self, ev: InputEvent);
}

/// 按控制端窗口尺寸确定编码分辨率：等比缩小以放进窗口，从不放大；
/// 缩小不到 8% 时没有意义，直接用原始分辨率。宽高取偶数。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn encode_size(src: (u32, u32), viewport: (u32, u32)) -> (u32, u32) {
    if viewport.0 == 0 || viewport.1 == 0 || src.0 == 0 || src.1 == 0 {
        return src;
    }
    let scale = (viewport.0 as f64 / src.0 as f64).min(viewport.1 as f64 / src.1 as f64);
    if scale > 0.92 {
        return src;
    }
    let even = |v: f64| ((v.round() as u32).max(64) + 1) & !1;
    (even(src.0 as f64 * scale), even(src.1 as f64 * scale))
}

#[cfg(test)]
mod tests {
    use super::encode_size;

    #[test]
    fn native_when_no_viewport_or_bigger() {
        assert_eq!(encode_size((3840, 2160), (0, 0)), (3840, 2160));
        assert_eq!(encode_size((1920, 1080), (2560, 1600)), (1920, 1080));
        assert_eq!(encode_size((3840, 2160), (3700, 2100)), (3840, 2160)); // 只小 3%
    }

    #[test]
    fn fits_inside_viewport_keeping_aspect() {
        assert_eq!(encode_size((3840, 2160), (2560, 1440)), (2560, 1440));
        // 窗口比画面“高”：按宽度缩放
        assert_eq!(encode_size((3840, 2160), (2560, 1600)), (2560, 1440));
        // 窗口比画面“宽”：按高度缩放
        assert_eq!(encode_size((3840, 2160), (3000, 1080)), (1920, 1080));
        let (w, h) = encode_size((3840, 2160), (2559, 1439));
        assert!(w % 2 == 0 && h % 2 == 0 && w <= 2560 && h <= 1440);
        assert_eq!(encode_size((3840, 2160), (10, 10)), (64, 64));
    }
}

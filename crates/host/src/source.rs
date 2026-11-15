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
    /// 阻塞至多约一帧时间。画面无变化时返回 `None`（不发送任何数据）。
    /// `force_key` 为 true 时必须尽快产出一个关键帧。
    fn next_frame(&mut self, force_key: bool) -> Result<Option<EncodedFrame>>;
}

/// 输入注入。析构时应松开所有仍按下的键/鼠标键。
pub trait InputSink: Send {
    fn inject(&mut self, ev: InputEvent);
}

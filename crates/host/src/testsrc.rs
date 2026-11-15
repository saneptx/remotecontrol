//! 非 Windows 平台上的测试图像源：用 ffmpeg 命令行生成 HEVC 4:4:4 测试码流，
//! 用于在没有 Windows 机器时联调协议与控制端。

use crate::source::{EncodedFrame, InputSink, VideoSource};
use crate::{now_us, Args};
use anyhow::{bail, Context, Result};
use rc_protocol::{hevc, InputEvent};
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

pub struct TestSource {
    child: Child,
    fps: u32,
    stdout: ChildStdout,
    buf: Vec<u8>,
    /// 已扫描过、确定不含下一个 AUD 的前缀长度。
    scanned: usize,
}

impl TestSource {
    pub fn new(args: &Args) -> Result<Self> {
        let fps = args.fps.unwrap_or(60);
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={WIDTH}x{HEIGHT}:rate={fps}"))
            // RC_TEST_420=1：输出 4:2:0（Windows 控制端的系统解码器不支持 4:4:4）
            .args(["-pix_fmt", if std::env::var("RC_TEST_420").is_ok_and(|v| v == "1") { "yuv420p" } else { "yuv444p" }])
            .args(["-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency"])
            .args(["-b:v", &format!("{}M", args.bitrate.min(30))])
            .args(["-x265-params", &format!("keyint={fps}:min-keyint={fps}:repeat-headers=1:aud=1:log-level=error")])
            .args(["-f", "hevc", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("启动 ffmpeg 失败（测试图像源需要 PATH 中有带 libx265 的 ffmpeg）")?;
        let stdout = child.stdout.take().unwrap();
        Ok(Self { child, fps, stdout, buf: Vec::new(), scanned: 0 })
    }
}

impl VideoSource for TestSource {
    fn size(&self) -> (u32, u32) {
        (WIDTH, HEIGHT)
    }

    fn fps(&self) -> u32 {
        self.fps
    }

    // 命令行编码器无法按需插入关键帧，这里靠 1 秒一个的周期关键帧恢复。
    fn next_frame(&mut self, _force_key: bool) -> Result<Option<EncodedFrame>> {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            // 访问单元以 AUD 开头：找到下一个 AUD 就说明当前这一个结束了。
            if let Some(end) = hevc::find_aud(&self.buf, self.scanned.max(4)) {
                let data: Vec<u8> = self.buf.drain(..end).collect();
                self.scanned = 0;
                let keyframe = hevc::contains_irap(&data);
                return Ok(Some(EncodedFrame { data, keyframe, capture_us: now_us() }));
            }
            self.scanned = self.buf.len().saturating_sub(4);
            let n = self.stdout.read(&mut chunk)?;
            if n == 0 {
                bail!("ffmpeg 已退出");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

impl Drop for TestSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct LogInput;

impl InputSink for LogInput {
    fn inject(&mut self, ev: InputEvent) {
        if !matches!(ev, InputEvent::MouseMoveAbs { .. }) {
            println!("输入: {ev:?}");
        }
    }
}

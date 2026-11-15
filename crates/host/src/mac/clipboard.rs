//! macOS 剪贴板（被控端）：轮询变化计数，变化时读取文字 / 图片 / 文件。

use crate::clip::{ClipEvent, ClipboardBackend};
use rc_macos::pasteboard;
use rc_protocol::data::ClipData;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct Clipboard {
    ours: Arc<AtomicIsize>,
    alive: Arc<AtomicBool>,
}

impl Clipboard {
    pub fn spawn(on_change: Box<dyn Fn(ClipEvent) + Send>) -> Self {
        let ours = Arc::new(AtomicIsize::new(pasteboard::change_count()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let (ours, alive) = (ours.clone(), alive.clone());
            std::thread::spawn(move || {
                let mut last = pasteboard::change_count();
                while alive.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(300));
                    let now = pasteboard::change_count();
                    if now == last {
                        continue;
                    }
                    last = now;
                    if now == ours.load(Ordering::Relaxed) {
                        continue; // 自己写入的
                    }
                    let files = pasteboard::read_files();
                    if !files.is_empty() {
                        on_change(ClipEvent::Files(files));
                    } else if let Some(d) = pasteboard::read() {
                        on_change(ClipEvent::Data(d));
                    }
                }
            });
        }
        Self { ours, alive }
    }
}

impl ClipboardBackend for Clipboard {
    fn set(&self, data: ClipData) {
        pasteboard::write(&data);
        self.ours.store(pasteboard::change_count(), Ordering::Relaxed);
    }
    fn set_files(&self, paths: Vec<PathBuf>) {
        pasteboard::write_files(&paths);
        self.ours.store(pasteboard::change_count(), Ordering::Relaxed);
    }
    fn files(&self) -> Vec<PathBuf> {
        pasteboard::read_files()
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

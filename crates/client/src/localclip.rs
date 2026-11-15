//! 本机剪贴板（控制端）：变化时回调，远端内容写入。

use rc_protocol::data::ClipData;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub use mac::LocalClipboard;
#[cfg(windows)]
pub use win::LocalClipboard;

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use rc_macos::pasteboard;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    pub struct LocalClipboard {
        /// 本程序最近一次写入后的变化计数：本地变化与它相同就不回传
        ours: Arc<AtomicIsize>,
        alive: Arc<AtomicBool>,
    }

    impl LocalClipboard {
        pub fn spawn(on_change: Box<dyn Fn(ClipData) + Send>) -> Self {
            let ours = Arc::new(AtomicIsize::new(pasteboard::change_count()));
            let alive = Arc::new(AtomicBool::new(true));
            let (o, a) = (ours.clone(), alive.clone());
            std::thread::spawn(move || {
                let mut last = pasteboard::change_count();
                while a.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(300));
                    let now = pasteboard::change_count();
                    if now == last {
                        continue;
                    }
                    last = now;
                    if now == o.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Some(d) = pasteboard::read() {
                        on_change(d);
                    }
                }
            });
            Self { ours, alive }
        }

        pub fn set(&self, d: &ClipData) {
            pasteboard::write(d);
            self.ours.store(pasteboard::change_count(), Ordering::Relaxed);
        }

        pub fn set_files(&self, paths: &[PathBuf]) {
            pasteboard::write_files(paths);
            self.ours.store(pasteboard::change_count(), Ordering::Relaxed);
        }
    }

    impl Drop for LocalClipboard {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    pub fn downloads() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads")
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use rc_protocol::data::ClipEvent;
    use rc_windows::clipboard::Clipboard;

    pub struct LocalClipboard(Clipboard);

    impl LocalClipboard {
        pub fn spawn(on_change: Box<dyn Fn(ClipData) + Send>) -> Self {
            Self(Clipboard::spawn(Box::new(move |ev| {
                // 控制端复制的文件不主动发送（拖进窗口即可发送）
                if let ClipEvent::Data(d) = ev {
                    on_change(d)
                }
            })))
        }

        pub fn set(&self, d: &ClipData) {
            self.0.set(d.clone());
        }

        pub fn set_files(&self, paths: &[PathBuf]) {
            self.0.set_files(paths.to_vec());
        }
    }

    pub fn downloads() -> PathBuf {
        rc_windows::clipboard::user_downloads_dir()
    }
}

/// 收到的文件放在“下载”文件夹（可用环境变量 RC_DOWNLOADS 改到别处）。
pub fn downloads() -> PathBuf {
    if let Some(d) = std::env::var_os("RC_DOWNLOADS") {
        return PathBuf::from(d);
    }
    #[cfg(target_os = "macos")]
    return mac::downloads();
    #[cfg(windows)]
    return win::downloads();
}

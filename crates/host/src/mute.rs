//! 会话期间让本机扬声器静音（控制端要求时）。声音采集不受影响：
//! Windows 用按进程环回采集（不经过扬声器音量），macOS 的 ScreenCaptureKit 也不受系统静音影响。
//!
//! 静音前记下原来的状态，会话结束时恢复；只有原本没静音才会去取消静音。
//! 为防止被控端中途崩溃让电脑一直静音，静音时在配置目录留一个标记文件，下次启动时据此恢复。

use std::path::PathBuf;

fn marker() -> PathBuf {
    crate::trust::config_dir().join("muted-by-supercontrol")
}

/// 静音期间持有；析构时恢复。
pub struct MuteGuard(());

impl MuteGuard {
    /// 本机原本没静音时静音并返回守卫；原本就静音或做不到时返回 None。
    pub fn engage() -> Option<Self> {
        match imp::muted() {
            Some(false) => {}
            Some(true) => return None,
            None => {
                eprintln!("无法读取扬声器静音状态，不自动静音");
                return None;
            }
        }
        let _ = std::fs::write(marker(), b"1");
        if !imp::set_muted(true) {
            let _ = std::fs::remove_file(marker());
            eprintln!("无法让扬声器静音");
            return None;
        }
        println!("已让本机扬声器静音（会话结束后恢复）");
        Some(Self(()))
    }
}

impl Drop for MuteGuard {
    fn drop(&mut self) {
        imp::set_muted(false);
        let _ = std::fs::remove_file(marker());
        println!("已恢复本机扬声器");
    }
}

/// 启动时调用：上次是本程序静音的、却没来得及恢复（崩溃、断电），现在恢复。
pub fn recover() {
    if marker().exists() {
        imp::set_muted(false);
        let _ = std::fs::remove_file(marker());
        println!("恢复了上次会话留下的扬声器静音");
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    fn endpoint() -> Option<IAudioEndpointVolume> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let dev = e.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            dev.Activate(CLSCTX_ALL, None).ok()
        }
    }

    pub fn muted() -> Option<bool> {
        unsafe { endpoint()?.GetMute().ok().map(|b| b.as_bool()) }
    }

    pub fn set_muted(on: bool) -> bool {
        unsafe { endpoint().is_some_and(|v| v.SetMute(on, std::ptr::null()).is_ok()) }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    fn osascript(script: &str) -> Option<String> {
        let out = Command::new("/usr/bin/osascript").args(["-e", script]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn muted() -> Option<bool> {
        // 没有静音开关的输出设备（例如部分 HDMI 显示器）返回 missing value
        match osascript("output muted of (get volume settings)")?.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    pub fn set_muted(on: bool) -> bool {
        osascript(&format!("set volume output muted {on}")).is_some()
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn muted() -> Option<bool> {
        None
    }
    pub fn set_muted(_on: bool) -> bool {
        false
    }
}

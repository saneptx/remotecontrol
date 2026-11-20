//! 电源与调度：阻止睡眠、唤醒显示器、提高计时精度与线程优先级。
//!
//! Windows 的执行状态（SetThreadExecutionState）是按线程记录的；`init` 与
//! `SessionAwake` 都必须在主线程（会话所在线程）上调用。

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};

static PREVENT_SLEEP: AtomicBool = AtomicBool::new(false);

pub fn init(prevent_sleep: bool) {
    unsafe {
        // 默认计时精度是 15.6ms，会让采集等待的超时严重偏长。
        timeBeginPeriod(1);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        if prevent_sleep {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
            println!("运行期间将阻止系统睡眠（显示器仍可按设置关闭）");
        } else {
            println!("本机按系统设置睡眠，有人连接时保持唤醒");
        }
    }
    PREVENT_SLEEP.store(prevent_sleep, Ordering::Relaxed);
}

/// 会话期间保持显示器常亮；析构时恢复。
pub struct SessionAwake;

impl SessionAwake {
    pub fn new() -> Self {
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED) };
        Self
    }
}

impl Drop for SessionAwake {
    fn drop(&mut self) {
        let base = if PREVENT_SLEEP.load(Ordering::Relaxed) { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
        unsafe { SetThreadExecutionState(base) };
    }
}

/// 点亮已关闭的显示器：重置空闲计时，并模拟一次细微的鼠标移动
/// （单靠执行状态在部分机器上无法唤醒已关闭的显示器）。
pub fn wake_display() {
    unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
    super::input::nudge_mouse();
}

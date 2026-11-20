//! macOS 电源：有控制端连接时唤醒显示器并保持唤醒；`--prevent-sleep` 时运行期间一直阻止空闲睡眠。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(ty: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, ty: u32, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const std::ffi::c_char, encoding: u32) -> *const c_void;
}

fn cfstr(s: &std::ffi::CStr) -> *const c_void {
    unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), 0x0800_0100) }
}

static ACTIVITY: AtomicU32 = AtomicU32::new(0);

pub fn init(prevent_sleep: bool) {
    if prevent_sleep {
        let mut id = 0;
        unsafe { IOPMAssertionCreateWithName(cfstr(c"PreventUserIdleSystemSleep"), 255, cfstr(c"SuperControl 被控端"), &mut id) };
        println!("运行期间将阻止系统睡眠（显示器仍可按设置关闭）");
    } else {
        println!("本机按系统设置睡眠，有人连接时保持唤醒");
    }
}

/// 会话期间阻止系统和显示器空闲睡眠（对方只看不操作时也不会黑屏、断开）；析构时恢复。
pub struct SessionAwake(Vec<u32>);

impl SessionAwake {
    pub fn new() -> Self {
        let mut ids = Vec::new();
        for ty in [c"PreventUserIdleSystemSleep", c"PreventUserIdleDisplaySleep"] {
            let mut id = 0;
            if unsafe { IOPMAssertionCreateWithName(cfstr(ty), 255, cfstr(c"SuperControl 远程控制中"), &mut id) } == 0 {
                ids.push(id);
            }
        }
        Self(ids)
    }
}

impl Drop for SessionAwake {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            unsafe { IOPMAssertionRelease(id) };
        }
    }
}

/// 模拟一次用户活动：点亮显示器并重置空闲计时。
pub fn wake_display() {
    let mut id = ACTIVITY.load(Ordering::Relaxed);
    unsafe { IOPMAssertionDeclareUserActivity(cfstr(c"SuperControl 远程控制"), 0, &mut id) };
    ACTIVITY.store(id, Ordering::Relaxed);
}

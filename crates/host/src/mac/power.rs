//! macOS 电源：运行期间阻止系统空闲睡眠；有控制端连接时唤醒显示器。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(ty: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, ty: u32, id: *mut u32) -> i32;
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
        unsafe { IOPMAssertionCreateWithName(cfstr(c"PreventUserIdleSystemSleep"), 255, cfstr(c"RemoteControl 被控端"), &mut id) };
        println!("运行期间将阻止系统睡眠（显示器仍可按设置关闭）；如需允许睡眠请加 --allow-sleep");
    }
}

/// 模拟一次用户活动：点亮显示器并重置空闲计时。
pub fn wake_display() {
    let mut id = ACTIVITY.load(Ordering::Relaxed);
    unsafe { IOPMAssertionDeclareUserActivity(cfstr(c"RemoteControl 远程控制"), 0, &mut id) };
    ACTIVITY.store(id, Ordering::Relaxed);
}

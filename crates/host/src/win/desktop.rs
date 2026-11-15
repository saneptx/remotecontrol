//! 跟随“输入桌面”切换。
//!
//! Windows 的锁屏、登录界面和 UAC 弹窗都在名为 Winlogon 的安全桌面上，平时的桌面叫 Default。
//! 屏幕采集（DXGI）和输入注入（SendInput）都只对调用线程所在的桌面生效，所以每当桌面切换，
//! 采集线程和输入线程都要先把自己切到当前的输入桌面。
//!
//! 只有以 SYSTEM 身份运行（即系统服务模式）时才有权打开 Winlogon 桌面；普通权限下
//! `OpenInputDesktop` 会失败，此时什么也不做。

use std::cell::Cell;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_ACCESS_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

const GENERIC_ALL: u32 = 0x1000_0000;

thread_local! {
    /// 本线程通过 SetThreadDesktop 设置的桌面句柄（切换后才能关闭旧的）。
    static CURRENT: Cell<Option<HDESK>> = const { Cell::new(None) };
}

fn desktop_name(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 把调用线程切到当前的输入桌面。返回是否发生了切换。
pub fn sync_thread() -> bool {
    unsafe {
        let Ok(input) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) else {
            return false;
        };
        let current = GetThreadDesktop(GetCurrentThreadId()).ok().and_then(desktop_name);
        let target = desktop_name(input);
        if target.is_none() || current == target {
            let _ = CloseDesktop(input);
            return false;
        }
        if SetThreadDesktop(input).is_err() {
            let _ = CloseDesktop(input);
            return false;
        }
        if let Some(old) = CURRENT.replace(Some(input)) {
            let _ = CloseDesktop(old);
        }
        println!("线程 {} 已切换到桌面 {}", GetCurrentThreadId(), target.unwrap_or_default());
        true
    }
}

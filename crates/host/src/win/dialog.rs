//! 配对确认对话框。
//!
//! 工作进程以 SYSTEM 身份运行在用户会话里，可以直接在用户桌面上弹出对话框。
//! 在新线程里弹出：新线程的桌面是进程默认的 winsta0\default（即使采集线程此刻切到了锁屏桌面）。

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
};

const IDYES: i32 = 6;
const TIMEOUT_MS: u32 = 60_000;

type MessageBoxTimeoutW = unsafe extern "system" fn(isize, PCWSTR, PCWSTR, u32, u16, u32) -> i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// 是 / 否对话框（默认“否”）；`timeout_ms` 内无人响应视为“否”。
pub fn ask(title: &str, text: &str, timeout_ms: u32) -> bool {
    let (text, title) = (wide(text), wide(title));
    std::thread::spawn(move || unsafe {
        let Ok(user32) = libloading::Library::new("user32.dll") else { return false };
        let Ok(f) = user32.get::<MessageBoxTimeoutW>(b"MessageBoxTimeoutW\0") else { return false };
        let flags = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL;
        f(0, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), flags.0, 0, timeout_ms) == IDYES
    })
    .join()
    .unwrap_or(false)
}

/// 询问是否允许配对；60 秒内无人响应视为拒绝。
pub fn ask_pairing(client: &str, peer: &str, code: &str) -> bool {
    let text = wide(&format!(
        "「{client}」（{peer}）请求远程控制这台电脑。\n\n配对码：{code}\n请确认与控制端屏幕上显示的配对码一致。\n\n允许后，该设备以后可以直接连接，无需再次确认。\n（60 秒内未响应将自动拒绝）"
    ));
    let title = wide("SuperControl 配对请求");
    std::thread::spawn(move || unsafe {
        let Ok(user32) = libloading::Library::new("user32.dll") else { return false };
        // 未公开但长期稳定存在的 API：带超时的 MessageBox。
        let Ok(f) = user32.get::<MessageBoxTimeoutW>(b"MessageBoxTimeoutW\0") else { return false };
        let flags = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL;
        f(0, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), flags.0, 0, TIMEOUT_MS) == IDYES
    })
    .join()
    .unwrap_or(false)
}

//! 远程窗口在前台时，把本机系统会截走的快捷键（切应用、切桌面、开始菜单 / 聚焦搜索……）留给远程窗口。
//!
//! - macOS：临时关闭系统的全局快捷键（`CGSSetGlobalHotKeyOperatingMode`，未公开但长期稳定，
//!   Jump Desktop、Moonlight 等也这么做）。这些按键随后像普通按键一样进入窗口。进程退出时系统自动恢复。
//! - Windows：低级键盘钩子。只拦 Win 键及其组合、Alt+Tab、Alt+Esc、Ctrl+Esc，拦下后通过回调交给远程窗口；
//!   其余按键照常走窗口消息。Ctrl+Alt+Del、Win+L 由系统保留，任何程序都拦不了。
//!
//! 窗口失去焦点、或用户按“交还本机”组合键时调用 `set_active(false)`，本机快捷键立即恢复。

use winit::keyboard::KeyCode;

/// 被拦下的按键交给远程窗口：(物理键, 是否按下)。
pub type OnKey = Box<dyn Fn(KeyCode, bool) + Send>;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSMainConnectionID() -> i32;
        fn CGSSetGlobalHotKeyOperatingMode(cid: i32, mode: i32) -> i32;
    }
    const ENABLE: i32 = 0;
    const DISABLE: i32 = 1;

    pub struct SysKeys {
        active: bool,
    }

    impl SysKeys {
        pub fn new(_on_key: OnKey, _window: isize) -> Self {
            Self { active: false }
        }

        pub fn set_active(&mut self, on: bool) {
            if on != self.active {
                self.active = on;
                unsafe { CGSSetGlobalHotKeyOperatingMode(CGSMainConnectionID(), if on { DISABLE } else { ENABLE }) };
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::Mutex;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_TAB};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
        LLKHF_ALTDOWN, LLKHF_EXTENDED, LLKHF_INJECTED, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };
    use winit::keyboard::PhysicalKey;
    use winit::platform::scancode::PhysicalKeyExtScancode;

    struct Ctx {
        window: isize,
        active: bool,
        /// Win 键按住期间，所有按键都拦下（否则 Win+D、Win+E 之类会被系统处理）
        win_down: bool,
        on_key: OnKey,
    }

    static CTX: Mutex<Option<Ctx>> = Mutex::new(None);

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            if let Ok(mut guard) = CTX.lock() {
                if let Some(ctx) = guard.as_mut() {
                    let ours = unsafe { GetForegroundWindow() }.0 as isize == ctx.window;
                    // 自己（或被控端）注入的按键不拦
                    if ctx.active && ours && kb.flags.0 & LLKHF_INJECTED.0 == 0 {
                        let vk = kb.vkCode as u16;
                        let is_win = vk == VK_LWIN.0 || vk == VK_RWIN.0;
                        if is_win {
                            ctx.win_down = down;
                        }
                        let alt = kb.flags.0 & LLKHF_ALTDOWN.0 != 0;
                        let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
                        let swallow = is_win
                            || ctx.win_down
                            || (alt && (vk == VK_TAB.0 || vk == VK_ESCAPE.0))
                            || (ctrl && vk == VK_ESCAPE.0);
                        if swallow {
                            let scan = kb.scanCode | if kb.flags.0 & LLKHF_EXTENDED.0 != 0 { 0xE000 } else { 0 };
                            if let PhysicalKey::Code(k) = PhysicalKey::from_scancode(scan) {
                                (ctx.on_key)(k, down);
                            }
                            return LRESULT(1);
                        }
                    }
                }
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    pub struct SysKeys {
        hook: Option<HHOOK>,
    }

    impl SysKeys {
        /// 在界面线程（有消息循环的线程）上创建：钩子回调在这个线程里执行。
        pub fn new(on_key: OnKey, window: isize) -> Self {
            *CTX.lock().unwrap() = Some(Ctx { window, active: false, win_down: false, on_key });
            let hook = unsafe {
                let module = GetModuleHandleW(None).ok();
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(|m| m.into()), 0)
            };
            if let Err(e) = &hook {
                eprintln!("无法拦截系统快捷键（Alt+Tab、Win 键将由本机处理）: {e}");
            }
            Self { hook: hook.ok() }
        }

        pub fn set_active(&mut self, on: bool) {
            if let Some(ctx) = CTX.lock().unwrap().as_mut() {
                ctx.active = on;
                ctx.win_down = false;
            }
        }
    }

    impl Drop for SysKeys {
        fn drop(&mut self) {
            if let Some(h) = self.hook.take() {
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
            }
            *CTX.lock().unwrap() = None;
        }
    }
}

pub use imp::SysKeys;

#[cfg(target_os = "macos")]
impl Drop for SysKeys {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

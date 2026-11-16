//! 通过 SendInput 注入键鼠事件。

use crate::source::{Area, InputSink};
use rc_protocol::InputEvent;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

pub struct Injector {
    /// 被捕获屏幕在虚拟桌面中的位置（切换显示器时由采集端更新）。
    area: Area,
    keys_down: HashSet<(u16, bool)>,
    buttons_down: HashSet<u8>,
    last_desktop_check: Instant,
}

/// 发送输入；若因桌面已切换（例如进入锁屏）而失败，切到新的输入桌面后重试一次。
fn send(inputs: &[INPUT]) {
    unsafe {
        if SendInput(inputs, size_of::<INPUT>() as i32) == 0 && super::desktop::sync_thread() {
            SendInput(inputs, size_of::<INPUT>() as i32);
        }
    }
}

impl Injector {
    pub fn new(area: Area) -> Self {
        Self { area, keys_down: HashSet::new(), buttons_down: HashSet::new(), last_desktop_check: Instant::now() - Duration::from_secs(1) }
    }

    fn mouse(&self, dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        send(&[input]);
    }

    fn key(&self, scancode: u16, extended: bool, down: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scancode, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        };
        send(&[input]);
    }

    fn button(&self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (4, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            _ => return,
        };
        self.mouse(0, 0, data, flags);
    }
}

/// 相对移动 1 像素再移回，用于唤醒显示器。
pub fn nudge_mouse() {
    let mv = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
    };
    super::desktop::sync_thread();
    send(&[mv(1), mv(-1)]);
}

/// 把屏幕内归一化坐标 (0..=65535) 换算成 SendInput 要求的“虚拟桌面归一化坐标”。
fn to_virtual(norm: u16, origin: i32, size: u32, v_origin: i32, v_size: i32) -> i32 {
    let px = origin as i64 + norm as i64 * (size.max(1) as i64 - 1) / 65535;
    ((px - v_origin as i64) * 65535 / (v_size.max(2) as i64 - 1)) as i32
}

impl InputSink for Injector {
    fn inject(&mut self, ev: InputEvent) {
        // SendInput 跨桌面时不一定报错（会被静默丢弃），所以也定期主动检查一次。
        if self.last_desktop_check.elapsed() > Duration::from_millis(200) {
            self.last_desktop_check = Instant::now();
            super::desktop::sync_thread();
        }
        match ev {
            InputEvent::MouseMoveAbs { x, y } => {
                let (left, top, w, h) = {
                    let a = *self.area.lock().unwrap();
                    (a.0 as i32, a.1 as i32, a.2 as u32, a.3 as u32)
                };
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                self.mouse(
                    to_virtual(x, left, w, vx, vw),
                    to_virtual(y, top, h, vy, vh),
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                );
            }
            InputEvent::MouseButton { button, down } => {
                if down {
                    self.buttons_down.insert(button);
                } else {
                    self.buttons_down.remove(&button);
                }
                self.button(button, down);
            }
            InputEvent::Wheel { dx, dy } => {
                if dy != 0 {
                    self.mouse(0, 0, dy as i32 as u32, MOUSEEVENTF_WHEEL);
                }
                if dx != 0 {
                    self.mouse(0, 0, dx as i32 as u32, MOUSEEVENTF_HWHEEL);
                }
            }
            InputEvent::Key { scancode, extended, down } => {
                if down {
                    self.keys_down.insert((scancode, extended));
                } else {
                    self.keys_down.remove(&(scancode, extended));
                }
                self.key(scancode, extended, down);
            }
        }
    }
}

impl Drop for Injector {
    /// 连接断开时松开所有仍按下的键，避免被控端出现“卡键”。
    fn drop(&mut self) {
        for &(scancode, extended) in &self.keys_down {
            self.key(scancode, extended, false);
        }
        for &b in &self.buttons_down {
            self.button(b, false);
        }
    }
}

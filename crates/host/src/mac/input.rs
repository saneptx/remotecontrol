//! macOS 输入注入（CGEvent）。需要“辅助功能”权限。

use crate::source::InputSink;
use rc_protocol::InputEvent;
use std::collections::HashSet;
use std::ffi::c_void;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    w: f64,
    h: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type Ev = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventCreateMouseEvent(src: *mut c_void, ty: u32, pos: CGPoint, button: u32) -> Ev;
    fn CGEventCreateKeyboardEvent(src: *mut c_void, key: u16, down: bool) -> Ev;
    fn CGEventCreateScrollWheelEvent2(src: *mut c_void, units: u32, count: u32, w1: i32, w2: i32, w3: i32) -> Ev;
    fn CGEventSetType(ev: Ev, ty: u32);
    fn CGEventSetFlags(ev: Ev, flags: u64);
    fn CGEventSetIntegerValueField(ev: Ev, field: u32, value: i64);
    fn CGEventPost(tap: u32, ev: Ev);
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(id: u32) -> CGRect;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

const HID_SYSTEM_STATE: i32 = 1;
const HID_EVENT_TAP: u32 = 0;
const MOUSE_MOVED: u32 = 5;
const FLAGS_CHANGED: u32 = 12;
const FIELD_CLICK_STATE: u32 = 1;
const FIELD_BUTTON_NUMBER: u32 = 3;
const SCROLL_PIXEL: u32 = 0;

const FLAG_SHIFT: u64 = 0x20000;
const FLAG_CONTROL: u64 = 0x40000;
const FLAG_OPTION: u64 = 0x80000;
const FLAG_COMMAND: u64 = 0x100000;

/// PC Set-1 扫描码 → macOS 虚拟键码。
fn to_keycode(scancode: u16, extended: bool) -> Option<u16> {
    Some(if extended {
        match scancode {
            0x1c => 0x4C, // 小键盘 Enter
            0x1d => 0x3E, // 右 Ctrl → 右 Control
            0x35 => 0x4B, // 小键盘 /
            0x37 => 0x69, // PrintScreen → F13
            0x38 => 0x3D, // 右 Alt → 右 Option
            0x47 => 0x73, // Home
            0x48 => 0x7E, // ↑
            0x49 => 0x74, // PageUp
            0x4b => 0x7B, // ←
            0x4d => 0x7C, // →
            0x4f => 0x77, // End
            0x50 => 0x7D, // ↓
            0x51 => 0x79, // PageDown
            0x52 => 0x72, // Insert → Help
            0x53 => 0x75, // Delete → 向前删除
            0x5b => 0x37, // 左 Win → Command
            0x5c => 0x36, // 右 Win → 右 Command
            0x5d => 0x6E, // 菜单键
            _ => return None,
        }
    } else {
        const T: [u16; 0x5a] = [
            0xFFFF, 0x35, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D, 0x1B, 0x18, 0x33, 0x30, // 00-0f
            0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x10, 0x20, 0x22, 0x1F, 0x23, 0x21, 0x1E, 0x24, 0x3B, 0x00, 0x01, // 10-1f
            0x02, 0x03, 0x05, 0x04, 0x26, 0x28, 0x25, 0x29, 0x27, 0x32, 0x38, 0x2A, 0x06, 0x07, 0x08, 0x09, // 20-2f
            0x0B, 0x2D, 0x2E, 0x2B, 0x2F, 0x2C, 0x3C, 0x43, 0x3A, 0x31, 0x39, 0x7A, 0x78, 0x63, 0x76, 0x60, // 30-3f
            0x61, 0x62, 0x64, 0x65, 0x6D, 0x47, 0xFFFF, 0x59, 0x5B, 0x5C, 0x4E, 0x56, 0x57, 0x58, 0x45, 0x53, // 40-4f
            0x54, 0x55, 0x52, 0x41, 0xFFFF, 0xFFFF, 0x0A, 0x67, 0x6F, 0x51, // 50-59
        ];
        match scancode {
            0x64 => 0x69,
            0x65 => 0x6B,
            0x66 => 0x71,
            s => *T.get(s as usize).filter(|&&k| k != 0xFFFF)?,
        }
    })
}

fn modifier_flag(keycode: u16) -> Option<u64> {
    match keycode {
        0x37 | 0x36 => Some(FLAG_COMMAND),
        0x38 | 0x3C => Some(FLAG_SHIFT),
        0x3A | 0x3D => Some(FLAG_OPTION),
        0x3B | 0x3E => Some(FLAG_CONTROL),
        _ => None,
    }
}

pub struct Injector {
    source: *mut c_void,
    pos: CGPoint,
    buttons: HashSet<u8>,
    keys: HashSet<u16>,
    flags: u64,
    /// 连击判断：上次按下的时间、位置、按钮、连击数
    last_click: Option<(Instant, CGPoint, u8, i64)>,
    wheel_acc: (f64, f64),
}

unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Self {
        let source = unsafe { CGEventSourceCreate(HID_SYSTEM_STATE) };
        Self { source, pos: CGPoint::default(), buttons: HashSet::new(), keys: HashSet::new(), flags: 0, last_click: None, wheel_acc: (0.0, 0.0) }
    }

    fn post(&self, ev: Ev) {
        if ev.is_null() {
            return;
        }
        unsafe {
            CGEventPost(HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
    }

    fn mouse_event(&self, ty: u32, button: u32, click_state: i64) -> Ev {
        unsafe {
            let ev = CGEventCreateMouseEvent(self.source, ty, self.pos, button);
            if !ev.is_null() {
                CGEventSetFlags(ev, self.flags);
                if click_state > 0 {
                    CGEventSetIntegerValueField(ev, FIELD_CLICK_STATE, click_state);
                }
                CGEventSetIntegerValueField(ev, FIELD_BUTTON_NUMBER, button as i64);
            }
            ev
        }
    }

    /// 按钮 → (按下类型, 抬起类型, 拖动类型, 按钮号)
    fn button_types(button: u8) -> (u32, u32, u32, u32) {
        match button {
            0 => (1, 2, 6, 0),
            1 => (3, 4, 7, 1),
            2 => (25, 26, 27, 2),
            3 => (25, 26, 27, 3), // 后退
            _ => (25, 26, 27, 4), // 前进
        }
    }

    fn button(&mut self, button: u8, down: bool) {
        let (down_ty, up_ty, _, number) = Self::button_types(button);
        let clicks = if down {
            let now = Instant::now();
            let n = match self.last_click {
                Some((t, p, b, n))
                    if b == button && now - t < Duration::from_millis(500) && (p.x - self.pos.x).abs() < 5.0 && (p.y - self.pos.y).abs() < 5.0 =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((now, self.pos, button, n));
            self.buttons.insert(button);
            n
        } else {
            self.buttons.remove(&button);
            self.last_click.map(|c| c.3).unwrap_or(1)
        };
        self.post(self.mouse_event(if down { down_ty } else { up_ty }, number, clicks));
    }

    fn key(&mut self, keycode: u16, down: bool) {
        if down {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
        unsafe {
            let ev = CGEventCreateKeyboardEvent(self.source, keycode, down);
            if ev.is_null() {
                return;
            }
            if let Some(f) = modifier_flag(keycode) {
                // 修饰键要以 FlagsChanged 事件发出，应用才会正确识别
                self.flags = if down { self.flags | f } else { self.flags & !f };
                CGEventSetType(ev, FLAGS_CHANGED);
            }
            CGEventSetFlags(ev, self.flags);
            self.post(ev);
        }
    }
}

impl InputSink for Injector {
    fn inject(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::MouseMoveAbs { x, y } => {
                let b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
                self.pos = CGPoint {
                    x: b.origin.x + x as f64 / 65535.0 * (b.size.w - 1.0),
                    y: b.origin.y + y as f64 / 65535.0 * (b.size.h - 1.0),
                };
                // 按住按钮时要发“拖动”事件，否则拖拽、框选无效
                let (ty, number) = match self.buttons.iter().min() {
                    Some(&b) => {
                        let t = Self::button_types(b);
                        (t.2, t.3)
                    }
                    None => (MOUSE_MOVED, 0),
                };
                self.post(self.mouse_event(ty, number, 0));
            }
            InputEvent::MouseButton { button, down } => self.button(button, down),
            InputEvent::Wheel { dx, dy } => {
                // Windows 一格 = 120；换算成像素滚动（一格约 48 像素）。macOS 横向正值表示向左
                self.wheel_acc.0 += -dx as f64 * 0.4;
                self.wheel_acc.1 += dy as f64 * 0.4;
                let (px, py) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
                self.wheel_acc.0 -= px;
                self.wheel_acc.1 -= py;
                if px != 0.0 || py != 0.0 {
                    unsafe {
                        let ev = CGEventCreateScrollWheelEvent2(self.source, SCROLL_PIXEL, 2, py as i32, px as i32, 0);
                        if !ev.is_null() {
                            CGEventSetFlags(ev, self.flags);
                        }
                        self.post(ev);
                    }
                }
            }
            InputEvent::Key { scancode, extended, down } => {
                if let Some(k) = to_keycode(scancode, extended) {
                    self.key(k, down);
                }
            }
        }
    }
}

impl Drop for Injector {
    /// 断开时松开所有按键与鼠标键，避免卡键。
    fn drop(&mut self) {
        for k in self.keys.clone() {
            self.key(k, false);
        }
        for b in self.buttons.clone() {
            self.button(b, false);
        }
        if !self.source.is_null() {
            unsafe { CFRelease(self.source) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::to_keycode;

    #[test]
    fn keymap() {
        assert_eq!(to_keycode(0x1e, false), Some(0x00)); // A
        assert_eq!(to_keycode(0x2e, false), Some(0x08)); // C
        assert_eq!(to_keycode(0x1c, false), Some(0x24)); // Enter
        assert_eq!(to_keycode(0x5b, true), Some(0x37)); // Win → Command
        assert_eq!(to_keycode(0x48, true), Some(0x7E)); // ↑
        assert_eq!(to_keycode(0x46, false), None);
        assert_eq!(to_keycode(0x7f, false), None);
    }
}

//! 系统快捷键的翻译：本机按的是“本机系统的习惯”，发给对方时换成“对方系统的习惯”。
//!
//! | 意图 | Mac 上按 | Windows 上按 |
//! |---|---|---|
//! | 切换应用 | ⌘Tab（⇧ 反向） | Alt+Tab（Shift 反向） |
//! | 切换桌面 | ⌃← / ⌃→ | Ctrl+Win+← / → |
//! | 总览所有窗口 | ⌃↑（调度中心） | Win+Tab（任务视图） |
//! | 显示桌面 | F11 | Win+D |
//! | 搜索 | ⌘空格（聚焦搜索） | 单按 Win（开始菜单） |
//!
//! 识别用本机的物理键和修饰键状态；输出是对方系统的扫描码（PC Set-1，与 [`crate::keymap`] 一致；
//! Mac 被控端把 Win 键当 Command、Ctrl 当 Control、Alt 当 Option）。

use winit::keyboard::KeyCode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Mac,
    Windows,
}

impl Os {
    pub const LOCAL: Os = if cfg!(target_os = "macos") { Os::Mac } else { Os::Windows };

    pub fn parse(s: &str) -> Option<Os> {
        match s {
            "macos" | "mac" => Some(Os::Mac),
            "windows" | "win" => Some(Os::Windows),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    /// 切换应用；需要按住修饰键连续切换，见 [`switch_modifier`]
    AppSwitch { reverse: bool },
    DesktopLeft,
    DesktopRight,
    Overview,
    ShowDesktop,
    Search,
}

/// 本机当前按住的修饰键。`meta`：Mac 的 Command / Windows 的 Win 键。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl Mods {
    pub fn from_keys<'a>(keys: impl IntoIterator<Item = &'a KeyCode>) -> Self {
        use KeyCode::*;
        let mut m = Mods::default();
        for k in keys {
            match k {
                ControlLeft | ControlRight => m.ctrl = true,
                AltLeft | AltRight => m.alt = true,
                ShiftLeft | ShiftRight => m.shift = true,
                SuperLeft | SuperRight => m.meta = true,
                _ => {}
            }
        }
        m
    }
}

/// 这次按键是不是某个系统快捷键。单按 Win 的“搜索”由调用方在松开 Win 时判断。
pub fn detect(local: Os, key: KeyCode, m: Mods) -> Option<Intent> {
    use KeyCode::*;
    match local {
        Os::Mac => match key {
            Tab if m.meta && !m.ctrl && !m.alt => Some(Intent::AppSwitch { reverse: m.shift }),
            ArrowLeft if m.ctrl && !m.meta && !m.alt && !m.shift => Some(Intent::DesktopLeft),
            ArrowRight if m.ctrl && !m.meta && !m.alt && !m.shift => Some(Intent::DesktopRight),
            ArrowUp if m.ctrl && !m.meta && !m.alt && !m.shift => Some(Intent::Overview),
            Space if m.meta && !m.ctrl && !m.alt && !m.shift => Some(Intent::Search),
            F11 if m == Mods::default() => Some(Intent::ShowDesktop),
            _ => None,
        },
        Os::Windows => match key {
            Tab if m.alt && !m.ctrl && !m.meta => Some(Intent::AppSwitch { reverse: m.shift }),
            ArrowLeft if m.ctrl && m.meta && !m.alt => Some(Intent::DesktopLeft),
            ArrowRight if m.ctrl && m.meta && !m.alt => Some(Intent::DesktopRight),
            Tab if m.meta && !m.ctrl && !m.alt => Some(Intent::Overview),
            KeyD if m.meta && !m.ctrl && !m.alt && !m.shift => Some(Intent::ShowDesktop),
            _ => None,
        },
    }
}

/// 本机切换应用时按住的修饰键：松开它即结束切换。
pub fn is_switch_modifier(local: Os, key: KeyCode) -> bool {
    use KeyCode::*;
    match local {
        Os::Mac => matches!(key, SuperLeft | SuperRight),
        Os::Windows => matches!(key, AltLeft | AltRight),
    }
}

pub type Scan = (u16, bool);

const CTRL: Scan = (0x1d, false);
const ALT: Scan = (0x38, false);
const SHIFT: Scan = (0x2a, false);
/// Windows 的 Win 键；Mac 被控端当作 Command
const WIN: Scan = (0x5b, true);
const TAB: Scan = (0x0f, false);
const LEFT: Scan = (0x4b, true);
const RIGHT: Scan = (0x4d, true);
const UP: Scan = (0x48, true);
const KEY_D: Scan = (0x20, false);
const SPACE: Scan = (0x39, false);
const F11: Scan = (0x57, false);

pub const SHIFT_SCAN: Scan = SHIFT;
pub const TAB_SCAN: Scan = TAB;

/// 切换应用时在对方那边按住的修饰键。
pub fn switch_modifier(remote: Os) -> Scan {
    match remote {
        Os::Mac => WIN,
        Os::Windows => ALT,
    }
}

/// 一次性快捷键在对方那边要按的：(修饰键, 主键)。切换应用不在这里（要按住修饰键）。
pub fn chord(remote: Os, intent: Intent) -> Option<(Vec<Scan>, Scan)> {
    Some(match (remote, intent) {
        (_, Intent::AppSwitch { .. }) => return None,
        (Os::Windows, Intent::DesktopLeft) => (vec![CTRL, WIN], LEFT),
        (Os::Windows, Intent::DesktopRight) => (vec![CTRL, WIN], RIGHT),
        (Os::Windows, Intent::Overview) => (vec![WIN], TAB),
        (Os::Windows, Intent::ShowDesktop) => (vec![WIN], KEY_D),
        (Os::Windows, Intent::Search) => (vec![], WIN),
        (Os::Mac, Intent::DesktopLeft) => (vec![CTRL], LEFT),
        (Os::Mac, Intent::DesktopRight) => (vec![CTRL], RIGHT),
        (Os::Mac, Intent::Overview) => (vec![CTRL], UP),
        (Os::Mac, Intent::ShowDesktop) => (vec![], F11),
        (Os::Mac, Intent::Search) => (vec![WIN], SPACE),
    })
}

/// “交还本机快捷键”的组合：Mac 上 ⌃⌥⌘ 同时按住，Windows 上 Ctrl+Alt+Win 同时按住。
pub fn is_release_combo(m: Mods) -> bool {
    m.ctrl && m.alt && m.meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCode::*;

    fn mods(ctrl: bool, alt: bool, shift: bool, meta: bool) -> Mods {
        Mods { ctrl, alt, shift, meta }
    }

    #[test]
    fn mac_shortcuts() {
        assert_eq!(detect(Os::Mac, Tab, mods(false, false, false, true)), Some(Intent::AppSwitch { reverse: false }));
        assert_eq!(detect(Os::Mac, Tab, mods(false, false, true, true)), Some(Intent::AppSwitch { reverse: true }));
        assert_eq!(detect(Os::Mac, ArrowLeft, mods(true, false, false, false)), Some(Intent::DesktopLeft));
        assert_eq!(detect(Os::Mac, ArrowUp, mods(true, false, false, false)), Some(Intent::Overview));
        assert_eq!(detect(Os::Mac, Space, mods(false, false, false, true)), Some(Intent::Search));
        // ⌃⌥← 是切换远端显示器，不是切换桌面
        assert_eq!(detect(Os::Mac, ArrowLeft, mods(true, true, false, false)), None);
        // 普通的 ⌘C 不翻译
        assert_eq!(detect(Os::Mac, KeyC, mods(false, false, false, true)), None);
    }

    #[test]
    fn windows_shortcuts() {
        assert_eq!(detect(Os::Windows, Tab, mods(false, true, false, false)), Some(Intent::AppSwitch { reverse: false }));
        assert_eq!(detect(Os::Windows, ArrowRight, mods(true, false, false, true)), Some(Intent::DesktopRight));
        assert_eq!(detect(Os::Windows, Tab, mods(false, false, false, true)), Some(Intent::Overview));
        assert_eq!(detect(Os::Windows, KeyD, mods(false, false, false, true)), Some(Intent::ShowDesktop));
        // Ctrl+Alt+← 是切换远端显示器
        assert_eq!(detect(Os::Windows, ArrowLeft, mods(true, true, false, false)), None);
    }

    #[test]
    fn chords_follow_remote_os() {
        assert_eq!(chord(Os::Windows, Intent::DesktopLeft), Some((vec![CTRL, WIN], LEFT)));
        assert_eq!(chord(Os::Mac, Intent::DesktopLeft), Some((vec![CTRL], LEFT)));
        assert_eq!(chord(Os::Mac, Intent::Search), Some((vec![WIN], SPACE)));
        assert_eq!(chord(Os::Windows, Intent::Search), Some((vec![], WIN)));
        assert_eq!(switch_modifier(Os::Mac), WIN);
        assert_eq!(switch_modifier(Os::Windows), ALT);
        assert!(is_release_combo(mods(true, true, false, true)));
        assert!(!is_release_combo(mods(true, true, false, false)));
    }
}

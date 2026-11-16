//! 识别当前光标形状：把 GetCursorInfo 的光标句柄和系统标准光标比较。
//! 系统标准光标是全局共享的，同一种光标在所有进程里句柄相同；应用自绘的光标按箭头处理。

use rc_protocol::CursorShape;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND,
    IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT,
};

pub struct Cursors {
    table: Vec<(isize, CursorShape)>,
    last: CursorShape,
}

impl Cursors {
    pub fn new() -> Self {
        let ids = [
            (IDC_ARROW, CursorShape::Arrow),
            (IDC_IBEAM, CursorShape::Text),
            (IDC_HAND, CursorShape::Hand),
            (IDC_WAIT, CursorShape::Wait),
            (IDC_APPSTARTING, CursorShape::Progress),
            (IDC_CROSS, CursorShape::Crosshair),
            (IDC_SIZENS, CursorShape::ResizeNS),
            (IDC_SIZEWE, CursorShape::ResizeEW),
            (IDC_SIZENWSE, CursorShape::ResizeNWSE),
            (IDC_SIZENESW, CursorShape::ResizeNESW),
            (IDC_SIZEALL, CursorShape::Move),
            (IDC_NO, CursorShape::NotAllowed),
            (IDC_HELP, CursorShape::Help),
        ];
        let table = ids
            .into_iter()
            .filter_map(|(id, shape)| unsafe { LoadCursorW(None, id) }.ok().map(|h| (h.0 as isize, shape)))
            .collect();
        Self { table, last: CursorShape::Arrow }
    }

    /// 读不到（例如桌面切换的瞬间）时沿用上一次的结果。
    pub fn current(&mut self) -> CursorShape {
        let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return self.last;
        }
        self.last = if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor == HCURSOR::default() {
            CursorShape::Hidden
        } else {
            let h = info.hCursor.0 as isize;
            self.table.iter().find(|(c, _)| *c == h).map(|(_, s)| *s).unwrap_or(CursorShape::Arrow)
        };
        self.last
    }
}

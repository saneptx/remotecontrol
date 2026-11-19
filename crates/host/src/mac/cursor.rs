//! 识别当前光标形状（macOS）：把系统当前光标渲染成 32×32 灰度小图，和 AppKit 的标准光标逐个比较。
//!
//! 光标不画进视频，而是把形状发给控制端，由它在本地画——这样光标移动不用绕一圈网络，跟手没有延迟。
//! 应用自绘的光标（以及忙碌时的彩色转圈）按箭头处理。

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use rc_protocol::CursorShape;
use std::ffi::c_void;
use std::time::{Duration, Instant};

const SIDE: usize = 32;
type Bitmap = Vec<u8>;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateDeviceGray() -> *mut c_void;
    fn CGColorSpaceRelease(cs: *mut c_void);
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bpc: usize, bpr: usize, cs: *mut c_void, info: u32) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *const c_void);
    fn CGContextRelease(ctx: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// 主线程启动时调用一次：初始化 AppKit（否则标准光标的图像是空的）。不会出现在程序坞里。
pub fn init() {
    unsafe {
        if let Some(cls) = AnyClass::get(c"NSApplication") {
            let app: *mut AnyObject = msg_send![cls, sharedApplication];
            // NSApplicationActivationPolicyProhibited = 2
            let _: Bool = msg_send![app, setActivationPolicy: 2isize];
        }
    }
}

fn render(cursor: &AnyObject) -> Option<Bitmap> {
    unsafe {
        let image: *mut AnyObject = msg_send![cursor, image];
        if image.is_null() {
            return None;
        }
        // 不指定尺寸：取图像原本的大小，下面画进 32×32 时再统一缩放
        let (rect, null) = (std::ptr::null_mut::<c_void>(), std::ptr::null::<AnyObject>());
        let cg: *const c_void = msg_send![image, CGImageForProposedRect: rect, context: null, hints: null];
        if cg.is_null() {
            return None;
        }
        let mut buf = vec![0u8; SIDE * SIDE];
        let cs = CGColorSpaceCreateDeviceGray();
        // kCGImageAlphaNone = 0
        let ctx = CGBitmapContextCreate(buf.as_mut_ptr().cast(), SIDE, SIDE, 8, SIDE, cs, 0);
        CGColorSpaceRelease(cs);
        if ctx.is_null() {
            return None;
        }
        CGContextDrawImage(ctx, CGRect { x: 0.0, y: 0.0, w: SIDE as f64, h: SIDE as f64 }, cg);
        CGContextRelease(ctx);
        Some(buf)
    }
}

/// AppKit 的某个标准光标（`NSCursor` 的类方法）。
macro_rules! standard {
    ($sel:ident) => {
        AnyClass::get(c"NSCursor").and_then(|cls| unsafe {
            let obj: *mut AnyObject = msg_send![cls, $sel];
            Retained::retain(obj)
        })
    };
}

pub struct Cursors {
    table: Vec<(Bitmap, CursorShape)>,
    last: CursorShape,
    checked: Instant,
}

impl Cursors {
    pub fn new() -> Self {
        let cursors = [
            (standard!(arrowCursor), CursorShape::Arrow),
            (standard!(IBeamCursor), CursorShape::Text),
            (standard!(IBeamCursorForVerticalLayout), CursorShape::Text),
            (standard!(pointingHandCursor), CursorShape::Hand),
            (standard!(crosshairCursor), CursorShape::Crosshair),
            (standard!(openHandCursor), CursorShape::Move),
            (standard!(closedHandCursor), CursorShape::Move),
            (standard!(resizeLeftRightCursor), CursorShape::ResizeEW),
            (standard!(resizeLeftCursor), CursorShape::ResizeEW),
            (standard!(resizeRightCursor), CursorShape::ResizeEW),
            (standard!(resizeUpDownCursor), CursorShape::ResizeNS),
            (standard!(resizeUpCursor), CursorShape::ResizeNS),
            (standard!(resizeDownCursor), CursorShape::ResizeNS),
            (standard!(operationNotAllowedCursor), CursorShape::NotAllowed),
            (standard!(contextualMenuCursor), CursorShape::Arrow),
        ];
        let table = cursors.into_iter().filter_map(|(c, shape)| c.and_then(|c| render(&c)).map(|b| (b, shape))).collect();
        Self { table, last: CursorShape::Arrow, checked: Instant::now() - Duration::from_secs(1) }
    }

    /// 每 50 ms 最多实际识别一次（每帧都会调用）。
    pub fn current(&mut self) -> CursorShape {
        if self.checked.elapsed() < Duration::from_millis(50) {
            return self.last;
        }
        self.checked = Instant::now();
        let Some(cur) = standard!(currentSystemCursor) else {
            return self.last;
        };
        let Some(bmp) = render(&cur) else { return self.last };
        let diff = |a: &Bitmap| a.iter().zip(&bmp).map(|(x, y)| x.abs_diff(*y) as u32).sum::<u32>();
        // 最接近的标准光标；差得太多（应用自绘的光标）按箭头处理
        self.last = self
            .table
            .iter()
            .map(|(b, s)| (diff(b), *s))
            .min_by_key(|(d, _)| *d)
            .filter(|(d, _)| *d < 4000)
            .map(|(_, s)| s)
            .unwrap_or(CursorShape::Arrow);
        self.last
    }
}

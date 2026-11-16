//! 托盘（Windows 通知区域）/ 菜单栏（macOS）图标。
//!
//! 关闭主窗口时只是隐藏到这里，被控端照常运行；菜单里可以重新打开窗口、开关“允许被控制”或退出。
//! 菜单和点击事件在系统的事件线程上回调，这里只把命令发给界面，并唤醒界面处理。

use anyhow::Result;
use eframe::egui;
use std::sync::mpsc::Sender;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayCmd {
    /// 打开主窗口
    Show,
    /// “允许其他设备控制”被点了（勾选状态已由菜单自己切换）
    ToggleHost,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    host: CheckMenuItem,
}

impl Tray {
    /// `rgba` 为 256×256 的应用图标。
    pub fn new(rgba: &[u8], host_on: bool, ctx: egui::Context, tx: Sender<TrayCmd>) -> Result<Self> {
        let show = MenuItem::new("打开 SuperControl", true, None);
        let host = CheckMenuItem::new("允许其他设备控制", true, host_on, None);
        let quit = MenuItem::new("退出 SuperControl", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &PredefinedMenuItem::separator(), &host, &PredefinedMenuItem::separator(), &quit])?;

        let ids = (show.id().clone(), host.id().clone(), quit.id().clone());
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let cmd = if e.id == ids.0 {
                    TrayCmd::Show
                } else if e.id == ids.1 {
                    TrayCmd::ToggleHost
                } else if e.id == ids.2 {
                    TrayCmd::Quit
                } else {
                    return;
                };
                let _ = tx.send(cmd);
                ctx.request_repaint();
            }));
        }
        // Windows：左键单击图标打开窗口（右键出菜单）
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                if cfg!(windows) {
                    let _ = tx.send(TrayCmd::Show);
                    ctx.request_repaint();
                }
            }
        }));

        let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("SuperControl");
        #[cfg(target_os = "macos")]
        let builder = {
            let (px, size) = template_icon(rgba, 36);
            builder.with_icon_templated(Icon::from_rgba(px, size, size)?).with_menu_on_left_click(true)
        };
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(Icon::from_rgba(resample(rgba, 256, 0, 0, 256, 32), 32, 32)?).with_menu_on_left_click(false);
        Ok(Self { _icon: builder.build()?, host })
    }

    pub fn host_checked(&self) -> bool {
        self.host.is_checked()
    }

    pub fn set_host(&self, on: bool) {
        self.host.set_checked(on);
    }
}

/// 从 `src`（宽 `w` 的 RGBA）里取左上角 (x0, y0)、边长 `side` 的正方形，缩成 `out`×`out`（区域平均）。
fn resample(src: &[u8], w: usize, x0: usize, y0: usize, side: usize, out: usize) -> Vec<u8> {
    let mut px = vec![0u8; out * out * 4];
    for oy in 0..out {
        for ox in 0..out {
            let (sx0, sx1) = (x0 + ox * side / out, x0 + (ox + 1) * side / out);
            let (sy0, sy1) = (y0 + oy * side / out, y0 + (oy + 1) * side / out);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in sy0..sy1.max(sy0 + 1) {
                for x in sx0..sx1.max(sx0 + 1) {
                    let i = (y * w + x) * 4;
                    // 预乘 alpha 后再平均，边缘不发灰
                    let a = src[i + 3] as u32;
                    acc[0] += src[i] as u32 * a / 255;
                    acc[1] += src[i + 1] as u32 * a / 255;
                    acc[2] += src[i + 2] as u32 * a / 255;
                    acc[3] += a;
                    n += 1;
                }
            }
            let o = (oy * out + ox) * 4;
            let a = acc[3] / n;
            px[o + 3] = a as u8;
            for c in 0..3 {
                px[o + c] = if a == 0 { 0 } else { ((acc[c] / n) * 255 / a).min(255) as u8 };
            }
        }
    }
    px
}

/// macOS 菜单栏用的模板图标：只取图标里白色的六边形（十字是镂空），系统会按深浅色自动着色。
#[cfg(target_os = "macos")]
fn template_icon(rgba: &[u8], out: usize) -> (Vec<u8>, u32) {
    let w = 256;
    let mut mask = vec![0u8; w * w * 4];
    let (mut min, mut max) = ((w, w), (0, 0));
    for y in 0..w {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let white = rgba[i] > 200 && rgba[i + 1] > 200 && rgba[i + 2] > 200 && rgba[i + 3] > 200;
            if white {
                mask[i + 3] = 255;
                min = (min.0.min(x), min.1.min(y));
                max = (max.0.max(x), max.1.max(y));
            }
        }
    }
    // 以六边形为中心裁成正方形，四周留一点边
    let side = ((max.0 - min.0).max(max.1 - min.1) as f32 * 1.12) as usize;
    let cx = (min.0 + max.0) / 2;
    let cy = (min.1 + max.1) / 2;
    let x0 = cx.saturating_sub(side / 2).min(w - side);
    let y0 = cy.saturating_sub(side / 2).min(w - side);
    (resample(&mask, w, x0, y0, side, out), out as u32)
}

/// macOS：窗口隐藏时也去掉程序坞里的图标，只留菜单栏图标；显示时恢复并切到前台。
#[cfg(target_os = "macos")]
pub fn set_dock_visible(visible: bool) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    unsafe {
        let Some(cls) = AnyClass::get(c"NSApplication") else { return };
        let app: *mut AnyObject = msg_send![cls, sharedApplication];
        // NSApplicationActivationPolicyRegular = 0，Accessory = 1
        let policy: isize = if visible { 0 } else { 1 };
        let _: Bool = msg_send![app, setActivationPolicy: policy];
        if visible {
            let _: () = msg_send![app, activateIgnoringOtherApps: Bool::YES];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_visible(_visible: bool) {}

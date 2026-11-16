//! 界面的视觉风格与小部件：配色、字体、开关、卡片、状态标签、手绘图标。
//!
//! 风格参考 Apple 的系统界面：黑白灰为主、大留白、细分隔线，只有状态圆点带颜色。

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, Pos2, Rect, Response, RichText, Sense, Shadow,
    Stroke, StrokeKind, TextStyle, Ui, Vec2, Widget,
};

pub const BLACK: Color32 = Color32::from_rgb(0x1D, 0x1D, 0x1F);
pub const BLACK_HOVER: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3C);
pub const TEXT: Color32 = BLACK;
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x6E, 0x6E, 0x73);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0xAE, 0xAE, 0xB2);
pub const BG: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF7);
pub const SIDEBAR: Color32 = Color32::from_rgb(0xEB, 0xEB, 0xEE);
pub const CARD: Color32 = Color32::WHITE;
pub const FILL: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF4);
pub const BORDER: Color32 = Color32::from_rgb(0xE5, 0xE5, 0xEA);
// 状态圆点
pub const GREEN: Color32 = Color32::from_rgb(0x34, 0xC7, 0x59);
pub const ORANGE: Color32 = Color32::from_rgb(0xFF, 0x9F, 0x0A);
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x30);
pub const GRAY: Color32 = Color32::from_rgb(0xC7, 0xC7, 0xCC);

/// 全局样式：浅色、圆角、宽松的间距。
pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.style_mut_of(egui::Theme::Light, |style| {
        use egui::FontFamily::Proportional;
        style.text_styles = [
            (TextStyle::Heading, FontId::new(24.0, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Small, FontId::new(12.0, Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, egui::FontFamily::Monospace)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 8.0);
        s.button_padding = Vec2::new(14.0, 6.0);
        s.interact_size.y = 28.0;

        let v = &mut style.visuals;
        v.override_text_color = None;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = Color32::WHITE;
        v.faint_bg_color = BG;
        v.window_corner_radius = CornerRadius::same(14);
        v.window_stroke = Stroke::NONE;
        v.window_shadow = Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(40) };
        v.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(25) };
        v.selection.bg_fill = Color32::from_rgb(0xCF, 0xDF, 0xFA);
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = TEXT;
        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        for (wv, fill) in [
            (&mut w.inactive, Color32::from_rgb(0xEC, 0xEC, 0xEF)),
            (&mut w.hovered, Color32::from_rgb(0xE3, 0xE3, 0xE7)),
            (&mut w.active, Color32::from_rgb(0xD9, 0xD9, 0xDE)),
        ] {
            wv.weak_bg_fill = fill;
            wv.bg_fill = fill;
            wv.bg_stroke = Stroke::NONE;
            wv.fg_stroke = Stroke::new(1.0, TEXT);
            wv.expansion = 0.0;
        }
        // 输入框：白底细边框
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xD1, 0xD1, 0xD6));
        for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            wv.corner_radius = CornerRadius::same(8);
        }
    });
}

/// 系统界面字体：macOS 用 SF Pro，Windows 用 Segoe UI（中文仍由系统字体补全）。
pub fn load_system_font(ctx: &egui::Context) {
    let path = if cfg!(target_os = "macos") { "/System/Library/Fonts/SFNS.ttf" } else { r"C:\Windows\Fonts\segoeui.ttf" };
    if let Ok(bytes) = std::fs::read(path) {
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "system-ui",
            egui::FontData::from_owned(bytes),
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
    }
}

/// 白色圆角卡片。
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(16))
        .shadow(Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(6) })
}

/// 主按钮（黑底白字）。
pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = BLACK;
        w.hovered.weak_bg_fill = BLACK_HOVER;
        w.active.weak_bg_fill = BLACK_HOVER;
        w.inactive.bg_stroke = Stroke::NONE;
        w.hovered.bg_stroke = Stroke::NONE;
        w.noninteractive.weak_bg_fill = Color32::from_rgb(0xC7, 0xC7, 0xCC);
        w.noninteractive.bg_stroke = Stroke::NONE;
        ui.add_enabled(enabled, egui::Button::new(RichText::new(text).color(Color32::WHITE)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 次要按钮（浅灰底）。
pub fn secondary_button(ui: &mut Ui, text: &str) -> Response {
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::NONE;
        ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::NONE;
        ui.add(egui::Button::new(RichText::new(text).color(TEXT)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 分段选择按钮：选中时黑底白字，否则浅灰底。
pub fn segment(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let (fg, bg) = if selected { (Color32::WHITE, BLACK) } else { (TEXT, FILL) };
    ui.add(egui::Button::new(RichText::new(text).size(12.5).color(fg)).fill(bg).stroke(Stroke::NONE).corner_radius(7))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 文字按钮（无底色）。
pub fn text_button(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(color).size(13.0)).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 关闭按钮（手绘的 ×，不依赖字体里有没有这个符号）。
pub fn close_button(ui: &mut Ui, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let c = if resp.hovered() { Color32::WHITE } else { color };
    let r = rect.shrink(5.0);
    let s = Stroke::new(1.5, c);
    ui.painter().line_segment([r.left_top(), r.right_bottom()], s);
    ui.painter().line_segment([r.right_top(), r.left_bottom()], s);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 开关（打开时为黑色）。
pub struct Toggle<'a>(pub &'a mut bool);

impl Widget for Toggle<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let size = Vec2::new(38.0, 22.0);
        let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
        if resp.clicked() {
            *self.0 = !*self.0;
            resp.mark_changed();
        }
        let t = ui.ctx().animate_bool_responsive(resp.id, *self.0);
        let bg = lerp_color(Color32::from_rgb(0xE0, 0xE0, 0xE5), BLACK, t);
        let p = ui.painter();
        p.rect_filled(rect, 11, bg);
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
        p.circle_filled(Pos2::new(x, rect.center().y + 0.6), 9.0, Color32::from_black_alpha(22));
        p.circle_filled(Pos2::new(x, rect.center().y), 8.5, Color32::WHITE);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// 状态标签：浅灰底 + 彩色圆点 + 深灰文字。尺寸固定、直接绘制，放在任何布局里都一样。
pub fn chip(ui: &mut Ui, text: &str, dot: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(12.0), TEXT_WEAK);
    let size = Vec2::new(8.0 + 7.0 + 5.0 + galley.size().x + 9.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 11, FILL);
    p.circle_filled(egui::pos2(rect.left() + 11.5, rect.center().y), 3.5, dot);
    p.galley(egui::pos2(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0), galley, TEXT_WEAK);
}

/// 设置项：左边标题 + 说明，右边控件（`right` 是给右侧控件留的宽度）。
pub fn setting_row(ui: &mut Ui, title: &str, desc: &str, right: f32, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new((ui.available_width() - right).max(120.0), 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(title).size(14.0).color(TEXT));
                if !desc.is_empty() {
                    ui.label(RichText::new(desc).size(12.0).color(TEXT_WEAK));
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// 卡片内的分隔线。
pub fn divider(ui: &mut Ui) {
    let r = ui.available_rect_before_wrap();
    let y = r.top() + 4.0;
    ui.painter().line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], Stroke::new(1.0, BORDER));
    ui.add_space(9.0);
}

// ---------------------------------------------------------------------------
// 手绘图标（线条风格，不依赖图标字体）
// ---------------------------------------------------------------------------

/// 台式显示器。
pub fn icon_monitor(p: &egui::Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let s = Stroke::new((w * 0.075).max(1.3), color);
    let screen = Rect::from_min_max(rect.min + Vec2::new(w * 0.08, w * 0.14), Pos2::new(rect.max.x - w * 0.08, rect.min.y + w * 0.70));
    p.rect_stroke(screen, w * 0.09, s, StrokeKind::Middle);
    let cx = rect.center().x;
    p.line_segment([Pos2::new(cx, screen.max.y), Pos2::new(cx, rect.min.y + w * 0.84)], s);
    p.line_segment([Pos2::new(cx - w * 0.2, rect.min.y + w * 0.86), Pos2::new(cx + w * 0.2, rect.min.y + w * 0.86)], s);
}

/// 笔记本电脑。
pub fn icon_laptop(p: &egui::Painter, rect: Rect, color: Color32) {
    let w = rect.width();
    let s = Stroke::new((w * 0.075).max(1.3), color);
    let screen = Rect::from_min_max(rect.min + Vec2::new(w * 0.17, w * 0.2), Pos2::new(rect.max.x - w * 0.17, rect.min.y + w * 0.68));
    p.rect_stroke(screen, w * 0.07, s, StrokeKind::Middle);
    let y = rect.min.y + w * 0.80;
    p.line_segment([Pos2::new(rect.min.x + w * 0.04, y), Pos2::new(rect.max.x - w * 0.04, y)], Stroke::new(s.width * 1.2, color));
}

/// 齿轮（设置）。
pub fn icon_gear(p: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = rect.width() * 0.3;
    let s = Stroke::new((rect.width() * 0.09).max(1.3), color);
    for i in 0..8 {
        let d = Vec2::angled(i as f32 * std::f32::consts::TAU / 8.0);
        p.line_segment([c + d * r, c + d * (r + rect.width() * 0.16)], Stroke::new(s.width * 1.4, color));
    }
    p.circle_stroke(c, r, s);
    p.circle_stroke(c, r * 0.38, s);
}

/// 设备图标：浅灰圆角底 + 线条显示器 / 笔记本。
pub fn device_tile(ui: &mut Ui, os: &str, size: f32, online: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let fg = if online { TEXT } else { TEXT_FAINT };
    ui.painter().rect_filled(rect, size * 0.26, FILL);
    let inner = rect.shrink(size * 0.22);
    if os == "macos" {
        icon_laptop(ui.painter(), inner, fg);
    } else {
        icon_monitor(ui.painter(), inner, fg);
    }
}

/// 系统名称（显示用）。
pub fn os_label(os: &str) -> &'static str {
    match os {
        "windows" => "Windows",
        "macos" => "macOS",
        _ => "电脑",
    }
}

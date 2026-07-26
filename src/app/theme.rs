//! 主题色板、控件样式与窗口外观。
//!
//! 颜色一律经 `pal()` 取用：色板是运行时值，深色模式换的是整套色板。
//! 这里曾经是一批编译期浅色常量，深色模式只改了 egui 的 visuals，而
//! 界面所有卡片/文字都直接引用常量，结果深色下仍是白底浅灰字。
use crate::worker::{LogLevel, WatchState};
use eframe::egui::{self, Align2, Color32, RichText, Sense, Vec2};
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

/// 当前是否深色。`apply_style` 是唯一写入点，必须在任何 `pal()` 读取
/// 之前调用（`CourseApp::new` 里已如此）。
static DARK_MODE: AtomicBool = AtomicBool::new(false);

/// 一套完整界面色板。浅色/深色各一份常量，运行时二选一。
pub(crate) struct Palette {
    /// 页面底色。
    pub fog: Color32,
    /// 常规卡片面。
    pub glass: Color32,
    /// 主面板/浮窗面。
    pub glass_strong: Color32,
    /// 卡片内的次级面（比 glass 略有层次差）。
    pub glass_soft: Color32,
    pub line: Color32,
    pub row_line: Color32,
    pub header_fill: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub blue: Color32,
    pub green: Color32,
    pub red: Color32,
    pub amber: Color32,
    pub quiet_fill: Color32,
    pub quiet_hover: Color32,
    pub disabled_fill: Color32,
    pub row_hover: Color32,
    pub row_alt: Color32,
    /// 聚焦/悬停时的描边。
    pub focus_line: Color32,
    /// 文本框、数值框等录入控件的底色。
    pub input_bg: Color32,
    /// 危险操作的浅底与描边（软红）。
    pub danger_fill: Color32,
    pub danger_line: Color32,
    /// 成功提示的浅底（吐司）。
    pub success_fill: Color32,
    /// 开关按钮的选中底色。
    pub toggle_on_fill: Color32,
    /// 落在强调色实底上的文字色：深色模式的强调色本身很亮，白字读不清。
    pub on_accent: Color32,
    /// 文本选区底色（带透明度）。
    pub selection_fill: Color32,
}

const LIGHT: Palette = Palette {
    fog: Color32::from_rgb(246, 248, 250),
    glass: Color32::from_rgb(255, 255, 255),
    glass_strong: Color32::from_rgb(255, 255, 255),
    glass_soft: Color32::from_rgb(248, 250, 251),
    line: Color32::from_rgb(220, 228, 233),
    row_line: Color32::from_rgb(232, 237, 240),
    header_fill: Color32::from_rgb(236, 243, 246),
    text: Color32::from_rgb(33, 42, 48),
    muted: Color32::from_rgb(108, 122, 130),
    blue: Color32::from_rgb(41, 111, 130),
    green: Color32::from_rgb(43, 122, 96),
    red: Color32::from_rgb(184, 90, 80),
    amber: Color32::from_rgb(164, 116, 54),
    quiet_fill: Color32::from_rgb(244, 247, 249),
    quiet_hover: Color32::from_rgb(232, 241, 245),
    disabled_fill: Color32::from_rgb(238, 241, 243),
    row_hover: Color32::from_rgb(238, 246, 248),
    row_alt: Color32::from_rgb(250, 252, 253),
    focus_line: Color32::from_rgb(170, 196, 206),
    input_bg: Color32::from_rgb(255, 255, 255),
    danger_fill: Color32::from_rgb(252, 240, 239),
    danger_line: Color32::from_rgb(232, 198, 194),
    success_fill: Color32::from_rgb(232, 245, 238),
    toggle_on_fill: Color32::from_rgb(232, 243, 247),
    on_accent: Color32::from_rgb(255, 255, 255),
    selection_fill: Color32::from_rgba_unmultiplied_const(57, 129, 146, 48),
};

// 深色一侧沿用浅色的层次关系，只是方向相反：越“抬起”的面越亮。
const DARK: Palette = Palette {
    fog: Color32::from_rgb(15, 23, 42),
    glass: Color32::from_rgb(30, 41, 59),
    glass_strong: Color32::from_rgb(30, 41, 59),
    glass_soft: Color32::from_rgb(37, 49, 70),
    line: Color32::from_rgb(71, 85, 105),
    row_line: Color32::from_rgb(51, 65, 85),
    header_fill: Color32::from_rgb(40, 54, 78),
    text: Color32::from_rgb(226, 232, 240),
    muted: Color32::from_rgb(148, 163, 184),
    blue: Color32::from_rgb(96, 165, 250),
    green: Color32::from_rgb(74, 222, 128),
    red: Color32::from_rgb(248, 113, 113),
    amber: Color32::from_rgb(251, 191, 36),
    quiet_fill: Color32::from_rgb(51, 65, 85),
    quiet_hover: Color32::from_rgb(71, 85, 105),
    disabled_fill: Color32::from_rgb(39, 51, 70),
    row_hover: Color32::from_rgb(45, 60, 84),
    row_alt: Color32::from_rgb(26, 36, 54),
    focus_line: Color32::from_rgb(96, 165, 250),
    input_bg: Color32::from_rgb(51, 65, 85),
    danger_fill: Color32::from_rgb(62, 32, 34),
    danger_line: Color32::from_rgb(122, 62, 60),
    success_fill: Color32::from_rgb(22, 58, 45),
    toggle_on_fill: Color32::from_rgb(30, 58, 84),
    on_accent: Color32::from_rgb(15, 23, 42),
    selection_fill: Color32::from_rgba_unmultiplied_const(59, 130, 246, 60),
};

/// 当前生效的色板。
pub(crate) fn pal() -> &'static Palette {
    if DARK_MODE.load(Ordering::Relaxed) {
        &DARK
    } else {
        &LIGHT
    }
}

pub(crate) const CONTROL_H: f32 = 38.0;
pub(crate) const PANEL_TITLE: f32 = 16.0;
pub(crate) const BODY_SIZE: f32 = 14.0;
pub(crate) const META_SIZE: f32 = 13.0;
pub(crate) const CAPTION_SIZE: f32 = 12.0;
pub(crate) const CARD_RADIUS: f32 = 12.0;
pub(crate) const WATCH_CARD_MIN_H: f32 = 78.0;

pub(crate) fn on_off(value: bool) -> &'static str {
    if value {
        "开"
    } else {
        "关"
    }
}

pub(crate) fn custom_host_requiring_confirmation(raw: &str) -> Option<String> {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return None;
    };
    let host_raw = parsed.host_str()?;
    let host = host_raw.to_ascii_lowercase();
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        None
    } else {
        Some(host)
    }
}

pub(crate) fn glass_strip(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(pal().glass)
        .stroke(egui::Stroke::new(1.0, pal().line))
        .corner_radius(CARD_RADIUS)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            let width = ui.available_width();
            if width.is_finite() {
                ui.set_width(width);
            }
            add(ui);
        });
}

pub(crate) fn glass_surface(ui: &mut egui::Ui, fill_height: bool, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(pal().glass_strong)
        .stroke(egui::Stroke::new(1.0, pal().line))
        .corner_radius(CARD_RADIUS + 1.0)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            let available = ui.available_size();
            if available.x.is_finite() {
                ui.set_width(available.x);
            }
            if fill_height && available.y.is_finite() {
                ui.set_min_height(available.y);
            }
            add(ui);
        });
}

pub(crate) fn primary_button(text: &str, color: Color32, width: f32) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .size(BODY_SIZE)
            .strong()
            .color(pal().on_accent),
    )
    .fill(color)
    .stroke(egui::Stroke::new(1.0, color))
    .corner_radius(8.0)
    .min_size(Vec2::new(width, CONTROL_H))
}

pub(crate) fn quiet_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(META_SIZE).color(pal().text))
        .fill(pal().quiet_fill)
        .stroke(egui::Stroke::new(1.0, pal().line))
        .corner_radius(8.0)
        .min_size(Vec2::new(width, CONTROL_H))
}

pub(crate) fn soft_danger_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .size(META_SIZE)
            .strong()
            .color(pal().red),
    )
    .fill(pal().danger_fill)
    .stroke(egui::Stroke::new(1.0, pal().danger_line))
    .corner_radius(8.0)
    .min_size(Vec2::new(width, CONTROL_H))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn number_drag_f64(
    ui: &mut egui::Ui,
    value: &mut f64,
    enabled: bool,
    range: std::ops::RangeInclusive<f64>,
    speed: f64,
    decimals: usize,
    unit: &str,
    width: f32,
) -> egui::Response {
    // Exactly one capsule: only the DragValue is styled. Unit is plain text beside it.
    let _ = width;
    let response = ui
        .scope(|ui| {
            style_single_number_capsule(ui);
            ui.add_enabled(
                enabled,
                egui::DragValue::new(value)
                    .speed(speed)
                    .range(range)
                    .min_decimals(decimals)
                    .max_decimals(decimals),
            )
        })
        .inner;
    if !unit.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(unit).size(META_SIZE).color(pal().muted));
    }
    response
}

pub(crate) fn number_drag_u32(
    ui: &mut egui::Ui,
    value: &mut u32,
    enabled: bool,
    range: std::ops::RangeInclusive<u32>,
    speed: f64,
    unit: &str,
    width: f32,
) -> egui::Response {
    let _ = width;
    let response = ui
        .scope(|ui| {
            style_single_number_capsule(ui);
            ui.add_enabled(
                enabled,
                egui::DragValue::new(value).speed(speed).range(range),
            )
        })
        .inner;
    if !unit.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(unit).size(META_SIZE).color(pal().muted));
    }
    response
}

pub(crate) fn style_single_number_capsule(ui: &mut egui::Ui) {
    // One flat capsule for idle / hover / edit — no outer frame wrapper.
    let visuals = ui.visuals_mut();
    let fill = pal().quiet_fill;
    let stroke = egui::Stroke::new(1.0, pal().line);
    let radius = 8.0.into();
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.bg_fill = fill;
        // DragValue/Button 的底色取自 weak_bg_fill，只设 bg_fill 的话胶囊
        // 样式根本不生效，深色下会露出 egui 默认灰。
        w.weak_bg_fill = fill;
        w.bg_stroke = stroke;
        w.fg_stroke = egui::Stroke::new(1.0, pal().text);
        w.corner_radius = radius;
        w.expansion = 0.0;
    }
    visuals.widgets.hovered.bg_fill = pal().quiet_hover;
    visuals.widgets.hovered.weak_bg_fill = pal().quiet_hover;
    visuals.widgets.active.bg_fill = pal().quiet_hover;
    visuals.widgets.active.weak_bg_fill = pal().quiet_hover;
    visuals.widgets.open.bg_fill = fill;
    visuals.widgets.open.weak_bg_fill = fill;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, pal().focus_line);
    // DragValue text-edit mode uses extreme_bg_color; keep it same as the capsule.
    visuals.extreme_bg_color = fill;
}

pub(crate) fn outline_toggle(ui: &mut egui::Ui, on: &mut bool, text: &str) {
    let fill = if *on {
        pal().toggle_on_fill
    } else {
        pal().glass
    };
    let stroke = if *on { pal().blue } else { pal().line };
    let color = if *on { pal().blue } else { pal().muted };
    let response = ui.add(
        egui::Button::new(RichText::new(text).size(META_SIZE).color(color))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(8.0)
            .min_size(Vec2::new(56.0, CONTROL_H)),
    );
    if response.clicked() {
        *on = !*on;
    }
}

pub(crate) fn icon_button<'a>(text: &'a str, _tooltip: &'a str) -> egui::Button<'a> {
    egui::Button::new(RichText::new(text).size(15.0).color(pal().muted))
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .corner_radius(6.0)
        .min_size(Vec2::new(26.0, 26.0))
}

pub(crate) fn mini_status(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied_const(
            color.r(),
            color.g(),
            color.b(),
            20,
        ))
        .stroke(egui::Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), 48),
        ))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(CAPTION_SIZE).strong().color(color));
        });
}

pub(crate) fn status_dot(ui: &mut egui::Ui, color: Color32, pulsing: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    let radius = if pulsing {
        let wave = ((ui.input(|input| input.time) * 2.4).sin() * 0.5 + 0.5) as f32;
        2.2 + wave * 0.6
    } else {
        2.4
    };
    ui.painter().circle_filled(rect.center(), radius, color);
}

pub(crate) fn soft_divider(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, height), Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range().shrink(6.0),
        egui::Stroke::new(1.0, pal().line),
    );
}

pub(crate) fn empty_hint(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.vertical_centered(|ui| {
        if !title.is_empty() {
            ui.label(
                RichText::new(title)
                    .size(BODY_SIZE)
                    .strong()
                    .color(pal().text),
            );
            ui.add_space(4.0);
        }
        ui.label(RichText::new(detail).size(META_SIZE).color(pal().muted));
    });
}

pub(crate) fn watch_color(state: WatchState) -> Color32 {
    match state {
        WatchState::Success => pal().green,
        WatchState::Full | WatchState::Electing | WatchState::Checking => pal().amber,
        WatchState::Failed | WatchState::Ambiguous | WatchState::Missing => pal().red,
        _ => pal().muted,
    }
}

pub(crate) fn log_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Error => pal().red,
        LogLevel::Warn => pal().amber,
        LogLevel::Success => pal().green,
        LogLevel::Info => pal().blue,
    }
}

/// 在预乘空间线性插值。`Color32` 的通道本来就是预乘值，插值结果必须
/// 用 `from_rgba_premultiplied` 装回去——用 unmultiplied 构造器会再乘一次
/// alpha，半透明起点（表格偶数行的 TRANSPARENT 底）会先变暗再变亮，
/// 悬停时整行闪一下灰。
pub(crate) fn mix_color(from: Color32, to: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgba_premultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

pub(crate) fn draw_table_header(ui: &mut egui::Ui, full_w: f32, widths: &[f32; 5]) {
    let height = 34.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(full_w, height), Sense::hover());
    ui.painter().rect_filled(rect, 8.0, pal().header_fill);
    ui.painter().rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0, pal().line),
        egui::StrokeKind::Inside,
    );
    let labels = ["课程序号", "课程名称", "教师", "已选 / 上限", ""];
    let mut x = rect.left();
    for index in 0..5 {
        let cell =
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(widths[index], height));
        if !labels[index].is_empty() {
            ui.painter().text(
                cell.left_center() + Vec2::new(12.0, 0.0),
                Align2::LEFT_CENTER,
                labels[index],
                egui::FontId::proportional(META_SIZE),
                pal().muted,
            );
        }
        x += widths[index];
    }
}

pub(crate) fn truncate_ui_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub(crate) fn apply_style(ctx: &egui::Context, dark: bool) {
    // 必须先切色板：下面整段 visuals 与之后每一帧的控件都从 pal() 取色。
    DARK_MODE.store(dark, Ordering::Relaxed);
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(theme);
    let mut style = (*ctx.style_of(theme)).clone();
    style.animation_time = 0.16;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size.y = CONTROL_H;
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(22.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(BODY_SIZE));
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::proportional(META_SIZE),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::proportional(CAPTION_SIZE),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::monospace(BODY_SIZE),
    );
    let p = pal();
    style.visuals.panel_fill = p.fog;
    style.visuals.window_fill = p.glass_strong;
    style.visuals.extreme_bg_color = p.input_bg;
    style.visuals.faint_bg_color = p.header_fill;
    style.visuals.selection.bg_fill = p.selection_fill;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, p.blue);
    style.visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    // Button/DragValue/ComboBox/复选框都从 weak_bg_fill 取底色，漏设的话
    // 这些控件会保留 egui 默认灰阶，深色模式下与整套色板脱节。
    // noninteractive 的 weak_bg_fill 还是禁用态“灰掉”时的目标色。
    style.visuals.widgets.noninteractive.weak_bg_fill = p.disabled_fill;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.line);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text);
    style.visuals.widgets.inactive.bg_fill = p.quiet_fill;
    style.visuals.widgets.inactive.weak_bg_fill = p.quiet_fill;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, p.line);
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, p.text);
    style.visuals.widgets.hovered.bg_fill = p.quiet_hover;
    style.visuals.widgets.hovered.weak_bg_fill = p.quiet_hover;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.focus_line);
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, p.text);
    style.visuals.widgets.active.bg_fill = p.quiet_fill;
    style.visuals.widgets.active.weak_bg_fill = p.quiet_fill;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, p.focus_line);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.text);
    style.visuals.widgets.open.bg_fill = p.quiet_fill;
    style.visuals.widgets.open.weak_bg_fill = p.quiet_fill;
    style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, p.focus_line);
    style.visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, p.text);
    style.visuals.widgets.inactive.corner_radius = 8.0.into();
    style.visuals.widgets.hovered.corner_radius = 8.0.into();
    style.visuals.widgets.active.corner_radius = 8.0.into();
    style.visuals.widgets.open.corner_radius = 8.0.into();
    style.visuals.window_corner_radius = CARD_RADIUS.into();
    style.visuals.window_stroke = egui::Stroke::new(1.0, p.line);
    style.visuals.override_text_color = Some(p.text);
    style.visuals.hyperlink_color = p.blue;
    style.visuals.warn_fg_color = p.amber;
    style.visuals.error_fg_color = p.red;
    ctx.set_style_of(theme, style);
}

#[cfg(target_os = "windows")]
pub(crate) fn configure_window_backdrop(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    unsafe {
        let backdrop = DWMSBT_MAINWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            (&backdrop as *const i32).cast(),
            std::mem::size_of_val(&backdrop) as u32,
        );
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&corner as *const i32).cast(),
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

/// 让系统标题栏跟随程序的深浅色。少了这一步，深色模式下窗口正文是深的、
/// 标题栏还是系统浅色，看起来就像深色模式没生效。
#[cfg(target_os = "windows")]
pub(crate) fn apply_titlebar_theme(frame: &eframe::Frame, dark: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    // BOOL 语义：非零为深色。旧版 Windows 不认这个属性，返回错误也无妨。
    let enabled: i32 = i32::from(dark);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&enabled as *const i32).cast(),
            std::mem::size_of_val(&enabled) as u32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn configure_window_backdrop(_frame: &eframe::Frame) {}

#[cfg(not(target_os = "windows"))]
pub(crate) fn apply_titlebar_theme(_frame: &eframe::Frame, _dark: bool) {}

pub(crate) fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cn".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cn".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "cn".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 深色模式的核心回归：色板必须真的换掉，而不是只改 egui 的 visuals。
    // 曾经这些 token 是编译期浅色常量，深色下界面仍是白底浅灰字。
    #[test]
    fn dark_palette_is_actually_dark_and_readable() {
        let ctx = egui::Context::default();
        apply_style(&ctx, false);
        let light_text = pal().text;
        assert!(luma(pal().fog) > 0.8, "light page must stay light");

        apply_style(&ctx, true);
        assert!(luma(pal().fog) < 0.2, "dark page must be dark");
        assert!(luma(pal().glass) < 0.3, "dark cards must not be white");
        assert!(luma(pal().text) > 0.7, "dark text must be light");
        assert_ne!(pal().text, light_text, "palette did not switch");
        // 正文与卡片、次要文字与卡片都必须有足够反差才谈得上可读。
        assert!(contrast(pal().text, pal().glass) > 4.5);
        assert!(contrast(pal().muted, pal().glass) > 3.0);
        // 强调实底上的文字：深色下强调色很亮，白字会糊掉。
        assert!(contrast(pal().on_accent, pal().blue) > 3.0);
        assert!(contrast(pal().on_accent, pal().green) > 3.0);
        // 状态色画在卡片上也要认得出。
        for state in [pal().red, pal().amber, pal().green, pal().blue] {
            assert!(contrast(state, pal().glass) > 3.0);
        }

        apply_style(&ctx, false);
        assert_eq!(pal().text, light_text, "must switch back to light");
    }

    // 表格偶数行的悬停底色是从 TRANSPARENT 渐变过来的。插值必须留在预乘
    // 空间：用 unmultiplied 构造器会二次乘 alpha，中间帧比卡片底色更暗，
    // 鼠标扫过整行会闪一下灰。
    #[test]
    fn mix_color_lerps_in_premultiplied_space() {
        let target = Color32::from_rgb(238, 246, 248);
        let half = mix_color(Color32::TRANSPARENT, target, 0.5);
        assert_eq!(half, Color32::from_rgba_premultiplied(119, 123, 124, 128));
        // 二次预乘会得到 (60,62,62,128)——必须不是这个值。
        assert_ne!(half, Color32::from_rgba_premultiplied(60, 62, 62, 128));
        // 端点保持精确。
        assert_eq!(
            mix_color(Color32::TRANSPARENT, target, 0.0),
            Color32::TRANSPARENT
        );
        assert_eq!(mix_color(Color32::TRANSPARENT, target, 1.0), target);
        // 不透明两端仍是常规线性插值。
        let opaque = mix_color(
            Color32::from_rgb(0, 0, 0),
            Color32::from_rgb(100, 200, 40),
            0.5,
        );
        assert_eq!(opaque, Color32::from_rgb(50, 100, 20));
    }

    fn luma(color: Color32) -> f32 {
        let channel = |value: u8| {
            let v = value as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    /// WCAG 相对对比度。
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (high, low) = if luma(a) >= luma(b) {
            (luma(a), luma(b))
        } else {
            (luma(b), luma(a))
        };
        (high + 0.05) / (low + 0.05)
    }
}

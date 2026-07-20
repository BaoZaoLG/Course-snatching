//! 主题色板、控件样式与窗口外观。
use crate::worker::{LogLevel, WatchState};
use eframe::egui::{self, Align2, Color32, RichText, Sense, Vec2};
use url::Url;

pub(crate) const FOG: Color32 = Color32::from_rgb(246, 248, 250);
pub(crate) const GLASS: Color32 = Color32::from_rgb(255, 255, 255);
pub(crate) const GLASS_STRONG: Color32 = Color32::from_rgb(255, 255, 255);
pub(crate) const GLASS_SOFT: Color32 = Color32::from_rgb(248, 250, 251);
pub(crate) const LINE: Color32 = Color32::from_rgb(220, 228, 233);
pub(crate) const ROW_LINE: Color32 = Color32::from_rgb(232, 237, 240);
pub(crate) const HEADER_FILL: Color32 = Color32::from_rgb(236, 243, 246);
pub(crate) const TEXT: Color32 = Color32::from_rgb(33, 42, 48);
pub(crate) const MUTED: Color32 = Color32::from_rgb(108, 122, 130);
pub(crate) const BLUE: Color32 = Color32::from_rgb(41, 111, 130);
pub(crate) const GREEN: Color32 = Color32::from_rgb(43, 122, 96);
pub(crate) const RED: Color32 = Color32::from_rgb(184, 90, 80);
pub(crate) const AMBER: Color32 = Color32::from_rgb(164, 116, 54);
pub(crate) const QUIET_FILL: Color32 = Color32::from_rgb(244, 247, 249);
pub(crate) const QUIET_HOVER: Color32 = Color32::from_rgb(232, 241, 245);
pub(crate) const DISABLED_FILL: Color32 = Color32::from_rgb(238, 241, 243);
pub(crate) const ROW_HOVER: Color32 = Color32::from_rgb(238, 246, 248);
pub(crate) const ROW_ALT: Color32 = Color32::from_rgb(250, 252, 253);
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
    let Some(host_raw) = parsed.host_str() else {
        return None;
    };
    let host = host_raw.to_ascii_lowercase();
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        None
    } else {
        Some(host)
    }
}

pub(crate) fn glass_strip(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(GLASS)
        .stroke(egui::Stroke::new(1.0, LINE))
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
        .fill(GLASS_STRONG)
        .stroke(egui::Stroke::new(1.0, LINE))
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
            .color(Color32::WHITE),
    )
    .fill(color)
    .stroke(egui::Stroke::new(1.0, color))
    .corner_radius(8.0)
    .min_size(Vec2::new(width, CONTROL_H))
}

pub(crate) fn quiet_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(META_SIZE).color(TEXT))
        .fill(QUIET_FILL)
        .stroke(egui::Stroke::new(1.0, LINE))
        .corner_radius(8.0)
        .min_size(Vec2::new(width, CONTROL_H))
}

pub(crate) fn soft_danger_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(META_SIZE).strong().color(RED))
        .fill(Color32::from_rgb(252, 240, 239))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(232, 198, 194)))
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
        ui.label(RichText::new(unit).size(META_SIZE).color(MUTED));
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
        ui.label(RichText::new(unit).size(META_SIZE).color(MUTED));
    }
    response
}

pub(crate) fn style_single_number_capsule(ui: &mut egui::Ui) {
    // One flat capsule for idle / hover / edit — no outer frame wrapper.
    let visuals = ui.visuals_mut();
    let fill = QUIET_FILL;
    let stroke = egui::Stroke::new(1.0, LINE);
    let radius = 8.0.into();
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.bg_fill = fill;
        w.bg_stroke = stroke;
        w.fg_stroke = egui::Stroke::new(1.0, TEXT);
        w.corner_radius = radius;
        w.expansion = 0.0;
    }
    visuals.widgets.hovered.bg_fill = QUIET_HOVER;
    visuals.widgets.active.bg_fill = QUIET_HOVER;
    visuals.widgets.open.bg_fill = fill;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(170, 196, 206));
    // DragValue text-edit mode uses extreme_bg_color; keep it same as the capsule.
    visuals.extreme_bg_color = fill;
}

pub(crate) fn outline_toggle(ui: &mut egui::Ui, on: &mut bool, text: &str) {
    let fill = if *on {
        Color32::from_rgb(232, 243, 247)
    } else {
        Color32::WHITE
    };
    let stroke = if *on { BLUE } else { LINE };
    let color = if *on { BLUE } else { MUTED };
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
    egui::Button::new(RichText::new(text).size(15.0).color(MUTED))
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
        egui::Stroke::new(1.0, LINE),
    );
}

pub(crate) fn empty_hint(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.vertical_centered(|ui| {
        if !title.is_empty() {
            ui.label(RichText::new(title).size(BODY_SIZE).strong().color(TEXT));
            ui.add_space(4.0);
        }
        ui.label(RichText::new(detail).size(META_SIZE).color(MUTED));
    });
}

pub(crate) fn watch_color(state: WatchState) -> Color32 {
    match state {
        WatchState::Success => GREEN,
        WatchState::Full | WatchState::Electing | WatchState::Checking => AMBER,
        WatchState::Failed | WatchState::Ambiguous | WatchState::Missing => RED,
        _ => MUTED,
    }
}

pub(crate) fn log_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Error => RED,
        LogLevel::Warn => AMBER,
        LogLevel::Success => GREEN,
        LogLevel::Info => BLUE,
    }
}

pub(crate) fn mix_color(from: Color32, to: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgba_unmultiplied_const(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

pub(crate) fn draw_table_header(ui: &mut egui::Ui, full_w: f32, widths: &[f32; 5]) {
    let height = 34.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(full_w, height), Sense::hover());
    ui.painter().rect_filled(rect, 8.0, HEADER_FILL);
    ui.painter().rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0, LINE),
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
                MUTED,
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
    style.visuals.panel_fill = FOG;
    style.visuals.window_fill = GLASS_STRONG;
    style.visuals.extreme_bg_color = Color32::from_rgb(255, 255, 255);
    style.visuals.faint_bg_color = HEADER_FILL;
    style.visuals.selection.bg_fill = Color32::from_rgba_unmultiplied_const(57, 129, 146, 48);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, BLUE);
    style.visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, LINE);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.inactive.bg_fill = QUIET_FILL;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.hovered.bg_fill = QUIET_HOVER;
    style.visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(170, 196, 206));
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.active.bg_fill = QUIET_FILL;
    style.visuals.widgets.active.bg_stroke =
        egui::Stroke::new(1.0, Color32::from_rgb(170, 196, 206));
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.open.bg_fill = QUIET_FILL;
    style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(170, 196, 206));
    style.visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.inactive.corner_radius = 8.0.into();
    style.visuals.widgets.hovered.corner_radius = 8.0.into();
    style.visuals.widgets.active.corner_radius = 8.0.into();
    style.visuals.widgets.open.corner_radius = 8.0.into();
    style.visuals.window_corner_radius = CARD_RADIUS.into();
    style.visuals.window_stroke = egui::Stroke::new(1.0, LINE);
    style.visuals.widgets.open.corner_radius = 8.0.into();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.hyperlink_color = BLUE;
    style.visuals.warn_fg_color = AMBER;
    style.visuals.error_fg_color = RED;
    if dark {
        let text = Color32::from_rgb(226, 232, 240);
        let muted = Color32::from_rgb(148, 163, 184);
        let panel = Color32::from_rgb(15, 23, 42);
        let window = Color32::from_rgb(30, 41, 59);
        let elevated = Color32::from_rgb(51, 65, 85);
        let line = Color32::from_rgb(71, 85, 105);
        style.visuals.override_text_color = Some(text);
        style.visuals.panel_fill = panel;
        style.visuals.window_fill = window;
        style.visuals.extreme_bg_color = elevated;
        style.visuals.faint_bg_color = Color32::from_rgb(30, 41, 59);
        style.visuals.window_stroke = egui::Stroke::new(1.0, line);
        style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, line);
        style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, muted);
        style.visuals.widgets.inactive.bg_fill = elevated;
        style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, line);
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(71, 85, 105);
        style.visuals.widgets.hovered.bg_stroke =
            egui::Stroke::new(1.0, Color32::from_rgb(96, 165, 250));
        style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(71, 85, 105);
        style.visuals.widgets.active.bg_stroke =
            egui::Stroke::new(1.0, Color32::from_rgb(96, 165, 250));
        style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, text);
        style.visuals.widgets.open.bg_fill = elevated;
        style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, line);
        style.visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, text);
        style.visuals.selection.bg_fill = Color32::from_rgba_unmultiplied_const(59, 130, 246, 60);
        style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::from_rgb(96, 165, 250));
        style.visuals.hyperlink_color = Color32::from_rgb(96, 165, 250);
        style.visuals.warn_fg_color = Color32::from_rgb(251, 191, 36);
        style.visuals.error_fg_color = Color32::from_rgb(248, 113, 113);
    }
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

#[cfg(not(target_os = "windows"))]
pub(crate) fn configure_window_backdrop(_frame: &eframe::Frame) {}

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

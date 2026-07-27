//! 底部日志抽屉。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    pub(in crate::app) fn show_log_drawer(&mut self, root_ui: &mut egui::Ui, reveal: f32) {
        egui::Panel::bottom("logs")
            .exact_size(34.0 + 160.0 * reveal)
            .frame(
                egui::Frame::NONE
                    .fill(pal().glass_strong)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 10,
                    })
                    .stroke(egui::Stroke::new(1.0, pal().line)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    ui.label(
                        RichText::new("运行日志")
                            .strong()
                            .size(PANEL_TITLE)
                            .color(pal().text),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if ui.add(ghost_button("清空")).clicked() {
                            self.confirm_clear_logs = true;
                        }
                        if ui.add(ghost_button("导出")).clicked() {
                            self.export_logs();
                        }
                        if ui.add(ghost_button("诊断包")).clicked() {
                            self.export_diagnostics(false);
                        }
                        if ui
                            .add(ghost_button("含原始页面…"))
                            .on_hover_text("仅在明确确认后导出原始调试页面")
                            .clicked()
                        {
                            self.confirm_export_raw_diagnostics = true;
                        }
                        egui::ComboBox::from_id_salt("log_filter")
                            .selected_text(match self.log_filter {
                                LogFilter::All => "全部",
                                LogFilter::Success => "成功",
                                LogFilter::Warn => "警告",
                                LogFilter::Error => "错误",
                            })
                            .width(64.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.log_filter, LogFilter::All, "全部");
                                ui.selectable_value(
                                    &mut self.log_filter,
                                    LogFilter::Success,
                                    "成功",
                                );
                                ui.selectable_value(&mut self.log_filter, LogFilter::Warn, "警告");
                                ui.selectable_value(&mut self.log_filter, LogFilter::Error, "错误");
                            });
                    });
                });
                if reveal > 0.04 {
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .id_salt("logs")
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_opacity(reveal);
                            let logs = self.state.logs.lock().clone();
                            if logs.is_empty() {
                                empty_hint(ui, "暂无运行日志", "登录、刷新和抢课状态会显示在这里");
                            } else {
                                for item in logs.into_iter().filter(|item| match self.log_filter {
                                    LogFilter::All => true,
                                    LogFilter::Success => {
                                        matches!(item.level, LogLevel::Success)
                                    }
                                    LogFilter::Warn => matches!(item.level, LogLevel::Warn),
                                    LogFilter::Error => matches!(item.level, LogLevel::Error),
                                }) {
                                    let color = log_color(item.level);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = Vec2::new(7.0, 2.0);
                                        ui.label(
                                            RichText::new(&item.time)
                                                .color(pal().muted)
                                                .monospace()
                                                .size(CAPTION_SIZE),
                                        );
                                        ui.label(
                                            RichText::new(format!("[{}]", item.level.label()))
                                                .size(CAPTION_SIZE)
                                                .strong()
                                                .color(color),
                                        );
                                        ui.label(
                                            RichText::new(&item.message)
                                                .size(META_SIZE)
                                                .color(pal().text),
                                        );
                                    });
                                    ui.add_space(2.0);
                                }
                            }
                        });
                }
            });
    }
}

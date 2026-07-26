//! 监控目标面板。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    pub(in crate::app) fn show_watch_panel(
        &mut self,
        root_ui: &mut egui::Ui,
        running: bool,
        watch_count: usize,
    ) {
        egui::Panel::left("watch")
            .resizable(true)
            .default_size(320.0)
            .size_range(300.0..=380.0)
            .frame(
                egui::Frame::NONE
                    .fill(pal().fog)
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 6,
                        top: 10,
                        bottom: 12,
                    }),
            )
            .show(root_ui, |ui| {
                glass_surface(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_height(CONTROL_H);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(
                                RichText::new("监控目标")
                                    .strong()
                                    .size(PANEL_TITLE)
                                    .color(pal().text),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("{watch_count} 门"))
                                        .size(META_SIZE)
                                        .color(pal().muted),
                                );
                            });
                        });
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.set_height(CONTROL_H);
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                        let input_w = (ui.available_width() - 76.0).max(160.0);
                        let response = ui.add_enabled(
                            !running,
                            egui::TextEdit::singleline(&mut self.new_serial)
                                .hint_text("输入课程序号")
                                .desired_width(input_w)
                                .margin(egui::Margin::symmetric(12, 9))
                                .vertical_align(Align::Center),
                        );
                        let clicked = ui
                            .add_enabled(!running, quiet_button("加入", 68.0))
                            .clicked();
                        if clicked
                            || (response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            self.add_watch();
                        }
                    });
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(10.0);

                    let watch_items = self.state.watch.lock().clone();
                    let status_map: HashMap<String, _> = watch_items
                        .into_iter()
                        .map(|item| (item.serial.clone(), item))
                        .collect();
                    let failed_count = status_map
                        .values()
                        .filter(|item| {
                            matches!(
                                item.state,
                                WatchState::Failed | WatchState::Missing | WatchState::Ambiguous
                            )
                        })
                        .count();
                    let success_count = status_map
                        .values()
                        .filter(|item| item.state == WatchState::Success)
                        .count();
                    if !running && (!status_map.is_empty() || !self.cfg.watch_serials.is_empty()) {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                            if ui
                                .add_enabled(
                                    failed_count > 0,
                                    quiet_button(&format!("清理失败 {failed_count}"), 100.0),
                                )
                                .on_hover_text("移除失败 / 未找到 / 多条匹配 的监控目标")
                                .clicked()
                            {
                                self.clear_watches_by_state(&[
                                    WatchState::Failed,
                                    WatchState::Missing,
                                    WatchState::Ambiguous,
                                ]);
                            }
                            if ui
                                .add_enabled(
                                    success_count > 0,
                                    quiet_button(&format!("清理成功 {success_count}"), 100.0),
                                )
                                .on_hover_text("移除已抢课成功的监控目标")
                                .clicked()
                            {
                                self.clear_watches_by_state(&[WatchState::Success]);
                            }
                        });
                        ui.add_space(8.0);
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("watch_list")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.cfg.watch_serials.is_empty() {
                                ui.add_space(34.0);
                                empty_hint(
                                    ui,
                                    "三步开始",
                                    "1. 登录账号　2. 刷新课程　3. 点右侧「监控」加入",
                                );
                            } else {
                                let mut ordered: Vec<(usize, String)> =
                                    self.cfg.watch_serials.iter().cloned().enumerate().collect();
                                ordered.sort_by_key(|(index, serial)| {
                                    let rank = match status_map.get(serial).map(|item| item.state) {
                                        Some(WatchState::Success) => 2u8,
                                        Some(
                                            WatchState::Failed
                                            | WatchState::Missing
                                            | WatchState::Ambiguous,
                                        ) => 1u8,
                                        _ => 0u8,
                                    };
                                    (rank, *index)
                                });
                                for (_index, serial) in ordered {
                                    let status = status_map.get(serial.as_str());
                                    let accent =
                                        status.map_or(pal().muted, |item| watch_color(item.state));
                                    egui::Frame::NONE
                                        .fill(pal().glass_soft)
                                        .stroke(egui::Stroke::new(1.0, pal().line))
                                        .corner_radius(CARD_RADIUS)
                                        .inner_margin(egui::Margin::symmetric(12, 10))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.set_min_height(WATCH_CARD_MIN_H);
                                            ui.horizontal(|ui| {
                                                ui.set_min_height(22.0);
                                                status_dot(ui, accent, running);
                                                ui.label(
                                                    RichText::new(serial.as_str())
                                                        .monospace()
                                                        .size(BODY_SIZE)
                                                        .strong()
                                                        .color(pal().text),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        if ui
                                                            .add_enabled(
                                                                !running,
                                                                icon_button("×", "移除监控目标"),
                                                            )
                                                            .on_hover_text("移除")
                                                            .clicked()
                                                        {
                                                            self.confirm_remove =
                                                                Some(serial.clone());
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                !running,
                                                                icon_button("↓", "降低优先级"),
                                                            )
                                                            .on_hover_text("降低优先级")
                                                            .clicked()
                                                        {
                                                            self.move_watch(serial.as_str(), 1);
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                !running,
                                                                icon_button("↑", "提高优先级"),
                                                            )
                                                            .on_hover_text(
                                                                "提高优先级（越靠前越先检查）",
                                                            )
                                                            .clicked()
                                                        {
                                                            self.move_watch(serial.as_str(), -1);
                                                        }
                                                    },
                                                );
                                            });
                                            ui.add_space(6.0);
                                            let meta = self.cfg.watch_meta.get(serial.as_str());
                                            let name = status
                                                .map(|item| item.name.as_str())
                                                .filter(|s| !s.is_empty())
                                                .or_else(|| meta.map(|m| m.name.as_str()))
                                                .unwrap_or("未命名课程");
                                            let teachers = status
                                                .map(|item| item.teachers.as_str())
                                                .filter(|s| !s.is_empty())
                                                .or_else(|| meta.map(|m| m.teachers.as_str()))
                                                .unwrap_or("—");
                                            ui.label(
                                                RichText::new(name)
                                                    .size(BODY_SIZE)
                                                    .strong()
                                                    .color(pal().text),
                                            );
                                            ui.label(
                                                RichText::new(teachers)
                                                    .size(CAPTION_SIZE)
                                                    .color(pal().muted),
                                            );
                                            // F-03：填同一个组名 = 任选其一，
                                            // 抢到其中一门就撤掉同组其余目标。
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new("组")
                                                        .size(CAPTION_SIZE)
                                                        .color(pal().muted),
                                                );
                                                let mut group = self
                                                    .cfg
                                                    .watch_groups
                                                    .get(serial.as_str())
                                                    .cloned()
                                                    .unwrap_or_default();
                                                let response = ui.add_enabled(
                                                    !running,
                                                    egui::TextEdit::singleline(&mut group)
                                                        .id(GROUP_FIELD_ID.with(serial.as_str()))
                                                        .hint_text("任选其一")
                                                        .desired_width(88.0)
                                                        .margin(egui::Margin::symmetric(8, 4)),
                                                );
                                                if response.changed() {
                                                    let trimmed = group.trim().to_string();
                                                    if trimmed.is_empty() {
                                                        self.cfg
                                                            .watch_groups
                                                            .remove(serial.as_str());
                                                    } else {
                                                        self.cfg
                                                            .watch_groups
                                                            .insert(serial.clone(), trimmed);
                                                    }
                                                    self.save_config();
                                                }
                                                if let Some(name) =
                                                    self.cfg.watch_groups.get(serial.as_str())
                                                {
                                                    let siblings =
                                                        self.cfg.group_siblings(serial.as_str());
                                                    if !siblings.is_empty() {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "与 {} 任选其一",
                                                                siblings.join("、")
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(pal().blue),
                                                        );
                                                    } else {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "组「{name}」暂无其它成员"
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(pal().muted),
                                                        );
                                                    }
                                                }
                                            });
                                            ui.add_space(4.0);
                                            if let Some(item) = status {
                                                ui.horizontal(|ui| {
                                                    mini_status(ui, item.state.label(), accent);
                                                    if !item.capacity.is_empty() {
                                                        ui.label(
                                                            RichText::new(&item.capacity)
                                                                .size(META_SIZE)
                                                                .color(pal().muted),
                                                        );
                                                    }
                                                    if item.checks > 0 {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "检查 {} 次",
                                                                item.checks
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(pal().muted),
                                                        );
                                                    }
                                                    if !item.last_check.is_empty() {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "上次 {}",
                                                                item.last_check
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(pal().muted),
                                                        );
                                                    }
                                                });
                                                ui.add_space(3.0);
                                                let detail = if item.detail.is_empty() {
                                                    "—"
                                                } else {
                                                    item.detail.as_str()
                                                };
                                                ui.label(
                                                    RichText::new(detail)
                                                        .size(CAPTION_SIZE)
                                                        .color(pal().muted),
                                                );
                                            } else {
                                                ui.horizontal(|ui| {
                                                    mini_status(ui, "等待", pal().muted);
                                                });
                                                ui.add_space(3.0);
                                                ui.label(
                                                    RichText::new("开始抢课后显示实时状态")
                                                        .size(CAPTION_SIZE)
                                                        .color(pal().muted),
                                                );
                                            }
                                        });
                                    ui.add_space(8.0);
                                }
                            }
                        });
                });
            });
    }
}

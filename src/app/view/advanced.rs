//! 高级设置面板，以及「我的已选课程」。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    pub(in crate::app) fn show_advanced_settings(
        &mut self,
        ui: &mut egui::Ui,
        running: bool,
        logging_in: bool,
        logged: bool,
    ) {
        glass_strip(ui, |ui| {
            // Scroll if the host rect is still tight (small window / high UI scale).
            egui::ScrollArea::vertical()
                .id_salt("advanced_settings_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_enabled_ui(!running && !logging_in, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("连接设置")
                                    .size(BODY_SIZE)
                                    .strong()
                                    .color(pal().text),
                            );
                            ui.add_space(12.0);
                            ui.label(RichText::new("教务地址").size(META_SIZE).color(pal().muted));
                            let base = ui.add_sized(
                                [350.0, CONTROL_H],
                                egui::TextEdit::singleline(&mut self.cfg.base_url)
                                    .vertical_align(Align::Center),
                            );
                            if base.lost_focus() {
                                let changed_session = {
                                    let client = self.state.client.lock();
                                    client.as_ref().is_some_and(|client| {
                                        !client.matches_base_url(&self.cfg.base_url)
                                    })
                                };
                                self.save_config();
                                if logged && changed_session {
                                    self.state.lessons.lock().clear();
                                    self.state.set_message("教务地址已修改，请重新登录");
                                }
                            }
                            ui.add_space(8.0);
                            ui.label(RichText::new("选课轮次").size(META_SIZE).color(pal().muted));
                            let profile = ui.add_sized(
                                [124.0, CONTROL_H],
                                egui::TextEdit::singleline(&mut self.cfg.profile_id)
                                    .hint_text("自动探测")
                                    .vertical_align(Align::Center),
                            );
                            if profile.lost_focus() {
                                let value = self.cfg.profile_id.trim().to_string();
                                if value.is_empty() {
                                    if logged {
                                        self.state
                                            .set_message("轮次已设为自动，下次登录时重新探测");
                                    }
                                } else {
                                    *self.state.profile_id.lock() = value;
                                }
                                self.save_config();
                            }
                            ui.label(
                                RichText::new("留空自动探测")
                                    .size(CAPTION_SIZE)
                                    .color(pal().muted),
                            );
                        });

                        ui.add_space(10.0);
                        // Debug options only — keep this row free of unrelated controls.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(10.0, 6.0);
                            let debug_changed = ui
                                .checkbox(
                                    &mut self.cfg.debug_dump_enabled,
                                    "本次会话保存原始调试页面",
                                )
                                .changed();
                            ui.label(
                                RichText::new("仅排障时开启，文件可能包含个人信息")
                                    .size(CAPTION_SIZE)
                                    .color(if self.cfg.debug_dump_enabled {
                                        pal().red
                                    } else {
                                        pal().muted
                                    }),
                            );
                            if let Some(host) =
                                custom_host_requiring_confirmation(&self.cfg.base_url)
                            {
                                let mut confirmed =
                                    self.confirmed_custom_host.as_deref() == Some(&host);
                                if ui
                                    .checkbox(&mut confirmed, format!("本次信任 {host}"))
                                    .changed()
                                {
                                    self.confirmed_custom_host = confirmed.then_some(host);
                                }
                            }
                            // The option intentionally stays in memory only; legacy config
                            // values are force-disabled when a new session starts.
                            let _ = debug_changed;
                        });

                        ui.add_space(10.0);
                        let mut dirty = false;
                        // Behavior switches
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(14.0, 8.0);
                            dirty |= ui
                                .checkbox(&mut self.cfg.adaptive_interval, "网络异常时自动降速")
                                .changed();
                            dirty |= ui
                                .checkbox(&mut self.cfg.grab_seats_first, "优先检查有余量")
                                .changed();
                            dirty |= ui
                                .checkbox(&mut self.cfg.monitor_only, "仅监控（不自动抢课）")
                                .changed();
                            dirty |= ui
                                .checkbox(&mut self.cfg.notify_enabled, "结果通知")
                                .changed();
                            dirty |= ui.checkbox(&mut self.cfg.sound_enabled, "提示音").changed();
                            if ui.checkbox(&mut self.cfg.dark_mode, "深色模式").changed() {
                                dirty = true;
                                apply_style(ui.ctx(), self.cfg.dark_mode);
                            }
                        });

                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(14.0, 8.0);
                            let remember = ui.checkbox(
                                &mut self.cfg.remember_credentials_for_session,
                                "会话过期时自动重新登录",
                            );
                            dirty |= remember.changed();
                            // 保留凭据是隐私相关的默认开启项，必须让用户看见
                            // 它到底做了什么、以及边界在哪。
                            ui.label(
                                RichText::new(
                                    "为此会在内存中保留本次登录的账号密码（永不写入磁盘，退出登录或关闭程序即抹除）。\
                                     关掉后，挂机期间会话一旦过期，抢课会直接停止。",
                                )
                                .size(CAPTION_SIZE)
                                .color(pal().muted),
                            );
                        });

                        ui.add_space(10.0);
                        self.show_elected_panel(ui, running);
                        ui.add_space(10.0);
                        self.show_network_diagnostics(ui);

                        ui.add_space(10.0);
                        // Run parameters: left-aligned label + capsule pairs (no far-right orphan).
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                            ui.label(
                                RichText::new("运行参数")
                                    .size(BODY_SIZE)
                                    .strong()
                                    .color(pal().text),
                            );
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new("连续错误上限")
                                    .size(META_SIZE)
                                    .color(pal().muted),
                            );
                            dirty |= number_drag_u32(
                                ui,
                                &mut self.cfg.max_consecutive_errors,
                                true,
                                1..=20,
                                1.0,
                                "次",
                                56.0,
                            )
                            .changed();
                            ui.add_space(14.0);
                            ui.label(RichText::new("界面缩放").size(META_SIZE).color(pal().muted));
                            let mut scale = f64::from(self.cfg.ui_scale);
                            if number_drag_f64(ui, &mut scale, true, 0.9..=1.5, 0.05, 2, "", 64.0)
                                .changed()
                            {
                                self.cfg.ui_scale = scale as f32;
                                ui.ctx()
                                    .set_pixels_per_point(self.cfg.ui_scale.clamp(0.9, 1.5));
                                dirty = true;
                            }
                            ui.label(
                                RichText::new("连续失败达到上限后自动停止")
                                    .size(CAPTION_SIZE)
                                    .color(pal().muted),
                            );
                        });

                        ui.add_space(10.0);
                        // Scheduled start block
                        dirty |= self.show_schedule_editor(ui);

                        ui.add_space(8.0);
                        // Config actions
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(10.0, 8.0);
                            if ui.add(quiet_button("导出配置", 88.0)).clicked() {
                                self.export_config();
                            }
                            if ui.add(quiet_button("导入配置", 88.0)).clicked() {
                                self.import_config(ui.ctx());
                            }
                            if ui.add(quiet_button("日志目录", 88.0)).clicked() {
                                self.open_data_dir();
                            }
                        });
                        if dirty {
                            self.save_config();
                        }
                    });
                });
        });
    }

    /// 「我的已选课程」面板。
    ///
    /// F-04：此前完全没有这个视图——用户无从知道自己到底选上了什么，
    /// 「抢到了才发现和已有课冲突」是高频痛点。
    /// F-05：退课入口也在这里，且只在这里（不可逆操作不该有自动化路径）。
    fn show_elected_panel(&mut self, ui: &mut egui::Ui, running: bool) {
        let elected = self.state.elected.lock().clone();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("我的已选课程")
                    .size(BODY_SIZE)
                    .strong()
                    .color(pal().text),
            );
            ui.add_space(8.0);
            if ui.add(quiet_button("刷新已选", 88.0)).clicked() {
                worker::refresh_elected(self.state.clone(), self.cfg.profile_id.clone());
            }
            ui.label(
                RichText::new(
                    "不同教务版本的接口差异很大：探测每次登录最多做一次，失败也会记住，不会反复打请求。",
                )
                .size(CAPTION_SIZE)
                .color(pal().muted),
            );
        });
        ui.add_space(6.0);

        if elected.is_empty() {
            ui.label(
                RichText::new("尚未获取。点击「刷新已选」拉取。")
                    .size(CAPTION_SIZE)
                    .color(pal().muted),
            );
            return;
        }

        // 冲突检测：与监控目标比对课程名。没有时间字段的教务版本上，
        // 这至少能挡住「同一门课的不同教学班」和重复选课。
        let watched: HashSet<String> = self.cfg.watch_serials.iter().cloned().collect();
        let mut drop_request: Option<String> = None;
        for lesson in &elected {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} · {}", lesson.no, lesson.name))
                        .size(META_SIZE)
                        .color(pal().text),
                );
                if !lesson.teachers.is_empty() {
                    ui.label(
                        RichText::new(&lesson.teachers)
                            .size(CAPTION_SIZE)
                            .color(pal().muted),
                    );
                }
                // 已选中的课还挂在监控里 = 白打请求，值得提醒。
                if watched.contains(&lesson.no) {
                    ui.label(
                        RichText::new("已在监控列表中（可移除）")
                            .size(CAPTION_SIZE)
                            .color(pal().amber),
                    );
                }
                // 与另一门已选课重名：多半是同一门课的不同教学班。
                let same_name = elected
                    .iter()
                    .filter(|other| other.name == lesson.name && other.id != lesson.id)
                    .count();
                if same_name > 0 {
                    ui.label(
                        RichText::new("与另一已选课程同名，请确认是否重复")
                            .size(CAPTION_SIZE)
                            .color(pal().red),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(!running, soft_danger_button("退课", 72.0))
                        .on_hover_text("退课不可逆：名额会立刻放给别人")
                        .clicked()
                    {
                        drop_request = Some(lesson.id.clone());
                    }
                });
            });
        }
        if let Some(lesson_id) = drop_request {
            self.confirm_drop = Some(lesson_id);
        }
    }
}

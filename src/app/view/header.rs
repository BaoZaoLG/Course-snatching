//! 顶栏：品牌、运行状态、账号与主要动作按钮，以及网络健康摘要。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn show_header(
        &mut self,
        root_ui: &mut egui::Ui,
        logged: bool,
        running: bool,
        stopping: bool,
        logging_in: bool,
        refreshing: bool,
        lesson_count: usize,
        watch_count: usize,
        active_pid: &str,
        network: &NetworkSnapshot,
    ) {
        let reveal = root_ui.ctx().animate_bool_with_time(
            egui::Id::new("advanced_settings_reveal"),
            self.show_advanced,
            0.22,
        );
        egui::Panel::top("header")
            .frame(
                egui::Frame::NONE
                    .fill(pal().glass_strong)
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .stroke(egui::Stroke::new(1.0, pal().line)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(32.0);
                    ui.spacing_mut().item_spacing.x = 8.0;

                    let (label, color) = if stopping {
                        ("正在停止", pal().amber)
                    } else if running {
                        ("抢课中", pal().green)
                    } else if logging_in {
                        ("登录中", pal().amber)
                    } else if refreshing {
                        ("刷新中", pal().amber)
                    } else if logged {
                        ("已登录", pal().blue)
                    } else {
                        ("未登录", pal().muted)
                    };
                    let detail = self.effective_status().trim().to_string();
                    let detail_is_error = self.status_is_error();
                    let show_detail = !detail.is_empty() && detail != label;

                    // Left: brand + live state
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(
                            RichText::new("Course-snatching")
                                .size(20.0)
                                .strong()
                                .color(pal().text),
                        );
                        ui.label(
                            RichText::new("选课助手")
                                .size(META_SIZE)
                                .strong()
                                .color(pal().blue),
                        );
                        ui.add_space(6.0);
                        status_dot(ui, color, running);
                        ui.label(RichText::new(label).size(META_SIZE).color(color));
                        if !active_pid.is_empty() && active_pid != "0" {
                            ui.label(
                                RichText::new(format!("·  轮次 {active_pid}"))
                                    .size(META_SIZE)
                                    .color(pal().muted),
                            );
                        }
                        if lesson_count > 0 {
                            ui.label(
                                RichText::new(format!("·  {lesson_count} 门"))
                                    .size(META_SIZE)
                                    .color(pal().muted),
                            );
                        }
                        if watch_count > 0 {
                            ui.label(
                                RichText::new(format!("·  监控 {watch_count}"))
                                    .size(META_SIZE)
                                    .color(pal().muted),
                            );
                        }
                    });

                    // Right: action tip as a clear status chip (more noticeable, intentional placement)
                    if show_detail {
                        // 失败类提示用软红底，别让用户在一片同色的提示里漏掉它。
                        let (fill, line, text_color) = if detail_is_error {
                            (pal().danger_fill, pal().danger_line, pal().red)
                        } else {
                            (pal().header_fill, pal().line, pal().text)
                        };
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            egui::Frame::NONE
                                .fill(fill)
                                .stroke(egui::Stroke::new(1.0, line))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(12, 6))
                                .show(ui, |ui| {
                                    ui.set_max_width(420.0);
                                    ui.label(
                                        RichText::new(detail).size(META_SIZE).color(text_color),
                                    );
                                });
                        });
                    }
                });

                ui.add_space(10.0);

                self.show_network_summary(ui, network);

                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.set_min_height(CONTROL_H);
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);

                    ui.add_enabled(
                        !running && !logging_in,
                        egui::TextEdit::singleline(&mut self.cfg.username)
                            .hint_text("学号")
                            .desired_width(180.0)
                            .margin(egui::Margin::symmetric(12, 9))
                            .vertical_align(Align::Center),
                    );
                    ui.add_enabled(
                        !running && !logging_in,
                        egui::TextEdit::singleline(&mut *self.password)
                            .id(PASSWORD_FIELD_ID.with("password"))
                            .password(true)
                            .hint_text("密码")
                            .desired_width(180.0)
                            .margin(egui::Margin::symmetric(12, 9))
                            .vertical_align(Align::Center),
                    );

                    let login_label = if logging_in {
                        "登录中"
                    } else if logged {
                        "重新登录"
                    } else {
                        "登录"
                    };
                    if ui
                        .add_enabled(
                            !logging_in && !running,
                            primary_button(login_label, pal().blue, 84.0),
                        )
                        .clicked()
                    {
                        self.do_login();
                    }

                    let refresh_text = if refreshing {
                        "刷新中…"
                    } else {
                        "刷新课程"
                    };
                    if ui
                        .add_enabled(
                            logged && !refreshing && !logging_in && !running,
                            quiet_button(refresh_text, 96.0),
                        )
                        .clicked()
                    {
                        if !self.cfg.profile_id.trim().is_empty() {
                            *self.state.profile_id.lock() = self.cfg.profile_id.trim().to_string();
                        }
                        self.save_config();
                        self.set_status("正在刷新课程…");
                        self.state.set_message("正在刷新课程…");
                        worker::refresh_lessons(self.state.clone(), self.cfg.profile_id.clone());
                    }

                    soft_divider(ui, CONTROL_H);

                    ui.label(RichText::new("间隔").size(META_SIZE).color(pal().muted));
                    if number_drag_f64(
                        ui,
                        &mut self.cfg.interval_seconds,
                        !running,
                        0.05..=30.0,
                        0.01,
                        2,
                        "秒",
                        72.0,
                    )
                    .changed()
                    {
                        self.save_config();
                    }
                    for (label, value) in
                        [("稳妥", 1.0), ("均衡", 0.5), ("激进", 0.15), ("开课", 0.05)]
                    {
                        if ui
                            .add_enabled(!running, quiet_button(label, 48.0))
                            .clicked()
                        {
                            self.cfg.interval_seconds = value;
                            self.save_config();
                        }
                    }

                    if ui
                        .add_enabled(
                            !running && logged,
                            primary_button("开始抢课", pal().green, 100.0),
                        )
                        .clicked()
                    {
                        self.confirm_start_grab = true;
                    }

                    if ui
                        .add_enabled(running && !stopping, soft_danger_button("停止", 68.0))
                        .clicked()
                    {
                        worker::stop_grab(&self.state);
                        self.set_status("正在停止…");
                    }

                    if logged
                        && ui
                            .add_enabled(
                                // 刷新中退出会让在途任务把旧账号数据写回：
                                // 会话代际已兜底，这里从交互上直接杜绝。
                                !running && !logging_in && !refreshing,
                                quiet_button("退出", 56.0),
                            )
                            .clicked()
                    {
                        self.confirm_logout = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        outline_toggle(ui, &mut self.show_advanced, "高级");
                        outline_toggle(ui, &mut self.show_logs, "日志");
                    });
                });

                if reveal > 0.001 {
                    ui.add_space(10.0 * reveal);
                    // Tall enough for connection + switches + full schedule picker + actions.
                    let advanced_h = 460.0 * reveal;
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), advanced_h),
                        Sense::hover(),
                    );
                    let mut child = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    child.set_clip_rect(rect);
                    child.set_opacity(reveal);
                    self.show_advanced_settings(&mut child, running, logging_in, logged);
                }
            });
    }

    fn show_network_summary(&self, ui: &mut egui::Ui, network: &NetworkSnapshot) {
        let mut items = vec![format!("实际 {:.1} 请求/秒", network.requests_per_second)];
        if !network.cooldown_remaining.is_zero() {
            items.push(format!(
                "服务器冷却 {} 秒",
                network.cooldown_remaining.as_secs().max(1)
            ));
        }
        if let Some(latency_ms) = network.latency_ewma_ms {
            items.push(format!("最近延迟 {:.0} ms", latency_ms));
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(12.0, 4.0);
            for item in items {
                ui.label(RichText::new(item).size(CAPTION_SIZE).color(pal().muted));
            }
        });
    }

    pub(in crate::app) fn show_network_diagnostics(&self, ui: &mut egui::Ui) {
        let network = self.state.network_snapshot();
        let circuit = match network.circuit_status {
            CircuitStatus::Closed => "正常",
            CircuitStatus::Open => "冷却中",
            CircuitStatus::HalfOpen => "恢复探测中",
        };
        let last_error = network
            .last_error_kind
            .map_or("无".to_owned(), |kind| kind.label().to_owned());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
            ui.label(
                RichText::new("网络诊断")
                    .size(BODY_SIZE)
                    .strong()
                    .color(pal().text),
            );
            ui.label(
                RichText::new(format!(
                    "累计限流 {}　连续网络错误 {}　熔断 {}　最近错误 {}",
                    network.total_rate_limits, network.consecutive_errors, circuit, last_error,
                ))
                .size(CAPTION_SIZE)
                .color(pal().muted),
            );
        });
        ui.label(
            RichText::new("请求预算、服务器冷却和熔断保护始终生效，关闭自动降速不会绕过这些保护。")
                .size(CAPTION_SIZE)
                .color(pal().muted),
        );
    }
}

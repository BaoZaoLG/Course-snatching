mod theme;

#[allow(unused_imports)]
use crate::app::theme::{
    apply_style, configure_fonts, configure_window_backdrop, custom_host_requiring_confirmation,
    draw_table_header, empty_hint, glass_strip, glass_surface, icon_button, log_color, mini_status,
    mix_color, number_drag_f64, number_drag_u32, on_off, outline_toggle, primary_button,
    quiet_button, soft_danger_button, soft_divider, status_dot, style_single_number_capsule,
    truncate_ui_text, watch_color, AMBER, BLUE, BODY_SIZE, CAPTION_SIZE, CARD_RADIUS, CONTROL_H,
    DISABLED_FILL, FOG, GLASS, GLASS_SOFT, GLASS_STRONG, GREEN, HEADER_FILL, LINE, META_SIZE,
    MUTED, PANEL_TITLE, QUIET_FILL, QUIET_HOVER, RED, ROW_ALT, ROW_HOVER, ROW_LINE, TEXT,
    WATCH_CARD_MIN_H,
};
use crate::config::{days_in_month, AppConfig, ScheduleStamp};
use crate::eams::Lesson;
use crate::worker::{self, LogLevel, SharedState, WatchState};
use eframe::egui::{self, Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use zeroize::Zeroize;

// Page / surface tokens — solid fills only, avoid translucent color stacking.

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    Success,
    Warn,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogSort {
    Default,
    SeatsFirst,
    Name,
}

pub struct CourseApp {
    cfg: AppConfig,
    password: String,
    state: Arc<SharedState>,
    new_serial: String,
    filter: String,
    only_available: bool,
    status_line: String,
    show_logs: bool,
    show_advanced: bool,
    was_logged_in: bool,
    confirmed_custom_host: Option<String>,
    window_backdrop_attempted: bool,
    log_filter: LogFilter,
    catalog_sort: CatalogSort,
    toast: Option<(f64, String, bool)>,
    confirm_logout: bool,
    confirm_clear_logs: bool,
    confirm_remove: Option<usize>,
    confirm_start_grab: bool,
    show_first_run: bool,
    last_keepalive: f64,
    /// 已触发过的定时开抢键：YYYY-MM-DD HH:MM:SS
    schedule_fired_for: Option<String>,
    /// Key currently armed in worker precise waiter.
    schedule_armed_for: Option<String>,
    was_running: bool,
    result_summary: Option<String>,
}

impl CourseApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);
        let (cfg, config_warning) = AppConfig::load_with_warning();
        apply_style(&cc.egui_ctx, cfg.dark_mode);
        cc.egui_ctx
            .set_pixels_per_point(cfg.ui_scale.clamp(0.9, 1.5));
        let state = SharedState::new();
        if let Some(warning) = &config_warning {
            state.log(LogLevel::Warn, warning.clone());
        }
        if !cfg.profile_id.trim().is_empty() {
            *state.profile_id.lock() = cfg.profile_id.trim().to_string();
        }
        let show_first_run = !cfg.first_run_ack;
        let filter = cfg.filter.clone();
        let only_available = cfg.only_available;
        Self {
            password: String::new(),
            cfg,
            state,
            new_serial: String::new(),
            filter,
            only_available,
            status_line: config_warning.unwrap_or_else(|| "登录后刷新课程，从课表加入监控".into()),
            show_logs: true,
            show_advanced: false,
            was_logged_in: false,
            confirmed_custom_host: None,
            window_backdrop_attempted: false,
            log_filter: LogFilter::All,
            catalog_sort: CatalogSort::Default,
            toast: None,
            confirm_logout: false,
            confirm_clear_logs: false,
            confirm_remove: None,
            confirm_start_grab: false,
            show_first_run,
            last_keepalive: 0.0,
            schedule_fired_for: None,
            schedule_armed_for: None,
            was_running: false,
            result_summary: None,
        }
    }
}

impl eframe::App for CourseApp {
    fn ui(&mut self, root_ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if !self.window_backdrop_attempted {
            self.window_backdrop_attempted = true;
            configure_window_backdrop(frame);
        }

        let ctx = root_ui.ctx().clone();
        let logged = self
            .state
            .logged_in
            .load(std::sync::atomic::Ordering::Acquire);
        let running = self
            .state
            .running
            .load(std::sync::atomic::Ordering::Acquire);
        let stopping = self
            .state
            .stopping
            .load(std::sync::atomic::Ordering::Acquire);
        let logging_in = self
            .state
            .logging_in
            .load(std::sync::atomic::Ordering::Acquire);
        let refreshing = self
            .state
            .refreshing
            .load(std::sync::atomic::Ordering::Acquire);
        if logged && !self.was_logged_in {
            self.password.zeroize();
            self.password.clear();
        }
        self.was_logged_in = logged;
        let schedule_soon = self.cfg.schedule_enabled
            && logged
            && !running
            && ScheduleStamp::parse(&self.cfg.schedule_time)
                .and_then(|s| s.to_local_seconds())
                .is_some_and(|target| {
                    let now = worker::local_now_seconds();
                    now <= target + 2 && target - now <= 120
                });
        ctx.request_repaint_after(std::time::Duration::from_millis(
            if running || logging_in || refreshing {
                50
            } else if schedule_soon {
                // Sub-frame wake while armed so UI countdown stays honest; precise fire is worker-side.
                30
            } else {
                400
            },
        ));

        // Alerts are only queued when notify_enabled was on at dispatch time.
        for alert in crate::notify::take_alerts() {
            let t = root_ui.ctx().input(|i| i.time);
            self.toast = Some((
                t + 4.5,
                format!("{}：{}", alert.title, alert.body),
                alert.success,
            ));
        }
        if let Some((until, _, _)) = self.toast {
            if root_ui.ctx().input(|i| i.time) > until {
                self.toast = None;
            }
        }
        let now = root_ui.ctx().input(|i| i.time);
        if logged && !running && !logging_in && !refreshing && now - self.last_keepalive > 180.0 {
            self.last_keepalive = now;
            worker::keepalive(
                self.state.clone(),
                self.cfg.notify_enabled,
                self.cfg.sound_enabled,
            );
        }

        if self.was_running && !running {
            self.result_summary = Some(self.build_result_summary());
        }
        self.was_running = running;

        self.maybe_trigger_schedule(logged, running, logging_in);

        let lesson_count = self.state.lessons.lock().len();
        let worker_msg = self.state.worker_message.lock().clone();
        if !worker_msg.is_empty() {
            self.status_line = worker_msg;
        }
        let active_pid = if self.cfg.profile_id.trim().is_empty() {
            self.state.profile_id.lock().clone()
        } else {
            self.cfg.profile_id.trim().to_string()
        };
        let watch_count = self.cfg.watch_serials.len();

        self.show_header(
            root_ui,
            logged,
            running,
            stopping,
            logging_in,
            refreshing,
            lesson_count,
            watch_count,
            &active_pid,
        );

        let log_reveal =
            ctx.animate_bool_with_time(egui::Id::new("log_drawer_reveal"), self.show_logs, 0.22);
        if log_reveal > 0.001 {
            self.show_log_drawer(root_ui, log_reveal);
        }
        self.show_watch_panel(root_ui, running, watch_count);
        self.show_course_catalog(root_ui, running, lesson_count);
        self.show_overlays(root_ui);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        FOG.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.password.zeroize();
        self.password.clear();
        worker::logout(&self.state);
    }
}

impl CourseApp {
    #[allow(clippy::too_many_arguments)]
    fn show_header(
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
    ) {
        let reveal = root_ui.ctx().animate_bool_with_time(
            egui::Id::new("advanced_settings_reveal"),
            self.show_advanced,
            0.22,
        );
        egui::Panel::top("header")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .stroke(egui::Stroke::new(1.0, LINE)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(32.0);
                    ui.spacing_mut().item_spacing.x = 8.0;

                    let (label, color) = if stopping {
                        ("正在停止", AMBER)
                    } else if running {
                        ("抢课中", GREEN)
                    } else if logging_in {
                        ("登录中", AMBER)
                    } else if refreshing {
                        ("刷新中", AMBER)
                    } else if logged {
                        ("已登录", BLUE)
                    } else {
                        ("未登录", MUTED)
                    };
                    let detail = self.status_line.trim().to_string();
                    let show_detail = !detail.is_empty() && detail != label;

                    // Left: brand + live state
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new("选课助手").size(22.0).strong().color(TEXT));
                        ui.label(RichText::new("SIAS").size(META_SIZE).strong().color(BLUE));
                        ui.add_space(6.0);
                        status_dot(ui, color, running);
                        ui.label(RichText::new(label).size(META_SIZE).color(color));
                        if !active_pid.is_empty() && active_pid != "0" {
                            ui.label(
                                RichText::new(format!("·  轮次 {active_pid}"))
                                    .size(META_SIZE)
                                    .color(MUTED),
                            );
                        }
                        if lesson_count > 0 {
                            ui.label(
                                RichText::new(format!("·  {lesson_count} 门"))
                                    .size(META_SIZE)
                                    .color(MUTED),
                            );
                        }
                        if watch_count > 0 {
                            ui.label(
                                RichText::new(format!("·  监控 {watch_count}"))
                                    .size(META_SIZE)
                                    .color(MUTED),
                            );
                        }
                    });

                    // Right: action tip as a clear status chip (more noticeable, intentional placement)
                    if show_detail {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            egui::Frame::NONE
                                .fill(HEADER_FILL)
                                .stroke(egui::Stroke::new(1.0, LINE))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(12, 6))
                                .show(ui, |ui| {
                                    ui.set_max_width(420.0);
                                    ui.label(RichText::new(detail).size(META_SIZE).color(TEXT));
                                });
                        });
                    }
                });

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
                        egui::TextEdit::singleline(&mut self.password)
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
                            primary_button(login_label, BLUE, 84.0),
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
                        self.status_line = "正在刷新课程…".into();
                        self.state.set_message("正在刷新课程…");
                        worker::refresh_lessons(self.state.clone(), self.cfg.profile_id.clone());
                    }

                    soft_divider(ui, CONTROL_H);

                    ui.label(RichText::new("间隔").size(META_SIZE).color(MUTED));
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
                        .add_enabled(!running && logged, primary_button("开始抢课", GREEN, 100.0))
                        .clicked()
                    {
                        self.confirm_start_grab = true;
                    }

                    if ui
                        .add_enabled(running && !stopping, soft_danger_button("停止", 68.0))
                        .clicked()
                    {
                        worker::stop_grab(&self.state);
                        self.status_line = "正在停止…".into();
                    }

                    if logged
                        && ui
                            .add_enabled(!running && !logging_in, quiet_button("退出", 56.0))
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

    fn show_advanced_settings(
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
                                    .color(TEXT),
                            );
                            ui.add_space(12.0);
                            ui.label(RichText::new("教务地址").size(META_SIZE).color(MUTED));
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
                            ui.label(RichText::new("选课轮次").size(META_SIZE).color(MUTED));
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
                                    .color(MUTED),
                            );
                        });

                        ui.add_space(10.0);
                        // Debug options only — keep this row free of unrelated controls.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(10.0, 6.0);
                            let debug_changed = ui
                                .checkbox(&mut self.cfg.debug_dump_enabled, "保存原始调试页面")
                                .changed();
                            ui.label(
                                RichText::new("仅排障时开启，文件可能包含个人信息")
                                    .size(CAPTION_SIZE)
                                    .color(if self.cfg.debug_dump_enabled {
                                        RED
                                    } else {
                                        MUTED
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
                            if debug_changed {
                                self.save_config();
                            }
                        });

                        ui.add_space(10.0);
                        let mut dirty = false;
                        // Behavior switches
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(14.0, 8.0);
                            dirty |= ui
                                .checkbox(&mut self.cfg.adaptive_interval, "限流自适应间隔")
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

                        ui.add_space(10.0);
                        // Run parameters: left-aligned label + capsule pairs (no far-right orphan).
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                            ui.label(
                                RichText::new("运行参数")
                                    .size(BODY_SIZE)
                                    .strong()
                                    .color(TEXT),
                            );
                            ui.add_space(8.0);
                            ui.label(RichText::new("连续错误上限").size(META_SIZE).color(MUTED));
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
                            ui.label(RichText::new("界面缩放").size(META_SIZE).color(MUTED));
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
                                    .color(MUTED),
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

    fn show_log_drawer(&mut self, root_ui: &mut egui::Ui, reveal: f32) {
        egui::Panel::bottom("logs")
            .exact_size(34.0 + 160.0 * reveal)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 10,
                    })
                    .stroke(egui::Stroke::new(1.0, LINE)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    ui.label(
                        RichText::new("运行日志")
                            .strong()
                            .size(PANEL_TITLE)
                            .color(TEXT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("清空").size(META_SIZE).color(MUTED),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.confirm_clear_logs = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("导出").size(META_SIZE).color(MUTED),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.export_logs();
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
                                                .color(MUTED)
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
                                                .color(TEXT),
                                        );
                                    });
                                    ui.add_space(2.0);
                                }
                            }
                        });
                }
            });
    }

    fn show_watch_panel(&mut self, root_ui: &mut egui::Ui, running: bool, watch_count: usize) {
        egui::Panel::left("watch")
            .resizable(true)
            .default_size(320.0)
            .size_range(300.0..=380.0)
            .frame(egui::Frame::NONE.fill(FOG).inner_margin(egui::Margin {
                left: 12,
                right: 6,
                top: 10,
                bottom: 12,
            }))
            .show(root_ui, |ui| {
                glass_surface(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_height(CONTROL_H);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(
                                RichText::new("监控目标")
                                    .strong()
                                    .size(PANEL_TITLE)
                                    .color(TEXT),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("{watch_count} 门"))
                                        .size(META_SIZE)
                                        .color(MUTED),
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
                                for (index, serial) in ordered {
                                    let status = status_map.get(serial.as_str());
                                    let accent =
                                        status.map_or(MUTED, |item| watch_color(item.state));
                                    egui::Frame::NONE
                                        .fill(GLASS_SOFT)
                                        .stroke(egui::Stroke::new(1.0, LINE))
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
                                                        .color(TEXT),
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
                                                            self.confirm_remove = Some(index);
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
                                                    .color(TEXT),
                                            );
                                            ui.label(
                                                RichText::new(teachers)
                                                    .size(CAPTION_SIZE)
                                                    .color(MUTED),
                                            );
                                            ui.add_space(4.0);
                                            if let Some(item) = status {
                                                ui.horizontal(|ui| {
                                                    mini_status(ui, item.state.label(), accent);
                                                    if !item.capacity.is_empty() {
                                                        ui.label(
                                                            RichText::new(&item.capacity)
                                                                .size(META_SIZE)
                                                                .color(MUTED),
                                                        );
                                                    }
                                                    if item.checks > 0 {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "检查 {} 次",
                                                                item.checks
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(MUTED),
                                                        );
                                                    }
                                                    if !item.last_check.is_empty() {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "上次 {}",
                                                                item.last_check
                                                            ))
                                                            .size(CAPTION_SIZE)
                                                            .color(MUTED),
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
                                                        .color(MUTED),
                                                );
                                            } else {
                                                ui.horizontal(|ui| {
                                                    mini_status(ui, "等待", MUTED);
                                                });
                                                ui.add_space(3.0);
                                                ui.label(
                                                    RichText::new("开始抢课后显示实时状态")
                                                        .size(CAPTION_SIZE)
                                                        .color(MUTED),
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

    fn show_course_catalog(&mut self, root_ui: &mut egui::Ui, running: bool, lesson_count: usize) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(FOG).inner_margin(egui::Margin {
                left: 6,
                right: 12,
                top: 10,
                bottom: 12,
            }))
            .show(root_ui, |ui| {
                glass_surface(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(CONTROL_H);
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(
                            RichText::new("可选课程")
                                .strong()
                                .size(PANEL_TITLE)
                                .color(TEXT),
                        );
                        ui.add_sized(
                            [240.0, CONTROL_H],
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("搜索 序号 / 名称 / 教师")
                                .vertical_align(Align::Center),
                        );
                        if ui.checkbox(&mut self.only_available, "仅有余量").changed() {
                            self.cfg.only_available = self.only_available;
                            self.save_config();
                        }
                        egui::ComboBox::from_id_salt("catalog_sort")
                            .selected_text(match self.catalog_sort {
                                CatalogSort::Default => "默认排序",
                                CatalogSort::SeatsFirst => "余量优先",
                                CatalogSort::Name => "按名称",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::Default,
                                    "默认排序",
                                );
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::SeatsFirst,
                                    "余量优先",
                                );
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::Name,
                                    "按名称",
                                );
                            });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("共 {lesson_count}"))
                                    .size(META_SIZE)
                                    .color(MUTED),
                            );
                        });
                    });
                    ui.add_space(12.0);

                    let lessons = self.state.lessons.lock().clone();
                    let filter = self.filter.trim().to_lowercase();
                    let filtered: Vec<Lesson> = lessons
                        .into_iter()
                        .filter(|lesson| {
                            if self.only_available && !lesson.has_seat() {
                                return false;
                            }
                            filter.is_empty()
                                || lesson.no.to_lowercase().contains(&filter)
                                || lesson.name.to_lowercase().contains(&filter)
                                || lesson.teachers.to_lowercase().contains(&filter)
                        })
                        .collect();
                    let mut filtered = filtered;
                    match self.catalog_sort {
                        CatalogSort::Default => {}
                        CatalogSort::SeatsFirst => {
                            filtered.sort_by_key(|lesson| !lesson.has_seat());
                        }
                        CatalogSort::Name => {
                            filtered.sort_by(|a, b| a.name.cmp(&b.name));
                        }
                    }
                    // persist search text lightly
                    if self.cfg.filter != self.filter {
                        self.cfg.filter = self.filter.clone();
                        self.save_config();
                    }
                    let shown = filtered.len();
                    let watched: HashSet<String> = self.cfg.watch_serials.iter().cloned().collect();
                    let watched_lesson_ids = self.cfg.watch_lesson_ids.clone();

                    let full_w = ui.available_width().max(1.0);
                    let action_w = 76.0;
                    let capacity_w = (full_w * 0.12).clamp(88.0, 116.0);
                    let teacher_w = (full_w * 0.16).clamp(110.0, 150.0);
                    let serial_w = (full_w * 0.22).clamp(168.0, 236.0);
                    let name_w = (full_w - serial_w - teacher_w - capacity_w - action_w).max(180.0);
                    let widths = [serial_w, name_w, teacher_w, capacity_w, action_w];
                    draw_table_header(ui, full_w, &widths);
                    ui.add_space(2.0);

                    egui::ScrollArea::vertical()
                        .id_salt("catalog")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_min_width(full_w);
                            if filtered.is_empty() {
                                ui.add_space(54.0);
                                empty_hint(
                                    ui,
                                    if lesson_count == 0 {
                                        "尚未载入课程"
                                    } else {
                                        "没有符合条件的课程"
                                    },
                                    if lesson_count == 0 {
                                        "登录后点击“刷新课程”即可载入"
                                    } else {
                                        "请调整搜索内容或筛选条件"
                                    },
                                );
                            } else {
                                for (index, lesson) in filtered.iter().take(800).enumerate() {
                                    let serial_watched = watched.contains(&lesson.no);
                                    let selected_id = watched_lesson_ids.get(&lesson.no);
                                    let already = selected_id.is_some_and(|id| id == &lesson.id);
                                    let switching = serial_watched
                                        && selected_id.is_some_and(|id| id != &lesson.id);
                                    let needs_specifying = serial_watched && selected_id.is_none();
                                    let row_h = 40.0;
                                    let (row_rect, row_response) = ui.allocate_exact_size(
                                        Vec2::new(full_w, row_h),
                                        Sense::click(),
                                    );
                                    let base = if index % 2 == 0 {
                                        Color32::TRANSPARENT
                                    } else {
                                        ROW_ALT
                                    };
                                    let hover = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("course_row", &lesson.id)),
                                        row_response.hovered(),
                                        0.12,
                                    );
                                    ui.painter().rect_filled(
                                        row_rect,
                                        0.0,
                                        mix_color(base, ROW_HOVER, hover),
                                    );
                                    ui.painter().hline(
                                        row_rect.x_range(),
                                        row_rect.bottom(),
                                        egui::Stroke::new(1.0, ROW_LINE),
                                    );

                                    let mut x = row_rect.left();
                                    let mut columns = [egui::Rect::ZERO; 5];
                                    for column in 0..5 {
                                        columns[column] = egui::Rect::from_min_size(
                                            egui::pos2(x, row_rect.top()),
                                            Vec2::new(widths[column], row_h),
                                        );
                                        x += widths[column];
                                    }
                                    ui.painter().text(
                                        columns[0].left_center() + Vec2::new(12.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        &lesson.no,
                                        egui::FontId::monospace(BODY_SIZE),
                                        TEXT,
                                    );
                                    let name_show = truncate_ui_text(&lesson.name, 22);
                                    ui.painter().text(
                                        columns[1].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        name_show,
                                        egui::FontId::proportional(BODY_SIZE),
                                        TEXT,
                                    );
                                    ui.painter().text(
                                        columns[2].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        &lesson.teachers,
                                        egui::FontId::proportional(META_SIZE),
                                        MUTED,
                                    );
                                    let capacity_color = if lesson.has_seat() {
                                        GREEN
                                    } else if !lesson.capacity_known() {
                                        MUTED
                                    } else {
                                        RED
                                    };
                                    ui.painter().text(
                                        columns[3].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        lesson.capacity_text(),
                                        egui::FontId::proportional(META_SIZE),
                                        capacity_color,
                                    );

                                    let button_text = if already {
                                        "已加入"
                                    } else if switching {
                                        "切换"
                                    } else if needs_specifying {
                                        "指定"
                                    } else {
                                        "监控"
                                    };
                                    let button_rect = egui::Rect::from_center_size(
                                        columns[4].center(),
                                        Vec2::new(58.0, 28.0),
                                    );
                                    let button_response = ui.interact(
                                        button_rect,
                                        ui.id().with(("monitor", index, &lesson.id)),
                                        Sense::click(),
                                    );
                                    let button_hover = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("monitor_hover", &lesson.id)),
                                        button_response.hovered(),
                                        0.12,
                                    );
                                    let fill = if already {
                                        DISABLED_FILL
                                    } else {
                                        mix_color(QUIET_FILL, QUIET_HOVER, button_hover)
                                    };
                                    ui.painter().rect_filled(button_rect, 8.0, fill);
                                    ui.painter().rect_stroke(
                                        button_rect,
                                        8.0,
                                        egui::Stroke::new(1.0, LINE),
                                        egui::StrokeKind::Inside,
                                    );
                                    ui.painter().text(
                                        button_rect.center(),
                                        Align2::CENTER_CENTER,
                                        button_text,
                                        egui::FontId::proportional(META_SIZE),
                                        if already { MUTED } else { BLUE },
                                    );
                                    if !running
                                        && !already
                                        && (button_response.clicked()
                                            || row_response.double_clicked())
                                    {
                                        self.add_watch_lesson(lesson);
                                    }
                                }
                                if shown > 800 {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new(format!(
                                            "当前筛选 {shown} 条，仅显示前 800 条"
                                        ))
                                        .size(12.0)
                                        .color(MUTED),
                                    );
                                }
                            }
                        });
                });
            });
    }
}

impl CourseApp {
    fn show_overlays(&mut self, root_ui: &mut egui::Ui) {
        if let Some((_, text, ok)) = self.toast.clone() {
            egui::Area::new(egui::Id::new("toast"))
                .anchor(Align2::CENTER_TOP, [0.0, 18.0])
                .order(egui::Order::Foreground)
                .show(root_ui.ctx(), |ui| {
                    egui::Frame::NONE
                        .fill(if ok {
                            Color32::from_rgb(232, 245, 238)
                        } else {
                            Color32::from_rgb(252, 240, 239)
                        })
                        .stroke(egui::Stroke::new(1.0, if ok { GREEN } else { RED }))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            ui.label(RichText::new(text).size(META_SIZE).color(if ok {
                                GREEN
                            } else {
                                RED
                            }));
                        });
                });
        }
        if self.show_first_run {
            egui::Window::new("欢迎使用选课助手")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(420.0);
                    ui.label("三步开始：");
                    ui.label("1. 登录账号");
                    ui.label("2. 刷新课程");
                    ui.label("3. 点右侧「监控」加入目标后开始抢课");
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("请仅用于本人账号，合理控制请求频率，遵守学校规定。")
                            .size(CAPTION_SIZE)
                            .color(MUTED),
                    );
                    ui.add_space(10.0);
                    if ui.add(primary_button("知道了", BLUE, 100.0)).clicked() {
                        self.show_first_run = false;
                        self.cfg.first_run_ack = true;
                        self.save_config();
                    }
                });
        }
        if self.confirm_logout {
            egui::Window::new("确认退出登录")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.label("退出后需要重新登录才能继续。");
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_logout = false;
                        }
                        if ui.add(soft_danger_button("退出", 72.0)).clicked() {
                            self.confirm_logout = false;
                            worker::logout(&self.state);
                            self.status_line = "已退出登录".into();
                        }
                    });
                });
        }
        if self.confirm_clear_logs {
            egui::Window::new("确认清空日志")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.label("清空后无法恢复。");
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_clear_logs = false;
                        }
                        if ui.add(soft_danger_button("清空", 72.0)).clicked() {
                            self.confirm_clear_logs = false;
                            self.state.logs.lock().clear();
                        }
                    });
                });
        }
        if let Some(index) = self.confirm_remove {
            egui::Window::new("确认移除监控")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    let serial = self
                        .cfg
                        .watch_serials
                        .get(index)
                        .cloned()
                        .unwrap_or_default();
                    ui.label(format!("移除监控目标：{serial}？"));
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_remove = None;
                        }
                        if ui.add(soft_danger_button("移除", 72.0)).clicked() {
                            if index < self.cfg.watch_serials.len() {
                                let removed = self.cfg.watch_serials.remove(index);
                                self.cfg.watch_lesson_ids.remove(&removed);
                                self.cfg.watch_meta.remove(&removed);
                                self.save_config();
                            }
                            self.confirm_remove = None;
                        }
                    });
                });
        }

        if self.confirm_start_grab {
            egui::Window::new("确认开始抢课")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(460.0);
                    let targets = self.cfg.cleaned_watch();
                    ui.label(
                        RichText::new(format!("监控目标 {} 门", targets.len()))
                            .strong()
                            .size(BODY_SIZE)
                            .color(TEXT),
                    );
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            for serial in &targets {
                                let name = self
                                    .cfg
                                    .watch_meta
                                    .get(serial)
                                    .map(|m| m.name.as_str())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("未命名课程");
                                ui.label(
                                    RichText::new(format!("· {serial}  {name}"))
                                        .size(META_SIZE)
                                        .color(TEXT),
                                );
                            }
                        });
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "间隔 {:.2} 秒　限流自适应 {}　优先余量 {}　仅监控 {}",
                            self.cfg.interval_seconds,
                            on_off(self.cfg.adaptive_interval),
                            on_off(self.cfg.grab_seats_first),
                            on_off(self.cfg.monitor_only),
                        ))
                        .size(META_SIZE)
                        .color(MUTED),
                    );
                    ui.label(
                        RichText::new(format!(
                            "结果通知 {}　提示音 {}",
                            on_off(self.cfg.notify_enabled),
                            on_off(self.cfg.sound_enabled),
                        ))
                        .size(META_SIZE)
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("开抢自检")
                            .strong()
                            .size(META_SIZE)
                            .color(TEXT),
                    );
                    for (ok, line) in self.preflight_checks() {
                        ui.label(
                            RichText::new(format!("{} {}", if ok { "✓" } else { "!" }, line))
                                .size(META_SIZE)
                                .color(if ok { GREEN } else { AMBER }),
                        );
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 84.0)).clicked() {
                            self.confirm_start_grab = false;
                        }
                        let can_start = self.preflight_checks().iter().all(|(ok, _)| *ok);
                        if ui
                            .add_enabled(can_start, primary_button("确认开始", GREEN, 110.0))
                            .clicked()
                        {
                            self.confirm_start_grab = false;
                            self.start_grab();
                        }
                    });
                });
        }

        if let Some(summary) = self.result_summary.clone() {
            egui::Window::new("本轮结果摘要")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(420.0);
                    ui.label(RichText::new(&summary).size(META_SIZE).color(TEXT));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(primary_button("知道了", BLUE, 100.0)).clicked() {
                            self.result_summary = None;
                        }
                        if ui.add(quiet_button("导出摘要", 100.0)).clicked() {
                            self.export_result_summary(&summary);
                        }
                    });
                });
        }
    }

    fn export_result_summary(&mut self, summary: &str) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("course-monitor-result.txt")
            .save_file()
        {
            match std::fs::write(&path, summary) {
                Ok(()) => self.status_line = format!("结果摘要已导出：{}", path.display()),
                Err(error) => self.status_line = format!("导出失败：{error}"),
            }
        }
    }

    fn export_logs(&mut self) {
        let logs = self.state.logs.lock().clone();
        let text = logs
            .iter()
            .map(|item| format!("{} [{}] {}", item.time, item.level.label(), item.message))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("course-monitor-logs.txt")
            .save_file()
        {
            match std::fs::write(&path, text) {
                Ok(()) => self.status_line = format!("日志已导出：{}", path.display()),
                Err(error) => self.status_line = format!("导出失败：{error}"),
            }
        }
    }

    fn export_config(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("course-monitor-config.toml")
            .save_file()
        {
            match self.cfg.export_to(&path) {
                Ok(()) => self.status_line = format!("配置已导出：{}", path.display()),
                Err(error) => self.status_line = format!("导出失败：{error:#}"),
            }
        }
    }

    fn import_config(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML", &["toml"])
            .pick_file()
        {
            match AppConfig::import_from(&path) {
                Ok(cfg) => {
                    self.cfg = cfg;
                    self.filter = self.cfg.filter.clone();
                    self.only_available = self.cfg.only_available;
                    apply_style(ctx, self.cfg.dark_mode);
                    ctx.set_pixels_per_point(self.cfg.ui_scale.clamp(0.9, 1.5));
                    if let Err(error) = self.cfg.save() {
                        self.status_line = format!("导入后保存失败：{error:#}");
                    } else {
                        self.status_line = format!("配置已导入：{}", path.display());
                    }
                }
                Err(error) => self.status_line = format!("导入失败：{error:#}"),
            }
        }
    }

    fn open_data_dir(&mut self) {
        let dir = AppConfig::data_dir();
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        }
        let crash = AppConfig::crash_log_path();
        self.status_line = format!(
            "数据目录：{}（崩溃日志：{}）",
            dir.display(),
            crash.display()
        );
    }

    fn save_config(&mut self) {
        if let Err(error) = self.cfg.save() {
            let message = format!("配置保存失败：{error:#}");
            self.status_line = message.clone();
            self.state.set_message(message.clone());
            self.state.log(LogLevel::Error, message);
        }
    }

    fn do_login(&mut self) {
        if self
            .state
            .logging_in
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.status_line = "正在登录，请稍候…".into();
            return;
        }
        if self.cfg.username.trim().is_empty() || self.password.is_empty() {
            self.status_line = "请填写账号和密码".into();
            self.state.set_message("请填写账号和密码");
            return;
        }
        if let Err(error) = self.cfg.validate_connection() {
            self.status_line = error.to_string();
            self.state.set_message(error.to_string());
            self.state.log(LogLevel::Warn, error.to_string());
            return;
        }
        if let Some(host) = custom_host_requiring_confirmation(&self.cfg.base_url) {
            if self.confirmed_custom_host.as_deref() != Some(&host) {
                let message =
                    format!("即将把账号密码提交到非默认域名 {host}，请在高级设置中确认本次信任");
                self.status_line = message.clone();
                self.state.set_message(message.clone());
                self.state.log(LogLevel::Warn, message);
                return;
            }
        }
        let pref = self.cfg.profile_id.trim().to_string();
        if !pref.is_empty() {
            *self.state.profile_id.lock() = pref.clone();
        }
        self.save_config();
        self.status_line = "正在登录…".into();
        worker::login_and_fetch(
            self.state.clone(),
            worker::LoginRequest {
                base_url: self.cfg.base_url.clone(),
                username: self.cfg.username.clone(),
                password: self.password.clone(),
                profile_preference: pref,
                timeout: self.cfg.timeout_seconds,
                auto_fetch: self.cfg.auto_fetch_on_login,
                debug_dump_enabled: self.cfg.debug_dump_enabled,
            },
        );
    }

    fn add_watch(&mut self) {
        let s = self.new_serial.trim().to_string();
        if s.is_empty() {
            return;
        }
        self.add_watch_serial(&s);
        self.new_serial.clear();
    }

    fn add_watch_lesson(&mut self, lesson: &Lesson) {
        if self
            .state
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.status_line = "运行期间不能修改监控目标".into();
            return;
        }
        if lesson.id.is_empty() || !lesson.id.chars().all(|ch| ch.is_ascii_digit()) {
            self.status_line = "课程教学班标识无效，无法加入监控".into();
            self.state.log(LogLevel::Error, self.status_line.clone());
            return;
        }
        if !self
            .cfg
            .watch_serials
            .iter()
            .any(|serial| serial == &lesson.no)
        {
            self.cfg.watch_serials.push(lesson.no.clone());
        }
        self.cfg
            .watch_lesson_ids
            .insert(lesson.no.clone(), lesson.id.clone());
        self.cfg.watch_meta.insert(
            lesson.no.clone(),
            crate::config::WatchMeta {
                name: lesson.name.clone(),
                teachers: lesson.teachers.clone(),
            },
        );
        self.save_config();
        let message = format!(
            "已指定监控：{} · {} · {}",
            lesson.no, lesson.name, lesson.teachers
        );
        self.status_line = message.clone();
        self.state.set_message(message.clone());
        self.state.log(LogLevel::Info, message);
    }

    fn add_watch_serial(&mut self, serial: &str) {
        if self
            .state
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.status_line = "运行期间不能修改监控目标".into();
            return;
        }
        if self.cfg.watch_serials.iter().any(|x| x == serial) {
            self.status_line = format!("已在监控：{serial}");
            return;
        }
        self.cfg.watch_serials.push(serial.to_string());
        self.save_config();
        self.status_line = format!("已加入监控：{serial}");
        self.state.set_message(format!("已加入监控：{serial}"));
        self.state
            .log(LogLevel::Info, format!("已加入监控：{serial}"));
    }

    fn start_grab(&mut self) {
        if let Err(e) = self.cfg.validate_watch() {
            self.status_line = format!("{e}");
            self.state.set_message(format!("{e}"));
            self.state.log(LogLevel::Warn, format!("{e}"));
            return;
        }
        if !self
            .state
            .logged_in
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.status_line = "请先登录".into();
            self.state.set_message("请先登录");
            return;
        }
        if !self.cfg.profile_id.trim().is_empty() {
            *self.state.profile_id.lock() = self.cfg.profile_id.trim().to_string();
        }
        self.save_config();
        self.result_summary = None;
        worker::start_grab(self.state.clone(), self.cfg.clone());
        self.status_line = "抢课进行中".into();
        self.state.set_message("抢课进行中");
    }

    fn preflight_checks(&self) -> Vec<(bool, String)> {
        let logged = self
            .state
            .logged_in
            .load(std::sync::atomic::Ordering::Acquire);
        let targets = self.cfg.cleaned_watch().len();
        let interval_ok = self.cfg.interval_seconds.is_finite()
            && (0.05..=30.0).contains(&self.cfg.interval_seconds);
        let mut checks = vec![
            (
                logged,
                if logged {
                    "已登录".into()
                } else {
                    "尚未登录".into()
                },
            ),
            (
                targets > 0,
                if targets > 0 {
                    format!("监控目标 {targets} 门")
                } else {
                    "监控目标为空".into()
                },
            ),
            (
                interval_ok,
                format!("轮询间隔 {:.2} 秒", self.cfg.interval_seconds),
            ),
        ];
        if self.cfg.schedule_enabled && self.cfg.interval_seconds > 0.5 {
            checks.push((
                false,
                format!(
                    "定时开抢建议间隔 ≤ 0.3 秒（当前 {:.2}s），否则开课瞬间可能被抢满",
                    self.cfg.interval_seconds
                ),
            ));
        }
        if self.cfg.interval_seconds < 0.3 {
            checks.push((
                true,
                "间隔较激进，遇限流会自动放慢（若已开启自适应）".into(),
            ));
        }
        checks
    }

    fn maybe_trigger_schedule(&mut self, logged: bool, running: bool, logging_in: bool) {
        // If a precise arm already started the run, mark the key fired so we don't double-trigger.
        if running {
            if let Some(key) = self.schedule_armed_for.take() {
                self.schedule_fired_for = Some(key);
            }
            return;
        }
        if !self.cfg.schedule_enabled || !logged || logging_in {
            worker::cancel_schedule_arm(&self.state);
            self.schedule_armed_for = None;
            return;
        }
        if self.cfg.cleaned_watch().is_empty() {
            worker::cancel_schedule_arm(&self.state);
            self.schedule_armed_for = None;
            return;
        }
        let Some(stamp) = ScheduleStamp::parse(&self.cfg.schedule_time) else {
            worker::cancel_schedule_arm(&self.state);
            return;
        };
        let Some(target_secs) = stamp.to_local_seconds() else {
            worker::cancel_schedule_arm(&self.state);
            return;
        };
        let key = stamp.display();
        if self.schedule_fired_for.as_deref() == Some(key.as_str()) {
            return;
        }
        let now = worker::local_now_seconds();
        // Missed the window (e.g. app opened long after the target) — mark expired, don't fire.
        if now > target_secs + 30 {
            self.schedule_fired_for = Some(key);
            worker::cancel_schedule_arm(&self.state);
            return;
        }
        if now >= target_secs {
            self.schedule_fired_for = Some(key.clone());
            self.schedule_armed_for = None;
            worker::cancel_schedule_arm(&self.state);
            self.state
                .log(LogLevel::Info, format!("定时开抢触发：{key}"));
            self.status_line = format!("定时开抢已触发（{key}）");
            self.start_grab();
            return;
        }
        // Precise background arm once per key (avoid re-arming every UI frame).
        if self.schedule_armed_for.as_deref() != Some(key.as_str()) {
            self.schedule_armed_for = Some(key.clone());
            worker::arm_schedule(self.state.clone(), self.cfg.clone(), target_secs);
            let remain = target_secs - now;
            self.status_line = format!("定时精准确待命，约 {remain}s 后开抢");
        }
    }

    fn show_schedule_editor(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;
        let mut stamp = ScheduleStamp::parse(&self.cfg.schedule_time).unwrap_or(ScheduleStamp {
            year: 2026,
            month: 1,
            day: 1,
            hour: 8,
            minute: 0,
            second: 0,
        });

        egui::Frame::NONE
            .fill(GLASS_SOFT)
            .stroke(egui::Stroke::new(1.0, LINE))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(10.0, 8.0);
                    dirty |= ui
                        .checkbox(&mut self.cfg.schedule_enabled, "定时开抢")
                        .changed();
                    ui.label(
                        RichText::new("到点自动开始（需已登录且有监控目标）")
                            .size(CAPTION_SIZE)
                            .color(MUTED),
                    );
                });

                ui.add_space(8.0);
                ui.label(
                    RichText::new("开抢时刻")
                        .size(META_SIZE)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(6.0);

                // Date row: 年 月 日
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    let mut year = stamp.year as u32;
                    if number_drag_u32(ui, &mut year, true, 2024..=2035, 0.2, "年", 64.0).changed()
                    {
                        stamp.year = year as i32;
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.month, true, 1..=12, 0.15, "月", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    let max_day = days_in_month(stamp.year, stamp.month).max(1);
                    if stamp.day > max_day {
                        stamp.day = max_day;
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.day, true, 1..=max_day, 0.15, "日", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.add_space(6.0);
                // Time row: 时 分 秒
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    if number_drag_u32(ui, &mut stamp.hour, true, 0..=23, 0.15, "时", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.minute, true, 0..=59, 0.2, "分", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.second, true, 0..=59, 0.2, "秒", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                    if ui.add(quiet_button("设为现在", 88.0)).clicked() {
                        let (y, m, d, h, mi, s) = worker::now_parts();
                        stamp = ScheduleStamp {
                            year: y,
                            month: m,
                            day: d,
                            hour: h,
                            minute: mi,
                            second: s,
                        };
                        dirty = true;
                    }
                    if ui.add(quiet_button("明天 08:00", 96.0)).clicked() {
                        let (y, m, d, _, _, _) = worker::now_parts();
                        let mut ny = y;
                        let mut nm = m;
                        let mut nd = d + 1;
                        let max = days_in_month(ny, nm);
                        if nd > max {
                            nd = 1;
                            nm += 1;
                            if nm > 12 {
                                nm = 1;
                                ny += 1;
                            }
                        }
                        stamp = ScheduleStamp {
                            year: ny,
                            month: nm,
                            day: nd,
                            hour: 8,
                            minute: 0,
                            second: 0,
                        };
                        dirty = true;
                    }
                    ui.label(
                        RichText::new(format!(
                            "设定 {}　现在 {}",
                            stamp.display(),
                            worker::now_stamp()
                        ))
                        .size(CAPTION_SIZE)
                        .color(MUTED),
                    );
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("开抢冲刺").size(META_SIZE).color(MUTED));
                    let mut burst = self.cfg.open_burst_seconds as f64;
                    if number_drag_f64(ui, &mut burst, true, 0.0..=120.0, 1.0, 0, "秒", 72.0)
                        .changed()
                    {
                        self.cfg.open_burst_seconds = burst.round().clamp(0.0, 120.0) as u32;
                        dirty = true;
                    }
                    ui.label(
                        RichText::new("开始后前 N 秒去掉轮询正抖动，首轮不等待")
                            .size(CAPTION_SIZE)
                            .color(MUTED),
                    );
                });

                if self.cfg.schedule_enabled {
                    ui.add_space(4.0);
                    if let Some(target) = stamp.to_local_seconds() {
                        let now = worker::local_now_seconds();
                        let hint = if now < target {
                            let wait = target - now;
                            let h = wait / 3600;
                            let m = (wait % 3600) / 60;
                            let s = wait % 60;
                            format!("距开抢还有 {h:02}:{m:02}:{s:02}")
                        } else if now <= target + 30 {
                            "即将触发…".into()
                        } else {
                            "该时刻已过，请重新选择".into()
                        };
                        ui.label(
                            RichText::new(hint)
                                .size(CAPTION_SIZE)
                                .color(if now < target { BLUE } else { AMBER }),
                        );
                    }
                }
            });

        if dirty {
            if let Some(valid) = stamp.validated() {
                self.cfg.schedule_time = valid.display();
                // re-arm when user changes the target
                self.schedule_fired_for = None;
                self.schedule_armed_for = None;
                worker::cancel_schedule_arm(&self.state);
            }
        }
        dirty
    }

    fn move_watch(&mut self, serial: &str, delta: isize) {
        let Some(idx) = self.cfg.watch_serials.iter().position(|s| s == serial) else {
            return;
        };
        let new_idx = idx as isize + delta;
        if new_idx < 0 || new_idx as usize >= self.cfg.watch_serials.len() {
            return;
        }
        self.cfg.watch_serials.swap(idx, new_idx as usize);
        self.save_config();
    }

    fn clear_watches_by_state(&mut self, states: &[WatchState]) {
        let serials: Vec<String> = self
            .state
            .watch
            .lock()
            .iter()
            .filter(|item| states.contains(&item.state))
            .map(|item| item.serial.clone())
            .collect();
        if serials.is_empty() {
            return;
        }
        self.cfg
            .watch_serials
            .retain(|serial| !serials.iter().any(|s| s == serial));
        for serial in &serials {
            self.cfg.watch_lesson_ids.remove(serial);
            self.cfg.watch_meta.remove(serial);
        }
        self.state
            .watch
            .lock()
            .retain(|item| !serials.iter().any(|s| s == &item.serial));
        self.save_config();
        self.status_line = format!("已清理 {} 个目标", serials.len());
        self.state.log(
            LogLevel::Info,
            format!("已清理 {} 个监控目标", serials.len()),
        );
    }

    fn build_result_summary(&self) -> String {
        let watch = self.state.watch.lock().clone();
        if watch.is_empty() {
            return self.status_line.clone();
        }
        let mut success = 0usize;
        let mut failed = 0usize;
        let mut stopped = 0usize;
        let mut other = 0usize;
        let mut total_checks = 0u32;
        let mut lines = Vec::new();
        for item in &watch {
            total_checks = total_checks.saturating_add(item.checks);
            match item.state {
                WatchState::Success => {
                    success += 1;
                    lines.push(format!("✓ {} {}", item.serial, item.name));
                }
                WatchState::Failed | WatchState::Missing | WatchState::Ambiguous => {
                    failed += 1;
                    lines.push(format!("× {} {} — {}", item.serial, item.name, item.detail));
                }
                WatchState::Stopped => {
                    stopped += 1;
                    lines.push(format!("· {} {} — 已停止", item.serial, item.name));
                }
                _ => {
                    other += 1;
                    lines.push(format!(
                        "· {} {} — {}",
                        item.serial,
                        item.name,
                        item.state.label()
                    ));
                }
            }
        }
        let mut head = format!(
            "成功 {success}　失败 {failed}　停止 {stopped}　其他 {other}\n总检查 {total_checks} 次"
        );
        if !lines.is_empty() {
            head.push_str("\n\n");
            head.push_str(&lines.join("\n"));
        }
        head
    }
}

#[cfg(test)]
mod tests {
    use super::theme::custom_host_requiring_confirmation;
    use super::*;

    #[test]
    fn right_aligned_header_toggle_keeps_layout_finite() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1240.0, 780.0),
            )),
            ..Default::default()
        };
        let mut enabled = false;
        let _ = ctx.run_ui(input.take(), |ui| {
            ui.horizontal(|ui| {
                ui.label("状态");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    outline_toggle(ui, &mut enabled, "日志");
                    ui.add_sized(
                        [ui.available_width().clamp(120.0, 420.0), 28.0],
                        egui::Label::new("运行状态").truncate(),
                    );
                });
            });
        });
    }

    #[test]
    fn custom_hosts_require_per_session_confirmation() {
        assert_eq!(
            custom_host_requiring_confirmation("https://jwxt.sias.edu.cn/eams"),
            None
        );
        assert_eq!(
            custom_host_requiring_confirmation("http://127.0.0.1:8080/eams"),
            None
        );
        assert_eq!(
            custom_host_requiring_confirmation("https://example.com/eams"),
            Some("example.com".into())
        );
    }
}

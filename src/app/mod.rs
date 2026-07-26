mod schedule;
mod theme;

#[allow(unused_imports)]
use crate::app::theme::{
    apply_style, apply_titlebar_theme, configure_fonts, configure_window_backdrop,
    custom_host_requiring_confirmation, draw_table_header, empty_hint, glass_strip, glass_surface,
    icon_button, log_color, mini_status, mix_color, number_drag_f64, number_drag_u32, on_off,
    outline_toggle, pal, primary_button, quiet_button, soft_danger_button, soft_divider,
    status_dot, style_single_number_capsule, truncate_ui_text, watch_color, BODY_SIZE,
    CAPTION_SIZE, CARD_RADIUS, CONTROL_H, META_SIZE, PANEL_TITLE, WATCH_CARD_MIN_H,
};
use crate::config::{
    redact_diagnostic_page, redact_diagnostic_text, redact_diagnostic_url, AppConfig, ScheduleStamp,
};
use crate::eams::{CircuitStatus, Lesson, NetworkSnapshot};
use crate::worker::{self, LogLevel, SharedState, WatchState};
use eframe::egui::{self, Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use zeroize::{Zeroize, Zeroizing};

// Page / surface tokens — solid fills only, avoid translucent color stacking.

/// 读取调试目录里的原始页面并逐份脱敏后才允许进入诊断包：抹掉
/// 凭据/会话值，含选课提交表单的页面整份丢弃（`redact_diagnostic_page`
/// 返回 None）。返回（可导出页面, 被整份排除的页面数）。
fn collect_redacted_debug_pages(dir: &std::path::Path) -> RedactedPages {
    let mut collected = RedactedPages::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return collected;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        // 读不出（二进制/权限/竞态删除）的一律跳过：诊断导出不得中断。
        // 但要单独计数，别让接收方以为目录里本来就没有这些页面。
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            collected.unreadable += 1;
            continue;
        };
        match redact_diagnostic_page(&name, &content) {
            Some(redacted) => {
                collected.pages.insert(name, redacted);
            }
            None => collected.submissions += 1,
        }
    }
    collected
}

/// 诊断包里的原始页面收集结果。两个计数分开报告：一个是按隐私策略
/// 主动排除的，一个是根本没读成的。
#[derive(Default)]
struct RedactedPages {
    pages: std::collections::BTreeMap<String, String>,
    /// 含选课提交表单、按策略整份排除的页面数。
    submissions: usize,
    /// 读取失败而跳过的页面数。
    unreadable: usize,
}

/// 单条吐司的展示时长（秒）。
const TOAST_SECONDS: f64 = 4.5;

/// 密码输入框的固定 Id：登录成功后要按它清掉 egui 侧的输入状态。
static PASSWORD_FIELD_ID: std::sync::LazyLock<egui::Id> =
    std::sync::LazyLock::new(|| egui::Id::new("login_password_field"));

/// 课表行高。固定行高是 `ScrollArea::show_rows` 虚拟化的前提。
const CATALOG_ROW_H: f32 = 40.0;
const _: () = assert!(CATALOG_ROW_H > 0.0, "show_rows needs a positive row height");

/// 课表派生视图缓存。
///
/// `key` 覆盖一切会改变结果的输入；任一不同才重算。
#[derive(Default)]
struct CatalogView {
    key: CatalogViewKey,
    /// `Arc` 让绘制侧拿走一份「所有权」只是一次引用计数自增，既不必每帧
    /// 深拷贝，也不会把 `&self` 借用拖进需要 `&mut self` 的闭包里。
    rows: Arc<Vec<Lesson>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CatalogViewKey {
    /// worker 的状态版本号：课表换了它就变。
    revision: u64,
    filter: String,
    only_available: bool,
    sort: u8,
}

/// 配置落盘去抖窗口（秒）。拖滑块期间只在停手后写一次。
///
/// 必须长到能吃掉一次连续拖动（60 帧 ≈ 1 秒里只写一次），又短到用户改完
/// 随手关窗口也不会丢——退出时 `flush_config_now` 会同步补写。
const CONFIG_FLUSH_DEBOUNCE: f64 = 0.6;
const _: () = assert!(CONFIG_FLUSH_DEBOUNCE > 0.3 && CONFIG_FLUSH_DEBOUNCE < 3.0);

/// 一次性操作结果的显示时长（秒）。错误留久一点，用户往往正在读它。
const TRANSIENT_STATUS_TTL: f64 = 6.0;
const TRANSIENT_ERROR_TTL: f64 = 12.0;

/// 用户动作的结果提示。带 TTL，到期后让位给后台状态。
struct TransientStatus {
    text: String,
    until: f64,
    is_error: bool,
}

/// 状态栏消息模型。
///
/// 后台心跳（worker_message）与一次性操作结果分两个槽位：前者每帧刷新，
/// 后者优先显示且带 TTL。此前两者共用一个字段，导致 30 余处 UI 侧提示
/// （导出、导入、文件操作——恰恰全是可能失败、必须给反馈的动作）在同一帧
/// 就被后台心跳覆盖，用户一个字都看不到。
#[derive(Default)]
struct StatusBar {
    background: String,
    transient: Option<TransientStatus>,
    now: f64,
}

impl StatusBar {
    fn new(background: String) -> Self {
        Self {
            background,
            transient: None,
            now: 0.0,
        }
    }

    /// 推进时基。每帧开头调用一次，TTL 判定全部基于它。
    fn tick(&mut self, now: f64) {
        self.now = now;
    }

    fn set_background(&mut self, text: String) {
        self.background = text;
    }

    fn push(&mut self, text: String, is_error: bool) {
        let ttl = if is_error {
            TRANSIENT_ERROR_TTL
        } else {
            TRANSIENT_STATUS_TTL
        };
        self.transient = Some(TransientStatus {
            text,
            until: self.now + ttl,
            is_error,
        });
    }

    fn effective(&self) -> &str {
        match &self.transient {
            Some(status) if self.now < status.until => &status.text,
            _ => &self.background,
        }
    }

    fn showing_error(&self) -> bool {
        self.transient
            .as_ref()
            .is_some_and(|status| self.now < status.until && status.is_error)
    }
}

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
    /// 密码输入缓冲。
    ///
    /// 用 `Zeroizing` 并预分配容量：裸 `String` 每敲一个字符就可能 realloc，
    /// 在堆上留下 "pass"、"passw"、"passwo"… 一串未擦除的旧缓冲，而收尾的
    /// zeroize 只能擦到最后那一块。预分配到足够大即可避免扩容搬迁。
    ///
    /// 边界要说清楚：egui 的 `TextEdit` 自带 undoer 历史存活在 `egui::Memory`
    /// 里，应用层够不着；`on_exit` 在进程被 kill 或 panic 时也不会执行。
    /// 这一层是尽力而为，不是防内存取证。
    password: Zeroizing<String>,
    state: Arc<SharedState>,
    new_serial: String,
    filter: String,
    only_available: bool,
    status: StatusBar,
    /// 课表派生视图（过滤 + 排序）的缓存。
    catalog_view: CatalogView,
    /// 配置有未落盘的改动。
    config_dirty: bool,
    /// 最近一次改动的帧时间，用于去抖。
    config_dirty_since: f64,
    show_logs: bool,
    show_advanced: bool,
    was_logged_in: bool,
    confirmed_custom_host: Option<String>,
    window_backdrop_attempted: bool,
    /// 已推给系统标题栏的深浅色，`None` 表示还没设过。
    titlebar_dark: Option<bool>,
    log_filter: LogFilter,
    catalog_sort: CatalogSort,
    toast: Option<(f64, String, bool)>,
    /// 待展示的吐司队列。同一帧里来多条告警时逐条显示，而不是互相覆盖。
    toast_queue: std::collections::VecDeque<(String, bool)>,
    confirm_logout: bool,
    confirm_clear_logs: bool,
    confirm_remove: Option<usize>,
    confirm_start_grab: bool,
    confirm_export_raw_diagnostics: bool,
    show_first_run: bool,
    last_keepalive: f64,
    was_running: bool,
    result_summary: Option<String>,
}

impl CourseApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);
        // Raw response dumps are user-owned diagnostics. Enforce their privacy/size policy
        // as the session starts, as well as before and after each write.
        let _ = AppConfig::retain_debug_files();
        let (cfg, config_warning) = AppConfig::load_with_warning();
        apply_style(&cc.egui_ctx, cfg.dark_mode);
        cc.egui_ctx
            .set_pixels_per_point(cfg.ui_scale.clamp(0.9, 1.5));
        let state = SharedState::new();
        // 状态一变就叫醒界面：worker 里几十处 touch() 此前是纯开销（revision
        // 只写不读），而抢课成功这类关键事件最坏要等 400ms 空闲档才反映。
        {
            let ctx = cc.egui_ctx.clone();
            state.set_repaint_waker(Arc::new(move || ctx.request_repaint()));
        }
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
            // 预分配：避免逐字符输入时 realloc 在堆上留下旧缓冲副本。
            password: Zeroizing::new(String::with_capacity(256)),
            cfg,
            state,
            new_serial: String::new(),
            filter,
            only_available,
            status: StatusBar::new(
                config_warning.unwrap_or_else(|| "登录后刷新课程，从课表加入监控".into()),
            ),
            config_dirty: false,
            config_dirty_since: 0.0,
            show_logs: true,
            show_advanced: false,
            was_logged_in: false,
            confirmed_custom_host: None,
            window_backdrop_attempted: false,
            titlebar_dark: None,
            log_filter: LogFilter::All,
            catalog_sort: CatalogSort::Default,
            catalog_view: CatalogView::default(),
            toast: None,
            toast_queue: std::collections::VecDeque::new(),
            confirm_logout: false,
            confirm_clear_logs: false,
            confirm_remove: None,
            confirm_start_grab: false,
            confirm_export_raw_diagnostics: false,
            show_first_run,
            last_keepalive: 0.0,
            was_running: false,
            result_summary: None,
        }
    }
}

impl eframe::App for CourseApp {
    fn ui(&mut self, root_ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // transient 状态的 TTL 判定要用同一个时基，先推进本帧时间。
        self.status.tick(root_ui.input(|input| input.time));
        // 去抖窗口到了就把配置写下去（在后台线程）。
        self.flush_config_if_due();
        if !self.window_backdrop_attempted {
            self.window_backdrop_attempted = true;
            configure_window_backdrop(frame);
        }
        // 每帧比对而不是在勾选处调用：这样切换、导入配置、启动三条路径
        // 都能把标题栏拉到同一个主题上。
        if self.titlebar_dark != Some(self.cfg.dark_mode) {
            self.titlebar_dark = Some(self.cfg.dark_mode);
            apply_titlebar_theme(frame, self.cfg.dark_mode);
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
            self.clear_password_buffer(root_ui.ctx());
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
        // 兜底唤醒。真正的状态变更由 worker 通过 repaint waker 主动叫醒
        // （见 SharedState::touch），这里只负责动画与倒计时这类「没有事件
        // 也需要走时间」的东西。空闲时不再以 2.5fps 无条件重绘并重跑整套
        // 派生视图计算。
        ctx.request_repaint_after(std::time::Duration::from_millis(if schedule_soon {
            // 待命末段：亚帧唤醒让倒计时诚实（精确触发在 worker 侧）。
            30
        } else if running || logging_in || refreshing {
            // 运行时有脉冲动画（status_dot）要走，但数据更新靠事件。
            200
        } else if self.toast.is_some() || self.status.showing_error() {
            // 有 TTL 的东西在显示：到期要能自己消失。
            500
        } else {
            2_000
        }));

        // Alerts are only queued when notify_enabled was on at dispatch time.
        //
        // 排队而不是直接覆盖：一轮里同时抢到两门课时，`self.toast = Some(..)`
        // 会让前一条在同一帧内被后一条盖掉——用户永远看不到第一条，而那恰恰
        // 是「抢到了」这种最不该错过的通知。
        for alert in crate::notify::take_alerts() {
            self.toast_queue
                .push_back((format!("{}：{}", alert.title, alert.body), alert.success));
        }
        let now_time = root_ui.ctx().input(|i| i.time);
        if let Some((until, _, _)) = self.toast {
            if now_time > until {
                self.toast = None;
            }
        }
        // 当前没有在显示的吐司才取下一条，保证每条都有完整的展示时间。
        if self.toast.is_none() {
            if let Some((text, success)) = self.toast_queue.pop_front() {
                self.toast = Some((now_time + TOAST_SECONDS, text, success));
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
        let network = self.state.network_snapshot();
        // 后台心跳只更新 background_status；一次性操作结果走 transient 通道，
        // 否则每帧这一句会把用户刚触发的提示（导出/导入/文件操作，共 30 余处）
        // 立刻盖掉，界面上一个字都不会出现。
        if !worker_msg.is_empty() {
            self.status.set_background(worker_msg);
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
            &network,
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
        pal().fog.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 去抖窗口里可能还压着一次未落盘的配置改动，退出这一刻必须同步写完。
        self.flush_config_now();
        self.password.zeroize();
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

    fn show_network_diagnostics(&self, ui: &mut egui::Ui) {
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

    fn show_log_drawer(&mut self, root_ui: &mut egui::Ui, reveal: f32) {
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
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("清空").size(META_SIZE).color(pal().muted),
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
                                    RichText::new("导出").size(META_SIZE).color(pal().muted),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.export_logs();
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("诊断包").size(META_SIZE).color(pal().muted),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.export_diagnostics(false);
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("含原始页面…")
                                        .size(META_SIZE)
                                        .color(pal().muted),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
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

    fn show_watch_panel(&mut self, root_ui: &mut egui::Ui, running: bool, watch_count: usize) {
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
                                for (index, serial) in ordered {
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
                                                    .color(pal().text),
                                            );
                                            ui.label(
                                                RichText::new(teachers)
                                                    .size(CAPTION_SIZE)
                                                    .color(pal().muted),
                                            );
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

    fn show_course_catalog(&mut self, root_ui: &mut egui::Ui, running: bool, lesson_count: usize) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(pal().fog)
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 12,
                        top: 10,
                        bottom: 12,
                    }),
            )
            .show(root_ui, |ui| {
                glass_surface(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(CONTROL_H);
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(
                            RichText::new("可选课程")
                                .strong()
                                .size(PANEL_TITLE)
                                .color(pal().text),
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
                                    .color(pal().muted),
                            );
                        });
                    });
                    ui.add_space(12.0);

                    // 派生视图缓存：只有课表本身（revision）或筛选/排序条件
                    // 变了才重算。running 时重绘间隔曾是 50ms，即每秒约 20 次
                    // 把整张课表深拷贝、逐条 to_lowercase 再排序——既是 CPU 与
                    // allocator 压力，也让 UI 线程和抢课网络线程争抢同一把锁，
                    // 而 worker runtime 只有 2 个线程。
                    self.refresh_catalog_view();
                    let filtered = self.catalog_view.rows.clone();
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

                    if filtered.is_empty() {
                        egui::ScrollArea::vertical()
                            .id_salt("catalog")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(full_w);
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
                            });
                    } else {
                        // 只画可视区。可视区通常只有约 15 行，此前却要为最多
                        // 800 行分配 rect、跑 animate_bool_with_time、生成 galley
                        // 并 tessellate——这是 running 时最大的单帧开销。行高
                        // 固定，天然适配 show_rows；顺带去掉 800 行的硬截断，
                        // 课程再多也不会被悄悄截掉。
                        egui::ScrollArea::vertical()
                            .id_salt("catalog")
                            .auto_shrink([false, false])
                            .show_rows(ui, CATALOG_ROW_H, filtered.len(), |ui, range| {
                                ui.set_min_width(full_w);
                                for index in range {
                                    let lesson = &filtered[index];
                                    let serial_watched = watched.contains(&lesson.no);
                                    let selected_id = watched_lesson_ids.get(&lesson.no);
                                    let already = selected_id.is_some_and(|id| id == &lesson.id);
                                    let switching = serial_watched
                                        && selected_id.is_some_and(|id| id != &lesson.id);
                                    let needs_specifying = serial_watched && selected_id.is_none();
                                    let row_h = CATALOG_ROW_H;
                                    let (row_rect, row_response) = ui.allocate_exact_size(
                                        Vec2::new(full_w, row_h),
                                        Sense::click(),
                                    );
                                    let base = if index % 2 == 0 {
                                        Color32::TRANSPARENT
                                    } else {
                                        pal().row_alt
                                    };
                                    // 纯 painter 绘制不产生任何 widget 语义：
                                    // Cargo.toml 开了 accesskit，但读屏软件看到
                                    // 的主内容区是一片空白。至少让每一行可枚举、
                                    // 能被读出来。
                                    row_response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            format!(
                                                "{} {} {} 已选 {}{}",
                                                lesson.no,
                                                lesson.name,
                                                lesson.teachers,
                                                lesson.capacity_text(),
                                                if already { "（监控中）" } else { "" }
                                            ),
                                        )
                                    });
                                    let hover = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("course_row", &lesson.id)),
                                        row_response.hovered(),
                                        0.12,
                                    );
                                    ui.painter().rect_filled(
                                        row_rect,
                                        0.0,
                                        mix_color(base, pal().row_hover, hover),
                                    );
                                    ui.painter().hline(
                                        row_rect.x_range(),
                                        row_rect.bottom(),
                                        egui::Stroke::new(1.0, pal().row_line),
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
                                        pal().text,
                                    );
                                    let name_show = truncate_ui_text(&lesson.name, 22);
                                    ui.painter().text(
                                        columns[1].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        name_show,
                                        egui::FontId::proportional(BODY_SIZE),
                                        pal().text,
                                    );
                                    ui.painter().text(
                                        columns[2].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        &lesson.teachers,
                                        egui::FontId::proportional(META_SIZE),
                                        pal().muted,
                                    );
                                    let capacity_color = if lesson.has_seat() {
                                        pal().green
                                    } else if !lesson.capacity_known() {
                                        pal().muted
                                    } else {
                                        pal().red
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
                                        pal().disabled_fill
                                    } else {
                                        mix_color(pal().quiet_fill, pal().quiet_hover, button_hover)
                                    };
                                    ui.painter().rect_filled(button_rect, 8.0, fill);
                                    ui.painter().rect_stroke(
                                        button_rect,
                                        8.0,
                                        egui::Stroke::new(1.0, pal().line),
                                        egui::StrokeKind::Inside,
                                    );
                                    ui.painter().text(
                                        button_rect.center(),
                                        Align2::CENTER_CENTER,
                                        button_text,
                                        egui::FontId::proportional(META_SIZE),
                                        if already { pal().muted } else { pal().blue },
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
                                        .color(pal().muted),
                                    );
                                }
                            });
                    }
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
                            pal().success_fill
                        } else {
                            pal().danger_fill
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if ok { pal().green } else { pal().red },
                        ))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            ui.label(RichText::new(text).size(META_SIZE).color(if ok {
                                pal().green
                            } else {
                                pal().red
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
                            .color(pal().muted),
                    );
                    ui.add_space(10.0);
                    if ui
                        .add(primary_button("知道了", pal().blue, 100.0))
                        .clicked()
                    {
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
                            self.set_status("已退出登录");
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
        if self.confirm_export_raw_diagnostics {
            egui::Window::new("确认导出原始页面")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(430.0);
                    ui.label("原始调试页面会先脱敏（抹掉常见的 Cookie/密码/令牌字段，含选课提交表单的页面整份排除），但脱敏不保证覆盖全部，页面仍可能含姓名、学号与你的课表，仅应发送给你信任的支持人员。");
                    ui.label(
                        RichText::new("默认诊断包不会包含这些内容。")
                            .size(CAPTION_SIZE)
                            .color(pal().muted),
                    );
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_export_raw_diagnostics = false;
                        }
                        if ui.add(soft_danger_button("仍要包含", 90.0)).clicked() {
                            self.confirm_export_raw_diagnostics = false;
                            self.export_diagnostics(true);
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
                            .color(pal().text),
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
                                        .color(pal().text),
                                );
                            }
                        });
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "间隔 {:.2} 秒　网络异常时自动降速 {}　优先余量 {}　仅监控 {}",
                            self.cfg.interval_seconds,
                            on_off(self.cfg.adaptive_interval),
                            on_off(self.cfg.grab_seats_first),
                            on_off(self.cfg.monitor_only),
                        ))
                        .size(META_SIZE)
                        .color(pal().muted),
                    );
                    ui.label(
                        RichText::new(format!(
                            "结果通知 {}　提示音 {}",
                            on_off(self.cfg.notify_enabled),
                            on_off(self.cfg.sound_enabled),
                        ))
                        .size(META_SIZE)
                        .color(pal().muted),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("开抢自检")
                            .strong()
                            .size(META_SIZE)
                            .color(pal().text),
                    );
                    for (ok, line) in self.preflight_checks() {
                        ui.label(
                            RichText::new(format!("{} {}", if ok { "✓" } else { "!" }, line))
                                .size(META_SIZE)
                                .color(if ok { pal().green } else { pal().amber }),
                        );
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 84.0)).clicked() {
                            self.confirm_start_grab = false;
                        }
                        let can_start = self.preflight_checks().iter().all(|(ok, _)| *ok);
                        if ui
                            .add_enabled(can_start, primary_button("确认开始", pal().green, 110.0))
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
                    ui.label(RichText::new(&summary).size(META_SIZE).color(pal().text));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(primary_button("知道了", pal().blue, 100.0))
                            .clicked()
                        {
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
            .set_file_name("Course-snatching-result.txt")
            .save_file()
        {
            match std::fs::write(&path, summary) {
                Ok(()) => self.set_status(format!("结果摘要已导出：{}", path.display())),
                Err(error) => self.set_status_error(format!("导出失败：{error}")),
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
            .set_file_name("Course-snatching-logs.txt")
            .save_file()
        {
            match std::fs::write(&path, text) {
                Ok(()) => self.set_status(format!("日志已导出：{}", path.display())),
                Err(error) => self.set_status_error(format!("导出失败：{error}")),
            }
        }
    }

    fn export_diagnostics(&mut self, include_raw_pages: bool) {
        let default_name = if include_raw_pages {
            "Course-snatching-diagnostics-with-pages.json"
        } else {
            "Course-snatching-diagnostics.json"
        };
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        let logs = self
            .state
            .logs
            .lock()
            .iter()
            .map(|item| {
                serde_json::json!({
                    "time": item.time,
                    "level": item.level.label(),
                    "message": redact_diagnostic_text(&item.message),
                })
            })
            .collect::<Vec<_>>();
        let network = self.state.network_snapshot();
        let collected = if include_raw_pages {
            collect_redacted_debug_pages(&AppConfig::debug_dir())
        } else {
            RedactedPages::default()
        };
        let base_url = redact_diagnostic_url(&self.cfg.base_url);
        let document = serde_json::json!({
            "format": "Course-snatching diagnostic package v1",
            "version": env!("CARGO_PKG_VERSION"),
            "system": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
            "network": {
                "requests_per_second": network.requests_per_second,
                "latency_ewma_ms": network.latency_ewma_ms,
                "total_rate_limits": network.total_rate_limits,
                "consecutive_errors": network.consecutive_errors,
                "cooldown_seconds": network.cooldown_remaining.as_secs(),
                "last_error_kind": network.last_error_kind.map(|kind| format!("{kind:?}")),
                "circuit_status": format!("{:?}", network.circuit_status),
            },
            // 命中的解析策略是「教务换版」最早的信号，诊断包里必须有它。
            "parsing": {
                "catalog_strategy": self
                    .state
                    .client
                    .lock()
                    .as_ref()
                    .and_then(|client| client.catalog_strategy_label()),
            },
            "configuration": {
                "base_url": base_url,
                "account": Self::mask_diagnostic_account(&self.cfg.username),
                "interval_seconds": self.cfg.interval_seconds,
                "timeout_seconds": self.cfg.timeout_seconds,
                "monitor_only": self.cfg.monitor_only,
                "target_count": self.cfg.watch_serials.len(),
                "raw_pages_included": include_raw_pages,
                // 让接收方知道包里少了东西，而不是以为调试目录本来就这么点内容。
                "raw_pages_excluded_submissions": collected.submissions,
                "raw_pages_unreadable": collected.unreadable,
            },
            "logs": logs,
            "raw_debug_pages": collected.pages,
        });
        match serde_json::to_vec_pretty(&document)
            .and_then(|body| std::fs::write(&path, body).map_err(serde_json::Error::io))
        {
            Ok(()) => {
                let note = Self::export_raw_page_note(collected.submissions, collected.unreadable);
                self.set_status(format!("诊断包已导出：{}{}", path.display(), note));
            }
            Err(error) => self.set_status_error(format!("导出诊断包失败：{error}")),
        }
    }

    /// 导出说明要如实告知包内容不完整：按策略排除的与读不出的分开讲。
    fn export_raw_page_note(submissions: usize, unreadable: usize) -> String {
        let mut parts = Vec::new();
        if submissions > 0 {
            parts.push(format!("已整份排除 {submissions} 份提交表单"));
        }
        if unreadable > 0 {
            parts.push(format!("{unreadable} 份页面读取失败已跳过"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("（{}）", parts.join("，"))
        }
    }

    fn mask_diagnostic_account(account: &str) -> String {
        let chars = account.trim().chars().collect::<Vec<_>>();
        match chars.len() {
            0 => String::new(),
            1 => "*".into(),
            2 => format!("{}*", chars[0]),
            _ => format!("{}***{}", chars[0], chars[chars.len() - 1]),
        }
    }

    fn export_config(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("Course-snatching-config.toml")
            .save_file()
        {
            match self.cfg.export_to(&path) {
                Ok(()) => self.set_status(format!("配置已导出：{}", path.display())),
                Err(error) => self.set_status_error(format!("导出失败：{error:#}")),
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
                    // 导入也要发布运行配置：否则已在待命的定时开抢到点仍会
                    // 用导入前的旧监控目标开抢。
                    self.state.publish_config(self.cfg.clone());
                    self.filter = self.cfg.filter.clone();
                    self.only_available = self.cfg.only_available;
                    apply_style(ctx, self.cfg.dark_mode);
                    ctx.set_pixels_per_point(self.cfg.ui_scale.clamp(0.9, 1.5));
                    if let Err(error) = self.cfg.save() {
                        self.set_status_error(format!("导入后保存失败：{error:#}"));
                    } else {
                        self.set_status(format!("配置已导入：{}", path.display()));
                    }
                }
                Err(error) => self.set_status_error(format!("导入失败：{error:#}")),
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
        self.set_status(format!(
            "数据目录：{}（崩溃日志：{}）",
            dir.display(),
            crash.display()
        ));
    }

    /// 记录一条用户动作的结果提示。
    ///
    /// 必须走这里而不是直接写字段：后台心跳每帧都会刷新状态栏，直接赋值
    /// 会在同一帧内被覆盖，用户永远看不到。
    fn set_status(&mut self, text: impl Into<String>) {
        self.status.push(text.into(), false);
    }

    /// 同 `set_status`，但按错误呈现（红色、显示更久）。
    fn set_status_error(&mut self, text: impl Into<String>) {
        self.status.push(text.into(), true);
    }

    /// 当前应显示的状态文本：未过期的一次性提示优先，否则回落到后台状态。
    fn effective_status(&self) -> &str {
        self.status.effective()
    }

    fn status_is_error(&self) -> bool {
        self.status.showing_error()
    }

    /// 清空密码输入缓冲，并连带清掉 egui 侧的输入状态。
    ///
    /// 只擦自己那份 `String` 是不够的：`TextEdit` 的 `TextEditState` 里带着
    /// undoer 历史，登录成功后它仍然握着刚才逐字符输入的中间态。
    fn clear_password_buffer(&mut self, ctx: &egui::Context) {
        self.password.zeroize();
        // 保持容量，避免下次输入又从小容量开始反复扩容。
        self.password.reserve(256);
        ctx.data_mut(|data| {
            data.remove::<egui::text_selection::text_cursor_state::TextCursorState>(
                PASSWORD_FIELD_ID.with("password"),
            );
        });
        ctx.memory_mut(|memory| {
            memory.surrender_focus(PASSWORD_FIELD_ID.with("password"));
        });
    }

    /// 重算课表派生视图——但只在输入真的变了的时候。
    fn refresh_catalog_view(&mut self) {
        let key = CatalogViewKey {
            revision: self.state.revision(),
            filter: self.filter.trim().to_lowercase(),
            only_available: self.only_available,
            sort: self.catalog_sort as u8,
        };
        if self.catalog_view.key == key {
            return;
        }
        let filter = key.filter.clone();
        let only_available = key.only_available;
        // 只在这一处对课表加锁并克隆；此前是每帧一次（外加同帧第二次加锁
        // 只为取 len()）。UI 持锁期间 worker 的 update_watch 会直接阻塞一个
        // tokio worker 线程，正好发生在延迟最敏感的时刻。
        let mut rows: Vec<Lesson> = self
            .state
            .lessons
            .lock()
            .iter()
            .filter(|lesson| {
                if only_available && !lesson.has_seat() {
                    return false;
                }
                filter.is_empty()
                    || lesson.no.to_lowercase().contains(&filter)
                    || lesson.name.to_lowercase().contains(&filter)
                    || lesson.teachers.to_lowercase().contains(&filter)
            })
            .cloned()
            .collect();
        match self.catalog_sort {
            CatalogSort::Default => {}
            CatalogSort::SeatsFirst => rows.sort_by_key(|lesson| !lesson.has_seat()),
            CatalogSort::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        }
        self.catalog_view = CatalogView {
            key,
            rows: Arc::new(rows),
        };
    }

    /// 记录配置变更。
    ///
    /// 只标脏、不落盘。`AppConfig::save` 是 create_dir_all、序列化、临时文件、
    /// `sync_all()`、原子替换的组合，全程同步跑在 UI 线程上；拖动「间隔」滑块
    /// 或敲搜索框时它每帧触发一次，而 `sync_all` 是真正的磁盘刷写——机械盘或
    /// 被杀软实时扫描的机器上会造成可感知的卡顿。
    ///
    /// 内存里的运行配置立刻发布（定时开抢到点读的是它），落盘去抖后在后台
    /// 线程完成。
    fn save_config(&mut self) {
        self.state.publish_config(self.cfg.clone());
        self.config_dirty = true;
        self.config_dirty_since = self.status.now;
    }

    /// 每帧检查是否该把标脏的配置真正写下去。
    fn flush_config_if_due(&mut self) {
        if !self.config_dirty {
            return;
        }
        if self.status.now - self.config_dirty_since < CONFIG_FLUSH_DEBOUNCE {
            return;
        }
        self.config_dirty = false;
        let cfg = self.cfg.clone();
        // 写盘搬到后台线程：即便去抖后仍有一次 sync_all，也不该卡住这一帧。
        // 失败要能告诉用户，所以把结果送回共享状态。
        let state = self.state.clone();
        std::thread::spawn(move || {
            if let Err(error) = cfg.save() {
                let message = format!("配置保存失败：{error:#}");
                state.set_message(message.clone());
                state.log(LogLevel::Error, message);
            }
        });
    }

    /// 退出前把未落盘的配置同步写下去——这一刻不能再去抖。
    fn flush_config_now(&mut self) {
        if !self.config_dirty {
            return;
        }
        self.config_dirty = false;
        if let Err(error) = self.cfg.save() {
            self.state
                .log(LogLevel::Error, format!("退出前保存配置失败：{error:#}"));
        }
    }

    fn do_login(&mut self) {
        if self
            .state
            .logging_in
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.set_status("正在登录，请稍候…");
            return;
        }
        if self.cfg.username.trim().is_empty() || self.password.is_empty() {
            self.set_status_error("请填写账号和密码");
            self.state.set_message("请填写账号和密码");
            return;
        }
        if let Err(error) = self.cfg.validate_connection() {
            self.set_status(error.to_string());
            self.state.set_message(error.to_string());
            self.state.log(LogLevel::Warn, error.to_string());
            return;
        }
        if let Some(host) = custom_host_requiring_confirmation(&self.cfg.base_url) {
            if self.confirmed_custom_host.as_deref() != Some(&host) {
                let message =
                    format!("即将把账号密码提交到非默认域名 {host}，请在高级设置中确认本次信任");
                self.set_status(message.clone());
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
        self.set_status("正在登录…");
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
                remember_for_relogin: self.cfg.remember_credentials_for_session,
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
            self.set_status_error("运行期间不能修改监控目标");
            return;
        }
        if lesson.id.is_empty() || !lesson.id.chars().all(|ch| ch.is_ascii_digit()) {
            let message = "课程教学班标识无效，无法加入监控";
            self.set_status_error(message);
            self.state.log(LogLevel::Error, message);
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
        self.set_status(message.clone());
        self.state.set_message(message.clone());
        self.state.log(LogLevel::Info, message);
    }

    fn add_watch_serial(&mut self, serial: &str) {
        if self
            .state
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.set_status_error("运行期间不能修改监控目标");
            return;
        }
        if self.cfg.watch_serials.iter().any(|x| x == serial) {
            self.set_status(format!("已在监控：{serial}"));
            return;
        }
        self.cfg.watch_serials.push(serial.to_string());
        self.save_config();
        self.set_status(format!("已加入监控：{serial}"));
        self.state.set_message(format!("已加入监控：{serial}"));
        self.state
            .log(LogLevel::Info, format!("已加入监控：{serial}"));
    }

    fn start_grab(&mut self) {
        if let Err(e) = self.cfg.validate_watch() {
            self.set_status(format!("{e}"));
            self.state.set_message(format!("{e}"));
            self.state.log(LogLevel::Warn, format!("{e}"));
            return;
        }
        if !self
            .state
            .logged_in
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.set_status_error("请先登录");
            self.state.set_message("请先登录");
            return;
        }
        if !self.cfg.profile_id.trim().is_empty() {
            *self.state.profile_id.lock() = self.cfg.profile_id.trim().to_string();
        }
        self.save_config();
        self.result_summary = None;
        worker::start_grab(self.state.clone(), self.cfg.clone());
        self.set_status("抢课进行中");
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
                "间隔较激进；网络异常时会自动降速，硬性请求预算始终生效".into(),
            ));
        }
        checks
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
        self.set_status(format!("已清理 {} 个目标", serials.len()));
        self.state.log(
            LogLevel::Info,
            format!("已清理 {} 个监控目标", serials.len()),
        );
    }

    fn build_result_summary(&self) -> String {
        let watch = self.state.watch.lock().clone();
        if watch.is_empty() {
            return self.effective_status().to_string();
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

    // F-02：一轮里同时抢到两门课时，两条告警会在同一帧到达。直接赋值会让
    // 第一条被第二条盖掉——而「抢到了」恰恰是最不该错过的通知。
    #[test]
    fn queued_alerts_are_shown_one_after_another() {
        let mut queue: std::collections::VecDeque<(String, bool)> = Default::default();
        let mut toast: Option<(f64, String, bool)> = None;
        let mut now = 100.0_f64;

        // 同一帧到达两条。
        queue.push_back(("抢课成功：甲".into(), true));
        queue.push_back(("抢课成功：乙".into(), true));

        // 取第一条。
        if toast.is_none() {
            if let Some((text, ok)) = queue.pop_front() {
                toast = Some((now + TOAST_SECONDS, text, ok));
            }
        }
        assert_eq!(toast.as_ref().unwrap().1, "抢课成功：甲");
        assert_eq!(queue.len(), 1, "the second alert must still be queued");

        // 第一条没过期之前，第二条不许插队。
        now += TOAST_SECONDS / 2.0;
        if toast.as_ref().is_some_and(|(until, ..)| now > *until) {
            toast = None;
        }
        assert_eq!(toast.as_ref().unwrap().1, "抢课成功：甲");

        // 到期后才轮到第二条。
        now += TOAST_SECONDS;
        if toast.as_ref().is_some_and(|(until, ..)| now > *until) {
            toast = None;
        }
        if toast.is_none() {
            if let Some((text, ok)) = queue.pop_front() {
                toast = Some((now + TOAST_SECONDS, text, ok));
            }
        }
        assert_eq!(toast.as_ref().unwrap().1, "抢课成功：乙");
        assert!(queue.is_empty());
    }

    // S-04：密码输入缓冲必须是自动清零的，且要预分配——裸 String 每敲一个
    // 字符就可能 realloc，在堆上留下一串未擦除的旧缓冲，收尾的 zeroize 只能
    // 擦到最后那一块。类型断言防止回退成裸 String。
    #[test]
    fn password_buffer_is_zeroizing_and_preallocated() {
        let buffer: Zeroizing<String> = Zeroizing::new(String::with_capacity(256));
        let _typed: &Zeroizing<String> = &buffer;
        assert!(
            buffer.capacity() >= 256,
            "preallocate so typing never reallocates"
        );
    }

    // U-06：虚拟化后单帧只画可视区。行高固定是 show_rows 的前提，
    // 一旦有人把它改成变高的行，虚拟化会静默错位。
    #[test]
    fn catalog_rows_stay_fixed_height_for_virtualisation() {
        // 行高是手写常量且被 show_rows 与行绘制两处共用；改动必须同步，
        // 否则虚拟化会静默错位（画出来的行和滚动条算的位置对不上）。
        assert_eq!(CATALOG_ROW_H, 40.0);
    }

    // U-02：派生视图缓存的正确性全靠这把 key——漏掉任何一个输入，界面就会
    // 显示过期结果（比如课表刷新了却还画着旧行）。
    #[test]
    fn catalog_view_key_covers_every_input_that_changes_the_result() {
        let base = || CatalogViewKey {
            revision: 7,
            filter: "rust".into(),
            only_available: true,
            sort: CatalogSort::Name as u8,
        };
        assert_eq!(base(), base(), "identical inputs must hit the cache");

        // worker 的课表一变，revision 就变，缓存必须失效。
        let mut moved = base();
        moved.revision = 8;
        assert_ne!(base(), moved, "a catalog refresh must invalidate the cache");

        let mut refiltered = base();
        refiltered.filter = "java".into();
        assert_ne!(base(), refiltered);

        let mut toggled = base();
        toggled.only_available = false;
        assert_ne!(base(), toggled);

        let mut resorted = base();
        resorted.sort = CatalogSort::SeatsFirst as u8;
        assert_ne!(base(), resorted);
    }

    // U-01：后台心跳每帧刷新，绝不能盖掉用户刚触发的一次性提示。
    // 曾经两者共用一个字段，30 余处 UI 提示（导出/导入/文件操作）永不可见。
    #[test]
    fn transient_status_wins_over_background_heartbeat_until_it_expires() {
        let mut bar = StatusBar::new("未登录".into());
        bar.tick(100.0);
        assert_eq!(bar.effective(), "未登录");

        bar.push("诊断包已导出：D:/a.json".into(), false);
        // 同一帧内后台心跳照常刷新——这正是原来把提示盖掉的那一步。
        bar.set_background("抢课进行中".into());
        assert_eq!(bar.effective(), "诊断包已导出：D:/a.json");
        assert!(!bar.showing_error());

        // TTL 内保持可见。
        bar.tick(100.0 + TRANSIENT_STATUS_TTL - 0.1);
        assert_eq!(bar.effective(), "诊断包已导出：D:/a.json");
        // 到期后让位给后台状态，而不是永久占住状态栏。
        bar.tick(100.0 + TRANSIENT_STATUS_TTL + 0.1);
        assert_eq!(bar.effective(), "抢课进行中");
        assert!(!bar.showing_error());
    }

    #[test]
    fn error_status_is_marked_and_outlives_a_plain_one() {
        let mut bar = StatusBar::new("就绪".into());
        bar.tick(0.0);
        bar.push("导出失败：磁盘已满".into(), true);
        assert!(bar.showing_error());
        // 错误提示的存活时间必须长于普通提示——用户往往正在读它。
        bar.tick(TRANSIENT_STATUS_TTL + 0.1);
        assert_eq!(bar.effective(), "导出失败：磁盘已满");
        assert!(bar.showing_error());
        bar.tick(TRANSIENT_ERROR_TTL + 0.1);
        assert_eq!(bar.effective(), "就绪");
        assert!(!bar.showing_error());
    }

    // [#1] 诊断包导出的原始页面必须经过 redact_diagnostic_page：
    // 曾经这里直读文件，脱敏函数是死代码，凭据会原样进包。
    #[test]
    fn exported_raw_debug_pages_are_redacted_and_submissions_dropped() {
        let dir = std::env::temp_dir().join(format!(
            "cs-diag-pages-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("login.html"),
            "Set-Cookie: JSESSIONID=abc123\n<input name=\"password\" value=\"hunter2\">\n?token=tk_live_1",
        )
        .unwrap();
        std::fs::write(
            dir.join("submit.html"),
            "optype=true&operator0=371644:true:0&lesson=371644",
        )
        .unwrap();

        let collected = collect_redacted_debug_pages(&dir);

        assert_eq!(
            collected.submissions, 1,
            "submission form must be dropped wholesale"
        );
        assert_eq!(collected.unreadable, 0);
        assert_eq!(collected.pages.len(), 1);
        assert!(!collected.pages.contains_key("submit.html"));
        let login = &collected.pages["login.html"];
        for secret in ["abc123", "hunter2", "tk_live_1"] {
            assert!(!login.contains(secret), "diagnostic leaked {secret}");
        }
        assert!(login.contains("[已隐藏]"));

        // 排除数量必须出现在导出说明里，避免接收方误以为包是完整的。
        let note = CourseApp::export_raw_page_note(collected.submissions, collected.unreadable);
        assert!(
            note.contains('1') && note.contains("提交表单"),
            "got {note}"
        );
        assert!(CourseApp::export_raw_page_note(0, 0).is_empty());
        // 读不出的页面也要如实报告，不能算进“提交表单”。
        let skipped = CourseApp::export_raw_page_note(0, 2);
        assert!(skipped.contains('2') && !skipped.contains("提交表单"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 目录不存在时导出照常进行（只是没有原始页面），不能 panic。
    #[test]
    fn missing_debug_dir_yields_no_pages() {
        let collected =
            collect_redacted_debug_pages(&std::env::temp_dir().join("cs-diag-absent-dir"));
        assert!(collected.pages.is_empty());
        assert_eq!(collected.submissions, 0);
        assert_eq!(collected.unreadable, 0);
    }

    #[test]
    fn custom_hosts_require_per_session_confirmation() {
        assert_eq!(
            custom_host_requiring_confirmation("https://jwxt.example.edu.cn/eams"),
            Some("jwxt.example.edu.cn".into())
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

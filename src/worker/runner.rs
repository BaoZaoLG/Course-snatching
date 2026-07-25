use super::state::*;
use super::time::*;
use crate::config::AppConfig;
use crate::eams::{
    is_auth_error, is_rate_limit_error, rate_limit_retry_after, EamsClient, ElectResult, Lesson,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::runtime::Runtime;
use zeroize::Zeroizing;

pub struct LoginRequest {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub profile_preference: String,
    pub timeout: u64,
    pub auto_fetch: bool,
    pub debug_dump_enabled: bool,
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("Course-snatching-worker")
            .enable_all()
            .build()
            .expect("failed to create async runtime")
    })
}

fn spawn_task(future: impl std::future::Future<Output = ()> + Send + 'static) {
    drop(runtime().spawn(future));
}

struct ActivityGuard {
    state: Arc<SharedState>,
    activity: Activity,
    /// Set for Activity::Run so Drop only clears `running` if this task still owns it.
    run_generation: Option<u64>,
}

#[derive(Clone, Copy)]
enum Activity {
    Login,
    Refresh,
    Run,
}

impl ActivityGuard {
    fn new(state: Arc<SharedState>, activity: Activity) -> Self {
        Self {
            state,
            activity,
            run_generation: None,
        }
    }

    fn for_run(state: Arc<SharedState>, generation: u64) -> Self {
        Self {
            state,
            activity: Activity::Run,
            run_generation: Some(generation),
        }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        match self.activity {
            Activity::Login => self.state.logging_in.store(false, Ordering::Release),
            Activity::Refresh => self.state.refreshing.store(false, Ordering::Release),
            Activity::Run => {
                if let Some(generation) = self.run_generation {
                    self.state.release_run_if_owner(generation);
                }
            }
        }
        self.state.touch();
    }
}

pub fn login_and_fetch(state: Arc<SharedState>, req: LoginRequest) {
    let LoginRequest {
        base_url,
        username,
        password,
        profile_preference,
        timeout,
        auto_fetch,
        debug_dump_enabled,
    } = req;
    if state.logging_in.swap(true, Ordering::AcqRel) {
        state.log(LogLevel::Warn, "登录进行中，请稍候…");
        return;
    }
    state.logged_in.store(false, Ordering::Release);
    *state.client.lock() = None;
    state.lessons.lock().clear();
    state.set_message("正在登录…");
    state.log(
        LogLevel::Info,
        format!("登录中：{}", mask_account(&username)),
    );

    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Login);
        let password = Zeroizing::new(password);
        let client = match EamsClient::new(&base_url, timeout, debug_dump_enabled) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                state.log(LogLevel::Error, format!("初始化失败：{error:#}"));
                state.set_message("初始化失败");
                return;
            }
        };

        if let Err(error) = client.login(&username, password.as_str()).await {
            state.logged_in.store(false, Ordering::Release);
            if is_rate_limit_error(&error) {
                state.log(
                    LogLevel::Error,
                    format!("登录失败：{error:#}（请等待几秒后再试）"),
                );
                state.set_message("教务限流，请稍后再登录");
            } else {
                state.log(LogLevel::Error, format!("登录失败：{error:#}"));
                state.set_message("登录失败");
            }
            return;
        }

        state.logged_in.store(true, Ordering::Release);
        *state.client.lock() = Some(client.clone());
        state.log(LogLevel::Success, "登录成功");
        state.set_message("登录成功");

        let profile = if !profile_preference.trim().is_empty() {
            profile_preference.trim().to_string()
        } else {
            match client.list_profiles().await {
                Ok(profiles) if !profiles.is_empty() => {
                    for (id, note) in &profiles {
                        state.log(LogLevel::Info, format!("发现选课轮次 id={id}  {note}"));
                    }
                    profiles
                        .iter()
                        .find(|(_, note)| note.contains("进入选课") || note.contains("选课轮次"))
                        .or_else(|| profiles.first())
                        .map(|(id, _)| id.clone())
                        .unwrap_or_default()
                }
                Ok(_) => {
                    state.log(LogLevel::Warn, "未发现开放的选课轮次");
                    String::new()
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        state.clear_session("登录失效，请重新登录");
                        state.log(LogLevel::Error, format!("轮次探测失败：{error:#}"));
                        return;
                    }
                    state.log(LogLevel::Warn, format!("探测选课轮次失败：{error:#}"));
                    String::new()
                }
            }
        };

        if profile.is_empty() {
            state.set_message("已登录（未找到选课轮次，可在高级设置中填写）");
            return;
        }

        *state.profile_id.lock() = profile.clone();
        if profile == "0" {
            state.log(LogLevel::Info, "使用会话默认选课轮次");
        } else {
            state.log(LogLevel::Info, format!("使用选课轮次 profileId={profile}"));
        }

        if auto_fetch {
            state.set_message("正在拉取课程列表…");
            match client.fetch_lessons(&profile).await {
                Ok(list) => {
                    let count = list.len();
                    *state.lessons.lock() = list;
                    state.log(LogLevel::Success, format!("已拉取 {count} 门可选课程"));
                    state.set_message(format!("已登录，课程 {count} 门"));
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        state.clear_session("登录失效，请重新登录");
                    } else {
                        state.set_message("已登录（课程列表拉取失败）");
                    }
                    state.log(LogLevel::Warn, format!("拉取课程失败：{error:#}"));
                }
            }
        }
    });
}

pub fn refresh_lessons(state: Arc<SharedState>, profile_preference: String) {
    let client = state.client.lock().clone();
    let profile = if profile_preference.trim().is_empty() {
        state.profile_id.lock().clone()
    } else {
        profile_preference.trim().to_string()
    };

    let Some(client) = client else {
        state.log(LogLevel::Warn, "请先登录");
        state.set_message("请先登录");
        return;
    };
    if profile.is_empty() {
        state.log(LogLevel::Warn, "缺少选课轮次号");
        state.set_message("缺少选课轮次号");
        return;
    }
    if state.refreshing.swap(true, Ordering::AcqRel) {
        state.log(LogLevel::Info, "课程正在刷新，请稍候");
        return;
    }

    *state.profile_id.lock() = profile.clone();
    state.log(
        LogLevel::Info,
        format!("刷新课程，使用 profileId={profile}"),
    );
    state.set_message("正在刷新课程…");
    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Refresh);
        match client.fetch_lessons(&profile).await {
            Ok(list) => {
                let count = list.len();
                *state.lessons.lock() = list;
                state.log(LogLevel::Success, format!("刷新完成，共 {count} 门"));
                state.set_message(format!("刷新完成，共 {count} 门课"));
            }
            Err(error) => {
                if is_auth_error(&error) {
                    state.clear_session("登录失效，请重新登录");
                } else {
                    state.set_message(format!("刷新失败：{error}"));
                }
                state.log(LogLevel::Error, format!("刷新失败：{error:#}"));
            }
        }
    });
}

pub fn start_grab(state: Arc<SharedState>, cfg: AppConfig) {
    if let Err(error) = cfg.validate_watch() {
        state.log(LogLevel::Warn, error.to_string());
        state.set_message(error.to_string());
        return;
    }
    let Some(generation) = state.claim_run() else {
        state.log(LogLevel::Warn, "抢课进行中或正在停止，请稍候");
        return;
    };

    let client = state.client.lock().clone();
    let profile = if cfg.profile_id.trim().is_empty() {
        state.profile_id.lock().clone()
    } else {
        cfg.profile_id.trim().to_string()
    };
    let Some(client) = client else {
        state.release_run_if_owner(generation);
        state.log(LogLevel::Error, "请先登录");
        state.set_message("请先登录");
        return;
    };
    if profile.is_empty() {
        state.release_run_if_owner(generation);
        state.log(LogLevel::Error, "缺少选课轮次号");
        state.set_message("缺少选课轮次号");
        return;
    }

    *state.profile_id.lock() = profile.clone();
    let watch = cfg.cleaned_watch();
    *state.watch.lock() = watch
        .iter()
        .map(|serial| {
            let meta = cfg.watch_meta.get(serial);
            WatchStatus {
                serial: serial.clone(),
                name: meta.map(|m| m.name.clone()).unwrap_or_default(),
                teachers: meta.map(|m| m.teachers.clone()).unwrap_or_default(),
                state: WatchState::Queued,
                detail: "等待".into(),
                capacity: "-".into(),
                checks: 0,
                last_check: String::new(),
            }
        })
        .collect();

    state.log(LogLevel::Info, format!("开始抢课，目标 {} 门", watch.len()));
    state.set_message("抢课进行中");

    spawn_task(async move {
        let _guard = ActivityGuard::for_run(state.clone(), generation);
        let mut pending: HashSet<String> = watch.into_iter().collect();
        let mut succeeded = 0usize;
        let mut terminal_failures = 0usize;
        let mut consecutive_errors = 0u32;
        let mut consecutive_submission_errors = 0u32;
        let mut stopped_for_errors = false;
        let mut effective_interval = cfg.interval_seconds.max(0.05);
        let mut seat_open_prev: HashMap<String, bool> = HashMap::new();
        let burst_secs = cfg.open_burst_seconds.min(120);
        let burst_deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(burst_secs));
        let mut first_round = true;

        while state.is_current_run(generation) && !pending.is_empty() {
            let in_burst = burst_secs > 0 && tokio::time::Instant::now() < burst_deadline;
            if in_burst {
                // Sprint: stick to user interval (no adaptive slowdown from prior errors).
                effective_interval = cfg.interval_seconds.max(0.05);
            }

            let catalog = match client.fetch_lessons(&profile).await {
                Ok(list) => {
                    consecutive_errors = 0;
                    if cfg.adaptive_interval {
                        effective_interval =
                            (effective_interval * 0.92).max(cfg.interval_seconds.max(0.05));
                    }
                    *state.lessons.lock() = list.clone();
                    enrich_watch_from_lessons(&state, &list);
                    list
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        state.log(LogLevel::Error, "登录失效，抢课已停止");
                        state.clear_session("登录失效，请重新登录");
                        crate::notify::dispatch_alert(
                            "登录失效",
                            "抢课已停止，请重新登录",
                            false,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                        break;
                    }
                    consecutive_errors += 1;
                    if cfg.adaptive_interval
                        && (is_rate_limit_error(&error)
                            || error.to_string().contains("限流")
                            || error.to_string().contains("过快"))
                    {
                        let old = effective_interval;
                        effective_interval = (effective_interval * 1.6).min(30.0);
                        state.log(
                            LogLevel::Warn,
                            format!("检测到限流/过快，间隔 {old:.2}s → {effective_interval:.2}s"),
                        );
                    }
                    state.log(
                        LogLevel::Warn,
                        format!(
                            "刷新课程失败（连续 {consecutive_errors}/{} 次）：{error:#}",
                            cfg.max_consecutive_errors
                        ),
                    );
                    for serial in &pending {
                        update_watch(
                            &state,
                            serial,
                            WatchState::Failed,
                            format!("刷新失败：{error}"),
                            None,
                            None,
                        );
                    }
                    if consecutive_errors >= cfg.max_consecutive_errors {
                        stopped_for_errors = true;
                        state.set_message(format!("连续失败 {consecutive_errors} 次，已自动停止"));
                        crate::notify::dispatch_alert(
                            "已自动停止",
                            format!("连续失败 {consecutive_errors} 次"),
                            false,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                        break;
                    }
                    let delay = rate_limit_retry_after(&error)
                        .unwrap_or_else(|| error_backoff(effective_interval, consecutive_errors));
                    if rate_limit_retry_after(&error).is_some() {
                        state.set_message(format!(
                            "服务器限流，{:.1}s 后重试（Retry-After）",
                            delay.as_secs_f64()
                        ));
                    } else {
                        state.set_message(format!("网络异常，{:.1}s 后重试", delay.as_secs_f64()));
                    }
                    sleep_cancellable(&state, generation, delay).await;
                    continue;
                }
            };

            let mut by_serial: HashMap<String, Vec<Lesson>> = HashMap::new();
            let mut by_id: HashMap<String, Lesson> = HashMap::new();
            for lesson in catalog {
                by_id.insert(lesson.id.clone(), lesson.clone());
                by_serial
                    .entry(lesson.no.trim().to_string())
                    .or_default()
                    .push(lesson);
            }

            let mut serials: Vec<String> = {
                // Keep user priority order from config; pending is a set.
                let order = cfg.cleaned_watch();
                let mut ordered: Vec<String> =
                    order.into_iter().filter(|s| pending.contains(s)).collect();
                for s in pending.iter() {
                    if !ordered.iter().any(|o| o == s) {
                        ordered.push(s.clone());
                    }
                }
                ordered
            };
            if cfg.grab_seats_first {
                serials.sort_by(|a, b| {
                    let a_open = by_serial
                        .get(a)
                        .and_then(|rows| rows.first())
                        .is_some_and(Lesson::has_seat);
                    let b_open = by_serial
                        .get(b)
                        .and_then(|rows| rows.first())
                        .is_some_and(Lesson::has_seat);
                    b_open.cmp(&a_open)
                });
            }
            for serial in serials {
                if !state.is_current_run(generation) {
                    break;
                }
                state.set_message(format!("检查 {serial}"));
                bump_watch_check(&state, &serial);
                update_watch(&state, &serial, WatchState::Checking, "查询中", None, None);

                let lesson = if let Some(expected_id) = cfg.watch_lesson_ids.get(&serial) {
                    match by_id.get(expected_id) {
                        Some(lesson) if lesson.no.trim() == serial => lesson.clone(),
                        _ => {
                            let detail = "指定教学班已变化或不在当前轮次，请重新从课程列表加入";
                            update_watch(&state, &serial, WatchState::Failed, detail, None, None);
                            state.log(LogLevel::Error, format!("[{serial}] {detail}"));
                            pending.remove(&serial);
                            terminal_failures += 1;
                            continue;
                        }
                    }
                } else {
                    let matches = by_serial.get(&serial).map(Vec::as_slice).unwrap_or(&[]);
                    match matches {
                        [] => {
                            update_watch(
                                &state,
                                &serial,
                                WatchState::Missing,
                                "未找到",
                                None,
                                None,
                            );
                            state.log(LogLevel::Warn, format!("[{serial}] 未找到"));
                            continue;
                        }
                        [lesson] => lesson.clone(),
                        many => {
                            let detail =
                                format!("精确匹配到 {} 条，请从课程列表指定教学班", many.len());
                            update_watch(
                                &state,
                                &serial,
                                WatchState::Ambiguous,
                                &detail,
                                None,
                                None,
                            );
                            state.log(LogLevel::Error, format!("[{serial}] {detail}"));
                            pending.remove(&serial);
                            terminal_failures += 1;
                            continue;
                        }
                    }
                };

                let capacity = lesson.capacity_text();
                if !lesson.capacity_known() {
                    update_watch(
                        &state,
                        &serial,
                        WatchState::Unknown,
                        "暂未取得准确人数，等待下轮",
                        Some(capacity.clone()),
                        Some(&lesson),
                    );
                    state.log(LogLevel::Warn, format!("[{serial}] 暂未取得准确人数"));
                    continue;
                }
                if !lesson.has_seat() {
                    let was_open = seat_open_prev.get(&serial).copied().unwrap_or(false);
                    update_watch(
                        &state,
                        &serial,
                        WatchState::Full,
                        format!("已满 {capacity}"),
                        Some(capacity.clone()),
                        Some(&lesson),
                    );
                    if was_open || !cfg.monitor_only {
                        state.log(LogLevel::Info, format!("[{serial}] 已满 {capacity}"));
                    }
                    if cfg.monitor_only && was_open {
                        crate::notify::dispatch_alert(
                            "余量已满",
                            format!("{serial} · {} · {capacity}", lesson.name),
                            false,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                    }
                    seat_open_prev.insert(serial.clone(), false);
                    continue;
                }

                if cfg.monitor_only {
                    let was_open = seat_open_prev.get(&serial).copied().unwrap_or(false);
                    update_watch(
                        &state,
                        &serial,
                        WatchState::Checking,
                        format!("有余量 {capacity}（仅监控）"),
                        Some(capacity.clone()),
                        Some(&lesson),
                    );
                    if !was_open {
                        state.log(
                            LogLevel::Success,
                            format!("[{serial}] 有余量 {capacity}（仅监控，不提交）"),
                        );
                        crate::notify::dispatch_alert(
                            "发现余量",
                            format!("{serial} · {} · {capacity}", lesson.name),
                            true,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                    }
                    seat_open_prev.insert(serial.clone(), true);
                    continue;
                }

                state.log(
                    LogLevel::Info,
                    format!("[{serial}] 有余量 {capacity}，提交选课"),
                );
                update_watch(
                    &state,
                    &serial,
                    WatchState::Electing,
                    format!("提交 {capacity}"),
                    Some(capacity),
                    Some(&lesson),
                );

                match client
                    .elect_lesson(&profile, &lesson.id, lesson.seat.selected())
                    .await
                {
                    Ok(ElectResult::Success { detail }) => {
                        consecutive_submission_errors = 0;
                        state.log(LogLevel::Success, format!("[{serial}] 成功：{detail}"));
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Success,
                            detail.clone(),
                            Some(lesson.capacity_text()),
                            Some(&lesson),
                        );
                        pending.remove(&serial);
                        succeeded += 1;
                        crate::notify::dispatch_alert(
                            "抢课成功",
                            format!("{serial} · {} · {detail}", lesson.name),
                            true,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                    }
                    Ok(ElectResult::Full { detail }) => {
                        consecutive_submission_errors = 0;
                        state.log(LogLevel::Info, format!("[{serial}] 已满：{detail}"));
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Full,
                            detail,
                            Some(lesson.capacity_text()),
                            Some(&lesson),
                        );
                    }
                    Ok(ElectResult::Failed { detail }) => {
                        consecutive_submission_errors = 0;
                        state.log(LogLevel::Error, format!("[{serial}] 选课失败：{detail}"));
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Failed,
                            detail,
                            Some(lesson.capacity_text()),
                            Some(&lesson),
                        );
                        pending.remove(&serial);
                        terminal_failures += 1;
                    }
                    Err(error) => {
                        if is_auth_error(&error) {
                            state.log(LogLevel::Error, "登录失效，抢课已停止");
                            state.clear_session("登录失效，请重新登录");
                            break;
                        }
                        state.log(LogLevel::Error, format!("[{serial}] 提交异常：{error:#}"));
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Failed,
                            format!("提交异常：{error}"),
                            None,
                            Some(&lesson),
                        );
                        consecutive_submission_errors += 1;
                        if consecutive_submission_errors >= cfg.max_consecutive_errors {
                            stopped_for_errors = true;
                            state.set_message(format!(
                                "提交连续失败 {consecutive_submission_errors} 次，已自动停止"
                            ));
                            crate::notify::dispatch_alert(
                                "已自动停止",
                                format!("提交连续失败 {consecutive_submission_errors} 次"),
                                false,
                                cfg.notify_enabled,
                                cfg.sound_enabled,
                            );
                            break;
                        }
                    }
                }
            }

            if pending.is_empty() || !state.is_current_run(generation) {
                break;
            }
            // First round after start/schedule: no artificial wait before we already polled.
            // Between rounds: burst mode uses the configured interval without jitter.
            let in_burst = burst_secs > 0 && tokio::time::Instant::now() < burst_deadline;
            let delay = if first_round {
                first_round = false;
                Duration::from_millis(0)
            } else {
                poll_delay_for_mode(effective_interval, in_burst)
            };
            if !delay.is_zero() {
                state.set_message(format!(
                    "{}剩余 {} 门，{:.2}s 后继续",
                    if in_burst {
                        "冲刺中，"
                    } else {
                        "本轮结束，"
                    },
                    pending.len(),
                    delay.as_secs_f64()
                ));
                sleep_cancellable(&state, generation, delay).await;
            } else {
                state.set_message(format!("冲刺开抢，剩余 {} 门", pending.len()));
            }
        }

        if pending.is_empty() {
            if terminal_failures == 0 {
                state.log(
                    LogLevel::Success,
                    format!("全部目标完成，共 {succeeded} 门"),
                );
                state.set_message(format!("全部完成，共 {succeeded} 门"));
                crate::notify::dispatch_alert(
                    "全部完成",
                    format!("成功 {succeeded} 门"),
                    true,
                    cfg.notify_enabled,
                    cfg.sound_enabled,
                );
            } else {
                state.log(
                    LogLevel::Warn,
                    format!("任务结束：成功 {succeeded}，失败/跳过 {terminal_failures}"),
                );
                state.set_message(format!(
                    "任务结束：成功 {succeeded}，失败/跳过 {terminal_failures}"
                ));
                crate::notify::dispatch_alert(
                    "任务结束",
                    format!("成功 {succeeded}，失败/跳过 {terminal_failures}"),
                    succeeded > 0,
                    cfg.notify_enabled,
                    cfg.sound_enabled,
                );
            }
        } else if state.logged_in.load(Ordering::Acquire) {
            mark_pending_stopped(&state, &pending);
            if !stopped_for_errors && consecutive_errors < cfg.max_consecutive_errors {
                state.log(LogLevel::Info, "已停止");
                state.set_message("已停止");
            }
        }
    });
}

pub fn keepalive(state: Arc<SharedState>, notify_enabled: bool, sound_enabled: bool) {
    if !state.logged_in.load(Ordering::Acquire)
        || state.running.load(Ordering::Acquire)
        || state.logging_in.load(Ordering::Acquire)
        || state.refreshing.load(Ordering::Acquire)
    {
        return;
    }
    let client = state.client.lock().clone();
    let profile = state.profile_id.lock().clone();
    let Some(client) = client else {
        return;
    };
    if profile.is_empty() {
        return;
    }
    spawn_task(async move {
        match client.fetch_lessons(&profile).await {
            Ok(list) => {
                *state.lessons.lock() = list;
                state.touch();
            }
            Err(error) if is_auth_error(&error) => {
                state.clear_session("登录已过期，请重新登录");
                state.log(LogLevel::Warn, "会话保活发现登录失效");
                crate::notify::dispatch_alert(
                    "登录失效",
                    "请重新登录后继续",
                    false,
                    notify_enabled,
                    sound_enabled,
                );
            }
            Err(_) => {}
        }
    });
}

pub fn logout(state: &SharedState) {
    state.running.store(false, Ordering::Release);
    state.stopping.store(false, Ordering::Release);
    state.run_generation.fetch_add(1, Ordering::AcqRel);
    state.logged_in.store(false, Ordering::Release);
    *state.client.lock() = None;
    state.lessons.lock().clear();
    state.set_message("已退出登录");
    state.log(LogLevel::Info, "已退出登录");
}

/// Cancel any pending precise schedule arm.
pub fn cancel_schedule_arm(state: &SharedState) {
    state.schedule_arm_generation.fetch_add(1, Ordering::AcqRel);
}

/// Sleep until `target_local_secs` (UTC+8 civil seconds) then start grab with minimal wake latency.
pub fn arm_schedule(state: Arc<SharedState>, cfg: AppConfig, target_local_secs: i64) {
    if target_local_secs <= 0 {
        return;
    }
    let arm_gen = state.schedule_arm_generation.fetch_add(1, Ordering::AcqRel) + 1;
    let now = local_now_seconds();
    if now >= target_local_secs {
        // Already due — start immediately (caller should also guard fired-once).
        if !state.running.load(Ordering::Acquire) {
            start_grab(state, cfg);
        }
        return;
    }
    state.set_message(format!("定时待命中，目标 T{:+}s", target_local_secs - now));
    spawn_task(async move {
        loop {
            if state.schedule_arm_generation.load(Ordering::Acquire) != arm_gen {
                return;
            }
            if state.running.load(Ordering::Acquire) || state.logging_in.load(Ordering::Acquire) {
                return;
            }
            let now = local_now_seconds();
            if now >= target_local_secs {
                break;
            }
            let remain = target_local_secs - now;
            // Coarse far away, then tighten to ~2ms near the deadline.
            let sleep_ms: u64 = if remain > 30 {
                500
            } else if remain > 5 {
                50
            } else if remain > 1 {
                10
            } else {
                2
            };
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
        if state.schedule_arm_generation.load(Ordering::Acquire) != arm_gen {
            return;
        }
        if state.running.load(Ordering::Acquire) {
            return;
        }
        state.log(
            LogLevel::Info,
            format!(
                "精确定时触发（偏差约 {}ms 内）",
                ((local_now_seconds() - target_local_secs).abs() * 1000).min(999)
            ),
        );
        start_grab(state, cfg);
    });
}

pub fn stop_grab(state: &SharedState) {
    if !state.running.load(Ordering::Acquire) {
        return;
    }
    if state.stopping.swap(true, Ordering::AcqRel) {
        return;
    }
    // Invalidate the current run but leave `running` true until the worker guard drops.
    // This prevents a stop→immediate-start race from being clobbered by the old Drop.
    state.run_generation.fetch_add(1, Ordering::AcqRel);
    state.set_message("正在停止…");
    state.log(LogLevel::Info, "请求停止");
}

fn update_watch(
    state: &SharedState,
    serial: &str,
    watch_state: WatchState,
    detail: impl Into<String>,
    capacity: Option<String>,
    lesson: Option<&Lesson>,
) {
    let detail = detail.into();
    if let Some(item) = state
        .watch
        .lock()
        .iter_mut()
        .find(|item| item.serial == serial)
    {
        item.state = watch_state;
        item.detail = detail;
        if !matches!(watch_state, WatchState::Queued | WatchState::Stopped) {
            item.last_check = now_hms();
        }
        if let Some(capacity) = capacity {
            item.capacity = capacity;
        }
        if let Some(lesson) = lesson {
            if !lesson.name.is_empty() {
                item.name = lesson.name.clone();
            }
            if !lesson.teachers.is_empty() {
                item.teachers = lesson.teachers.clone();
            }
            item.capacity = lesson.capacity_text();
        }
    }
    state.touch();
}

fn bump_watch_check(state: &SharedState, serial: &str) {
    if let Some(item) = state
        .watch
        .lock()
        .iter_mut()
        .find(|item| item.serial == serial)
    {
        item.checks = item.checks.saturating_add(1);
        item.last_check = now_hms();
    }
}

fn enrich_watch_from_lessons(state: &SharedState, lessons: &[Lesson]) {
    let mut by_no: HashMap<&str, &Lesson> = HashMap::new();
    for lesson in lessons {
        by_no.entry(lesson.no.trim()).or_insert(lesson);
    }
    {
        let mut watch = state.watch.lock();
        for item in watch.iter_mut() {
            if let Some(lesson) = by_no.get(item.serial.as_str()) {
                if !lesson.name.is_empty() {
                    item.name = lesson.name.clone();
                }
                if !lesson.teachers.is_empty() {
                    item.teachers = lesson.teachers.clone();
                }
                if matches!(
                    item.state,
                    WatchState::Queued
                        | WatchState::Checking
                        | WatchState::Full
                        | WatchState::Unknown
                ) {
                    item.capacity = lesson.capacity_text();
                }
            }
        }
    }
    state.touch();
}

fn mark_pending_stopped(state: &SharedState, pending: &HashSet<String>) {
    let mut watch = state.watch.lock();
    for item in watch
        .iter_mut()
        .filter(|item| pending.contains(&item.serial))
    {
        if !matches!(item.state, WatchState::Success | WatchState::Failed) {
            item.state = WatchState::Stopped;
            item.detail = "已停止".into();
        }
    }
    state.touch();
}

/// `burst`: use the base interval without jitter during the open-course sprint window.
fn poll_delay_for_mode(interval_seconds: f64, burst: bool) -> Duration {
    let base = interval_seconds.clamp(0.05, 30.0);
    if burst {
        return Duration::from_secs_f64(base);
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    // ±10% jitter keeps steady-state traffic less robotic.
    let jitter = (f64::from(nanos % 2001) / 10_000.0) - 0.1;
    Duration::from_secs_f64((base * (1.0 + jitter)).clamp(0.05, 30.0))
}

fn error_backoff(interval_seconds: f64, consecutive_errors: u32) -> Duration {
    let factor = 2u64.saturating_pow(consecutive_errors.saturating_sub(1).min(5));
    Duration::from_secs_f64((interval_seconds * factor as f64).clamp(1.0, 30.0))
}

fn mask_account(username: &str) -> String {
    let chars: Vec<char> = username.chars().collect();
    match chars.len() {
        0 => String::new(),
        1 => "*".into(),
        2 => format!("{}*", chars[0]),
        _ => format!("{}***{}", chars[0], chars[chars.len() - 1]),
    }
}

async fn sleep_cancellable(state: &SharedState, generation: u64, duration: Duration) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if !state.is_current_run(generation) {
            return;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(100))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::eams::EamsClient;
    use crate::worker::time::{civil_from_days, now_hms};
    use crate::worker::{local_now_seconds, now_parts, now_stamp, SharedState};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn poll_delay_is_bounded() {
        for _ in 0..100 {
            let delay = poll_delay_for_mode(1.5, false).as_secs_f64();
            assert!((1.35..=1.65).contains(&delay));
        }
        for _ in 0..100 {
            let delay = poll_delay_for_mode(0.1, false).as_secs_f64();
            assert!((0.05..=0.12).contains(&delay), "got {delay}");
        }
        for _ in 0..20 {
            let burst = poll_delay_for_mode(0.1, true).as_secs_f64();
            assert!((0.05..=0.1).contains(&burst), "burst got {burst}");
        }
    }

    #[test]
    fn civil_from_days_known_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
    }
    #[test]
    fn now_helpers_are_well_formed() {
        assert_eq!(now_stamp().len(), 19);
        assert_eq!(now_hms().len(), 8);
        let parts = now_parts();
        assert!((1..=12).contains(&parts.1));
        assert!(local_now_seconds() > 0);
    }

    #[test]
    fn error_backoff_is_capped() {
        assert_eq!(error_backoff(1.0, 1), Duration::from_secs(1));
        assert_eq!(error_backoff(1.0, 2), Duration::from_secs(2));
        assert_eq!(error_backoff(10.0, 10), Duration::from_secs(30));
    }

    #[test]
    fn worker_completes_a_unique_target_end_to_end() {
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust',teachers:'张老师',stdCount:1,limitCount:2}];",
            "window.lessonId2Counts={'371644':{sc:1,lc:2}}",
            "选课成功",
            // post-elect verify: lesson gone from electable list
            "var lessonJSONs=[];",
        ]);
        let state = prepared_state(&base);
        let cfg = test_config(&base, "ABC.001");
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock();
        assert_eq!(watch.len(), 1);
        assert_eq!(watch[0].state, WatchState::Success);
        assert!(state.worker_message.lock().contains("全部完成"));
    }

    #[test]
    fn worker_uses_selected_lesson_id_when_serial_is_duplicated() {
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust A',teachers:'甲',stdCount:1,limitCount:2},{id:371645,no:'ABC.001',name:'Rust B',teachers:'乙',stdCount:1,limitCount:2}];",
            "window.lessonId2Counts={'371644':{sc:1,lc:2},'371645':{sc:1,lc:2}}",
            "选课成功",
            "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust A',teachers:'甲',stdCount:1,limitCount:2}];",
        ]);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "ABC.001");
        cfg.watch_lesson_ids
            .insert("ABC.001".into(), "371645".into());
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock();
        assert_eq!(watch[0].state, WatchState::Success);
        assert!(state.worker_message.lock().contains("全部完成"));
    }

    #[test]
    fn worker_never_reports_ambiguous_targets_as_complete() {
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust A',teachers:'甲',stdCount:1,limitCount:2},{id:371645,no:'ABC.001',name:'Rust B',teachers:'乙',stdCount:1,limitCount:2}];",
            "window.lessonId2Counts={'371644':{sc:1,lc:2},'371645':{sc:1,lc:2}}",
        ]);
        let state = prepared_state(&base);
        let cfg = test_config(&base, "ABC.001");
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock();
        assert_eq!(watch[0].state, WatchState::Ambiguous);
        let message = state.worker_message.lock().clone();
        assert!(message.contains("失败/跳过 1"));
        assert!(!message.contains("全部完成"));
    }

    #[test]
    fn mask_account_redacts_middle() {
        assert_eq!(mask_account(""), "");
        assert_eq!(mask_account("a"), "*");
        assert_eq!(mask_account("ab"), "a*");
        assert_eq!(mask_account("student01"), "s***1");
    }

    #[test]
    fn activity_guard_does_not_clear_newer_run() {
        let state = SharedState::new();
        let gen1 = state.claim_run().expect("first claim");
        {
            let guard = ActivityGuard::for_run(state.clone(), gen1);
            // Simulate stop then immediate restart owned by a newer generation.
            state.run_generation.fetch_add(1, Ordering::AcqRel);
            // Old task would still see running=true (Stopping); release then re-claim.
            state.release_run_if_owner(gen1);
            let gen2 = state.claim_run().expect("second claim");
            assert_ne!(gen1, gen2);
            assert!(state.running.load(Ordering::Acquire));
            drop(guard); // old guard must not clear the new run
            assert!(
                state.running.load(Ordering::Acquire),
                "newer run was cleared by old ActivityGuard drop"
            );
            assert_eq!(state.run_owner.load(Ordering::Acquire), gen2);
            state.release_run_if_owner(gen2);
        }
        assert!(!state.running.load(Ordering::Acquire));
    }

    #[test]
    fn stop_then_immediate_restart_keeps_new_run_alive() {
        // Two independent servers so response sequences never interleave.
        let base_slow = serve_sequence(vec![
            "<html>elect page</html>",
            "var lessonJSONs=[{id:371644,no:'SLOW.001',name:'Slow',teachers:'A',stdCount:2,limitCount:2}];",
            "window.lessonId2Counts={'371644':{sc:2,lc:2}}",
            // Keep first run busy if it polls again before stop fully settles.
            "var lessonJSONs=[{id:371644,no:'SLOW.001',name:'Slow',teachers:'A',stdCount:2,limitCount:2}];",
            "window.lessonId2Counts={'371644':{sc:2,lc:2}}",
        ]);
        let base_fast = serve_sequence(vec![
            "<html>elect page</html>",
            "var lessonJSONs=[{id:371645,no:'FAST.001',name:'Fast',teachers:'B',stdCount:1,limitCount:2}];",
            "window.lessonId2Counts={'371645':{sc:1,lc:2}}",
            "选课成功",
            "var lessonJSONs=[];",
        ]);

        let state = prepared_state(&base_slow);
        start_grab(state.clone(), test_config(&base_slow, "SLOW.001"));
        let start_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !state.running.load(Ordering::Acquire) && std::time::Instant::now() < start_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            state.running.load(Ordering::Acquire),
            "first run did not start"
        );

        stop_grab(&state);

        // Wait until Stopping clears, then restart against a fresh server/client.
        let stop_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while state.running.load(Ordering::Acquire) && std::time::Instant::now() < stop_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !state.running.load(Ordering::Acquire),
            "first run did not enter Idle after stop"
        );

        *state.client.lock() = Some(Arc::new(EamsClient::new(&base_fast, 5, false).unwrap()));
        start_grab(state.clone(), test_config(&base_fast, "FAST.001"));
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert!(
            watch
                .iter()
                .any(|w| w.serial == "FAST.001" && w.state == WatchState::Success),
            "expected FAST.001 success after restart, got: {watch:?}"
        );
    }

    #[test]
    fn monitor_only_does_not_submit_when_seat_open() {
        let data = "var lessonJSONs=[{id:371644,no:'MON.001',name:'Monitor',teachers:'甲',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let mut seq = vec!["<html>elect page</html>"];
        for _ in 0..16 {
            seq.push(data);
            seq.push(counts);
        }
        let base = serve_sequence(seq);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "MON.001");
        cfg.monitor_only = true;
        cfg.interval_seconds = 0.2;
        cfg.open_burst_seconds = 0; // steady polling for this test
        cfg.max_consecutive_errors = 5;
        start_grab(state.clone(), cfg);
        std::thread::sleep(Duration::from_millis(500));
        stop_grab(&state);
        wait_until_stopped(&state);
        let watch = state.watch.lock().clone();
        assert_eq!(watch.len(), 1);
        assert_ne!(watch[0].state, WatchState::Success);
        assert!(
            watch[0].detail.contains("仅监控")
                || watch[0].detail.contains("余量")
                || matches!(
                    watch[0].state,
                    WatchState::Checking
                        | WatchState::Stopped
                        | WatchState::Full
                        | WatchState::Queued
                ),
            "unexpected watch: {:?}",
            watch[0]
        );
        let logs = state
            .logs
            .lock()
            .iter()
            .map(|l| l.message.clone())
            .collect::<Vec<_>>();
        assert!(
            logs.iter().any(|m| m.contains("仅监控")),
            "expected monitor-only log, got {logs:?}"
        );
    }

    fn prepared_state(base: &str) -> Arc<SharedState> {
        let state = SharedState::new();
        state.logged_in.store(true, Ordering::Release);
        *state.profile_id.lock() = "0".into();
        *state.client.lock() = Some(Arc::new(EamsClient::new(base, 5, false).unwrap()));
        state
    }

    fn test_config(base: &str, serial: &str) -> AppConfig {
        AppConfig {
            base_url: base.into(),
            profile_id: "0".into(),
            watch_serials: vec![serial.into()],
            interval_seconds: 0.5,
            timeout_seconds: 5,
            max_consecutive_errors: 2,
            ..Default::default()
        }
    }

    fn wait_until_stopped(state: &SharedState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !state.running.load(Ordering::Acquire),
            "worker did not stop in time"
        );
    }

    fn serve_sequence(responses: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}/eams")
    }
}

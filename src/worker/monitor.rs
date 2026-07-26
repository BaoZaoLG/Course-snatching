use super::runtime::{spawn_task, ActivityGuard, BurstModeGuard};
use super::state::*;
use super::time::*;
use crate::config::AppConfig;
use crate::eams::{
    backend_error_kind, is_auth_error, BackendErrorKind, CircuitStatus, ElectResult, Lesson,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
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
        let _burst_guard = BurstModeGuard::new(client.clone());
        let mut pending: HashSet<String> = watch.into_iter().collect();
        let mut succeeded = 0usize;
        let mut terminal_failures = 0usize;
        let mut consecutive_refresh_failures = 0u32;
        let mut consecutive_network_failures = 0u32;
        let mut consecutive_submission_errors = 0u32;
        let mut stopped_for_errors = false;
        let requested_interval = cfg.interval_seconds.max(0.05);
        let mut effective_interval = requested_interval;
        // Only relax a previously increased interval after a stable sequence
        // of complete refreshes, preventing fast/slow oscillation.
        let mut consecutive_successful_refreshes = 0u32;
        let mut seat_open_prev: HashMap<String, bool> = HashMap::new();
        let burst_secs = cfg.open_burst_seconds.min(120);
        let burst_deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(burst_secs));
        // A circuit-breaker trip ends this sprint permanently; after its
        // cooldown the worker resumes ordinary monitoring instead.
        let mut burst_aborted_by_circuit = false;

        while state.is_current_run(generation) && !pending.is_empty() {
            let round_started = tokio::time::Instant::now();
            if client.circuit_is_open() {
                burst_aborted_by_circuit = true;
            }
            let in_burst = burst_secs > 0
                && !burst_aborted_by_circuit
                && tokio::time::Instant::now() < burst_deadline;
            client.set_burst_mode(in_burst);

            let catalog = match client.fetch_lessons_for_monitoring(&profile).await {
                Ok((list, complete_refresh)) => {
                    consecutive_refresh_failures = 0;
                    if complete_refresh {
                        consecutive_network_failures = 0;
                        consecutive_successful_refreshes =
                            consecutive_successful_refreshes.saturating_add(1);
                        effective_interval = recovered_interval_after_success(
                            effective_interval,
                            requested_interval,
                            consecutive_successful_refreshes,
                            cfg.adaptive_interval,
                        );
                    } else {
                        consecutive_successful_refreshes = 0;
                        consecutive_network_failures = client.network_snapshot().consecutive_errors;
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
                    consecutive_refresh_failures = consecutive_refresh_failures.saturating_add(1);
                    let reported_kind = backend_error_kind(&error);
                    let error_text = format!("{error:#}");
                    let error_kind = worker_error_kind(reported_kind, &error_text);
                    let is_network_failure = is_network_error(error_kind);
                    if is_network_failure {
                        consecutive_network_failures =
                            consecutive_network_failures.saturating_add(1);
                    } else {
                        consecutive_network_failures = 0;
                    }
                    consecutive_successful_refreshes = 0;
                    if cfg.adaptive_interval && is_network_failure {
                        let old = effective_interval;
                        effective_interval = (effective_interval * 1.6).min(30.0);
                        state.log(
                            LogLevel::Warn,
                            format!(
                                "检测到{}，间隔 {old:.2}s → {effective_interval:.2}s",
                                error_kind.label()
                            ),
                        );
                    }
                    let network = client.network_snapshot();
                    let circuit_protecting = circuit_protects(network.circuit_status);
                    if circuit_protecting {
                        burst_aborted_by_circuit = true;
                        client.set_burst_mode(false);
                    }
                    state.log(
                        LogLevel::Warn,
                        format!(
                            "刷新课程失败（{}，连续刷新失败 {consecutive_refresh_failures}/{}，连续网络异常 {consecutive_network_failures}）：{error:#}",
                            error_kind.label(),
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
                    // 网络瞬态由不可绕过的 governor 冷却/熔断处理；旧的停止阈值
                    // 继续约束解析或未知程序错误，避免页面改版后无限空转。
                    if !is_network_failure
                        && consecutive_refresh_failures >= cfg.max_consecutive_errors
                    {
                        stopped_for_errors = true;
                        state.set_message(format!(
                            "连续刷新失败 {consecutive_refresh_failures} 次，已自动停止"
                        ));
                        crate::notify::dispatch_alert(
                            "已自动停止",
                            format!("连续刷新失败 {consecutive_refresh_failures} 次"),
                            false,
                            cfg.notify_enabled,
                            cfg.sound_enabled,
                        );
                        break;
                    }
                    // User cadence is measured from round start. Server cooldown is
                    // measured from the response and must not be shortened.
                    let requested_period =
                        Duration::from_secs_f64(effective_interval.clamp(0.05, 30.0));
                    let user_wait = requested_period.saturating_sub(round_started.elapsed());
                    let compatibility_backoff = if reported_kind == BackendErrorKind::Unknown
                        && error_kind == BackendErrorKind::RateLimited
                    {
                        fixed_network_backoff(consecutive_network_failures)
                    } else {
                        Duration::ZERO
                    };
                    let delay = user_wait
                        .max(network.cooldown_remaining)
                        .max(compatibility_backoff);
                    if error_kind == BackendErrorKind::RateLimited
                        && !network.cooldown_remaining.is_zero()
                    {
                        state.set_message(format!(
                            "服务器限流，{:.1}s 后重试（服务器冷却）",
                            delay.as_secs_f64()
                        ));
                    } else if burst_aborted_by_circuit {
                        state.set_message(format!(
                            "网络保护已启用，{:.1}s 后以普通监控继续",
                            delay.as_secs_f64()
                        ));
                    } else if is_network_failure {
                        state.set_message(format!("网络异常，{:.1}s 后重试", delay.as_secs_f64()));
                    } else {
                        state.set_message(format!("刷新异常，{:.1}s 后重试", delay.as_secs_f64()));
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
                    // 瞬态繁忙/结果存疑：非终态，目标保留在 pending 中下一轮重试。
                    Ok(ElectResult::Busy { detail }) => {
                        consecutive_submission_errors = 0;
                        state.log(
                            LogLevel::Warn,
                            format!("[{serial}] 服务器繁忙或结果待确认，下轮重试：{detail}"),
                        );
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Checking,
                            format!("繁忙待重试：{detail}"),
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
                        let reported_kind = backend_error_kind(&error);
                        let error_text = format!("{error:#}");
                        let error_kind = worker_error_kind(reported_kind, &error_text);
                        let network_failure = is_network_error(error_kind);
                        state.log(
                            LogLevel::Error,
                            format!("[{serial}] 提交异常（{}）：{error:#}", error_kind.label()),
                        );
                        update_watch(
                            &state,
                            &serial,
                            WatchState::Failed,
                            format!("提交异常：{error}"),
                            None,
                            Some(&lesson),
                        );
                        consecutive_submission_errors =
                            consecutive_submission_errors.saturating_add(1);
                        if circuit_protects(client.network_snapshot().circuit_status) {
                            burst_aborted_by_circuit = true;
                            client.set_burst_mode(false);
                        }
                        if !network_failure
                            && consecutive_submission_errors >= cfg.max_consecutive_errors
                        {
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
            // The first request starts immediately; every subsequent round honours
            // the expected period, counting governor/request time already elapsed.
            let in_burst = burst_secs > 0
                && !burst_aborted_by_circuit
                && !client.circuit_is_open()
                && tokio::time::Instant::now() < burst_deadline;
            let desired_period = poll_delay_for_mode(effective_interval, in_burst);
            let delay = desired_period.saturating_sub(round_started.elapsed());
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
            if !stopped_for_errors {
                state.log(LogLevel::Info, "已停止");
                state.set_message("已停止");
            }
        }
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
    // 0–10% positive jitter keeps traffic less robotic without shortening the
    // user's requested polling period.
    let jitter = f64::from(nanos % 1001) / 10_000.0;
    Duration::from_secs_f64((base * (1.0 + jitter)).clamp(0.05, 30.0))
}

fn worker_error_kind(reported: BackendErrorKind, message: &str) -> BackendErrorKind {
    if reported == BackendErrorKind::Unknown
        && ["限流", "过快", "太快", "频繁", "稍后"]
            .iter()
            .any(|marker| message.contains(marker))
    {
        BackendErrorKind::RateLimited
    } else {
        reported
    }
}

fn is_network_error(kind: BackendErrorKind) -> bool {
    matches!(
        kind,
        BackendErrorKind::RateLimited
            | BackendErrorKind::Timeout
            | BackendErrorKind::Server
            | BackendErrorKind::Transport
    )
}

fn circuit_protects(status: CircuitStatus) -> bool {
    matches!(status, CircuitStatus::Open | CircuitStatus::HalfOpen)
}

fn recovered_interval_after_success(
    current: f64,
    requested: f64,
    consecutive_successes: u32,
    enabled: bool,
) -> f64 {
    if enabled && consecutive_successes >= 5 && current > requested {
        (current * 0.92).max(requested)
    } else {
        current
    }
}

fn fixed_network_backoff(consecutive_failures: u32) -> Duration {
    const DELAYS: [Duration; 5] = [
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(30),
    ];
    DELAYS[consecutive_failures.saturating_sub(1).min(4) as usize]
}

#[cfg(test)]
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
            assert!((1.5..=1.65).contains(&delay));
        }
        for _ in 0..100 {
            let delay = poll_delay_for_mode(0.1, false).as_secs_f64();
            assert!((0.1..=0.11).contains(&delay), "got {delay}");
        }
        for _ in 0..20 {
            let burst = poll_delay_for_mode(0.1, true).as_secs_f64();
            assert!((0.099..=0.101).contains(&burst), "burst got {burst}");
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
    fn interval_recovers_only_after_five_complete_refreshes() {
        let requested = 1.0;
        let slowed = 8.0;
        assert_eq!(
            recovered_interval_after_success(slowed, requested, 4, true),
            slowed
        );
        let recovered = recovered_interval_after_success(slowed, requested, 5, true);
        assert!(recovered < slowed && recovered >= requested);
        assert_eq!(
            recovered_interval_after_success(slowed, requested, 10, false),
            slowed
        );
    }

    #[test]
    fn typed_error_wins_and_only_unknown_text_uses_rate_limit_fallback() {
        assert_eq!(
            worker_error_kind(BackendErrorKind::Business, "业务失败：请求过快"),
            BackendErrorKind::Business
        );
        assert_eq!(
            worker_error_kind(BackendErrorKind::Unknown, "请不要过快点击"),
            BackendErrorKind::RateLimited
        );
        assert_eq!(
            worker_error_kind(BackendErrorKind::Unknown, "页面结构未知"),
            BackendErrorKind::Unknown
        );
    }

    #[test]
    fn circuit_protection_ends_burst_without_classifying_business_results_as_network() {
        assert!(circuit_protects(CircuitStatus::Open));
        assert!(circuit_protects(CircuitStatus::HalfOpen));
        assert!(!circuit_protects(CircuitStatus::Closed));
        assert!(!is_network_error(BackendErrorKind::Business));
        assert!(is_network_error(BackendErrorKind::RateLimited));
    }

    #[test]
    fn fallback_network_backoff_uses_documented_sequence() {
        assert_eq!(fixed_network_backoff(1), Duration::from_secs(2));
        assert_eq!(fixed_network_backoff(2), Duration::from_secs(4));
        assert_eq!(fixed_network_backoff(3), Duration::from_secs(8));
        assert_eq!(fixed_network_backoff(4), Duration::from_secs(16));
        assert_eq!(fixed_network_backoff(5), Duration::from_secs(30));
        assert_eq!(fixed_network_backoff(99), Duration::from_secs(30));
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
    fn worker_retries_after_transient_busy_submission() {
        let data = "var lessonJSONs=[{id:371644,no:'BSY.001',name:'Busy',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            data,
            counts,
            // 第一轮提交：瞬态繁忙，目标必须保留待下一轮
            "系统繁忙，请稍后再试",
            data,
            counts,
            "选课成功",
            // post-elect verify: empty catalog is inconclusive but still success
            "var lessonJSONs=[];",
        ]);
        let state = prepared_state(&base);
        let cfg = test_config(&base, "BSY.001");
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch.len(), 1);
        assert_eq!(watch[0].state, WatchState::Success);
        assert!(state.worker_message.lock().contains("全部完成"));
        let logs = state
            .logs
            .lock()
            .iter()
            .map(|l| l.message.clone())
            .collect::<Vec<_>>();
        assert!(
            logs.iter().any(|m| m.contains("下轮重试")),
            "expected busy retry log, got {logs:?}"
        );
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

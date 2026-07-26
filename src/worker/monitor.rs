use super::runtime::{spawn_task, ActivityGuard, BurstModeGuard};
use super::state::*;
use super::time::*;
use crate::config::AppConfig;
use crate::eams::{
    backend_error_kind, is_auth_error, BackendErrorKind, CircuitStatus, EamsClient, ElectResult,
    Lesson,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// 指定教学班在目录中连续缺失该轮数后才终态放弃：单轮缺失可能只是
/// 服务器抖动或兜底解析得到的子集，直接放弃会在最关键时刻丢掉目标。
const MAX_ID_MISSING_ROUNDS: u32 = 5;
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
        // 本次 run 内连续的自动重登失败次数；成功一次即清零。
        let mut relogin_failures = 0u32;
        let mut stopped_for_errors = false;
        let requested_interval = cfg.interval_seconds.max(0.05);
        let mut effective_interval = requested_interval;
        // Only relax a previously increased interval after a stable sequence
        // of complete refreshes, preventing fast/slow oscillation.
        let mut consecutive_successful_refreshes = 0u32;
        let mut seat_open_prev: HashMap<String, bool> = HashMap::new();
        // 指定教学班连续未在目录中出现的轮数（见 MAX_ID_MISSING_ROUNDS）。
        let mut id_missing_rounds: HashMap<String, u32> = HashMap::new();
        let burst_secs = cfg.open_burst_seconds.min(120);
        let burst_deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(burst_secs));
        // A circuit-breaker trip ends this sprint permanently; after its
        // cooldown the worker resumes ordinary monitoring instead.
        let mut burst_aborted_by_circuit = false;

        'run: while state.is_current_run(generation) && !pending.is_empty() {
            let round_started = tokio::time::Instant::now();
            if client.circuit_is_open() {
                burst_aborted_by_circuit = true;
            }
            let in_burst = burst_secs > 0
                && !burst_aborted_by_circuit
                && tokio::time::Instant::now() < burst_deadline;
            client.set_burst_mode(in_burst);

            // 停止请求必须能穿透 governor 等待与在途 HTTP：熔断/限流冷却
            // 可达分钟级，取消只影响本地等待，不影响服务器端已收到的请求。
            let fetch_result = tokio::select! {
                biased;
                () = run_cancelled(&state, generation) => break,
                result = client.fetch_lessons_for_monitoring(&profile) => result,
            };
            // 换版预警：解析策略降级通常先于彻底解析失败几天发生。
            for notice in client.take_strategy_notices() {
                state.log(LogLevel::Warn, notice);
            }
            let catalog = match fetch_result {
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
                        if !confirm_auth_expired(&state, generation, &client).await {
                            if !state.is_current_run(generation) {
                                break;
                            }
                            state.log(LogLevel::Warn, "检测到疑似登录失效，复核未确认，继续监控");
                            sleep_cancellable(
                                &state,
                                generation,
                                Duration::from_secs_f64(effective_interval.clamp(0.05, 30.0)),
                            )
                            .await;
                            continue;
                        }
                        // 先试自动重登：这是一个「设定时开抢、半夜挂机」的
                        // 工具，会话在等待期间过期是必然事件而非异常。
                        if attempt_relogin(&state, &client, &mut relogin_failures).await {
                            continue;
                        }
                        state.log(LogLevel::Error, "登录失效，抢课已停止");
                        // 先终态化看板再 clear_session：后者会立刻把 running 置 false，
                        // 界面此刻就可能读到状态，不能让「提交中/检查中」的行在
                        // “已停止”之后还挂着。
                        mark_pending_stopped(&state, &pending);
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
            // 检测段只判断「该不该提交」，把要提交的目标攒起来；
            // 提交段在循环之后统一并发发出。
            let mut ready: Vec<(String, Lesson)> = Vec::new();
            for serial in serials {
                if !state.is_current_run(generation) {
                    break;
                }
                state.set_message(format!("检查 {serial}"));
                bump_watch_check(&state, &serial);
                update_watch(&state, &serial, WatchState::Checking, "查询中", None, None);

                let lesson = if let Some(expected_id) = cfg.watch_lesson_ids.get(&serial) {
                    match by_id.get(expected_id) {
                        Some(lesson) if lesson.no.trim() == serial => {
                            id_missing_rounds.remove(&serial);
                            lesson.clone()
                        }
                        Some(_) => {
                            // id 仍在目录中但序号对不上：教学班确实已变化，终态失败。
                            let detail = "指定教学班已变化或不在当前轮次，请重新从课程列表加入";
                            update_watch(&state, &serial, WatchState::Failed, detail, None, None);
                            state.log(LogLevel::Error, format!("[{serial}] {detail}"));
                            pending.remove(&serial);
                            terminal_failures += 1;
                            continue;
                        }
                        None => {
                            // 单轮目录缺失可能只是服务器抖动或 HTML 兜底解析出的
                            // 子集：按 Missing 重试（与按序号路径一致），连续多轮
                            // 仍未刷到才终结，避免瞬态缺失葬送唯一目标。
                            let missing = *id_missing_rounds
                                .entry(serial.clone())
                                .and_modify(|count| *count = count.saturating_add(1))
                                .or_insert(1);
                            if missing >= MAX_ID_MISSING_ROUNDS {
                                let detail = format!(
                                    "指定教学班连续 {MAX_ID_MISSING_ROUNDS} 轮未刷到，可能已撤销，请重新从课程列表加入"
                                );
                                update_watch(
                                    &state,
                                    &serial,
                                    WatchState::Failed,
                                    detail.clone(),
                                    None,
                                    None,
                                );
                                state.log(LogLevel::Error, format!("[{serial}] {detail}"));
                                pending.remove(&serial);
                                terminal_failures += 1;
                                continue;
                            }
                            update_watch(
                                &state,
                                &serial,
                                WatchState::Missing,
                                format!(
                                    "本轮未刷到（{missing}/{MAX_ID_MISSING_ROUNDS}），继续重试"
                                ),
                                None,
                                None,
                            );
                            state.log(
                                LogLevel::Warn,
                                format!(
                                    "[{serial}] 指定教学班本轮未刷到（{missing}/{MAX_ID_MISSING_ROUNDS}），继续重试"
                                ),
                            );
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

                ready.push((serial.clone(), lesson));
            }

            // ── 提交段：检测与提交解耦，有界并发 ──────────────────────
            //
            // 原实现一轮里 N 个目标严格顺序提交，且底层闸门是单许可信号量——
            // 即使上层改成并发也会被重新串行化。教务高峰 RTT 常在 300–800ms，
            // 第 5 个目标的 POST 要晚 1.5–4 秒才发出，而抢课是零和的先到先得，
            // 这个延迟直接决定成败。
            //
            // 并发度由 governor 的 submission_gate 决定（默认 3），令牌桶与
            // 熔断在其下兜底：放开的是「同时在途的提交数」，不是速率上限。
            // 结果按原优先级顺序回放，保证日志与看板的顺序仍然可预期。
            let confirm_mode = if tokio::time::Instant::now() < burst_deadline
                && burst_secs > 0
                && !burst_aborted_by_circuit
            {
                crate::eams::ConfirmMode::Optimistic
            } else {
                crate::eams::ConfirmMode::Verify
            };
            let mut submitted: Vec<(String, Lesson, anyhow::Result<ElectResult>)> = Vec::new();
            if !ready.is_empty() {
                let mut set = tokio::task::JoinSet::new();
                for (index, (serial, lesson)) in ready.into_iter().enumerate() {
                    let client = client.clone();
                    let profile = profile.clone();
                    set.spawn(async move {
                        let result = client
                            .elect_lesson(
                                &profile,
                                &lesson.id,
                                lesson.seat.selected(),
                                confirm_mode,
                            )
                            .await;
                        (index, serial, lesson, result)
                    });
                }
                // 停止请求必须能穿透整批在途提交，而不是等它们各自跑完。
                submitted = tokio::select! {
                    biased;
                    () = run_cancelled(&state, generation) => {
                        set.abort_all();
                        break 'run;
                    }
                    collected = collect_submissions(&mut set) => collected,
                };
            }

            for (serial, lesson, elect_result) in submitted {
                match elect_result {
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
                            if confirm_auth_expired(&state, generation, &client).await {
                                // 提交阶段掉线同样先试自动重登；目标全部保留在
                                // pending 里，重登成功后下一轮继续。
                                if attempt_relogin(&state, &client, &mut relogin_failures).await {
                                    continue;
                                }
                                state.log(LogLevel::Error, "登录失效，抢课已停止");
                                // 同刷新路径：clear_session 会立刻置 running=false，
                                // 看板必须在那之前终态化。
                                mark_pending_stopped(&state, &pending);
                                state.clear_session("登录失效，请重新登录");
                                // 与刷新路径一致：提交阶段掉线正是最需要提醒的时刻。
                                crate::notify::dispatch_alert(
                                    "登录失效",
                                    "提交阶段登录失效，抢课已停止，请重新登录",
                                    false,
                                    cfg.notify_enabled,
                                    cfg.sound_enabled,
                                );
                                break 'run;
                            }
                            if !state.is_current_run(generation) {
                                break 'run;
                            }
                            // 复核未确认失效：本次提交结果未知，保留目标下轮复核。
                            state.log(
                                LogLevel::Warn,
                                format!("[{serial}] 提交时疑似登录失效，复核未确认，下轮重试"),
                            );
                            update_watch(
                                &state,
                                &serial,
                                WatchState::Checking,
                                "登录复核正常，下轮重试",
                                Some(lesson.capacity_text()),
                                Some(&lesson),
                            );
                            continue;
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
                            // 必须终止整个 run：普通 break 只会跳出本轮的
                            // for 循环，pending 非空时 while 会继续下一轮。
                            break 'run;
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
            // 冲刺结束后线性回落，而不是断崖式切回常规间隔。
            let burst_progress = if in_burst {
                0.0
            } else if burst_secs == 0 || burst_aborted_by_circuit {
                1.0
            } else {
                let since_end = tokio::time::Instant::now()
                    .saturating_duration_since(burst_deadline)
                    .as_secs_f64();
                (since_end / BURST_RAMP_SECS).clamp(0.0, 1.0)
            };
            let desired_period = poll_delay_for_mode(
                effective_interval,
                cfg.burst_interval_seconds,
                burst_progress,
            );
            // 提交路径设置的服务器冷却也在轮末等待中兑现：让等待可见、
            // 可取消，而不是下一轮请求在 acquire 里被无提示地阻塞。
            let cooldown_wait = client.network_snapshot().cooldown_remaining;
            let delay = desired_period
                .saturating_sub(round_started.elapsed())
                .max(cooldown_wait);
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
        } else {
            // 登录失效/登出也会走到这里：看板中未终态的行必须标记为已停止，
            // 否则“提交中”等状态会永久停留；状态消息则保持掉线提示不被覆盖。
            mark_pending_stopped(&state, &pending);
            if !stopped_for_errors && state.logged_in.load(Ordering::Acquire) {
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
    // 上面两步之间 run 可能恰好自然结束（guard drop 已清 running/stopping）。
    // 此时必须回滚 stopping，否则 UI 会一直显示“正在停止”。
    if !state.running.load(Ordering::Acquire) {
        state.stopping.store(false, Ordering::Release);
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
/// 冲刺结束后回落到常规间隔所用的秒数。
///
/// 不硬切：速率断崖（10 rps 瞬间掉到 0.7 rps）比平滑回落更容易被服务端
/// 的行为风控识别出来。
const BURST_RAMP_SECS: f64 = 3.0;

/// 计算下一轮的轮询周期。
///
/// `burst_progress` 是从冲刺回落到常规的进度：0.0 表示仍在冲刺窗口内，
/// 1.0 表示已完全回到用户设定的间隔。
///
/// 冲刺此前唯一的差别是去掉 0–10% 的正抖动，即最多快 10%——默认 1.5s 间隔下，
/// 20 秒的「冲刺窗口」只轮询约 13 次，令牌桶的 10 rps 上限永远碰不到，
/// 整段限速余量被浪费。现在冲刺有独立的（更短的）间隔，令牌桶才真正成为
/// 限流点。
fn poll_delay_for_mode(
    interval_seconds: f64,
    burst_interval_seconds: f64,
    burst_progress: f64,
) -> Duration {
    let interval = interval_seconds.clamp(0.05, 30.0);
    // 冲刺间隔不得慢于常规间隔，否则「冲刺」反而是减速。
    let burst_interval = burst_interval_seconds.clamp(0.05, 30.0).min(interval);
    let progress = burst_progress.clamp(0.0, 1.0);
    let base = burst_interval + (interval - burst_interval) * progress;
    // 抖动只向正方向：绝不比用户设定的间隔更快。真正的限速由令牌桶承担，
    // 这里的抖动只为让节奏不那么机械。冲刺期幅度更小，别浪费窗口。
    let fraction = if progress < 1.0 { 0.03 } else { 0.10 };
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let jitter = f64::from(nanos % 1001) / 1000.0 * fraction;
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

/// 会话过期后尝试自动重登。返回 true 表示已恢复，调用方应继续本次 run。
///
/// 没有保留凭据（用户关掉了开关）或连续失败达上限时返回 false，由调用方
/// 走原来的停机路径——自动重登是尽力而为，绝不能变成无限重试。
async fn attempt_relogin(
    state: &Arc<SharedState>,
    client: &Arc<crate::eams::EamsClient>,
    failures: &mut u32,
) -> bool {
    if *failures >= super::session::MAX_RELOGIN_ATTEMPTS {
        return false;
    }
    *failures += 1;
    match super::session::try_relogin(state, client, *failures).await {
        None => {
            state.log(
                LogLevel::Warn,
                "会话已过期，且未保留本次会话凭据，无法自动重登（可在高级设置中开启）",
            );
            false
        }
        Some(Ok(())) => {
            *failures = 0;
            state.logged_in.store(true, Ordering::Release);
            state.log(LogLevel::Success, "自动重新登录成功，继续抢课");
            state.set_message("已自动重新登录，继续抢课");
            true
        }
        Some(Err(error)) => {
            state.log(LogLevel::Error, format!("自动重新登录失败：{error:#}"));
            false
        }
    }
}

/// 收齐一轮的并发提交结果，并按原优先级顺序回放。
///
/// 顺序很重要：日志、看板与「连续失败计数」都按目标优先级读起来才可预期，
/// 不该因为哪个请求先回来而变。
async fn collect_submissions(
    set: &mut tokio::task::JoinSet<(usize, String, Lesson, anyhow::Result<ElectResult>)>,
) -> Vec<(String, Lesson, anyhow::Result<ElectResult>)> {
    let mut collected = Vec::new();
    while let Some(joined) = set.join_next().await {
        // JoinError 只可能来自取消或提交任务自身 panic：前者由 select! 处理，
        // 后者不该让整轮结果一起丢掉。
        if let Ok(item) = joined {
            collected.push(item);
        }
    }
    collected.sort_by_key(|(index, ..)| *index);
    collected
        .into_iter()
        .map(|(_, serial, lesson, result)| (serial, lesson, result))
        .collect()
}

fn is_network_error(kind: BackendErrorKind) -> bool {
    matches!(
        kind,
        BackendErrorKind::RateLimited
            | BackendErrorKind::Timeout
            | BackendErrorKind::Server
            | BackendErrorKind::Transport
            // 细分出来的传输类失败与 Transport 同权：它们同样不该计入
            // 「非网络失败」的停机阈值，选课窗口期不允许静默停机。
            | BackendErrorKind::Connect
            | BackendErrorKind::Tls
            | BackendErrorKind::Redirect
            | BackendErrorKind::Decode
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

/// governor 没给建议时的兜底退避。
///
/// 与 governor 的本地阶梯、登录重试共用同一个 decorrelated jitter 实现：
/// 三处各写一份确定性阶梯，等于让所有客户端在服务器抖动后同时重试。
fn fixed_network_backoff(consecutive_failures: u32) -> Duration {
    crate::eams::backoff_for_attempt(consecutive_failures)
}

/// 当前 run 被取消（stop_grab/登出/新一轮开始都会推进 generation）后完成。
/// 与网络 future 一起 select，让停止请求穿透 governor 等待与在途 HTTP，
/// 停止延迟从最长可达分钟级收敛到亚秒级。
async fn run_cancelled(state: &SharedState, generation: u64) {
    while state.is_current_run(generation) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 登录页特征的单次嗅探可能是教务高峰期负载抖动的误报。间隔 1 秒用
/// 轻量请求复核一次，连续两次判失效才视为真正掉线；真失效仅多花约 1 秒，
/// 换来误检不再在最关键时刻终止整个抢课任务。
async fn confirm_auth_expired(state: &SharedState, generation: u64, client: &EamsClient) -> bool {
    sleep_cancellable(state, generation, Duration::from_secs(1)).await;
    if !state.is_current_run(generation) {
        return false;
    }
    tokio::select! {
        biased;
        () = run_cancelled(state, generation) => false,
        result = client.ensure_logged_in() => match result {
            Ok(()) => false,
            Err(error) => is_auth_error(&error),
        },
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

    // C-04：自动重登是尽力而为，绝不能变成无限重试——连续失败到上限后必须
    // 干脆停机并告知用户，而不是继续拿同一份显然不对的凭据反复打服务器。
    #[test]
    fn relogin_stops_after_the_attempt_cap_without_touching_the_network() {
        let state = crate::worker::SharedState::new();
        let client = Arc::new(EamsClient::new("http://127.0.0.1:9/eams", 5, false).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut failures = crate::worker::session::MAX_RELOGIN_ATTEMPTS;
        let resumed = runtime.block_on(attempt_relogin(&state, &client, &mut failures));
        assert!(!resumed, "must give up once the cap is reached");
        assert_eq!(
            failures,
            crate::worker::session::MAX_RELOGIN_ATTEMPTS,
            "a refused attempt must not inflate the counter further"
        );
        assert!(
            log_messages(&state).is_empty(),
            "capped relogin must not even announce an attempt"
        );
    }

    // 没有保留凭据时要明确告诉用户为什么没有自动重登，并指出开关在哪。
    #[test]
    fn relogin_without_credentials_explains_itself() {
        let state = crate::worker::SharedState::new();
        let client = Arc::new(EamsClient::new("http://127.0.0.1:9/eams", 5, false).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut failures = 0u32;
        let resumed = runtime.block_on(attempt_relogin(&state, &client, &mut failures));
        assert!(!resumed);
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("未保留本次会话凭据")),
            "user must learn why nothing happened, got {logs:?}"
        );
    }

    // G-03：多目标必须并发提交，而不是一个等一个。
    //
    // 顺便补上档案指出的覆盖缺口——此前所有用例的 watch_serials 都只有一个
    // 序号，多目标场景零覆盖。这里两个目标都有余量，两次提交都应成功，且
    // 结果按原优先级顺序回放。
    #[test]
    fn multiple_ready_targets_are_submitted_and_all_succeed() {
        let data = "var lessonJSONs=[\
            {id:371644,no:'MT.001',name:'甲',teachers:'张老师',stdCount:1,limitCount:9},\
            {id:371645,no:'MT.002',name:'乙',teachers:'李老师',stdCount:2,limitCount:9}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:9},'371645':{sc:2,lc:9}}";
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            data,
            counts,
            // 两个目标的提交并发发出，响应内容相同，顺序无关。
            "选课成功",
            "选课成功",
        ]);
        let state = prepared_state(&base);
        let cfg = AppConfig {
            base_url: base.clone(),
            profile_id: "0".into(),
            watch_serials: vec!["MT.001".into(), "MT.002".into()],
            interval_seconds: 0.5,
            timeout_seconds: 5,
            max_consecutive_errors: 2,
            ..Default::default()
        };
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch.len(), 2, "both targets must be tracked: {watch:?}");
        for row in &watch {
            assert_eq!(
                row.state,
                WatchState::Success,
                "every ready target must be submitted: {watch:?}"
            );
        }
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("全部目标完成")),
            "run must finish cleanly, got {logs:?}"
        );
    }

    #[test]
    fn poll_delay_is_bounded() {
        // 常规：抖动只向正方向，绝不比用户设定的间隔更快。
        for _ in 0..100 {
            let delay = poll_delay_for_mode(1.5, 0.2, 1.0).as_secs_f64();
            assert!((1.5..=1.65).contains(&delay), "got {delay}");
        }
        for _ in 0..100 {
            let delay = poll_delay_for_mode(0.1, 0.05, 1.0).as_secs_f64();
            assert!((0.1..=0.11).contains(&delay), "got {delay}");
        }
    }

    // G-01：冲刺必须真的更快。此前冲刺唯一的差别是去掉正抖动（最多快 10%），
    // 默认 1.5s 间隔下 20 秒窗口只轮询约 13 次，令牌桶的 10 rps 永远碰不到。
    #[test]
    fn burst_actually_shortens_the_polling_period() {
        let normal = poll_delay_for_mode(1.5, 0.2, 1.0);
        let burst = poll_delay_for_mode(1.5, 0.2, 0.0);
        assert!(
            burst.as_secs_f64() < normal.as_secs_f64() / 5.0,
            "burst ({burst:?}) must be far shorter than normal ({normal:?}), not ~10% faster"
        );
        assert!((0.2..=0.21).contains(&burst.as_secs_f64()), "got {burst:?}");

        // 20 秒冲刺窗口内的轮数必须够多——这正是原实现拿不到的东西。
        let rounds = 20.0 / burst.as_secs_f64();
        assert!(rounds > 90.0, "only {rounds:.0} rounds in a 20s burst");
    }

    #[test]
    fn burst_ramps_back_instead_of_falling_off_a_cliff() {
        let burst = poll_delay_for_mode(1.5, 0.2, 0.0).as_secs_f64();
        let mid = poll_delay_for_mode(1.5, 0.2, 0.5).as_secs_f64();
        let normal = poll_delay_for_mode(1.5, 0.2, 1.0).as_secs_f64();
        assert!(
            burst < mid && mid < normal,
            "ramp must be monotonic: {burst} -> {mid} -> {normal}"
        );
        // 中点大致在两端之间，说明是线性回落而不是提前跳到常规值。
        assert!((0.8..=0.95).contains(&mid), "mid ramp got {mid}");
    }

    // 冲刺间隔配得比常规还慢时，冲刺不能反而变成减速。
    #[test]
    fn burst_interval_never_exceeds_the_normal_interval() {
        let delay = poll_delay_for_mode(0.3, 5.0, 0.0).as_secs_f64();
        assert!(
            delay <= 0.32,
            "burst must not be slower than normal: {delay}"
        );
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
    // 退避改成 decorrelated jitter 后，断言从「精确序列」变成「包络 + 打散」：
    // 精确序列本身就是雪崩的成因（所有客户端同时重试），不该再被锁死。
    fn fallback_network_backoff_uses_documented_sequence() {
        for attempt in [1u32, 2, 3, 4, 5, 99] {
            let delay = fixed_network_backoff(attempt);
            assert!(
                delay >= Duration::from_secs(2) && delay <= Duration::from_secs(30),
                "attempt {attempt} produced {delay:?} outside the documented envelope"
            );
        }
        let samples: Vec<Duration> = (0..64).map(|_| fixed_network_backoff(3)).collect();
        assert!(
            samples.windows(2).any(|w| w[0] != w[1]),
            "fallback backoff must be jittered, not a fixed ladder"
        );
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

    #[test]
    fn specified_lesson_missing_from_one_round_is_retried() {
        let others = "var lessonJSONs=[{id:999888,no:'OTH.001',name:'其他课',teachers:'李老师',stdCount:1,limitCount:2}];";
        let others_counts = "window.lessonId2Counts={'999888':{sc:1,lc:2}}";
        let target = "var lessonJSONs=[{id:371644,no:'IDR.001',name:'目标课',teachers:'张老师',stdCount:1,limitCount:2}];";
        let target_counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            // 第一轮目录缺失指定教学班：必须按 Missing 重试而不是终态放弃
            others,
            others_counts,
            target,
            target_counts,
            "选课成功",
            "var lessonJSONs=[];",
        ]);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "IDR.001");
        cfg.watch_lesson_ids
            .insert("IDR.001".into(), "371644".into());
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch[0].state, WatchState::Success, "got {watch:?}");
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("本轮未刷到")),
            "expected missing-retry log, got {logs:?}"
        );
    }

    #[test]
    fn specified_lesson_missing_for_max_rounds_becomes_terminal() {
        let others = "var lessonJSONs=[{id:999888,no:'OTH.001',name:'其他课',teachers:'李老师',stdCount:1,limitCount:2}];";
        let others_counts = "window.lessonId2Counts={'999888':{sc:1,lc:2}}";
        let mut seq = vec!["<html>elect page</html>"];
        for _ in 0..MAX_ID_MISSING_ROUNDS {
            seq.push(others);
            seq.push(others_counts);
        }
        let base = serve_sequence(seq);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "IDX.001");
        cfg.interval_seconds = 0.1;
        cfg.watch_lesson_ids
            .insert("IDX.001".into(), "371644".into());
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch[0].state, WatchState::Failed, "got {watch:?}");
        assert!(watch[0].detail.contains("连续"), "got {watch:?}");
        assert!(state.worker_message.lock().contains("失败/跳过 1"));
    }

    #[test]
    fn full_submission_result_keeps_target_pending_for_next_round() {
        let data = "var lessonJSONs=[{id:371644,no:'FUL.001',name:'Full',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            data,
            counts,
            // 第一轮提交返回容量已满：非终态，目标必须留在监控队列
            "上限人数已满",
            data,
            counts,
            "选课成功",
            "var lessonJSONs=[];",
        ]);
        let state = prepared_state(&base);
        start_grab(state.clone(), test_config(&base, "FUL.001"));
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch[0].state, WatchState::Success, "got {watch:?}");
        assert!(state.worker_message.lock().contains("全部完成"));
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("已满")),
            "expected full log, got {logs:?}"
        );
    }

    #[test]
    fn non_network_submission_failures_stop_after_threshold() {
        let data = "var lessonJSONs=[{id:371644,no:'ERR.001',name:'Err',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence_with_status(vec![
            (200, "<html>elect page</html>"),
            (200, data),
            (200, counts),
            // 4xx 属非网络类失败：连续 2 次必须触发自动停止
            (400, "请求参数错误"),
            (200, data),
            (200, counts),
            (400, "请求参数错误"),
        ]);
        let state = prepared_state(&base);
        start_grab(state.clone(), test_config(&base, "ERR.001"));
        wait_until_stopped(&state);

        assert!(
            state
                .worker_message
                .lock()
                .contains("提交连续失败 2 次，已自动停止"),
            "got message: {}",
            state.worker_message.lock()
        );
        let watch = state.watch.lock().clone();
        assert_eq!(watch[0].state, WatchState::Failed, "got {watch:?}");
    }

    #[test]
    fn consecutive_server_errors_do_not_trigger_auto_stop() {
        // 网络类失败（HTTP 500）交给 governor 冷却处理，绝不能命中停机
        // 阈值：反向锁定 !is_network_failure 守卫，选课窗口期不允许静默停机。
        let base = serve_sequence_with_status(vec![
            (500, "server error"),
            (500, "server error"),
            (500, "server error"),
            (500, "server error"),
        ]);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "NET.001");
        cfg.max_consecutive_errors = 1;
        start_grab(state.clone(), cfg);

        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline
            && !log_messages(&state)
                .iter()
                .any(|m| m.contains("刷新课程失败"))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("刷新课程失败")),
            "no refresh failure logged: {logs:?}"
        );
        assert!(
            state.running.load(Ordering::Acquire),
            "network failures must not stop the worker"
        );
        assert!(!state.worker_message.lock().contains("已自动停止"));
        stop_grab(&state);
        wait_until_stopped(&state);
    }

    #[test]
    fn non_network_refresh_failures_stop_after_threshold() {
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            // 数据接口返回完全不可解析的页面：解析失败属非网络类，须停机
            "<html>完全无法解析的页面</html>",
        ]);
        let state = prepared_state(&base);
        let mut cfg = test_config(&base, "PRS.001");
        cfg.max_consecutive_errors = 1;
        start_grab(state.clone(), cfg);
        wait_until_stopped(&state);

        assert!(
            state
                .worker_message
                .lock()
                .contains("连续刷新失败 1 次，已自动停止"),
            "got message: {}",
            state.worker_message.lock()
        );
    }

    #[test]
    fn transient_login_page_sniff_is_reverified_before_stopping() {
        let login_page = r#"<form id="loginForm"><input name="password"></form>"#;
        let data = "var lessonJSONs=[{id:371644,no:'AUT.001',name:'Auth',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence(vec![
            // 首轮刷新被误判为登录页：复核通过后必须继续而不是停机
            login_page,
            "<html>home ok</html>",
            "<html>elect page</html>",
            data,
            counts,
            "选课成功",
            "var lessonJSONs=[];",
        ]);
        let state = prepared_state(&base);
        start_grab(state.clone(), test_config(&base, "AUT.001"));
        wait_until_stopped(&state);

        let watch = state.watch.lock().clone();
        assert_eq!(watch[0].state, WatchState::Success, "got {watch:?}");
        assert!(
            state.logged_in.load(Ordering::Acquire),
            "a false alarm must not clear the session"
        );
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("复核未确认")),
            "expected reverify log, got {logs:?}"
        );
    }

    #[test]
    fn confirmed_auth_expiry_stops_and_marks_rows_stopped() {
        let login_page = r#"<form id="loginForm"><input name="password"></form>"#;
        let base = serve_sequence(vec![login_page, login_page]);
        let state = prepared_state(&base);
        start_grab(state.clone(), test_config(&base, "AUT.002"));
        wait_until_stopped(&state);

        assert!(!state.logged_in.load(Ordering::Acquire));
        assert!(state.worker_message.lock().contains("登录失效"));
        let watch = state.watch.lock().clone();
        assert_eq!(
            watch[0].state,
            WatchState::Stopped,
            "row must be marked stopped after auth loss: {watch:?}"
        );
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("登录失效，抢课已停止")),
            "expected auth stop log, got {logs:?}"
        );
    }

    #[test]
    fn submission_auth_expiry_notifies_and_marks_rows_stopped() {
        let login_page = r#"<form id="loginForm"><input name="password"></form>"#;
        let data = "var lessonJSONs=[{id:371644,no:'SUB.001',name:'Sub',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence(vec![
            "<html>elect page</html>",
            data,
            counts,
            // 提交响应是登录页；复核仍是登录页：确认掉线
            login_page,
            login_page,
        ]);
        let state = prepared_state(&base);
        start_grab(state.clone(), test_config(&base, "SUB.001"));
        wait_until_stopped(&state);

        assert!(!state.logged_in.load(Ordering::Acquire));
        assert!(state.worker_message.lock().contains("登录失效"));
        let watch = state.watch.lock().clone();
        assert_eq!(
            watch[0].state,
            WatchState::Stopped,
            "“提交中”行必须被标记已停止: {watch:?}"
        );
        let logs = log_messages(&state);
        assert!(
            logs.iter().any(|m| m.contains("登录失效，抢课已停止")),
            "expected submission auth stop log, got {logs:?}"
        );
    }

    #[test]
    fn stop_penetrates_governor_cooldown_quickly() {
        let base = serve_sequence(vec![]);
        let state = prepared_state(&base);
        // 模拟提交路径刚设下的长冷却：下一次刷新会在 acquire 中被阻塞
        state
            .client
            .lock()
            .as_ref()
            .unwrap()
            .set_cooldown_for_tests(Duration::from_secs(30));
        start_grab(state.clone(), test_config(&base, "CAN.001"));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !state.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(state.running.load(Ordering::Acquire), "run did not start");
        // 让 worker 进入 acquire 的冷却等待
        std::thread::sleep(Duration::from_millis(200));

        let stop_started = std::time::Instant::now();
        stop_grab(&state);
        wait_until_stopped(&state);
        assert!(
            stop_started.elapsed() < Duration::from_secs(2),
            "stop took {:?}, must penetrate governor cooldown sub-second",
            stop_started.elapsed()
        );
    }

    #[test]
    fn stop_race_with_natural_finish_never_strands_stopping_flag() {
        for _ in 0..200 {
            let state = SharedState::new();
            let generation = state.claim_run().expect("claim");
            let racer = state.clone();
            let handle = std::thread::spawn(move || racer.release_run_if_owner(generation));
            stop_grab(&state);
            handle.join().unwrap();
            assert!(
                !state.stopping.load(Ordering::Acquire),
                "run ended but stopping flag remained set"
            );
        }
    }

    fn log_messages(state: &SharedState) -> Vec<String> {
        state
            .logs
            .lock()
            .iter()
            .map(|l| l.message.clone())
            .collect()
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
            "worker did not stop in time; message={:?}, logs={:?}",
            state.worker_message.lock().clone(),
            log_messages(state)
        );
    }

    fn serve_sequence(responses: Vec<&'static str>) -> String {
        serve_sequence_with_status(responses.into_iter().map(|body| (200, body)).collect())
    }

    fn serve_sequence_with_status(responses: Vec<(u16, &'static str)>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}/eams")
    }
}

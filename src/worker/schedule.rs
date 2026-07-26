//! Precise scheduled-start arming and cancellation.
//!
//! 定时开抢的唯一触发点在本模块：armed/fired 键都记录在 SharedState 上，
//! UI 帧循环只根据 `schedule_decision` 决定重新待命、标记过期或不动作，
//! 绝不直接开抢。

use super::monitor::start_grab;
use super::runtime::spawn_task;
use super::{local_now_seconds, LogLevel, SharedState};
use crate::config::AppConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// 错过开抢时刻后的补触发宽限期（秒）：超过即视为过期，不再补抢。
pub const SCHEDULE_GRACE_SECS: i64 = 30;

/// UI 帧循环对定时开抢可执行的动作（由 `schedule_decision` 给出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    /// 需要（重新）登记 worker 精确待命；已到点但未过宽限期时由
    /// `arm_schedule` 的立即触发分支完成开抢。
    Arm,
    /// 已错过宽限期：标记过期，不触发。
    MarkExpired,
    /// 保持现状。
    Noop,
}

/// 定时开抢的触发决策。纯函数：时间与去重语义集中在此，便于边界测试。
pub fn schedule_decision(now: i64, target: i64, fired: bool, armed: bool) -> ScheduleAction {
    if fired {
        return ScheduleAction::Noop;
    }
    if now > target + SCHEDULE_GRACE_SECS {
        return ScheduleAction::MarkExpired;
    }
    if !armed {
        return ScheduleAction::Arm;
    }
    ScheduleAction::Noop
}

/// Cancel any pending precise schedule arm.
pub fn cancel_schedule_arm(state: &SharedState) {
    state.cancel_schedule_arm();
}

/// Sleep until `target_local_secs` (UTC+8 civil seconds) then start grab with minimal wake latency.
///
/// 到点开抢用 UI 最近保存的配置（latest_config），而不是 arm 时刻的快照：
/// 用户提前几小时待命后继续增删监控目标也必须生效。
pub fn arm_schedule(state: Arc<SharedState>, cfg: AppConfig, key: String, target_local_secs: i64) {
    if target_local_secs <= 0 {
        return;
    }
    let arm_gen = state.begin_schedule_arm(&key);
    // fire 时读取的配置基线；之后每次保存配置都会覆盖（见 save_config）。
    state.publish_config(cfg.clone());
    let now = local_now_seconds();
    if now >= target_local_secs {
        // Already due (inside the grace window) — fire immediately, worker-side.
        if state.running.load(Ordering::Acquire) {
            state.disarm_schedule_if_current(arm_gen);
        } else if state.claim_schedule_fire(&key, arm_gen) {
            state.log(LogLevel::Info, format!("定时开抢触发：{key}"));
            let run_cfg = state.latest_config.lock().clone().unwrap_or(cfg);
            start_grab(state, run_cfg);
        }
        return;
    }
    state.set_message(format!("定时待命中，目标 T{:+}s", target_local_secs - now));
    spawn_task(async move {
        loop {
            if state.schedule_arm_generation.load(Ordering::Acquire) != arm_gen {
                // 已被取消或新的待命接管：armed 键由接管方维护，不能动。
                return;
            }
            if state.running.load(Ordering::Acquire) || state.logging_in.load(Ordering::Acquire) {
                // 手动开抢/登录期间退出待命：只解除待命、绝不标记 fired，
                // 结束后 UI 会重新待命——定时开抢不能因一次手动操作静默失效。
                state.disarm_schedule_if_current(arm_gen);
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
        if state.running.load(Ordering::Acquire) {
            state.disarm_schedule_if_current(arm_gen);
            return;
        }
        if !state.claim_schedule_fire(&key, arm_gen) {
            return;
        }
        state.log(
            LogLevel::Info,
            format!(
                "精确定时触发（偏差约 {}ms 内）",
                ((local_now_seconds() - target_local_secs).abs() * 1000).min(999)
            ),
        );
        let run_cfg = state.latest_config.lock().clone().unwrap_or(cfg);
        start_grab(state, run_cfg);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eams::EamsClient;
    use crate::worker::monitor::stop_grab;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn schedule_decision_covers_grace_and_dedup_boundaries() {
        use ScheduleAction::*;
        let target = 1_000_000_i64;
        // fired 去重最优先：任何时刻都不再动作。
        assert_eq!(schedule_decision(target - 10, target, true, false), Noop);
        assert_eq!(schedule_decision(target + 60, target, true, true), Noop);
        // 未到点：未待命则待命，已待命保持。
        assert_eq!(schedule_decision(target - 10, target, false, false), Arm);
        assert_eq!(schedule_decision(target - 10, target, false, true), Noop);
        // 到点后宽限期内（含正好 +30s）仍要触发，由 arm 的立即分支完成。
        assert_eq!(schedule_decision(target, target, false, false), Arm);
        assert_eq!(
            schedule_decision(target + SCHEDULE_GRACE_SECS, target, false, false),
            Arm
        );
        // 超过宽限期一秒即过期，即使仍处于待命（如手动运行横跨目标时刻）。
        assert_eq!(
            schedule_decision(target + SCHEDULE_GRACE_SECS + 1, target, false, false),
            MarkExpired
        );
        assert_eq!(
            schedule_decision(target + SCHEDULE_GRACE_SECS + 1, target, false, true),
            MarkExpired
        );
    }

    #[test]
    fn overdue_arm_fires_immediately_and_marks_fired() {
        let base = serve_nothing();
        let state = prepared_state(&base);
        let key = "2026-01-01 08:00:00";
        arm_schedule(
            state.clone(),
            test_config(&base, "DUE.001"),
            key.into(),
            local_now_seconds() - 5,
        );
        // 立即触发分支是同步的：返回时 run 已开始、fired 已标记、待命解除。
        assert!(state.running.load(Ordering::Acquire));
        assert!(state.schedule_fired_matches(key));
        assert!(!state.schedule_armed_matches(key));
        let watch = state.watch.lock().clone();
        assert_eq!(watch.len(), 1);
        assert_eq!(watch[0].serial, "DUE.001");
        stop_grab(&state);
        wait_until_stopped(&state);
    }

    #[test]
    fn waiter_fires_at_target_with_latest_config() {
        let base = serve_nothing();
        let state = prepared_state(&base);
        let key = "2026-01-01 08:00:00";
        arm_schedule(
            state.clone(),
            test_config(&base, "OLD.001"),
            key.into(),
            local_now_seconds() + 2,
        );
        assert!(state.schedule_armed_matches(key));
        // arm 之后用户继续修改配置：到点必须用最新配置而不是 arm 快照。
        state.publish_config(test_config(&base, "NEW.001"));
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while !state.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            state.running.load(Ordering::Acquire),
            "scheduled fire did not start the run"
        );
        let watch = state.watch.lock().clone();
        assert_eq!(watch.len(), 1);
        assert_eq!(
            watch[0].serial, "NEW.001",
            "fire must use the latest config"
        );
        assert!(state.schedule_fired_matches(key));
        assert!(!state.schedule_armed_matches(key));
        stop_grab(&state);
        wait_until_stopped(&state);
    }

    #[test]
    fn manual_run_during_armed_wait_disarms_without_firing() {
        let state = crate::worker::SharedState::new();
        let key = "2026-01-01 08:00:00";
        arm_schedule(
            state.clone(),
            AppConfig::default(),
            key.into(),
            local_now_seconds() + 30,
        );
        assert!(state.schedule_armed_matches(key));
        // 定时待命期间手动开抢：待命任务必须退出且不得把该键标成 fired，
        // 否则手动停止后定时开抢会静默失效。
        state.running.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.schedule_armed_matches(key) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!state.schedule_armed_matches(key), "waiter did not disarm");
        assert!(
            !state.schedule_fired_matches(key),
            "manual run must not consume the schedule"
        );
        state.running.store(false, Ordering::Release);
        // 运行结束后 UI 决策必须重新待命。
        assert_eq!(
            schedule_decision(local_now_seconds(), local_now_seconds() + 20, false, false),
            ScheduleAction::Arm
        );
    }

    #[test]
    fn cancelled_arm_never_fires() {
        let state = crate::worker::SharedState::new();
        let key = "2026-01-01 08:00:00";
        let cfg = AppConfig {
            watch_serials: vec!["CXL.001".into()],
            ..Default::default()
        };
        arm_schedule(state.clone(), cfg, key.into(), local_now_seconds() + 1);
        assert!(state.schedule_armed_matches(key));
        cancel_schedule_arm(&state);
        assert!(!state.schedule_armed_matches(key));
        // 目标时刻过去后仍不得触发、不得标记 fired。
        std::thread::sleep(Duration::from_secs(2));
        assert!(!state.running.load(Ordering::Acquire));
        assert!(!state.schedule_fired_matches(key));
    }

    fn prepared_state(base: &str) -> Arc<crate::worker::SharedState> {
        let state = crate::worker::SharedState::new();
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

    fn wait_until_stopped(state: &crate::worker::SharedState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !state.running.load(Ordering::Acquire),
            "worker did not stop in time; message={:?}",
            state.worker_message.lock().clone()
        );
    }

    /// 只占一个端口、从不 accept：fire 后的请求会挂起等待，由 stop 的
    /// 取消机制穿透。测试仅关注触发语义本身。
    fn serve_nothing() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::mem::forget(listener);
        format!("http://{address}/eams")
    }
}

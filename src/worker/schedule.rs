//! Precise scheduled-start arming and cancellation.

use super::monitor::start_grab;
use super::runtime::spawn_task;
use super::{local_now_seconds, LogLevel, SharedState};
use crate::config::AppConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

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
        if state.schedule_arm_generation.load(Ordering::Acquire) != arm_gen
            || state.running.load(Ordering::Acquire)
        {
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

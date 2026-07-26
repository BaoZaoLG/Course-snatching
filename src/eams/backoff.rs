//! 统一的退避实现（decorrelated jitter）。
//!
//! 项目里原有三处各写一遍的退避表：governor 的本地阶梯、worker 的网络退避、
//! 登录重试。三处都是**确定性函数**——选课开放瞬间服务器返回 5xx，所有跑这个
//! 工具的学生会同时进入 `consecutive_errors = 1`，然后整齐划一地在 +2s、+6s、
//! +14s、+30s 重新撞上去。这是教科书式的重试雪崩，也是最容易把整个学院 IP
//! 段打进封禁名单的行为。
//!
//! 抖动的方向也曾是反的：服务器下发的 `Retry-After` 本来就是各客户端错开的，
//! 最不需要抖动；而本地阶梯最需要。而且原来只有 +0~10% 的单边抖动——单边抖动
//! 不能打散相位，只能把所有人整体往后推。
//!
//! 现在统一为 decorrelated jitter：第 n 次退避在 `[base, prev * 3]` 上均匀
//! 取样并截到上限。既保留指数增长的期望，又真正把相位打散。

use std::cell::Cell;
use std::time::{Duration, SystemTime};

/// 退避阶梯的下限与上限。三处调用点共用，避免再次漂移。
pub const BACKOFF_BASE: Duration = Duration::from_secs(2);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// 按「第几次连续失败」取样一次退避时长。
///
/// 无状态：调用点只需要知道自己失败了几次。等价于把 decorrelated jitter 的
/// 上界 `prev * 3` 用 `base * 3^(n-1)` 近似，再在 `[base, 上界]` 上均匀取样。
pub fn backoff_for_attempt(attempt: u32, base: Duration, max: Duration) -> Duration {
    let base_ms = base.as_millis().max(1) as u64;
    let max_ms = max.as_millis().max(base_ms as u128) as u64;
    // 3^(n-1)，按上限截断，避免 attempt 很大时溢出。
    let growth = 3u64.saturating_pow(attempt.saturating_sub(1).min(20));
    let ceiling = base_ms.saturating_mul(growth).min(max_ms);
    Duration::from_millis(sample_in_range(base_ms, ceiling))
}

/// 在 `[lo, hi]` 上均匀取样（含端点）。
fn sample_in_range(lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        return lo;
    }
    lo + next_u64() % (hi - lo + 1)
}

/// xorshift64*：不引入 rand 依赖，但比「拿当前纳秒当随机数」强得多。
/// 后者在并发失败时会给出高度相关的样本——恰恰是打散相位最需要避免的。
fn next_u64() -> u64 {
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|state| {
        let mut x = state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.set(x);
        // 乘上奇数常量改善低位质量（xorshift64* 的标准做法）。
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    // 混入进程 id 与线程地址，保证同一毫秒内启动的多个进程/线程不同序列。
    let pid = u64::from(std::process::id());
    let stack_addr = &nanos as *const u64 as u64;
    let mixed = nanos ^ pid.rotate_left(32) ^ stack_addr.rotate_left(16);
    if mixed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        mixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_stay_within_the_documented_envelope() {
        for attempt in 1..=8u32 {
            for _ in 0..200 {
                let delay = backoff_for_attempt(attempt, BACKOFF_BASE, BACKOFF_MAX);
                assert!(
                    delay >= BACKOFF_BASE && delay <= BACKOFF_MAX,
                    "attempt {attempt} produced {delay:?} outside [{BACKOFF_BASE:?}, {BACKOFF_MAX:?}]"
                );
            }
        }
        // 上限必须真的封顶，不能因为 3^n 溢出而回绕。
        assert!(backoff_for_attempt(u32::MAX, BACKOFF_BASE, BACKOFF_MAX) <= BACKOFF_MAX);
    }

    // 退避雪崩的判据就是「所有客户端同一时刻重试」。抖动必须真的把样本
    // 打散——原实现是确定性的，1000 个样本会全部相等。
    #[test]
    fn repeated_backoffs_are_actually_spread_out() {
        let samples: Vec<f64> = (0..1000)
            .map(|_| backoff_for_attempt(3, BACKOFF_BASE, BACKOFF_MAX).as_secs_f64())
            .collect();
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let variance =
            samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / samples.len() as f64;
        let stddev = variance.sqrt();
        // 第 3 次退避的取值区间是 [2s, 18s]；均匀分布的标准差约 4.6s。
        // 阈值取一半，确定性实现（stddev = 0）会立刻被抓住。
        assert!(
            stddev > 2.0,
            "backoff samples are not spread out: mean={mean:.2}s stddev={stddev:.2}s"
        );
        assert!(
            samples.windows(2).any(|w| w[0] != w[1]),
            "backoff is deterministic"
        );
    }

    #[test]
    fn growth_is_monotonic_in_expectation() {
        let mean_of = |attempt: u32| {
            let total: f64 = (0..400)
                .map(|_| backoff_for_attempt(attempt, BACKOFF_BASE, BACKOFF_MAX).as_secs_f64())
                .sum();
            total / 400.0
        };
        // 期望随失败次数上升（直到撞上限）。
        assert!(mean_of(1) < mean_of(2), "backoff must grow with failures");
        assert!(mean_of(2) < mean_of(4));
    }
}

/// 东八区本地秒（自 Unix 历日推算，与配置中的定时时刻共用同一基准）。
use std::time::SystemTime;

pub fn local_now_seconds() -> i64 {
    local_now_millis().div_euclid(1000)
}

/// 东八区本地毫秒。定时触发偏差必须用毫秒精度衡量——整秒精度下
/// 「偏差约 N ms」只可能打出 0 或上限值，反而掩盖真实抖动。
pub fn local_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
        + 8 * 3600 * 1000
        + test_clock_offset_ms()
}

/// 服务器视角的东八区毫秒。
///
/// 定时开抢一律用它：Windows 大约每周才同步一次时间，本机偏差 3–10 秒
/// 非常常见，而抢课场景下 3 秒偏差等于必输。未对时时它等于本机时间。
pub fn server_now_millis() -> i64 {
    crate::eams::clock::ClockSync::global().server_unix_millis()
        + 8 * 3600 * 1000
        + test_clock_offset_ms()
}

/// 服务器视角的东八区秒。
pub fn server_now_seconds() -> i64 {
    server_now_millis().div_euclid(1000)
}

/// 测试用的墙钟偏移。
///
/// 定时逻辑要能测「休眠跨过目标时刻后唤醒」「系统对时导致墙钟跳变」这类
/// 场景，而真实等待既慢又不确定。生产构建里这个函数是常量 0，会被完全
/// 优化掉；只有测试能拨动它。
#[cfg(test)]
pub(crate) fn test_clock_offset_ms() -> i64 {
    TEST_CLOCK_OFFSET_MS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(test))]
#[inline(always)]
fn test_clock_offset_ms() -> i64 {
    0
}

#[cfg(test)]
static TEST_CLOCK_OFFSET_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// 把墙钟往前拨 `millis` 毫秒（可为负）。仅测试可用。
#[cfg(test)]
pub(crate) fn advance_test_clock(millis: i64) {
    TEST_CLOCK_OFFSET_MS.fetch_add(millis, std::sync::atomic::Ordering::Relaxed);
}

/// 复位测试时钟。测试之间必须复位，否则会互相污染。
#[cfg(test)]
pub(crate) fn reset_test_clock() {
    TEST_CLOCK_OFFSET_MS.store(0, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn local_seconds() -> i64 {
    local_now_seconds()
}

pub(crate) fn now_hms() -> String {
    let time_of_day = local_seconds().rem_euclid(24 * 3600) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// 东八区当前日期，格式 YYYY-MM-DD。
pub fn now_ymd() -> String {
    let days = local_seconds().div_euclid(24 * 3600);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 东八区当前完整时刻，格式 YYYY-MM-DD HH:MM:SS。
pub fn now_stamp() -> String {
    format!("{} {}", now_ymd(), now_hms())
}

/// 东八区当前年月日时分秒。
pub fn now_parts() -> (i32, u32, u32, u32, u32, u32) {
    let secs = local_seconds();
    let days = secs.div_euclid(24 * 3600);
    let tod = secs.rem_euclid(24 * 3600) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
}

pub(crate) fn civil_from_days(days: i64) -> (i32, u32, u32) {
    // Howard Hinnant civil_from_days algorithm.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

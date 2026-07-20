/// 东八区本地秒（自 Unix 历日推算，与配置中的定时时刻共用同一基准）。
use std::time::SystemTime;

pub fn local_now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0) as i64
        + 8 * 3600
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

//! 与教务服务器对时。
//!
//! 定时开抢的全部价值建立在「本机时间等于服务器时间」这个前提上，而这个前提
//! 默认不成立：Windows 大约每周才同步一次时间，本机偏差 3–10 秒非常常见，而
//! 抢课场景下 3 秒偏差等于必输。
//!
//! 做法是零额外请求的被动对时：每个 HTTP 响应都带 `Date` 头，把它与请求的
//! 收发时刻一起喂进来即可估计偏移。只采信 RTT 更小的样本——RTT 越小，
//! 「服务器生成 Date 的时刻 ≈ 收发中点」这个近似越准。
//!
//! 精度上限是 `Date` 头只有秒级分辨率，所以偏移估计天然有 ±0.5s 的量化误差；
//! 这仍然比 3–10 秒的本机漂移好一个数量级。对时状态会如实报告给界面，
//! 没对上时明确说「未对时」而不是假装准确。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 进程级共享。教务地址在一次会话里是固定的，没必要按 origin 分。
static GLOBAL: OnceLock<ClockSync> = OnceLock::new();

#[derive(Debug)]
pub struct ClockSync {
    /// server_now - local_now，毫秒。
    offset_ms: AtomicI64,
    /// 采信度：产生当前 offset 的那个样本的 RTT，越小越可信。
    best_rtt_ms: AtomicU64,
    synced: AtomicBool,
}

/// 对时状态快照，供界面如实展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSyncSnapshot {
    pub synced: bool,
    pub offset_ms: i64,
    pub best_rtt_ms: u64,
}

impl ClockSync {
    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(|| Self {
            offset_ms: AtomicI64::new(0),
            best_rtt_ms: AtomicU64::new(u64::MAX),
            synced: AtomicBool::new(false),
        })
    }

    /// 喂入一个样本。`sent`/`received` 是同一次请求的单调时刻，
    /// `date_header` 是响应的 `Date` 头原文。
    ///
    /// 只有 RTT 严格更小的样本才会更新偏移：网络抖动大的样本对
    /// 「Date 生成于收发中点」这个假设的破坏最大。
    pub fn observe(&self, sent: Instant, received: Instant, date_header: Option<&str>) {
        let Some(server_unix_ms) = date_header.and_then(parse_http_date_unix_ms) else {
            return;
        };
        let rtt = received.saturating_duration_since(sent);
        let rtt_ms = rtt.as_millis() as u64;
        // 荒谬的 RTT（>30s）多半是被挂起/断点，样本不可信。
        if rtt_ms > 30_000 {
            return;
        }
        if rtt_ms >= self.best_rtt_ms.load(Ordering::Acquire) && self.synced.load(Ordering::Acquire)
        {
            return;
        }
        // 请求发出与响应收到的中点，近似服务器生成 Date 的本机时刻。
        let Some(local_mid_ms) = local_unix_millis_at(received, rtt_ms / 2) else {
            return;
        };
        self.offset_ms
            .store(server_unix_ms - local_mid_ms, Ordering::Release);
        self.best_rtt_ms.store(rtt_ms, Ordering::Release);
        self.synced.store(true, Ordering::Release);
    }

    /// 服务器视角的当前 Unix 毫秒。未对时则退化为本机时间。
    pub fn server_unix_millis(&self) -> i64 {
        local_unix_millis() + self.offset_ms.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> ClockSyncSnapshot {
        let best = self.best_rtt_ms.load(Ordering::Acquire);
        ClockSyncSnapshot {
            synced: self.synced.load(Ordering::Acquire),
            offset_ms: self.offset_ms.load(Ordering::Acquire),
            best_rtt_ms: if best == u64::MAX { 0 } else { best },
        }
    }

    /// 会话结束/切换教务地址时复位：换一台服务器后旧偏移不再有意义。
    pub fn reset(&self) {
        self.offset_ms.store(0, Ordering::Release);
        self.best_rtt_ms.store(u64::MAX, Ordering::Release);
        self.synced.store(false, Ordering::Release);
    }
}

fn local_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// `received` 往回退 `back_ms` 毫秒对应的 Unix 毫秒。
/// 用单调时钟测量间隔、只用一次墙钟读数，避免期间的墙钟跳变污染样本。
fn local_unix_millis_at(received: Instant, back_ms: u64) -> Option<i64> {
    let now_wall = local_unix_millis();
    let since_received = Instant::now()
        .saturating_duration_since(received)
        .as_millis() as i64;
    Some(now_wall - since_received - back_ms as i64)
}

/// 解析 HTTP-date。
///
/// 只支持 RFC 7231 的 IMF-fixdate（`Sun, 26 Jul 2026 12:34:56 GMT`）——这是
/// 规范要求服务器发送的唯一格式，另外两种过时格式不值得为它们引入依赖。
fn parse_http_date_unix_ms(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    // "Sun, 26 Jul 2026 12:34:56 GMT"
    let rest = raw
        .split_once(',')
        .map(|(_, rest)| rest)
        .unwrap_or(raw)
        .trim();
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_from_abbrev(parts.next()?)?;
    let year: i32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let zone = parts.next().unwrap_or("GMT");
    if !zone.eq_ignore_ascii_case("GMT") && !zone.eq_ignore_ascii_case("UTC") {
        return None;
    }
    let mut hms = time.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;
    if day == 0 || day > 31 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000)
}

fn month_from_abbrev(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(name))
        .map(|index| index as u32 + 1)
}

/// Howard Hinnant days_from_civil。与 config.rs 的同名实现同源，
/// 那边是私有的，这里不值得为几行代码把它变成跨模块公开 API。
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_imf_fixdate_against_known_instants() {
        // 1970-01-01T00:00:00Z
        assert_eq!(
            parse_http_date_unix_ms("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
        // 2024-02-29T00:00:00Z == 1_709_164_800
        assert_eq!(
            parse_http_date_unix_ms("Thu, 29 Feb 2024 00:00:00 GMT"),
            Some(1_709_164_800_000)
        );
        // 闰日翌日
        assert_eq!(
            parse_http_date_unix_ms("Fri, 01 Mar 2024 00:00:00 GMT"),
            Some(1_709_251_200_000)
        );
        // 带时分秒
        assert_eq!(
            parse_http_date_unix_ms("Sun, 26 Jul 2026 12:34:56 GMT"),
            Some(days_from_civil(2026, 7, 26) * 86_400_000 + (12 * 3600 + 34 * 60 + 56) * 1000)
        );
    }

    #[test]
    fn rejects_shapes_it_cannot_trust() {
        for bad in [
            "",
            "not a date",
            "Sun, 26 Jul 2026 12:34:56 +0800", // 非 GMT 一律不采信
            "Sun, 26 Foo 2026 12:34:56 GMT",   // 月份非法
            "Sun, 00 Jul 2026 12:34:56 GMT",   // 日为 0
            "Sun, 26 Jul 2026 25:00:00 GMT",   // 小时越界
            "Sun, 26 Jul 2026 12:34 GMT",      // 缺秒
        ] {
            assert!(
                parse_http_date_unix_ms(bad).is_none(),
                "must not trust {bad:?}"
            );
        }
    }

    #[test]
    fn keeps_the_lowest_rtt_sample_and_reports_being_unsynced() {
        let clock = ClockSync {
            offset_ms: AtomicI64::new(0),
            best_rtt_ms: AtomicU64::new(u64::MAX),
            synced: AtomicBool::new(false),
        };
        assert!(!clock.snapshot().synced, "must not claim accuracy upfront");

        // 没有 Date 头的响应不该改变任何状态。
        let now = Instant::now();
        clock.observe(now, now, None);
        assert!(!clock.snapshot().synced);

        // 第一个样本：RTT 大。
        let sent = Instant::now();
        let received = sent + Duration::from_millis(800);
        clock.observe(sent, received, Some("Sun, 26 Jul 2026 12:00:00 GMT"));
        let first = clock.snapshot();
        assert!(first.synced);
        assert_eq!(first.best_rtt_ms, 800);

        // 更差的样本（RTT 更大）不得覆盖。
        let worse_sent = Instant::now();
        let worse_received = worse_sent + Duration::from_millis(2_000);
        clock.observe(
            worse_sent,
            worse_received,
            Some("Sun, 26 Jul 2026 13:00:00 GMT"),
        );
        assert_eq!(
            clock.snapshot().best_rtt_ms,
            800,
            "a noisier sample must not replace a cleaner one"
        );

        // 更好的样本（RTT 更小）才更新。
        let better_sent = Instant::now();
        let better_received = better_sent + Duration::from_millis(20);
        clock.observe(
            better_sent,
            better_received,
            Some("Sun, 26 Jul 2026 14:00:00 GMT"),
        );
        assert_eq!(clock.snapshot().best_rtt_ms, 20);

        clock.reset();
        assert!(!clock.snapshot().synced, "reset must drop stale offsets");
    }

    // 荒谬的 RTT 多半是进程被挂起，样本不可信。
    #[test]
    fn absurd_round_trips_are_discarded() {
        let clock = ClockSync {
            offset_ms: AtomicI64::new(0),
            best_rtt_ms: AtomicU64::new(u64::MAX),
            synced: AtomicBool::new(false),
        };
        let sent = Instant::now();
        let received = sent + Duration::from_secs(120);
        clock.observe(sent, received, Some("Sun, 26 Jul 2026 12:00:00 GMT"));
        assert!(!clock.snapshot().synced);
    }

    // 偏移必须真的作用到「服务器现在几点」上，否则对时等于没做。
    #[test]
    fn offset_moves_server_now() {
        let clock = ClockSync {
            offset_ms: AtomicI64::new(0),
            best_rtt_ms: AtomicU64::new(u64::MAX),
            synced: AtomicBool::new(false),
        };
        let before = clock.server_unix_millis();
        clock.offset_ms.store(5_000, Ordering::Release);
        let after = clock.server_unix_millis();
        assert!(
            (after - before - 5_000).abs() < 500,
            "offset must shift server_now by exactly the offset"
        );
    }
}

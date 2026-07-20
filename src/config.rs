use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchMeta {
    pub name: String,
    pub teachers: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub base_url: String,
    pub username: String,
    pub interval_seconds: f64,
    /// 要抢的课程序号。执行前会再次精确解析为唯一课程。
    pub watch_serials: Vec<String>,
    /// 从课程列表选择时记录精确 lesson_id；手动输入的目标可以没有该值。
    pub watch_lesson_ids: HashMap<String, String>,
    /// 课程序号对应的课程名/教师，便于监控卡片展示。
    pub watch_meta: HashMap<String, WatchMeta>,
    /// 留空时自动探测；0 表示使用会话默认轮次。
    pub profile_id: String,
    pub timeout_seconds: u64,
    pub auto_fetch_on_login: bool,
    /// 仅在排障时开启；调试文件可能包含教务页面中的个人信息。
    pub debug_dump_enabled: bool,
    /// 连续网络/解析失败达到该次数后自动停止，避免无效请求服务器。
    pub max_consecutive_errors: u32,
    /// 检测到限流时自动拉长轮询间隔。
    pub adaptive_interval: bool,
    /// 抢课成功/失败时弹出应用内通知。
    pub notify_enabled: bool,
    /// 成功/失败时播放系统提示音。
    pub sound_enabled: bool,
    /// 界面缩放（0.9～1.5）。
    pub ui_scale: f32,
    /// 深色主题。
    pub dark_mode: bool,
    /// 每轮优先检查有余量的目标。
    pub grab_seats_first: bool,
    /// 仅监控余量，不自动提交选课。
    pub monitor_only: bool,
    /// 记住「仅有余量」筛选。
    pub only_available: bool,
    /// 记住课程搜索关键字。
    pub filter: String,
    /// 是否已确认首次使用提示。
    pub first_run_ack: bool,
    /// 是否启用定时开抢（东八区指定时刻到达后自动开始）。
    pub schedule_enabled: bool,
    /// 定时开抢时刻，格式 YYYY-MM-DD HH:MM:SS（兼容旧版 HH:MM）。
    pub schedule_time: String,
    /// 开始后前 N 秒进入冲刺：更短间隔、去掉正抖动，抢开课窗口。
    pub open_burst_seconds: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            base_url: "https://example.edu/eams".into(),
            username: String::new(),
            interval_seconds: 1.5,
            watch_serials: vec![],
            watch_lesson_ids: HashMap::new(),
            watch_meta: HashMap::new(),
            profile_id: String::new(),
            timeout_seconds: 15,
            auto_fetch_on_login: true,
            debug_dump_enabled: false,
            max_consecutive_errors: 5,
            adaptive_interval: true,
            notify_enabled: true,
            sound_enabled: true,
            ui_scale: 1.0,
            dark_mode: false,
            grab_seats_first: false,
            monitor_only: false,
            only_available: false,
            filter: String::new(),
            first_run_ack: false,
            schedule_enabled: false,
            schedule_time: default_schedule_time(),
            open_burst_seconds: 20,
        }
    }
}

impl AppConfig {
    /// 用户配置放在 roaming AppData，避免安装目录无写权限，也避免和发布文件混在一起。
    pub fn path() -> PathBuf {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return PathBuf::from(dir).join("Course-snatching").join("config.toml");
        }
        Self::legacy_path()
    }

    fn legacy_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("config.toml");
            }
        }
        PathBuf::from("config.toml")
    }

    /// 返回配置和可展示给用户的非致命提示。
    pub fn load_with_warning() -> (Self, Option<String>) {
        let path = Self::path();
        let legacy = Self::legacy_path();

        if !path.is_file() && legacy != path && legacy.is_file() {
            match Self::read_from(&legacy) {
                Ok(cfg) => match cfg.save() {
                    Ok(()) => {
                        return (cfg, Some(format!("已将旧配置迁移到 {}", path.display())));
                    }
                    Err(error) => {
                        return (
                            cfg,
                            Some(format!("配置迁移失败，将继续使用本次读取结果：{error:#}")),
                        );
                    }
                },
                Err(error) => {
                    return (
                        Self::default(),
                        Some(format!("旧配置读取失败，已使用默认配置：{error:#}")),
                    );
                }
            }
        }

        if !path.is_file() {
            let cfg = Self::default();
            let warning = cfg
                .save()
                .err()
                .map(|error| format!("首次保存配置失败：{error:#}"));
            return (cfg, warning);
        }

        match Self::read_from(&path) {
            Ok(cfg) => (cfg, None),
            Err(error) => {
                let backup = invalid_backup_path(&path);
                let backup_note = match fs::copy(&path, &backup) {
                    Ok(_) => format!("，原文件已备份到 {}", backup.display()),
                    Err(copy_error) => format!("，且备份失败：{copy_error}"),
                };
                (
                    Self::default(),
                    Some(format!(
                        "配置文件损坏，已使用默认配置：{error:#}{backup_note}"
                    )),
                )
            }
        }
    }

    fn read_from(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("读取配置失败：{}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("解析配置失败：{}", path.display()))?;
        cfg.normalize();
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
        }
        let mut normalized = self.clone();
        normalized.normalize();
        let body = toml::to_string_pretty(&normalized).context("序列化配置失败")?;
        let tmp_path = path.with_extension("toml.tmp");
        let backup_path = path.with_extension("toml.bak");
        {
            let mut file = fs::File::create(&tmp_path)
                .with_context(|| format!("写入临时配置失败：{}", tmp_path.display()))?;
            file.write_all(body.as_bytes())
                .with_context(|| format!("写入临时配置失败：{}", tmp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("同步临时配置失败：{}", tmp_path.display()))?;
        }
        if path.is_file() {
            let _ = fs::copy(path, &backup_path);
        }
        if let Err(error) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            if backup_path.is_file() {
                let _ = fs::copy(&backup_path, path);
            }
            return Err(error).with_context(|| format!("替换配置失败：{}", path.display()));
        }
        let _ = fs::remove_file(backup_path);
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        self.username = self.username.trim().to_string();
        self.profile_id = self.profile_id.trim().to_string();
        self.watch_serials = self.cleaned_watch();
        self.schedule_time = normalize_schedule_time(&self.schedule_time);
        let serials: HashSet<&str> = self.watch_serials.iter().map(String::as_str).collect();
        self.watch_lesson_ids.retain(|serial, lesson_id| {
            serials.contains(serial.as_str())
                && !lesson_id.is_empty()
                && lesson_id.chars().all(|ch| ch.is_ascii_digit())
                && lesson_id != "0"
        });
        self.watch_meta
            .retain(|serial, _| serials.contains(serial.as_str()));
        if !self.ui_scale.is_finite() {
            self.ui_scale = 1.0;
        }
        self.ui_scale = self.ui_scale.clamp(0.9, 1.5);
        self.filter = self.filter.trim().to_string();
    }

    pub fn cleaned_watch(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.watch_serials
            .iter()
            .map(|serial| serial.trim().to_string())
            .filter(|serial| !serial.is_empty() && seen.insert(serial.clone()))
            .collect()
    }

    pub fn validate_connection(&self) -> Result<()> {
        let url = Url::parse(self.base_url.trim()).context("教务地址格式错误")?;
        let host = url.host_str().unwrap_or_default();
        if host.is_empty() {
            bail!("教务地址缺少域名");
        }
        let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            bail!("为保护账号密码，教务地址必须使用 HTTPS（本机测试地址除外）");
        }
        if !(5..=120).contains(&self.timeout_seconds) {
            bail!("请求超时必须在 5～120 秒之间");
        }
        self.validate_profile_id()?;
        Ok(())
    }

    pub fn validate_profile_id(&self) -> Result<()> {
        let profile = self.profile_id.trim();
        if !profile.is_empty() && !profile.chars().all(|ch| ch.is_ascii_digit()) {
            bail!("选课轮次只能填写数字，留空表示自动探测");
        }
        Ok(())
    }

    pub fn validate_watch(&self) -> Result<()> {
        self.validate_connection()?;
        if self.cleaned_watch().is_empty() {
            bail!("请先添加要抢的课程序号");
        }
        if self.watch_lesson_ids.iter().any(|(serial, lesson_id)| {
            !self.watch_serials.iter().any(|item| item == serial)
                || lesson_id.is_empty()
                || !lesson_id.chars().all(|ch| ch.is_ascii_digit())
                || lesson_id == "0"
        }) {
            bail!("监控目标中的教学班标识无效，请删除后重新加入");
        }
        if !self.interval_seconds.is_finite() || !(0.05..=30.0).contains(&self.interval_seconds) {
            bail!("轮询间隔必须在 0.1～30 秒之间");
        }
        if !(1..=20).contains(&self.max_consecutive_errors) {
            bail!("连续错误上限必须在 1～20 之间");
        }
        if self.schedule_enabled && ScheduleStamp::parse(&self.schedule_time).is_none() {
            bail!("定时开抢时间无效，请重新选择年月日时分秒");
        }
        Ok(())
    }

    pub fn export_to(&self, path: &Path) -> Result<()> {
        let mut exported = self.clone();
        exported.debug_dump_enabled = false;
        exported.save_to(path)
    }

    pub fn import_from(path: &Path) -> Result<Self> {
        let mut cfg = Self::read_from(path)?;
        cfg.debug_dump_enabled = false;
        cfg.normalize();
        cfg.validate_connection()?;
        Ok(cfg)
    }

    pub fn data_dir() -> PathBuf {
        Self::path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn crash_log_path() -> PathBuf {
        Self::data_dir().join("crash.log")
    }
}

/// 定时开抢的完整本地时刻（东八区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleStamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl ScheduleStamp {
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        // Full: YYYY-MM-DD HH:MM:SS or YYYY-MM-DD HH:MM
        if let Some((date, time)) = trimmed.split_once(' ') {
            let date_parts: Vec<_> = date.split('-').collect();
            let time_parts: Vec<_> = time.split(':').collect();
            if date_parts.len() == 3 && (time_parts.len() == 2 || time_parts.len() == 3) {
                let year = parse_digits(date_parts[0], 1, 9999)? as i32;
                let month = parse_digits(date_parts[1], 1, 12)?;
                let day = parse_digits(date_parts[2], 1, 31)?;
                let hour = parse_digits(time_parts[0], 0, 23)?;
                let minute = parse_digits(time_parts[1], 0, 59)?;
                let second = if time_parts.len() == 3 {
                    parse_digits(time_parts[2], 0, 59)?
                } else {
                    0
                };
                let stamp = Self {
                    year,
                    month,
                    day,
                    hour,
                    minute,
                    second,
                };
                return stamp.validated();
            }
        }
        // Legacy: HH:MM or HH:MM:SS — attach today's date at load time is handled in normalize.
        let time_parts: Vec<_> = trimmed.split(':').collect();
        if time_parts.len() == 2 || time_parts.len() == 3 {
            let hour = parse_digits(time_parts[0], 0, 23)?;
            let minute = parse_digits(time_parts[1], 0, 59)?;
            let second = if time_parts.len() == 3 {
                parse_digits(time_parts[2], 0, 59)?
            } else {
                0
            };
            // Placeholder date; normalize() will rewrite with current local date.
            return Some(Self {
                year: 1970,
                month: 1,
                day: 1,
                hour,
                minute,
                second,
            });
        }
        None
    }

    pub fn validated(self) -> Option<Self> {
        if !(1..=12).contains(&self.month)
            || !(0..=23).contains(&self.hour)
            || !(0..=59).contains(&self.minute)
            || !(0..=59).contains(&self.second)
        {
            return None;
        }
        let max_day = days_in_month(self.year, self.month);
        if self.day == 0 || self.day > max_day {
            return None;
        }
        Some(self)
    }

    pub fn display(self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// 东八区本地秒（与 worker 侧 local_seconds 同基准）。
    pub fn to_local_seconds(self) -> Option<i64> {
        self.validated()?;
        let days = days_from_civil(self.year, self.month, self.day);
        Some(
            days * 86_400
                + i64::from(self.hour) * 3600
                + i64::from(self.minute) * 60
                + i64::from(self.second),
        )
    }
}

fn parse_digits(raw: &str, min: u32, max: u32) -> Option<u32> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: u32 = raw.parse().ok()?;
    if (min..=max).contains(&value) {
        Some(value)
    } else {
        None
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u64;
    i64::from(era) * 146_097 + doe as i64 - 719_468
}

fn normalize_schedule_time(raw: &str) -> String {
    let Some(mut stamp) = ScheduleStamp::parse(raw) else {
        return default_schedule_time();
    };
    // Legacy HH:MM used placeholder 1970-01-01 — upgrade to "today" local date.
    if stamp.year == 1970 && stamp.month == 1 && stamp.day == 1 && !raw.contains('-') {
        if let Some(today) = current_local_stamp() {
            stamp.year = today.year;
            stamp.month = today.month;
            stamp.day = today.day;
        }
    }
    stamp
        .validated()
        .map(ScheduleStamp::display)
        .unwrap_or_else(default_schedule_time)
}

fn default_schedule_time() -> String {
    current_local_stamp()
        .map(|mut s| {
            s.hour = 8;
            s.minute = 0;
            s.second = 0;
            s.display()
        })
        .unwrap_or_else(|| "2026-01-01 08:00:00".into())
}

fn current_local_stamp() -> Option<ScheduleStamp> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
        + 8 * 3600;
    let days = seconds.div_euclid(86_400);
    let tod = seconds.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days_cfg(days);
    Some(ScheduleStamp {
        year,
        month,
        day,
        hour: tod / 3600,
        minute: (tod % 3600) / 60,
        second: tod % 60,
    })
}

fn civil_from_days_cfg(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn invalid_backup_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    path.with_extension(format!("invalid-{timestamp}.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaned_watch_trims_and_deduplicates() {
        let cfg = AppConfig {
            watch_serials: vec![" A.1 ".into(), "A.1".into(), "".into(), " B.2".into()],
            ..Default::default()
        };
        assert_eq!(cfg.cleaned_watch(), vec!["A.1", "B.2"]);
    }
    #[test]
    fn default_schedule_time_uses_today() {
        let stamp = default_schedule_time();
        let today = current_local_stamp().expect("local stamp");
        assert!(
            stamp.starts_with(&format!(
                "{:04}-{:02}-{:02}",
                today.year, today.month, today.day
            )),
            "default schedule should use today, got {stamp}"
        );
        assert!(stamp.ends_with(" 08:00:00"), "got {stamp}");
        assert_eq!(AppConfig::default().schedule_time, stamp);
    }

    #[test]
    fn normalize_schedule_time_pads() {
        let full = normalize_schedule_time("8:5");
        assert!(full.ends_with(" 08:05:00"), "got {full}");
        assert!(ScheduleStamp::parse(&full).is_some());
        assert!(ScheduleStamp::parse("2026-07-20 08:00:00").is_some());
        assert!(ScheduleStamp::parse("2026-02-30 08:00:00").is_none());
        assert!(ScheduleStamp::parse("24:00").is_none());
        let stamp = ScheduleStamp::parse("2026-07-20 09:30:15").unwrap();
        assert_eq!(stamp.display(), "2026-07-20 09:30:15");
        assert!(stamp.to_local_seconds().unwrap() > 0);
    }

    #[test]
    fn normalize_keeps_only_valid_lesson_ids_for_active_serials() {
        let mut cfg = AppConfig {
            watch_serials: vec!["A.1".into()],
            watch_lesson_ids: HashMap::from([
                ("A.1".into(), "12345".into()),
                ("B.2".into(), "67890".into()),
                ("C.3".into(), "invalid".into()),
            ]),
            watch_meta: HashMap::from([
                (
                    "A.1".into(),
                    WatchMeta {
                        name: "课".into(),
                        teachers: "师".into(),
                    },
                ),
                (
                    "B.2".into(),
                    WatchMeta {
                        name: "旧".into(),
                        teachers: String::new(),
                    },
                ),
            ]),
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.watch_lesson_ids.len(), 1);
        assert_eq!(
            cfg.watch_lesson_ids.get("A.1").map(String::as_str),
            Some("12345")
        );
        assert_eq!(cfg.watch_meta.len(), 1);
        assert!(cfg.watch_meta.contains_key("A.1"));
    }

    #[test]
    fn validates_secure_connection_and_profile() {
        let mut cfg = AppConfig::default();
        assert!(cfg.validate_connection().is_ok());
        cfg.base_url = "http://example.com/eams".into();
        assert!(cfg.validate_connection().is_err());
        cfg.base_url = "http://127.0.0.1:8080/eams".into();
        cfg.profile_id = "abc".into();
        assert!(cfg.validate_connection().is_err());
    }

    #[test]
    fn save_never_serializes_a_password_field() {
        let dir =
            std::env::temp_dir().join(format!("Course-snatching-config-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let cfg = AppConfig::default();
        cfg.save_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password"));
        let _ = fs::remove_dir_all(dir);
    }
}

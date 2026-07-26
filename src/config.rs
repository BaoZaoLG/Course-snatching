use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CONFIG_SAVE_LOCK: Mutex<()> = Mutex::new(());
static CRASH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub const DEBUG_FILE_LIMIT: usize = 10;
pub const DEBUG_TOTAL_BYTES_LIMIT: u64 = 20 * 1024 * 1024;
pub const DEBUG_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
pub const CRASH_FILE_LIMIT: usize = 3;
/// 崩溃报告也要有年龄上限：只按数量轮转的话，一份含服务器文本的报告可以
/// 无限期留在盘上。
pub const CRASH_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

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
    /// 旧配置读取兼容字段。该开关只在当前会话有效，永不以开启状态写回配置。
    #[serde(skip_serializing)]
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
    /// 冲刺期的轮询间隔（秒）。
    ///
    /// 冲刺此前唯一的差别只是去掉 0–10% 的正抖动，也就是最多快 10%——默认
    /// 间隔 1.5s 时，20 秒的「冲刺窗口」只轮询约 13 次，令牌桶的 10 rps
    /// 上限永远触发不到。真正的提速必须靠独立的冲刺间隔，让令牌桶（而不是
    /// 用户间隔）成为限流点。
    pub burst_interval_seconds: f64,
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
            burst_interval_seconds: 0.2,
        }
    }
}

impl AppConfig {
    /// 用户配置放在 roaming AppData，避免安装目录无写权限，也避免和发布文件混在一起。
    pub fn path() -> PathBuf {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return PathBuf::from(dir)
                .join("Course-snatching")
                .join("config.toml");
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
            Ok(mut cfg) => {
                // Raw page dumps may contain personal information. A persisted legacy option
                // must never silently re-enable them in a new application session.
                cfg.debug_dump_enabled = false;
                (cfg, None)
            }
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
        // This legacy field is accepted on read only. Raw pages can contain personal data,
        // so every fresh application session must start with dumping disabled.
        cfg.debug_dump_enabled = false;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let _save_guard = CONFIG_SAVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
        }
        let mut normalized = self.clone();
        normalized.normalize();
        let body = toml::to_string_pretty(&normalized).context("序列化配置失败")?;
        let tmp_path = next_config_temp_path(path);
        let write_result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .with_context(|| format!("写入临时配置失败：{}", tmp_path.display()))?;
            file.write_all(body.as_bytes())
                .with_context(|| format!("写入临时配置失败：{}", tmp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("同步临时配置失败：{}", tmp_path.display()))?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        atomic_replace(&tmp_path, path)
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
        // normalize() 是唯一的「变安全」漏斗：所有数值不变式都在这里兜底，
        // 免得 UI 与 worker 各兜一次还漏掉导入/手改配置的路径。
        if !self.interval_seconds.is_finite() {
            self.interval_seconds = 1.5;
        }
        self.interval_seconds = self.interval_seconds.clamp(0.05, 30.0);
        if !self.burst_interval_seconds.is_finite() {
            self.burst_interval_seconds = 0.2;
        }
        // 上限取常规间隔：冲刺比常规还慢没有意义。
        self.burst_interval_seconds = self
            .burst_interval_seconds
            .clamp(0.05, self.interval_seconds.max(0.05));
        self.open_burst_seconds = self.open_burst_seconds.min(120);
        self.timeout_seconds = self.timeout_seconds.clamp(5, 120);
        self.max_consecutive_errors = self.max_consecutive_errors.clamp(1, 100);
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
        Self::crash_dir()
    }

    pub fn debug_dir() -> PathBuf {
        Self::data_dir().join("debug")
    }

    pub fn crash_dir() -> PathBuf {
        Self::data_dir().join("crash")
    }

    /// Best-effort rotation for privacy-sensitive raw pages. Errors intentionally do not
    /// interrupt login, monitoring, or a diagnostic dump.
    pub fn retain_debug_files() -> io::Result<()> {
        retain_files(
            &Self::debug_dir(),
            DEBUG_FILE_LIMIT,
            DEBUG_TOTAL_BYTES_LIMIT,
            Some(DEBUG_MAX_AGE_SECS),
        )
    }

    pub fn retain_crash_reports() -> io::Result<()> {
        retain_files(
            &Self::crash_dir(),
            CRASH_FILE_LIMIT,
            u64::MAX,
            Some(CRASH_MAX_AGE_SECS),
        )
    }

    /// Panic reporting must not trigger a second panic. Each report has its own file so a
    /// later crash does not overwrite the evidence from an earlier one.
    ///
    /// 报告在这里统一过一遍脱敏：panic payload 只要经手过服务器文本
    /// （`.expect(&format!(...))` 一次疏忽就够）就会原样落盘，而这条曾是
    /// 全项目唯一绕过脱敏的落盘路径。
    pub fn write_crash_report(report: &str) -> io::Result<PathBuf> {
        Self::write_crash_report_in(&Self::crash_dir(), report)
    }

    /// 目录可注入，测试才能验证「落盘的那份确实已脱敏」而不是往用户
    /// 真实的 %APPDATA% 里写东西。
    pub(crate) fn write_crash_report_in(dir: &Path, report: &str) -> io::Result<PathBuf> {
        let report = redact_diagnostic_text(report);
        let report = report.as_str();
        fs::create_dir_all(dir)?;
        let _ = retain_files(
            dir,
            CRASH_FILE_LIMIT.saturating_sub(1),
            u64::MAX,
            Some(CRASH_MAX_AGE_SECS),
        );
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let seq = CRASH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "crash-{}-{}-{}.log",
            std::process::id(),
            stamp,
            seq
        ));
        fs::write(&path, report)?;
        let _ = retain_files(dir, CRASH_FILE_LIMIT, u64::MAX, Some(CRASH_MAX_AGE_SECS));
        Ok(path)
    }
}

#[derive(Debug)]
struct RetainedFile {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

/// Removes oldest regular files until all supplied bounds hold. Directories and unreadable
/// entries are left untouched, making this safe to use in a user-owned data directory.
pub fn retain_files(
    dir: &Path,
    max_files: usize,
    max_total_bytes: u64,
    max_age_secs: Option<u64>,
) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let now = SystemTime::now();
    let mut files = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| RetainedFile {
                path: entry.path(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                len: metadata.len(),
            })
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|file| file.modified);

    let mut remaining_files = files.len();
    let mut total = files.iter().map(|file| file.len).sum::<u64>();
    for file in files {
        let expired = max_age_secs.is_some_and(|max_age| {
            now.duration_since(file.modified)
                .map(|age| age.as_secs() > max_age)
                .unwrap_or(false)
        });
        let over_limit = remaining_files > max_files || total > max_total_bytes;
        if (expired || over_limit) && fs::remove_file(&file.path).is_ok() {
            remaining_files = remaining_files.saturating_sub(1);
            total = total.saturating_sub(file.len);
        }
    }
    Ok(())
}

// 脱敏正则全部 LazyLock 缓存：脱敏现在跑在日志写入端（每条日志一次），
// 每次调用重新编译三个正则的开销落在 worker 热路径上。
static RE_REDACT_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| {
    // 折行的 header 续行（以空白开头）也要一并抹掉，否则
    // "Set-Cookie:\n\tJSESSIONID=…" 只抹掉了首行。
    regex::Regex::new(r"(?im)^(cookie|set-cookie|authorization)\s*:\s*.*(?:\r?\n[ \t]+.*)*$")
        .expect("header redaction regex")
});
static RE_REDACT_KEY_VALUE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b(password|passwd|pwd|token|jsessionid|session(?:id)?)(\s*[:=]\s*)([^\s,;]+)",
    )
    .expect("redaction regex")
});
static RE_REDACT_QUERY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)([?&](?:password|passwd|pwd|token|jsessionid|session(?:id)?)=)[^&#\s]+")
        .expect("query redaction regex")
});
/// 一行里出现多对 `名=值`（典型的 Cookie 行 `a=1; JSESSIONID=x; b=2`）时，
/// 上面的 key_value 正则会逐对命中，但一行只写了一个 header 前缀的情况
/// 靠这条兜底：把 `; ` 分隔的敏感对逐个抹掉。
static RE_REDACT_COOKIE_PAIR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(jsessionid|sessionid|session|token|auth)=([^;&\s]+)")
        .expect("cookie pair redaction regex")
});

/// Logs and diagnostic notes can contain server text. Strip the common credential and session
/// forms before any export; raw pages are handled by a separate explicit confirmation.
pub fn redact_diagnostic_text(text: &str) -> String {
    let text = RE_REDACT_HEADER.replace_all(text, "$1: [已隐藏]");
    let text = RE_REDACT_KEY_VALUE.replace_all(&text, "$1$2[已隐藏]");
    let text = RE_REDACT_QUERY.replace_all(&text, "${1}[已隐藏]");
    RE_REDACT_COOKIE_PAIR
        .replace_all(&text, "$1=[已隐藏]")
        .into_owned()
}

/// Sanitises a raw debug response before it can be added to an explicitly requested
/// diagnostic bundle. Submission forms are excluded wholesale because retaining an
/// election payload would create an avoidable privacy risk.
pub fn redact_diagnostic_page(name: &str, content: &str) -> Option<String> {
    let lowered = format!("{name}\n{content}").to_ascii_lowercase();
    const SUBMISSION_MARKERS: [&str; 5] = [
        "batchoperator",
        "operator0",
        "optype",
        "elect_lesson",
        "selection payload",
    ];
    if SUBMISSION_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return None;
    }

    let text = redact_diagnostic_text(content);
    let text = RE_REDACT_EMBEDDED.replace_all(&text, "$1$2[已隐藏]");
    let text = RE_REDACT_HTML_VALUE.replace_all(&text, "$1[已隐藏]$3");
    // value 写在 name 之前的 input，以及 CSRF/隐藏令牌字段，上面两条都盖不到。
    let text = RE_REDACT_HTML_VALUE_FIRST.replace_all(&text, "$1[已隐藏]$3");
    Some(
        RE_REDACT_HIDDEN_TOKEN
            .replace_all(&text, "$1[已隐藏]$3")
            .into_owned(),
    )
}

static RE_REDACT_EMBEDDED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(password|passwd|token|session(?:id)?|jsessionid|authorization|cookie)(\s*[\"']?\s*[:=]\s*[\"']?)([^\"'&<>\s,;]+)"#,
    )
    .expect("embedded diagnostic secret regex")
});
static RE_REDACT_HTML_VALUE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(<(?:input|meta)\b[^>]*(?:name|id)\s*=\s*[\"']?(?:password|passwd|token|session(?:id)?|jsessionid)[\"']?[^>]*\bvalue\s*=\s*[\"'])([^\"']*)([\"'])"#,
    )
    .expect("HTML diagnostic secret regex")
});
/// `<input value="secret" name="password">`：value 在 name 之前。
static RE_REDACT_HTML_VALUE_FIRST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(<(?:input|meta)\b[^>]*\bvalue\s*=\s*[\"'])([^\"']*)([\"'][^>]*(?:name|id)\s*=\s*[\"']?(?:password|passwd|token|session(?:id)?|jsessionid))"#,
    )
    .expect("HTML value-first secret regex")
});
/// 隐藏的 CSRF / 一次性令牌字段：名字五花八门，凡 hidden 且名字里带
/// csrf/token/nonce/state/ticket 的一律抹掉值。
static RE_REDACT_HIDDEN_TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(<input\b[^>]*(?:name|id)\s*=\s*[\"']?[a-z0-9_\-]*(?:csrf|xsrf|token|nonce|state|ticket|salt)[a-z0-9_\-]*[\"']?[^>]*\bvalue\s*=\s*[\"'])([^\"']*)([\"'])"#,
    )
    .expect("hidden token redaction regex")
});

pub fn redact_diagnostic_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.split('?').next().unwrap_or_default().to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
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

fn next_config_temp_path(destination: &Path) -> PathBuf {
    let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("config.toml"))
        .to_string_lossy();
    destination.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

fn atomic_replace(temp: &Path, destination: &Path) -> Result<()> {
    atomic_replace_with(temp, destination, platform_atomic_replace)
}

fn atomic_replace_with<F>(temp: &Path, destination: &Path, replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path, &Path) -> io::Result<()>,
{
    let backup = temp.with_extension("bak");
    match replace(temp, destination, &backup) {
        Ok(()) => {
            if backup.is_file() {
                fs::remove_file(&backup)
                    .with_context(|| format!("配置已替换，但删除备份失败：{}", backup.display()))?;
            }
            Ok(())
        }
        Err(error) => {
            let restore_note = if backup.is_file() {
                match fs::copy(&backup, destination) {
                    Ok(_) => String::new(),
                    Err(restore_error) => format!("；从备份恢复原配置失败：{restore_error}"),
                }
            } else {
                String::new()
            };
            let cleanup_note = match fs::remove_file(temp) {
                Ok(()) => String::new(),
                Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                    String::new()
                }
                Err(cleanup_error) => format!("；临时文件清理失败：{cleanup_error}"),
            };
            anyhow::bail!(
                "替换配置失败：{}；备份路径：{}；系统错误：{}{}{}",
                destination.display(),
                backup.display(),
                error,
                restore_note,
                cleanup_note
            )
        }
    }
}

#[cfg(windows)]
fn platform_atomic_replace(temp: &Path, destination: &Path, backup: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH, REPLACEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let temp = wide(temp);
    let destination_wide = wide(destination);
    let backup = wide(backup);
    // SAFETY: all pointers reference live, NUL-terminated UTF-16 buffers for the call duration.
    let replaced = unsafe {
        if destination.is_file() {
            ReplaceFileW(
                destination_wide.as_ptr(),
                temp.as_ptr(),
                backup.as_ptr(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        } else {
            MoveFileExW(
                temp.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn platform_atomic_replace(temp: &Path, destination: &Path, _backup: &Path) -> io::Result<()> {
    fs::rename(temp, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "Course-snatching-config-test-{}-{}-{label}",
            std::process::id(),
            CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

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

    // days_from_civil 是手写历法算法，必须有已知日期的数值断言兜底；
    // 基准：东八区本地秒 = Unix 秒 + 8*3600（与 worker::local_now_seconds 一致）。
    #[test]
    fn to_local_seconds_matches_known_utc8_instants() {
        let epoch = ScheduleStamp {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(epoch.to_local_seconds(), Some(0));
        // 闰日：2024-02-29 08:00:00 UTC+8 == 2024-02-29T00:00:00Z == Unix 1_709_164_800。
        let leap_day = ScheduleStamp {
            year: 2024,
            month: 2,
            day: 29,
            hour: 8,
            minute: 0,
            second: 0,
        };
        assert_eq!(leap_day.to_local_seconds(), Some(1_709_164_800 + 8 * 3600));
        // 闰日翌日 2024-03-01 00:00:00 UTC+8 == 2024-02-29T16:00:00Z == Unix 1_709_222_400。
        let after_leap = ScheduleStamp {
            year: 2024,
            month: 3,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(
            after_leap.to_local_seconds(),
            Some(1_709_222_400 + 8 * 3600)
        );
        // 平年没有 2/29；闰年有。
        assert!(ScheduleStamp::parse("2025-02-29 08:00:00").is_none());
        assert!(ScheduleStamp::parse("2024-02-29 08:00:00").is_some());
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
        let dir = config_test_dir("password");
        let path = dir.join("config.toml");
        let cfg = AppConfig::default();
        cfg.save_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn debug_dump_is_session_only_and_never_written_back() {
        let dir = config_test_dir("debug-session");
        let path = dir.join("config.toml");
        let cfg = AppConfig {
            debug_dump_enabled: true,
            ..Default::default()
        };

        cfg.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("debug_dump_enabled"));
        assert!(!AppConfig::read_from(&path).unwrap().debug_dump_enabled);

        fs::write(&path, "debug_dump_enabled = true\n").unwrap();
        assert!(!AppConfig::read_from(&path).unwrap().debug_dump_enabled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retention_removes_oldest_files_until_count_and_size_fit() {
        let dir = config_test_dir("retention");
        fs::create_dir_all(&dir).unwrap();
        for (name, content) in [
            ("old.txt", "1111"),
            ("middle.txt", "2222"),
            ("new.txt", "3333"),
        ] {
            fs::write(dir.join(name), content).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        retain_files(&dir, 2, 8, None).unwrap();

        assert!(!dir.join("old.txt").exists());
        assert!(dir.join("middle.txt").exists());
        assert!(dir.join("new.txt").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retention_enforces_zero_count_and_expiry_bounds() {
        let dir = config_test_dir("retention-expiry");
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.txt");
        fs::write(&old, "old").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&old).unwrap();
        let old_time = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(8 * 24 * 60 * 60))
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        drop(file);

        retain_files(&dir, 10, DEBUG_TOTAL_BYTES_LIMIT, Some(DEBUG_MAX_AGE_SECS)).unwrap();
        assert!(!old.exists(), "files older than seven days must be removed");

        fs::write(dir.join("one.txt"), "1").unwrap();
        retain_files(&dir, 0, u64::MAX, None).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn diagnostic_redaction_hides_credentials_sessions_and_query_values() {
        let text = "Cookie: session=very-secret; other=value\npassword=hunter2 token: abc123 \
https://example.edu/eams?sessionId=abc&course=1";
        let redacted = redact_diagnostic_text(text);
        assert!(!redacted.contains("very-secret"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("sessionId=abc"));
        assert!(redacted.contains("[已隐藏]"));

        let url = redact_diagnostic_url("https://user:secret@example.edu/eams?a=1#detail");
        assert_eq!(url, "https://example.edu/eams");
    }

    // E-06：示例配置漂移过两次（教用户写一个 skip_serializing 的字段、
    // 写死一个必然过期的日期）。用测试把它钉在真实结构上。
    #[test]
    fn example_config_stays_in_sync_with_the_real_struct() {
        let text = fs::read_to_string("config.example.toml").expect("example config must exist");
        let parsed: AppConfig = toml::from_str(&text).expect("example config must deserialize");
        // 示例里不得出现只在会话内有效、永不写回的字段。
        assert!(
            !text.contains("debug_dump_enabled"),
            "example must not teach a session-only field"
        );
        // 也不得写死一个必然过期的开抢时刻。
        assert!(
            !text.contains("schedule_time = \"20"),
            "example must not hardcode a date that expires on release"
        );
        // 字段集合必须与默认配置一致：新增字段忘了同步会立刻暴露。
        let example_keys = toml::to_string(&parsed)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim().to_string()))
            .collect::<HashSet<_>>();
        let default_keys = toml::to_string(&AppConfig::default())
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim().to_string()))
            .collect::<HashSet<_>>();
        assert_eq!(
            example_keys, default_keys,
            "config.example.toml drifted from AppConfig"
        );
    }

    // 数值不变式统一收敛到 normalize()：UI 与 worker 各兜一次总会漏掉
    // 导入/手改配置的路径。
    #[test]
    fn normalize_is_the_single_funnel_for_numeric_invariants() {
        let mut cfg = AppConfig {
            interval_seconds: f64::NAN,
            burst_interval_seconds: 99.0,
            open_burst_seconds: 9_999,
            timeout_seconds: 1,
            max_consecutive_errors: 0,
            ui_scale: f32::INFINITY,
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.interval_seconds, 1.5, "NaN interval must be repaired");
        assert!(
            cfg.burst_interval_seconds <= cfg.interval_seconds,
            "burst must never be slower than normal"
        );
        assert_eq!(cfg.open_burst_seconds, 120);
        assert_eq!(cfg.timeout_seconds, 5);
        assert_eq!(cfg.max_consecutive_errors, 1);
        assert_eq!(cfg.ui_scale, 1.0);
    }

    // S-02：崩溃报告曾是全项目唯一绕过脱敏的落盘路径。panic payload 只要
    // 经手过服务器文本（一次 .expect(&format!(...)) 疏忽就够）就会原样落盘。
    #[test]
    fn crash_reports_are_redacted_before_they_hit_the_disk() {
        let dir = std::env::temp_dir().join(format!("cs-crash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let report = "panicked at 'server said Set-Cookie: JSESSIONID=abc123'\n\
                      context: password=hunter2 token=tk_live_1\n\
                      url: https://x.edu/eams/a?sessionid=zzz";
        let path = AppConfig::write_crash_report_in(&dir, report).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        for secret in ["abc123", "hunter2", "tk_live_1", "zzz"] {
            assert!(!written.contains(secret), "crash report leaked {secret}");
        }
        // 报告主体（定位信息）必须保留，否则脱敏等于把排障价值也删了。
        assert!(written.contains("panicked at"));
        let _ = fs::remove_dir_all(&dir);
    }

    // 折行 header、一行多对 cookie、value 在 name 之前的 input、隐藏 CSRF
    // 字段——这些都是原正则盖不到的形状。
    #[test]
    fn redaction_covers_folded_headers_multi_pair_cookies_and_hidden_tokens() {
        let folded = "Set-Cookie: JSESSIONID=abc123;\n\tPath=/; HttpOnly\nNext-Header: ok";
        let out = redact_diagnostic_text(folded);
        assert!(!out.contains("abc123"), "folded header leaked: {out}");
        assert!(out.contains("Next-Header: ok"), "over-redacted: {out}");

        let multi = "cookie line: a=1; JSESSIONID=deadbeef; theme=dark";
        let out = redact_diagnostic_text(multi);
        assert!(!out.contains("deadbeef"), "multi-pair cookie leaked: {out}");
        assert!(out.contains("theme=dark"), "over-redacted: {out}");

        let value_first = r#"<input value="s3cret" name="password">"#;
        let out = redact_diagnostic_page("p.html", value_first).unwrap();
        assert!(!out.contains("s3cret"), "value-before-name leaked: {out}");

        let csrf = r#"<input type="hidden" name="csrfToken" value="ct_9f8e7d">"#;
        let out = redact_diagnostic_page("p.html", csrf).unwrap();
        assert!(!out.contains("ct_9f8e7d"), "hidden CSRF leaked: {out}");
    }

    #[test]
    fn raw_diagnostic_pages_never_export_secrets_or_submission_forms() {
        let page = concat!(
            "Cookie: JSESSIONID=cookie-secret\n",
            "Authorization: Bearer auth-secret\n",
            "<input name='password' value='test-password'>",
            "<script>var token='token-secret'; var sessionId='session-secret';</script>"
        );
        let redacted = redact_diagnostic_page("login.html", page).unwrap();
        for secret in [
            "cookie-secret",
            "auth-secret",
            "test-password",
            "token-secret",
            "session-secret",
        ] {
            assert!(!redacted.contains(secret), "diagnostic leaked {secret}");
        }

        let submission = "<form action='stdElectCourse!batchOperator.action'><input name='operator0' value='123:true:0'></form>";
        assert!(redact_diagnostic_page("submit.html", submission).is_none());
    }

    #[test]
    fn first_save_creates_a_readable_config() {
        let dir = config_test_dir("first-save");
        let path = dir.join("config.toml");
        let cfg = AppConfig {
            username: "student01".into(),
            ..Default::default()
        };

        cfg.save_to(&path).unwrap();

        assert_eq!(AppConfig::read_from(&path).unwrap().username, "student01");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn repeated_overwrites_keep_the_latest_config_without_residue() {
        let dir = config_test_dir("overwrite");
        let path = dir.join("config.toml");
        for index in 0..20 {
            let cfg = AppConfig {
                username: format!("student-{index}"),
                ..Default::default()
            };
            cfg.save_to(&path).unwrap();
        }

        assert_eq!(AppConfig::read_from(&path).unwrap().username, "student-19");
        let residue = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path() != path)
            .collect::<Vec<_>>();
        assert!(residue.is_empty(), "unexpected save residue: {residue:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replacement_failure_preserves_original_and_removes_temp() {
        let dir = config_test_dir("failure");
        fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("config.toml");
        let temp = next_config_temp_path(&destination);
        fs::write(&destination, "original").unwrap();
        fs::write(&temp, "replacement").unwrap();

        let error = atomic_replace_with(&temp, &destination, |_, destination, backup| {
            fs::rename(destination, backup).unwrap();
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replacement failure",
            ))
        })
        .unwrap_err();

        assert_eq!(fs::read_to_string(&destination).unwrap(), "original");
        assert!(!temp.exists());
        assert_eq!(
            fs::read_to_string(temp.with_extension("bak")).unwrap(),
            "original"
        );
        let message = format!("{error:#}");
        assert!(message.contains("备份路径"));
        assert!(message.contains("injected replacement failure"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_temp_names_are_unique_and_stay_beside_destination() {
        let destination = config_test_dir("names").join("config.toml");
        let names = (0..32)
            .map(|_| {
                let destination = destination.clone();
                std::thread::spawn(move || next_config_temp_path(&destination))
            })
            .map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();

        assert_eq!(names.len(), 32);
        assert!(names
            .iter()
            .all(|path| path.parent() == destination.parent()));
        assert!(names.iter().all(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&std::process::id().to_string()))
        }));
    }
}

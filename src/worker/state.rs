use super::time::now_hms;
use crate::config::AppConfig;
use crate::eams::{EamsClient, Lesson, NetworkSnapshot};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Success,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Success => "success",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogItem {
    pub time: String,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchState {
    Queued,
    Checking,
    Full,
    Unknown,
    Electing,
    Success,
    Missing,
    Ambiguous,
    Failed,
    Stopped,
}

impl WatchState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "等待",
            Self::Checking => "查询中",
            Self::Full => "已满",
            Self::Unknown => "人数未知",
            Self::Electing => "提交中",
            Self::Success => "成功",
            Self::Missing => "未找到",
            Self::Ambiguous => "多条匹配",
            Self::Failed => "失败",
            Self::Stopped => "已停止",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatchStatus {
    pub serial: String,
    pub name: String,
    pub teachers: String,
    pub state: WatchState,
    pub detail: String,
    pub capacity: String,
    pub checks: u32,
    /// 最近一次检查时间（本地时分秒），空表示尚未检查。
    pub last_check: String,
}

pub struct SharedState {
    pub running: AtomicBool,
    /// True after stop requested until the worker guard drops.
    pub stopping: AtomicBool,
    pub logging_in: AtomicBool,
    pub refreshing: AtomicBool,
    pub logged_in: AtomicBool,
    pub revision: AtomicU64,
    pub(crate) run_generation: AtomicU64,
    /// Generation of the task that currently owns `running`.
    pub(crate) run_owner: AtomicU64,
    /// Bumped to cancel an armed scheduled start.
    pub(crate) schedule_arm_generation: AtomicU64,
    /// 会话代际：登录开始/退出/清会话时自增。刷新与保活等后台任务完成
    /// 时据此丢弃陈旧结果，避免旧账号数据写回新会话或误杀新会话。
    pub(crate) session_generation: AtomicU64,
    /// 当前精确待命的定时开抢键（YYYY-MM-DD HH:MM:SS）。
    pub(crate) schedule_armed_key: Mutex<Option<String>>,
    /// 已触发或已确认过期的定时开抢键：同一时刻只触发一次。
    pub(crate) schedule_fired_key: Mutex<Option<String>>,
    /// UI 最近一次保存的运行配置：定时到点开抢读取它而非 arm 时刻快照。
    pub(crate) latest_config: Mutex<Option<AppConfig>>,
    pub logs: Mutex<VecDeque<LogItem>>,
    pub lessons: Mutex<Vec<Lesson>>,
    pub watch: Mutex<Vec<WatchStatus>>,
    pub worker_message: Mutex<String>,
    pub profile_id: Mutex<String>,
    pub client: Mutex<Option<Arc<EamsClient>>>,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            logging_in: AtomicBool::new(false),
            refreshing: AtomicBool::new(false),
            logged_in: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            run_generation: AtomicU64::new(0),
            run_owner: AtomicU64::new(0),
            schedule_arm_generation: AtomicU64::new(0),
            session_generation: AtomicU64::new(0),
            schedule_armed_key: Mutex::new(None),
            schedule_fired_key: Mutex::new(None),
            latest_config: Mutex::new(None),
            logs: Mutex::new(VecDeque::new()),
            lessons: Mutex::new(Vec::new()),
            watch: Mutex::new(Vec::new()),
            worker_message: Mutex::new("未登录".into()),
            profile_id: Mutex::new(String::new()),
            client: Mutex::new(None),
        })
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let item = LogItem {
            time: now_hms(),
            level,
            message: message.into(),
        };
        let mut logs = self.logs.lock();
        logs.push_front(item);
        while logs.len() > 400 {
            logs.pop_back();
        }
        self.touch();
    }

    pub fn set_message(&self, message: impl Into<String>) {
        *self.worker_message.lock() = message.into();
        self.touch();
    }

    pub fn clear_session(&self, message: &str) {
        self.logged_in.store(false, Ordering::Release);
        self.running.store(false, Ordering::Release);
        self.stopping.store(false, Ordering::Release);
        self.run_generation.fetch_add(1, Ordering::AcqRel);
        self.invalidate_session_tasks();
        *self.client.lock() = None;
        self.lessons.lock().clear();
        self.set_message(message);
    }

    /// 当前会话代际，配合 `is_current_session` 保护后台任务的回写。
    pub(crate) fn session_token(&self) -> u64 {
        self.session_generation.load(Ordering::Acquire)
    }

    pub(crate) fn is_current_session(&self, token: u64) -> bool {
        self.session_generation.load(Ordering::Acquire) == token
    }

    /// 会话更替（登录开始/退出/清会话）：作废在途 refresh/keepalive 的回写。
    pub(crate) fn invalidate_session_tasks(&self) {
        self.session_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// 登记新的定时待命：自增代际使旧待命任务退出、记录键，返回持有代际。
    pub(crate) fn begin_schedule_arm(&self, key: &str) -> u64 {
        let mut armed = self.schedule_armed_key.lock();
        let arm_gen = self.schedule_arm_generation.fetch_add(1, Ordering::AcqRel) + 1;
        *armed = Some(key.to_string());
        arm_gen
    }

    /// 取消定时待命：代际自增并清空 armed 键。
    pub(crate) fn cancel_schedule_arm(&self) {
        let mut armed = self.schedule_armed_key.lock();
        self.schedule_arm_generation.fetch_add(1, Ordering::AcqRel);
        *armed = None;
    }

    /// 待命被手动开抢/登录打断时解除（不标记 fired，之后可重新待命）。
    /// 仅在代际未被新待命/取消接管时清键，避免误清接管方的状态。
    pub(crate) fn disarm_schedule_if_current(&self, arm_gen: u64) {
        let mut armed = self.schedule_armed_key.lock();
        if self.schedule_arm_generation.load(Ordering::Acquire) == arm_gen {
            *armed = None;
        }
    }

    /// 到点认领触发权：代际仍属当前待命则解除待命并把该键标记为已触发。
    /// 返回 false 表示已被取消或新的待命接管，调用方必须放弃开抢。
    pub(crate) fn claim_schedule_fire(&self, key: &str, arm_gen: u64) -> bool {
        let mut armed = self.schedule_armed_key.lock();
        if self.schedule_arm_generation.load(Ordering::Acquire) != arm_gen {
            return false;
        }
        *armed = None;
        *self.schedule_fired_key.lock() = Some(key.to_string());
        true
    }

    /// 错过触发窗口：解除待命并把该键标记为已触发（过期不补抢）。
    pub(crate) fn mark_schedule_expired(&self, key: &str) {
        let mut armed = self.schedule_armed_key.lock();
        self.schedule_arm_generation.fetch_add(1, Ordering::AcqRel);
        *armed = None;
        *self.schedule_fired_key.lock() = Some(key.to_string());
    }

    /// 用户修改开抢时刻后重置 fired 去重键，让新时刻可以再次触发。
    pub(crate) fn clear_schedule_fired(&self) {
        *self.schedule_fired_key.lock() = None;
    }

    pub(crate) fn schedule_armed_matches(&self, key: &str) -> bool {
        self.schedule_armed_key.lock().as_deref() == Some(key)
    }

    pub(crate) fn schedule_fired_matches(&self, key: &str) -> bool {
        self.schedule_fired_key.lock().as_deref() == Some(key)
    }

    /// 发布最新运行配置：定时到点开抢用它，而不是 arm 时刻的快照。
    pub(crate) fn publish_config(&self, cfg: AppConfig) {
        *self.latest_config.lock() = Some(cfg);
    }

    /// Returns the current in-memory network health for the active session.
    ///
    /// Network metrics are intentionally not persisted: they contain only aggregate
    /// request behaviour for this run and reset when the user logs out.
    pub fn network_snapshot(&self) -> NetworkSnapshot {
        let client = self.client.lock().clone();
        client.map_or_else(NetworkSnapshot::default, |client| client.network_snapshot())
    }

    pub(crate) fn touch(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn is_current_run(&self, generation: u64) -> bool {
        // Generation alone is the cancel token. `running` may stay true while Stopping.
        self.run_generation.load(Ordering::Acquire) == generation
    }

    pub(crate) fn claim_run(&self) -> Option<u64> {
        if self.running.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.stopping.store(false, Ordering::Release);
        let generation = self.run_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.run_owner.store(generation, Ordering::Release);
        Some(generation)
    }

    pub(crate) fn release_run_if_owner(&self, generation: u64) {
        if self.run_owner.load(Ordering::Acquire) == generation {
            self.running.store(false, Ordering::Release);
            self.stopping.store(false, Ordering::Release);
        }
    }

    #[allow(dead_code)]
    pub fn is_busy(&self) -> bool {
        self.running.load(Ordering::Acquire)
            || self.logging_in.load(Ordering::Acquire)
            || self.refreshing.load(Ordering::Acquire)
    }
}

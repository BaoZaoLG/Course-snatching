use super::time::now_hms;
use crate::eams::{EamsClient, Lesson};
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
        *self.client.lock() = None;
        self.lessons.lock().clear();
        self.set_message(message);
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

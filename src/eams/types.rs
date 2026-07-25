use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    RateLimited,
    Timeout,
    Server,
    Transport,
    AuthExpired,
    HttpClient,
    ResponseTooLarge,
    Parse,
    Business,
    Unknown,
}

impl BackendErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "服务器限流",
            Self::Timeout => "请求超时",
            Self::Server => "服务器异常",
            Self::Transport => "网络连接异常",
            Self::AuthExpired => "登录失效",
            Self::HttpClient => "请求被拒绝",
            Self::ResponseTooLarge => "响应过大",
            Self::Parse => "响应解析失败",
            Self::Business => "业务结果",
            Self::Unknown => "未知异常",
        }
    }

    pub(crate) fn needs_backoff(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Timeout | Self::Server | Self::Transport
        )
    }
}

impl std::fmt::Display for BackendErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitStatus {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryAdvice {
    None,
    Cooldown(Duration),
    CircuitOpen(Duration),
}

#[derive(Debug, Clone)]
pub struct NetworkSnapshot {
    pub requests_per_second: f64,
    pub latency_ewma_ms: Option<f64>,
    pub total_rate_limits: u64,
    pub consecutive_errors: u32,
    pub cooldown_remaining: Duration,
    pub last_error_kind: Option<BackendErrorKind>,
    pub circuit_status: CircuitStatus,
}

impl Default for NetworkSnapshot {
    fn default() -> Self {
        Self {
            requests_per_second: 0.0,
            latency_ewma_ms: None,
            total_rate_limits: 0,
            consecutive_errors: 0,
            cooldown_remaining: Duration::ZERO,
            last_error_kind: None,
            circuit_status: CircuitStatus::Closed,
        }
    }
}

#[derive(Debug, Error)]
pub enum EamsError {
    #[error("登录已失效，请重新登录")]
    AuthExpired,
    #[error("教务服务器限流：{message}")]
    RateLimited {
        message: String,
        /// 服务器 Retry-After（秒）；无则由上层自行退避。
        retry_after_secs: Option<u64>,
    },
    #[error("教务服务器返回 HTTP {status}: {summary}")]
    HttpStatus { status: u16, summary: String },
    #[error("{kind}: {message}")]
    Network {
        kind: BackendErrorKind,
        message: String,
    },
    #[error("响应解析失败：{message}")]
    Parse { message: String },
    #[error("业务请求被拒绝：{message}")]
    Business { message: String },
    #[error("服务器响应过大（超过 {0} MiB）")]
    ResponseTooLarge(usize),
    #[error("教务地址必须使用 HTTPS（本机测试地址除外）")]
    InsecureBaseUrl,
}

pub fn backend_error_kind(error: &anyhow::Error) -> BackendErrorKind {
    error
        .chain()
        .find_map(|cause| {
            cause.downcast_ref::<EamsError>().map(|error| match error {
                EamsError::AuthExpired => BackendErrorKind::AuthExpired,
                EamsError::RateLimited { .. } => BackendErrorKind::RateLimited,
                EamsError::HttpStatus { status, .. } if *status >= 500 => BackendErrorKind::Server,
                EamsError::HttpStatus { .. } => BackendErrorKind::HttpClient,
                EamsError::Network { kind, .. } => *kind,
                EamsError::Parse { .. } => BackendErrorKind::Parse,
                EamsError::Business { .. } => BackendErrorKind::Business,
                EamsError::ResponseTooLarge(_) => BackendErrorKind::ResponseTooLarge,
                EamsError::InsecureBaseUrl => BackendErrorKind::HttpClient,
            })
        })
        .unwrap_or(BackendErrorKind::Unknown)
}

pub fn is_auth_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<EamsError>()
            .is_some_and(|e| matches!(e, EamsError::AuthExpired))
    })
}

pub fn is_rate_limit_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<EamsError>().is_some_and(|e| {
            matches!(
                e,
                EamsError::RateLimited { .. } | EamsError::HttpStatus { status: 429, .. }
            )
        })
    })
}

/// 从错误链中提取 Retry-After（若服务器提供）。
pub fn rate_limit_retry_after(error: &anyhow::Error) -> Option<Duration> {
    error.chain().find_map(|cause| {
        cause.downcast_ref::<EamsError>().and_then(|e| match e {
            EamsError::RateLimited {
                retry_after_secs: Some(secs),
                ..
            } => Some(Duration::from_secs((*secs).clamp(1, 120))),
            _ => None,
        })
    })
}

/// 教学班余量状态：区分“未知”和“已满/有余量”，避免 0/0 或缺少 limit 被误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatInfo {
    /// 人数接口未返回可信容量（含 limit 缺失、0 占位）。
    Unknown,
    /// 已解析到容量（limit > 0）。
    Known { selected: u32, limit: u32 },
}

impl SeatInfo {
    pub fn from_counts(selected: Option<u32>, limit: Option<u32>) -> Self {
        match limit {
            Some(limit) if limit > 0 => Self::Known {
                selected: selected.unwrap_or(0),
                limit,
            },
            _ => Self::Unknown,
        }
    }

    pub fn capacity_text(self) -> String {
        match self {
            Self::Unknown => "-".into(),
            Self::Known { selected, limit } => format!("{selected}/{limit}"),
        }
    }

    pub fn is_known(self) -> bool {
        matches!(self, Self::Known { .. })
    }

    pub fn has_seat(self) -> bool {
        matches!(self, Self::Known { selected, limit } if selected < limit)
    }

    pub fn is_full(self) -> bool {
        matches!(self, Self::Known { selected, limit } if selected >= limit)
    }

    pub fn selected(self) -> Option<u32> {
        match self {
            Self::Known { selected, .. } => Some(selected),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lesson {
    pub id: String,
    pub no: String,
    pub name: String,
    pub teachers: String,
    pub seat: SeatInfo,
}

impl Lesson {
    pub fn capacity_text(&self) -> String {
        self.seat.capacity_text()
    }

    pub fn capacity_known(&self) -> bool {
        self.seat.is_known()
    }

    pub fn has_seat(&self) -> bool {
        self.seat.has_seat()
    }
}

#[derive(Debug, Clone)]
pub enum ElectResult {
    Success { detail: String },
    Full { detail: String },
    Failed { detail: String },
}

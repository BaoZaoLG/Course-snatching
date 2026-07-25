use std::time::Duration;
use thiserror::Error;

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
    #[error("服务器响应过大（超过 {0} MiB）")]
    ResponseTooLarge(usize),
    #[error("教务地址必须使用 HTTPS（本机测试地址除外）")]
    InsecureBaseUrl,
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
            } => Some(Duration::from_secs((*secs).clamp(1, 300))),
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

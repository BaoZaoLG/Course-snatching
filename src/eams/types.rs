use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    RateLimited,
    Timeout,
    Server,
    Transport,
    /// 连接建立失败（DNS 解析失败、连接被拒、连接重置、代理错误）。
    Connect,
    /// TLS 握手或证书校验失败。
    Tls,
    /// 重定向被策略拒绝（跨域跳转、跳数超限）。
    Redirect,
    /// 响应体解码失败（gzip 损坏、字符集无法解码）。
    Decode,
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
            // 这些分类的全部意义在于让用户报障时能说清是哪一类：
            // 「学校证书过期」「校园网 DNS 劫持」「被 WAF 掐连接」以前
            // 全都塌缩成同一句「网络连接异常」。
            Self::Connect => "无法建立连接",
            Self::Tls => "HTTPS 证书或握手失败",
            Self::Redirect => "跳转被拒绝",
            Self::Decode => "响应解码失败",
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
            Self::RateLimited
                | Self::Timeout
                | Self::Server
                | Self::Transport
                | Self::Connect
                | Self::Tls
                | Self::Decode
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
        /// 保留 reqwest 的原始错误，`{error:#}` 的 anyhow 链会自动带上根因。
        /// 这是一个「用户在自己电脑上跑、开发者拿不到现场」的桌面工具，
        /// 错误文本就是唯一的诊断入口。
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
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

/// 被重定向策略拦下的跨域跳转，且目标看起来是 SSO 登录端点。
///
/// 单独一个类型是为了能在传输层把它下钻回来映射成 `AuthExpired`——
/// 靠错误文本匹配太脆。
#[derive(Debug, Error)]
#[error("会话已过期，服务器要求跳转到统一身份认证：{0}")]
pub(crate) struct SsoRedirectBlocked(pub String);

/// 目标 URL 看起来是统一身份认证的登录端点。
pub(crate) fn looks_like_sso_endpoint(url: &reqwest::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    ["/cas", "/authserver", "/sso", "/login", "/idp", "/oauth"]
        .iter()
        .any(|marker| path.contains(marker))
        || ["cas.", "sso.", "authserver.", "id.", "passport."]
            .iter()
            .any(|marker| host.starts_with(marker))
}

/// 错误链里是否有被拦下的 SSO 跳转。
pub(crate) fn sso_redirect_target(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current = Some(error);
    while let Some(err) = current {
        if let Some(blocked) = err.downcast_ref::<SsoRedirectBlocked>() {
            return Some(blocked.0.clone());
        }
        current = err.source();
    }
    None
}

/// 把 reqwest 的错误分成可诊断的类别，并抽出最深一层根因文本。
///
/// 原实现只用 `is_timeout()` 判一次就把 error 丢掉：DNS 失败、TLS 证书错误、
/// 连接被拒、连接重置、代理错误、重定向被拒全部塌缩成同一句「请求 xxx 失败」。
pub(crate) fn classify_reqwest_error(error: &reqwest::Error) -> (BackendErrorKind, String) {
    let cause = deepest_cause(error);
    let lowered = cause.to_ascii_lowercase();
    // reqwest 不区分 TLS，只能看根因文本。证书/握手类问题的处置和普通连接
    // 失败不同（用户要去检查系统时间或学校证书），值得单独一档。
    let tls = ["certificate", "tls", "handshake", "cert", "invalidcert"]
        .iter()
        .any(|marker| lowered.contains(marker));
    let kind = if error.is_timeout() {
        BackendErrorKind::Timeout
    } else if error.is_redirect() {
        BackendErrorKind::Redirect
    } else if tls {
        BackendErrorKind::Tls
    } else if error.is_connect() {
        BackendErrorKind::Connect
    } else if error.is_decode() {
        BackendErrorKind::Decode
    } else {
        BackendErrorKind::Transport
    };
    (kind, cause)
}

fn deepest_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut current = error;
    while let Some(next) = current.source() {
        current = next;
    }
    current.to_string()
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
/// 上限与解析层 parse_retry_after_secs 保持一致（300s）：服务器要求的
/// 长冷却被截短会导致提前重试，反而加重封禁风险。
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
    Success {
        detail: String,
    },
    Full {
        detail: String,
    },
    /// 服务器瞬态繁忙或结果暂不可判定（“系统繁忙，请稍后再试”等）：
    /// 非终态，目标保留在监控队列中下一轮重试。
    Busy {
        detail: String,
    },
    Failed {
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性返回 302 的本地 mock：用于确定性地触发重定向策略拒绝。
    fn serve_redirect_to(location: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/eams/")
    }

    /// 接受连接但从不回应：确定性地触发超时。
    fn serve_silent() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let held = listener.accept();
            // 握住连接直到测试结束，什么都不写。
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        format!("http://{address}/eams/")
    }

    fn rate_limited(retry_after_secs: Option<u64>) -> anyhow::Error {
        anyhow::Error::new(EamsError::RateLimited {
            message: "限流".into(),
            retry_after_secs,
        })
    }

    // C-08：不同的传输失败必须落到不同的类别，且根因文本要保留下来。
    // 用户报「网络连接异常」时，开发者拿不到现场，错误文本是唯一线索。
    #[tokio::test(flavor = "current_thread")]
    async fn transport_failures_are_classified_and_keep_their_root_cause() {
        // 用本地 socket 制造确定性的失败，不依赖外网/DNS/防火墙行为。
        // 重定向被策略拒绝：必须归到 Redirect，而不是笼统的 Transport——
        // 这正是 CAS/统一身份认证站点会话过期时走的那条路径。
        let base = serve_redirect_to("https://cas.other.example/login");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                attempt.error("blocked cross-origin redirect")
            }))
            .build()
            .unwrap();
        let error = client
            .get(&base)
            .send()
            .await
            .expect_err("cross-origin redirect must be refused");
        let (kind, cause) = classify_reqwest_error(&error);
        assert_eq!(kind, BackendErrorKind::Redirect, "cause={cause}");
        assert!(!cause.is_empty(), "root cause must not be discarded");

        // 超时仍然是超时，不能被新分类抢走。
        let base = serve_silent();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let error = client
            .get(&base)
            .send()
            .await
            .expect_err("silent server must time out");
        let (kind, _) = classify_reqwest_error(&error);
        assert_eq!(kind, BackendErrorKind::Timeout);

        // 细分出来的类别必须仍然算作「需要退避的网络失败」。
        for kind in [
            BackendErrorKind::Connect,
            BackendErrorKind::Tls,
            BackendErrorKind::Decode,
        ] {
            assert!(kind.needs_backoff(), "{kind:?} must back off");
        }
        // 且每一档都有自己的中文标签，不再共用「网络连接异常」。
        let labels = [
            BackendErrorKind::Connect.label(),
            BackendErrorKind::Tls.label(),
            BackendErrorKind::Redirect.label(),
            BackendErrorKind::Decode.label(),
            BackendErrorKind::Transport.label(),
        ];
        let unique = labels.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), labels.len(), "labels must be distinguishable");
    }

    #[test]
    fn retry_after_extraction_honors_the_parse_layer_ceiling() {
        assert_eq!(
            rate_limit_retry_after(&rate_limited(Some(7))),
            Some(Duration::from_secs(7))
        );
        // 服务器要求 300s 必须完整兑现，不能再被截断到 120s。
        assert_eq!(
            rate_limit_retry_after(&rate_limited(Some(300))),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            rate_limit_retry_after(&rate_limited(Some(9_999))),
            Some(Duration::from_secs(300))
        );
        assert_eq!(rate_limit_retry_after(&rate_limited(None)), None);
    }
}

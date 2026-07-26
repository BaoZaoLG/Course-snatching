//! Shared HTTP transport, session cookie handling, and request governance.

use super::parse::{
    decode_body, extract_login_error, looks_like_login_page, normalize_base, origin_key,
    parse_retry_after_secs, read_body_limited, summarize_html,
};
use super::types::{
    classify_reqwest_error, looks_like_sso_endpoint, sso_redirect_target, SsoRedirectBlocked,
};
use super::{
    backend_error_kind, rate_limit_retry_after, BackendErrorKind, EamsClient, EamsError,
    ProfileContext, RequestGovernor, RequestPriority, ResponseHandling, MAX_RESPONSE_BYTES, UA,
};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Url};
use std::collections::HashMap;
use std::time::Duration;

impl EamsClient {
    pub fn new(base_url: &str, timeout_secs: u64, debug_dump_enabled: bool) -> Result<Self> {
        let base = normalize_base(base_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(UA));
        let allowed_origin = origin_key(&base);
        let governor = RequestGovernor::shared_for_origin(&allowed_origin);
        let http = Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            // SYN 丢包不该独占整个总超时预算：5s 建不起连接就放弃重试，
            // 保住冲刺窗口（默认 20s）内的剩余机会。
            .connect_timeout(Duration::from_secs(5))
            // 低于常见 Tomcat keepAliveTimeout（20-60s）：长退避/熔断冷却后
            // 不复用可能已被服务端关闭的陈旧连接，避免恢复后的第一发
            // 提交先吃一个连接错误。
            .pool_idle_timeout(Duration::from_secs(15))
            .redirect(Policy::custom(move |attempt| {
                if attempt.previous().len() >= 12 {
                    return attempt.error("too many redirects");
                }
                if origin_key(attempt.url()) == allowed_origin {
                    attempt.follow()
                } else if let Some(target) =
                    looks_like_sso_endpoint(attempt.url()).then(|| attempt.url().to_string())
                {
                    // 拦截策略本身是对的（cookie 不能跟着跑到第三方），但错误
                    // 映射不能把它当成普通网络故障：国内高校 EAMS 前面基本都挂
                    // CAS/统一身份认证，会话过期时返回的正是跳到另一 origin 的
                    // SSO 登录页。归成 Transport 的话，登录失效检测在最常见的
                    // 部署形态下整体失效——用户只会看到无限「网络异常重试中」。
                    attempt.error(SsoRedirectBlocked(target))
                } else {
                    attempt.error("blocked cross-origin redirect")
                }
            }))
            .build()?;
        Ok(Self {
            http,
            base,
            debug_dump_enabled,
            profile_context: Mutex::new(HashMap::<String, ProfileContext>::new()),
            governor,
            catalog_strategy: Mutex::new(None),
            strategy_changes: Mutex::new(Vec::new()),
            elected_endpoint: Mutex::new(None),
        })
    }

    pub(super) async fn send_raw_text(
        &self,
        request: RequestBuilder,
        action: &str,
        allow_login_page: bool,
    ) -> Result<(Url, String)> {
        self.send_raw_text_with_priority(
            request,
            action,
            ResponseHandling::Standard { allow_login_page },
            RequestPriority::Session,
        )
        .await
    }

    pub(super) async fn send_raw_text_with_priority(
        &self,
        request: RequestBuilder,
        action: &str,
        response_handling: ResponseHandling,
        priority: RequestPriority,
    ) -> Result<(Url, String)> {
        let permit = self.governor.acquire(priority).await;
        // 对时是被动的：每个响应都带 Date 头，零额外请求。
        let sent_at = std::time::Instant::now();
        let result = async {
            let response = request.send().await.map_err(|error| {
                // 被拦下的 SSO 跳转 = 会话过期，必须走登录失效分支而不是网络重试。
                if sso_redirect_target(&error).is_some() {
                    return EamsError::AuthExpired;
                }
                let (kind, cause) = classify_reqwest_error(&error);
                EamsError::Network {
                    kind,
                    message: format!("{action}失败：{cause}"),
                    source: Some(Box::new(error)),
                }
            })?;
            // 一收到响应头就对时：此刻的 Instant 离服务器生成 Date 最近，
            // 读体、解码、解析都会污染 RTT 估计。
            super::clock::ClockSync::global().observe(
                sent_at,
                std::time::Instant::now(),
                response
                    .headers()
                    .get(reqwest::header::DATE)
                    .and_then(|value| value.to_str().ok()),
            );
            let status = response.status();
            let final_url = response.url().clone();
            let retry_after_secs = parse_retry_after_secs(response.headers());
            // 响应体被 read_body_limited 消费掉之前先留下字符集声明。
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(EamsError::ResponseTooLarge(MAX_RESPONSE_BYTES / 1024 / 1024).into());
            }
            let bytes = read_body_limited(response, MAX_RESPONSE_BYTES)
                .await
                .map_err(|error| {
                    if backend_error_kind(&error) != BackendErrorKind::Unknown {
                        error.context(format!("读取{action}响应失败"))
                    } else {
                        let (kind, cause) = error
                            .chain()
                            .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
                            .map_or(
                                (BackendErrorKind::Transport, error.to_string()),
                                classify_reqwest_error,
                            );
                        anyhow::Error::new(EamsError::Network {
                            kind,
                            message: format!("读取{action}响应失败：{cause}"),
                            source: None,
                        })
                    }
                })?;
            let text = decode_body(content_type.as_deref(), &bytes)
                .with_context(|| format!("{action}响应"))?;
            let code = status.as_u16();
            if code == 429 || (code == 503 && retry_after_secs.is_some()) {
                let summary = summarize_html(&text);
                return Err(EamsError::RateLimited {
                    message: if summary.is_empty() {
                        format!("HTTP {code}")
                    } else {
                        summary
                    },
                    retry_after_secs,
                }
                .into());
            }
            if !status.is_success() {
                return Err(EamsError::HttpStatus {
                    status: code,
                    summary: summarize_html(&text),
                }
                .into());
            }
            let is_login_page = looks_like_login_page(&final_url, &text);
            match response_handling {
                ResponseHandling::LoginSubmission if is_login_page => {
                    let reason = extract_login_error(&text)
                        .unwrap_or_else(|| "账号或密码错误，或需要验证码".into());
                    if ["过快", "太快", "频繁"]
                        .iter()
                        .any(|marker| reason.contains(marker))
                    {
                        return Err(EamsError::RateLimited {
                            message: reason,
                            retry_after_secs,
                        }
                        .into());
                    }
                    return Err(EamsError::Business { message: reason }.into());
                }
                ResponseHandling::Standard {
                    allow_login_page: false,
                } if is_login_page => return Err(EamsError::AuthExpired.into()),
                _ => {}
            }
            Ok((final_url, text))
        }
        .await;
        match &result {
            Ok(_) => self.governor.record_success(&permit),
            Err(error) => {
                let _ = self.governor.record_failure(
                    &permit,
                    backend_error_kind(error),
                    rate_limit_retry_after(error),
                );
            }
        }
        result
    }

    pub(super) async fn send_text(&self, request: RequestBuilder, action: &str) -> Result<String> {
        self.send_raw_text(request, action, false)
            .await
            .map(|(_, text)| text)
    }

    pub(super) async fn send_text_with_priority(
        &self,
        request: RequestBuilder,
        action: &str,
        priority: RequestPriority,
    ) -> Result<String> {
        self.send_raw_text_with_priority(
            request,
            action,
            ResponseHandling::Standard {
                allow_login_page: false,
            },
            priority,
        )
        .await
        .map(|(_, text)| text)
    }
}

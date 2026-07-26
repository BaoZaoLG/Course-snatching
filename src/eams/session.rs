//! Login and authenticated-session lifecycle.

use super::parse::{extract_password_salt, sha1_password};
use super::{
    backend_error_kind, BackendErrorKind, EamsClient, EamsError, RequestPriority, ResponseHandling,
};
use anyhow::{anyhow, Context, Result};
use reqwest::header::{CONTENT_TYPE, REFERER};
use std::time::Duration;
use zeroize::Zeroizing;

impl EamsClient {
    /// 程序内登录：GET 登录页取 salt + cookie，再 POST 加密密码。
    pub async fn login(&self, username: &str, password: &str) -> Result<()> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            anyhow::bail!("账号和密码不能为空");
        }

        let mut last_error: Option<anyhow::Error> = None;
        for attempt in 1..=4 {
            match self.login_once(username, password).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let kind = backend_error_kind(&error);
                    let rate_limited = kind == BackendErrorKind::RateLimited
                        || (kind == BackendErrorKind::Unknown
                            && ["过快", "太快", "频繁", "稍后"]
                                .iter()
                                .any(|marker| error.to_string().contains(marker)));
                    if !rate_limited || self.circuit_is_open() {
                        return Err(error);
                    }
                    last_error = Some(error);
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_millis(1200 * attempt as u64)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("登录失败")))
    }

    async fn login_once(&self, username: &str, password: &str) -> Result<()> {
        let login_url = self.url("loginExt.action")?;
        let (_, login_html) = self
            .send_raw_text(
                self.http
                    .get(login_url.clone())
                    .header(REFERER, self.base.as_str()),
                "打开登录页",
                true,
            )
            .await?;
        let salt = extract_password_salt(&login_html).ok_or_else(|| EamsError::Parse {
            message: "无法从登录页解析密码 salt，页面可能已改版".into(),
        })?;
        tokio::time::sleep(Duration::from_millis(800)).await;

        let encoded = Zeroizing::new(sha1_password(salt.as_str(), password));
        let form = [
            ("username", username),
            ("password", encoded.as_str()),
            ("session_locale", "zh_CN"),
        ];
        self.send_raw_text_with_priority(
            self.http
                .post(login_url.clone())
                .header(REFERER, login_url.as_str())
                .header(
                    CONTENT_TYPE,
                    "application/x-www-form-urlencoded; charset=UTF-8",
                )
                .form(&form),
            "提交登录",
            ResponseHandling::LoginSubmission,
            RequestPriority::Session,
        )
        .await?;
        self.ensure_logged_in()
            .await
            .context("登录后会话无效，请重试")
    }

    pub async fn ensure_logged_in(&self) -> Result<()> {
        let home = self.url("homeExt.action")?;
        self.send_text(self.http.get(home), "验证登录状态").await?;
        Ok(())
    }
}

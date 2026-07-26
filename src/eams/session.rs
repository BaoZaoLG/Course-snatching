//! Login and authenticated-session lifecycle.

use super::parse::{
    extract_captcha, extract_password_salt, login_page_mentions_salt, origin_key,
    read_body_limited, sha1_password,
};
use super::{
    backend_error_kind, BackendErrorKind, EamsClient, EamsError, RequestPriority, ResponseHandling,
};
use anyhow::{anyhow, bail, Context, Result};
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
                        // 与另外两处退避共用 decorrelated jitter：开抢瞬间被限流的
                        // 学生会同时进入登录重试，确定性的 1.2/2.4/3.6s 阶梯会让
                        // 他们整齐地再撞一次。
                        tokio::time::sleep(super::backoff_for_attempt(attempt)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("登录失败")))
    }

    async fn login_once(&self, username: &str, password: &str) -> Result<()> {
        self.login_once_with_captcha(username, password, None).await
    }

    /// 带验证码答案的单次登录。
    ///
    /// `captcha` 为 `None` 且登录页要求验证码时，返回 `EamsError::CaptchaRequired`
    /// 并附上图片字节，由界面弹窗让用户手填——不做 OCR，一次性输入即可。
    pub(super) async fn login_once_with_captcha(
        &self,
        username: &str,
        password: &str,
        captcha: Option<&str>,
    ) -> Result<()> {
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
            // 区分「没有这套机制」与「有但格式不认识」：前者重试多少次都没用，
            // 后者是解析层要跟进的换版。
            message: if login_page_mentions_salt(&login_html) {
                "登录页的密码加盐格式无法识别（页面已改版），请提交诊断包反馈".into()
            } else {
                "该教务地址的登录页不使用本程序支持的加盐登录方式，请确认地址是否正确".into()
            },
        })?;

        // 验证码要素只从页面本身取，绝不猜测端点；没有就是没有。
        let challenge = extract_captcha(&login_html);
        if let Some(challenge) = &challenge {
            if captcha.is_none() {
                let image = self.fetch_captcha_image(&challenge.image_src).await?;
                return Err(EamsError::CaptchaRequired {
                    image,
                    field_name: challenge.field_name.clone(),
                }
                .into());
            }
        }

        tokio::time::sleep(Duration::from_millis(800)).await;

        let encoded = Zeroizing::new(sha1_password(salt.as_str(), password));
        let mut form = vec![
            ("username", username),
            ("password", encoded.as_str()),
            ("session_locale", "zh_CN"),
        ];
        // 只有页面真的要验证码时才带上这个字段。
        if let (Some(challenge), Some(answer)) = (&challenge, captcha) {
            form.push((challenge.field_name.as_str(), answer));
        }
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

    /// 取验证码图片。
    ///
    /// 地址来自登录页，解析成绝对 URL 后必须仍是同源——否则就是页面被改过
    /// 或被注入，宁可失败也不去请求第三方。
    async fn fetch_captcha_image(&self, src: &str) -> Result<Vec<u8>> {
        let url = self.base.join(src).context("验证码图片地址无法解析")?;
        if origin_key(&url) != origin_key(&self.base) {
            bail!("验证码图片指向其它站点，已拒绝加载");
        }
        let permit = self.governor.acquire(RequestPriority::Session).await;
        let result = async {
            let response = self.http.get(url).send().await?;
            let bytes = read_body_limited(response, 2 * 1024 * 1024).await?;
            anyhow::Ok(bytes)
        }
        .await;
        match &result {
            Ok(_) => self.governor.record_success(&permit),
            Err(error) => {
                let _ = self
                    .governor
                    .record_failure(&permit, backend_error_kind(error), None);
            }
        }
        result.context("获取验证码图片失败")
    }

    /// 用户填完验证码后重新提交登录。
    pub async fn login_with_captcha(
        &self,
        username: &str,
        password: &str,
        captcha: &str,
    ) -> Result<()> {
        self.login_once_with_captcha(username, password, Some(captcha))
            .await
    }

    pub async fn ensure_logged_in(&self) -> Result<()> {
        let home = self.url("homeExt.action")?;
        self.send_text(self.http.get(home), "验证登录状态").await?;
        Ok(())
    }
}

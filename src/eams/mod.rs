//! 教务系统客户端：登录、课程拉取、选课提交。

mod governor;
mod parse;
mod types;

use governor::{RequestGovernor, RequestPriority};
pub use types::{
    backend_error_kind, is_auth_error, is_rate_limit_error, rate_limit_retry_after,
    BackendErrorKind, CircuitStatus, EamsError, ElectResult, Lesson, NetworkSnapshot, SeatInfo,
};

use crate::eams::parse::{
    classify_elect_response, extract_all_profile_ids, extract_login_error, extract_password_salt,
    extract_profiles_detailed, extract_project_semester, looks_like_login_page,
    merge_lesson_counts, normalize_base, origin_key, page_looks_like_elect_ui,
    parse_lessons_from_html_table, parse_lessons_from_page, parse_lessons_js_like,
    parse_lessons_json, parse_retry_after_secs, plausible_profile_id, read_body_limited,
    save_debug_text, score_elect_page, sha1_password, summarize_html, validate_numeric_id,
    VerifyOutcome,
};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, REFERER, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Url};
use std::collections::HashMap;
use std::time::Duration;
use zeroize::Zeroizing;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36 Course-snatching/0.10";

const MAX_RESPONSE_BYTES: usize = 12 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct ProfileContext {
    project_id: Option<String>,
    semester_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum ResponseHandling {
    Standard { allow_login_page: bool },
    LoginSubmission,
}

pub struct EamsClient {
    http: Client,
    base: Url,
    debug_dump_enabled: bool,
    profile_context: Mutex<HashMap<String, ProfileContext>>,
    governor: std::sync::Arc<RequestGovernor>,
}

impl EamsClient {
    pub fn new(base_url: &str, timeout_secs: u64, debug_dump_enabled: bool) -> Result<Self> {
        let base = normalize_base(base_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(UA));
        let allowed_origin = origin_key(&base);
        let http = Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .redirect(Policy::custom(move |attempt| {
                if attempt.previous().len() >= 12 {
                    return attempt.error("too many redirects");
                }
                if origin_key(attempt.url()) == allowed_origin {
                    attempt.follow()
                } else {
                    attempt.error("blocked cross-origin redirect")
                }
            }))
            .build()?;
        Ok(Self {
            http,
            base,
            debug_dump_enabled,
            profile_context: Mutex::new(HashMap::new()),
            governor: RequestGovernor::new(),
        })
    }

    async fn send_raw_text(
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

    async fn send_raw_text_with_priority(
        &self,
        request: RequestBuilder,
        action: &str,
        response_handling: ResponseHandling,
        priority: RequestPriority,
    ) -> Result<(Url, String)> {
        let permit = self.governor.acquire(priority).await;
        let result = async {
            let response = request.send().await.map_err(|error| EamsError::Network {
                kind: if error.is_timeout() {
                    BackendErrorKind::Timeout
                } else {
                    BackendErrorKind::Transport
                },
                message: format!("{action}失败"),
            })?;
            let status = response.status();
            let final_url = response.url().clone();
            let retry_after_secs = parse_retry_after_secs(response.headers());
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(EamsError::ResponseTooLarge(MAX_RESPONSE_BYTES / 1024 / 1024).into());
            }
            let bytes = read_body_limited(response, MAX_RESPONSE_BYTES)
                .await
                .map_err(|error| {
                    // `Response::chunk` can fail after headers have arrived.  Keep
                    // that failure in the same typed path as send-time failures so
                    // the governor can apply its network backoff consistently.
                    if backend_error_kind(&error) != BackendErrorKind::Unknown {
                        error.context(format!("读取{action}响应失败"))
                    } else {
                        let kind = error
                            .chain()
                            .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
                            .map_or(BackendErrorKind::Transport, |cause| {
                                if cause.is_timeout() {
                                    BackendErrorKind::Timeout
                                } else {
                                    BackendErrorKind::Transport
                                }
                            });
                        anyhow::Error::new(EamsError::Network {
                            kind,
                            message: format!("读取{action}响应失败"),
                        })
                    }
                })?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
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

    async fn send_text(&self, request: RequestBuilder, action: &str) -> Result<String> {
        self.send_raw_text(request, action, false)
            .await
            .map(|(_, text)| text)
    }

    async fn send_text_with_priority(
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

    pub fn network_snapshot(&self) -> NetworkSnapshot {
        self.governor.snapshot()
    }

    pub fn set_burst_mode(&self, enabled: bool) {
        self.governor.set_burst_mode(enabled);
    }

    pub fn circuit_is_open(&self) -> bool {
        self.governor.circuit_is_open()
    }

    pub fn matches_base_url(&self, raw: &str) -> bool {
        normalize_base(raw).is_ok_and(|url| url == self.base)
    }

    fn save_debug(&self, name: &str, content: &str) -> bool {
        self.debug_dump_enabled && save_debug_text(name, content).is_ok()
    }

    fn url(&self, path: &str) -> Result<Url> {
        if path.starts_with("http://") || path.starts_with("https://") {
            bail!("内部请求路径不能是绝对 URL");
        }
        let p = path.trim_start_matches('/');
        Ok(self.base.join(p)?)
    }

    /// 程序内登录：GET 登录页取 salt + cookie，再 POST 加密密码
    /// 教务有“请不要过快点击”限流，因此内置等待与自动重试。
    pub async fn login(&self, username: &str, password: &str) -> Result<()> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            bail!("账号和密码不能为空");
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
                    if !rate_limited {
                        return Err(error);
                    }
                    if self.circuit_is_open() {
                        return Err(error);
                    }
                    last_error = Some(error);
                    if attempt < 4 {
                        let wait_ms = 1200u64 * attempt as u64;
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("登录失败")))
    }

    async fn login_once(&self, username: &str, password: &str) -> Result<()> {
        let login_url = self.url("loginExt.action")?;

        // 1) 打开登录页，拿到 JSESSIONID + 动态 salt
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

        // 模拟人工：打开页面后稍等再提交，避免被当成连点
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Hash salt and password in pieces so the password is never materialised
        // into an intermediate non-zeroizing String.
        let encoded = Zeroizing::new(sha1_password(salt.as_str(), password));
        let form = [
            ("username", username),
            ("password", encoded.as_str()),
            ("session_locale", "zh_CN"),
        ];

        // 2) 提交登录
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

        // 再访问一次首页确认会话
        self.ensure_logged_in()
            .await
            .context("登录后会话无效，请重试")?;
        Ok(())
    }

    pub async fn ensure_logged_in(&self) -> Result<()> {
        let home = self.url("homeExt.action")?;
        self.send_text(self.http.get(home), "验证登录状态").await?;
        Ok(())
    }

    /// 解析当前可选课轮次。返回 (id, 备注说明)
    pub async fn list_profiles(&self) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        let push = |id: String, note: String, out: &mut Vec<(String, String)>| {
            if !id.is_empty() && !out.iter().any(|(existing, _)| existing == &id) {
                out.push((id, note));
            }
        };

        let paths = [
            "stdElectCourse.action",
            "stdElectCourse!defaultPage.action",
            "stdElectCourse!innerIndex.action",
            "stdElectCourse!elect.action",
            "stdElectCourse!index.action",
            "homeExt.action",
            "home.action",
        ];

        let mut debug_blob = String::new();
        let mut successful_requests = 0usize;
        let mut last_error: Option<anyhow::Error> = None;
        for path in paths {
            match self
                .get_text_with_priority(path, RequestPriority::Session)
                .await
            {
                Ok(text) => {
                    successful_requests += 1;
                    debug_blob.push_str(&format!("\n===== {path} len={} =====\n", text.len()));
                    let sample: String = text.chars().take(2500).collect();
                    debug_blob.push_str(&sample);
                    for (id, note) in extract_profiles_detailed(&text) {
                        push(id, note, &mut out);
                    }
                    for id in extract_all_profile_ids(&text) {
                        push(id, format!("来自 {path}"), &mut out);
                    }
                    if out.is_empty()
                        && path == "stdElectCourse!defaultPage.action"
                        && page_looks_like_elect_ui(&text)
                    {
                        push("0".into(), "会话默认轮次(无显式profileId)".into(), &mut out);
                    }
                    if out
                        .iter()
                        .any(|(_, note)| note.contains("进入选课") || note.contains("选课轮次"))
                    {
                        break;
                    }
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }

        if successful_requests == 0 {
            return Err(last_error.unwrap_or_else(|| anyhow!("所有选课入口均请求失败")));
        }

        out.retain(|(id, note)| plausible_profile_id(id, note));
        out.sort_by(|left, right| {
            let score = |note: &str| {
                if note.contains("进入选课") {
                    0
                } else if note.contains("选课") {
                    1
                } else {
                    2
                }
            };
            score(&left.1)
                .cmp(&score(&right.1))
                .then_with(|| left.0.cmp(&right.0))
        });

        self.save_debug("profiles_probe.txt", &debug_blob);
        Ok(out)
    }

    /// 进入指定选课轮次（模拟页面“进入选课>>>”），继承调用方优先级。
    async fn enter_elect_profile_with_priority(
        &self,
        profile_id: &str,
        priority: RequestPriority,
    ) -> Result<String> {
        let pid = validate_numeric_id(profile_id, "profileId", true)?;
        if pid == "0" {
            return self
                .get_text_with_priority("stdElectCourse!defaultPage.action", priority)
                .await;
        }

        let default_url = self.url("stdElectCourse!defaultPage.action")?;
        let mut candidates: Vec<(String, String)> = Vec::new();
        let mut last_error: Option<anyhow::Error> = None;

        for (label, query_name) in [
            ("get_electionProfile", "electionProfile.id"),
            ("get_profileId", "profileId"),
        ] {
            let mut url = default_url.clone();
            url.query_pairs_mut().append_pair(query_name, pid);
            match self
                .send_text_with_priority(self.http.get(url), "进入选课轮次", priority)
                .await
            {
                Ok(text) => {
                    if score_elect_page(&text) >= 200 {
                        return Ok(text);
                    }
                    candidates.push((label.into(), text));
                }
                Err(error) if is_auth_error(&error) => return Err(error),
                Err(error) => last_error = Some(error),
            }
        }

        let referer = self.url("stdElectCourse.action")?;
        match self
            .get_text_with_priority("stdElectCourse.action", priority)
            .await
        {
            Ok(_) => {}
            Err(error) if is_auth_error(&error) => return Err(error),
            Err(error) => last_error = Some(error),
        }
        let forms: [(&str, Vec<(&str, &str)>); 3] = [
            ("post_electionProfile", vec![("electionProfile.id", pid)]),
            ("post_profileId", vec![("profileId", pid)]),
            (
                "post_both",
                vec![
                    ("electionProfile.id", pid),
                    ("profileId", pid),
                    ("shortTerm", "0"),
                ],
            ),
        ];
        for (label, form) in forms {
            match self
                .send_text_with_priority(
                    self.http
                        .post(default_url.clone())
                        .header(REFERER, referer.as_str())
                        .form(&form),
                    "进入选课轮次",
                    priority,
                )
                .await
            {
                Ok(text) => {
                    if score_elect_page(&text) >= 200 {
                        return Ok(text);
                    }
                    candidates.push((label.into(), text));
                }
                Err(error) if is_auth_error(&error) => return Err(error),
                Err(error) => last_error = Some(error),
            }
        }

        let best = candidates
            .into_iter()
            .max_by_key(|(_, text)| score_elect_page(text))
            .map(|(_, text)| text)
            .unwrap_or_default();
        if score_elect_page(&best) < 200 {
            self.save_debug(&format!("enter_profile_{pid}.html"), &best);
        }
        if best.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| anyhow!("服务器无有效响应"))
                .context("进入选课轮次失败"));
        }
        Ok(best)
    }

    /// 拉取全部可选课程（结构化数据）
    /// 实际接口：
    /// 1) POST/GET 进入 defaultPage
    /// 2) GET stdElectCourse!data.action?profileId=...  -> var lessonJSONs = [...]
    /// 3) GET stdElectCourse!queryStdCount.action?profileId=...&projectId=...&semesterId=...
    pub async fn fetch_lessons(&self, profile_id: &str) -> Result<Vec<Lesson>> {
        self.fetch_lessons_for_monitoring(profile_id)
            .await
            .map(|(lessons, _complete)| lessons)
    }

    pub(crate) async fn fetch_lessons_for_monitoring(
        &self,
        profile_id: &str,
    ) -> Result<(Vec<Lesson>, bool)> {
        let _refresh_guard = self.governor.enter_refresh().await;
        self.fetch_lessons_under_refresh_gate(profile_id, RequestPriority::Refresh)
            .await
    }

    /// Background keepalive is skipped while a full refresh is active, so it
    /// never waits in the full-refresh FIFO ahead of foreground work.
    pub async fn fetch_lessons_for_keepalive(
        &self,
        profile_id: &str,
    ) -> Result<Option<Vec<Lesson>>> {
        let Some(_refresh_guard) = self.governor.try_enter_refresh() else {
            return Ok(None);
        };
        self.fetch_lessons_under_refresh_gate(profile_id, RequestPriority::KeepAlive)
            .await
            .map(|(lessons, _complete)| Some(lessons))
    }

    async fn fetch_lessons_under_refresh_gate(
        &self,
        profile_id: &str,
        priority: RequestPriority,
    ) -> Result<(Vec<Lesson>, bool)> {
        let pid = validate_numeric_id(profile_id, "profileId", true)?;

        // 每个登录会话只需建立一次轮次上下文，后续轮询直接请求数据接口。
        let cached_context = self.profile_context.lock().get(pid).cloned();
        let (project_id, semester_id, entered) = if let Some(context) = cached_context {
            (context.project_id, context.semester_id, String::new())
        } else {
            let entered = match self.enter_elect_profile_with_priority(pid, priority).await {
                Ok(text) => text,
                Err(error) if is_auth_error(&error) => return Err(error),
                Err(_) => String::new(),
            };
            let (project, semester) = extract_project_semester(&entered);
            if !entered.is_empty() {
                self.profile_context.lock().insert(
                    pid.to_string(),
                    ProfileContext {
                        project_id: project.clone(),
                        semester_id: semester.clone(),
                    },
                );
            }
            (project, semester, entered)
        };

        // 核心：课程 JSON（其实是 JS 对象字面量）
        let data_path = format!("stdElectCourse!data.action?profileId={pid}");
        let data_text = match self.get_text_with_priority(&data_path, priority).await {
            Ok(text) => text,
            Err(error) => {
                self.profile_context.lock().remove(pid);
                return Err(error).context("请求课程数据失败");
            }
        };
        // data dump on failure below

        let mut lessons = match parse_lessons_from_page(&data_text)
            .or_else(|_| parse_lessons_js_like(&data_text))
            .or_else(|_| parse_lessons_json(&data_text))
        {
            Ok(lessons) => lessons,
            Err(_error) => {
                self.profile_context.lock().remove(pid);
                let saved = self.save_debug("lessons_last.html", &data_text);
                if !entered.is_empty() {
                    self.save_debug(&format!("fetch_enter_{pid}.html"), &entered);
                }
                let hint = if saved {
                    "，原始响应已保存到 runtime/debug/lessons_last.html"
                } else if self.debug_dump_enabled {
                    "，但调试响应保存失败"
                } else {
                    "；可在高级设置中临时开启原始调试页面后重试"
                };
                return Err(EamsError::Parse {
                    message: format!("解析课程数据失败{hint}"),
                }
                .into());
            }
        };

        // 人数：queryStdCount
        let mut count_paths = vec![format!(
            "stdElectCourse!queryStdCount.action?profileId={pid}"
        )];
        if let (Some(p), Some(s)) = (project_id.as_deref(), semester_id.as_deref()) {
            count_paths.insert(
                0,
                format!(
                    "stdElectCourse!queryStdCount.action?profileId={pid}&projectId={p}&semesterId={s}"
                ),
            );
        } else {
            if let Some(p) = project_id.as_deref() {
                count_paths.push(format!(
                    "stdElectCourse!queryStdCount.action?profileId={pid}&projectId={p}"
                ));
            }
            if let Some(s) = semester_id.as_deref() {
                count_paths.push(format!(
                    "stdElectCourse!queryStdCount.action?profileId={pid}&semesterId={s}"
                ));
            }
        }

        // 人数接口失败不应让课程列表整体失败；但限流/登录失效要向上抛出。
        // 失败时课程保持 SeatInfo::Unknown，由上层决定是否提交。
        let mut merged_counts = 0usize;
        for path in count_paths {
            match self.get_text_with_priority(&path, priority).await {
                Ok(counts) => {
                    let n = merge_lesson_counts(&mut lessons, &counts);
                    if n > 0 {
                        merged_counts = n;
                        break;
                    }
                }
                Err(error) if is_auth_error(&error) => {
                    self.profile_context.lock().remove(pid);
                    return Err(error);
                }
                Err(error) if is_rate_limit_error(&error) => {
                    return Err(error).context("查询课程人数触发限流");
                }
                Err(_) => {}
            }
        }
        if lessons.is_empty() {
            // 兜底：旧解析路径
            if !entered.is_empty() {
                if let Ok(list) = parse_lessons_from_page(&entered) {
                    lessons = list;
                } else {
                    lessons = parse_lessons_from_html_table(&entered);
                }
            }
        }

        if lessons.is_empty() {
            self.profile_context.lock().remove(pid);
            self.save_debug("lessons_last.html", &data_text);
            if !entered.is_empty() {
                self.save_debug(&format!("fetch_enter_{pid}.html"), &entered);
            }
            let hint = if self.debug_dump_enabled {
                "，请检查 runtime/debug/lessons_last.html"
            } else {
                "；可在高级设置中临时开启原始调试页面后重试"
            };
            return Err(EamsError::Parse {
                message: format!("未能解析课程列表。请确认选课已开放{hint}"),
            }
            .into());
        }
        Ok((lessons, merged_counts > 0))
    }

    /// 提交选课；对“成功”响应做轻量二次确认，降低误报。
    pub async fn elect_lesson(
        &self,
        profile_id: &str,
        lesson_id: &str,
        prior_selected: Option<u32>,
    ) -> Result<ElectResult> {
        let _submission_guard = self.governor.enter_submission().await;
        let pid = validate_numeric_id(profile_id, "profileId", true)?;
        let lesson_id = validate_numeric_id(lesson_id, "lessonId", false)?;
        let before_selected = prior_selected;

        let mut url = self.url("stdElectCourse!batchOperator.action")?;
        if pid != "0" {
            url.query_pairs_mut().append_pair("profileId", pid);
        }
        let mut referer = self.url("stdElectCourse!defaultPage.action")?;
        if pid != "0" {
            referer
                .query_pairs_mut()
                .append_pair("electionProfile.id", pid);
        }
        let operator = format!("{lesson_id}:true:0");
        let form = [("optype", "true"), ("operator0", operator.as_str())];
        let text = self
            .send_text_with_priority(
                self.http
                    .post(url)
                    .header(REFERER, referer.as_str())
                    .form(&form),
                "提交选课",
                RequestPriority::Submission,
            )
            .await?;
        let result = classify_elect_response(&text);
        if let ElectResult::Success { detail } = &result {
            match self
                .verify_elect_success(pid, lesson_id, before_selected, detail)
                .await
            {
                Ok(VerifyOutcome::Rejected(reason)) => {
                    return Ok(ElectResult::Failed { detail: reason });
                }
                Ok(VerifyOutcome::Confirmed) => {
                    return Ok(ElectResult::Success {
                        detail: if detail.contains("已确认") {
                            detail.clone()
                        } else {
                            format!("{detail}（已二次确认）")
                        },
                    });
                }
                Ok(VerifyOutcome::Inconclusive) => {
                    return Ok(ElectResult::Success {
                        detail: format!("{detail}（提交成功，二次确认暂不确定）"),
                    });
                }
                Err(_) => {}
            }
        }
        Ok(result)
    }

    /// 成功响应后的确认：人数上升 / 不在可选列表 / 已满 视为确认；
    /// 若仍有余量且人数未变，且响应文案偏弱，则拒绝以免误报。
    async fn verify_elect_success(
        &self,
        profile_id: &str,
        lesson_id: &str,
        before_selected: Option<u32>,
        success_detail: &str,
    ) -> Result<VerifyOutcome> {
        // Brief settle time for server-side state.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let data_path = format!("stdElectCourse!data.action?profileId={profile_id}");
        let data_text = self
            .get_text_with_priority(&data_path, RequestPriority::Submission)
            .await?;
        if !data_text.contains(lesson_id) {
            return Ok(VerifyOutcome::Confirmed);
        }
        let mut lessons = parse_lessons_from_page(&data_text)
            .or_else(|_| parse_lessons_js_like(&data_text))
            .or_else(|_| parse_lessons_json(&data_text))
            .unwrap_or_default();
        if lessons.is_empty() {
            return Ok(VerifyOutcome::Inconclusive);
        }
        let count_path = format!("stdElectCourse!queryStdCount.action?profileId={profile_id}");
        if let Ok(counts) = self
            .get_text_with_priority(&count_path, RequestPriority::Submission)
            .await
        {
            let _ = merge_lesson_counts(&mut lessons, &counts);
        }
        let Some(lesson) = lessons.iter().find(|l| l.id == lesson_id) else {
            return Ok(VerifyOutcome::Confirmed);
        };

        if let (Some(before), Some(after)) = (before_selected, lesson.seat.selected()) {
            if after > before {
                return Ok(VerifyOutcome::Confirmed);
            }
        }
        if lesson.seat.is_full() {
            return Ok(VerifyOutcome::Confirmed);
        }
        if lesson.seat.has_seat() {
            let strong = success_detail.contains("选课成功")
                || success_detail.contains("已经选过")
                || success_detail.contains("已选过")
                || success_detail.contains("操作成功");
            if !strong {
                return Ok(VerifyOutcome::Rejected(
                    "提交返回疑似成功，但人数未变化且课程仍可选，已按失败处理".into(),
                ));
            }
            return Ok(VerifyOutcome::Inconclusive);
        }
        Ok(VerifyOutcome::Inconclusive)
    }

    #[cfg(test)]
    async fn get_text(&self, path: &str) -> Result<String> {
        self.get_text_with_priority(path, RequestPriority::Session)
            .await
    }

    async fn get_text_with_priority(
        &self,
        path: &str,
        priority: RequestPriority,
    ) -> Result<String> {
        let url = self.url(path)?;
        let endpoint = path.split_once('?').map_or(path, |(endpoint, _)| endpoint);
        self.send_text_with_priority(self.http.get(url), &format!("请求 {endpoint}"), priority)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::parse::*;
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    #[ignore = "requires live network access"]
    fn live_example_login_page_exposes_the_expected_password_salt() {
        let client = EamsClient::new("https://example.edu/eams", 15, false).unwrap();
        let login_url = client.url("loginExt.action").unwrap();
        let (_, html) = test_runtime()
            .block_on(client.send_raw_text(client.http.get(login_url), "打开登录页", true))
            .unwrap();
        assert!(extract_password_salt(&html).is_some());
    }

    #[test]
    fn utf8_profile_context_never_slices_inside_a_character() {
        let html = format!(
            "{}<h2>春季选课</h2>{}checkPaymentBeforeElect(2031, 0)",
            "中".repeat(1500),
            "文".repeat(100)
        );
        let profiles = extract_profiles_detailed(&html);
        assert_eq!(profiles, vec![("2031".into(), "春季选课".into())]);
    }

    #[test]
    fn election_result_does_not_treat_failure_as_success() {
        assert!(matches!(
            classify_elect_response("操作未成功，请稍后重试"),
            ElectResult::Failed { .. }
        ));
        assert!(matches!(
            classify_elect_response("选课成功"),
            ElectResult::Success { .. }
        ));
        assert!(matches!(
            classify_elect_response("上限人数已满"),
            ElectResult::Full { .. }
        ));
        assert!(matches!(
            classify_elect_response(r#"{"success":false,"message":"时间冲突"}"#),
            ElectResult::Failed { .. }
        ));
    }

    #[test]
    fn parses_js_lessons_and_merges_counts() {
        let source = r#"
            var lessonJSONs = [
              {id:371644,no:'ABC.001',name:'Rust 程序设计',teachers:'张老师',stdCount:0,limitCount:0},
              {id:371645,no:'ABC.002',name:'系统设计',teachers:'李老师',stdCount:3,limitCount:50}
            ];
        "#;
        let mut lessons = parse_lessons_js_like(source).unwrap();
        assert_eq!(lessons.len(), 2);
        assert_eq!(lessons[0].name, "Rust 程序设计");
        let merged = merge_lesson_counts(
            &mut lessons,
            "window.lessonId2Counts={'371644':{sc:10,lc:50},'371645':{sc:50,lc:50}}",
        );
        assert_eq!(merged, 2);
        assert!(lessons[0].has_seat());
        assert!(!lessons[1].has_seat());
    }

    #[test]
    fn fetch_lessons_reuses_profile_context_after_first_request() {
        let data = "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_many(vec!["<html>elect page</html>", data, counts, data, counts]);
        let client = EamsClient::new(&base, 5, false).unwrap();
        let runtime = test_runtime();
        let first = runtime.block_on(client.fetch_lessons("0")).unwrap();
        let second = runtime.block_on(client.fetch_lessons("0")).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].no, "ABC.001");
    }

    #[test]
    fn validates_base_url_security_and_normalization() {
        assert!(normalize_base("http://example.com").is_err());
        let local = normalize_base("http://127.0.0.1:3000").unwrap();
        assert_eq!(local.path(), "/eams/");
        let secure = normalize_base("https://example.com/eams/").unwrap();
        assert_eq!(secure.as_str(), "https://example.com/eams/");
    }

    #[test]
    fn detects_auth_expiry_from_http_response() {
        let base = serve_once(
            200,
            r#"<form id="loginForm"><input name="password"></form>"#,
        );
        let client = EamsClient::new(&base, 5, false).unwrap();
        let error = test_runtime()
            .block_on(client.get_text("homeExt.action"))
            .unwrap_err();
        assert!(is_auth_error(&error));
    }

    #[test]
    fn rejects_unsuccessful_http_status() {
        let base = serve_once(503, "service unavailable");
        let client = EamsClient::new(&base, 5, false).unwrap();
        let error = test_runtime()
            .block_on(client.get_text("homeExt.action"))
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 503"));
    }

    #[test]
    fn login_flow_hashes_password_and_reuses_session_cookie() {
        let salt = "12345678-1234-1234-1234-123456789abc";
        let (base, requests) = serve_login_flow(salt);
        let client = EamsClient::new(&base, 5, false).unwrap();
        test_runtime()
            .block_on(client.login("student01", "secret"))
            .unwrap();

        let requests = requests
            .into_iter()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n---REQUEST---\n");
        let expected = sha1_password(salt, "secret");
        assert!(requests.contains("username=student01"));
        assert!(requests.contains(&format!("password={expected}")));
        assert!(requests.contains("JSESSIONID=test-session"));
        assert!(!requests.contains("secret"));
    }

    #[test]
    fn http_200_login_rate_limit_text_updates_governor_before_success() {
        let page = r#"<form id="loginForm"><input name="password"><div class="actionError">请不要过快点击</div></form>"#;
        let base = serve_many(vec![page]);
        let mut client = EamsClient::new(&base, 5, false).unwrap();
        client.governor = RequestGovernor::new_for_semantic_response_tests();
        let login_url = client.url("loginExt.action").unwrap();

        let error = test_runtime()
            .block_on(client.send_raw_text_with_priority(
                client.http.post(login_url),
                "提交登录",
                ResponseHandling::LoginSubmission,
                RequestPriority::Session,
            ))
            .unwrap_err();

        assert!(is_rate_limit_error(&error));
        let snapshot = client.network_snapshot();
        assert_eq!(snapshot.total_rate_limits, 1);
        assert_eq!(snapshot.consecutive_errors, 1);
        assert_eq!(
            snapshot.last_error_kind,
            Some(BackendErrorKind::RateLimited)
        );
        assert!(!snapshot.cooldown_remaining.is_zero());
        assert_eq!(snapshot.circuit_status, CircuitStatus::Closed);
    }

    #[test]
    fn three_http_200_login_rate_limit_pages_open_circuit() {
        let page = r#"<form id="loginForm"><input name="password"><div class="actionError">请不要过快点击</div></form>"#;
        let base = serve_many(vec![page, page, page]);
        let mut client = EamsClient::new(&base, 5, false).unwrap();
        client.governor = RequestGovernor::new_for_semantic_response_tests();
        let login_url = client.url("loginExt.action").unwrap();
        let runtime = test_runtime();

        for attempt in 0..3 {
            let error = runtime
                .block_on(client.send_raw_text_with_priority(
                    client.http.post(login_url.clone()),
                    "提交登录",
                    ResponseHandling::LoginSubmission,
                    RequestPriority::Session,
                ))
                .unwrap_err();
            assert!(is_rate_limit_error(&error));
            if attempt < 2 {
                client.governor.clear_cooldown_for_tests();
            }
        }

        let snapshot = client.network_snapshot();
        assert_eq!(snapshot.total_rate_limits, 3);
        assert_eq!(snapshot.consecutive_errors, 3);
        assert_eq!(
            snapshot.last_error_kind,
            Some(BackendErrorKind::RateLimited)
        );
        assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
        assert!(!snapshot.cooldown_remaining.is_zero());
    }

    #[test]
    fn half_open_http_200_login_rate_limit_reopens_circuit() {
        let page = r#"<form id="loginForm"><input name="password"><div class="actionError">请不要过快点击</div></form>"#;
        let base = serve_many(vec![page, page, page, page]);
        let mut client = EamsClient::new(&base, 5, false).unwrap();
        client.governor = RequestGovernor::new_for_semantic_response_tests();
        let login_url = client.url("loginExt.action").unwrap();
        let runtime = test_runtime();

        for attempt in 0..3 {
            runtime
                .block_on(client.send_raw_text_with_priority(
                    client.http.post(login_url.clone()),
                    "提交登录",
                    ResponseHandling::LoginSubmission,
                    RequestPriority::Session,
                ))
                .unwrap_err();
            if attempt < 2 {
                client.governor.clear_cooldown_for_tests();
            }
        }
        tokio_sleep(&runtime, Duration::from_millis(50));
        assert_eq!(
            client.network_snapshot().circuit_status,
            CircuitStatus::HalfOpen
        );

        let error = runtime
            .block_on(client.send_raw_text_with_priority(
                client.http.post(login_url),
                "提交登录",
                ResponseHandling::LoginSubmission,
                RequestPriority::Session,
            ))
            .unwrap_err();
        assert!(is_rate_limit_error(&error));
        let snapshot = client.network_snapshot();
        assert_eq!(snapshot.total_rate_limits, 4);
        assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
        assert!(snapshot.cooldown_remaining >= Duration::from_millis(35));
    }

    #[test]
    fn sha1_password_matches_known_digest() {
        assert_eq!(
            sha1_password("salt", "secret"),
            "5c9244fbb9b4dbe89423d65bff3e8218b813ec40"
        );
    }

    #[test]
    fn rejects_oversized_chunked_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let header =
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(header);
            let chunk = vec![b'A'; 64 * 1024];
            let chunk_header = format!("{:x}\r\n", chunk.len()).into_bytes();
            for _ in 0..220 {
                if stream.write_all(&chunk_header).is_err() {
                    return;
                }
                if stream.write_all(&chunk).is_err() {
                    return;
                }
                if stream.write_all(b"\r\n").is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        });

        let base = format!("http://{address}/eams");
        let client = EamsClient::new(&base, 5, false).unwrap();
        let url = format!("{base}/big");
        let error = test_runtime()
            .block_on(client.send_raw_text(client.http.get(url), "读取超大响应", true))
            .unwrap_err();
        let msg = format!("{error:#}");
        assert!(
            msg.contains("过大") || msg.contains("MiB"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn seat_info_treats_missing_or_zero_limit_as_unknown() {
        assert_eq!(SeatInfo::from_counts(Some(3), None), SeatInfo::Unknown);
        assert_eq!(SeatInfo::from_counts(Some(0), Some(0)), SeatInfo::Unknown);
        assert_eq!(
            SeatInfo::from_counts(Some(1), Some(2)),
            SeatInfo::Known {
                selected: 1,
                limit: 2
            }
        );
        assert!(!SeatInfo::from_counts(Some(5), None).has_seat());
        assert!(SeatInfo::from_counts(Some(1), Some(2)).has_seat());
        assert!(SeatInfo::from_counts(Some(2), Some(2)).is_full());
        assert_eq!(SeatInfo::from_counts(Some(1), Some(2)).selected(), Some(1));
        assert_eq!(SeatInfo::Unknown.selected(), None);
    }

    #[test]
    fn parse_lessons_with_selected_but_missing_limit_stays_unknown() {
        let source =
            "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust',teachers:'张',stdCount:12}];";
        let lessons = parse_lessons_js_like(source).unwrap();
        assert_eq!(lessons.len(), 1);
        assert!(!lessons[0].capacity_known());
        assert!(!lessons[0].has_seat());
        assert_eq!(lessons[0].capacity_text(), "-");
    }

    #[test]
    fn rate_limited_response_honors_retry_after_header() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = "too many requests";
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let base = format!("http://{address}/eams");
        let client = EamsClient::new(&base, 5, false).unwrap();
        let error = test_runtime()
            .block_on(client.get_text("stdElectCourse!data.action?profileId=0"))
            .unwrap_err();
        assert!(is_rate_limit_error(&error));
        assert_eq!(rate_limit_retry_after(&error), Some(Duration::from_secs(7)));
    }

    #[test]
    fn fetch_lessons_succeeds_when_count_endpoint_fails() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let data = "var lessonJSONs=[{id:371644,no:'ABC.001',name:'Rust',teachers:'张老师',stdCount:0,limitCount:0}];";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            // enter page
            {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = "<html>elect page</html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
            // data.action
            {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/javascript\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
            // queryStdCount fails
            {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = "error";
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let base = format!("http://{address}/eams");
        let client = EamsClient::new(&base, 5, false).unwrap();
        let (lessons, complete_refresh) = test_runtime()
            .block_on(client.fetch_lessons_for_monitoring("0"))
            .unwrap();
        assert_eq!(lessons.len(), 1);
        assert!(!complete_refresh);
        assert!(!lessons[0].capacity_known());
        assert_eq!(lessons[0].no, "ABC.001");
    }

    fn serve_login_flow(salt: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header_text = String::from_utf8_lossy(&bytes[..header_end + 4]);
                        let content_length = header_text
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length: ")
                                    .or_else(|| line.strip_prefix("Content-Length: "))
                            })
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                sender
                    .send(String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap();
                let (body, cookie) = if index == 0 {
                    (
                        format!(
                            "<script>CryptoJS.SHA1('{salt}-' + form['password'].value)</script>"
                        ),
                        "Set-Cookie: JSESSIONID=test-session; Path=/eams\r\n",
                    )
                } else {
                    ("<html><body>home</body></html>".to_string(), "")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\n{cookie}Content-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}/eams"), receiver)
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn tokio_sleep(runtime: &tokio::runtime::Runtime, duration: Duration) {
        runtime.block_on(async {
            tokio::time::sleep(duration).await;
        });
    }

    fn serve_many(bodies: Vec<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}/eams")
    }

    fn serve_once(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let reason = if status == 200 {
                "OK"
            } else {
                "Service Unavailable"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{address}/eams")
    }
}

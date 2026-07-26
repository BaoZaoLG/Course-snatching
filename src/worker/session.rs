//! Login, manual catalog refresh, and passive session keepalive tasks.

use super::runtime::{spawn_task, Activity, ActivityGuard};
use super::{LogLevel, SharedState};
use crate::eams::{backend_error_kind, is_auth_error, BackendErrorKind, EamsClient};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use zeroize::Zeroizing;

pub struct LoginRequest {
    pub base_url: String,
    pub username: String,
    /// 自动清零包装：无论哪条路径（早退、初始化失败、正常登录）drop
    /// 时都会抹掉堆上的明文密码。
    pub password: Zeroizing<String>,
    pub profile_preference: String,
    pub timeout: u64,
    pub auto_fetch: bool,
    pub debug_dump_enabled: bool,
    /// 是否在本次会话内保留凭据，用于会话过期后自动重登。
    pub remember_for_relogin: bool,
    /// 用户填写的验证码（仅在教务要求时有值）。
    pub captcha_answer: Option<String>,
}

/// 从错误链里取出验证码图片。
fn captcha_image(error: &anyhow::Error) -> Option<Vec<u8>> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<crate::eams::EamsError>()
            .and_then(|error| match error {
                crate::eams::EamsError::CaptchaRequired { image, .. } => Some(image.clone()),
                _ => None,
            })
    })
}

/// 本次会话保留的登录凭据。
///
/// 只存在于内存、永不落盘，退出登录与关闭程序时清空。存它的理由是：这是
/// 一个「设定时开抢、半夜挂机」的工具，会话在等待期间过期是必然事件而非
/// 异常，而此前 AuthExpired 一律终止整个 run——等于定时功能在最常见的情况
/// 下不可用。内存里本来就有一份与账号等价的会话 Cookie，多留一份密码的
/// 边际风险有限，但必须让用户看得见并且能关掉。
pub struct SessionCredentials {
    pub username: String,
    pub password: Zeroizing<String>,
}

/// 会话过期后的自动重登上限。超过就停机并明确告知，不无限重试。
pub(super) const MAX_RELOGIN_ATTEMPTS: u32 = 3;

/// 用本次会话保留的凭据重新登录。
///
/// 复用同一个 `EamsClient`：它的 cookie jar 会被登录刷新，而 per-origin 的
/// governor 本来就跨 client 共享——重登绝不能顺手清掉限流与熔断历史。
///
/// 返回 `None` 表示没有可用凭据（用户关掉了「保留凭据」或从未成功登录）。
pub(super) async fn try_relogin(
    state: &SharedState,
    client: &EamsClient,
    attempt: u32,
) -> Option<anyhow::Result<()>> {
    // parking_lot 的 guard 不能跨 await 持有；先把凭据复制出来再放锁。
    let (username, password) = {
        let guard = state.credentials.lock();
        let credentials = guard.as_ref()?;
        (credentials.username.clone(), credentials.password.clone())
    };
    if attempt > 1 {
        // 与其它退避共用 decorrelated jitter：会话集中过期时，所有客户端
        // 会在同一时刻一起重登。
        tokio::time::sleep(crate::eams::backoff_for_attempt(attempt - 1)).await;
    }
    state.log(
        LogLevel::Warn,
        format!(
            "会话已过期，正在自动重新登录（第 {attempt}/{MAX_RELOGIN_ATTEMPTS} 次）：{}",
            mask_account(&username)
        ),
    );
    Some(client.login(&username, password.as_str()).await)
}

pub fn login_and_fetch(state: Arc<SharedState>, req: LoginRequest) {
    let LoginRequest {
        base_url,
        username,
        password,
        profile_preference,
        timeout,
        auto_fetch,
        debug_dump_enabled,
        remember_for_relogin,
        captcha_answer,
    } = req;
    if state.logging_in.swap(true, Ordering::AcqRel) {
        state.log(LogLevel::Warn, "登录进行中，请稍候…");
        return;
    }
    state.logged_in.store(false, Ordering::Release);
    // 新登录开启新会话：作废上一会话仍在途的刷新/保活任务的回写。
    state.invalidate_session_tasks();
    // 可能换了教务地址，旧的时钟偏移不再有意义；重新登录会立刻用新样本对上。
    crate::eams::clock::ClockSync::global().reset();
    *state.client.lock() = None;
    state.lessons.lock().clear();
    state.set_message("正在登录…");
    state.log(
        LogLevel::Info,
        format!("登录中：{}", mask_account(&username)),
    );

    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Login);
        let client = match EamsClient::new(&base_url, timeout, debug_dump_enabled) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                state.log(LogLevel::Error, format!("初始化失败：{error:#}"));
                state.set_message("初始化失败");
                return;
            }
        };

        let login_result = match captcha_answer.as_deref() {
            Some(answer) => {
                client
                    .login_with_captcha(&username, password.as_str(), answer)
                    .await
            }
            None => client.login(&username, password.as_str()).await,
        };
        if let Err(error) = login_result {
            state.logged_in.store(false, Ordering::Release);
            // 需要验证码：把图片交给界面，等用户填完再重新提交。
            if let Some(image) = captcha_image(&error) {
                *state.pending_captcha.lock() = Some(image);
                state.log(LogLevel::Warn, "教务要求输入验证码，请在弹窗中填写");
                state.set_message("需要输入验证码");
                // 凭据先留着，用户填完验证码后直接复用，不必重敲密码。
                *state.credentials.lock() = Some(SessionCredentials {
                    username: username.clone(),
                    password: password.clone(),
                });
                return;
            }
            if backend_error_kind(&error) == BackendErrorKind::RateLimited {
                state.log(
                    LogLevel::Error,
                    format!("登录失败：{error:#}（请等待几秒后再试）"),
                );
                state.set_message("教务限流，请稍后再登录");
            } else {
                state.log(LogLevel::Error, format!("登录失败：{error:#}"));
                state.set_message("登录失败");
            }
            return;
        }

        state.logged_in.store(true, Ordering::Release);
        *state.client.lock() = Some(client.clone());
        // 凭据只在登录成功后保留，且只在用户允许时保留。
        // 登录成功，清掉待填验证码。
        *state.pending_captcha.lock() = None;
        *state.credentials.lock() = remember_for_relogin.then(|| SessionCredentials {
            username: username.clone(),
            password: password.clone(),
        });
        state.log(LogLevel::Success, "登录成功");
        state.set_message("登录成功");

        let profile = if !profile_preference.trim().is_empty() {
            profile_preference.trim().to_string()
        } else {
            match client.list_profiles().await {
                Ok(profiles) if !profiles.is_empty() => {
                    for (id, note) in &profiles {
                        state.log(LogLevel::Info, format!("发现选课轮次 id={id}  {note}"));
                    }
                    profiles
                        .iter()
                        .find(|(_, note)| note.contains("进入选课") || note.contains("选课轮次"))
                        .or_else(|| profiles.first())
                        .map(|(id, _)| id.clone())
                        .unwrap_or_default()
                }
                Ok(_) => {
                    state.log(LogLevel::Warn, "未发现开放的选课轮次");
                    String::new()
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        state.clear_session("登录失效，请重新登录");
                        state.log(LogLevel::Error, format!("轮次探测失败：{error:#}"));
                        return;
                    }
                    state.log(LogLevel::Warn, format!("探测选课轮次失败：{error:#}"));
                    String::new()
                }
            }
        };

        if profile.is_empty() {
            state.set_message("已登录（未找到选课轮次，可在高级设置中填写）");
            return;
        }

        *state.profile_id.lock() = profile.clone();
        if profile == "0" {
            state.log(LogLevel::Info, "使用会话默认选课轮次");
        } else {
            state.log(LogLevel::Info, format!("使用选课轮次 profileId={profile}"));
        }

        if auto_fetch {
            state.set_message("正在拉取课程列表…");
            match client.fetch_lessons(&profile).await {
                Ok(list) => {
                    let count = list.len();
                    *state.lessons.lock() = list;
                    state.log(LogLevel::Success, format!("已拉取 {count} 门可选课程"));
                    state.set_message(format!("已登录，课程 {count} 门"));
                }
                Err(error) => {
                    if is_auth_error(&error) {
                        state.clear_session("登录失效，请重新登录");
                    } else {
                        state.set_message("已登录（课程列表拉取失败）");
                    }
                    state.log(LogLevel::Warn, format!("拉取课程失败：{error:#}"));
                }
            }
        }
    });
}

pub fn refresh_lessons(state: Arc<SharedState>, profile_preference: String) {
    let client = state.client.lock().clone();
    let profile = if profile_preference.trim().is_empty() {
        state.profile_id.lock().clone()
    } else {
        profile_preference.trim().to_string()
    };
    let Some(client) = client else {
        state.log(LogLevel::Warn, "请先登录");
        state.set_message("请先登录");
        return;
    };
    if profile.is_empty() {
        state.log(LogLevel::Warn, "缺少选课轮次号");
        state.set_message("缺少选课轮次号");
        return;
    }
    if state.refreshing.swap(true, Ordering::AcqRel) {
        state.log(LogLevel::Info, "课程正在刷新，请稍候");
        return;
    }
    *state.profile_id.lock() = profile.clone();
    state.log(
        LogLevel::Info,
        format!("刷新课程，使用 profileId={profile}"),
    );
    state.set_message("正在刷新课程…");
    let session = state.session_token();
    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Refresh);
        let result = client.fetch_lessons(&profile).await;
        // 会话代际校验：请求期间用户已退出/重新登录时静默丢弃整个结果，
        // 既不把旧账号课表写回新会话，也不让陈旧的失效判定误杀新会话。
        if !state.is_current_session(session) {
            return;
        }
        match result {
            Ok(list) => {
                let count = list.len();
                *state.lessons.lock() = list;
                state.log(LogLevel::Success, format!("刷新完成，共 {count} 门"));
                state.set_message(format!("刷新完成，共 {count} 门课"));
            }
            Err(error) => {
                if is_auth_error(&error) {
                    state.clear_session("登录失效，请重新登录");
                } else {
                    state.set_message(format!("刷新失败：{error}"));
                }
                state.log(LogLevel::Error, format!("刷新失败：{error:#}"));
            }
        }
    });
}

pub fn keepalive(state: Arc<SharedState>, notify_enabled: bool, sound_enabled: bool) {
    if !state.logged_in.load(Ordering::Acquire)
        || state.running.load(Ordering::Acquire)
        || state.logging_in.load(Ordering::Acquire)
        || state.refreshing.load(Ordering::Acquire)
    {
        return;
    }
    let client = state.client.lock().clone();
    let profile = state.profile_id.lock().clone();
    let Some(client) = client else {
        return;
    };
    if profile.is_empty() {
        return;
    }
    let session = state.session_token();
    spawn_task(async move {
        let result = client.fetch_lessons_for_keepalive(&profile).await;
        // 陈旧保活（期间退出或重新登录）不得写回旧课表，更不得因旧
        // 会话的失效判定 clear_session 注销刚登录的新会话。
        if !state.is_current_session(session) {
            return;
        }
        match result {
            Ok(Some(list)) => {
                *state.lessons.lock() = list;
                state.touch();
            }
            Ok(None) => {}
            Err(error) if is_auth_error(&error) => {
                state.clear_session("登录已过期，请重新登录");
                state.log(LogLevel::Warn, "会话保活发现登录失效");
                crate::notify::dispatch_alert(
                    "登录失效",
                    "请重新登录后继续",
                    false,
                    notify_enabled,
                    sound_enabled,
                );
            }
            Err(_) => {}
        }
    });
}

pub fn logout(state: &SharedState) {
    state.running.store(false, Ordering::Release);
    state.stopping.store(false, Ordering::Release);
    state.run_generation.fetch_add(1, Ordering::AcqRel);
    state.invalidate_session_tasks();
    state.forget_credentials();
    state.logged_in.store(false, Ordering::Release);
    *state.client.lock() = None;
    state.lessons.lock().clear();
    state.set_message("已退出登录");
    state.log(LogLevel::Info, "已退出登录");
}

fn mask_account(username: &str) -> String {
    let chars: Vec<char> = username.chars().collect();
    match chars.len() {
        0 => String::new(),
        1 => "*".into(),
        2 => format!("{}*", chars[0]),
        _ => format!("{}***{}", chars[0], chars[chars.len() - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // 直接测生产实现：脱敏是隐私相关逻辑，不能靠测试侧复制品假覆盖。
    #[test]
    fn mask_account_redacts_middle() {
        assert_eq!(mask_account(""), "");
        assert_eq!(mask_account("a"), "*");
        assert_eq!(mask_account("ab"), "a*");
        assert_eq!(mask_account("student01"), "s***1");
    }

    // [38] 密码字段必须保持自动清零包装：早退等任何路径 drop 时都会
    // 抹掉堆上明文。类型断言防止回退成裸 String。
    #[test]
    fn login_request_password_stays_zeroizing() {
        let request = LoginRequest {
            base_url: String::new(),
            username: String::new(),
            password: Zeroizing::new("secret".into()),
            profile_preference: String::new(),
            timeout: 5,
            auto_fetch: false,
            debug_dump_enabled: false,
            remember_for_relogin: false,
            captcha_answer: None,
        };
        let _typed: &Zeroizing<String> = &request.password;
    }

    // C-04：保留的凭据只在内存、且必须在退出登录时被抹掉。
    #[test]
    fn session_credentials_are_dropped_on_logout() {
        let state = SharedState::new();
        *state.credentials.lock() = Some(SessionCredentials {
            username: "student01".into(),
            password: Zeroizing::new("secret".into()),
        });
        assert!(state.credentials.lock().is_some());
        logout(&state);
        assert!(
            state.credentials.lock().is_none(),
            "logout must not leave credentials in memory"
        );

        // clear_session（登录失效路径）同样要抹掉。
        *state.credentials.lock() = Some(SessionCredentials {
            username: "student01".into(),
            password: Zeroizing::new("secret".into()),
        });
        state.clear_session("登录失效");
        assert!(state.credentials.lock().is_none());
    }

    // 没有保留凭据时，自动重登必须干脆地返回「没有凭据」，
    // 而不是拿空账号去打服务器。
    #[test]
    fn relogin_without_credentials_does_not_hit_the_network() {
        let state = SharedState::new();
        let client = EamsClient::new("http://127.0.0.1:9/eams", 5, false).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(try_relogin(&state, &client, 1));
        assert!(
            outcome.is_none(),
            "must not attempt a credential-less login"
        );
    }

    #[test]
    fn refresh_updates_lessons_for_current_session() {
        let data = "var lessonJSONs=[{id:371644,no:'RF.001',name:'Rust',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence_delayed(
            vec!["<html>elect page</html>", data, counts],
            Duration::ZERO,
        );
        let state = prepared_state(&base);
        refresh_lessons(state.clone(), String::new());
        wait_refresh_done(&state);
        assert_eq!(state.lessons.lock().len(), 1);
        assert!(state.worker_message.lock().contains("刷新完成"));
    }

    #[test]
    fn stale_refresh_result_is_discarded_after_logout() {
        let data = "var lessonJSONs=[{id:371644,no:'RF.002',name:'Rust',teachers:'张老师',stdCount:1,limitCount:2}];";
        let counts = "window.lessonId2Counts={'371644':{sc:1,lc:2}}";
        let base = serve_sequence_delayed(
            vec!["<html>elect page</html>", data, counts],
            Duration::from_millis(200),
        );
        let state = prepared_state(&base);
        refresh_lessons(state.clone(), String::new());
        assert!(state.refreshing.load(Ordering::Acquire));
        // 刷新在途时退出登录：会话代际推进，旧任务的结果必须整体作废。
        logout(&state);
        wait_refresh_done(&state);
        assert!(
            state.lessons.lock().is_empty(),
            "stale refresh wrote lessons back after logout"
        );
        assert_eq!(state.worker_message.lock().clone(), "已退出登录");
        let logs = log_messages(&state);
        assert!(
            !logs
                .iter()
                .any(|m| m.contains("刷新完成") || m.contains("刷新失败")),
            "stale refresh must stay silent, got {logs:?}"
        );
    }

    #[test]
    fn stale_keepalive_auth_error_does_not_kill_new_session() {
        let login_page = r#"<form id="loginForm"><input name="password"></form>"#;
        let base = serve_sequence_delayed(vec![login_page], Duration::from_millis(300));
        let state = prepared_state(&base);
        keepalive(state.clone(), false, false);
        // 保活在途时退出并完成一次新登录（旧 client 的请求仍会返回登录页）。
        logout(&state);
        state.logged_in.store(true, Ordering::Release);
        *state.client.lock() = Some(Arc::new(
            EamsClient::new("http://127.0.0.1:9/eams", 5, false).unwrap(),
        ));
        // 旧保活返回登录失效后，不得注销新会话。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            assert!(
                state.logged_in.load(Ordering::Acquire),
                "stale keepalive cleared the new session"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(state.client.lock().is_some());
        let logs = log_messages(&state);
        assert!(
            !logs.iter().any(|m| m.contains("会话保活发现登录失效")),
            "stale keepalive must stay silent, got {logs:?}"
        );
    }

    fn log_messages(state: &SharedState) -> Vec<String> {
        state
            .logs
            .lock()
            .iter()
            .map(|l| l.message.clone())
            .collect()
    }

    fn prepared_state(base: &str) -> Arc<SharedState> {
        let state = SharedState::new();
        state.logged_in.store(true, Ordering::Release);
        *state.profile_id.lock() = "0".into();
        *state.client.lock() = Some(Arc::new(EamsClient::new(base, 5, false).unwrap()));
        state
    }

    fn wait_refresh_done(state: &SharedState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.refreshing.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !state.refreshing.load(Ordering::Acquire),
            "refresh task did not finish in time"
        );
    }

    /// 与 monitor.rs 的 serve_sequence 同款手写 TCP mock，多一个响应前
    /// 延迟参数，用于制造“任务在途时会话已更替”的时序。
    fn serve_sequence_delayed(responses: Vec<&'static str>, delay: Duration) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                std::thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}/eams")
    }
}

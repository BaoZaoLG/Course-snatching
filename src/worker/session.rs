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
    pub password: String,
    pub profile_preference: String,
    pub timeout: u64,
    pub auto_fetch: bool,
    pub debug_dump_enabled: bool,
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
    } = req;
    if state.logging_in.swap(true, Ordering::AcqRel) {
        state.log(LogLevel::Warn, "登录进行中，请稍候…");
        return;
    }
    state.logged_in.store(false, Ordering::Release);
    *state.client.lock() = None;
    state.lessons.lock().clear();
    state.set_message("正在登录…");
    state.log(
        LogLevel::Info,
        format!("登录中：{}", mask_account(&username)),
    );

    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Login);
        let password = Zeroizing::new(password);
        let client = match EamsClient::new(&base_url, timeout, debug_dump_enabled) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                state.log(LogLevel::Error, format!("初始化失败：{error:#}"));
                state.set_message("初始化失败");
                return;
            }
        };

        if let Err(error) = client.login(&username, password.as_str()).await {
            state.logged_in.store(false, Ordering::Release);
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
    spawn_task(async move {
        let _guard = ActivityGuard::new(state.clone(), Activity::Refresh);
        match client.fetch_lessons(&profile).await {
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
    spawn_task(async move {
        match client.fetch_lessons_for_keepalive(&profile).await {
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

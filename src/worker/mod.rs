//! 后台任务：登录、刷新课程、抢课循环与会话保活。

mod runner;
mod state;
mod time;

pub use runner::{
    keepalive, login_and_fetch, logout, refresh_lessons, start_grab, stop_grab, LoginRequest,
};
#[allow(unused_imports)]
pub use state::{LogItem, LogLevel, SharedState, WatchState, WatchStatus};
pub use time::{local_now_seconds, now_parts, now_stamp, now_ymd};

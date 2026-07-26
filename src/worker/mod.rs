//! 后台任务：登录、刷新课程、抢课循环与会话保活。

mod monitor;
mod runtime;
mod schedule;
mod session;
mod state;
mod time;

pub use monitor::{start_grab, stop_grab};
pub use schedule::{arm_schedule, cancel_schedule_arm};
pub use session::{keepalive, login_and_fetch, logout, refresh_lessons, LoginRequest};
#[allow(unused_imports)]
pub use state::{LogItem, LogLevel, SharedState, WatchState, WatchStatus};
pub use time::{local_now_seconds, now_parts, now_stamp};

//! Course-snatching 的库入口。
//!
//! 拆出 lib target 不是为了给别人当依赖用，而是为了解锁三样东西：
//! `tests/` 目录下的集成测试、`fuzz/` 的模糊测试目标、以及文档测试。
//! 此前所有测试都只能内联在 `#[cfg(test)]` 里，连
//! `protocol_fixtures.rs`（含一个完整的 MockServer TCP 实现）这种实质上的
//! 集成测试也挂在 `src/` 下跟着发布二进制一起参与编译。
//!
//! `main.rs` 只保留窗口启动与 panic 报告，其余全部在这里。

pub mod app;
pub mod config;
pub mod eams;
pub mod notify;
pub mod single_instance;
pub mod worker;

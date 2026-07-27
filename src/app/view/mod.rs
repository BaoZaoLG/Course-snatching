//! 视图层。
//!
//! 每个子模块对应界面上的一块区域，内容都是 `impl CourseApp` 的绘制方法。
//! 状态字段、用户动作、派生视图缓存仍然留在 `app/mod.rs`——这里拆开的是
//! 「画」，不是把状态也散出去。
//!
//! 拆分前 `app/mod.rs` 有 3200 行，`show_header` 一个函数 255 行、10 个参数、
//! 挂着 `#[allow(clippy::too_many_arguments)]`，`show_watch_panel` 闭包嵌套
//! 六层。现在改哪块界面、看哪个文件是确定的。

mod advanced;
mod catalog;
mod header;
mod logs;
mod overlay;
mod watch;

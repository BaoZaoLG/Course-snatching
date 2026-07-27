#![no_main]
//! 解析层模糊测试。
//!
//! 不变式只有一条：任何输入都不许 panic。返回 Err 完全可以接受。
//!
//! 为什么是这一层：`parse.rs` 的输入全部来自远端 HTML，而 panic 发生在
//! `spawn_task` 里且 `JoinHandle` 被 drop——异常会被整个吞掉，用户侧表现是
//! 抢课任务在冲刺中途无声死掉。`parse_lessons_json` 的切片越界就是这么被
//! 发现的（内联测试里的「所有 UTF-8 前缀都不 panic」思路很强，相当于穷举的
//! 截断 fuzz，只是当时覆盖的函数集合太窄）。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    course_snatching::eams::fuzz_api::parse_all(data);
});

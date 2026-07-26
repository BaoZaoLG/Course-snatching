#![no_main]
//! 字符集解码模糊测试。
//!
//! 解码是远端字节流进入程序的第一道门：Content-Type 声明、`<meta charset>`
//! 嗅探、UTF-8 与 GB18030 的回退分支都在这里。任意字节序列都不许 panic。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 前 32 字节当作 Content-Type 头（可能是任意乱码），其余当作响应体，
    // 这样同一份语料能同时覆盖「声明」与「内容」两条路径。
    let split = data.len().min(32);
    let (header, body) = data.split_at(split);
    let content_type = std::str::from_utf8(header).ok();
    course_snatching::eams::fuzz_api::decode_body(content_type, body);
});

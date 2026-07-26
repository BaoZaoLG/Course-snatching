//! 单轮请求预算的端到端断言。
//!
//! 这是档案点名「最该补」的一条：`enter_elect_profile_with_priority` 首次进入
//! 要打 6 个探测请求、`list_profiles` 轮询 7 个端点、`queryStdCount` 最多 3 个
//! 候选路径——而此前没有任何测试断言「一次监控轮最多打几个请求」。
//!
//! 它挡住的是两类回归：
//! 1. 请求放大（解析失败清缓存 → 下一轮重新探测 → 压力更大的正反馈环）；
//! 2. 提速改动把请求数悄悄翻倍。
//!
//! 断言的是**上界**而不是精确值：精确值会因为无关的实现细节频繁失效，
//! 而我们真正在意的是「别爆炸」。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use course_snatching::eams::EamsClient;

/// 一轮监控（目录 + 人数）允许的请求数上界。
///
/// 正常路径是 2 个（data + queryStdCount）；首次进入轮次会多几个探测请求。
/// 留出余量，但绝不允许上到两位数。
const MONITOR_ROUND_BUDGET: usize = 8;

#[test]
fn one_monitoring_round_stays_within_its_request_budget() {
    let data = "var lessonJSONs=[{id:371644,no:'BUD.001',name:'预算',teachers:'张老师',stdCount:1,limitCount:9}];";
    let counts = "window.lessonId2Counts={'371644':{sc:1,lc:9}}";
    let server = CountingServer::start(vec![
        "<html>elect page</html>".into(),
        data.into(),
        counts.into(),
    ]);
    let client = EamsClient::new(&server.base, 5, false).expect("client");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (lessons, complete) = runtime
        .block_on(client.fetch_lessons_for_monitoring("0"))
        .expect("first monitoring round must succeed");
    assert_eq!(lessons.len(), 1);
    assert!(complete, "counts must have merged");

    let first_round = server.requests();
    assert!(
        first_round <= MONITOR_ROUND_BUDGET,
        "first monitoring round spent {first_round} requests, budget is {MONITOR_ROUND_BUDGET}"
    );
}

#[test]
fn a_warm_round_is_cheaper_than_the_first_one() {
    let data = "var lessonJSONs=[{id:371644,no:'BUD.002',name:'预算',teachers:'张老师',stdCount:1,limitCount:9}];";
    let counts = "window.lessonId2Counts={'371644':{sc:1,lc:9}}";
    let server = CountingServer::start(vec![
        "<html>elect page</html>".into(),
        data.into(),
        counts.into(),
        data.into(),
        counts.into(),
    ]);
    let client = EamsClient::new(&server.base, 5, false).expect("client");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    runtime
        .block_on(client.fetch_lessons_for_monitoring("0"))
        .expect("first round");
    let after_first = server.requests();
    runtime
        .block_on(client.fetch_lessons_for_monitoring("0"))
        .expect("second round");
    let second_round = server.requests() - after_first;

    // profile_context 缓存的全部意义就在这里：第二轮不该重跑探测。
    assert!(
        second_round <= 2,
        "a warm round spent {second_round} requests; the profile context cache is not working"
    );
}

/// 记录请求次数的脚本式 mock 服务。响应用完后循环使用最后一个，
/// 这样「多打了请求」表现为计数变大，而不是测试挂在连接上。
struct CountingServer {
    base: String,
    count: Arc<AtomicUsize>,
}

impl CountingServer {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        std::thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 8192];
                let _ = stream.read(&mut buffer);
                counter.fetch_add(1, Ordering::Relaxed);
                let body = responses
                    .get(index)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            base: format!("http://{address}/eams"),
            count,
        }
    }

    fn requests(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

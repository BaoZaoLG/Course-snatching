//! 离线、脱敏的 EAMS 协议回归测试。
//!
//! 所有样本均为人工构造，不包含真实账号、Cookie 或页面内容。

use super::parse::{body_looks_like_login_page, parse_lessons_from_page, parse_lessons_js_like};
use super::{
    is_rate_limit_error, rate_limit_retry_after, EamsClient, ElectResult, SeatInfo,
    MAX_RESPONSE_BYTES,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const NORMAL: &str = include_str!("fixtures/normal_lessons.js");
const MISSING_FIELDS: &str = include_str!("fixtures/missing_fields.js");
const UNKNOWN_CAPACITY: &str = include_str!("fixtures/unknown_capacity.js");
const DUPLICATE_NUMBERS: &str = include_str!("fixtures/duplicate_course_numbers.js");
const CHINESE_TEACHER: &str = include_str!("fixtures/chinese_teacher.js");
const EMPTY: &str = include_str!("fixtures/empty_lessons.js");
const LOGIN_EXPIRED: &str = include_str!("fixtures/login_expired.html");
const NON_STANDARD_JS: &str = include_str!("fixtures/non_standard_js.js");
const TRUNCATED: &str = include_str!("fixtures/truncated_response.js");
const PAGE_NOISE: &str = include_str!("fixtures/page_noise.html");

#[derive(Clone)]
struct MockResponse {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl MockResponse {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// 简单的脚本式 HTTP 服务：每个请求依次获得一个响应，并记录完整请求供断言。
/// 它让协议测试始终离线运行，也避免每个测试各自实现一套 TCP 服务器。
struct MockServer {
    base: String,
    requests: Receiver<String>,
}

impl MockServer {
    fn scripted(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let (sender, requests) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept mock request");
                let request = read_request(&mut stream);
                let _ = sender.send(request);
                write_response(&mut stream, response);
            }
        });
        Self {
            base: format!("http://{address}/eams"),
            requests,
        }
    }

    fn client(&self) -> EamsClient {
        EamsClient::new(&self.base, 5, false).expect("mock client")
    }

    fn requests(&self, count: usize) -> Vec<String> {
        (0..count)
            .map(|_| {
                self.requests
                    .recv_timeout(Duration::from_secs(2))
                    .expect("recorded request")
            })
            .collect()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read mock request");
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end + 4]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim())
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_response(stream: &mut std::net::TcpStream, response: MockResponse) {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Mock Response",
    };
    let headers = response
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let wire = format!(
        "HTTP/1.1 {} {}\r\n{}Content-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{}",
        response.status, reason, headers, response.body.len(), response.body
    );
    stream
        .write_all(wire.as_bytes())
        .expect("write mock response");
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

fn parse_fixture(sample_name: &str, source: &str) -> anyhow::Result<Vec<super::Lesson>> {
    parse_lessons_from_page(source)
        .or_else(|_| parse_lessons_js_like(source))
        .map_err(|error| anyhow::anyhow!("fixture {sample_name}: {error}"))
}

#[test]
fn parser_fixtures_have_golden_results() {
    let normal = parse_fixture("normal_lessons", NORMAL).unwrap();
    assert_eq!(normal.len(), 2);
    assert_eq!(normal[0].no, "CS.1001");
    assert_eq!(normal[0].teachers, "张老师");
    assert_eq!(
        normal[0].seat,
        SeatInfo::Known {
            selected: 12,
            limit: 40
        }
    );
    assert!(normal[0].has_seat());
    assert!(normal[1].seat.is_full());

    let missing = parse_fixture("missing_fields", MISSING_FIELDS).unwrap();
    assert_eq!(missing[0].name, "字段不完整");
    assert_eq!(missing[0].teachers, "");
    assert_eq!(missing[0].seat, SeatInfo::Unknown);

    let unknown = parse_fixture("unknown_capacity", UNKNOWN_CAPACITY).unwrap();
    assert_eq!(unknown[0].seat, SeatInfo::Unknown);

    let duplicates = parse_fixture("duplicate_course_numbers", DUPLICATE_NUMBERS).unwrap();
    assert_eq!(duplicates.len(), 2);
    assert_eq!(duplicates[0].no, duplicates[1].no);
    assert_ne!(duplicates[0].id, duplicates[1].id);

    let chinese = parse_fixture("chinese_teacher", CHINESE_TEACHER).unwrap();
    assert_eq!(chinese[0].teachers, "欧阳娜娜、阿不都热西提·买买提");

    let non_standard = parse_fixture("non_standard_js", NON_STANDARD_JS).unwrap();
    assert_eq!(non_standard[0].name, "非标准 JS");

    let noisy = parse_fixture("page_noise", PAGE_NOISE).unwrap();
    assert_eq!(noisy[0].no, "CS.1010");

    assert!(parse_fixture("empty_lessons", EMPTY).is_err());
    assert!(parse_fixture("truncated_response", TRUNCATED).is_err());
    assert!(body_looks_like_login_page(LOGIN_EXPIRED));
}

// G-02：冲刺窗口内一次提交只准打一个请求。
//
// 原实现一次提交要 3 个 RTT + 50ms，且三个请求全部走最高优先级——目标 A 的
// 「事后确认」会插队挡住目标 B 的真实提交。N=3 时单轮光提交就超过 1 秒，
// 而冲刺间隔本该是 0.05–0.15 秒。
#[test]
fn optimistic_submission_spends_exactly_one_request() {
    let server = MockServer::scripted(vec![MockResponse::ok("选课成功")]);
    let client = server.client();
    let result = runtime()
        .block_on(client.elect_lesson("0", "2002", Some(2), crate::eams::ConfirmMode::Optimistic))
        .expect("submission must succeed");
    assert!(matches!(result, ElectResult::Success { .. }));
    let requests = server.requests(1);
    assert_eq!(requests.len(), 1, "burst submission must not verify");
    assert!(
        requests[0].contains("batchOperator"),
        "the single request must be the submission itself"
    );
}

// 常规模式仍然做二次确认——只是不再占着提交闸门做。
#[test]
fn verifying_submission_still_confirms_but_outside_the_gate() {
    let data = "var lessonJSONs=[{id:2003,no:'X.1',name:'其他课',teachers:'王老师',stdCount:1,limitCount:9}];";
    let server = MockServer::scripted(vec![
        MockResponse::ok("选课成功"),
        MockResponse::ok(data),
        MockResponse::ok("window.lessonId2Counts={'2003':{sc:1,lc:9}}"),
    ]);
    let client = server.client();
    let result = runtime()
        .block_on(client.elect_lesson("0", "2002", Some(2), crate::eams::ConfirmMode::Verify))
        .expect("submission must succeed");
    match result {
        ElectResult::Success { detail } => {
            assert!(detail.contains("已二次确认"), "got {detail}");
        }
        other => panic!("expected a confirmed success, got {other:?}"),
    }
    let requests = server.requests(3);
    assert!(requests[0].contains("batchOperator"));
    assert!(requests[1].contains("data.action"));
}

#[test]
fn every_fixture_prefix_is_panic_free() {
    for (name, source) in [
        ("normal_lessons", NORMAL),
        ("missing_fields", MISSING_FIELDS),
        ("unknown_capacity", UNKNOWN_CAPACITY),
        ("duplicate_course_numbers", DUPLICATE_NUMBERS),
        ("chinese_teacher", CHINESE_TEACHER),
        ("empty_lessons", EMPTY),
        ("login_expired", LOGIN_EXPIRED),
        ("non_standard_js", NON_STANDARD_JS),
        ("truncated_response", TRUNCATED),
        ("page_noise", PAGE_NOISE),
    ] {
        for end in 0..=source.len() {
            if !source.is_char_boundary(end) {
                continue;
            }
            let prefix = &source[..end];
            // 覆盖面必须包含每一个「输入来自远端、签名是 &str -> _」的解析
            // 入口。原来只测三个，parse_lessons_json 的切片 panic 恰好漏在
            // 覆盖面之外。
            let result = std::panic::catch_unwind(|| {
                let _ = parse_lessons_from_page(prefix);
                let _ = parse_lessons_js_like(prefix);
                let _ = body_looks_like_login_page(prefix);
                let _ = super::parse::parse_lessons_json(prefix);
                let _ = super::parse::classify_elect_response(prefix);
                let _ = super::parse::js_like_to_json(prefix);
                let _ = super::parse::summarize_html(prefix);
                let _ = super::parse::extract_password_salt(prefix);
                let _ = super::parse::extract_login_error(prefix);
                let _ = super::parse::extract_all_profile_ids(prefix);
                let _ = super::parse::parse_lessons_by_regex(prefix);
                let _ = super::parse::parse_lessons_from_html_table(prefix);
            });
            assert!(
                result.is_ok(),
                "fixture {name} panicked at UTF-8 byte {end}"
            );
        }
    }
}

#[test]
fn mock_server_verifies_cookie_and_same_origin_redirect() {
    let salt = "12345678-1234-1234-1234-123456789abc";
    let login_page = format!("<script>CryptoJS.SHA1('{salt}-' + form['password'].value)</script>");
    let server = MockServer::scripted(vec![
        MockResponse::ok(login_page).header("Set-Cookie", "JSESSIONID=fixture-session; Path=/eams"),
        MockResponse::ok("<html>submitted</html>"),
        MockResponse::ok("<html>home</html>"),
    ]);
    runtime()
        .block_on(server.client().login("student01", "fixture-password"))
        .unwrap();
    let requests = server.requests(3).join("\n---REQUEST---\n");
    assert!(requests
        .to_ascii_lowercase()
        .contains("cookie: jsessionid=fixture-session"));
    assert!(!requests.contains("fixture-password"));

    let redirect_server = MockServer::scripted(vec![
        MockResponse::status(302, "").header("Location", "/eams/homeExt.action"),
        MockResponse::ok("<html>redirect complete</html>"),
    ]);
    let client = redirect_server.client();
    assert_eq!(
        runtime()
            .block_on(client.get_text("homeExt.action"))
            .unwrap(),
        "<html>redirect complete</html>"
    );
    assert_eq!(redirect_server.requests(2).len(), 2);
}

#[test]
fn mock_server_covers_rate_limit_oversized_count_failure_and_submit_confirmation() {
    let rate_limited = MockServer::scripted(vec![
        MockResponse::status(429, "too many requests").header("Retry-After", "7")
    ]);
    let client = rate_limited.client();
    let error = runtime()
        .block_on(client.get_text("stdElectCourse!data.action?profileId=0"))
        .unwrap_err();
    assert!(is_rate_limit_error(&error));
    assert_eq!(rate_limit_retry_after(&error), Some(Duration::from_secs(7)));
    rate_limited.requests(1);

    let data = "var lessonJSONs=[{id:2001,no:'CS.2001',name:'计数降级',teachers:'老师',stdCount:1,limitCount:2}];";
    let count_failure = MockServer::scripted(vec![
        MockResponse::ok("<html>elect page</html>"),
        MockResponse::ok(data),
        MockResponse::status(500, "count endpoint unavailable"),
    ]);
    let client = count_failure.client();
    let (lessons, complete) = runtime()
        .block_on(client.fetch_lessons_for_monitoring("0"))
        .unwrap();
    assert!(!complete);
    assert_eq!(
        lessons[0].seat,
        SeatInfo::Known {
            selected: 1,
            limit: 2
        }
    );
    count_failure.requests(3);

    let confirm_data = "var lessonJSONs=[{id:2002,no:'CS.2002',name:'确认提交',teachers:'老师',stdCount:2,limitCount:30}];";
    let submitted = MockServer::scripted(vec![
        MockResponse::ok("选课成功"),
        MockResponse::ok(confirm_data),
        MockResponse::ok("window.lessonId2Counts={'2002':{sc:3,lc:30}}"),
    ]);
    let client = submitted.client();
    let result = runtime()
        .block_on(client.elect_lesson("0", "2002", Some(2), crate::eams::ConfirmMode::Verify))
        .unwrap();
    assert!(matches!(result, ElectResult::Success { detail } if detail.contains("已二次确认")));
    let submit_requests = submitted.requests(3).join("\n");
    assert!(submit_requests.contains("batchOperator.action"));
    assert!(submit_requests.contains("stdElectCourse!data.action"));
    assert!(submit_requests.contains("queryStdCount.action"));

    let oversized =
        MockServer::scripted(vec![MockResponse::ok("A".repeat(MAX_RESPONSE_BYTES + 1))]);
    let client = oversized.client();
    let error = runtime().block_on(client.get_text("big")).unwrap_err();
    assert!(format!("{error:#}").contains("过大"));
    oversized.requests(1);
}

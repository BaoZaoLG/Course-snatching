use super::{EamsError, ElectResult, Lesson, SeatInfo};
use crate::config::AppConfig;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::header::HeaderMap;
use reqwest::Url;
use scraper::{Html, Selector};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::sync::LazyLock;

static RE_SALT_PRIMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"CryptoJS\.SHA1\('([0-9a-fA-F-]{36})-'\s*\+\s*form\['password'\]\.value\)")
        .expect("salt regex")
});
static RE_SALT_FALLBACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"SHA1\('([0-9a-fA-F-]{36})-'").expect("salt fallback regex"));
static RE_LESSON_COUNTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"['"]?(\d+)['"]?\s*:\s*\{\s*sc\s*:\s*(\d+)\s*,\s*lc\s*:\s*(\d+)"#)
        .expect("counts regex")
});

/// 容量语境的“已满”文案才算 Full（可重试）。裸“已满”也出现在
/// “学分已满，不允许再选”“选课门数已满”等终态拒绝里，不能触发 Full。
fn is_capacity_full_text(text: &str) -> bool {
    ["人数已满", "名额已满", "上限人数"]
        .iter()
        .any(|marker| text.contains(marker))
}

/// 终态拒绝：学分/门数上限这类改不了的条件，重试只是白打请求。
/// 必须先于容量与瞬态判定，否则「学分已满」会被当成可重试。
fn is_hard_reject_text(text: &str) -> bool {
    ["学分已满", "门数已满", "不允许再选"]
        .iter()
        .any(|marker| text.contains(marker))
}

/// 明确的成功标记。必须先于瞬态繁忙词表：「操作成功，请稍后在已选课程中
/// 查看」这类常见文案含「稍后」，先判繁忙会把成功当成可重试，目标留在
/// pending 里重复提交。
fn has_strong_success_text(text: &str) -> bool {
    ["已经选过", "已选过", "选课成功", "操作成功"]
        .iter()
        .any(|marker| text.contains(marker))
}

/// 服务器瞬态繁忙文案：高峰期以 HTTP 200 正文出现（“系统繁忙，请稍后再试”等），
/// 命中即视为可重试，避免开抢时刻被终态放弃。词表与 worker 对 Err 路径的
/// 限流兜底识别（限流/过快/太快/频繁/稍后）保持一致并补充繁忙类说法。
fn is_transient_busy_text(text: &str) -> bool {
    [
        "稍后",
        "繁忙",
        "系统忙",
        "限流",
        "过快",
        "太快",
        "频繁",
        "人数过多",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// 选课结果的判定范围。
///
/// 直接在整页上做子串匹配会被无关内容带偏：页脚一句「请勿频繁刷新」能让
/// 每次提交都变成 Busy，页面上列出的其它满员课程会误触容量判定。所以先抠
/// 服务器的结果容器（`extract_login_error` 已经是这个写法），抠不到才退回
/// 整页摘要。
fn extract_result_scope(html: &str) -> Option<String> {
    // 结果容器里有「稍后」这种词才算真的在讲本次提交结果。
    let doc = Html::parse_document(html);
    for sel in [
        "#actionMessage",
        ".actionMessage",
        ".actionError",
        "#msgboxDiv",
        ".alert",
        ".error",
    ] {
        let Ok(selector) = Selector::parse(sel) else {
            continue;
        };
        if let Some(node) = doc.select(&selector).next() {
            let text = node
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

pub(crate) fn classify_elect_response(text: &str) -> ElectResult {
    let summary = summarize_html(text);
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let detail = first_str(&value, &["message", "msg", "detail", "error"])
            .unwrap_or_else(|| summary.clone());
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            return ElectResult::Success { detail };
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            if is_hard_reject_text(&detail) {
                return ElectResult::Failed { detail };
            }
            if is_capacity_full_text(&detail) {
                return ElectResult::Full { detail };
            }
            if is_transient_busy_text(&detail) {
                return ElectResult::Busy { detail };
            }
            return ElectResult::Failed { detail };
        }
    }

    // 判定只看结果容器；抠不到才退回整页摘要。
    let scope = extract_result_scope(text).unwrap_or_else(|| summary.clone());
    let detail = if scope.is_empty() {
        summary.clone()
    } else {
        scope.clone()
    };

    // 强成功标记最优先——含「稍后」的成功文案不能被判成 Busy。
    if has_strong_success_text(&scope) {
        return ElectResult::Success { detail };
    }
    // 终态拒绝先于容量：「学分已满」不是可重试的「人数已满」。
    if is_hard_reject_text(&scope) {
        return ElectResult::Failed { detail };
    }
    if is_capacity_full_text(&scope) {
        return ElectResult::Full { detail };
    }
    // 瞬态繁忙先于普通失败标记：“操作未成功，请稍后重试”这类明确邀请重试的
    // 文案宁可多试一轮，也不要在开抢高峰被终态放弃。
    if is_transient_busy_text(&scope) {
        return ElectResult::Busy { detail };
    }
    if ["失败", "未成功", "冲突", "不允许", "错误", "不可选"]
        .iter()
        .any(|marker| scope.contains(marker))
    {
        return ElectResult::Failed { detail };
    }
    ElectResult::Failed {
        detail: if summary.is_empty() {
            "服务器返回空响应，未确认选课成功".into()
        } else {
            summary
        },
    }
}

pub(crate) fn validate_numeric_id<'a>(
    raw: &'a str,
    label: &str,
    allow_zero: bool,
) -> Result<&'a str> {
    let value = raw.trim();
    if value.is_empty() || value.len() > 20 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("{label} 必须是数字");
    }
    if value == "0" && !allow_zero {
        bail!("{label} 不能为 0");
    }
    Ok(value)
}

pub(crate) fn normalize_base(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim()).context("教务地址格式错误")?;
    let host = url.host_str().ok_or_else(|| anyhow!("教务地址缺少域名"))?;
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err(EamsError::InsecureBaseUrl.into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("教务地址不能内嵌账号或密码");
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/eams") && path != "eams" {
        let next = if path.is_empty() || path == "/" {
            "/eams/".to_string()
        } else {
            format!("{path}/eams/")
        };
        url.set_path(&next);
    } else {
        let normalized = format!("{path}/");
        url.set_path(&normalized);
    }
    Ok(url)
}

pub(crate) fn extract_password_salt(html: &str) -> Option<String> {
    RE_SALT_PRIMARY
        .captures(html)
        .map(|c| c[1].to_string())
        .or_else(|| RE_SALT_FALLBACK.captures(html).map(|c| c[1].to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifyOutcome {
    Confirmed,
    Inconclusive,
    #[allow(dead_code)]
    Rejected(String),
}

pub(crate) fn parse_retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let raw = raw.trim();
    // Only delta-seconds is handled; HTTP-date forms are ignored.
    let secs = raw.parse::<u64>().ok()?;
    Some(secs.clamp(1, 300))
}

pub(crate) fn sha1_password(salt: &str, password: &str) -> String {
    let mut h = Sha1::new();
    h.update(salt.as_bytes());
    h.update(b"-");
    h.update(password.as_bytes());
    hex::encode(h.finalize())
}

pub(crate) fn origin_key(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.port_or_known_default().unwrap_or(0)
    )
}

pub(crate) async fn read_body_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        let chunk = response.chunk().await?;
        let Some(chunk) = chunk else {
            break;
        };
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(EamsError::ResponseTooLarge(max_bytes / 1024 / 1024).into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 从 `Content-Type` 头里取字符集标签。
fn charset_from_content_type(content_type: Option<&str>) -> Option<&'static encoding_rs::Encoding> {
    let value = content_type?.to_ascii_lowercase();
    let idx = value.find("charset=")?;
    let label = value[idx + "charset=".len()..]
        .split(&[';', ' ', '"', '\''][..])
        .next()?
        .trim();
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// 从 HTML 头部的 `<meta charset>` / `<meta http-equiv>` 里取字符集。
/// 只扫前 2KB：声明必须出现在文档开头，扫全文既慢又容易被正文误导。
fn charset_from_meta(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    static RE_META_CHARSET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?\s*([a-z0-9_\-]+)"#)
            .expect("meta charset regex")
    });
    let head = &bytes[..bytes.len().min(2048)];
    // 字符集声明本身一定是 ASCII，lossy 足够。
    let head = String::from_utf8_lossy(head);
    let label = RE_META_CHARSET.captures(&head)?.get(1)?.as_str();
    encoding_rs::Encoding::for_label(label.as_bytes())
}

fn replacement_ratio(text: &str) -> f32 {
    let total = text.chars().count();
    if total == 0 {
        return 0.0;
    }
    let bad = text
        .chars()
        .filter(|ch| *ch == char::REPLACEMENT_CHARACTER)
        .count();
    bad as f32 / total as f32
}

/// 允许的替换字符比例。个别坏字节不该让整个响应作废，但成片乱码必须报错。
const MAX_REPLACEMENT_RATIO: f32 = 0.02;

/// 按 `Content-Type` → `<meta charset>` → UTF-8 → GB18030 的顺序解码响应体。
///
/// 原实现是 `String::from_utf8_lossy`：GB2312/GBK 站点整页变成 U+FFFD，而它
/// 之上全部是中文子串判定（人数已满 / 系统繁忙 / 限流词表 / 登录错误），
/// 会整体静默失效——选课成功被判成 Failed 并终态放弃。
/// 关键在于解码失败要显式报错，静默的乱码比明确的失败危险得多。
pub(crate) fn decode_body(content_type: Option<&str>, bytes: &[u8]) -> Result<String> {
    // decode() 自带 BOM 嗅探，BOM 会覆盖这里选定的编码。
    if let Some(encoding) =
        charset_from_content_type(content_type).or_else(|| charset_from_meta(bytes))
    {
        let (text, _, had_errors) = encoding.decode(bytes);
        if !had_errors || replacement_ratio(&text) <= MAX_REPLACEMENT_RATIO {
            return Ok(text.into_owned());
        }
        return Err(EamsError::Parse {
            message: format!("响应按 {} 解码失败（大量乱码）", encoding.name()),
        }
        .into());
    }

    // 没有任何声明：先按 UTF-8 试，明显不像 UTF-8 时再试 GB18030
    // （中文高校站点唯一现实的另一种可能），两者都不成才报错。
    let (utf8_text, _, utf8_errors) = encoding_rs::UTF_8.decode(bytes);
    if !utf8_errors || replacement_ratio(&utf8_text) <= MAX_REPLACEMENT_RATIO {
        return Ok(utf8_text.into_owned());
    }
    let (gbk_text, _, gbk_errors) = encoding_rs::GB18030.decode(bytes);
    if !gbk_errors || replacement_ratio(&gbk_text) < replacement_ratio(&utf8_text) {
        return Ok(gbk_text.into_owned());
    }
    Err(EamsError::Parse {
        message: "响应解码失败：既不是 UTF-8 也不是 GB18030，且未声明字符集".into(),
    }
    .into())
}

pub(crate) fn looks_like_login_page(url: &Url, text: &str) -> bool {
    body_looks_like_login_page(text)
        || (url.as_str().to_ascii_lowercase().contains("login") && text.trim().is_empty())
}

/// 页面是否是登录页。
///
/// 判据分两级：便宜的子串预筛（绝大多数响应在这里就被排除，不必为每个响应
/// 付 HTML 解析的代价）+ 结构确认。只做子串匹配的话，选课页导航栏里一个
/// 「修改密码」表单、或打包进页面的 JS bundle 含这几个 token 就会命中，而
/// 系统性误报会稳定地自我确认两次，直接终止抢课。
pub(crate) fn body_looks_like_login_page(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    let cheap_hit = lowered.contains("loginform")
        || lowered.contains("name=\"password\"")
        || lowered.contains("id=\"password\"")
        || lowered.contains("type=\"password\"")
        || (lowered.contains("username")
            && lowered.contains("password")
            && lowered.contains("login"));
    if !cheap_hit {
        return false;
    }
    structural_login_page(text)
}

/// `action`/`id` 指向登录端点。
fn is_login_endpoint(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["login", "cas", "authserver", "sso", "signin", "idp"]
        .iter()
        .any(|marker| value.contains(marker))
}

/// 页面里有真正的选课目录结构。
///
/// 只认结构性标记，不认泛泛的「选课」二字——登录页横幅上也可能写着选课，
/// 用它做否定判据会漏掉真实的会话过期，那是更危险的方向。
fn page_has_elect_catalog(text: &str) -> bool {
    [
        "electableLesson",
        "lessonListOperator",
        "stdElectCourse",
        "lessonJSONs",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn structural_login_page(html: &str) -> bool {
    static FORM_SEL: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("form").ok());
    static PASSWORD_SEL: LazyLock<Option<Selector>> =
        LazyLock::new(|| Selector::parse("input[type=password]").ok());
    let doc = Html::parse_document(html);
    // 一等判据：存在 form 且它指向登录端点。
    if let Some(selector) = FORM_SEL.as_ref() {
        let has_login_form = doc.select(selector).any(|node| {
            let value = node.value();
            value.attr("action").is_some_and(is_login_endpoint)
                || value.attr("id").is_some_and(is_login_endpoint)
                || value.attr("name").is_some_and(is_login_endpoint)
        });
        if has_login_form {
            return true;
        }
    }
    // 二等判据：有密码输入框，且页面里没有选课目录结构。
    // 「修改密码」表单出现在选课页上时会被这一条挡住。
    if let Some(selector) = PASSWORD_SEL.as_ref() {
        if doc.select(selector).next().is_some() && !page_has_elect_catalog(html) {
            return true;
        }
    }
    false
}

pub(crate) fn extract_login_error(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    for sel in [
        "#loginForm .actionError",
        ".actionError",
        ".actionMessage",
        ".error",
        ".alert-danger",
        "#error",
        ".login-error",
    ] {
        if let Ok(selector) = Selector::parse(sel) {
            if let Some(n) = doc.select(&selector).next() {
                let t = n
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }
    // 优先匹配完整常见句
    for key in [
        "请不要过快点击",
        "密码错误",
        "用户不存在",
        "验证码",
        "被锁定",
        "账号被禁用",
    ] {
        if html.contains(key) {
            return Some(key.into());
        }
    }
    if html.contains("失败") {
        return Some("登录失败".into());
    }
    None
}

pub(crate) fn page_looks_like_elect_ui(text: &str) -> bool {
    text.contains("electableLesson")
        || text.contains("electableLessonList")
        || text.contains("lessonListOperator")
        || text.contains("stdElectCourse")
        || text.contains("选课")
}

pub(crate) fn plausible_profile_id(id: &str, note: &str) -> bool {
    let id = id.trim();
    if id == "0" {
        return true;
    }
    if id.len() < 3 || id.len() > 10 || !id.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let entry_context = note.contains("进入选课")
        || note.contains("checkPayment")
        || note.contains("electionProfile")
        || note.contains("链接");
    let looks_like_year = id
        .parse::<u32>()
        .is_ok_and(|value| (2000..=2100).contains(&value));
    !(looks_like_year && (note.contains("学年") || note.contains("学期")) && !entry_context)
}

pub(crate) fn score_elect_page(text: &str) -> i64 {
    if text.trim().is_empty() {
        return -1000;
    }
    if text.contains("请求参数非法") {
        return -500;
    }
    if body_looks_like_login_page(text) {
        return -400;
    }
    let mut score = text.len() as i64 / 200;
    for (k, w) in [
        ("lessonJSONs", 500i64),
        ("electableLessonList", 300),
        ("electableLesson.no", 250),
        ("lessonListOperator", 250),
        ("electCourseTable", 200),
        ("batchOperator", 150),
        ("stdCount", 120),
        ("已选", 50),
        ("课程序号", 80),
    ] {
        if text.contains(k) {
            score += w;
        }
    }
    // 纯壳页面很小且几乎没有正文
    if text.len() < 6000 && !text.contains("electableLesson") && !text.contains("lessonJSONs") {
        score -= 100;
    }
    score
}

pub(crate) fn extract_profiles_detailed(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |id: String, note: String| {
        if id.is_empty() {
            return;
        }
        if !out.iter().any(|(x, _)| x == &id) {
            out.push((id, note));
        }
    };

    // / Beangen: checkPaymentBeforeElect(profileId, shortTerm)
    if let Ok(re) = Regex::new(r#"checkPaymentBeforeElect\s*\(\s*(\d+)\s*,\s*(\d+)\s*\)"#) {
        let heading_re = Regex::new(r#"(?s)<h2[^>]*>(.*?)</h2>"#).ok();
        for c in re.captures_iter(text) {
            let id = c[1].to_string();
            let start = c.get(0).map(|m| m.start()).unwrap_or(0);
            let prefix = &text[..start];
            let window_start = prefix
                .char_indices()
                .rev()
                .nth(1200)
                .map(|(index, _)| index)
                .unwrap_or(0);
            let window = &text[window_start..start];
            let note = heading_re
                .as_ref()
                .and_then(|heading| {
                    heading
                        .captures_iter(window)
                        .last()
                        .map(|capture| summarize_html(&capture[1]))
                })
                .filter(|note| !note.is_empty())
                .unwrap_or_else(|| "进入选课入口".into());
            push(id, note);
        }
    }

    // href / action 链接
    let re_href = Regex::new(
        r#"(?i)(?:href|action|url)\s*=\s*['"][^'"]*electionProfile\.id=(\d+)[^'"]*['"]"#,
    )
    .ok();
    if let Some(re) = re_href {
        for c in re.captures_iter(text) {
            push(c[1].to_string(), "链接 electionProfile.id".into());
        }
    }
    let re_href2 =
        Regex::new(r#"(?i)(?:href|action|url)\s*=\s*['"][^'"]*profileId=(\d+)[^'"]*['"]"#).ok();
    if let Some(re) = re_href2 {
        for c in re.captures_iter(text) {
            push(c[1].to_string(), "链接 profileId".into());
        }
    }

    // onclick / JS
    let re_js = Regex::new(r#"(?i)electionProfile\.id\s*[:=]\s*['"]?(\d+)['"]?"#).ok();
    if let Some(re) = re_js {
        for c in re.captures_iter(text) {
            push(c[1].to_string(), "脚本 electionProfile.id".into());
        }
    }

    // hidden input
    let re_hidden = Regex::new(
        r#"(?i)<input[^>]*(?:name|id)\s*=\s*['"][^'"]*(?:electionProfile\.id|profileId|profile\.id)[^'"]*['"][^>]*value\s*=\s*['"](\d+)['"]"#,
    )
    .ok();
    if let Some(re) = re_hidden {
        for c in re.captures_iter(text) {
            push(c[1].to_string(), "隐藏域".into());
        }
    }
    let re_hidden2 = Regex::new(
        r#"(?i)<input[^>]*value\s*=\s*['"](\d+)['"][^>]*(?:name|id)\s*=\s*['"][^'"]*(?:electionProfile\.id|profileId)[^'"]*['"]"#,
    )
    .ok();
    if let Some(re) = re_hidden2 {
        for c in re.captures_iter(text) {
            push(c[1].to_string(), "隐藏域".into());
        }
    }

    // 表格行：选课轮次名称 + id
    // e.g. ...id=1234...春季选课...
    let re_row = Regex::new(r#"electionProfile\.id=(\d+)[^<]{0,200}"#).ok();
    if let Some(re) = re_row {
        for c in re.captures_iter(text) {
            let id = c[1].to_string();
            let ctx = c.get(0).map(|m| m.as_str()).unwrap_or("");
            let note = summarize_html(ctx);
            push(
                id,
                if note.is_empty() {
                    "页面片段".into()
                } else {
                    note
                },
            );
        }
    }

    out
}

/// 原始调试页面落盘。
///
/// 落盘即脱敏，而不是只在导出时脱敏：会话 Cookie 在有效期内等于账号，
/// 把最危险的一份留在 %APPDATA% 里、把最安全的一份交出去，顺序是反的。
/// 导出侧的 `redact_diagnostic_page` 保留为二次防线。
pub(crate) fn save_debug_text(name: &str, content: &str) -> Result<()> {
    let dir = AppConfig::debug_dir();
    save_debug_text_in(&dir, name, content)?;
    let _ = AppConfig::retain_debug_files();
    Ok(())
}

/// 目录可注入，测试才能验证「落盘的那份确实已脱敏」而不是往用户
/// 真实的 %APPDATA% 里写东西。
pub(crate) fn save_debug_text_in(
    dir: &std::path::Path,
    name: &str,
    content: &str,
) -> Result<Option<std::path::PathBuf>> {
    // 含选课提交表单的页面整份丢弃，与导出侧语义一致。
    let Some(content) = crate::config::redact_diagnostic_page(name, content) else {
        return Ok(None);
    };
    std::fs::create_dir_all(dir)?;
    let safe_name = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let path = dir.join(safe_name);
    std::fs::write(&path, content.as_bytes())?;
    Ok(Some(path))
}

pub(crate) fn extract_all_profile_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let patterns = [
        r#"electionProfile\.id=(\d+)"#,
        r#"electionProfile\.id\s*[:=]\s*['"]?(\d+)"#,
        r#"profileId=(\d+)"#,
        r#"profileId\s*[:=]\s*['"]?(\d+)"#,
        r#"checkPaymentBeforeElect\s*\(\s*(\d+)\s*,"#,
    ];
    for p in patterns {
        if let Ok(re) = Regex::new(p) {
            for c in re.captures_iter(text) {
                let id = c[1].to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

pub(crate) fn extract_project_semester(html: &str) -> (Option<String>, Option<String>) {
    let project = Regex::new(r#"projectId=(\d+)"#)
        .ok()
        .and_then(|re| re.captures(html).map(|c| c[1].to_string()));
    let semester = Regex::new(r#"semesterId=(\d+)"#)
        .ok()
        .and_then(|re| re.captures(html).map(|c| c[1].to_string()));
    (project, semester)
}

/// 解析 返回的 JS 风格数组：var lessonJSONs = [{id:1,no:'x',...}]
pub(crate) fn parse_lessons_js_like(text: &str) -> Result<Vec<Lesson>> {
    let arr = extract_js_array_after(text, "lessonJSONs")
        .or_else(|| extract_js_array_after(text, "electableLessons"))
        .or_else(|| {
            // 整段就是数组时
            let t = text.trim();
            if t.starts_with('[') {
                Some(t.to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("no lessonJSONs array"))?;

    // 优先：正则直接抠字段（最稳，不依赖完整 JSON 转换）
    if let Ok(list) = parse_lessons_by_regex(&arr) {
        if !list.is_empty() {
            return Ok(list);
        }
    }

    // 次选：JS 字面量转 JSON
    let json = js_like_to_json(&arr)?;
    parse_lessons_json(&json)
}

/// 直接从 JS 对象字面量里提取课程字段
pub(crate) fn parse_lessons_by_regex(arr: &str) -> Result<Vec<Lesson>> {
    // 每个课对象以 {id:数字 开头
    let re_obj = Regex::new(r"\{id:(\d+),").map_err(|e| anyhow!(e))?;
    let re_no = Regex::new(r"\bno:'((?:\\'|[^'])*)'").map_err(|e| anyhow!(e))?;
    let re_name = Regex::new(r"\bname:'((?:\\'|[^'])*)'").map_err(|e| anyhow!(e))?;
    let re_teachers = Regex::new(r"\bteachers:'((?:\\'|[^'])*)'").map_err(|e| anyhow!(e))?;
    let re_std = Regex::new(r"\bstdCount:(\d+)").map_err(|e| anyhow!(e))?;
    let re_limit = Regex::new(r"\blimitCount:(\d+)").map_err(|e| anyhow!(e))?;

    let mut starts: Vec<usize> = re_obj.find_iter(arr).map(|m| m.start()).collect();
    if starts.is_empty() {
        bail!("no lesson objects");
    }
    starts.push(arr.len());

    let mut out = Vec::new();
    for w in starts.windows(2) {
        let chunk = &arr[w[0]..w[1]];
        let Some(id_caps) = re_obj.captures(chunk) else {
            continue;
        };
        let id = id_caps[1].to_string();
        let no = re_no
            .captures(chunk)
            .map(|c| unesc_js_str(&c[1]))
            .unwrap_or_default();
        if no.is_empty() {
            continue;
        }
        let name = re_name
            .captures(chunk)
            .map(|c| unesc_js_str(&c[1]))
            .unwrap_or_default();
        let teachers = re_teachers
            .captures(chunk)
            .map(|c| unesc_js_str(&c[1]))
            .unwrap_or_default();
        let selected = re_std.captures(chunk).and_then(|c| c[1].parse().ok());
        let limit = re_limit.captures(chunk).and_then(|c| c[1].parse().ok());
        out.push(Lesson {
            id,
            no,
            name,
            teachers,
            seat: SeatInfo::from_counts(selected, limit),
        });
    }
    if out.is_empty() {
        bail!("regex parsed 0 lessons");
    }
    Ok(out)
}

pub(crate) fn unesc_js_str(s: &str) -> String {
    s.replace("\\'", "'").replace("\\\\", "\\")
}

pub(crate) fn js_like_to_json(input: &str) -> Result<String> {
    // 用 char 遍历，正确处理中文等多字节字符
    let mut out = String::with_capacity(input.len() + 128);
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut in_str = false;
    let mut str_quote: char = '\0';
    let mut esc = false;

    while i < chars.len() {
        let ch = chars[i];
        if in_str {
            if esc {
                match ch {
                    '\'' if str_quote == '\'' => out.push('\''),
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    'n' => out.push_str("\\n"),
                    'r' => out.push_str("\\r"),
                    't' => out.push_str("\\t"),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                esc = false;
                i += 1;
                continue;
            }
            if ch == '\\' {
                esc = true;
                i += 1;
                continue;
            }
            if ch == str_quote {
                out.push('"');
                in_str = false;
                i += 1;
                continue;
            }
            if ch == '"' {
                out.push_str("\\\"");
            } else if ch == '\n' || ch == '\r' {
                out.push(' ');
            } else {
                out.push(ch);
            }
            i += 1;
            continue;
        }

        // not in string
        if ch == '\'' || ch == '"' {
            in_str = true;
            str_quote = ch;
            out.push('"');
            i += 1;
            continue;
        }

        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            if j < chars.len() && chars[j] == ':' {
                out.push('"');
                out.push_str(&ident);
                out.push('"');
            } else {
                out.push_str(&ident);
            }
            continue;
        }

        out.push(ch);
        i += 1;
    }
    Ok(out)
}

pub(crate) fn merge_lesson_counts(lessons: &mut [Lesson], counts_js: &str) -> usize {
    // window.lessonId2Counts={'371644':{sc:10,lc:50},...}
    let re = &*RE_LESSON_COUNTS;
    let mut map = std::collections::HashMap::<String, (u32, u32)>::new();
    for c in re.captures_iter(counts_js) {
        let id = c[1].to_string();
        let sc = c[2].parse::<u32>().unwrap_or(0);
        let lc = c[3].parse::<u32>().unwrap_or(0);
        map.insert(id, (sc, lc));
    }
    if map.is_empty() {
        return 0;
    }
    let mut n = 0usize;
    for les in lessons.iter_mut() {
        if let Some((sc, lc)) = map.get(&les.id) {
            les.seat = SeatInfo::from_counts(Some(*sc), Some(*lc));
            if les.seat.is_known() {
                n += 1;
            }
        }
    }
    n
}

pub(crate) fn parse_lessons_json(text: &str) -> Result<Vec<Lesson>> {
    let text = text.trim();
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            let start = text.find('[').ok_or_else(|| anyhow!("not json"))?;
            let end = text.rfind(']').ok_or_else(|| anyhow!("not json"))?;
            // start > end（WAF 拦截页、乱序截断的壳页，如 "参数非法]…x = ["）时
            // &text[start..=end] 直接 panic。这条路径是可达的：调用方在前两级
            // 解析失败后把服务器原始响应喂进来，而 panic 发生在 spawn_task 里且
            // JoinHandle 被 drop——抢课任务会在冲刺中途无声死掉。
            if start >= end {
                bail!("not json: unbalanced brackets");
            }
            serde_json::from_str(&text[start..=end]).context("json array parse failed")?
        }
    };
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("data").and_then(|d| d.as_array().cloned()))
        .or_else(|| value.get("lessons").and_then(|d| d.as_array().cloned()))
        .ok_or_else(|| anyhow!("no array"))?;

    let mut out = Vec::new();
    for item in arr {
        let id = first_str(&item, &["id", "lessonId"]).unwrap_or_default();
        let no = first_str(&item, &["no", "lessonNo", "code"]).unwrap_or_default();
        if id.is_empty() || no.is_empty() {
            continue;
        }
        let name = first_str(&item, &["name", "courseName", "course.name"]).unwrap_or_default();
        let teachers =
            first_str(&item, &["teachers", "teacher", "teacherNames"]).unwrap_or_default();
        let selected = first_u32(&item, &["stdCount", "selectedCount", "sc", "electedCount"]);
        let limit = first_u32(
            &item,
            &["limitCount", "limit", "lc", "maxStdCount", "courseLimit"],
        );
        out.push(Lesson {
            id,
            no,
            name,
            teachers,
            seat: SeatInfo::from_counts(selected, limit),
        });
    }
    if out.is_empty() {
        bail!("empty lessons");
    }
    Ok(out)
}

pub(crate) fn parse_lessons_from_page(text: &str) -> Result<Vec<Lesson>> {
    let markers = ["lessonJSONs", "electableLessons", "var lessons"];
    for marker in markers {
        if let Some(json) = extract_js_array_after(text, marker) {
            if let Ok(list) = parse_lessons_json(&json) {
                if !list.is_empty() {
                    return Ok(list);
                }
            }
        }
    }
    let patterns = [
        r"lessonJSONs\s*=\s*(\[[\s\S]*?\]);",
        r"electableLessons\s*=\s*(\[[\s\S]*?\]);",
        r"var\s+lessons\s*=\s*(\[[\s\S]*?\]);",
    ];
    for p in patterns {
        let re = Regex::new(p)?;
        if let Some(c) = re.captures(text) {
            if let Ok(list) = parse_lessons_json(&c[1]) {
                return Ok(list);
            }
        }
    }
    bail!("no embedded lessons")
}

pub(crate) fn extract_js_array_after(text: &str, marker: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find(marker) {
        let abs = search_from + rel;
        let after = &text[abs + marker.len()..];
        let Some(eq_rel) = after.find('[') else {
            search_from = abs + marker.len();
            continue;
        };
        let between = &after[..eq_rel];
        if !between.contains('=') {
            search_from = abs + marker.len();
            continue;
        }
        let start = abs + marker.len() + eq_rel;
        let bytes = text.as_bytes();
        if start >= bytes.len() || bytes[start] != b'[' {
            search_from = abs + marker.len();
            continue;
        }
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        let mut quote = b'\0';
        for i in start..bytes.len() {
            let b = bytes[i];
            if in_str {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == quote {
                    in_str = false;
                }
                continue;
            }
            match b {
                b'\'' | b'"' => {
                    in_str = true;
                    quote = b;
                }
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[start..=i].to_string());
                    }
                }
                _ => {}
            }
        }
        search_from = abs + marker.len();
    }
    None
}

pub(crate) fn parse_lessons_from_html_table(text: &str) -> Vec<Lesson> {
    let doc = Html::parse_document(text);
    let row_sel = Selector::parse("#electableLessonList_data tr, table tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let count_sel = Selector::parse(".stdCount").unwrap();
    let re_count = Regex::new(r"(\d+)\s*/\s*(\d+)").unwrap();
    let re_id = Regex::new(r"(?:lessonId|electLesson|[?&]id)=?(\d{3,})").unwrap();

    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let tds: Vec<String> = row
            .select(&td_sel)
            .map(|td| {
                td.text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .collect();
        if tds.is_empty() {
            continue;
        }
        let no = tds
            .iter()
            .find(|s| s.contains('.') && s.chars().any(|c| c.is_ascii_digit()) && s.len() >= 5)
            .cloned()
            .unwrap_or_default();
        if no.is_empty() {
            continue;
        }
        let capacity_text = row
            .select(&count_sel)
            .next()
            .map(|n| n.text().collect::<String>())
            .or_else(|| tds.iter().find(|s| re_count.is_match(s)).cloned())
            .unwrap_or_default();
        let seat = re_count
            .captures(&capacity_text)
            .map(|c| SeatInfo::from_counts(c[1].parse().ok(), c[2].parse().ok()))
            .unwrap_or(SeatInfo::Unknown);
        let row_html = row.html();
        let id = re_id
            .captures(&row_html)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let name = tds.get(1).cloned().unwrap_or_default();
        let teachers = tds.get(2).cloned().unwrap_or_default();
        out.push(Lesson {
            id,
            no,
            name,
            teachers,
            seat,
        });
    }
    out
}

pub(crate) fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(s) = x.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(n) = x.as_i64() {
                return Some(n.to_string());
            }
            if let Some(n) = x.as_u64() {
                return Some(n.to_string());
            }
        }
        if k.contains('.') {
            let mut cur = v;
            let mut ok = true;
            for part in k.split('.') {
                match cur.get(part) {
                    Some(next) => cur = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some(s) = cur.as_str() {
                    return Some(s.to_string());
                }
                if let Some(n) = cur.as_i64() {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn first_u32(v: &Value, keys: &[&str]) -> Option<u32> {
    first_str(v, keys)?.parse().ok()
}

pub(crate) fn summarize_html(text: &str) -> String {
    let plain = Regex::new(r"<[^>]+>")
        .ok()
        .map(|re| re.replace_all(text, " ").to_string())
        .unwrap_or_else(|| text.to_string());
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = plain.chars().take(160).collect();
    if plain.chars().count() > 160 {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C-01：所有 ']' 都出现在第一个 '[' 之前时，&text[start..=end] 会 panic。
    // 这条路径是可达的（WAF 拦截页、乱序截断的壳页），而 panic 在 spawn_task
    // 里被吞掉——抢课任务会在冲刺中途无声死掉。必须是 Err，不是 panic。
    #[test]
    fn unbalanced_brackets_are_an_error_not_a_panic() {
        for text in [
            "参数非法]<script>var x = [",
            "]",
            "][",
            "] 请求被拦截 [",
            "}] 网关错误 [{",
        ] {
            let result = std::panic::catch_unwind(|| parse_lessons_json(text));
            assert!(result.is_ok(), "parse_lessons_json panicked on {text:?}");
            assert!(
                result.unwrap().is_err(),
                "unbalanced {text:?} must be a parse error"
            );
        }
        // 正常的数组仍然要解析成功。
        assert!(parse_lessons_json(
            r#"[{"id":"371644","no":"CS.1","name":"x","teachers":"t","stdCount":1,"limitCount":2}]"#
        )
        .is_ok());
    }

    // C-06：判定必须先抠结果容器，且强成功标记优先于瞬态繁忙词表。
    #[test]
    fn success_message_containing_later_is_not_downgraded_to_busy() {
        // 「操作成功，请稍后在已选课程中查看」含「稍后」。先判繁忙就会把
        // 成功当成可重试，目标留在 pending 里被反复重复提交。
        let html = r#"<html><body><div class="actionMessage">操作成功，请稍后在已选课程中查看</div></body></html>"#;
        assert!(matches!(
            classify_elect_response(html),
            ElectResult::Success { .. }
        ));
    }

    #[test]
    fn page_footer_and_unrelated_full_courses_do_not_hijack_the_verdict() {
        // 页脚一句「请勿频繁刷新」曾能让每次提交都变成 Busy。
        let html = r#"<html><body>
            <div class="actionMessage">选课成功</div>
            <table><tr><td>其他课程 人数已满</td></tr></table>
            <div class="footer">请勿频繁刷新本页面</div>
        </body></html>"#;
        assert!(
            matches!(classify_elect_response(html), ElectResult::Success { .. }),
            "result container must win over footer and unrelated rows"
        );
    }

    #[test]
    fn credit_cap_rejection_stays_terminal_while_seat_full_stays_retryable() {
        let credit = r#"<div class="actionError">学分已满，不允许再选</div>"#;
        assert!(matches!(
            classify_elect_response(credit),
            ElectResult::Failed { .. }
        ));
        let seats = r#"<div class="actionError">人数已满</div>"#;
        assert!(matches!(
            classify_elect_response(seats),
            ElectResult::Full { .. }
        ));
        let busy = r#"<div class="actionError">系统繁忙，请稍后再试</div>"#;
        assert!(matches!(
            classify_elect_response(busy),
            ElectResult::Busy { .. }
        ));
    }

    // C-02：GBK 站点整页变 U+FFFD 后，其上所有中文子串判定都会静默失效——
    // 「人数已满」认不出、选课成功被判成 Failed 并终态放弃。
    #[test]
    fn gbk_bodies_decode_by_header_meta_and_sniffing() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("人数已满，请稍后再试");

        // 1) Content-Type 声明
        let text = decode_body(Some("text/html; charset=GBK"), &gbk_bytes).unwrap();
        assert!(text.contains("人数已满"), "header charset ignored: {text}");

        // 2) <meta charset>
        let mut with_meta = b"<html><head><meta charset=\"gb2312\"></head><body>".to_vec();
        with_meta.extend_from_slice(&gbk_bytes);
        with_meta.extend_from_slice(b"</body></html>");
        let text = decode_body(Some("text/html"), &with_meta).unwrap();
        assert!(text.contains("人数已满"), "meta charset ignored: {text}");

        // 3) 什么都没声明：UTF-8 解不动时回退 GB18030
        let text = decode_body(None, &gbk_bytes).unwrap();
        assert!(text.contains("人数已满"), "sniffing failed: {text}");

        // 4) UTF-8 正常路径不受影响
        let text = decode_body(Some("text/html; charset=utf-8"), "选课成功".as_bytes()).unwrap();
        assert_eq!(text, "选课成功");

        // 5) 声明了字符集却整片解不动：必须显式报错，不能继续喂乱码给判定层
        let broken = vec![0xff_u8; 64];
        assert!(
            decode_body(Some("text/html; charset=utf-8"), &broken).is_err(),
            "silent mojibake is worse than an explicit failure"
        );
    }

    // C-07：选课页导航栏里的「修改密码」表单不能被当成登录页。
    // 系统性误报会稳定地自我确认两次，直接 clear_session 终止抢课。
    #[test]
    fn change_password_form_on_an_elect_page_is_not_a_login_page() {
        let elect_page = r#"<html><body>
            <div id="nav"><form action="/eams/security/my.action">
                <input type="password" name="password"><input type="password" name="password2">
            </form></div>
            <script>var lessonJSONs=[{id:371644,no:'CS.1'}];</script>
            <div id="electableLessonList">课程列表</div>
        </body></html>"#;
        assert!(
            !body_looks_like_login_page(elect_page),
            "change-password form must not read as a login page"
        );

        // 真正的登录页仍然要认出来——漏判是更危险的方向。
        for page in [
            r#"<form id="loginForm"><input name="password"></form>"#,
            r#"<html><body><form action="/eams/loginExt.action" method="post">
                 <input name="username"><input type="password" name="password">
               </form></body></html>"#,
            r#"<html><body><form action="https://cas.example.edu/login">
                 <input type="password" name="password">
               </form></body></html>"#,
        ] {
            assert!(
                body_looks_like_login_page(page),
                "missed a real login page: {page}"
            );
        }
    }

    // S-01：调试页落盘时就必须脱敏。会话 Cookie 在有效期内等于账号，
    // 而 %APPDATA%\debug 最长驻留 7 天。
    #[test]
    fn debug_pages_are_redacted_before_they_hit_the_disk() {
        let dir = std::env::temp_dir().join(format!("cs-debug-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let page = "Set-Cookie: JSESSIONID=abc123\n<input name=\"password\" value=\"hunter2\">";
        let path = save_debug_text_in(&dir, "login.html", page)
            .unwrap()
            .expect("normal page must be written");
        let written = std::fs::read_to_string(&path).unwrap();
        for secret in ["abc123", "hunter2"] {
            assert!(!written.contains(secret), "debug dump leaked {secret}");
        }
        assert!(written.contains("[已隐藏]"));

        // 含提交表单的页面整份不落盘。
        let submission =
            save_debug_text_in(&dir, "submit.html", "optype=true&operator0=371644:true:0").unwrap();
        assert!(submission.is_none(), "submission form must not be written");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tmp_debug {
    #[test]
    fn tmp_login_fixture() {
        let page = r#"<form id="loginForm"><input name="password"></form>"#;
        assert!(
            super::body_looks_like_login_page(page),
            "fixture not detected"
        );
    }
}

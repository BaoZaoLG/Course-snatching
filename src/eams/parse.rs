use super::{EamsError, ElectResult, Lesson, SeatInfo};
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

pub(crate) fn classify_elect_response(text: &str) -> ElectResult {
    let summary = summarize_html(text);
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let detail = first_str(&value, &["message", "msg", "detail", "error"])
            .unwrap_or_else(|| summary.clone());
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            return ElectResult::Success { detail };
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            if detail.contains("已满") || detail.contains("上限人数") {
                return ElectResult::Full { detail };
            }
            return ElectResult::Failed { detail };
        }
    }

    if text.contains("上限人数已满") || text.contains("人数已满") || text.contains("已满")
    {
        return ElectResult::Full { detail: summary };
    }
    if text.contains("已经选过") || text.contains("已选过") {
        return ElectResult::Success { detail: summary };
    }
    if ["失败", "未成功", "冲突", "不允许", "错误", "不可选"]
        .iter()
        .any(|marker| text.contains(marker))
    {
        return ElectResult::Failed { detail: summary };
    }
    if text.contains("选课成功") || text.contains("操作成功") {
        return ElectResult::Success { detail: summary };
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

pub(crate) fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
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

pub(crate) fn looks_like_login_page(url: &Url, text: &str) -> bool {
    body_looks_like_login_page(text)
        || (url.as_str().to_ascii_lowercase().contains("login") && text.trim().is_empty())
}

pub(crate) fn body_looks_like_login_page(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("loginform")
        || text.contains("name=\"password\"")
        || text.contains("id=\"password\"")
        || (text.contains("username") && text.contains("password") && text.contains("login"))
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

    // SIAS / Beangen: checkPaymentBeforeElect(profileId, shortTerm)
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

pub(crate) fn save_debug_text(name: &str, content: &str) -> Result<()> {
    let dir = debug_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(name), content)?;
    Ok(())
}

fn debug_dir() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.join("runtime").join("debug");
        }
    }
    std::path::PathBuf::from("runtime/debug")
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

/// 解析 SIAS 返回的 JS 风格数组：var lessonJSONs = [{id:1,no:'x',...}]
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

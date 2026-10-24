//! Agent 内置工具的实现：时间 / 计算 / 联网搜索 / 抓网页正文。
//!
//! 全部本地完成，不依赖任何第三方 API Key：
//! * 搜索走 DuckDuckGo 的 HTML 端点（免 key），失败时回退 Bing；
//! * 抓网页只取正文（剥 script/style），并做长度截断，避免把整个页面塞进上下文。

use chrono::Datelike;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单次抓网页的正文上限（字符）。
const MAX_PAGE_CHARS: usize = 12_000;
/// 搜索结果条数上限。
const MAX_RESULTS: usize = 8;
/// 抓取超时（秒）。
const FETCH_TIMEOUT_SECS: u64 = 20;

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| e.to_string())
}

/// 当前日期时间（本地时区，含星期）。
pub fn current_time_text() -> String {
    let now = chrono::Local::now();
    let wk = match now.weekday() {
        chrono::Weekday::Mon => "星期一",
        chrono::Weekday::Tue => "星期二",
        chrono::Weekday::Wed => "星期三",
        chrono::Weekday::Thu => "星期四",
        chrono::Weekday::Fri => "星期五",
        chrono::Weekday::Sat => "星期六",
        chrono::Weekday::Sun => "星期日",
    };
    now.format("%Y-%m-%d %H:%M:%S").to_string() + " " + wk
}

/// 计算数学表达式：先转 RPN 再求值，不用 eval。
pub fn calculate(expr: &str) -> String {
    let e = expr.trim();
    if e.is_empty() {
        return "表达式为空".into();
    }
    if e.len() > 500 {
        return "表达式过长".into();
    }
    match eval_expr(e) {
        Some(v) => {
            // 整数就不带小数点
            if (v - v.round()).abs() < 1e-12 {
                format!("{}", v.round() as i64)
            } else {
                format!("{}", v)
            }
        }
        None => format!("无法计算表达式：{}", e),
    }
}

// ---------------------------- 表达式求值 ----------------------------

fn eval_expr(s: &str) -> Option<f64> {
    let tokens = tokenize(s)?;
    let rpn = to_rpn(&tokens)?;
    eval_rpn(&rpn)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Num(f64),
    Op(char),
    LP,
    RP,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut num = String::new();
    for ch in s.chars() {
        match ch {
            '0'..='9' | '.' => num.push(ch),
            '+' | '-' | '*' | '/' | '%' | '^' => {
                if !num.is_empty() {
                    out.push(Tok::Num(num.parse().ok()?));
                    num.clear();
                }
                out.push(Tok::Op(ch));
            }
            '(' => {
                if !num.is_empty() {
                    out.push(Tok::Num(num.parse().ok()?));
                    num.clear();
                }
                out.push(Tok::LP);
            }
            ')' => {
                if !num.is_empty() {
                    out.push(Tok::Num(num.parse().ok()?));
                    num.clear();
                }
                out.push(Tok::RP);
            }
            ' ' | '\t' | ',' => {}
            _ => return None,
        }
    }
    if !num.is_empty() {
        out.push(Tok::Num(num.parse().ok()?));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn prec(op: char) -> u8 {
    match op {
        '+' | '-' => 1,
        '*' | '/' | '%' => 2,
        '^' => 3,
        _ => 0,
    }
}

fn right_assoc(op: char) -> bool {
    op == '^'
}

fn to_rpn(toks: &[Tok]) -> Option<Vec<Tok>> {
    let mut out: Vec<Tok> = Vec::new();
    let mut ops: Vec<Tok> = Vec::new();
    for t in toks {
        match t {
            Tok::Num(_) => out.push(t.clone()),
            Tok::Op(o) => {
                while let Some(Tok::Op(top)) = ops.last().copied() {
                    if prec(top) > prec(*o) || (prec(top) == prec(*o) && !right_assoc(*o)) {
                        out.push(Tok::Op(top));
                        ops.pop();
                    } else {
                        break;
                    }
                }
                ops.push(Tok::Op(*o));
            }
            Tok::LP => ops.push(Tok::LP),
            Tok::RP => {
                let mut found = false;
                while let Some(t) = ops.pop() {
                    if t == Tok::LP {
                        found = true;
                        break;
                    }
                    out.push(t);
                }
                if !found {
                    return None;
                }
            }
        }
    }
    while let Some(t) = ops.pop() {
        if t == Tok::LP {
            return None;
        }
        out.push(t);
    }
    Some(out)
}

fn eval_rpn(rpn: &[Tok]) -> Option<f64> {
    let mut st: Vec<f64> = Vec::new();
    for t in rpn {
        match t {
            Tok::Num(n) => st.push(*n),
            Tok::Op(o) => {
                let b = st.pop()?;
                let a = st.pop()?;
                let v = match o {
                    '+' => a + b,
                    '-' => a - b,
                    '*' => a * b,
                    '/' => {
                        if b == 0.0 {
                            return None;
                        }
                        a / b
                    }
                    '%' => {
                        if b == 0.0 {
                            return None;
                        }
                        a % b
                    }
                    '^' => a.powf(b),
                    _ => return None,
                };
                if !v.is_finite() {
                    return None;
                }
                st.push(v);
            }
            _ => return None,
        }
    }
    if st.len() == 1 {
        Some(st[0])
    } else {
        None
    }
}

// ---------------------------- 联网搜索 ----------------------------

/// 联网搜索：DuckDuckGo HTML 端点优先，失败回退 Bing。
pub async fn web_search(query: &str) -> String {
    let q = query.trim();
    if q.is_empty() {
        return "搜索词为空".into();
    }
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return format!("搜索失败：{}", e),
    };
    match ddg_search(&client, q).await {
        Some(r) if !r.is_empty() => r,
        _ => match bing_search(&client, q).await {
            Some(r) if !r.is_empty() => r,
            _ => "没有搜到结果（搜索引擎可能不可达，可让用户检查网络/代理）".into(),
        },
    }
}

async fn ddg_search(client: &reqwest::Client, q: &str) -> Option<String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlenc(q));
    let html = client
        .get(&url)
        .header("User-Agent", UA)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let items = parse_ddg(&html);
    if items.is_empty() {
        return None;
    }
    Some(render_results(&items))
}

/// 解析 DDG HTML：result__a 是标题链接，result__snippet 是摘要。
/// 链接形如 //duckduckgo.com/l/?uddg=<encoded real url>，需要解出来。
fn parse_ddg(html: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(a) = rest.find("result__a") {
        let seg = &rest[a..];
        let href = match attr(seg, "href") {
            Some(h) => h,
            None => break,
        };
        let title = text_between(seg, '>', '<');
        let snippet = match seg.find("result__snippet") {
            Some(sp) => text_between(&seg[sp..], '>', '<'),
            None => String::new(),
        };
        let real = decode_uddg(&href).unwrap_or_else(|| href.clone());
        if !title.is_empty() {
            out.push((title, real, snippet));
        }
        if out.len() >= MAX_RESULTS {
            break;
        }
        let adv = href.len().max(1).min(seg.len());
        rest = &seg[adv..];
    }
    out
}

fn decode_uddg(href: &str) -> Option<String> {
    let idx = href.find("uddg=")?;
    let tail = &href[idx + 5..];
    let end = tail.find('&').unwrap_or(tail.len());
    Some(urldecode(&tail[..end]))
}

async fn bing_search(client: &reqwest::Client, q: &str) -> Option<String> {
    let url = format!("https://www.bing.com/search?q={}", urlenc(q));
    let html = client
        .get(&url)
        .header("User-Agent", UA)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let mut out = Vec::new();
    let mut rest = html.as_str();
    while let Some(i) = rest.find("b_algo") {
        let seg = &rest[i..];
        let href = match attr(seg, "href") {
            Some(h) => h,
            None => break,
        };
        let title = text_between(seg, '>', '<');
        let snippet = match seg.find("<p") {
            Some(p) => strip_tags(&text_between(&seg[p..], '>', '<')),
            None => String::new(),
        };
        if !title.is_empty() && href.starts_with("http") {
            out.push((title, href, snippet));
        }
        if out.len() >= MAX_RESULTS {
            break;
        }
        rest = &seg[16..];
    }
    if out.is_empty() {
        None
    } else {
        Some(render_results(&out))
    }
}

fn render_results(items: &[(String, String, String)]) -> String {
    let mut s = String::new();
    for (i, (t, u, sn)) in items.iter().enumerate() {
        s.push_str(&format!("{}. {}\n   {}\n", i + 1, t, u));
        if !sn.trim().is_empty() {
            s.push_str(&format!("   {}\n", sn.trim()));
        }
    }
    s
}

// ---------------------------- 抓网页正文 ----------------------------

pub async fn fetch_url(url: &str) -> String {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return "URL 必须以 http(s):// 开头".into();
    }
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return format!("抓取失败：{}", e),
    };
    let resp = match client.get(u).header("User-Agent", UA).send().await {
        Ok(r) => r,
        Err(e) => return format!("抓取失败：{}", e),
    };
    if !resp.status().is_success() {
        return format!("抓取失败：HTTP {}", resp.status().as_u16());
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return format!("读取失败：{}", e),
    };
    let text = html_to_text(&body);
    if text.trim().is_empty() {
        return "页面没有可提取的文本（可能是纯 JS 渲染或二进制内容）".into();
    }
    text.chars().take(MAX_PAGE_CHARS).collect()
}

/// HTML -> 正文：去 script/style，块级元素补换行，剥标签，解码常见实体。
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let lb = lower.as_bytes();
    let mut i = 0usize;
    let mut skip_until: Option<&[u8]> = None;
    while i < bytes.len() {
        if let Some(close) = skip_until {
            if lb[i..].starts_with(close) {
                skip_until = None;
                i += close.len();
            } else {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'<' {
            let end = match html[i..].find('>') {
                Some(e) => i + e + 1,
                None => break,
            };
            let tag = &lower[i..end];
            if tag.starts_with("<script") {
                skip_until = Some(b"</script>");
            } else if tag.starts_with("<style") {
                skip_until = Some(b"</style>");
            } else if tag.starts_with("<!--") {
                skip_until = Some(b"-->");
            } else if BLOCK_TAGS.iter().any(|b| tag.starts_with(b)) {
                out.push('\n');
            }
            i = end;
            continue;
        }
        let next = match html[i..].find('<') {
            Some(n) => i + n,
            None => bytes.len(),
        };
        out.push_str(&html[i..next]);
        i = next;
    }
    decode_entities(&out)
}

const BLOCK_TAGS: &[&str] = &[
    "</p", "</div", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6", "</li", "</tr", "</td",
    "</th", "</table", "</section", "</article", "</header", "</footer", "</nav", "</ul",
    "</ol", "</blockquote", "<br", "</pre", "</dd", "</dt",
];

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "\u{2014}")
        .replace("&hellip;", "\u{2026}")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_entities(out.trim())
}

// ---------------------------- 小工具 ----------------------------

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";

/// 从一段 HTML 里取某个属性值（从当前位置向后找第一个同名属性）。
fn attr(seg: &str, name: &str) -> Option<String> {
    let idx = seg.find(name)?;
    let after = &seg[idx + name.len()..];
    let eq = after.find('=')?;
    let rest = after[eq + 1..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)?;
    Some(rest[1..1 + end].to_string())
}

/// 取 open 与下一个 close 之间的文本并去标签。
fn text_between(seg: &str, open: char, close: char) -> String {
    let Some(s) = seg.find(open) else {
        return String::new();
    };
    let s = s + 1;
    let Some(e) = seg[s..].find(close) else {
        return String::new();
    };
    strip_tags(&seg[s..s + e])
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]));
                if let (Some(a), Some(b)) = h {
                    out.push(a * 16 + b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 生成调用 id（模型有时不给，需要补一个稳定的）。
pub fn new_call_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("call_{:x}", n)
}

/// 把工具结果压成一行摘要，用于前端时间线展示。
pub fn brief(name: &str, result: &str) -> String {
    let one: String = result
        .chars()
        .take(120)
        .collect::<String>()
        .replace('\n', " ");
    format!("{} -> {}", name, one)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_basic_expressions() {
        assert_eq!(calculate("1+2"), "3");
        assert_eq!(calculate("(12+3)*4.5"), "67.5");
        assert_eq!(calculate("2^10"), "1024");
        assert_eq!(calculate("10/4"), "2.5");
        assert_eq!(calculate(" 7 % 3 "), "1");
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(calculate("1+").contains("无法计算"));
        assert!(calculate("(1+2").contains("无法计算"));
        assert!(calculate("1/0").contains("无法计算"));
        assert!(calculate("abc").contains("无法计算"));
    }

    #[test]
    fn decodes_entities_and_strips_tags() {
        assert_eq!(strip_tags("<b>hi</b> &amp; bye"), "hi & bye");
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
    }

    #[test]
    fn html_to_text_drops_scripts() {
        let h = "<html><head><style>body{color:red}</style></head><body><p>Hello</p><script>var x=1;</script><p>World</p></body></html>";
        let t = html_to_text(h);
        assert!(t.contains("Hello"), "{}", t);
        assert!(t.contains("World"), "{}", t);
        assert!(!t.contains("color:red"), "style should be dropped: {}", t);
        assert!(!t.contains("var x"), "script should be dropped: {}", t);
    }

    #[test]
    fn url_coding_roundtrip() {
        let s = "今天天气怎么样?";
        let e = urlenc(s);
        assert!(!e.contains(' '));
        assert_eq!(urldecode(&e), s);
    }

    #[test]
    fn parses_ddg_like_html() {
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&amp;rut=xyz">示例标题</a><a class="result__snippet">这是摘要内容</a>"#;
        let items = parse_ddg(html);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "示例标题");
        assert_eq!(items[0].1, "https://example.com/a");
        assert_eq!(items[0].2, "这是摘要内容");
    }
}

use crate::chat_attach::{
    build_openai_content, image_markdown, is_image_model, Attachment,
};
use crate::config::{load_config, mark_account_error, resolve_accounts, touch_account};
use crate::files;
use crate::providers::{build_client, chat_url};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 流式中止标志：后端按 conv_id 登记，前端点「停止」时置位。
pub type AbortFlag = Arc<AtomicBool>;

pub fn is_aborted(abort: &Option<AbortFlag>) -> bool {
    abort.as_ref().map(|a| a.load(Ordering::SeqCst)).unwrap_or(false)
}

/// UI 通过该通道接收流式事件。
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// 一段增量文本
    Delta(String),
    /// 账号异常，已切换到下一个（携带账号名与原因）
    Switch { account: String, reason: String },
    /// 当前生效账号（成功建立连接时）
    Active { account: String },
    /// 流结束
    Done,
    /// 出错（所有账号都失败或致命错误）
    Error(String),
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// 附件（图片/文本/文件）。旧会话没有该字段，`#[serde(default)]` 保证兼容。
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// 在后台任务里调用 provider 的 OpenAI 兼容 /chat/completions（stream=true）。
/// 遍历账号池做 failover：某账号失效/被封/额度耗尽时自动切下一个健康账号。
///
/// `image_mode`：用户手动打开 🎨 开关（或模型名是生图模型）时走生图分支。
#[allow(clippy::too_many_arguments)]
pub async fn stream_chat(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    conv_id: &str,
    tx: mpsc::UnboundedSender<ChatEvent>,
    image_mode: bool,
    abort: Option<AbortFlag>,
) {
    // 按协议类型分发：网页版（ChatGPT / Claude）走 webchat 模块
    let ptype = load_config(dir)
        .ok()
        .and_then(|c| c.providers.into_iter().find(|p| p.id == provider_id))
        .map(|p| p.ptype)
        .unwrap_or_else(|| "openai".to_string());

    if ptype == "chatgpt_web" || ptype == "claude_web" {
        crate::webchat::stream_web_chat(
            dir, provider_id, model, messages, conv_id, tx, abort,
        )
        .await;
        return;
    }

    // Gemini 原生协议（非 OpenAI 兼容层）
    if ptype == "gemini_native" {
        crate::gemini::stream_gemini(dir, provider_id, model, messages, tx, abort).await;
        return;
    }

    // 生图分支：手动开关或模型名命中生图模型（image / dall-e / flux / gpt-image …）
    let want_image = image_mode || is_image_model(model);
    if want_image {
        let prompt = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        if let Err(e) = generate_image(dir, provider_id, model, &prompt, &tx, &abort).await {
            let _ = tx.send(ChatEvent::Error(e));
        }
        return;
    }

    if let Err(e) = run(dir, provider_id, model, messages, &tx, &abort).await {
        let _ = tx.send(ChatEvent::Error(e));
    }
}

async fn run(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<(), String> {
    let cfg = load_config(dir)?;
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| format!("未找到 provider: {}", provider_id))?;

    let accounts = resolve_accounts(&provider);
    if accounts.is_empty() {
        return Err("该服务未配置任何账号或 API Key，请先在设置中添加".into());
    }
    let total = accounts.len();

    let client = build_client(&provider)?;
    let url = chat_url(&provider);
    let body = json!({
        "model": model,
        "messages": messages
            .iter()
            .map(|m| json!({
                "role": m.role,
                "content": build_openai_content(dir, m)
            }))
            .collect::<Vec<_>>(),
        "stream": true,
        "temperature": 0.7,
    });

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let secret = acc.secret.trim();
        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if !secret.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", secret));
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let pid = provider.id.clone();
                    let aid = acc.id.clone();
                    let aname = acc.name.clone();
                    let _ = tx.send(ChatEvent::Active {
                        account: aname.clone(),
                    });
                    match stream_response(resp, tx, abort).await {
                        Ok(_) => {
                            touch_account(dir, &pid, &aid);
                            let _ = tx.send(ChatEvent::Done);
                            return Ok(());
                        }
                        Err(e) => {
                            mark_account_error(dir, &pid, &aid, &e);
                            let _ = tx.send(ChatEvent::Error(e.clone()));
                            return Err(e);
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let reason = classify_error(code, &text);
                    mark_account_error(dir, &provider.id, &acc.id, &reason);
                    let _ = tx.send(ChatEvent::Switch {
                        account: acc.name.clone(),
                        reason: reason.clone(),
                    });
                    continue;
                }
            }
            Err(e) => {
                let reason = format!("网络请求失败: {}", e);
                mark_account_error(dir, &provider.id, &acc.id, &reason);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason: reason.clone(),
                });
                continue;
            }
        }
    }

    let _ = tx.send(ChatEvent::Error(format!(
        "所有账号均不可用（已尝试 {} 个），请检查账号池或网络",
        total
    )));
    Err("所有账号均不可用".into())
}

/// 生图分支：POST {base}/images/generations，base64 落盘后以 Markdown 图片推回聊天流。
async fn generate_image(
    dir: &Path,
    provider_id: &str,
    model: &str,
    prompt: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<(), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("生图需要先输入提示词（prompt）".into());
    }
    let cfg = load_config(dir)?;
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| format!("未找到 provider: {}", provider_id))?;
    let accounts = resolve_accounts(&provider);
    if accounts.is_empty() {
        return Err("该服务未配置任何账号或 API Key，请先在设置中添加".into());
    }
    let client = build_client(&provider)?;
    let base = provider.base_url.trim().trim_end_matches('/');
    let url = format!("{}/images/generations", base);
    let body = json!({
        "model": model,
        "prompt": prompt,
        "n": 1,
        "response_format": "b64_json",
    });

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let secret = acc.secret.trim();
        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if !secret.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", secret));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let reason = format!("网络请求失败: {}", e);
                mark_account_error(dir, &provider.id, &acc.id, &reason);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason,
                });
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let reason = classify_error(status.as_u16(), &text);
            mark_account_error(dir, &provider.id, &acc.id, &reason);
            let _ = tx.send(ChatEvent::Switch {
                account: acc.name.clone(),
                reason,
            });
            continue;
        }
        let text = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析生图响应失败: {}（{}）", e, text.chars().take(200).collect::<String>()))?;
        let data = v
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| "生图响应里没有 data 字段".to_string())?;

        touch_account(dir, &provider.id, &acc.id);
        let _ = tx.send(ChatEvent::Active {
            account: acc.name.clone(),
        });

        // 优先 b64_json（落盘本地，隐私且不依赖图床），其次 url
        if let Some(b64) = data.get("b64_json").and_then(|x| x.as_str()) {
            let bytes = base64_decode(b64)?;
            let mime = "image/png";
            let id = files::save_image(dir, mime, &bytes)?;
            let _ = tx.send(ChatEvent::Delta(image_markdown(&format!("/api/images/{}", id), prompt)));
        } else if let Some(u) = data.get("url").and_then(|x| x.as_str()) {
            let _ = tx.send(ChatEvent::Delta(image_markdown(u, prompt)));
        } else {
            return Err("生图响应既没有 b64_json 也没有 url".into());
        }
        let _ = tx.send(ChatEvent::Done);
        return Ok(());
    }
    Err("所有账号均不可用（生图失败），请检查账号池或端点是否支持 /images/generations".into())
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    B64.decode(s.trim()).map_err(|e| format!("base64 解码失败: {}", e))
}

async fn stream_response(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<(), String> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut raw = String::new();
    let mut got_any = false;

    while let Some(chunk) = stream.next().await {
        if is_aborted(abort) {
            return Ok(());
        }
        let chunk = chunk.map_err(|e| format!("读取响应流失败: {}", e))?;
        let text = String::from_utf8_lossy(&chunk);
        buf.push_str(&text);
        raw.push_str(&text);
        while let Some(idx) = buf.find("\n\n") {
            let event = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if let Some(delta) = parse_sse(&event) {
                got_any = true;
                let _ = tx.send(ChatEvent::Delta(delta));
            }
        }
    }

    if !buf.trim().is_empty() {
        if let Some(delta) = parse_sse(&buf) {
            got_any = true;
            let _ = tx.send(ChatEvent::Delta(delta));
        }
    }

    // 兜底：非流式单条 JSON 响应
    if !got_any && !raw.trim().is_empty() {
        if let Some(content) = parse_completion(&raw) {
            let _ = tx.send(ChatEvent::Delta(content));
        }
    }
    Ok(())
}

fn classify_error(code: u16, body: &str) -> String {
    match code {
        401 => "鉴权失败/账号登录已过期".into(),
        403 => "账号被封禁或无权限访问".into(),
        429 => "触发速率限制或额度已用尽".into(),
        400 => format!("请求被拒: {}", body.chars().take(200).collect::<String>()),
        _ => format!("HTTP {}: {}", code, body.chars().take(300).collect::<String>()),
    }
}

fn parse_sse(event: &str) -> Option<String> {
    let mut delta = String::new();
    let mut done = false;
    for line in event.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data == "[DONE]" {
                done = true;
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(d) = extract_content(&v) {
                    delta.push_str(&d);
                }
            }
        }
    }
    if done && delta.is_empty() {
        return None;
    }
    if delta.is_empty() {
        None
    } else {
        Some(delta)
    }
}

fn extract_content(v: &Value) -> Option<String> {
    let choices = v.get("choices")?.as_array()?;
    let first = choices.get(0)?;
    if let Some(d) = first
        .get("delta")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
    {
        return Some(d.to_string());
    }
    if let Some(d) = first
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        return Some(d.to_string());
    }
    if let Some(d) = first.get("text").and_then(|t| t.as_str()) {
        return Some(d.to_string());
    }
    None
}

fn parse_completion(raw: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        return extract_content(&v);
    }
    None
}

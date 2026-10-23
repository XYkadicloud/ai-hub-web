use crate::config::{load_config, mark_account_error, resolve_accounts, touch_account};
use crate::providers::{build_client, chat_url};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::mpsc;

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
}

/// 在后台任务里调用 provider 的 OpenAI 兼容 /chat/completions（stream=true）。
/// 遍历账号池做 failover：某账号失效/被封/额度耗尽时自动切下一个健康账号。
pub async fn stream_chat(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    conv_id: &str,
    tx: mpsc::UnboundedSender<ChatEvent>,
) {
    // 按协议类型分发：网页版（ChatGPT / Claude）走 webchat 模块
    let ptype = load_config(dir)
        .ok()
        .and_then(|c| c.providers.into_iter().find(|p| p.id == provider_id))
        .map(|p| p.ptype)
        .unwrap_or_else(|| "openai".to_string());

    if ptype == "chatgpt_web" || ptype == "claude_web" {
        crate::webchat::stream_web_chat(dir, provider_id, model, messages, conv_id, tx).await;
        return;
    }

    // Gemini 原生协议（非 OpenAI 兼容层）
    if ptype == "gemini_native" {
        crate::gemini::stream_gemini(dir, provider_id, model, messages, tx).await;
        return;
    }

    if let Err(e) = run(dir, provider_id, model, messages, &tx).await {
        let _ = tx.send(ChatEvent::Error(e));
    }
}

async fn run(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    tx: &mpsc::UnboundedSender<ChatEvent>,
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
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect::<Vec<_>>(),
        "stream": true,
        "temperature": 0.7,
    });

    for acc in accounts {
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
                    match stream_response(resp, tx).await {
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

async fn stream_response(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> Result<(), String> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut raw = String::new();
    let mut got_any = false;

    while let Some(chunk) = stream.next().await {
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

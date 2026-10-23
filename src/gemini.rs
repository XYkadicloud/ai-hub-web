//! Gemini 原生协议（Google Generative Language API）。
//!
//! 与 OpenAI 兼容层不同，这里直接对接 Google 官方的原生接口：
//!
//! * 流式：`POST {base}/models/{model}:streamGenerateContent?alt=sse`
//! * 非流式：`POST {base}/models/{model}:generateContent`
//! * 鉴权：`x-goog-api-key: <API_KEY>`（AI Studio 免费 Key 即可）
//! * 请求体：`{ contents: [{role, parts:[{text}]}], systemInstruction, generationConfig }`
//! * 响应：`candidates[0].content.parts[*].text`（`thought: true` 的思维链部分跳过）
//!
//! 相比兼容层，原生协议能保留 systemInstruction、思维链、联网等能力，
//! 且不依赖 Google 的兼容适配层。

use crate::chat::{AbortFlag, ChatEvent, ChatMessage};
use crate::chat_attach::build_gemini_parts;
use crate::config::{load_config, mark_account_error, resolve_accounts, touch_account};
use crate::providers::build_client;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::mpsc;

fn is_aborted(abort: &Option<AbortFlag>) -> bool {
    abort.as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false)
}

/// 默认原生端点（不含 /openai 后缀）。
pub const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// 规范化 base_url：去掉末尾斜杠，兼容用户误填的 `/v1beta/openai`。
pub fn normalize_base(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    b.trim_end_matches("/openai").trim_end_matches('/').to_string()
}

fn model_id(model: &str) -> String {
    model.trim().trim_start_matches("models/").to_string()
}

/// 把内部消息列表转换为 Gemini 的 contents + systemInstruction。
fn build_body(dir: &Path, model: &str, messages: &[ChatMessage]) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system: Vec<String> = Vec::new();

    for m in messages {
        let role = m.role.as_str();
        if role == "system" {
            system.push(m.content.clone());
            continue;
        }
        // Gemini 只认 user / model
        let r = if role == "assistant" { "model" } else { "user" };
        contents.push(json!({
            "role": r,
            "parts": build_gemini_parts(dir, m)
        }));
    }

    // Gemini 要求 contents 非空且以 user 开头
    if contents.is_empty() {
        contents.push(json!({ "role": "user", "parts": [{ "text": "" }] }));
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": { "temperature": 0.7 },
    });

    if !system.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{ "text": system.join("\n\n") }]
        });
    }

    // 记录模型，便于日志排查（服务端以 URL 中的模型为准）
    body["_model_hint"] = json!(model_id(model));
    body
}

// ============================ 流式入口 ============================

pub async fn stream_gemini(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    tx: mpsc::UnboundedSender<ChatEvent>,
    abort: Option<AbortFlag>,
) {
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
    let base = normalize_base(&provider.base_url);
    let mid = model_id(model);
    let url = format!("{}/models/{}:streamGenerateContent?alt=sse", base, mid);
    let mut body = build_body(dir, model, messages);
    body.as_object_mut().map(|o| o.remove("_model_hint"));

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let key = acc.secret.trim().to_string();
        if key.is_empty() {
            let _ = tx.send(ChatEvent::Switch {
                account: acc.name.clone(),
                reason: "账号为空".into(),
            });
            continue;
        }

        let req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", &key)
            .json(&body);

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let _ = tx.send(ChatEvent::Active {
                        account: acc.name.clone(),
                    });
                    match stream_response(resp, tx, abort).await {
                        Ok(_) => {
                            touch_account(dir, provider_id, &acc.id);
                            let _ = tx.send(ChatEvent::Done);
                            return Ok(());
                        }
                        Err(e) => {
                            mark_account_error(dir, provider_id, &acc.id, &e);
                            let _ = tx.send(ChatEvent::Error(e.clone()));
                            return Err(e);
                        }
                    }
                } else {
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let reason = classify_error(code, &text);
                    mark_account_error(dir, provider_id, &acc.id, &reason);
                    let _ = tx.send(ChatEvent::Switch {
                        account: acc.name.clone(),
                        reason: reason.clone(),
                    });
                    continue;
                }
            }
            Err(e) => {
                let reason = format!("网络请求失败: {}", e);
                mark_account_error(dir, provider_id, &acc.id, &reason);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason: reason.clone(),
                });
                continue;
            }
        }
    }

    let _ = tx.send(ChatEvent::Error(format!(
        "所有账号均不可用（已尝试 {} 个），请检查 Key 或网络",
        total
    )));
    Err("所有账号均不可用".into())
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
    let mut block_reason: Option<String> = None;

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
            if let Some(delta) = parse_sse_event(&event, &mut block_reason) {
                got_any = true;
                let _ = tx.send(ChatEvent::Delta(delta));
            }
        }
    }
    if !buf.trim().is_empty() {
        if let Some(delta) = parse_sse_event(&buf, &mut block_reason) {
            got_any = true;
            let _ = tx.send(ChatEvent::Delta(delta));
        }
    }

    // 兜底：某些代理会把 SSE 折叠成单条 JSON（或 JSON 数组）
    if !got_any && !raw.trim().is_empty() {
        if let Some(content) = parse_non_stream(&raw) {
            let _ = tx.send(ChatEvent::Delta(content));
        }
    }

    if let Some(r) = block_reason {
        return Err(format!("内容被安全策略拦截（{}）", r));
    }
    Ok(())
}

/// 解析一条 SSE 事件，返回正文增量。
fn parse_sse_event(event: &str, block_reason: &mut Option<String>) -> Option<String> {
    let mut out = String::new();
    for line in event.lines() {
        let line = line.trim();
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if let Some(reason) = v
                .get("promptFeedback")
                .and_then(|f| f.get("blockReason"))
                .and_then(|r| r.as_str())
            {
                *block_reason = Some(reason.to_string());
            }
            if let Some(t) = extract_text(&v) {
                out.push_str(&t);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 从 candidates[0].content.parts 中取正文（跳过思维链）。
fn extract_text(v: &Value) -> Option<String> {
    let cands = v.get("candidates")?.as_array()?;
    let c = cands.first()?;
    let parts = c.get("content")?.get("parts")?.as_array()?;
    let mut s = String::new();
    for p in parts {
        if p.get("thought").and_then(|t| t.as_bool()).unwrap_or(false) {
            continue; // 思维链不展示
        }
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
            s.push_str(t);
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn parse_non_stream(raw: &str) -> Option<String> {
    let t = raw.trim();
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(t) {
        let mut s = String::new();
        for it in &items {
            if let Some(x) = extract_text(it) {
                s.push_str(&x);
            }
        }
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return extract_text(&v);
    }
    None
}

fn classify_error(code: u16, body: &str) -> String {
    // Google 的错误体：{"error":{"code":400,"message":"API key not valid...","status":"INVALID_ARGUMENT"}}
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(200).collect());

    match code {
        400 if msg.to_lowercase().contains("api key") => {
            format!("API Key 无效或未启用（{}）", msg)
        }
        400 => format!("请求被拒：{}", msg),
        401 | 403 => format!("鉴权失败或无权限：{}", msg),
        404 => format!("模型不存在或该 Key 无权访问：{}", msg),
        429 => format!("额度/速率超限：{}", msg),
        _ => format!("HTTP {}：{}", code, msg),
    }
}

// ============================ 校验与模型列表 ============================

/// 校验 Key 是否有效：拉一次模型列表（最轻量的鉴权探针）。
pub async fn verify_key(base_url: &str, api_key: &str, proxy: Option<&str>) -> Result<Value, String> {
    let base = normalize_base(base_url);
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(p) = proxy {
        let p = p.trim();
        if !p.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?);
        }
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}/models?pageSize=1", base);
    let resp = client
        .get(&url)
        .header("x-goog-api-key", api_key.trim())
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(json!({ "ok": true, "message": "Key 有效，可正常调用 Gemini 原生接口" }))
    } else {
        Err(classify_error(status.as_u16(), &text))
    }
}

/// 拉取该 Key 可用、且支持 generateContent 的模型名列表。
pub async fn list_models(base_url: &str, api_key: &str, proxy: Option<&str>) -> Result<Vec<String>, String> {
    let base = normalize_base(base_url);
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(p) = proxy {
        let p = p.trim();
        if !p.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?);
        }
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}/models?pageSize=200", base);
    let resp = client
        .get(&url)
        .header("x-goog-api-key", api_key.trim())
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(classify_error(status.as_u16(), &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {}", e))?;
    let mut out = Vec::new();
    if let Some(items) = v.get("models").and_then(|m| m.as_array()) {
        for m in items {
            let supports = m
                .get("supportedGenerationMethods")
                .and_then(|s| s.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some("generateContent")))
                .unwrap_or(false);
            if !supports {
                continue;
            }
            if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                out.push(name.trim_start_matches("models/").to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

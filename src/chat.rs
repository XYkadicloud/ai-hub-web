use crate::chat_attach::{
    build_openai_content, image_markdown, is_image_model, Attachment,
};
use crate::config::{load_config, mark_account_error, resolve_accounts, touch_account};
use crate::files;
use crate::providers::{build_client, chat_urls};
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
    /// 一段增量「思考过程」（DeepSeek R1 / Qwen / GLM 等推理模型的 reasoning_content）
    Reasoning(String),
    /// 账号异常，已切换到下一个（携带账号名与原因）
    Switch { account: String, reason: String },
    /// 当前生效账号（成功建立连接时）
    Active { account: String },
    /// Agent 开始调用某个工具（携带工具名与参数）
    ToolCall { name: String, args: Value },
    /// Agent 的工具调用结束（携带一行摘要）
    ToolResult { name: String, ok: bool, summary: String },
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
/// `agent_mode`：开启 Agent 能力（工具调用 + 联网）。**只对 API 协议生效**——
/// ChatGPT 网页版刻意不接，网页端逆向叠加高频工具循环几乎必然封号。
#[allow(clippy::too_many_arguments)]
pub async fn stream_chat(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    conv_id: &str,
    tx: mpsc::UnboundedSender<ChatEvent>,
    image_mode: bool,
    agent_mode: bool,
    abort: Option<AbortFlag>,
) {
    // 按协议类型分发：ChatGPT 网页版走 webchat 模块，其余走 API 协议
    let ptype = load_config(dir)
        .ok()
        .and_then(|c| c.providers.into_iter().find(|p| p.id == provider_id))
        .map(|p| p.ptype)
        .unwrap_or_else(|| "openai".to_string());

    if ptype == "chatgpt_web" {
        // 网页版不支持 Agent：即便用户开了开关也按普通对话走，避免封号
        crate::webchat::stream_web_chat(
            dir, provider_id, model, messages, conv_id, tx, abort,
        )
        .await;
        return;
    }

    // Agent 分支：让模型自己决定调工具（搜索 / 抓网页 / 计算 / 时间）
    if agent_mode && !is_image_model(model) {
        let opts = crate::agent::AgentOpts::default();
        let r = if ptype == "gemini_native" {
            crate::agent::run_gemini_agent(dir, provider_id, model, messages, &tx, &abort, &opts)
                .await
        } else {
            crate::agent::run_openai_agent(dir, provider_id, model, messages, &tx, &abort, &opts)
                .await
        };
        if let Err(e) = r {
            let _ = tx.send(ChatEvent::Error(e));
        }
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
    // base_url 的写法各家不一（带不带 /v1、末尾有没有斜杠、有没有写全），
    // 这里给出一组候选，遇到 404/405 就换下一个，用户不用纠结格式。
    let urls = chat_urls(&provider);
    if urls.is_empty() {
        return Err("该服务的 Base URL 为空，请在设置里填写".into());
    }
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
    });

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let secret = acc.secret.trim();
        let mut last_reason = String::from("未知错误");

        for (idx, url) in urls.iter().enumerate() {
            let mut req = client
                .post(url)
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
                        let _ = tx.send(ChatEvent::Active {
                            account: acc.name.clone(),
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
                    }
                    let code = status.as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    // 端点路径不对（常见于漏写 /v1 或写多了 /v1）：换下一个候选
                    if (code == 404 || code == 405) && idx + 1 < urls.len() {
                        last_reason = format!("{} 不存在（HTTP {}）", url, code);
                        continue;
                    }
                    last_reason = classify_error(code, &text);
                    break;
                }
                Err(e) => {
                    last_reason = format!("网络请求失败: {}", e);
                    break;
                }
            }
        }

        mark_account_error(dir, &provider.id, &acc.id, &last_reason);
        let _ = tx.send(ChatEvent::Switch {
            account: acc.name.clone(),
            reason: last_reason,
        });
    }

    Err(format!("{} 个账号均不可用，请检查 Key / 余额 / 网络", total))
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
        // 有些网关/反代用 CRLF 分帧，统一成 LF 再按空行切事件
        let text = String::from_utf8_lossy(&chunk).replace("\r\n", "\n");
        buf.push_str(&text);
        raw.push_str(&text);
        while let Some(idx) = buf.find("\n\n") {
            let event = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if handle_event(&event, tx) {
                got_any = true;
            }
        }
    }

    if !buf.trim().is_empty() && handle_event(&buf, tx) {
        got_any = true;
    }

    // 兜底：非流式单条 JSON 响应（部分兼容端点忽略 stream=true）
    if !got_any && !raw.trim().is_empty() {
        if let Some(content) = parse_completion(&raw) {
            let _ = tx.send(ChatEvent::Delta(content));
        }
    }
    Ok(())
}

/// 解析一个 SSE 事件块，把正文与思考过程分别推给前端。
/// 返回是否产出了内容。
fn handle_event(event: &str, tx: &mpsc::UnboundedSender<ChatEvent>) -> bool {
    let mut got = false;
    for line in event.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // 有的端点先回 200，再在流里塞一个 error 对象
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_else(|| "服务端返回错误");
            let _ = tx.send(ChatEvent::Error(format!("服务端返回错误: {}", msg)));
            return got;
        }
        if let Some(r) = extract_reasoning(&v) {
            if !r.is_empty() {
                let _ = tx.send(ChatEvent::Reasoning(r));
                got = true;
            }
        }
        if let Some(c) = extract_content(&v) {
            if !c.is_empty() {
                let _ = tx.send(ChatEvent::Delta(c));
                got = true;
            }
        }
    }
    got
}

pub fn classify_error(code: u16, body: &str) -> String {
    // 尽量把服务端给的原因原样带出来，比只报状态码有用得多
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    match code {
        400 => format!("请求被拒（参数或模型名不对）：{}", detail),
        401 => format!("鉴权失败，API Key 无效或已过期：{}", detail),
        402 => format!("余额不足：{}", detail),
        403 => format!("无权限访问该模型/接口：{}", detail),
        404 => format!("接口或模型不存在：{}", detail),
        422 => format!("参数校验失败：{}", detail),
        429 => format!("触发限流或额度已用尽：{}", detail),
        _ => format!("HTTP {}：{}", code, detail),
    }
}

fn extract_reasoning(v: &Value) -> Option<String> {
    let first = v.get("choices")?.as_array()?.get(0)?;
    for key in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(r) = first.get("delta").and_then(|d| d.get(key)).and_then(|c| c.as_str()) {
            return Some(r.to_string());
        }
        if let Some(r) = first.get("message").and_then(|d| d.get(key)).and_then(|c| c.as_str()) {
            return Some(r.to_string());
        }
    }
    None
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

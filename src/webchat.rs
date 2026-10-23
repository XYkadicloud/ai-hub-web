//! 网页版（逆向）协议客户端。
//!
//! 与 OpenAI 兼容接口不同，ChatGPT / Claude 的网页版并不暴露标准 API，本模块按社区
//! 长期验证的逆向流程实现：
//!
//! * ChatGPT：`accessToken` + Cookie → `sentinel/chat-requirements` → 求解 PoW
//!   (SHA3-512) → `backend-api/conversation` → SSE 增量解析。
//! * Claude：Cookie `sessionKey` → `/api/account` 取组织 UUID → 建会话 →
//!   `completion` → SSE 增量解析，并用 `parent_message_uuid` 维持多轮。
//!
//! 说明：这类接口随时可能被官方调整或加强风控而失效，属预期风险；账号池的
//! 自动切换逻辑可缓解单账号失效，但无法对抗协议级变更。

use crate::chat::{AbortFlag, ChatEvent, ChatMessage};
use crate::chat_attach::build_web_text;
use crate::config::{load_config, mark_account_error, resolve_accounts, touch_account, update_account};
use crate::history;
use crate::oauth;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, COOKIE,
    ORIGIN, REFERER, USER_AGENT,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha3::{Digest, Sha3_512};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

fn is_aborted(abort: &Option<AbortFlag>) -> bool {
    abort.as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const CHATGPT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";
const CLAUDE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const ANTHROPIC_CLIENT_VERSION: &str = "2025-04-01";
/// chatgpt.com 前端构建号，仅参与 PoW 载荷构造（服务端不校验其内容）。
const DEFAULT_DPL: &str = "prod-f501fe933b3edf57aea882da888e1a544df99840";

static UUID_SEQ: AtomicU64 = AtomicU64::new(0);

/// 会话级状态（按会话持久化到 Conversation.meta）。
#[derive(Serialize, Deserialize, Default, Clone)]
struct WebMeta {
    #[serde(default)]
    chatgpt_conversation_id: Option<String>,
    #[serde(default)]
    chatgpt_parent_id: Option<String>,
    #[serde(default)]
    claude_org_uuid: Option<String>,
    #[serde(default)]
    claude_conv_uuid: Option<String>,
    #[serde(default)]
    claude_parent_uuid: Option<String>,
}

// ============================ 公共入口 ============================

/// 网页版流式聊天入口：遍历账号池做 failover。
pub async fn stream_web_chat(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    conv_id: &str,
    tx: mpsc::UnboundedSender<ChatEvent>,
    abort: Option<AbortFlag>,
) {
    if let Err(e) = run_web(dir, provider_id, model, messages, conv_id, &tx, &abort).await {
        let _ = tx.send(ChatEvent::Error(e));
    }
}

async fn run_web(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    conv_id: &str,
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
        return Err("该服务未配置任何账号或 Token，请先在设置中添加".into());
    }
    let total = accounts.len();

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    let base_meta = load_meta(dir, conv_id);
    let cur_meta = base_meta;

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let mut sec = acc.secret.trim().to_string();
        if sec.is_empty() {
            mark_account_error(dir, provider_id, &acc.id, "账号为空");
            let _ = tx.send(ChatEvent::Switch {
                account: acc.name.clone(),
                reason: "账号为空".into(),
            });
            continue;
        }

        // ChatGPT OAuth 账号自动续期：带 refresh_token 且 access_token 临期时先刷新
        if provider.ptype == "chatgpt_web" {
            let rt = acc
                .meta
                .as_ref()
                .and_then(|m| m.get("refresh_token"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(rt) = rt {
                let exp = oauth::jwt_exp(&sec).unwrap_or(0);
                if exp - now_secs() < 300 {
                    match oauth::refresh_token(&client, &rt).await {
                        Ok(ts) => {
                            let mut meta = acc.meta.clone().unwrap_or(Value::Object(Default::default()));
                            if let Some(obj) = meta.as_object_mut() {
                                if let Some(rt2) = &ts.refresh_token {
                                    obj.insert("refresh_token".into(), json!(rt2));
                                }
                                if let Some(idt) = &ts.id_token {
                                    if let Some(c) = oauth::parse_jwt_claims(idt) {
                                        if let Some(e) = c.email {
                                            obj.insert("email".into(), json!(e));
                                        }
                                        if let Some(a) = c.chatgpt_account_id {
                                            obj.insert("chatgpt_account_id".into(), json!(a));
                                        }
                                    }
                                }
                            }
                            update_account(
                                dir,
                                provider_id,
                                &acc.id,
                                Some(ts.access_token.clone()),
                                Some(meta),
                            );
                            sec = ts.access_token;
                        }
                        Err(e) => {
                            mark_account_error(dir, provider_id, &acc.id, &e);
                            let _ = tx.send(ChatEvent::Switch {
                                account: acc.name.clone(),
                                reason: format!("Token 续期失败: {}", e),
                            });
                            continue;
                        }
                    }
                }
            }
        }

        let _ = tx.send(ChatEvent::Active {
            account: acc.name.clone(),
        });
        let attempt = cur_meta.clone();

        let res = match provider.ptype.as_str() {
            "chatgpt_web" => {
                let account_id = acc
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("chatgpt_account_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                chatgpt_send(
                    &client,
                    dir,
                    &provider.base_url,
                    &sec,
                    acc.cookie.as_deref(),
                    account_id.as_deref(),
                    model,
                    messages,
                    attempt,
                    tx,
                    abort,
                )
                .await
            }
            "claude_web" => {
                claude_send(
                    &client,
                    &provider.base_url,
                    &sec,
                    acc.cookie.as_deref(),
                    model,
                    messages,
                    attempt,
                    tx,
                    abort,
                )
                .await
            }
            other => Err(format!("未知网页协议类型: {}", other)),
        };

        match res {
            Ok(new_meta) => {
                save_meta(dir, conv_id, &new_meta);
                touch_account(dir, provider_id, &acc.id);
                let _ = tx.send(ChatEvent::Done);
                return Ok(());
            }
            Err(e) => {
                mark_account_error(dir, provider_id, &acc.id, &e);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason: e.clone(),
                });
                continue;
            }
        }
    }

    let _ = tx.send(ChatEvent::Error(format!(
        "所有账号均不可用（已尝试 {} 个）",
        total
    )));
    Err("所有账号均不可用".into())
}

fn load_meta(dir: &Path, conv_id: &str) -> WebMeta {
    let v = history::get_conversation_meta(dir, conv_id);
    serde_json::from_value(v).unwrap_or_default()
}
fn save_meta(dir: &Path, conv_id: &str, m: &WebMeta) {
    if let Ok(v) = serde_json::to_value(m) {
        let _ = history::set_conversation_meta(dir, conv_id, v);
    }
}

// ============================ ChatGPT 网页版 ============================

/// Codex 官方接口（`/backend-api/codex/responses`）兜底。
///
/// OAuth（Codex 客户端）签发的 access_token 天生就是给这个接口用的，
/// 走网页版协议偶尔会被 sentinel 拒；这里用 Responses API 的格式再试一次。
/// 只有确实拿到文本增量才算成功，否则返回 Err 让调用方继续报原错误。
#[allow(clippy::too_many_arguments)]
async fn chatgpt_codex_send(
    client: &Client,
    host: &str,
    token: &str,
    account_id: &str,
    model: &str,
    dir: &Path,
    messages: &[ChatMessage],
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<(), String> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, hv(&format!("Bearer {}", token))?);
    headers.insert(CONTENT_TYPE, hv("application/json")?);
    headers.insert(USER_AGENT, hv(CHATGPT_UA)?);
    headers.insert(ACCEPT, hv("text/event-stream")?);
    headers.insert(ORIGIN, hv("https://chatgpt.com")?);
    headers.insert(REFERER, hv("https://chatgpt.com/")?);
    headers.insert(ACCEPT_LANGUAGE, hv("en-US,en;q=0.9")?);
    for (k, v) in [
        ("chatgpt-account-id", account_id),
        ("OpenAI-Beta", "responses=experimental"),
        ("originator", "codex_cli_rs"),
        ("oai-language", "en-US"),
    ] {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), hv(v)) {
            headers.insert(name, val);
        }
    }
    let device = device_id_for(account_id);
    if let (Ok(name), Ok(val)) = (
        HeaderName::from_bytes(b"session_id"),
        hv(device.as_str()),
    ) {
        headers.insert(name, val);
    }

    let input: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "type": "message",
                "role": m.role,
                "content": [{
                    "type": if m.role == "assistant" { "output_text" } else { "input_text" },
                    "text": build_web_text(dir, m),
                }]
            })
        })
        .collect();
    let body = json!({
        "model": model,
        "instructions": "You are ChatGPT, a helpful assistant.",
        "input": input,
        "stream": true,
        "store": false,
        "parallel_tool_calls": false,
    });

    let resp = client
        .post(format!("{}/backend-api/codex/responses", host))
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("连接 Codex 接口失败: {}", e))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Codex 接口 HTTP {}: {}",
            code,
            text.chars().take(160).collect::<String>()
        ));
    }

    let mut buf = String::new();
    let mut got = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if is_aborted(abort) {
            return if got { Ok(()) } else { Err("已停止".into()) };
        }
        let chunk = chunk.map_err(|e| format!("读取响应流失败: {}", e))?;
        buf.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
        while let Some(idx) = buf.find("\n\n") {
            let ev = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if codex_event(&ev, tx) {
                got = true;
            }
        }
    }
    if !buf.trim().is_empty() && codex_event(&buf, tx) {
        got = true;
    }
    if got {
        Ok(())
    } else {
        Err("Codex 接口未返回文本".into())
    }
}

/// 解析 Responses API 的 SSE 事件，只关心文本增量。
fn codex_event(ev: &str, tx: &mpsc::UnboundedSender<ChatEvent>) -> bool {
    let mut got = false;
    for line in ev.lines() {
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
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    if !d.is_empty() {
                        let _ = tx.send(ChatEvent::Delta(d.to_string()));
                        got = true;
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    if !d.is_empty() {
                        let _ = tx.send(ChatEvent::Reasoning(d.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    got
}

async fn chatgpt_send(
    client: &Client,
    dir: &Path,
    base_url: &str,
    token: &str,
    cookie: Option<&str>,
    account_id: Option<&str>,
    model: &str,
    messages: &[ChatMessage],
    mut meta: WebMeta,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<WebMeta, String> {
    let host = base_url.trim().trim_end_matches('/');
    let host = if host.is_empty() {
        "https://chatgpt.com"
    } else {
        host
    };

    // OAuth 拿到的 access_token 带组织信息，backend-api 要求显式声明用哪个
    // 账号，否则 sentinel 接口会直接 500（FastAPI 里取不到 header 抛异常）。
    // 优先用账号 meta 里存的，其次从 JWT 的 auth claim 里读。
    let account_id = account_id
        .map(|s| s.to_string())
        .or_else(|| {
            oauth::parse_jwt_claims(token)
                .and_then(|c| c.chatgpt_account_id)
        })
        .filter(|s| !s.trim().is_empty());

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, hv(&format!("Bearer {}", token))?);
    headers.insert(CONTENT_TYPE, hv("application/json")?);
    headers.insert(USER_AGENT, hv(CHATGPT_UA)?);
    headers.insert(ACCEPT, hv("*/*")?);
    // 补齐网页端会带的头，缺了容易被 sentinel 判定为异常客户端
    headers.insert(ORIGIN, hv("https://chatgpt.com")?);
    headers.insert(REFERER, hv("https://chatgpt.com/")?);
    headers.insert(ACCEPT_LANGUAGE, hv("en-US,en;q=0.9")?);
    let device_id = device_id_for(account_id.as_deref().unwrap_or(token));
    for (k, v) in [
        ("oai-device-id", device_id.as_str()),
        ("oai-language", "en-US"),
        ("sec-ch-ua", "\"Chromium\";v=\"142\", \"Not:A-Brand\";v=\"24\""),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"Windows\""),
        ("sec-fetch-dest", "empty"),
        ("sec-fetch-mode", "cors"),
        ("sec-fetch-site", "same-origin"),
    ] {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), hv(v)) {
            headers.insert(name, val);
        }
    }
    if let Some(aid) = &account_id {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(b"chatgpt-account-id"),
            hv(aid.as_str()),
        ) {
            headers.insert(name, val);
        }
    }
    if let Some(c) = cookie {
        if !c.trim().is_empty() {
            headers.insert(COOKIE, hv(c)?);
        }
    }

    // 1) 申请聊天凭证 + PoW 挑战
    // 实测（2026-10）：这个接口对「长 p 载荷」会直接 500，而首次申请凭证
    // 本来就不需要有效 PoW —— 传空 p 一样能拿到 token。真正需要 PoW 时，
    // 服务端会在响应里给 proofofwork 挑战，下面再解，所以这里不再预生成。
    let resp = client
        .post(format!("{}/backend-api/sentinel/chat-requirements", host))
        .headers(headers.clone())
        .json(&json!({ "p": "" }))
        .send()
        .await
        .map_err(|e| format!("连接 ChatGPT 失败: {}", e))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let e = http_err("ChatGPT chat-requirements", resp).await;
        // OAuth 拿到的 token 在网页版协议里偶尔会被拒（sentinel 500/401）。
        // 这类 token 的「原生」用法是 Codex 的 responses 接口，试一次兜底。
        if code == 500 || code == 401 || code == 403 {
            if let Some(aid) = &account_id {
                let first = chatgpt_codex_send(
                    client, host, token, aid, model, dir, messages, tx, abort,
                )
                .await;
                let ok = match first {
                    Ok(()) => true,
                    // 端点不认识这个模型名（网页版模型和 Codex 模型不是一套），
                    // 换 gpt-5 再试一次；只有「请求没被接受」时才重试，避免重复输出
                    Err(e) if e.contains("HTTP 4") && model != "gpt-5" => {
                        chatgpt_codex_send(
                            client, host, token, aid, "gpt-5", dir, messages, tx, abort,
                        )
                        .await
                        .is_ok()
                    }
                    Err(_) => false,
                };
                if ok {
                    return Ok(meta);
                }
            }
        }
        let hint = if code == 500 && account_id.is_none() {
            "（没取到账号 ID：OAuth token 里没有组织信息，请重新登录该账号）"
        } else {
            ""
        };
        return Err(format!("{}{}", e, hint));
    }
    let j: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 chat-requirements 失败: {}", e))?;
    let chat_token = j
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "未返回 chat token（账号可能失效或需人机验证）".to_string())?
        .to_string();

    let mut pow_seed_debug = String::new();
    let proof = if j
        .pointer("/proofofwork/required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let seed = j
            .pointer("/proofofwork/seed")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let diff = j
            .pointer("/proofofwork/difficulty")
            .and_then(|v| v.as_str())
            .unwrap_or("000032");
        pow_seed_debug = seed.to_string();
        if seed.is_empty() {
            None
        } else {
            Some(format!("gAAAAAB{}", solve_pow(seed, diff, DEFAULT_DPL)))
        }
    } else {
        None
    };

    // 2) 组装消息体（附件降级为文字说明）
    let chat_messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "id": new_uuid(),
                "author": { "role": m.role },
                "content": { "content_type": "text", "parts": [build_web_text(dir, m)] },
                "metadata": {}
            })
        })
        .collect();

    let parent = meta.chatgpt_parent_id.clone().unwrap_or_else(new_uuid);
    let mut payload = json!({
        "action": "next",
        "messages": chat_messages,
        "parent_message_id": parent,
        "model": model,
        "history_and_training_disabled": false,
        "conversation_mode": { "kind": "primary_assistant" },
        "force_paragen": false,
        "force_rate_limit": false,
        "suggestions": [],
        "supported_encodings": [],
        "client_contextual_info": {
            "is_dark_mode": true,
            "time_since_loaded": 1200,
            "page_height": 900,
            "page_width": 1400,
            "pixel_ratio": 2,
            "screen_height": 1080,
            "screen_width": 1920
        }
    });
    if let Some(cid) = meta.chatgpt_conversation_id.clone() {
        payload["conversation_id"] = json!(cid);
    }

    let mut ch = headers.clone();
    ch.insert(ACCEPT, hv("text/event-stream")?);
    ch.insert(
        HeaderName::from_bytes(b"openai-sentinel-chat-requirements-token").map_err(|e| e.to_string())?,
        hv(&chat_token)?,
    );
    if let Some(pf) = &proof {
        ch.insert(
            HeaderName::from_bytes(b"openai-sentinel-proof-token").map_err(|e| e.to_string())?,
            hv(pf)?,
        );
        if std::env::var("AI_HUB_DEBUG_POW").is_ok() {
            let _ = std::fs::write(
                std::env::temp_dir().join("ai-hub-proof.txt"),
                format!("seed={}\nproof={}", pow_seed_debug, pf),
            );
        }
    }

    let resp = client
        .post(format!("{}/backend-api/conversation", host))
        .headers(ch)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("发送对话失败: {}", e))?;
    if !resp.status().is_success() {
        if std::env::var("AI_HUB_DEBUG_POW").is_ok() {
            let st = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            eprintln!(
                "[pow-debug] conv={} proof_len={} chat_token_len={} model={} -> {} {}",
                st,
                proof.as_deref().map(|s| s.len()).unwrap_or(0),
                chat_token.len(),
                model,
                st,
                &body[..body.len().min(300)]
            );
            return Err(format!("ChatGPT conversation HTTP {}: {}", st, body));
        }
        return Err(http_err("ChatGPT conversation", resp).await);
    }

    // 3) SSE 增量解析
    let mut emitted = String::new();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if is_aborted(abort) {
            return if emitted.is_empty() {
                Err("已停止".into())
            } else {
                Ok(meta)
            };
        }
        let chunk = chunk.map_err(|e| format!("读取响应流失败: {}", e))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find("\n\n") {
            let ev = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            handle_chatgpt_event(&ev, &mut emitted, &mut meta, tx);
        }
    }
    if !buf.trim().is_empty() {
        handle_chatgpt_event(&buf, &mut emitted, &mut meta, tx);
    }

    if emitted.is_empty() {
        return Err("ChatGPT 未返回内容（可能被风控拦截，请重试或更换账号）".into());
    }
    Ok(meta)
}

fn handle_chatgpt_event(
    ev: &str,
    emitted: &mut String,
    meta: &mut WebMeta,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    for line in ev.lines() {
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
        if let Some(cid) = v.get("conversation_id").and_then(|x| x.as_str()) {
            meta.chatgpt_conversation_id = Some(cid.to_string());
        }
        if let Some(msg) = v.get("message") {
            let role = msg
                .pointer("/author/role")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if let Some(mid) = msg.get("id").and_then(|x| x.as_str()) {
                if role == "assistant" || role.is_empty() {
                    meta.chatgpt_parent_id = Some(mid.to_string());
                }
            }
            if let Some(parts) = msg.pointer("/content/parts").and_then(|x| x.as_array()) {
                let text: String = parts.iter().filter_map(|p| p.as_str()).collect();
                if text.len() > emitted.len() && text.starts_with(emitted.as_str()) {
                    let delta = text[emitted.len()..].to_string();
                    emitted.push_str(&delta);
                    let _ = tx.send(ChatEvent::Delta(delta));
                }
            }
        }
    }
}

// ============================ Claude 网页版 ============================

/// 组装 Claude 的 Cookie：优先用用户粘贴的完整 Cookie（含 cf_clearance 等
/// Cloudflare 校验项），否则退回只带 sessionKey。
fn claude_cookie(session_key: &str, cookie_extra: Option<&str>) -> String {
    let sk = session_key.trim();
    match cookie_extra {
        Some(c) if c.trim().len() > 24 => {
            let c = c.trim();
            if c.contains("sessionKey=") {
                c.to_string()
            } else {
                format!("{}; sessionKey={}", c, sk)
            }
        }
        _ => format!("sessionKey={}", sk),
    }
}

/// Claude 网页端请求头。尽量还原真实浏览器特征——Cloudflare / 风控会看这些头，
/// 只有 UA + Cookie 很容易被判定为非浏览器客户端而直接 403。
fn claude_headers(cookie: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, hv("application/json")?);
    headers.insert(USER_AGENT, hv(CLAUDE_UA)?);
    headers.insert(COOKIE, hv(cookie)?);
    headers.insert(
        HeaderName::from_bytes(b"anthropic-client-version").map_err(|e| e.to_string())?,
        hv(ANTHROPIC_CLIENT_VERSION)?,
    );
    headers.insert(ACCEPT_LANGUAGE, hv("en-US,en;q=0.9")?);
    for (k, v) in [
        ("anthropic-client-platform", "web_claude_ai"),
        ("anthropic-device-id", "d7c1f0a4-6f3b-4b2e-9c8a-2f5e1b7d9a30"),
        ("sec-ch-ua", "\"Chromium\";v=\"142\", \"Not:A-Brand\";v=\"24\""),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"Windows\""),
        ("sec-fetch-dest", "empty"),
        ("sec-fetch-mode", "cors"),
        ("sec-fetch-site", "same-origin"),
        ("priority", "u=1, i"),
    ] {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), hv(v)) {
            headers.insert(name, val);
        }
    }
    Ok(headers)
}

async fn claude_send(
    client: &Client,
    base_url: &str,
    session_key: &str,
    cookie_extra: Option<&str>,
    model: &str,
    messages: &[ChatMessage],
    mut meta: WebMeta,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> Result<WebMeta, String> {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://claude.ai/api"
    } else {
        base
    };

    // 只给 sessionKey 时风控通过率较低；用户若粘贴了完整 Cookie（含
    // cf_clearance 等），则整串带上，Cloudflare 校验会宽松很多。
    let cookie = claude_cookie(session_key, cookie_extra);
    let mut headers = claude_headers(&cookie)?;
    headers.insert(ACCEPT, hv("text/event-stream")?);
    headers.insert(ORIGIN, hv("https://claude.ai")?);

    // 1) 组织 UUID
    let org = if let Some(o) = meta.claude_org_uuid.clone() {
        o
    } else {
        let resp = client
            .get(format!("{}/account", base))
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| format!("连接 Claude 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(http_err("Claude /account", resp).await);
        }
        let j: Value = resp
            .json()
            .await
            .map_err(|e| format!("解析 account 失败: {}", e))?;
        let o = j
            .pointer("/memberships/0/organization/uuid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "未能获取组织 UUID（sessionKey 可能无效）".to_string())?
            .to_string();
        meta.claude_org_uuid = Some(o.clone());
        o
    };

    // 2) 会话 UUID
    let conv_uuid = if let Some(c) = meta.claude_conv_uuid.clone() {
        c
    } else {
        let body = json!({
            "name": "",
            "include_conversation_preferences": true,
            "is_temporary": false
        });
        let resp = client
            .post(format!("{}/organizations/{}/chat_conversations", base, org))
            .headers(headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("创建 Claude 会话失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(http_err("Claude 创建会话", resp).await);
        }
        let j: Value = resp
            .json()
            .await
            .map_err(|e| format!("解析会话失败: {}", e))?;
        let c = j
            .get("uuid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "未返回会话 uuid".to_string())?
            .to_string();
        meta.claude_conv_uuid = Some(c.clone());
        c
    };

    // 3) 组装 completion 请求体（附件降级拼进 prompt）
    let last = messages.last();
    let prompt = last
        .map(|m| crate::chat_attach::build_web_text(Path::new(""), m))
        .unwrap_or_default();
    let is_follow_up = meta.claude_parent_uuid.is_some();
    let mut body = json!({
        "prompt": prompt,
        "timezone": "Asia/Shanghai",
        "personalized_styles": [{
            "type": "default",
            "key": "Default",
            "name": "Normal",
            "nameKey": "normal_style_name",
            "prompt": "Normal\n",
            "summary": "Default responses from Claude",
            "summaryKey": "normal_style_summary",
            "isDefault": true
        }],
        "locale": "en-US",
        "model": model,
        "tools": [
            { "type": "web_search_v0", "name": "web_search" },
            { "type": "artifacts_v0", "name": "artifacts" },
            { "type": "repl_v0", "name": "repl" }
        ],
        "attachments": [],
        "files": [],
        "sync_sources": [],
        "rendering_mode": "messages"
    });
    if !is_follow_up {
        body["create_conversation_params"] = json!({
            "name": "",
            "include_conversation_preferences": true,
            "is_temporary": false
        });
    }
    if let Some(p) = meta.claude_parent_uuid.clone() {
        body["parent_message_uuid"] = json!(p);
    }
    // 注意：不要再塞 `turn_message_uuid` 之类的自定义字段——Claude 会直接回
    // 400 invalid_request_error: "turn_message_uuid: Extra inputs are not permitted"。

    // Referer 必须指向具体会话，风控会校验来源页面
    let mut send_headers = headers.clone();
    if let Ok(v) = hv(&format!("https://claude.ai/chat/{}", conv_uuid)) {
        send_headers.insert(REFERER, v);
    }

    let resp = client
        .post(format!(
            "{}/organizations/{}/chat_conversations/{}/completion",
            base, org, conv_uuid
        ))
        .headers(send_headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("发送 Claude 消息失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(http_err("Claude completion", resp).await);
    }

    // 4) SSE 增量解析
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    let mut got = false;
    while let Some(chunk) = stream.next().await {
        if is_aborted(abort) {
            return if got { Ok(meta) } else { Err("已停止".into()) };
        }
        let chunk = chunk.map_err(|e| format!("读取响应流失败: {}", e))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find("\n\n") {
            let ev = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if handle_claude_event(&ev, &mut meta, tx) {
                got = true;
            }
        }
    }
    if !buf.trim().is_empty() && handle_claude_event(&buf, &mut meta, tx) {
        got = true;
    }

    if !got {
        return Err("Claude 未返回内容".into());
    }
    Ok(meta)
}

fn handle_claude_event(
    ev: &str,
    meta: &mut WebMeta,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> bool {
    let mut got = false;
    for line in ev.lines() {
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
        let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match kind {
            "content_block_delta" => {
                let text = v
                    .pointer("/delta/text")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.get("delta").and_then(|d| d.as_str()))
                    .or_else(|| v.get("text").and_then(|t| t.as_str()));
                if let Some(t) = text {
                    if !t.is_empty() {
                        let _ = tx.send(ChatEvent::Delta(t.to_string()));
                        got = true;
                    }
                }
            }
            "message_start" => {
                let uuid = v
                    .pointer("/message/uuid")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.pointer("/message/id").and_then(|x| x.as_str()));
                if let Some(u) = uuid {
                    meta.claude_parent_uuid = Some(u.to_string());
                }
            }
            _ => {
                if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() {
                        let _ = tx.send(ChatEvent::Delta(t.to_string()));
                        got = true;
                    }
                }
            }
        }
    }
    got
}

// ============================ PoW 求解 ============================

/// ChatGPT 的 PoW：对 `SHA3-512(seed || base64(config_json))` 求前 N 字节小于难度目标。
/// 返回 base64(config_json)；调用方负责加 `gAAAAAB` / `gAAAAAC` 前缀。
fn solve_pow(seed: &str, diff_hex: &str, dpl: &str) -> String {
    let target = hex::decode(diff_hex).ok().filter(|t| !t.is_empty());
    let target = match target {
        Some(t) => t,
        None => hex::decode("000032").unwrap(),
    };
    let diff_len = target.len();
    let seed_enc = seed.as_bytes();
    let (p1, p2, p3) = build_pow_config(dpl);
    let p1 = p1.into_bytes();
    let p2 = p2.into_bytes();
    let p3 = p3.into_bytes();

    // 预分配缓冲区，内循环零堆分配（PoW 可能要跑几十万次）
    let mut buf: Vec<u8> = Vec::with_capacity(p1.len() + p2.len() + p3.len() + 24);
    let mut b64out = vec![0u8; 4096];

    for i in 0u64..2_000_000u64 {
        buf.clear();
        buf.extend_from_slice(&p1);
        push_u64(&mut buf, i);
        buf.push(b',');
        push_u64(&mut buf, i >> 1);
        buf.push(b',');
        buf.extend_from_slice(&p2);
        buf.extend_from_slice(&p3);

        let n = match B64.encode_slice(&buf, &mut b64out) {
            Ok(n) => n,
            Err(_) => break,
        };
        let b64 = &b64out[..n];

        let mut h = Sha3_512::new();
        h.update(seed_enc);
        h.update(b64);
        let digest = h.finalize();
        if &digest[..diff_len] <= &target[..] {
            return String::from_utf8_lossy(b64).into_owned();
        }
    }
    // 兜底：保证不 panic（服务端会拒绝，交由 failover 处理）
    let fallback = format!("\"{}\"", seed);
    format!(
        "wQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D{}",
        B64.encode(fallback.as_bytes())
    )
}

fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    for j in (0..i).rev() {
        buf.push(tmp[j]);
    }
}

/// chat-requirements 首包所需的 `p` 参数（低难度占位证明）。
/// 由账号标识派生一个稳定的设备 ID（UUID 形状）。
/// sentinel 会把它当设备指纹，同一账号必须保持不变，所以不能每次随机。
fn device_id_for(seed: &str) -> String {
    // FNV-1a 64 位，两轮不同盐，凑出 32 个十六进制字符
    let mut out = [0u8; 16];
    for (i, salt) in [0x9e37u64, 0x85ebu64].iter().enumerate() {
        let mut h: u64 = 0xcbf29ce484222325 ^ salt;
        for b in seed.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        out[i * 8..i * 8 + 8].copy_from_slice(&h.to_be_bytes());
    }
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    let h: Vec<String> = out.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13],
        h[14], h[15]
    )
}

/// 生成 sentinel 的 `p` 载荷（长 PoW 证明）。
/// 注意：chat-requirements 目前对长载荷会返回 500，故首次申请不再使用它；
/// 保留实现供 `proofofwork` 阶段与后续格式变化复用。
#[allow(dead_code)]
fn requirements_token(dpl: &str) -> String {
    let seed = format!("0.{}", now_millis().unsigned_abs() % 1_000_000_000);
    format!("gAAAAAC{}", solve_pow(&seed, "0fffff", dpl))
}

/// 按 chat2api 的 18 元素 config 结构拼出三段静态 JSON（i 与 i>>1 由调用方插入）。
fn build_pow_config(dpl: &str) -> (String, String, String) {
    let time_str = format!(
        "{} GMT-0500 (Eastern Standard Time)",
        (Utc::now() - chrono::Duration::hours(5))
            .format("%a %b %d %Y %H:%M:%S")
            .to_string()
    );
    let script = format!(
        "https://cdn.oaistatic.com/_next/static/cXh69klOLzS0Gy2joLDRS/_ssgManifest.js?dpl={}",
        dpl
    );
    let nav = "registerProtocolHandler\u{2212}function registerProtocolHandler() { [native code] }";
    let doc = "__NEXT_DATA__";
    let win = "__next_set_public_path__";
    let perf = now_millis() as f64 + 12.5;
    let uuid = new_uuid();
    let cores = 8u32;
    let timediff = now_millis() as f64 - 3.5;

    // config[0..3] -> "[res,time,4294705152,"
    let part1 = format!("[{},{},4294705152,", 3000u32, json_str(&time_str));
    // config[5..9] -> "ua,script,dpl,\"en-US\",\"en-US,es-US,en,es\","
    // （config[3]=i 与 config[4]=i>>1 由 solve_pow 插在 part1 与 part2 之间）
    let part2 = format!(
        "{},{},{},{},{},",
        json_str(CHATGPT_UA),
        json_str(&script),
        json_str(dpl),
        json_str("en-US"),
        json_str("en-US,es-US,en,es")
    );
    // config[10..] -> "nav,doc,win,perf,uuid,\"\",cores,timediff]"
    // 注意结尾的 `]`：漏掉会让整段 JSON 不合法，服务端直接回 500。
    let part3 = format!(
        "{},{},{},{},{},{},{},{}]",
        json_str(nav),
        json_str(doc),
        json_str(win),
        num_json(perf),
        json_str(&uuid),
        json_str(""),
        cores,
        num_json(timediff)
    );
    (part1, part2, part3)
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}
fn num_json(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成 UUIDv4 形式的唯一串（不依赖外部熵源，跨平台稳定）。
fn new_uuid() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = UUID_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut x = (now.as_nanos() as u64) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut hex = String::with_capacity(32);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        hex.push_str(&format!("{:016x}", x));
    }
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ============================ 账号验证（设置页用） ============================

/// 校验 ChatGPT accessToken（+可选 Cookie）是否有效，返回用户信息。
pub async fn verify_chatgpt(
    token: &str,
    cookie: Option<&str>,
    base_url: &str,
) -> Result<Value, String> {
    let host = base_url.trim().trim_end_matches('/');
    let host = if host.is_empty() {
        "https://chatgpt.com"
    } else {
        host
    };
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, hv(&format!("Bearer {}", token.trim()))?);
    headers.insert(USER_AGENT, hv(CHATGPT_UA)?);
    headers.insert(ACCEPT, hv("application/json")?);
    if let Some(c) = cookie {
        if !c.trim().is_empty() {
            headers.insert(COOKIE, hv(c)?);
        }
    }
    let resp = client
        .get(format!("{}/api/auth/session", host))
        .headers(headers)
        .send()
        .await
        .map_err(|e| format!("连接 ChatGPT 失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(http_err("ChatGPT 校验", resp).await);
    }
    let j: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 session 失败: {}", e))?;
    if j.get("accessToken").and_then(|v| v.as_str()).is_none()
        && j.get("user").is_none()
    {
        return Err("Token 无效或已过期（未返回用户信息）".into());
    }
    Ok(json!({
        "ok": true,
        "email": j.pointer("/user/email").and_then(|v| v.as_str()),
        "name": j.pointer("/user/name").and_then(|v| v.as_str()),
        "user_id": j.pointer("/user/id").and_then(|v| v.as_str()),
    }))
}

/// 校验 Claude sessionKey 是否有效，返回组织信息。
pub async fn verify_claude(
    session_key: &str,
    cookie_extra: Option<&str>,
    base_url: &str,
) -> Result<Value, String> {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://claude.ai/api"
    } else {
        base
    };
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let cookie = claude_cookie(session_key, cookie_extra);
    let headers = claude_headers(&cookie)?;
    let resp = client
        .get(format!("{}/account", base))
        .headers(headers)
        .send()
        .await
        .map_err(|e| format!("连接 Claude 失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(http_err("Claude 校验", resp).await);
    }
    let j: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 account 失败: {}", e))?;
    let org = j
        .pointer("/memberships/0/organization/uuid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "sessionKey 无效（未返回组织信息）".to_string())?;
    Ok(json!({
        "ok": true,
        "org_uuid": org,
        "email": j.pointer("/email_address").and_then(|v| v.as_str()),
        "name": j.pointer("/full_name").and_then(|v| v.as_str()),
    }))
}

// ============================ 工具 ============================

fn hv(s: &str) -> Result<HeaderValue, String> {
    HeaderValue::from_str(s).map_err(|e| e.to_string())
}

async fn http_err(stage: &str, resp: reqwest::Response) -> String {
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let short: String = text.chars().take(300).collect();
    match code {
        401 => format!("{} 鉴权失败/登录已过期（请重新抓取 Token）", stage),
        403 => {
            let low = text.to_lowercase();
            let is_cf = low.contains("cloudflare")
                || low.contains("<!doctype html")
                || low.contains("just a moment")
                || low.contains("attention required");
            let banned = low.contains("banned")
                || low.contains("blocked")
                || low.contains("violat")
                || low.contains("suspended");
            if banned {
                format!(
                    "{} 账号被风控/封禁，换一个账号再试：{}",
                    stage, short
                )
            } else if is_cf {
                format!(
                    "{} 被 Cloudflare 拦截：请粘贴**完整 Cookie**（含 cf_clearance）而不是只给 sessionKey：{}",
                    stage, short
                )
            } else {
                format!("{} 被风控拦截或无权限：{}", stage, short)
            }
        }
        429 => format!(
            "{} 触发限流或额度用尽（网页版免费额度耗尽也会这样，换账号或稍后再试）",
            stage
        ),
        _ => format!("{} HTTP {}: {}", stage, code, short),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_and_uuid_shaped() {
        let a = device_id_for("acc-123");
        let b = device_id_for("acc-123");
        let c = device_id_for("acc-456");
        assert_eq!(a, b, "同一账号必须稳定，否则 sentinel 会认为换设备");
        assert_ne!(a, c);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    }

    #[test]
    fn pow_solves_and_verifies() {
        let seed = "0.4231";
        for diff in ["0fffff", "00ffff", "000032"] {
            let t = std::time::Instant::now();
            let b64 = solve_pow(seed, diff, DEFAULT_DPL);
            let elapsed = t.elapsed();
            let target = hex::decode(diff).unwrap();
            let mut h = Sha3_512::new();
            h.update(seed.as_bytes());
            h.update(b64.as_bytes());
            let d = h.finalize();
            let ok = &d[..target.len()] <= &target[..];
            println!("diff={} ok={} elapsed={:?}", diff, ok, elapsed);
            assert!(ok, "diff {} 未求解成功", diff);
        }
    }

    /// 回归测试：PoW 载荷必须是合法 JSON（曾经漏了结尾 `]`，
    /// 服务端解析失败后一律回 500 "Hmm...something seems to have gone wrong."）。
    #[test]
    fn pow_payload_is_valid_json() {
        let b64 = solve_pow("0.5", "000032", DEFAULT_DPL);
        let raw = B64.decode(b64.as_bytes()).expect("proof 不是合法 base64");
        let v: Value = serde_json::from_slice(&raw).expect("PoW 载荷不是合法 JSON");
        let arr = v.as_array().expect("PoW 载荷应为数组");
        assert_eq!(arr.len(), 18, "PoW config 应为 18 个元素");
        assert!(arr[3].is_u64(), "config[3] 应是自增计数 i");
        assert!(arr[4].is_u64(), "config[4] 应是 i>>1");
        assert!(arr[5].as_str().unwrap_or("").contains("Mozilla"), "config[5] 应是 UA");
        assert!(
            arr[10].as_str().unwrap_or("").contains("registerProtocolHandler"),
            "config[10] 应是 nav 指纹"
        );
    }
}

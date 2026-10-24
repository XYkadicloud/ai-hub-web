//! Agent 能力：让模型自己决定调用工具（联网搜索 / 抓网页 / 取时间）。
//!
//! **设计红线：只走 API 通道，绝不碰 ChatGPT 网页端。**
//! 网页端逆向（PoW / sentinel）本身就有封号风险，再叠加工具循环的高频请求，
//! 几乎必然触发风控。因此本模块只服务 `openai` 与 `gemini_native` 两种协议。
//!
//! 循环结构（对齐 ChatGPT 网页端的 Agent 体验）：
//!   1. 带 tools 定义发起请求；
//!   2. 模型返回 tool_calls → 推送「正在调用 xx」事件 → 本地执行 → 结果回填；
//!   3. 继续下一轮，直到模型给出最终文本或达到步数上限。
//!
//! 每一步都通过 `ChatEvent` 推到前端，前端以时间线呈现，和官方客户端的
//! 「正在搜索网页…」一致。

use crate::chat::{AbortFlag, ChatEvent, ChatMessage, is_aborted};
use crate::chat_attach::build_openai_content;
use crate::config::{load_config, resolve_accounts};
use crate::providers::{build_client, chat_urls};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::mpsc;

/// Agent 运行参数。
#[derive(Debug, Clone)]
pub struct AgentOpts {
    /// 最大工具调用轮数（防止模型无限调用）。
    pub max_steps: usize,
    /// 是否允许联网工具（用户可在界面关掉）。
    pub allow_web: bool,
}

impl Default for AgentOpts {
    fn default() -> Self {
        Self {
            max_steps: 6,
            allow_web: true,
        }
    }
}

/// OpenAI 兼容协议的 tools 定义（JSON Schema）。
pub fn openai_tools(allow_web: bool) -> Value {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "get_current_time",
                "description": "获取当前日期与时间。用户问'现在几点''今天几号'或需要时间上下文时调用。",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "calculate",
                "description": "计算一个数学表达式，支持 + - * / % ^ 与括号、小数。需要精确算术时调用，不要自己口算。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "expression": { "type": "string", "description": "如 (12+3)*4.5" }
                    },
                    "required": ["expression"]
                }
            }
        }),
    ];
    if allow_web {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "联网搜索。需要最新信息、时事、价格、文档、或任何你不确定的事实时调用。返回标题、链接与摘要列表。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索关键词" }
                    },
                    "required": ["query"]
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "fetch_url",
                "description": "抓取某个网页的正文内容。当搜索结果里某个链接看起来最相关时调用它读取详细内容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "完整 URL，需以 http(s):// 开头" }
                    },
                    "required": ["url"]
                }
            }
        }));
    }
    json!(tools)
}

/// Gemini 原生协议的 functionDeclarations。
pub fn gemini_tools(allow_web: bool) -> Value {
    let mut decls = vec![
        json!({
            "name": "get_current_time",
            "description": "获取当前日期与时间。用户问'现在几点''今天几号'或需要时间上下文时调用。",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "calculate",
            "description": "计算数学表达式，支持 + - * / % ^ 与括号、小数。需要精确算术时调用。",
            "parameters": {
                "type": "object",
                "properties": {
                    "expression": { "type": "string", "description": "如 (12+3)*4.5" }
                },
                "required": ["expression"]
            }
        }),
    ];
    if allow_web {
        decls.push(json!({
            "name": "web_search",
            "description": "联网搜索。需要最新信息、时事、价格、文档、或任何你不确定的事实时调用。",
            "parameters": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }
        }));
        decls.push(json!({
            "name": "fetch_url",
            "description": "抓取某个网页的正文内容。搜索结果里最相关的链接用这个读取详情。",
            "parameters": {
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }
        }));
    }
    json!([{ "functionDeclarations": decls }])
}

/// 本地执行一个工具，返回给模型看的文本结果。
pub async fn run_tool(name: &str, args: &Value) -> String {
    match name {
        "get_current_time" => crate::agent_tools::current_time_text(),
        "calculate" => {
            let expr = args.get("expression").and_then(|v| v.as_str()).unwrap_or("");
            crate::agent_tools::calculate(expr)
        }
        "web_search" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            crate::agent_tools::web_search(q).await
        }
        "fetch_url" => {
            let u = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            crate::agent_tools::fetch_url(u).await
        }
        other => format!("未知工具：{}", other),
    }
}

/// OpenAI 兼容协议的 Agent 主循环。
///
/// 流式请求：边收文本边推 Delta，同时按 index 拼装 tool_calls。
/// 出现 tool_calls 就执行工具、回填消息、进入下一轮；否则正常收尾。
pub async fn run_openai_agent(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
    opts: &AgentOpts,
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
    let client = build_client(&provider)?;
    let urls = chat_urls(&provider);
    if urls.is_empty() {
        return Err("该服务的 Base URL 为空，请在设置里填写".into());
    }
    let tools = openai_tools(opts.allow_web);

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let secret = acc.secret.trim();
        let mut history: Vec<Value> = messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": build_openai_content(dir, m) }))
            .collect();

        let _ = tx.send(ChatEvent::Active {
            account: acc.name.clone(),
        });

        for _step in 0..opts.max_steps {
            if is_aborted(abort) {
                return Ok(());
            }
            let body = json!({
                "model": model,
                "messages": history,
                "stream": true,
                "tools": tools,
            });

            let mut resp = None;
            let mut last_err = String::from("请求失败");
            for (ui, url) in urls.iter().enumerate() {
                let mut req = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .json(&body);
                if !secret.is_empty() {
                    req = req.header("Authorization", format!("Bearer {}", secret));
                }
                match req.send().await {
                    Ok(r) => {
                        if r.status().is_success() {
                            resp = Some(r);
                            break;
                        }
                        let code = r.status().as_u16();
                        if (code == 404 || code == 405) && ui + 1 < urls.len() {
                            last_err = format!("{} 不存在（HTTP {}）", url, code);
                            continue;
                        }
                        let text = r.text().await.unwrap_or_default();
                        last_err = crate::chat::classify_error(code, &text);
                        break;
                    }
                    Err(e) => {
                        last_err = format!("网络请求失败: {}", e);
                        break;
                    }
                }
            }
            let resp = match resp {
                Some(r) => r,
                None => {
                    crate::config::mark_account_error(dir, &provider.id, &acc.id, &last_err);
                    let _ = tx.send(ChatEvent::Switch {
                        account: acc.name.clone(),
                        reason: last_err,
                    });
                    break;
                }
            };

            let outcome = pump_openai_agent(resp, tx, abort).await;
            match outcome {
                AgentRound::Text => {
                    crate::config::touch_account(dir, &provider.id, &acc.id);
                    let _ = tx.send(ChatEvent::Done);
                    return Ok(());
                }
                AgentRound::Calls(hist) => {
                    history.extend(hist);
                }
                AgentRound::Failed(e) => {
                    crate::config::mark_account_error(dir, &provider.id, &acc.id, &e);
                    let _ = tx.send(ChatEvent::Switch {
                        account: acc.name.clone(),
                        reason: e,
                    });
                    break;
                }
                AgentRound::Aborted => return Ok(()),
            }
        }

        let _ = tx.send(ChatEvent::Delta(
            "\n\n_（已达到工具调用上限，以上是已获取到的信息）_".into(),
        ));
    }
    Err("所有账号均不可用".into())
}

/// 一轮请求的结果。
enum AgentRound {
    /// 模型给了最终文本，结束
    Text,
    /// 模型要调工具，附带需要追加进 history 的消息
    Calls(Vec<Value>),
    /// 失败（携带原因）
    Failed(String),
    /// 用户中止
    Aborted,
}

/// 消费一轮 OpenAI 流式响应：推文本、收集 tool_calls、执行工具。
async fn pump_openai_agent(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
) -> AgentRound {
    let mut buf = String::new();
    let mut text_out = String::new();
    let mut calls: Vec<(usize, String, String, String)> = Vec::new();
    let mut stream = resp.bytes_stream();
    let mut stream_err: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        if is_aborted(abort) {
            return AgentRound::Aborted;
        }
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                stream_err = Some(format!("读取响应流失败: {}", e));
                break;
            }
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        let normalized = buf.replace("\r\n", "\n");
        buf = normalized;
        while let Some(i) = buf.find("\n\n") {
            let ev = buf[..i].to_string();
            buf = buf[i + 2..].to_string();
            if let Some(e) = handle_openai_agent_event(&ev, tx, &mut text_out, &mut calls) {
                stream_err = Some(e);
            }
        }
    }
    if !buf.trim().is_empty() {
        if let Some(e) = handle_openai_agent_event(&buf, tx, &mut text_out, &mut calls) {
            stream_err = Some(e);
        }
    }
    if let Some(e) = stream_err {
        return AgentRound::Failed(e);
    }
    if calls.is_empty() {
        return AgentRound::Text;
    }

    // 执行工具并组装回填消息
    let mut extra: Vec<Value> = Vec::new();
    extra.push(json!({
        "role": "assistant",
        "content": if text_out.is_empty() { Value::Null } else { json!(text_out) },
        "tool_calls": calls.iter().map(|(_, id, name, args)| json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": args }
        })).collect::<Vec<_>>()
    }));
    for (_, id, name, args) in &calls {
        if is_aborted(abort) {
            return AgentRound::Aborted;
        }
        let parsed: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
        let _ = tx.send(ChatEvent::ToolCall {
            name: name.clone(),
            args: parsed.clone(),
        });
        let result = run_tool(name, &parsed).await;
        let ok = !result.starts_with("抓取失败")
            && !result.starts_with("搜索失败")
            && !result.starts_with("无法计算")
            && !result.starts_with("URL 必须")
            && !result.starts_with("表达式")
            && !result.starts_with("搜索词为空");
        let _ = tx.send(ChatEvent::ToolResult {
            name: name.clone(),
            ok,
            summary: crate::agent_tools::brief(name, &result),
        });
        extra.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": result
        }));
    }
    AgentRound::Calls(extra)
}

/// 处理一条 SSE 事件：推文本增量，累积 tool_calls。返回 Some(错误) 表示流内出错。
fn handle_openai_agent_event(
    ev: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    text_out: &mut String,
    calls: &mut Vec<(usize, String, String, String)>,
) -> Option<String> {
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
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("流内错误")
                .to_string();
            return Some(msg);
        }
        let Some(ch) = v.pointer("/choices/0/delta") else {
            continue;
        };
        if let Some(rc) = ch.get("reasoning_content").and_then(|x| x.as_str()) {
            if !rc.is_empty() {
                let _ = tx.send(ChatEvent::Reasoning(rc.to_string()));
            }
        }
        if let Some(c) = ch.get("content").and_then(|x| x.as_str()) {
            if !c.is_empty() {
                text_out.push_str(c);
                let _ = tx.send(ChatEvent::Delta(c.to_string()));
            }
        }
        if let Some(arr) = ch.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in arr {
                let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let slot = match calls.iter().position(|(i, ..)| *i == idx) {
                    Some(p) => &mut calls[p],
                    None => {
                        calls.push((idx, String::new(), String::new(), String::new()));
                        calls.last_mut().unwrap()
                    }
                };
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() && slot.1.is_empty() {
                        slot.1 = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                        if !n.is_empty() {
                            slot.2.push_str(n);
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                        slot.3.push_str(a);
                    }
                }
            }
        }
    }
    None
}

// ============================ Gemini 原生协议 ============================

/// Gemini 原生协议的 Agent 主循环。
/// Gemini 用 functionDeclarations 声明工具，模型返回 functionCall，
/// 我们把结果以 functionResponse 回填。
pub async fn run_gemini_agent(
    dir: &Path,
    provider_id: &str,
    model: &str,
    messages: &[ChatMessage],
    tx: &mpsc::UnboundedSender<ChatEvent>,
    abort: &Option<AbortFlag>,
    opts: &AgentOpts,
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
    let client = build_client(&provider)?;
    let base = crate::gemini::normalize_base(&provider.base_url);
    let tools = gemini_tools(opts.allow_web);

    for acc in accounts {
        if is_aborted(abort) {
            return Ok(());
        }
        let secret = acc.secret.trim();
        let mut contents: Vec<Value> = messages
            .iter()
            .map(|m| {
                json!({
                    "role": if m.role == "assistant" { "model" } else { "user" },
                    "parts": [{ "text": m.content }]
                })
            })
            .collect();

        let _ = tx.send(ChatEvent::Active {
            account: acc.name.clone(),
        });

        for _step in 0..opts.max_steps {
            if is_aborted(abort) {
                return Ok(());
            }
            let url = format!(
                "{}/models/{}:streamGenerateContent?alt=sse&key={}",
                base, model, secret
            );
            let body = json!({ "contents": contents, "tools": tools });
            let resp = match client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("网络请求失败: {}", e);
                    let _ = tx.send(ChatEvent::Switch {
                        account: acc.name.clone(),
                        reason: msg,
                    });
                    break;
                }
            };
            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                let msg = crate::chat::classify_error(code, &text);
                crate::config::mark_account_error(dir, &provider.id, &acc.id, &msg);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason: msg,
                });
                break;
            }

            let mut buf = String::new();
            let mut text_out = String::new();
            let mut calls: Vec<(String, Value)> = Vec::new();
            let mut stream = resp.bytes_stream();
            let mut stream_err: Option<String> = None;
            while let Some(chunk) = stream.next().await {
                if is_aborted(abort) {
                    return Ok(());
                }
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        stream_err = Some(format!("读取响应流失败: {}", e));
                        break;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                buf = buf.replace("\r\n", "\n");
                while let Some(i) = buf.find("\n\n") {
                    let ev = buf[..i].to_string();
                    buf = buf[i + 2..].to_string();
                    for line in ev.lines() {
                        let Some(d) = line.trim().strip_prefix("data:") else {
                            continue;
                        };
                        let Ok(v) = serde_json::from_str::<Value>(d.trim()) else {
                            continue;
                        };
                        if let Some(err) = v.get("error") {
                            let m = err
                                .get("message")
                                .and_then(|x| x.as_str())
                                .unwrap_or("流内错误");
                            stream_err = Some(m.to_string());
                            continue;
                        }
                        if let Some(arr) =
                            v.pointer("/candidates/0/content/parts").and_then(|x| x.as_array())
                        {
                            for p in arr {
                                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                    if !t.is_empty() {
                                        text_out.push_str(t);
                                        let _ = tx.send(ChatEvent::Delta(t.to_string()));
                                    }
                                }
                                if let Some(fc) = p.get("functionCall") {
                                    let name = fc
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                                    calls.push((name, args));
                                }
                            }
                        }
                    }
                }
            }

            if let Some(e) = stream_err {
                crate::config::mark_account_error(dir, &provider.id, &acc.id, &e);
                let _ = tx.send(ChatEvent::Switch {
                    account: acc.name.clone(),
                    reason: e,
                });
                break;
            }

            if calls.is_empty() {
                crate::config::touch_account(dir, &provider.id, &acc.id);
                let _ = tx.send(ChatEvent::Done);
                return Ok(());
            }

            // 回填：模型这次的 functionCall + 我们的 functionResponse
            let model_parts: Vec<Value> = calls
                .iter()
                .map(|(n, a)| json!({ "functionCall": { "name": n, "args": a } }))
                .collect();
            if !text_out.is_empty() {
                let mut mp = vec![json!({ "text": text_out })];
                mp.extend(model_parts);
                contents.push(json!({ "role": "model", "parts": mp }));
            } else {
                contents.push(json!({ "role": "model", "parts": model_parts }));
            }

            let mut resp_parts: Vec<Value> = Vec::new();
            for (name, args) in &calls {
                if is_aborted(abort) {
                    return Ok(());
                }
                let _ = tx.send(ChatEvent::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                });
                let result = run_tool(name, args).await;
                let ok = !result.starts_with("抓取失败")
                    && !result.starts_with("搜索失败")
                    && !result.starts_with("无法计算");
                let _ = tx.send(ChatEvent::ToolResult {
                    name: name.clone(),
                    ok,
                    summary: crate::agent_tools::brief(name, &result),
                });
                resp_parts.push(json!({
                    "functionResponse": { "name": name, "response": { "result": result } }
                }));
            }
            contents.push(json!({ "role": "user", "parts": resp_parts }));
        }

        let _ = tx.send(ChatEvent::Delta(
            "\n\n_（已达到工具调用上限，以上是已获取到的信息）_".into(),
        ));
    }
    Err("所有账号均不可用".into())
}

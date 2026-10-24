use crate::chat::{self, AbortFlag, ChatEvent, ChatMessage};
use crate::chat_attach::Attachment;
use crate::config::{self, Account, AppConfig};
use crate::files;
use crate::history::{self, Conversation};
use crate::oauth::{self, PendingOAuth};
use crate::webchat;
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Notify};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

/// 前端静态资源内嵌进二进制（单文件交付）。
#[derive(RustEmbed)]
#[folder = "web/"]
struct WebAssets;

/// 通过 SSE 推给前端的事件信封。
#[derive(Clone)]
pub struct ServerEvent {
    pub event: String,
    pub payload: Value,
}

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub tx: broadcast::Sender<ServerEvent>,
    pub shutdown: Arc<Notify>,
    /// 指定后优先从磁盘读前端（`--web-dir`），便于改 UI 不必重新编译。
    pub web_dir: Option<PathBuf>,
    /// conv_id → 流式中止标志（前端点「停止」时置位）。
    pub aborts: Arc<Mutex<HashMap<String, AbortFlag>>>,
    /// session_id → 待完成的 ChatGPT OAuth 授权会话（带创建时间，10 分钟过期）。
    pub oauth_pending: Arc<Mutex<HashMap<String, (PendingOAuth, i64)>>>,
    /// 当前活跃的 SSE 连接数（页面开着就有 ≥1）。
    pub sse_clients: Arc<std::sync::atomic::AtomicUsize>,
    /// 是否曾经有页面连过 SSE——用于区分「刚启动还没人打开」和「页面被关掉/强杀」。
    pub saw_client: Arc<std::sync::atomic::AtomicBool>,
    /// 最近一次 HTTP 请求时间（Unix 秒），同上，兜底退出判断用。
    pub last_activity: Arc<std::sync::atomic::AtomicI64>,
    /// 退出死线（Unix 秒）：页面关闭（pagehide）时置位；任何新请求都会取消它
    /// （这样「刷新页面」不会误杀服务，只有页面真的关了才退出）。
    pub quit_deadline: Arc<Mutex<Option<i64>>>,
    /// 实际监听端口（绑定后回填）。OAuth 的 loopback 回调地址要用它。
    pub port: Arc<std::sync::atomic::AtomicU16>,
}

/// 页面关闭后等待多久才允许退出（秒）。前端每 5s 一次心跳，
/// 只要还有任何一个标签页开着，心跳就会在死线前把退出取消掉。
pub const QUIT_DELAY_SECS: i64 = 8;
/// 兜底退出：页面曾打开过，之后既无 SSE 也无任何请求持续这么久 → 判定页面被强杀。
pub const ORPHAN_TIMEOUT_SECS: i64 = 600;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn build_router(state: AppState) -> Router {
    let quit_deadline = state.quit_deadline.clone();
    let last_activity = state.last_activity.clone();
    Router::new()
        .route("/", get(index))
        .route("/api/config", get(get_config).post(post_config))
        .route(
            "/api/accounts",
            get(get_accounts).post(add_account).delete(del_account),
        )
        .route("/api/accounts/status", post(set_status))
        .route(
            "/api/capture-login-token",
            post(capture_token).options(capture_preflight),
        )
        .route("/api/verify-account", post(verify_account))
        .route("/api/models", post(list_models))
        .route("/api/version", get(version))
        .route("/api/log", get(log_tail))
        .route("/api/chat", post(chat))
        .route("/api/chat/stop", post(chat_stop))
        .route("/api/upload", post(upload))
        .route("/api/files/:id", get(serve_file))
        .route("/api/images/:id", get(serve_image))
        .route("/api/oauth/start", post(oauth_start))
        .route("/api/oauth/finish", post(oauth_finish))
        .route("/auth/callback", get(oauth_callback))
        .route("/api/conversations", get(list_convs).post(create_conv))
        .route(
            "/api/conversations/:id",
            get(get_conv).delete(del_conv),
        )
        .route("/api/conversations/:id/rename", post(rename_conv))
        .route("/api/conversations/:id/messages", post(save_msg))
        .route("/api/events", get(events))
        .route("/api/quit", post(quit))
        .route("/api/ping", get(ping).post(ping))
        .route("/api/debug-exit", get(debug_exit))
        .route("/*path", get(static_handler))
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024))
        .layer(middleware::from_fn(move |req, next| {
            cors_layer(quit_deadline.clone(), last_activity.clone(), req, next)
        }))
        .with_state(state)
}

/// 全局 CORS：本地服务 + 浏览器书签脚本跨域回传需要。
/// 只监听 127.0.0.1，且所有接口本就面向本机单用户，放开来源没有额外风险。
/// 顺带两件事：① 每个请求都取消「待退出」状态——刷新页面时新页面会立刻
/// 发请求，因此不会把服务误杀；② 刷新「最近活动时间」供兜底退出判断。
async fn cors_layer(
    quit_deadline: Arc<Mutex<Option<i64>>>,
    last_activity: Arc<std::sync::atomic::AtomicI64>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Ok(mut d) = quit_deadline.lock() {
        *d = None;
    }
    last_activity.store(now_secs(), std::sync::atomic::Ordering::SeqCst);
    let is_preflight = req.method() == axum::http::Method::OPTIONS;
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "*".parse().unwrap(),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, DELETE, OPTIONS".parse().unwrap(),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Content-Type, Authorization, X-File-Name, X-File-Type".parse().unwrap(),
    );
    if is_preflight {
        *resp.status_mut() = StatusCode::NO_CONTENT;
    }
    resp
}

/// 书签脚本 preflight 兜底（被 cors_layer 覆盖，这里显式声明意图）。
async fn capture_preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------- 错误类型 ----------
struct AppErr(String);
impl From<String> for AppErr {
    fn from(s: String) -> Self {
        AppErr(s)
    }
}
impl IntoResponse for AppErr {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

// ---------- 配置 ----------
async fn get_config(State(s): State<AppState>) -> Result<Json<AppConfig>, AppErr> {
    Ok(Json(config::load_config(&s.data_dir).map_err(AppErr::from)?))
}
async fn post_config(
    State(s): State<AppState>,
    Json(cfg): Json<AppConfig>,
) -> Result<StatusCode, AppErr> {
    config::save_config(&s.data_dir, &cfg).map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}

// ---------- 账号池 ----------
#[derive(Deserialize)]
struct ProviderIdQuery {
    provider_id: String,
}
async fn get_accounts(
    State(s): State<AppState>,
    Query(q): Query<ProviderIdQuery>,
) -> Result<Json<Vec<Account>>, AppErr> {
    let cfg = config::load_config(&s.data_dir).map_err(AppErr::from)?;
    let p = cfg
        .providers
        .iter()
        .find(|p| p.id == q.provider_id)
        .ok_or_else(|| AppErr("未找到 provider".into()))?;
    Ok(Json(p.accounts.clone()))
}
#[derive(Deserialize)]
struct AddAccountBody {
    provider_id: String,
    account: Account,
}
async fn add_account(
    State(s): State<AppState>,
    Json(b): Json<AddAccountBody>,
) -> Result<Json<Account>, AppErr> {
    Ok(Json(
        config::add_account(&s.data_dir, &b.provider_id, b.account).map_err(AppErr::from)?,
    ))
}
#[derive(Deserialize)]
struct DelAccountQuery {
    provider_id: String,
    account_id: String,
}
async fn del_account(
    State(s): State<AppState>,
    Query(q): Query<DelAccountQuery>,
) -> Result<StatusCode, AppErr> {
    config::delete_account(&s.data_dir, &q.provider_id, &q.account_id).map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}
#[derive(Deserialize)]
struct StatusBody {
    provider_id: String,
    account_id: String,
    status: String,
}
async fn set_status(
    State(s): State<AppState>,
    Json(b): Json<StatusBody>,
) -> Result<StatusCode, AppErr> {
    config::set_account_status(&s.data_dir, &b.provider_id, &b.account_id, &b.status)
        .map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}
#[derive(Deserialize)]
struct CaptureBody {
    provider_id: String,
    secret: String,
    name: Option<String>,
    cookie: Option<String>,
    meta: Option<Value>,
}
async fn capture_token(
    State(s): State<AppState>,
    Json(b): Json<CaptureBody>,
) -> Result<Json<Account>, AppErr> {
    let secret = b.secret.trim().to_string();
    if secret.is_empty() {
        return Err(AppErr("未提供 token".into()));
    }
    let acc = Account {
        id: String::new(),
        name: b.name.unwrap_or_else(|| "网页登录账号".into()),
        cred_type: "token".into(),
        secret,
        cookie: b.cookie.filter(|c| !c.trim().is_empty()),
        meta: b.meta,
        status: "active".into(),
        last_error: None,
        last_used: None,
        priority: 0,
    };
    let saved =
        config::add_account(&s.data_dir, &b.provider_id, acc).map_err(AppErr::from)?;
    let _ = s.tx.send(ServerEvent {
        event: "login-captured".into(),
        payload: json!({ "provider": b.provider_id, "account": saved.name }),
    });
    Ok(Json(saved))
}

#[derive(Deserialize)]
struct VerifyBody {
    provider_id: String,
    secret: String,
    cookie: Option<String>,
}
async fn verify_account(
    State(s): State<AppState>,
    Json(b): Json<VerifyBody>,
) -> Result<Json<Value>, AppErr> {
    let cfg = config::load_config(&s.data_dir).map_err(AppErr::from)?;
    let p = cfg
        .providers
        .iter()
        .find(|p| p.id == b.provider_id)
        .ok_or_else(|| AppErr("未找到 provider".into()))?;
    let res = match p.ptype.as_str() {
        "chatgpt_web" => {
            webchat::verify_chatgpt(&b.secret, b.cookie.as_deref(), &p.base_url).await
        }
        "gemini_native" => {
            crate::gemini::verify_key(&p.base_url, &b.secret, p.proxy.as_deref()).await
        }
        _ => {
            // OpenAI 兼容端点：用 /models 探针校验 Key
            verify_openai_key(&p, &b.secret).await
        }
    };
    Ok(Json(match res {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e }),
    }))
}

/// 构建信息：版本号 + 构建时刻（前端用于核对是否为最新 exe）。
async fn version(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_ts": env!("BUILD_TS").parse::<u64>().unwrap_or(0),
        "mode": if cfg!(debug_assertions) { "debug" } else { "release" },
        "data_dir": s.data_dir.display().to_string(),
    }))
}

/// 最近一段启动日志（排障用）：只读 data_dir/startup.log 末尾 8KB。
async fn log_tail(State(s): State<AppState>) -> Result<Json<Value>, AppErr> {
    let p = s.data_dir.join("startup.log");
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let tail: String = text
        .chars()
        .rev()
        .take(8192)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(Json(json!({ "log": tail })))
}

/// OpenAI 兼容端点的 Key 探针：GET {base}/models。
/// 部分反代不支持该接口（404），此时不判失败，只提示「无法校验」。
async fn verify_openai_key(
    p: &config::Provider,
    secret: &str,
) -> Result<Value, String> {
    let base = p.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("未填写 Base URL".into());
    }
    let client = crate::providers::build_client(p)?;
    let resp = client
        .get(format!("{}/models", base))
        .header("Authorization", format!("Bearer {}", secret.trim()))
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(json!({ "ok": true, "message": "Key 有效（端点 /models 可达）" }))
    } else if status.as_u16() == 404 {
        Ok(json!({
            "ok": true,
            "message": "端点不支持 /models 探针，无法预校验；已保存，首次对话时再验证",
            "unverified": true
        }))
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        Err(format!(
            "Key 无效或无权限（HTTP {}）：{}",
            status.as_u16(),
            text.chars().take(200).collect::<String>()
        ))
    } else {
        Err(format!(
            "校验失败（HTTP {}）：{}",
            status.as_u16(),
            text.chars().take(200).collect::<String>()
        ))
    }
}

#[derive(Deserialize)]
struct ModelsBody {
    provider_id: String,
    secret: Option<String>,
}
/// 拉取该 provider 可用的模型列表（用于设置页「刷新模型」）。
async fn list_models(
    State(s): State<AppState>,
    Json(b): Json<ModelsBody>,
) -> Result<Json<Value>, AppErr> {
    let cfg = config::load_config(&s.data_dir).map_err(AppErr::from)?;
    let p = cfg
        .providers
        .iter()
        .find(|p| p.id == b.provider_id)
        .ok_or_else(|| AppErr("未找到 provider".into()))?;

    // 优先用传入的 Key（刚粘贴、尚未保存），否则取账号池第一个
    let key = b
        .secret
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| config::resolve_accounts(p).first().map(|a| a.secret.clone()))
        .ok_or_else(|| AppErr("没有可用的 Key，请先填写".into()))?;

    let res: Result<Vec<String>, String> = if p.ptype == "gemini_native" {
        crate::gemini::list_models(&p.base_url, &key, p.proxy.as_deref()).await
    } else if p.ptype == "chatgpt_web" {
        return Ok(Json(json!({ "models": p.models })));
    } else {
        let client = crate::providers::build_client(p)?;
        // base_url 可能带/不带 /v1，两个候选都试一遍
        let mut last_err = String::from("无法获取模型列表");
        let mut found: Option<Vec<String>> = None;
        for url in crate::providers::models_urls(p) {
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", key))
                .send()
                .await
                .map_err(|e| AppErr(format!("网络请求失败: {}", e)))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                last_err = format!(
                    "HTTP {}：{}",
                    status.as_u16(),
                    text.chars().take(200).collect::<String>()
                );
                continue;
            }
            let v: Value = serde_json::from_str(&text).map_err(|e| AppErr(e.to_string()))?;
            let mut ids: Vec<String> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            ids.sort();
            if ids.is_empty() {
                last_err = format!("{} 的返回里没有 data[].id", url);
                continue;
            }
            found = Some(ids);
            break;
        }
        match found {
            Some(ids) => Ok(ids),
            None => Err(last_err),
        }
    };

    match res {
        Ok(models) => Ok(Json(json!({ "models": models }))),
        Err(e) => Err(AppErr(e)),
    }
}

// ---------- 对话 ----------
#[derive(Deserialize)]
struct ChatBody {
    provider_id: String,
    model: String,
    messages: Vec<ChatMessage>,
    conv_id: String,
    /// 🎨 生图开关（手动）；模型名命中生图模型时后端也会自动走生图分支。
    #[serde(default)]
    image_mode: bool,
    /// 是否启用 Agent 模式（工具调用 + 联网）。仅对 API 协议生效。
    #[serde(default)]
    agent_mode: bool,
}
async fn chat(State(s): State<AppState>, Json(b): Json<ChatBody>) -> Result<Json<Value>, AppErr> {
    let (mtx, mut mrx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
    let dir = s.data_dir.clone();
    let pid = b.provider_id.clone();
    let model = b.model.clone();
    let msgs = b.messages.clone();
    let conv_for_chat = b.conv_id.clone();
    let image_mode = b.image_mode;
    let agent_mode = b.agent_mode;

    // 登记中止标志；同会话上次还在跑的任务先停掉
    let flag: AbortFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut map = s.aborts.lock().unwrap();
        if let Some(old) = map.insert(conv_for_chat.clone(), flag.clone()) {
            old.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let flag_for_task = flag.clone();
    tokio::spawn(async move {
        chat::stream_chat(
            &dir,
            &pid,
            &model,
            &msgs,
            &conv_for_chat,
            mtx,
            image_mode,
            agent_mode,
            Some(flag_for_task),
        )
        .await;
    });

    let tx = s.tx.clone();
    let conv_id = b.conv_id.clone();
    let aborts = s.aborts.clone();
    let flag_done = flag.clone();
    tokio::spawn(async move {
        while let Some(ev) = mrx.recv().await {
            let (event, payload) = match ev {
                ChatEvent::Delta(d) => (
                    "chat_stream",
                    json!({"conv_id": conv_id, "delta": d, "done": false, "error": null}),
                ),
                ChatEvent::Reasoning(r) => (
                    "chat_reasoning",
                    json!({"conv_id": conv_id, "delta": r}),
                ),
                ChatEvent::Active { account } => ("account_ok", json!({"account": account})),
                ChatEvent::ToolCall { name, args } => (
                    "agent_tool",
                    json!({"conv_id": conv_id, "phase": "call", "name": name, "args": args}),
                ),
                ChatEvent::ToolResult { name, ok, summary } => (
                    "agent_tool",
                    json!({"conv_id": conv_id, "phase": "result", "name": name,
                           "ok": ok, "summary": summary}),
                ),
                ChatEvent::Switch { account, reason } => {
                    ("account_switch", json!({"account": account, "reason": reason}))
                }
                ChatEvent::Done => (
                    "chat_stream",
                    json!({"conv_id": conv_id, "delta": "", "done": true, "error": null}),
                ),
                ChatEvent::Error(e) => (
                    "chat_stream",
                    json!({"conv_id": conv_id, "delta": "", "done": true, "error": e}),
                ),
            };
            if tx
                .send(ServerEvent {
                    event: event.into(),
                    payload,
                })
                .is_err()
            {
                break;
            }
        }
        // 流结束：清理中止标志（仅当还是自己这一个）
        if let Ok(mut map) = aborts.lock() {
            if let Some(cur) = map.get(&conv_id) {
                if Arc::ptr_eq(cur, &flag_done) {
                    map.remove(&conv_id);
                }
            }
        }
    });
    Ok(Json(json!({"ok": true})))
}

/// 停止某个会话的流式输出。
#[derive(Deserialize)]
struct StopBody {
    conv_id: String,
}
async fn chat_stop(State(s): State<AppState>, Json(b): Json<StopBody>) -> StatusCode {
    if let Ok(map) = s.aborts.lock() {
        if let Some(f) = map.get(&b.conv_id) {
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    StatusCode::OK
}

// ---------- 文件上传 / 本地文件读取 ----------
#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    #[serde(default)]
    mime: Option<String>,
}
/// 原始字节上传（不做 multipart，省依赖）：
/// POST /api/upload?name=xx.png&mime=image/png，body 为文件字节。
async fn upload(
    State(s): State<AppState>,
    Query(q): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, AppErr> {
    if body.is_empty() {
        return Err(AppErr("上传内容为空".into()));
    }
    let saved = files::save_upload(&s.data_dir, &q.name, q.mime.as_deref().unwrap_or(""), &body)
        .map_err(AppErr::from)?;
    let url = format!("/api/files/{}", saved.id);
    Ok(Json(json!({
        "id": saved.id,
        "name": saved.name,
        "mime": saved.mime,
        "size": saved.size,
        "kind": saved.kind.as_str(),
        "text": saved.text,
        "url": url,
    })))
}

async fn serve_file(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match files::read_upload(&s.data_dir, &id) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(&id)),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve_image(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match files::read_image(&s.data_dir, &id) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(&id)),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---------- ChatGPT OAuth 登录 ----------
#[derive(Deserialize)]
struct OauthStartBody {
    #[serde(default)]
    email_hint: Option<String>,
}

/// 本机 loopback 回调地址。Codex 客户端在 Auth0 侧注册了 loopback 回调，
/// 任意 localhost 端口都合法，所以这里直接用本服务的实际端口。
fn loopback_redirect(port: u16) -> String {
    format!("http://127.0.0.1:{}/auth/callback", port)
}

/// 第一步：生成 PKCE + authorize URL，由前端打开让用户登录。
/// 授权完成后浏览器会直接跳回本服务 `/auth/callback`，全自动，无需粘网址。
async fn oauth_start(
    State(s): State<AppState>,
    Json(_b): Json<OauthStartBody>,
) -> Result<Json<Value>, AppErr> {
    let (verifier, challenge) = oauth::generate_pkce();
    let session_id = oauth::random_hex(16);
    let state = format!("{}.{}", session_id, oauth::random_hex(8));
    let port = s.port.load(std::sync::atomic::Ordering::Relaxed);
    let redirect_uri = loopback_redirect(port);
    let pending = PendingOAuth {
        code_verifier: verifier,
        code_challenge: challenge,
        state: state.clone(),
        redirect_uri: redirect_uri.clone(),
    };
    let url = oauth::codex_authorize_url(&pending, &redirect_uri);
    {
        let mut map = s.oauth_pending.lock().unwrap();
        // 清理 10 分钟前的过期会话
        let now = now_secs();
        map.retain(|_, (_, ts)| now - *ts < 600);
        map.insert(session_id.clone(), (pending, now));
    }
    Ok(Json(json!({
        "session_id": session_id,
        "authorize_url": url,
        "redirect_uri": redirect_uri,
    })))
}

/// 把 token 三件套落进账号池（callback / finish 共用）。
async fn persist_oauth_account(s: &AppState, tokens: oauth::TokenSet) -> Result<Account, AppErr> {
    // 账号 ID / 套餐 这些 claim 有时在 id_token、有时只在 access_token 里，
    // 两边都解析再合并——只取 id_token 会漏掉 chatgpt_account_id，
    // 而它缺失会让 backend-api 的 sentinel 接口直接 500。
    let mut merged = oauth::IdClaims::default();
    for c in [
        tokens.id_token.as_deref().and_then(oauth::parse_jwt_claims),
        oauth::parse_jwt_claims(&tokens.access_token),
    ]
    .into_iter()
    .flatten()
    {
        if merged.email.is_none() {
            merged.email = c.email;
        }
        if merged.chatgpt_account_id.is_none() {
            merged.chatgpt_account_id = c.chatgpt_account_id;
        }
        if merged.chatgpt_plan_type.is_none() {
            merged.chatgpt_plan_type = c.chatgpt_plan_type;
        }
    }
    let has_claims =
        merged.email.is_some() || merged.chatgpt_account_id.is_some();
    let claims = if has_claims { Some(merged) } else { None };
    let mut meta = serde_json::Map::new();
    if let Some(rt) = &tokens.refresh_token {
        meta.insert("refresh_token".into(), json!(rt));
    }
    if let Some(c) = &claims {
        if let Some(e) = &c.email {
            meta.insert("email".into(), json!(e));
        }
        if let Some(a) = &c.chatgpt_account_id {
            meta.insert("chatgpt_account_id".into(), json!(a));
        }
        if let Some(p) = &c.chatgpt_plan_type {
            meta.insert("plan_type".into(), json!(p));
        }
    }
    let acc = Account {
        id: String::new(),
        name: claims
            .as_ref()
            .and_then(|c| c.email.clone())
            .unwrap_or_else(|| "ChatGPT OAuth 账号".into()),
        cred_type: "token".into(),
        secret: tokens.access_token,
        cookie: None,
        meta: Some(Value::Object(meta)),
        status: "active".into(),
        last_error: None,
        last_used: None,
        priority: 0,
    };
    let saved = config::add_account(&s.data_dir, "chatgpt", acc).map_err(AppErr::from)?;
    let _ = s.tx.send(ServerEvent {
        event: "login-captured".into(),
        payload: json!({ "provider": "chatgpt", "account": saved.name }),
    });
    Ok(saved)
}

/// 浏览器回调页（用户看到的最后一屏）。成功时自动关标签页。
fn callback_page(ok: bool, title: &str, detail: &str, app_url: &str) -> Html<String> {
    let color = if ok { "#16a34a" } else { "#dc2626" };
    let auto_close = if ok {
        "<script>setTimeout(function(){try{window.close()}catch(e){}},1200)</script>"
    } else {
        ""
    };
    Html(format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<title>{title}</title><meta name="viewport" content="width=device-width,initial-scale=1">
<style>
body{{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;
background:#0f1115;color:#e6e8ee;font-family:-apple-system,"Segoe UI",system-ui,sans-serif}}
.card{{max-width:520px;padding:36px 40px;border-radius:16px;background:#171a21;
border:1px solid #262b36;box-shadow:0 18px 50px rgba(0,0,0,.45);text-align:center}}
.badge{{display:inline-block;font-size:13px;font-weight:600;letter-spacing:.08em;
padding:4px 12px;border-radius:999px;border:1px solid {color};color:{color};margin-bottom:16px}}
h1{{font-size:20px;margin:0 0 10px;color:{color}}}
p{{margin:0;color:#9aa4b8;font-size:14px;line-height:1.7;word-break:break-all}}
a.btn{{display:inline-block;margin-top:22px;padding:10px 22px;border-radius:9px;
background:#3b82f6;color:#fff;text-decoration:none;font-size:14px}}
a.btn:hover{{background:#2563eb}}
</style></head><body><div class="card">
<div class="badge">AI HUB</div><h1>{title}</h1><p>{detail}</p>
<a class="btn" href="{app_url}">返回 AI Hub</a>
</div>{auto_close}</body></html>"#
    ))
}

#[derive(Deserialize)]
struct OauthCallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// 第二步（全自动）：OpenAI 授权后浏览器直接跳到这里，服务端拿 code 换 token。
async fn oauth_callback(
    State(s): State<AppState>,
    Query(q): Query<OauthCallbackQuery>,
) -> Html<String> {
    let port = s.port.load(std::sync::atomic::Ordering::Relaxed);
    let app_url = format!("http://127.0.0.1:{}/", port);

    if let Some(err) = q.error.as_deref() {
        let desc = q.error_description.clone().unwrap_or_default();
        return callback_page(
            false,
            "授权未完成",
            &format!("OpenAI 返回错误：{} {}", err, desc),
            &app_url,
        );
    }
    let (Some(code), Some(state)) = (q.code.clone(), q.state.clone()) else {
        return callback_page(false, "授权未完成", "回调缺少 code / state 参数", &app_url);
    };
    // state 形如 "<session_id>.<nonce>"
    let session_id = state.split('.').next().unwrap_or("").to_string();
    let pending = {
        let map = s.oauth_pending.lock().unwrap();
        map.get(&session_id).map(|(p, _)| p.clone())
    };
    let Some(pending) = pending else {
        return callback_page(
            false,
            "登录会话已过期",
            "请回到 AI Hub 重新点「一键登录」，10 分钟内完成授权。",
            &app_url,
        );
    };
    if pending.state != state {
        return callback_page(
            false,
            "state 校验失败",
            "可能点过两次「一键登录」。请回到 AI Hub 重新发起。",
            &app_url,
        );
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => return callback_page(false, "内部错误", &e.to_string(), &app_url),
    };
    let tokens = match oauth::codex_exchange_code(
        &client,
        &code,
        &pending.code_verifier,
        &pending.redirect_uri,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return callback_page(false, "换取 Token 失败", &e, &app_url),
    };
    match persist_oauth_account(&s, tokens).await {
        Ok(acc) => {
            s.oauth_pending.lock().unwrap().remove(&session_id);
            callback_page(
                true,
                "登录成功",
                &format!("已添加账号 {}，本页会自动关闭。", acc.name),
                &app_url,
            )
        }
        Err(e) => callback_page(false, "保存账号失败", &e.0, &app_url),
    }
}

#[derive(Deserialize)]
struct OauthFinishBody {
    session_id: String,
    /// 兜底流程：用户粘贴回来的完整回调 URL（或裸 code）。
    callback: String,
}
/// 兜底：手动粘贴回调 URL 完成登录（loopback 被拦截时使用）。
async fn oauth_finish(
    State(s): State<AppState>,
    Json(b): Json<OauthFinishBody>,
) -> Result<Json<Value>, AppErr> {
    let pending = {
        let map = s.oauth_pending.lock().unwrap();
        map.get(&b.session_id).map(|(p, _)| p.clone())
    };
    let Some(pending) = pending else {
        return Err(AppErr("OAuth 会话已过期或不存在，请重新点「一键登录」".into()));
    };
    let (code, state, _base) = oauth::extract_callback(&b.callback)?;
    if !state.is_empty() && state != pending.state {
        return Err(AppErr(
            "state 不匹配：可能点过两次「一键登录」。请重新生成再走一遍。".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| AppErr(e.to_string()))?;
    let tokens = oauth::codex_exchange_code(
        &client,
        &code,
        &pending.code_verifier,
        &pending.redirect_uri,
    )
    .await?;
    let saved = persist_oauth_account(&s, tokens).await?;
    s.oauth_pending.lock().unwrap().remove(&b.session_id);
    Ok(Json(json!({ "ok": true, "account": saved })))
}

// ---------- 会话历史 ----------
async fn list_convs(State(s): State<AppState>) -> Result<Json<Vec<Conversation>>, AppErr> {
    Ok(Json(history::list_conversations(&s.data_dir)))
}
#[derive(Deserialize)]
struct CreateConvBody {
    id: String,
    title: String,
    provider: String,
    model: String,
}
async fn create_conv(
    State(s): State<AppState>,
    Json(b): Json<CreateConvBody>,
) -> Result<Json<Conversation>, AppErr> {
    Ok(Json(
        history::create_conversation(&s.data_dir, &b.id, &b.title, &b.provider, &b.model)
            .map_err(AppErr::from)?,
    ))
}
async fn get_conv(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Conversation>, AppErr> {
    Ok(Json(history::get_conversation(&s.data_dir, &id).map_err(AppErr::from)?))
}
async fn del_conv(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, AppErr> {
    history::delete_conversation(&s.data_dir, &id).map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}
#[derive(Deserialize)]
struct RenameBody {
    title: String,
}
async fn rename_conv(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<RenameBody>,
) -> Result<StatusCode, AppErr> {
    history::rename_conversation(&s.data_dir, &id, &b.title).map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}
#[derive(Deserialize)]
struct SaveMsgBody {
    role: String,
    content: String,
    #[serde(default)]
    attachments: Vec<Attachment>,
}
async fn save_msg(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<SaveMsgBody>,
) -> Result<StatusCode, AppErr> {
    history::save_message(&s.data_dir, &id, &b.role, &b.content, b.attachments)
        .map_err(AppErr::from)?;
    Ok(StatusCode::OK)
}

// ---------- SSE 事件通道 ----------
async fn events(
    State(s): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    let rx = s.tx.subscribe();
    let inner = BroadcastStream::new(rx).filter_map(|r| match r {
        Ok(ev) => {
            let data = json!({"event": ev.event, "payload": ev.payload}).to_string();
            Some(Ok(axum::response::sse::Event::default().data(data)))
        }
        Err(_) => None,
    });

    // 活跃 SSE 计数：包一层流，断开（页面关闭/切走/崩溃）时 Drop 自动减一。
    struct Counting<S> {
        inner: S,
        n: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl<S> futures_util::Stream for Counting<S>
    where
        S: futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
            + Unpin,
    {
        type Item = S::Item;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::pin::Pin::new(&mut self.inner).poll_next(cx)
        }
    }
    impl<S> Drop for Counting<S> {
        fn drop(&mut self) {
            self.n.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    s.sse_clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    s.saw_client
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let stream = Counting {
        inner,
        n: s.sse_clients.clone(),
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ---------- 退出 ----------
/// 前端心跳：证明「这个标签页还活着」。任何请求都会取消待退出状态，
/// 所以多标签时关掉其中一个不会把服务带走。
async fn ping() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// 临时诊断：查看退出判定相关状态。
async fn debug_exit(State(s): State<AppState>) -> Json<Value> {
    let deadline = s.quit_deadline.lock().ok().and_then(|d| *d);
    Json(json!({
        "now": now_secs(),
        "deadline": deadline,
        "sse_clients": s.sse_clients.load(std::sync::atomic::Ordering::SeqCst),
        "saw_client": s.saw_client.load(std::sync::atomic::Ordering::SeqCst),
        "should_quit": should_quit(&s.quit_deadline, &s.sse_clients, &s.saw_client, &s.last_activity),
    }))
}
/// 页面关闭（pagehide）时由前端 sendBeacon 调用。
/// 不再立即 shutdown——刷新页面同样会触发 pagehide，立即退出会误杀。
/// 改为设置退出死线；期间任何新请求都会取消它，只有页面真的不再回来了才退出。
async fn quit(State(s): State<AppState>) -> StatusCode {
    // 不区分单/多标签：一律设死线。多标签时另一个页面会继续发心跳
    // （每 15s 一次 /api/ping），死线会被立刻取消，所以不会误杀。
    if let Ok(mut d) = s.quit_deadline.lock() {
        if d.is_none() {
            *d = Some(now_secs() + QUIT_DELAY_SECS);
        }
    }
    StatusCode::OK
}

/// 退出判定：由 main 的 watcher 每 500ms 调用。
/// - 页面关闭：死线到达 → 退出（页面还开着的话心跳会把死线清掉）
/// - 兜底：页面曾打开过，之后既无 SSE 也无请求持续 ORPHAN_TIMEOUT_SECS（被强杀）
pub fn should_quit(
    quit_deadline: &Arc<Mutex<Option<i64>>>,
    sse_clients: &Arc<std::sync::atomic::AtomicUsize>,
    saw_client: &Arc<std::sync::atomic::AtomicBool>,
    last_activity: &Arc<std::sync::atomic::AtomicI64>,
) -> bool {
    let now = now_secs();
    let deadline = match quit_deadline.lock() {
        Ok(d) => *d,
        Err(_) => return false,
    };
    let clients = sse_clients.load(std::sync::atomic::Ordering::SeqCst);
    match deadline {
        // 死线到点就退：页面开着时它的心跳（每 5s 一次 /api/ping）会
        // 不断把死线清掉，所以这里不需要再看 SSE 计数——SSE 连接在
        // 页面导航后可能滞留不释放，用它判断会永远退不掉。
        Some(d) => now >= d,
        None => {
            let saw = saw_client.load(std::sync::atomic::Ordering::SeqCst);
            let activity = last_activity.load(std::sync::atomic::Ordering::SeqCst);
            saw && clients == 0 && now - activity > ORPHAN_TIMEOUT_SECS
        }
    }
}

// ---------- 静态资源 ----------
async fn index(State(s): State<AppState>) -> Response {
    serve_asset(&s, "index.html")
}
async fn static_handler(State(s): State<AppState>, Path(path): Path<String>) -> Response {
    let p = path.trim_start_matches('/');
    if p == "api" || p.starts_with("api/") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    serve_asset(&s, p)
}
fn serve_asset(s: &AppState, path: &str) -> Response {
    let path = if path.is_empty() { "index.html" } else { path };

    // 调试模式：从磁盘读（改完刷新即可，无需重新编译）
    if let Some(dir) = &s.web_dir {
        let file = dir.join(path);
        if let Ok(bytes) = std::fs::read(&file) {
            return (
                [
                    (header::CONTENT_TYPE, content_type(path)),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response();
        }
    }

    if let Some(f) = WebAssets::get(path) {
        (
            [
                (header::CONTENT_TYPE, content_type(path)),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            f.data.into_owned(),
        )
            .into_response()
    } else if let Some(f) = WebAssets::get("index.html") {
        (
            [
                (
                    header::CONTENT_TYPE,
                    "text/html; charset=utf-8".to_string(),
                ),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            f.data.into_owned(),
        )
            .into_response()
    } else {
        (StatusCode::NOT_FOUND, "not found").into_response()
    }
}
/// 按扩展名推断 Content-Type（覆盖聊天附件常见类型，不只是 web 资源）。
/// 之前只认 html/js/css/图片，导致 txt/md/pdf/csv 一律 `octet-stream`，
/// 浏览器只会下载而不会内联预览。
fn content_type(path: &str) -> String {
    let ext = match path.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => String::new(),
    };
    let m = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" | "jsonl" => "application/json; charset=utf-8",
        "xml" => "text/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" | "log" | "srt" | "vtt" => "text/plain; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "csv" | "tsv" => "text/csv; charset=utf-8",
        "py" | "rs" | "go" | "java" | "c" | "h" | "cpp" | "hpp" | "cs" | "sh" | "bash" | "zsh"
        | "ps1" | "bat" | "cmd" | "sql" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf"
        | "tsx" | "jsx" | "ts" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };
    m.to_string()
}

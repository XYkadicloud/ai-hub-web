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
    response::{IntoResponse, Response, Sse},
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
}

pub fn build_router(state: AppState) -> Router {
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
        .route("/api/chat", post(chat))
        .route("/api/chat/stop", post(chat_stop))
        .route("/api/upload", post(upload))
        .route("/api/files/:id", get(serve_file))
        .route("/api/images/:id", get(serve_image))
        .route("/api/oauth/start", post(oauth_start))
        .route("/api/oauth/finish", post(oauth_finish))
        .route("/api/conversations", get(list_convs).post(create_conv))
        .route(
            "/api/conversations/:id",
            get(get_conv).delete(del_conv),
        )
        .route("/api/conversations/:id/rename", post(rename_conv))
        .route("/api/conversations/:id/messages", post(save_msg))
        .route("/api/events", get(events))
        .route("/api/quit", post(quit))
        .route("/*path", get(static_handler))
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024))
        .layer(middleware::from_fn(cors_layer))
        .with_state(state)
}

/// 全局 CORS：本地服务 + 浏览器书签脚本跨域回传需要。
/// 只监听 127.0.0.1，且所有接口本就面向本机单用户，放开来源没有额外风险。
async fn cors_layer(req: axum::extract::Request, next: Next) -> Response {
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
        "claude_web" => webchat::verify_claude(&b.secret, &p.base_url).await,
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
async fn version() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_ts": env!("BUILD_TS").parse::<u64>().unwrap_or(0),
        "mode": if cfg!(debug_assertions) { "debug" } else { "release" },
    }))
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
    } else if p.ptype == "chatgpt_web" || p.ptype == "claude_web" {
        return Ok(Json(json!({ "models": p.models })));
    } else {
        let base = p.base_url.trim().trim_end_matches('/');
        let client = crate::providers::build_client(p)?;
        let resp = client
            .get(format!("{}/models", base))
            .header("Authorization", format!("Bearer {}", key))
            .send()
            .await
            .map_err(|e| AppErr(format!("网络请求失败: {}", e)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppErr(format!(
                "HTTP {}：{}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            )));
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
        Ok(ids)
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
}
async fn chat(State(s): State<AppState>, Json(b): Json<ChatBody>) -> Result<Json<Value>, AppErr> {
    let (mtx, mut mrx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
    let dir = s.data_dir.clone();
    let pid = b.provider_id.clone();
    let model = b.model.clone();
    let msgs = b.messages.clone();
    let conv_for_chat = b.conv_id.clone();
    let image_mode = b.image_mode;

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
                ChatEvent::Active { account } => ("account_ok", json!({"account": account})),
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
/// 第一步：生成 PKCE + authorize URL，由前端打开让用户登录。
async fn oauth_start(
    State(s): State<AppState>,
    Json(b): Json<OauthStartBody>,
) -> Result<Json<Value>, AppErr> {
    let (verifier, challenge) = oauth::generate_pkce();
    let session_id = oauth::random_hex(16);
    let state = format!("{}.{}", session_id, oauth::random_hex(8));
    let pending = PendingOAuth {
        code_verifier: verifier,
        code_challenge: challenge,
        state,
    };
    let url = oauth::authorize_url(&pending, b.email_hint.as_deref().unwrap_or(""));
    {
        let mut map = s.oauth_pending.lock().unwrap();
        // 清理 10 分钟前的过期会话
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        map.retain(|_, (_, ts)| now - *ts < 600);
        map.insert(session_id.clone(), (pending, now));
    }
    Ok(Json(json!({ "session_id": session_id, "authorize_url": url })))
}

#[derive(Deserialize)]
struct OauthFinishBody {
    session_id: String,
    /// 用户从 platform.openai.com/auth/callback 地址栏复制的完整 URL（或裸 code）
    callback: String,
}
/// 第二步：用 code 换 token，落账号池。
async fn oauth_finish(
    State(s): State<AppState>,
    Json(b): Json<OauthFinishBody>,
) -> Result<Json<Value>, AppErr> {
    let pending = {
        let map = s.oauth_pending.lock().unwrap();
        map.get(&b.session_id).map(|(p, _)| p.clone())
    };
    let Some(pending) = pending else {
        return Err(AppErr("OAuth 会话已过期或不存在，请重新点「打开授权页」".into()));
    };
    let (code, state) = oauth::extract_code(&b.callback)?;
    if !state.is_empty() && state != pending.state {
        return Err(AppErr(
            "state 不匹配：可能点过两次「打开授权页」。请重新生成再走一遍。".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| AppErr(e.to_string()))?;
    let tokens = oauth::exchange_code(&client, &code, &pending.code_verifier).await?;

    // 解析 id_token 拿邮箱 / 账号 ID
    let claims = tokens
        .id_token
        .as_deref()
        .and_then(oauth::parse_jwt_claims);
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
    {
        let mut map = s.oauth_pending.lock().unwrap();
        map.remove(&b.session_id);
    }
    let _ = s.tx.send(ServerEvent {
        event: "login-captured".into(),
        payload: json!({ "provider": "chatgpt", "account": saved.name }),
    });
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
    let stream = BroadcastStream::new(rx).filter_map(|r| match r {
        Ok(ev) => {
            let data = json!({"event": ev.event, "payload": ev.payload}).to_string();
            Some(Ok(axum::response::sse::Event::default().data(data)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ---------- 退出 ----------
async fn quit(State(s): State<AppState>) -> StatusCode {
    s.shutdown.notify_one();
    StatusCode::OK
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
fn content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("");
    let m = match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    };
    m.to_string()
}

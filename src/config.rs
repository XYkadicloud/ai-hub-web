use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn new_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}-{}-{}", prefix, now(), n)
}

/// 账号池中的一个账号。secret 既是网页登录抓到的 token，也可以是官方 API Key，
/// 在 OpenAI 兼容接口里统一用作 `Authorization: Bearer <secret>`。
#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    #[serde(default)]
    pub id: String, // 为空时由后端生成
    pub name: String,
    pub cred_type: String, // "token" | "api_key"
    pub secret: String,
    #[serde(default)]
    pub cookie: Option<String>, // ChatGPT 网页版需要完整 Cookie
    #[serde(default)]
    pub meta: Option<serde_json::Value>, // 协议附加信息（Claude org_uuid、ChatGPT 用户信息等）
    #[serde(default = "default_status")]
    pub status: String, // "active" | "disabled" | "error" | "expired"
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
    #[serde(default)]
    pub priority: u32,
}

fn default_status() -> String {
    "active".into()
}

impl Account {
    pub fn healthy(&self) -> bool {
        self.status != "disabled"
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub ptype: String,
    pub base_url: String,
    pub api_key: String,
    pub login_url: Option<String>,
    pub models: Vec<String>,
    pub default_model: String,
    pub proxy: Option<String>,
    pub enabled: bool,
    pub preferred_account: Option<String>,
    pub accounts: Vec<Account>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AppConfig {
    pub providers: Vec<Provider>,
    pub selected_provider: String,
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

fn default_config() -> AppConfig {
    AppConfig {
        selected_provider: "gemini".into(),
        providers: vec![
            Provider {
                id: "gemini".into(),
                name: "Gemini (官方原生)".into(),
                ptype: "gemini_native".into(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                api_key: "".into(),
                login_url: None,
                models: vec![
                    "gemini-2.5-flash".into(),
                    "gemini-2.5-pro".into(),
                    "gemini-2.0-flash".into(),
                ],
                default_model: "gemini-2.5-flash".into(),
                proxy: None,
                enabled: true,
                preferred_account: None,
                accounts: vec![],
            },
            Provider {
                id: "chatgpt".into(),
                name: "ChatGPT (网页版登录)".into(),
                ptype: "chatgpt_web".into(),
                base_url: "https://chatgpt.com".into(),
                api_key: "".into(),
                login_url: Some("https://chatgpt.com".into()),
                models: vec![
                    "gpt-4o-mini".into(),
                    "gpt-4o".into(),
                    "gpt-4.1".into(),
                    "o3".into(),
                ],
                default_model: "gpt-4o-mini".into(),
                proxy: None,
                enabled: true,
                preferred_account: None,
                accounts: vec![],
            },
            Provider {
                id: "claude".into(),
                name: "Claude (网页版登录)".into(),
                ptype: "claude_web".into(),
                base_url: "https://claude.ai/api".into(),
                api_key: "".into(),
                login_url: Some("https://claude.ai".into()),
                // 网页版只认不带日期后缀的别名；带日期的老名字会回
                // model_not_available（不是账号问题，是名字不对）。
                models: vec![
                    "claude-sonnet-4-5".into(),
                    "claude-opus-4-1".into(),
                    "claude-3-7-sonnet-latest".into(),
                ],
                default_model: "claude-sonnet-4-5".into(),
                proxy: None,
                enabled: true,
                preferred_account: None,
                accounts: vec![],
            },
            Provider {
                id: "deepseek".into(),
                name: "DeepSeek (官方/反代)".into(),
                ptype: "openai".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                api_key: "".into(),
                login_url: None,
                models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
                default_model: "deepseek-chat".into(),
                proxy: None,
                enabled: true,
                preferred_account: None,
                accounts: vec![],
            },
        ],
    }
}

pub fn ensure_config(dir: &Path) -> Result<(), String> {
    let p = config_path(dir);
    if !p.exists() {
        let c = default_config();
        save_config_inner(dir, &c)?;
    }
    Ok(())
}

pub fn load_config(dir: &Path) -> Result<AppConfig, String> {
    let p = config_path(dir);
    if !p.exists() {
        ensure_config(dir)?;
    }
    let s = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let mut c: AppConfig = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    migrate(&mut c);
    Ok(c)
}

/// 老配置迁移：把早期用 OpenAI 兼容层的 Gemini 端点升级为原生协议。
fn migrate(c: &mut AppConfig) {
    for p in c.providers.iter_mut() {
        if p.id == "gemini"
            && p.ptype == "openai"
            && p.base_url.contains("generativelanguage.googleapis.com")
        {
            p.ptype = "gemini_native".into();
            p.base_url = crate::gemini::DEFAULT_BASE.into();
            p.name = "Gemini (官方原生)".into();
        }
    }
}

pub fn save_config(dir: &Path, c: &AppConfig) -> Result<(), String> {
    save_config_inner(dir, c)
}

fn save_config_inner(dir: &Path, c: &AppConfig) -> Result<(), String> {
    let _ = fs::create_dir_all(dir);
    let p = config_path(dir);
    let s = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    fs::write(&p, s).map_err(|e| e.to_string())
}

/// 取某 provider 当前可用账号（过滤禁用、按优先级排序、首选置顶）。
/// 账号池为空但填了单 key 时，把该 key 作为兜底账号。
pub fn resolve_accounts(p: &Provider) -> Vec<Account> {
    let mut accs = p.accounts.clone();
    if accs.is_empty() && !p.api_key.trim().is_empty() {
        accs.push(Account {
            id: "legacy".into(),
            name: "默认 API Key".into(),
            cred_type: "api_key".into(),
            secret: p.api_key.clone(),
            cookie: None,
            meta: None,
            status: "active".into(),
            last_error: None,
            last_used: None,
            priority: 0,
        });
    }
    let pref = p.preferred_account.clone();
    accs.retain(|a| a.healthy());
    accs.sort_by_key(|a| a.priority);
    if let Some(pid) = pref {
        if let Some(pos) = accs.iter().position(|a| a.id == pid) {
            let a = accs.remove(pos);
            accs.insert(0, a);
        }
    }
    accs
}

pub fn mark_account_error(dir: &Path, provider_id: &str, account_id: &str, err: &str) {
    let _ = with_provider(dir, provider_id, |p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.id == account_id) {
            a.status = "error".to_string();
            a.last_error = Some(err.to_string());
        }
    });
}

pub fn touch_account(dir: &Path, provider_id: &str, account_id: &str) {
    let _ = with_provider(dir, provider_id, |p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.id == account_id) {
            a.last_used = Some(now().to_string());
            if a.status == "error" {
                a.status = "active".into();
                a.last_error = None;
            }
        }
    });
}

fn with_provider<F>(dir: &Path, provider_id: &str, f: F) -> Result<(), String>
where
    F: FnOnce(&mut Provider),
{
    let mut cfg = load_config(dir)?;
    if let Some(p) = cfg.providers.iter_mut().find(|p| p.id == provider_id) {
        f(p);
        save_config_inner(dir, &cfg)
    } else {
        Err(format!("未找到 provider: {}", provider_id))
    }
}

/// 手动添加 / 网页登录写入账号（保存到账号池并返回）。
pub fn add_account(dir: &Path, provider_id: &str, mut account: Account) -> Result<Account, String> {
    if account.id.is_empty() {
        account.id = new_id("acc");
    }
    if account.name.is_empty() {
        account.name = "账号".into();
    }
    if account.status.is_empty() {
        account.status = "active".into();
    }
    with_provider(dir, provider_id, |p| {
        if let Some(existing) = p.accounts.iter_mut().find(|a| a.id == account.id) {
            let _ = std::mem::replace(existing, account.clone());
        } else {
            p.accounts.push(account.clone());
        }
    })?;
    Ok(account)
}

pub fn delete_account(dir: &Path, provider_id: &str, account_id: &str) -> Result<(), String> {
    with_provider(dir, provider_id, |p| {
        p.accounts.retain(|a| a.id != account_id);
        if p.preferred_account.as_deref() == Some(account_id) {
            p.preferred_account = None;
        }
    })
}

pub fn set_account_status(
    dir: &Path,
    provider_id: &str,
    account_id: &str,
    status: &str,
) -> Result<(), String> {
    with_provider(dir, provider_id, |p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.id == account_id) {
            a.status = status.to_string();
            if status == "active" {
                a.last_error = None;
            }
        }
    })
}

/// 更新账号凭据（OAuth 续期后写回新的 access_token / meta）。
pub fn update_account(
    dir: &Path,
    provider_id: &str,
    account_id: &str,
    secret: Option<String>,
    meta: Option<serde_json::Value>,
) {
    let _ = with_provider(dir, provider_id, |p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.id == account_id) {
            if let Some(s) = secret {
                a.secret = s;
                if a.status == "error" {
                    a.status = "active".into();
                    a.last_error = None;
                }
            }
            if let Some(m) = meta {
                a.meta = Some(m);
            }
        }
    });
}

/// 启动时修补：早期版本的 OAuth 登录只解析 id_token，导致账号 meta 里
/// 没有 `chatgpt_account_id`；缺它会让 backend-api 的 sentinel 接口 500。
/// 这里从 access_token 的 JWT claim 里把它补回去，用户不必重新登录。
pub fn backfill_chatgpt_meta(dir: &Path) -> Result<(), String> {
    let mut cfg = load_config(dir)?;
    let mut changed = false;
    for p in cfg.providers.iter_mut() {
        if p.ptype != "chatgpt_web" {
            continue;
        }
        for a in p.accounts.iter_mut() {
            let mut meta = match a.meta.clone() {
                Some(Value::Object(m)) => m,
                _ => serde_json::Map::new(),
            };
            let has_id = meta
                .get("chatgpt_account_id")
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if has_id {
                continue;
            }
            let Some(c) = crate::oauth::parse_jwt_claims(&a.secret) else {
                continue;
            };
            let mut dirty = false;
            if let Some(id) = c.chatgpt_account_id {
                meta.insert("chatgpt_account_id".into(), Value::String(id));
                dirty = true;
            }
            if let Some(pt) = c.chatgpt_plan_type {
                meta.insert("plan_type".into(), Value::String(pt));
                dirty = true;
            }
            if let Some(em) = c.email {
                meta.insert("email".into(), Value::String(em));
                dirty = true;
            }
            if dirty {
                a.meta = Some(Value::Object(meta));
                changed = true;
            }
        }
    }
    if changed {
        save_config(dir, &cfg)?;
    }
    Ok(())
}

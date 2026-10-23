//! ChatGPT 官方 OAuth（PKCE 授权码）登录与 Token 续期。
//!
//! 流程参考社区长期维护的 chat2api 类项目（`yukkcat/chatgpt2api` 的
//! `services/oauth_login_service.py`）：
//!
//! 1. 后端生成 `code_verifier` / `code_challenge`（S256）与 `state`，
//!    构造 `auth.openai.com/api/accounts/authorize` 授权链接；
//! 2. 用户在系统浏览器里正常登录 OpenAI；
//! 3. OpenAI 把浏览器重定向到 `platform.openai.com/auth/callback?code=...`，
//!    用户把地址栏 URL 回填（页面本身会报错，但 URL 里的 code 有效）；
//! 4. 后端用 verifier + code 调 `/api/accounts/oauth/token` 换取
//!    `access_token` / `refresh_token` / `id_token`；
//! 5. 之后 access_token 临期或失效时，用 refresh_token 调 `/oauth/token`
//!    自动续期（无需用户再次登录）。
//!
//! 这样拿到的 access_token 与网页版抓包得到的 JWT 同规格，可直接用于
//! `backend-api` 调用，且能自动续期，比一次性抓 token 更稳。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha3::{Digest, Sha3_256};

const AUTH_BASE: &str = "https://auth.openai.com";
const PLATFORM_BASE: &str = "https://platform.openai.com";
/// OpenAI 平台侧 OAuth 客户端（公开客户端，无 client_secret）。
pub const OAUTH_CLIENT_ID: &str = "app_2SKx67EdpoN0G6j64rFvigXD";
pub const OAUTH_REDIRECT_URI: &str = "https://platform.openai.com/auth/callback";
const OAUTH_AUDIENCE: &str = "https://api.openai.com/v1";
const AUTH0_CLIENT: &str =
    "eyJuYW1lIjoiYXV0aDAtc3BhLWpzIiwidmVyc2lvbiI6IjEuMjEuMCJ9";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

/// 一次待完成的 OAuth 授权会话（内存中，10 分钟有效）。
#[derive(Clone, Debug)]
pub struct PendingOAuth {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

/// 生成 PKCE 对：返回 (code_verifier, code_challenge)。
pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let mut h = Sha3_256::new();
    h.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(h.finalize());
    (verifier, challenge)
}

/// 生成随机 state（"<session_id>.<nonce>" 由调用方拼装，这里只出随机段）。
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 构造授权 URL。
pub fn authorize_url(pending: &PendingOAuth, email_hint: &str) -> String {
    let mut params = vec![
        ("issuer".to_string(), AUTH_BASE.to_string()),
        ("client_id".to_string(), OAUTH_CLIENT_ID.to_string()),
        ("audience".to_string(), OAUTH_AUDIENCE.to_string()),
        ("redirect_uri".to_string(), OAUTH_REDIRECT_URI.to_string()),
        ("device_id".to_string(), uuid_v4()),
        ("screen_hint".to_string(), "login_or_signup".to_string()),
        ("max_age".to_string(), "0".to_string()),
        (
            "scope".to_string(),
            "openid profile email offline_access".to_string(),
        ),
        ("response_type".to_string(), "code".to_string()),
        ("response_mode".to_string(), "query".to_string()),
        ("state".to_string(), pending.state.clone()),
        ("nonce".to_string(), random_hex(16)),
        (
            "code_challenge".to_string(),
            pending.code_challenge.clone(),
        ),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("auth0Client".to_string(), AUTH0_CLIENT.to_string()),
    ];
    let hint = email_hint.trim();
    if !hint.is_empty() {
        params.push(("login_hint".to_string(), hint.to_string()));
    }
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}/api/accounts/authorize?{}", AUTH_BASE, qs)
}

/// 从回调 URL（或裸 code）提取 (code, state)。
pub fn extract_code(value: &str) -> Result<(String, String), String> {
    let raw = value.trim();
    if raw.is_empty() {
        return Err("缺少 code 或回调 URL".into());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let query = raw.split('?').nth(1).unwrap_or("");
        let mut code = String::new();
        let mut state = String::new();
        for kv in query.split('&') {
            let mut it = kv.splitn(2, '=');
            let k = it.next().unwrap_or("");
            let v = it.next().unwrap_or("");
            let v = url_decode(v);
            match k {
                "code" => code = v,
                "state" => state = v,
                _ => {}
            }
        }
        if code.is_empty() {
            return Err("回调 URL 里没有 code 参数（登录可能未完成）".into());
        }
        Ok((code, state))
    } else {
        Ok((raw.to_string(), String::new()))
    }
}

/// code + verifier 换取 token 三件套。
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    code_verifier: &str,
) -> Result<TokenSet, String> {
    let resp = client
        .post(format!("{}/api/accounts/oauth/token", AUTH_BASE))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Origin", PLATFORM_BASE)
        .header("Referer", format!("{}/", PLATFORM_BASE))
        .header("auth0-client", AUTH0_CLIENT)
        .header("User-Agent", BROWSER_UA)
        .json(&json!({
            "client_id": OAUTH_CLIENT_ID,
            "code_verifier": code_verifier,
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": OAUTH_REDIRECT_URI,
        }))
        .send()
        .await
        .map_err(|e| format!("换 token 网络异常: {}", e))?;
    parse_token_response(resp, "OpenAI 拒绝换 token").await
}

/// refresh_token 续期 access_token。
pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<TokenSet, String> {
    let resp = client
        .post(format!("{}/oauth/token", AUTH_BASE))
        .header("Accept", "application/json")
        .header("User-Agent", BROWSER_UA)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OAUTH_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|e| format!("刷新 token 网络异常: {}", e))?;
    parse_token_response(resp, "OpenAI 拒绝刷新 token").await
}

async fn parse_token_response(
    resp: reqwest::Response,
    prefix: &str,
) -> Result<TokenSet, String> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = data
            .get("error_description")
            .or_else(|| data.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!(
            "{}（HTTP {}）{}{}",
            prefix,
            status.as_u16(),
            if detail.is_empty() { "" } else { ": " },
            if detail.is_empty() {
                text.chars().take(200).collect::<String>()
            } else {
                detail.to_string()
            }
        ));
    }
    let access_token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if access_token.is_empty() {
        return Err(format!("{}：响应里没有 access_token", prefix));
    }
    Ok(TokenSet {
        access_token,
        refresh_token: data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
        id_token: data
            .get("id_token")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string()),
    })
}

#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
}

/// JWT payload 的 claims（只取我们关心的字段）。
#[derive(Debug, Default, Deserialize)]
pub struct IdClaims {
    pub email: Option<String>,
    /// `https://api.openai.com/auth` 里的账号信息
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    #[serde(default)]
    pub chatgpt_plan_type: Option<String>,
}

/// 解析 id_token / access_token（JWT）的 payload。
/// 不校验签名——token 是从 OpenAI 官方端点经 TLS 直接取得的，可信。
pub fn parse_jwt_claims(jwt: &str) -> Option<IdClaims> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let auth = v.pointer("/https://api.openai.com/auth");
    Some(IdClaims {
        email: v.get("email").and_then(|x| x.as_str()).map(String::from),
        chatgpt_account_id: auth
            .and_then(|a| a.get("chatgpt_account_id"))
            .and_then(|x| x.as_str())
            .map(String::from),
        chatgpt_plan_type: auth
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|x| x.as_str())
            .map(String::from),
    })
}

/// 读取 JWT 的 exp（Unix 秒），解析失败返回 None。
pub fn jwt_exp(jwt: &str) -> Option<i64> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|x| x.as_i64())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = (bytes[i + 1] as char).to_digit(16);
                let l = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h * 16 + l) as u8);
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

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7],
        h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

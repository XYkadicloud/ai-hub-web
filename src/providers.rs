use crate::config::Provider;
use reqwest::Client;

/// 去掉首尾空白与末尾斜杠。
fn clean_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_string()
}

fn has_path(base: &str) -> bool {
    base.split("://")
        .nth(1)
        .map(|rest| rest.contains('/'))
        .unwrap_or(false)
}

/// 把 `provider.base_url` 规范化为若干候选的 chat completions 地址。
///
/// 各家「OpenAI 兼容」端点对 base_url 的约定并不统一，用户也很少知道
/// 到底该不该写 `/v1`，所以这里不猜死一个，而是给出一组候选、按顺序尝试：
///
/// * 已经写到 `/chat/completions` → 直接用；
/// * 带路径（如 `https://x/v1`）→ 后面接 `/chat/completions`；
/// * 只有主机名（如 `https://api.deepseek.com`）→ 先试 `/v1/chat/completions`，
///   再退回 `/chat/completions`。
///
/// 调用方遇到 404/405 就换下一个候选，这样 `https://x`、`https://x/v1`、
/// `https://x/v1/` 三种写法都能用。
pub fn chat_urls(p: &Provider) -> Vec<String> {
    let base = clean_base(&p.base_url);
    if base.is_empty() {
        return vec![];
    }
    if base.ends_with("/chat/completions") {
        return vec![base];
    }
    if has_path(&base) {
        vec![format!("{}/chat/completions", base)]
    } else {
        vec![
            format!("{}/v1/chat/completions", base),
            format!("{}/chat/completions", base),
        ]
    }
}

/// 兼容旧调用点：取第一个候选。
pub fn chat_url(p: &Provider) -> String {
    chat_urls(p)
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("{}/chat/completions", clean_base(&p.base_url)))
}

/// 模型列表地址（OpenAI 协议：`GET {base}/models`）。
pub fn models_urls(p: &Provider) -> Vec<String> {
    let base = clean_base(&p.base_url);
    if base.is_empty() {
        return vec![];
    }
    if let Some(stripped) = base.strip_suffix("/chat/completions") {
        return vec![format!("{}/models", stripped)];
    }
    if has_path(&base) {
        vec![format!("{}/models", base)]
    } else {
        vec![format!("{}/v1/models", base), format!("{}/models", base)]
    }
}

/// 按 provider 配置构造 reqwest client（含可选代理）。
pub fn build_client(p: &Provider) -> Result<Client, String> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(600));
    if let Some(proxy) = &p.proxy {
        let proxy = proxy.trim();
        if !proxy.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| e.to_string())?);
        }
    }
    builder.build().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(base: &str) -> Provider {
        Provider {
            id: "t".into(),
            name: "t".into(),
            ptype: "openai".into(),
            base_url: base.into(),
            api_key: String::new(),
            login_url: None,
            models: vec![],
            default_model: String::new(),
            proxy: None,
            enabled: true,
            preferred_account: None,
            accounts: vec![],
        }
    }

    #[test]
    fn normalizes_common_base_urls() {
        assert_eq!(
            chat_urls(&prov("https://api.deepseek.com/v1")),
            vec!["https://api.deepseek.com/v1/chat/completions"]
        );
        assert_eq!(
            chat_urls(&prov("https://api.deepseek.com/v1/")),
            vec!["https://api.deepseek.com/v1/chat/completions"]
        );
        assert_eq!(
            chat_urls(&prov("https://api.deepseek.com")),
            vec![
                "https://api.deepseek.com/v1/chat/completions",
                "https://api.deepseek.com/chat/completions"
            ]
        );
        assert_eq!(
            chat_urls(&prov("https://x.com/v1/chat/completions")),
            vec!["https://x.com/v1/chat/completions"]
        );
    }

    #[test]
    fn models_url_derives_from_base() {
        assert_eq!(
            models_urls(&prov("https://x.com/v1")),
            vec!["https://x.com/v1/models"]
        );
        assert_eq!(
            models_urls(&prov("https://x.com/v1/chat/completions")),
            vec!["https://x.com/v1/models"]
        );
    }
}

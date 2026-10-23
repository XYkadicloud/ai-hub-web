use crate::config::Provider;
use reqwest::Client;

/// 把 provider.base_url 规范化为 OpenAI 兼容的 chat completions 地址。
/// 例: "https://x/v1" -> "https://x/v1/chat/completions"
pub fn chat_url(p: &Provider) -> String {
    let base = p.base_url.trim().trim_end_matches('/');
    format!("{}/chat/completions", base)
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

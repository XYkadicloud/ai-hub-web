#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat;
mod chat_attach;
mod config;
mod files;
mod gemini;
mod history;
mod oauth;
mod providers;
mod server;
mod webchat;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 默认固定端口：书签脚本 / OAuth 回跳都依赖「端口不变」。
/// 被占用时回退为随机端口（功能仍可用，只是书签脚本要按界面提示的端口改）。
pub const DEFAULT_PORT: u16 = 25880;

fn data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("ai-hub")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".ai-hub")
    } else {
        PathBuf::from("ai-hub-data")
    }
}

fn log(dir: &PathBuf, msg: &str) {
    let p = dir.join("startup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("无法创建 Tokio 运行时");
    rt.block_on(run());
}

/// 解析 `--web-dir <path>`（也支持 `--web-dir=<path>`）。
fn parse_web_dir() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--web-dir=") {
            return Some(PathBuf::from(v));
        }
        if a == "--web-dir" {
            if let Some(v) = args.get(i + 1) {
                return Some(PathBuf::from(v));
            }
        }
        i += 1;
    }
    None
}

/// 端口优先级：`--port` > `AI_HUB_PORT` > 默认固定端口。
fn parse_port() -> u16 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--port=") {
            if let Ok(p) = v.parse() {
                return p;
            }
        }
        if a == "--port" {
            if let Some(v) = args.get(i + 1) {
                if let Ok(p) = v.parse() {
                    return p;
                }
            }
        }
        i += 1;
    }
    if let Ok(v) = std::env::var("AI_HUB_PORT") {
        if let Ok(p) = v.trim().parse() {
            return p;
        }
    }
    DEFAULT_PORT
}

async fn run() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(dir.join("conversations"));
    if let Err(e) = config::ensure_config(&dir) {
        log(&dir, &format!("初始化配置失败: {}", e));
    }

    // 可选：--web-dir <目录> 从磁盘加载前端（改 UI 无需重新编译）
    let web_dir = parse_web_dir();
    if let Some(w) = &web_dir {
        log(&dir, &format!("前端热加载目录: {}", w.display()));
    }

    let (tx, _rx) = tokio::sync::broadcast::channel::<server::ServerEvent>(1024);
    let state = server::AppState {
        data_dir: dir.clone(),
        tx,
        shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
        web_dir,
        aborts: Arc::new(Mutex::new(HashMap::new())),
        oauth_pending: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = server::build_router(state.clone());
    let shutdown = state.shutdown.clone();

    // 固定端口优先（书签脚本/OAuth 依赖端口稳定），被占用则回退随机端口
    let want = parse_port();
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", want)).await {
        Ok(l) => l,
        Err(e) => {
            log(&dir, &format!("固定端口 {} 绑定失败({})，回退随机端口", want, e));
            match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => l,
                Err(e2) => {
                    log(&dir, &format!("绑定本地端口失败: {}", e2));
                    return;
                }
            }
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{}/", port);
    log(&dir, &format!("AI Hub 已启动: {}（固定端口 {}）", url, want));

    // 自动打开系统默认浏览器
    let _ = open::that(&url);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown.notified() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await
        .ok();
    log(&dir, "AI Hub 已退出");
}

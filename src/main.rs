#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod agent_tools;
mod chat;
mod chat_attach;
mod config;
mod docext;
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

/// `--no-open`：只起服务不自动打开浏览器（服务端模式 / 调试用）。
fn parse_no_open() -> bool {
    std::env::args().skip(1).any(|a| a == "--no-open" || a == "--no-browser")
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
    // 修补老版本 OAuth 账号缺失的 chatgpt_account_id（否则网页版接口会 500）
    if let Err(e) = config::backfill_chatgpt_meta(&dir) {
        log(&dir, &format!("修补账号信息失败: {}", e));
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
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        saw_client: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_activity: Arc::new(std::sync::atomic::AtomicI64::new(server::now_secs())),
        quit_deadline: Arc::new(Mutex::new(None)),
        port: Arc::new(std::sync::atomic::AtomicU16::new(0)),
    };
    let app = server::build_router(state.clone());
    let shutdown = state.shutdown.clone();

    // 退出 watcher：页面关闭（/api/quit 设死线）或页面被强杀（无 SSE 无请求）
    // 才真正退出；刷新页面不会误杀。详见 server::should_quit。
    {
        let st = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if server::should_quit(
                    &st.quit_deadline,
                    &st.sse_clients,
                    &st.saw_client,
                    &st.last_activity,
                ) {
                    log(&st.data_dir, "检测到页面已关闭，正在退出");
                    shutdown.notify_one();
                    break;
                }
            }
        });
    }

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
    state
        .port
        .store(port, std::sync::atomic::Ordering::Relaxed);
    let url = format!("http://127.0.0.1:{}/", port);
    log(&dir, &format!("AI Hub 已启动: {}（固定端口 {}）", url, want));

    // 自动打开系统默认浏览器（--no-open 时跳过）
    if !parse_no_open() {
        let _ = open::that(&url);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown.notified() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            // SSE 长连接不会自己结束，优雅关闭可能一直挂着——
            // 3 秒后强制退出，保证进程一定能退干净（不会留下
            // 「无监听端口但占着连接」的僵尸状态）。
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                std::process::exit(0);
            });
        })
        .await
        .ok();
    log(&dir, "AI Hub 已退出");
}

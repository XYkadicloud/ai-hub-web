use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // 把构建时刻写入环境变量，供 /api/version 展示，便于核对 exe 是否为最新。
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_TS={}", ts);
}

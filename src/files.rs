//! 本地文件存储：上传目录、生图落盘、安全寻址。
//!
//! 所有用户文件都放在数据目录下：
//! * `uploads/` —— 聊天附件（图片 / 文本 / 其他）
//! * `images/`  —— 生图结果（base64 落盘，经 `/api/images/{id}` 提供访问）
//!
//! 文件名一律使用「内容哈希 + 扩展名」，既天然去重，也杜绝路径穿越
//! （外部传入的 id 只可能是 `[a-zA-Z0-9._-]` 形态的纯文件名）。

use sha3::{Digest, Sha3_256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 附件分类。
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Image,
    Text,
    Other,
}

impl FileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileKind::Image => "image",
            FileKind::Text => "text",
            FileKind::Other => "file",
        }
    }
}

/// 根据 MIME 与文件名推断附件分类。
pub fn detect_kind(mime: &str, name: &str) -> FileKind {
    let m = mime.trim().to_ascii_lowercase();
    if m.starts_with("image/") {
        return FileKind::Image;
    }
    if m.starts_with("text/") || m == "application/json" {
        return FileKind::Text;
    }
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    const TEXT_EXTS: &[&str] = &[
        "txt", "md", "markdown", "json", "jsonl", "csv", "tsv", "py", "js", "mjs", "ts", "tsx",
        "jsx", "html", "htm", "css", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf", "log",
        "sql", "rs", "go", "java", "c", "h", "cpp", "hpp", "cs", "sh", "bash", "zsh", "ps1",
        "bat", "cmd", "srt", "vtt", "tex",
    ];
    if TEXT_EXTS.contains(&ext.as_str()) {
        return FileKind::Text;
    }
    FileKind::Other
}

/// 文本类附件的最大内联字符数（防止把整个代码库塞进上下文）。
pub const MAX_INLINE_TEXT: usize = 120_000;

pub fn uploads_dir(dir: &Path) -> PathBuf {
    dir.join("uploads")
}
pub fn images_dir(dir: &Path) -> PathBuf {
    dir.join("images")
}

/// 生成安全文件名：内容哈希 + 原始扩展名。
fn hashed_name(bytes: &[u8], ext: &str) -> String {
    let mut h = Sha3_256::new();
    h.update(bytes);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    h.update(n.to_le_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(12).map(|b| format!("{:02x}", b)).collect();
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    let ext: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if ext.is_empty() {
        hex
    } else {
        format!("{}.{}", hex, ext)
    }
}

/// 取扩展名。注意必须用 `rsplit_once('.')`：`"README".rsplit('.').next()`
/// 会返回整个字符串（split 至少产出一个元素），于是无扩展名文件的 id 会被
/// 拼成 `xxx.readme`。
fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, e)) => e.to_string(),
        None => String::new(),
    }
}

/// 保存一份上传文件，返回可供前端引用的元数据。
/// 文本类文件会顺带读出截断内容（`text` 字段）；PDF / Office 也尽量提取文本。
pub fn save_upload(dir: &Path, name: &str, mime: &str, bytes: &[u8]) -> Result<SavedFile, String> {
    let d = uploads_dir(dir);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let id = hashed_name(bytes, &ext_of(name));
    let path = d.join(&id);
    if !path.exists() {
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    }
    let kind = detect_kind(mime, name);
    let text = match kind {
        FileKind::Text => Some(read_text_truncated(bytes)),
        // PDF / Office 不再只能报文件名：尽力抽出正文，失败仍降级为「仅文件名」
        FileKind::Other => crate::docext::extract_text(bytes, mime, name)
            .map(|t| t.chars().take(MAX_INLINE_TEXT).collect()),
        FileKind::Image => None,
    };
    Ok(SavedFile {
        id,
        name: sanitize_display_name(name),
        mime: if mime.trim().is_empty() {
            guess_mime(kind)
        } else {
            mime.to_string()
        },
        size: bytes.len() as u64,
        kind,
        text,
    })
}

/// 保存生图结果，返回文件 id（相对 images/）。
pub fn save_image(dir: &Path, mime: &str, bytes: &[u8]) -> Result<String, String> {
    let d = images_dir(dir);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let ext = match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    let id = hashed_name(bytes, ext);
    let path = d.join(&id);
    if !path.exists() {
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    }
    Ok(id)
}

/// 按 id 取上传文件内容（id 不信任外部输入，仅接受纯文件名字符）。
pub fn read_upload(dir: &Path, id: &str) -> Option<Vec<u8>> {
    if !is_safe_id(id) {
        return None;
    }
    std::fs::read(uploads_dir(dir).join(id)).ok()
}
pub fn read_image(dir: &Path, id: &str) -> Option<Vec<u8>> {
    if !is_safe_id(id) {
        return None;
    }
    std::fs::read(images_dir(dir).join(id)).ok()
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn read_text_truncated(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.chars().take(MAX_INLINE_TEXT).collect()
}

fn sanitize_display_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if cleaned.trim().is_empty() {
        "未命名文件".into()
    } else {
        cleaned
    }
}

fn guess_mime(kind: FileKind) -> String {
    match kind {
        FileKind::Image => "image/png".into(),
        FileKind::Text => "text/plain".into(),
        FileKind::Other => "application/octet-stream".into(),
    }
}

/// 保存上传/生图后返回给前端的结构。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub kind: FileKind,
    #[serde(default)]
    pub text: Option<String>,
}

use serde::{Deserialize, Serialize};

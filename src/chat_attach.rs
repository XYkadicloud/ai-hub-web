//! 聊天附件（图片 / 文本 / 其他文件）的协议适配。
//!
//! 设计要点：
//! * 附件文件本体存在后端 `uploads/`，消息里只带轻量元数据 + 文件 id；
//! * 发请求时按目标协议实时组装：OpenAI 兼容用 `content` 多模态数组，
//!   Gemini 原生用 `inline_data` parts，网页版协议降级为文字说明；
//! * 文本类附件直接内联进消息（带长度截断），图片转 base64 data URL。

use crate::files;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// 一条消息携带的附件（与前端 `SavedFile` 对应）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Attachment {
    /// image | text | file
    pub kind: String,
    pub name: String,
    pub mime: String,
    #[serde(default)]
    pub size: u64,
    /// 后端 uploads/ 下的文件 id（图片/文件类必有）
    #[serde(default)]
    pub path: Option<String>,
    /// 文本类附件的截断内容（前端回显与后端内联共用）
    #[serde(default)]
    pub text: Option<String>,
    /// 前端可访问的 URL（/api/files/{id} 或 /api/images/{id}）
    #[serde(default)]
    pub url: Option<String>,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.kind == "image"
    }
}

/// 模型名命中生图模型的关键词（小写包含匹配）。
const IMAGE_KEYWORDS: &[&str] = &[
    "image",
    "dall-e",
    "dalle",
    "flux",
    "gpt-image",
    "nano-banana",
    "imagen",
    "midjourney",
    "stable-diffusion",
    "cogview",
    "kolors",
    "seedream",
    "recraft",
    "ideogram",
    "playground-v",
    "janus",
];

/// 判断模型名是否为生图模型（用于 🎨 自动识别）。
pub fn is_image_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return false;
    }
    IMAGE_KEYWORDS.iter().any(|k| m.contains(k))
}

/// 生图结果推回聊天流的 Markdown。
pub fn image_markdown(url: &str, prompt: &str) -> String {
    let alt: String = prompt
        .chars()
        .take(60)
        .filter(|c| !matches!(c, '[' | ']' | '(' | ')' | '|'))
        .collect();
    let alt = alt.trim();
    if alt.is_empty() {
        format!("![image]({})", url)
    } else {
        format!("![{}]({})", alt, url)
    }
}

/// 读取附件的内联文本（优先用消息里带的 text，否则回读文件）。
fn attachment_text(dir: &Path, a: &Attachment) -> Option<String> {
    if let Some(t) = &a.text {
        if !t.trim().is_empty() {
            return Some(t.clone());
        }
    }
    let id = a.path.as_deref()?;
    let bytes = files::read_upload(dir, id)?;
    Some(String::from_utf8_lossy(&bytes).chars().take(files::MAX_INLINE_TEXT).collect())
}

/// 读取图片附件的 base64 data URL。
fn image_data_url(dir: &Path, a: &Attachment) -> Option<String> {
    let id = a.path.as_deref()?;
    let bytes = files::read_upload(dir, id)?;
    let mime = if a.mime.trim().is_empty() {
        "image/png"
    } else {
        a.mime.as_str()
    };
    Some(format!("data:{};base64,{}", mime, B64.encode(bytes)))
}

/// 把附件折算成「附加在正文之后的文字」。
fn textual_attachment_notes(dir: &Path, m: &crate::chat::ChatMessage) -> String {
    let mut notes = String::new();
    for a in &m.attachments {
        match a.kind.as_str() {
            "text" => {
                if let Some(t) = attachment_text(dir, a) {
                    notes.push_str(&format!(
                        "\n\n【附件：{}】\n```\n{}\n```",
                        a.name,
                        t.trim_end()
                    ));
                }
            }
            "file" => {
                notes.push_str(&format!(
                    "\n\n【附件：{}（{} 字节，{}）——当前会话无法直接读取该文件内容】",
                    a.name, a.size, a.mime
                ));
            }
            _ => {}
        }
    }
    notes
}

/// OpenAI 兼容协议的消息 content：无附件时是字符串，有图片时是多模态数组。
pub fn build_openai_content(dir: &Path, m: &crate::chat::ChatMessage) -> Value {
    let images: Vec<&Attachment> = m.attachments.iter().filter(|a| a.is_image()).collect();
    let notes = textual_attachment_notes(dir, m);
    let text = if notes.is_empty() {
        m.content.clone()
    } else {
        format!("{}{}", m.content, notes)
    };
    if images.is_empty() {
        return json!(text);
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    for a in images {
        if let Some(url) = image_data_url(dir, a) {
            parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
        }
    }
    json!(parts)
}

/// Gemini 原生协议的 parts：文本 + inline_data 图片。
pub fn build_gemini_parts(dir: &Path, m: &crate::chat::ChatMessage) -> Vec<Value> {
    let notes = textual_attachment_notes(dir, m);
    let text = if notes.is_empty() {
        m.content.clone()
    } else {
        format!("{}{}", m.content, notes)
    };
    let mut parts = vec![json!({ "text": text })];
    for a in m.attachments.iter().filter(|a| a.is_image()) {
        if let Some(id) = &a.path {
            if let Some(bytes) = files::read_upload(dir, id) {
                let mime = if a.mime.trim().is_empty() {
                    "image/png".to_string()
                } else {
                    a.mime.clone()
                };
                parts.push(json!({
                    "inline_data": { "mime": mime, "data": B64.encode(bytes) }
                }));
            }
        }
    }
    parts
}

/// 网页版协议（ChatGPT/Claude）只支持文本：附件降级为文字说明。
pub fn build_web_text(dir: &Path, m: &crate::chat::ChatMessage) -> String {
    let mut notes = String::new();
    for a in &m.attachments {
        match a.kind.as_str() {
            "image" => notes.push_str(&format!("\n\n【附带图片：{}（{}）】", a.name, a.mime)),
            "text" => {
                if let Some(t) = attachment_text(dir, a) {
                    notes.push_str(&format!("\n\n【附件：{}】\n{}", a.name, t.trim_end()));
                }
            }
            _ => notes.push_str(&format!("\n\n【附件：{}】", a.name)),
        }
    }
    if notes.is_empty() {
        m.content.clone()
    } else {
        format!("{}{}", m.content, notes)
    }
}

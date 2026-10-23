use crate::chat_attach::Attachment;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
pub struct Msg {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub messages: Vec<Msg>,
    #[serde(default)]
    pub meta: Value,
    pub created: i64,
    pub updated: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn conv_dir(dir: &Path) -> PathBuf {
    dir.join("conversations")
}

fn conv_path(dir: &Path, id: &str) -> PathBuf {
    conv_dir(dir).join(format!("{}.json", id))
}

pub fn list_conversations(dir: &Path) -> Vec<Conversation> {
    let d = conv_dir(dir);
    let _ = fs::create_dir_all(&d);
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&d) {
        for e in entries.flatten() {
            if let Ok(s) = fs::read_to_string(e.path()) {
                if let Ok(c) = serde_json::from_str::<Conversation>(&s) {
                    out.push(c);
                }
            }
        }
    }
    out.sort_by(|a, b| b.updated.cmp(&a.updated));
    out
}

pub fn create_conversation(
    dir: &Path,
    id: &str,
    title: &str,
    provider: &str,
    model: &str,
) -> Result<Conversation, String> {
    let d = conv_dir(dir);
    let _ = fs::create_dir_all(&d);
    let t = now();
    let c = Conversation {
        id: id.to_string(),
        title: if title.trim().is_empty() {
            "新对话".into()
        } else {
            title.to_string()
        },
        provider: provider.to_string(),
        model: model.to_string(),
        messages: vec![],
        meta: Value::Object(Default::default()),
        created: t,
        updated: t,
    };
    let s = serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?;
    fs::write(conv_path(dir, id), s).map_err(|e| e.to_string())?;
    Ok(c)
}

pub fn get_conversation(dir: &Path, id: &str) -> Result<Conversation, String> {
    let s = fs::read_to_string(conv_path(dir, id)).map_err(|_| "会话不存在".to_string())?;
    serde_json::from_str(&s).map_err(|e| e.to_string())
}

pub fn delete_conversation(dir: &Path, id: &str) -> Result<(), String> {
    let p = conv_path(dir, id);
    if p.exists() {
        fs::remove_file(p).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn get_conversation_meta(dir: &Path, id: &str) -> Value {
    match get_conversation(dir, id) {
        Ok(c) => c.meta.clone(),
        Err(_) => Value::Object(Default::default()),
    }
}

pub fn set_conversation_meta(dir: &Path, id: &str, meta: Value) -> Result<(), String> {
    let p = conv_path(dir, id);
    if !p.exists() {
        return Err("会话不存在".into());
    }
    let mut c: Conversation =
        serde_json::from_str(&fs::read_to_string(&p).map_err(|_| "会话不存在".to_string())?)
            .map_err(|e| e.to_string())?;
    c.meta = meta;
    c.updated = now();
    fs::write(&p, serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn rename_conversation(dir: &Path, id: &str, title: &str) -> Result<(), String> {
    let p = conv_path(dir, id);
    if !p.exists() {
        return Err("会话不存在".into());
    }
    let mut c: Conversation =
        serde_json::from_str(&fs::read_to_string(&p).map_err(|_| "会话不存在".to_string())?)
            .map_err(|e| e.to_string())?;
    c.title = title.to_string();
    c.updated = now();
    fs::write(&p, serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn save_message(
    dir: &Path,
    id: &str,
    role: &str,
    content: &str,
    attachments: Vec<Attachment>,
) -> Result<(), String> {
    let p = conv_path(dir, id);
    if !p.exists() {
        return Err("会话不存在".into());
    }
    let mut c: Conversation =
        serde_json::from_str(&fs::read_to_string(&p).map_err(|_| "会话不存在".to_string())?)
            .map_err(|e| e.to_string())?;
    let first_title = if c.messages.is_empty() && c.title == "新对话" {
        let t: String = content.chars().take(20).collect();
        if !t.trim().is_empty() {
            Some(t)
        } else {
            None
        }
    } else {
        None
    };
    c.messages.push(Msg {
        role: role.to_string(),
        content: content.to_string(),
        attachments,
        ts: now(),
    });
    c.updated = now();
    if let Some(t) = first_title {
        c.title = t;
    }
    fs::write(&p, serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(())
}

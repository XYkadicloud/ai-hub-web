//! 从常见文档格式里抽取纯文本，让「file」类附件也能真正读内容。
//!
//! 支持：
//! * **PDF** —— 解析对象流，`FlateDecode` 解压后取 `Tj` / `TJ` 里的字符串；
//!   不做完整 PDF 语法解析（那是几十万行的活），只做「能救回多少救多少」。
//! * **Office**（docx / xlsx / pptx）—— 本质是 zip，解出 `word/document.xml`、
//!   `xl/sharedStrings.xml`、`ppt/slides/slideN.xml`，剥掉 XML 标签即可。
//!
//! 提取结果统一截断到 `files::MAX_INLINE_TEXT`，防止把整本书塞进上下文。

use flate2::read::DeflateDecoder;
use std::io::Read;

/// 从文件字节里尽力提取文本。失败返回 None（调用方降级为「仅文件名」）。
pub fn extract_text(bytes: &[u8], mime: &str, name: &str) -> Option<String> {
    let m = mime.trim().to_ascii_lowercase();
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if m == "application/pdf" || ext == "pdf" || looks_like_pdf(bytes) {
        return extract_pdf(bytes);
    }
    if m.contains("officedocument") || m.contains("ms-excel") || m.contains("ms-powerpoint")
        || ["docx", "xlsx", "pptx"].contains(&ext.as_str())
        || looks_like_zip(bytes)
    {
        return extract_zip_document(bytes);
    }
    None
}

fn looks_like_pdf(b: &[u8]) -> bool {
    b.starts_with(b"%PDF-")
}
fn looks_like_zip(b: &[u8]) -> bool {
    b.starts_with(b"PK\x03\x04")
}

// ---------------------------- PDF ----------------------------

/// 提取 PDF 文本：按 `stream ... endstream` 切出流，尝试 inflate，
/// 再在内容流里找 `(...) Tj` 与 `[...] TJ` 的字符串操作符。
fn extract_pdf(bytes: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut idx = 0usize;
    while let Some(rel) = find(bytes, idx, b"stream") {
        let start = rel + 6;
        // 跳过 EOL
        let mut s = start;
        while s < bytes.len() && (bytes[s] == b'\r' || bytes[s] == b'\n') {
            s += 1;
        }
        let end = match find(bytes, s, b"endstream") {
            Some(e) => e,
            None => break,
        };
        let raw = &bytes[s..end];
        if !raw.is_empty() {
            // 先按原样试（未压缩流），再按 zlib 试
            let decoded = if raw.starts_with(&[0x78]) || raw.starts_with(&[0x0b]) {
                inflate(raw).unwrap_or_else(|| raw.to_vec())
            } else {
                inflate(raw).unwrap_or_else(|| raw.to_vec())
            };
            pdf_strings_to_text(&decoded, &mut out);
        }
        idx = end + 9;
        if out.len() > 200_000 {
            break;
        }
    }
    if out.trim().is_empty() {
        // 有些 PDF 把文本直接以明文放在对象里（未压缩）
        pdf_strings_to_text(bytes, &mut out);
    }
    let t = clean_text(&out);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 在内容流里找文本绘制操作：`(text) Tj`、`[(a) -2 (b)] TJ`、`'`、`"`。
fn pdf_strings_to_text(data: &[u8], out: &mut String) {
    let mut i = 0usize;
    while i < data.len() {
        let c = data[i];
        if c == b'(' {
            // 字面字符串，处理 \( \) \\ 与八进制 \ddd
            let mut j = i + 1;
            let mut depth = 1;
            let mut s = Vec::new();
            while j < data.len() {
                let ch = data[j];
                if ch == b'\\' && j + 1 < data.len() {
                    let n = data[j + 1];
                    match n {
                        b'n' => s.push(b'\n'),
                        b'r' => s.push(b'\r'),
                        b't' => s.push(b'\t'),
                        b'(' | b')' | b'\\' => s.push(n),
                        b'0'..=b'7' => {
                            let mut oct = String::new();
                            let mut k = j + 1;
                            while k < data.len()
                                && oct.len() < 3
                                && (b'0'..=b'7').contains(&data[k])
                            {
                                oct.push(data[k] as char);
                                k += 1;
                            }
                            if let Ok(v) = u8::from_str_radix(&oct, 8) {
                                s.push(v);
                            }
                            // 八进制转义占 k-j 个字节，外层统一 j += 2，这里回退差值
                            j = k - 2;
                        }
                        _ => s.push(n),
                    }
                    j += 2;
                    continue;
                }
                if ch == b'(' {
                    depth += 1;
                    s.push(ch);
                } else if ch == b')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    s.push(ch);
                } else {
                    s.push(ch);
                }
                j += 1;
            }
            push_lossy(&s, out);
            // 跳过操作符部分
            i = j + 1;
            continue;
        }
        if c == b'<' && i + 1 < data.len() && data[i + 1] != b'<' {
            if let Some(end) = find(&data[i..], 0, b">") {
                let hex = &data[i + 1..i + end];
                push_hex_string(hex, out);
                i += end + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn push_hex_string(hex: &[u8], out: &mut String) {
    let mut s = String::new();
    for ch in hex {
        if ch.is_ascii_hexdigit() {
            s.push(*ch as char);
        }
    }
    if s.len() >= 2 {
        let bytes: Vec<u8> = (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap_or(b'?'))
            .collect();
        push_lossy(&bytes, out);
    }
}

fn push_lossy(bytes: &[u8], out: &mut String) {
    out.push_str(&String::from_utf8_lossy(bytes));
    out.push(' ');
}

// ---------------------------- ZIP / Office ----------------------------

/// Office 文档本质是 zip，挑出含文本的 XML，剥标签。
fn extract_zip_document(bytes: &[u8]) -> Option<String> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(f) = archive.by_index(i) {
            let n = f.name().to_string();
            if is_text_xml(&n) {
                names.push(n);
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    let mut out = String::new();
    for n in names {
        if let Ok(mut f) = archive.by_name(&n) {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                out.push_str(&strip_xml_tags(&String::from_utf8_lossy(&buf)));
                out.push('\n');
            }
        }
        if out.len() > 200_000 {
            break;
        }
    }
    let t = clean_text(&out);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn is_text_xml(name: &str) -> bool {
    name.starts_with("word/document")
        || name.starts_with("word/header")
        || name.starts_with("word/footer")
        || name.starts_with("xl/sharedStrings")
        || name.starts_with("ppt/slides/slide")
        || name.starts_with("ppt/notesSlides/notesSlide")
        || name == "word/document.xml"
}

/// 剥 XML 标签，按块级元素插入换行（`</w:p>`、`</a:p>`、`</row>` 等）。
/// 注意：`tag` 里存的是 `<` 之后、`>` 之前的内容，所以结束标签形如 `/w:p`。
fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in xml.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                if t.starts_with("/w:p")
                    || t.starts_with("/a:p")
                    || t.starts_with("/row")
                    || t.starts_with("/w:tr")
                    || t.starts_with("/w:tc")
                    || t == "/w:tbl"
                {
                    out.push('\n');
                } else if t.starts_with("w:tab") || t.starts_with("w:br") {
                    out.push(' ');
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        out.push(ch);
    }
    out
}

// ---------------------------- 工具 ----------------------------

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut d = DeflateDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out).ok()?;
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// 合并空白、去掉控制字符。
/// 语义：连续空白只保留一个，且**保留原本的类型**（空格还是空格、换行还是换行），
/// 只是 tab 统一折成空格。
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_blank = false;
    for ch in s.chars() {
        if ch == '\u{feff}' || (ch.is_control() && ch != '\n' && ch != '\t' && ch != '\r') {
            continue;
        }
        match ch {
            '\n' | '\r' => {
                if !last_blank {
                    out.push('\n');
                    last_blank = true;
                }
            }
            '\t' | ' ' => {
                if !last_blank {
                    out.push(' ');
                    last_blank = true;
                }
            }
            _ => {
                last_blank = false;
                out.push(ch);
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_xml_and_breaks_lines() {
        let xml = r#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>第一行</w:t></w:r></w:p><w:p><w:r><w:t>第二行</w:t></w:r></w:p></w:body></w:document>"#;
        let t = strip_xml_tags(xml);
        assert!(t.contains("第一行"), "应取出正文: {}", t);
        assert!(t.contains("第二行"));
        assert!(t.contains('\n'), "段落之间应有换行");
        assert!(!t.contains("<w:"));
    }

    #[test]
    fn detects_kind_by_magic() {
        assert!(looks_like_pdf(b"%PDF-1.7\n..."));
        assert!(looks_like_zip(b"PK\x03\x04rest"));
        assert!(!looks_like_pdf(b"hello"));
    }

    #[test]
    fn pdf_literal_strings_are_extracted() {
        // 未压缩内容流：(Hello) Tj (World) Tj
        let stream = b"BT /F1 12 Tf (Hello) Tj 0 -14 Td (World) Tj ET";
        let mut out = String::new();
        pdf_strings_to_text(stream, &mut out);
        assert!(out.contains("Hello"), "{}", out);
        assert!(out.contains("World"), "{}", out);
    }

    #[test]
    fn pdf_octal_escapes_decode() {
        let stream = b"(A\\050B) Tj"; // \050 = '('
        let mut out = String::new();
        pdf_strings_to_text(stream, &mut out);
        assert!(out.contains("A(B"), "{}", out);
    }

    #[test]
    fn clean_text_collapses_whitespace() {
        let t = clean_text("a\r\n\r\n\r\nb   c\td");
        assert_eq!(t, "a\nb c d");
    }

    #[test]
    fn non_document_returns_none() {
        assert!(extract_text(b"plain bytes", "application/octet-stream", "x.bin").is_none());
    }
}

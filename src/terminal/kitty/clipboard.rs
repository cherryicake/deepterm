//! Kitty clipboard protocol（OSC 5522）的解析、写入事务与应答编码。
//!
//! 当前接入系统剪贴板的文本通道：
//! - `type=write` / `wdata` / `walias` 按 MIME 累积，提交时把 `text/plain`
//!   写入系统剪贴板；
//! - `type=read` 把系统剪贴板文本按 4096 字节分片回给客户端，支持 `.`
//!   targets 列表；
//! - `walias` 在事务内解析别名；不支持的 MIME 静默忽略（协议用“没有 DATA
//!   包”表示不可用）。
//!
//! 图片/富文本剪贴板与 Kitty 的权限 grant（OTP）仍需平台异步剪贴板 API
//! 与权限 UI，不在纯解析层伪造。

use std::collections::HashMap;

use super::command::encode_base64;

const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MIMES: usize = 64;
const READ_CHUNK: usize = 4096;

/// 一次剪贴板请求的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    None,
    Write {
        id: String,
        text: String,
    },
    Read {
        id: String,
        mimes: Vec<String>,
        list: bool,
        primary: bool,
    },
}

#[derive(Debug, Clone)]
struct MimeData {
    /// 已拼接的 base64 文本。
    encoded: String,
}

#[derive(Debug)]
struct WriteTxn {
    id: String,
    current_mime: Option<String>,
    entries: HashMap<String, MimeData>,
    aliases: HashMap<String, String>,
    bytes: usize,
}

impl Default for WriteTxn {
    fn default() -> Self {
        Self {
            id: String::new(),
            current_mime: None,
            entries: HashMap::new(),
            aliases: HashMap::new(),
            bytes: 0,
        }
    }
}

/// 每个终端一份的协议状态。
#[derive(Debug, Default)]
pub struct ClipboardState {
    write: Option<WriteTxn>,
    reply: Option<Vec<u8>>,
}

impl ClipboardState {
    /// 处理一条 OSC 5522 内容（不含 `5522;` 前缀）。
    pub fn handle(&mut self, content: &[u8]) -> Request {
        self.reply = None;
        let Some((meta_raw, payload)) = split_once(content, b';') else {
            return self.handle_meta(content, b"");
        };
        self.handle_meta(meta_raw, payload)
    }

    /// 取走协议错误应答（`EINVAL`/`EFBIG`）。成功写入不应答。
    pub fn take_reply(&mut self) -> Option<Vec<u8>> {
        self.reply.take()
    }

    fn handle_meta(&mut self, meta_raw: &[u8], payload: &[u8]) -> Request {
        let Some(meta) = parse_meta(meta_raw) else {
            return Request::None;
        };
        let Some(op) = meta.get("type").map(String::as_str) else {
            return Request::None;
        };

        match op {
            "write" => {
                let Some(mime) = meta.get("mime").cloned() else {
                    return Request::None;
                };
                let id = sanitize_id(meta.get("id").map(String::as_str).unwrap_or(""));
                let mut txn = WriteTxn {
                    id,
                    ..Default::default()
                };
                txn.current_mime = Some(mime.clone());
                txn.entries.insert(
                    mime,
                    MimeData {
                        encoded: String::new(),
                    },
                );
                self.write = Some(txn);
                self.append_payload(payload);
                Request::None
            }
            "wdata" => {
                if self.write.is_none() {
                    return Request::None;
                }
                let new_mime = meta.get("mime").cloned();
                if let Some(mime) = new_mime.as_ref() {
                    let txn = self.write.as_mut().expect("checked above");
                    if txn.entries.len() >= MAX_MIMES && !txn.entries.contains_key(mime) {
                        self.fail("EFBIG");
                        return Request::None;
                    }
                    txn.current_mime = Some(mime.clone());
                    txn.entries.entry(mime.clone()).or_insert_with(|| MimeData {
                        encoded: String::new(),
                    });
                }
                if new_mime.is_none() && !payload.is_empty() {
                    // 提交包必须没有 payload；带数据但无 mime 的包无法归属，
                    // 按 Kitty 的 wdata 语义视为 EINVAL。
                    self.fail("EINVAL");
                    return Request::None;
                }
                self.append_payload(payload);
                if new_mime.is_none() {
                    return self.commit_write();
                }
                Request::None
            }
            "walias" => {
                if self.write.is_none() {
                    return Request::None;
                }
                let Some(alias) = meta.get("mime").cloned() else {
                    self.fail("EINVAL");
                    return Request::None;
                };
                let Some(target) = decode_base64_strict(payload) else {
                    self.fail("EINVAL");
                    return Request::None;
                };
                let Ok(target) = String::from_utf8(target) else {
                    self.fail("EINVAL");
                    return Request::None;
                };
                let txn = self.write.as_mut().expect("checked above");
                if alias.len() + target.len() > 512 || txn.aliases.len() >= MAX_MIMES {
                    self.fail("EFBIG");
                    return Request::None;
                }
                txn.aliases.insert(alias, target);
                Request::None
            }
            "read" => {
                let mimes = meta
                    .get("mime")
                    .map(|m| parse_mime_list(m))
                    .unwrap_or_else(|| vec!["text/plain".to_string()]);
                Request::Read {
                    id: sanitize_id(meta.get("id").map(String::as_str).unwrap_or("")),
                    list: mimes.iter().any(|m| m == "."),
                    mimes,
                    primary: meta.get("loc").is_some_and(|l| l == "primary"),
                }
            }
            _ => Request::None,
        }
    }

    fn append_payload(&mut self, payload: &[u8]) {
        let Some(txn) = self.write.as_mut() else {
            return;
        };
        let Some(mime) = txn.current_mime.clone() else {
            return;
        };
        let entry = txn.entries.entry(mime).or_insert_with(|| MimeData {
            encoded: String::new(),
        });
        entry.encoded.push_str(&String::from_utf8_lossy(payload));
        txn.bytes = txn.bytes.saturating_add(payload.len());
        if txn.bytes > MAX_WRITE_BYTES {
            self.fail("EFBIG");
        }
    }

    fn commit_write(&mut self) -> Request {
        let Some(txn) = self.write.take() else {
            return Request::None;
        };
        // 优先 text/plain 及其 charset 变体；别名解析后同样生效。
        let mut chosen: Option<&str> = None;
        for mime in txn.entries.keys() {
            let resolved = txn.aliases.get(mime).map(String::as_str).unwrap_or(mime);
            if resolved == "text/plain" || resolved.starts_with("text/plain;") {
                chosen = Some(mime);
                break;
            }
        }
        let Some(mime) = chosen else {
            return Request::None;
        };
        let encoded = &txn.entries[mime].encoded;
        let Some(bytes) = decode_base64_strict(encoded.as_bytes()) else {
            self.fail("EINVAL");
            return Request::None;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            self.fail("EINVAL");
            return Request::None;
        };
        Request::Write { id: txn.id, text }
    }

    fn fail(&mut self, status: &str) {
        let id = self
            .write
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_default();
        self.write = None;
        self.reply = Some(encode_status("wdata", status, &id));
    }
}

/// 构造一次读请求的完整应答（OK + 可选 targets + DATA 分片 + DONE）。
///
/// `text` 是系统剪贴板文本；没有文本时只回 OK/DONE，不伪造空 DATA。
pub fn encode_read_response(
    id: &str,
    mimes: &[String],
    list: bool,
    primary: bool,
    text: Option<&str>,
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_packet(&mut out, "read", "OK", id, primary, None, None);
    if list {
        let available = if text.is_some() { "text/plain\n" } else { "" };
        encode_packet(
            &mut out,
            "read",
            "DATA",
            id,
            primary,
            Some("."),
            Some(available.as_bytes()),
        );
    }
    let wants_text = mimes.is_empty()
        || mimes
            .iter()
            .any(|m| m == "text/plain" || m.starts_with("text/plain;"));
    if let Some(text) = text.filter(|_| wants_text) {
        let data = text.as_bytes();
        for chunk in data.chunks(READ_CHUNK) {
            encode_packet(
                &mut out,
                "read",
                "DATA",
                id,
                primary,
                Some("text/plain"),
                Some(chunk),
            );
        }
    }
    encode_packet(&mut out, "read", "DONE", id, primary, None, None);
    out
}

fn encode_status(op: &str, status: &str, id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    encode_packet(&mut out, op, status, id, false, None, None);
    out
}

fn encode_packet(
    out: &mut Vec<u8>,
    op: &str,
    status: &str,
    id: &str,
    primary: bool,
    mime: Option<&str>,
    payload: Option<&[u8]>,
) {
    out.extend_from_slice(b"\x1b]5522;");
    out.extend_from_slice(format!("type={op}:status={status}").as_bytes());
    if primary {
        out.extend_from_slice(b":loc=primary");
    }
    if !id.is_empty() {
        out.extend_from_slice(format!(":id={id}").as_bytes());
    }
    if let Some(mime) = mime {
        out.extend_from_slice(format!(":mime={}", encode_base64(mime.as_bytes())).as_bytes());
    }
    if let Some(payload) = payload {
        out.push(b';');
        out.extend_from_slice(encode_base64(payload).as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
}

/// 严格 RFC 4648 解码：非法字符/错误填充一律失败。
///
/// 图形协议那边允许忽略换行等噪声（历史行为），剪贴板协议要求严格；
/// 不能复用宽松的 `decode_base64`，否则损坏数据会被静默解成空串。
fn decode_base64_strict(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    for (i, group) in input.chunks(4).enumerate() {
        for (j, &c) in group.iter().enumerate() {
            chunk[j] = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => 0,
                _ => return None,
            };
        }
        let pad = group.iter().filter(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != input.len() / 4) {
            return None;
        }
        if pad == 1 && group[3] != b'=' {
            return None;
        }
        if pad == 2 && (group[2] != b'=' || group[3] != b'=') {
            return None;
        }
        let n = 3usize.saturating_sub(pad);
        let acc = (u32::from(chunk[0]) << 18)
            | (u32::from(chunk[1]) << 12)
            | (u32::from(chunk[2]) << 6)
            | u32::from(chunk[3]);
        for k in 0..n {
            out.push((acc >> (16 - k * 8)) as u8);
        }
    }
    Some(out)
}

fn split_once(data: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let at = data.iter().position(|b| *b == sep)?;
    Some((&data[..at], &data[at + 1..]))
}

fn parse_meta(raw: &[u8]) -> Option<HashMap<String, String>> {
    let text = std::str::from_utf8(raw).ok()?;
    if text.is_empty() {
        return None;
    }
    let mut out = HashMap::new();
    for field in text.split(':') {
        let (key, value) = field.split_once('=')?;
        let value = match key {
            "mime" | "id" | "pw" | "password" | "name" => {
                String::from_utf8(decode_base64_strict(value.as_bytes())?).ok()?
            }
            _ => value.to_string(),
        };
        out.insert(key.to_string(), value);
    }
    Some(out)
}

fn parse_mime_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .take(MAX_MIMES)
        .collect()
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        encode_base64(s.as_bytes())
    }

    #[test]
    fn write_and_commit_text() {
        let mut state = ClipboardState::default();
        let req = state
            .handle(format!("type=write:mime={};{}", b64("text/plain"), b64("hello")).as_bytes());
        assert_eq!(req, Request::None);
        let req = state.handle(b"type=wdata;");
        assert_eq!(
            req,
            Request::Write {
                id: String::new(),
                text: "hello".into()
            }
        );
    }

    #[test]
    fn write_chunks_are_concatenated_before_decode() {
        let mut state = ClipboardState::default();
        let full = encode_base64(b"hello world");
        let (a, b) = full.split_at(4);
        state.handle(format!("type=write:mime={};{a}", b64("text/plain")).as_bytes());
        state.handle(format!("type=wdata:mime={};{b}", b64("text/plain")).as_bytes());
        let req = state.handle(b"type=wdata;");
        assert_eq!(
            req,
            Request::Write {
                id: String::new(),
                text: "hello world".into()
            }
        );
    }

    #[test]
    fn alias_resolves_text_mime() {
        let mut state = ClipboardState::default();
        state.handle(
            format!("type=write:mime={};{}", b64("text/plain"), b64("via-alias")).as_bytes(),
        );
        let req = state.handle(
            format!(
                "type=walias:mime={};{}",
                b64("text/plain"),
                b64("text/plain")
            )
            .as_bytes(),
        );
        assert_eq!(req, Request::None);
        let req = state.handle(b"type=wdata;");
        assert_eq!(
            req,
            Request::Write {
                id: String::new(),
                text: "via-alias".into()
            }
        );
    }

    #[test]
    fn invalid_base64_aborts_with_einval() {
        let mut state = ClipboardState::default();
        state.handle(format!("type=write:mime={};!!!!", b64("text/plain")).as_bytes());
        let req = state.handle(b"type=wdata;");
        assert_eq!(req, Request::None);
        let reply = state.take_reply().expect("应回 EINVAL");
        assert!(String::from_utf8_lossy(&reply).contains("status=EINVAL"));
    }

    #[test]
    fn read_response_chunks_and_lists() {
        let bytes = encode_read_response(
            "r1",
            &[".".into(), "text/plain".into()],
            true,
            false,
            Some("hello"),
        );
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("type=read:status=OK"));
        assert!(s.contains("status=DATA"));
        assert!(s.contains("status=DONE"));
        assert!(
            s.contains("bWltZQ==") || s.contains("Lg=="),
            "应带 mime=. 的 DATA"
        );
    }
}

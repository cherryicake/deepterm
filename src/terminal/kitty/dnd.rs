//! Kitty drag-and-drop 协议（OSC 72）的 drop-target 子集。
//!
//! 支持：
//! - `t=a` 注册 / `t=A` 注销（含分片 MIME 列表）
//! - `t=q` 能力查询应答
//! - 原生文件拖入时发送 `t=M` drop 事件（`text/uri-list` + `text/plain`）
//! - 客户端 `t=r:x=N` 请求数据，按 3072 字节 base64 分片回传
//! - `t=r:o=N` 结束 drop
//! - 客户端发起 drag-out（`t=o`/`t=P`）明确回 `EPERM`
//!
//! 未实现的是终端作为**拖拽源**以及 hover/accept 阶段的 OS 反馈——这需要
//! GPUI 的拖拽会话 API，与终端字节协议无关。

use super::command::encode_base64;

const MAX_ITEMS: usize = 16;
const MAX_MIME_LIST: usize = 1024 * 1024;
const DATA_CHUNK: usize = 3072;

/// 一项可被拖入终端应用的数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub mime: String,
    pub data: Vec<u8>,
}

/// 客户端请求后需要终端执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Concluded(u32),
}

/// 每个终端一份的 DnD 状态。
#[derive(Debug, Default)]
pub struct DndState {
    registered: bool,
    client_id: u32,
    mimes: Vec<String>,
    items: Vec<Item>,
    register_more: bool,
}

impl DndState {
    pub fn is_registered(&self) -> bool {
        self.registered && self.client_id != 0 || self.registered
    }

    /// 处理一条 OSC 72 内容（不含 `72;` 前缀）。返回需要回写给 PTY 的字节。
    pub fn handle(&mut self, content: &[u8]) -> (Vec<u8>, Action) {
        let Some((meta_raw, payload)) = split_once(content, b';') else {
            return self.handle_meta(content, b"");
        };
        self.handle_meta(meta_raw, payload)
    }

    fn handle_meta(&mut self, meta_raw: &[u8], payload: &[u8]) -> (Vec<u8>, Action) {
        let Some(meta) = parse_meta(meta_raw) else {
            return (Vec::new(), Action::None);
        };
        let Some(t) = meta.get("t").and_then(|v| v.as_bytes().first().copied()) else {
            return (Vec::new(), Action::None);
        };
        let client_id = meta.u32("i").unwrap_or(self.client_id);

        match t {
            b'a' => {
                if meta.u32("x") == Some(1) {
                    // 远程 drop machine ID：接受但忽略。
                    return (Vec::new(), Action::None);
                }
                let more = meta.u32("m") == Some(1);
                if !self.registered || !self.register_more {
                    self.mimes.clear();
                }
                if self.mimes.iter().map(|m| m.len() + 1).sum::<usize>() + payload.len()
                    > MAX_MIME_LIST
                {
                    return (Vec::new(), Action::None);
                }
                if !payload.is_empty() {
                    let text = String::from_utf8_lossy(payload);
                    self.mimes.extend(
                        text.split_ascii_whitespace()
                            .filter(|m| !m.is_empty())
                            .map(str::to_string),
                    );
                }
                self.client_id = client_id;
                self.register_more = more;
                if !more {
                    self.registered = true;
                }
                (Vec::new(), Action::None)
            }
            b'A' => {
                self.registered = false;
                self.items.clear();
                self.mimes.clear();
                self.client_id = client_id;
                (Vec::new(), Action::None)
            }
            b'q' => {
                let mut out = Vec::new();
                encode(&mut out, "t=q", client_id, None);
                (out, Action::None)
            }
            b'r' => self.data_request(&meta, client_id),
            b'o' | b'p' | b'P' => {
                let mut out = Vec::new();
                encode_error(
                    &mut out,
                    'E',
                    client_id,
                    None,
                    "EPERM",
                    "drag out is not supported by this terminal",
                );
                (out, Action::None)
            }
            // 终端从不发起 drag-out，忽略客户端对这些响应的回复。
            b'e' | b'E' | b'k' | b'M' | b'R' => (Vec::new(), Action::None),
            _ => (Vec::new(), Action::None),
        }
    }

    fn data_request(&mut self, meta: &Meta, client_id: u32) -> (Vec<u8>, Action) {
        let x = meta.i32("x").unwrap_or(0);
        let y = meta.i32("y").unwrap_or(0);
        let y_upper = meta.i32("Y").unwrap_or(0);

        // x=y=Y=0：客户端结束本次 drop，o 是实际执行的操作。
        if x == 0 && y == 0 && y_upper == 0 {
            let op = meta.u32("o").unwrap_or(0);
            let dropped = !self.items.is_empty();
            self.items.clear();
            return (
                Vec::new(),
                if dropped {
                    Action::Concluded(op)
                } else {
                    Action::None
                },
            );
        }

        // Y=0, y=0, x!=0：请求第 x 个 MIME 的数据（1-based）。
        if y == 0 && y_upper == 0 && x > 0 {
            let index = x as usize;
            if index > self.items.len() || index == 0 {
                let mut out = Vec::new();
                encode_error(
                    &mut out,
                    'R',
                    client_id,
                    Some(x as u32),
                    "ENOENT",
                    "drop data request index out of bounds",
                );
                return (out, Action::None);
            }
            let item = &self.items[index - 1];
            let mut out = Vec::new();
            for chunk in item.data.chunks(DATA_CHUNK) {
                encode(&mut out, &format!("t=r:x={x}"), client_id, Some(chunk));
            }
            // 空 DATA 包表示该项传输结束。
            encode(&mut out, &format!("t=r:x={x}"), client_id, Some(b""));
            return (out, Action::None);
        }

        // 远程 drop / 目录请求未启用。
        let mut out = Vec::new();
        encode_error(
            &mut out,
            'R',
            client_id,
            Some(x as u32),
            "EINVAL",
            "remote drop data is not supported",
        );
        (out, Action::None)
    }

    /// 原生文件拖入时调用。未注册或没有可提供的项时返回 `None`。
    pub fn begin_drop(
        &mut self,
        items: Vec<Item>,
        cell: (i32, i32),
        pixel: (i32, i32),
    ) -> Option<Vec<u8>> {
        if !self.registered || items.is_empty() {
            return None;
        }
        self.items = items.into_iter().take(MAX_ITEMS).collect();
        let mut mimes = String::new();
        for item in &self.items {
            mimes.push_str(&item.mime);
            mimes.push(' ');
        }
        let header = format!(
            "t=M:x={}:y={}:X={}:Y={}:o=1",
            cell.0, cell.1, pixel.0, pixel.1
        );
        let mut out = Vec::new();
        // MIME 列表按协议是 plain payload，不 base64。
        encode_plain(&mut out, &header, self.client_id, Some(mimes.as_bytes()));
        Some(out)
    }
}

#[derive(Debug, Default)]
struct Meta {
    entries: std::collections::HashMap<String, String>,
}

impl Meta {
    fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key)
    }
    fn u32(&self, key: &str) -> Option<u32> {
        self.get(key)?.parse().ok()
    }
    fn i32(&self, key: &str) -> Option<i32> {
        self.get(key)?.parse().ok()
    }
}

fn parse_meta(raw: &[u8]) -> Option<Meta> {
    let text = std::str::from_utf8(raw).ok()?;
    if text.is_empty() {
        return None;
    }
    let mut entries = std::collections::HashMap::new();
    for field in text.split(':') {
        let (key, value) = field.split_once('=')?;
        if key.len() != 1 {
            continue;
        }
        entries.insert(key.to_string(), value.to_string());
    }
    Some(Meta { entries })
}

fn split_once(data: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let at = data.iter().position(|b| *b == sep)?;
    Some((&data[..at], &data[at + 1..]))
}

fn encode_plain(out: &mut Vec<u8>, header: &str, client_id: u32, payload: Option<&[u8]>) {
    out.extend_from_slice(b"\x1b]72;");
    out.extend_from_slice(header.as_bytes());
    if client_id != 0 {
        out.extend_from_slice(format!(":i={client_id}").as_bytes());
    }
    if let Some(payload) = payload {
        out.push(b';');
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(b"\x1b\\");
}

fn encode(out: &mut Vec<u8>, header: &str, client_id: u32, payload: Option<&[u8]>) {
    out.extend_from_slice(b"\x1b]72;");
    out.extend_from_slice(header.as_bytes());
    if client_id != 0 {
        out.extend_from_slice(format!(":i={client_id}").as_bytes());
    }
    if let Some(payload) = payload.filter(|p| !p.is_empty()) {
        out.push(b';');
        out.extend_from_slice(encode_base64(payload).as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
}

fn encode_error(
    out: &mut Vec<u8>,
    kind: char,
    client_id: u32,
    x: Option<u32>,
    errno: &str,
    message: &str,
) {
    out.extend_from_slice(b"\x1b]72;");
    out.extend_from_slice(format!("t={kind}").as_bytes());
    if let Some(x) = x {
        out.extend_from_slice(format!(":x={x}").as_bytes());
    }
    if client_id != 0 {
        out.extend_from_slice(format!(":i={client_id}").as_bytes());
    }
    out.extend_from_slice(b";");
    out.extend_from_slice(format!("{errno}:{message}").as_bytes());
    out.extend_from_slice(b"\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        encode_base64(bytes)
    }

    #[test]
    fn register_then_drop_and_serve_uri_list() {
        let mut d = DndState::default();
        let (reply, _) = d.handle(format!("t=a:i=7;{}", "text/uri-list text/plain").as_bytes());
        assert!(reply.is_empty());
        assert!(d.is_registered());

        let out = d
            .begin_drop(
                vec![
                    Item {
                        mime: "text/uri-list".into(),
                        data: b"file:///tmp/a\r\n".to_vec(),
                    },
                    Item {
                        mime: "text/plain".into(),
                        data: b"/tmp/a\n".to_vec(),
                    },
                ],
                (3, 4),
                (40, 80),
            )
            .expect("已注册应发送 drop 事件");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("t=M:x=3:y=4:X=40:Y=80:o=1:i=7"));
        assert!(text.contains("text/uri-list text/plain "));

        let (reply, action) = d.handle(b"t=r:x=1:i=7");
        assert_eq!(action, Action::None);
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("t=r:x=1:i=7;"));
        assert!(text.contains(&b64(b"file:///tmp/a\r\n")));
        // 末尾空 DATA 包。
        assert!(text.ends_with("\x1b]72;t=r:x=1:i=7\x1b\\"));

        let (_, action) = d.handle(b"t=r:o=1:i=7");
        assert_eq!(action, Action::Concluded(1));
        assert!(!d.begin_drop(Vec::new(), (0, 0), (0, 0)).is_some());
    }

    #[test]
    fn query_is_answered_with_client_id() {
        let mut d = DndState::default();
        let (reply, _) = d.handle(b"t=q:i=9");
        assert_eq!(String::from_utf8_lossy(&reply), "\x1b]72;t=q:i=9\x1b\\");
    }

    #[test]
    fn unregister_stops_drops() {
        let mut d = DndState::default();
        d.handle(b"t=a:i=1;text/plain");
        assert!(d.is_registered());
        d.handle(b"t=A:i=1");
        assert!(!d.is_registered());
        assert!(
            d.begin_drop(
                vec![Item {
                    mime: "text/plain".into(),
                    data: b"x".to_vec()
                }],
                (0, 0),
                (0, 0)
            )
            .is_none()
        );
    }

    #[test]
    fn out_of_bounds_request_reports_enoent() {
        let mut d = DndState::default();
        d.handle(b"t=a:i=1;text/plain");
        let (reply, _) = d.handle(b"t=r:x=1:i=1");
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("t=R:x=1:i=1;ENOENT"));
    }

    #[test]
    fn drag_out_is_refused() {
        let mut d = DndState::default();
        let (reply, _) = d.handle(b"t=o:x=0:i=3");
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("t=E:i=3;EPERM"));
    }
}

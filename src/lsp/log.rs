//! 语言服务器的 JSON-RPC 报文日志（View Logs 的数据源）。
//!
//! 对照 Zed 的 `project/src/lsp_store/log_store.rs`：它用一个全局 `LogStore`
//! 按服务器分桶记录收发的每一帧，再由 `language_tools/src/lsp_log_view.rs`
//! 渲染成可筛选的面板。这里按本项目的体量收敛成同一个形状——有界环形缓冲 +
//! 一个只读面板。
//!
//! **为什么值得记**：LSP 的故障几乎都不在客户端这一侧，而是「服务器从没收到
//! 这个请求」或「回了个我们没处理的方法」。没有报文日志时，用户能看到的现象
//! 只是「补全不出来」，无从判断是进程没起来、握手失败、还是请求发丢了。
//!
//! 代价：每帧多一次字符串截断 + 一次 `VecDeque` 推入。`didChange` 在快速输入
//! 时会比较密，所以 [`MAX_MESSAGES`] 必须有界，且预览统一截断——否则长时间
//! 编辑会把内存吃光。
//!
//! **为什么是进程级静态而不是 gpui 全局**：记录点全在后台——`read_loop` 是
//! `smol::spawn` 出去的独立任务，请求发送也在后台 future 里。gpui 的全局
//! 只在主线程可见，从那些地方 `try_global` 拿不到东西。日志又必须跨线程写、
//! 主线程读，所以用最朴素的 `OnceLock<Mutex<..>>`。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use crate::lsp::project::ServerKey;

/// 每个服务器保留的报文条数上限。
///
/// 2000 条足够覆盖「打开一个文件 → 触发一次诊断」的完整往返，又不至于让
/// 视图滚动起来吃力。
pub const MAX_MESSAGES: usize = 2000;

/// 兜底 JSON 预览的最大字符数。
///
/// 只有在 [`summarize`] 认不出来的方法上才会走到这里；认得出的那些早就被
/// 精简成文件名/条数了。仍然要有界：`didChange` 的负载是整个文件，全量存
/// 下来等于把工作区复制一遍。
const MAX_JSON_CHARS: usize = 120;

/// 全部服务器的日志表。
fn logs() -> &'static Mutex<HashMap<ServerKey, ServerLog>> {
    static LOGS: OnceLock<Mutex<HashMap<ServerKey, ServerLog>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 报文的流向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 我们发给服务器。
    Outgoing,
    /// 服务器发给我们的。
    Incoming,
}

/// 报文的种类（JSON-RPC 的三类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Notification,
}

/// 一条日志记录。
#[derive(Debug, Clone)]
pub struct LogMessage {
    pub direction: Direction,
    pub kind: MessageKind,
    /// 方法名。响应里没有方法（JSON-RPC 的响应只带 id），视图上用 id 反查。
    pub method: String,
    /// 请求/响应的 id；通知没有。
    pub id: Option<i64>,
    /// 精简后的关键信息（见 [`summarize`]）。
    ///
    /// 刻意**不是**原始 JSON：`didChange` 的负载是整个文件，`publishDiagnostics`
    /// 能有几百条诊断——原样存下来一屏只能看一帧，日志就白记了。这里只留
    /// 「人在排障时真正会看的」：哪个文件、第几版、几条。
    pub summary: String,
}

impl LogMessage {
    /// 方向箭头（→ 我们发出，← 收到）。
    pub fn arrow(&self) -> &'static str {
        match self.direction {
            Direction::Outgoing => "→",
            Direction::Incoming => "←",
        }
    }

    /// 视图里的方法列：通知/请求用方法名，响应用 id。
    pub fn label(&self) -> String {
        match self.kind {
            MessageKind::Request | MessageKind::Notification => self.method.clone(),
            MessageKind::Response => match self.id {
                Some(id) => format!("#{id}"),
                None => "response".to_string(),
            },
        }
    }
}

/// 单个服务器的日志。
#[derive(Default)]
pub struct ServerLog {
    messages: VecDeque<LogMessage>,
}

impl ServerLog {
    /// 记一条，超出上限时丢最旧的。
    fn push(&mut self, message: LogMessage) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(message);
    }
}

/// 日志接收端：`Connection` 只认这个 trait，不知道存储长什么样。
///
/// 跨线程（后台读循环 → 主线程 UI），所以要求 `Send + Sync`。
pub trait LogSink: Send + Sync {
    fn record(&self, message: LogMessage);
}

/// 把报文归到某个 key 名下的 sink。
pub struct KeyedSink {
    key: ServerKey,
}

impl LogSink for KeyedSink {
    fn record(&self, message: LogMessage) {
        // 锁中毒（另一线程 push 时 panic）不该把发送路径带崩：
        // `if let Ok` 静默丢弃这一次记录。
        if let Ok(mut servers) = logs().lock() {
            servers.entry(self.key.clone()).or_default().push(message);
        }
    }
}

/// 取一个把报文归到 `key` 名下的 sink。
///
/// 按需建桶：没有该服务器的日志时（还没发过任何消息）不预建条目，
/// 免得表里堆满从未通信过的 key。
pub fn sink_for(key: &ServerKey) -> std::sync::Arc<dyn LogSink> {
    std::sync::Arc::new(KeyedSink { key: key.clone() })
}

/// 某服务器的全部报文（按时间正序）。
pub fn messages(key: &ServerKey) -> Vec<LogMessage> {
    logs()
        .lock()
        .ok()
        .and_then(|servers| {
            servers
                .get(key)
                .map(|log| log.messages.iter().cloned().collect())
        })
        .unwrap_or_default()
}

/// 清空某服务器的日志。
pub fn clear(key: &ServerKey) {
    if let Ok(mut servers) = logs().lock() {
        servers.entry(key.clone()).or_default().messages.clear();
    }
}

/// 构造一条记录（收敛构造点，统一走 [`summarize`] 精简）。
pub fn message(
    direction: Direction,
    kind: MessageKind,
    method: impl Into<String>,
    id: Option<i64>,
    payload: Option<&serde_json::Value>,
) -> LogMessage {
    let method = method.into();
    LogMessage {
        summary: summarize(kind, &method, payload),
        direction,
        kind,
        method,
        id,
    }
}

/// 负载 → 一行精简信息。
///
/// 按方法挑「人真正会看的字段」：
/// - 带 `textDocument.uri` 的 → **文件名**（不是全路径：项目根前缀占了
///   大半个字宽还每次都一样，扫列表时纯噪音）
/// - `didChange` → 版本号 + 字节数
/// - `publishDiagnostics` → 条数（几百条诊断没人会逐条看，数量才是信号）
/// - `$/progress` → 阶段 + 百分比（对应状态栏那盏灯）
/// - 出错响应 → 错误消息原文（`-32801 content modified` 这种正是排障目标）
/// - 其余 → 紧凑 JSON，截短
///
/// `method` 对**响应**为空串（JSON-RPC 的响应不带方法），那时只判断出错与否。
fn summarize(kind: MessageKind, method: &str, payload: Option<&serde_json::Value>) -> String {
    let Some(value) = payload else {
        return String::new();
    };
    if kind == MessageKind::Response {
        if let Some(message) = value.get("error").and_then(|e| e.as_str()) {
            return format!("error: {message}");
        }
        return compact(value, MAX_JSON_CHARS);
    }

    let params = value.get("params").unwrap_or(value);

    if let Some(uri) = json_uri(params) {
        // 只留文件名：`/Users/…/project/src/foo.rs` 里每一帧都一样的部分
        // 正是最该省掉的。
        let file = uri.rsplit('/').next().unwrap_or(uri);
        return match method {
            "textDocument/didChange" => {
                let version = params
                    .get("textDocument")
                    .and_then(|d| d.get("version"))
                    .and_then(|v| v.as_i64());
                let bytes = params
                    .get("contentChanges")
                    .and_then(|c| c.as_array())
                    .and_then(|changes| {
                        changes
                            .first()
                            .and_then(|c| c.get("text"))
                            .and_then(|t| t.as_str())
                    })
                    .map(|t| t.len())
                    .unwrap_or(0);
                match version {
                    Some(v) => format!("{file} · v{v} · {bytes} B"),
                    None => file.to_string(),
                }
            }
            "textDocument/publishDiagnostics" => {
                let n = params
                    .get("diagnostics")
                    .and_then(|d| d.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("{file} · {n} 条")
            }
            _ => file.to_string(),
        };
    }

    match method {
        "$/progress" => {
            let Some(value) = params.get("value") else {
                return compact(params, MAX_JSON_CHARS);
            };
            let title = value.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let pct = value
                .get("percentage")
                .and_then(|p| p.as_u64())
                .map(|p| format!("{p}%"))
                .unwrap_or_default();
            let line = format!("{title} {pct}");
            if line.trim().is_empty() {
                compact(params, MAX_JSON_CHARS)
            } else {
                line
            }
        }
        // 这两个通知的负载是路径/glob 列表，长而没有信息量。
        "workspace/didChangeWatchedFiles" | "$/cancelRequest" => compact(params, 64),
        // 握手那一大坨 capabilities 没人看得到完。
        "initialize" => "握手".to_string(),
        _ => compact(params, MAX_JSON_CHARS),
    }
}

/// 从 params 里取文档 uri（`params.textDocument.uri` 或 `params.uri`）。
fn json_uri(params: &serde_json::Value) -> Option<&str> {
    params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .or_else(|| params.get("uri"))
        .and_then(|u| u.as_str())
}

/// 紧凑 JSON，截到 `max` 个字符。
fn compact(value: &serde_json::Value, max: usize) -> String {
    // `to_string` 出的是紧凑 JSON：没有换行与多余空格，一行放得下。
    let raw = value.to_string();
    if raw.chars().count() <= max {
        return raw;
    }
    // 按字符截断再拼省略号：直接按字节切会把多字节字符劈开。
    let kept: String = raw.chars().take(max).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(lang: &str) -> ServerKey {
        ServerKey {
            root: std::path::PathBuf::from("/tmp/log-test"),
            language: lang.to_string(),
        }
    }

    #[test]
    fn appends_and_trims_in_order() {
        let k = key("trim");
        clear(&k);
        let sink = sink_for(&k);
        for i in 0..(MAX_MESSAGES + 10) {
            sink.record(message(
                Direction::Outgoing,
                MessageKind::Request,
                format!("m{i}"),
                Some(i as i64),
                None,
            ));
        }
        let all = messages(&k);
        assert_eq!(all.len(), MAX_MESSAGES);
        // 最旧的被丢掉。
        assert_eq!(all.first().unwrap().method, "m10");
        // 剩下的正是最后 MAX_MESSAGES 条，且顺序与进入时一致。
        let methods: Vec<&str> = all.iter().map(|m| m.method.as_str()).collect();
        let expected: Vec<String> = (10..10 + MAX_MESSAGES).map(|i| format!("m{i}")).collect();
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        assert_eq!(methods, expected);
        clear(&k);
    }

    /// `publishDiagnostics` 只报条数：几百条诊断没人逐条看，数量才是信号。
    #[test]
    fn publish_diagnostics_summary_counts_items() {
        let msg = message(
            Direction::Incoming,
            MessageKind::Notification,
            "textDocument/publishDiagnostics",
            None,
            Some(&serde_json::json!({
                "uri": "file:///w/notes.txt",
                "diagnostics": [{ "message": "unused" }, { "message": "deprecated" }],
            })),
        );
        assert_eq!(msg.summary, "notes.txt · 2 条");
    }

    /// `didChange` 精简成「文件名 · 版本 · 字节数」。
    ///
    /// 原样存 JSON 的话，一帧就是整个文件——日志一屏只能看一帧，等于白记。
    #[test]
    fn did_change_summary_is_compact() {
        let msg = message(
            Direction::Outgoing,
            MessageKind::Notification,
            "textDocument/didChange",
            None,
            Some(&serde_json::json!({
                "textDocument": { "uri": "file:///w/src/main.rs", "version": 7 },
                "contentChanges": [{ "text": "fn main() {}" }],
            })),
        );
        assert_eq!(msg.summary, "main.rs · v7 · 12 B");
    }

    /// 带 uri 的通知只留文件名：项目根前缀每帧都一样，是纯噪音。
    #[test]
    fn uri_summary_keeps_only_the_file_name() {
        let msg = message(
            Direction::Outgoing,
            MessageKind::Notification,
            "textDocument/didOpen",
            None,
            Some(&serde_json::json!({
                "textDocument": { "uri": "file:///very/long/project/path/src/foo.rs" },
            })),
        );
        assert_eq!(msg.summary, "foo.rs");
    }

    /// 出错的响应：错误原文就是排障目标，一个字符都不能省。
    #[test]
    fn errored_response_shows_the_message_verbatim() {
        let msg = message(
            Direction::Incoming,
            MessageKind::Response,
            "",
            Some(3),
            Some(&serde_json::json!({ "error": "content modified" })),
        );
        assert_eq!(msg.summary, "error: content modified");
    }

    /// `$/progress` 对应状态栏那盏灯，阶段 + 百分比是有效信息。
    #[test]
    fn progress_summary_shows_title_and_percentage() {
        let msg = message(
            Direction::Incoming,
            MessageKind::Notification,
            "$/progress",
            None,
            Some(&serde_json::json!({
                "token": "indexing",
                "value": { "kind": "report", "title": "Indexing", "percentage": 42 },
            })),
        );
        assert_eq!(msg.summary, "Indexing 42%");
    }

    /// 兜底 JSON 仍有界：认不出来的方法不能把一帧撑成整个文件。
    #[test]
    fn unknown_method_falls_back_to_truncated_json() {
        let long = "中".repeat(5000);
        let msg = message(
            Direction::Incoming,
            MessageKind::Notification,
            "some/unknownMethod",
            None,
            Some(&serde_json::json!({ "blob": long })),
        );
        assert_eq!(msg.summary.chars().count(), MAX_JSON_CHARS + 1);
        assert!(msg.summary.ends_with('…'));
    }

    #[test]
    fn short_summary_is_kept_verbatim() {
        let msg = message(
            Direction::Outgoing,
            MessageKind::Notification,
            "initialized",
            None,
            Some(&serde_json::json!({})),
        );
        assert_eq!(msg.summary, "{}");
    }

    #[test]
    fn arrow_and_label_distinguish_the_three_kinds() {
        let request = message(
            Direction::Outgoing,
            MessageKind::Request,
            "textDocument/hover",
            Some(3),
            None,
        );
        assert_eq!(request.arrow(), "→");
        assert_eq!(request.label(), "textDocument/hover");

        let response = message(
            Direction::Incoming,
            MessageKind::Response,
            "",
            Some(3),
            None,
        );
        assert_eq!(response.arrow(), "←");
        assert_eq!(response.label(), "#3");

        let notification = message(
            Direction::Incoming,
            MessageKind::Notification,
            "$/progress",
            None,
            None,
        );
        assert_eq!(notification.arrow(), "←");
        assert_eq!(notification.label(), "$/progress");
    }

    /// 按 key 分桶：两个服务器的日志不能混。
    #[test]
    fn messages_are_bucketed_per_server() {
        let (a, b) = (key("bucket-rust"), key("bucket-go"));
        clear(&a);
        clear(&b);
        sink_for(&a).record(message(
            Direction::Outgoing,
            MessageKind::Request,
            "a-method",
            None,
            None,
        ));
        assert_eq!(messages(&a).len(), 1);
        assert_eq!(messages(&b).len(), 0);
        assert_eq!(messages(&a)[0].method, "a-method");
        clear(&a);
    }
}

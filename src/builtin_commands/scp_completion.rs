//! `/scp` 命令专用补全引擎。
//!
//! `/scp` 有多个参数（除最后一个目的外，其余都是源），每个参数可能是：
//! - 本地路径（无 `:`）
//! - `host:path`：host 可未填完（列出已保存主机），path 端可能是远端目录
//!
//! 本模块负责：
//! 1. 解析当前光标所在 token（第 1 / 第 2 个参数），以及该 token 在缓冲区的
//!    起止位置 —— 用于替换时精确保留另一侧参数。
//! 2. 判断光标侧是哪一阶段：
//!    - `HostList`：用户还在打 host 前缀（没出现 `:`），列出匹配的保存主机。
//!    - `RemotePath { host, prefix }`：已有 `:`，光标在 path 前缀上，需要远端目录。
//!    - `LocalPath { prefix }`：token 没有 `:`，按本地路径补全。
//! 3. 给出替换文本：补全命中后，用新的 token 拼回原缓冲区。

use crate::assets::Icon;
use crate::ssh::SshConnectionConfig;
use crate::ssh::platform::{RemoteOS, detect_local_os};

/// 一次 shell 风格 token 的解析结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellToken {
    /// token 在原缓冲区中的字节起点（含引号/反斜杠原样）
    pub start: usize,
    /// token 在原缓冲区中的字节终点（exclusive）
    pub end: usize,
    /// 去除引号/反斜杠转义后的逻辑内容
    pub value: String,
}

/// 按 shell 风格切分 `/scp` 参数区。
///
/// 支持：
/// - 双引号 `"..."`（内部允许空格）
/// - 单引号 `'...'`（内部原样）
/// - 反斜杠转义 `\ `（空格被保留进 token）
///
/// 输入的 `area` 应当是"/scp " 之后的参数区（无需裁剪末尾空白）。
/// `base_offset` 用于把 token 偏移翻译回完整 buffer 的绝对位置。
///
/// 实现说明：必须按 **字符**（char）而非字节迭代，否则 UTF-8 多字节序列的
/// 中间字节会被当作独立 char（Latin-1 解释），破坏中文等非 ASCII 文本。
pub fn shell_split_tokens(area: &str, base_offset: usize) -> Vec<ShellToken> {
    let chars: Vec<(usize, char)> = area.char_indices().collect();
    let total = chars.len();
    let area_len = area.len();
    let mut tokens: Vec<ShellToken> = Vec::with_capacity(8);
    let mut idx = 0;
    while idx < total {
        // 跳过空白
        while idx < total && chars[idx].1.is_whitespace() {
            idx += 1;
        }
        if idx >= total {
            break;
        }
        let token_byte_start = chars[idx].0;
        let mut value = String::new();
        let mut mode = QuoteMode::None;
        while idx < total {
            let c = chars[idx].1;
            match mode {
                QuoteMode::None => {
                    if c.is_whitespace() {
                        break;
                    }
                    match c {
                        '"' => {
                            mode = QuoteMode::Double;
                            idx += 1;
                        }
                        '\'' => {
                            mode = QuoteMode::Single;
                            idx += 1;
                        }
                        '\\' if idx + 1 < total => {
                            // `\<any>` 保留下一个字符原样（主要用于 `\ `）
                            value.push(chars[idx + 1].1);
                            idx += 2;
                        }
                        _ => {
                            value.push(c);
                            idx += 1;
                        }
                    }
                }
                QuoteMode::Double => match c {
                    '"' => {
                        mode = QuoteMode::None;
                        idx += 1;
                    }
                    '\\' if idx + 1 < total => {
                        value.push(chars[idx + 1].1);
                        idx += 2;
                    }
                    _ => {
                        value.push(c);
                        idx += 1;
                    }
                },
                QuoteMode::Single => match c {
                    '\'' => {
                        mode = QuoteMode::None;
                        idx += 1;
                    }
                    _ => {
                        value.push(c);
                        idx += 1;
                    }
                },
            }
        }
        let token_byte_end = if idx < total { chars[idx].0 } else { area_len };
        tokens.push(ShellToken {
            start: base_offset + token_byte_start,
            end: base_offset + token_byte_end,
            value,
        });
    }
    tokens
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuoteMode {
    None,
    Single,
    Double,
}

/// 判断原始 token（未去引号）的第一个"真正的冒号"位置。
///
/// 真正的冒号：不处于引号内、也不被反斜杠转义。返回相对 token 的**字节**索引。
pub fn first_unquoted_colon(raw_token: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = raw_token.char_indices().collect();
    let total = chars.len();
    let mut idx = 0;
    let mut mode = QuoteMode::None;
    while idx < total {
        let (byte_pos, c) = chars[idx];
        match mode {
            QuoteMode::None => match c {
                ':' => return Some(byte_pos),
                '"' => {
                    mode = QuoteMode::Double;
                    idx += 1;
                }
                '\'' => {
                    mode = QuoteMode::Single;
                    idx += 1;
                }
                '\\' if idx + 1 < total => {
                    // 反斜杠转义：跳过反斜杠本身，保留下一个字符原样。
                    // 关键：如果下一个字符是 ':'，那这个 ':' 是被转义的，
                    // 不应被视为 host/path 分隔符。
                    idx += 2;
                }
                _ => idx += 1,
            },
            QuoteMode::Double => match c {
                '"' => {
                    mode = QuoteMode::None;
                    idx += 1;
                }
                '\\' if idx + 1 < total => idx += 2,
                _ => idx += 1,
            },
            QuoteMode::Single => match c {
                '\'' => {
                    mode = QuoteMode::None;
                    idx += 1;
                }
                _ => idx += 1,
            },
        }
    }
    None
}

/// 把纯文本按 shell 安全规则转义为一个 token。
///
/// - 为空 → `""`
/// - 含空格或特殊字符 → 用双引号包裹，内部 `"` / `\` 做反斜杠转义
/// - 否则原样
pub fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quote = text.chars().any(|c| matches!(c, ' ' | '\t' | '"' | '\''));
    if !needs_quote {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `/scp` 一次补全请求的上下文。
#[derive(Clone, Debug, PartialEq)]
pub struct ScpCompletionQuery {
    /// 当前 token 在 `buffer` 中的绝对字节起点（用于替换）
    pub token_start: usize,
    /// 当前 token 在 `buffer` 中的绝对字节终点（exclusive）
    pub token_end: usize,
    /// 补全类别
    pub phase: ScpCompletionPhase,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScpCompletionPhase {
    /// 没有 `:` 的 token：既可能是本地路径，也可能是正在输入 host 名
    HostOrLocal { prefix: String },
    /// `host:<prefix>` —— host 已解析成功
    RemotePath { host_token: String, prefix: String },
}

/// 分析 `/scp` 缓冲区的当前补全焦点（光标默认在末尾）。
///
/// `buffer` 应当以 `/scp` 开头，后面跟 token。
/// 返回 `None` 表示还不在可补全的位置（比如光标还停在命令名区域内）。
/// 多源时除最后一个 token 为目的外，其余 token 都是源 —— 每个 token 的补全规则
/// 相同（本地路径 / 主机名 / `host:远端路径`），故任意 token 位置都可补全。
pub fn analyse(buffer: &str) -> Option<ScpCompletionQuery> {
    analyse_at(buffer, buffer.len())
}

/// 与 [`analyse`] 相同，但显式给出光标字节偏移：补全焦点落在**光标所在的 token**，
/// 而不是缓冲末尾的 token。这样当用户把光标移回前一个参数（如本地源路径）时，
/// 弹窗会列出该参数对应的候选，而不是末尾参数（如远端目的）。
///
/// `cursor` 超出 `buffer` 长度时夹紧到末尾；光标落在命令名区域内时返回 `None`。
pub fn analyse_at(buffer: &str, cursor: usize) -> Option<ScpCompletionQuery> {
    let cursor = cursor.min(buffer.len());

    // 找到第一个空白作为命令名结束
    let after_cmd_offset = {
        let mut iter = buffer.char_indices();
        loop {
            match iter.next() {
                Some((i, c)) if c.is_whitespace() => break i + c.len_utf8(),
                Some(_) => {}
                None => return None,
            }
        }
    };

    // 光标仍在命令名区域（如 `/sc|p `）时不做路径补全
    if cursor < after_cmd_offset {
        return None;
    }

    let args_area = &buffer[after_cmd_offset..];
    let tokens = shell_split_tokens(args_area, after_cmd_offset);

    // 找到光标所在 token：`start <= cursor <= end`。
    // token 之间至少隔一个空白，故 `cursor == end(前)` 与 `cursor == start(后)`
    // 不会同时命中两个 token（区间不重叠）。
    let cursor_token = tokens.iter().find(|t| cursor >= t.start && cursor <= t.end);

    let (token_start, token_end, raw_token): (usize, usize, &str) = match cursor_token {
        Some(t) => (t.start, t.end, &buffer[t.start..t.end]),
        None => {
            // 光标落在空白 / 空位：当前位置是一个空 token。
            // 已有两个完整 token 后不再自动弹候选（多源时用户敲入新 token
            // 的首字符即会触发该 token 的补全）。
            if tokens.len() >= 2 {
                return None;
            }
            (cursor, cursor, "")
        }
    };

    // 用 "未去引号的冒号位置" 判断是否已经进入 RemotePath
    let phase = match first_unquoted_colon(raw_token) {
        Some(pos) if pos > 0 => {
            // 把 host 部分与 path 部分分别再做一次 shell 展开
            let host_raw = &raw_token[..pos];
            let path_raw = &raw_token[pos + 1..];
            ScpCompletionPhase::RemotePath {
                host_token: unquote_raw(host_raw),
                prefix: unquote_raw(path_raw),
            }
        }
        _ => ScpCompletionPhase::HostOrLocal {
            prefix: unquote_raw(raw_token),
        },
    };

    Some(ScpCompletionQuery {
        token_start,
        token_end,
        phase,
    })
}

/// 把 raw token 做一次 shell 展开（丢掉引号/反斜杠修饰）。
pub fn unquote_raw(raw: &str) -> String {
    // 用相同的分词器处理单个 token
    shell_split_tokens(raw, 0)
        .into_iter()
        .next()
        .map(|t| t.value)
        .unwrap_or_default()
}

/// 判断 host token 是否是保留关键字 `Local`（不区分大小写），
/// 或者补全后的标准形态 `xxx@localhost` / `xxx@127.0.0.1`。
pub fn is_local_keyword_str(token: &str) -> bool {
    if token.eq_ignore_ascii_case("local") {
        return true;
    }
    if let Some((_, host_part)) = token.split_once('@') {
        let host_lc = host_part.to_lowercase();
        return host_lc == "localhost" || host_lc == "127.0.0.1" || host_lc == "::1";
    }
    false
}

/// 按 host 子串匹配已保存连接 + 保留关键字 `Local`。
///
/// 匹配规则（大小写不敏感）：
/// 0. 关键字 `Local`（始终指向本机）
/// 1. nickname 子串
/// 2. host 子串
/// 3. host_label 子串（`user@host[:port]`）
///
/// 输出统一格式：
/// - `display`：匹配源（用于次要文字，例如保留 nickname 让用户认得）
/// - `replacement`：写到命令行的 host 部分，统一为 `user@host[:port]`
///   （`Local` 写成 `username@localhost`）。
pub fn match_hosts_by_prefix<'a>(
    prefix: &str,
    saved: &'a [SshConnectionConfig],
) -> Vec<ScpHostSuggestion<'a>> {
    let prefix_lc = prefix.to_lowercase();
    let mut out: Vec<ScpHostSuggestion<'a>> = Vec::new();
    let mut pushed_ids = std::collections::HashSet::new();

    // 0) 保留关键字 `Local`：始终排在最前，使用本机 OS 图标
    let local_replacement = local_replacement_token();
    if "local".contains(&prefix_lc) || local_replacement.to_lowercase().contains(&prefix_lc) {
        out.push(ScpHostSuggestion {
            config: None,
            display: "Local".to_string(),
            replacement: local_replacement,
            kind: HostSuggestionKind::Local,
            icon: Some(detect_local_os().icon()),
        });
    }

    // 1) nickname 子串
    for cfg in saved {
        if let Some(nick) = cfg.nickname.as_deref() {
            if nick.to_lowercase().contains(&prefix_lc) && pushed_ids.insert(cfg.id()) {
                out.push(ScpHostSuggestion {
                    config: Some(cfg),
                    display: nick.to_string(),
                    replacement: cfg.host_label(),
                    kind: HostSuggestionKind::Nickname,
                    icon: icon_for_config(cfg),
                });
            }
        }
    }

    // 2) host 字段子串
    for cfg in saved {
        if cfg.host.to_lowercase().contains(&prefix_lc) && pushed_ids.insert(cfg.id()) {
            out.push(ScpHostSuggestion {
                config: Some(cfg),
                display: cfg.host.clone(),
                replacement: cfg.host_label(),
                kind: HostSuggestionKind::Host,
                icon: icon_for_config(cfg),
            });
        }
    }

    // 3) host_label：`user@host[:port]` —— 包含带端口主机
    for cfg in saved {
        let label = cfg.host_label();
        if label.to_lowercase().contains(&prefix_lc) && pushed_ids.insert(cfg.id()) {
            out.push(ScpHostSuggestion {
                config: Some(cfg),
                display: label.clone(),
                replacement: label,
                kind: HostSuggestionKind::Label,
                icon: icon_for_config(cfg),
            });
        }
    }

    out
}

/// 根据已保存连接的 `os` 字段解析对应图标；未配置 OS 时返回 `None`。
fn icon_for_config(cfg: &SshConnectionConfig) -> Option<Icon> {
    cfg.os.as_deref().map(RemoteOS::from_id).map(RemoteOS::icon)
}

/// 关键字 `Local` 在命令行中实际用的字面量：`<当前用户>@localhost`。
pub fn local_replacement_token() -> String {
    let user = crate::ssh::get_current_username().unwrap_or_else(|| "user".to_string());
    format!("{}@localhost", user)
}

#[derive(Clone, Debug)]
pub struct ScpHostSuggestion<'a> {
    /// `Some` 表示已保存的远端主机；`None` 表示保留关键字 `Local`
    pub config: Option<&'a SshConnectionConfig>,
    /// UI 显示的匹配名（命中的字段，如 nickname）
    pub display: String,
    /// 写入命令行的 host 部分，统一为 `user@host[:port]`；`Local` 为 `user@localhost`
    pub replacement: String,
    pub kind: HostSuggestionKind,
    /// 操作系统图标：Local 使用本机 OS 图标；其他来自已保存连接的 `os` 字段
    pub icon: Option<Icon>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSuggestionKind {
    /// 运行 DeepTerm 的本机
    Local,
    Nickname,
    Host,
    Label,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyse_empty_args_returns_empty_host_or_local() {
        let q = analyse("/scp ").unwrap();
        if let ScpCompletionPhase::HostOrLocal { prefix } = q.phase {
            assert_eq!(prefix, "");
        } else {
            panic!("expected HostOrLocal");
        }
        assert_eq!(q.token_start, 5);
        assert_eq!(q.token_end, 5);
    }

    #[test]
    fn analyse_host_prefix() {
        let q = analyse("/scp ali").unwrap();
        if let ScpCompletionPhase::HostOrLocal { prefix } = q.phase {
            assert_eq!(prefix, "ali");
        } else {
            panic!("expected HostOrLocal");
        }
        assert_eq!(q.token_start, 5);
        assert_eq!(q.token_end, 8);
    }

    #[test]
    fn analyse_remote_path_prefix() {
        let q = analyse("/scp alice:/var/lo").unwrap();
        match &q.phase {
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                assert_eq!(host_token, "alice");
                assert_eq!(prefix, "/var/lo");
            }
            _ => panic!("expected RemotePath"),
        }
    }

    #[test]
    fn analyse_second_arg() {
        let q = analyse("/scp alice:/a bob:/tm").unwrap();
        match &q.phase {
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                assert_eq!(host_token, "bob");
                assert_eq!(prefix, "/tm");
            }
            _ => panic!("expected RemotePath"),
        }
        assert_eq!(
            &"/scp alice:/a bob:/tm"[q.token_start..q.token_end],
            "bob:/tm"
        );
    }

    #[test]
    fn analyse_empty_second_arg_after_space() {
        let q = analyse("/scp alice:/a ").unwrap();
        if let ScpCompletionPhase::HostOrLocal { prefix } = q.phase {
            assert_eq!(prefix, "");
        } else {
            panic!("expected HostOrLocal");
        }
    }

    #[test]
    fn analyse_third_source_token() {
        // 多源：第三个 token 也是源，应正常补全（不再受“只能两个参数”限制）。
        let q = analyse("/scp a b c").unwrap();
        match &q.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => assert_eq!(prefix, "c"),
            _ => panic!("expected HostOrLocal"),
        }
        assert_eq!(&"/scp a b c"[q.token_start..q.token_end], "c");
    }

    #[test]
    fn analyse_third_token_remote_path() {
        // 多源：第三个 token（目的）也可以是 host:path。
        let q = analyse("/scp a.txt alice:/dst/ bob:/tm").unwrap();
        match &q.phase {
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                assert_eq!(host_token, "bob");
                assert_eq!(prefix, "/tm");
            }
            _ => panic!("expected RemotePath"),
        }
    }

    #[test]
    fn analyse_cursor_on_third_token_targets_it() {
        // 光标在第三个 token（源）上，末尾是目的：补全焦点在光标处。
        let buffer = "/scp a.txt bob:/tm alice:/dst/";
        let cursor = "/scp a.txt ".len() + 3; // 光标在 "bob" 中间
        let q = analyse_at(buffer, cursor).unwrap();
        match &q.phase {
            ScpCompletionPhase::RemotePath { host_token, .. } => {
                assert_eq!(host_token, "bob");
            }
            _ => panic!("expected RemotePath"),
        }
    }

    #[test]
    fn analyse_whitespace_after_two_tokens_still_stops() {
        // 两个完整 token 后的空白位不弹候选（与多源前的行为一致）。
        assert!(analyse("/scp a.txt prod:/tmp/ ").is_none());
    }

    #[test]
    fn analyse_at_targets_token_under_cursor() {
        // 光标落在第一个参数（本地源）末尾，而末尾 token 是远端目的。
        // 旧 analyse 会返回末尾 token；analyse_at 应返回光标处的本地源 token。
        let buffer = "/scp Downloads/ root@1.2.3.4:~/";
        let cursor = "/scp Downloads/".len(); // 光标在 "Downloads/" 之后的空格处
        let q = analyse_at(buffer, cursor).unwrap();
        match &q.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => {
                assert_eq!(prefix, "Downloads/");
            }
            _ => panic!("expected HostOrLocal for local source token under cursor"),
        }
        assert_eq!(&buffer[q.token_start..q.token_end], "Downloads/");
    }

    #[test]
    fn analyse_at_targets_second_token_when_cursor_there() {
        let buffer = "/scp Downloads/ root@1.2.3.4:/tm";
        // 光标在末尾（远端目的 token 内）
        let q = analyse_at(buffer, buffer.len()).unwrap();
        match &q.phase {
            ScpCompletionPhase::RemotePath { host_token, prefix } => {
                assert_eq!(host_token, "root@1.2.3.4");
                assert_eq!(prefix, "/tm");
            }
            _ => panic!("expected RemotePath for dest token under cursor"),
        }
    }

    #[test]
    fn analyse_at_cursor_in_command_name_returns_none() {
        // 光标还在 "/scp" 命令名里
        assert!(analyse_at("/scp Downloads/", 2).is_none());
    }

    #[test]
    fn analyse_at_cursor_in_middle_of_first_token() {
        // 光标在 "Down|loads/"：仍应返回该 token（HostOrLocal），prefix 为完整 token
        let buffer = "/scp Downloads/ root@1.2.3.4:~/";
        let cursor = "/scp Down".len();
        let q = analyse_at(buffer, cursor).unwrap();
        match &q.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => {
                assert_eq!(prefix, "Downloads/");
            }
            _ => panic!("expected HostOrLocal"),
        }
        assert_eq!(&buffer[q.token_start..q.token_end], "Downloads/");
    }

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        let tokens = shell_split_tokens("\"my host\" 'p a/th' a\\ b", 5);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].value, "my host");
        assert_eq!(tokens[1].value, "p a/th");
        assert_eq!(tokens[2].value, "a b");
        // token 起点 / 终点包含引号与反斜杠
        assert_eq!(tokens[0].start, 5);
        assert_eq!(tokens[0].end, 5 + "\"my host\"".len());
    }

    #[test]
    fn analyse_quoted_host_and_path() {
        let buffer = "/scp \"my host\":/var/log/app.log /tmp/";
        let q = analyse(buffer).unwrap();
        match &q.phase {
            ScpCompletionPhase::HostOrLocal { prefix } => {
                // 第二个 token 是 /tmp/，是本地路径
                assert_eq!(prefix, "/tmp/");
            }
            _ => panic!("expected HostOrLocal for /tmp/"),
        }
    }

    #[test]
    fn first_unquoted_colon_ignores_in_quotes() {
        assert_eq!(first_unquoted_colon("alice:/a"), Some(5));
        assert_eq!(first_unquoted_colon("\"my:host\":/a"), Some(9));
        assert_eq!(first_unquoted_colon("'a:b'"), None);
    }

    #[test]
    fn shell_quote_wraps_whitespace_and_specials() {
        assert_eq!(shell_quote(""), "\"\"");
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "\"a b\"");
        assert_eq!(shell_quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn shell_split_preserves_multibyte_chars() {
        // 中文/日文等多字节字符必须按字符迭代，否则会被拆成单字节 Latin-1 解释
        let area = "~/Downloads/第五版.pdf 中文目录/";
        let tokens = shell_split_tokens(area, 0);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].value, "~/Downloads/第五版.pdf");
        assert_eq!(tokens[1].value, "中文目录/");
        // token 的字节范围应能从原始字符串切回相同子串
        assert_eq!(
            &area[tokens[0].start..tokens[0].end],
            "~/Downloads/第五版.pdf"
        );
        assert_eq!(&area[tokens[1].start..tokens[1].end], "中文目录/");
    }

    #[test]
    fn first_unquoted_colon_returns_byte_index_for_multibyte_host() {
        let raw = "中文Host:/tmp/file.txt";
        let pos = first_unquoted_colon(raw).unwrap();
        // pos 必须是字节索引，恰好指向 ':'
        assert_eq!(&raw[pos..pos + 1], ":");
        assert_eq!(&raw[..pos], "中文Host");
        assert_eq!(&raw[pos + 1..], "/tmp/file.txt");
    }
}

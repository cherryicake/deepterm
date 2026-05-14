use std::path::PathBuf;

use gpui::Entity;

use crate::builtin_commands::registry::{BuiltinCommand, CommandHandlerId};
use crate::builtin_commands::{ArgumentParser, LocalPathContext, PathResolver};
use crate::file_browser::FileBrowserTarget;
use crate::file_transfer::{ConflictPolicy, FileEndpoint, TransferOperation, TransferRequest};
use crate::ssh::SshConnectionConfig;
use crate::ssh::config::SshGlobalConfig;
use crate::terminal_view::TerminalView;

/// 命令执行上下文
///
/// 传递给 Handler 的执行环境信息，包含终端视图、工作目录、连接配置等。
pub struct CommandContext {
    pub terminal_view: Option<Entity<TerminalView>>,
    pub working_directory: Option<PathBuf>,
    /// 本地 shell 进程的实时工作目录（通过 /proc/<pid>/cwd 或 lsof 查询）
    pub process_cwd: Option<PathBuf>,
    /// 远程终端动态追踪的工作目录（通过 OSC 7 / OSC 1337 序列更新）
    pub remote_cwd: Option<String>,
    pub is_remote: bool,
    pub ssh_config: Option<SshConnectionConfig>,
}

/// 命令执行结果
///
/// Handler 返回此结果，由调用方（TerminalView）负责发射事件。
///
/// 瀑布语义（参考 Warp 的 `execute_slash_command -> bool`，app/src/terminal/input/slash_commands/mod.rs:328）：
/// - `OpenFileBrowser` / `OpenFile` / `StartTransfer` / `Log` / `None` / `Error`
///   表示 handler **吃掉**了这条命令，TerminalView 不再把原文交给 shell。
/// - `FallThrough { shell_command }` 表示 handler 只做了"副作用 / 预处理"，
///   仍需要 shell 处理一段文本；若 `None` 则发原始命令行，若 `Some(s)` 则发
///   `s`（handler 可能已改写）。用于将来像 `/plan` 这样"做完内建动作后把 prompt 交回下一层"的命令。
pub enum CommandResult {
    /// 打开文件浏览器
    OpenFileBrowser { target: FileBrowserTarget },
    /// 打开文件
    OpenFile { path: String },
    /// 启动一次文件传输（由 workspace 层挂接进度/冲突弹窗/任务列表）
    StartTransfer { request: TransferRequest },
    /// 仅记录日志
    Log { message: String },
    /// 无操作
    None,
    /// 错误
    Error { message: String },
    /// Handler 不完全接管：把（原始或改写过的）命令行交给 shell 继续处理
    FallThrough { shell_command: Option<String> },
}

/// 命令处理器 Trait
///
/// 每个内建命令的实现者需实现此 trait，处理具体的命令逻辑。
/// trait 是 dyn-compatible（对象安全）的，因为 execute 方法没有泛型参数。
pub trait CommandHandler: Send + Sync {
    /// 执行命令
    ///
    /// `argument` 为用户输入的参数（命令名后的空格分隔内容），可能为 None。
    /// 返回命令执行结果，由调用方决定如何呈现。
    fn execute(&self, argument: Option<&str>, ctx: CommandContext) -> CommandResult;
}

/// 命令执行引擎
///
/// 管理所有已注册的命令处理器，负责根据命令元数据分发给对应 Handler。
pub struct CommandExecutor {
    handlers: std::collections::HashMap<CommandHandlerId, Box<dyn CommandHandler>>,
}

impl CommandExecutor {
    /// 创建空的执行引擎
    pub fn new() -> Self {
        Self {
            handlers: std::collections::HashMap::new(),
        }
    }

    /// 注册一个处理器
    pub fn register(&mut self, id: CommandHandlerId, handler: Box<dyn CommandHandler>) {
        self.handlers.insert(id, handler);
    }

    /// 执行命令
    ///
    /// 根据命令的 `handler_id` 查找对应处理器并执行。
    /// 如果找不到处理器，返回 `false`。
    pub fn execute(
        &self,
        command: &BuiltinCommand,
        argument: Option<&str>,
        ctx: CommandContext,
    ) -> Option<CommandResult> {
        match self.handlers.get(&command.handler_id) {
            Some(handler) => Some(handler.execute(argument, ctx)),
            None => {
                log::warn!(
                    "No handler registered for command '{}' (handler_id: {:?})",
                    command.name,
                    command.handler_id
                );
                None
            }
        }
    }

    /// 检查是否有指定处理器的注册
    pub fn has_handler(&self, id: CommandHandlerId) -> bool {
        self.handlers.contains_key(&id)
    }
}

impl Default for CommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷函数：显示错误提示
pub fn show_error_toast(message: impl Into<String>) {
    // TODO: 实现 toast 提示
    // 目前先使用 log 记录
    log::warn!("Command error: {}", message.into());
}

/// 打开文件浏览器处理器
pub struct OpenFileBrowserHandler;

impl CommandHandler for OpenFileBrowserHandler {
    fn execute(&self, argument: Option<&str>, ctx: CommandContext) -> CommandResult {
        let target = if ctx.is_remote {
            let ssh_config = ctx
                .ssh_config
                .expect("remote terminal should carry ssh config");

            // 使用远程终端动态追踪的 CWD（来自 OSC 序列），作为无参时的默认路径
            let requested_path = argument
                .and_then(|raw| PathResolver::resolve_remote(raw, ctx.remote_cwd.as_deref()));

            FileBrowserTarget::Remote {
                ssh_config,
                // 无参数时默认打开远程终端当前工作目录
                requested_path: requested_path.or_else(|| ctx.remote_cwd.clone()),
            }
        } else {
            let local_ctx = LocalPathContext {
                working_directory: ctx.working_directory.clone(),
                process_cwd: None,
            };

            let requested_path = argument.map(|raw| PathResolver::resolve_local(raw, &local_ctx));

            FileBrowserTarget::Local {
                // 优先使用进程实时 CWD，其次用初始工作目录，最后回退到家目录和根目录
                requested_path: requested_path
                    .or_else(|| ctx.process_cwd.clone())
                    .or_else(|| local_ctx.working_directory)
                    .or_else(dirs::home_dir)
                    .or(Some(PathBuf::from("/"))),
            }
        };

        CommandResult::OpenFileBrowser { target }
    }
}

/// 打开文件处理器
pub struct OpenFileHandler;

impl CommandHandler for OpenFileHandler {
    fn execute(&self, argument: Option<&str>, ctx: CommandContext) -> CommandResult {
        let parsed = match ArgumentParser::parse(argument, false) {
            Ok(p) => p,
            Err(e) => {
                return CommandResult::Error {
                    message: e.to_string(),
                };
            }
        };

        let Some(raw_path) = parsed.first_token() else {
            return CommandResult::Error {
                message: "Please provide a file path".to_string(),
            };
        };

        // 解析行号和列号（如 "file.txt:4:2"）
        let (path_part, _line, _col) =
            crate::builtin_commands::PathCompletionEngine::parse_line_column(raw_path);

        let path = if ctx.is_remote {
            PathResolver::resolve_remote(&path_part, None).unwrap_or_else(|| path_part.to_string())
        } else {
            let local_ctx = LocalPathContext {
                working_directory: ctx.working_directory.clone(),
                process_cwd: ctx.process_cwd.clone(),
            };

            PathResolver::resolve_local(&path_part, &local_ctx)
                .to_string_lossy()
                .into_owned()
        };

        CommandResult::OpenFile { path }
    }
}

/// 日志处理器
///
/// 仅记录命令执行日志，不执行任何操作。
/// 用于占位或未实现的命令。
pub struct LogOnlyHandler;

impl CommandHandler for LogOnlyHandler {
    fn execute(&self, argument: Option<&str>, _ctx: CommandContext) -> CommandResult {
        let message = if let Some(arg) = argument {
            format!("[LogOnly] command executed with argument: {}", arg)
        } else {
            "[LogOnly] command executed without argument".to_string()
        };
        log::info!("{}", message);
        CommandResult::Log { message }
    }
}

/// `/scp` 处理器：在已配置主机（或本地）之间触发一次文件传输。
///
/// 语法：`/scp <src> <dst>`
///
/// 每个 token 形式：
/// - `<host>:/abs/path` / `<host>:~/rel` / `<host>:rel` / `<host>:/dir/`
/// - 不带 `:` → 本地路径（支持 `~`、相对路径）
/// - 尾部 `/` 表示目标是目录；否则视为具体文件名（触发重命名）
///
/// `<host>` 按优先级匹配 `SshGlobalConfig::saved_connections`：
/// 1. `nickname`（大小写不敏感，精确）
/// 2. `host` 字段（大小写不敏感，精确）
/// 3. `host_label()` 即 `username@host`（仅限默认端口 22）
pub struct ScpHandler;

impl ScpHandler {
    pub fn execute_with_configs(
        &self,
        argument: Option<&str>,
        ctx: CommandContext,
        saved_configs: &[SshConnectionConfig],
    ) -> CommandResult {
        let Some(raw) = argument else {
            return CommandResult::Error {
                message: "用法: /scp <src> <dst>（需要两个参数）".to_string(),
            };
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return CommandResult::Error {
                message: "用法: /scp <src> <dst>（需要两个参数）".to_string(),
            };
        }

        // 用 shell 风格分词，允许 "with space" / 'abc' / a\ b
        let tokens = crate::builtin_commands::scp_completion::shell_split_tokens(raw, 0);

        if tokens.len() < 2 {
            return CommandResult::Error {
                message: "用法: /scp <src> <dst>（需要两个参数）".to_string(),
            };
        }
        if tokens.len() > 2 {
            return CommandResult::Error {
                message: "/scp 只支持单个源与单个目的，路径含空格请用双引号包裹".to_string(),
            };
        }

        let src_raw = &raw[tokens[0].start..tokens[0].end];
        let dst_raw = &raw[tokens[1].start..tokens[1].end];

        let src_spec = match parse_scp_token(src_raw, &ctx, saved_configs) {
            Ok(spec) => spec,
            Err(msg) => {
                return CommandResult::Error { message: msg };
            }
        };

        let dst_spec = match parse_scp_token(dst_raw, &ctx, saved_configs) {
            Ok(spec) => spec,
            Err(msg) => {
                return CommandResult::Error { message: msg };
            }
        };

        // dst 是否视作目录：以尾斜杠结尾，或明确表示为目录（纯 host:~ / host:/ 等场景）
        let (destination_endpoint, rename_to) =
            split_destination(dst_spec.endpoint, dst_spec.trailing_slash);

        let request = TransferRequest {
            sources: vec![src_spec.endpoint],
            destination: destination_endpoint,
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to,
        };

        CommandResult::StartTransfer { request }
    }
}

impl CommandHandler for ScpHandler {
    fn execute(&self, argument: Option<&str>, ctx: CommandContext) -> CommandResult {
        let saved = SshGlobalConfig::load().saved_connections;
        self.execute_with_configs(argument, ctx, &saved)
    }
}

#[derive(Debug)]
struct ScpEndpointSpec {
    endpoint: FileEndpoint,
    /// 原始 token 是否以 `/` 结尾（目录语义提示）
    trailing_slash: bool,
}

fn parse_scp_token(
    raw_token: &str,
    ctx: &CommandContext,
    saved_configs: &[SshConnectionConfig],
) -> Result<ScpEndpointSpec, String> {
    use crate::builtin_commands::scp_completion::{first_unquoted_colon, unquote_raw};

    let raw = raw_token.trim();
    if raw.is_empty() {
        return Err("路径不能为空".to_string());
    }

    // 先看"未加引号的冒号"。引号内部的 `:` 属于文本本身，不分隔 host 与 path。
    let trailing_slash = {
        let unquoted = unquote_raw(raw);
        unquoted.ends_with('/') && unquoted.len() > 1
    };

    match first_unquoted_colon(raw) {
        None => {
            // 没有引号外冒号。可能：
            //  1) `host:path` 整段被引号包住（兜底解析）
            //  2) 没有 host 前缀的"裸路径" —— 解析跟随当前终端：
            //     - 远程终端 → 当前服务器的远端路径（用 remote_cwd 做相对基准）
            //     - 本地终端 → 本机路径
            let unquoted = unquote_raw(raw);
            for (host_candidate, path_candidate) in candidate_host_path_splits(&unquoted) {
                if let Some(endpoint) =
                    resolve_host_token(host_candidate, path_candidate, ctx, saved_configs)
                {
                    return Ok(ScpEndpointSpec {
                        endpoint,
                        trailing_slash,
                    });
                }
            }
            Ok(ScpEndpointSpec {
                endpoint: resolve_bare_path(&unquoted, ctx),
                trailing_slash,
            })
        }
        Some(0) => {
            // `:...` 当作裸路径（跟随当前终端）
            let path = unquote_raw(raw);
            Ok(ScpEndpointSpec {
                endpoint: resolve_bare_path(&path, ctx),
                trailing_slash,
            })
        }
        Some(pos) => {
            let host_raw = &raw[..pos];
            let path_raw = &raw[pos + 1..];
            let host = unquote_raw(host_raw);
            let path = unquote_raw(path_raw);

            // 关键字 `Local` 始终指向运行 DeepTerm 的本机
            if is_local_keyword(&host) {
                return Ok(ScpEndpointSpec {
                    endpoint: resolve_local_endpoint(&path, ctx),
                    trailing_slash,
                });
            }

            match match_host(&host, saved_configs) {
                HostMatch::None => Err(format_unknown_host_error(&host, saved_configs)),
                HostMatch::Ambiguous(names) => Err(format!(
                    "主机 \"{}\" 匹配多个保存的连接：{}，请使用 nickname 区分",
                    host,
                    names.join(", ")
                )),
                HostMatch::One(config) => {
                    let resolved = resolve_remote_path(&path, config.clone(), ctx);
                    Ok(ScpEndpointSpec {
                        endpoint: resolved,
                        trailing_slash,
                    })
                }
            }
        }
    }
}

/// 给定 unquote 后的 token，按"先长后短"列出可能的 (host, path) 切分。
///
/// 用于"整段引号包裹"的兜底场景。比如 `user@host:port:/abs/path`：
/// - 第一个 `:` 切：host=`user@host`，path=`port:/abs/path`（错）
/// - 第二个 `:` 切：host=`user@host:port`，path=`/abs/path`（对）
///
/// 返回所有合理切分（含 keyword `Local`），调用方按命中第一个 host 来定。
fn candidate_host_path_splits(token: &str) -> Vec<(&str, &str)> {
    let mut splits: Vec<(&str, &str)> = Vec::new();
    for (i, c) in token.char_indices() {
        if c != ':' {
            continue;
        }
        if i == 0 {
            continue;
        }
        splits.push((&token[..i], &token[i + 1..]));
    }
    // 长度长的优先（带端口的 host 在前）
    splits.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    splits
}

/// 解析"无 host 前缀的裸路径"。规则：跟随当前终端所在主机。
/// - 本地终端 → `FileEndpoint::Local`
/// - 远程终端 → `FileEndpoint::Remote`，相对路径以 `remote_cwd` 为基准；
///   `remote_cwd` 缺失时退化为登录家目录（即 `./xx`，由对端 SFTP 解析）
fn resolve_bare_path(path: &str, ctx: &CommandContext) -> FileEndpoint {
    if ctx.is_remote {
        if let Some(ssh_config) = ctx.ssh_config.clone() {
            return resolve_remote_path_with_cwd(path, ssh_config, ctx.remote_cwd.as_deref());
        }
    }
    resolve_local_endpoint(path, ctx)
}

/// 引号包裹整段 `host:path` 的兜底解析：左侧匹配到 host 时返回相应 endpoint。
fn resolve_host_token(
    host_part: &str,
    path_part: &str,
    ctx: &CommandContext,
    saved_configs: &[SshConnectionConfig],
) -> Option<FileEndpoint> {
    if is_local_keyword(host_part) {
        return Some(resolve_local_endpoint(path_part, ctx));
    }
    if let HostMatch::One(config) = match_host(host_part, saved_configs) {
        return Some(resolve_remote_path(path_part, config.clone(), ctx));
    }
    None
}

/// 关键字 `Local`（不区分大小写）—— 始终指向运行 DeepTerm 的本机。
///
/// 同时识别"用户级别名"形式 `xxx@localhost` / `xxx@127.0.0.1` —— 这是补全
/// 写入命令行的标准形态，回写后必须仍能被 parse 识别为本机。
pub fn is_local_keyword(token: &str) -> bool {
    if token.eq_ignore_ascii_case("local") {
        return true;
    }
    if let Some((_, host_part)) = token.split_once('@') {
        let host_lc = host_part.to_lowercase();
        return host_lc == "localhost" || host_lc == "127.0.0.1" || host_lc == "::1";
    }
    false
}

fn resolve_local_endpoint(raw: &str, ctx: &CommandContext) -> FileEndpoint {
    let local_ctx = LocalPathContext {
        working_directory: ctx.working_directory.clone(),
        process_cwd: ctx.process_cwd.clone(),
    };
    FileEndpoint::Local {
        path: PathResolver::resolve_local(raw, &local_ctx),
    }
}

fn resolve_remote_path(
    path_part: &str,
    ssh_config: SshConnectionConfig,
    _ctx: &CommandContext,
) -> FileEndpoint {
    // 显式 `host:path` 的远端解析：相对路径以登录家目录为基准（与 scp CLI 一致）。
    resolve_remote_path_with_cwd(path_part, ssh_config, None)
}

/// 远端路径解析的统一实现。`base_cwd` 提供"当前所在目录"用于解析裸的相对路径
/// （例如远程终端里直接写 `xx/yy.tar`），`None` 则按登录家目录处理。
fn resolve_remote_path_with_cwd(
    path_part: &str,
    ssh_config: SshConnectionConfig,
    base_cwd: Option<&str>,
) -> FileEndpoint {
    let trimmed = path_part.trim();
    let path = if trimmed.is_empty() || trimmed == "~" {
        "./".to_string()
    } else if let Some(stripped) = trimmed.strip_prefix("~/") {
        // ~/x → 家目录相对，用 SFTP 端解析
        format!("./{}", stripped)
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else if let Some(cwd) = base_cwd {
        // 远程终端的裸相对路径：以 OSC 7 报上来的 cwd 为基准拼接
        let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
        crate::ssh::sftp_fs::join_remote_path(cwd, stripped)
    } else {
        // 没有 cwd 信号 → 等价 `~/rel`，由对端 SFTP 自己 resolve home
        format!("./{}", trimmed)
    };
    FileEndpoint::Remote { ssh_config, path }
}

fn split_destination(
    endpoint: FileEndpoint,
    trailing_slash: bool,
) -> (FileEndpoint, Option<String>) {
    if trailing_slash {
        return (endpoint, None);
    }
    match endpoint {
        FileEndpoint::Local { path } => {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty());
            match file_name {
                Some(name) => {
                    let parent = path.parent().map(PathBuf::from).unwrap_or_else(|| {
                        // 路径没有 parent（例如 "/" 或 ""），降级为目录语义
                        path.clone()
                    });
                    if parent.as_os_str().is_empty() {
                        (FileEndpoint::Local { path }, None)
                    } else {
                        (FileEndpoint::Local { path: parent }, Some(name))
                    }
                }
                None => (FileEndpoint::Local { path }, None),
            }
        }
        FileEndpoint::Remote { ssh_config, path } => {
            let name = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .map(|n| n.to_string());
            match name {
                None => (FileEndpoint::Remote { ssh_config, path }, None),
                Some(name) => {
                    let parent = crate::ssh::sftp_fs::remote_parent(&path).unwrap_or_else(|| {
                        // path 是裸的 "."/"./x" 之类（没有前导 /）
                        if let Some(stripped) = path.strip_suffix(&format!("/{}", name)) {
                            stripped.to_string()
                        } else if path == name {
                            "./".to_string()
                        } else {
                            "/".to_string()
                        }
                    });
                    (
                        FileEndpoint::Remote {
                            ssh_config,
                            path: parent,
                        },
                        Some(name),
                    )
                }
            }
        }
    }
}

enum HostMatch<'a> {
    None,
    One(&'a SshConnectionConfig),
    Ambiguous(Vec<String>),
}

fn match_host<'a>(token: &str, saved: &'a [SshConnectionConfig]) -> HostMatch<'a> {
    let token_lc = token.to_lowercase();
    // 依次按优先级收集匹配：nickname → host → username@host
    let nickname_hits: Vec<&SshConnectionConfig> = saved
        .iter()
        .filter(|c| {
            c.nickname
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(token))
                .unwrap_or(false)
        })
        .collect();
    if nickname_hits.len() == 1 {
        return HostMatch::One(nickname_hits[0]);
    }
    if nickname_hits.len() > 1 {
        return HostMatch::Ambiguous(nickname_hits.iter().map(|c| c.display_name()).collect());
    }

    let host_hits: Vec<&SshConnectionConfig> = saved
        .iter()
        .filter(|c| c.host.to_lowercase() == token_lc)
        .collect();
    if host_hits.len() == 1 {
        return HostMatch::One(host_hits[0]);
    }
    if host_hits.len() > 1 {
        return HostMatch::Ambiguous(host_hits.iter().map(|c| c.display_name()).collect());
    }

    let label_hits: Vec<&SshConnectionConfig> = saved
        .iter()
        .filter(|c| c.host_label().eq_ignore_ascii_case(token))
        .collect();
    if label_hits.len() == 1 {
        return HostMatch::One(label_hits[0]);
    }
    if label_hits.len() > 1 {
        return HostMatch::Ambiguous(label_hits.iter().map(|c| c.display_name()).collect());
    }

    HostMatch::None
}

fn format_unknown_host_error(token: &str, saved: &[SshConnectionConfig]) -> String {
    if saved.is_empty() {
        return format!(
            "未找到主机 \"{}\"。尚未在 DeepTerm 中保存任何 SSH 连接。",
            token
        );
    }
    let mut labels: Vec<String> = saved
        .iter()
        .map(|c| c.nickname.clone().unwrap_or_else(|| c.host_label()))
        .collect();
    labels.sort();
    labels.dedup();
    let shown: Vec<String> = labels.iter().take(5).cloned().collect();
    let suffix = if labels.len() > shown.len() {
        format!("，共 {} 个", labels.len())
    } else {
        String::new()
    };
    format!(
        "未找到主机 \"{}\"。可用: {}{}",
        token,
        shown.join(", "),
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Icon;
    use crate::builtin_commands::registry::{ArgumentType, Availability};

    struct TestHandler;

    impl CommandHandler for TestHandler {
        fn execute(&self, argument: Option<&str>, _ctx: CommandContext) -> CommandResult {
            log::info!("Test handler executed with argument: {:?}", argument);
            CommandResult::None
        }
    }

    #[test]
    fn test_executor_registration() {
        let mut executor = CommandExecutor::new();
        let id = CommandHandlerId("test");

        assert!(!executor.has_handler(id));
        executor.register(id, Box::new(TestHandler));
        assert!(executor.has_handler(id));
    }

    #[test]
    fn test_execute_unknown_handler() {
        let executor = CommandExecutor::new();
        let command = BuiltinCommand {
            name: "/test",
            description: "Test",
            icon: Icon::Folder,
            availability: Availability::Always,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("unknown"),
        };

        assert!(
            executor
                .execute(
                    &command,
                    None,
                    CommandContext {
                        terminal_view: None,
                        working_directory: None,
                        process_cwd: None,
                        remote_cwd: None,
                        is_remote: false,
                        ssh_config: None,
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn test_open_file_browser_local() {
        let handler = OpenFileBrowserHandler;
        let result = handler.execute(
            Some("/tmp"),
            CommandContext {
                terminal_view: None,
                working_directory: Some(PathBuf::from("/home/user")),
                process_cwd: None,
                remote_cwd: None,
                is_remote: false,
                ssh_config: None,
            },
        );

        match result {
            CommandResult::OpenFileBrowser { target } => match target {
                FileBrowserTarget::Local { requested_path } => {
                    assert_eq!(requested_path, Some(PathBuf::from("/tmp")));
                }
                _ => panic!("Expected local target"),
            },
            _ => panic!("Expected OpenFileBrowser result"),
        }
    }

    #[test]
    fn test_open_file_handler() {
        let handler = OpenFileHandler;
        let result = handler.execute(
            Some("src/main.rs"),
            CommandContext {
                terminal_view: None,
                working_directory: Some(PathBuf::from("/home/user/project")),
                process_cwd: None,
                remote_cwd: None,
                is_remote: false,
                ssh_config: None,
            },
        );

        match result {
            CommandResult::OpenFile { path } => {
                assert_eq!(path, "/home/user/project/src/main.rs");
            }
            _ => panic!("Expected OpenFile result"),
        }
    }

    // ── /scp handler tests ─────────────────────────────────────────────────────

    fn scp_ctx() -> CommandContext {
        CommandContext {
            terminal_view: None,
            working_directory: Some(PathBuf::from("/home/user")),
            process_cwd: Some(PathBuf::from("/home/user/project")),
            remote_cwd: None,
            is_remote: false,
            ssh_config: None,
        }
    }

    fn host(nickname: Option<&str>, username: &str, host: &str) -> SshConnectionConfig {
        use crate::ssh::config::AuthMethod;
        let mut cfg =
            SshConnectionConfig::new(host.to_string(), username.to_string(), AuthMethod::Agent);
        if let Some(name) = nickname {
            cfg.nickname = Some(name.to_string());
        }
        cfg
    }

    fn unwrap_transfer(result: CommandResult) -> TransferRequest {
        match result {
            CommandResult::StartTransfer { request } => request,
            other => panic!(
                "expected StartTransfer, got {}",
                match other {
                    CommandResult::Error { message } => format!("Error({})", message),
                    CommandResult::Log { .. } => "Log".into(),
                    CommandResult::OpenFile { .. } => "OpenFile".into(),
                    CommandResult::OpenFileBrowser { .. } => "OpenFileBrowser".into(),
                    CommandResult::None => "None".into(),
                    CommandResult::FallThrough { .. } => "FallThrough".into(),
                    CommandResult::StartTransfer { .. } => unreachable!(),
                }
            ),
        }
    }

    #[test]
    fn scp_handler_remote_to_remote_rename() {
        let saved = vec![
            host(Some("alice"), "root", "1.2.3.4"),
            host(Some("bob"), "deploy", "5.6.7.8"),
        ];
        let result = ScpHandler.execute_with_configs(
            Some("alice:/var/log/syslog bob:/backup/syslog-2026.log"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);

        assert_eq!(req.sources.len(), 1);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("alice"));
                assert_eq!(path, "/var/log/syslog");
            }
            _ => panic!("expected remote source"),
        }
        match &req.destination {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("bob"));
                assert_eq!(path, "/backup");
            }
            _ => panic!("expected remote destination"),
        }
        assert_eq!(req.rename_to.as_deref(), Some("syslog-2026.log"));
        assert_eq!(req.operation, TransferOperation::Copy);
    }

    #[test]
    fn scp_handler_trailing_slash_means_directory() {
        let saved = vec![host(Some("alice"), "root", "1.2.3.4")];
        let result = ScpHandler.execute_with_configs(
            Some("alice:/etc/hosts alice:/tmp/"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        assert!(req.rename_to.is_none());
        match &req.destination {
            FileEndpoint::Remote { path, .. } => assert_eq!(path, "/tmp/"),
            _ => panic!("expected remote destination"),
        }
    }

    #[test]
    fn scp_handler_local_to_remote() {
        let saved = vec![host(Some("prod"), "deploy", "prod.example.com")];
        let result =
            ScpHandler.execute_with_configs(Some("./Cargo.toml prod:/tmp/"), scp_ctx(), &saved);
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Local { path } => {
                assert_eq!(path, &PathBuf::from("/home/user/project/./Cargo.toml"));
            }
            _ => panic!("expected local source"),
        }
        match &req.destination {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("prod"));
                assert_eq!(path, "/tmp/");
            }
            _ => panic!("expected remote destination"),
        }
        assert!(req.rename_to.is_none());
    }

    #[test]
    fn scp_handler_remote_to_local_with_rename() {
        let saved = vec![host(Some("prod"), "deploy", "prod.example.com")];
        let result = ScpHandler.execute_with_configs(
            Some("prod:/var/log/syslog ~/syslog.copy"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.destination {
            FileEndpoint::Local { path } => {
                let home = dirs::home_dir().unwrap();
                assert_eq!(path, &home);
            }
            _ => panic!("expected local destination"),
        }
        assert_eq!(req.rename_to.as_deref(), Some("syslog.copy"));
    }

    #[test]
    fn scp_handler_local_to_local_rename() {
        let saved: Vec<SshConnectionConfig> = vec![];
        let result =
            ScpHandler.execute_with_configs(Some("/tmp/a.txt /tmp/b.txt"), scp_ctx(), &saved);
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Local { path } => assert_eq!(path, &PathBuf::from("/tmp/a.txt")),
            _ => panic!("expected local"),
        }
        match &req.destination {
            FileEndpoint::Local { path } => assert_eq!(path, &PathBuf::from("/tmp")),
            _ => panic!("expected local"),
        }
        assert_eq!(req.rename_to.as_deref(), Some("b.txt"));
    }

    #[test]
    fn scp_handler_unknown_host_errors() {
        let saved = vec![host(Some("alice"), "root", "1.2.3.4")];
        let result = ScpHandler.execute_with_configs(Some("ghost:/a /tmp/"), scp_ctx(), &saved);
        match result {
            CommandResult::Error { message } => {
                assert!(message.contains("ghost"), "msg: {}", message);
                assert!(message.contains("alice"), "msg: {}", message);
            }
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn scp_handler_ambiguous_host_errors() {
        // 两个 nickname 同名 -> 歧义
        let saved = vec![
            host(Some("db"), "root", "10.0.0.1"),
            host(Some("db"), "root", "10.0.0.2"),
        ];
        let result = ScpHandler.execute_with_configs(Some("db:/a /tmp/"), scp_ctx(), &saved);
        match result {
            CommandResult::Error { message } => {
                assert!(message.contains("db"), "msg: {}", message);
                assert!(message.contains("多个") || message.contains("multiple"));
            }
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn scp_handler_missing_args_errors() {
        let saved: Vec<SshConnectionConfig> = vec![];
        let result = ScpHandler.execute_with_configs(Some("/tmp/only"), scp_ctx(), &saved);
        assert!(matches!(result, CommandResult::Error { .. }));

        let result = ScpHandler.execute_with_configs(None, scp_ctx(), &saved);
        assert!(matches!(result, CommandResult::Error { .. }));
    }

    #[test]
    fn scp_handler_remote_tilde_path() {
        let saved = vec![host(Some("alice"), "root", "1.2.3.4")];
        let result =
            ScpHandler.execute_with_configs(Some("alice:~/logs/app.log ~/"), scp_ctx(), &saved);
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { path, .. } => {
                assert_eq!(path, "./logs/app.log");
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn scp_handler_host_label_match() {
        // 无 nickname，用 username@host 匹配
        let cfg = host(None, "alice", "box.example.com");
        let saved = vec![cfg];
        let result = ScpHandler.execute_with_configs(
            Some("alice@box.example.com:/a /tmp/"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, .. } => {
                assert_eq!(ssh_config.host, "box.example.com");
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn scp_handler_accepts_quoted_host_and_path() {
        let saved = vec![host(Some("my host"), "root", "1.2.3.4")];
        let result = ScpHandler.execute_with_configs(
            Some("\"my host\":/etc/hosts \"/tmp/my folder/\""),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("my host"));
                assert_eq!(path, "/etc/hosts");
            }
            _ => panic!("expected remote source"),
        }
        match &req.destination {
            FileEndpoint::Local { path } => {
                assert_eq!(path, &PathBuf::from("/tmp/my folder/"));
            }
            _ => panic!("expected local destination"),
        }
        // 尾斜杠 ⇒ 目录
        assert!(req.rename_to.is_none());
    }

    #[test]
    fn scp_handler_backslash_escaped_space() {
        let saved = vec![host(Some("alice"), "root", "1.2.3.4")];
        let result = ScpHandler.execute_with_configs(
            Some("alice:/tmp/with\\ space.txt /tmp/"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { path, .. } => {
                assert_eq!(path, "/tmp/with space.txt");
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn scp_handler_accepts_whole_token_quoted_host_path() {
        // 用户可能把 `host:path` 整体放引号里（scp CLI 常见写法）。
        // 即便引号内没有未引号 `:`，只要 unquote 后左侧匹配到已保存主机就应当
        // 被识别成远端 token，而不是本地路径。
        let saved = vec![host(Some("my host"), "root", "1.2.3.4")];
        let result = ScpHandler.execute_with_configs(
            Some("\"my host:logs/app.tar\" /tmp/"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("my host"));
                // 相对路径会被转成 `./…`
                assert_eq!(path, "./logs/app.tar");
            }
            _ => panic!("expected remote source"),
        }
    }

    // ── 当前终端上下文：裸路径解析跟随 is_remote ─────────────────────────────

    fn remote_ctx(cfg: &SshConnectionConfig, cwd: Option<&str>) -> CommandContext {
        CommandContext {
            terminal_view: None,
            working_directory: Some(PathBuf::from("/home/user")),
            process_cwd: None,
            remote_cwd: cwd.map(|s| s.to_string()),
            is_remote: true,
            ssh_config: Some(cfg.clone()),
        }
    }

    #[test]
    fn scp_bare_path_is_remote_when_terminal_is_remote() {
        // 远程终端里 `./sn2-miner.tar` 应被识别为远程主机的当前目录文件
        let cur = host(Some("webNX-c01"), "root", "1.2.3.4");
        let saved = vec![cur.clone()];
        let result = ScpHandler.execute_with_configs(
            Some("./sn2-miner.tar Local:~/Downloads/"),
            remote_ctx(&cur, Some("/root/work")),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("webNX-c01"));
                assert_eq!(path, "/root/work/sn2-miner.tar");
            }
            _ => panic!("expected remote source for bare path in remote ctx"),
        }
        match &req.destination {
            FileEndpoint::Local { path } => {
                let home = dirs::home_dir().unwrap();
                assert_eq!(path, &home.join("Downloads"));
            }
            _ => panic!("expected local destination for Local: prefix"),
        }
    }

    #[test]
    fn scp_bare_path_falls_back_to_home_when_no_remote_cwd() {
        let cur = host(Some("webNX-c01"), "root", "1.2.3.4");
        let saved = vec![cur.clone()];
        let result = ScpHandler.execute_with_configs(
            Some("xx/yy.tar Local:~/Downloads/"),
            remote_ctx(&cur, None),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { path, .. } => {
                // 没有 cwd 信号 → 等价 ~/rel
                assert_eq!(path, "./xx/yy.tar");
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn scp_local_keyword_targets_this_machine_from_remote_terminal() {
        let cur = host(Some("webNX-c01"), "root", "1.2.3.4");
        let saved = vec![cur.clone()];
        let result = ScpHandler.execute_with_configs(
            Some("/etc/hosts Local:/tmp/"),
            remote_ctx(&cur, Some("/root")),
            &saved,
        );
        let req = unwrap_transfer(result);
        // 源（裸绝对路径）→ 远程终端 host 上的 /etc/hosts
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.nickname.as_deref(), Some("webNX-c01"));
                assert_eq!(path, "/etc/hosts");
            }
            _ => panic!("expected remote source"),
        }
        match &req.destination {
            FileEndpoint::Local { path } => assert_eq!(path, &PathBuf::from("/tmp")),
            _ => panic!("expected local destination"),
        }
    }

    #[test]
    fn scp_local_keyword_in_local_terminal_equivalent_to_bare_path() {
        let saved: Vec<SshConnectionConfig> = vec![];
        let result1 =
            ScpHandler.execute_with_configs(Some("Local:/tmp/a.txt /tmp/b/"), scp_ctx(), &saved);
        let req1 = unwrap_transfer(result1);
        match &req1.sources[0] {
            FileEndpoint::Local { path } => assert_eq!(path, &PathBuf::from("/tmp/a.txt")),
            _ => panic!("expected local"),
        }
    }

    #[test]
    fn scp_completion_includes_local_keyword() {
        let saved = vec![host(Some("alice"), "root", "1.2.3.4")];
        let suggestions =
            crate::builtin_commands::scp_completion::match_hosts_by_prefix("", &saved);
        assert!(
            suggestions.iter().any(|s| s.kind
                == crate::builtin_commands::scp_completion::HostSuggestionKind::Local
                && s.display == "Local"),
            "Local keyword should be present at empty prefix"
        );
        // `loc` 前缀也应命中
        let lo = crate::builtin_commands::scp_completion::match_hosts_by_prefix("loc", &saved);
        assert!(
            lo.iter()
                .any(|s| s.kind
                    == crate::builtin_commands::scp_completion::HostSuggestionKind::Local)
        );
    }

    #[test]
    fn scp_handler_user_at_localhost_is_local() {
        // 补全输出 `username@localhost:` 形态后，再次解析仍要被识别为本机。
        let cur = host(Some("webNX"), "root", "1.2.3.4");
        let saved = vec![cur.clone()];
        let result = ScpHandler.execute_with_configs(
            Some("./a.tar zhousong@localhost:/tmp/"),
            remote_ctx(&cur, Some("/root")),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.destination {
            FileEndpoint::Local { path } => assert_eq!(path, &PathBuf::from("/tmp")),
            _ => panic!("expected local destination"),
        }
    }

    #[test]
    fn scp_handler_user_at_host_with_port_quoted() {
        // 补全输出 `"user@host:port":path` 形态，提交时整段在引号里没有未引号 `:`。
        // 兜底解析应当尝试多种切分点找到带端口的 host。
        use crate::ssh::config::AuthMethod;
        let mut cfg = SshConnectionConfig::new(
            "1.2.3.4".to_string(),
            "deploy".to_string(),
            AuthMethod::Agent,
        );
        cfg.port = 2222;
        let saved = vec![cfg];
        let result = ScpHandler.execute_with_configs(
            Some("\"deploy@1.2.3.4:2222:/tmp/x.txt\" /tmp/"),
            scp_ctx(),
            &saved,
        );
        let req = unwrap_transfer(result);
        match &req.sources[0] {
            FileEndpoint::Remote { ssh_config, path } => {
                assert_eq!(ssh_config.host, "1.2.3.4");
                assert_eq!(ssh_config.port, 2222);
                assert_eq!(path, "/tmp/x.txt");
            }
            _ => panic!("expected remote with port"),
        }
    }
}

// ── Global Executor ──────────────────────────────────────────────────────────

use std::sync::{Mutex, OnceLock};

static GLOBAL_EXECUTOR: OnceLock<Mutex<CommandExecutor>> = OnceLock::new();

fn create_default_executor() -> CommandExecutor {
    let mut executor = CommandExecutor::new();
    executor.register(
        CommandHandlerId("open_file_browser"),
        Box::new(OpenFileBrowserHandler),
    );
    executor.register(CommandHandlerId("open_file"), Box::new(OpenFileHandler));
    executor.register(CommandHandlerId("scp_transfer"), Box::new(ScpHandler));
    executor.register(CommandHandlerId("log_only"), Box::new(LogOnlyHandler));
    executor
}

/// 初始化全局命令执行引擎
///
/// 在应用启动时调用一次，注册所有内置命令处理器。
pub fn init_global_executor() {
    let _ = GLOBAL_EXECUTOR.get_or_init(|| Mutex::new(create_default_executor()));
}

/// 获取全局命令执行引擎的引用
pub fn global_executor() -> &'static Mutex<CommandExecutor> {
    GLOBAL_EXECUTOR.get_or_init(|| Mutex::new(create_default_executor()))
}

use std::path::PathBuf;

use gpui::Entity;

use crate::builtin_commands::registry::{BuiltinCommand, CommandHandlerId};
use crate::builtin_commands::{ArgumentParser, LocalPathContext, PathResolver};
use crate::file_browser::FileBrowserTarget;
use crate::ssh::SshConnectionConfig;
use crate::terminal_view::TerminalView;

/// 命令执行上下文
///
/// 传递给 Handler 的执行环境信息，包含终端视图、工作目录、连接配置等。
pub struct CommandContext {
    pub terminal_view: Option<Entity<TerminalView>>,
    pub working_directory: Option<PathBuf>,
    pub is_remote: bool,
    pub ssh_config: Option<SshConnectionConfig>,
}

/// 命令执行结果
///
/// Handler 返回此结果，由调用方（TerminalView）负责发射事件。
pub enum CommandResult {
    /// 打开文件浏览器
    OpenFileBrowser { target: FileBrowserTarget },
    /// 打开文件
    OpenFile { path: String },
    /// 仅记录日志
    Log { message: String },
    /// 无操作
    None,
    /// 错误
    Error { message: String },
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
            // 注意：由于 trait 是 dyn-compatible 的，无法直接使用 GPUI 上下文
            // 因此远程 cwd 的解析暂时使用 ssh_config 中的信息
            // 更完整的实现需要在 TerminalView 中预解析 remote_cwd 后传入 ctx
            let requested_path = argument.and_then(|raw| PathResolver::resolve_remote(raw, None));

            FileBrowserTarget::Remote {
                ssh_config,
                requested_path,
            }
        } else {
            let local_ctx = LocalPathContext {
                working_directory: ctx.working_directory.clone(),
                process_cwd: None, // 需要在 TerminalView 中预获取
            };

            let requested_path = argument.map(|raw| PathResolver::resolve_local(raw, &local_ctx));

            FileBrowserTarget::Local {
                requested_path: requested_path
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

        let path = if ctx.is_remote {
            PathResolver::resolve_remote(raw_path, None).unwrap_or_else(|| raw_path.to_string())
        } else {
            let local_ctx = LocalPathContext {
                working_directory: ctx.working_directory.clone(),
                process_cwd: None,
            };

            PathResolver::resolve_local(raw_path, &local_ctx)
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

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::assets::Icon;

/// 命令处理器标识符
///
/// 用于在注册表中关联命令与处理器，不暴露具体实现。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommandHandlerId(pub &'static str);

/// 命令可用性条件
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    /// 始终可用
    Always,
    /// 仅在本地终端可用
    LocalOnly,
    /// 仅在远程终端可用
    RemoteOnly,
}

impl Availability {
    /// 检查在当前环境下是否可用
    pub fn is_available(self, is_remote: bool) -> bool {
        match self {
            Availability::Always => true,
            Availability::LocalOnly => !is_remote,
            Availability::RemoteOnly => is_remote,
        }
    }
}

/// 参数类型定义
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgumentType {
    /// 无参数
    None,
    /// 可选参数
    Optional {
        /// 参数提示文本
        hint: Option<&'static str>,
    },
    /// 必填参数
    Required {
        /// 参数提示文本
        hint: &'static str,
    },
}

/// 内建命令定义
///
/// 这是注册表中存储的命令元数据，比 `BuiltinCommandSpec` 更丰富。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltinCommand {
    /// 命令名称（如 "/open"）
    pub name: &'static str,
    /// 命令描述
    pub description: &'static str,
    /// 命令图标
    pub icon: Icon,
    /// 可用性条件
    pub availability: Availability,
    /// 参数类型
    pub argument: ArgumentType,
    /// 处理器标识符
    pub handler_id: CommandHandlerId,
}

/// 命令注册表
///
/// 支持动态注册内建命令，提供按名称查找和按环境过滤功能。
/// 使用全局单例模式，确保整个应用共享同一注册表。
pub struct CommandRegistry {
    commands: Vec<Arc<BuiltinCommand>>,
    commands_by_name: HashMap<&'static str, Arc<BuiltinCommand>>,
}

impl CommandRegistry {
    /// 创建空注册表
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            commands_by_name: HashMap::new(),
        }
    }

    /// 注册一个命令
    pub fn register(&mut self, command: BuiltinCommand) {
        let name = command.name;
        let cmd = Arc::new(command);
        self.commands.push(cmd.clone());
        self.commands_by_name.insert(name, cmd);
    }

    /// 按名称查找命令
    pub fn find(&self, name: &str) -> Option<Arc<BuiltinCommand>> {
        self.commands_by_name.get(name).cloned()
    }

    /// 获取当前环境下所有可用命令
    pub fn active_commands(&self, is_remote: bool) -> Vec<Arc<BuiltinCommand>> {
        self.commands
            .iter()
            .filter(|cmd| cmd.availability.is_available(is_remote))
            .cloned()
            .collect()
    }

    /// 获取所有命令（不考虑环境）
    pub fn all_commands(&self) -> Vec<Arc<BuiltinCommand>> {
        self.commands.clone()
    }

    /// 从命令列表初始化
    pub fn from_commands(commands: Vec<BuiltinCommand>) -> Self {
        let mut registry = Self::new();
        for cmd in commands {
            registry.register(cmd);
        }
        registry
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 初始化全局命令注册表
///
/// 注册所有内置命令。
pub fn init_global_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();

    registry.register(BuiltinCommand {
        name: "/open",
        description: "Open DeepTerm's File Browser.",
        icon: Icon::Folder,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("open_file_browser"),
    });
    registry.register(BuiltinCommand {
        name: "/open-file",
        description: "Open a file in DeepTerm's editor.",
        icon: Icon::Screen,
        availability: Availability::Always,
        argument: ArgumentType::Optional {
            hint: Some("file_path"),
        },
        handler_id: CommandHandlerId("open_file"),
    });
    registry.register(BuiltinCommand {
        name: "/open-repo",
        description: "Switch to another indexed repository.",
        icon: Icon::Host,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/open-skill",
        description: "Inspect an installed skill definition.",
        icon: Icon::KeyShield,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/open-rules",
        description: "View global and project agent rules.",
        icon: Icon::SettingsHub,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/open-code-review",
        description: "Open the code review workspace.",
        icon: Icon::Plus,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/open-mcp-servers",
        description: "Inspect configured MCP servers.",
        icon: Icon::Server,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/open-project-rules",
        description: "Open the project rules file (AGENTS.md).",
        icon: Icon::Host,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/new-component",
        description: "Create new GPUI components. Use when building components or UI elements.",
        icon: Icon::Plus,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/generate-component-story",
        description: "Create story examples for components.",
        icon: Icon::Loader,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });
    registry.register(BuiltinCommand {
        name: "/generate-component-documentation",
        description: "Generate documentation for new components.",
        icon: Icon::Loader,
        availability: Availability::Always,
        argument: ArgumentType::None,
        handler_id: CommandHandlerId("log_only"),
    });

    registry
}

/// 全局注册表单例
static GLOBAL_REGISTRY: OnceLock<CommandRegistry> = OnceLock::new();

/// 获取全局命令注册表
///
/// 首次调用时会使用默认命令初始化。
pub fn global_registry() -> &'static CommandRegistry {
    GLOBAL_REGISTRY.get_or_init(init_global_registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Icon;

    fn test_cmd(name: &'static str, description: &'static str) -> BuiltinCommand {
        BuiltinCommand {
            name,
            description,
            icon: Icon::Folder,
            availability: Availability::Always,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("test"),
        }
    }

    #[test]
    fn test_find_existing_command() {
        let mut registry = CommandRegistry::new();
        registry.register(test_cmd("/open", "Open file browser"));
        registry.register(test_cmd("/close", "Close window"));

        let cmd = registry.find("/open");
        assert!(cmd.is_some());
        assert_eq!(cmd.unwrap().name, "/open");
    }

    #[test]
    fn test_find_missing_command() {
        let mut registry = CommandRegistry::new();
        registry.register(test_cmd("/open", "Open file browser"));
        assert!(registry.find("/missing").is_none());
    }

    #[test]
    fn test_active_commands_local() {
        let mut registry = CommandRegistry::new();
        registry.register(BuiltinCommand {
            name: "/local",
            description: "Local only",
            icon: Icon::Folder,
            availability: Availability::LocalOnly,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("test"),
        });
        registry.register(BuiltinCommand {
            name: "/remote",
            description: "Remote only",
            icon: Icon::Folder,
            availability: Availability::RemoteOnly,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("test"),
        });
        registry.register(BuiltinCommand {
            name: "/any",
            description: "Any",
            icon: Icon::Folder,
            availability: Availability::Always,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("test"),
        });

        let local = registry.active_commands(false);
        assert_eq!(local.len(), 2);
        assert!(local.iter().any(|c| c.name == "/local"));
        assert!(local.iter().any(|c| c.name == "/any"));

        let remote = registry.active_commands(true);
        assert_eq!(remote.len(), 2);
        assert!(remote.iter().any(|c| c.name == "/remote"));
        assert!(remote.iter().any(|c| c.name == "/any"));
    }

    #[test]
    fn test_global_registry_initialized() {
        let registry = global_registry();
        assert!(registry.find("/open").is_some());
        assert!(registry.find("/open-file").is_some());
    }
}

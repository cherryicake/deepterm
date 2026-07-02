use crate::builtin_commands::registry::{BuiltinCommand, global_registry};
use std::sync::Arc;

/// 命令输入状态机
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandEntryState {
    /// 输入内容与内建命令无关
    None,
    /// 正在输入命令（以 '/' 开头但未匹配到完整命令）
    Composing { filter: String },
    /// 检测到有效的内建命令
    Detected(DetectedCommand),
    /// 内建命令被禁用，直到缓冲区清空
    DisabledUntilEmpty,
}

impl CommandEntryState {
    /// 获取检测到的命令（如果有）
    pub fn detected_command(&self) -> Option<&DetectedCommand> {
        match self {
            CommandEntryState::Detected(cmd) => Some(cmd),
            _ => None,
        }
    }

    /// 检查是否检测到有效命令
    pub fn is_detected(&self) -> bool {
        matches!(self, CommandEntryState::Detected(_))
    }

    /// 检查是否处于命令输入模式
    pub fn is_active(&self) -> bool {
        !matches!(self, CommandEntryState::None)
    }
}

/// 检测到的命令及其参数
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedCommand {
    /// 匹配的命令
    pub command: Arc<BuiltinCommand>,
    /// 命令后的参数（空格分隔的第一个词之后的所有内容）
    pub argument: Option<String>,
}

/// 内建命令模型
///
/// 封装命令输入状态机，提供纯函数式命令检测。
/// 这是一个轻量级状态管理器，不依赖 GPUI，可被 TerminalView 直接持有。
pub struct BuiltinCommandModel {
    state: CommandEntryState,
}

impl BuiltinCommandModel {
    pub fn new() -> Self {
        Self {
            state: CommandEntryState::None,
        }
    }

    /// 获取当前状态
    pub fn state(&self) -> &CommandEntryState {
        &self.state
    }

    /// 纯函数：检测文本是否匹配内建命令
    ///
    /// 不修改模型状态，可用于外部预览或测试。
    pub fn detect_command(&self, text: &str) -> CommandEntryState {
        if text.is_empty() {
            return CommandEntryState::None;
        }

        if !text.starts_with('/') {
            return CommandEntryState::None;
        }

        // 提取命令名和参数
        let (command_name, argument) = match text.split_once(' ') {
            Some((name, arg)) => (name, Some(arg.trim().to_string())),
            None => (text, None),
        };

        // 在注册表中查找
        match global_registry().find(command_name) {
            Some(cmd) => CommandEntryState::Detected(DetectedCommand {
                command: cmd,
                argument,
            }),
            None => {
                // 去掉 '/' 后的过滤文本
                let filter = command_name
                    .strip_prefix('/')
                    .unwrap_or(command_name)
                    .to_string();
                CommandEntryState::Composing { filter }
            }
        }
    }

    /// 更新状态，基于新的输入文本
    ///
    /// 返回 `(新状态, 是否变更)`
    pub fn update(&mut self, text: &str) -> (&CommandEntryState, bool) {
        let new_state = self.detect_command(text);
        let changed = self.state != new_state;
        self.state = new_state;
        (&self.state, changed)
    }

    /// 重置状态
    pub fn reset(&mut self) {
        self.state = CommandEntryState::None;
    }

    /// 标记为禁用（直到清空缓冲区）
    pub fn disable(&mut self) {
        if !matches!(self.state, CommandEntryState::None) {
            self.state = CommandEntryState::DisabledUntilEmpty;
        }
    }
}

impl Default for BuiltinCommandModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input_returns_none() {
        let model = BuiltinCommandModel::new();
        assert_eq!(model.detect_command(""), CommandEntryState::None);
    }

    #[test]
    fn test_no_slash_returns_none() {
        let model = BuiltinCommandModel::new();
        assert_eq!(model.detect_command("hello"), CommandEntryState::None);
    }

    #[test]
    fn test_detected_command_without_argument() {
        let model = BuiltinCommandModel::new();
        let state = model.detect_command("/open");

        match state {
            CommandEntryState::Detected(cmd) => {
                assert_eq!(cmd.command.name, "/open");
                assert_eq!(cmd.argument, None);
            }
            other => panic!("Expected Detected, got {:?}", other),
        }
    }

    #[test]
    fn test_composing_state_for_unknown_command() {
        let model = BuiltinCommandModel::new();
        let state = model.detect_command("/unk");

        match state {
            CommandEntryState::Composing { filter } => {
                assert_eq!(filter, "unk");
            }
            other => panic!("Expected Composing, got {:?}", other),
        }
    }

    #[test]
    fn test_update_returns_change_flag() {
        let mut model = BuiltinCommandModel::new();
        let (_, changed) = model.update("/open");
        assert!(changed);

        let (_, changed) = model.update("/open");
        assert!(!changed);
    }

    #[test]
    fn test_reset_clears_state() {
        let mut model = BuiltinCommandModel::new();
        model.update("/open");
        assert!(model.state().is_detected());

        model.reset();
        assert_eq!(model.state(), &CommandEntryState::None);
    }
}

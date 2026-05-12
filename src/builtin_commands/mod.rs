use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{IntoElement, ParentElement, Styled};
use gpui::{MouseButton, Pixels, div, px, svg};

use crate::theme::UiTheme;

pub mod argument;
pub mod command_model;
pub mod executor;
pub mod path_completion;
pub mod path_resolver;
pub mod registry;
pub mod suggestion_engine;
pub use argument::{ArgumentError, ArgumentParser, ParsedArgument};
pub use command_model::{BuiltinCommandModel, CommandEntryState, DetectedCommand};
pub use executor::{CommandContext, CommandExecutor, CommandHandler, CommandResult};
pub use path_completion::{PathCompletionEngine, PathCompletionItem};
pub use path_resolver::{LocalPathContext, PathResolver, RemotePathContext};
pub use registry::{
    ArgumentType, Availability, BuiltinCommand, CommandHandlerId, CommandRegistry, global_registry,
};
pub use suggestion_engine::{ScoredCommand, SuggestionEngine};

/// 输入阶段：区分命令名输入和参数输入
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryPhase {
    /// 正在输入命令名（如 "/ope"）
    CommandName,
    /// 命令已确定，正在输入参数（如 "/open-file src/"）
    ArgumentInput,
}

/// 内建命令管理器，封装命令状态和建议逻辑
pub struct BuiltinCommandManager {
    /// 命令输入缓冲区
    state: Option<BuiltinCommandState>,
    /// 当前查询对应的命令建议缓存
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    /// 当前选中的命令索引
    selected_index: usize,
    /// 用户是否已取消提示（ESC）
    dismissed: bool,
    /// 当前输入阶段
    entry_phase: EntryPhase,
    /// 路径补全候选列表
    pub path_completions: Vec<PathCompletionItem>,
    /// 路径补全选中索引
    pub path_selected_index: usize,
    /// 当前已确定的命令（用于参数补全阶段）
    pub resolved_command: Option<Arc<BuiltinCommand>>,
    /// 当前工作目录缓存
    cwd: Option<PathBuf>,
}

impl BuiltinCommandManager {
    /// 创建一个空闲的内建命令管理器。
    pub fn new() -> Self {
        Self {
            state: None,
            suggestions: Rc::from(Vec::new()),
            selected_index: 0,
            dismissed: false,
            entry_phase: EntryPhase::CommandName,
            path_completions: Vec::new(),
            path_selected_index: 0,
            resolved_command: None,
            cwd: std::env::current_dir().ok(),
        }
    }

    /// 获取当前缓冲区内容
    pub fn query(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.buffer())
    }

    /// 检查是否有活跃的命令输入
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// 检查是否已取消提示
    pub fn is_dismissed(&self) -> bool {
        self.dismissed
    }

    /// 检查是否处于参数输入阶段
    pub fn is_argument_input(&self) -> bool {
        self.entry_phase == EntryPhase::ArgumentInput
    }

    /// 获取命令建议列表缓存的共享引用。
    pub fn suggestions(&self) -> Rc<[Arc<BuiltinCommand>]> {
        self.suggestions.clone()
    }

    /// 获取当前选中的命令
    pub fn selected_command(&self) -> Option<Arc<BuiltinCommand>> {
        let index = self.selected_index();
        self.suggestions.get(index).cloned()
    }

    /// 获取当前选中的路径补全项
    pub fn selected_path_completion(&self) -> Option<&PathCompletionItem> {
        self.path_completions.get(self.path_selected_index)
    }

    /// 追加输入内容。
    pub fn update(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(state) = self.state.as_mut() {
            state.push_str(text);
        } else {
            self.state = Some(BuiltinCommandState::new(text.to_string()));
        }
        // 重置取消状态和选择
        self.dismissed = false;
        self.selected_index = 0;
        self.path_selected_index = 0;
        self.refresh_state();
    }

    /// 删除最后一个字符。
    pub fn pop(&mut self) {
        let mut should_clear = false;
        if let Some(state) = self.state.as_mut() {
            state.pop();
            should_clear = state.is_empty();
        }
        if should_clear {
            self.state = None;
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            self.path_completions.clear();
        }
        self.dismissed = false;
        self.selected_index = 0;
        self.path_selected_index = 0;
        self.refresh_state();
    }

    /// 清空命令状态，完全退出内建命令模式。
    pub fn clear(&mut self) {
        self.state = None;
        self.suggestions = Rc::from(Vec::new());
        self.selected_index = 0;
        self.dismissed = false;
        self.entry_phase = EntryPhase::CommandName;
        self.path_completions.clear();
        self.path_selected_index = 0;
        self.resolved_command = None;
    }

    /// 标记为已取消（ESC）。
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.selected_index = 0;
        self.path_selected_index = 0;
    }

    /// 在建议列表中循环选择下一项。
    pub fn select_next(&mut self) {
        match self.entry_phase {
            EntryPhase::CommandName => {
                let total = self.suggestions.len();
                if total > 0 {
                    self.selected_index = (self.selected_index + 1) % total;
                }
            }
            EntryPhase::ArgumentInput => {
                let total = self.path_completions.len();
                if total > 0 {
                    self.path_selected_index = (self.path_selected_index + 1) % total;
                }
            }
        }
    }

    /// 在建议列表中循环选择上一项。
    pub fn select_previous(&mut self) {
        match self.entry_phase {
            EntryPhase::CommandName => {
                let total = self.suggestions.len();
                if total > 0 {
                    self.selected_index = if self.selected_index == 0 {
                        total - 1
                    } else {
                        self.selected_index - 1
                    };
                }
            }
            EntryPhase::ArgumentInput => {
                let total = self.path_completions.len();
                if total > 0 {
                    self.path_selected_index = if self.path_selected_index == 0 {
                        total - 1
                    } else {
                        self.path_selected_index - 1
                    };
                }
            }
        }
    }

    /// 用当前选中项替换命令名前缀，同时保留已经输入的参数部分。
    pub fn complete(&mut self) {
        let Some(selected) = self.selected_command() else {
            return;
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();
        let args = current_input
            .splitn(2, |c: char| c.is_whitespace())
            .nth(1)
            .unwrap_or("");

        let new_input = if args.is_empty() {
            selected.name.to_string()
        } else {
            format!("{} {}", selected.name, args)
        };

        state.set_buffer(new_input);
        // 重置取消状态
        self.dismissed = false;
        self.selected_index = 0;
        self.path_selected_index = 0;
        self.refresh_state();
    }

    /// 应用当前选中的路径补全。
    ///
    /// 将当前路径参数替换为选中的完整路径。
    pub fn complete_path(&mut self) {
        let Some(selected) = self.selected_path_completion().cloned() else {
            return;
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let current_input = state.buffer().to_string();

        // 分离命令名和参数
        let mut parts = current_input.splitn(2, |c: char| c.is_whitespace());
        let command_name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("");

        // 解析参数的目录前缀和文件名前缀
        let (dir_part, _file_prefix) = PathCompletionEngine::split_path_prefix(args);

        // 构建新的参数：目录前缀 + 选中的名称
        let new_arg = if selected.is_directory {
            format!("{}{}/", dir_part, selected.name)
        } else {
            format!("{}{}", dir_part, selected.name)
        };

        let new_input = if new_arg.is_empty() {
            command_name.to_string()
        } else {
            format!("{} {}", command_name, new_arg)
        };

        state.set_buffer(new_input);
        self.path_selected_index = 0;
        self.refresh_path_completions();
    }

    /// 按外部交互（如鼠标点击）直接设置选中项。
    pub fn set_selected_index(&mut self, index: usize) {
        let total = self.suggestions.len();
        self.selected_index = if total == 0 { 0 } else { index.min(total - 1) };
    }

    /// 设置路径补全选中项。
    pub fn set_path_selected_index(&mut self, index: usize) {
        let total = self.path_completions.len();
        self.path_selected_index = if total == 0 { 0 } else { index.min(total - 1) };
    }

    /// 处理一次显式选中。
    pub fn pick(&mut self, index: usize) {
        self.set_selected_index(index);
        self.complete();
    }

    /// 处理路径补全的显式选中。
    pub fn pick_path(&mut self, index: usize) {
        self.set_path_selected_index(index);
        self.complete_path();
    }

    /// 获取当前有效的选中索引。
    pub fn selected_index(&self) -> usize {
        let total = self.suggestions.len();
        if total == 0 {
            0
        } else {
            self.selected_index.min(total - 1)
        }
    }

    /// 根据当前缓冲区文本重算所有状态。
    fn refresh_state(&mut self) {
        self.refresh_suggestions();
        self.detect_entry_phase();
        if self.entry_phase == EntryPhase::ArgumentInput {
            self.refresh_path_completions();
        } else {
            self.path_completions.clear();
            self.path_selected_index = 0;
        }
    }

    /// 检测当前输入阶段。
    fn detect_entry_phase(&mut self) {
        let Some(query) = self.query() else {
            self.entry_phase = EntryPhase::CommandName;
            self.resolved_command = None;
            return;
        };

        // 检查是否已经有空格分隔的参数
        let has_argument = query.split_whitespace().nth(1).is_some();

        if has_argument {
            // 已经有参数，进入参数输入阶段
            let command_name = query.split_whitespace().next().unwrap_or("");
            if let Some(cmd) = global_registry().find(command_name) {
                self.entry_phase = EntryPhase::ArgumentInput;
                self.resolved_command = Some(cmd);
            } else {
                self.entry_phase = EntryPhase::CommandName;
                self.resolved_command = None;
            }
        } else {
            // 没有参数，检查当前输入是否匹配完整命令名
            let trimmed = query.trim();
            if let Some(cmd) = global_registry().find(trimmed) {
                // 精确匹配到命令名
                if cmd.argument != ArgumentType::None {
                    // 命令需要参数，保持命令名阶段直到用户输入空格
                    self.entry_phase = EntryPhase::CommandName;
                    self.resolved_command = Some(cmd);
                } else {
                    // 无参数命令，保持在命令名阶段
                    self.entry_phase = EntryPhase::CommandName;
                    self.resolved_command = Some(cmd);
                }
            } else {
                self.entry_phase = EntryPhase::CommandName;
                self.resolved_command = None;
            }
        }
    }

    /// 刷新命令建议列表。
    fn refresh_suggestions(&mut self) {
        self.suggestions = self
            .query()
            .map(|query| {
                let all_commands = global_registry().all_commands();
                let scored = SuggestionEngine::suggest(&all_commands, query);
                scored.into_iter().map(|s| s.command).collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into();
        self.selected_index = self.selected_index();
    }

    /// 刷新路径补全列表。
    fn refresh_path_completions(&mut self) {
        let Some(query) = self.query() else {
            self.path_completions.clear();
            return;
        };

        // 提取参数部分（仅按第一个空白字符分割，保留路径中的空格）
        let args = query
            .trim_start()
            .splitn(2, |c: char| c.is_whitespace())
            .nth(1)
            .unwrap_or("")
            .trim_start();
        if args.is_empty() {
            self.path_completions.clear();
            return;
        }

        self.path_completions = PathCompletionEngine::complete(args, self.cwd.as_deref());
        self.path_selected_index = 0;
    }

    /// 获取参数提示文本（如 "file_path"）。
    pub fn argument_hint(&self) -> Option<&'static str> {
        self.resolved_command
            .as_ref()
            .and_then(|cmd| match cmd.argument {
                ArgumentType::Optional { hint } => hint,
                ArgumentType::Required { hint } => Some(hint),
                ArgumentType::None => None,
            })
    }
}

impl Default for BuiltinCommandManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinCommandState {
    buffer: String,
}

impl BuiltinCommandState {
    /// 用初始文本创建命令缓冲区。
    pub fn new(buffer: String) -> Self {
        Self { buffer }
    }

    /// 在缓冲区末尾追加文本。
    pub fn push_str(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    /// 删除缓冲区末尾的一个字符。
    pub fn pop(&mut self) {
        self.buffer.pop();
    }

    /// 判断缓冲区是否为空。
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 返回当前缓冲区文本。
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// 替换整个缓冲内容
    pub fn set_buffer(&mut self, buffer: String) {
        self.buffer = buffer;
    }
}

/// 根据命令行首个 token 查找内建命令定义。
///
/// 只匹配命令名本身，后续参数不会参与查找。
pub fn find_builtin_command(input: &str) -> Option<Arc<BuiltinCommand>> {
    let command_name = input.split_whitespace().next()?;
    global_registry().find(command_name)
}

/// 根据当前查询生成建议列表，并按匹配分数从高到低排序。
///
/// 优先级依次是：精确匹配、前缀匹配、名称子串匹配、描述子串匹配。
pub fn command_suggestions(query: &str) -> Vec<Arc<BuiltinCommand>> {
    let all_commands = global_registry().all_commands();
    let scored = SuggestionEngine::suggest(&all_commands, query);
    scored.into_iter().map(|s| s.command).collect()
}

/// 根据当前输入和选中项，生成最终要提交的命令行。
///
/// `force_selected = true` 用于显式选择场景（如鼠标点击），允许直接用选中项覆盖当前前缀。
pub fn resolve_command_line(
    query: &str,
    selected: Option<&BuiltinCommand>,
    force_selected: bool,
) -> String {
    let Some(command_name) = query.split_whitespace().next() else {
        return query.to_string();
    };
    let Some(selected) = selected else {
        return query.to_string();
    };

    if !force_selected && !selected.name.starts_with(command_name) {
        return query.to_string();
    }

    let suffix = query.strip_prefix(command_name).unwrap_or_default();
    format!("{}{}", selected.name, suffix)
}

fn render_key_hint(key: &str, label: Option<&str>, theme: UiTheme) -> impl IntoElement {
    let key = key.to_string();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .px(px(7.0))
                .py(px(2.0))
                .bg(theme.elevated_background)
                .rounded(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.text)
                .child(key),
        )
        .when_some(label, |this, label| {
            this.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(label.to_string()),
            )
        })
}

/// 渲染内建命令建议浮层。
///
/// `on_pick` 由外层提供，用来响应鼠标选中某一条建议。
pub fn render_overlay<F>(
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    selected_index: usize,
    top: Pixels,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);

    div()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .top(top)
        .flex()
        .flex_col()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(div().h(px(1.0)).bg(theme.border))
        .gap_1()
        .child(
            div()
                .px(px(6.0))
                .flex()
                .items_center()
                .gap_2()
                .child(render_key_hint("↑↓", Some("navigate"), theme))
                .child(render_key_hint("esc", Some("dismiss"), theme)),
        )
        .child(div().h(px(1.0)).bg(theme.border))
        .child(
            div()
                .max_h(px(420.0))
                .children(
                    suggestions
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(move |(index, cmd)| {
                            let on_pick = on_pick.clone();
                            let is_selected = index == selected_index;
                            div()
                                .w_full()
                                .h_6()
                                .px(px(6.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .bg(if is_selected {
                                    theme.accent.opacity(if is_dark { 0.16 } else { 0.09 })
                                } else {
                                    theme.panel_background
                                })
                                .when(!is_selected, |this| {
                                    this.hover(move |style| style.bg(theme.tab_hover_background))
                                })
                                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                    cx.stop_propagation();
                                    on_pick(index, window, cx);
                                })
                                .child(
                                    div()
                                        .w(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            svg()
                                                .size(px(10.0))
                                                .text_color(theme.icon_muted)
                                                .path(cmd.icon.path()),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(390.0))
                                        .text_size(px(13.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(cmd.name),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(13.0))
                                        .text_color(theme.text_muted)
                                        .child(cmd.description),
                                )
                        }),
                ),
        )
}

/// 渲染路径补全浮层。
///
/// `on_pick` 由外层提供，用来响应鼠标选中某一条路径建议。
pub fn render_path_overlay<F>(
    completions: &[PathCompletionItem],
    selected_index: usize,
    top: Pixels,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);

    div()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .top(top)
        .flex()
        .flex_col()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(div().h(px(1.0)).bg(theme.border))
        .gap_1()
        .child(
            div()
                .px(px(6.0))
                .flex()
                .items_center()
                .gap_2()
                .child(render_key_hint("↑↓", Some("navigate"), theme))
                .child(render_key_hint("tab", Some("complete"), theme))
                .child(render_key_hint("esc", Some("dismiss"), theme)),
        )
        .child(div().h(px(1.0)).bg(theme.border))
        .child(
            div()
                .max_h(px(420.0))
                .children(
                    completions
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(move |(index, item)| {
                            let on_pick = on_pick.clone();
                            let is_selected = index == selected_index;
                            let icon_path = if item.is_directory {
                                crate::assets::Icon::Folder.path()
                            } else {
                                crate::assets::Icon::Screen.path()
                            };
                            div()
                                .w_full()
                                .h_6()
                                .px(px(6.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .bg(if is_selected {
                                    theme.accent.opacity(if is_dark { 0.16 } else { 0.09 })
                                } else {
                                    theme.panel_background
                                })
                                .when(!is_selected, |this| {
                                    this.hover(move |style| style.bg(theme.tab_hover_background))
                                })
                                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                    cx.stop_propagation();
                                    on_pick(index, window, cx);
                                })
                                .child(
                                    div()
                                        .w(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            svg()
                                                .size(px(10.0))
                                                .text_color(theme.icon_muted)
                                                .path(icon_path),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(item.name.clone()),
                                )
                                .when(!item.is_directory, |this| {
                                    this.child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(12.0))
                                            .text_color(theme.text_muted)
                                            .child(item.path.to_string_lossy().into_owned()),
                                    )
                                })
                        }),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{BuiltinCommandManager, find_builtin_command, resolve_command_line};

    #[test]
    fn parses_builtin_open_command() {
        assert_eq!(
            find_builtin_command("/open").map(|cmd| cmd.name),
            Some("/open")
        );
        assert_eq!(
            find_builtin_command("/open my-target").map(|cmd| cmd.name),
            Some("/open")
        );
    }

    #[test]
    fn leaves_unknown_slash_command_to_shell() {
        assert_eq!(find_builtin_command("/unknown"), None);
        assert_eq!(find_builtin_command("ls"), None);
    }

    #[test]
    fn fake_builtin_command_specs_are_available_for_prompt() {
        let registry = super::global_registry();
        assert!(registry.find("/open-file").is_some());
        assert!(registry.find("/open-repo").is_some());
    }

    #[test]
    fn resolves_selected_command_when_query_is_a_prefix() {
        let registry = super::global_registry();
        let selected = registry.find("/open");
        assert_eq!(
            resolve_command_line("/op target", selected.as_deref(), false),
            "/open target"
        );
    }

    #[test]
    fn keeps_unknown_query_when_selection_only_comes_from_fallback() {
        let registry = super::global_registry();
        let selected = registry.find("/open");
        assert_eq!(
            resolve_command_line("/unknown target", selected.as_deref(), false),
            "/unknown target"
        );
    }

    #[test]
    fn forces_selected_command_for_explicit_pick() {
        let registry = super::global_registry();
        let selected = registry.find("/open-file");
        assert_eq!(
            resolve_command_line("/unknown target", selected.as_deref(), true),
            "/open-file target"
        );
    }

    #[test]
    fn pick_completes_selected_command_and_keeps_args() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/op target");
        manager.pick(0);

        assert_eq!(manager.query(), Some("/open target"));
        assert_eq!(
            manager.selected_command().map(|cmd| cmd.name),
            Some("/open")
        );
        assert!(!manager.is_dismissed());
    }

    #[test]
    fn pick_uses_clicked_index() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/op target");
        manager.pick(1);

        assert_eq!(manager.query(), Some("/open-file target"));
        assert_eq!(
            manager.selected_command().map(|cmd| cmd.name),
            Some("/open-file")
        );
    }
}

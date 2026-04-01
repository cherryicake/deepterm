use gpui::prelude::*;
use gpui::{IntoElement, ParentElement, Styled};
use gpui::{MouseButton, Pixels, div, px, svg};

use crate::assets::Icon;
use crate::theme::UiTheme;

/// 内建命令管理器，封装命令状态和建议逻辑
pub struct BuiltinCommandManager {
    /// 命令输入缓冲区
    state: Option<BuiltinCommandState>,
    /// 当前选中的命令索引
    selected_index: usize,
    /// 用户是否已取消提示（ESC）
    dismissed: bool,
}

impl BuiltinCommandManager {
    pub fn new() -> Self {
        Self {
            state: None,
            selected_index: 0,
            dismissed: false,
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

    /// 获取建议列表
    pub fn suggestions(&self) -> Vec<&'static BuiltinCommandSpec> {
        let Some(query) = self.query() else {
            return Vec::new();
        };
        command_suggestions(query)
    }

    /// 获取当前选中的命令
    pub fn selected_command(&self) -> Option<&'static BuiltinCommandSpec> {
        let suggestions = self.suggestions();
        let index = self.selected_index.min(suggestions.len().saturating_sub(1));
        suggestions.get(index).copied()
    }

    /// 更新输入内容
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
    }

    /// 删除最后一个字符
    pub fn pop(&mut self) {
        let mut should_clear = false;
        if let Some(state) = self.state.as_mut() {
            state.pop();
            should_clear = state.is_empty();
        }
        if should_clear {
            self.state = None;
        }
        self.dismissed = false;
        self.selected_index = 0;
    }

    /// 清空命令状态
    pub fn clear(&mut self) {
        self.state = None;
        self.selected_index = 0;
    }

    /// 标记为已取消（ESC）
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.selected_index = 0;
    }

    /// 选择下一个命令
    pub fn select_next(&mut self) {
        let total = self.suggestions().len();
        if total > 0 {
            self.selected_index = (self.selected_index + 1) % total;
        }
    }

    /// 选择上一个命令
    pub fn select_previous(&mut self) {
        let total = self.suggestions().len();
        if total > 0 {
            self.selected_index = if self.selected_index == 0 {
                total - 1
            } else {
                self.selected_index - 1
            };
        }
    }

    /// Tab 补全：将选中的命令替换到输入中，保留参数
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
    }

    /// 获取当前选中的索引
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// 获取缓冲区引用（用于渲染）
    pub fn state(&self) -> Option<&BuiltinCommandState> {
        self.state.as_ref()
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
    pub fn new(buffer: String) -> Self {
        Self { buffer }
    }

    pub fn push_str(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    pub fn pop(&mut self) {
        self.buffer.pop();
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// 替换整个缓冲内容
    pub fn set_buffer(&mut self, buffer: String) {
        self.buffer = buffer;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinCommandHandler {
    OpenFileBrowser,
    LogOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinCommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub preview: &'static str,
    pub icon: Icon,
    pub handler: BuiltinCommandHandler,
}

pub const BUILTIN_COMMAND_OVERLAY_PADDING: f32 = 4.0;

pub const BUILTIN_COMMAND_SPECS: &[BuiltinCommandSpec] = &[
    BuiltinCommandSpec {
        name: "/open",
        description: "Open DeepTerm's File Browser.",
        preview: "Open DeepTerm's File Browser",
        icon: Icon::Terminal,
        handler: BuiltinCommandHandler::OpenFileBrowser,
    },
    BuiltinCommandSpec {
        name: "/open-file",
        description: "Open a file in DeepTerm's editor.",
        preview: "Open a file in DeepTerm's code editor",
        icon: Icon::Screen,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-repo",
        description: "Switch to another indexed repository.",
        preview: "Switch to another indexed repository",
        icon: Icon::Host,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-skill",
        description: "Inspect an installed skill definition.",
        preview: "Open a skill markdown file in DeepTerm",
        icon: Icon::KeyShield,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-rules",
        description: "View global and project agent rules.",
        preview: "View all global and project rules",
        icon: Icon::SettingsHub,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-code-review",
        description: "Open the code review workspace.",
        preview: "Open the code review workspace",
        icon: Icon::Plus,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-mcp-servers",
        description: "Inspect configured MCP servers.",
        preview: "Open MCP servers",
        icon: Icon::Server,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-project-rules",
        description: "Open the project rules file (AGENTS.md).",
        preview: "Open the project rules file (AGENTS.md)",
        icon: Icon::Host,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/new-component",
        description: "Create new GPUI components. Use when building components or UI elements.",
        preview: "Create new GPUI components",
        icon: Icon::Plus,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/generate-component-story",
        description: "Create story examples for components.",
        preview: "Create story examples for components",
        icon: Icon::Loader,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/generate-component-documentation",
        description: "Generate documentation for new components.",
        preview: "Generate documentation for new components",
        icon: Icon::Loader,
        handler: BuiltinCommandHandler::LogOnly,
    },
];

pub fn find_builtin_command(input: &str) -> Option<&'static BuiltinCommandSpec> {
    let command_name = input.split_whitespace().next()?;
    BUILTIN_COMMAND_SPECS
        .iter()
        .find(|spec| spec.name == command_name)
}

pub fn command_suggestions(query: &str) -> Vec<&'static BuiltinCommandSpec> {
    let command_prefix = query.split_whitespace().next().unwrap_or(query);
    // 去掉前导 '/' 用于子串匹配
    let query_for_substring = command_prefix.strip_prefix('/').unwrap_or(command_prefix);
    let query_lower = query_for_substring.to_lowercase();

    let mut scored: Vec<(i32, &BuiltinCommandSpec)> = BUILTIN_COMMAND_SPECS
        .iter()
        .filter_map(|spec| {
            let name = spec.name;
            let name_lower = name.to_lowercase();
            let name_without_slash = name_lower.strip_prefix('/').unwrap_or(&name_lower);
            let desc_lower = spec.description.to_lowercase();

            let score = if name == command_prefix {
                // 精确匹配
                10_000
            } else if name.starts_with(command_prefix) {
                // 前缀匹配：匹配前缀越长分数越高，命令名越短分数越高
                let prefix_len = command_prefix.len() as i32;
                let name_len = name.len() as i32;
                5_000 + prefix_len * 100 + (100 - name_len)
            } else if name_without_slash.contains(&query_lower) {
                // 名称子串匹配（去掉 '/' 后）
                let name_len = name.len() as i32;
                1_000 + (100 - name_len)
            } else if desc_lower.contains(&query_lower) {
                // 描述子串匹配
                100
            } else {
                return None;
            };
            Some((score, spec))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, spec)| spec).collect()
}

pub fn resolve_command_line(
    query: &str,
    selected: Option<&BuiltinCommandSpec>,
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

/// Renders the builtin command overlay with interactive selection
pub fn render_overlay(
    suggestions: Vec<&'static BuiltinCommandSpec>,
    selected_index: usize,
    top: Pixels,
    theme: UiTheme,
    is_dark: bool,
) -> impl IntoElement {
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
                        .into_iter()
                        .enumerate()
                        .map(move |(index, spec)| {
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
                                                .path(spec.icon.path()),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(390.0))
                                        .text_size(px(13.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(spec.name),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(13.0))
                                        .text_color(theme.text_muted)
                                        .child(spec.description),
                                )
                        }),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN_COMMAND_SPECS, find_builtin_command, resolve_command_line};

    #[test]
    fn parses_builtin_open_command() {
        assert_eq!(
            find_builtin_command("/open").map(|spec| spec.name),
            Some("/open")
        );
        assert_eq!(
            find_builtin_command("/open my-target").map(|spec| spec.name),
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
        assert!(
            BUILTIN_COMMAND_SPECS
                .iter()
                .any(|spec| spec.name == "/open-file")
        );
        assert!(
            BUILTIN_COMMAND_SPECS
                .iter()
                .any(|spec| spec.name == "/open-repo")
        );
    }

    #[test]
    fn resolves_selected_command_when_query_is_a_prefix() {
        let selected = BUILTIN_COMMAND_SPECS
            .iter()
            .find(|spec| spec.name == "/open");
        assert_eq!(
            resolve_command_line("/op target", selected, false),
            "/open target"
        );
    }

    #[test]
    fn keeps_unknown_query_when_selection_only_comes_from_fallback() {
        let selected = BUILTIN_COMMAND_SPECS
            .iter()
            .find(|spec| spec.name == "/open");
        assert_eq!(
            resolve_command_line("/unknown target", selected, false),
            "/unknown target"
        );
    }

    #[test]
    fn forces_selected_command_for_explicit_pick() {
        let selected = BUILTIN_COMMAND_SPECS
            .iter()
            .find(|spec| spec.name == "/open-file");
        assert_eq!(
            resolve_command_line("/unknown target", selected, true),
            "/open-file target"
        );
    }
}

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{IntoElement, ParentElement, Styled};
use gpui::{MouseButton, Pixels, div, px, svg};

use crate::assets::Icon;
use crate::theme::UiTheme;

/// 内建命令管理器，封装命令状态和建议逻辑
pub struct BuiltinCommandManager {
    /// 命令输入缓冲区
    state: Option<BuiltinCommandState>,
    /// 当前查询对应的建议缓存，避免每次读取都重新匹配和排序
    suggestions: Rc<[&'static BuiltinCommandSpec]>,
    /// 当前选中的命令索引
    selected_index: usize,
    /// 用户是否已取消提示（ESC）
    dismissed: bool,
}

impl BuiltinCommandManager {
    /// 创建一个空闲的内建命令管理器。
    pub fn new() -> Self {
        Self {
            state: None,
            suggestions: Rc::from(Vec::new()),
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

    /// 获取建议列表缓存的共享引用。
    ///
    /// 返回 `Rc` 仅复制句柄，不会复制底层建议数组。
    pub fn suggestions(&self) -> Rc<[&'static BuiltinCommandSpec]> {
        self.suggestions.clone()
    }

    /// 获取当前选中的命令
    pub fn selected_command(&self) -> Option<&'static BuiltinCommandSpec> {
        let index = self.selected_index();
        self.suggestions.get(index).copied()
    }

    /// 追加输入内容。
    ///
    /// 一旦有新输入，就重新展示建议列表，并把选中项回到第一条。
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
        self.refresh_suggestions();
    }

    /// 删除最后一个字符。
    ///
    /// 当缓冲区删空时，同时结束当前内建命令输入状态。
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
        self.refresh_suggestions();
    }

    /// 清空命令状态，完全退出内建命令模式。
    pub fn clear(&mut self) {
        self.state = None;
        self.suggestions = Rc::from(Vec::new());
        self.selected_index = 0;
    }

    /// 标记为已取消（ESC）。
    ///
    /// 取消后保留缓冲区内容，后续回车会把文本原样发给 shell。
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.selected_index = 0;
    }

    /// 在建议列表中循环选择下一项。
    pub fn select_next(&mut self) {
        let total = self.suggestions.len();
        if total > 0 {
            self.selected_index = (self.selected_index + 1) % total;
        }
    }

    /// 在建议列表中循环选择上一项。
    pub fn select_previous(&mut self) {
        let total = self.suggestions.len();
        if total > 0 {
            self.selected_index = if self.selected_index == 0 {
                total - 1
            } else {
                self.selected_index - 1
            };
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
        self.refresh_suggestions();
    }

    /// 按外部交互（如鼠标点击）直接设置选中项。
    pub fn set_selected_index(&mut self, index: usize) {
        let total = self.suggestions.len();
        self.selected_index = if total == 0 { 0 } else { index.min(total - 1) };
    }

    /// 处理一次显式选中。
    ///
    /// 这等价于“先选中某条建议，再执行一次 Tab 补全”。
    pub fn pick(&mut self, index: usize) {
        self.set_selected_index(index);
        self.complete();
    }

    /// 获取当前有效的选中索引。
    ///
    /// 这里会根据当前建议数量做一次收敛，避免索引越界。
    pub fn selected_index(&self) -> usize {
        let total = self.suggestions.len();
        if total == 0 {
            0
        } else {
            self.selected_index.min(total - 1)
        }
    }

    /// 根据当前缓冲区文本重算建议缓存，并收敛选中索引。
    fn refresh_suggestions(&mut self) {
        self.suggestions = self
            .query()
            .map(command_suggestions)
            .unwrap_or_default()
            .into();
        self.selected_index = self.selected_index();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinCommandHandler {
    OpenFileBrowser,
    LogOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinCommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub icon: Icon,
    pub handler: BuiltinCommandHandler,
}

pub const BUILTIN_COMMAND_SPECS: &[BuiltinCommandSpec] = &[
    BuiltinCommandSpec {
        name: "/open",
        description: "Open DeepTerm's File Browser.",
        icon: Icon::Folder,
        handler: BuiltinCommandHandler::OpenFileBrowser,
    },
    BuiltinCommandSpec {
        name: "/open-file",
        description: "Open a file in DeepTerm's editor.",
        icon: Icon::Screen,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-repo",
        description: "Switch to another indexed repository.",
        icon: Icon::Host,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-skill",
        description: "Inspect an installed skill definition.",
        icon: Icon::KeyShield,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-rules",
        description: "View global and project agent rules.",
        icon: Icon::SettingsHub,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-code-review",
        description: "Open the code review workspace.",
        icon: Icon::Plus,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-mcp-servers",
        description: "Inspect configured MCP servers.",
        icon: Icon::Server,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/open-project-rules",
        description: "Open the project rules file (AGENTS.md).",
        icon: Icon::Host,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/new-component",
        description: "Create new GPUI components. Use when building components or UI elements.",
        icon: Icon::Plus,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/generate-component-story",
        description: "Create story examples for components.",
        icon: Icon::Loader,
        handler: BuiltinCommandHandler::LogOnly,
    },
    BuiltinCommandSpec {
        name: "/generate-component-documentation",
        description: "Generate documentation for new components.",
        icon: Icon::Loader,
        handler: BuiltinCommandHandler::LogOnly,
    },
];

/// 根据命令行首个 token 查找内建命令定义。
///
/// 只匹配命令名本身，后续参数不会参与查找。
pub fn find_builtin_command(input: &str) -> Option<&'static BuiltinCommandSpec> {
    let command_name = input.split_whitespace().next()?;
    BUILTIN_COMMAND_SPECS
        .iter()
        .find(|spec| spec.name == command_name)
}

/// 根据当前查询生成建议列表，并按匹配分数从高到低排序。
///
/// 优先级依次是：精确匹配、前缀匹配、名称子串匹配、描述子串匹配。
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

/// 根据当前输入和选中项，生成最终要提交的命令行。
///
/// `force_selected = true` 用于显式选择场景（如鼠标点击），允许直接用选中项覆盖当前前缀。
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

/// 渲染内建命令建议浮层。
///
/// `on_pick` 由外层提供，用来响应鼠标选中某一条建议。
pub fn render_overlay<F>(
    suggestions: Rc<[&'static BuiltinCommandSpec]>,
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
                        .copied()
                        .enumerate()
                        .map(move |(index, spec)| {
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
    use super::{
        BUILTIN_COMMAND_SPECS, BuiltinCommandManager, find_builtin_command, resolve_command_line,
    };

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

    #[test]
    fn pick_completes_selected_command_and_keeps_args() {
        let mut manager = BuiltinCommandManager::new();
        manager.update("/op target");
        manager.pick(0);

        assert_eq!(manager.query(), Some("/open target"));
        assert_eq!(
            manager.selected_command().map(|spec| spec.name),
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
            manager.selected_command().map(|spec| spec.name),
            Some("/open-file")
        );
    }
}

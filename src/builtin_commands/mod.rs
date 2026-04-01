use gpui::prelude::*;
use gpui::{AnyElement, Pixels, div, px, svg};
use gpui::{IntoElement, ParentElement, Styled};

use crate::assets::Icon;
use crate::theme::UiTheme;

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinCommandHandler {
    OpenPicker,
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
        description: "Open DeepTerm's SSH picker.",
        preview: "Open DeepTerm's SSH picker",
        icon: Icon::Terminal,
        handler: BuiltinCommandHandler::OpenPicker,
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
    let mut suggestions: Vec<_> = BUILTIN_COMMAND_SPECS
        .iter()
        .filter(|spec| spec.name.starts_with(command_prefix))
        .collect();

    if suggestions.is_empty() && command_prefix.starts_with('/') {
        suggestions = BUILTIN_COMMAND_SPECS
            .iter()
            .filter(|spec| spec.name.starts_with("/open"))
            .collect();
    }

    suggestions
}

pub fn render_overlay(
    query: &str,
    suggestions: &[&'static BuiltinCommandSpec],
    _left: Pixels,
    top: Pixels,
    theme: UiTheme,
    is_dark: bool,
) -> AnyElement {
    div()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .top(top)
        .flex()
        .flex_col()
        .child(div().h(px(1.0)).bg(theme.border))
        .gap_1()
        .child(
            div()
                .px(px(6.0))
                .flex()
                .items_center()
                .gap_1()
                .child(render_key_hint("↑", None, theme))
                .child(render_key_hint("↓", Some("to navigate"), theme))
                .child(render_key_hint("esc", Some("to dismiss"), theme)),
        )
        .child(div().h(px(1.0)).bg(theme.border))
        .child(
            div()
                .max_h(px(420.0))
                .children(suggestions.iter().enumerate().map(|(index, spec)| {
                    let is_selected = index == 0;
                    div()
                        .w_full()
                        .h_6()
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(6.0))
                        .bg(if is_selected {
                            theme.accent.opacity(if is_dark { 0.16 } else { 0.09 })
                        } else {
                            theme.panel_background
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
                })),
        )
        .into_any_element()
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
                .rounded(px(7.0))
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

#[cfg(test)]
mod tests {
    use super::{BUILTIN_COMMAND_SPECS, find_builtin_command};

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
}

//! SSH 连接选择器弹窗
//!
//! 提供类似 Command Palette 的界面用于选择 SSH 连接配置。
//! 支持搜索过滤、键盘导航和回车确认。

use gpui::prelude::*;
use gpui::{
    actions, div, px, rems, rgb, svg, App, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Styled,
    Window,
};

use crate::assets::Icon;
use crate::ssh::config::{SshConnectionConfig, SshGlobalConfig};
use crate::workspace::workspace::Workspace;

actions!(ssh_picker, [Dismiss, SelectNext, SelectPrevious, Confirm]);

// 颜色常量
const COLOR_BG_DEEP: u32 = 0x11111b;
const COLOR_BG_SELECTED: u32 = 0x313244;
const COLOR_BG_HOVER: u32 = 0x252538;
const COLOR_BG_PANEL: u32 = 0x181825;
const COLOR_TEXT_PRIMARY: u32 = 0x9aa3b8;
const COLOR_TEXT_SECONDARY: u32 = 0x5d5d7c;
const COLOR_TEXT_TERTIARY: u32 = 0x666688;
const COLOR_TEXT_SHORTCUT: u32 = 0x4a4a6a;

// 尺寸常量
const ITEM_HEIGHT: f32 = 30.0;
const PANEL_WIDTH: f32 = 520.0;
const SEARCH_HEIGHT: f32 = 36.0;

/// 本地终端选项
#[derive(Clone, Debug)]
pub struct LocalTerminalOption {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub icon: Icon,
}

impl LocalTerminalOption {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        icon: Icon,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            icon,
        }
    }
}

/// 解码 WSL 的 UTF-16LE 输出
fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 {
        let utf16: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// 检测命令是否可用
fn is_command_available(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 扫描 Windows 本地终端
#[cfg(windows)]
fn scan_windows_terminals() -> Vec<LocalTerminalOption> {
    let mut options = Vec::new();

    // PowerShell（默认首选）
    if is_command_available("powershell.exe", &["-Command", "echo ok"]) {
        options.push(LocalTerminalOption::new(
            "PowerShell",
            "powershell.exe",
            vec![],
            Icon::Terminal,
        ));
    }

    // PowerShell Core
    if is_command_available("pwsh.exe", &["-Command", "echo ok"]) {
        options.push(LocalTerminalOption::new(
            "PowerShell Core",
            "pwsh.exe",
            vec![],
            Icon::Terminal,
        ));
    }

    // CMD 终端
    options.push(LocalTerminalOption::new(
        "CMD",
        "cmd.exe",
        vec![],
        Icon::Terminal,
    ));

    // WSL 发行版
    if let Ok(output) = std::process::Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .output()
    {
        if output.status.success() {
            let output_str = decode_wsl_output(&output.stdout);
            for line in output_str.lines() {
                let distro = line.trim().replace('\0', "");
                if !distro.is_empty() {
                    options.push(LocalTerminalOption::new(
                        format!("WSL: {distro}"),
                        "wsl.exe",
                        vec!["~".to_string(), "-d".to_string(), distro],
                        Icon::Linux,
                    ));
                }
            }
        }
    }

    options
}

/// 扫描 Unix 本地终端
#[cfg(not(windows))]
fn scan_unix_terminals() -> Vec<LocalTerminalOption> {
    let mut options = Vec::new();

    // 默认 shell
    if let Ok(shell) = std::env::var("SHELL") {
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Shell".to_string());
        options.push(LocalTerminalOption::new(
            shell_name,
            shell,
            vec![],
            Icon::Terminal,
        ));
    } else {
        options.push(LocalTerminalOption::new(
            "Shell",
            "/bin/sh",
            vec![],
            Icon::Terminal,
        ));
    }

    // 常见 shell
    for shell in ["bash", "zsh", "fish"] {
        if let Ok(output) = std::process::Command::new("which").arg(shell).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    options.push(LocalTerminalOption::new(
                        shell.to_string(),
                        path,
                        vec![],
                        Icon::Terminal,
                    ));
                }
            }
        }
    }

    options
}

/// SSH 连接选择器
pub struct SshPicker {
    focus_handle: FocusHandle,
    query: String,
    configs: Vec<SshConnectionConfig>,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    workspace: Option<Entity<Workspace>>,
    pub visible: bool,
    local_terminal_options: Vec<LocalTerminalOption>,
    filtered_local_indices: Vec<usize>,
}

impl SshPicker {
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle();
        let configs = Self::load_configs();
        let filtered_indices = (0..configs.len()).collect();
        let local_terminal_options = Self::scan_local_terminals();
        let filtered_local_indices = (0..local_terminal_options.len()).collect();

        Self {
            focus_handle,
            query: String::new(),
            configs,
            filtered_indices,
            selected_index: 0,
            workspace: None,
            visible: false,
            local_terminal_options,
            filtered_local_indices,
        }
    }

    pub fn with_workspace(mut self, workspace: Entity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    fn load_configs() -> Vec<SshConnectionConfig> {
        SshGlobalConfig::load()
            .map(|config| config.saved_connections)
            .unwrap_or_default()
    }

    fn scan_local_terminals() -> Vec<LocalTerminalOption> {
        #[cfg(windows)]
        {
            scan_windows_terminals()
        }
        #[cfg(not(windows))]
        {
            scan_unix_terminals()
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        self.query.clear();
        self.configs = Self::load_configs();
        self.filtered_indices = (0..self.configs.len()).collect();
        self.selected_index = 0;
        self.local_terminal_options = Self::scan_local_terminals();
        self.filtered_local_indices = (0..self.local_terminal_options.len()).collect();

        cx.defer_in(window, |this, window, cx| {
            window.focus(&this.focus_handle, cx);
        });
        cx.notify();
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        cx.notify();
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(cx);
        } else {
            self.show(window, cx);
        }
    }

    fn update_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query.to_lowercase();
        self.filter_items();
        self.selected_index = 0;
        cx.notify();
    }

    fn filter_items(&mut self) {
        // 过滤本地终端选项
        self.filtered_local_indices = if self.query.is_empty() {
            (0..self.local_terminal_options.len()).collect()
        } else {
            self.local_terminal_options
                .iter()
                .enumerate()
                .filter(|(_, opt)| opt.name.to_lowercase().contains(&self.query))
                .map(|(i, _)| i)
                .collect()
        };

        // 过滤 Host 列表
        self.filtered_indices = if self.query.is_empty() {
            (0..self.configs.len()).collect()
        } else {
            self.configs
                .iter()
                .enumerate()
                .filter(|(_, config)| {
                    let query = &self.query;
                    config.host.to_lowercase().contains(query)
                        || config.username.to_lowercase().contains(query)
                        || config
                            .nickname
                            .as_ref()
                            .is_some_and(|n| n.to_lowercase().contains(query))
                })
                .map(|(i, _)| i)
                .collect()
        };
    }

    fn total_items(&self) -> usize {
        self.filtered_local_indices.len() + self.filtered_indices.len()
    }

    fn select_next(&mut self, cx: &mut Context<Self>) {
        let total = self.total_items();
        if total > 0 {
            self.selected_index = (self.selected_index + 1) % total;
            cx.notify();
        }
    }

    fn select_previous(&mut self, cx: &mut Context<Self>) {
        let total = self.total_items();
        if total > 0 {
            self.selected_index = if self.selected_index == 0 {
                total - 1
            } else {
                self.selected_index - 1
            };
            cx.notify();
        }
    }

    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        self.hide(cx);

        let local_count = self.filtered_local_indices.len();

        if self.selected_index < local_count {
            // 本地终端选项
            if let Some(&idx) = self.filtered_local_indices.get(self.selected_index) {
                if let Some(option) = self.local_terminal_options.get(idx) {
                    let (command, args) = (option.command.clone(), option.args.clone());
                    workspace.update(cx, |w, cx| {
                        w.create_local_terminal_with_command(&command, &args, window, cx)
                    });
                }
            }
        } else {
            // Host 列表
            let host_index = self.selected_index - local_count;
            if let Some(&idx) = self.filtered_indices.get(host_index) {
                if let Some(config) = self.configs.get(idx) {
                    let config = config.clone();
                    workspace.update(cx, |w, cx| {
                        w.create_ssh_terminal_with_config(&config, window, cx)
                    });
                }
            }
        }
    }

    fn handle_input(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "down" => self.select_next(cx),
            "up" => self.select_previous(cx),
            "enter" | "return" => self.confirm_selection(window, cx),
            "escape" => self.hide(cx),
            "backspace" => {
                if !self.query.is_empty() {
                    let new_query = self.query[..self.query.len() - 1].to_string();
                    self.update_query(new_query, cx);
                }
            }
            _ => {
                if let Some(ref text) = event.keystroke.key_char {
                    let new_query = format!("{}{}", self.query, text);
                    self.update_query(new_query, cx);
                }
            }
        }
    }

    fn get_os_icon(host: &str, nickname: Option<&str>) -> Icon {
        let text = nickname.unwrap_or(host).to_lowercase();
        if text.contains("ubuntu")
            || text.contains("centos")
            || text.contains("debian")
            || text.contains("fedora")
            || text.contains("arch")
            || text.contains("alpine")
            || text.contains("linux")
        {
            Icon::Linux
        } else if text.contains("windows")
            || text.contains("win")
            || text.contains("mac")
            || text.contains("darwin")
            || text.contains("apple")
        {
            Icon::Screen
        } else {
            Icon::Server
        }
    }

    fn render_search(&self) -> impl IntoElement {
        div().px(px(12.0)).py(px(12.0)).child(
            div()
                .h(px(SEARCH_HEIGHT))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .bg(rgb(COLOR_BG_DEEP))
                .rounded(px(8.0))
                .child(
                    div().w(px(16.0)).h(px(16.0)).child(
                        svg()
                            .size(px(16.0))
                            .text_color(rgb(COLOR_TEXT_TERTIARY))
                            .path(Icon::MagnifyingGlass.path()),
                    ),
                )
                .child(div().flex_1().child(if self.query.is_empty() {
                    div()
                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                        .text_size(px(14.0))
                        .child("Search hosts or tabs")
                        .into_any_element()
                } else {
                    div()
                        .text_color(rgb(COLOR_TEXT_PRIMARY))
                        .text_size(px(14.0))
                        .child(self.query.clone())
                        .into_any_element()
                }))
                .child(
                    div()
                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                        .text_size(px(12.0))
                        .child("⌘+K"),
                ),
        )
    }

    fn render_section_label(label: &str, shortcut: Option<&str>) -> impl IntoElement {
        div()
            .px(px(16.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_color(rgb(COLOR_TEXT_SECONDARY))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label.to_string()),
            )
            .when_some(shortcut, |this, s| {
                this.child(
                    div()
                        .text_color(rgb(COLOR_TEXT_SHORTCUT))
                        .text_size(px(11.0))
                        .child(s.to_string()),
                )
            })
    }

    fn render_list_item_content(
        icon: Icon,
        name: impl Into<String>,
        is_selected: bool,
        id: impl Into<String>,
        item_index: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id.into())
            .mx(px(8.0))
            .h(px(ITEM_HEIGHT))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .when(is_selected, |this| this.bg(rgb(COLOR_BG_SELECTED)))
            .when(!is_selected, |this| {
                this.hover(|s| s.bg(rgb(COLOR_BG_HOVER)))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_index = item_index;
                this.confirm_selection(window, cx);
            }))
            .child(
                div()
                    .w(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(16.0))
                            .text_color(rgb(COLOR_TEXT_PRIMARY))
                            .path(icon.path()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(rgb(COLOR_TEXT_PRIMARY))
                    .text_size(px(14.0))
                    .child(name.into()),
            )
    }

    fn render_local_terminal_item(
        &self,
        item_index: usize,
        option_index: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let option = &self.local_terminal_options[option_index];
        Self::render_list_item_content(
            option.icon,
            option.name.clone(),
            item_index == self.selected_index,
            format!("local-terminal-{option_index}"),
            item_index,
            cx,
        )
    }

    fn render_ssh_item(
        &self,
        item_index: usize,
        config_index: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let config = &self.configs[config_index];
        let local_count = self.filtered_local_indices.len();
        let actual_index = item_index + local_count;
        let icon = Self::get_os_icon(&config.host, config.nickname.as_deref());

        Self::render_list_item_content(
            icon,
            config.display_name(),
            actual_index == self.selected_index,
            format!("ssh-item-{}", config.id()),
            actual_index,
            cx,
        )
    }

    fn render_key_hint(key: &str, label: &str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .px(px(4.0))
                    .py(px(2.0))
                    .bg(rgb(COLOR_BG_DEEP))
                    .rounded(px(4.0))
                    .text_color(rgb(COLOR_TEXT_PRIMARY))
                    .text_size(px(11.0))
                    .child(key.to_string()),
            )
            .child(
                div()
                    .text_color(rgb(COLOR_TEXT_SECONDARY))
                    .text_size(px(11.0))
                    .child(label.to_string()),
            )
    }
}

impl Focusable for SshPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SshPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }

        div()
            .absolute()
            .inset_0()
            .bg(gpui::rgba(0x00000080))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _cx, cx| this.hide(cx)),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(120.0))
                    .child(
                        div()
                            .w(px(PANEL_WIDTH))
                            .flex()
                            .flex_col()
                            .bg(rgb(COLOR_BG_PANEL))
                            .rounded(px(12.0))
                            .shadow_lg()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .key_context("SshPicker")
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                this.handle_input(event, window, cx);
                            }))
                            .track_focus(&self.focus_handle)
                            .child(self.render_search())
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(COLOR_BG_SELECTED)))
                            // Local connect 区域
                            .when(!self.filtered_local_indices.is_empty(), |this| {
                                this.child(Self::render_section_label("Local connect", None))
                                    .child(div().pb(px(8.0)).children(
                                        self.filtered_local_indices.iter().enumerate().map(
                                            |(item_index, &option_index)| {
                                                self.render_local_terminal_item(
                                                    item_index,
                                                    option_index,
                                                    cx,
                                                )
                                                .into_any_element()
                                            },
                                        ),
                                    ))
                                    .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(COLOR_BG_SELECTED)))
                            })
                            // Host 区域
                            .child(Self::render_section_label("Host", None))
                            .child(
                                div()
                                    .pb(px(8.0))
                                    .id("ssh-hosts-list")
                                    .max_h(rems(16.))
                                    .overflow_y_scroll()
                                    .children(self.filtered_indices.iter().enumerate().map(
                                        |(item_index, &config_index)| {
                                            self.render_ssh_item(item_index, config_index, cx)
                                                .into_any_element()
                                        },
                                    )),
                            )
                            // 空列表提示
                            .when(
                                self.filtered_local_indices.is_empty()
                                    && self.filtered_indices.is_empty(),
                                |this| {
                                    this.child(
                                        div().h(px(1.0)).mx(px(12.0)).bg(rgb(COLOR_BG_SELECTED)),
                                    )
                                    .child(
                                        div().px(px(16.0)).py(px(12.0)).child(
                                            div()
                                                .text_color(rgb(COLOR_TEXT_SECONDARY))
                                                .text_size(px(13.0))
                                                .child("No matching results"),
                                        ),
                                    )
                                },
                            )
                            // 页脚
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(COLOR_BG_SELECTED)))
                            .child(
                                div()
                                    .px(px(12.0))
                                    .py(px(8.0))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(12.0))
                                            .child(Self::render_key_hint("↑↓", "Navigate"))
                                            .child(Self::render_key_hint("⏎", "Open")),
                                    )
                                    .child(Self::render_key_hint("Esc", "Close")),
                            ),
                    ),
            )
            .into_any_element()
    }
}

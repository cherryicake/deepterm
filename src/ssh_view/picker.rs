//! SSH 连接选择器弹窗
//!
//! 提供类似 Command Palette 的界面用于选择 SSH 连接配置。
//! 支持搜索过滤、键盘导航和回车确认。

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, ScrollHandle, Styled, Subscription, Window, actions, div,
    px, rems, svg,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::ssh::config::{SshConnectionConfig, SshGlobalConfig};
use crate::ssh::platform::{RemoteOS, guess_os_from_text};
use crate::theme::ActiveTheme;
use crate::workspace::workspace::Workspace;

actions!(ssh_picker, [Dismiss, SelectNext, SelectPrevious, Confirm]);

// Action to show the SSH picker.
actions!(ssh_picker_actions, [ShowSshPicker]);

/// 全局 SSH picker 引用，用于从 Workspace 触发显示
pub struct GlobalSshPicker(pub Entity<SshPicker>);
impl gpui::Global for GlobalSshPicker {}

// 尺寸常量
const ITEM_HEIGHT: f32 = 30.0;
const PANEL_WIDTH: f32 = 600.0;
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

/// 检测命令是否可用（仅 Windows 的终端扫描使用）
#[cfg(windows)]
fn is_command_available(cmd: &str, args: &[&str]) -> bool {
    let mut command = crate::utils::console::hidden_command(cmd);
    command.args(args);
    command
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// WSL 发行版的启动参数。
///
/// 用**文档化的显式选项** `-d <distro> --cd ~`，理由是实测（`wsl.exe --exec pwd`）：
///
/// | 参数形态                                | 起点                          |
/// |-----------------------------------------|-------------------------------|
/// | `~ -d <distro>`（旧）                    | `/home/<user>`✓ 但依赖「首个非选项参数当起始目录」这一未文档化行为 |
/// | `-d <distro>`（曾经的修法）               | `/mnt/c/<宿主 cwd>` ✗ 起点变成宿主目录 |
/// | `-d <distro> --cd ~`（本函数）            | `/home/<user>` ✓              |
///
/// `wsl.exe` 的第一个**非选项**参数按文档是「要在发行版里执行的命令」，旧形态把
/// `~` 放在那个位置是靠未文档化行为兜住的——一旦 WSL 收紧解析，症状是 conpty 立刻
/// 退出、终端打不开且无日志线索。显式的 `--cd` 没有这个歧义。
///
/// `--cd ~` 里的 `~` 由 `wsl.exe` 自己展开（不经 shell），已实测生效。
#[cfg(windows)]
fn wsl_launch_args(distro: &str) -> Vec<String> {
    vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        "~".to_string(),
    ]
}

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
    for distro in crate::utils::wsl::installed_distros() {
        let icon = crate::utils::wsl::distro_icon(&distro);
        // 参数顺序必须是 `-d <distro>`（见 `wsl_launch_args` 的文档）。
        options.push(LocalTerminalOption::new(
            format!("WSL: {distro}"),
            "wsl.exe",
            wsl_launch_args(&distro),
            icon,
        ));
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
        if let Ok(output) = std::process::Command::new("which").arg(shell).output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // 避免重复添加已存在的 shell
            if !path.is_empty() && !options.iter().any(|opt| opt.command == path) {
                options.push(LocalTerminalOption::new(
                    shell.to_string(),
                    path,
                    vec![],
                    Icon::Terminal,
                ));
            }
        }
    }

    options
}

/// SSH 连接选择器
pub struct SshPicker {
    focus_handle: FocusHandle,
    search_input: Entity<InputState>,
    query: String,
    configs: Vec<SshConnectionConfig>,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    workspace: Option<Entity<Workspace>>,
    pub visible: bool,
    local_terminal_options: Vec<LocalTerminalOption>,
    filtered_local_indices: Vec<usize>,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SshPicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let configs = Self::load_configs();
        let filtered_indices = (0..configs.len()).collect();
        let local_terminal_options = Self::scan_local_terminals();
        let filtered_local_indices = (0..local_terminal_options.len()).collect();

        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search hosts or tabs...")
                .placeholder_font_size(px(14.0))
        });

        let subscription = cx.subscribe(
            &search_input,
            |this: &mut SshPicker, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let text = this.search_input.read(cx).value().to_string();
                    this.update_query(text, cx);
                }
            },
        );

        Self {
            focus_handle,
            search_input,
            query: String::new(),
            configs,
            filtered_indices,
            selected_index: 0,
            workspace: None,
            visible: false,
            local_terminal_options,
            filtered_local_indices,
            scroll_handle: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    pub fn with_workspace(mut self, workspace: Entity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn set_workspace(&mut self, workspace: Entity<Workspace>) {
        self.workspace = Some(workspace);
    }

    fn load_configs() -> Vec<SshConnectionConfig> {
        SshGlobalConfig::load().saved_connections
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
        // Local terminals rarely change during a session; reuse cached scan
        // instead of blocking the UI thread with synchronous process spawns.
        self.filtered_local_indices = (0..self.local_terminal_options.len()).collect();

        self.search_input.update(cx, |input, cx| input.clear(cx));

        let input_focus = self.search_input.read(cx).focus_handle(cx);
        cx.defer_in(window, move |_, window, cx| {
            window.focus(&input_focus, cx);
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
        self.scroll_to_selected(cx);
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
            self.scroll_to_selected(cx);
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
            self.scroll_to_selected(cx);
            cx.notify();
        }
    }

    fn scroll_to_selected(&self, _cx: &Context<Self>) {
        let local_count = self.filtered_local_indices.len();
        if self.selected_index >= local_count {
            let host_index = self.selected_index - local_count;
            self.scroll_handle.scroll_to_item(host_index);
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
            if let Some(&idx) = self.filtered_local_indices.get(self.selected_index)
                && let Some(option) = self.local_terminal_options.get(idx)
            {
                let (command, args) = (option.command.clone(), option.args.clone());
                workspace.update(cx, |w, cx| {
                    w.create_local_terminal_with_command(&command, &args, window, cx)
                });
            }
        } else {
            // Host 列表
            let host_index = self.selected_index - local_count;
            if let Some(&idx) = self.filtered_indices.get(host_index)
                && let Some(config) = self.configs.get(idx)
            {
                let config = config.clone();
                workspace.update(cx, |w, cx| {
                    w.create_ssh_terminal_with_config(&config, window, cx)
                });
            }
        }
    }

    fn handle_input(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "down" | "tab" => self.select_next(cx),
            "up" => self.select_previous(cx),
            "enter" | "return" => self.confirm_selection(window, cx),
            "escape" => self.hide(cx),
            _ => {}
        }
    }

    fn get_os_icon(host: &str, nickname: Option<&str>, os: Option<&str>) -> Icon {
        if let Some(os_id) = os {
            let detected = RemoteOS::from_id(os_id);
            if detected != RemoteOS::Unknown {
                return detected.icon();
            }
        }
        let guessed = guess_os_from_text(nickname.unwrap_or(host));
        if guessed != RemoteOS::Unknown {
            guessed.icon()
        } else {
            Icon::Server
        }
    }

    fn render_search(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div().px(px(12.0)).py(px(12.0)).child(
            div()
                .h(px(SEARCH_HEIGHT))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .bg(theme.app_background)
                .border_1()
                .border_color(theme.border)
                .rounded(px(8.0))
                .child(
                    div().w(px(16.0)).h(px(16.0)).child(
                        svg()
                            .size(px(16.0))
                            .text_color(theme.icon_muted)
                            .path(Icon::MagnifyingGlass.path()),
                    ),
                )
                .child(EditorInput::new(&self.search_input).transparent())
                .child(
                    div()
                        .text_color(theme.text_subtle)
                        .text_size(px(12.0))
                        .child(if cfg!(target_os = "windows") {
                            "Ctrl+K"
                        } else {
                            "⌘+K"
                        }),
                ),
        )
    }

    fn render_section_label(label: &str, shortcut: Option<&str>, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .px(px(16.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_color(theme.text_muted)
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label.to_string()),
            )
            .when_some(shortcut, |this, s| {
                this.child(
                    div()
                        .text_color(theme.text_subtle)
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
        let theme = cx.theme().ui;
        div()
            .id(id.into())
            .mx(px(8.0))
            .h(px(ITEM_HEIGHT))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .when(is_selected, |this| {
                this.bg(theme.accent.opacity(if cx.theme().appearance.is_dark() {
                    0.18
                } else {
                    0.10
                }))
            })
            .when(!is_selected, |this| {
                this.hover(move |s| s.bg(theme.tab_hover_background))
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
                            .text_color(theme.icon)
                            .path(icon.path()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(theme.text)
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
        let icon = Self::get_os_icon(
            &config.host,
            config.nickname.as_deref(),
            config.os.as_deref(),
        );

        Self::render_list_item_content(
            icon,
            config.display_name(),
            actual_index == self.selected_index,
            format!("ssh-item-{}", config.id()),
            actual_index,
            cx,
        )
    }

    fn render_key_hint(key: &str, label: &str, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .px(px(4.0))
                    .py(px(2.0))
                    .bg(theme.app_background)
                    .rounded(px(4.0))
                    .text_color(theme.text)
                    .text_size(px(11.0))
                    .child(key.to_string()),
            )
            .child(
                div()
                    .text_color(theme.text_muted)
                    .text_size(px(11.0))
                    .child(label.to_string()),
            )
    }
}

crate::impl_focusable!(SshPicker);

impl Render for SshPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }
        let theme = cx.theme().ui;
        let overlay = theme
            .overlay_background
            .opacity(if cx.theme().appearance.is_dark() {
                0.5
            } else {
                0.2
            });

        div()
            .absolute()
            .inset_0()
            .bg(overlay)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _cx, cx| this.hide(cx)),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
                            .bg(theme.panel_background)
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(12.0))
                            .shadow_lg()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .key_context("SshPicker")
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                this.handle_input(event, window, cx);
                            }))
                            .track_focus(&self.focus_handle)
                            .child(self.render_search(cx))
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(theme.border))
                            // Local connect 区域
                            .when(!self.filtered_local_indices.is_empty(), |this| {
                                this.child(Self::render_section_label("Local connect", None, cx))
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
                                    .child(div().h(px(1.0)).mx(px(12.0)).bg(theme.border))
                            })
                            // Host 区域
                            .child(Self::render_section_label("Host", None, cx))
                            .child(
                                div()
                                    .pb(px(8.0))
                                    .id("ssh-hosts-list")
                                    .max_h(rems(16.))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll_handle)
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
                                    this.child(div().h(px(1.0)).mx(px(12.0)).bg(theme.border))
                                        .child(
                                            div().px(px(16.0)).py(px(12.0)).child(
                                                div()
                                                    .text_color(theme.text_muted)
                                                    .text_size(px(13.0))
                                                    .child("No matching results"),
                                            ),
                                        )
                                },
                            )
                            // 页脚
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(theme.border))
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
                                            .child(Self::render_key_hint("↑↓", "Navigate", cx))
                                            .child(Self::render_key_hint("⏎", "Open", cx)),
                                    )
                                    .child(Self::render_key_hint("Esc", "Close", cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::wsl_launch_args;
    use crate::utils::wsl::decode_list_output;

    /// 启动参数必须是 `-d <distro> --cd ~`，且不能出现裸的 `~` 位置参数。
    ///
    /// 两条各钉一类故障：
    /// - 裸 `~` 位置参数（旧的 `["~", "-d", distro]`）落在 `wsl.exe` 文档里
    ///   「首个非选项参数 = 要执行的命令」的位置，靠未文档化行为才没出错；
    /// - 漏掉 `--cd ~` 则起点从家目录变成宿主目录（`/mnt/c/...`）。
    #[test]
    fn wsl_args_use_documented_flags_and_start_at_home() {
        let args = wsl_launch_args("Ubuntu-24.04");
        assert_eq!(
            args,
            vec![
                "-d".to_string(),
                "Ubuntu-24.04".to_string(),
                "--cd".to_string(),
                "~".to_string(),
            ],
            "WSL 启动参数只能是 -d <distro> --cd ~"
        );
        assert_eq!(args.first().map(String::as_str), Some("-d"));
        // 发行版名后面必须紧跟 --cd，而不是让 `~` 裸占一个位置参数。
        assert_eq!(args.get(2).map(String::as_str), Some("--cd"));
    }

    /// 把字符串编码成「无 BOM 的 UTF-16LE」，即本机 `wsl.exe --list --quiet`
    /// 经管道重定向时的真实形态。
    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// 无 BOM 的 UTF-16LE 必须真解码，不能掉进 UTF-8 分支。
    ///
    /// 掉进去的代价不是「显示难看」而是**打不开终端**：NUL 夹在 CR/LF 之间
    /// 使 `lines()` 剥不掉行尾 `\r`，`Ubuntu-24.04\r` 随后作为 `-d` 的参数
    /// 发给 `wsl.exe`，发行版找不到。
    #[test]
    fn decodes_utf16le_without_bom() {
        let decoded = decode_list_output(&utf16le("Ubuntu-24.04\r\narchlinux\r\n\r\n"));
        let distros: Vec<&str> = decoded
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(distros, vec!["Ubuntu-24.04", "archlinux"]);
        // 关键：行尾不能残留 `\r`，否则 `-d` 会带上它。
        assert!(
            distros.iter().all(|d| !d.contains('\r')),
            "解码后不得残留 CR：{distros:?}"
        );
    }

    /// 带 BOM 的 UTF-16LE 仍要正确处理（BOM 本身不计入内容）。
    #[test]
    fn decodes_utf16le_with_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(utf16le("Ubuntu-24.04\r\n"));
        assert_eq!(decode_list_output(&bytes).trim(), "Ubuntu-24.04");
    }

    /// 真的就是 UTF-8 时（老版本 / 控制台直出）不能被误判成 UTF-16LE。
    #[test]
    fn keeps_plain_utf8_intact() {
        let decoded = decode_list_output(b"Ubuntu-24.04\narchlinux\n");
        let distros: Vec<&str> = decoded.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(distros, vec!["Ubuntu-24.04", "archlinux"]);
    }

    /// 本机 `wsl.exe --list --quiet` 经管道重定向的**逐字节实录**
    /// （`C:\Users\<u>\AppData\Local\Temp` 下抓的 50 字节原样贴进来）。
    ///
    /// 钉住两个事实：无 BOM；CRLF 是 UTF-16 编码的，所以 NUL 夹在 CR 与 LF
    /// 之间。用它跑一遍完整链路（解码 → 按行取名），确保拼出来的 `-d` 参数
    /// 就是干净的发行版名。
    #[test]
    fn decodes_real_wsl_list_capture() {
        let captured: Vec<u8> = vec![
            0x55, 0x00, 0x62, 0x00, 0x75, 0x00, 0x6e, 0x00, 0x74, 0x00, 0x75, 0x00, 0x2d, 0x00,
            0x32, 0x00, 0x34, 0x00, 0x2e, 0x00, 0x30, 0x00, 0x34, 0x00, 0x0d, 0x00, 0x0a, 0x00,
            0x61, 0x00, 0x72, 0x00, 0x63, 0x00, 0x68, 0x00, 0x6c, 0x00, 0x69, 0x00, 0x6e, 0x00,
            0x75, 0x00, 0x78, 0x00, 0x0d, 0x00, 0x0a, 0x00,
        ];
        // 复现 `scan_windows_terminals` 里的取名方式。
        let distros: Vec<String> = decode_list_output(&captured)
            .lines()
            .map(|l| l.trim().replace('\0', ""))
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(distros, vec!["Ubuntu-24.04", "archlinux"]);
    }
}

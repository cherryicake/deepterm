//! SSH 连接选择器弹窗
//!
//! 提供类似 Command Palette 的界面用于选择 SSH 连接配置。
//! 支持搜索过滤、键盘导航和回车确认。

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, Styled, Window, actions, div, px, rems, rgb, svg,
};

use crate::assets::Icon;
use crate::ssh::config::{SshConnectionConfig, SshGlobalConfig};
use crate::workspace::workspace::Workspace;

actions!(ssh_picker, [Dismiss, SelectNext, SelectPrevious, Confirm]);

/// SSH 连接选择器
pub struct SshPicker {
    /// 焦点句柄
    focus_handle: FocusHandle,
    /// 搜索查询
    query: String,
    /// 所有可用的连接配置
    configs: Vec<SshConnectionConfig>,
    /// 过滤后的配置索引
    filtered_indices: Vec<usize>,
    /// 当前选中项索引
    selected_index: usize,
    /// 工作区引用
    workspace: Option<Entity<Workspace>>,
    /// 是否可见
    pub visible: bool,
    /// Local Terminal 是否匹配当前过滤条件
    local_terminal_matches: bool,
}

impl SshPicker {
    /// 创建新的 SSH 选择器
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle();
        let configs = Self::load_configs();
        let filtered_indices: Vec<usize> = (0..configs.len()).collect();

        Self {
            focus_handle,
            query: String::new(),
            configs,
            filtered_indices,
            selected_index: 0,
            workspace: None,
            visible: false,
            local_terminal_matches: true,
        }
    }

    /// 设置关联的工作区
    pub fn with_workspace(mut self, workspace: Entity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// 加载配置文件
    fn load_configs() -> Vec<SshConnectionConfig> {
        SshGlobalConfig::load()
            .map(|config| config.saved_connections)
            .unwrap_or_default()
    }

    /// 显示选择器
    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        self.query.clear();
        self.configs = Self::load_configs();
        self.filtered_indices = (0..self.configs.len()).collect();
        self.selected_index = 0;
        self.local_terminal_matches = true;

        // 自动聚焦到选择器
        cx.defer_in(window, |this, window, cx| {
            window.focus(&this.focus_handle, cx);
        });

        cx.notify();
    }

    /// 隐藏选择器
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        cx.notify();
    }

    /// 切换显示状态
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(cx);
        } else {
            self.show(window, cx);
        }
    }

    /// 更新搜索查询并过滤列表
    fn update_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query.to_lowercase();
        self.filter_items();
        self.selected_index = 0;
        cx.notify();
    }

    /// 过滤列表项（包括 Local Terminal）
    fn filter_items(&mut self) {
        // 检查 Local Terminal 是否匹配
        let local_terminal_text = "local terminal";
        self.local_terminal_matches =
            self.query.is_empty() || local_terminal_text.contains(&self.query.to_lowercase());

        // 过滤 Host 列表
        if self.query.is_empty() {
            self.filtered_indices = (0..self.configs.len()).collect();
        } else {
            self.filtered_indices = self
                .configs
                .iter()
                .enumerate()
                .filter(|(_, config)| {
                    let host_match = config.host.to_lowercase().contains(&self.query);
                    let username_match = config.username.to_lowercase().contains(&self.query);
                    let nickname_match = config
                        .nickname
                        .as_ref()
                        .map(|n| n.to_lowercase().contains(&self.query))
                        .unwrap_or(false);
                    host_match || username_match || nickname_match
                })
                .map(|(i, _)| i)
                .collect();
        }
    }

    /// 获取总项目数（Quick connect + Host）
    fn total_items(&self) -> usize {
        let local_terminal_count = if self.local_terminal_matches { 1 } else { 0 };
        local_terminal_count + self.filtered_indices.len()
    }

    /// 选择下一项
    fn select_next(&mut self, cx: &mut Context<Self>) {
        let total = self.total_items();
        if total > 0 {
            self.selected_index = (self.selected_index + 1) % total;
            cx.notify();
        }
    }

    /// 选择上一项
    fn select_previous(&mut self, cx: &mut Context<Self>) {
        let total = self.total_items();
        if total > 0 {
            if self.selected_index == 0 {
                self.selected_index = total - 1;
            } else {
                self.selected_index -= 1;
            }
            cx.notify();
        }
    }

    /// 确认选择
    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = match self.workspace.clone() {
            Some(w) => w,
            None => return,
        };

        self.hide(cx);

        let local_count = if self.local_terminal_matches { 1 } else { 0 };
        if self.selected_index < local_count {
            // Quick connect - Local Terminal
            workspace.update(cx, |w, cx| w.create_local_terminal(window, cx));
            return;
        }

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

    /// 处理键盘输入
    fn handle_input(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 处理特殊键
        match event.keystroke.key.as_str() {
            "down" => {
                self.select_next(cx);
                return;
            }
            "up" => {
                self.select_previous(cx);
                return;
            }
            "enter" | "return" => {
                self.confirm_selection(window, cx);
                return;
            }
            "escape" => {
                self.hide(cx);
                return;
            }
            "backspace" => {
                // 删除最后一个字符
                if !self.query.is_empty() {
                    let new_query: String = self.query.chars().take(self.query.len() - 1).collect();
                    self.update_query(new_query, cx);
                }
                return;
            }
            _ => {}
        }

        // 处理普通字符输入
        if let Some(ref text) = event.keystroke.key_char {
            let new_query = format!("{}{}", self.query, text);
            self.update_query(new_query, cx);
        }
    }

    /// 根据主机地址或昵称推断 OS 类型并返回对应图标
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

    /// 渲染搜索框
    fn render_search(&self) -> impl IntoElement {
        div().px(px(12.0)).py(px(12.0)).child(
            div()
                .h(px(36.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .bg(rgb(0x11111b)) // 比弹窗背景更深的颜色
                .rounded(px(8.0))
                .child(
                    // 搜索图标 - 使用嵌入的 magnifying_glass SVG
                    div().w(px(16.0)).h(px(16.0)).child(
                        svg()
                            .size(px(16.0))
                            .text_color(rgb(0x666688))
                            .path(Icon::MagnifyingGlass.path()),
                    ),
                )
                .child(
                    // 输入框区域
                    div().flex_1().child(if self.query.is_empty() {
                        div()
                            .text_color(rgb(0x5d5d7c))
                            .text_size(px(14.0))
                            .child("Search hosts or tabs")
                            .into_any_element()
                    } else {
                        div()
                            .text_color(rgb(0x9aa3b8)) // 终端前景色
                            .text_size(px(14.0))
                            .child(self.query.clone())
                            .into_any_element()
                    }),
                )
                .child(
                    // 快捷键提示
                    div()
                        .text_color(rgb(0x5d5d7c))
                        .text_size(px(12.0))
                        .child("⌘+K"),
                ),
        )
    }

    /// 渲染区域标签
    fn render_section_label(label: &str, shortcut: Option<&str>) -> impl IntoElement {
        div()
            .px(px(16.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_color(rgb(0x5d5d7c))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label.to_string()),
            )
            .when(shortcut.is_some(), |this| {
                this.child(
                    div()
                        .text_color(rgb(0x4a4a6a))
                        .text_size(px(11.0))
                        .child(shortcut.unwrap().to_string()),
                )
            })
    }

    /// 渲染 Local connect 项（Local Terminal）
    fn render_local_connect_item(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_selected = self.selected_index == 0;

        div()
            .id("quick-connect-local-terminal")
            .mx(px(8.0))
            .h(px(30.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .when(is_selected, |this| this.bg(rgb(0x313244))) // 与终端主题协调的选中色
            .when(!is_selected, |this| {
                this.hover(|style| style.bg(rgb(0x252538)))
            })
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                this.selected_index = 0;
                this.confirm_selection(window, cx);
            }))
            .child(
                // Terminal 图标 - 使用嵌入的 SVG
                div()
                    .w(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(16.0))
                            .text_color(rgb(0x9aa3b8)) // 终端前景色
                            .path(Icon::Terminal.path()),
                    ),
            )
            .child(
                // 显示名称
                div()
                    .flex_1()
                    .text_color(rgb(0x9aa3b8)) // 终端前景色
                    .text_size(px(14.0))
                    .child("Local Terminal"),
            )
    }

    /// 渲染列表项
    fn render_list_item(
        &self,
        item_index: usize,
        config_index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let config = &self.configs[config_index];
        // Host 列表的索引需要根据 Local Terminal 是否显示来调整
        let local_terminal_count = if self.local_terminal_matches { 1 } else { 0 };
        let actual_index = item_index + local_terminal_count;
        let is_selected = actual_index == self.selected_index;

        div()
            .id(format!("ssh-item-{}", config.id()))
            .mx(px(8.0))
            .h(px(30.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .when(is_selected, |this| this.bg(rgb(0x313244))) // 与终端主题协调的选中色
            .when(!is_selected, |this| {
                this.hover(|style| style.bg(rgb(0x252538)))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_index = actual_index;
                this.confirm_selection(window, cx);
            }))
            .child(
                // OS 图标 - 使用嵌入的 SVG
                {
                    let icon = Self::get_os_icon(&config.host, config.nickname.as_deref());
                    div()
                        .w(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            svg()
                                .size(px(16.0))
                                .text_color(rgb(0x9aa3b8)) // 终端前景色
                                .path(icon.path()),
                        )
                },
            )
            .child(
                // 显示名称 - 优先使用 nickname
                div()
                    .flex_1()
                    .text_color(rgb(0x9aa3b8)) // 终端前景色
                    .text_size(px(14.0))
                    .child(config.display_name()),
            )
    }

    /// 渲染快捷键提示
    fn render_key_hint(key: &str, label: &str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .px(px(4.0))
                    .py(px(2.0))
                    .bg(rgb(0x11111b)) // 更深的按键背景
                    .rounded(px(4.0))
                    .text_color(rgb(0x9aa3b8)) // 终端前景色
                    .text_size(px(11.0))
                    .child(key.to_string()),
            )
            .child(
                div()
                    .text_color(rgb(0x5d5d7c))
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
        // 如果不可见，返回空元素
        if !self.visible {
            return div().into_any_element();
        }

        // 遮罩层 + 弹窗
        div()
            .absolute()
            .inset_0()
            // 半透明遮罩层背景
            .bg(gpui::rgba(0x00000080))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    // 点击遮罩层关闭弹窗
                    this.hide(cx);
                }),
            )
            .child(
                // 弹窗容器 - 阻止点击事件冒泡
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(120.0))
                    .child(
                        div()
                            .w(px(520.0))
                            .flex()
                            .flex_col()
                            .bg(rgb(0x181825)) // 更深的终端 Black 色
                            .rounded(px(12.0))
                            .shadow_lg()
                            // 阻止点击事件冒泡到遮罩层
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            // 键盘事件处理
                            .key_context("SshPicker")
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                this.handle_input(event, window, cx);
                            }))
                            // 跟踪焦点
                            .track_focus(&self.focus_handle)
                            // 搜索栏
                            .child(self.render_search())
                            // 分隔线
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(0x313244)))
                            // Local connect 区域（仅当匹配时显示）
                            .when(self.local_terminal_matches, |this| {
                                this.child(Self::render_section_label("Local connect", None))
                                    .child(
                                        div().pb(px(8.0)).child(self.render_local_connect_item(cx)),
                                    )
                                    .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(0x313244)))
                            })
                            // Host 区域
                            .child(Self::render_section_label("Host", None))
                            // 服务器列表 - 限制最大高度并启用滚动
                            .child(
                                div()
                                    .pb(px(8.0))
                                    .id("ssh-hosts-list")
                                    .max_h(rems(16.))
                                    .overflow_y_scroll()
                                    .children(self.filtered_indices.iter().enumerate().map(
                                        |(item_index, &config_index)| {
                                            self.render_list_item(item_index, config_index, cx)
                                                .into_any_element()
                                        },
                                    )),
                            )
                            // 空列表提示（当 Quick connect 和 Host 都为空时）
                            .when(
                                !self.local_terminal_matches && self.filtered_indices.is_empty(),
                                |this| {
                                    this.child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(0x313244)))
                                        .child(
                                            div().px(px(16.0)).py(px(12.0)).child(
                                                div()
                                                    .text_color(rgb(0x5d5d7c))
                                                    .text_size(px(13.0))
                                                    .child("No matching results"),
                                            ),
                                        )
                                },
                            )
                            // 页脚 - 快捷键提示
                            .child(div().h(px(1.0)).mx(px(12.0)).bg(rgb(0x313244)))
                            .child(
                                div()
                                    .px(px(12.0))
                                    .py(px(8.0))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        // 左侧提示
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(12.0))
                                            .child(Self::render_key_hint("↑↓", "Navigate"))
                                            .child(Self::render_key_hint("⏎", "Open")),
                                    )
                                    .child(
                                        // 右侧提示
                                        Self::render_key_hint("Esc", "Close"),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

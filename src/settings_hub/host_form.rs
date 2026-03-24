use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render, Window, div,
    px, rgb, svg,
};

use crate::assets::Icon;
use crate::components::input::Input;
use crate::ssh::config::{AuthMethod, SshConnectionConfig, SshGlobalConfig};
use crate::ssh::get_current_username;

/// 认证方式类型
#[derive(Clone, Debug, PartialEq)]
pub enum AuthType {
    Password,
    PrivateKey,
    Agent,
}

impl AuthType {
    pub fn display_name(&self) -> &'static str {
        match self {
            AuthType::Password => "Password",
            AuthType::PrivateKey => "Private Key",
            AuthType::Agent => "SSH Agent",
        }
    }

    pub fn all() -> Vec<AuthType> {
        vec![AuthType::Password, AuthType::PrivateKey, AuthType::Agent]
    }
}

/// Host Form 组件 - 用于创建和编辑 SSH 主机配置
pub struct HostForm {
    focus_handle: FocusHandle,
    // 输入框实体
    nickname_input: Entity<Input>,
    hostname_input: Entity<Input>,
    port_input: Entity<Input>,
    username_input: Entity<Input>,
    password_input: Entity<Input>,
    key_path_input: Entity<Input>,
    // 状态
    auth_type: AuthType,
    show_auth_dropdown: bool,
    error_message: Option<String>,
    success_message: Option<String>,
    editing_config_id: Option<String>,
    // 回调
    on_cancel: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_saved: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
}

impl HostForm {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            // 获取当前系统用户名作为默认值
            let default_username = get_current_username().unwrap_or_default();

            // 创建输入框
            let nickname_input = cx.new(|cx| Input::new(cx).placeholder("My Server"));

            let hostname_input =
                cx.new(|cx| Input::new(cx).placeholder("192.168.1.100 or example.com"));

            let port_input = cx.new(|cx| Input::new(cx).value("22"));

            let username_input = cx.new(|cx| Input::new(cx).value(&default_username));

            let password_input = cx.new(|cx| {
                Input::new(cx)
                    .placeholder("Leave empty to prompt on connect")
                    .masked(true)
                    .mask_toggle(true)
            });

            let key_path_input = cx.new(|cx| Input::new(cx).placeholder("~/.ssh/id_rsa"));

            Self {
                focus_handle,
                nickname_input,
                hostname_input,
                port_input,
                username_input,
                password_input,
                key_path_input,
                auth_type: AuthType::Password,
                show_auth_dropdown: false,
                error_message: None,
                success_message: None,
                editing_config_id: None,
                on_cancel: None,
                on_saved: None,
            }
        })
    }

    /// 设置取消回调
    pub fn on_cancel<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_cancel = Some(Box::new(callback));
    }

    /// 设置保存成功回调
    pub fn on_saved<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_saved = Some(Box::new(callback));
    }

    /// 加载配置到表单（用于编辑）
    pub fn load_config(&mut self, config: &SshConnectionConfig, cx: &mut Context<Self>) {
        self.editing_config_id = Some(config.id());
        self.nickname_input.update(cx, |input, cx| {
            if let Some(ref nick) = config.nickname {
                input.set_text(nick, cx);
            } else {
                input.clear(cx);
            }
        });
        self.hostname_input.update(cx, |input, cx| {
            input.set_text(&config.host, cx);
        });
        self.port_input.update(cx, |input, cx| {
            input.set_text(&config.port.to_string(), cx);
        });
        self.username_input.update(cx, |input, cx| {
            input.set_text(&config.username, cx);
        });

        // Clear auth fields first, then set based on auth method
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.key_path_input.update(cx, |input, cx| input.clear(cx));
        match config.auth_method {
            AuthMethod::Password { ref password } => {
                self.auth_type = AuthType::Password;
                self.password_input.update(cx, |input, cx| {
                    input.set_text(password, cx);
                });
            }
            AuthMethod::PrivateKey { ref path, .. } => {
                self.auth_type = AuthType::PrivateKey;
                self.key_path_input.update(cx, |input, cx| {
                    let path_str = path.to_string_lossy().into_owned();
                    input.set_text(path_str, cx);
                });
            }
            AuthMethod::Agent => {
                self.auth_type = AuthType::Agent;
            }
            AuthMethod::Auto => {
                self.auth_type = AuthType::Password;
            }
        }

        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    /// 重置表单
    pub fn reset_form(&mut self, cx: &mut Context<Self>) {
        self.editing_config_id = None;
        self.nickname_input.update(cx, |input, cx| {
            input.clear(cx);
        });
        self.hostname_input.update(cx, |input, cx| {
            input.clear(cx);
        });
        self.port_input.update(cx, |input, cx| {
            input.set_text("22", cx);
        });
        self.username_input.update(cx, |input, cx| {
            input.set_text(&get_current_username().unwrap_or_default(), cx);
        });
        self.password_input.update(cx, |input, cx| {
            input.clear(cx);
        });
        self.key_path_input.update(cx, |input, cx| {
            input.clear(cx);
        });
        self.auth_type = AuthType::Password;
        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    /// 验证表单
    fn validate_form(&self, cx: &Context<Self>) -> Result<(), String> {
        let hostname = self.hostname_input.read(cx).text().to_string();
        let username = self.username_input.read(cx).text().to_string();
        let port = self.port_input.read(cx).text().to_string();

        if hostname.trim().is_empty() {
            return Err("Host address is required".to_string());
        }

        if username.trim().is_empty() {
            return Err("Username is required".to_string());
        }

        // 验证端口号
        if port.parse::<u16>().is_err() {
            return Err("Invalid port number".to_string());
        }

        Ok(())
    }

    /// 保存配置
    fn save_config(&mut self, cx: &mut Context<Self>) {
        self.error_message = None;
        self.success_message = None;

        // 验证表单
        if let Err(e) = self.validate_form(cx) {
            self.error_message = Some(e);
            cx.notify();
            return;
        }

        // 获取输入值
        let nickname = self.nickname_input.read(cx).text().to_string();
        let hostname = self.hostname_input.read(cx).text().to_string();
        let username = self.username_input.read(cx).text().to_string();
        let port = self.port_input.read(cx).text().parse::<u16>().unwrap_or(22);
        let password = self.password_input.read(cx).text().to_string();
        let key_path = self.key_path_input.read(cx).text().to_string();

        // 创建认证方式
        let auth_method = match self.auth_type {
            AuthType::Password => {
                if password.is_empty() {
                    AuthMethod::Auto
                } else {
                    AuthMethod::Password { password }
                }
            }
            AuthType::PrivateKey => {
                if key_path.is_empty() {
                    AuthMethod::Auto
                } else {
                    AuthMethod::PrivateKey {
                        path: std::path::PathBuf::from(&key_path),
                        passphrase: None,
                    }
                }
            }
            AuthType::Agent => AuthMethod::Agent,
        };

        // 创建配置
        let mut config = SshConnectionConfig::new(hostname.trim(), username.trim(), auth_method);

        // 设置端口
        config = config.with_port(port);

        // 设置昵称
        if !nickname.trim().is_empty() {
            config = config.with_nickname(nickname.trim());
        }

        // 加载现有配置
        let mut global_config = SshGlobalConfig::load().unwrap_or_else(SshGlobalConfig::new);

        // 如果是编辑模式，先删除旧配置
        if let Some(ref editing_id) = self.editing_config_id {
            global_config.remove_connection(editing_id);
        }

        // 添加新/更新配置
        global_config.add_connection(config);

        // 保存配置
        match global_config.save() {
            Ok(()) => {
                self.success_message = Some("Host saved successfully".to_string());
                cx.notify();

                // 触发回调
                if let Some(ref callback) = self.on_saved {
                    callback(cx);
                }
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to save: {}", e));
                cx.notify();
            }
        }
    }

    /// 设置认证类型
    fn set_auth_type(&mut self, auth_type: AuthType, cx: &mut Context<Self>) {
        self.auth_type = auth_type;
        self.show_auth_dropdown = false;
        cx.notify();
    }

    /// 取消按钮处理
    fn handle_cancel(&mut self, cx: &mut Context<Self>) {
        self.reset_form(cx);
        if let Some(ref callback) = self.on_cancel {
            callback(cx);
        }
    }

    /// 渲染带标签的输入框
    fn render_labeled_input(&self, label: &str, input: &Entity<Input>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8888aa))
                    .child(label.to_string()),
            )
            .child(input.clone())
    }

    /// 渲染认证方式选择器
    fn render_auth_type_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_auth = self.auth_type.clone();
        let show_dropdown = self.show_auth_dropdown;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8888aa))
                    .child("Authentication"),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    // 当前选择
                    .child(
                        div()
                            .w_full()
                            .h(px(36.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(rgb(0x2d2d44))
                            .bg(rgb(0x13131f))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.show_auth_dropdown = !this.show_auth_dropdown;
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xcccccc))
                                    .child(current_auth.display_name()),
                            )
                            .child(
                                svg()
                                    .size(px(12.0))
                                    .path(Icon::ChevronDown.path())
                                    .text_color(rgb(0x8888aa)),
                            ),
                    )
                    // 下拉选项
                    .when(show_dropdown, |this| {
                        this.child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(rgb(0x2d2d44))
                                .bg(rgb(0x1a1a2e))
                                .children(AuthType::all().into_iter().map(|auth| {
                                    let is_selected = auth == current_auth;
                                    let auth_clone = auth.clone();
                                    div()
                                        .w_full()
                                        .h(px(32.0))
                                        .px(px(10.0))
                                        .flex()
                                        .items_center()
                                        .cursor_pointer()
                                        .when(is_selected, |d| d.bg(rgb(0x252538)))
                                        .when(!is_selected, |d| d.hover(|s| s.bg(rgb(0x252538))))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                this.set_auth_type(auth_clone.clone(), cx);
                                            }),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(if is_selected {
                                                    rgb(0xffffff)
                                                } else {
                                                    rgb(0xcccccc)
                                                })
                                                .child(auth.display_name()),
                                        )
                                        .into_any_element()
                                })),
                        )
                    }),
            )
    }

    /// 渲染认证方式的额外字段
    fn render_auth_extra_field(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.auth_type {
            AuthType::Password => self
                .render_labeled_input("Password (optional)", &self.password_input)
                .into_any_element(),
            AuthType::PrivateKey => self
                .render_labeled_input("Key Path (optional)", &self.key_path_input)
                .into_any_element(),
            AuthType::Agent => div()
                .w_full()
                .py(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x666688))
                        .child("Uses SSH agent for authentication"),
                )
                .into_any_element(),
        }
    }

    /// 渲染表单头部
    fn render_header(&self) -> impl IntoElement {
        let is_editing = self.editing_config_id.is_some();
        let (title, subtitle) = if is_editing {
            ("Edit Host", "Modify SSH host configuration")
        } else {
            ("New Host", "Add a new SSH host configuration")
        };

        div()
            .w_full()
            .pb(px(16.0))
            .border_b_1()
            .border_color(rgb(0x2d2d44))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0xffffff))
                    .child(title),
            )
            .child(div().text_xs().text_color(rgb(0x8888aa)).child(subtitle))
    }

    /// 渲染错误/成功消息
    fn render_message(&self) -> Option<impl IntoElement> {
        if let Some(ref error) = self.error_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(rgb(0x3d1f1f))
                    .border_1()
                    .border_color(rgb(0x5d2f2f))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xff6666))
                            .child(error.clone()),
                    )
                    .into_any_element(),
            )
        } else if let Some(ref success) = self.success_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(rgb(0x1f3d1f))
                    .border_1()
                    .border_color(rgb(0x2f5d2f))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x66ff66))
                            .child(success.clone()),
                    )
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    /// 渲染操作按钮
    fn render_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .pt(px(16.0))
            .child(
                // Cancel button
                div()
                    .flex_1()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .bg(rgb(0x2d2d44))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x3d3d5c)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.handle_cancel(cx);
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xcccccc))
                            .child("Cancel"),
                    ),
            )
            .child(
                // Save button
                div()
                    .flex_1()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .bg(rgb(0x3d5c3d))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x4a6a4a)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.save_config(cx);
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xffffff))
                            .child("Save"),
                    ),
            )
    }
}

impl Focusable for HostForm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("new-host-form")
            .track_focus(&self.focus_handle)
            .w_full()
            .h_full()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(self.render_header())
            .children(self.render_message().into_iter())
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    // Nickname
                    .child(self.render_labeled_input("Nickname (optional)", &self.nickname_input))
                    // Hostname
                    .child(self.render_labeled_input("Host Address *", &self.hostname_input))
                    // Port and Username row
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .w(px(80.0))
                                    .child(self.render_labeled_input("Port", &self.port_input)),
                            )
                            .child(div().flex_1().child(
                                self.render_labeled_input("Username *", &self.username_input),
                            )),
                    )
                    // Authentication type
                    .child(self.render_auth_type_selector(cx))
                    // Extra field based on auth type
                    .child(self.render_auth_extra_field(cx)),
            )
            .child(self.render_buttons(cx))
    }
}

use std::path::PathBuf;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    px,
};

use crate::components::button::{Button, ButtonVariants};
use crate::components::input::{EditorInput, InputState};
use crate::components::select::Select;
use crate::ssh::config::{AuthMethod, SshConnectionConfig, SshGlobalConfig};
use crate::ssh::get_current_username;
use crate::ssh::keychain::{KeychainItem, KeychainItemKind, KeychainStore, classify_key_flavor};
use crate::theme::ActiveTheme;

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
            AuthType::Password => "密码",
            AuthType::PrivateKey => "私钥",
            AuthType::Agent => "SSH Agent",
        }
    }

    pub fn all() -> Vec<AuthType> {
        vec![AuthType::Password, AuthType::PrivateKey, AuthType::Agent]
    }
}

/// macOS 风格的 New Host 配置表单。
///
/// 布局参考 macOS 系统设置弹窗：
/// - 标签在左（固定宽度，右对齐），控件在右
/// - 分组标题 + 分割线
/// - 底部按钮右对齐
pub struct NewHostForm {
    focus_handle: FocusHandle,
    nickname_input: Entity<InputState>,
    hostname_input: Entity<InputState>,
    port_input: Entity<InputState>,
    username_input: Entity<InputState>,
    password_input: Entity<InputState>,
    key_path_input: Entity<InputState>,
    key_passphrase_input: Entity<InputState>,
    credential_label_input: Entity<InputState>,
    auth_type: AuthType,
    auth_type_select: Entity<Select>,
    error_message: Option<String>,
    success_message: Option<String>,
    on_cancel: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_saved: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
}

impl NewHostForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let default_username = get_current_username().unwrap_or_default();

            let nickname_input = cx.new(|cx| InputState::new(window, cx).placeholder("我的服务器"));
            let hostname_input = cx
                .new(|cx| InputState::new(window, cx).placeholder("192.168.1.100 或 example.com"));
            let port_input = cx.new(|cx| InputState::new(window, cx));
            port_input.update(cx, |input, cx| input.set_text("22", cx));
            let username_input = cx.new(|cx| InputState::new(window, cx));
            username_input.update(cx, |input, cx| input.set_text(&default_username, cx));
            let password_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("保存到系统钥匙串")
                    .masked(true)
            });
            let key_path_input =
                cx.new(|cx| InputState::new(window, cx).placeholder("~/.ssh/id_ed25519"));
            let key_passphrase_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("可选的密钥密码")
                    .masked(true)
            });
            let credential_label_input =
                cx.new(|cx| InputState::new(window, cx).placeholder("DeepTerm 自动生成标签"));

            let auth_labels: Vec<SharedString> = AuthType::all()
                .into_iter()
                .map(|a| a.display_name().into())
                .collect();
            let auth_type_select =
                Select::new(window, cx, auth_labels, AuthType::Password.display_name());

            let entity = cx.entity();
            auth_type_select.update(cx, |select, _cx| {
                select.searchable(false);
                select.on_select(move |label, _index, cx| {
                    let auth_type = match label.as_ref() {
                        "密码" => AuthType::Password,
                        "私钥" => AuthType::PrivateKey,
                        "SSH Agent" => AuthType::Agent,
                        _ => AuthType::Password,
                    };
                    cx.update_entity(&entity, |this: &mut NewHostForm, _cx| {
                        this.auth_type = auth_type;
                        _cx.notify();
                    });
                });
            });

            Self {
                focus_handle,
                nickname_input,
                hostname_input,
                port_input,
                username_input,
                password_input,
                key_path_input,
                key_passphrase_input,
                credential_label_input,
                auth_type: AuthType::Password,
                auth_type_select,
                error_message: None,
                success_message: None,
                on_cancel: None,
                on_saved: None,
            }
        })
    }

    pub fn on_cancel<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_cancel = Some(Box::new(callback));
    }

    pub fn on_saved<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_saved = Some(Box::new(callback));
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.nickname_input.update(cx, |input, cx| input.clear(cx));
        self.hostname_input.update(cx, |input, cx| input.clear(cx));
        self.port_input
            .update(cx, |input, cx| input.set_text("22", cx));
        self.username_input.update(cx, |input, cx| {
            input.set_text(&get_current_username().unwrap_or_default(), cx);
        });
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.key_path_input.update(cx, |input, cx| input.clear(cx));
        self.key_passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        self.credential_label_input
            .update(cx, |input, cx| input.clear(cx));
        self.auth_type = AuthType::Password;
        self.auth_type_select.update(cx, |select, cx| {
            select.set_selected(AuthType::Password.display_name(), cx);
        });
        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    // ─── Validation & Save ────────────────────────────────────────────

    fn validate(&self, cx: &Context<Self>) -> Result<(), String> {
        let hostname = self.hostname_input.read(cx).value().to_string();
        let username = self.username_input.read(cx).value().to_string();
        let port = self.port_input.read(cx).value().to_string();

        if hostname.trim().is_empty() {
            return Err("主机地址不能为空".to_string());
        }
        if username.trim().is_empty() {
            return Err("用户名不能为空".to_string());
        }
        if port.parse::<u16>().is_err() {
            return Err("端口格式不正确".to_string());
        }
        Ok(())
    }

    fn build_auth_method(
        &self,
        username: &str,
        host_label: &str,
        cx: &Context<Self>,
    ) -> Result<AuthMethod, String> {
        let credential_label = self.credential_label_input.read(cx).value().to_string();
        let password = self.password_input.read(cx).value().to_string();
        let key_path = self.key_path_input.read(cx).value().to_string();
        let key_passphrase = self.key_passphrase_input.read(cx).value().to_string();

        let mut store = KeychainStore::load_or_default();
        let label = if credential_label.trim().is_empty() {
            host_label.to_string()
        } else {
            credential_label.trim().to_string()
        };

        match self.auth_type {
            AuthType::Password => {
                if !password.trim().is_empty() {
                    let keychain_id = store
                        .create_identity(&label, Some(username.to_string()), &password)
                        .map_err(|err| format!("保存凭据失败: {}", err))?;
                    Ok(AuthMethod::password_ref(keychain_id))
                } else {
                    Ok(AuthMethod::Auto)
                }
            }
            AuthType::PrivateKey => {
                if !key_path.trim().is_empty() {
                    let keychain_id = store
                        .create_key(
                            &label,
                            PathBuf::from(key_path.trim()),
                            None,
                            classify_key_flavor(PathBuf::from(key_path.trim()).as_path(), None),
                            (!key_passphrase.trim().is_empty()).then_some(key_passphrase.as_str()),
                        )
                        .map_err(|err| format!("保存密钥失败: {}", err))?;
                    Ok(AuthMethod::key_ref(keychain_id))
                } else {
                    Ok(AuthMethod::Auto)
                }
            }
            AuthType::Agent => Ok(AuthMethod::Agent),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        self.error_message = None;
        self.success_message = None;

        if let Err(e) = self.validate(cx) {
            self.error_message = Some(e);
            cx.notify();
            return;
        }

        let nickname = self.nickname_input.read(cx).value().to_string();
        let hostname = self.hostname_input.read(cx).value().to_string();
        let username = self.username_input.read(cx).value().to_string();
        let port = self
            .port_input
            .read(cx)
            .value()
            .parse::<u16>()
            .unwrap_or(22);

        let host_label = if nickname.trim().is_empty() {
            format!("{}@{}", username.trim(), hostname.trim())
        } else {
            nickname.trim().to_string()
        };

        let auth_method = match self.build_auth_method(username.trim(), &host_label, cx) {
            Ok(am) => am,
            Err(err) => {
                self.error_message = Some(err);
                cx.notify();
                return;
            }
        };

        let mut config = SshConnectionConfig::new(hostname.trim(), username.trim(), auth_method);
        config = config.with_port(port);
        if !nickname.trim().is_empty() {
            config = config.with_nickname(nickname.trim());
        }

        let mut global_config = SshGlobalConfig::load();
        global_config.add_connection(config);

        match global_config.save() {
            Ok(()) => {
                self.success_message = Some("保存成功".to_string());
                cx.notify();
                if let Some(ref cb) = self.on_saved {
                    cb(cx);
                }
            }
            Err(e) => {
                self.error_message = Some(format!("保存失败: {}", e));
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        if let Some(ref cb) = self.on_cancel {
            cb(cx);
        }
    }

    // ─── Rendering helpers ───────────────────────────────────────────

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .pb(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.text)
                    .child("新主机"),
            )
            .child(
                div()
                    .mt(px(4.))
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child("添加 SSH 主机配置"),
            )
    }

    fn render_row(
        &self,
        label: &str,
        required: bool,
        control: impl IntoElement,
        _cx: &App,
    ) -> impl IntoElement {
        let theme = _cx.theme().ui;
        let label_text = label.to_string();

        div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .w(px(60.))
                    .flex()
                    .justify_end()
                    .child(div().text_sm().text_color(theme.text).child(label_text)),
            )
            .child(div().flex_1().min_w_0().child(control))
    }

    fn render_section_title(&self, title: &str, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .pt(px(4.))
            .pb(px(2.))
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_subtle)
                    .child(title.to_string()),
            )
    }

    fn render_message(&self, cx: &App) -> Option<impl IntoElement> {
        let theme = cx.theme().ui;
        if let Some(ref err) = self.error_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .bg(theme.error_background)
                    .border_1()
                    .border_color(theme.error)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.error_foreground)
                            .child(err.clone()),
                    )
                    .into_any_element(),
            )
        } else if let Some(ref succ) = self.success_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .bg(theme.success_background)
                    .border_1()
                    .border_color(theme.success)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.success_foreground)
                            .child(succ.clone()),
                    )
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    fn render_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(8.))
            .pt(px(16.))
            .border_t_1()
            .border_color(cx.theme().ui.border)
            .child(
                Button::new("new-host-cancel")
                    .label("取消")
                    .rounded(px(6.))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("new-host-save")
                    .label("保存")
                    .success()
                    .rounded(px(6.))
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }
}

impl Focusable for NewHostForm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NewHostForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .id("new-host-form-macos")
            .track_focus(&self.focus_handle)
            .w(px(480.))
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(self.render_header(cx))
            .children(self.render_message(cx).into_iter())
            // ── Basic Info ──
            .child(self.render_section_title("基本信息", cx))
            .child(self.render_row("昵称", false, EditorInput::new(&self.nickname_input), cx))
            .child(self.render_row("地址", false, EditorInput::new(&self.hostname_input), cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(16.))
                    // Port group
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .w(px(140.))
                            .child(
                                div()
                                    .w(px(60.))
                                    .flex()
                                    .justify_end()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.text)
                                            .child("端口"),
                                    ),
                            )
                            .child(div().w(px(68.)).child(EditorInput::new(&self.port_input))),
                    )
                    // Username group (fixed width to align with above)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .w(px(324.))
                            .child(
                                div()
                                    .w(px(60.))
                                    .flex()
                                    .justify_end()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.text)
                                            .child("用户名"),
                                    ),
                            )
                            .child(div().flex_1().child(EditorInput::new(&self.username_input))),
                    ),
            )
            // ── Authentication ──
            .child(self.render_section_title("认证", cx))
            .child(self.render_row("方式", false, self.auth_type_select.clone(), cx))
            .when(
                matches!(self.auth_type, AuthType::Password | AuthType::PrivateKey),
                |this| {
                    this.child(self.render_row(
                        "标签",
                        false,
                        EditorInput::new(&self.credential_label_input),
                        cx,
                    ))
                },
            )
            .when(matches!(self.auth_type, AuthType::Password), |this| {
                this.child(self.render_row(
                    "密码",
                    false,
                    EditorInput::new(&self.password_input).mask_toggle(),
                    cx,
                ))
            })
            .when(matches!(self.auth_type, AuthType::PrivateKey), |this| {
                this.child(self.render_row(
                    "密钥路径",
                    false,
                    EditorInput::new(&self.key_path_input),
                    cx,
                ))
                .child(self.render_row(
                    "密钥密码",
                    false,
                    EditorInput::new(&self.key_passphrase_input).mask_toggle(),
                    cx,
                ))
            })
            .when(matches!(self.auth_type, AuthType::Agent), |this| {
                this.child(
                    div().w_full().pl(px(72.)).child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("使用 SSH Agent 认证。"),
                    ),
                )
            })
            // ── Footer ──
            .child(self.render_buttons(cx))
    }
}

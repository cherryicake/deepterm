use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    px,
};

use crate::components::button::{Button, ButtonVariants, Sizable, Size as ButtonSize};
use crate::components::input::{EditorInput, InputState};
use crate::components::macos_select::MacosSelect;
use crate::ssh::config::{AuthMethod, SshConnectionConfig, SshGlobalConfig};
use crate::ssh::get_current_username;
use crate::ssh::keychain::{KeychainItem, KeychainStore, classify_key_flavor};
use crate::theme::ActiveTheme;

/// 认证方式类型
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AuthType {
    Agent,
    PrivateKey,
    Password,
}

impl AuthType {
    pub fn display_name(&self) -> &'static str {
        match self {
            AuthType::Agent => "SSH Agent",
            AuthType::PrivateKey => "私钥",
            AuthType::Password => "密码",
        }
    }

    pub fn all() -> Vec<AuthType> {
        vec![AuthType::Agent, AuthType::PrivateKey, AuthType::Password]
    }
}

/// Host配置表单
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
    auth_type_select: Entity<MacosSelect>,
    keychain_select: Entity<MacosSelect>,
    keychain_options: Vec<(String, String)>,
    selected_keychain_id: Option<String>,
    editing_config_id: Option<String>,

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

            let nickname_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("我的服务器")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let hostname_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("192.168.1.100 或 example.com")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let port_input = cx.new(|cx| InputState::new(window, cx).font_size(px(13.0)));
            port_input.update(cx, |input, cx| input.set_text("22", cx));
            let username_input = cx.new(|cx| InputState::new(window, cx).font_size(px(13.0)));
            username_input.update(cx, |input, cx| input.set_text(&default_username, cx));
            let password_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("保存到系统钥匙串")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
                    .masked(true)
            });
            let key_path_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("~/.ssh/id_ed25519")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let key_passphrase_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("可选的密钥密码")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
                    .masked(true)
            });
            let credential_label_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("DeepTerm 自动生成标签")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });

            let auth_labels: Vec<SharedString> = AuthType::all()
                .into_iter()
                .map(|a| a.display_name().into())
                .collect();
            let auth_type_select =
                MacosSelect::new(window, cx, auth_labels, AuthType::Agent.display_name());

            let keychain_select = MacosSelect::new(window, cx, vec![], "—");

            let entity = cx.entity();
            auth_type_select.update(cx, |select, _cx| {
                let entity = entity.clone();
                select.on_select(move |label, _index, _window, cx| {
                    let auth_type = match label.as_ref() {
                        "SSH Agent" => AuthType::Agent,
                        "私钥" => AuthType::PrivateKey,
                        "密码" => AuthType::Password,
                        _ => AuthType::Password,
                    };
                    cx.update_entity(&entity, |this: &mut NewHostForm, cx| {
                        this.auth_type = auth_type;
                        this.refresh_keychain_options(cx);
                        cx.notify();
                    });
                });
            });

            let entity2 = entity.clone();
            keychain_select.update(cx, |select, _cx| {
                select.on_select(move |_label, index, _window, cx| {
                    cx.update_entity(&entity2, |this: &mut NewHostForm, cx| {
                        // index 0 是"新密码"/"新密钥"，不引用已有钥匙串项
                        this.selected_keychain_id = index
                            .checked_sub(1)
                            .and_then(|i| this.keychain_options.get(i))
                            .map(|(id, _)| id.clone());
                        cx.notify();
                    });
                });
            });

            let mut this = Self {
                focus_handle,
                nickname_input,
                hostname_input,
                port_input,
                username_input,
                password_input,
                key_path_input,
                key_passphrase_input,
                credential_label_input,
                auth_type: AuthType::Agent,
                auth_type_select,
                keychain_select,
                keychain_options: Vec::new(),
                selected_keychain_id: None,
                editing_config_id: None,
                error_message: None,
                success_message: None,
                on_cancel: None,
                on_saved: None,
            };
            this.refresh_keychain_options(cx);
            this
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

    fn refresh_keychain_options(&mut self, cx: &mut Context<Self>) {
        let store = KeychainStore::load_or_default();
        let items: Vec<&KeychainItem> = match self.auth_type {
            AuthType::Password => store.identities(),
            AuthType::PrivateKey => store.keys(),
            AuthType::Agent => vec![],
        };

        let default_label = match self.auth_type {
            AuthType::Password => "新密码",
            AuthType::PrivateKey => "新密钥",
            AuthType::Agent => "—",
        };

        let mut labels: Vec<SharedString> = vec![default_label.into()];
        labels.extend(items.iter().map(|item| SharedString::from(item.label.clone())));

        self.keychain_options = items
            .iter()
            .map(|item| (item.id.clone(), item.label.clone()))
            .collect();
        self.selected_keychain_id = None;

        self.keychain_select.update(cx, |select, cx| {
            select.set_items(labels, cx);
            select.set_selected(default_label, cx);
        });
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
        self.auth_type = AuthType::Agent;
        self.auth_type_select.update(cx, |select, cx| {
            select.set_selected(AuthType::Agent.display_name(), cx);
        });
        self.keychain_select.update(cx, |select, cx| {
            select.set_items(vec![], cx);
            select.set_selected("—", cx);
        });
        self.keychain_options.clear();
        self.selected_keychain_id = None;
        self.editing_config_id = None;
        self.refresh_keychain_options(cx);
        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    /// 加载已有配置到表单，进入编辑模式。
    pub fn edit_config(&mut self, config: &SshConnectionConfig, cx: &mut Context<Self>) {
        self.reset(cx);

        self.nickname_input.update(cx, |input, cx| {
            if let Some(ref nick) = config.nickname {
                input.set_text(nick, cx);
            } else {
                input.clear(cx);
            }
        });
        self.hostname_input
            .update(cx, |input, cx| input.set_text(&config.host, cx));
        self.port_input
            .update(cx, |input, cx| input.set_text(&config.port.to_string(), cx));
        self.username_input
            .update(cx, |input, cx| input.set_text(&config.username, cx));

        // 解析认证方式
        let (auth_type, keychain_id) = match config.auth_method.clone() {
            AuthMethod::Agent => (AuthType::Agent, None),
            AuthMethod::PasswordRef { keychain_id } => (AuthType::Password, Some(keychain_id)),
            AuthMethod::PrivateKeyRef { keychain_id } => (AuthType::PrivateKey, Some(keychain_id)),
            AuthMethod::Password { password } => {
                self.password_input
                    .update(cx, |input, cx| input.set_text(&password, cx));
                (AuthType::Password, None)
            }
            AuthMethod::PrivateKey { path, passphrase } => {
                self.key_path_input.update(cx, |input, cx| {
                    input.set_text(path.to_string_lossy().to_string(), cx);
                });
                if let Some(pp) = passphrase {
                    self.key_passphrase_input
                        .update(cx, |input, cx| input.set_text(pp, cx));
                }
                (AuthType::PrivateKey, None)
            }
            AuthMethod::Auto => (AuthType::Agent, None),
        };

        self.auth_type = auth_type;
        self.auth_type_select.update(cx, |select, cx| {
            select.set_selected(auth_type.display_name(), cx);
        });

        self.refresh_keychain_options(cx);

        if let Some(ref id) = keychain_id {
            self.selected_keychain_id = Some(id.clone());
            if let Some((_, label)) = self.keychain_options.iter().find(|(k_id, _)| k_id == id) {
                self.keychain_select.update(cx, |select, cx| {
                    select.set_selected(label.clone(), cx);
                });
            }
        }

        self.editing_config_id = Some(config.id());
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
        // 优先使用选中的钥匙串项
        if let Some(ref id) = self.selected_keychain_id {
            return match self.auth_type {
                AuthType::Password => Ok(AuthMethod::password_ref(id)),
                AuthType::PrivateKey => Ok(AuthMethod::key_ref(id)),
                AuthType::Agent => Ok(AuthMethod::Agent),
            };
        }

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

        let mut global_config = SshGlobalConfig::load();

        let saved = if let Some(ref editing_id) = self.editing_config_id {
            // 编辑模式：更新已有配置
            if let Some(pos) = global_config
                .saved_connections
                .iter()
                .position(|c| &c.id() == editing_id)
            {
                let existing = &mut global_config.saved_connections[pos];
                existing.host = hostname.trim().to_string();
                existing.port = port;
                existing.username = username.trim().to_string();
                existing.auth_method = auth_method;
                existing.nickname = if nickname.trim().is_empty() {
                    None
                } else {
                    Some(nickname.trim().to_string())
                };
                true
            } else {
                false
            }
        } else {
            // 新建模式
            let mut config =
                SshConnectionConfig::new(hostname.trim(), username.trim(), auth_method);
            config = config.with_port(port);
            if !nickname.trim().is_empty() {
                config = config.with_nickname(nickname.trim());
            }
            global_config.add_connection(config);
            true
        };

        if !saved {
            self.error_message = Some("未找到要编辑的主机配置".to_string());
            cx.notify();
            return;
        }

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
        let (title, subtitle) = if self.editing_config_id.is_some() {
            ("编辑主机", "修改 SSH 主机配置")
        } else {
            ("添加新主机", "添加 SSH 主机配置")
        };
        div()
            .w_full()
            .pb(px(16.))
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                div()
                    .mt(px(4.))
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(subtitle),
            )
    }

    fn render_row(
        &self,
        label: &str,
        _required: bool,
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
        div().w_full().pt(px(4.)).pb(px(2.)).child(
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
                    .child(div().text_sm().text_color(theme.error).child(err.clone()))
                    .into_any_element(),
            )
        } else if let Some(ref succ) = self.success_message {
            Some(
                div()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.success)
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
            .mt_2()
            .flex()
            .gap(px(8.0))
            .child(
                div().flex_1().child(
                    Button::new("new-host-cancel")
                        .label("取消")
                        .rounded(px(12.))
                        .with_size(ButtonSize::Small)
                        // .width(px(86.))
                        .px(px(14.))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                ),
            )
            .child(
                div().flex_1().child(
                    Button::new("new-host-save")
                        .label("保存")
                        .success()
                        .rounded(px(12.))
                        .with_size(ButtonSize::Small)
                        // .width(px(86.))
                        .px(px(14.))
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                ),
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
                                    .child(div().text_sm().text_color(theme.text).child("端口")),
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
                                    .child(div().text_sm().text_color(theme.text).child("用户名")),
                            )
                            .child(div().flex_1().child(EditorInput::new(&self.username_input))),
                    ),
            )
            // ── Authentication ──
            .child(self.render_section_title("认证", cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .w(px(60.))
                            .flex()
                            .justify_end()
                            .child(div().text_sm().text_color(theme.text).child("方式")),
                    )
                    .child(self.auth_type_select.clone()),
            )
            .when(
                matches!(self.auth_type, AuthType::Password | AuthType::PrivateKey),
                |this| {
                    this.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .w(px(60.))
                                    .flex()
                                    .justify_end()
                                    .child(div().text_sm().text_color(theme.text).child("钥匙串")),
                            )
                            .child(self.keychain_select.clone()),
                    )
                },
            )
            .when(
                matches!(self.auth_type, AuthType::Password | AuthType::PrivateKey)
                    && self.selected_keychain_id.is_none(),
                |this| {
                    this.child(self.render_row(
                        "标签",
                        false,
                        EditorInput::new(&self.credential_label_input),
                        cx,
                    ))
                },
            )
            .when(
                matches!(self.auth_type, AuthType::Password) && self.selected_keychain_id.is_none(),
                |this| {
                    this.child(self.render_row(
                        "密码",
                        false,
                        EditorInput::new(&self.password_input).mask_toggle(),
                        cx,
                    ))
                },
            )
            .when(
                matches!(self.auth_type, AuthType::PrivateKey)
                    && self.selected_keychain_id.is_none(),
                |this| {
                    this.child(self.render_row(
                        "密钥路径",
                        false,
                        EditorInput::new(&self.key_path_input),
                        cx,
                    ))
                },
            )
            // ── Footer ──
            .child(self.render_buttons(cx))
    }
}

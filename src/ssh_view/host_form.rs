use std::path::PathBuf;

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
    keychain_select: Entity<Select>,
    selected_keychain_id: Option<String>,
    keychain_items: Vec<KeychainItem>,
    error_message: Option<String>,
    success_message: Option<String>,
    editing_config_id: Option<String>,
    on_cancel: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_saved: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
}

impl HostForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let default_username = get_current_username().unwrap_or_default();

            let nickname_input = cx.new(|cx| InputState::new(window, cx).placeholder("My Server"));
            let hostname_input = cx
                .new(|cx| InputState::new(window, cx).placeholder("192.168.1.100 or example.com"));
            let port_input = cx.new(|cx| InputState::new(window, cx));
            port_input.update(cx, |input, cx| input.set_text("22", cx));
            let username_input = cx.new(|cx| InputState::new(window, cx));
            username_input.update(cx, |input, cx| input.set_text(&default_username, cx));
            let password_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Save into system keychain")
                    .masked(true)
            });
            let key_path_input =
                cx.new(|cx| InputState::new(window, cx).placeholder("~/.ssh/id_ed25519"));
            let key_passphrase_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Optional passphrase")
                    .masked(true)
            });
            let credential_label_input =
                cx.new(|cx| InputState::new(window, cx).placeholder("DeepTerm generated label"));

            let entity = cx.entity();
            let entity2 = entity.clone();

            let auth_type_select = {
                let auth_labels: Vec<SharedString> = AuthType::all()
                    .into_iter()
                    .map(|a| a.display_name().into())
                    .collect();
                let select =
                    Select::new(window, cx, auth_labels, AuthType::Password.display_name());
                select.update(cx, |select, _cx| {
                    select.searchable(false);
                    select.on_select(move |label, _index, cx| {
                        let auth_type = match label.as_ref() {
                            "Password" => AuthType::Password,
                            "Private Key" => AuthType::PrivateKey,
                            "SSH Agent" => AuthType::Agent,
                            _ => AuthType::Password,
                        };
                        cx.update_entity(&entity, |this: &mut HostForm, _cx| {
                            this.auth_type = auth_type;
                            this.selected_keychain_id = None;
                            this.reload_keychain_items();

                            let (select_items, subtitles) = {
                                let items = this.available_items();
                                let mut select_items: Vec<SharedString> = vec!["Create new".into()];
                                let mut subtitles: Vec<SharedString> = vec!["".into()];
                                for item in items {
                                    select_items.push(item.label.clone().into());
                                    subtitles.push(item.subtitle().into());
                                }
                                (select_items, subtitles)
                            };

                            this.keychain_select.update(_cx, |select: &mut Select, cx| {
                                select.set_items(select_items, cx);
                                select.set_subtitles(subtitles, cx);
                                select.set_selected("Create new", cx);
                            });

                            _cx.notify();
                        });
                    });
                });
                select
            };

            let keychain_select = {
                let select = Select::new(window, cx, vec!["Create new".into()], "Create new");
                select.update(cx, |select, _cx| {
                    select.searchable(false);
                    select.on_select(move |_label, index, cx| {
                        let keychain_id = cx.update_entity(&entity2, |this: &mut HostForm, _cx| {
                            if index == 0 {
                                None
                            } else {
                                this.available_items()
                                    .get(index - 1)
                                    .map(|item| item.id.clone())
                            }
                        });
                        cx.update_entity(&entity2, |this: &mut HostForm, _cx| {
                            this.selected_keychain_id = keychain_id.clone();

                            let username = keychain_id.as_ref().and_then(|id| {
                                this.keychain_items
                                    .iter()
                                    .find(|item| item.id == *id)
                                    .and_then(|item| {
                                        if item.kind == KeychainItemKind::Identity {
                                            item.username.clone()
                                        } else {
                                            None
                                        }
                                    })
                            });
                            if let Some(username) = username {
                                this.username_input
                                    .update(_cx, |input: &mut InputState, cx| {
                                        input.set_text(&username, cx);
                                    });
                            }

                            _cx.notify();
                        });
                    });
                });
                select
            };

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
                keychain_select,
                selected_keychain_id: None,
                keychain_items: Self::load_keychain_items(),
                error_message: None,
                success_message: None,
                editing_config_id: None,
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

    pub fn load_config(&mut self, config: &SshConnectionConfig, cx: &mut Context<Self>) {
        self.reload_keychain_items();
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

        self.clear_auth_inputs(cx);
        match &config.auth_method {
            AuthMethod::PasswordRef { keychain_id } => {
                self.auth_type = AuthType::Password;
                self.selected_keychain_id = Some(keychain_id.clone());
            }
            AuthMethod::PrivateKeyRef { keychain_id } => {
                self.auth_type = AuthType::PrivateKey;
                self.selected_keychain_id = Some(keychain_id.clone());
            }
            AuthMethod::Password { password } => {
                self.auth_type = AuthType::Password;
                self.selected_keychain_id = None;
                self.password_input.update(cx, |input, cx| {
                    input.set_text(password, cx);
                });
            }
            AuthMethod::PrivateKey { path, passphrase } => {
                self.auth_type = AuthType::PrivateKey;
                self.selected_keychain_id = None;
                self.key_path_input.update(cx, |input, cx| {
                    input.set_text(path.to_string_lossy().to_string(), cx);
                });
                if let Some(passphrase) = passphrase {
                    self.key_passphrase_input.update(cx, |input, cx| {
                        input.set_text(passphrase, cx);
                    });
                }
            }
            AuthMethod::Agent => {
                self.auth_type = AuthType::Agent;
                self.selected_keychain_id = None;
            }
            AuthMethod::Auto => {
                self.auth_type = AuthType::Password;
                self.selected_keychain_id = None;
            }
        }

        // Update selects to match loaded config
        self.auth_type_select.update(cx, |select, cx| {
            select.set_selected(self.auth_type.display_name(), cx);
        });

        let (select_items, subtitles, selected_label) = {
            let items = self.available_items();
            let mut select_items: Vec<SharedString> = vec!["Create new".into()];
            let mut subtitles: Vec<SharedString> = vec!["".into()];
            for item in items {
                select_items.push(item.label.clone().into());
                subtitles.push(item.subtitle().into());
            }
            let selected_label = self
                .selected_item()
                .map(|item| item.label.clone())
                .unwrap_or_else(|| "Create new".to_string());
            (select_items, subtitles, selected_label)
        };

        self.keychain_select.update(cx, |select, cx| {
            select.set_items(select_items, cx);
            select.set_subtitles(subtitles, cx);
            select.set_selected(selected_label, cx);
        });

        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    pub fn reset_form(&mut self, cx: &mut Context<Self>) {
        self.reload_keychain_items();
        self.editing_config_id = None;
        self.nickname_input.update(cx, |input, cx| input.clear(cx));
        self.hostname_input.update(cx, |input, cx| input.clear(cx));
        self.port_input
            .update(cx, |input, cx| input.set_text("22", cx));
        self.username_input.update(cx, |input, cx| {
            input.set_text(&get_current_username().unwrap_or_default(), cx);
        });
        self.clear_auth_inputs(cx);
        self.auth_type = AuthType::Password;
        self.selected_keychain_id = None;

        self.auth_type_select.update(cx, |select, cx| {
            select.set_selected(AuthType::Password.display_name(), cx);
        });

        let items = self.available_items();
        let mut select_items: Vec<SharedString> = vec!["Create new".into()];
        let mut subtitles: Vec<SharedString> = vec!["".into()];
        for item in items {
            select_items.push(item.label.clone().into());
            subtitles.push(item.subtitle().into());
        }

        self.keychain_select.update(cx, |select, cx| {
            select.set_items(select_items, cx);
            select.set_subtitles(subtitles, cx);
            select.set_selected("Create new", cx);
        });

        self.error_message = None;
        self.success_message = None;
        cx.notify();
    }

    fn load_keychain_items() -> Vec<KeychainItem> {
        KeychainStore::load_or_default().items
    }

    fn reload_keychain_items(&mut self) {
        self.keychain_items = Self::load_keychain_items();
    }

    fn clear_auth_inputs(&mut self, cx: &mut Context<Self>) {
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.key_path_input.update(cx, |input, cx| input.clear(cx));
        self.key_passphrase_input
            .update(cx, |input, cx| input.clear(cx));
        self.credential_label_input
            .update(cx, |input, cx| input.clear(cx));
    }

    fn available_items(&self) -> Vec<&KeychainItem> {
        let expected_kind = match self.auth_type {
            AuthType::Password => Some(KeychainItemKind::Identity),
            AuthType::PrivateKey => Some(KeychainItemKind::Key),
            AuthType::Agent => None,
        };

        self.keychain_items
            .iter()
            .filter(|item| {
                expected_kind
                    .as_ref()
                    .is_some_and(|kind| item.kind == *kind)
            })
            .collect()
    }

    fn selected_item(&self) -> Option<&KeychainItem> {
        let selected = self.selected_keychain_id.as_ref()?;
        self.keychain_items.iter().find(|item| item.id == *selected)
    }

    fn validate_form(&self, cx: &Context<Self>) -> Result<(), String> {
        let hostname = self.hostname_input.read(cx).value().to_string();
        let username = self.username_input.read(cx).value().to_string();
        let port = self.port_input.read(cx).value().to_string();

        if hostname.trim().is_empty() {
            return Err("Host address is required".to_string());
        }

        if username.trim().is_empty() {
            return Err("Username is required".to_string());
        }

        if port.parse::<u16>().is_err() {
            return Err("Invalid port number".to_string());
        }

        Ok(())
    }

    fn create_auth_method(
        &self,
        username: &str,
        host_label: &str,
        cx: &Context<Self>,
    ) -> Result<AuthMethod, String> {
        let selected_keychain_id = self.selected_keychain_id.clone();
        let credential_label = self.credential_label_input.read(cx).value().to_string();
        let password = self.password_input.read(cx).value().to_string();
        let key_path = self.key_path_input.read(cx).value().to_string();
        let key_passphrase = self.key_passphrase_input.read(cx).value().to_string();

        let mut store = KeychainStore::load_or_default();
        let fallback_label = if credential_label.trim().is_empty() {
            host_label.to_string()
        } else {
            credential_label.trim().to_string()
        };

        match self.auth_type {
            AuthType::Password => {
                if let Some(id) = selected_keychain_id {
                    let item = store
                        .find_item(&id)
                        .ok_or_else(|| "Selected identity no longer exists".to_string())?;
                    if item.kind != KeychainItemKind::Identity {
                        return Err("Selected keychain item is not an identity".to_string());
                    }
                    let identity_label = item.label.clone();
                    if !password.trim().is_empty() {
                        store
                            .upsert_identity(
                                Some(&id),
                                &identity_label,
                                Some(username.to_string()),
                                password.trim(),
                            )
                            .map_err(|err| {
                                format!("Failed to update identity in keychain: {}", err)
                            })?;
                        Ok(AuthMethod::password_ref(id))
                    } else if store
                        .get_password_secret(&id)
                        .map_err(|err| format!("Failed to read identity from keychain: {}", err))?
                        .is_some()
                    {
                        Ok(AuthMethod::password_ref(id))
                    } else {
                        Err("Selected identity has no stored password. Re-enter the password or create a new identity.".to_string())
                    }
                } else if !password.trim().is_empty() {
                    let keychain_id = store
                        .create_identity(&fallback_label, Some(username.to_string()), &password)
                        .map_err(|err| format!("Failed to save identity into keychain: {}", err))?;
                    Ok(AuthMethod::password_ref(keychain_id))
                } else {
                    Ok(AuthMethod::Auto)
                }
            }
            AuthType::PrivateKey => {
                if let Some(id) = selected_keychain_id {
                    let item = store
                        .find_item(&id)
                        .ok_or_else(|| "Selected key no longer exists".to_string())?;
                    if item.kind != KeychainItemKind::Key {
                        return Err("Selected keychain item is not a key".to_string());
                    }
                    let key_label = item.label.clone();
                    let key_path = item.key_path.clone();
                    let certificate_path = item.certificate_path.clone();
                    let key_flavor = item.key_flavor;
                    if !key_passphrase.trim().is_empty() {
                        let key_path = key_path
                            .ok_or_else(|| "Selected key is missing its path".to_string())?;
                        store
                            .upsert_key(
                                Some(&id),
                                &key_label,
                                key_path,
                                certificate_path,
                                key_flavor,
                                Some(key_passphrase.trim()),
                            )
                            .map_err(|err| format!("Failed to update keychain key: {}", err))?;
                    }
                    Ok(AuthMethod::key_ref(id))
                } else if !key_path.trim().is_empty() {
                    let keychain_id = store
                        .create_key(
                            &fallback_label,
                            PathBuf::from(key_path.trim()),
                            None,
                            classify_key_flavor(PathBuf::from(key_path.trim()).as_path(), None),
                            (!key_passphrase.trim().is_empty()).then_some(key_passphrase.as_str()),
                        )
                        .map_err(|err| format!("Failed to save key into keychain: {}", err))?;
                    Ok(AuthMethod::key_ref(keychain_id))
                } else {
                    Ok(AuthMethod::Auto)
                }
            }
            AuthType::Agent => Ok(AuthMethod::Agent),
        }
    }

    fn save_config(&mut self, cx: &mut Context<Self>) {
        self.error_message = None;
        self.success_message = None;

        if let Err(e) = self.validate_form(cx) {
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

        let auth_method = match self.create_auth_method(username.trim(), &host_label, cx) {
            Ok(auth_method) => auth_method,
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
        if let Some(ref editing_id) = self.editing_config_id {
            global_config.remove_connection(editing_id);
        }
        global_config.add_connection(config);

        match global_config.save() {
            Ok(()) => {
                self.reload_keychain_items();
                self.success_message = Some("Host saved successfully".to_string());
                cx.notify();

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

    fn handle_cancel(&mut self, cx: &mut Context<Self>) {
        self.reset_form(cx);
        if let Some(ref callback) = self.on_cancel {
            callback(cx);
        }
    }

    fn render_labeled_input(
        &self,
        label: &str,
        input: &Entity<InputState>,
        mask_toggle: bool,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        let editor = if mask_toggle {
            EditorInput::new(input).mask_toggle()
        } else {
            EditorInput::new(input)
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(label.to_string()),
            )
            .child(editor)
    }

    fn render_auth_type_selector(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("Authentication"),
            )
            .child(self.auth_type_select.clone())
    }

    fn render_keychain_selector(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let title = match self.auth_type {
            AuthType::Password => "Keychain Identity",
            AuthType::PrivateKey => "Keychain Key",
            AuthType::Agent => "",
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(div().text_xs().text_color(theme.text_muted).child(title))
            .child(self.keychain_select.clone())
    }

    fn render_new_identity_fields(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(self.render_labeled_input("Identity Label", &self.credential_label_input, false, cx))
            .child(self.render_labeled_input("Password", &self.password_input, true, cx))
            .child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(theme.elevated_background)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child("Password is stored in the system keychain and only the reference is written into ssh.json."),
                    ),
            )
    }

    fn render_new_key_fields(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(self.render_labeled_input("Key Label", &self.credential_label_input, false, cx))
            .child(self.render_labeled_input("Private Key Path", &self.key_path_input, false, cx))
            .child(self.render_labeled_input("Passphrase (optional)", &self.key_passphrase_input, true, cx))
            .child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(theme.elevated_background)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child("DeepTerm stores the key metadata here and keeps the optional passphrase in the system keychain."),
                    ),
            )
    }

    fn render_selected_item_hint(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
        let subtitle = self
            .selected_item()
            .map(|item| item.subtitle())
            .unwrap_or_default();

        div()
            .w_full()
            .px(px(10.0))
            .py(px(10.0))
            .rounded(px(6.0))
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text)
                            .child("Using existing keychain item"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child(subtitle),
                    ),
            )
    }

    fn render_auth_extra_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        match self.auth_type {
            AuthType::Password => div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(self.render_keychain_selector(cx))
                .when(self.selected_keychain_id.is_some(), |this| {
                    this.child(self.render_selected_item_hint(cx))
                })
                .when(self.selected_keychain_id.is_none(), |this| {
                    this.child(self.render_new_identity_fields(cx))
                })
                .into_any_element(),
            AuthType::PrivateKey => div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(self.render_keychain_selector(cx))
                .when(self.selected_keychain_id.is_some(), |this| {
                    this.child(self.render_selected_item_hint(cx))
                })
                .when(self.selected_keychain_id.is_none(), |this| {
                    this.child(self.render_new_key_fields(cx))
                })
                .into_any_element(),
            AuthType::Agent => div()
                .w_full()
                .py(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_subtle)
                        .child("Uses SSH agent for authentication."),
                )
                .into_any_element(),
        }
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme().ui;
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
            .border_color(theme.border)
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(div().text_xs().text_color(theme.text_muted).child(subtitle))
    }

    fn render_message(&self, cx: &App) -> Option<impl IntoElement> {
        let theme = cx.theme().ui;
        if let Some(ref error) = self.error_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(theme.error_background)
                    .border_1()
                    .border_color(theme.error)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.error_foreground)
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
                    .bg(theme.success_background)
                    .border_1()
                    .border_color(theme.success)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.success_foreground)
                            .child(success.clone()),
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
            .gap(px(8.0))
            .pt(px(16.0))
            .child(
                div()
                    .flex_1()
                    .child(
                        Button::new("cancel-btn")
                            .label("Cancel")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.handle_cancel(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .child(
                        Button::new("save-btn")
                            .label("Save")
                            .success()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.save_config(cx);
                            })),
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
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(self.render_header(cx))
            .children(self.render_message(cx).into_iter())
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(self.render_labeled_input(
                        "Nickname (optional)",
                        &self.nickname_input,
                        false,
                        cx,
                    ))
                    .child(self.render_labeled_input(
                        "Host Address *",
                        &self.hostname_input,
                        false,
                        cx,
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap(px(12.0))
                            .child(div().w(px(80.0)).child(self.render_labeled_input(
                                "Port",
                                &self.port_input,
                                false,
                                cx,
                            )))
                            .child(div().flex_1().child(self.render_labeled_input(
                                "Username *",
                                &self.username_input,
                                false,
                                cx,
                            ))),
                    )
                    .child(self.render_auth_type_selector(cx))
                    .child(self.render_auth_extra_field(cx)),
            )
            .child(self.render_buttons(cx))
    }
}

use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, IntoElement, Render, SharedString, Window, div, px,
};

use crate::components::callbacks::OptionalAppCallback;
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

/// 端口转发草稿的默认远端绑定地址。
///
/// 只绑回环：`-R 3801:...` 在 OpenSSH 里等价于绑 127.0.0.1，与本应用一致——
/// 对外暴露必须是用户的显式选择（自己把地址改成 `0.0.0.0`）。
const DEFAULT_BIND_HOST: &str = "127.0.0.1";

/// 解析地址输入框：去空白后非空即原样采用（IPv6 用方括号形态 `[::1]`）。
fn parse_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 解析端口输入框：只接受 `0..=65535` 的十进制数。
/// `0` 对远端端口是合法的「让服务器挑」，对本机目标端口则无意义，
/// 由调用方决定要不要拒绝。
fn parse_port(raw: &str) -> Option<u16> {
    raw.trim().parse::<u16>().ok()
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
    group_select: Entity<MacosSelect>,
    group_options: Vec<Option<String>>,
    selected_group_id: Option<String>,

    /// 已添加的端口转发规则（保存时写入 Host 配置）。
    port_forwards: Vec<crate::ssh::port_forward::PortForwardRule>,
    /// 新增规则的草稿输入：远端 地址/端口 与 本机 地址/端口 各一对，
    /// 用 `:` 分隔展示（ip 和 port 分框，而不是让用户手拼 `host:port`）。
    forward_bind_host_input: Entity<InputState>,
    forward_bind_port_input: Entity<InputState>,
    forward_target_host_input: Entity<InputState>,
    forward_target_port_input: Entity<InputState>,

    error_message: Option<String>,
    success_message: Option<String>,
    on_cancel: OptionalAppCallback,
    on_saved: OptionalAppCallback,
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
            let forward_bind_host_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(DEFAULT_BIND_HOST)
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let forward_bind_port_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("3801")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let forward_target_host_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("127.0.0.1")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });
            let forward_target_port_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("3801")
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
            let group_select = MacosSelect::new(window, cx, vec!["未分组".into()], "未分组");

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
            let entity3 = entity.clone();
            group_select.update(cx, |select, _cx| {
                select.on_select(move |_label, index, _window, cx| {
                    cx.update_entity(&entity3, |this: &mut NewHostForm, cx| {
                        this.selected_group_id =
                            this.group_options.get(index).and_then(|v| v.clone());
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
                group_select,
                group_options: vec![None],
                selected_group_id: None,
                port_forwards: Vec::new(),
                forward_bind_host_input,
                forward_bind_port_input,
                forward_target_host_input,
                forward_target_port_input,
                error_message: None,
                success_message: None,
                on_cancel: None,
                on_saved: None,
            };
            this.refresh_keychain_options(cx);
            this.refresh_group_options(cx);
            this
        })
    }

    fn refresh_group_options(&mut self, cx: &mut Context<Self>) {
        let cfg = SshGlobalConfig::load();
        let mut labels = Vec::with_capacity(cfg.host_groups.len() + 1);
        let mut opts = Vec::with_capacity(cfg.host_groups.len() + 1);
        labels.push("未分组".into());
        opts.push(None);
        for group in &cfg.host_groups {
            labels.push(group.name.clone().into());
            opts.push(Some(group.id.clone()));
        }
        self.group_options = opts;
        let current_label = self
            .selected_group_id
            .as_ref()
            .and_then(|id| cfg.host_groups.iter().find(|g| &g.id == id))
            .map(|g| SharedString::from(g.name.clone()))
            .unwrap_or_else(|| "未分组".into());
        self.group_select.update(cx, |select, cx| {
            select.set_items(labels, cx);
            select.set_selected(current_label, cx);
        });
    }

    pub fn set_target_group(&mut self, group_id: Option<String>, cx: &mut Context<Self>) {
        self.selected_group_id = group_id;
        self.refresh_group_options(cx);
        cx.notify();
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
        labels.extend(
            items
                .iter()
                .map(|item| SharedString::from(item.label.clone())),
        );

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
            input.set_text(get_current_username().unwrap_or_default(), cx);
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
        self.selected_group_id = None;
        self.port_forwards.clear();
        for input in [
            &self.forward_bind_host_input,
            &self.forward_bind_port_input,
            &self.forward_target_host_input,
            &self.forward_target_port_input,
        ] {
            input.update(cx, |input, cx| input.clear(cx));
        }
        self.refresh_keychain_options(cx);
        self.refresh_group_options(cx);
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
            .update(cx, |input, cx| input.set_text(config.port.to_string(), cx));
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
        self.selected_group_id = config.group_id.clone();
        self.refresh_group_options(cx);

        self.port_forwards = config.port_forwards.clone();

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
                existing.group_id = self.selected_group_id.clone();
                existing.port_forwards = self.port_forwards.clone();
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
            config.group_id = self.selected_group_id.clone();
            config.port_forwards = self.port_forwards.clone();
            let next_sort = global_config
                .saved_connections
                .iter()
                .filter(|c| c.group_id == config.group_id)
                .filter_map(|c| c.sort_order)
                .max()
                .map_or(0, |max| max + 1);
            config.sort_order = Some(next_sort);
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
        } else {
            self.success_message.as_ref().map(|succ| {
                div()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.success)
                            .child(succ.clone()),
                    )
                    .into_any_element()
            })
        }
    }

    /// 端口转发规则编辑器：列出已添加的规则，并提供一行
    /// 「远端 地址:端口 -> 本机 地址:端口」的草稿。
    ///
    /// 逐条显示而不是一个总开关：转发本来就是多条规则（`ssh -R` 可以重复出现），
    /// 一个开关没法表达「哪一条没建起来」。
    ///
    /// 地址与端口分框、用 `:` 连接：手拼 `host:port` 容易写错（IPv6 冒号歧义）
    /// 也读不出结构。两侧地址框等宽（`flex_1`）、端口框定宽，中间 `->` 表达方向。
    fn render_port_forwards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let rows = self.port_forwards.iter().enumerate().map(|(index, rule)| {
            let text = rule.summary();
            div()
                .id(("forward-row", index))
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(6.))
                .bg(theme.elevated_background)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.text)
                        .child(text),
                )
                .child(
                    Button::new(("forward-remove", index))
                        .label("移除")
                        .ghost()
                        .with_size(ButtonSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if index < this.port_forwards.len() {
                                this.port_forwards.remove(index);
                            }
                            cx.notify();
                        })),
                )
        });

        // 草稿行：远端 [地址]:[端口] -> 本机 [地址]:[端口]。
        // ip 和端口分成两个框、中间用 `:` 连接——让用户手拼 `host:port`
        // 既容易写错（IPv6 冒号歧义）也读不出结构。
        let colon = || {
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_subtle)
                .child(":")
        };
        let draft = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .id("forward-bind-host-input")
                    .debug_selector(|| "forward-bind-host-input".to_string())
                    .flex_1()
                    .min_w_0()
                    .child(EditorInput::new(&self.forward_bind_host_input)),
            )
            .child(colon())
            .child(
                div()
                    .id("forward-bind-port-input")
                    .debug_selector(|| "forward-bind-port-input".to_string())
                    .w(px(56.))
                    .flex_shrink_0()
                    .child(EditorInput::new(&self.forward_bind_port_input)),
            )
            .child(
                // 箭头：这条规则的方向（远端 -> 本机）。文字 `->` 比 chevron
                // 图标更明确表达「转发到」；宽度固定，两侧地址框才能等宽。
                div()
                    .flex_shrink_0()
                    .px(px(2.))
                    .text_xs()
                    .text_color(theme.text_subtle)
                    .child("->"),
            )
            .child(
                div()
                    .id("forward-target-host-input")
                    .debug_selector(|| "forward-target-host-input".to_string())
                    .flex_1()
                    .min_w_0()
                    .child(EditorInput::new(&self.forward_target_host_input)),
            )
            .child(colon())
            .child(
                div()
                    .id("forward-target-port-input")
                    .debug_selector(|| "forward-target-port-input".to_string())
                    .w(px(56.))
                    .flex_shrink_0()
                    .child(EditorInput::new(&self.forward_target_port_input)),
            )
            .child(
                Button::new("forward-add")
                    .label("添加")
                    .with_size(ButtonSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.add_forward_draft(cx))),
            );

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(rows)
            .child(draft)
            .child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(theme.text_subtle)
                    .child("远端 地址:端口 -> 本机 地址:端口；远端地址留空默认 127.0.0.1（只允许远端本机访问）"),
            )
    }

    /// 把草稿行加进规则列表。
    ///
    /// 四个框分别读远端 地址/端口 与本机 地址/端口；解析不出来就在表单上
    /// 给出原因，而不是静默丢弃（用户会以为加上了）。
    fn add_forward_draft(&mut self, cx: &mut Context<Self>) {
        let bind_host_raw = self.forward_bind_host_input.read(cx).value().trim().to_string();
        let bind_port_raw = self.forward_bind_port_input.read(cx).value().trim().to_string();
        let target_host_raw = self.forward_target_host_input.read(cx).value().trim().to_string();
        let target_port_raw = self.forward_target_port_input.read(cx).value().trim().to_string();

        // 远端地址留空 = 默认绑回环（对外暴露必须显式填 0.0.0.0）
        let bind_host = if bind_host_raw.is_empty() {
            DEFAULT_BIND_HOST.to_string()
        } else {
            match parse_address(&bind_host_raw) {
                Some(v) => v,
                None => {
                    self.error_message = Some(format!("远端地址 `{bind_host_raw}` 不合法"));
                    cx.notify();
                    return;
                }
            }
        };
        let Some(bind_port) = parse_port(&bind_port_raw) else {
            self.error_message = Some(format!(
                "远端端口 `{bind_port_raw}` 不合法：写 0-65535（0 = 让远端自己挑）"
            ));
            cx.notify();
            return;
        };
        let Some(target_host) = parse_address(&target_host_raw) else {
            self.error_message =
                Some("本机地址不能为空：写服务监听的地址（通常是 127.0.0.1）".to_string());
            cx.notify();
            return;
        };
        let Some(target_port) = parse_port(&target_port_raw).filter(|p| *p > 0) else {
            self.error_message =
                Some(format!("本机端口 `{target_port_raw}` 不合法：写 1-65535"));
            cx.notify();
            return;
        };

        // 同一远端绑定重复添加没有意义（建立时会报「已登记过」），当场拦下。
        // 端口 0 除外：那是「让远端挑端口」，多条并存是合法的。
        if bind_port != 0
            && self
                .port_forwards
                .iter()
                .any(|r| r.bind_host == bind_host && r.bind_port == bind_port)
        {
            self.error_message =
                Some(format!("远端绑定 {bind_host}:{bind_port} 已经在列表里了"));
            cx.notify();
            return;
        }

        self.port_forwards
            .push(crate::ssh::port_forward::PortForwardRule {
                bind_host,
                bind_port,
                target_host,
                target_port,
                description: String::new(),
            });
        for input in [
            &self.forward_bind_host_input,
            &self.forward_bind_port_input,
            &self.forward_target_host_input,
            &self.forward_target_port_input,
        ] {
            input.update(cx, |input, cx| input.clear(cx));
        }
        self.error_message = None;
        cx.notify();
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

crate::impl_focusable!(NewHostForm);

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
            .children(self.render_message(cx))
            // ── Basic Info ──
            .child(self.render_section_title("基本信息", cx))
            // 分组行与认证行同构：select 直接作 flex 子项取自然宽度。
            // 不能走 render_row 的 flex_1 包裹——那会把触发器拉满整列，
            // 弹窗锚在触发器全宽的左上角，跑到列首去（认证行是对的，照它修）。
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
                            .child(div().text_sm().text_color(theme.text).child("分组")),
                    )
                    .child(self.group_select.clone()),
            )
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
            // ── Port forward ──
            .child(self.render_section_title("端口转发（把本机服务暴露到远端）", cx))
            .child(self.render_port_forwards(cx))
            // ── Footer ──
            .child(self.render_buttons(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_address_is_rejected_so_caller_can_default_it() {
        // 远端地址留空默认绑回环由 add_forward_draft 负责；解析器只认
        // 「非空即原样采用」，空白串必须报 None 而不是空串。
        assert_eq!(parse_address(""), None);
        assert_eq!(parse_address("   "), None);
        assert_eq!(parse_address("0.0.0.0"), Some("0.0.0.0".to_string()));
        assert_eq!(parse_address(" [::1] "), Some("[::1]".to_string()));
    }

    #[test]
    fn ports_are_strictly_decimal_u16() {
        assert_eq!(parse_port("3801"), Some(3801));
        // 0 对远端端口合法（让服务器挑端口）；本机端口由调用方拒绝 0
        assert_eq!(parse_port("0"), Some(0));
        assert_eq!(parse_port("65535"), Some(65535));
        assert_eq!(parse_port("65536"), None, "越界端口");
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port(" 80 "), Some(80), "首尾空白应被裁掉");
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;
    use gpui::{TestAppContext, px, size};

    struct FormHost {
        form: Entity<NewHostForm>,
    }

    impl Render for FormHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(520.)).h(px(900.)).child(self.form.clone())
        }
    }

    /// 表单能真实渲染出来（走完整 render 路径，而不是只调解析函数）。
    ///
    /// 覆盖的是「加规则 → 出现在列表 → 保存进配置」这条用户路径：
    /// 只测解析函数无法发现 render 里把字段接错。
    #[gpui::test]
    fn adding_a_forward_rule_shows_up_and_saves(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);

        let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<NewHostForm>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot_for_window = slot.clone();
        let window = cx.open_window(size(px(520.), px(900.)), move |window, cx| {
            let form = NewHostForm::new(window, cx);
            *slot_for_window.borrow_mut() = Some(form.clone());
            FormHost { form }
        });
        cx.run_until_parked();
        let form = slot.borrow().clone().expect("表单应已构造");

        // 真正画一帧：render 路径里的接线错误（字段用错、元素缺失）只有渲染才暴露。
        let handle: gpui::AnyWindowHandle = window.into();
        let _ = cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        });

        // 填一条规则并添加：远端地址留空 → 默认绑回环
        cx.update(|cx| {
            form.update(cx, |form, cx| {
                form.hostname_input
                    .update(cx, |input, cx| input.set_text("example.com", cx));
                form.forward_bind_port_input
                    .update(cx, |input, cx| input.set_text("3801", cx));
                form.forward_target_host_input
                    .update(cx, |input, cx| input.set_text("127.0.0.1", cx));
                form.forward_target_port_input
                    .update(cx, |input, cx| input.set_text("8080", cx));
                form.add_forward_draft(cx);

                assert_eq!(form.port_forwards.len(), 1, "草稿应被加进规则列表");
                let rule = &form.port_forwards[0];
                assert_eq!(rule.bind_host, "127.0.0.1", "远端地址留空默认绑回环");
                assert_eq!(rule.bind_port, 3801);
                assert_eq!(rule.target_host, "127.0.0.1");
                assert_eq!(rule.target_port, 8080);
                assert!(form.error_message.is_none(), "合法输入不该报错");
                assert_eq!(
                    form.forward_bind_port_input.read(cx).value().as_ref(),
                    "",
                    "添加后草稿应清空，方便连着加下一条"
                );
            });
        });

        // 加完再画一帧：规则行本身也要能渲染出来。
        let _ = cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        });

        // 坏输入：报错且不产生规则
        cx.update(|cx| {
            form.update(cx, |form, cx| {
                form.forward_bind_port_input
                    .update(cx, |input, cx| input.set_text("bad-port", cx));
                form.forward_target_host_input
                    .update(cx, |input, cx| input.set_text("127.0.0.1", cx));
                form.forward_target_port_input
                    .update(cx, |input, cx| input.set_text("8080", cx));
                form.add_forward_draft(cx);
                assert_eq!(form.port_forwards.len(), 1, "非法输入不该加规则");
                assert!(form.error_message.is_some(), "非法输入必须给出原因");
            });
        });

        // 重复的远端绑定当场拦下（端口 0 除外：让远端挑端口可多条并存）
        cx.update(|cx| {
            form.update(cx, |form, cx| {
                form.forward_bind_port_input
                    .update(cx, |input, cx| input.set_text("3801", cx));
                form.forward_target_host_input
                    .update(cx, |input, cx| input.set_text("127.0.0.1", cx));
                form.forward_target_port_input
                    .update(cx, |input, cx| input.set_text("9000", cx));
                form.add_forward_draft(cx);
                assert_eq!(form.port_forwards.len(), 1, "重复的远端绑定不该加进去");
                assert!(
                    form.error_message.as_deref().unwrap_or("").contains("已经在列表里"),
                    "重复添加必须说明原因：{:?}",
                    form.error_message
                );
            });
        });

        // 布局：两侧地址框等宽（同一条规则的两端，一宽一窄读不出「A -> B」），
        // 端口框定宽且等宽，顺序为 远端地址 : 端口 -> 本机地址 : 端口。
        // debug_bounds 在 VisualTestContext 上（TestAppContext 没有）。
        let mut visual = gpui::VisualTestContext::from_window(handle, cx);
        visual.run_until_parked();
        let bind_host = visual
            .debug_bounds("forward-bind-host-input")
            .expect("远端地址框应有布局");
        let bind_port = visual
            .debug_bounds("forward-bind-port-input")
            .expect("远端端口框应有布局");
        let target_host = visual
            .debug_bounds("forward-target-host-input")
            .expect("本机地址框应有布局");
        let target_port = visual
            .debug_bounds("forward-target-port-input")
            .expect("本机端口框应有布局");
        assert_eq!(
            bind_host.size.width, target_host.size.width,
            "两侧地址框宽度必须一致：远端 {:?} vs 本机 {:?}",
            bind_host.size.width, target_host.size.width
        );
        assert_eq!(
            bind_port.size.width, target_port.size.width,
            "两个端口框宽度必须一致"
        );
        assert!(
            bind_host.right() <= bind_port.left()
                && bind_port.right() <= target_host.left()
                && target_host.right() <= target_port.left(),
            "顺序应为 远端地址:端口 -> 本机地址:端口"
        );

        // 移除
        cx.update(|cx| {
            form.update(cx, |form, cx| {
                form.port_forwards.clear();
                assert!(form.port_forwards.is_empty());
                cx.notify();
            });
        });
    }
}

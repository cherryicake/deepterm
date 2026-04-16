use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use anyhow::anyhow;
use gpui::prelude::*;
use gpui::{
    App, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, Render, ScrollHandle, ScrollWheelEvent, Window, div, px, svg,
};

use crate::assets::Icon;
use crate::components::button::{Button, ButtonVariants, Icon as ButtonIconType, Sizable};
use crate::components::input::{EditorInput, InputState};
use crate::ssh::keychain::{
    KeychainItem, KeychainItemKind, KeychainKeyFlavor, KeychainStore, normalize_private_key_path,
    sibling_certificate_path,
};
use crate::theme::ActiveTheme;
const GRID_GAP_PX: f32 = 12.0;
const CONTENT_PADDING_PX: f32 = 0.0;

pub struct MultilineTextArea {
    focus_handle: FocusHandle,
    text: String,
    placeholder: String,
    height: f32,
    scroll_handle: ScrollHandle,
    on_change: Option<Box<dyn Fn(&str, &mut App) + Send + Sync>>,
}

impl MultilineTextArea {
    pub fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            text: String::new(),
            placeholder: String::new(),
            height: 120.0,
            scroll_handle: ScrollHandle::new(),
            on_change: None,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        if let Some(ref callback) = self.on_change {
            callback(&self.text, cx);
        }
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.text.clear();
        if let Some(ref callback) = self.on_change {
            callback(&self.text, cx);
        }
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn append_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.text.push_str(text);
        if let Some(ref callback) = self.on_change {
            callback(&self.text, cx);
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if modifiers.secondary() && key.eq_ignore_ascii_case("v") {
            if let Some(clipboard) = cx.read_from_clipboard()
                && let Some(text) = clipboard.text()
            {
                self.append_text(&text, cx);
            }
            return;
        }

        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }

        match key {
            "backspace" => {
                self.text.pop();
                if let Some(ref callback) = self.on_change {
                    callback(&self.text, cx);
                }
                cx.notify();
            }
            "enter" | "return" => self.append_text("\n", cx),
            "space" => self.append_text(" ", cx),
            _ if key.len() == 1 => self.append_text(key, cx),
            _ => {}
        }
    }
}

impl Focusable for MultilineTextArea {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MultilineTextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_focused = self.focus_handle.is_focused(window);
        let lines: Vec<String> = if self.text.is_empty() {
            vec![if is_focused {
                String::new()
            } else {
                self.placeholder.clone()
            }]
        } else {
            self.text.split('\n').map(|line| line.to_string()).collect()
        };

        let mut rendered_lines = lines;
        if is_focused {
            if let Some(last) = rendered_lines.last_mut() {
                last.push('▏');
            }
        }

        div()
            .id("keychain-private-key-textarea")
            .w_full()
            .h(px(self.height))
            .track_scroll(&self.scroll_handle)
            .track_focus(&self.focus_handle)
            .key_context("KeychainMultiline")
            .cursor(CursorStyle::IBeam)
            .rounded(px(8.0))
            .border_1()
            .border_color(if is_focused {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.elevated_background)
            .px(px(12.0))
            .py(px(10.0))
            .overflow_y_scroll()
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|_, _event: &ScrollWheelEvent, _window, cx| {
                cx.notify();
            }))
            .child(div().w_full().flex().flex_col().gap(px(2.0)).children(
                rendered_lines.into_iter().map(|line| {
                    div()
                        .text_sm()
                        .text_color(if self.text.is_empty() && !is_focused {
                            theme.text_subtle
                        } else {
                            theme.text
                        })
                        .child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line
                        })
                        .into_any_element()
                }),
            ))
    }
}

pub struct KeychainView {
    focus_handle: FocusHandle,
    items: Vec<KeychainItem>,
    selected_item_id: Option<String>,
    on_new_key_click: Option<Box<dyn Fn(&mut App, Option<PathBuf>) + Send + Sync>>,
    on_new_identity_click: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_edit_item_click: Option<Box<dyn Fn(&mut App, KeychainItem) + Send + Sync>>,
}

impl KeychainView {
    pub fn new(_window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            items: Self::load_items(),
            selected_item_id: None,
            on_new_key_click: None,
            on_new_identity_click: None,
            on_edit_item_click: None,
        })
    }

    fn load_items() -> Vec<KeychainItem> {
        KeychainStore::load_or_default().items
    }

    pub fn refresh_items(&mut self, cx: &mut Context<Self>) {
        self.items = Self::load_items();
        cx.notify();
    }

    pub fn on_new_key_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App, Option<PathBuf>) + Send + Sync + 'static,
    {
        self.on_new_key_click = Some(Box::new(callback));
    }

    pub fn on_new_identity_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App) + Send + Sync + 'static,
    {
        self.on_new_identity_click = Some(Box::new(callback));
    }

    pub fn on_edit_item_click<F>(&mut self, callback: F)
    where
        F: Fn(&mut App, KeychainItem) + Send + Sync + 'static,
    {
        self.on_edit_item_click = Some(Box::new(callback));
    }

    fn keys(&self) -> Vec<&KeychainItem> {
        self.items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Key)
            .collect()
    }

    fn identities(&self) -> Vec<&KeychainItem> {
        self.items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Identity)
            .collect()
    }

    fn render_card(
        &self,
        item: &KeychainItem,
        icon: Icon,
        icon_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_selected = self.selected_item_id.as_ref() == Some(&item.id);
        let item_id = item.id.clone();
        let item_for_callback = item.clone();

        div()
            .id(format!("keychain-card-{}", item.id))
            .w_full()
            .h(px(64.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(if is_selected {
                theme.accent.opacity(if cx.theme().appearance.is_dark() {
                    0.18
                } else {
                    0.10
                })
            } else {
                theme.elevated_background
            })
            .border_1()
            .border_color(if is_selected {
                theme.accent
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|style| {
                if is_selected {
                    style
                } else {
                    style.bg(theme.tab_hover_background)
                }
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.selected_item_id = Some(item_id.clone());
                    if let Some(ref callback) = this.on_edit_item_click {
                        callback(cx, item_for_callback.clone());
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(40.0))
                    .h(px(40.0))
                    .rounded(px(8.0))
                    .bg(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .size(px(20.0))
                            .path(icon.path())
                            .text_color(theme.accent_foreground),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .truncate()
                            .child(item.label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_subtle)
                            .child(item.subtitle()),
                    ),
            )
    }

    fn render_section_header(&self, title: &str, _count: usize, cx: &App) -> impl IntoElement {
        div().flex().items_center().child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().ui.text_muted)
                .child(title.to_string()),
        )
    }

    fn render_action_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(28.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                // NEW KEY button using Button component
                Button::new("new-key-btn")
                    .label("NEW KEY")
                    .icon(ButtonIconType::new("icons/plus.svg").small())
                    .small()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        if let Some(ref callback) = this.on_new_key_click {
                            callback(cx, None);
                        }
                    })),
            )
            .child(
                // NEW IDENTITY button using Button component
                Button::new("new-identity-btn")
                    .label("NEW IDENTITY")
                    .icon(ButtonIconType::new("icons/plus.svg").small())
                    .small()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        if let Some(ref callback) = this.on_new_identity_click {
                            callback(cx);
                        }
                    })),
            )
    }

    fn render_list_grid(
        &self,
        items: &[&KeychainItem],
        icon: Icon,
        icon_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .grid()
            .grid_cols(2)
            .gap(px(GRID_GAP_PX))
            .children(items.iter().map(|item| {
                self.render_card(item, icon, icon_color, cx)
                    .into_any_element()
            }))
    }
}

impl Focusable for KeychainView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for KeychainView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let keys = self.keys();
        let identities = self.identities();

        div()
            .id("keychain-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.panel_background)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                if let Some(path) = paths.paths().first() {
                    this.selected_item_id = None;
                    if let Some(ref callback) = this.on_new_key_click {
                        callback(cx, Some(path.clone()));
                    }
                    log::info!("Dropped key path into keychain view: {:?}", path);
                }
            }))
            .p(px(CONTENT_PADDING_PX))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .px(px(16.0))
                    .pt(px(8.0))
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("Keychain"),
            )
            .child(
                div()
                    .px(px(16.0))
                    .text_sm()
                    .text_color(theme.text_subtle)
                    .child("Create and reuse SSH keys and password identities."),
            )
            .child(div().px(px(16.0)).child(self.render_action_buttons(cx)))
            .child(
                div()
                    .px(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(self.render_section_header("Keys", keys.len(), cx))
                    .child(self.render_list_grid(&keys, Icon::KeyShield, theme.accent, cx)),
            )
            .child(
                div()
                    .px(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(self.render_section_header("Identities", identities.len(), cx))
                    .child(self.render_list_grid(&identities, Icon::Host, theme.success, cx)),
            )
    }
}

pub struct KeychainEditor {
    focus_handle: FocusHandle,
    label_input: Entity<InputState>,
    username_input: Entity<InputState>,
    password_input: Entity<InputState>,
    private_key_input: Entity<MultilineTextArea>,
    draft_kind: KeychainItemKind,
    editing_item_id: Option<String>,
    dropped_key_path: Option<PathBuf>,
    original_private_key_text: String,
    stored_passphrase: Option<String>,
    editing_key_flavor: KeychainKeyFlavor,
    editing_certificate_path: Option<PathBuf>,
    error_message: Option<String>,
    success_message: Option<String>,
    on_cancel: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
    on_saved: Option<Box<dyn Fn(&mut App) + Send + Sync>>,
}

impl KeychainEditor {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            label_input: cx.new(|cx| InputState::new(window, cx).placeholder("Add a label...")),
            username_input: cx.new(|cx| InputState::new(window, cx).placeholder("root")),
            password_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Password")
                    .masked(true)
            }),
            private_key_input: cx.new(|cx| {
                MultilineTextArea::new(cx)
                    .placeholder("Private key")
                    .height(112.0)
            }),
            draft_kind: KeychainItemKind::Key,
            editing_item_id: None,
            dropped_key_path: None,
            original_private_key_text: String::new(),
            stored_passphrase: None,
            editing_key_flavor: KeychainKeyFlavor::Key,
            editing_certificate_path: None,
            error_message: None,
            success_message: None,
            on_cancel: None,
            on_saved: None,
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

    pub fn start_new_key(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        self.draft_kind = KeychainItemKind::Key;
        self.editing_key_flavor = KeychainKeyFlavor::Key;
        self.editing_certificate_path = None;
        cx.notify();
    }

    pub fn start_new_identity(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        self.draft_kind = KeychainItemKind::Identity;
        cx.notify();
    }

    pub fn load_item(&mut self, item: &KeychainItem, cx: &mut Context<Self>) {
        self.reset(cx);
        self.editing_item_id = Some(item.id.clone());
        self.draft_kind = item.kind.clone();
        self.editing_key_flavor = item.key_flavor;
        self.editing_certificate_path = item.certificate_path.clone();
        self.label_input
            .update(cx, |input, cx| input.set_text(&item.label, cx));

        match item.kind {
            KeychainItemKind::Identity => {
                self.username_input.update(cx, |input, cx| {
                    input.set_text(item.username.clone().unwrap_or_default(), cx);
                });
                let store = KeychainStore::load_or_default();
                let password = match store.get_password_secret(&item.id) {
                    Ok(Some(password)) => password,
                    Ok(None) => {
                        self.error_message = Some(
                            "This identity has no password stored in system keychain.".to_string(),
                        );
                        String::new()
                    }
                    Err(err) => {
                        self.error_message =
                            Some(format!("Failed to read system keychain: {}", err));
                        String::new()
                    }
                };
                self.password_input
                    .update(cx, |input, cx| input.set_text(&password, cx));
            }
            KeychainItemKind::Key => {
                if let Some(path) = item.key_path.clone() {
                    self.load_key_bundle(path, item.certificate_path.clone(), cx);
                }
                let store = KeychainStore::load_or_default();
                self.stored_passphrase = store.get_key_passphrase_secret(&item.id).ok().flatten();
            }
        }

        cx.notify();
    }

    pub fn import_dropped_key_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.start_new_key(cx);
        match normalize_imported_key_path(path) {
            Ok((private_key_path, certificate_path)) => {
                self.load_key_bundle(private_key_path, certificate_path, cx);
                cx.notify();
            }
            Err(err) => {
                self.error_message = Some(err.to_string());
                cx.notify();
            }
        }
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.editing_item_id = None;
        self.dropped_key_path = None;
        self.original_private_key_text.clear();
        self.stored_passphrase = None;
        self.editing_key_flavor = KeychainKeyFlavor::Key;
        self.editing_certificate_path = None;
        self.error_message = None;
        self.success_message = None;
        self.label_input.update(cx, |input, cx| input.clear(cx));
        self.username_input.update(cx, |input, cx| input.clear(cx));
        self.password_input.update(cx, |input, cx| input.clear(cx));
        self.private_key_input
            .update(cx, |input, cx| input.clear(cx));
    }

    fn load_key_bundle(
        &mut self,
        private_key_path: PathBuf,
        preferred_certificate_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.dropped_key_path = Some(private_key_path.clone());

        match fs::read_to_string(&private_key_path) {
            Ok(text) => {
                self.original_private_key_text = text.clone();
                self.private_key_input.update(cx, |input, cx| {
                    input.set_text(text, cx);
                });
            }
            Err(err) => {
                self.original_private_key_text.clear();
                self.private_key_input
                    .update(cx, |input, cx| input.clear(cx));
                self.error_message = Some(format!("Failed to read private key: {}", err));
            }
        }

        let certificate_path = preferred_certificate_path
            .or_else(|| sibling_certificate_path(&private_key_path).filter(|path| path.exists()));
        self.editing_certificate_path = certificate_path.clone();

        if self.label_input.read(cx).value().trim().is_empty() {
            if let Some(file_name) = private_key_path.file_name() {
                self.label_input.update(cx, |input, cx| {
                    input.set_text(file_name.to_string_lossy().to_string(), cx);
                });
            }
        }
    }

    fn import_key_from_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            let ssh_dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".ssh");
            let prompt = choose_file_with_osascript(&ssh_dir);
            match prompt {
                Ok(Some(path)) => {
                    self.start_new_key(cx);
                    match normalize_imported_key_path(path) {
                        Ok((private_key_path, certificate_path)) => {
                            self.load_key_bundle(private_key_path, certificate_path, cx);
                            cx.notify();
                        }
                        Err(err) => {
                            self.error_message = Some(err.to_string());
                            cx.notify();
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    self.error_message = Some(err.to_string());
                    cx.notify();
                }
            }
            let _ = window;
            return;
        }

        #[cfg(not(target_os = "macos"))]
        {
            let picker = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Import key file".into()),
            });
            cx.spawn_in(window, async move |this, cx| {
                let Ok(result) = picker.await else {
                    return Ok(());
                };

                match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.first() {
                            let _ = this.update(cx, |this, cx| {
                                this.start_new_key(cx);
                                match normalize_imported_key_path(path.clone()) {
                                    Ok((private_key_path, certificate_path)) => {
                                        this.load_key_bundle(
                                            private_key_path,
                                            certificate_path,
                                            cx,
                                        );
                                    }
                                    Err(err) => {
                                        this.error_message = Some(err.to_string());
                                        cx.notify();
                                    }
                                }
                            });
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = this.update(cx, |this, cx| {
                            this.error_message = Some(err.to_string());
                            cx.notify();
                        });
                    }
                }
                anyhow::Ok(())
            })
            .detach();
        }
    }

    fn save_item(&mut self, cx: &mut Context<Self>) {
        let label = self.label_input.read(cx).value().to_string();
        let username = self.username_input.read(cx).value().to_string();
        let password = self.password_input.read(cx).value().to_string();
        let private_key_text = self.private_key_input.read(cx).text().to_string();

        self.error_message = None;
        self.success_message = None;

        if label.trim().is_empty() {
            self.error_message = Some("Label is required".to_string());
            cx.notify();
            return;
        }

        let mut store = KeychainStore::load_or_default();
        let result = match self.draft_kind {
            KeychainItemKind::Identity => {
                if password.trim().is_empty() {
                    Err(anyhow!("Password is required"))
                } else {
                    store.upsert_identity(
                        self.editing_item_id.as_deref(),
                        label.trim(),
                        (!username.trim().is_empty()).then_some(username.trim().to_string()),
                        password.trim(),
                    )
                }
            }
            KeychainItemKind::Key => {
                if private_key_text.trim().is_empty() {
                    Err(anyhow!("Private key is required"))
                } else {
                    match self.persist_private_key_text(label.trim(), &private_key_text) {
                        Ok(final_path) => store.upsert_key(
                            self.editing_item_id.as_deref(),
                            label.trim(),
                            final_path,
                            self.editing_certificate_path.clone(),
                            self.editing_key_flavor,
                            self.stored_passphrase.as_deref(),
                        ),
                        Err(err) => Err(err),
                    }
                }
            }
        };

        match result {
            Ok(_) => {
                self.success_message = Some("Keychain updated".to_string());
                if let Some(ref callback) = self.on_saved {
                    callback(cx);
                }
            }
            Err(err) => {
                self.error_message = Some(err.to_string());
                cx.notify();
            }
        }
    }

    fn handle_cancel(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        if let Some(ref callback) = self.on_cancel {
            callback(cx);
        }
    }

    fn persist_private_key_text(&self, label: &str, text: &str) -> anyhow::Result<PathBuf> {
        if let Some(existing_path) = self.dropped_key_path.clone()
            && self.original_private_key_text == text
        {
            return Ok(existing_path);
        }

        let file_path = managed_key_file_path(label, self.editing_item_id.as_deref())?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, text)?;
        Ok(file_path)
    }

    fn render_labeled_input(
        &self,
        label: &str,
        input: &Entity<InputState>,
        cx: &App,
    ) -> impl IntoElement {
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
                    .child(label.to_string()),
            )
            .child(EditorInput::new(input))
    }

    fn render_labeled_multiline_input(
        &self,
        label: &str,
        input: &Entity<MultilineTextArea>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme().ui;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.success)
                    .child(label.to_string()),
            )
            .child(input.clone())
    }

    fn render_message(&self, cx: &App) -> Option<impl IntoElement> {
        let theme = cx.theme().ui;
        if let Some(error) = &self.error_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.error_background)
                    .border_1()
                    .border_color(theme.error)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.error_foreground)
                            .child(error.clone()),
                    )
                    .into_any_element(),
            )
        } else if let Some(success) = &self.success_message {
            Some(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.success_background)
                    .border_1()
                    .border_color(theme.success)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.success_foreground)
                            .child(success.clone()),
                    )
                    .into_any_element(),
            )
        } else {
            None
        }
    }
}

impl Focusable for KeychainEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for KeychainEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let title = if self.editing_item_id.is_some() {
            if self.draft_kind == KeychainItemKind::Key {
                "Edit Key"
            } else {
                "Edit Identity"
            }
        } else if self.draft_kind == KeychainItemKind::Key {
            "New Key"
        } else {
            "New Identity"
        };

        let subtitle = if self.draft_kind == KeychainItemKind::Key {
            "File-backed SSH credential"
        } else {
            "Password identity stored in system keychain"
        };

        div()
            .id("keychain-editor")
            .track_focus(&self.focus_handle)
            .w_full()
            .h_full()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .pb(px(12.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(div().text_xs().text_color(theme.text_muted).child(subtitle)),
            )
            .children(self.render_message(cx).into_iter())
            .child(self.render_labeled_input("Label", &self.label_input, cx))
            .when(self.draft_kind == KeychainItemKind::Identity, |this| {
                this.child(self.render_labeled_input("Username", &self.username_input, cx))
                    .child(self.render_labeled_input("Password", &self.password_input, cx))
            })
            .when(self.draft_kind == KeychainItemKind::Key, |this| {
                this.child(div().pt(px(4.0)).child(self.render_labeled_multiline_input(
                    "Private key *",
                    &self.private_key_input,
                    cx,
                )))
                .child(
                    div().pt(px(8.0)).child(
                        // Import from key file button using Button component
                        Button::new("import-key-btn")
                            .label("Import from key file")
                            .success()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.import_key_from_file(window, cx);
                            })),
                    ),
                )
            })
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        // Cancel button using Button component
                        div()
                            .flex_1()
                            .child(Button::new("cancel-btn").label("Cancel").on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.handle_cancel(cx);
                                }),
                            )),
                    )
                    .child(
                        // Save button using Button component
                        div().flex_1().child(
                            Button::new("save-btn")
                                .label("Save")
                                .success()
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.save_item(cx);
                                })),
                        ),
                    ),
            )
    }
}

fn normalize_imported_key_path(path: PathBuf) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    normalize_private_key_path(&path)
}

fn managed_key_file_path(label: &str, existing_id: Option<&str>) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Failed to locate home directory"))?;
    let dir = home.join(".config").join("deepterm").join("keys");
    let stem = existing_id
        .map(sanitize_filename)
        .unwrap_or_else(|| sanitize_filename(label));
    Ok(dir.join(format!("{}.pem", stem)))
}

fn sanitize_filename(value: &str) -> String {
    let mut sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized = "key".to_string();
    }
    sanitized
}

#[cfg(target_os = "macos")]
fn choose_file_with_osascript(initial_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let dir = initial_dir.to_string_lossy().replace('"', "\\\"");
    let script = format!(
        "POSIX path of (choose file with prompt \"Import key file\" default location (POSIX file \"{}\"))",
        dir
    );
    let output = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|err| anyhow!("Failed to open file picker: {}", err))?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(path)))
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") || stderr.contains("-128") {
            Ok(None)
        } else {
            Err(anyhow!("Failed to select key file: {}", stderr.trim()))
        }
    }
}

use std::collections::HashSet;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render, ScrollHandle,
    SharedString, Subscription, Window, actions, div, px,
};

use crate::assets::Icon;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{ContextMenuExt, PopupMenu};
use crate::components::modal::{Modal, ModalState};
use crate::sidebar_view::keychain_editor::KeychainEditor;
use crate::sidebar_view::style::{button_bg, button_bg_hover, hover_bg};
use crate::ssh::keychain::{KeychainItem, KeychainItemKind, KeychainStore};
use crate::theme::ActiveTheme;

actions!(keychain_actions, [EditKeychainItem, DeleteKeychainItem]);

/// Keychain 侧边栏视图组件，管理 Keys/Identities 列表和编辑弹窗。
pub struct KeychainSidebarView {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    selected_ids: HashSet<SharedString>,
    editor: Option<Entity<KeychainEditor>>,
    modal_state: ModalState,
    on_select: Option<Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>>,
    context_menu_item_id: Option<SharedString>,
    search_input: Entity<InputState>,
    _search_subscription: Subscription,
    search_query: SharedString,
}

impl KeychainSidebarView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let scroll_handle = ScrollHandle::new();

            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索密钥或身份")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut KeychainSidebarView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.search_query = text.to_lowercase().into();
                        cx.notify();
                    }
                },
            );

            Self {
                focus_handle,
                scroll_handle,
                selected_ids: HashSet::new(),
                editor: None,
                modal_state: ModalState::new(),
                on_select: None,
                context_menu_item_id: None,
                search_input,
                _search_subscription: subscription,
                search_query: SharedString::new_static(""),
            }
        })
    }

    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(&str, &mut Window, &mut App) + 'static,
    {
        self.on_select = Some(Rc::new(callback));
    }

    fn open_new_key_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self
            .editor
            .get_or_insert_with(|| KeychainEditor::new(window, cx))
            .clone();
        let entity = cx.entity().clone();
        editor.update(cx, |ed, cx| {
            ed.start_new_key(cx);
            let entity_clone = entity.clone();
            ed.on_cancel(move |cx| {
                entity_clone.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
            let entity_clone2 = entity.clone();
            ed.on_saved(move |cx| {
                entity_clone2.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
        });
        self.modal_state.open();
        cx.notify();
    }

    fn open_edit_modal(
        &mut self,
        item: &KeychainItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self
            .editor
            .get_or_insert_with(|| KeychainEditor::new(window, cx))
            .clone();
        let entity = cx.entity().clone();
        editor.update(cx, |ed, cx| {
            ed.load_item(item, cx);
            let entity_clone = entity.clone();
            ed.on_cancel(move |cx| {
                entity_clone.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
            let entity_clone2 = entity.clone();
            ed.on_saved(move |cx| {
                entity_clone2.update(cx, |this, cx| {
                    this.modal_state.close(cx, |this| &mut this.modal_state);
                });
            });
        });
        self.modal_state.open();
        cx.notify();
    }

    pub fn build_modal(&mut self, cx: &mut Context<Self>) -> Option<Modal> {
        if !self.modal_state.is_rendered() {
            return None;
        }
        let editor = self.editor.as_ref()?.clone();
        let entity = cx.entity();
        Some(
            Modal::new("keychain-modal")
                .width(px(540.))
                .max_height(px(700.))
                .top_padding(px(120.))
                .closing(self.modal_state.is_closing())
                .epoch(self.modal_state.epoch())
                .on_close(move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.modal_state.close(cx, |this| &mut this.modal_state);
                    });
                })
                .child(editor),
        )
    }
}

impl Focusable for KeychainSidebarView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for KeychainSidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();
        let items = KeychainStore::load_or_default().items;

        let on_select = self.on_select.clone();

        // 搜索过滤（复用 hosts_view.rs 的过滤逻辑）
        let query = self.search_query.as_ref();
        let filtered_items: Vec<&KeychainItem> = if query.is_empty() {
            items.iter().collect()
        } else {
            items
                .iter()
                .filter(|item| {
                    item.label.to_lowercase().contains(query)
                        || item.subtitle().to_lowercase().contains(query)
                })
                .collect()
        };

        let keys: Vec<&KeychainItem> = filtered_items
            .iter()
            .filter(|i| i.kind == KeychainItemKind::Key)
            .copied()
            .collect();
        let identities: Vec<&KeychainItem> = filtered_items
            .iter()
            .filter(|i| i.kind == KeychainItemKind::Identity)
            .copied()
            .collect();

        // 顶部导航栏
        let header = div()
            .w_full()
            .h(px(36.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(4.0))
            .mb(px(8.0))
            .child(
                div()
                    .w(px(28.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(999.0))
                    .bg(button_bg(is_dark))
                    .cursor_pointer()
                    .hover(|style| style.bg(button_bg_hover(is_dark)))
                    .child(
                        gpui::svg()
                            .size(px(8.0))
                            .path(Icon::ChevronLeft.path())
                            .text_color(theme.text),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        let on_select = on_select.clone();
                        move |_, window, cx| {
                            if let Some(ref f) = on_select {
                                f("__back", window, cx);
                            }
                        }
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("Keychain"),
            )
            .child(
                div()
                    .w(px(28.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(999.0))
                    .bg(button_bg(is_dark))
                    .cursor_pointer()
                    .hover(|style| style.bg(button_bg_hover(is_dark)))
                    .child(
                        gpui::svg()
                            .size(px(12.0))
                            .path(Icon::Plus.path())
                            .text_color(theme.text),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        cx.listener(|this, _, window, cx| {
                            this.open_new_key_modal(window, cx);
                        })
                    }),
            );

        // 搜索框
        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search_box = div()
            .w_full()
            .h(px(32.0))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(12.0))
            .bg(hover_bg(is_dark))
            .when(search_focused, |this| {
                this.border(px(3.0)).border_color(theme.accent.opacity(0.5))
            })
            .mb(px(12.0))
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .path(Icon::MagnifyingGlass.path())
                    .text_color(theme.text_muted),
            )
            .child(EditorInput::new(&self.search_input).transparent());

        let mut list = div().w_full().flex().flex_col();

        // Keys 区域
        if !keys.is_empty() || query.is_empty() {
            list = list.child(
                div()
                    .h(px(28.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .pt_3()
                    .pb_3()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_subtle)
                            .child(format!("Keys ({})", keys.len())),
                    ),
            );

            for (index, item) in keys.iter().enumerate() {
                list = list.child(render_keychain_item_row(
                    item,
                    index,
                    Icon::KeyShield,
                    theme.accent,
                    is_dark,
                    theme,
                    &self.selected_ids,
                    on_select.clone(),
                    cx,
                ));
            }
        }

        // Identities 区域
        if !identities.is_empty() || query.is_empty() {
            list = list.child(
                div()
                    .h(px(28.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .pt_3()
                    .pb_3()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_subtle)
                            .child(format!("Identities ({})", identities.len())),
                    ),
            );

            for (index, item) in identities.iter().enumerate() {
                list = list.child(render_keychain_item_row(
                    item,
                    index + keys.len(),
                    Icon::PersonBadgeKey,
                    theme.success,
                    is_dark,
                    theme,
                    &self.selected_ids,
                    on_select.clone(),
                    cx,
                ));
            }
        }

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            // Edit action
            .on_action(cx.listener(|this, _: &EditKeychainItem, window, cx| {
                let item_id = this
                    .context_menu_item_id
                    .clone()
                    .or_else(|| this.selected_ids.iter().next().cloned());
                if let Some(id) = item_id {
                    let store = KeychainStore::load_or_default();
                    if let Some(item) = store.items.iter().find(|i| i.id == id.as_ref()) {
                        this.open_edit_modal(item, window, cx);
                    }
                }
            }))
            // Delete action
            .on_action(cx.listener(|this, _: &DeleteKeychainItem, window, cx| {
                let item_ids: Vec<_> = this.selected_ids.iter().cloned().collect();
                if item_ids.is_empty() {
                    return;
                }
                let count = item_ids.len();
                let store = KeychainStore::load_or_default();
                let names: Vec<_> = item_ids
                    .iter()
                    .filter_map(|id| {
                        store
                            .items
                            .iter()
                            .find(|i| i.id == id.as_ref())
                            .map(|i| i.label.clone())
                    })
                    .collect();

                let message = if count == 1 {
                    if let Some(name) = names.first() {
                        format!("确定要删除 '{}' 吗？此操作不可撤销。", name)
                    } else {
                        "确定要删除该项吗？此操作不可撤销。".to_string()
                    }
                } else {
                    format!("确定要删除选中的 {} 项吗？此操作不可撤销。", count)
                };

                let weak = cx.weak_entity();
                let ids_to_delete = item_ids.clone();
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    "确认删除",
                    Some(&message),
                    &["删除", "取消"],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    if let Ok(0) = answer.await {
                        if let Some(entity) = weak.upgrade() {
                            let _ = entity.update(cx, |this, cx| {
                                let mut store = KeychainStore::load_or_default();
                                for id in &ids_to_delete {
                                    let _ = store.delete_item(id);
                                }
                                this.selected_ids.clear();
                                this.context_menu_item_id = None;
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }))
            .child(header)
            .child(search_box)
            .child(
                div()
                    .id("keychain-scroll")
                    .track_scroll(&self.scroll_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(list),
            )
    }
}

fn render_keychain_item_row(
    item: &KeychainItem,
    index: usize,
    icon: Icon,
    icon_color: gpui::Hsla,
    is_dark: bool,
    theme: crate::theme::UiTheme,
    selected_ids: &HashSet<SharedString>,
    on_select: Option<Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>>,
    cx: &mut Context<KeychainSidebarView>,
) -> impl IntoElement {
    let item_id = item.id.clone();
    let is_selected = selected_ids.iter().any(|s| s.as_ref() == item_id);

    div()
        .id(("keychain-item", index))
        .w_full()
        .h(px(52.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(8.0))
        .cursor_pointer()
        .when(is_selected, |this| {
            this.bg(theme.accent).text_color(theme.accent_foreground)
        })
        .when(!is_selected, |this| {
            this.hover(|style| style.bg(hover_bg(is_dark)))
                .text_color(theme.text)
        })
        .child(
            div()
                .w(px(32.0))
                .h(px(32.0))
                .rounded(px(8.0))
                .bg(if is_selected {
                    theme.accent_foreground
                } else {
                    icon_color
                })
                .flex()
                .items_center()
                .justify_center()
                .child(
                    gpui::svg()
                        .size(px(16.0))
                        .path(icon.path())
                        .text_color(if is_selected {
                            theme.accent
                        } else {
                            theme.accent_foreground
                        }),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if is_selected {
                                    theme.accent_foreground
                                } else {
                                    theme.text
                                })
                                .child(item.label.clone()),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_size(px(10.0))
                        .line_height(px(14.0))
                        .text_color(if is_selected {
                            theme.accent_foreground.opacity(0.7)
                        } else {
                            theme.text_muted
                        })
                        .child(item.subtitle()),
                ),
        )
        .on_click(cx.listener({
            let id = item_id.clone();
            move |this, event: &gpui::ClickEvent, window, cx| {
                let is_cmd_pressed = event.modifiers().secondary();
                if event.click_count() == 1 {
                    if is_cmd_pressed {
                        // Cmd+click: toggle selection
                        let item_shared: SharedString = id.clone().into();
                        if this.selected_ids.contains(&item_shared) {
                            this.selected_ids.remove(&item_shared);
                        } else {
                            this.selected_ids.insert(item_shared);
                        }
                    } else {
                        // Normal click: clear all and select only this one
                        this.selected_ids.clear();
                        this.selected_ids.insert(id.clone().into());
                    }
                    if let Some(ref f) = on_select {
                        f(&id, window, cx);
                    }
                    cx.notify();
                }
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener({
                let item_id = item_id.clone();
                move |this, _event, _window, cx| {
                    let item_shared: SharedString = item_id.clone().into();
                    this.context_menu_item_id = Some(item_shared.clone());
                    if !this.selected_ids.contains(&item_shared) {
                        this.selected_ids.clear();
                        this.selected_ids.insert(item_shared);
                    }
                    cx.notify();
                }
            }),
        )
        .context_menu(format!("keychain-item-menu-{}", item_id), {
            let entity = cx.entity().clone();
            use crate::components::button::Icon as MenuIcon;
            move |menu: PopupMenu, _window, _cx| {
                let count = entity.read(_cx).selected_ids.len();
                let mut m = menu;
                if count == 1 {
                    m = m
                        .menu_with_icon(
                            "Edit",
                            MenuIcon::new("icons/edit.svg"),
                            Box::new(EditKeychainItem),
                        )
                        .separator();
                }
                let label = if count == 1 {
                    "Delete".to_string()
                } else {
                    format!("Delete {} items", count)
                };
                m.menu_with_icon(
                    &label,
                    MenuIcon::new("icons/trash.svg"),
                    Box::new(DeleteKeychainItem),
                )
            }
        })
}

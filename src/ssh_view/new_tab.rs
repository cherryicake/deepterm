use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, px,
};
use crate::components::sidebar::Sidebar;
use crate::ssh::config::SshConnectionConfig;

/// Right sidebar width in pixels
const RIGHT_SIDEBAR_WIDTH: f32 = 360.0;
use crate::ssh_view::host_form::HostForm;
use crate::ssh_view::hosts_view::HostsView;
use crate::ssh_view::keychain_view::KeychainEditor;
use crate::theme::ActiveTheme;
use crate::workspace::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RightSidebarContent {
    HostForm,
    KeychainEditor,
}

/// SSH New Tab 视图 - 合并 Hosts 和 Keychain 页面
pub struct SshNewTabView {
    hosts_view: Entity<HostsView>,
    host_form: Entity<HostForm>,
    keychain_editor: Entity<KeychainEditor>,
    right_sidebar_visible: bool,
    right_sidebar_rendered: bool,
    right_sidebar_content: RightSidebarContent,
    focus_handle: FocusHandle,
}

impl SshNewTabView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::new_with_workspace(window, cx, None)
    }

    pub fn new_with_workspace(
        window: &mut Window,
        cx: &mut App,
        workspace: Option<Entity<Workspace>>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let hosts_view = HostsView::new_with_workspace(window, cx, workspace.clone());
            let host_form = HostForm::new(window, cx);
            let keychain_editor = KeychainEditor::new(window, cx);

            let mut view = Self {
                hosts_view,
                host_form,
                keychain_editor,
                right_sidebar_visible: false,
                right_sidebar_rendered: false,
                right_sidebar_content: RightSidebarContent::HostForm,
                focus_handle,
            };

            view.setup_callbacks(cx);
            view
        })
    }

    fn setup_callbacks(&mut self, cx: &mut Context<Self>) {
        // HostsView NEW HOST 按钮回调
        let view_entity = cx.entity();
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_new_host_click(move |app| {
                view_entity.update(app, |view, cx| {
                    view.host_form.update(cx, |form, cx| {
                        form.reset_form(cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::HostForm, cx);
                });
            });
        });

        // HostsView 编辑主机回调
        let view_entity_for_edit = cx.entity();
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_edit_host_click(move |app, config| {
                view_entity_for_edit.update(app, |view, cx| {
                    view.edit_host(&config, cx);
                });
            });
        });

        // HostsView NEW KEY 回调
        let view_entity_for_new_key = cx.entity();
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_new_key_click(move |app, path| {
                view_entity_for_new_key.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        if let Some(path) = path.clone() {
                            editor.import_dropped_key_path(path, cx);
                        } else {
                            editor.start_new_key(cx);
                        }
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        // HostsView NEW IDENTITY 回调
        let view_entity_for_new_identity = cx.entity();
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_new_identity_click(move |app| {
                view_entity_for_new_identity.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        editor.start_new_identity(cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        // HostsView 编辑 keychain item 回调
        let view_entity_for_edit_keychain = cx.entity();
        self.hosts_view.update(cx, |hosts_view, _cx| {
            hosts_view.on_edit_keychain_item_click(move |app, item| {
                view_entity_for_edit_keychain.update(app, |view, cx| {
                    view.keychain_editor.update(cx, |editor, cx| {
                        editor.load_item(&item, cx);
                    });
                    view.open_right_sidebar_with(RightSidebarContent::KeychainEditor, cx);
                });
            });
        });

        // HostForm 回调
        let view_entity_for_cancel = cx.entity();
        let view_entity_for_save = cx.entity();
        let hosts_view_for_refresh = self.hosts_view.clone();

        self.host_form.update(cx, |form, _cx| {
            form.on_cancel(move |app| {
                view_entity_for_cancel.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });

            form.on_saved(move |app| {
                hosts_view_for_refresh.update(app, |hosts_view, cx| {
                    hosts_view.refresh_hosts(cx);
                });
                view_entity_for_save.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });
        });

        // KeychainEditor 回调
        let view_entity_for_keychain_cancel = cx.entity();
        let view_entity_for_keychain_save = cx.entity();
        let hosts_view_for_keychain_refresh = self.hosts_view.clone();

        self.keychain_editor.update(cx, |editor, _cx| {
            editor.on_cancel(move |app| {
                view_entity_for_keychain_cancel.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });

            editor.on_saved(move |app| {
                hosts_view_for_keychain_refresh.update(app, |hosts_view, cx| {
                    hosts_view.refresh_items(cx);
                });
                view_entity_for_keychain_save.update(app, |view, cx| {
                    view.close_right_sidebar(cx);
                });
            });
        });
    }

    /// Open sidebar for editing a host
    pub fn edit_host(&mut self, config: &SshConnectionConfig, cx: &mut Context<Self>) {
        self.host_form.update(cx, |form, cx| {
            form.load_config(config, cx);
        });
        self.open_right_sidebar_with(RightSidebarContent::HostForm, cx);
    }

    fn open_right_sidebar(&mut self, cx: &mut Context<Self>) {
        self.right_sidebar_visible = true;
        self.right_sidebar_rendered = true;
        // Update hosts view sidebar width
        // Use defer to avoid nested entity update when called from HostsView action handler
        let hosts_view = self.hosts_view.clone();
        cx.defer(move |cx| {
            hosts_view.update(cx, |hosts_view, _cx| {
                hosts_view.set_sidebar_widths(0.0, RIGHT_SIDEBAR_WIDTH);
            });
        });
        cx.notify();
    }

    fn open_right_sidebar_with(&mut self,
        content: RightSidebarContent,
        cx: &mut Context<Self>,
    ) {
        self.right_sidebar_content = content;
        self.open_right_sidebar(cx);
    }

    fn close_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.right_sidebar_visible {
            self.right_sidebar_visible = false;
            // Reset hosts view sidebar width
            // Use defer to avoid nested entity update when called from HostsView action handler
            let hosts_view = self.hosts_view.clone();
            cx.defer(move |cx| {
                hosts_view.update(cx, |hosts_view, _cx| {
                    hosts_view.set_sidebar_widths(0.0, 0.0);
                });
            });
            cx.notify();

            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(
                        crate::components::sidebar::ANIMATION_DURATION_EXIT_MS,
                    ))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.right_sidebar_rendered = false;
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

impl Focusable for SshNewTabView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SshNewTabView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let show_right_sidebar = self.right_sidebar_visible;
        let render_sidebar = self.right_sidebar_rendered;
        let sidebar_content = match self.right_sidebar_content {
            RightSidebarContent::HostForm => self.host_form.clone().into_any_element(),
            RightSidebarContent::KeychainEditor => {
                self.keychain_editor.clone().into_any_element()
            }
        };

        div()
            .id("ssh-new-tab-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.hosts_view.clone()),
            )
            .when(render_sidebar, |this| {
                this.child(
                    Sidebar::new()
                        .with_width(RIGHT_SIDEBAR_WIDTH)
                        .render_with_toggle_animation(
                            "right-sidebar-animation",
                            show_right_sidebar,
                            sidebar_content,
                            cx,
                        ),
                )
            })
    }
}

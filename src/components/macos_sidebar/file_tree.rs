//! 文件树：与终端会话 / Git 面板的接线（订阅、状态绑定）与页面渲染。

use super::*;

impl MacosSidebar {
    fn subscribe_file_tree_to_terminal(
        &mut self,
        terminal_view: &Entity<TerminalView>,
        file_tree: Entity<SidebarFileTree>,
        cx: &mut Context<Self>,
    ) {
        let sub = cx.subscribe(
            terminal_view,
            move |this, subscribed_tv, event: &crate::terminal_view::terminal_view::Event, cx| {
                let should_sync = matches!(
                    event,
                    crate::terminal_view::terminal_view::Event::WorkingDirectoryChanged
                        | crate::terminal_view::terminal_view::Event::TitleChanged
                );
                if !should_sync {
                    return;
                }
                if this.file_tree_terminal_view.as_ref() != Some(&subscribed_tv) {
                    return;
                }
                let state = subscribed_tv.read(cx).session_state(cx);
                let (cwd, target_label) = state.file_tree_target();
                if cwd.as_deref() == Some(file_tree.read(cx).root_path()) {
                    return;
                }
                this.file_tree_target_label = Some(target_label);
                let _ = file_tree.update(cx, |tree, cx| {
                    tree.open_path(cwd, cx);
                });
                cx.notify();
                this.notify_app_window(cx);
            },
        );
        self.file_tree_subscription = Some(sub);
    }

    fn subscribe_file_tree_events(
        &mut self,
        file_tree: Entity<SidebarFileTree>,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_event_subscription.take();
        let sub = cx.subscribe(
            &file_tree,
            move |this, _emitter, event: &FileTreeEvent, cx| match event {
                FileTreeEvent::OpenFile { path, ssh_config } => {
                    cx.emit(SidebarEvent::OpenFile {
                        path: path.clone(),
                        ssh_config: ssh_config.clone(),
                    });
                }
                FileTreeEvent::RootChanged => {
                    this.ensure_git_panel(cx);
                }
            },
        );
        self.file_tree_event_subscription = Some(sub);
    }

    pub(crate) fn set_file_tree_view(
        &mut self,
        view: Entity<SidebarFileTree>,
        target_label: String,
        terminal_view: Option<Entity<TerminalView>>,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_subscription.take();
        if let Some(old_tree) = self.file_tree_view.take() {
            let _ = old_tree.update(cx, |tree, cx| tree.cleanup(cx));
        }
        self.file_tree_terminal_view = terminal_view.clone();

        if let Some(tv) = &terminal_view {
            self.subscribe_file_tree_to_terminal(tv, view.clone(), cx);
        }

        self.subscribe_file_tree_events(view.clone(), cx);

        self.file_tree_view = Some(view);
        self.file_tree_target_label = Some(target_label);
        self.current_view = SidebarView::FileTree;
        self.ensure_git_panel(cx);
        cx.notify();
        let app_window = self.app_window.clone();
        cx.defer(move |cx| {
            if let Some(aw) = &app_window {
                aw.update(cx, |_, cx| cx.notify()).ok();
            }
        });
    }

    /// 确保 Git 面板已创建，并让它指向当前文件树的 workspace 路径。
    ///
    /// 首次调用会创建面板并建立文件监视订阅；路径变化时重建订阅；
    /// 路径相同时保持现有订阅不动。
    fn ensure_git_panel(&mut self, cx: &mut Context<Self>) {
        if self.git_panel.is_none() {
            let git_panel = cx.new(|cx| crate::sidebar_view::git_panel::GitPanel::new(cx));
            self.git_panel = Some(git_panel);
        }

        let Some(ref git_panel) = self.git_panel else {
            return;
        };
        let Some(ref file_tree) = self.file_tree_view else {
            return;
        };

        let (workspace_path, is_local, ssh_config, filesystem) = {
            let tree = file_tree.read(cx);
            (
                tree.root_path().to_path_buf(),
                tree.is_local(),
                tree.ssh_config().cloned(),
                if tree.is_local() {
                    None
                } else {
                    Some(tree.filesystem().clone())
                },
            )
        };
        git_panel.update(cx, |panel, cx| {
            panel.set_workspace_path(workspace_path, is_local, ssh_config, filesystem, cx);
        });

        // 订阅 Git 面板事件（OpenDiff 等）转发给侧边栏
        self.git_panel_subscription.take();
        let sub = cx.subscribe(
            git_panel,
            |this, _, event: &crate::sidebar_view::git_panel::GitPanelEvent, cx| match event {
                crate::sidebar_view::git_panel::GitPanelEvent::OpenDiff { path, diff, repo } => {
                    cx.emit(SidebarEvent::OpenDiff {
                        path: path.clone(),
                        diff: diff.clone(),
                        repo: repo.clone(),
                    });
                }
                _ => {}
            },
        );
        self.git_panel_subscription = Some(sub);
    }

    /// 将文件树切换到指定终端。路径+后端相同时复用现有树（仅切换订阅），否则重建。
    pub fn set_terminal_file_tree(
        &mut self,
        terminal_view: Entity<TerminalView>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let (path, display_name, is_local, ssh_config) = {
            let state = terminal_view.read(cx).session_state(cx);
            let (p, d) = state.file_tree_target();
            (p, d, state.is_local(), state.ssh_config().cloned())
        };

        // 同一终端实例 → 跳过
        if self.file_tree_terminal_view.as_ref() == Some(&terminal_view) {
            return;
        }

        // 获取旧的文件树标识符
        let old_file_tree_key = self.get_current_file_tree_key(cx);

        // 路径+后端一致 → 复用（仅切换订阅）
        if let Some(ref path) = path {
            if let Some(tree) = &self.file_tree_view {
                if tree
                    .read(cx)
                    .matches_location(path, is_local, ssh_config.as_ref())
                {
                    self.file_tree_subscription.take();
                    self.file_tree_terminal_view = Some(terminal_view.clone());
                    self.subscribe_file_tree_to_terminal(&terminal_view, tree.clone(), cx);
                    self.file_tree_target_label = Some(display_name);
                    self.ensure_git_panel(cx);

                    // 文件树 key 发生变化时才触发编辑器状态保存/恢复
                    let new_key = crate::editor::FileTreeKey::new(
                        Some(path.clone()),
                        is_local,
                        ssh_config.clone(),
                    );
                    if old_file_tree_key != Some(new_key.clone()) {
                        let app_window = self.app_window.clone();
                        let window_handle = window.window_handle();
                        cx.defer(move |cx| {
                            if let Some(aw) = &app_window {
                                cx.update_window(window_handle, |_view, window, cx| {
                                    aw.update(cx, |this, cx| {
                                        this.on_file_tree_switched(
                                            old_file_tree_key,
                                            new_key,
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                                })
                                .ok();
                            }
                        });
                    }

                    cx.notify();
                    return;
                }
            }
        }

        // 不同 → 重建
        let (backend, requested_path): (Arc<dyn FileSystem>, Option<PathBuf>) = if !is_local {
            if let Some(config) = ssh_config.clone() {
                let p = path
                    .as_ref()
                    .map(|p| PathBuf::from(p.to_string_lossy().to_string()));
                (Arc::new(RemoteFileSystem::new(config)), p)
            } else {
                log::error!("非本地文件树但缺少 SSH 配置，退化为本地");
                (Arc::new(LocalFileSystem), path.clone())
            }
        } else {
            (Arc::new(LocalFileSystem), path.clone())
        };
        let file_tree = cx.new(|cx| {
            let mut tree = SidebarFileTree::new(cx, backend, requested_path);
            tree.open_path(path.clone(), cx);
            tree
        });

        // 共享缓存引用由 SidebarFileTree 构造时 acquire、Drop 时 release，无需手动管理。
        self.file_tree_subscription.take();
        if let Some(old) = self.file_tree_view.take() {
            // 旧树保存展开状态并释放 watcher/订阅；缓存引用随实体 Drop 归还，
            // 归零后进入保温期，切回同一服务器时可直接复用。
            old.update(cx, |t, cx| t.cleanup(cx));
        }
        self.file_tree_terminal_view = Some(terminal_view.clone());
        self.subscribe_file_tree_to_terminal(&terminal_view, file_tree.clone(), cx);
        self.subscribe_file_tree_events(file_tree.clone(), cx);
        self.file_tree_view = Some(file_tree);
        self.file_tree_target_label = Some(display_name);
        self.current_view = SidebarView::FileTree;

        self.ensure_git_panel(cx);

        // 触发编辑器状态保存/恢复
        let new_key = crate::editor::FileTreeKey::new(path, is_local, ssh_config);
        let app_window = self.app_window.clone();
        let window_handle = window.window_handle();
        cx.defer(move |cx| {
            if let Some(aw) = &app_window {
                cx.update_window(window_handle, |_view, window, cx| {
                    aw.update(cx, |this, cx| {
                        this.on_file_tree_switched(old_file_tree_key, new_key, window, cx);
                    })
                    .ok();
                })
                .ok();
            }
        });

        cx.notify();
    }

    /// 获取当前文件树的标识符
    fn get_current_file_tree_key(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<crate::editor::FileTreeKey> {
        let terminal_view = self.file_tree_terminal_view.as_ref()?;
        let state = terminal_view.read(cx).session_state(cx);
        let (path, _display_name) = state.file_tree_target();
        Some(crate::editor::FileTreeKey::new(
            path,
            state.is_local(),
            state.ssh_config().cloned(),
        ))
    }

    /// 轻量版：仅切换侧边栏到文件树页面，不设订阅、不创建实体。
    /// 从 listener 回调中直接调用（无需 Context<Self>），然后通过 AppWindow 触发重绘。
    pub fn show_file_tree_page(&mut self) {
        self.current_view = SidebarView::FileTree;
        self.file_tree_target_label = None;
    }

    pub(super) fn render_file_tree_content(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance.is_dark();

        let header = div()
            .w_full()
            .h(px(36.0))
            .flex()
            .items_center()
            .gap(px(8.0))
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
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _window, cx| {
                            this.handle_item_select("__back", cx);
                            this.notify_app_window(cx);
                        }),
                    ),
            )
            .child(self.render_file_tree_panel_tabs(is_dark, cx));

        let content_element: gpui::AnyElement = match self.file_tree_panel_tabs.current {
            FileTreePanel::Files => {
                let file_tree_content = self
                    .file_tree_view
                    .clone()
                    .map(|view| div().w_full().h_full().child(view))
                    .unwrap_or_else(|| div().w_full().h_full());
                file_tree_content.into_any_element()
            }
            FileTreePanel::Search => div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_sm().text_color(theme.text_subtle).child("搜索"))
                .into_any_element(),
            FileTreePanel::Git => {
                if let Some(ref git_panel) = self.git_panel {
                    div()
                        .w_full()
                        .h_full()
                        .child(git_panel.clone())
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(theme.text_subtle)
                        .child("Git")
                        .into_any_element()
                }
            }
        };

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(content_element),
            )
    }

    fn render_file_tree_panel_tabs(
        &self,
        is_dark: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().ui;
        let current = &self.file_tree_panel_tabs.current;
        let prev = &self.file_tree_panel_tabs.prev;
        let anim_epoch = self.file_tree_panel_tabs.anim_epoch;

        let mut container = div()
            .h(px(28.0))
            .flex()
            .items_center()
            .gap(px(file_tree_panel_tabs::TAB_GAP));

        for item in TAB_ITEMS {
            let is_active = *current == item.panel;
            let is_prev = *prev == item.panel;
            let panel_clone = item.panel.clone();
            let active_w = item.active_width;
            let label = item.label;

            let btn = file_tree_panel_tabs::build_tab_button(item, is_active, is_dark, &theme)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _window, cx| {
                        let old = this.file_tree_panel_tabs.current.clone();
                        if old != panel_clone {
                            this.file_tree_panel_tabs.select(panel_clone.clone());
                            // 切到 Git 标签时确保面板已创建并初始化（订阅文件监视）
                            if panel_clone == FileTreePanel::Git {
                                this.ensure_git_panel(cx);
                            }
                            cx.notify();
                            this.notify_app_window(cx);
                        }
                    }),
                )
                .with_animation(
                    (label, anim_epoch),
                    Animation::new(Duration::from_millis(200)).with_easing(ease_in_out),
                    move |this, delta| {
                        let factor_target: f32 = if is_active { 1.0 } else { 0.0 };
                        let factor_source: f32 = if is_prev { 1.0 } else { 0.0 };
                        let width_factor = factor_source + (factor_target - factor_source) * delta;
                        let w = 28.0 + (active_w - 28.0) * width_factor;
                        let text_w = 28.0 * width_factor;
                        let gap_w = 4.0 * width_factor;

                        let this = this.w(px(w)).gap(px(gap_w));

                        if width_factor > 0.0 {
                            this.child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if is_active {
                                        theme.accent_foreground
                                    } else {
                                        theme.text
                                    })
                                    .flex_none()
                                    .w(px(text_w))
                                    .h(px(28.0))
                                    .flex()
                                    .items_center()
                                    .opacity(width_factor)
                                    .overflow_hidden()
                                    .child(label),
                            )
                        } else {
                            this
                        }
                    },
                );

            container = container.child(btn);
        }

        container.into_any_element()
    }
}

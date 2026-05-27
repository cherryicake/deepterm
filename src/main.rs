#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod file_browser;
pub mod file_transfer;
mod sidebar_view;
pub mod ssh;
pub mod ssh_view;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    AnimationExt, App, Context, Entity, IntoElement, KeyBinding, ParentElement, Render, Window,
    actions, div, px,
};

use crate::components::macos_sidebar::MacosSidebar;
use crate::components::title_bar::TitleBar;
use crate::sidebar_view::font_view::FontSettingsView;
use crate::sidebar_view::host_view::HostView;
use crate::sidebar_view::keychain_view::KeychainSidebarView;
use crate::ssh_view::picker::{ShowSshPicker, SshPicker};
use crate::theme::ActiveTheme;
use crate::workspace::ToggleSidebar;
use crate::workspace::window_manager::WindowManager;
use crate::workspace::workspace::Workspace;

// Define application-level actions
actions!(app, [Quit, NewWindow]);

const CONTEXT: &str = "AppWindow";

/// Root component that contains title bar, sidebar and workspace
pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    workspace: Entity<Workspace>,
    ssh_picker: Entity<SshPicker>,
    sidebar: MacosSidebar,
    _sidebar_hide_task: Option<gpui::Task<()>>,
    host_view: Entity<HostView>,
    keychain_view: Entity<KeychainSidebarView>,
}

impl AppWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = Workspace::new(window, cx);

        let tab_bar = workspace.read(cx).tab_bar.clone();
        let workspace_weak = workspace.downgrade();
        let title_bar = cx.new(|cx| {
            let mut bar = TitleBar::new("title-bar").with_tab_bar(tab_bar);
            bar.bind_workspace(workspace_weak, cx);
            bar
        });

        let workspace_for_picker = workspace.clone();
        let ssh_picker =
            cx.new(|cx| SshPicker::new(window, cx).with_workspace(workspace_for_picker));

        workspace.update(cx, |ws, cx| {
            ws.set_ssh_picker(ssh_picker.clone(), cx);
        });

        let mut sidebar = MacosSidebar::new();
        let font_settings_view = FontSettingsView::new(window, cx);
        sidebar.set_font_view(font_settings_view);

        // 创建 HostView 并设置回调
        let host_view = HostView::new(window, cx);
        let entity = cx.entity();
        host_view.update(cx, |hv, _cx| {
            hv.on_select(move |id, _window, cx| {
                entity.update(cx, |this, cx| {
                    this.sidebar.handle_item_select(id);
                    cx.notify();
                });
            });
        });
        let workspace_for_host = workspace.clone();
        host_view.update(cx, |hv, _cx| {
            hv.on_connect(move |config, window, cx| {
                workspace_for_host.update(cx, |ws, cx| {
                    ws.create_ssh_terminal_with_config(config, window, cx);
                });
            });
        });
        sidebar.set_host_view(host_view.clone());

        // 创建 KeychainSidebarView 并设置回调
        let keychain_view = KeychainSidebarView::new(window, cx);
        let entity = cx.entity();
        keychain_view.update(cx, |kv, _cx| {
            kv.on_select(move |id, _window, cx| {
                entity.update(cx, |this, cx| {
                    this.sidebar.handle_item_select(id);
                    cx.notify();
                });
            });
        });
        sidebar.set_keychain_view(keychain_view.clone());

        Self {
            title_bar,
            workspace,
            ssh_picker,
            sidebar,
            _sidebar_hide_task: None,
            host_view,
            keychain_view,
        }
    }

    /// 设置初始焦点到工作区的第一个终端
    fn setup_initial_focus(&self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.set_initial_focus(window, cx);
        });
    }

    /// 切换侧边栏显示/隐藏
    fn toggle_sidebar(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) {
        let needs_hide_task = self.sidebar.toggle();
        if needs_hide_task {
            let entity = cx.entity();
            self._sidebar_hide_task = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250 + 40))
                    .await;
                entity.update(cx, |this, cx| {
                    this.sidebar.finish_animation();
                    cx.notify();
                });
            }));
        } else {
            self._sidebar_hide_task = None;
        }
        cx.notify();
    }

    /// 渲染侧边栏区域
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.sidebar.clone();
        let entity = cx.entity();
        div().child(sidebar.render_sidebar(
            cx,
            move |id, _window, cx| {
                entity.update(cx, |this, cx| {
                    this.sidebar.handle_item_select(id);
                    cx.notify();
                });
            },
            move |config, window, cx| {
                // HostView 内部已处理双击连接，此回调保留但不再用于 Host 视图
                let _ = (config, window, cx);
            },
        ))
    }

    /// 渲染侧边栏切换按钮
    fn render_sidebar_toggle(
        &self,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.sidebar.render_toggle_button(
            cx,
            window.is_fullscreen(),
            cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_sidebar(window, cx);
            }),
        )
    }

    /// 渲染右侧主内容区域（TitleBar + Workspace）。
    /// TitleBar 的 padding 补偿动画与侧边栏显隐同步。
    fn render_main_area(&self, window: &gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        const TITLE_BAR_EXTRA_PAD_MACOS: f32 = 116.0;
        const TITLE_BAR_EXTRA_PAD_MACOS_FS: f32 = 40.0;
        const TITLE_BAR_EXTRA_PAD_WIN: f32 = 44.0;
        const SIDEBAR_ENTER_MS: u64 = 300;
        const SIDEBAR_EXIT_MS: u64 = 250;

        let is_fullscreen = window.is_fullscreen();

        // macOS 非全屏: 关闭时需为交通灯+toggle 按钮留出空间(116px)；
        //   打开后按钮移入 sidebar，padding 恢复。
        // macOS 全屏: toggle 在 8px，用 40px 外层 padding 保持与
        //   非全屏一致的 4px 间距；打开后按钮移入 sidebar，padding 恢复。
        // Windows / Linux: 关闭时 toggle 按钮在左边缘(8px)，
        //   需为按钮留出 44px 空间；打开后按钮移入 sidebar，padding 恢复。
        let needs_traffic_pad = cfg!(target_os = "macos") && !is_fullscreen;
        let compact_pad = if cfg!(target_os = "macos") {
            TITLE_BAR_EXTRA_PAD_MACOS_FS
        } else {
            TITLE_BAR_EXTRA_PAD_WIN
        };
        let (start_pl, end_pl, dur_ms) = if self.sidebar.is_visible() {
            if needs_traffic_pad {
                (TITLE_BAR_EXTRA_PAD_MACOS, 0.0, SIDEBAR_ENTER_MS)
            } else {
                (compact_pad, 0.0, SIDEBAR_ENTER_MS)
            }
        } else {
            if needs_traffic_pad {
                (0.0, TITLE_BAR_EXTRA_PAD_MACOS, SIDEBAR_EXIT_MS)
            } else {
                (0.0, compact_pad, SIDEBAR_EXIT_MS)
            }
        };

        // 初始状态（epoch == 0）直接应用最终 padding，避免打开时出现一次不必要的展开动画
        let animated_title_bar: gpui::AnyElement = if self.sidebar.animation_epoch() == 0 {
            div()
                .child(self.title_bar.clone())
                .pl(px(end_pl))
                .into_any_element()
        } else {
            div()
                .child(self.title_bar.clone())
                .with_animation(
                    ("titlebar-pad-anim", self.sidebar.animation_epoch()),
                    gpui::Animation::new(std::time::Duration::from_millis(dur_ms))
                        .with_easing(gpui::ease_in_out),
                    move |this, delta| this.pl(px(start_pl + (end_pl - start_pl) * delta)),
                )
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(animated_title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.workspace.clone()),
            )
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        let mut root = div()
            .size_full()
            .bg(theme.app_background)
            .flex()
            .flex_row()
            .relative()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.ssh_picker
                    .update(cx, |picker, cx| picker.toggle(window, cx));
            }))
            .on_action(cx.listener(|_this, _: &NewWindow, _window, cx| {
                WindowManager::open_window(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.toggle_sidebar(window, cx);
            }))
            .child(self.render_sidebar(cx))
            .child(self.render_main_area(window, cx))
            .child(self.render_sidebar_toggle(window, cx))
            .child(self.ssh_picker.clone());

        // Host Modal 必须在窗口根节点渲染，才能全屏居中，不被侧边栏裁剪。
        if let Some(modal) = self.host_view.update(cx, |hv, cx| hv.build_modal(cx)) {
            root = root.child(modal);
        }

        // Keychain Modal 同样在窗口根节点渲染
        if let Some(modal) = self.keychain_view.update(cx, |kv, cx| kv.build_modal(cx)) {
            root = root.child(modal);
        }

        root
    }
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Debug)
        .filter_module("russh", log::LevelFilter::Warn)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    use crate::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
            theme::init(cx);
            sidebar_view::font_settings::FontState::init(cx);
            components::init(cx);
            terminal_view::init(cx);
            workspace::init(cx);
            WindowManager::init(cx);
            crate::builtin_commands::executor::init_global_executor();
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                #[cfg(target_os = "windows")]
                KeyBinding::new("ctrl-k", ShowSshPicker, None),
                #[cfg(not(target_os = "windows"))]
                KeyBinding::new("cmd-k", ShowSshPicker, None),
                KeyBinding::new("cmd-n", NewWindow, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            WindowManager::open_window(cx);
            cx.activate(true);
        });
}

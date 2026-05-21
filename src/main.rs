#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod file_browser;
pub mod file_transfer;
mod settings_hub;
pub mod ssh;
pub mod ssh_view;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    AnimationExt, App, Context, Entity, IntoElement, KeyBinding,
    ParentElement, Render, Window, actions, div, px,
};

use crate::components::macos_sidebar::MacosSidebar;
use crate::components::title_bar::TitleBar;
use crate::settings_hub::font_view::FontSettingsView;
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
}

impl AppWindow {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
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
        font_settings_view.update(cx, |view, _cx| {
            view.set_compact(true);
        });
        sidebar.set_font_view(font_settings_view);

        Self {
            title_bar,
            workspace,
            ssh_picker,
            sidebar,
            _sidebar_hide_task: None,
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
        div().child(sidebar.render_sidebar(cx, move |id, _window, cx| {
            entity.update(cx, |this, cx| {
                this.sidebar.handle_item_select(id);
                cx.notify();
            });
        }))
    }

    /// 渲染侧边栏切换按钮
    fn render_sidebar_toggle(&self, window: &gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let animated_title_bar = div().child(self.title_bar.clone()).with_animation(
            ("titlebar-pad-anim", self.sidebar.animation_epoch()),
            gpui::Animation::new(std::time::Duration::from_millis(dur_ms))
                .with_easing(gpui::ease_in_out),
            move |this, delta| this.pl(px(start_pl + (end_pl - start_pl) * delta)),
        );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(animated_title_bar)
            .child(div().flex_1().min_h_0().overflow_hidden().child(self.workspace.clone()))
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;

        div()
            .size_full()
            .bg(theme.app_background)
            .flex()
            .flex_row()
            .relative()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.ssh_picker.update(cx, |picker, cx| picker.toggle(window, cx));
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
            .child(self.ssh_picker.clone())
    }
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .filter_module("russh", log::LevelFilter::Warn)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    use crate::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
            theme::init(cx);
            settings_hub::font_settings::FontState::init(cx);
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

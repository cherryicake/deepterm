#![cfg_attr(target_family = "wasm", no_main)]

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
    App, Context, Entity, IntoElement, KeyBinding,
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
                let _ = entity.update(cx, |this, cx| {
                    this.sidebar.finish_animation();
                    cx.notify();
                });
            }));
        } else {
            self._sidebar_hide_task = None;
        }
        cx.notify();
    }
}

impl Render for AppWindow {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .when(self.sidebar.should_render(), |this| {
                let sidebar = self.sidebar.clone();
                let entity = cx.entity();
                this.child(
                    div()
                        .child(sidebar.render_sidebar(cx, move |id, _window, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                match id {
                                    "themes" => {
                                        this.sidebar.select_item(id);
                                        this.sidebar.navigate_to(
                                            crate::components::macos_sidebar::SidebarView::Theme);
                                    }
                                    "font" => {
                                        this.sidebar.select_item(id);
                                        this.sidebar.navigate_to(
                                            crate::components::macos_sidebar::SidebarView::Font);
                                    }
                                    "__back" => {
                                        this.sidebar.go_back();
                                    }
                                    _ => {
                                        this.sidebar.select_item(id);
                                    }
                                }
                                cx.notify();
                            });
                        })),
                )
            })
            .child(self.sidebar.render_main_area(
                self.title_bar.clone(),
                self.workspace.clone(),
                cx,
            ))
            .when(cfg!(target_os = "macos"), |this| {
                this.child(self.sidebar.render_toggle_button(
                    cx,
                    cx.listener(|this, _, _window, cx| {
                        cx.stop_propagation();
                        this.sidebar.toggle();
                        cx.notify();
                    }),
                ))
            })
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
                KeyBinding::new("cmd-k", ShowSshPicker, None),
                KeyBinding::new("cmd-n", NewWindow, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            WindowManager::open_window(cx);
            cx.activate(true);
        });
}

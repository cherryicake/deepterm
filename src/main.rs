#![cfg_attr(target_family = "wasm", no_main)]

pub mod components;
pub mod layout;
pub mod panels;
pub mod ssh;
pub mod ssh_picker;
pub mod terminal;
pub mod terminal_view;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Entity, IntoElement, KeyBinding, ParentElement, Render, WindowOptions,
    actions, div, size,
};

use crate::components::title_bar::TitleBar;
use crate::ssh_picker::ssh_picker::SshPicker;
use crate::terminal_view::terminal_view::{Copy, Paste};
use crate::workspace::workspace::Workspace;

// Define application-level actions
actions!(app, [Quit, NewLocalTerminal, ShowSshPicker]);

/// 全局 SSH picker 引用，用于从 Workspace 触发显示
struct GlobalSshPicker(Entity<SshPicker>);
impl gpui::Global for GlobalSshPicker {}

/// Root component that contains title bar and workspace
pub struct AppWindow {
    title_bar: Entity<TitleBar>,
    workspace: Entity<Workspace>,
    ssh_picker: Entity<SshPicker>,
}

impl AppWindow {
    pub fn new(cx: &mut App) -> Self {
        // 先创建 Workspace
        let workspace = Workspace::new(cx);

        // 从 Workspace 获取 TabBar，然后创建 TitleBar
        let tab_bar = workspace.read(cx).tab_bar.clone();
        let title_bar = cx.new(|_cx| TitleBar::new("title-bar").with_tab_bar(tab_bar));

        // 创建 SSH 选择器（先不关联 workspace，在 cx.new 内部设置）
        let workspace_for_picker = workspace.clone();
        let ssh_picker = cx.new(|cx| SshPicker::new(cx).with_workspace(workspace_for_picker));

        // 设置全局 SSH picker 引用，让 Workspace 可以通过 action 访问
        cx.set_global(GlobalSshPicker(ssh_picker.clone()));

        Self {
            title_bar,
            workspace,
            ssh_picker,
        }
    }

    /// 显示 SSH 选择器
    pub fn show_ssh_picker(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.ssh_picker.update(cx, |picker, cx| {
            picker.toggle(window, cx);
        });
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("AppWindow")
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.show_ssh_picker(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewLocalTerminal, window, cx| {
                this.workspace.update(cx, |workspace, cx| {
                    workspace.create_local_terminal(window, cx);
                });
            }))
            // Title bar for window management
            .child(self.title_bar.clone())
            // Workspace content
            .child(div().flex().flex_1().child(self.workspace.clone()))
            // SSH picker overlay
            .child(self.ssh_picker.clone())
    }
}

fn main() {
    // 初始化文件日志
    let _ = std::fs::remove_file("/tmp/deepterm.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("/tmp/deepterm.log")
        .expect("无法创建日志文件");

    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .filter_level(log::LevelFilter::Debug)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    gpui::Application::with_platform(gpui_platform::current_platform(false)).run(|cx: &mut App| {
        // Bind keyboard shortcuts
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-l", NewLocalTerminal, None),
            KeyBinding::new("cmd-k", ShowSshPicker, None),
        ]);

        // Register global action handlers
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(gpui::px(1400.0), gpui::px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                // macOS: 禁用原生窗口拖拽，改用 GPUI 手动实现
                // 这样可以避免透明标题栏导致整个区域都可拖拽的问题
                #[cfg(target_os = "macos")]
                is_movable: false,
                #[cfg(not(target_os = "macos"))]
                is_movable: true,
                ..Default::default()
            },
            |_, cx| cx.new(|_cx| AppWindow::new(_cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

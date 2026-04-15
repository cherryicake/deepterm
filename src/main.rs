#![cfg_attr(target_family = "wasm", no_main)]

pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod file_browser;
pub mod highlighter;
mod settings_hub;
pub mod ssh;
pub mod ssh_view;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod workspace;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Entity, IntoElement, KeyBinding, ParentElement, Render, Window,
    WindowBackgroundAppearance, WindowDecorations, WindowOptions, actions, div, size,
};

use crate::components::title_bar::TitleBar;
use crate::ssh_view::picker::SshPicker;
use crate::workspace::workspace::Workspace;

// Define application-level actions
actions!(app, [Quit, ShowSshPicker]);

// Host management actions for context menu
actions!(
    host_actions,
    [ConnectHost, EditHost, DuplicateHost, DeleteHost]
);
const CONTEXT: &str = "AppWindow";

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
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        // 先创建 Workspace
        let workspace = Workspace::new(window, cx);

        // 从 Workspace 获取 TabBar，然后创建 TitleBar
        let tab_bar = workspace.read(cx).tab_bar.clone();
        let title_bar = cx.new(|_cx| TitleBar::new("title-bar").with_tab_bar(tab_bar));

        // 创建 SSH 选择器（先不关联 workspace，在 cx.new 内部设置）
        let workspace_for_picker = workspace.clone();
        let ssh_picker = cx.new(|cx| SshPicker::new(window, cx).with_workspace(workspace_for_picker));

        // 设置全局 SSH picker 引用，让 Workspace 可以通过 action 访问
        cx.set_global(GlobalSshPicker(ssh_picker.clone()));

        Self {
            title_bar,
            workspace,
            ssh_picker,
        }
    }

    /// 设置初始焦点到工作区的第一个终端
    /// 在窗口创建后通过 cx.defer_in 调用
    fn setup_initial_focus(&self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.set_initial_focus(window, cx);
        });
    }

    /// 显示 SSH 选择器
    pub fn show_ssh_picker(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.ssh_picker.update(cx, |picker, cx| {
            picker.toggle(window, cx);
        });
    }
}

impl Render for AppWindow {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.show_ssh_picker(window, cx);
            }))
            // Title bar for window management
            .child(self.title_bar.clone())
            // Workspace content
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.workspace.clone()),
            )
            // SSH picker overlay
            .child(self.ssh_picker.clone())
    }
}

fn main() {
    // 初始化文件日志
    let log_path = if cfg!(windows) {
        std::env::temp_dir().join("deepterm.log")
    } else {
        std::path::PathBuf::from("/tmp/deepterm.log")
    };

    let _ = std::fs::remove_file(&log_path);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .expect("无法创建日志文件");

    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .filter_level(log::LevelFilter::Debug)
        .filter_module("russh", log::LevelFilter::Warn)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    use crate::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
            theme::init(cx);
            components::init(cx);
            terminal_view::init(cx);
            workspace::init(cx);
            // Bind keyboard shortcuts
            // 使用 None 作为 context，让这些快捷键全局有效
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-k", ShowSshPicker, None),
            ]);

            // Register global action handlers
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(gpui::px(1400.0), gpui::px(900.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitleBar::title_bar_options()),
                    // 使用客户端装饰，隐藏系统标题栏
                    window_decorations: Some(WindowDecorations::Client),
                    // macOS: 禁用原生窗口拖拽，改用 GPUI 手动实现
                    // 这样可以避免透明标题栏导致整个区域都可拖拽的问题
                    #[cfg(target_os = "macos")]
                    is_movable: false,
                    #[cfg(not(target_os = "macos"))]
                    is_movable: true,
                    // 窗口背景使用毛玻璃效果
                    window_background: WindowBackgroundAppearance::Blurred,
                    ..Default::default()
                },
                |window, cx| {
                    let app_window = cx.new(|_cx| AppWindow::new(window, _cx));
                    // Schedule initial focus after the window is fully created
                    let window_handle = app_window.clone();
                    window.defer(cx, move |window, cx| {
                        window_handle.update(cx, |app, cx| {
                            app.setup_initial_focus(window, cx);
                        });
                    });
                    app_window
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

#![cfg_attr(target_family = "wasm", no_main)]

pub mod assets;
pub mod builtin_commands;
pub mod components;
pub mod file_browser;
pub mod file_transfer;
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
    AnimationExt, App, Context, ElementId, Entity, IntoElement, KeyBinding, MouseButton,
    ParentElement, Render, Window, actions, div, px,
};

use crate::components::macos_sidebar::MacosSidebar;
use crate::components::title_bar::TitleBar;
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
    sidebar_visible: bool,
    sidebar_animating_out: bool,
    sidebar_epoch: u32,
    _sidebar_hide_task: Option<gpui::Task<()>>,
}

impl AppWindow {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        // 先创建 Workspace
        let workspace = Workspace::new(window, cx);

        // 从 Workspace 获取 TabBar，然后创建 TitleBar
        let tab_bar = workspace.read(cx).tab_bar.clone();
        let workspace_weak = workspace.downgrade();
        let title_bar = cx.new(|cx| {
            let mut bar = TitleBar::new("title-bar").with_tab_bar(tab_bar);
            bar.bind_workspace(workspace_weak, cx);
            bar
        });

        // 创建 SSH 选择器（先不关联 workspace，在 cx.new 内部设置）
        let workspace_for_picker = workspace.clone();
        let ssh_picker =
            cx.new(|cx| SshPicker::new(window, cx).with_workspace(workspace_for_picker));

        // 将 SSH picker 注入到 Workspace，供 TabBar 的 "+" 按钮使用
        workspace.update(cx, |ws, cx| {
            ws.set_ssh_picker(ssh_picker.clone(), cx);
        });

        Self {
            title_bar,
            workspace,
            ssh_picker,
            sidebar_visible: true,
            sidebar_animating_out: false,
            sidebar_epoch: 0,
            _sidebar_hide_task: None,
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

    /// 切换侧边栏显示/隐藏
    fn toggle_sidebar(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.sidebar_visible {
            self.sidebar_visible = false;
            self.sidebar_animating_out = true;
            self.sidebar_epoch = self.sidebar_epoch.wrapping_add(1);
            let entity = cx.entity();
            self._sidebar_hide_task = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250 + 40))
                    .await;
                let _ = entity.update(cx, |this, cx| {
                    this.sidebar_animating_out = false;
                    cx.notify();
                });
            }));
        } else {
            self.sidebar_visible = true;
            self.sidebar_animating_out = false;
            self._sidebar_hide_task = None;
            self.sidebar_epoch = self.sidebar_epoch.wrapping_add(1);
        }
        cx.notify();
    }
}

impl Render for AppWindow {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let is_macos = cfg!(target_os = "macos");
        let text_muted = theme.text;

        // 侧边栏 toggle 按钮的位置：跟随侧边栏开合在 "侧边栏右上角" 与
        // "红绿灯右侧" 之间平滑切换。
        // - 打开时：按钮位于侧边栏面板右上角内侧（距左 228px）
        // - 关闭时：按钮位于窗口左上角红绿灯右侧（距左 84px）
        const BUTTON_LEFT_OPEN: f32 = 228.0;
        const BUTTON_LEFT_CLOSED: f32 = 84.0;
        // 红绿灯 侧边栏按钮 TittleBar 三者中心对齐到 y=22（TitleBar 44px / 2）：
        const BUTTON_TOP: f32 = 12.0;
        const SIDEBAR_ANIM_ENTER_MS: u64 = 300;
        const SIDEBAR_ANIM_EXIT_MS: u64 = 250;
        // sidebar 关闭后，TitleBar 容器需要左移让出红绿灯 + 按钮空间。
        // 红绿灯结束 ~82px、按钮 84..112、再留 8px 间距 => TabBar 起点 = 120。
        // sidebar 打开时该 padding 归零，TabBar 紧贴 sidebar 右边缘。
        const TITLE_BAR_EXTRA_PAD: f32 = 120.0;

        div()
            .size_full()
            .bg(theme.app_background)
            .flex()
            .flex_row()
            .relative()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ShowSshPicker, window, cx| {
                this.show_ssh_picker(window, cx);
            }))
            .on_action(cx.listener(|_this, _: &NewWindow, _window, cx| {
                WindowManager::open_window(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.toggle_sidebar(window, cx);
            }))
            // 侧边栏（上边界距窗口顶部 8px，左/下距边界 8px，右侧紧贴 workspace）
            .when(self.sidebar_visible || self.sidebar_animating_out, |this| {
                this.child(div().pl(px(8.0)).pt(px(8.0)).mb(px(8.0)).child(
                    MacosSidebar::new().render_animated(
                        self.sidebar_visible,
                        self.sidebar_epoch,
                        cx,
                    ),
                ))
            })
            // 右侧区域：title bar + workspace
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    // sidebar 关闭后，TitleBar 起始位置需要为按钮让出空间，
                    // 避免 TabBar 与按钮重叠。这里给 title-bar 容器加一段
                    // 动画 padding，开合时与 sidebar 宽度动画同步过渡。
                    .child({
                        let (start_pl, end_pl, dur_ms) = if is_macos {
                            if self.sidebar_visible {
                                (TITLE_BAR_EXTRA_PAD, 0.0, SIDEBAR_ANIM_ENTER_MS)
                            } else {
                                (0.0, TITLE_BAR_EXTRA_PAD, SIDEBAR_ANIM_EXIT_MS)
                            }
                        } else {
                            (0.0, 0.0, 0)
                        };
                        let pad_anim_id: ElementId =
                            ("titlebar-pad-anim", self.sidebar_epoch).into();
                        let title_bar = self.title_bar.clone();
                        if is_macos {
                            div().child(title_bar).with_animation(
                                pad_anim_id,
                                gpui::Animation::new(std::time::Duration::from_millis(dur_ms))
                                    .with_easing(gpui::ease_in_out),
                                move |this, delta| {
                                    let current = start_pl + (end_pl - start_pl) * delta;
                                    this.pl(px(current))
                                },
                            )
                        } else {
                            div().child(title_bar).with_animation(
                                pad_anim_id,
                                gpui::Animation::new(std::time::Duration::from_millis(1))
                                    .with_easing(gpui::ease_in_out),
                                move |this, _| this,
                            )
                        }
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.workspace.clone()),
                    ),
            )
            // macOS 侧边栏开关按钮：始终渲染并跟随侧边栏宽度动画位移。
            // 视觉上属于侧边栏组件，但用 absolute 浮在标题栏区域之上，
            // 这样侧边栏关闭时按钮自然落到红绿灯右侧。
            .when(is_macos, |this| {
                let (start_left, end_left, duration_ms) = if self.sidebar_visible {
                    (BUTTON_LEFT_CLOSED, BUTTON_LEFT_OPEN, SIDEBAR_ANIM_ENTER_MS)
                } else {
                    (BUTTON_LEFT_OPEN, BUTTON_LEFT_CLOSED, SIDEBAR_ANIM_EXIT_MS)
                };
                let anim_id: ElementId = ("sidebar-toggle-anim", self.sidebar_epoch).into();
                this.child(
                    div()
                        .id(("sidebar-toggle-btn", self.sidebar_epoch))
                        .absolute()
                        .top(px(BUTTON_TOP))
                        .left(px(start_left))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(36.))
                        .h(px(22.))
                        .rounded(px(12.0))
                        .text_color(text_muted)
                        .hover(|style| style.bg(theme.title_bar_hover))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.toggle_sidebar(window, cx);
                            }),
                        )
                        .child(
                            gpui::svg()
                                .size(px(20.0))
                                .text_color(text_muted)
                                .path(crate::assets::Icon::Sidebar.path()),
                        )
                        .with_animation(
                            anim_id,
                            gpui::Animation::new(std::time::Duration::from_millis(duration_ms))
                                .with_easing(gpui::ease_in_out),
                            move |this, delta| {
                                let current = start_left + (end_left - start_left) * delta;
                                this.left(px(current))
                            },
                        ),
                )
            })
            // SSH picker overlay
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
            // Bind keyboard shortcuts
            // 使用 None 作为 context，让这些快捷键全局有效
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-k", ShowSshPicker, None),
                KeyBinding::new("cmd-n", NewWindow, None),
            ]);

            // Register global action handlers
            cx.on_action(|_: &Quit, cx| cx.quit());

            WindowManager::open_window(cx);
            cx.activate(true);
        });
}

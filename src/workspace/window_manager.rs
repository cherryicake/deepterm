//! Window manager for multi-window support
//!
//! Manages all open DeepTerm windows and provides operations to open new windows.

use gpui::{
    App, AppContext, BorrowAppContext, Entity, Global, WeakEntity, WindowBackgroundAppearance,
    WindowDecorations, WindowOptions,
};

use super::title_bar::TitleBar;
use crate::AppWindow;

/// Manages all open DeepTerm windows.
///
/// Stored as a GPUI global so it can be accessed from anywhere in the app.
pub struct WindowManager {
    /// 会话持久化的责任窗口（"主窗口"）。
    ///
    /// 会话状态写入单一的 `session_state.json`：每个窗口各自持有
    /// `PersistenceManager` 时，后落盘的窗口会整体覆盖先落盘的窗口，
    /// 导致其他窗口的标签页与 SSH 会话全部丢失。因此只有主窗口写盘。
    main_window: Option<WeakEntity<AppWindow>>,
}

impl Global for WindowManager {}

impl WindowManager {
    pub fn init(cx: &mut App) {
        cx.set_global(Self { main_window: None });

        // Quit the application when the last window is closed.
        cx.set_quit_mode(gpui::QuitMode::LastWindowClosed);
    }

    /// 窗口创建时调用：首个窗口成为主窗口。
    /// 全局不存在时（如非窗口路径的单测）视为唯一窗口，返回 true。
    pub fn claim_main_window(cx: &mut App, window: &Entity<AppWindow>) -> bool {
        if !cx.has_global::<Self>() {
            return true;
        }
        cx.update_global::<Self, _>(|this, _| this.claim(window))
    }

    /// 主窗口已关闭时，由存活的窗口接管落盘职责，
    /// 避免"主窗口关掉后，剩下的窗口再也无法持久化"。
    pub fn take_over_main_window(cx: &mut App, window: &Entity<AppWindow>) -> bool {
        if !cx.has_global::<Self>() {
            return true;
        }
        cx.update_global::<Self, _>(|this, _| this.claim(window))
    }

    /// 主窗口槽位为空（尚未分配，或原主窗口已析构）时认领之。
    fn claim(&mut self, window: &Entity<AppWindow>) -> bool {
        let slot_free = self
            .main_window
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .is_none();
        if slot_free {
            self.main_window = Some(window.downgrade());
            true
        } else {
            false
        }
    }

    /// Open a new DeepTerm window with the default size and options.
    ///
    /// `open_paths`, when given, are opened in the editor once the window is
    /// ready (supports `deepterm <file>...` from the command line).
    /// 从上次会话里读回窗口尺寸。
    ///
    /// 读不到（首次启动、文件损坏、字段缺失）就返回 `None`，由调用方用
    /// 默认尺寸。这里**不能**因为读失败就中断启动——窗口尺寸是锦上添花，
    /// 不是启动前置条件。
    ///
    /// 尺寸还会被夹在合理区间内：会话文件可能被手工改过，或者上一次是
    /// 在一块已经拔掉的外接屏上留下的巨型尺寸。直接用会让窗口开到屏幕外。
    fn restored_window_size() -> Option<(f32, f32)> {
        /// 低于这个尺寸窗口会挤成一团（连标题栏都放不下）。
        const MIN: f32 = 400.0;
        /// 高于这个尺寸基本不可能是一块真实屏幕，更可能是坏数据。
        const MAX: f32 = 10_000.0;

        let manager = crate::workspace::persistence::PersistenceManager::new();
        let session = match manager.load_session() {
            Ok(Some(session)) => session,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("读取窗口尺寸失败（{e}），用默认尺寸");
                return None;
            }
        };
        let (w, h) = session.window_size?;
        if !w.is_finite() || !h.is_finite() {
            return None;
        }
        // 存的是 `Window::bounds().size`（内容区），而这里建窗用的是
        // `WindowBounds::Windowed`，尺寸按**外框**理解。两者差一圈边框
        // （本机实测 20×20），不补偿的话每重启一次窗口就长一点，几次后
        // 终端列数变了，TUI 又要重新布局——正是我们要避免的。
        const BORDER: f32 = 20.0;
        Some(((w - BORDER).clamp(MIN, MAX), (h - BORDER).clamp(MIN, MAX)))
    }

    pub fn open_window(cx: &mut App, open_paths: Vec<std::path::PathBuf>) {
        // 恢复上次关窗时的尺寸；没有记录就用默认的 1400×900 居中。
        //
        // 这一步直接关系到"重启后 TUI 布局乱"：窗口尺寸不变，守护进程里的
        // 全屏程序（nvim / htop）就不必按新列数重新布局，接回来的画面能原样
        // 对上。以前每次都用 1400×900，关掉前若是最大化，重开后窗口骤变，
        // 界面自然错位。
        let size = Self::restored_window_size()
            .map(|(w, h)| gpui::size(gpui::px(w), gpui::px(h)))
            .unwrap_or_else(|| gpui::size(gpui::px(1400.0), gpui::px(900.0)));
        let bounds = gpui::Bounds::centered(None, size, cx);

        if let Err(e) = cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                window_decorations: Some(WindowDecorations::Client),
                // 桌面环境靠它识别应用：X11 下 gpui 据此写 WM_CLASS，
                // Wayland 下据此写 xdg_toplevel 的 app_id。
                //
                // 不设的话这两个属性**都不存在**（不是空值，是压根没有这个
                // property），GNOME Shell 匹配不到任何 .desktop 条目，dock
                // 只能画通用的「未知应用」图标 —— 此时 _NET_WM_ICON 也是空的，
                // 连兜底都没有。值必须与 deepterm.desktop 的 StartupWMClass
                // 一致才能匹配上 Icon=deepterm 装进 hicolor 的那张图。
                app_id: Some("deepterm".to_string()),
                #[cfg(target_os = "macos")]
                is_movable: false,
                #[cfg(not(target_os = "macos"))]
                is_movable: true,
                window_background: WindowBackgroundAppearance::Blurred,
                show: false,
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                let app_window = cx.new(|_cx| AppWindow::new(window, _cx));
                let window_handle = app_window.clone();
                window.defer(cx, move |window, cx| {
                    window_handle.update(cx, |app, cx| {
                        app.setup_initial_focus(window, cx);
                        for path in &open_paths {
                            app.editor.update(cx, |editor, cx| {
                                editor.open_file(path.clone(), window, cx);
                            });
                        }
                    });
                    // 窗口内容初始化完成后，再显示并激活窗口
                    // 避免 Windows 上启动时出现空白窗口/闪烁问题
                    window.activate_window();
                });
                app_window
            },
        ) {
            log::error!("Failed to open DeepTerm window: {e:?}");
        }
    }
}

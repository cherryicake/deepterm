#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use mimalloc::MiMalloc;

use deepterm::{
    AppWindow, NewWindow, Quit,
    builtin_commands::executor::init_global_executor,
    components, editor, ssh_view::picker::ShowSshPicker, terminal_view,
    theme,
    workspace::{self, window_manager::WindowManager},
};
use gpui::{App, KeyBinding};

// 计数分配器仅用于 bench/测试环境的分配抖动测量；
// 正式产物保持纯 mimalloc。
#[cfg(feature = "alloc-probe")]
#[global_allocator]
static GLOBAL: deepterm::utils::alloc_probe::CountingAllocator<MiMalloc> =
    deepterm::utils::alloc_probe::CountingAllocator { inner: MiMalloc };

#[cfg(not(feature = "alloc-probe"))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    let default_level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    env_logger::Builder::from_default_env()
        .filter_level(default_level)
        .filter_module("russh", log::LevelFilter::Warn)
        // macOS 的 .AppleSystemUIFont 存在重复注册的字体条目，gpui 的文本系统
        // 每次启动都会为此打 WARN（上游已知噪音，gpui 为固定 rev 的 git 依赖），
        // 静音到 Error 级别。
        .filter_module("gpui_macos", log::LevelFilter::Error)
        .init();

    log::info!("=== DeepTerm 启动 ===");

    use deepterm::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
            theme::init(cx);
            deepterm::sidebar_view::font_settings::FontState::init(cx);
            deepterm::file_manager::watcher::init(cx);
            components::init(cx);
            terminal_view::init(cx);
            workspace::init(cx);
            editor::init(cx);
            WindowManager::init(cx);
            init_global_executor();
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                #[cfg(target_os = "windows")]
                KeyBinding::new("ctrl-k", ShowSshPicker, None),
                #[cfg(not(target_os = "windows"))]
                KeyBinding::new("cmd-k", ShowSshPicker, None),
                KeyBinding::new("cmd-n", NewWindow, None),
            ]);
            cx.on_action(|_: &Quit, cx| {
                // 退出前同步落盘所有窗口的待保存会话：
                // defer_save_session 是 300ms 防抖任务，若刚变更完立刻退出，
                // 任务未触发就会丢失最后一次会话状态。
                for handle in cx.windows() {
                    let _ = handle.update(cx, |_view, window, cx| {
                        if let Some(app) = window.root::<AppWindow>().and_then(|e| e) {
                            app.update(cx, |app, cx| app.flush_pending_save(cx));
                        }
                    });
                }
                cx.quit();
            });

            // `deepterm <file>...` opens files in the editor on startup.
            let open_paths: Vec<std::path::PathBuf> = std::env::args()
                .skip(1)
                .filter(|a| !a.starts_with('-'))
                .map(std::path::PathBuf::from)
                .collect();
            for path in &open_paths {
                if !path.exists() {
                    log::warn!("open_path does not exist: {:?}", path);
                }
            }
            WindowManager::open_window(cx, open_paths);
            cx.activate(true);
        });
}

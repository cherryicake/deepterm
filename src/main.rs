#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use deepterm::{
    AppWindow, NewWindow, Quit,
    builtin_commands::executor::init_global_executor,
    components, editor, ssh_view::picker::ShowSshPicker, terminal_view,
    theme,
    workspace::{self, window_manager::WindowManager},
};
use gpui::{App, KeyBinding};

// 分配器：非 Windows 用 mimalloc，Windows 用系统分配器。
//
// Windows 侧退回 `System` 的原因见 `Cargo.toml` 里同名的 gate 注释——是
// MSVC 无法处理含非 ASCII 字符的 cargo 源码路径，不是 mimalloc 缺 Windows 产物。
#[cfg(not(target_os = "windows"))]
type BaseAllocator = mimalloc::MiMalloc;
#[cfg(not(target_os = "windows"))]
const BASE_ALLOCATOR: BaseAllocator = mimalloc::MiMalloc;

#[cfg(target_os = "windows")]
type BaseAllocator = std::alloc::System;
#[cfg(target_os = "windows")]
const BASE_ALLOCATOR: BaseAllocator = std::alloc::System;

// 计数分配器仅用于 bench/测试环境的分配抖动测量；
// 正式产物保持纯基础分配器。
#[cfg(feature = "alloc-probe")]
#[global_allocator]
static GLOBAL: deepterm::utils::alloc_probe::CountingAllocator<BaseAllocator> =
    deepterm::utils::alloc_probe::CountingAllocator {
        inner: BASE_ALLOCATOR,
    };

#[cfg(not(feature = "alloc-probe"))]
#[global_allocator]
static GLOBAL: BaseAllocator = BASE_ALLOCATOR;

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
            // 设置系统必须最先就绪：theme / font / editor / keymap 都从它取值。
            deepterm::settings::init(cx);
            theme::init(cx);
            deepterm::sidebar_view::font_settings::FontState::init(cx);
            // keymap 依赖 settings（vim 开关决定是否加载 vim 键位表）。
            deepterm::keymap::init(cx);
            deepterm::file_manager::watcher::init(cx);
            // LSP 全局管理器：必须在 `editor::init` 之前（编辑器开 tab 时会
            // 向它查询/启动语言服务器）。它依赖 settings 的 lsp 段。
            deepterm::lsp::init(cx);
            components::init(cx);
            terminal_view::init(cx);
            workspace::init(cx);
            editor::init(cx);
            WindowManager::init(cx);
            init_global_executor();
            deepterm::keymap::bind(cx, [
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
                // 语言服务器走 LSP 关闭握手（shutdown → exit）再退出：
                // 直接丢弃会让子进程变成孤儿（编辑器没了、服务器还在跑）。
                // 握手是异步的，给它一点时间把帧写出去；`shutdown_all`
                // 内部有 5s 上限，最坏情况会强杀，不会永久挂住退出。
                deepterm::lsp::LspManager::shutdown_all(cx);
                std::thread::sleep(std::time::Duration::from_millis(150));
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

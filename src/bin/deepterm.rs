#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use deepterm::{
    AppWindow, NewWindow, Quit, builtin_commands::executor::init_global_executor,
    ssh_view::picker::ShowSshPicker, workspace::window_manager::WindowManager,
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
    // 日志配置与 deepcore 共用（`utils::logging`）：排查"终端为什么没恢复"
    // 时要对着两边的日志看，格式与级别必须一致。
    let mut builder =
        deepterm::utils::logging::base_log_builder(deepterm::utils::logging::default_level());
    // GUI 程序没有可用的 stderr（release 下还是
    // `windows_subsystem = "windows"`，连控制台都没有），排查「补全不弹窗」
    // 这类问题时只能靠日志文件。写失败就退回仅 stderr，不能因此起不来。
    let log_path = deepterm::utils::logging::app_log_path();
    if let Some(path) = &log_path {
        match deepterm::utils::logging::CappedLog::open(path.clone()) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!("deepterm: cannot open log file {}: {e}", path.display());
            }
        }
    }
    builder.init();
    deepterm::utils::logging::install_panic_hook();

    log::info!("=== DeepTerm 启动 ===");
    if let Some(path) = &log_path {
        log::info!("日志文件: {}", path.display());
    }

    // 单实例：同一个用户只跑一个 GUI。
    //
    // 没有这一步时，用户多点几次桌面图标就起好几个进程，每个都开一个窗口；
    // 而 gpui 在 Linux 上 `activate()` 是空实现（源码里就写着
    // `activate is not implemented on Linux`），新窗口既不置前也不抢焦点，
    // 用户看到的是"点了没反应，过一会儿又突然能开了"。
    //
    // 拿不到锁说明已有实例在跑：直接退出，不再开窗。锁在进程退出时由内核
    // 释放，不会留下"文件在、进程没了"的假阳性。
    let _instance_lock = match deepterm::single_instance::InstanceLock::acquire() {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            log::info!("已有 DeepTerm 实例在运行，本次启动退出");
            // 给终端用户的反馈：从命令行启动时能立刻明白发生了什么。
            eprintln!("deepterm: 已有一个 DeepTerm 实例在运行。");
            return;
        }
        Err(e) => {
            // 锁机制坏了不能变成"应用打不开"：记一条日志，照常启动。
            log::warn!("无法建立实例锁（{e}），本次启动不做单实例检查");
            None
        }
    };

    use deepterm::assets::Assets;

    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(Assets)
        .run(|cx: &mut App| {
            // 所有子系统的初始化在库里（`deepterm::init`），这样它可被
            // `cargo test --lib` 覆盖——`main.rs` 是二进制目标，测试看不到。
            // 此前 `vim::init` 就是在这里漏掉的，导致 vim 的寄存器全局
            // 从未注册、按 `yy` 直接 panic。
            deepterm::init(cx);
            init_global_executor();
            deepterm::keymap::bind(
                cx,
                [
                    // `secondary-` = macOS 的 ⌘ / 其他平台的 Ctrl。
                    // 写死 `cmd-` 在 Linux/Windows 上会映射到 Super 键，按 Ctrl 无效。
                    KeyBinding::new("secondary-q", Quit, None),
                    KeyBinding::new("secondary-k", ShowSshPicker, None),
                    KeyBinding::new("secondary-n", NewWindow, None),
                ],
            );
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

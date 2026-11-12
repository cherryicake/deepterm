#![cfg_attr(target_family = "wasm", no_main)]
// 在 Windows Release 构建时隐藏控制台窗口，避免双击启动时弹出终端
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use deepterm::{
    AppWindow, NewWindow, Quit, builtin_commands::executor::init_global_executor,
    ssh_view::picker::ShowSshPicker, workspace::window_manager::WindowManager,
};
use gpui::{App, KeyBinding};
use std::io::{Seek as _, Write as _};

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

/// 日志文件路径（`<config>/deepterm/deepterm.log`）。
///
/// 与 `keychain.json` / `ssh.json` 同一目录约定。路径取不到就返回 `None`
/// （退回仅 stderr，不影响启动）。
fn log_file_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("deepterm").join("deepterm.log"))
}

/// 日志文件上限：超过后**只保留末尾一小段**，不生成 `.1` 备份。
///
/// 之前是 16 MB + 一份 `deepterm.log.1`；用户只需要“崩溃前最后一点日志”，
/// 不需要多份历史文件。现在单文件最多 1 MiB，超限时原地裁剪到末尾
/// 256 KiB，既不占磁盘，也保留最近一次崩溃的上下文。
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// 超限后保留的末尾字节数。取 256 KiB：足够装下带栈的 panic 前后文，
/// 又不会让单文件继续膨胀。
const KEEP_LOG_BYTES: usize = 256 * 1024;

/// 单文件、超限裁尾的日志 writer。
///
/// `env_logger::Target::Pipe` 要求 `Write + Send`，这里只实现 `write`/
/// `flush`；裁剪失败（磁盘满、权限）时继续写当前文件，不能因为日志问题
/// 影响应用启动/运行。
struct CappedLog {
    path: std::path::PathBuf,
    file: std::fs::File,
    len: u64,
}

impl CappedLog {
    fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        // 升级清理：旧版本会留 `deepterm.log.1`，不再需要。
        let legacy_backup = path.with_extension("log.1");
        let _ = std::fs::remove_file(&legacy_backup);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.seek(std::io::SeekFrom::End(0))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut this = Self { path, file, len };
        if this.len > MAX_LOG_BYTES {
            let _ = this.trim_to_tail();
        }
        Ok(this)
    }

    /// 把文件原地截成最后 [`KEEP_LOG_BYTES`] 字节（尽量从行首开始）。
    ///
    /// 先读后写：日志文件最多 1 MiB，一次性读入内存没有压力。
    fn trim_to_tail(&mut self) -> std::io::Result<()> {
        let data = std::fs::read(&self.path)?;
        let mut start = data.len().saturating_sub(KEEP_LOG_BYTES);
        // 不要从半行开始：往后找到第一个换行；找不到就保留原起点。
        if start > 0
            && let Some(offset) = data[start..].iter().position(|byte| *byte == b'\n')
        {
            start += offset + 1;
        }
        let tail = &data[start..];

        // 复用同一个句柄原地截断，避免 Windows 上“旧句柄未关、再以
        // truncate 打开同名文件”的共享冲突。
        self.file.flush()?;
        self.file.set_len(0)?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(tail)?;
        self.file.flush()?;
        self.len = tail.len() as u64;
        Ok(())
    }
}

impl std::io::Write for CappedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.len.saturating_add(buf.len() as u64) > MAX_LOG_BYTES {
            let _ = self.trim_to_tail();
        }
        let written = self.file.write(buf)?;
        self.len = self.len.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deepterm-log-test-{}-{tag}.log",
            std::process::id()
        ))
    }

    #[test]
    fn log_caps_itself_and_keeps_only_tail() {
        let path = temp_path("cap");
        let _ = std::fs::remove_file(&path);
        {
            let mut log = CappedLog::open(path.clone()).expect("open log");
            // 写入超过一个上限，且每行带标记，方便确认留下的是末尾。
            let line = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
            let lines = (MAX_LOG_BYTES as usize / line.len()) + 128;
            for _ in 0..lines {
                log.write_all(line).expect("write log");
            }
            log.flush().expect("flush");
        }
        let len = std::fs::metadata(&path).expect("metadata").len();
        assert!(len <= MAX_LOG_BYTES, "日志必须被裁到上限内，实际 {len}");
        assert!(
            len >= KEEP_LOG_BYTES as u64 / 2,
            "裁尾后仍应保留一段崩溃上下文，实际 {len}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_removes_legacy_deepterm_log_1() {
        let path = temp_path("legacy");
        let backup = path.with_extension("log.1");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
        std::fs::write(&backup, b"old").expect("write legacy");
        let log = CappedLog::open(path.clone()).expect("open log");
        drop(log);
        assert!(!backup.exists(), "旧的 deepterm.log.1 应被清理");
        let _ = std::fs::remove_file(path);
    }
}

fn main() {
    // release 默认 Info：之前 debug/release 都硬编码 Debug，单次会话能写出
    // 几十 MB 日志，真正的问题反而被 DEBUG 淹没。需要排查时用 RUST_LOG 覆盖。
    let default_level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    let mut builder = env_logger::Builder::from_default_env();
    builder
        .filter_level(default_level)
        .filter_module("russh", log::LevelFilter::Warn)
        // 终端应用大量使用 shell integration/OSC，vte 对每条未处理的 OSC
        // 都打一条 DEBUG；这是噪音，不是异常。
        .filter_module("vte::ansi", log::LevelFilter::Warn)
        // macOS 的 .AppleSystemUIFont 存在重复注册的字体条目，gpui 的文本系统
        // 每次启动都会为此打 WARN（上游已知噪音，gpui 为固定 rev 的 git 依赖），
        // 静音到 Error 级别。
        .filter_module("gpui_macos", log::LevelFilter::Error)
        // gpui 每帧都在打 a11y 树更新（实测一次会话 6.9 万行），会把日志文件
        // 撑爆并冲掉真正有用的线索。
        .filter_module("gpui::window", log::LevelFilter::Warn);

    // 日志同时落文件：GUI 程序没有可用的stderr（release 下还是
    // `windows_subsystem = "windows"`，连控制台都没有），排查「补全不弹窗」
    // 这类问题时只能靠日志文件。写失败就退回仅stderr，不能因此起不来。
    if let Some(path) = log_file_path() {
        match CappedLog::open(path.clone()) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!("deepterm: cannot open log file {}: {e}", path.display());
            }
        }
    }
    builder.init();

    log::info!("=== DeepTerm 启动 ===");
    if let Some(path) = log_file_path() {
        log::info!("日志文件: {}", path.display());
    }

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

//! 两个可执行文件共用的启动杂项：日志初始化与路径约定。
//!
//! `deepterm`（GUI）与 `deepcore`（终端守护进程）是两个独立进程，但它们
//! **必须写同一套日志配置**：排查"终端为什么没恢复"这类问题时，两边的
//! 时间线要能对着看。这份逻辑放库里，`src/cli/*.rs` 只保留各自的入口。

use std::io::{Seek as _, Write as _};

/// 日志文件上限：超过后**只保留末尾一小段**，不生成 `.1` 备份。
///
/// 之前是 16 MB + 一份 `.log.1`；用户只需要"崩溃前最后一点日志"，
/// 不需要多份历史文件。现在单文件最多 1 MiB，超限时原地裁剪到末尾
/// 256 KiB，既不占磁盘，也保留最近一次崩溃的上下文。
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// 超限后保留的末尾字节数。取 256 KiB：足够装下带栈的 panic 前后文，
/// 又不会让单文件继续膨胀。
pub const KEEP_LOG_BYTES: usize = 256 * 1024;

/// 单文件、超限裁尾的日志 writer。
///
/// `env_logger::Target::Pipe` 要求 `Write + Send`，这里只实现 `write`/
/// `flush`；裁剪失败（磁盘满、权限）时继续写当前文件，不能因为日志问题
/// 影响应用启动/运行。
pub struct CappedLog {
    path: std::path::PathBuf,
    file: std::fs::File,
    len: u64,
}

impl CappedLog {
    pub fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
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

        // 复用同一个句柄原地截断，避免 Windows 上"旧句柄未关、再以
        // truncate 打开同名文件"的共享冲突。
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

/// GUI 的日志文件路径（`<config>/deepterm/deepterm.log`）。
pub fn app_log_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("deepterm").join("deepterm.log"))
}

/// 守护进程的日志文件路径（`<config>/deepterm/deepcore.log`）。
///
/// 与 GUI **分开**：两者都是长跑进程，混在一个文件里会让时间线互相穿插，
/// 而且 GUI 的裁尾会把守护进程的历史挤掉。
pub fn core_log_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("deepterm").join("deepcore.log"))
}

/// 装上 `env_logger` 的基础过滤规则（两个进程共用）。
///
/// 只设级别与噪音模块，不碰输出目标——由调用方决定写文件还是 stderr。
pub fn base_log_builder(default_level: log::LevelFilter) -> env_logger::Builder {
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
    builder
}

/// 安装 panic hook：把 panic 的线程、位置、消息和强制捕获的栈写进日志文件。
///
/// 为什么必须有：GUI 进程从 Finder 启动时 stderr 指向 `/dev/null`，Rust
/// 默认 hook 打印的 panic 消息直接丢失；而 `extern "C"` 回调里的 panic 在
/// `.ips` 崩溃报告里只剩 `panic_cannot_unwind`，没有消息也没有调用方。
/// 之前几次崩溃因此完全无法定位。hook 返回后运行时照常 abort，不改变行为。
///
/// 两个二进制都该装：`deepcore` 是常驻进程，它崩了更难被注意到——没有
/// 窗口、没有 stderr，只剩日志里这一条。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("unnamed");
        let thread_id = thread.id();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        // `force_capture` 不受 RUST_BACKTRACE 影响：release 版默认不打印栈，
        // 这里必须无条件抓取。
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!(
            "=== PANIC === thread={thread_name} ({thread_id:?}) at {location}: {message}\n{backtrace}"
        );
        log::logger().flush();
    }));
}

/// 默认日志级别：debug 构建给 Debug，release 给 Info。
///
/// 之前 release 也硬编码 Debug，单次会话能写出几十 MB，真正的问题反而被
/// DEBUG 淹没。需要排查时用 `RUST_LOG` 覆盖。
pub fn default_level() -> log::LevelFilter {
    if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    }
}

#[cfg(test)]
mod tests {
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

    /// 两个进程的日志必须分文件。
    ///
    /// 混在一起时，GUI 的裁尾会把守护进程的历史挤掉——而这恰恰是排查
    /// "终端为什么没恢复"时最需要的那段。
    #[test]
    fn the_two_processes_log_to_different_files() {
        let (Some(app), Some(core)) = (app_log_path(), core_log_path()) else {
            return; // 无 HOME 的环境跳过
        };
        assert_ne!(app, core);
        assert!(app.to_string_lossy().contains("deepterm.log"));
        assert!(core.to_string_lossy().contains("deepcore.log"));
    }
}

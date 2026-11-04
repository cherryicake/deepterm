//! 子进程的 `PATH` 策略。
//!
//! 本模块只有两个出口，按「谁来读 rc 文件」分成两档：
//!
//! | 出口 | 用途 | 用户级目录 |
//! |---|---|---|
//! | [`local_spawn_path`] | 按**名字**启动的子进程 | 前置 |
//! | [`system_search_path`] | 终端 PTY 的初始环境 | 不含 |
//!
//! # 一、`local_spawn_path`：按名字启动的子进程
//!
//! deepterm 里有三类子进程是 `Command::new("rust-analyzer")` 这种**按名字**
//! 启动的，它们都不经过用户的交互 shell，因此拿不到 rc 文件里设置的 `PATH`：
//!
//! 1. 语言服务器（`lsp::jsonrpc::Connection::spawn`）；
//! 2. agent CLI（`terminal_view` 启用 Codex hooks 时调 `codex`）；
//! 3. git（`editor::git_repository::git_command`）。
//!
//! 拼装顺序：**用户级目录 → 系统目录（含 Homebrew）→ 继承来的 `PATH`**。
//!
//! # 二、`system_search_path`：终端 PTY 的初始环境
//!
//! [`crate::terminal::mappings::default_terminal_env`] 用的就是它，**不含**
//! 用户级目录。这是刻意的：PTY 里跑的是**交互 shell**，它自己会读
//! `~/.zshrc` 把 `PATH` 补回来；deepterm 只需要保证「系统目录都在」，多塞
//! 用户级目录反而会在 rc 之前抢答（同名二进制时可能选中用户并不想用的那份）。
//!
//! # 为什么继承来的 `PATH` 不够
//!
//! 从 Finder / Dock 启动时，本进程的 `PATH` 是 launchd 给的那份
//! （`/usr/bin:/bin:/usr/sbin:/sbin`），`~/.cargo/bin` 与 Homebrew 都不在其中。
//! 于是 `Command::new("rust-analyzer")` 直接 `NotFound`——而同一台机器上用户
//! 在终端里敲 `rust-analyzer --version` 一切正常。差别只在启动方式。
//!
//! 实测现场：Finder 启动后打开 `.rs` 文件，状态栏显示「LSP 失败: rust
//! 服务器启动失败」，日志里是 `failed to spawn language server
//! rust-analyzer`；而 `~/.cargo/bin/rust-analyzer` 一直都在。
//!
//! # 为什么要显式补进用户级目录
//!
//! [`USER_BIN_DIRS`] 这些目录「只在 rc 文件里出现」，继承来的 `PATH` 里没有。
//! 装 Codex hook 时 deepterm 要**自己**调一次 agent 的命令行（Codex 需要
//! `features enable hooks`），而这条调用两边都不读 rc：远端是 SFTP 会话上的
//! `channel.exec`（无 PTY、非登录 shell），本地是 `Command::new` 配一份自拼的
//! `PATH`。
//!
//! 实测：Codex 由 standalone 安装器装在 `~/.local/bin`，远端非交互通道
//! `command -v codex` 为空、`codex features enable hooks` 退出 127，安装
//! 因此在写 `hooks.json` 之前中断——现场就是「上报脚本在、hook 没装」。
//!
//! 登录 shell 不是替代方案：zsh 的 `-l` 只读 `~/.zprofile`，而 `PATH` 常写在
//! `~/.zshrc` 里；nvm 管理的 node 目录同样只在 rc 里。列目录更确定。
//!
//! # 用户级目录为什么前置
//!
//! 同名的 `codex` / `rust-analyzer` 若在系统目录里也有一份（Homebrew、
//! 发行版包管理器），用户日常在用的是 `~/.local/bin` / `~/.cargo/bin` 里
//! 那份——装 hook 要开的是**那个**版本的特性，索引也要用**那个**编译器。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// macOS 系统路径配置文件。
#[cfg(target_os = "macos")]
const PATHS_FILE: &str = "/etc/paths";
#[cfg(target_os = "macos")]
const PATHS_D_DIR: &str = "/etc/paths.d";

/// Homebrew 的固定落点（Apple Silicon 与 Intel 各一份）。
#[cfg(target_os = "macos")]
const HOMEBREW_PATHS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// 用户级可执行目录（相对 `$HOME`）：standalone 安装器（Codex）、cargo、
/// npm / bun 全局安装的默认落点。
///
/// 这些都是「只在 rc 文件里出现」的目录，继承来的 `PATH` 里没有——理由与
/// 取值见[模块文档](self#为什么要显式补进用户级目录)。**只**用于
/// [`local_spawn_path`] 这一档；终端 PTY 的初始环境不含它们。
pub const USER_BIN_DIRS: &[&str] = &[
    ".local/bin",
    ".cargo/bin",
    ".npm-global/bin",
    ".bun/bin",
    "bin",
];

/// [`USER_BIN_DIRS`] 在给定主目录下的绝对路径。
pub fn user_bin_dirs(home: &Path) -> Vec<PathBuf> {
    USER_BIN_DIRS.iter().map(|dir| home.join(dir)).collect()
}

/// 把用户级可执行目录**前置**到一组既有搜索路径上，拼成可直接塞进
/// `Command::env("PATH", …)` 的字符串。
///
/// 重复目录不去重：`PATH` 里重复项无害，而去重要在每个平台上处理
/// 大小写/符号链接语义，收益不成比例。
pub fn path_with_user_bins(home: &Path, base: &[PathBuf]) -> Option<String> {
    let dirs = user_bin_dirs(home).into_iter().chain(base.iter().cloned());
    std::env::join_paths(dirs)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// 解析 `/etc/paths` 与 `/etc/paths.d/*` 的行格式：每行一个目录，
/// 空行与 `#` 注释跳过。
#[cfg(target_os = "macos")]
fn parse_paths_content(content: &str, paths: &mut Vec<String>) {
    for line in content.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            paths.push(line.to_string());
        }
    }
}

/// 系统级搜索路径（不含用户级目录）。
///
/// macOS 上由四段组成：继承来的 `PATH`、`/etc/paths`、`/etc/paths.d/*`、
/// Homebrew 表。读这些文件是因为从 Finder 启动时本进程 `PATH` 只有 launchd
/// 那几个目录，Homebrew 装的语言服务器（`typescript-language-server`、
/// `gopls`…）全靠它才找得到。其它平台就是继承来的 `PATH`。
#[cfg(target_os = "macos")]
fn build_system_search_path(inherited: Option<&std::ffi::OsStr>) -> String {
    let mut paths: Vec<String> = Vec::with_capacity(16);

    if let Some(inherited) = inherited {
        paths.extend(std::env::split_paths(inherited).map(|p| p.to_string_lossy().into_owned()));
    }

    if let Ok(content) = std::fs::read_to_string(PATHS_FILE) {
        parse_paths_content(&content, &mut paths);
    }

    if let Ok(entries) = std::fs::read_dir(PATHS_D_DIR) {
        for entry in entries.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                parse_paths_content(&content, &mut paths);
            }
        }
    }

    for path in HOMEBREW_PATHS {
        // 已存在则不再追加：`/usr/local/bin` 常常已在 `/etc/paths` 里。
        // 比的是**整段相等**而非包含——子串比较会把 `/usr/local/bin-xcode`
        // 误判成 `/usr/local/bin` 已存在，于是真目录反而被漏掉。
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }

    paths.join(":")
}

#[cfg(not(target_os = "macos"))]
fn build_system_search_path(inherited: Option<&std::ffi::OsStr>) -> String {
    inherited
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".to_string())
}

/// [`build_system_search_path`] 的缓存版：读盘只做一次。
///
/// 终端每次创建、每个语言服务器启动都要用，不能每次都读 `/etc/paths.d`。
pub fn system_search_path() -> &'static str {
    static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

    SYSTEM_PATH.get_or_init(|| build_system_search_path(std::env::var_os("PATH").as_deref()))
}

/// 本地子进程（语言服务器、agent CLI、git）应当使用的完整 `PATH`。
///
/// 返回 `None` 表示取不到主目录（拼不出用户级目录）——调用方此时应保持
/// 继承行为，而不是设一个残缺的 `PATH`。
pub fn local_spawn_path() -> Option<String> {
    let home = dirs::home_dir()?;
    Some(local_spawn_path_for(&home, system_search_path()))
}

/// [`local_spawn_path`] 的可注入版本（`home` 与系统路径都由调用方给）。
///
/// 拆出来是为了让「Finder 启动时 `PATH` 只有 launchd 那几个目录」这个场景
/// 可测：直接改进程的 `HOME` / `PATH` 会与并行跑的其它用例相互污染。
pub fn local_spawn_path_for(home: &Path, system_search_path: &str) -> String {
    let base: Vec<PathBuf> = std::env::split_paths(system_search_path).collect();
    // `path_with_user_bins` 只在含非 UTF-8 路径时失败，那属于极端环境；
    // 退回系统路径仍然比继承 launchd 的最小 `PATH` 好。
    path_with_user_bins(home, &base).unwrap_or_else(|| system_search_path.to_string())
}

#[cfg(test)]
mod tests {
    /// 用户级目录必须**前置**到既有 PATH 上，且既有路径一个都不能丢。
    ///
    /// 前置：同名二进制同时在 `~/.cargo/bin` 和 Homebrew 里时，用户日常在用的
    /// 是前者。
    /// 不丢：Finder 启动时本进程 PATH 只有 launchd 那几个目录，
    /// `system_search_path` 补的 Homebrew 表是唯一能找到 Homebrew 安装的
    /// 语言服务器的线索。
    #[test]
    #[cfg(unix)]
    fn path_with_user_bins_prepends_and_keeps_the_base() {
        let home = Path::new("/home/someone");
        let base: Vec<PathBuf> = vec!["/usr/bin".into(), "/opt/homebrew/bin".into()];
        let path = path_with_user_bins(home, &base).expect("必须能拼出 PATH");
        let dirs: Vec<&str> = path.split(':').collect();

        assert_eq!(dirs[0], "/home/someone/.local/bin", "用户目录必须在前");
        for dir in USER_BIN_DIRS {
            let expected = format!("/home/someone/{dir}");
            assert!(dirs.contains(&expected.as_str()), "缺少 ~/{dir}：{path}");
        }
        for kept in ["/usr/bin", "/opt/homebrew/bin"] {
            assert!(dirs.contains(&kept), "既有路径 {kept} 被丢了：{path}");
        }
    }

    /// 按名字启动的子进程必须真的能在这份 `PATH` 里解析到用户级目录里的
    /// 可执行文件。
    ///
    /// 这条覆盖的正是 LSP 启动失败的机制：`Command::new(name)` 不走 shell，
    /// 靠的是子进程环境里的 `PATH` 查找。用例造的是 POSIX shell 脚本并依赖
    /// 可执行位，故只在 unix 上跑。
    #[test]
    #[cfg(unix)]
    fn path_with_user_bins_lets_command_resolve_a_user_bin_binary() {
        let home =
            std::env::temp_dir().join(format!("deepterm-spawn-path-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = home.join("argv.txt");
        let fake = bin.join("fake-language-server");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > {}\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 刻意把 base 留空：模拟「系统目录里没有这个二进制，只在 ~/.cargo/bin」。
        let path = path_with_user_bins(&home, &[]).expect("必须能拼出 PATH");
        let output = std::process::Command::new("fake-language-server")
            .arg("--stdio")
            .env("PATH", path)
            .output()
            .expect("必须能在自拼的 PATH 里找到用户级目录下的可执行文件");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "--stdio");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Finder 启动的现场：继承来的 `PATH` 只有 launchd 那几个目录，
    /// 拼出来的 `PATH` 仍必须含 `~/.cargo/bin`（rust-analyzer 在那里）
    /// 与 Homebrew 表。
    #[test]
    #[cfg(unix)]
    fn local_spawn_path_covers_user_bins_when_launched_from_finder() {
        let home = Path::new("/home/someone");
        // launchd 给 GUI 程序的就是这几个目录。
        let path = local_spawn_path_for(home, "/usr/bin:/bin:/usr/sbin:/sbin");
        let dirs: Vec<&str> = path.split(':').collect();

        assert!(
            dirs.contains(&"/home/someone/.cargo/bin"),
            "缺 ~/.cargo/bin：rustup 装的 rust-analyzer 在这里，找不到就是\
             「LSP 失败: rust 服务器启动失败」：{path}"
        );
        assert!(dirs.contains(&"/usr/bin"), "继承来的目录不能丢：{path}");
    }

    /// macOS 的系统路径表必须补上 Homebrew，且保留继承来的 `PATH`。
    ///
    /// Homebrew 装的语言服务器（`typescript-language-server`、`gopls`、
    /// `taplo`…）在 Finder 启动时只能靠这张表找到。
    #[test]
    #[cfg(target_os = "macos")]
    fn system_search_path_keeps_inherited_and_adds_homebrew() {
        let path = build_system_search_path(Some(std::ffi::OsStr::new("/usr/bin:/bin")));
        let dirs: Vec<&str> = path.split(':').collect();

        assert!(dirs.contains(&"/usr/bin"), "继承来的 PATH 被丢了：{path}");
        for brew in ["/opt/homebrew/bin", "/usr/local/bin"] {
            assert!(dirs.contains(&brew), "缺 Homebrew 目录 {brew}：{path}");
        }
    }
}

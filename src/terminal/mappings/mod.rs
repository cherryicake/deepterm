pub mod colors;
pub mod keys;
pub mod mouse;

use std::collections::HashMap;

/// 检查系统中是否存在指定 terminfo 条目。
/// 移植自 alacritty_terminal::tty::terminfo_exists。
fn terminfo_exists(name: &str) -> bool {
    let first = name.get(..1).unwrap_or_default();
    let first_hex = format!("{:x}", first.as_bytes()[0]);

    macro_rules! check_path {
        ($path:expr) => {{
            let p = $path;
            if p.join(first).join(name).exists() || p.join(&first_hex).join(name).exists() {
                return true;
            }
        }};
    }

    if let Some(dir) = std::env::var_os("TERMINFO") {
        check_path!(std::path::PathBuf::from(dir));
    }

    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    if let Some(ref home) = home {
        check_path!(home.join(".terminfo"));
    }

    if let Ok(dirs) = std::env::var("TERMINFO_DIRS") {
        for dir in std::env::split_paths(&dirs) {
            check_path!(dir);
        }
    }

    check_path!(std::path::PathBuf::from("/usr/share/terminfo"));
    check_path!(std::path::PathBuf::from("/etc/terminfo"));
    check_path!(std::path::PathBuf::from("/lib/terminfo"));

    false
}

/// Creates the standard terminal environment variables.
/// Used by both local terminals and SSH PTY sessions.
pub fn default_terminal_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("DEEPTERM".to_string(), "true".to_string());
    env.insert("TERM_PROGRAM".to_string(), "deepterm".to_string());
    env.insert(
        "TERM_PROGRAM_VERSION".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    // 如 deepterm terminfo 已安装则使用，否则回退 xterm-256color
    let term = if terminfo_exists("deepterm") {
        "deepterm"
    } else {
        "xterm-256color"
    };
    env.insert("TERM".to_string(), term.to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());

    // kitty 图形能力标记。
    //
    // 大量客户端判定「终端是否支持 kitty 图形协议」时**不发探测**，只查
    // 环境变量/终端名（kitty 自己的 `KITTY_WINDOW_ID`、`TERM` 含 `kitty`、
    // `TERM_PROGRAM` 品牌表……）。这些品牌表里没有 "deepterm"，缺这个标记
    // 时它们会直接判定不支持、关掉图片渲染——即使协议本身实现得完全正确
    // （实测 grok pager 的 `protocol_for_brand(brand)` 即如此）。
    //
    // 选 `KITTY_WINDOW_ID` 而不是改 `TERM`/`TERM_PROGRAM`：后两者是系统级
    // 身份（terminfo 查表、shell integration、遥测都依赖），谎报成
    // `xterm-kitty` 会让缺该 terminfo 的 ncurses 程序直接启动失败；而这个
    // 标记的客户端侧语义基本都是**存在性判断**。
    //
    // 值取进程 id：单值、纯数字、同一进程内所有终端会话一致（kitty 自身
    // 是窗口 id，客户端不关心具体数值）。
    env.insert(
        "KITTY_WINDOW_ID".to_string(),
        std::process::id().to_string(),
    );

    // Set a comprehensive PATH that includes system and Homebrew paths
    // This ensures shell initialization works correctly when launched from Finder
    let path = crate::utils::path::system_search_path().to_string();
    env.insert("PATH".to_string(), path);

    // Set UTF-8 locale to ensure proper Unicode/Chinese character display
    // This fixes garbled text when the terminal is launched from Finder
    env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    env.insert("LC_ALL".to_string(), "en_US.UTF-8".to_string());

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按标记识别 kitty 图形能力的客户端（grok pager、image.nvim、viuer 等）
    /// 只做**存在性判断**，但值必须是纯数字——kitty 自身的窗口 id 是数值，
    /// 个别客户端会真的解析它。
    #[test]
    fn default_env_advertises_kitty_graphics_marker() {
        let env = default_terminal_env();
        let id = env.get("KITTY_WINDOW_ID").expect(
            "缺 KITTY_WINDOW_ID：按环境变量识别 kitty 图形协议的客户端会判定\
             「不支持」并直接关闭图片渲染（协议实现得再正确也不会被调用）",
        );
        assert!(
            id.parse::<u32>().is_ok(),
            "KITTY_WINDOW_ID 应为数值字符串，实际 {id:?}"
        );
    }

    /// 这个标记只补能力位，不顶掉品牌身份：TERM_PROGRAM 仍自称 deepterm。
    #[test]
    fn kitty_marker_does_not_replace_brand_identity() {
        let env = default_terminal_env();
        assert_eq!(
            env.get("TERM_PROGRAM").map(String::as_str),
            Some("deepterm")
        );
        // TERM 也不允许被谎报成 xterm-kitty（多数系统没有这条 terminfo，
        // ncurses 程序会直接启动失败）。
        let term = env.get("TERM").map(String::as_str).unwrap_or_default();
        assert!(!term.contains("kitty"), "TERM 不应谎报成 kitty：{term}");
    }
}

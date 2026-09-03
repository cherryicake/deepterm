//! 安全的外部链接打开工具（中文注释）。
//!
//! ## 背景
//! `open::that(url)` 会按系统默认应用打开任意 Scheme。
//! 若直接打开 Markdown/终端中的链接，克隆仓的 `README.md` 可嵌入
//! `file:///etc/passwd`、自定义 Scheme（`vscode://`、`iterm2://` 等）
//! 触发外部 App，造成信息泄露或命令执行面扩大。
//!
//! ## 策略
//! - 默认白名单仅 `http://` / `https://`（大小写不敏感，前后空白已 trim）。
//! - `file://` 与其他 Scheme 默认拒绝并记 warn；如未来需要支持本地文件预览，
//!   必须走二次确认弹窗，不得直接调用本函数。
//! - 本函数永不 panic，失败仅记日志，调用方可忽略返回值。

/// 判断 URL 是否允许直接用系统默认应用打开。
#[must_use]
pub fn is_safe_to_open(url: &str) -> bool {
    let trimmed = url.trim();
    // 仅允许 http/https，且冒号前不得有空白/控制字符（防 `http ://` 绕过）。
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 安全打开外部链接：白名单外直接拒绝并记日志。
pub fn open_url_safely(url: &str) {
    if is_safe_to_open(url) {
        if let Err(e) = open::that(url.trim()) {
            log::warn!("打开链接失败 '{}': {e}", url.trim());
        }
    } else {
        log::warn!("已拦截非白名单链接（仅允许 http/https）: {url:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_http_https() {
        assert!(is_safe_to_open("http://example.com"));
        assert!(is_safe_to_open("https://example.com/a?b=1"));
        assert!(is_safe_to_open("  HTTPS://example.com  "));
    }

    #[test]
    fn blocks_file_and_custom_schemes() {
        assert!(!is_safe_to_open("file:///etc/passwd"));
        assert!(!is_safe_to_open("vscode://file/etc/passwd"));
        assert!(!is_safe_to_open("iterm2://open"));
        assert!(!is_safe_to_open("javascript:alert(1)"));
        assert!(!is_safe_to_open(""));
        assert!(!is_safe_to_open("http ://evil.com"));
    }
}

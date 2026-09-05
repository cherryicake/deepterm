//! Shell 单引号转义的统一实现。
//!
//! 此前同一段算法在 file_manager::utils、ssh::mod、editor::git_repository
//! 三处各有一份副本。shell 转义直接关系命令注入防线，必须收敛到唯一实现：
//! 修一处即修全部，漏改任何副本都会留下注入面。

/// 转义单引号内的内容（**不含外层单引号**）。
///
/// 调用方必须手写 `'{}'` 包裹，例如：
/// `format!("'{}'", shell_single_quote(path))`。
/// 忘记包裹即构成命令注入，故本函数标记 `#[must_use]` 并在文档中显式警告。
/// 如需直接得到带引号的结果，请用 [`shell_quote_single`]。
#[must_use]
pub fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// 返回带外层单引号的安全 shell 词（推荐新代码使用）。
/// 避免"忘记包裹"类注入。
#[must_use]
pub fn shell_quote_single(value: &str) -> String {
    format!("'{}'", shell_single_quote(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_single_wraps_with_quotes() {
        assert_eq!(shell_quote_single("hello"), "'hello'");
        assert_eq!(shell_quote_single("foo'bar"), "'foo'\\''bar'");
        assert_eq!(shell_quote_single(""), "''");
        assert_eq!(shell_quote_single("hello world"), "'hello world'");
    }

    #[test]
    fn shell_single_quote_escapes_only_inner_quotes() {
        assert_eq!(shell_single_quote("a'b"), "a'\\''b");
        assert_eq!(shell_single_quote("plain"), "plain");
        // 反斜杠在单引号内原样保留，不做转义
        assert_eq!(shell_single_quote("foo\\bar"), "foo\\bar");
        // foo\'bar → 'foo\'\''bar' → shell 原样得到 foo\'bar
        assert_eq!(shell_single_quote("foo\\'bar"), "foo\\'\\''bar");
        assert_eq!(shell_single_quote("it's a test"), "it'\\''s a test");
    }
}

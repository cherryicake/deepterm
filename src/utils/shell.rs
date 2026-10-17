//! Shell 单引号转义与脚本行尾归一化的统一实现。
//!
//! 此前同一段转义算法在 file_manager::utils、ssh::mod、editor::git_repository
//! 三处各有一份副本。shell 转义直接关系命令注入防线，必须收敛到唯一实现：
//! 修一处即修全部，漏改任何副本都会留下注入面。

use std::borrow::Cow;

/// 把 shell 脚本内容归一化为 LF 行尾（dos2unix 语义：CRLF 与孤立 CR 都转为 LF）。
///
/// Windows 上 `core.autocrlf=true` 会把资源文件以 CRLF 检出，`include_str!`
/// 会原样嵌入；脚本写入远端或本地 Un*x shell 后，行尾 `\r` 会参与语法解析，
/// bash/zsh 报 `$'\r': command not found`、`未预期的记号 "$'do\r'"`。
/// `.gitattributes` 固定这些资源以 LF 检出是根治手段，这里再做一次运行期
/// 归一化，保证任意检出状态下脚本都可被 shell 解析。
///
/// 已是 LF 时返回借用的原串，不产生额外分配。
#[must_use]
pub fn normalize_line_endings(content: &str) -> Cow<'_, str> {
    if !content.contains('\r') {
        return Cow::Borrowed(content);
    }
    // 先折叠 CRLF，再把残余的孤立 CR（旧 Mac 行尾/杂散字节）也归一化为 LF
    Cow::Owned(content.replace("\r\n", "\n").replace('\r', "\n"))
}

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

    #[test]
    fn normalize_line_endings_converts_crlf_and_lone_cr() {
        // 回归：Windows（core.autocrlf=true）检出的 shell integration 资源曾带
        // CRLF，被原样上传到远端后 bash 报 `$'\r': command not found`。
        assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb\n");
        // 孤立 CR（旧 Mac 行尾/杂散字节）同样归一化
        assert_eq!(normalize_line_endings("a\rb"), "a\nb");
        // 已是 LF：零拷贝借用，不产生额外分配
        assert!(matches!(
            normalize_line_endings("a\nb"),
            Cow::Borrowed("a\nb")
        ));
    }
}

/// 参数解析错误
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentError {
    /// 缺少必填参数
    MissingRequired,
    /// 参数格式错误
    InvalidFormat { message: String },
}

impl std::fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgumentError::MissingRequired => {
                write!(f, "Missing required argument")
            }
            ArgumentError::InvalidFormat { message } => {
                write!(f, "Invalid argument format: {}", message)
            }
        }
    }
}

impl std::error::Error for ArgumentError {}

/// 解析后的参数
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedArgument {
    /// 原始参数字符串
    pub raw: String,
    /// 按空白分隔的令牌
    pub tokens: Vec<String>,
}

impl ParsedArgument {
    /// 从原始字符串解析参数
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let tokens = Self::tokenize(trimmed);
        Self {
            raw: trimmed.to_string(),
            tokens,
        }
    }

    /// 分词：委托 [`crate::builtin_commands::scp_completion::shell_split_tokens`]，
    /// 与 /scp 补全同一条 shell 语义（双引号/单引号/反斜杠转义），避免两套
    /// 分词器对同一输入给出不同 token 列表。
    fn tokenize(input: &str) -> Vec<String> {
        crate::builtin_commands::scp_completion::shell_split_tokens(input, 0)
            .into_iter()
            .map(|t| t.value)
            .collect()
    }

    /// 检查是否有参数
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// 获取第一个令牌（通常是主路径/目标）
    pub fn first_token(&self) -> Option<&str> {
        self.tokens.first().map(String::as_str)
    }
}

/// 参数解析器
///
/// 根据命令的 `ArgumentType` 定义，解析和校验用户输入。
pub struct ArgumentParser;

impl ArgumentParser {
    /// 从命令行中提取参数部分
    ///
    /// `command_line` 是完整输入（如 "/open-file src/main.rs"）
    /// `command_name` 是命令名（如 "/open-file"）
    pub fn extract_argument<'a>(command_line: &'a str, command_name: &str) -> Option<&'a str> {
        command_line
            .strip_prefix(command_name)
            .map(|rest| rest.trim())
            .filter(|rest| !rest.is_empty())
    }

    /// 解析参数并校验
    pub fn parse(argument: Option<&str>, required: bool) -> Result<ParsedArgument, ArgumentError> {
        match argument {
            Some(raw) => Ok(ParsedArgument::parse(raw)),
            None if required => Err(ArgumentError::MissingRequired),
            None => Ok(ParsedArgument::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_argument() {
        let arg = ParsedArgument::parse("src/main.rs");
        assert_eq!(arg.tokens, vec!["src/main.rs"]);
        assert!(!arg.is_empty());
    }

    #[test]
    fn test_parse_quoted_path() {
        let arg = ParsedArgument::parse("\"/path/with spaces\"");
        assert_eq!(arg.tokens, vec!["/path/with spaces"]);
    }

    #[test]
    fn test_parse_multiple_tokens() {
        let arg = ParsedArgument::parse("file1.txt file2.txt");
        assert_eq!(arg.tokens, vec!["file1.txt", "file2.txt"]);
    }

    #[test]
    fn test_parse_empty() {
        let arg = ParsedArgument::parse("");
        assert!(arg.is_empty());
        assert!(arg.tokens.is_empty());
    }

    #[test]
    fn test_extract_argument() {
        assert_eq!(
            ArgumentParser::extract_argument("/open-file src/main.rs", "/open-file"),
            Some("src/main.rs")
        );
        assert_eq!(ArgumentParser::extract_argument("/open", "/open"), None);
        assert_eq!(ArgumentParser::extract_argument("/unknown", "/open"), None);
    }

    #[test]
    fn test_parse_required_present() {
        let result = ArgumentParser::parse(Some("file.txt"), true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tokens, vec!["file.txt"]);
    }

    #[test]
    fn test_parse_required_missing() {
        let result = ArgumentParser::parse(None, true);
        assert!(matches!(result, Err(ArgumentError::MissingRequired)));
    }

    #[test]
    fn test_parse_optional_missing() {
        let result = ArgumentParser::parse(None, false);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}

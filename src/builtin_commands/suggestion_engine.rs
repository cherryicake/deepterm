use std::sync::Arc;

use crate::builtin_commands::registry::BuiltinCommand;

/// 带评分的命令建议
#[derive(Clone, Debug)]
pub struct ScoredCommand {
    pub command: Arc<BuiltinCommand>,
    pub score: i32,
}

/// 命令建议引擎
///
/// 负责根据用户输入查询，从命令列表中匹配并排序最相关的建议。
/// 这是一个无状态工具结构，所有数据通过参数传入，便于测试和复用。
pub struct SuggestionEngine;

impl SuggestionEngine {
    /// 根据查询生成建议列表，并按匹配分数从高到低排序。
    ///
    /// 评分优先级：
    /// 1. 精确匹配：10,000
    /// 2. 前缀匹配：5,000 + 前缀长度×100 + (100 - 命令名长度)
    /// 3. 名称子串匹配（去掉 '/'）：1,000 + (100 - 命令名长度)
    /// 4. 描述子串匹配：100
    pub fn suggest(commands: &[Arc<BuiltinCommand>], query: &str) -> Vec<ScoredCommand> {
        let command_prefix = query.split_whitespace().next().unwrap_or(query);
        let query_for_substring = command_prefix.strip_prefix('/').unwrap_or(command_prefix);
        let query_lower = query_for_substring.to_lowercase();

        let mut scored: Vec<ScoredCommand> = commands
            .iter()
            .filter_map(|cmd| {
                Self::score(query, command_prefix, &query_lower, cmd).map(|score| ScoredCommand {
                    command: cmd.clone(),
                    score,
                })
            })
            .collect();

        scored.sort_by_key(|b| std::cmp::Reverse(b.score));
        scored
    }

    /// 计算单个命令的匹配分数
    fn score(
        _full_query: &str,
        command_prefix: &str,
        query_lower: &str,
        cmd: &BuiltinCommand,
    ) -> Option<i32> {
        let name = cmd.name;
        let name_lower = name.to_lowercase();
        let name_without_slash = name_lower.strip_prefix('/').unwrap_or(&name_lower);
        let desc_lower = cmd.description.to_lowercase();

        let score = if name == command_prefix {
            // 精确匹配
            10_000
        } else if name.starts_with(command_prefix) {
            // 前缀匹配：匹配前缀越长分数越高，命令名越短分数越高
            let prefix_len = command_prefix.len() as i32;
            let name_len = name.len() as i32;
            5_000 + prefix_len * 100 + (100 - name_len)
        } else if name_without_slash.contains(query_lower) {
            // 名称子串匹配（去掉 '/' 后）
            let name_len = name.len() as i32;
            1_000 + (100 - name_len)
        } else if desc_lower.contains(query_lower) {
            // 描述子串匹配
            100
        } else {
            return None;
        };

        Some(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Icon;
    use crate::builtin_commands::registry::{
        ArgumentType, Availability, BuiltinCommand, CommandHandlerId,
    };

    fn test_cmd(name: &'static str, description: &'static str) -> Arc<BuiltinCommand> {
        Arc::new(BuiltinCommand {
            name,
            description,
            icon: Icon::Folder,
            availability: Availability::Always,
            argument: ArgumentType::None,
            handler_id: CommandHandlerId("test"),
        })
    }

    #[test]
    fn test_exact_match_has_highest_score() {
        let commands = vec![
            test_cmd("/open", "Open file browser"),
            test_cmd("/open-file", "Open a file"),
        ];

        let suggestions = SuggestionEngine::suggest(&commands, "/open");
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].command.name, "/open");
        assert_eq!(suggestions[0].score, 10_000);
    }

    #[test]
    fn test_prefix_match_scores_higher_than_substring() {
        let commands = vec![
            test_cmd("/open", "Open file browser"),
            test_cmd("/open-file", "Open a file"),
        ];

        let suggestions = SuggestionEngine::suggest(&commands, "/op");
        assert_eq!(suggestions[0].command.name, "/open");
        assert!(suggestions[0].score >= 5_000);
        assert!(suggestions[1].score >= 5_000);
    }

    #[test]
    fn test_substring_match_without_slash() {
        let commands = vec![test_cmd("/open", "Open file browser")];

        let suggestions = SuggestionEngine::suggest(&commands, "open");
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].score >= 1_000);
    }

    #[test]
    fn test_description_match() {
        let commands = vec![test_cmd("/open", "Browse files")];

        let suggestions = SuggestionEngine::suggest(&commands, "browse");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].score, 100);
    }

    #[test]
    fn test_no_match_returns_empty() {
        let commands = vec![test_cmd("/open", "Open file browser")];
        let suggestions = SuggestionEngine::suggest(&commands, "xyz");
        assert!(suggestions.is_empty());
    }
}

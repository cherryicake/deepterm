use std::path::{Path, PathBuf};

use crate::assets::file_icon_or_default;

/// 补全项的语义分类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// 本地文件 / 远端文件
    File,
    /// 本地目录 / 远端目录
    Directory,
    /// 已保存主机条目（用于 `/scp` host 前缀阶段）
    Host,
}

/// 路径补全项
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCompletionItem {
    /// 显示名称（文件名/目录名/主机名）
    pub name: String,
    /// 完整路径（Host 条目用空路径占位）
    pub path: PathBuf,
    /// 兼容字段：对 `File/Directory` 分别对应 false/true；对 Host 条目为 false
    pub is_directory: bool,
    /// 条目种类
    pub kind: CompletionKind,
    /// 描述信息（可选，用于 UI 次要文字，比如 host_label / 显示名）
    pub description: Option<String>,
    /// 操作系统图标（Host 条目使用，根据远端 OS 选择对应图标）
    pub icon: Option<crate::assets::Icon>,
    /// 外部补全引擎（如 `/scp`）计算出的"替换文本"
    ///
    /// 若为 `Some`，BuiltinCommandManager 会直接用该文本替换整个 token；
    /// 否则退化为按 `name` / 父目录前缀拼接的传统行为。
    pub replacement: Option<String>,
}

impl PathCompletionItem {
    /// 是否为非交互占位项（"读取远端目录中…" / "无法读取远端目录"）。
    ///
    /// 占位项既没有可替换文本也没有真实路径，绝不能被选中应用——
    /// 否则会把占位文案写进命令行。
    pub fn is_placeholder(&self) -> bool {
        self.replacement.is_none() && self.path.as_os_str().is_empty()
    }
}

/// 文件系统路径补全引擎
///
/// 根据用户输入的路径前缀，提供文件和目录的补全建议。
pub struct PathCompletionEngine;

impl PathCompletionEngine {
    /// 根据路径前缀生成补全建议。
    ///
    /// `prefix` 是用户当前输入的路径（如 "src/main" 或 "/etc/hosts" 或 "~/doc"）。
    /// `cwd` 是当前工作目录，用于解析相对路径。
    ///
    /// 返回按目录优先、字母顺序排序的补全项列表。
    pub fn complete(prefix: &str, cwd: Option<&Path>) -> Vec<PathCompletionItem> {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            // 空前缀：列出当前目录内容
            let dir = cwd.unwrap_or_else(|| Path::new("."));
            return Self::list_directory(dir, "");
        }

        // 解析前缀，分离目录部分和文件名前缀
        let (dir_part, file_prefix) = Self::split_path_prefix(prefix);

        // 解析目录的绝对路径
        let resolved_dir = Self::resolve_directory(&dir_part, cwd);

        match resolved_dir {
            Some(dir) => Self::list_directory(&dir, &file_prefix),
            None => Vec::new(),
        }
    }

    /// 将路径前缀分离为目录部分和文件名前缀。
    ///
    /// 例如：
    /// - "src/main" → ("src/", "main")
    /// - "/etc/hosts" → ("/etc/", "hosts")
    /// - "file" → ("", "file")
    /// - "dir/" → ("dir/", "")
    pub fn split_path_prefix(prefix: &str) -> (String, String) {
        if let Some(last_sep) = prefix.rfind('/') {
            let dir = &prefix[..=last_sep];
            let file = &prefix[last_sep + 1..];
            (dir.to_string(), file.to_string())
        } else {
            ("".to_string(), prefix.to_string())
        }
    }

    /// 解析目录路径，返回其绝对路径。
    ///
    /// 处理以下情况：
    /// - 绝对路径（以 `/` 开头）
    /// - 家目录（以 `~` 开头）
    /// - 相对路径（基于 cwd）
    fn resolve_directory(dir_part: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        if dir_part.is_empty() {
            // 没有目录部分，使用当前目录
            return Some(cwd.unwrap_or_else(|| Path::new(".")).to_path_buf());
        }

        let path = Path::new(dir_part);

        if path.is_absolute() {
            // 绝对路径：直接使用
            Some(path.to_path_buf())
        } else if dir_part.starts_with("~/") || dir_part == "~" {
            // 家目录展开
            dirs::home_dir().map(|home| {
                if dir_part == "~" {
                    home
                } else {
                    home.join(&dir_part[2..])
                }
            })
        } else {
            // 相对路径：基于 cwd 解析
            cwd.map(|cwd| cwd.join(path))
        }
    }

    /// 列出目录内容，过滤匹配文件名前缀的项。
    fn list_directory(dir: &Path, file_prefix: &str) -> Vec<PathCompletionItem> {
        let prefix_lower = file_prefix.to_lowercase();

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        let mut items: Vec<PathCompletionItem> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().into_owned();

                // 隐藏文件：只在用户显式输入 '.' 时显示
                if name.starts_with('.') && !prefix_lower.starts_with('.') {
                    return None;
                }

                // 前缀匹配（不区分大小写）
                if !name.to_lowercase().starts_with(&prefix_lower) {
                    return None;
                }

                let is_directory = entry.file_type().ok()?.is_dir();
                let path = entry.path();
                let icon = if is_directory {
                    None
                } else {
                    Some(file_icon_or_default(&path))
                };

                Some(PathCompletionItem {
                    name,
                    path,
                    is_directory,
                    kind: if is_directory {
                        CompletionKind::Directory
                    } else {
                        CompletionKind::File
                    },
                    description: None,
                    icon,
                    replacement: None,
                })
            })
            .collect();

        // 排序：目录在前，然后按字母顺序
        items.sort_by(|a, b| match (b.is_directory, a.is_directory) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.cmp(&b.name),
        });

        items
    }

    /// 对路径中的空格进行 shell 转义。
    ///
    /// 将空格替换为 `\ `，以便在 shell 中使用。
    pub fn escape_path(path: &str) -> String {
        path.replace(' ', "\\ ")
    }

    /// 对路径中的 shell 转义进行反转义。
    ///
    /// 将 `\ ` 替换为空格。
    pub fn unescape_path(path: &str) -> String {
        path.replace("\\ ", " ")
    }

    /// 解析带行号列号的路径（如 "file.txt:4:2"）。
    ///
    /// 返回 `(路径, 行号, 列号)`。行号和列号可能为 None。
    pub fn parse_line_column(path: &str) -> (String, Option<u32>, Option<u32>) {
        // 先处理转义
        let path = Self::unescape_path(path);

        // 从末尾查找 `:line:col` 或 `:line` 模式
        let mut parts = vec![];
        let mut current = String::new();

        for c in path.chars() {
            if c == ':' {
                if !current.is_empty() {
                    parts.push(current.clone());
                    current.clear();
                }
            } else {
                current.push(c);
            }
        }
        if !current.is_empty() {
            parts.push(current);
        }

        match parts.len() {
            0 => (path, None, None),
            1 => (parts[0].clone(), None, None),
            2 => {
                let line = parts[1].parse::<u32>().ok();
                (parts[0].clone(), line, None)
            }
            3 => {
                let line = parts[parts.len() - 2].parse::<u32>().ok();
                let col = parts.last().unwrap().parse::<u32>().ok();
                // 路径部分是除了最后两个数字部分之外的所有内容
                let file_path = parts[..parts.len() - 2].join(":");
                (file_path, line, col)
            }
            _ => {
                // 多个冒号：最后两个是行号和列号，其余是路径
                let line = parts[parts.len() - 2].parse::<u32>().ok();
                let col = parts.last().unwrap().parse::<u32>().ok();
                let file_path = parts[..parts.len() - 2].join(":");
                (file_path, line, col)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_path_prefix() {
        assert_eq!(
            PathCompletionEngine::split_path_prefix("src/main"),
            ("src/".to_string(), "main".to_string())
        );
        assert_eq!(
            PathCompletionEngine::split_path_prefix("/etc/hosts"),
            ("/etc/".to_string(), "hosts".to_string())
        );
        assert_eq!(
            PathCompletionEngine::split_path_prefix("file"),
            ("".to_string(), "file".to_string())
        );
        assert_eq!(
            PathCompletionEngine::split_path_prefix("dir/"),
            ("dir/".to_string(), "".to_string())
        );
    }

    #[test]
    fn test_escape_unescape() {
        assert_eq!(
            PathCompletionEngine::escape_path("/path/with spaces"),
            "/path/with\\ spaces"
        );
        assert_eq!(
            PathCompletionEngine::unescape_path("/path/with\\ spaces"),
            "/path/with spaces"
        );
    }

    #[test]
    fn test_parse_line_column() {
        let (path, line, col) = PathCompletionEngine::parse_line_column("file.txt:4:2");
        assert_eq!(path, "file.txt");
        assert_eq!(line, Some(4));
        assert_eq!(col, Some(2));

        let (path, line, col) = PathCompletionEngine::parse_line_column("file.txt:10");
        assert_eq!(path, "file.txt");
        assert_eq!(line, Some(10));
        assert_eq!(col, None);

        let (path, line, col) = PathCompletionEngine::parse_line_column("file.txt");
        assert_eq!(path, "file.txt");
        assert_eq!(line, None);
        assert_eq!(col, None);
    }

    #[test]
    fn test_complete_current_dir() {
        // 测试当前目录补全（至少应该有 Cargo.toml）
        let items = PathCompletionEngine::complete("Cargo", Some(Path::new(".")));
        assert!(
            items.iter().any(|i| i.name == "Cargo.toml"),
            "Should find Cargo.toml"
        );
    }

    #[test]
    fn test_complete_with_prefix() {
        // 测试带前缀的补全
        let items = PathCompletionEngine::complete("src/", Some(Path::new(".")));
        assert!(
            items.iter().any(|i| i.name == "main.rs"),
            "Should find src/main.rs"
        );
    }
}

//! 语言服务器的配置表：内置默认 + `settings.json` 覆盖。
//!
//! 内置表只放「主流且命令行接口稳定」的服务器，覆盖本项目
//! [`crate::editor::syntax::languages::Language`] 里有 tree-sitter 语法的语言。
//! 用户可在 `settings.json` 的 `lsp` 段按语言 id 覆盖，或整项禁用。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 一个语言服务器的启动配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 可执行文件名（在 `PATH` 里查找）或绝对路径。
    pub command: String,
    /// 固定参数。多数服务器需要 `--stdio` / `start` 之类的子命令。
    pub args: Vec<String>,
    /// 置 false 时不为该语言启动服务器（覆盖内置默认）。
    pub enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            enabled: true,
        }
    }
}

/// `settings.json` 里的 `lsp` 段。
///
/// `servers` 的键是语言 id（`rust` / `typescript` / `python` …），
/// 与 [`crate::editor::syntax::languages::Language::name`] 一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LspContent {
    /// 全局开关：false 时整个 LSP 子系统不启动任何服务器。
    pub enabled: bool,
    /// 按语言的覆盖项。未列出的语言走内置默认表。
    pub servers: HashMap<String, ServerConfig>,
}

impl Default for LspContent {
    /// 默认**开启**。
    ///
    /// **不能用 `derive(Default)`**：derive 出来的 `enabled` 是 `false`，而
    /// 设置合并是「整字段替换」（`SettingsContent::merge_from`）。用户文档里
    /// 没有 `lsp` 段时，serde 会用这里的 `Default` 填一个全 false 的空段，
    /// 再反过来把 `default_settings.json` 里的 `true` 覆盖掉——表现为开箱
    /// 即用时 LSP 完全不启动。
    ///
    /// 其余设置段（`EditorContent` 等）都用显式 `Default` 与
    /// `default_settings.json` 对齐，这里必须跟随同一约定。
    fn default() -> Self {
        Self {
            enabled: true,
            servers: HashMap::new(),
        }
    }
}

impl LspContent {
    /// 显式构造一个默认开启的配置（语义同 [`Self::default`]）。
    pub fn default_enabled() -> Self {
        Self::default()
    }
}

/// 内置默认服务器表。
///
/// 命令名都是各自生态里 `PATH` 上的约定名称；参数带上是因为不加的话
/// 多数服务器会进入非 stdio 模式或打印帮助后退出。
///
/// **缓存成 `OnceLock`**：这张表是常量，但状态栏现在每帧都要问一次
/// 「当前语言的服务器叫什么」（呼吸点那一帧）。`server_for_language` 在
/// 渲染热路径上，原来的做法是每次调用都重建整张表（19 个 `String` +
/// 若干 `Vec`）——呼吸动画把它从「每帧一点」放大成真正的每帧开销。
fn builtin_servers() -> &'static HashMap<String, ServerConfig> {
    static TABLE: std::sync::OnceLock<HashMap<String, ServerConfig>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        let insert =
            |m: &mut HashMap<String, ServerConfig>, lang: &str, cmd: &str, args: &[&str]| {
                m.insert(
                    lang.to_string(),
                    ServerConfig {
                        command: cmd.to_string(),
                        args: args.iter().map(|s| s.to_string()).collect(),
                        enabled: true,
                    },
                );
            };

        insert(&mut m, "rust", "rust-analyzer", &[]);
        insert(&mut m, "go", "gopls", &[]);
        // pyright 的 stdio 入口是 `pyright-langserver`；`pyright` 本体不带
        // `--stdio`。二者都常见，前者优先。
        insert(&mut m, "python", "pyright-langserver", &["--stdio"]);
        insert(
            &mut m,
            "typescript",
            "typescript-language-server",
            &["--stdio"],
        );
        insert(&mut m, "tsx", "typescript-language-server", &["--stdio"]);
        insert(
            &mut m,
            "javascript",
            "typescript-language-server",
            &["--stdio"],
        );
        insert(&mut m, "c", "clangd", &[]);
        insert(&mut m, "cpp", "clangd", &[]);
        insert(&mut m, "json", "vscode-json-language-server", &["--stdio"]);
        insert(&mut m, "html", "vscode-html-language-server", &["--stdio"]);
        insert(&mut m, "css", "vscode-css-language-server", &["--stdio"]);
        insert(&mut m, "lua", "lua-language-server", &[]);
        insert(&mut m, "zig", "zls", &[]);
        insert(&mut m, "java", "jdtls", &[]);
        insert(&mut m, "bash", "bash-language-server", &["start"]);
        insert(&mut m, "yaml", "yaml-language-server", &["--stdio"]);
        insert(&mut m, "toml", "taplo", &["lsp", "stdio"]);
        insert(&mut m, "kotlin", "kotlin-language-server", &[]);

        m
    })
}

/// 查某语言最终生效的服务器配置。
///
/// 优先级：用户覆盖 > 内置默认 > 无（该语言不支持 LSP）。
/// 用户的 `enabled: false` 会屏蔽内置默认——这是禁用某语言的唯一途径。
pub fn server_for_language(language: &str, settings: Option<&LspContent>) -> Option<ServerConfig> {
    let settings = settings?;
    if !settings.enabled {
        return None;
    }
    if let Some(cfg) = settings.servers.get(language) {
        // 用户显式配了但没填命令 → 视为禁用（而不是退回默认，
        // 否则「想关掉但写了空命令」会莫名其妙又启动）。
        if cfg.command.is_empty() || !cfg.enabled {
            return None;
        }
        return Some(cfg.clone());
    }
    builtin_servers().get(language).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：默认必须**开启**。
    ///
    /// 曾经用 `derive(Default)`，`enabled` 是 `false`；而设置合并是整字段
    /// 替换，用户文档没有 `lsp` 段时 serde 填一个全 false 的空段，把
    /// `default_settings.json` 里的 `true` 覆盖掉——表现为 LSP 完全不启动。
    #[test]
    fn lsp_default_is_enabled() {
        assert!(LspContent::default().enabled);
        assert!(LspContent::default_enabled().enabled);
    }

    /// 回归：空的用户文档合并后不能把默认开关关掉。
    #[test]
    fn merging_empty_user_document_keeps_lsp_enabled() {
        let mut merged = crate::settings::content::SettingsContent::default();
        // 模拟「用户 settings.json 里没有 lsp 段」：serde 用 Default 填充。
        let user: crate::settings::content::SettingsContent = serde_json::from_str("{}").unwrap();
        assert!(user.lsp.enabled, "empty user doc must default to enabled");
        merged.merge_from(&user);
        assert!(merged.lsp.enabled, "merge must not disable lsp");
    }

    #[test]
    fn builtin_table_covers_main_languages() {
        for lang in ["rust", "go", "python", "typescript", "c", "json"] {
            let cfg = server_for_language(
                lang,
                Some(&LspContent {
                    enabled: true,
                    servers: HashMap::new(),
                }),
            );
            assert!(cfg.is_some(), "no builtin server for {lang}");
            assert!(!cfg.unwrap().command.is_empty());
        }
    }

    #[test]
    fn global_switch_disables_everything() {
        assert!(
            server_for_language(
                "rust",
                Some(&LspContent {
                    enabled: false,
                    servers: HashMap::new(),
                })
            )
            .is_none()
        );
    }

    #[test]
    fn user_override_wins() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust".to_string(),
            ServerConfig {
                command: "/opt/my-ra".to_string(),
                args: vec!["--custom".to_string()],
                enabled: true,
            },
        );
        let cfg = server_for_language(
            "rust",
            Some(&LspContent {
                enabled: true,
                servers,
            }),
        )
        .unwrap();
        assert_eq!(cfg.command, "/opt/my-ra");
        assert_eq!(cfg.args, vec!["--custom"]);
    }

    #[test]
    fn user_can_disable_a_builtin_language() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust".to_string(),
            ServerConfig {
                command: "rust-analyzer".to_string(),
                args: vec![],
                enabled: false,
            },
        );
        assert!(
            server_for_language(
                "rust",
                Some(&LspContent {
                    enabled: true,
                    servers,
                })
            )
            .is_none()
        );
    }

    #[test]
    fn empty_command_means_disabled_not_fallback() {
        let mut servers = HashMap::new();
        servers.insert("go".to_string(), ServerConfig::default());
        assert!(
            server_for_language(
                "go",
                Some(&LspContent {
                    enabled: true,
                    servers,
                })
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert!(
            server_for_language(
                "brainfuck",
                Some(&LspContent {
                    enabled: true,
                    servers: HashMap::new(),
                })
            )
            .is_none()
        );
    }

    #[test]
    fn deserializes_partial_settings() {
        // `serde(default)`：用户只写 `enabled` 也能解析。
        let v: LspContent = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!v.enabled);
        assert!(v.servers.is_empty());
    }
}

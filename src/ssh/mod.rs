//! SSH 远程终端模块
//!
//! 提供通过 SSH 协议连接到远程服务器并在本地终端界面中操作远程 shell 的能力。
//!
//! # 主要组件
//!
//! - [`config`]: SSH 连接配置（主机、端口、认证方式等）
//! - [`ssh_pty`]: PTY + ssh 命令方式实现 SSH 终端
//! - [`russh_terminal`]: russh 实现的 SSH 终端（默认传输路径）
//! - [`sftp_fs`]: SFTP 文件系统

pub mod agent;
pub mod auth;
pub mod config;
pub mod host_key;
pub mod keychain;
pub mod platform;
pub mod russh_terminal;
pub mod sftp_fs;
pub mod shared_runtime;
pub mod ssh_pty;

pub use config::{AuthMethod, SshConnectionConfig};
pub use keychain::{KeychainItem, KeychainItemKind, KeychainStore};
pub use russh_terminal::RusshTerminal;
pub use ssh_pty::SshPtyTerminal;


const REMOTE_BASH_INTEGRATION: &str =
    include_str!("../../assets/shell-integration/bash/deepterm.bash");
const REMOTE_BASH_PREEXEC: &str =
    include_str!("../../assets/shell-integration/bash/bash-preexec.sh");
const REMOTE_ZSHENV: &str = include_str!("../../assets/shell-integration/zsh/.zshenv");
const REMOTE_ZSH_INTEGRATION: &str =
    include_str!("../../assets/shell-integration/zsh/deepterm-integration");

fn append_remote_file(script: &mut String, path: &str, marker: &str, content: &str) {
    script.push_str("cat > ");
    script.push_str(path);
    script.push_str(" <<'");
    script.push_str(marker);
    script.push_str("'\n");
    script.push_str(content);
    if !content.ends_with('\n') {
        script.push('\n');
    }
    script.push_str(marker);
    script.push('\n');
}

fn shell_quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn remote_shell_tracking_command(initial_cwd: Option<&str>) -> String {
    let mut script = String::new();
    script.push_str(
        r#"set +e
__deepterm_root="${TMPDIR:-/tmp}/deepterm-shell-integration.$$"
mkdir -p "$__deepterm_root/shell-integration/bash" "$__deepterm_root/shell-integration/zsh" 2>/dev/null
"#,
    );

    append_remote_file(
        &mut script,
        "\"$__deepterm_root/shell-integration/bash/deepterm.bash\"",
        "DEEPTERM_REMOTE_BASH",
        REMOTE_BASH_INTEGRATION,
    );
    append_remote_file(
        &mut script,
        "\"$__deepterm_root/shell-integration/bash/bash-preexec.sh\"",
        "DEEPTERM_REMOTE_BASH_PREEXEC",
        REMOTE_BASH_PREEXEC,
    );
    append_remote_file(
        &mut script,
        "\"$__deepterm_root/shell-integration/zsh/.zshenv\"",
        "DEEPTERM_REMOTE_ZSHENV",
        REMOTE_ZSHENV,
    );
    append_remote_file(
        &mut script,
        "\"$__deepterm_root/shell-integration/zsh/deepterm-integration\"",
        "DEEPTERM_REMOTE_ZSH_INTEGRATION",
        REMOTE_ZSH_INTEGRATION,
    );

    script.push_str(
        r#"
chmod 600 "$__deepterm_root/shell-integration/bash/deepterm.bash" \
  "$__deepterm_root/shell-integration/bash/bash-preexec.sh" \
  "$__deepterm_root/shell-integration/zsh/.zshenv" \
  "$__deepterm_root/shell-integration/zsh/deepterm-integration" 2>/dev/null

__deepterm_os_id=""
if [ -r /etc/os-release ]; then
  __deepterm_os_id=$(sed -n 's/^ID=//p' /etc/os-release | head -n 1 | tr -d '"')
else
  __deepterm_os_id=$(uname -s 2>/dev/null | tr '[:upper:]' '[:lower:]')
fi
printf '\033]1338;%s\007' "$__deepterm_os_id"

"#,
    );

    if let Some(cwd) = initial_cwd.filter(|cwd| !cwd.trim().is_empty()) {
        // 远程恢复 cwd 发生在 shell 启动前，不写入 PTY 输入流，因此用户不会
        // 在终端里看到 `cd ...`。这等价于给远程 shell 设置启动目录；若目录
        // 不存在或无权限，忽略失败并回到远程默认目录。
        script.push_str("__deepterm_initial_cwd=");
        script.push_str(&shell_quote_single(cwd.trim()));
        script.push_str(
            r#"
cd "$__deepterm_initial_cwd" 2>/dev/null || true
unset __deepterm_initial_cwd

"#,
        );
    }

    script.push_str(
        r#"
__deepterm_shell="${SHELL:-/bin/sh}"
__deepterm_shell_name="${__deepterm_shell##*/}"
case "$__deepterm_shell_name" in
  zsh)
    if [ -n "${ZDOTDIR+x}" ]; then
      export DEEPTERM_ZSH_ZDOTDIR="$ZDOTDIR"
    fi
    export DEEPTERM_RESOURCES_DIR="$__deepterm_root"
    export ZDOTDIR="$__deepterm_root/shell-integration/zsh"
    exec "$__deepterm_shell" -i
    ;;
  bash)
    if [ -n "${ENV+x}" ]; then
      export DEEPTERM_BASH_ENV="$ENV"
    fi
    export DEEPTERM_RESOURCES_DIR="$__deepterm_root"
    export ENV="$__deepterm_root/shell-integration/bash/deepterm.bash"
    export DEEPTERM_BASH_INJECT=1
    if [ -z "${HISTFILE+x}" ] && [ -n "$HOME" ]; then
      export HISTFILE="$HOME/.bash_history"
      export DEEPTERM_BASH_UNEXPORT_HISTFILE=1
    fi
    exec "$__deepterm_shell" --posix -i
    ;;
  *)
    exec "$__deepterm_shell" -i
    ;;
esac
"#,
    );

    script
}

/// 支持的 SSH 密钥类型
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyType {
    /// RSA 密钥
    Rsa,
    /// ECDSA 密钥
    Ecdsa,
    /// Ed25519 密钥
    Ed25519,
    /// DSA 密钥（已不推荐）
    Dss,
}

impl KeyType {
    /// 从密钥文件名推断密钥类型
    pub fn from_filename(filename: &str) -> Option<Self> {
        let filename = filename.to_lowercase();
        if filename.contains("ed25519") {
            Some(KeyType::Ed25519)
        } else if filename.contains("ecdsa") {
            Some(KeyType::Ecdsa)
        } else if filename.contains("rsa") {
            Some(KeyType::Rsa)
        } else if filename.contains("dsa") || filename.contains("dss") {
            Some(KeyType::Dss)
        } else {
            None
        }
    }

    /// 获取默认的密钥文件名
    pub fn default_filename(&self) -> &'static str {
        match self {
            KeyType::Rsa => "id_rsa",
            KeyType::Ecdsa => "id_ecdsa",
            KeyType::Ed25519 => "id_ed25519",
            KeyType::Dss => "id_dsa",
        }
    }
}

/// 查找默认的 SSH 密钥
///
/// 在 `~/.ssh` 目录下查找常见的密钥文件
pub fn find_default_ssh_keys() -> Vec<std::path::PathBuf> {
    let mut keys = Vec::with_capacity(4);

    if let Some(home_dir) = dirs::home_dir() {
        let ssh_dir = home_dir.join(".ssh");
        if ssh_dir.exists() {
            let key_names = ["id_ed25519", "id_rsa", "id_ecdsa", "id_dsa"];
            for name in &key_names {
                let key_path = ssh_dir.join(name);
                if key_path.exists() {
                    keys.push(key_path);
                }
            }
        }
    }

    keys
}

/// Get the current system username
///
/// Tries the `USER` environment variable first, then `USERNAME` (for Windows).
/// Returns `None` if neither is set.
pub fn get_current_username() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
}

/// 解析 SSH 连接字符串
///
/// 支持格式:
/// - `user@host`
/// - `user@host:port`
/// - `host`（使用当前用户名）
///
/// # 示例
///
/// ```
/// use deepterm::ssh::parse_ssh_target;
///
/// let (user, host, port) = parse_ssh_target("admin@192.168.1.100:2222");
/// assert_eq!(user, "admin");
/// assert_eq!(host, "192.168.1.100");
/// assert_eq!(port, 2222);
/// ```
pub fn parse_ssh_target(target: &str) -> (String, String, u16) {
    let target = target.trim();

    // 解析 user@host 部分和 port 部分
    let (user_host, port) = if let Some(colon_pos) = target.rfind(':') {
        // 检查是否是 IPv6 地址的结尾
        let before_colon = &target[..colon_pos];
        let after_colon = &target[colon_pos + 1..];

        // 如果冒号后面是数字，则认为是端口
        if after_colon.chars().all(|c| c.is_ascii_digit()) {
            let port = after_colon.parse::<u16>().unwrap_or(22);
            (before_colon, port)
        } else {
            (target, 22)
        }
    } else {
        (target, 22)
    };

    // 解析 user@host
    let (user, host) = if let Some(at_pos) = user_host.rfind('@') {
        let user = user_host[..at_pos].to_string();
        let host = user_host[at_pos + 1..].to_string();
        (user, host)
    } else {
        // 没有用户名，使用当前系统用户名
        let user = get_current_username().unwrap_or_else(|| "root".to_string());
        (user, user_host.to_string())
    };

    (user, host, port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_ssh_target_full() {
        let (user, host, port) = parse_ssh_target("admin@192.168.1.100:2222");
        assert_eq!(user, "admin");
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 2222);
    }

    #[test]
    fn test_parse_ssh_target_no_port() {
        let (user, host, port) = parse_ssh_target("admin@example.com");
        assert_eq!(user, "admin");
        assert_eq!(host, "example.com");
        assert_eq!(port, 22);
    }

    #[test]
    fn test_parse_ssh_target_host_only() {
        let (_, host, port) = parse_ssh_target("example.com");
        // 使用当前系统用户名
        assert!(!host.is_empty());
        assert_eq!(port, 22);
    }

    #[test]
    fn test_key_type_from_filename() {
        assert_eq!(KeyType::from_filename("id_rsa"), Some(KeyType::Rsa));
        assert_eq!(KeyType::from_filename("id_ed25519"), Some(KeyType::Ed25519));
        assert_eq!(KeyType::from_filename("id_ecdsa"), Some(KeyType::Ecdsa));
        assert_eq!(KeyType::from_filename("id_dsa"), Some(KeyType::Dss));
        assert_eq!(KeyType::from_filename("unknown"), None);
    }

    #[test]
    fn remote_shell_tracking_command_uses_ghostty_style_resources() {
        let command = remote_shell_tracking_command(Some("/tmp/deepterm cwd"));

        assert!(command.contains("shell-integration/bash/deepterm.bash"));
        assert!(command.contains("shell-integration/zsh/.zshenv"));
        assert!(command.contains("DEEPTERM_BASH_INJECT"));
        assert!(command.contains("DEEPTERM_ZSH_ZDOTDIR"));
        assert!(command.contains("kitty-shell-cwd://"));
        assert!(command.contains("__deepterm_initial_cwd='/tmp/deepterm cwd'"));
        assert!(!command.contains("__deepterm_precmd(){"));
        assert!(!command.contains("add-zsh-hook precmd __deepterm_precmd"));
    }

    #[test]
    fn remote_shell_tracking_command_is_posix_shell_parseable() {
        let command = remote_shell_tracking_command(Some("/tmp/deepterm cwd"));
        let path = std::env::temp_dir().join(format!(
            "deepterm-remote-bootstrap-{}.sh",
            std::process::id()
        ));

        {
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(command.as_bytes()).unwrap();
        }

        let output = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

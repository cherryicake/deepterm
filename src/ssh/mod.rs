//! SSH 远程终端模块
//!
//! 提供通过 SSH 协议连接到远程服务器并在本地终端界面中操作远程 shell 的能力。
//!
//! # 主要组件
//!
//! - [`config`]: SSH 连接配置（主机、端口、认证方式等）
//! - [`connection_manager`]: 管理所有 SSH 连接的生命周期
//! - [`ssh_pty`]: PTY + ssh 命令方式实现 SSH 终端
//!
//! # 使用示例
//!
//! ```rust,ignore
//! use ssh::{SshConnectionConfig, AuthMethod, SshConnectionManager};
//!
//! let config = SshConnectionConfig {
//!     id: "server-1".to_string(),
//!     host: "192.168.1.100".to_string(),
//!     port: 22,
//!     username: "admin".to_string(),
//!     auth_method: AuthMethod::Password { password: "secret".to_string() },
//!     nickname: Some("My Server".to_string()),
//! };
//!
//! let manager = SshConnectionManager::new();
//! manager.connect(config).await?;
//! ```

pub mod config;
pub mod connection_manager;
pub mod error;
pub mod ssh_pty;

pub use config::{AuthMethod, SshConnectionConfig};
pub use connection_manager::{ConnectionState, SshConnectionManager};
pub use error::{SshError, SshResult};
pub use ssh_pty::SshPtyTerminal;

use std::sync::Arc;
use parking_lot::Mutex;

/// SSH 客户端配置
#[derive(Clone, Debug)]
pub struct SshClientConfig {
    /// 连接超时时间（秒）
    pub connection_timeout: u64,
    /// 是否启用严格主机密钥检查
    pub strict_host_key_checking: bool,
    /// 密钥交换算法偏好
    pub kex_algorithms: Option<Vec<String>>,
    /// 加密算法偏好
    pub cipher_algorithms: Option<Vec<String>>,
}

impl Default for SshClientConfig {
    fn default() -> Self {
        Self {
            connection_timeout: 30,
            strict_host_key_checking: true,
            kex_algorithms: None,
            cipher_algorithms: None,
        }
    }
}

/// 全局 SSH 状态管理
///
/// 用于在应用级别共享 SSH 连接管理器
#[derive(Clone)]
pub struct SshGlobalState {
    /// 连接管理器
    manager: Arc<Mutex<SshConnectionManager>>,
    /// 客户端配置
    client_config: Arc<SshClientConfig>,
}

impl SshGlobalState {
    /// 创建新的 SSH 全局状态
    pub fn new() -> Self {
        Self {
            manager: Arc::new(Mutex::new(SshConnectionManager::new())),
            client_config: Arc::new(SshClientConfig::default()),
        }
    }

    /// 获取连接管理器的引用
    pub fn manager(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, SshConnectionManager>> {
        // Note: parking_lot::Mutex doesn't return a Result, it returns the guard directly
        // We keep this signature for compatibility with standard Mutex API
        panic!("Use manager_lock() for parking_lot Mutex")
    }

    /// 获取连接管理器的锁（parking_lot 版本）
    pub fn manager_lock(&self) -> parking_lot::MutexGuard<'_, SshConnectionManager> {
        self.manager.lock()
    }

    /// 获取客户端配置
    pub fn client_config(&self) -> &SshClientConfig {
        &self.client_config
    }

    /// 更新客户端配置
    pub fn set_client_config(&mut self, config: SshClientConfig) {
        self.client_config = Arc::new(config);
    }
}

impl Default for SshGlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// 连接状态变更事件
#[derive(Clone, Debug)]
pub enum ConnectionEvent {
    /// 连接已建立
    Connected { connection_id: String },
    /// 连接已断开
    Disconnected { connection_id: String, reason: Option<String> },
    /// 连接失败
    ConnectionFailed { connection_id: String, error: String },
    /// 正在连接
    Connecting { connection_id: String },
    /// 正在重连
    Reconnecting { connection_id: String, attempt: u32 },
}

/// 生成唯一的连接 ID
pub fn generate_connection_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("ssh-{}-{}", timestamp, rand::random::<u16>())
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
    let mut keys = Vec::new();

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
}

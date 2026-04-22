//! SSH 连接配置定义
//!
//! 提供 SSH 连接所需的配置结构和认证方式定义。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ssh::keychain::{KeychainStore, migrate_auth_method};
use crate::ssh::parse_ssh_target;

fn generate_id(username: &str, host: &str, port: u16, nickname: Option<&str>) -> String {
    if let Some(nick) = nickname {
        format!("{}@{}:{}:{}", username, host, port, nick)
    } else {
        format!("{}@{}:{}", username, host, port)
    }
}

/// SSH 连接配置
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SshConnectionConfig {
    /// 主机地址（IP 或域名）
    pub host: String,
    /// 端口号（默认 22）
    #[serde(default = "default_port")]
    pub port: u16,
    /// 用户名
    pub username: String,
    /// 认证方式
    #[serde(flatten)]
    pub auth_method: AuthMethod,
    /// 可选的昵称（用于显示）
    pub nickname: Option<String>,
    /// 额外的 SSH 参数
    pub extra_args: Option<Vec<String>>,
    /// 连接超时（秒，默认 30）
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// 是否保存此配置
    #[serde(default = "default_save")]
    pub save_config: bool,
    /// 检测到的远程操作系统 ID
    #[serde(default)]
    pub os: Option<String>,
}

impl SshConnectionConfig {
    /// 获取连接唯一标识符（基于用户名、主机、端口和昵称生成）
    pub fn id(&self) -> String {
        generate_id(
            &self.username,
            &self.host,
            self.port,
            self.nickname.as_deref(),
        )
    }
}

fn default_port() -> u16 {
    22
}

fn default_timeout() -> u64 {
    30
}

fn default_save() -> bool {
    false
}

impl SshConnectionConfig {
    /// 创建新的连接配置
    pub fn new(
        host: impl Into<String>,
        username: impl Into<String>,
        auth_method: AuthMethod,
    ) -> Self {
        let host = host.into();
        let username = username.into();

        Self {
            host,
            port: default_port(),
            username,
            auth_method,
            nickname: None,
            extra_args: None,
            timeout: default_timeout(),
            save_config: default_save(),
            os: None,
        }
    }

    /// 从连接字符串创建配置
    ///
    /// 支持格式：
    /// - `user@host`
    /// - `user@host:port`
    /// - `host`（使用当前用户名）
    pub fn from_string(target: &str, auth_method: AuthMethod) -> Self {
        let (username, host, port) = parse_ssh_target(target);

        Self {
            host,
            port,
            username,
            auth_method,
            nickname: None,
            extra_args: None,
            timeout: default_timeout(),
            save_config: default_save(),
            os: None,
        }
    }

    /// 设置端口号
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// 设置昵称
    pub fn with_nickname(mut self, nickname: impl Into<String>) -> Self {
        self.nickname = Some(nickname.into());
        self
    }

    /// 设置是否保存配置
    pub fn with_save(mut self, save: bool) -> Self {
        self.save_config = save;
        self
    }

    /// 设置超时时间
    pub fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }

    /// 获取显示名称
    pub fn display_name(&self) -> String {
        self.nickname
            .clone()
            .unwrap_or_else(|| format!("{}@{}", self.username, self.host))
    }

    /// 获取主机标签（用于 Tab 标题和连接视图）
    /// 格式：`username@host`，非标准端口时为 `username@host:port`
    pub fn host_label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }

    /// 获取完整的连接标识符
    pub fn connection_string(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    /// 创建简单密码认证配置
    pub fn with_password(host: &str, username: &str, password: &str) -> Self {
        Self::new(
            host,
            username,
            AuthMethod::Password {
                password: password.to_string(),
            },
        )
    }

    /// 创建密钥认证配置
    pub fn with_key(host: &str, username: &str, key_path: PathBuf) -> Self {
        Self::new(
            host,
            username,
            AuthMethod::PrivateKey {
                path: key_path,
                passphrase: None,
            },
        )
    }
}

/// 认证方式
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "auth_type", content = "auth_data")]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// 密码认证（引用 Keychain 项）
    PasswordRef { keychain_id: String },
    /// 私钥认证（引用 Keychain 项）
    PrivateKeyRef { keychain_id: String },
    /// 密码认证（旧版内联存储，加载时迁移）
    Password { password: String },
    /// 私钥认证（旧版内联存储，加载时迁移）
    PrivateKey {
        /// 密钥文件路径
        path: PathBuf,
        /// 密钥密码（passphrase）
        passphrase: Option<String>,
    },
    /// SSH Agent 认证
    Agent,
    /// 尝试多种方法
    #[serde(skip)]
    Auto,
}

impl AuthMethod {
    /// 获取认证方式的显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            AuthMethod::PasswordRef { .. } | AuthMethod::Password { .. } => "密码",
            AuthMethod::PrivateKeyRef { .. } | AuthMethod::PrivateKey { .. } => "密钥",
            AuthMethod::Agent => "SSH Agent",
            AuthMethod::Auto => "自动",
        }
    }

    /// 是否是密码认证
    pub fn is_password(&self) -> bool {
        matches!(
            self,
            AuthMethod::PasswordRef { .. } | AuthMethod::Password { .. }
        )
    }

    /// 是否是密钥认证
    pub fn is_key(&self) -> bool {
        matches!(
            self,
            AuthMethod::PrivateKeyRef { .. } | AuthMethod::PrivateKey { .. }
        )
    }

    /// 是否是 Agent 认证
    pub fn is_agent(&self) -> bool {
        matches!(self, AuthMethod::Agent)
    }

    /// 返回引用的 keychain id
    pub fn keychain_id(&self) -> Option<&str> {
        match self {
            AuthMethod::PasswordRef { keychain_id } | AuthMethod::PrivateKeyRef { keychain_id } => {
                Some(keychain_id.as_str())
            }
            _ => None,
        }
    }

    /// 创建密码认证（旧版内联配置，用于兼容测试和迁移）
    pub fn password(password: impl Into<String>) -> Self {
        AuthMethod::Password {
            password: password.into(),
        }
    }

    /// 创建密码引用认证
    pub fn password_ref(keychain_id: impl Into<String>) -> Self {
        AuthMethod::PasswordRef {
            keychain_id: keychain_id.into(),
        }
    }

    /// 创建密钥认证（旧版内联配置，用于兼容测试和迁移）
    pub fn key(path: impl Into<PathBuf>) -> Self {
        AuthMethod::PrivateKey {
            path: path.into(),
            passphrase: None,
        }
    }

    /// 创建密钥认证（有密码）
    pub fn key_with_passphrase(path: impl Into<PathBuf>, passphrase: impl Into<String>) -> Self {
        AuthMethod::PrivateKey {
            path: path.into(),
            passphrase: Some(passphrase.into()),
        }
    }

    /// 创建密钥引用认证
    pub fn key_ref(keychain_id: impl Into<String>) -> Self {
        AuthMethod::PrivateKeyRef {
            keychain_id: keychain_id.into(),
        }
    }
}

impl Default for AuthMethod {
    fn default() -> Self {
        AuthMethod::Auto
    }
}

/// SSH 已知主机配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownHost {
    /// 主机地址
    pub host: String,
    /// 端口号
    #[serde(default = "default_port")]
    pub port: u16,
    /// 主机密钥指纹
    pub fingerprint: String,
    /// 密钥类型
    pub key_type: String,
    /// 是否已验证
    #[serde(default)]
    pub verified: bool,
}

impl KnownHost {
    /// 创建新的已知主机记录
    pub fn new(
        host: impl Into<String>,
        fingerprint: impl Into<String>,
        key_type: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port: default_port(),
            fingerprint: fingerprint.into(),
            key_type: key_type.into(),
            verified: false,
        }
    }

    /// 匹配主机
    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.host == host && self.port == port
    }

    /// 验证指纹
    pub fn verify_fingerprint(&self, fingerprint: &str) -> bool {
        self.fingerprint == fingerprint
    }
}

/// SSH 全局配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshGlobalConfig {
    /// 默认连接超时
    #[serde(default = "default_timeout")]
    pub default_timeout: u64,
    /// 是否启用严格主机密钥检查
    #[serde(default = "default_true")]
    pub strict_host_key_checking: bool,
    /// 默认私钥路径列表
    #[serde(default = "default_identity_files")]
    pub default_identity_files: Vec<PathBuf>,
    /// 已保存的连接配置
    #[serde(default)]
    pub saved_connections: Vec<SshConnectionConfig>,
    /// 已知主机列表
    #[serde(default)]
    pub known_hosts: Vec<KnownHost>,
    /// 是否启用 SSH Agent
    #[serde(default = "default_true")]
    pub use_agent: bool,
}

fn default_true() -> bool {
    true
}

fn default_identity_files() -> Vec<PathBuf> {
    if let Some(home_dir) = dirs::home_dir() {
        let ssh_dir = home_dir.join(".ssh");
        vec![
            ssh_dir.join("id_ed25519"),
            ssh_dir.join("id_rsa"),
            ssh_dir.join("id_ecdsa"),
        ]
    } else {
        Vec::new()
    }
}

impl SshGlobalConfig {
    /// 创建默认配置
    pub fn new() -> Self {
        Self {
            default_timeout: default_timeout(),
            strict_host_key_checking: true,
            default_identity_files: default_identity_files(),
            saved_connections: Vec::new(),
            known_hosts: Vec::new(),
            use_agent: true,
        }
    }

    /// 添加保存的连接
    pub fn add_connection(&mut self, config: SshConnectionConfig) {
        self.saved_connections.push(config);
    }

    /// 移除保存的连接
    pub fn remove_connection(&mut self, id: &str) -> Option<SshConnectionConfig> {
        let pos = self.saved_connections.iter().position(|c| c.id() == id)?;
        Some(self.saved_connections.remove(pos))
    }

    /// 查找连接配置
    pub fn find_connection(&self, id: &str) -> Option<&SshConnectionConfig> {
        self.saved_connections.iter().find(|c| c.id() == id)
    }

    /// 查找已知主机
    pub fn find_known_host(&self, host: &str, port: u16) -> Option<&KnownHost> {
        self.known_hosts.iter().find(|h| h.matches(host, port))
    }

    /// 添加或更新已知主机
    pub fn add_known_host(&mut self, host: KnownHost) {
        self.known_hosts
            .retain(|h| !(h.host == host.host && h.port == host.port));
        self.known_hosts.push(host);
    }

    /// 获取保存的连接数量
    pub fn saved_connections_count(&self) -> usize {
        self.saved_connections.len()
    }

    /// 从配置文件加载
    /// 配置文件路径: ~/.config/deepterm/ssh.json
    /// 如果文件不存在或解析失败，返回默认配置（不自动创建文件）
    pub fn load() -> Self {
        let config_path = match Self::config_path() {
            Some(path) => path,
            None => return Self::new(),
        };

        let content = match std::fs::read_to_string(&config_path) {
            Ok(content) => content,
            Err(_) => return Self::new(),
        };

        let mut config = serde_json::from_str(&content)
            .ok()
            .unwrap_or_else(Self::new);
        if let Err(err) = config.migrate_keychain_refs() {
            log::warn!("SSH 凭据迁移失败: {}", err);
        }
        config
    }

    /// 加载配置，如果不存在则创建默认配置文件
    pub fn load_or_create() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path().ok_or("Failed to get config path")?;

        let mut config = match std::fs::read_to_string(&config_path) {
            Ok(content) => serde_json::from_str(&content)?,
            Err(_) => {
                let default_config = Self::new();
                default_config.save()?;
                default_config
            }
        };

        config
            .migrate_keychain_refs()
            .map_err(|err| -> Box<dyn std::error::Error> { err.to_string().into() })?;
        Ok(config)
    }

    /// 保存配置到文件
    /// 配置文件路径: ~/.config/deepterm/ssh.json
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_path().ok_or("Failed to get config path")?;

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }

    /// 获取配置文件路径
    fn config_path() -> Option<std::path::PathBuf> {
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("deepterm").join("ssh.json"))
    }

    fn migrate_keychain_refs(&mut self) -> anyhow::Result<bool> {
        let mut store = KeychainStore::load_or_default();
        let mut changed = false;

        for connection in &mut self.saved_connections {
            changed |= migrate_auth_method(connection, &mut store)?;
        }

        if changed {
            self.save()
                .map_err(|err| anyhow::anyhow!("保存迁移后的 SSH 配置失败: {}", err))?;
        }

        Ok(changed)
    }
}

impl Default for SshGlobalConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_connection_config() {
        let config =
            SshConnectionConfig::new("example.com", "admin", AuthMethod::password("secret"));

        assert_eq!(config.host, "example.com");
        assert_eq!(config.username, "admin");
        assert_eq!(config.port, 22);
        assert!(config.auth_method.is_password());
        assert_eq!(config.display_name(), "admin@example.com");
    }

    #[test]
    fn test_ssh_connection_with_nickname() {
        let config =
            SshConnectionConfig::new("192.168.1.100", "root", AuthMethod::key("~/.ssh/id_rsa"))
                .with_nickname("My Server");

        assert_eq!(config.display_name(), "My Server");
    }

    #[test]
    fn test_auth_method_display_name() {
        assert_eq!(AuthMethod::password("test").display_name(), "密码");
        assert_eq!(AuthMethod::key("~/.ssh/id_rsa").display_name(), "密钥");
        assert_eq!(AuthMethod::Agent.display_name(), "SSH Agent");
    }

    #[test]
    fn test_known_host_matching() {
        let host = KnownHost::new("example.com", "SHA256:abc123", "ssh-ed25519");

        assert!(host.matches("example.com", 22));
        assert!(!host.matches("example.com", 2222));
        assert!(!host.matches("other.com", 22));
    }

    #[test]
    fn test_global_config_add_remove_connection() {
        let mut config = SshGlobalConfig::new();

        let conn = SshConnectionConfig::new("host1", "user1", AuthMethod::Agent);
        config.add_connection(conn.clone());

        assert_eq!(config.saved_connections_count(), 1);
        assert!(config.find_connection(&conn.id()).is_some());

        config.remove_connection(&conn.id());
        assert_eq!(config.saved_connections_count(), 0);
    }
}

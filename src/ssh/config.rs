//! SSH 连接配置定义
//!
//! 提供 SSH 连接所需的配置结构和认证方式定义。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ssh::keychain::{KeychainStore, migrate_auth_method};

fn generate_id(username: &str, host: &str, port: u16, nickname: Option<&str>) -> String {
    if let Some(nick) = nickname {
        format!("{}@{}:{}:{}", username, host, port, nick)
    } else {
        format!("{}@{}:{}", username, host, port)
    }
}

/// 文件夹排序方式
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SortMode {
    #[default]
    NameAsc,
    NameDesc,
    CreatedDesc,
    CreatedAsc,
    CountDesc,
    Custom,
}

impl SortMode {
    pub fn display_name(&self) -> &'static str {
        match self {
            SortMode::NameAsc => "名称 A → Z",
            SortMode::NameDesc => "名称 Z → A",
            SortMode::CreatedDesc => "创建时间 新→旧",
            SortMode::CreatedAsc => "创建时间 旧→新",
            SortMode::CountDesc => "主机数 多→少",
            SortMode::Custom => "自定义排序",
        }
    }
}

fn validate_group_name(
    name: &str,
    existing: &[HostGroup],
    exclude_id: Option<&str>,
) -> Result<String, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("文件夹名称不能为空".to_string());
    }
    if trimmed.chars().count() > 20 {
        return Err("文件夹名称不能超过20字符".to_string());
    }
    let is_duplicate = existing.iter().any(|g| {
        if let Some(exclude) = exclude_id
            && g.id == exclude
        {
            return false;
        }
        g.name.eq_ignore_ascii_case(&trimmed)
    });
    if is_duplicate {
        return Err(format!("文件夹 '{}' 已存在", trimmed));
    }
    Ok(trimmed)
}

fn cmp_name_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Host 文件夹分组
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub sort_order: u32,
    pub created_at: DateTime<Utc>,
    /// 兼容旧折叠字段，现不使用但保留以避免反序列化失败
    #[serde(default)]
    pub collapsed: bool,
}

impl HostGroup {
    pub fn new(name: impl Into<String>, sort_order: u32) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            sort_order,
            created_at: Utc::now(),
            collapsed: false,
        }
    }
}

/// SSH 连接配置
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// 端口转发：把本机服务经 SSH 暴露到远端（`ssh -R` 语义），
    /// 连接该主机时逐条建立。见 [`crate::ssh::port_forward`]。
    #[serde(default)]
    pub port_forwards: Vec<crate::ssh::port_forward::PortForwardRule>,
    /// 所属文件夹分组 id，None 表示未分组
    #[serde(default)]
    pub group_id: Option<String>,
    /// 组内排序
    #[serde(default)]
    pub sort_order: Option<u32>,
    /// 创建时间
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    /// 本次连接是否经系统代理（见 [`crate::ssh::system_proxy`]）。
    ///
    /// **只存在于内存**：Host 菜单的「使用系统代理连接」是对这一次连接的指定，
    /// 不是主机的持久属性，因此 `serde(skip)` 不落盘——写进 `ssh.json` 会让
    /// 「用代理连过一次」变成这台主机的永久设置。
    ///
    /// 代价与对策：
    /// - Tab 菜单「重新连接」与「复制标签页」走内存，标记天然保留；
    /// - 退出重启走 `session_state.json`，那里的 `TerminalSessionState::Ssh`
    ///   自己存了一份并在此合回（见 `TerminalSessionState::ssh_config_with_proxy`）。
    #[serde(skip)]
    pub use_system_proxy: bool,
}

impl PartialEq for SshConnectionConfig {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.auth_method == other.auth_method
            && self.nickname == other.nickname
            && self.extra_args == other.extra_args
            && self.timeout == other.timeout
            && self.save_config == other.save_config
            && self.port_forwards == other.port_forwards
            && self.os == other.os
            && self.group_id == other.group_id
            && self.sort_order == other.sort_order
        // `use_system_proxy` 有意不参与：它是本次连接的传输选择，不改变主机身份。
        // 文件树后端匹配（`FileEndpoint::matches_fs`）与远端缓存键都拿这个相等性
        // 判断「是不是同一台机器」，把代理标记算进去会让「经代理连上」的终端
        // 与它自己的文件树判为不同来源。
        // created_at is metadata and intentionally excluded from equality
    }
}

impl Eq for SshConnectionConfig {}

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

    pub fn with_group_id(mut self, group_id: Option<String>) -> Self {
        self.group_id = group_id;
        self
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
            port_forwards: Vec::new(),
            group_id: None,
            sort_order: None,
            created_at: Utc::now(),
            use_system_proxy: false,
        }
    }

    /// 本地文件系统占位配置，用于共享缓存 key
    pub fn local() -> Self {
        Self {
            host: "localhost".into(),
            port: 0,
            username: "localhost".into(),
            auth_method: AuthMethod::Agent,
            nickname: None,
            extra_args: None,
            timeout: default_timeout(),
            save_config: false,
            os: None,
            port_forwards: Vec::new(),
            group_id: None,
            sort_order: None,
            created_at: Utc::now(),
            use_system_proxy: false,
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
}

/// 认证方式
///
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "auth_type", content = "auth_data")]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
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
    /// 尝试多种方法（agent → 默认密钥）。
    ///
    /// serde：adjacent tag 布局下带数据的变体要求 content 字段，`Auto` 无数据
    /// 不能用 `#[serde(skip)]`（skip 变体序列化必然失败，导致"密码/密钥留空
    /// 保存主机"必然报英文 serde 错误）。改为显式空 content 的单元形式。
    #[default]
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

    /// 创建密码引用认证
    pub fn password_ref(keychain_id: impl Into<String>) -> Self {
        AuthMethod::PasswordRef {
            keychain_id: keychain_id.into(),
        }
    }

    /// 创建密钥引用认证
    pub fn key_ref(keychain_id: impl Into<String>) -> Self {
        AuthMethod::PrivateKeyRef {
            keychain_id: keychain_id.into(),
        }
    }
}

pub fn sort_host_groups(
    groups: &mut [HostGroup],
    mode: SortMode,
    host_counts: Option<&std::collections::HashMap<String, usize>>,
) {
    match mode {
        SortMode::NameAsc => groups.sort_by(|a, b| cmp_name_insensitive(&a.name, &b.name)),
        SortMode::NameDesc => groups.sort_by(|a, b| cmp_name_insensitive(&b.name, &a.name)),
        SortMode::CreatedDesc => groups.sort_by_key(|g| std::cmp::Reverse(g.created_at)),
        SortMode::CreatedAsc => groups.sort_by_key(|g| g.created_at),
        SortMode::CountDesc => {
            if let Some(counts) = host_counts {
                groups.sort_by(|a, b| {
                    let ca = counts.get(&a.id).copied().unwrap_or(0);
                    let cb = counts.get(&b.id).copied().unwrap_or(0);
                    cb.cmp(&ca).then_with(|| a.name.cmp(&b.name))
                });
            } else {
                groups.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        SortMode::Custom => groups.sort_by_key(|g| g.sort_order),
    }
}

/// SSH 全局配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshGlobalConfig {
    /// 默认连接超时
    #[serde(default = "default_timeout")]
    pub default_timeout: u64,
    /// 是否启用严格主机密钥检查
    ///
    /// 默认 false（accept-new，与 OpenSSH 默认及本应用历史行为一致）：
    /// 未知主机首次连接时自动学习密钥（trust-on-first-use）。
    /// 设为 true 后只接受 known_hosts 中已记录的主机，未知主机直接拒绝。
    #[serde(default)]
    pub strict_host_key_checking: bool,
    /// 已保存的连接配置
    #[serde(default)]
    pub saved_connections: Vec<SshConnectionConfig>,
    /// 一次性迁移标记：旧版本的 new() 默认写入 strict_host_key_checking=true，
    /// 且该设置从未有任何 UI、也从未被执行——旧配置文件里的 true 是序列化
    /// 默认值而非用户选择。首次加载时把它归位为 false（accept-new）。
    /// 此后用户显式设置的 true 会被尊重，不会再被改写。
    #[serde(default)]
    host_key_strict_migrated: bool,
    /// Host 文件夹列表
    #[serde(default)]
    pub host_groups: Vec<HostGroup>,
    /// 文件夹排序方式
    #[serde(default)]
    pub folder_sort: SortMode,
    /// Host 排序方式
    #[serde(default)]
    pub host_sort: SortMode,
}

/// 进程内 ssh.json 解析结果缓存（见 [`SshGlobalConfig::load`]）：
/// mtime+size 未变时免读盘免解析。save() 后同步刷新。
struct CachedSshConfig {
    path: std::path::PathBuf,
    mtime: std::time::SystemTime,
    len: u64,
    config: std::sync::Arc<SshGlobalConfig>,
}

fn ssh_config_cache() -> &'static std::sync::Mutex<Option<CachedSshConfig>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<CachedSshConfig>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

impl SshGlobalConfig {
    /// 创建默认配置
    pub fn new() -> Self {
        Self {
            default_timeout: default_timeout(),
            strict_host_key_checking: false,
            saved_connections: Vec::new(),
            // 新建配置本身就是新默认值，无需迁移
            host_key_strict_migrated: true,
            host_groups: Vec::new(),
            folder_sort: SortMode::default(),
            host_sort: SortMode::default(),
        }
    }

    // ─── HostGroup 管理 ────────────────────────────────────────────────
    pub fn create_group(&mut self, name: &str) -> Result<HostGroup, String> {
        let trimmed = validate_group_name(name, &self.host_groups, None)?;
        let sort_order = self.host_groups.len() as u32;
        let group = HostGroup::new(trimmed, sort_order);
        self.host_groups.push(group.clone());
        Ok(group)
    }

    pub fn rename_group(&mut self, id: &str, new_name: &str) -> Result<(), String> {
        let trimmed = validate_group_name(new_name, &self.host_groups, Some(id))?;
        let group = self
            .host_groups
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or("未找到文件夹")?;
        group.name = trimmed;
        Ok(())
    }

    pub fn delete_group(&mut self, id: &str, delete_hosts: bool) -> Result<(), String> {
        let pos = self
            .host_groups
            .iter()
            .position(|g| g.id == id)
            .ok_or("未找到文件夹")?;
        self.host_groups.remove(pos);
        // 归一化 sort_order
        for (i, g) in self.host_groups.iter_mut().enumerate() {
            g.sort_order = i as u32;
        }
        if delete_hosts {
            self.saved_connections
                .retain(|c| c.group_id.as_deref() != Some(id));
        } else {
            for c in &mut self.saved_connections {
                if c.group_id.as_deref() == Some(id) {
                    c.group_id = None;
                }
            }
        }
        Ok(())
    }

    pub fn move_hosts_to_group(&mut self, host_ids: &[String], target: Option<&str>) {
        let host_ids: std::collections::HashSet<&String> = host_ids.iter().collect();
        let mut next_sort = self
            .saved_connections
            .iter()
            .filter(|c| c.group_id.as_deref() == target)
            .filter_map(|c| c.sort_order)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        for conn in &mut self.saved_connections {
            if host_ids.contains(&conn.id()) {
                conn.group_id = target.map(|s| s.to_string());
                conn.sort_order = Some(next_sort);
                next_sort += 1;
            }
        }
    }

    pub fn find_group(&self, id: &str) -> Option<&HostGroup> {
        self.host_groups.iter().find(|g| g.id == id)
    }

    /// 自愈孤儿 group_id（指向不存在分组的 Host 归为未分组）
    fn heal_orphan_group_ids(&mut self) -> bool {
        let valid: std::collections::HashSet<String> =
            self.host_groups.iter().map(|g| g.id.clone()).collect();
        let mut changed = false;
        for conn in &mut self.saved_connections {
            if let Some(gid) = conn.group_id.as_deref()
                && !valid.contains(gid)
            {
                conn.group_id = None;
                changed = true;
            }
        }
        changed
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

    /// 获取保存的连接数量
    pub fn saved_connections_count(&self) -> usize {
        self.saved_connections.len()
    }

    /// 从配置文件加载
    /// 优先 dirs::config_dir()/deepterm/ssh.json，兼容旧路径 ~/.config/deepterm/ssh.json
    /// 如果文件不存在或解析失败，返回默认配置（不自动创建文件）
    ///
    /// 进程内缓存：mtime+size 未变时直接复用上次的解析结果（一次 stat 代替
    /// 读盘+JSON 解析）。HostView/KeychainView 的 render 路径会频繁调用 load()，
    /// 避免每帧读盘；save() 后同步刷新缓存，其它窗口/进程的写入靠 mtime 失效。
    pub fn load() -> Self {
        (*Self::load_shared()).clone()
    }

    /// 与 [`Self::load`] 同源，但返回缓存里的 `Arc`，避免 UI 渲染路径每次
    /// 深拷贝整份配置。`Arc::ptr_eq` 可直接用作“配置是否变化”的版本判断。
    pub fn load_shared() -> std::sync::Arc<Self> {
        let config_path = match Self::config_path() {
            Some(path) => path,
            None => return std::sync::Arc::new(Self::new()),
        };
        if let Some(cached) = Self::cached_shared(&config_path) {
            return cached;
        }
        let config = Self::load_uncached(&config_path);
        Self::store_cache(&config_path, &config);
        Self::cached_shared(&config_path).unwrap_or_else(|| std::sync::Arc::new(config))
    }

    /// 查询缓存：路径 + mtime + size 完全一致时返回克隆（`load_or_create`
    /// 会修改配置，不能拿共享句柄）。
    fn cached(path: &std::path::Path) -> Option<Self> {
        Self::cached_shared(path).map(|config| (*config).clone())
    }

    /// 查询缓存：路径 + mtime + size 完全一致时返回共享句柄。
    fn cached_shared(path: &std::path::Path) -> Option<std::sync::Arc<Self>> {
        let (mtime, len) = Self::stat_sig(path)?;
        let cache = ssh_config_cache().lock().ok()?;
        let entry = cache.as_ref()?;
        if entry.path == path && entry.mtime == mtime && entry.len == len {
            Some(entry.config.clone())
        } else {
            None
        }
    }

    /// 写入缓存（load 解析完 / save 落盘后调用）。
    fn store_cache(path: &std::path::Path, config: &Self) {
        let Some((mtime, len)) = Self::stat_sig(path) else {
            return;
        };
        if let Ok(mut cache) = ssh_config_cache().lock() {
            *cache = Some(CachedSshConfig {
                path: path.to_path_buf(),
                mtime,
                len,
                config: std::sync::Arc::new(config.clone()),
            });
        }
    }

    fn stat_sig(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
        let md = std::fs::metadata(path).ok()?;
        Some((md.modified().ok()?, md.len()))
    }

    fn load_uncached(config_path: &std::path::Path) -> Self {
        // 优先新路径，若不存在则尝试旧路径（自动迁移）
        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(_) => {
                if let Some(legacy) = Self::legacy_config_path()
                    && legacy != config_path
                    && let Ok(legacy_content) = std::fs::read_to_string(&legacy)
                    && let Ok(mut cfg) = serde_json::from_str::<Self>(&legacy_content)
                {
                    log::info!(
                        "Migrating SSH config {} -> {}",
                        legacy.display(),
                        config_path.display()
                    );
                    // 先完成凭据迁移（内联密码 → keychain 引用），
                    // 否则下方 save() 会被明文阻断，导致每次启动重复尝试迁移
                    if let Err(err) = cfg.migrate_keychain_refs() {
                        log::warn!("SSH 凭据迁移失败: {}", err);
                    }
                    let _ = cfg.migrate_legacy_host_key_strict();
                    if let Err(e) = cfg.save() {
                        log::warn!("Failed to migrate SSH config to new location: {e}");
                    }
                    return cfg;
                }
                return Self::new();
            }
        };

        let mut config = serde_json::from_str(&content)
            .ok()
            .or_else(|| {
                // 主文件损坏：尝试 .bak 恢复，避免静默清空 host 列表
                let bak = config_path.with_extension("json.bak");
                if let Ok(bak_content) = std::fs::read_to_string(&bak)
                    && let Ok(cfg) = serde_json::from_str::<Self>(&bak_content)
                {
                    log::info!("Recovered SSH config from backup {}", bak.display());
                    return Some(cfg);
                }
                log::warn!(
                    "Failed to parse {} and no valid .bak, using empty config",
                    config_path.display()
                );
                None
            })
            .unwrap_or_else(Self::new);
        if let Err(err) = config.migrate_keychain_refs() {
            log::warn!("SSH 凭据迁移失败: {}", err);
        }
        if config.migrate_legacy_host_key_strict() {
            // 一次性回写，避免每次启动重复迁移
            if let Err(e) = config.save() {
                log::warn!("SSH: 回写 strict_host_key_checking 迁移结果失败: {e}");
            }
        }
        if config.heal_orphan_group_ids() {
            log::warn!("SSH: 自愈孤儿 group_id，已归为未分组");
        }
        config
    }

    /// 一次性迁移：把旧版本机器写入的 strict_host_key_checking=true 归位为 false。
    /// 返回 true 表示配置发生变化，调用方应当保存。
    /// 迁移标记落盘后，用户显式设置的 true 不再受影响。
    fn migrate_legacy_host_key_strict(&mut self) -> bool {
        if self.host_key_strict_migrated {
            return false;
        }
        self.host_key_strict_migrated = true;
        if self.strict_host_key_checking {
            self.strict_host_key_checking = false;
            log::info!(
                "SSH: 迁移旧默认值 strict_host_key_checking=true → false \
                （旧值由旧版默认写入，从未可被用户设置也从未被执行；如需严格模式请显式设为 true）"
            );
            return true;
        }
        false
    }

    /// 加载配置，如果不存在则创建默认配置文件（兼容旧路径迁移）
    pub fn load_or_create() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path().ok_or("Failed to get config path")?;

        // 与 load() 同样的 mtime 缓存：命中则免读盘。
        if let Some(cached) = Self::cached(&config_path) {
            return Ok(cached);
        }

        let mut config = match std::fs::read_to_string(&config_path) {
            Ok(content) => serde_json::from_str(&content)?,
            Err(_) => {
                // 新路径不存在，尝试旧路径迁移
                if let Some(legacy) = Self::legacy_config_path()
                    && legacy != config_path
                    && let Ok(legacy_content) = std::fs::read_to_string(&legacy)
                    && let Ok(mut cfg) = serde_json::from_str::<Self>(&legacy_content)
                {
                    log::info!(
                        "Migrating SSH config (load_or_create) {} -> {}",
                        legacy.display(),
                        config_path.display()
                    );
                    let _ = cfg.migrate_keychain_refs();
                    // 保存到新路径
                    let _ = cfg.save();
                    return Ok(cfg);
                }
                let default_config = Self::new();
                default_config.save()?;
                default_config
            }
        };

        config
            .migrate_keychain_refs()
            .map_err(|err| -> Box<dyn std::error::Error> { err.to_string().into() })?;
        if config.migrate_legacy_host_key_strict() {
            let _ = config.save();
        }
        let _ = config.heal_orphan_group_ids();
        Self::store_cache(&config_path, &config);
        Ok(config)
    }

    /// 保存配置到文件
    /// 配置文件路径: dirs::config_dir()/deepterm/ssh.json，原子写入 + 明文阻断
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        // 阻断明文密码/密钥落盘：若仍有旧版内联凭据，拒绝保存并提示迁移
        for conn in &self.saved_connections {
            match &conn.auth_method {
                AuthMethod::Password { .. } => {
                    return Err("拒绝保存：存在明文密码，请先完成 Keychain 迁移".into());
                }
                AuthMethod::PrivateKey {
                    passphrase: Some(_),
                    ..
                } => {
                    return Err("拒绝保存：存在明文私钥口令，请先完成 Keychain 迁移".into());
                }
                _ => {}
            }
        }
        let config_path = Self::config_path().ok_or("Failed to get config path")?;

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        // 原子写入，避免截断
        let tmp = config_path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, content)?;
        if config_path.exists() {
            let bak = config_path.with_extension("json.bak");
            let _ = std::fs::copy(&config_path, &bak);
        }
        std::fs::rename(&tmp, &config_path)?;
        // 限制权限 600，避免同机用户读取
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
        }
        // 落盘成功后同步刷新进程内缓存，后续 load() 直接命中
        Self::store_cache(&config_path, self);
        Ok(())
    }

    /// 获取配置文件路径：优先 dirs::config_dir，回退 ~/.config
    pub(crate) fn config_path() -> Option<std::path::PathBuf> {
        if let Some(dir) = dirs::config_dir() {
            return Some(dir.join("deepterm").join("ssh.json"));
        }
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("deepterm").join("ssh.json"))
    }

    fn legacy_config_path() -> Option<std::path::PathBuf> {
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

    /// 代理标记是**本次连接**的传输选择，不是主机的持久属性：序列化必须丢掉它，
    /// 否则「用代理连过一次」会变成这台主机的永久设置，之后每次都走代理。
    #[test]
    fn proxy_choice_is_never_persisted() {
        let mut config = SshConnectionConfig::new("example.com", "admin", AuthMethod::Agent);
        config.use_system_proxy = true;

        let json = serde_json::to_string(&config).expect("配置应可序列化");
        assert!(
            !json.contains("use_system_proxy"),
            "代理标记不得出现在落盘 JSON 里: {json}"
        );

        let restored: SshConnectionConfig = serde_json::from_str(&json).expect("配置应可反序列化");
        assert!(
            !restored.use_system_proxy,
            "从磁盘读回的主机必须是直连，代理标记不跨进程存活"
        );
        // 其余字段不受影响
        assert_eq!(restored.host, "example.com");
        assert_eq!(restored.username, "admin");
    }

    /// 代理标记不参与相等性：它不改变主机身份。
    ///
    /// 这一点被文件树后端匹配（`FileEndpoint::matches_fs`）依赖——「经代理连上」
    /// 的终端与它自己的文件树必须判为同一来源，否则文件树会拒绝刷新。
    #[test]
    fn proxy_choice_does_not_change_host_identity() {
        let direct = SshConnectionConfig::new("example.com", "admin", AuthMethod::Agent);
        let mut proxied = direct.clone();
        proxied.use_system_proxy = true;

        assert_eq!(direct, proxied, "代理标记不该让主机变得不相等");
        assert_eq!(direct.id(), proxied.id(), "主机 id 与代理选择无关");
    }

    #[test]
    fn test_ssh_connection_config() {
        let config = SshConnectionConfig::new(
            "example.com",
            "admin",
            AuthMethod::Password {
                password: "secret".into(),
            },
        );

        assert_eq!(config.host, "example.com");
        assert_eq!(config.username, "admin");
        assert_eq!(config.port, 22);
        assert!(matches!(config.auth_method, AuthMethod::Password { .. }));
        assert_eq!(config.display_name(), "admin@example.com");
    }

    #[test]
    fn test_ssh_connection_with_nickname() {
        let config = SshConnectionConfig::new(
            "192.168.1.100",
            "root",
            AuthMethod::PrivateKey {
                path: "~/.ssh/id_rsa".into(),
                passphrase: None,
            },
        )
        .with_nickname("My Server");

        assert_eq!(config.display_name(), "My Server");
    }

    #[test]
    fn test_auth_method_display_name() {
        assert_eq!(
            AuthMethod::Password {
                password: "test".into()
            }
            .display_name(),
            "密码"
        );
        assert_eq!(
            AuthMethod::PrivateKey {
                path: "~/.ssh/id_rsa".into(),
                passphrase: None,
            }
            .display_name(),
            "密钥"
        );
        assert_eq!(AuthMethod::Agent.display_name(), "SSH Agent");
    }

    #[test]
    fn test_auth_method_auto_serde_roundtrip() {
        // 回归：Auto 曾带 #[serde(skip)]，导致保存"密码/密钥留空"的主机时
        // 序列化必然失败（host_form 保存路径返回 Auto）。
        let json = serde_json::to_string(&AuthMethod::Auto).expect("Auto 必须可序列化");
        let back: AuthMethod = serde_json::from_str(&json).expect("Auto 必须可反序列化");
        assert!(matches!(back, AuthMethod::Auto));

        // 整个连接配置（save() 的实际形态）也要能往返。
        let config = SshConnectionConfig::new("example.com", "admin", AuthMethod::Auto);
        let json = serde_json::to_string(&config).expect("含 Auto 的配置必须可序列化");
        let back: SshConnectionConfig =
            serde_json::from_str(&json).expect("含 Auto 的配置必须可反序列化");
        assert!(matches!(back.auth_method, AuthMethod::Auto));
    }

    #[test]
    fn existing_host_json_without_port_forwards_still_loads() {
        // 升级兼容：老 ssh.json 里没有 port_forwards 字段，必须照常反序列化
        // （缺省为空规则表），否则用户保存的主机会整体消失。
        let json = r#"{
            "host": "example.com",
            "port": 22,
            "username": "root",
            "auth_type": "agent",
            "nickname": "old-host",
            "extra_args": null,
            "timeout": 30,
            "save_config": false,
            "os": "ubuntu",
            "group_id": null,
            "sort_order": null,
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let conn: SshConnectionConfig = serde_json::from_str(json).expect("老配置必须可读");
        assert_eq!(conn.nickname.as_deref(), Some("old-host"));
        assert!(conn.port_forwards.is_empty(), "老配置没有转发规则");
    }

    #[test]
    fn port_forwards_roundtrip_and_participate_in_equality() {
        use crate::ssh::port_forward::PortForwardRule;

        let mut conn = SshConnectionConfig::new("example.com", "root", AuthMethod::Agent);
        let rule = PortForwardRule {
            bind_host: "0.0.0.0".to_string(),
            bind_port: 8080,
            target_host: "127.0.0.1".to_string(),
            target_port: 3000,
            description: "dev server".to_string(),
        };
        conn.port_forwards = vec![rule.clone()];

        let json = serde_json::to_string(&conn).unwrap();
        let back: SshConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.port_forwards, vec![rule]);

        // 转发规则是连接身份的一部分：改了规则就必须被认为「配置变了」
        // （否则「已保存的主机」判重会漏掉这次修改）。
        let mut other = conn.clone();
        other.port_forwards.clear();
        assert_ne!(conn, other);
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

    #[test]
    fn legacy_strict_host_key_default_is_migrated_once() {
        // 旧版配置：字段由旧 new() 默认写入 true，且无迁移标记
        let json = r#"{"strict_host_key_checking": true, "saved_connections": []}"#;
        let mut cfg: SshGlobalConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.migrate_legacy_host_key_strict());
        assert!(!cfg.strict_host_key_checking, "旧默认 true 应回位为 false");

        // 二次迁移不再触发，且用户此后显式设置的 true 会被尊重
        assert!(!cfg.migrate_legacy_host_key_strict());
        cfg.strict_host_key_checking = true;
        assert!(!cfg.migrate_legacy_host_key_strict());
        assert!(cfg.strict_host_key_checking, "迁移后的显式 true 不得被改写");
    }

    #[test]
    fn new_config_uses_accept_new_default() {
        let mut cfg = SshGlobalConfig::new();
        assert!(!cfg.strict_host_key_checking);
        assert!(!cfg.migrate_legacy_host_key_strict(), "新建配置无需迁移");
    }

    #[test]
    fn host_group_crud_and_sort() {
        let mut cfg = SshGlobalConfig::new();
        let g1 = cfg.create_group("生产").unwrap();
        let g2 = cfg.create_group("测试").unwrap();
        assert_eq!(cfg.host_groups.len(), 2);
        assert!(cfg.create_group("生产").is_err()); // 重名大小写不敏感
        assert!(cfg.create_group("").is_err());
        // rename
        cfg.rename_group(&g1.id, "预发").unwrap();
        assert_eq!(cfg.find_group(&g1.id).unwrap().name, "预发");
        assert!(cfg.rename_group(&g2.id, "预发").is_err());
        // move hosts
        let conn = SshConnectionConfig::new("10.0.0.1", "root", AuthMethod::Agent);
        let conn_id = conn.id();
        cfg.add_connection(conn);
        cfg.move_hosts_to_group(std::slice::from_ref(&conn_id), Some(&g1.id));
        assert_eq!(
            cfg.find_connection(&conn_id).unwrap().group_id.as_deref(),
            Some(g1.id.as_str())
        );
        // delete group keeps hosts
        cfg.delete_group(&g1.id, false).unwrap();
        assert_eq!(cfg.find_connection(&conn_id).unwrap().group_id, None);
        // recreate and delete with hosts
        let g3 = cfg.create_group("临时").unwrap();
        cfg.move_hosts_to_group(std::slice::from_ref(&conn_id), Some(&g3.id));
        cfg.delete_group(&g3.id, true).unwrap();
        assert!(cfg.find_connection(&conn_id).is_none());
    }

    #[test]
    fn legacy_ssh_json_without_groups_loads() {
        let json = r#"{
            "saved_connections": [{"host":"1.1.1.1","port":22,"username":"root","auth_type":"agent","auth_data":null}]
        }"#;
        let cfg: SshGlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host_groups.len(), 0);
        assert_eq!(cfg.saved_connections[0].group_id, None);
        assert_eq!(cfg.folder_sort, SortMode::NameAsc);
    }

    #[test]
    fn legacy_ssh_json_with_removed_fields_still_loads() {
        // 回归锁定：known_hosts / use_agent / default_identity_files 字段
        // 已从 SshGlobalConfig 删除（2026-09 清理），但旧版本写出的
        // ssh.json 仍含这些字段——serde 必须忽略而非报错，否则 load()
        // 会静默回退到空配置，用户的主机列表"消失"。
        let json = r#"{
            "default_timeout": 10,
            "strict_host_key_checking": false,
            "saved_connections": [],
            "known_hosts": [{"host":"h","port":22,"fingerprint":"f","key_type":"t","verified":true}],
            "default_identity_files": ["/home/u/.ssh/id_ed25519"],
            "use_agent": true
        }"#;
        let cfg: SshGlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.saved_connections.len(), 0);
        assert_eq!(cfg.default_timeout, 10);
    }

    #[test]
    fn sort_host_groups_name_and_count() {
        let mut groups = vec![
            HostGroup {
                id: "2".into(),
                name: "b".into(),
                sort_order: 1,
                created_at: Utc::now(),
                collapsed: false,
            },
            HostGroup {
                id: "1".into(),
                name: "a".into(),
                sort_order: 0,
                created_at: Utc::now(),
                collapsed: false,
            },
        ];
        sort_host_groups(&mut groups, SortMode::NameAsc, None);
        assert_eq!(groups[0].name, "a");
        sort_host_groups(&mut groups, SortMode::NameDesc, None);
        assert_eq!(groups[0].name, "b");
        let mut counts = std::collections::HashMap::new();
        counts.insert("1".to_string(), 5);
        counts.insert("2".to_string(), 10);
        sort_host_groups(&mut groups, SortMode::CountDesc, Some(&counts));
        assert_eq!(groups[0].id, "2");
    }

    #[test]
    fn heal_orphan_group_ids() {
        let mut cfg = SshGlobalConfig::new();
        let mut conn = SshConnectionConfig::new("1.1.1.1", "root", AuthMethod::Agent);
        conn.group_id = Some("nonexistent".into());
        cfg.add_connection(conn);
        assert!(cfg.heal_orphan_group_ids());
        assert_eq!(cfg.saved_connections[0].group_id, None);
    }
}

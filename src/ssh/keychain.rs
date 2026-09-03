use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use keyring::{Entry, Error as KeyringError};
use russh::keys::{PrivateKey, load_openssh_certificate, load_secret_key};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ssh::KeyType;
use crate::ssh::config::{AuthMethod, SshConnectionConfig};

const KEYCHAIN_SERVICE: &str = "com.deepterm.app.credentials";
const PASSWORD_ACCOUNT_PREFIX: &str = "identity-password";
const PASSPHRASE_ACCOUNT_PREFIX: &str = "key-passphrase";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeychainItemKind {
    Identity,
    Key,
}

impl KeychainItemKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            KeychainItemKind::Identity => "Identity",
            KeychainItemKind::Key => "Key",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeychainKeyFlavor {
    Key,
    Certificate,
    TouchId,
    Fido2,
}

impl KeychainKeyFlavor {
    pub fn display_name(&self) -> &'static str {
        match self {
            KeychainKeyFlavor::Key => "Key",
            KeychainKeyFlavor::Certificate => "Certificate",
            KeychainKeyFlavor::TouchId => "Touch ID",
            KeychainKeyFlavor::Fido2 => "FIDO2",
        }
    }

    pub fn requires_pty(&self) -> bool {
        matches!(self, KeychainKeyFlavor::TouchId | KeychainKeyFlavor::Fido2)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeychainItem {
    pub id: String,
    pub label: String,
    pub kind: KeychainItemKind,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    #[serde(default)]
    pub certificate_path: Option<PathBuf>,
    #[serde(default = "default_key_flavor")]
    pub key_flavor: KeychainKeyFlavor,
    #[serde(default)]
    pub algorithm: Option<String>,
    #[serde(default)]
    pub has_secret: bool,
    #[serde(default)]
    pub has_passphrase: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_key_flavor() -> KeychainKeyFlavor {
    KeychainKeyFlavor::Key
}

impl KeychainItem {
    pub fn subtitle(&self) -> String {
        match self.kind {
            KeychainItemKind::Identity => self
                .username
                .clone()
                .map(|username| format!("Auth password, {}", username))
                .unwrap_or_else(|| "Auth password".to_string()),
            KeychainItemKind::Key => {
                let algorithm = self
                    .algorithm
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string());
                match self.key_flavor {
                    KeychainKeyFlavor::Key => format!("Type {}", algorithm),
                    KeychainKeyFlavor::Certificate => {
                        format!("Type {}, certificate", algorithm)
                    }
                    KeychainKeyFlavor::TouchId => format!("Type {}, Touch ID", algorithm),
                    KeychainKeyFlavor::Fido2 => format!("Type {}, FIDO2", algorithm),
                }
            }
        }
    }

    pub fn identity(label: impl Into<String>, username: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            label: label.into(),
            kind: KeychainItemKind::Identity,
            username,
            key_path: None,
            certificate_path: None,
            key_flavor: KeychainKeyFlavor::Key,
            algorithm: None,
            has_secret: true,
            has_passphrase: false,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn key(
        label: impl Into<String>,
        key_path: impl Into<PathBuf>,
        certificate_path: Option<PathBuf>,
        key_flavor: KeychainKeyFlavor,
        algorithm: Option<String>,
        has_passphrase: bool,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            label: label.into(),
            kind: KeychainItemKind::Key,
            username: None,
            key_path: Some(key_path.into()),
            certificate_path,
            key_flavor,
            algorithm,
            has_secret: false,
            has_passphrase,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeychainStore {
    #[serde(default)]
    pub items: Vec<KeychainItem>,
}

#[derive(Clone, Debug)]
pub enum ResolvedAuthMethod {
    Password {
        password: String,
    },
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
        certificate_path: Option<PathBuf>,
        key_flavor: KeychainKeyFlavor,
    },
    Agent,
    Auto,
}

#[derive(Clone, Debug)]
pub enum PreferredSshTransport {
    Russh,
    Pty,
}

impl KeychainStore {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub fn load() -> Option<Self> {
        let path = Self::config_path()?;
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Self>(&content) {
                Ok(store) => Some(store),
                Err(e) => {
                    log::warn!("Failed to parse {}: {e}, trying .bak", path.display());
                    Self::load_from_bak(&path)
                }
            },
            Err(_) => {
                // 新路径不存在：尝试旧路径并自动迁移（仅当新文件不存在，不覆盖）
                if let Some(legacy) = Self::legacy_config_path() {
                    if legacy != path {
                        if let Ok(content) = std::fs::read_to_string(&legacy) {
                            if let Ok(store) = serde_json::from_str::<Self>(&content) {
                                log::info!(
                                    "Migrating keychain {} -> {}",
                                    legacy.display(),
                                    path.display()
                                );
                                if let Err(e) = store.save() {
                                    log::warn!("Failed to migrate keychain: {e}");
                                }
                                return Some(store);
                            }
                        }
                    }
                }
                Some(Self::new())
            }
        }
    }

    /// 从同名 .bak 恢复；均失败则返回空库（避免返回 None 破坏调用方）
    fn load_from_bak(path: &Path) -> Option<Self> {
        let bak = path.with_extension("json.bak");
        if let Ok(content) = std::fs::read_to_string(&bak) {
            if let Ok(store) = serde_json::from_str::<Self>(&content) {
                log::info!("Recovered keychain from backup {}", bak.display());
                return Some(store);
            }
        }
        Some(Self::new())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.persist()
    }

    pub fn identities(&self) -> Vec<&KeychainItem> {
        self.items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Identity)
            .collect()
    }

    pub fn keys(&self) -> Vec<&KeychainItem> {
        self.items
            .iter()
            .filter(|item| item.kind == KeychainItemKind::Key)
            .collect()
    }

    pub fn find_item(&self, id: &str) -> Option<&KeychainItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn create_identity(
        &mut self,
        label: &str,
        username: Option<String>,
        password: &str,
    ) -> anyhow::Result<String> {
        let item = KeychainItem::identity(label, username);
        let id = item.id.clone();
        self.set_password_secret(&id, password)?;
        self.items.push(item);
        self.save()?;
        Ok(id)
    }

    pub fn upsert_identity(
        &mut self,
        existing_id: Option<&str>,
        label: &str,
        username: Option<String>,
        password: &str,
    ) -> anyhow::Result<String> {
        match existing_id {
            Some(id) => {
                let item = self
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| anyhow!("未找到对应的 identity"))?;
                item.label = label.to_string();
                item.username = username;
                item.updated_at = Utc::now();
                item.has_secret = true;
                self.set_password_secret(id, password)?;
                self.save()?;
                Ok(id.to_string())
            }
            None => self.create_identity(label, username, password),
        }
    }

    pub fn create_key(
        &mut self,
        label: &str,
        key_path: PathBuf,
        certificate_path: Option<PathBuf>,
        key_flavor: KeychainKeyFlavor,
        passphrase: Option<&str>,
    ) -> anyhow::Result<String> {
        let (key_path, inferred_certificate_path) = normalize_private_key_path(&key_path)?;
        let certificate_path = certificate_path.or(inferred_certificate_path);
        let key_flavor = normalized_key_flavor(key_flavor, &key_path, certificate_path.as_deref());
        let algorithm = infer_key_algorithm(&key_path);
        let mut item = KeychainItem::key(
            label,
            key_path,
            certificate_path,
            key_flavor,
            algorithm,
            passphrase.is_some(),
        );
        let id = item.id.clone();
        if let Some(passphrase) = passphrase.filter(|value| !value.trim().is_empty()) {
            self.set_key_passphrase_secret(&id, passphrase)?;
            item.has_passphrase = true;
        }
        self.items.push(item);
        self.save()?;
        Ok(id)
    }

    pub fn upsert_key(
        &mut self,
        existing_id: Option<&str>,
        label: &str,
        key_path: PathBuf,
        certificate_path: Option<PathBuf>,
        key_flavor: KeychainKeyFlavor,
        passphrase: Option<&str>,
    ) -> anyhow::Result<String> {
        let normalized_passphrase = passphrase
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let (key_path, inferred_certificate_path) = normalize_private_key_path(&key_path)?;
        let certificate_path = certificate_path.or(inferred_certificate_path);
        let key_flavor = normalized_key_flavor(key_flavor, &key_path, certificate_path.as_deref());

        match existing_id {
            Some(id) => {
                let item = self
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| anyhow!("未找到对应的 key"))?;
                item.label = label.to_string();
                item.key_path = Some(key_path.clone());
                item.certificate_path = certificate_path;
                item.key_flavor = key_flavor;
                item.algorithm = infer_key_algorithm(&key_path);
                item.updated_at = Utc::now();
                item.has_passphrase = normalized_passphrase.is_some();

                if let Some(passphrase) = normalized_passphrase {
                    self.set_key_passphrase_secret(id, &passphrase)?;
                } else {
                    self.delete_secret(PASSPHRASE_ACCOUNT_PREFIX, id)?;
                }

                self.save()?;
                Ok(id.to_string())
            }
            None => self.create_key(
                label,
                key_path,
                certificate_path,
                key_flavor,
                normalized_passphrase.as_deref(),
            ),
        }
    }

    pub fn delete_item(&mut self, id: &str) -> anyhow::Result<()> {
        self.items.retain(|item| item.id != id);
        self.delete_secret(PASSWORD_ACCOUNT_PREFIX, id)?;
        self.delete_secret(PASSPHRASE_ACCOUNT_PREFIX, id)?;
        self.save()?;
        Ok(())
    }

    pub fn get_password_secret(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.get_secret(PASSWORD_ACCOUNT_PREFIX, id)
    }

    pub fn get_key_passphrase_secret(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.get_secret(PASSPHRASE_ACCOUNT_PREFIX, id)
    }

    fn set_password_secret(&self, id: &str, password: &str) -> anyhow::Result<()> {
        self.set_secret(PASSWORD_ACCOUNT_PREFIX, id, password)
    }

    fn set_key_passphrase_secret(&self, id: &str, passphrase: &str) -> anyhow::Result<()> {
        self.set_secret(PASSPHRASE_ACCOUNT_PREFIX, id, passphrase)
    }

    fn set_secret(&self, prefix: &str, id: &str, value: &str) -> anyhow::Result<()> {
        let entry = Self::entry(prefix, id)?;
        entry
            .set_password(value)
            .with_context(|| format!("写入系统 Keychain 失败: {}:{}", prefix, id))?;
        match entry.get_password() {
            Ok(saved) if saved == value => Ok(()),
            Ok(_) => Err(anyhow!("系统 Keychain 回读结果不一致: {}:{}", prefix, id)),
            Err(KeyringError::NoEntry) => {
                Err(anyhow!("系统 Keychain 未保存凭据: {}:{}", prefix, id))
            }
            Err(err) => Err(anyhow!(
                "系统 Keychain 回读失败: {}: {}:{}",
                err,
                prefix,
                id
            )),
        }
    }

    fn get_secret(&self, prefix: &str, id: &str) -> anyhow::Result<Option<String>> {
        let entry = Self::entry(prefix, id)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(anyhow!(
                "读取系统 Keychain 失败: {}: {}:{}",
                err,
                prefix,
                id
            )),
        }
    }

    fn delete_secret(&self, prefix: &str, id: &str) -> anyhow::Result<()> {
        let entry = Self::entry(prefix, id)?;
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(err) => Err(anyhow!(
                "删除系统 Keychain 凭据失败: {}: {}:{}",
                err,
                prefix,
                id
            )),
        }
    }

    fn entry(prefix: &str, id: &str) -> anyhow::Result<Entry> {
        Entry::new(KEYCHAIN_SERVICE, &format!("{}:{}", prefix, id))
            .map_err(|err| anyhow!("创建 Keychain 入口失败: {}", err))
    }

    fn config_path() -> Option<PathBuf> {
        // 与 ssh.json / theme.json 统一：优先系统标准 config 目录
        if let Some(dir) = dirs::config_dir() {
            return Some(dir.join("deepterm").join("keychain.json"));
        }
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("deepterm").join("keychain.json"))
    }

    /// 旧版路径（~/.config/deepterm/keychain.json），用于自动迁移
    fn legacy_config_path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("deepterm").join("keychain.json"))
    }

    /// 原子写入 + 权限限制，与 ssh.json 保持一致
    fn persist(&self) -> anyhow::Result<()> {
        let path = Self::config_path().ok_or_else(|| anyhow!("无法确定 keychain 配置路径"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, content)?;
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("json.bak"));
        }
        std::fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// 以 0600 权限写入敏感文件（私钥、凭据缓存等）。
/// - unix 上创建时即带 0600（避免"先写后 chmod"窗口期内其他用户可读）；
/// - 若文件已存在且权限更宽松，写后收紧为 0600；
/// - 非 unix 平台退化为普通写入。
pub(crate) fn write_secret_file(path: &std::path::Path, content: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content)?;
        // 文件已存在时 mode 不生效，显式收紧
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)?;
        Ok(())
    }
}

pub fn migrate_auth_method(
    connection: &mut SshConnectionConfig,
    store: &mut KeychainStore,
) -> anyhow::Result<bool> {
    match &connection.auth_method {
        AuthMethod::Password { password } => {
            if password.trim().is_empty() {
                connection.auth_method = AuthMethod::Auto;
                return Ok(true);
            }

            let label = connection
                .nickname
                .clone()
                .unwrap_or_else(|| format!("{} password", connection.host_label()));
            let keychain_id =
                store.create_identity(&label, Some(connection.username.clone()), password)?;
            connection.auth_method = AuthMethod::password_ref(keychain_id);
            Ok(true)
        }
        AuthMethod::PrivateKey { path, passphrase } => {
            let (normalized_path, certificate_path) = normalize_private_key_path(path)?;
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("{} key", connection.host_label()));
            let keychain_id = store.create_key(
                &label,
                normalized_path,
                certificate_path.clone(),
                classify_key_flavor(path, certificate_path.as_deref()),
                passphrase.as_deref(),
            )?;
            connection.auth_method = AuthMethod::key_ref(keychain_id);
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn resolve_auth_method(auth_method: &AuthMethod) -> anyhow::Result<ResolvedAuthMethod> {
    match auth_method {
        AuthMethod::PasswordRef { keychain_id } => {
            let store = KeychainStore::load_or_default();
            let item = store
                .find_item(keychain_id)
                .ok_or_else(|| anyhow!("未找到关联的身份条目: {}", keychain_id))?;
            if item.kind != KeychainItemKind::Identity {
                return Err(anyhow!("认证引用不是有效的 identity: {}", keychain_id));
            }
            let password = store
                .get_password_secret(keychain_id)?
                .ok_or_else(|| anyhow!("系统 Keychain 中缺少身份密码: {}", keychain_id))?;
            Ok(ResolvedAuthMethod::Password { password })
        }
        AuthMethod::PrivateKeyRef { keychain_id } => {
            let store = KeychainStore::load_or_default();
            let item = store
                .find_item(keychain_id)
                .ok_or_else(|| anyhow!("未找到关联的私钥条目: {}", keychain_id))?;
            if item.kind != KeychainItemKind::Key {
                return Err(anyhow!("认证引用不是有效的 key: {}", keychain_id));
            }
            let path = item
                .key_path
                .clone()
                .ok_or_else(|| anyhow!("Keychain 私钥缺少路径配置: {}", keychain_id))?;
            let (path, inferred_certificate_path) = normalize_private_key_path(&path)?;
            let passphrase = store.get_key_passphrase_secret(keychain_id)?;
            Ok(ResolvedAuthMethod::PrivateKey {
                path,
                passphrase,
                certificate_path: item.certificate_path.clone().or(inferred_certificate_path),
                key_flavor: item.key_flavor,
            })
        }
        AuthMethod::Password { password } => Ok(ResolvedAuthMethod::Password {
            password: password.clone(),
        }),
        AuthMethod::PrivateKey { path, passphrase } => {
            let (path, certificate_path) = normalize_private_key_path(path)?;
            let key_flavor = classify_key_flavor(&path, certificate_path.as_deref());
            Ok(ResolvedAuthMethod::PrivateKey {
                path,
                passphrase: passphrase.clone(),
                certificate_path,
                key_flavor,
            })
        }
        AuthMethod::Agent => Ok(ResolvedAuthMethod::Agent),
        AuthMethod::Auto => Ok(ResolvedAuthMethod::Auto),
    }
}

pub fn preferred_ssh_transport(auth_method: &AuthMethod) -> anyhow::Result<PreferredSshTransport> {
    match resolve_auth_method(auth_method)? {
        ResolvedAuthMethod::PrivateKey { key_flavor, .. } => {
            // 只有 russh 处理不了的密钥类型才回退到系统 ssh（PTY）；
            // 普通密钥与带证书的密钥都走 russh。
            if key_flavor.requires_pty() {
                Ok(PreferredSshTransport::Pty)
            } else {
                Ok(PreferredSshTransport::Russh)
            }
        }
        ResolvedAuthMethod::Agent
        | ResolvedAuthMethod::Password { .. }
        | ResolvedAuthMethod::Auto => Ok(PreferredSshTransport::Russh),
    }
}

pub fn load_key_material(path: &Path, passphrase: Option<&str>) -> anyhow::Result<Arc<PrivateKey>> {
    let (path, _) = normalize_private_key_path(path)?;
    let key = load_secret_key(&path, passphrase)
        .map_err(|err| anyhow!("加载私钥失败 ({:?}): {}", path, err))?;
    Ok(Arc::new(key))
}

pub fn load_certificate(path: &Path) -> anyhow::Result<russh::keys::ssh_key::Certificate> {
    load_openssh_certificate(path)
        .map_err(|err| anyhow!("加载 OpenSSH 证书失败 ({:?}): {}", path, err))
}

pub fn classify_key_flavor(key_path: &Path, certificate_path: Option<&Path>) -> KeychainKeyFlavor {
    if certificate_path.is_some() {
        return KeychainKeyFlavor::Certificate;
    }

    let filename = key_path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if filename.contains("fido") || filename.ends_with("_sk") {
        return KeychainKeyFlavor::Fido2;
    }

    if filename.contains("touchid") || filename.contains("secure_enclave") {
        return KeychainKeyFlavor::TouchId;
    }

    KeychainKeyFlavor::Key
}

fn normalized_key_flavor(
    requested_flavor: KeychainKeyFlavor,
    key_path: &Path,
    certificate_path: Option<&Path>,
) -> KeychainKeyFlavor {
    if certificate_path.is_some() {
        return KeychainKeyFlavor::Certificate;
    }

    match requested_flavor {
        KeychainKeyFlavor::TouchId | KeychainKeyFlavor::Fido2 => requested_flavor,
        KeychainKeyFlavor::Key | KeychainKeyFlavor::Certificate => {
            classify_key_flavor(key_path, certificate_path)
        }
    }
}

pub fn expand_home_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }

    if let Some(remainder) = path_str.strip_prefix("~/")
        && let Some(home_dir) = dirs::home_dir()
    {
        return home_dir.join(remainder);
    }

    path.to_path_buf()
}

pub fn sibling_certificate_path(private_key_path: &Path) -> Option<PathBuf> {
    let file_name = private_key_path.file_name()?.to_string_lossy();
    Some(private_key_path.with_file_name(format!("{}-cert.pub", file_name)))
}

pub fn normalize_private_key_path(path: &Path) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    let expanded_path = expand_home_path(path);
    let path_str = expanded_path.to_string_lossy();

    if path_str.ends_with("-cert.pub") {
        let private_key = PathBuf::from(path_str.trim_end_matches("-cert.pub"));
        if private_key.exists() {
            return Ok((private_key, Some(expanded_path)));
        }
        return Err(anyhow!("选择的证书文件没有对应的私钥: {:?}", expanded_path));
    }

    if path_str.ends_with(".pub") {
        let private_key = PathBuf::from(path_str.trim_end_matches(".pub"));
        if private_key.exists() {
            let certificate_path = sibling_certificate_path(&private_key).filter(|p| p.exists());
            return Ok((private_key, certificate_path));
        }
        return Err(anyhow!(
            "选中的文件是公钥，请改为选择私钥文件: {:?}",
            expanded_path
        ));
    }

    let certificate_path = sibling_certificate_path(&expanded_path).filter(|p| p.exists());
    Ok((expanded_path, certificate_path))
}

fn infer_key_algorithm(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_string_lossy();
    KeyType::from_filename(&filename).map(|key_type| match key_type {
        KeyType::Rsa => "RSA".to_string(),
        KeyType::Ecdsa => "ECDSA".to_string(),
        KeyType::Ed25519 => "ED25519".to_string(),
        KeyType::Dss => "DSS".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::normalize_private_key_path;

    fn temp_ssh_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deepterm-keychain-test-{}-{}",
            name,
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn normalizes_public_key_path_to_private_key() {
        let private_key = temp_ssh_path("id_ed25519");
        let public_key = std::path::PathBuf::from(format!("{}.pub", private_key.display()));
        fs::write(&private_key, "dummy").expect("failed to write private key");
        fs::write(&public_key, "dummy").expect("failed to write public key");

        let (normalized_path, certificate_path) =
            normalize_private_key_path(&public_key).expect("expected public key path to normalize");

        assert_eq!(normalized_path, private_key);
        assert!(certificate_path.is_none());

        let _ = fs::remove_file(&public_key);
        let _ = fs::remove_file(&private_key);
    }

    #[test]
    fn keeps_matching_certificate_when_normalizing_cert_path() {
        let private_key = temp_ssh_path("id_rsa");
        let certificate_path =
            std::path::PathBuf::from(format!("{}-cert.pub", private_key.display()));
        fs::write(&private_key, "dummy").expect("failed to write private key");
        fs::write(&certificate_path, "dummy").expect("failed to write certificate");

        let (normalized_path, normalized_certificate_path) =
            normalize_private_key_path(&certificate_path)
                .expect("expected certificate path to normalize");

        assert_eq!(normalized_path, private_key);
        assert_eq!(normalized_certificate_path, Some(certificate_path.clone()));

        let _ = fs::remove_file(&certificate_path);
        let _ = fs::remove_file(&private_key);
    }
}

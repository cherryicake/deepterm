use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use russh::client::{self, AuthResult};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileType;
use tokio::io::AsyncReadExt;

use crate::ssh::config::SshConnectionConfig;
use crate::ssh::keychain::{
    ResolvedAuthMethod, load_certificate, load_key_material, resolve_auth_method,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteFileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub kind: RemoteFileKind,
    pub size: u64,
    pub modified_at: Option<std::time::SystemTime>,
}

#[derive(Clone, Debug)]
pub struct RemoteFileMetadata {
    pub kind: RemoteFileKind,
    pub size: u64,
    pub modified_at: Option<std::time::SystemTime>,
}

struct SftpClientHandler;

impl client::Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct BlockingSftpSession {
    runtime: tokio::runtime::Runtime,
    handle: client::Handle<SftpClientHandler>,
    sftp: SftpSession,
}

impl BlockingSftpSession {
    pub fn connect(config: &SshConnectionConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;
        let (handle, sftp) = runtime.block_on(connect_sftp(config))?;
        Ok(Self {
            runtime,
            handle,
            sftp,
        })
    }

    pub fn canonicalize(&mut self, path: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            self.sftp
                .canonicalize(path)
                .await
                .map_err(anyhow::Error::from)
        })
    }

    pub fn metadata(&mut self, path: &str) -> anyhow::Result<RemoteFileMetadata> {
        self.runtime.block_on(async {
            let metadata = self.sftp.metadata(path).await?;
            Ok(convert_metadata(
                metadata.file_type(),
                metadata.len(),
                metadata.mtime,
            ))
        })
    }

    pub fn read_dir(&mut self, path: &str) -> anyhow::Result<Vec<RemoteFileEntry>> {
        self.runtime.block_on(async {
            let entries = self
                .sftp
                .read_dir(path)
                .await?
                .map(|entry| {
                    let metadata = entry.metadata();
                    RemoteFileEntry {
                        name: entry.file_name(),
                        path: join_remote_path(path, &entry.file_name()),
                        kind: convert_file_type(entry.file_type()),
                        size: metadata.len(),
                        modified_at: metadata.mtime.map(|secs| {
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
                        }),
                    }
                })
                .collect();
            Ok(entries)
        })
    }

    pub fn read_text_preview(&mut self, path: &str) -> anyhow::Result<Option<String>> {
        const PREVIEW_BYTES: usize = 64 * 1024;

        self.runtime.block_on(async {
            let file = self.sftp.open(path).await?;
            let mut bytes = Vec::new();
            file.take(PREVIEW_BYTES as u64)
                .read_to_end(&mut bytes)
                .await?;

            if bytes.is_empty() {
                return Ok(Some("Empty file".to_string()));
            }

            let text = String::from_utf8_lossy(&bytes).into_owned();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        })
    }
}

type SharedBlockingSftpSession = Arc<Mutex<BlockingSftpSession>>;

fn session_cache() -> &'static Mutex<HashMap<String, SharedBlockingSftpSession>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SharedBlockingSftpSession>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(config: &SshConnectionConfig) -> String {
    serde_json::to_string(config)
        .unwrap_or_else(|_| format!("{}@{}:{}", config.username, config.host, config.port))
}

fn get_or_connect_session(
    config: &SshConnectionConfig,
    key: &str,
) -> anyhow::Result<SharedBlockingSftpSession> {
    if let Some(session) = session_cache().lock().unwrap().get(key).cloned() {
        return Ok(session);
    }

    let session = Arc::new(Mutex::new(BlockingSftpSession::connect(config)?));
    let mut cache = session_cache().lock().unwrap();
    Ok(cache
        .entry(key.to_string())
        .or_insert_with(|| session.clone())
        .clone())
}

fn evict_session(key: &str, session: &SharedBlockingSftpSession) {
    let mut cache = session_cache().lock().unwrap();
    if cache
        .get(key)
        .is_some_and(|cached| Arc::ptr_eq(cached, session))
    {
        cache.remove(key);
    }
}

pub fn with_cached_session<T>(
    config: &SshConnectionConfig,
    f: impl Fn(&mut BlockingSftpSession) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let key = cache_key(config);
    let session = get_or_connect_session(config, &key)?;

    match f(&mut session.lock().unwrap()) {
        Ok(result) => Ok(result),
        Err(_) => {
            evict_session(&key, &session);
            let session = get_or_connect_session(config, &key)?;
            f(&mut session.lock().unwrap())
        }
    }
}

impl Drop for BlockingSftpSession {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            let _ = self.sftp.close().await;
            let _ = self
                .handle
                .disconnect(russh::Disconnect::ByApplication, "", "English")
                .await;
        });
    }
}

pub fn canonicalize(config: &SshConnectionConfig, path: &str) -> anyhow::Result<String> {
    with_cached_session(config, |session| session.canonicalize(path))
}

pub fn metadata(config: &SshConnectionConfig, path: &str) -> anyhow::Result<RemoteFileMetadata> {
    with_cached_session(config, |session| session.metadata(path))
}

pub fn read_dir(config: &SshConnectionConfig, path: &str) -> anyhow::Result<Vec<RemoteFileEntry>> {
    with_cached_session(config, |session| session.read_dir(path))
}

pub fn read_text_preview(
    config: &SshConnectionConfig,
    path: &str,
) -> anyhow::Result<Option<String>> {
    with_cached_session(config, |session| session.read_text_preview(path))
}

async fn connect_sftp(
    config: &SshConnectionConfig,
) -> anyhow::Result<(client::Handle<SftpClientHandler>, SftpSession)> {
    let russh_config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    let mut handle = client::connect(
        russh_config,
        (config.host.as_str(), config.port),
        SftpClientHandler,
    )
    .await
    .map_err(|e| anyhow!("TCP/SSH 握手失败: {}", e))?;

    let auth_result = authenticate(&mut handle, config).await?;
    if !auth_result.success() {
        return Err(anyhow!("SSH 认证被拒绝（用户: {}）", config.username));
    }

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| anyhow!("打开 SSH 通道失败: {}", e))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| anyhow!("启动 SFTP 子系统失败: {}", e))?;

    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| anyhow!("初始化 SFTP 会话失败: {}", e))?;
    Ok((handle, sftp))
}

async fn authenticate(
    handle: &mut client::Handle<SftpClientHandler>,
    config: &SshConnectionConfig,
) -> anyhow::Result<AuthResult> {
    let resolved_auth = resolve_auth_method(&config.auth_method)?;

    match &resolved_auth {
        ResolvedAuthMethod::Password { password } => handle
            .authenticate_password(&config.username, password)
            .await
            .map_err(|e| anyhow!("密码认证失败: {}", e)),
        ResolvedAuthMethod::PrivateKey {
            path,
            passphrase,
            certificate_path,
            ..
        } => {
            let key = load_key_material(path, passphrase.as_deref())?;
            if let Some(certificate_path) = certificate_path {
                let cert = load_certificate(certificate_path)?;
                handle
                    .authenticate_openssh_cert(&config.username, key, cert)
                    .await
                    .map_err(|e| anyhow!("OpenSSH 证书认证失败: {}", e))
            } else {
                let hash_alg = handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten();
                handle
                    .authenticate_publickey(
                        &config.username,
                        PrivateKeyWithHashAlg::new(key, hash_alg),
                    )
                    .await
                    .map_err(|e| anyhow!("公钥认证失败: {}", e))
            }
        }
        ResolvedAuthMethod::Auto => authenticate_with_default_keys(handle, config).await,
        ResolvedAuthMethod::Agent => {
            let agent_result = russh::keys::agent::client::AgentClient::connect_env().await;

            if let Ok(mut agent) = agent_result {
                let identities = agent
                    .request_identities()
                    .await
                    .map_err(|e| anyhow!("获取 SSH Agent 密钥列表失败: {}", e))?;

                if !identities.is_empty() {
                    let hash_alg = handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();

                    for identity in &identities {
                        let pub_key = identity.public_key().into_owned();
                        match handle
                            .authenticate_publickey_with(
                                &config.username,
                                pub_key,
                                hash_alg,
                                &mut agent,
                            )
                            .await
                        {
                            Ok(result) if result.success() => return Ok(result),
                            Ok(_) | Err(_) => {}
                        }
                    }
                }
            }

            authenticate_with_default_keys(handle, config).await
        }
    }
}

async fn authenticate_with_default_keys(
    handle: &mut client::Handle<SftpClientHandler>,
    config: &SshConnectionConfig,
) -> anyhow::Result<AuthResult> {
    let default_keys = crate::ssh::find_default_ssh_keys();
    if default_keys.is_empty() {
        return Err(anyhow!("自动认证失败：~/.ssh 中未找到密钥文件"));
    }

    let hash_alg = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();

    for key_path in &default_keys {
        let key = match russh::keys::load_secret_key(key_path, None) {
            Ok(key) => key,
            Err(_) => continue,
        };

        match handle
            .authenticate_publickey(
                &config.username,
                PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
            )
            .await
        {
            Ok(result) if result.success() => return Ok(result),
            Ok(_) | Err(_) => {}
        }
    }

    Err(anyhow!("所有默认密钥均无效（用户: {}）", config.username))
}

fn convert_file_type(file_type: FileType) -> RemoteFileKind {
    match file_type {
        FileType::Dir => RemoteFileKind::Directory,
        FileType::File => RemoteFileKind::File,
        FileType::Symlink => RemoteFileKind::Symlink,
        FileType::Other => RemoteFileKind::Other,
    }
}

fn convert_metadata(file_type: FileType, size: u64, mtime: Option<u32>) -> RemoteFileMetadata {
    RemoteFileMetadata {
        kind: convert_file_type(file_type),
        size,
        modified_at: mtime
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)),
    }
}

pub fn join_remote_path(base: &str, child: &str) -> String {
    if base == "/" {
        format!("/{}", child.trim_start_matches('/'))
    } else if base.is_empty() {
        child.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            child.trim_start_matches('/')
        )
    }
}

pub fn remote_parent(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }

    let trimmed = path.trim_end_matches('/');
    if let Some(index) = trimmed.rfind('/') {
        if index == 0 {
            Some("/".to_string())
        } else {
            Some(trimmed[..index].to_string())
        }
    } else {
        None
    }
}

pub fn remote_components(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

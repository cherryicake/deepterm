use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use parking_lot::Mutex;

use russh::client::{AuthResult, Handle};
use russh::keys::PrivateKeyWithHashAlg;
#[cfg(unix)]
use russh::keys::agent::client::AgentClient;

use crate::ssh::config::SshConnectionConfig;

/// 进程内缓存：记录"DeepTerm 连接 `<user>@<host>:<port>` 时，最终是哪一把磁盘密钥认过的"。
///
/// 用于 [`crate::file_transfer`] 的远程到远程 scp 直连：当目标使用 `Agent`/`Auto`
/// 且 ssh-agent 没有可用身份时，DeepTerm 自己会回退到 `~/.ssh` 默认密钥认证；
/// 此时同一把磁盘密钥就是源端 scp 唯一需要的。把它直接复用即可，避免再把所有默认
/// 密钥都丢给 scp 试一遍（每次失败都要一次完整握手）。
fn auth_key_cache() -> &'static Mutex<HashMap<String, PathBuf>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn auth_cache_key(config: &SshConnectionConfig) -> String {
    format!("{}@{}:{}", config.username, config.host, config.port)
}

/// 记录一次成功的"磁盘密钥 → 目标"认证关系。
pub fn record_authenticated_key(config: &SshConnectionConfig, path: &Path) {
    let mut map = auth_key_cache().lock();
    map.insert(auth_cache_key(config), path.to_path_buf());
}

/// 查询某个目标最近一次认证成功用的磁盘密钥。
pub fn lookup_authenticated_key(config: &SshConnectionConfig) -> Option<PathBuf> {
    auth_key_cache()
        .lock()
        .get(&auth_cache_key(config))
        .cloned()
}

/// Try SSH Agent authentication, falling back to default keys if unavailable.
///
/// This centralizes the Agent auth logic shared between russh_terminal and sftp_fs.
pub async fn try_agent_auth<H: russh::client::Handler>(
    handle: &mut Handle<H>,
    config: &SshConnectionConfig,
) -> anyhow::Result<AuthResult> {
    #[cfg(unix)]
    {
        match AgentClient::connect_env().await {
            Ok(mut agent) => {
                let identities = agent
                    .request_identities()
                    .await
                    .map_err(|e| anyhow::anyhow!("获取 SSH Agent 密钥列表失败: {}", e))?;

                if !identities.is_empty() {
                    log::info!("russh: SSH Agent 中有 {} 个密钥", identities.len());

                    let hash_alg = handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();

                    for identity in &identities {
                        let pub_key = identity.public_key().into_owned();
                        log::info!("russh: 尝试 Agent 密钥: {}", pub_key.key_data().algorithm());
                        match handle
                            .authenticate_publickey_with(
                                &config.username,
                                pub_key,
                                hash_alg,
                                &mut agent,
                            )
                            .await
                        {
                            Ok(result) if result.success() => {
                                log::info!("russh: SSH Agent 密钥认证成功");
                                return Ok(result);
                            }
                            Ok(_) => {
                                log::debug!("russh: 该 Agent 密钥未成功，继续尝试下一个");
                            }
                            Err(e) => {
                                log::debug!("russh: Agent 密钥认证错误: {}", e);
                            }
                        }
                    }

                    return Err(anyhow::anyhow!(
                        "russh: SSH Agent 认证失败：所有密钥均无效（用户: {}）",
                        config.username
                    ));
                }

                log::info!("russh: SSH Agent 中没有密钥，回退到默认密钥文件");
            }
            Err(e) => {
                log::info!("russh: 无法连接 SSH Agent ({}), 回退到默认密钥文件", e);
            }
        }
    }
    #[cfg(not(unix))]
    {
        log::info!("russh: Windows 平台不支持 SSH Agent，回退到默认密钥文件");
    }

    // Agent 不可用或无密钥时，自动尝试 ~/.ssh 中的默认密钥
    try_default_keys(handle, config).await
}

/// Try authenticating with default keys in ~/.ssh.
pub async fn try_default_keys<H: russh::client::Handler>(
    handle: &mut Handle<H>,
    config: &SshConnectionConfig,
) -> anyhow::Result<AuthResult> {
    let mut default_keys = crate::ssh::find_default_ssh_keys();
    if default_keys.is_empty() {
        return Err(anyhow::anyhow!("自动认证失败：~/.ssh 中未找到密钥文件"));
    }

    // 若之前对同一目标成功认证过某把磁盘密钥，把它挪到最前面。避免每次新建 SFTP
    // session（文件浏览器、文件传输都会触发）时都要先对着错误密钥走一个 RTT。
    if let Some(cached) = lookup_authenticated_key(config) {
        if let Some(idx) = default_keys.iter().position(|p| p == &cached) {
            if idx != 0 {
                let hit = default_keys.remove(idx);
                default_keys.insert(0, hit);
                log::info!("复用上次认证过的密钥 {:?}，优先尝试", default_keys[0]);
            }
        }
    }

    let hash_alg = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();

    for key_path in &default_keys {
        log::info!("尝试默认密钥 {:?}", key_path);
        let key = match russh::keys::load_secret_key(key_path, None) {
            Ok(k) => k,
            Err(e) => {
                log::debug!("跳过密钥 {:?}: {}", key_path, e);
                continue;
            }
        };
        match handle
            .authenticate_publickey(
                &config.username,
                PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), hash_alg),
            )
            .await
        {
            Ok(result) if result.success() => {
                log::info!("默认密钥认证成功 ({:?})", key_path);
                record_authenticated_key(config, key_path);
                return Ok(result);
            }
            Ok(_) => {
                log::debug!("密钥 {:?} 未成功，继续", key_path);
            }
            Err(e) => {
                log::debug!("密钥 {:?} 认证错误: {}", key_path, e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "所有默认密钥均无效（用户: {}）",
        config.username
    ))
}

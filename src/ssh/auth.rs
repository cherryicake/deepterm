use russh::client::{AuthResult, Handle};
use russh::keys::PrivateKeyWithHashAlg;
#[cfg(unix)]
use russh::keys::agent::client::AgentClient;

use crate::ssh::config::SshConnectionConfig;

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
                        log::info!(
                            "russh: 尝试 Agent 密钥: {}",
                            pub_key.key_data().algorithm()
                        );
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
    let default_keys = crate::ssh::find_default_ssh_keys();
    if default_keys.is_empty() {
        return Err(anyhow::anyhow!(
            "自动认证失败：~/.ssh 中未找到密钥文件"
        ));
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

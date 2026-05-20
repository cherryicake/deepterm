//! SSH agent 可用性检测，用于远程到远程传输的 agent forwarding 策略（方案 3a）。
//!
//! 本模块只做 **只读** 查询：
//! - 检查本机是否有可用的 ssh-agent（`SSH_AUTH_SOCK` 存在且可连接）。
//! - 检查指定私钥对应的公钥指纹是否已加载到 agent 中。
//! - 粗粒度探活：agent 是否可达、内部是否有任何身份。
//!
//! 不会主动 `ssh-add` 加载密钥，避免意外修改用户的 agent 状态。

use std::path::Path;

use anyhow::anyhow;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key;
use russh::keys::{HashAlg, PrivateKey, PublicKey};

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn create_agent_client() -> Result<AgentClient<UnixStream>, anyhow::Error> {
    AgentClient::connect_env()
        .await
        .map_err(|e| anyhow!("连接 ssh-agent 失败: {}", e))
}

#[cfg(windows)]
async fn create_agent_client() -> Result<AgentClient<NamedPipeClient>, anyhow::Error> {
    let path = std::env::var("SSH_AUTH_SOCK")
        .map_err(|_| anyhow!("SSH_AUTH_SOCK 未设置"))?;
    AgentClient::connect_named_pipe(&path)
        .await
        .map_err(|e| anyhow!("连接 ssh-agent 失败: {}", e))
}

/// 判断本机 ssh-agent 是否可用（可连接），以及里面是否至少有一把身份。
///
/// 当目标服务器的认证方式是 `Agent` / `Auto`（没有具体密钥路径）时，
/// 只要本机 agent 里有任意一把 identity，就有机会通过 agent forwarding
/// 让源端 `scp` 鉴权到目标 —— 是否真的能认过由 scp 自行尝试并报错即可。
///
/// 返回：
/// - `Ok(true)`：agent 可达且 `request_identities` 至少有 1 把 key。
/// - `Ok(false)`：agent 可达但未加载任何 key。
/// - `Err(_)`：agent 不可达（通常是 `SSH_AUTH_SOCK` 未设置或 socket 连不上）。
pub fn agent_is_available() -> anyhow::Result<bool> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("创建临时 runtime 失败: {}", e))?;

    rt.block_on(async move {
        let mut client = create_agent_client().await?;
        let identities = client
            .request_identities()
            .await
            .map_err(|e| anyhow!("ssh-agent 列出密钥失败: {}", e))?;
        Ok(!identities.is_empty())
    })
}

/// 判断本机 ssh-agent 是否可用 **且** 已加载 `key_path` 对应的私钥。
///
/// 返回 `Ok(true)` 表示可以走 agent forwarding；`Ok(false)` 表示 agent 存在
/// 但没有这把密钥；`Err(_)` 表示 agent 不可用（通常是 `SSH_AUTH_SOCK` 未设置
/// 或 socket 连不上）。
pub fn agent_has_key(key_path: &Path) -> anyhow::Result<bool> {
    // 读取私钥并计算其公钥指纹，用于与 agent 中的 identities 比对。
    let key_bytes = std::fs::read(key_path)
        .map_err(|e| anyhow!("读取私钥 {} 失败: {}", key_path.display(), e))?;

    // 仅支持无密码短语的私钥——带 passphrase 的密钥通常不会直接进入 agent
    // （进入时 ssh-add 会提示解锁）；我们这里假设已加载的密钥就是解锁过的。
    let private = PrivateKey::from_openssh(&key_bytes)
        .map_err(|e| anyhow!("解析私钥 {} 失败: {}", key_path.display(), e))?;

    let public = private.public_key().clone();
    let target_fp = public.fingerprint(HashAlg::Sha256);

    // AgentClient 需要 tokio runtime；这里起一个临时 current_thread runtime，
    // 调用者通常不在 async 上下文里。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("创建临时 runtime 失败: {}", e))?;

    let has = rt.block_on(async move {
        let mut client = create_agent_client().await?;
        let identities = client
            .request_identities()
            .await
            .map_err(|e| anyhow!("ssh-agent 列出密钥失败: {}", e))?;
        Ok::<bool, anyhow::Error>(
            identities
                .iter()
                .any(|id| identity_matches(id, &public, &target_fp)),
        )
    })?;

    Ok(has)
}

fn identity_matches(
    identity: &AgentIdentity,
    _target_public: &PublicKey,
    target_fp: &ssh_key::Fingerprint,
) -> bool {
    match identity {
        AgentIdentity::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256) == *target_fp,
        AgentIdentity::Certificate { certificate, .. } => {
            certificate.public_key().fingerprint(HashAlg::Sha256) == *target_fp
        }
    }
}

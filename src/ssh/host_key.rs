//! SSH 主机密钥校验：known_hosts 检查 + 可选自动学习。
//!
//! russh 的 `client::Handler` 需要为每一条连接实现 `check_server_key`。
//! 终端通道（`russh_terminal`）与 SFTP 通道（`sftp_fs`）必须共用这里的
//! 逻辑 —— 只要有一侧漏做校验，那条通道就会对中间人攻击完全敞开。

use russh::keys::PublicKey;

/// 主机密钥策略，由 `SshGlobalConfig::strict_host_key_checking` 决定。
///
/// - `AcceptNew`（默认关闭严格模式）：未知主机按 accept-new 语义写入
///   known_hosts 后接受（trust-on-first-use，与 OpenSSH 默认一致）。
/// - `Strict`：只接受 known_hosts 中已记录的密钥，未知主机直接拒绝，
///   不静默学习。需要先用系统 ssh 连接一次、或人工导入 known_hosts。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyPolicy {
    AcceptNew,
    Strict,
}

impl HostKeyPolicy {
    fn from_strict_flag(strict: bool) -> Self {
        if strict {
            HostKeyPolicy::Strict
        } else {
            HostKeyPolicy::AcceptNew
        }
    }
}

/// 读取全局配置中的主机密钥策略。读不到配置时退回 Strict（安全优先）。
pub fn policy_from_global_config() -> HostKeyPolicy {
    HostKeyPolicy::from_strict_flag(crate::ssh::config::SshGlobalConfig::load().strict_host_key_checking)
}

/// 校验服务端主机密钥。
///
/// - 已知且匹配 → 接受
/// - 未知主机 → `AcceptNew` 下写入 known_hosts 并接受；`Strict` 下拒绝
/// - 密钥变更 → 拒绝（主机重装或中间人攻击，需要人工确认后才能继续）
/// - 其他检查错误（known_hosts 不可读/解析失败等）→ **拒绝**。
///   过去这里的处理是"接受并记日志"，等于在 known_hosts 损坏期间向
///   中间人完全敞开；无法验证主机身份时应当报错而不是放行。
pub fn check_and_trust_host_key(
    host: &str,
    port: u16,
    server_public_key: &PublicKey,
    policy: HostKeyPolicy,
) -> Result<bool, russh::Error> {
    match russh::keys::known_hosts::check_known_hosts(host, port, server_public_key) {
        Ok(true) => {
            log::debug!("ssh: 主机密钥已知且匹配: {host}:{port}");
            Ok(true)
        }
        Ok(false) => match policy {
            HostKeyPolicy::AcceptNew => {
                log::warn!(
                    "ssh: 新主机 {host}:{port}，按 accept-new 自动添加密钥到 known_hosts \
                    （首次连接无法防范 MITM；可在 SSH 设置中启用严格主机密钥检查后人工确认）"
                );
                if let Err(e) =
                    russh::keys::known_hosts::learn_known_hosts(host, port, server_public_key)
                {
                    log::warn!("ssh: 写入 known_hosts 失败: {e}");
                }
                Ok(true)
            }
            HostKeyPolicy::Strict => {
                log::error!(
                    "ssh: {host}:{port} 不在 known_hosts 中，严格模式下拒绝连接。\
                    请先用系统 ssh 连接一次该主机并确认指纹，或关闭“严格主机密钥检查”"
                );
                Err(russh::Error::Inconsistent)
            }
        },
        Err(russh::keys::Error::KeyChanged { line }) => {
            log::error!(
                "ssh: 主机密钥已变更！{host}:{port} 与 known_hosts 第 {line} 行不匹配，拒绝连接"
            );
            Err(russh::Error::Inconsistent)
        }
        Err(e) => {
            log::error!(
                "ssh: known_hosts 检查出错，拒绝连接（无法验证主机身份）: {e}。\
                请检查 ~/.ssh/known_hosts 是否存在且可读"
            );
            Err(russh::Error::Inconsistent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_maps_strict_flag() {
        assert_eq!(HostKeyPolicy::from_strict_flag(true), HostKeyPolicy::Strict);
        assert_eq!(
            HostKeyPolicy::from_strict_flag(false),
            HostKeyPolicy::AcceptNew
        );
    }
}

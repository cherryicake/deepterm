//! SSH 主机密钥校验：known_hosts 检查 + accept-new 自动学习。
//!
//! russh 的 `client::Handler` 需要为每一条连接实现 `check_server_key`。
//! 终端通道（`russh_terminal`）与 SFTP 通道（`sftp_fs`）必须共用这里的
//! 逻辑 —— 只要有一侧漏做校验，那条通道就会对中间人攻击完全敞开。

use russh::keys::PublicKey;

/// 校验服务端主机密钥。
///
/// - 已知且匹配 → 接受
/// - 未知主机 → 按 accept-new 语义写入 known_hosts 并接受
/// - 密钥变更 → 拒绝（主机重装或中间人攻击，需要人工确认后才能继续）
/// - 其他检查错误 → 接受并记日志：known_hosts 文件不可读、权限异常等环境问题
///   严格拒绝会让所有连接都失败，这里与 OpenSSH 的宽容路径保持一致。
pub fn check_and_trust_host_key(
    host: &str,
    port: u16,
    server_public_key: &PublicKey,
) -> Result<bool, russh::Error> {
    match russh::keys::known_hosts::check_known_hosts(host, port, server_public_key) {
        Ok(true) => {
            log::debug!("ssh: 主机密钥已知且匹配: {host}:{port}");
            Ok(true)
        }
        Ok(false) => {
            log::info!("ssh: 新主机 {host}:{port}，自动添加密钥到 known_hosts");
            if let Err(e) =
                russh::keys::known_hosts::learn_known_hosts(host, port, server_public_key)
            {
                log::warn!("ssh: 写入 known_hosts 失败: {e}");
            }
            Ok(true)
        }
        Err(russh::keys::Error::KeyChanged { line }) => {
            log::error!(
                "ssh: 主机密钥已变更！{host}:{port} 与 known_hosts 第 {line} 行不匹配，拒绝连接"
            );
            Err(russh::Error::Inconsistent)
        }
        Err(e) => {
            log::warn!("ssh: known_hosts 检查出错: {e}");
            Ok(true)
        }
    }
}

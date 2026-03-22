//! SSH 错误类型定义

use std::fmt;
use std::io;

/// SSH 操作结果类型别名
pub type SshResult<T> = Result<T, SshError>;

/// SSH 错误类型
#[derive(Debug)]
pub enum SshError {
    /// 连接错误
    ConnectionFailed { host: String, reason: String },
    /// 认证失败
    AuthenticationFailed { username: String, method: String },
    /// 主机密钥验证失败
    HostKeyVerificationFailed { host: String, fingerprint: String },
    /// 通道创建失败
    ChannelCreationFailed { reason: String },
    /// 请求 PTY 失败
    PtyRequestFailed { reason: String },
    /// 执行命令失败
    CommandExecutionFailed { command: String, reason: String },
    /// 配置错误
    InvalidConfig { field: String, reason: String },
    /// IO 错误
    Io(io::Error),
    /// 密钥错误
    KeyError { path: String, reason: String },
    /// 超时
    Timeout { operation: String, duration: u64 },
    /// 连接已关闭
    ConnectionClosed,
    /// 不支持的特性
    UnsupportedFeature { feature: String },
    /// 内部错误
    Internal(String),
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshError::ConnectionFailed { host, reason } => {
                write!(f, "无法连接到 '{}': {}", host, reason)
            }
            SshError::AuthenticationFailed { username, method } => {
                write!(f, "用户 '{}' 认证失败 (方法: {})", username, method)
            }
            SshError::HostKeyVerificationFailed { host, fingerprint } => {
                write!(f, "主机 '{}' 的密钥验证失败。指纹: {}", host, fingerprint)
            }
            SshError::ChannelCreationFailed { reason } => {
                write!(f, "创建 SSH 通道失败: {}", reason)
            }
            SshError::PtyRequestFailed { reason } => {
                write!(f, "请求 PTY 失败: {}", reason)
            }
            SshError::CommandExecutionFailed { command, reason } => {
                write!(f, "执行命令 '{}' 失败: {}", command, reason)
            }
            SshError::InvalidConfig { field, reason } => {
                write!(f, "配置错误 ({}): {}", field, reason)
            }
            SshError::Io(err) => {
                write!(f, "IO 错误: {}", err)
            }
            SshError::KeyError { path, reason } => {
                write!(f, "密钥文件 '{}' 错误: {}", path, reason)
            }
            SshError::Timeout {
                operation,
                duration,
            } => {
                write!(f, "操作 '{}' 超时 ({} 秒)", operation, duration)
            }
            SshError::ConnectionClosed => {
                write!(f, "SSH 连接已关闭")
            }
            SshError::UnsupportedFeature { feature } => {
                write!(f, "不支持的特性: {}", feature)
            }
            SshError::Internal(msg) => {
                write!(f, "内部错误: {}", msg)
            }
        }
    }
}

impl std::error::Error for SshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SshError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SshError {
    fn from(err: io::Error) -> Self {
        SshError::Io(err)
    }
}

impl From<russh::Error> for SshError {
    fn from(err: russh::Error) -> Self {
        // 简化处理：将所有 russh 错误映射为内部错误或连接错误
        let err_str = format!("{:?}", err);
        SshError::Internal(format!("SSH 错误: {}", err_str))
    }
}

impl From<russh::keys::Error> for SshError {
    fn from(err: russh::keys::Error) -> Self {
        SshError::KeyError {
            path: "unknown".to_string(),
            reason: format!("{:?}", err),
        }
    }
}

/// 扩展 Result 类型的错误转换 trait
pub trait SshResultExt<T> {
    /// 将错误转换为 ConnectionFailed
    fn with_host(self, host: impl Into<String>) -> SshResult<T>;
}

impl<T, E> SshResultExt<T> for Result<T, E>
where
    E: std::fmt::Display,
{
    fn with_host(self, host: impl Into<String>) -> SshResult<T> {
        self.map_err(|e| SshError::ConnectionFailed {
            host: host.into(),
            reason: e.to_string(),
        })
    }
}

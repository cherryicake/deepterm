//! SSH 连接管理器
//!
//! 管理所有 SSH 连接的生命周期，包括建立连接、断开连接和状态监控。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::ssh::{
    config::SshConnectionConfig,
    error::{SshError, SshResult},
    ConnectionEvent, generate_connection_id,
};

/// 连接状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// 空闲（尚未连接）
    Idle,
    /// 正在连接
    Connecting,
    /// 已连接
    Connected,
    /// 正在断开连接
    Disconnecting,
    /// 已断开
    Disconnected,
    /// 连接失败
    Failed,
    /// 正在重连
    Reconnecting,
}

impl ConnectionState {
    /// 获取状态的显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            ConnectionState::Idle => "空闲",
            ConnectionState::Connecting => "连接中",
            ConnectionState::Connected => "已连接",
            ConnectionState::Disconnecting => "断开中",
            ConnectionState::Disconnected => "已断开",
            ConnectionState::Failed => "连接失败",
            ConnectionState::Reconnecting => "重连中",
        }
    }

    /// 是否是活动状态（可以发送数据）
    pub fn is_active(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    /// 是否是正在连接/重连状态
    pub fn is_connecting(&self) -> bool {
        matches!(
            self,
            ConnectionState::Connecting | ConnectionState::Reconnecting
        )
    }

    /// 是否是终端状态
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ConnectionState::Disconnected | ConnectionState::Failed
        )
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        ConnectionState::Idle
    }
}

/// 连接信息
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    /// 连接 ID
    pub id: String,
    /// 连接配置
    pub config: SshConnectionConfig,
    /// 当前状态
    pub state: ConnectionState,
    /// 连接建立时间
    pub connected_at: Option<std::time::Instant>,
    /// 最后活动时间
    pub last_activity: Option<std::time::Instant>,
    /// 错误信息（如果有）
    pub error: Option<String>,
    /// 连接尝试次数
    pub connect_attempts: u32,
}

impl ConnectionInfo {
    /// 创建新的连接信息
    pub fn new(config: SshConnectionConfig) -> Self {
        let id = generate_connection_id();
        Self {
            id,
            config,
            state: ConnectionState::Idle,
            connected_at: None,
            last_activity: None,
            error: None,
            connect_attempts: 0,
        }
    }

    /// 获取连接时长
    pub fn connection_duration(&self) -> Option<Duration> {
        self.connected_at.map(|t| t.elapsed())
    }

    /// 获取空闲时间
    pub fn idle_duration(&self) -> Option<Duration> {
        self.last_activity.map(|t| t.elapsed())
    }

    /// 更新状态
    pub fn set_state(&mut self, state: ConnectionState) {
        self.state = state;
        match state {
            ConnectionState::Connected => {
                self.connected_at = Some(std::time::Instant::now());
                self.last_activity = Some(std::time::Instant::now());
                self.error = None;
            }
            ConnectionState::Failed => {
                self.connect_attempts += 1;
            }
            _ => {}
        }
    }

    /// 更新活动时间
    pub fn touch(&mut self) {
        self.last_activity = Some(std::time::Instant::now());
    }

    /// 设置错误
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.state = ConnectionState::Failed;
    }
}

/// 活动的 SSH 连接
struct ActiveConnection {
    /// 连接信息
    info: ConnectionInfo,
    /// 断开连接的接收器（保留供未来使用）
    #[allow(dead_code)]
    disconnect_rx: async_channel::Receiver<()>,
}

/// SSH 连接管理器
pub struct SshConnectionManager {
    /// 所有连接
    connections: Mutex<HashMap<String, ActiveConnection>>,
    /// 连接状态变更回调
    event_handlers: Mutex<Vec<Box<dyn Fn(ConnectionEvent) + Send + Sync>>>,
    /// 全局配置（保留供未来使用）
    #[allow(dead_code)]
    client_config: Arc<crate::ssh::SshClientConfig>,
}

impl SshConnectionManager {
    /// 创建新的连接管理器
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            event_handlers: Mutex::new(Vec::new()),
            client_config: Arc::new(crate::ssh::SshClientConfig::default()),
        }
    }

    /// 创建带配置的连接管理器
    pub fn with_config(client_config: crate::ssh::SshClientConfig) -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            event_handlers: Mutex::new(Vec::new()),
            client_config: Arc::new(client_config),
        }
    }

    /// 注册事件处理器
    pub fn on_event<F>(&self, handler: F)
    where
        F: Fn(ConnectionEvent) + Send + Sync + 'static,
    {
        self.event_handlers.lock().push(Box::new(handler));
    }

    /// 发送事件到所有处理器
    fn emit_event(&self, event: ConnectionEvent) {
        for handler in self.event_handlers.lock().iter() {
            handler(event.clone());
        }
    }

    /// 建立新的 SSH 连接
    ///
    /// 这是一个异步方法，需要在异步上下文中调用
    pub async fn connect(&self, config: SshConnectionConfig) -> SshResult<String> {
        let connection_id = generate_connection_id();

        // 创建连接信息
        let mut info = ConnectionInfo::new(config);
        info.id = connection_id.clone();
        info.set_state(ConnectionState::Connecting);

        self.emit_event(ConnectionEvent::Connecting {
            connection_id: connection_id.clone(),
        });

        // 创建断开信号通道
        let (disconnect_tx, disconnect_rx) = async_channel::bounded(1);

        // 尝试建立连接
        // 注意：当前使用 SshPtyTerminal（ssh_pty.rs）直接创建 PTY 连接
        // connection_manager 主要保留用于连接状态管理和配置存储
        match self.do_connect(&info.config).await {
            Ok(()) => {
                let mut info = info;
                info.set_state(ConnectionState::Connected);

                let active_conn = ActiveConnection {
                    info: info.clone(),
                    disconnect_rx,
                };

                // 存储连接
                self.connections
                    .lock()
                    .insert(connection_id.clone(), active_conn);

                self.emit_event(ConnectionEvent::Connected {
                    connection_id: connection_id.clone(),
                });

                // 启动断开监控任务
                self.spawn_disconnect_monitor(connection_id.clone(), disconnect_tx);

                Ok(connection_id)
            }
            Err(e) => {
                let mut info = info;
                info.set_error(&e.to_string());

                let active_conn = ActiveConnection {
                    info: info.clone(),
                    disconnect_rx,
                };

                self.connections
                    .lock()
                    .insert(connection_id.clone(), active_conn);

                self.emit_event(ConnectionEvent::ConnectionFailed {
                    connection_id: connection_id.clone(),
                    error: e.to_string(),
                });

                Err(e)
            }
        }
    }

    /// 实际执行连接逻辑
    ///
    /// 注意：当前实现直接返回成功，因为实际的 SSH 连接由 SshPtyTerminal 处理。
    /// 这个方法主要用于验证配置和建立连接记录。
    async fn do_connect(&self, _config: &SshConnectionConfig) -> SshResult<()> {
        // 当前使用 SshPtyTerminal（ssh_pty.rs）直接创建 PTY 连接
        // 验证配置基本有效性
        if _config.host.is_empty() {
            return Err(SshError::InvalidConfig {
                field: "host".to_string(),
                reason: "主机名不能为空".to_string(),
            });
        }
        if _config.username.is_empty() {
            return Err(SshError::InvalidConfig {
                field: "username".to_string(),
                reason: "用户名不能为空".to_string(),
            });
        }
        Ok(())
    }

    /// 断开指定的连接
    pub async fn disconnect(&self, connection_id: &str) -> SshResult<()> {
        let mut connections = self.connections.lock();
        if let Some(conn) = connections.get_mut(connection_id) {
            conn.info.set_state(ConnectionState::Disconnecting);
            conn.info.set_state(ConnectionState::Disconnected);

            self.emit_event(ConnectionEvent::Disconnected {
                connection_id: connection_id.to_string(),
                reason: Some("用户主动断开".to_string()),
            });

            Ok(())
        } else {
            Err(SshError::InvalidConfig {
                field: "connection_id".to_string(),
                reason: "连接不存在".to_string(),
            })
        }
    }

    /// 断开所有连接
    pub async fn disconnect_all(&self) {
        let ids: Vec<String> = self.connections.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.disconnect(&id).await;
        }
    }

    /// 获取连接信息
    pub fn get_connection_info(&self, connection_id: &str) -> Option<ConnectionInfo> {
        let connections = self.connections.lock();
        connections
            .get(connection_id)
            .map(|conn| conn.info.clone())
    }

    /// 获取所有连接信息
    pub fn get_all_connections(&self) -> Vec<ConnectionInfo> {
        let connections = self.connections.lock();
        connections
            .values()
            .map(|conn| conn.info.clone())
            .collect()
    }

    /// 获取活动连接数量
    pub fn active_connection_count(&self) -> usize {
        let connections = self.connections.lock();
        connections
            .values()
            .filter(|conn| conn.info.state.is_active())
            .count()
    }

    /// 检查连接是否存在
    pub fn has_connection(&self, connection_id: &str) -> bool {
        self.connections.lock().contains_key(connection_id)
    }

    /// 检查连接是否处于活动状态
    pub fn is_connected(&self, connection_id: &str) -> bool {
        let connections = self.connections.lock();
        connections
            .get(connection_id)
            .map(|conn| conn.info.state.is_active())
            .unwrap_or(false)
    }

    /// 更新连接活动时间
    pub fn touch_connection(&self, connection_id: &str) {
        let mut connections = self.connections.lock();
        if let Some(conn) = connections.get_mut(connection_id) {
            conn.info.touch();
        }
    }

    /// 移除连接记录
    pub fn remove_connection(&self, connection_id: &str) -> Option<ConnectionInfo> {
        let mut connections = self.connections.lock();
        connections
            .remove(connection_id)
            .map(|conn| conn.info.clone())
    }

    /// 启动断开连接监控任务
    fn spawn_disconnect_monitor(&self, _connection_id: String, _disconnect_tx: async_channel::Sender<()>) {
        // TODO: 实现断开监控
        // 当连接断开时，需要更新状态并发送事件
    }

    /// 尝试重新连接
    pub async fn reconnect(&self, connection_id: &str) -> SshResult<()> {
        // 获取现有配置
        let config = self
            .get_connection_info(connection_id)
            .ok_or_else(|| SshError::InvalidConfig {
                field: "connection_id".to_string(),
                reason: "连接不存在".to_string(),
            })?
            .config;

        // 移除旧连接
        self.remove_connection(connection_id);

        // 建立新连接
        self.connect(config).await?;

        Ok(())
    }
}

impl Default for SshConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 连接管理器的异步操作句柄
#[derive(Clone)]
pub struct ConnectionManagerHandle {
    inner: Arc<SshConnectionManager>,
}

impl ConnectionManagerHandle {
    /// 创建新的句柄
    pub fn new(manager: SshConnectionManager) -> Self {
        Self {
            inner: Arc::new(manager),
        }
    }

    /// 获取底层管理器的引用
    pub fn inner(&self) -> &SshConnectionManager {
        &self.inner
    }

    /// 连接到指定主机
    pub async fn connect(&self, config: SshConnectionConfig) -> SshResult<String> {
        self.inner.connect(config).await
    }

    /// 断开连接
    pub async fn disconnect(&self, connection_id: &str) -> SshResult<()> {
        self.inner.disconnect(connection_id).await
    }

    /// 获取连接信息
    pub fn get_connection_info(&self, connection_id: &str) -> Option<ConnectionInfo> {
        self.inner.get_connection_info(connection_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::AuthMethod;

    #[test]
    fn test_connection_state() {
        assert!(ConnectionState::Connected.is_active());
        assert!(!ConnectionState::Idle.is_active());
        assert!(ConnectionState::Connecting.is_connecting());
        assert!(ConnectionState::Reconnecting.is_connecting());
    }

    #[test]
    fn test_connection_info() {
        let config = SshConnectionConfig::new(
            "test.com",
            "user",
            AuthMethod::Agent,
        );

        let mut info = ConnectionInfo::new(config);
        assert_eq!(info.state, ConnectionState::Idle);

        info.set_state(ConnectionState::Connected);
        assert!(info.connected_at.is_some());
        assert!(info.connection_duration().is_some());
    }

    #[test]
    fn test_connection_manager() {
        let manager = SshConnectionManager::new();
        assert_eq!(manager.active_connection_count(), 0);
    }
}

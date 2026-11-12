//! 守护进程的服务端：监听 socket、接受连接、分发请求。
//!
//! # 线程模型
//!
//! 一条**接受线程**（阻塞在 `accept`）+ 每条连接**两条线程**（读、写）。
//! 写线程是必需的：终端的输出由终端自己的线程产生，而 socket 写入可能阻塞
//! （客户端卡住时内核缓冲会满），不能让终端线程被一个卡住的客户端拖死。
//!
//! 连接读线程解析控制请求与输入帧；写线程从 `async_channel` 取帧写出。
//! 客户端断开时两边都要能干净退出——否则守护进程会攒下一堆僵尸线程。

use std::sync::Arc;

use serde_json::{Value, json};

use super::registry::{Registry, SpawnRequest};
use super::terminal::State;

/// 一个连接上某终端的挂接信息。
struct Attachment {
    address: String,
    subscriber_id: u64,
}

/// 服务端状态：注册表 + 关停标志。
pub struct Server {
    pub registry: Arc<Registry>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Server {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 请求关闭服务端（守护进程退出）。
    ///
    /// **注意**：这会结束所有终端。正常退出不应该是"关 socket"，
    /// 而应该是让守护进程在没终端时自然退出。
    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 处理一条控制请求，返回要发回的 JSON。
///
/// 纯函数式：除了注册表之外没有副作用，方便测试。
pub fn handle_request(registry: &Registry, request: &Value) -> Value {
    let op = request.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "ping" => json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}),

        "list" => {
            let terminals: Vec<Value> = registry
                .list()
                .iter()
                .map(|handle| {
                    json!({
                        "address": handle.address,
                        "state": match handle.state() {
                            State::Live => "live",
                            State::Finished => "finished",
                        },
                        "exit_code": handle.exit_code(),
                        "cwd": handle.cwd,
                        "created_ms": handle.created_ms,
                        "bytes_out": handle.bytes_out(),
                    })
                })
                .collect();
            json!({"ok": true, "terminals": terminals})
        }

        "get" => {
            let Some(address) = request.get("address").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "缺少 address"});
            };
            match registry.get(address) {
                Some(handle) => json!({
                    "ok": true,
                    "terminal": {
                        "address": handle.address,
                        "state": match handle.state() {
                            State::Live => "live",
                            State::Finished => "finished",
                        },
                        "exit_code": handle.exit_code(),
                        "cwd": handle.cwd,
                        "created_ms": handle.created_ms,
                    }
                }),
                None => json!({"ok": false, "error": "没有这个终端", "kind": "not_found"}),
            }
        }

        "remove" => {
            let Some(address) = request.get("address").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "缺少 address"});
            };
            if registry.remove(address) {
                json!({"ok": true})
            } else {
                json!({"ok": false, "error": "终端不存在或仍在运行", "kind": "not_removable"})
            }
        }

        "reap" => json!({"ok": true, "removed": registry.reap()}),

        "snapshot" => {
            let Some(address) = request.get("address").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "缺少 address"});
            };
            match registry.get(address) {
                Some(handle) => {
                    let bytes = handle.snapshot();
                    json!({
                        "ok": true,
                        "bytes": bytes.len(),
                        "data": String::from_utf8_lossy(&bytes),
                    })
                }
                None => json!({"ok": false, "error": "没有这个终端", "kind": "not_found"}),
            }
        }

        _ => json!({"ok": false, "error": format!("未知的请求 {op:?}")}),
    }
}

/// 起一个终端。与 `handle_request` 分开是因为它要创建资源，失败路径更啰嗦。
pub fn handle_spawn(registry: &Registry, request: &Value) -> Value {
    let shell_program = request
        .get("shell")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let args: Vec<String> = request
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let cwd = request.get("cwd").and_then(Value::as_str).map(Into::into);
    let cols = request
        .get("cols")
        .and_then(Value::as_u64)
        .unwrap_or(super::registry::DEFAULT_COLS as u64) as u16;
    let rows = request
        .get("rows")
        .and_then(Value::as_u64)
        .unwrap_or(super::registry::DEFAULT_ROWS as u64) as u16;

    // shell integration 必须与本地终端走**同一套**准备逻辑：否则守护进程
    // 起出来的 shell 拿不到 OSC 7 上报，`cd` 之后文件树不跟随、提示符标记
    // 收不到，用户会觉得"开了持久化之后功能少了一半"。
    let mut env = crate::terminal::mappings::default_terminal_env();
    let shell = match shell_program {
        Some(program) => {
            crate::terminal::terminal::configure_local_shell_integration(program, args, &mut env)
        }
        // 没指定 shell 就用本机默认的。
        None => match default_shell_program() {
            Some(program) => crate::terminal::terminal::configure_local_shell_integration(
                program,
                Vec::new(),
                &mut env,
            ),
            None => return json!({"ok": false, "error": "找不到可用的 shell"}),
        },
    };

    let mut spawn = SpawnRequest::new(shell);
    spawn.cwd = cwd;
    spawn.env = env;
    spawn.cols = cols;
    spawn.rows = rows;

    match registry.spawn(spawn) {
        Ok(handle) => json!({"ok": true, "address": handle.address}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// 本机的默认 shell 程序路径。
///
/// 只返回程序名，不返回 `tty::Shell`：调用方还要拿它去套 shell integration。
fn default_shell_program() -> Option<String> {
    #[cfg(windows)]
    {
        return Some(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()));
    }
    #[cfg(not(windows))]
    {
        Some(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new()
    }

    /// ping 必须回版本，客户端靠它判断对面是否可用。
    #[test]
    fn ping_reports_the_version() {
        let response = handle_request(&registry(), &json!({"op": "ping"}));
        assert_eq!(response["ok"], true);
        assert_eq!(response["version"], env!("CARGO_PKG_VERSION"));
    }

    /// 空注册表 list 返回空数组而不是缺失字段。
    #[test]
    fn list_on_an_empty_registry_returns_an_empty_array() {
        let response = handle_request(&registry(), &json!({"op": "list"}));
        assert_eq!(response["ok"], true);
        assert!(
            response["terminals"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
            "terminals 应是空数组: {response}"
        );
    }

    /// 查询不存在的终端要给出可判别的 kind，客户端据此决定是否重建。
    #[test]
    fn getting_an_unknown_terminal_reports_not_found() {
        let response = handle_request(&registry(), &json!({"op": "get", "address": "t99"}));
        assert_eq!(response["ok"], false);
        assert_eq!(response["kind"], "not_found");
    }

    /// 缺少 address 时要报错而不是 panic 或静默成功。
    #[test]
    fn requests_without_an_address_are_rejected() {
        for op in ["get", "remove", "snapshot"] {
            let response = handle_request(&registry(), &json!({"op": op}));
            assert_eq!(response["ok"], false, "{op} 应拒绝");
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("address")),
                "{op} 的错误要说清缺 address: {response}"
            );
        }
    }

    /// 未知 op 必须明确报错，不能当成成功。
    #[test]
    fn unknown_operations_are_reported() {
        let response = handle_request(&registry(), &json!({"op": "explode"}));
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("explode"))
        );
    }

    /// 缺 op 也不能崩。
    #[test]
    fn a_request_without_an_op_is_an_error_not_a_panic() {
        let response = handle_request(&registry(), &json!({}));
        assert_eq!(response["ok"], false);
    }

    /// 默认 shell 必须能解析出来，否则守护进程起不了任何终端。
    #[test]
    fn a_default_shell_is_available() {
        assert!(default_shell_program().is_some(), "本机应能找到默认 shell");
    }
}

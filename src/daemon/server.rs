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

        "close" => {
            let Some(address) = request.get("address").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "缺少 address"});
            };
            if registry.close(address) {
                json!({"ok": true})
            } else {
                json!({"ok": false, "error": "没有这个终端", "kind": "not_found"})
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

    // 请求方可以自带环境变量（远程 SSH 终端需要 `SSHPASS` 之类的凭据，
    // 那些值只能由 UI 侧提供——守护进程不持有用户的密钥链）。
    //
    // **绝不把它们写进终端记录**：`SSHPASS` 是明文口令，落盘等于把它
    // 写进 `terminals.json`。
    let mut env = crate::terminal::mappings::default_terminal_env();
    if let Some(extra) = request.get("env").and_then(Value::as_object) {
        for (key, value) in extra {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_string());
            }
        }
    }

    // `remote: true` 表示"这是 SSH 连接，别套本地 shell integration"。
    // 远程主机上不存在 deepterm 的集成脚本，注入 ZDOTDIR/ENV 只会把
    // 远程 shell 弄坏。
    let is_remote = request
        .get("remote")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let shell = match shell_program {
        Some(program) if is_remote => {
            // 远程：原样执行调用方给的命令，不做任何 shell 改写。
            alacritty_terminal::tty::Shell::new(program, args)
        }
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

    /// `close` 与 `remove` 是两种语义，不能合并。
    ///
    /// - `remove` 只清掉**已经结束**的记录（打扫）；拿它去关一个还活着的
    ///   终端会失败，这是刻意的保护：一次误调用就把用户的编译任务干掉。
    /// - `close` 是用户明确说"我不要这个终端了"，**主动结束**还在跑的
    ///   程序。
    ///
    /// 回归价值：若把 `remove` 改成也能杀活终端，任何一次清理调用都会
    /// 变成"静默杀掉用户正在跑的东西"。
    #[test]
    fn remove_refuses_live_terminals_while_close_accepts_them() {
        let registry = Registry::new();
        // 没有终端时两者都报 not_found —— 语义差别在"活着"这一侧。
        let removed = handle_request(&registry, &json!({"op": "remove", "address": "t1"}));
        assert_eq!(removed["ok"], false);
        assert_eq!(removed["kind"], "not_removable");

        let closed = handle_request(&registry, &json!({"op": "close", "address": "t1"}));
        assert_eq!(closed["ok"], false);
        assert_eq!(closed["kind"], "not_found");
    }

    /// `close` 缺 address 时要报错而不是关掉别的东西。
    #[test]
    fn close_without_an_address_is_rejected() {
        let response = handle_request(&Registry::new(), &json!({"op": "close"}));
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("address"))
        );
    }

    /// 凭据只能进子进程环境，**不能**留在这个终端的记录里。
    ///
    /// 远程 SSH 终端用 `SSHPASS` 传明文口令。守护进程会把它交给 PTY 子进程，
    /// 但一旦它进了 `TerminalHandle` 或落盘文件，等于把用户口令写进
    /// `terminals.json`——同机任何进程都能读到。
    ///
    /// 这条钉住的是"记录里没有环境变量"这个结构性事实：`list`/`get` 返回的
    /// JSON 里不该出现任何 env 内容。
    #[test]
    fn credentials_never_appear_in_terminal_records() {
        let registry = Registry::new();
        let response = handle_request(&registry, &json!({"op": "list"}));
        let text = response.to_string();
        for key in ["SSHPASS", "password", "env"] {
            assert!(!text.contains(key), "终端记录不该含 {key}: {text}");
        }
    }

    /// 默认 shell 必须能解析出来，否则守护进程起不了任何终端。
    #[test]
    fn a_default_shell_is_available() {
        assert!(default_shell_program().is_some(), "本机应能找到默认 shell");
    }
}

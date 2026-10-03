//! 项目根探测 + 服务器实例管理。
//!
//! `LspManager` 是 gpui 全局（`Global`），按「项目根 + 语言」复用服务器
//! 进程：同一项目里开十个 Rust 文件只启动一个 rust-analyzer。
//!
//! 服务器进程是有状态的（它持有自己那一份文档文本），所以复用不是优化
//! 而是正确性要求——两个进程各持一份会导致诊断与补全对不上。
//!
//! **为什么状态放在 `Arc<Mutex<..>>` 而不是 gpui 全局里**：gpui 的 `App`
//! 不是 `Send`，后台启动任务（spawn + initialize 握手）无法访问它。所以
//! 真正的服务器表放在一个 `Send + Sync` 的共享结构里，`LspManager` 只是
//! 它的 gpui 门面。这与 Zed 把 `LspStore` 做成 entity、内部状态分开存放
//! 是同一个约束下的同一解法。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Global};

use crate::lsp::client::LspClient;
use crate::lsp::registry::{LspContent, server_for_language};

/// 项目根的标记文件：命中任一个即认定该目录为根。
const ROOT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "CMakeLists.txt",
    "Makefile",
    "tsconfig.json",
    "jsconfig.json",
    "composer.json",
    "Gemfile",
    "deno.json",
    "build.zig",
    "flake.nix",
];

/// 向上探测项目根。
///
/// 从 `path` 所在目录开始逐级向上，返回第一个含标记文件的目录。
/// 找不到标记时回退到**文件所在目录**——这样单个游离文件仍有 LSP
/// （服务器以该文件所在目录为根，能工作但不跨文件），而不是完全没有。
pub fn project_root(path: &Path) -> PathBuf {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
    };

    let mut current = dir.clone();
    loop {
        for marker in ROOT_MARKERS {
            if current.join(marker).exists() {
                return current.clone();
            }
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    dir
}

/// 服务器实例的键：项目根 + 语言。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub root: PathBuf,
    pub language: String,
}

/// 后台可见的服务器表。
///
/// 全部字段在 `Mutex` 下，`Send + Sync`，可被 `smol::spawn` 的任务直接访问。
#[derive(Default)]
struct ServerTable {
    /// 已就绪的服务器。
    ready: HashMap<ServerKey, Arc<LspClient>>,
    /// 启动失败的 key：不再重试。避免 `rust-analyzer` 不存在时，
    /// 每开一个 Rust 文件都白 spawn 一次进程。
    failed: HashSet<ServerKey>,
    /// 正在启动的 key → 等待结果的通知 channel。并发开同语言文件时
    /// 只 spawn 一次，其余排队。
    starting: HashMap<ServerKey, Vec<async_channel::Sender<Option<Arc<LspClient>>>>>,
}

impl ServerTable {
    /// 已就绪且进程存活的服务器。
    fn get(&self, key: &ServerKey) -> Option<Arc<LspClient>> {
        self.ready.get(key).filter(|c| !c.is_closed()).cloned()
    }

    /// 启动完成：写入结果并唤醒全部等待者。
    fn finish(&mut self, key: ServerKey, client: Option<Arc<LspClient>>) {
        let waiters = self.starting.remove(&key).unwrap_or_default();
        match client {
            Some(client) => {
                self.ready.insert(key, client.clone());
                for tx in waiters {
                    let _ = tx.try_send(Some(client.clone()));
                }
            }
            None => {
                self.failed.insert(key);
                for tx in waiters {
                    let _ = tx.try_send(None);
                }
            }
        }
    }
}

/// LSP 全局管理器（gpui 门面）。
///
/// 实际状态在 [`LspManager::table`] 里，这一层只提供 gpui 侧的入口。
pub struct LspManager {
    table: Arc<parking_lot::Mutex<ServerTable>>,
}

impl Global for LspManager {}

impl LspManager {
    pub fn init(cx: &mut App) {
        cx.set_global(Self {
            table: Arc::new(parking_lot::Mutex::new(ServerTable::default())),
        });
    }

    /// 取（必要时启动）某文件对应语言的服务器。
    ///
    /// 返回 `None` 表示：该语言无可用服务器、已被显式禁用、或之前启动失败。
    /// 调用方降级为「无 LSP」即可。
    ///
    /// 返回 `Some(rx)` 时，结果通过 channel 送达——启动握手可能耗时数百毫秒，
    /// 不能在 UI 线程上等。
    pub fn server_for_file(
        cx: &mut App,
        path: &Path,
        language: &str,
    ) -> Option<async_channel::Receiver<Option<Arc<LspClient>>>> {
        let settings: LspContent = cx
            .try_global::<crate::settings::SettingsStore>()
            .map(|s| s.content().lsp.clone())
            .unwrap_or_else(LspContent::default_enabled);

        let config = server_for_language(language, Some(&settings))?;
        let root = project_root(path);
        let key = ServerKey {
            root,
            language: language.to_string(),
        };

        let table = cx.global::<Self>().table.clone();

        // 已就绪：立即投递，不起任务。
        {
            let mut guard = lock(&table);
            if let Some(client) = guard.get(&key) {
                let (tx, rx) = async_channel::bounded(1);
                let _ = tx.try_send(Some(client));
                return Some(rx);
            }
            // 进程已死（崩溃/被杀）：清掉旧记录，允许重启。
            if guard.ready.contains_key(&key) {
                guard.ready.remove(&key);
            }
            if guard.failed.contains(&key) {
                return None;
            }
        }

        let (tx, rx) = async_channel::bounded(1);
        let should_spawn = {
            let mut guard = lock(&table);
            match guard.starting.get_mut(&key) {
                Some(waiters) => {
                    waiters.push(tx);
                    false
                }
                None => {
                    guard.starting.insert(key.clone(), vec![tx]);
                    true
                }
            }
        };

        if should_spawn {
            let table2 = table.clone();
            let key2 = key.clone();
            let program = config.command.clone();
            let args = config.args.clone();
            smol::spawn(async move {
                let result = LspClient::start(&program, &args, key2.root.clone()).await;
                let client = match result {
                    Ok(client) => {
                        log::info!("lsp: started `{program}` for {}", key2.language);
                        Some(Arc::new(client))
                    }
                    Err(e) => {
                        // 降级：只记日志。服务器缺失是常态，不能弹窗打断编辑。
                        log::error!(
                            "lsp: failed to start `{program}` for {}: {e}",
                            key2.language
                        );
                        None
                    }
                };
                lock(&table2).finish(key2, client);
            })
            .detach();
        }

        Some(rx)
    }

    /// 同步查询已就绪的服务器（无则 `None`）。
    pub fn get(&self, root: &Path, language: &str) -> Option<Arc<LspClient>> {
        lock(&self.table).get(&ServerKey {
            root: root.to_path_buf(),
            language: language.to_string(),
        })
    }

    /// 清除失败记录。
    ///
    /// 设置变更后调用：用户改了命令路径，之前的失败不该永久生效。
    pub fn clear_failures(cx: &mut App) {
        lock(&cx.global::<Self>().table).failed.clear();
    }

    /// 关闭全部服务器进程（应用退出或设置里全局禁用时）。
    pub fn shutdown_all(cx: &mut App) {
        let mut guard = lock(&cx.global::<Self>().table);
        for (_, client) in guard.ready.drain() {
            client.shutdown();
        }
        guard.starting.clear();
    }
}

/// 取锁。
///
/// `parking_lot` 的锁不带毒化语义——持锁线程 panic 时数据可能处于半更新
/// 状态，但锁本身仍可用。这里不做额外处理：表内只有「服务器句柄」这类
/// 可丢弃的状态，最坏情况是重试一次启动，而不是崩溃。
fn lock(table: &Arc<parking_lot::Mutex<ServerTable>>) -> parking_lot::MutexGuard<'_, ServerTable> {
    table.lock()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deepterm-lsp-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn root_is_nearest_marker_directory() {
        let base = tmp_dir("root-marker");
        let nested = base.join("src").join("deep").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        // 标记放在中间层：应从文件向上找到它，而不是 base。
        std::fs::write(base.join("src").join("Cargo.toml"), "").unwrap();

        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        assert_eq!(project_root(&file), base.join("src"));
    }

    #[test]
    fn root_prefers_closest_ancestor() {
        let base = tmp_dir("root-closest");
        let inner = base.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::write(base.join(".git").join("HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(inner.join("package.json"), "{}").unwrap();

        let file = inner.join("index.js");
        std::fs::write(&file, "").unwrap();
        // inner 的 package.json 比 base 的 .git 更近。
        assert_eq!(project_root(&file), inner);
    }

    #[test]
    fn root_falls_back_to_file_directory() {
        // 无标记：回退到文件所在目录，让游离文件仍有 LSP。
        let dir = tmp_dir("root-fallback");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hi").unwrap();
        assert_eq!(project_root(&file), dir);
    }

    #[test]
    fn root_of_marked_directory_returns_itself() {
        let dir = tmp_dir("root-dir");
        std::fs::write(dir.join("go.mod"), "module x").unwrap();
        assert_eq!(project_root(&dir), dir);
    }

    #[test]
    fn root_never_panics_on_filesystem_root() {
        let _ = project_root(Path::new("/"));
    }

    #[test]
    fn table_finish_marks_failure_and_wakes_waiters() {
        let table = Arc::new(parking_lot::Mutex::new(ServerTable::default()));
        let key = ServerKey {
            root: PathBuf::from("/tmp/x"),
            language: "rust".into(),
        };
        let (tx, rx) = async_channel::bounded(1);
        table.lock().starting.insert(key.clone(), vec![tx]);

        table.lock().finish(key.clone(), None);

        assert!(rx.try_recv().unwrap().is_none());
        assert!(table.lock().failed.contains(&key));
        assert!(table.lock().starting.is_empty());
    }

    #[test]
    fn table_get_filters_out_dead_clients() {
        // `is_closed` 在连接未真正建立时也为 false，这里只验证
        // 「表里没有 → None」这一分支，避免构造真实进程。
        let table = ServerTable::default();
        let key = ServerKey {
            root: PathBuf::from("/tmp/y"),
            language: "go".into(),
        };
        assert!(table.get(&key).is_none());
    }
}

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

/// 一个服务器进程的运行状态。
///
/// 状态栏和控制菜单共用这一份枚举：菜单里的圆点颜色、可用操作都从它推。
/// 对照 Zed 的 `BinaryStatus` / `ServerHealth` 两个维度，这里合成一个——
/// DeepTerm 一个 key 只有一个进程，没有 Zed 的"下载二进制"那几步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    /// 进程活着，握手已完成。
    Running,
    /// 进程已 spawn，`initialize` 握手中。
    Starting,
    /// 用户主动停掉的。
    Stopped,
    /// 启动失败（二进制不存在、握手超时等）。
    Failed,
}

/// 一个服务器进程的快照，供 UI 列举（菜单 / 状态栏）。
///
/// 之所以拷贝成不依赖锁的结构：UI 渲染不能持着 `parking_lot` 的 guard，
/// 而 `Arc<LspClient>` 又只该被用来发请求，不该被 UI 攥着。
#[derive(Debug, Clone)]
pub struct ServerSnapshot {
    pub key: ServerKey,
    /// 最终生效的启动命令名（`rust-analyzer` / `gopls` …）。
    pub command: String,
    pub state: ServerState,
    /// 服务器自报的 pid（`true` 表示进程仍活着）。
    pub pid: Option<u32>,
    /// 服务器自报的版本，如 "1.98.0-nightly"。
    pub version: Option<String>,
}

impl ServerSnapshot {
    /// 是否在跑（菜单据此决定「启动」还是「停止」可用）。
    pub fn is_running(&self) -> bool {
        self.state == ServerState::Running
    }
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
    /// 被用户手动停掉的 key：`server_for_file` 直接返回 `None`。
    ///
    /// 对照 Zed：`LocalLspStore::stopped_language_servers` +
    /// `all_language_servers_stopped`。没有这张表的话，关掉服务器后
    /// 【换个标签再切回来】就会把它重新拉起来——用户的「停止」活不过一次
    /// 切换，等同于没这个功能。
    stopped: HashSet<ServerKey>,
}

impl ServerTable {
    /// 已就绪且进程存活的服务器。
    fn get(&self, key: &ServerKey) -> Option<Arc<LspClient>> {
        self.ready.get(key).filter(|c| !c.is_closed()).cloned()
    }

    /// 停掉一个已就绪的服务器，并把它标记为「用户已停止」。
    ///
    /// 返回被停掉的客户端（调用方负责发关闭握手）；本来就没在跑时返回
    /// `None`——这种情况下**仍然**要写 `stopped` 标记：用户按「停止」时
    /// 服务器可能正在握手（`starting` 里），不放标记会在他松手那一刻被
    /// 拉起。
    fn stop(&mut self, key: &ServerKey) -> Option<Arc<LspClient>> {
        self.stopped.insert(key.clone());
        self.starting.remove(key);
        self.ready.remove(key)
    }

    /// 是否被用户停掉。
    fn is_stopped(&self, key: &ServerKey) -> bool {
        self.stopped.contains(key)
    }

    /// 解除停止/失败标记，允许重新拉起。
    ///
    /// 「启动」「重启」「重启全部」都走这里。两条标记都要清：`failed`
    /// 是上次启动失败的疤痕，用户改了命令路径后本就不该再生效；
    /// `stopped` 则是本次要解开的。
    fn allow_start(&mut self, key: &ServerKey) {
        self.stopped.remove(key);
        self.failed.remove(key);
    }

    /// 启动完成：写入结果并唤醒全部等待者。
    ///
    /// **`stopped` 优先于结果**：启动是异步的，用户在握手期间按「停止」后
    /// 这里才收到结果。此时必须把这个「迟到」的进程丢掉，否则它会悄悄回到
    /// `ready` 里，下次打开文件直接被复用——停止白按。
    fn finish(&mut self, key: ServerKey, client: Option<Arc<LspClient>>) {
        if !self.accepts_start(&key) {
            log::info!(
                "lsp: discarding late-started `{language}` for {root:?} (stopped by user)",
                language = key.language,
                root = key.root,
            );
            self.starting.remove(&key);
            return;
        }
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

    /// 这次启动的结果能不能落地。
    ///
    /// 抽成纯判定是为了可测：`finish` 要一个真实的 `LspClient` 才能在
    /// 单测里跑到那一步，而这个「停止」判错正是最贵的那种 bug（用户必须
    /// 重启应用才能把服务器停下来）。
    fn accepts_start(&self, key: &ServerKey) -> bool {
        !self.stopped.contains(key)
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
        Self::server_for_key(cx, &server_key_for(path, language))
    }

    /// 按 key 取（必要时启动）服务器。
    ///
    /// 与 [`Self::server_for_file`] 分成两层：菜单要操作的服务器不一定对应
    /// 「当前打开的文件」，而项目根 + 语言才是进程的唯一标识。
    ///
    /// 返回 `None` 表示：该语言无可用服务器、已被显式禁用、用户手动停掉、
    /// 或之前启动失败。调用方降级为「无 LSP」即可。
    ///
    /// 返回 `Some(rx)` 时，结果通过 channel 送达——启动握手可能耗时数百毫秒，
    /// 不能在 UI 线程上等。
    pub fn server_for_key(
        cx: &mut App,
        key: &ServerKey,
    ) -> Option<async_channel::Receiver<Option<Arc<LspClient>>>> {
        let settings: LspContent = cx
            .try_global::<crate::settings::SettingsStore>()
            .map(|s| s.content().lsp.clone())
            .unwrap_or_else(LspContent::default_enabled);

        let config = server_for_language(&key.language, Some(&settings))?;

        let table = cx.global::<Self>().table.clone();

        // 用户按过「停止」：不再自动拉起。这一条必须排在 `ready` 查询**之前**，
        // 否则「停止 → 换标签再切回来」会把服务器复活。
        if lock(&table).is_stopped(key) {
            log::debug!(
                "lsp: {} for {:?} is stopped by user, not starting",
                key.language,
                key.root
            );
            return None;
        }

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
            // 日志 sink 在 spawn 前就建好：`initialize` 握手那几帧要能看到。
            // 挂在连接上而不是客户端上——发送与接收都发生在 `Connection` 里。
            let logger = crate::lsp::log::sink_for(&key2);
            smol::spawn(async move {
                let result =
                    LspClient::start_with_logger(&program, &args, key2.root.clone(), Some(logger))
                        .await;
                let client = match result {
                    Ok(client) => {
                        log::info!("lsp: started `{program}` for {}", key2.language);
                        Some(Arc::new(client))
                    }
                    Err(e) => {
                        // 降级：只记日志。服务器缺失是常态，不能弹窗打断编辑。
                        //
                        // `{e:#}` 而不是 `{e}`：后者只打最外层的上下文
                        // （`failed to spawn language server \`rust-analyzer\``），
                        // 把真正的原因（`No such file or directory`——即命令不在
                        // `PATH` 里）藏在 source chain 里，排查时看不出是缺二进制
                        // 还是别的问题。
                        log::error!(
                            "lsp: failed to start `{program}` for {}: {e:#}",
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

    /// 某语言最终生效的启动命令（无可用服务器时为 `None`）。
    ///
    /// 供失败提示「指名道姓」用：只说「rust 服务器启动失败」，用户不知道
    /// 缺的是哪个二进制、该装什么。给出 `rust-analyzer` 就能直接对照。
    pub fn configured_command(cx: &App, language: &str) -> Option<String> {
        let settings: LspContent = cx
            .try_global::<crate::settings::SettingsStore>()
            .map(|s| s.content().lsp.clone())
            .unwrap_or_else(LspContent::default_enabled);
        server_for_language(language, Some(&settings)).map(|c| c.command)
    }

    /// 列举当前已知的全部服务器进程。
    ///
    /// 顺序：跑着的在前，启动中、已停止、失败的排后面；同状态内按语言名排，
    /// 让菜单每次刷新不要跳。
    ///
    /// **只在用户打开菜单时调一次**：这里会持有锁并从每个 client 读
    /// pid / 版本，虽然很快，但不该出现在 `render` 路径上。
    pub fn snapshots(cx: &App) -> Vec<ServerSnapshot> {
        let table = cx.global::<Self>().table.clone();
        let settings: LspContent = cx
            .try_global::<crate::settings::SettingsStore>()
            .map(|s| s.content().lsp.clone())
            .unwrap_or_else(LspContent::default_enabled);

        let guard = lock(&table);
        let mut out: Vec<ServerSnapshot> = Vec::new();
        let mut push = |key: &ServerKey, state: ServerState, client: Option<&Arc<LspClient>>| {
            let command = server_for_language(&key.language, Some(&settings))
                .map(|c| c.command)
                .unwrap_or_default();
            out.push(ServerSnapshot {
                pid: client.and_then(|c| c.connection_pid()),
                version: client.and_then(|c| {
                    c.capabilities()
                        .server_info
                        .as_ref()
                        .and_then(|i| i.version.clone())
                }),
                key: key.clone(),
                command,
                state,
            });
        };

        for (key, client) in &guard.ready {
            // `ready` 里可能留着刚崩溃的进程（`server_for_file` 才清）。
            // 如实报成 Failed，别让用户点一个已经死了的「停止」。
            if client.is_closed() {
                push(key, ServerState::Failed, None);
            } else {
                push(key, ServerState::Running, Some(client));
            }
        }
        for key in guard.starting.keys() {
            push(key, ServerState::Starting, None);
        }
        for key in &guard.stopped {
            push(key, ServerState::Stopped, None);
        }
        for key in &guard.failed {
            push(key, ServerState::Failed, None);
        }
        // 同一 key 可能同时出现在 ready 和 stopped（停止后未被摘干净）：
        // 按 key 去重，保留先 push 的（优先级即上面的顺序）。
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| seen.insert(s.key.clone()));
        out.sort_by(|a, b| {
            state_rank(a.state)
                .cmp(&state_rank(b.state))
                .then_with(|| a.key.language.cmp(&b.key.language))
                .then_with(|| a.key.root.cmp(&b.key.root))
        });
        out
    }

    /// 按 key 停服务器。菜单里操作的是 key（不依赖当前打开哪个文件）。
    pub fn stop_server_for_key(cx: &App, key: &ServerKey) -> bool {
        let client = lock(&cx.global::<Self>().table).stop(key);
        if let Some(client) = client {
            log::info!(
                "lsp: stopped `{}` for {:?} (user request)",
                key.language,
                key.root
            );
            client.shutdown();
            true
        } else {
            log::debug!(
                "lsp: no running `{}` server for {:?}, marking stopped",
                key.language,
                key.root
            );
            false
        }
    }

    /// 按 key 解除停止/失败标记。
    pub fn resume_server_for_key(cx: &App, key: &ServerKey) {
        lock(&cx.global::<Self>().table).allow_start(key);
    }

    /// 停掉全部服务器。
    ///
    /// 返回停掉的进程数——「停止全部」在没有任何服务器跑着时是个空操作，
    /// 调用方拿这个数字决定要不要弹提示。
    pub fn stop_all(cx: &mut App) -> usize {
        let mut guard = lock(&cx.global::<Self>().table);
        let keys: Vec<ServerKey> = guard.ready.keys().cloned().collect();
        let mut stopped = 0;
        for key in keys {
            if let Some(client) = guard.stop(&key) {
                client.shutdown();
                stopped += 1;
            }
        }
        log::info!("lsp: stopped all servers ({stopped}) (user request)");
        stopped
    }

    /// 关闭全部服务器进程（应用退出或设置里全局禁用时）。
    ///
    /// 与 [`Self::stop_all`] 的区别：这是**应用生命周期**的收尾，不写
    /// `stopped` 标记（进程都要没了，标记没有意义）。
    pub fn shutdown_all(cx: &mut App) {
        let mut guard = lock(&cx.global::<Self>().table);
        for (_, client) in guard.ready.drain() {
            client.shutdown();
        }
        guard.starting.clear();
        guard.stopped.clear();
    }
}

/// 由文件路径与语言构造服务器键（项目根探测 + 语言）。
fn server_key_for(path: &Path, language: &str) -> ServerKey {
    ServerKey {
        root: project_root(path),
        language: language.to_string(),
    }
}

/// 菜单里的排序权重：用户关心的排在前面。
///
/// 跑着的服务器是菜单的主要内容；已停止的排最后面——它们只是"可以再启动"
/// 的记录，不该占据最显眼的位置。
fn state_rank(state: ServerState) -> u8 {
    match state {
        ServerState::Running => 0,
        ServerState::Starting => 1,
        ServerState::Failed => 2,
        ServerState::Stopped => 3,
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

    #[test]
    fn stop_marks_key_stopped() {
        let mut table = ServerTable::default();
        let key = ServerKey {
            root: PathBuf::from("/tmp/stop-a"),
            language: "rust".into(),
        };
        // 没在跑也返回 None，但标记必须写下去——否则「停止」之后
        // 一次标签切换就把服务器悄悄拉起来了。
        assert!(table.stop(&key).is_none());
        assert!(table.is_stopped(&key));
    }

    #[test]
    fn allow_start_clears_both_stopped_and_failed() {
        let mut table = ServerTable::default();
        let key = ServerKey {
            root: PathBuf::from("/tmp/stop-b"),
            language: "rust".into(),
        };
        table.stop(&key);
        table.failed.insert(key.clone());

        table.allow_start(&key);

        assert!(!table.is_stopped(&key));
        assert!(!table.failed.contains(&key));
    }

    /// 回归：启动握手是异步的，用户在等待期间按「停止」。
    ///
    /// 旧实现里 `finish` 无条件写 `ready`，这个迟到的进程会回到表里被复用
    /// ——停止等于没按。现在 `stopped` 必须优先于启动结果。
    #[test]
    fn late_start_after_user_stop_is_discarded() {
        let mut table = ServerTable::default();
        let key = ServerKey {
            root: PathBuf::from("/tmp/stop-c"),
            language: "rust".into(),
        };
        assert!(
            table.accepts_start(&key),
            "启动前没有 stopped 标记，理应接受"
        );

        // 用户按下停止。
        table.stop(&key);

        assert!(
            !table.accepts_start(&key),
            "握手期间按了停止，迟到的启动结果必须被丢弃"
        );
    }
}

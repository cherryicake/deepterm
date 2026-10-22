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
use crate::lsp::registry::LspContent;

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

/// 项目根探测的最大向上深度。
///
/// 本地探测靠 `parent()` 自然收敛到 `/`，不需要这个上限；远端探测是
/// 一条在远端跑的 `while` 循环（见 [`remote_project_root`]），没有上限的话
/// 一个深层路径要向上问几十层——每层十几个 `test`。
const MAX_ROOT_DEPTH: usize = 12;

/// 探测失败时的根：文件所在目录。
///
/// 抽出来是因为**多个地方**要用同一个口径（本地 `project_root` 的兜底、
/// `remote_project_root` 的兜底、`root_for` 的兜底、编辑器
/// 菜单算 key 的兜底）——口径漂移会让「编辑器算的根」与「服务器表里的键」
/// 对不上，表现为启动了一个服务器却连不上已打开的标签。
pub fn fallback_root(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.to_path_buf();
    }
    path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| path.to_path_buf())
}

/// 向上探测项目根。
///
/// 从 `path` 所在目录开始逐级向上，返回第一个含标记文件的目录。
/// 找不到标记时回退到**文件所在目录**——这样单个游离文件仍有 LSP
/// （服务器以该文件所在目录为根，能工作但不跨文件），而不是完全没有。
pub fn project_root(path: &Path) -> PathBuf {
    let dir = fallback_root(path);

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

/// 在**远端主机上**探测项目根（同步，一次远端命令）。
///
/// 与 [`project_root`] 同样的标记规则，只是存在性判断走远端。网络失败或
/// 拿不到会话时按「找不到标记」处理，最终落回 [`fallback_root`]——这里不
/// 猜更深的目录：猜错根会让两个不同项目共用一台服务器，比退回「文件所在
/// 目录」更糟。因此本函数**总有**结果，口径与本地 `project_root` 一致。
///
/// # 为什么一条命令走完，而不是逐级 SFTP stat
///
/// 逐级探测是 `深度 × 标记数` 次往返（本仓库 17 个标记 × 12 层 ≈ 200 次），
/// 而这个函数在 UI 路径上被调用（算 `ServerKey.root`：开标签、点开菜单、
/// `tabs_for_server_key` 每标签一次）。实测一次 LAN 往返 ~1ms 时那是
/// 0.2s 的卡顿，主机稍慢或跨 WAN 就是几秒的界面冻结，而且**每次**都冻。
/// 改成一条 `while` 脚本后固定一次往返，代价是远端要 fork 几个 `dirname`。
///
/// # 结果按 (主机, 起始目录) 缓存
///
/// 同一项目里开 10 个标签原本要探 10 遍（`tabs_for_server_key` 对每个标签
/// 都算一次根）。会话期内缓存，理由与候选链的探测缓存相同：项目根在一次
/// 编辑会话里不会变，而口径必须稳定——根算不一致会让菜单操作落到一个
/// 不存在的进程上。
/// **探测失败不缓存根**：那是问话失败，不是「这里没有标记」的事实，缓存下来
/// 会把第一次赶上连接未就绪的主机永久判成无根项目（同 registry 的策略）。
/// 但要记下「问过且没问到」（`root_probe_failures_lock`）——调用方靠它区分
/// 「还没问」与「问不到」，见 [`remote_root_is_known`]。
pub fn remote_project_root(config: &crate::ssh::SshConnectionConfig, path: &str) -> PathBuf {
    let dir = fallback_root(std::path::Path::new(path));
    let key = root_cache_key(config, &dir);
    if let Some(hit) = root_cache_lock().get(&key).cloned() {
        return hit;
    }
    // 刚问过且没问到：退回兜底根，不再撞第二次超时。间隔过后自然重试。
    if root_probe_failures_lock()
        .get(&key)
        .is_some_and(|at| at.elapsed() < ROOT_PROBE_RETRY)
    {
        return dir;
    }

    // 同一目录只允许一次探测在飞。后来者在这里等：前一个探测无论成功
    // （写根）还是失败（写时间戳）都会留下结论，下面复查一遍就能直接返回，
    // 不必再打一趟 SSH。等待发生在后台线程（调用方都已改成后台探测）。
    let probe_lock = root_probe_lock(&key);
    let _probing = probe_lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = root_cache_lock().get(&key).cloned() {
        return hit;
    }
    if root_probe_failures_lock()
        .get(&key)
        .is_some_and(|at| at.elapsed() < ROOT_PROBE_RETRY)
    {
        return dir;
    }

    let probed = crate::ssh::sftp_fs::with_cached_session(config, |session| {
        session.execute_command_bytes(&root_probe_script(&dir), 15)
    });
    match probed {
        // 脚本正常跑完并打印了一个目录：那就是根。
        Ok(raw) if raw.success() => {
            match crate::ssh::lsp_channel::first_path_line(&raw.stdout) {
                Some(found) => {
                    let root = PathBuf::from(found);
                    root_cache_lock().insert(key, root.clone());
                    root
                }
                // 跑成了却没输出：与退出码 1 同义（没有标记），走兜底并缓存。
                None => {
                    root_cache_lock().insert(key.clone(), dir.clone());
                    dir
                }
            }
        }
        // 退出码 1：脚本走到头没找到标记。确定性结论——缓存兜底根，
        // 免得每标签每次菜单都重问一遍。
        Ok(_) => {
            root_cache_lock().insert(key.clone(), dir.clone());
            dir
        }
        // 问不到（连不上 / 超时 / shell 缺失）：如实兜底，但**不落缓存**。
        Err(err) => {
            log::debug!("lsp(remote): project root probe failed for {dir:?}: {err}");
            // 记下「问过但没问到」的时刻：否则每次为该目录接标签都要再撞一遍
            // 15s 超时，而 `remote_root_is_known` 会一直回答「不知道」，把接标签
            // 的路径拖成无限期的后台重试。
            root_probe_failures_lock().insert(key, std::time::Instant::now());
            dir
        }
    }
}

/// 这个远端文件的项目根**现在**能不能不联网算出来。
///
/// 为 `false` 时调用方别在 UI 线程上调 [`remote_project_root`]——那是一次最多
/// 15s 的同步往返（主机卡住时就是 15s 的界面冻结）。正确做法是把探测丢到后台，
/// 探完再接这个标签。
///
/// 「最近问过但没问到」的目录也算知道：兜底根就是当下的答案，重试由
/// [`ROOT_PROBE_RETRY`] 间隔管着，不该让调用方为此再等一趟往返。
pub fn remote_root_is_known(config: &crate::ssh::SshConnectionConfig, path: &str) -> bool {
    let dir = fallback_root(std::path::Path::new(path));
    let key = root_cache_key(config, &dir);
    if root_cache_lock().contains_key(&key) {
        return true;
    }
    root_probe_failures_lock()
        .get(&key)
        .is_some_and(|at| at.elapsed() < ROOT_PROBE_RETRY)
}

/// 探测失败后的重试间隔：与候选链探测同一个口径。
///
/// 「问不到」既不是事实（这台机器可能装有标记），也不能每次都当作第一次问——
/// 主机不可达时每个标签、每次菜单点击都会撞一次 15s 超时。记一次时间戳，
/// 间隔内直接返回兜底根，间隔外允许重试。
const ROOT_PROBE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// 项目根**探测失败**的时间戳（键同 [`root_cache_key`]）。
///
/// 与缓存根互斥：这里记的是「问过但没问到」，所以既不当作答案缓存，
/// 也不让调用方以为还得再问一次。理由见 [`remote_project_root`]。
fn root_probe_failures_lock() -> std::sync::MutexGuard<'static, HashMap<String, std::time::Instant>>
{
    use std::sync::{Mutex, OnceLock};
    static FAILURES: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 项目根缓存键：主机维度 + 起始目录。
///
/// 主机维度用 `username@host:port`，与 [`crate::lsp::registry`] 的候选探测
/// 缓存同一个口径：两台机器的同名目录是两个不同的根。
fn root_cache_key(config: &crate::ssh::SshConnectionConfig, dir: &Path) -> String {
    format!("{}|{}", config.connection_string(), dir.display())
}

/// 正在探测中的根（按 `root_cache_key` 串行化）。
///
/// 同一目录下的 N 个标签会各自发起探测（`pending_root_attach` 只按 tab id
/// 去重，同一目录的不同文件仍是不同 tab），而每次探测都是一次 SSH 往返。
/// 让后来者在同一把锁上等：前一个探完把结果（根或失败时间戳）写进缓存，
/// 后来者拿到锁后走「复查缓存」直接返回，不再多打一趟。
///
/// 表里只存 `Weak`：最后一个持有者释放后条目就是死引用，下次插入时清掉。
/// 常态下这张表是空的，不会随目录数增长。
fn root_probe_locks()
-> &'static std::sync::Mutex<HashMap<String, std::sync::Weak<std::sync::Mutex<()>>>> {
    use std::sync::{Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取某个目录的探测锁（没有就建一个）。
fn root_probe_lock(key: &str) -> Arc<std::sync::Mutex<()>> {
    let mut locks = root_probe_locks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = locks.get(key).and_then(std::sync::Weak::upgrade) {
        return existing;
    }
    // 顺手回收死引用，表的大小跟着「同时探测的目录数」走。
    locks.retain(|_, weak| weak.strong_count() > 0);
    let lock = Arc::new(std::sync::Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}

fn root_cache_lock() -> std::sync::MutexGuard<'static, HashMap<String, PathBuf>> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 远端项目根探测脚本（POSIX sh，不依赖 bash）。
///
/// 从起始目录逐级向上，打印第一个含任一标记的目录并以 0 退出；走到根或
/// 超过 [`MAX_ROOT_DEPTH`] 仍以 1 退出——「找过且没有」与「跑不起来」由此
/// 区分开（后者由传输层报错，压根到不了这个退出码）。
fn root_probe_script(dir: &Path) -> String {
    let markers = ROOT_MARKERS
        .iter()
        .map(|m| crate::utils::shell::shell_quote_single(m))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"d={start}
i=0
while : ; do
  for m in {markers}; do
    if [ -e "$d/$m" ]; then printf '%s\n' "$d"; exit 0; fi
  done
  p=$(dirname "$d")
  [ "$p" = "$d" ] && exit 1
  d="$p"
  i=$((i + 1))
  [ "$i" -gt {depth} ] && exit 1
done"#,
        start = crate::utils::shell::shell_quote_single(&dir.to_string_lossy()),
        markers = markers,
        depth = MAX_ROOT_DEPTH,
    )
}

/// 按文件归属算项目根：远端在那台主机上探测，本地走本地探测。
///
/// 唯一的口径来源。编辑器（菜单快照、标签归属）与 `LspManager`
/// （`key_for_file`）都从这里算，三处若各写各的回退，
/// 「编辑器算的根」与「服务器表里的键」会错开——远端一侧尤其危险，
/// 用本地 `project_root` 回退是在**本地磁盘**上找一个远端路径。
pub fn root_for(path: &Path, ssh_config: Option<&crate::ssh::SshConnectionConfig>) -> PathBuf {
    match ssh_config {
        Some(config) => remote_project_root(config, &path.to_string_lossy()),
        None => project_root(path),
    }
}

/// 读 `lsp` 段设置。
///
/// 各处都要读一次（候选链 gate、chip、菜单、快照表），统一走这里是为了
/// 「没有 SettingsStore」时的回退口径只有一个：默认**关闭**。
fn lsp_settings(cx: &App) -> LspContent {
    cx.try_global::<crate::settings::SettingsStore>()
        .map(|s| s.content().lsp.clone())
        .unwrap_or_else(LspContent::default_disabled)
}

/// 服务器实例的键：项目根 + 语言 + （可选的）远端主机。
///
/// `ssh_config` 是 `None` 时表示本地。加上这一维是因为**同一路径在不同
/// 主机上是两个不同的服务器**：两台机器都有 `/home/user/project`，
/// 路径字符串完全相同，但跑的是两个 rust-analyzer 进程、持有两份文档。
/// 只用 `root + language` 做键会让它们互相顶掉（表现为「连了 B 机之后
/// A 机的诊断没了」）。
///
/// `os` 等字段**不参与**键的比较，见 [`host_identity`]。
#[derive(Debug, Clone)]
pub struct ServerKey {
    pub root: PathBuf,
    pub language: String,
    pub ssh_config: Option<crate::ssh::SshConnectionConfig>,
}

/// 服务器进程「跑在哪台机器的哪个账号上」。
///
/// 只取 host + port + username。不用 `SshConnectionConfig` 的整体相等性，
/// 因为它把 `os` 也算进去，而 `os` 是**连上之后探测再写回**的
/// （`workspace.rs` 里 `conn.os = Some(os_id)`）。于是同一个进程空间会有
/// 「探测前」与「探测后」两个键：`HashMap` 找不到已在跑的服务器 → 再起一个，
/// 两个 rust-analyzer 各持一份文档，诊断互相打脸；按 key 匹配已打开标签的
/// 归属（菜单 Start 后接回标签）也会一起失效。
///
/// 昵称、超时、端口转发同理：它们改变**怎么连**，不改变**连到谁**。
/// 认证方式也不算身份——同一主机换一种认证重连，服务器该复用而不是再起一个。
///
/// 标签归属判断（`Editor::tabs_for_server_key`）共用这一份口径：那里若改回
/// 整份配置比较，键这边合了、那边不合，菜单 Start 仍然接不回标签。
pub(crate) fn host_identity(
    cfg: Option<&crate::ssh::SshConnectionConfig>,
) -> Option<(&String, u16, &String)> {
    cfg.map(|c| (&c.host, c.port, &c.username))
}

impl PartialEq for ServerKey {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.language == other.language
            && host_identity(self.ssh_config.as_ref()) == host_identity(other.ssh_config.as_ref())
    }
}

impl Eq for ServerKey {}

/// 口径必须与上面那份 `PartialEq` **逐项一致**，否则违反
/// `a == b ⇒ hash(a) == hash(b)`，`HashMap` 会把相等的键分到两个桶里——
/// 表现正是「表里明明有，查不到」。
impl std::hash::Hash for ServerKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.root.hash(state);
        self.language.hash(state);
        host_identity(self.ssh_config.as_ref()).hash(state);
    }
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
    /// 被用户手动停掉的 key：`server_for_key` 直接返回 `None`。
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

    /// 启动「问不到主机」时的收尾：唤醒等待者，但**不拉黑**。
    ///
    /// 与 `finish(key, None)` 的唯一区别就是不写 `failed`。理由见
    /// [`crate::ssh::lsp_channel::RemoteUnreachable`]：SSH 抖动、认证临时失败、
    /// 探测超时都不是「这台机器上没有这个服务器」的事实，写成事实的话
    /// 一次网络抖动就永久拉黑这个项目的服务器——之后每次开文件都直接失败，
    /// 直到重启应用。
    fn finish_retryable(&mut self, key: ServerKey) {
        log::debug!(
            "lsp: {} for {:?} not reachable; not marking as failed",
            key.language,
            key.root
        );
        let waiters = self.starting.remove(&key).unwrap_or_default();
        for tx in waiters {
            let _ = tx.try_send(None);
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

    /// 某个文件**该归哪个服务器键**（本地与远端同一个口径）。
    ///
    /// 取代原来的 `server_for_file` / `server_for_remote_file` 两个几乎重复的
    /// 入口：两者只在「根怎么算」与「键里带不带主机」上不同，而调用方
    /// （`LspSession::connect`）还需要拿到**算出来的那个根**——只管发请求的
    /// 包装函数给不了。跨文件跳转要靠它把目标文件挂回发起跳转的那个进程
    /// （见 `crate::editor::Editor::lsp_jump_roots`）。
    ///
    /// 远端的根在**那台机器上**探测（经 [`root_for`]）：退回本地
    /// `project_root` 看的是本地磁盘，结果必错。探测是一次同步往返，
    /// 只在「首次为该目录算根」时发生，之后走缓存（见 [`remote_root_is_known`]）。
    pub fn key_for_file(
        path: &Path,
        language: &str,
        ssh_config: Option<crate::ssh::SshConnectionConfig>,
    ) -> ServerKey {
        ServerKey {
            root: root_for(path, ssh_config.as_ref()),
            language: language.to_string(),
            ssh_config,
        }
    }

    /// 按 key 取（必要时启动）服务器。
    ///
    /// 与 [`Self::key_for_file`] 分成两层：菜单要操作的服务器不一定对应
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
        Self::server_for_key_with_override(cx, key, false)
    }

    /// 按 key 取（必要时启动）服务器，`override_disabled` 时无视全局开关。
    ///
    /// # 为什么需要这个开关
    ///
    /// 默认关闭（`lsp.enabled = false`）是产品决策，但用户仍然要能**手动开启**
    /// ——需求原话就是「默认关闭 + 用户可通过菜单开启/关闭」。若菜单的
    /// 「Start Server」也走全局开关，那默认状态下这一项点了没反应：
    /// 用户看到 chip、点开菜单、按启动，什么都没有发生。
    ///
    /// 所以只有**用户显式操作**（菜单 Start / Restart）才传 `true`：
    /// 那是一次明确的授权，不该被全局默认值挡回去。自动路径
    /// （打开文件、切换标签）一律传 `false`，保持默认关闭。
    ///
    /// 语言级的显式禁用（`servers.<lang>.enabled = false` 或空命令）**仍然
    /// 生效**：那是用户针对这门语言的明确选择，与全局默认值不是一回事。
    ///
    /// 实现上把 `override_disabled` 一路传到候选链的 gate，而不是克隆一份
    /// 改过 `enabled` 的设置——这个入口在渲染/菜单路径上会被反复调用。
    pub fn server_for_key_with_override(
        cx: &mut App,
        key: &ServerKey,
        override_disabled: bool,
    ) -> Option<async_channel::Receiver<Option<Arc<LspClient>>>> {
        let settings = lsp_settings(cx);
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
        //
        // **这一步与全局开关无关**：进程是用户显式拉起来的（菜单 Start），
        // 那时已经授权过一次。默认关闭的含义是「不自动起」，不是「起了也要
        // 装作没起」——否则用户在项目里开第二个文件时会另起一个进程，
        // 两个进程各持一份文档，诊断与补全互相错。
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
            // **正在握手的也算「已经有授权了」**：菜单按「Start」时
            // `start_lsp_server_for_key` 先用 `override_disabled = true` 把进程
            // 拉起来，紧接着对每个标签调 `start_lsp_for_tab`，而那条路走的是
            // `override_disabled = false`。若这里只认 `ready` 不认 `starting`，
            // 全局开关（默认关闭）就会在 gate 处把标签挡在门外：进程起了，
            // 但没有任何标签向它 `didOpen`，当前文件依旧没有诊断。Restart 更糟
            // ——`stop_sessions_for_key` 已经把会话拆了，从此再也接不回来。
            // 与上面 `ready` 那一档同理：进程是用户显式拉起来的，那时已经授权
            // 过一次，「默认关闭」是不自动起，不是起了也装作没起。
            if let Some(waiters) = guard.starting.get_mut(&key) {
                // 只登记，不发东西：结果由启动任务完成时统一投递给所有 waiter。
                let (tx, rx) = async_channel::bounded(1);
                waiters.push(tx);
                return Some(rx);
            }
        }

        // 到这里才查语言配置：前面两条（已停止 / 已就绪）与开关无关，
        // 而这个 gate 只该挡「要不要**新起**一个进程」。
        //
        // gate **只看设置，不碰主机**：三种拒绝原因（全局关闭且未授权、
        // 语言级禁用、不在表里）都不需要一次往返，而候选链「装没装」的探测
        // 挪到下面的 spawn 任务里做——这个函数在 UI 线程上被调用（打开文件、
        // 切标签、菜单启动），远端探测是同步 SSH 往返，放在这里就是界面卡顿。
        let candidates =
            crate::lsp::registry::launch_gate(&key.language, &settings, override_disabled);
        if candidates.is_empty() {
            return None;
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
            // 日志 sink 在 spawn 前就建好：`initialize` 握手那几帧要能看到。
            // 挂在连接上而不是客户端上——发送与接收都发生在 `Connection` 里。
            let logger = crate::lsp::log::sink_for(&key2);
            smol::spawn(async move {
                // 按**目标主机**挑候选：远端文件必须问那台机器，否则本地有
                // pyright、远端只有 ruff 时会选中前者并在远端失败。
                // 这一步在后台线程上阻塞是故意的（见上面的 gate）。
                let Some(config) = crate::lsp::registry::pick_from_candidates(
                    &candidates,
                    key2.ssh_config.as_ref(),
                ) else {
                    lock(&table2).finish(key2, None);
                    return;
                };
                let program = config.command.clone();
                let args = config.args.clone();
                // 只标 `user@host`（不带端口）：日志是给人扫的，一台机器的多个
                // 端口转发在服务器归属上没有区别。
                let remote_tag = key2
                    .ssh_config
                    .as_ref()
                    .map(|c| format!(" (remote {}@{})", c.username, c.host))
                    .unwrap_or_default();
                // 本地起子进程，远端经 SSH channel。两者共用同一条握手代码，
                // 差别只在「连接怎么来」（见 `LspClient::start_remote`）。
                let result = match key2.ssh_config.clone() {
                    Some(ssh_config) => {
                        LspClient::start_remote(
                            &ssh_config,
                            &program,
                            &args,
                            key2.root.clone(),
                            Some(logger),
                        )
                        .await
                    }
                    None => {
                        LspClient::start_with_logger(
                            &program,
                            &args,
                            key2.root.clone(),
                            Some(logger),
                        )
                        .await
                    }
                };
                // 「问不到主机」与「这台机器没装」必须分道收尾：前者不写
                // `failed`，否则一次 SSH 抖动就永久拉黑这个项目的服务器。
                let unreachable = matches!(
                    &result,
                    Err(e) if e.downcast_ref::<crate::ssh::lsp_channel::RemoteUnreachable>().is_some()
                );
                let client = match result {
                    Ok(client) => {
                        log::info!(
                            "lsp: started `{program}` for {}{remote_tag}",
                            key2.language
                        );
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
                        //
                        // 远端多一类失败：命令在那台机器上不存在。
                        // `resolve_remote_binary` 已经把原因写进错误消息
                        // （含 user@host 与「去装还是给绝对路径」的建议），
                        // 这里原样透出即可，不要再包一层。
                        log::error!(
                            "lsp: failed to start `{program}` for {}{remote_tag}: {e:#}",
                            key2.language
                        );
                        None
                    }
                };
                if unreachable && client.is_none() {
                    lock(&table2).finish_retryable(key2);
                } else {
                    lock(&table2).finish(key2, client);
                }
            })
            .detach();
        }

        Some(rx)
    }

    /// 某语言在**目标主机**上最终生效的启动命令（无可用服务器时为 `None`）。
    ///
    /// 供失败提示与 chip「指名道姓」用：只说「rust 服务器启动失败」，用户
    /// 不知道缺的是哪个二进制、该装什么。给出 `rust-analyzer` 就能直接对照。
    ///
    /// 状态栏渲染远端标签时必须传 `ssh_config`：否则 chip 上显示的是
    /// **本机**装的那个服务器名，与远端实际跑的不是一个。
    ///
    /// **绕开全局 `enabled` 开关**（`override_disabled = true`）：这里回答的是
    /// 「这个文件归哪个服务器管」，而开关回答的是「现在起没起」——两者混用
    /// 时，默认关闭会让 chip 显示不出服务器名、菜单也只能退回语言名 `python`，
    /// 用户既不知道该接哪个 LSP，也看不出点「启动」会起什么。
    /// 语言级的显式禁用（`enabled: false` / 空命令）仍然返回 `None`。
    ///
    /// **只读缓存，绝不在这里发网络请求**（`Resolve::Cached`）：这个函数在
    /// 状态栏的 `render` 路径上，每帧一次。远端探测是同步 SSH 往返，
    /// 主机不可达时一次要 15s——每帧去问等于把界面冻死。返回 `None` 表示
    /// 「还不知道或确实没有」，编辑器起后台补探（见 [`Self::pending_chain_probe`]）。
    pub fn configured_command_on(
        cx: &App,
        language: &str,
        ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    ) -> Option<String> {
        let settings = lsp_settings(cx);
        crate::lsp::registry::server_for_language_cached(
            language,
            Some(&settings),
            ssh_config,
            true,
        )
        .map(|c| c.command)
    }

    /// 这台主机上还有哪些候选「没探过」——编辑器据此起一次后台补探。
    ///
    /// 返回空表示不需要探（本机、用户显式配了命令、或全部已有确定结论）。
    /// 本函数自身只读缓存，所以可以在开标签/开菜单时随手调。
    pub fn pending_chain_probe(
        cx: &App,
        language: &str,
        ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    ) -> Vec<String> {
        let settings = lsp_settings(cx);
        crate::lsp::registry::pending_chain_probe(language, &settings, ssh_config)
    }

    /// 冷却期里最早还有多久可以重问（远端专用）。
    ///
    /// 待探列表为空**不等于**不用探：主机不可达时那 30 秒的退避窗口内候选
    /// 不会被列出，于是没人再去问。编辑器据此排一次延迟重试。
    pub fn chain_probe_cooldown(
        cx: &App,
        language: &str,
        ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    ) -> Option<std::time::Duration> {
        let settings = lsp_settings(cx);
        crate::lsp::registry::chain_probe_cooldown(language, &settings, ssh_config)
    }

    /// 这门语言**有**可用的服务器吗（不关心此刻起没起），候选链在**目标主机**上问。
    ///
    /// 与 [`Self::configured_command_on`] 的差别只有返回值：这个给「状态栏该不该
    /// 占位」用。默认关闭时 `server_for_language` 返回 `None`，但语言表里
    /// 确实有 rust-analyzer——此时状态栏要显示「未启动」而不是留白，
    /// 否则用户无从得知这个文件本来可以接 LSP。
    ///
    /// 远端标签必须传 `ssh_config`：否则「有没有服务器」按本机 PATH 判定，
    /// 而实际能不能起要看那台机器。两边口径不一致会表现成两种坏结果——
    /// 本机没装、远端装了 → 该显示「未启动」却留白；
    /// 本机装了、远端没装 → 显示「未启动」，用户点了启动后在远端失败。
    ///
    /// 同样**不阻塞**：还没探过的远端答 `true`（宁可先挂出一个可点的 chip，
    /// 也别把唯一的开启入口藏起来），探明全都没有才答 `false`。
    pub fn language_has_server_on(
        cx: &App,
        language: &str,
        ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    ) -> bool {
        let settings = lsp_settings(cx);
        crate::lsp::registry::language_might_have_server_on(&settings, language, ssh_config)
    }

    /// 列举当前已知的全部服务器进程。
    ///
    /// 顺序：跑着的在前，启动中、已停止、失败的排后面；同状态内按语言名排，
    /// 让菜单每次刷新不要跳。
    ///
    /// **只在用户打开菜单时调一次**：这里要读每个 client 的 pid / 版本并解析
    /// 生效命令，不该出现在 `render` 路径上。
    ///
    /// 先在持锁下把表抄成 (key, state, client) 列表，**放锁之后**再解析命令：
    /// 候选链的「装没装」在缓存未命中时会跑 `--version` 子进程或 SSH 探测，
    /// 持着锁做这些 I/O 会把并发的 `server_for_key` / `stop` 全部堵住。
    pub fn snapshots(cx: &App) -> Vec<ServerSnapshot> {
        let table = cx.global::<Self>().table.clone();
        let settings = lsp_settings(cx);

        let entries: Vec<(ServerKey, ServerState, Option<Arc<LspClient>>)> = {
            let guard = lock(&table);
            let mut entries: Vec<(ServerKey, ServerState, Option<Arc<LspClient>>)> = Vec::new();
            for (key, client) in &guard.ready {
                // `ready` 里可能留着刚崩溃的进程（`server_for_key` 才清）。
                // 如实报成 Failed，别让用户点一个已经死了的「停止」。
                if client.is_closed() {
                    entries.push((key.clone(), ServerState::Failed, None));
                } else {
                    entries.push((key.clone(), ServerState::Running, Some(client.clone())));
                }
            }
            for key in guard.starting.keys() {
                entries.push((key.clone(), ServerState::Starting, None));
            }
            for key in &guard.stopped {
                entries.push((key.clone(), ServerState::Stopped, None));
            }
            for key in &guard.failed {
                entries.push((key.clone(), ServerState::Failed, None));
            }
            entries
        };

        let mut out: Vec<ServerSnapshot> = entries
            .into_iter()
            .map(|(key, state, client)| {
                // 名字要**绕开全局开关**：进程已经起在那里（多半是菜单 Start
                // 起的），而默认关闭会让开关把命令判成 `None`，菜单里那一行就
                // 只剩语言名——用户看不出这是哪个进程。
                // 同样只读缓存（`Cached`）：这个函数在点开菜单时调用，远端
                // 探测不能在这儿阻塞。启动路径自己探过，所以跑着的服务器
                // 一定有确定答案。
                let command = crate::lsp::registry::server_for_language_cached(
                    &key.language,
                    Some(&settings),
                    key.ssh_config.as_ref(),
                    true,
                )
                .map(|c| c.command)
                .unwrap_or_default();
                let client = client.as_ref();
                ServerSnapshot {
                    pid: client.and_then(|c| c.connection_pid()),
                    version: client.and_then(|c| {
                        c.capabilities()
                            .server_info
                            .as_ref()
                            .and_then(|i| i.version.clone())
                    }),
                    key,
                    command,
                    state,
                }
            })
            .collect();
        // 同一 key 可能同时出现在 ready 和 stopped（停止后未被摘干净）：
        // 按 key 去重，保留先出现的（优先级即上面 entries 的组装顺序）。
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
    ///
    /// **不**回收日志桶：重启（先停后起）也走这里，而重启正是为了看报文
    /// 排障，把桶一起删掉就没了证据。只有用户显式「Stop」才回收桶
    /// （见 [`Self::stop_server_for_key_and_purge_logs`]）。
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

    /// 按 key 停服务器并回收日志桶（菜单里用户显式「Stop」用）。
    ///
    /// 桶数无界（键 = 会话中启动过的每个 (项目根, 语言)），停止的服务器其桶
    /// 会永久驻留，见 `log::remove_server`。启动失败的桶**不**回收——那是
    /// 用户排障「服务器为什么没起来」的唯一证据。
    pub fn stop_server_for_key_and_purge_logs(cx: &App, key: &ServerKey) -> bool {
        let stopped = Self::stop_server_for_key(cx, key);
        crate::lsp::log::remove_server(key);
        stopped
    }

    /// 按 key 解除停止/失败标记。
    pub fn resume_server_for_key(cx: &App, key: &ServerKey) {
        lock(&cx.global::<Self>().table).allow_start(key);
    }

    /// 停掉全部服务器（用户显式操作：菜单/设置里「停止全部」）。
    ///
    /// 返回停掉的进程数——「停止全部」在没有任何服务器跑着时是个空操作，
    /// 调用方拿这个数字决定要不要弹提示。
    ///
    /// 日志桶一并回收：这是用户主动清场的批量操作，不打算再留着这些报文。
    /// （单键的 `stop_server_for_key` 不回收，重启要走它。）
    pub fn stop_all(cx: &mut App) -> usize {
        let mut guard = lock(&cx.global::<Self>().table);
        let keys: Vec<ServerKey> = guard.ready.keys().cloned().collect();
        let mut stopped = 0;
        for key in keys {
            if let Some(client) = guard.stop(&key) {
                client.shutdown();
                stopped += 1;
            }
            crate::lsp::log::remove_server(&key);
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

    /// 兜底口径：文件路径与目录路径给出一致的结果。
    ///
    /// 各处（本地探测、远端探测、`root_for` 的两个分支）共用这一个
    /// 函数，口径漂移的后果是「编辑器算的根」与「服务器表里的键」对不上——
    /// 表现为起了一个服务器却连不上已打开的标签。
    #[test]
    fn fallback_root_treats_files_and_directories_alike() {
        let dir = tmp_dir("fallback-root");
        let file = dir.join("src").join("main.rs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();

        assert_eq!(fallback_root(&file), dir.join("src"));
        // 传目录时返回它自己，而不是它的父目录。
        assert_eq!(fallback_root(&dir.join("src")), dir.join("src"));
        // 根目录不会 panic。
        let _ = fallback_root(Path::new("/"));
    }

    /// 远端探测在连不上主机时必须**降级**而不是 panic。
    ///
    /// 这里传一个必然连不上的地址：会话建立失败时探测脚本压根跑不起来，
    /// 函数按「找不到标记」落回兜底目录。若改成 `?` 或 `unwrap`，一次网络
    /// 抖动就会让打开远程文件直接报错。
    ///
    /// 顺带钉住另一条：这类失败**不能进缓存**——否则一次抖动就把这台主机
    /// 永久判成「没有项目根」，主机恢复之后也再也探不回来。
    #[test]
    fn remote_root_degrades_when_host_is_unreachable() {
        let config = crate::ssh::SshConnectionConfig::new(
            "127.0.0.1",
            "nobody",
            crate::ssh::AuthMethod::Agent,
        )
        .with_port(1); // 保留端口之外无人监听
        assert_eq!(
            remote_project_root(&config, "/some/remote/project/src/lib.rs"),
            PathBuf::from("/some/remote/project/src"),
            "探测不到时必须退回文件所在目录，与本地口径一致"
        );
    }

    /// 「挂起等根」必须有尽头，且这个尽头要能自己收回。
    ///
    /// `Editor::start_lsp_for_tab` 在项目根未知时把标签挂起、把探测丢给后台，
    /// 探完再重走接标签。整套做法押在两条不变量上，这里一条一个断言：
    ///
    /// 1. **问到了答案就结束挂起**——包括「问不到主机」这种没有答案的问法。
    ///    否则主机不可达时每个远程标签都陷入 挂起 → 后台超时 → 再挂起，
    ///    界面永远停在接不上 LSP 的那一态。
    /// 2. **尽头不是永久判决**——失败只买一段 [`ROOT_PROBE_RETRY`] 的静默，
    ///    过了就得重新变成「不知道」。反过来（把失败当事实缓存）会让主机
    ///    恢复之后这个项目永远探不回真正的根，`ServerKey.root` 停在兜底目录，
    ///    于是「起了服务器却接不上标签」重现。
    ///
    /// 不测真机：`127.0.0.1:1` 无人监听，会话建立必然失败，走的正是第 1 条
    /// 那条降级分支。第 2 条靠把记录的时刻拨旧来触发，不睡 30 秒。
    #[test]
    fn deferred_attach_ends_even_when_the_probe_fails() {
        let config = crate::ssh::SshConnectionConfig::new(
            "127.0.0.1",
            "nobody",
            crate::ssh::AuthMethod::Agent,
        )
        .with_port(1);
        // 独占一个目录：缓存键含「主机:端口|起始目录」，别的用例（同端口同
        // 主机但不同目录）才不会被这里的拨表影响。
        let path = "/some/remote/deferred/src/main.rs";
        let fallback = PathBuf::from("/some/remote/deferred/src");

        assert!(
            !remote_root_is_known(&config, path),
            "没问过就是不知道——调用方据此才该走后台"
        );

        assert_eq!(remote_project_root(&config, path), fallback);

        assert!(
            remote_root_is_known(&config, path),
            "问过（哪怕没问到）就必须给挂起的标签一个尽头"
        );
        let key = root_cache_key(&config, &fallback);
        assert!(
            root_probe_failures_lock().contains_key(&key),
            "记下的是时刻，重试间隔才有得算"
        );
        // 静默期内答复必须是「知道」：UI 线程因此一趟网络都不撞。
        let started = std::time::Instant::now();
        assert_eq!(remote_project_root(&config, path), fallback);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "已知（含静默期内的失败）的根不该再联网: {:?}",
            started.elapsed()
        );

        // 把记录的时刻拨到静默期之外。
        root_probe_failures_lock().insert(
            key.clone(),
            std::time::Instant::now() - ROOT_PROBE_RETRY - std::time::Duration::from_secs(1),
        );
        assert!(
            !remote_root_is_known(&config, path),
            "静默期过后要允许再探一次，主机恢复才探得回真根"
        );
    }

    #[test]
    fn table_finish_marks_failure_and_wakes_waiters() {
        let table = Arc::new(parking_lot::Mutex::new(ServerTable::default()));
        let key = ServerKey {
            root: PathBuf::from("/tmp/x"),
            language: "rust".into(),
            ssh_config: None,
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
            ssh_config: None,
        };
        assert!(table.get(&key).is_none());
    }

    #[test]
    fn stop_marks_key_stopped() {
        let mut table = ServerTable::default();
        let key = ServerKey {
            root: PathBuf::from("/tmp/stop-a"),
            language: "rust".into(),
            ssh_config: None,
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
            ssh_config: None,
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
            ssh_config: None,
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

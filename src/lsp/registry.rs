//! 语言服务器的配置表：内置默认 + `settings.json` 覆盖。
//!
//! 内置表只放「主流且命令行接口稳定」的服务器，覆盖本项目
//! [`crate::editor::syntax::languages::Language`] 里有 tree-sitter 语法的语言。
//! 用户可在 `settings.json` 的 `lsp` 段按语言 id 覆盖，或整项禁用。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// 一个语言服务器的启动配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 可执行文件名（在 `PATH` 里查找）或绝对路径。
    pub command: String,
    /// 固定参数。多数服务器需要 `--stdio` / `start` 之类的子命令。
    pub args: Vec<String>,
    /// 置 false 时不为该语言启动服务器（覆盖内置默认）。
    pub enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            enabled: true,
        }
    }
}

/// 内置表的候选命令：按顺序取第一个**真的存在**的。
///
/// # 为什么需要候选而不是单一命令
///
/// 同一门语言有多个主流服务器，装哪个取决于用户/机器：Python 可能是
/// pyright（npm 全局）、python-lsp-server（pip）、或 ruff（单文件二进制）。
/// 只写一个默认值意味着另外两种用户开箱即失败，且失败信息是
/// 「找不到 pyright-langserver」——用户明明装了 Python LSP。
///
/// 远端场景下这个问题更突出：服务器跑在另一台机器上，本地装没装无关，
/// 只取决于那台机器有什么。
///
/// **用户的显式配置永远优先**（`settings.json` 的 `lsp.servers.<lang>`），
/// 候选链只在内置默认这一层生效。
type Candidate = (&'static str, &'static [&'static str]);

/// 内置候选表：语言 → [(命令, 参数), ...]。
///
/// `pub` 而不是 `pub(crate)`：真机用例要读它来验证候选顺序（见
/// `remote_editor_tests::remote_python_candidate_chain_picks_what_the_host_has`）。
pub fn builtin_candidates() -> &'static HashMap<&'static str, Vec<Candidate>> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<HashMap<&'static str, Vec<Candidate>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m: HashMap<&'static str, Vec<Candidate>> = HashMap::new();
        let mut insert = |lang: &'static str, list: Vec<Candidate>| {
            m.insert(lang, list);
        };
        insert("rust", vec![("rust-analyzer", &[])]);
        insert("go", vec![("gopls", &[])]);
        // Python 的主流服务器。pyright 系优先（类型检查最强）。
        //
        // **候选名必须是可执行文件名，不是包名**：pip 分发的 `python-lsp-server`
        // 装出来的入口叫 `pylsp`，写包名等于这一档永远探不到——机器上明明装了
        // python-lsp-server，却跳过它选中排在后面的 ruff，用户拿到的是只有
        // lint 没有补全的降级结果，且没有任何线索指向原因。basedpyright 同理：
        // 包名 `basedpyright`、语言服务器入口 `basedpyright-langserver`。
        //
        // basedpyright 排在 pyright 前面：它是社区 fork，命令行接口完全一致，
        // 但 actively maintained（上游 pyright 的 issue 区长期停滞）。
        // ruff 只做 lint+format、不做类型推导，所以垫底。
        insert(
            "python",
            vec![
                ("basedpyright-langserver", &["--stdio"]),
                ("pyright-langserver", &["--stdio"]),
                ("pylsp", &[]),
                ("ruff", &["server"]),
            ],
        );
        insert(
            "typescript",
            vec![("typescript-language-server", &["--stdio"])],
        );
        insert("tsx", vec![("typescript-language-server", &["--stdio"])]);
        insert(
            "javascript",
            vec![("typescript-language-server", &["--stdio"])],
        );
        insert("c", vec![("clangd", &[])]);
        insert("cpp", vec![("clangd", &[])]);
        insert("json", vec![("vscode-json-language-server", &["--stdio"])]);
        insert("html", vec![("vscode-html-language-server", &["--stdio"])]);
        insert("css", vec![("vscode-css-language-server", &["--stdio"])]);
        insert("lua", vec![("lua-language-server", &[])]);
        insert("zig", vec![("zls", &[])]);
        insert("java", vec![("jdtls", &[])]);
        insert("bash", vec![("bash-language-server", &["start"])]);
        insert("yaml", vec![("yaml-language-server", &["--stdio"])]);
        insert("toml", vec![("taplo", &["lsp", "stdio"])]);
        insert("kotlin", vec![("kotlin-language-server", &[])]);
        m
    })
}

/// 命令是否可用（在 PATH 里，或本身是绝对路径且存在）。
///
/// **会起子进程**：只允许从后台探测路径进来（UI 线程一律只读缓存，
/// 见 [`command_availability_cached`]）。曾经 UI 路径也内联调它，结果
/// 冷机器上第一次打开代码文件的整帧被一个数秒级的进程冷启动吞掉。
fn command_exists(command: &str) -> bool {
    // 绝对路径：直接看文件在不在，不必过 PATH。
    if command.starts_with('/') {
        return std::path::Path::new(command).is_file();
    }
    // `--version` 而不是 `--help`：前者短、退出快、且不读 stdin。
    // rustup 的代理存根在组件缺失时会以非零退出（见 `integration_tests`
    // 里 `has_server` 的注释），这里沿用同一判定。
    let started = std::time::Instant::now();
    let mut cmd = std::process::Command::new(command);
    cmd.arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Windows 上不给 `CREATE_NO_WINDOW`：控制台程序会附带一个新分配的控制台
    // 窗口，冷启动时要额外花时间，任务栏还会闪一下黑框。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let hit = cmd.status().map(|s| s.success()).unwrap_or(false);
    // 正常一次几十毫秒（探过的缓存另算，见 `probe_cache`）；上到百毫秒乃至
    // 数秒说明赶上了进程冷启动（安全软件扫描新二进制、代理存根初始化……）。
    // 只在慢时打日志：卡顿现场排查时这是第一条线索，平时不刷屏。
    let elapsed = started.elapsed();
    if elapsed > std::time::Duration::from_millis(200) {
        log::warn!("lsp: probing `{command} --version` took {elapsed:?} (available={hit})");
    }
    hit
}

/// `settings.json` 里的 `lsp` 段。
///
/// `servers` 的键是语言 id（`rust` / `typescript` / `python` …），
/// 与 [`crate::editor::syntax::languages::Language::name`] 一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LspContent {
    /// 全局开关：false 时整个 LSP 子系统不启动任何服务器。
    ///
    /// **默认关闭**（与 `default_settings.json` 一致）。用户打开代码文件时
    /// 状态栏会显示**该接哪个服务器**（`pylsp` / `rust-analyzer` …）并给出
    /// 开启入口，由用户决定要不要起进程——语言服务器是常驻进程，大项目上
    /// rust-analyzer 能吃几百 MB 内存，不该在用户没要求的时候就跑起来。
    ///
    /// 这个开关只决定「起不起」，不决定「显示不显示名字」：
    /// [`crate::lsp::LspManager::configured_command_on`] 刻意绕开它去解析命令名。
    /// 否则默认关闭时状态栏只剩「未启动」三个字，用户既不知道该接哪个 LSP，
    /// 也看不出点「启动」会起什么。
    ///
    /// 本地与远端共用这一个开关：远端还要额外考虑「在别人机器上起进程」
    /// 这件事本身需要用户授权。
    pub enabled: bool,
    /// 按语言的覆盖项。未列出的语言走内置默认表。
    pub servers: HashMap<String, ServerConfig>,
}

impl Default for LspContent {
    /// 默认**关闭**。
    ///
    /// **不能用 `derive(Default)`**：derive 出来的 `enabled` 恰好也是 `false`，
    /// 但那是巧合而非约定。设置合并是「整字段替换」
    /// （`SettingsContent::merge_from`）：用户文档里没有 `lsp` 段时，serde
    /// 会用这里的 `Default` 填一个空段，再反过来覆盖 `default_settings.json`
    /// 里的值。显式写出来是为了让「默认值」只有一个来源，
    /// 改这里时必须同步 `assets/default_settings.json`。
    fn default() -> Self {
        Self {
            enabled: false,
            servers: HashMap::new(),
        }
    }
}

impl LspContent {
    /// 显式构造一个默认**关闭**的配置（语义同 [`Self::default`]）。
    ///
    /// 存在理由只有一个：作为 `unwrap_or_else` 的回退值时，名字必须直说
    /// 回退到**关闭**——旧名 `default_enabled` 曾被多处误读成「回退到开启」。
    pub fn default_disabled() -> Self {
        Self::default()
    }
}

/// 查某语言最终生效的服务器配置。
///
/// 优先级：用户覆盖 > 内置候选链（取第一个可用的）> 无（该语言不支持 LSP）。
/// 用户的 `enabled: false` 会屏蔽内置默认——这是禁用某语言的唯一途径。
///
/// **候选链为什么要在这里才判定**：内置表里同一门语言可能列多个命令
/// （Python 有 pyright / basedpyright / pylsp / ruff 四种主流实现），只有
/// 「真的要去起进程」时才该逐个问「装没装」。放在这里而不是启动路径上，
/// 是因为 `configured_command_on` 也要用同一个答案显示在状态栏/菜单上——
/// 两处算出不同的命令，用户看到的和实际跑的会对不上。
pub fn server_for_language(
    language: &str,
    settings: Option<&LspContent>,
    override_disabled: bool,
) -> Option<ServerConfig> {
    server_for_language_on(language, settings, None, override_disabled)
}

/// 同 [`server_for_language`]，但候选链的「装没装」在**指定主机**上问。
///
/// `ssh_config` 为 `None` 时问本机。远端场景必须传它：本地装了 pyright
/// 而远端只有 ruff 时，按本机 PATH 挑会选中 pyright，起进程时才在远端
/// 失败——而用户在自己的机器上明明看得到 pyright，很难反推原因。
///
/// `override_disabled` 只放过全局开关这一道 gate：菜单「Start」是一次
/// 明确的授权，不该被默认关闭挡回去；语言级的显式禁用（`enabled: false`
/// 或空命令）仍然生效。传这个布尔而不是克隆一份改过 `enabled` 的设置,
/// 是为了让这个入口在渲染热路径上零开销。
pub fn server_for_language_on(
    language: &str,
    settings: Option<&LspContent>,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    override_disabled: bool,
) -> Option<ServerConfig> {
    server_for_language_inner(
        language,
        settings,
        ssh_config,
        override_disabled,
        false,
        Resolve::Blocking,
    )
}

/// 同 [`server_for_language_on`]，但一个候选都没装时返回 `None`。
///
/// # 为什么需要两个变体
///
/// 「准备起进程」与「状态栏该不该占位」要的答案不同：
///
/// - 起进程时希望**指名道姓地失败**：一个都没装也要给出第一个候选，
///   这样错误信息是「找不到 pyright-langserver」而不是「找不到 Python 服务器」，
///   用户才知道该装什么；
/// - 占位判断时要**如实回答没有**：一台什么都没装的机器不该挂满
///   「未启动」chip——用户点了启动只会失败。
///
/// 所以后者传 `strict = true`。
pub fn server_for_language_strict(
    language: &str,
    settings: Option<&LspContent>,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    override_disabled: bool,
) -> Option<ServerConfig> {
    server_for_language_inner(
        language,
        settings,
        ssh_config,
        override_disabled,
        true,
        Resolve::Blocking,
    )
}

/// 候选链的**解析方式**：能不能为了拿答案而去探测目标主机。
///
/// 存在的理由是「UI 线程一次网络请求都不能发」。远端探测是一次同步的
/// SSH 往返（超时 15s），而状态栏 chip 每帧都要问一次「这台主机上的服务器
/// 叫什么名」。探测本身没错，错在问的时机——所以把「允许探测」做成一个
/// 显式参数，启动/后台路径传 [`Resolve::Blocking`]，渲染与菜单路径传
/// [`Resolve::Cached`]，后者只读缓存、没探过就当不知道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    /// 允许逐个探测目标主机（后台任务、启动路径、测试）。
    Blocking,
    /// 只读已确定的缓存；没探过的候选视为「不知道」并跳过。
    Cached,
}

/// 起进程前的 gate：**只看设置**，不碰目标主机。
///
/// 返回按优先级排好的 `(命令, 参数)` 列表（用户显式配置时只有一条），
/// 由后台任务用 [`pick_from_candidates`] 挑出真正要起的那个。
/// 返回空表示**不该起进程**，三种原因都在这里判，且都不需要一次往返：
/// 全局关闭且这次没有显式授权、语言级显式禁用（`enabled: false` / 空命令）、
/// 这门语言压根不在表里。
///
/// 为什么把「挑哪个」推后：这一步过去在 UI 线程上做（`server_for_key` 由
/// 打开文件与菜单调用），远端是同步 SSH 往返。既然真正的 spawn 已经在
/// `smol` 后台任务里了，探测也该在那儿。
pub fn launch_gate(
    language: &str,
    settings: &LspContent,
    override_disabled: bool,
) -> Vec<(String, Vec<String>)> {
    if !settings.enabled && !override_disabled {
        return Vec::new();
    }
    if let Some(cfg) = settings.servers.get(language) {
        if cfg.command.is_empty() || !cfg.enabled {
            return Vec::new();
        }
        return vec![(cfg.command.clone(), cfg.args.clone())];
    }
    let Some(candidates) = builtin_candidates().get(language) else {
        return Vec::new();
    };
    candidates
        .iter()
        .map(|(command, args)| {
            (
                command.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            )
        })
        .collect()
}

/// 从候选列表里挑出要起的命令（**会探测目标主机**，只在后台任务里调）。
///
/// 一个都没装时退回第一条：起进程必然失败，但失败信息能指名道姓
/// （「找不到 pyright-langserver」而不是「找不到 Python 服务器」），
/// 用户才知道该装什么。
pub fn pick_from_candidates(
    candidates: &[(String, Vec<String>)],
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> Option<ServerConfig> {
    for (command, args) in candidates {
        if command_available_on(command, ssh_config) {
            return Some(ServerConfig {
                command: command.clone(),
                args: args.clone(),
                enabled: true,
            });
        }
    }
    candidates.first().map(|(command, args)| ServerConfig {
        command: command.clone(),
        args: args.clone(),
        enabled: true,
    })
}

/// 同 [`server_for_language_on`]，但**绝不在调用线程上做 I/O**。
///
/// 状态栏/菜单专用。返回 `None` 有两种含义，且对 UI 是同一件事：要么这台
/// 主机上确实没有（探明过），要么还没探过（后台在补）。前者要撤掉 chip 的
/// 占位，后者要保持——区分它们用 [`language_might_have_server_on`]。
pub fn server_for_language_cached(
    language: &str,
    settings: Option<&LspContent>,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    override_disabled: bool,
) -> Option<ServerConfig> {
    server_for_language_inner(
        language,
        settings,
        ssh_config,
        override_disabled,
        true,
        Resolve::Cached,
    )
}

fn server_for_language_inner(
    language: &str,
    settings: Option<&LspContent>,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
    override_disabled: bool,
    strict: bool,
    resolve: Resolve,
) -> Option<ServerConfig> {
    let settings = settings?;
    if !settings.enabled && !override_disabled {
        return None;
    }
    if let Some(cfg) = settings.servers.get(language) {
        // 用户显式配了但没填命令 → 视为禁用（而不是退回默认，
        // 否则「想关掉但写了空命令」会莫名其妙又启动）。
        if cfg.command.is_empty() || !cfg.enabled {
            return None;
        }
        return Some(cfg.clone());
    }
    // 内置候选链：取第一个在目标主机上找得到的。
    //
    // 缓存判定结果：本机那一侧要起一个子进程（`--version`），而
    // `server_for_language` 在状态栏的渲染热路径上（每帧问一次服务器名）。
    // 装没装服务器不会在会话中途变，所以按命令名缓存是安全的。
    // 远端那一侧走 SFTP，同理按 (host, command) 缓存。
    let candidates = builtin_candidates().get(language)?;
    for (command, args) in candidates {
        let known = match resolve {
            Resolve::Blocking => Some(command_available_on(command, ssh_config)),
            Resolve::Cached => command_availability_cached(command, ssh_config),
        };
        if known == Some(true) {
            return Some(config_from(command, args));
        }
    }
    // `Cached` 模式下「没找到已知的」不等于「一个都没装」：可能有还没探过的。
    // 无论如何都不该在这里指名道姓——名字是给用户看的，猜一个会在启动失败时
    // 把用户往错误的方向引（chip 显示 pyright，实际会起的是 ruff）。
    if strict || resolve == Resolve::Cached {
        return None;
    }
    // 一个都没装：退回第一个候选。这样失败信息能指名道姓
    // （「找不到 pyright-langserver」而不是「找不到 Python 服务器」），
    // 用户才知道该装什么。
    let (command, args) = candidates.first()?;
    Some(config_from(command, args))
}

fn config_from(command: &str, args: &[&str]) -> ServerConfig {
    ServerConfig {
        command: command.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        enabled: true,
    }
}

/// 命令在目标主机上是否可用（本机走 PATH，远端走 SSH）。
///
/// 远端一侧直接复用启动时的同一个解析器
/// [`crate::ssh::lsp_channel::resolve_remote_binary`]：挑候选与起进程若各探
/// 各的，PATH 策略一改（换 shell、加候选级别）两处就会漂移——菜单显示
/// 选中了 ruff、启动却按另一套探测失败。
///
/// # 远端只缓存确定性结论
///
/// `Unreachable`（连不上 / 认证失败 / 超时）**不入缓存**：那不是「这台机器
/// 没有这个命令」的事实，而是一次问话失败。把它当成事实存下来，一次赶上
/// SSH 会话还没就绪的探测就会永久把主机判成「什么都没装」——症状是状态栏
/// 报「远端找不到 pyright-langserver」，而那台机器上明明装着 pylsp，且重启
/// 应用之前再也问不出第二个答案。
/// 一个 (主机, 命令) 的探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// 确定性答案（本机 PATH 命中 / 远端找到 / 远端 shell 答「没这个命令」）。
    /// 会话期内不再重探。
    Settled(bool),
    /// 远端问不到（连不上 / 认证失败 / 超时）。不是关于软件安装的事实，
    /// 所以不能当 `Settled(false)` 存，但要带一个冷却期免得每帧去问。
    RetryAt(std::time::Instant),
}

/// 问不到之后的重试间隔。
///
/// 状态栏 chip 每帧都要问一次服务器名，而远端探测是同步的 SFTP 往返
/// （超时 15s）。断开 SSH 连接但留着远程标签时，没有这个冷却就等于
/// **每一帧**发起一次网络请求，UI 会直接冻住。
const UNREACHABLE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

fn command_available_on(
    command: &str,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> bool {
    // 本机路径没有「不知道」这一态：问一次就有答案，且答案便宜。
    if ssh_config.is_none() {
        return settle_probe(command, None) == Probe::Settled(true);
    }
    // 远端：先读缓存。冷却期内的「问不到」按「不可用」回答，但**不再去问**——
    // 与 `RetryAt` 的语义一致（那是问话失败，不是这台机器没装）。
    if let Some(probe) = cached_probe(&probe_key(command, ssh_config)) {
        match probe {
            Probe::Settled(hit) => return hit,
            Probe::RetryAt(at) if std::time::Instant::now() < at => return false,
            Probe::RetryAt(_) => {}
        }
    }
    settle_probe(command, ssh_config) == Probe::Settled(true)
}

/// 缓存键：本机与「哪台主机」共用一张表。
fn probe_key(command: &str, ssh_config: Option<&crate::ssh::SshConnectionConfig>) -> String {
    match ssh_config {
        Some(c) => format!("{}|{command}", c.connection_string()),
        None => format!("local|{command}"),
    }
}

/// 后台探测中的候选链（键 = 主机 + 语言）。
///
/// 没有这道闸门时，`warm_lsp_chain` 每被调一次就新起一个任务：一次打开 N 个
/// 远端文件、或反复点开菜单，就会叠出 N 个任务。主机不可达时每个任务都要把
/// 候选链撞到超时（每个候选两次 shell 尝试 × 15s），并发任务还会各自新建
/// SSH 会话——用户看到的是「什么都没发生，但网络一直很忙」。
fn chain_probes_in_flight() -> &'static std::sync::Mutex<HashSet<String>> {
    use std::sync::{Mutex, OnceLock};
    static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn chain_probe_key(language: &str, ssh_config: Option<&crate::ssh::SshConnectionConfig>) -> String {
    match ssh_config {
        Some(c) => format!("{}|{language}", c.connection_string()),
        None => format!("local|{language}"),
    }
}

/// 一次后台候选链探测的占用凭据：存在期间同一 (主机, 语言) 不会重复探测。
///
/// 用守卫而不是 `begin`/`end` 两个函数：任务被取消或 panic 时 `Drop` 照样
/// 注销，否则这个语言在这台主机上会**永久**不再补探（chip 停在语言名）。
pub struct ChainProbeGuard {
    key: String,
}

impl Drop for ChainProbeGuard {
    fn drop(&mut self) {
        let mut in_flight = chain_probes_in_flight()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        in_flight.remove(&self.key);
    }
}

/// 登记一次后台候选链探测；已有任务在飞时返回 `None`。
///
/// 调用方拿到 `Some` 后应把守卫一起移进后台任务（`let _guard = guard;`），
/// 任务结束才注销。
pub fn begin_chain_probe(
    language: &str,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> Option<ChainProbeGuard> {
    let key = chain_probe_key(language, ssh_config);
    let mut in_flight = chain_probes_in_flight()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !in_flight.insert(key.clone()) {
        return None;
    }
    Some(ChainProbeGuard { key })
}

fn probe_cache() -> &'static std::sync::Mutex<HashMap<String, Probe>> {
    use std::sync::Mutex;
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Probe>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_probe(key: &str) -> Option<Probe> {
    let guard = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
    guard.get(key).copied()
}

/// 真的去问一次目标主机，并把结论落进缓存。
fn settle_probe(command: &str, ssh_config: Option<&crate::ssh::SshConnectionConfig>) -> Probe {
    // 绝对路径：本机看文件在不在；远端交给启动时的 `resolve_remote_binary`
    // （那里才做真实探测，这里没法替它答）。
    if command.starts_with('/') {
        let hit = ssh_config.is_none() && std::path::Path::new(command).is_file();
        let probe = Probe::Settled(hit);
        record(command, ssh_config, probe.clone());
        return probe;
    }
    let probe = match ssh_config {
        Some(config) => {
            use crate::ssh::lsp_channel::RemoteBinary;
            match crate::ssh::lsp_channel::resolve_remote_binary_detailed(config, command) {
                RemoteBinary::Found(_) => Probe::Settled(true),
                RemoteBinary::NotInstalled => Probe::Settled(false),
                RemoteBinary::Unreachable => {
                    Probe::RetryAt(std::time::Instant::now() + UNREACHABLE_RETRY)
                }
            }
        }
        None => Probe::Settled(command_exists(command)),
    };
    record(command, ssh_config, probe.clone());
    probe
}

fn record(command: &str, ssh_config: Option<&crate::ssh::SshConnectionConfig>, probe: Probe) {
    let key = probe_key(command, ssh_config);
    let mut guard = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, probe);
}

/// UI 线程专用的判定：**任何主机都不做 I/O**。
///
/// `Some(true/false)` 是缓存里的确定答案；`None` 是「还不知道」——从没探过，
/// 或上次问不到且冷却期已过。调用方（状态栏 chip、菜单）据此显示占位
/// 并交给后台补探（见 [`pending_chain_probe`]）。
///
/// 本机一侧曾经在这里内联探测（`command_exists` 的一次 `--version` 子进程），
/// 理由写在旧注释里：「便宜且按命令名缓存一次即可」。这个假设在冷机器上
/// 不成立——代理存根 + 安全软件环境下实测 5s+，而这个函数在状态栏**每帧**
/// 的渲染路径上。现在本机与远端同一条规矩：缓存未命中就答「不知道」，
/// 由后台补探填充（本机探测的子进程成本与远端往返同级）。
fn command_availability_cached(
    command: &str,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> Option<bool> {
    let key = probe_key(command, ssh_config);
    match cached_probe(&key) {
        Some(Probe::Settled(hit)) => Some(hit),
        // `RetryAt`（远端问不到、冷却中）与「从没探过」对 UI 是同一件事。
        _ => None,
    }
}

/// 这门语言在这台主机上「**有可能**」起得来一个服务器吗（UI 占位判断）。
///
/// 与 [`server_for_language_cached`] 的分工：那个指名「已知装的是哪个」，
/// 这个只回答「要不要给 chip 留位置」。默认关闭时后者返回 `None` 而前者为
/// `true`——状态栏要靠这个差别决定显示「未启动」还是不留位。
///
/// `Cached` 语义：**不阻塞**。远端还没探过时返回 `true`——宁可先挂出一个能点
/// 的 chip（用户点 Start 时启动路径自己会去探测，那时探不到会如实报错），
/// 也不要因为「我们还没问」就把唯一的开启入口藏起来。探明全部候选都不存在
/// 时返回 `false`，占位随之撤掉。
pub fn language_might_have_server_on(
    settings: &LspContent,
    language: &str,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> bool {
    if let Some(cfg) = settings.servers.get(language) {
        // 用户的显式配置优先，且不需要探测：配了命令就是他要这个。
        return !cfg.command.is_empty() && cfg.enabled;
    }
    let Some(candidates) = builtin_candidates().get(language) else {
        return false;
    };
    candidates
        .iter()
        .any(|(command, _)| command_availability_cached(command, ssh_config) != Some(false))
}

/// 还没探过的候选（本机与远端共用；用户显式配置时返回空）。
///
/// 拆成「谁去问」与「谁来起后台任务」两半，是因为 registry 拿不到 gpui 的
/// 实体句柄：它只能探测，不能通知界面重画。通知必须由调用方（编辑器）用
/// `cx.spawn` 做，所以这里先把「要问哪些」交出去。
///
/// 只列**没有确定结论**的候选：已有答案的不再问。
///
/// 冷却期内的 `RetryAt` **不算**待重问：那 30 秒就是为「主机不可达」准备的
/// 退避窗口，冷却期内再问一次只会再撞一轮 15s 超时。等窗口过去，下一次
/// 调用自然会把它列出来。
pub fn pending_chain_probe(
    language: &str,
    settings: &LspContent,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> Vec<String> {
    // 用户显式配了命令就不经候选链（那条命令由启动路径自己探）。
    if settings.servers.contains_key(language) {
        return Vec::new();
    }
    let now = std::time::Instant::now();
    builtin_candidates()
        .get(language)
        .map(|candidates| {
            candidates
                .iter()
                .filter(
                    |(command, _)| match cached_probe(&probe_key(command, ssh_config)) {
                        Some(Probe::Settled(_)) => false,
                        // 冷却期内：答案暂时就是「不可用」，别再去问。
                        Some(Probe::RetryAt(at)) => now >= at,
                        None => true,
                    },
                )
                .map(|(command, _)| command.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 冷却期里最早还有多久可以重问（远端专用）。
///
/// 「主机不可达」的退避窗口是 30 秒：窗口内 [`pending_chain_probe`] 不列候选，
/// 于是**没人**会再去问一次——chip 就一直停在「还不知道」，直到用户碰巧又
/// 开了一个标签。调用方拿这个时长排一次延迟重试，窗口一过就补问。
pub fn chain_probe_cooldown(
    language: &str,
    settings: &LspContent,
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) -> Option<std::time::Duration> {
    if ssh_config.is_none() || settings.servers.contains_key(language) {
        return None;
    }
    let now = std::time::Instant::now();
    builtin_candidates()
        .get(language)?
        .iter()
        .filter_map(
            |&(command, _)| match cached_probe(&probe_key(command, ssh_config)) {
                Some(Probe::RetryAt(at)) if at > now => Some(at - now),
                _ => None,
            },
        )
        .min()
}

/// 后台线程专用：把交出来的候选探完（**会阻塞**，别在 UI 线程调）。
///
/// 按候选顺序问，问到一个可用的就停——后面的候选保持「不知道」，
/// 因为已经用不着：链子取第一个命中的。这样一台装了 pyright 的主机
/// 只需一次往返，而不是四个。
pub fn probe_commands_pending(
    commands: &[String],
    ssh_config: Option<&crate::ssh::SshConnectionConfig>,
) {
    for command in commands {
        let probe = settle_probe(command, ssh_config);
        if probe == Probe::Settled(true) {
            return;
        }
    }
}

/// 这门语言是否**有**可配的服务器（不关心 `enabled` 开关与是否在跑）。
///
/// 与 [`server_for_language`] 的分工：那个回答「现在该不该起一个」，
/// 这个回答「这门语言能不能有」。默认关闭时前者返回 `None` 而后者为
/// `true`——状态栏要靠这个差别决定显示「未启动」还是不留位。
///
/// 用户的显式覆盖优先：配了 `command: ""` 视为「这门语言我主动关掉」，
/// 与「压根没配」区分开。
pub fn language_known(settings: &LspContent, language: &str) -> bool {
    match settings.servers.get(language) {
        Some(cfg) => !cfg.command.is_empty(),
        // 内置候选表才是真相：「有没有」只看这门语言在不在表里，
        // 不必先重建一份单一配置表。
        None => builtin_candidates().contains_key(language),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：默认必须**关闭**。
    ///
    /// 语言服务器是常驻进程，大项目上 rust-analyzer 能吃几百 MB 内存。
    /// 产品决策是「用户开了代码文件才在状态栏提示，由用户决定起不起」，
    /// 所以默认值必须是 `false`。
    ///
    /// 这个断言看着像在测常量，实际守的是一个真实故障：设置合并是
    /// 「整字段替换」，用户文档没有 `lsp` 段时 serde 会用 `Default` 填一个
    /// 空段再覆盖 `default_settings.json`。两处必须一致，改一处忘一处
    /// 就会表现成「改了默认值却没生效」。
    #[test]
    fn lsp_default_is_disabled() {
        assert!(!LspContent::default().enabled);
        assert!(!LspContent::default_disabled().enabled);
    }

    /// 默认关闭时，状态栏仍要知道「这门语言本来可以接 LSP」。
    ///
    /// 这是「默认关闭 + 可发现开启入口」这个决策的测试锚点：
    /// `enabled: false` 让 `server_for_language` 返回 `None`（不起进程），
    /// 但 `language_known` 必须仍为 `true`，否则状态栏不留位，用户永远
    /// 不知道有这个功能、更找不到开启它的入口。
    #[test]
    fn default_off_still_reports_the_language_as_known() {
        let settings = LspContent::default();
        assert!(!settings.enabled);
        assert!(server_for_language("rust", Some(&settings), false).is_none());
        assert!(
            language_known(&settings, "rust"),
            "默认关闭不该把 rust 变成「没有 LSP 的语言」"
        );
    }

    /// 用户配了空命令 = 主动关掉这门语言，与「没配」区分。
    #[test]
    fn empty_user_command_means_the_language_is_unknown() {
        let mut servers = HashMap::new();
        servers.insert("go".to_string(), ServerConfig::default());
        let settings = LspContent {
            enabled: true,
            servers,
        };
        assert!(!language_known(&settings, "go"));
        // 没配过的语言走内置表。
        assert!(language_known(&settings, "rust"));
        assert!(!language_known(&settings, "brainfuck"));
    }

    /// 回归：`default_settings.json` 与 `Default` 必须一致。
    ///
    /// 上面那个用例只守 Rust 侧；这个守资源文件侧。两边漂移时
    /// 「表现」取决于哪个被 serde 先读到，很难从症状反推原因。
    #[test]
    fn default_settings_json_matches_default() {
        const EMBEDDED: &str = include_str!("../../assets/default_settings.json");
        let parsed: serde_json::Value =
            serde_json::from_str(EMBEDDED).expect("default_settings.json must be valid JSON");
        let lsp = parsed
            .get("lsp")
            .expect("default_settings.json must carry an lsp section");
        assert_eq!(
            lsp.get("enabled").and_then(|v| v.as_bool()),
            Some(LspContent::default().enabled),
            "default_settings.json lsp.enabled drifted from LspContent::default"
        );
    }

    /// 回归：空的用户文档合并后必须保持默认（关闭）状态。
    #[test]
    fn merging_empty_user_document_keeps_lsp_default() {
        let mut merged = crate::settings::content::SettingsContent::default();
        // 模拟「用户 settings.json 里没有 lsp 段」：serde 用 Default 填充。
        let user: crate::settings::content::SettingsContent = serde_json::from_str("{}").unwrap();
        assert!(!user.lsp.enabled, "empty user doc must default to disabled");
        merged.merge_from(&user);
        assert!(!merged.lsp.enabled, "merge must not flip the default");
    }

    #[test]
    fn builtin_table_covers_main_languages() {
        for lang in ["rust", "go", "python", "typescript", "c", "json"] {
            let cfg = server_for_language(
                lang,
                Some(&LspContent {
                    enabled: true,
                    servers: HashMap::new(),
                }),
                false,
            );
            assert!(cfg.is_some(), "no builtin server for {lang}");
            assert!(!cfg.unwrap().command.is_empty());
        }
    }

    #[test]
    fn global_switch_disables_everything() {
        assert!(
            server_for_language(
                "rust",
                Some(&LspContent {
                    enabled: false,
                    servers: HashMap::new(),
                }),
                false
            )
            .is_none()
        );
    }

    #[test]
    fn user_override_wins() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust".to_string(),
            ServerConfig {
                command: "/opt/my-ra".to_string(),
                args: vec!["--custom".to_string()],
                enabled: true,
            },
        );
        let cfg = server_for_language(
            "rust",
            Some(&LspContent {
                enabled: true,
                servers,
            }),
            false,
        )
        .unwrap();
        assert_eq!(cfg.command, "/opt/my-ra");
        assert_eq!(cfg.args, vec!["--custom"]);
    }

    #[test]
    fn user_can_disable_a_builtin_language() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust".to_string(),
            ServerConfig {
                command: "rust-analyzer".to_string(),
                args: vec![],
                enabled: false,
            },
        );
        assert!(
            server_for_language(
                "rust",
                Some(&LspContent {
                    enabled: true,
                    servers,
                }),
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn empty_command_means_disabled_not_fallback() {
        let mut servers = HashMap::new();
        servers.insert("go".to_string(), ServerConfig::default());
        assert!(
            server_for_language(
                "go",
                Some(&LspContent {
                    enabled: true,
                    servers,
                }),
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert!(
            server_for_language(
                "brainfuck",
                Some(&LspContent {
                    enabled: true,
                    servers: HashMap::new(),
                }),
                false
            )
            .is_none()
        );
    }

    /// 回归：Python 候选链里必须是**可执行文件名**，不能是 pip 包名。
    ///
    /// 守的是实际故障：链里写的 `python-lsp-server` 是包名，pip/uv 装出来的
    /// 入口叫 `pylsp`，于是那一档在装了 python-lsp-server 的机器上永远探不到，
    /// 候选链静默退到 ruff（只有 lint/format，没有类型检查与补全）。
    /// 用户看到的是「远程 Python 只有红线没有补全」，而症状上完全看不出
    /// 是名字写错——排错方向会被带去「远端没装服务器」。
    #[test]
    fn python_candidates_are_executables_not_package_names() {
        let chain = builtin_candidates()
            .get("python")
            .expect("python must have a builtin chain");
        let names: Vec<&str> = chain.iter().map(|(cmd, _)| *cmd).collect();

        assert!(
            names.contains(&"pylsp"),
            "python-lsp-server 的入口是 pylsp，链里必须有它：{names:?}"
        );
        assert!(
            !names.contains(&"python-lsp-server"),
            "包名不是可执行文件名，探不到：{names:?}"
        );
        // basedpyright 是 pyright 的活跃分支，入口带 -langserver 后缀，
        // 与包名 `basedpyright` 不同名，同样容易被漏掉。
        assert!(
            names.contains(&"basedpyright-langserver"),
            "远端只有 basedpyright 时要能选中它：{names:?}"
        );
        // ruff 只做 lint+format，必须排在有类型检查的服务器之后。
        assert_eq!(
            names.last().copied(),
            Some("ruff"),
            "ruff 不该抢在类型检查服务器前面：{names:?}"
        );
    }

    #[test]
    fn deserializes_partial_settings() {
        // `serde(default)`：用户只写 `enabled` 也能解析。
        let v: LspContent = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!v.enabled);
        assert!(v.servers.is_empty());
    }

    fn remote_config(tag: &str) -> crate::ssh::SshConnectionConfig {
        let mut config = crate::ssh::SshConnectionConfig::local();
        config.host = format!("registry-test-{tag}");
        config.username = format!("registry-user-{tag}");
        config
    }

    /// 同一个 (主机, 语言) 同时只允许一个后台候选链探测在飞。
    ///
    /// 没有这道闸门时，每打开一个远端文件都会叠一个任务；主机不可达时每个
    /// 任务都要把候选链撞到超时，用户看到的是「什么都没发生，但网络一直忙」。
    /// 守卫还必须在 Drop 时注销，否则任务 panic 后这门语言永久不再补探。
    #[test]
    fn chain_probe_guard_deduplicates_and_releases_on_drop() {
        let config = remote_config("chain-guard");

        let guard = begin_chain_probe("python", Some(&config)).expect("first probe should start");
        assert!(
            begin_chain_probe("python", Some(&config)).is_none(),
            "同一 (主机, 语言) 不该并发探测两次"
        );
        // 不同语言是不同的键，互不阻塞。
        assert!(begin_chain_probe("rust", Some(&config)).is_some());

        drop(guard);
        assert!(
            begin_chain_probe("python", Some(&config)).is_some(),
            "守卫 Drop 之后必须释放占位"
        );
    }

    /// 回归：主机不可达后的 30 秒冷却期不能被 `pending_chain_probe` 绕过。
    ///
    /// 冷却期存在的意义就是「别再问」。若这里仍把候选交出去，状态栏每次重建
    /// 都会重新起一个后台任务，每个任务再撞一轮 15s 超时——UI 线程虽不阻塞，
    /// 网络与 SSH 会话却被反复占用。
    #[test]
    fn pending_chain_probe_respects_unreachable_cooldown() {
        let config = remote_config("cooldown");
        let candidates: Vec<String> = builtin_candidates()
            .get("python")
            .expect("python must have a builtin chain")
            .iter()
            .map(|(command, _)| command.to_string())
            .collect();
        let settings = LspContent::default();

        // 全部候选都在冷却中：没有待重问的。
        let cooldown = std::time::Instant::now() + UNREACHABLE_RETRY;
        for command in &candidates {
            record(command, Some(&config), Probe::RetryAt(cooldown));
        }
        assert!(
            pending_chain_probe("python", &settings, Some(&config)).is_empty(),
            "冷却期内不该再列候选"
        );

        // 冷却到期：重新列出整条链。
        let expired = std::time::Instant::now();
        for command in &candidates {
            record(command, Some(&config), Probe::RetryAt(expired));
        }
        assert_eq!(
            pending_chain_probe("python", &settings, Some(&config)),
            candidates,
            "冷却到期后应重新补探"
        );
    }

    /// 回归：本机缓存未命中时，UI 路径**只答「不知道」**，不得就地探测。
    ///
    /// 守的是「第一次打开代码文件整帧被吞 5s+」的故障：状态栏 chip 在
    /// `render` 里每帧问一次服务器名，旧实现（本机）缓存未命中就内联起一个
    /// `--version` 子进程，冷机器上进程冷启动实测数秒。现在本机与远端同一
    /// 条规矩——UI 只读缓存，未命中答 `None`，由后台补探填充。
    ///
    /// 用「必然不存在的唯一命令名」而不是慢速假二进制：不必动 PATH（进程级
    /// 全局状态，与并行测试互扰），而且足以区分新旧行为——旧实现会当场落下
    /// `Settled(false)` 并返回 `Some(false)`；新实现返回 `None`，且缓存里
    /// 不留任何记录（留记录 = 起过子进程）。
    #[test]
    fn local_cache_miss_reports_unknown_without_probing() {
        // 唯一性靠 pid + 名字本身：缓存是按命令名的进程级静态表，同一进程
        // 里只有本测试用这个名字。
        let name = format!("deepterm-test-missing-cmd-{}", std::process::id());
        let key = probe_key(&name, None);
        assert!(
            cached_probe(&key).is_none(),
            "唯一命令名不该有缓存记录（前置条件）"
        );
        assert_eq!(
            command_availability_cached(&name, None),
            None,
            "本机未命中必须答「不知道」，不能就地探测成 Settled"
        );
        assert!(
            cached_probe(&key).is_none(),
            "只读路径不得写探测缓存（写缓存等于起过子进程）"
        );
        // 后台补探才是允许落缓存的路径：探完 chip 那边就能从占位转成
        // 确定答案（这里是「确定没有」）。
        probe_commands_pending(&[name.clone()], None);
        assert_eq!(command_availability_cached(&name, None), Some(false));
    }
}

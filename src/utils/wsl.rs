//! WSL 相关的纯逻辑：从启动参数认出发行版、把 Linux 路径翻译成 Windows 可浏览的路径。
//!
//! # 为什么需要翻译
//!
//! WSL 标签页的 shell 工作在 **Linux 侧**，cwd 形如 `/home/you/project`；而
//! 文件树、编辑器用的都是 Windows 侧的文件系统 API。这个路径在 Windows 上直接
//! 打不开——`C:\home\you\project` 并不是它。
//!
//! 好在 WSL 把自己挂成了 UNC 共享：`\\wsl$\<distro>\<linux-path>`（新版本还
//! 有 `\\wsl.localhost\...`）。于是**同一个 `LocalFileSystem`** 直接就能列目录、
//! 读文件、展开树，不需要为 WSL 再写一套后端。前提只是把路径翻译过去。
//!
//! # 与「拿不到 cwd」的区别
//!
//! 进程 cwd 查询（[`crate::utils::win_process_cwd`]）读的是 `wsl.exe` 这个
//! **Windows** 进程的 cwd，它会被翻译成 `/mnt/c/...`，对 Linux 侧 shell 毫无
//! 意义。所以 WSL 标签页的 cwd 必须另找来源（OSC 7 上报，或一次性
//! `wsl.exe -- pwd`），绝不能拿 PEB 的结果当 Linux 路径。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::ssh::platform::RemoteOS;

/// WSL 发行版在 UNC 里的前缀。
const WSL_UNC_PREFIX: &str = r"\\wsl$\";

/// 命令是否是一个 WSL 启动器（`wsl.exe` / `wsl`）。
///
/// 比的是**文件名主干**（`file_stem`）而不是后缀：`ends_with("wsl")` 会把
/// `showsl`、`mywsl` 一并误判成 WSL，而误判的代价不小——非 Windows 平台上
/// 会去翻译一条打不开的 UNC 路径，或给普通 shell 注入 WSLENV。
///
/// 本函数是**纯逻辑**（任何平台结果都一样），因此可在任意平台单测；
/// 「要不要走 WSL 分支」由调用方用 `cfg!(windows)` 把关。
pub fn is_wsl_command(program: &str) -> bool {
    Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("wsl"))
}

/// 从 `wsl.exe` 的参数里取发行版名。
///
/// 认这三种写法（`wsl.exe` 自身支持的）：
/// - `-d <name>` / `--distribution <name>`
/// - `-d<name>`（连写）
/// - `--distribution=<name>`
///
/// 取不到返回 `None`：意味着「用默认发行版」，而默认发行版**没有**统一的名字
/// 可猜（`wsl.exe --list` 的第一行才是），得另外查。
pub fn distro_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-d" || arg == "--distribution" {
            return iter
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
        if let Some(rest) = arg.strip_prefix("--distribution=") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        // `-d<name>` 连写。注意 `--distribution=` 已在上面处理，这里要排除
        // 任何 `--` 开头的长选项，否则 `--delete` 会被读成发行版名「elete」。
        if let Some(rest) = arg.strip_prefix("-d")
            && !rest.is_empty()
            && !arg.starts_with("--")
        {
            return Some(rest.to_string());
        }
    }
    None
}

/// `\\wsl$\<distro>` 根。
pub fn unc_distro_root(distro: &str) -> PathBuf {
    PathBuf::from(format!("{WSL_UNC_PREFIX}{distro}"))
}

/// Linux 路径 → `\\wsl$\<distro>\<linux-path>`。
///
/// `/home/you` → `\\wsl$\Ubuntu-24.04\home\you`。开头的 `/` 直接转成路径分隔符；
/// UNC 前缀本身自带的 `\wsl$\` 不重复加。
pub fn linux_path_to_unc(distro: &str, linux_path: &str) -> PathBuf {
    let mut out = unc_distro_root(distro);
    for segment in linux_path.split('/') {
        if segment.is_empty() {
            continue;
        }
        out.push(segment);
    }
    out
}

/// 该路径是否已经是 WSL 的 UNC 形式。
pub fn is_unc_wsl_path(path: &Path) -> bool {
    path.to_string_lossy()
        .get(..WSL_UNC_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(WSL_UNC_PREFIX))
}

/// 把可能是 Linux 形式的路径规范成 Windows 可浏览的形式。
///
/// 已经是 UNC 或 Windows 绝对路径的原样返回（不做多余改写）；`/...` 形式按
/// 发行版翻译。发行版未知时返回 `None`——宁可没有根，也不要给一个打不开的路径。
pub fn to_browsable_path(distro: Option<&str>, path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    if is_unc_wsl_path(path) {
        return Some(path.to_path_buf());
    }
    // Windows 绝对路径（`C:\...`）原样返回。
    if !text.starts_with('/') {
        return Some(path.to_path_buf());
    }
    let distro = distro?;
    Some(linux_path_to_unc(distro, &text))
}

/// 解码 `wsl.exe --list --quiet` 的输出。
///
/// 坑有两层，缺一不可：
/// 1. 输出被重定向（管道）时是 **UTF-16LE**，而不是控制台下的 UTF-8。
/// 2. **常常不带 BOM**（实测就是裸的 `55 00 62 00 ...`）。只认 BOM 会掉进
///    UTF-8 分支，得到夹满 NUL 的垃圾串。
///
/// 为什么不能靠调用方 `.replace('\0', "")` 糊过去：NUL 夹在 CR 与 LF 之间
/// （`0D 00 0A 00`），`str::lines()` 只剥**紧贴**行尾的那个 `\r`，于是
/// `Ubuntu-24.04\r` 被原样留下，再作为 `-d` 的参数发给 `wsl.exe` → 发行版
/// 找不到，终端开不起来。必须在这里真正解码。
pub fn decode_list_output(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    // 发行版名都是 ASCII，所以「所有奇数位都是 0」足以判定 UTF-16LE。
    let is_utf16le = body.len() >= 2
        && body.len().is_multiple_of(2)
        && body.iter().skip(1).step_by(2).all(|b| *b == 0);
    if is_utf16le {
        let (units, _) = body.as_chunks::<2>();
        let units: Vec<u16> = units
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// 列出所有已安装的发行版名（按 `wsl.exe --list --quiet` 的顺序，第一个是默认）。
pub fn installed_distros() -> Vec<String> {
    let Ok(output) = crate::utils::console::hidden_command("wsl.exe")
        .args(["--list", "--quiet"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    decode_list_output(&output.stdout)
        .lines()
        .map(|l| l.trim().replace('\0', "").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn query_default_distro() -> Option<String> {
    installed_distros().into_iter().next()
}

/// 默认发行版名（`wsl.exe --list --quiet` 的第一行，即带 `*` 的那个）。
///
/// 带缓存：结果在整个进程生命周期内基本不变，而查询要起一个 wsl 进程。
/// 查不到返回 `None`（没装 WSL / 没发行版），调用方按「无法浏览」处理。
pub fn default_distro() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(query_default_distro).as_deref()
}

/// 解析指定发行版的 Linux home（`wsl.exe -d <distro> --cd ~ -- pwd`）。
///
/// `--cd ~` 是必要的：不给的话 `wsl.exe` 会沿用 **Windows** 当前目录并翻译成
/// `/mnt/c/...`，拿到的是宿主目录而不是发行版的家目录。
pub fn distro_home(distro: &str) -> Option<String> {
    let output = crate::utils::console::hidden_command("wsl.exe")
        .args(["-d", distro, "--cd", "~", "--", "pwd"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let home = text.trim();
    if home.is_empty() {
        return None;
    }
    // 只接受 Linux 绝对路径：万一周期性命令跑飞了，不要把宿主路径当发行版路径。
    if !home.starts_with('/') {
        log::warn!("wsl.exe -- pwd 返回了非 Linux 路径: {home}");
        return None;
    }
    Some(home.to_string())
}

/// cwd 标记变量名：随 `wsl.exe` 注入发行版，用来在 `/proc` 里认出「本标签页
/// 的那个 shell」。
///
/// 为什么需要标记：WSL 标签页没有 shell integration（`wsl.exe` 不是 bash/zsh，
/// 进不了 ENV/ZDOTDIR 注入分支），拿不到 OSC 7；而 Windows 侧的进程 cwd 查询
/// 只能看到 `wsl.exe` 自己的目录（见 [`to_browsable_path`] 的模块文档）。唯一
/// 可行的实时来源是**进发行版里问**：给本会话的 shell 塞一个唯一标记，轮询时
/// 扫 `/proc/*/environ` 认出它，读 `/proc/<pid>/cwd`。
pub const CWD_TOKEN_ENV: &str = "DEEPTERM_CWD_TOKEN";

/// [`CWD_TOKEN_ENV`] 的 WSLENV 共享条目（`/u` = 允许该变量进发行版）。
///
/// WSL **只**透传 WSLENV 里点名的变量：不加这条，标记根本到不了 Linux 侧
/// （实测：不带 WSLENV 时 Windows 变量在 `wsl.exe -- env` 里一个都看不到）。
pub const CWD_TOKEN_WSLENV_ENTRY: &str = "DEEPTERM_CWD_TOKEN/u";

/// 生成本次终端会话的 cwd 标记值。
///
/// 不是安全凭据（只在本机 `/proc` 扫描里回答「哪个进程是我这个标签页的
/// shell」），但必须唯一：同一台机器可能有多个 WSL 标签页共用一个发行版，
/// 撞了就会把别的标签页的 cwd 抢过来。
pub fn new_cwd_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 把 cwd 标记写进 `wsl.exe` 的 spawn 环境，返回标记值。
///
/// 两件必须一起做，缺一不可：
/// 1. `DEEPTERM_CWD_TOKEN=<token>`——标记本体；
/// 2. `WSLENV` 追加 [`CWD_TOKEN_WSLENV_ENTRY`]——否则变量进不了发行版。
///
/// 用户已有的 WSLENV 条目原样保留（追加而非覆盖）。
pub fn inject_cwd_token(env: &mut HashMap<String, String>) -> String {
    let token = new_cwd_token();
    env.insert(CWD_TOKEN_ENV.to_string(), token.clone());

    let mut wslenv = env
        .get("WSLENV")
        .cloned()
        .or_else(|| std::env::var("WSLENV").ok())
        .unwrap_or_default();
    if !wslenv.is_empty() && !wslenv.ends_with(':') {
        wslenv.push(':');
    }
    wslenv.push_str(CWD_TOKEN_WSLENV_ENTRY);
    env.insert("WSLENV".to_string(), wslenv);
    token
}

/// 发行版内扫描脚本：按 pid 升序找第一个 environ 里带 cwd 标记的进程，
/// 打印它的 cwd；找不到退出码 1。
///
/// 三条都是踩过坑的硬约束，改之前先读注释：
/// - **整个脚本不能出现双引号**：它作为一条 argv 交给 `wsl.exe`，双引号会让
///   Windows 命令行转义进入「谁再解析一遍」的浑水；只含空格与单引号的参数
///   能原样到达 sh。测试也把「无双引号」钉成了断言。
/// - 必须配 `--exec`（见 [`query_distro_cwd`]）：`wsl.exe -- cmd` 默认**先过
///   一遍发行版的默认 shell**，脚本里的 `$(...)` 会被外层 shell 先展开，
///   语法直接崩在 sh 这一层。
/// - `-r /proc/$pid/environ` 先筛一遍，读不了的（root 进程）直接跳过；
///   子 shell再补 `2>/dev/null` 压掉「重定向失败是 shell 报的」那半——只给
///   `tr` 重定向压不住，进程消失的竞态会往 stderr 刷满 Permission denied。
const CWD_SCAN_SCRIPT: &str = r"for pid in $(ls /proc | grep -E '^[0-9]+$' | sort -n); do [ -r /proc/$pid/environ ] || continue; if (tr '\0' '\n' < /proc/$pid/environ) 2>/dev/null | grep -qxF DEEPTERM_CWD_TOKEN=$1; then readlink /proc/$pid/cwd; exit 0; fi; done; exit 1";

/// 进发行版里问「带 cwd 标记的 shell 现在在哪」；问不到返回 `None`。
///
/// 命令形状（`--exec` 是关键，理由见 [`CWD_SCAN_SCRIPT`]）：
///
/// ```text
/// wsl.exe [-d <distro>] --exec sh -c <script> deepterm-cwd-scan <token>
/// ```
///
/// - `--exec` 让 WSL 直接 exec 这条 argv，不经过发行版默认 shell；
/// - `deepterm-cwd-scan` 占的是 `$0`（`sh -c` 的第一个多余参数），
///   真正的标记是 `$1`；
/// - 本进程**不带**标记去问：标记进了扫描者自己的 environ 的话，
///   它会先扫到自己。
///
/// 失败（没装 WSL / 发行版没起来 / 标记没进发行版）一律 `None`，
/// 与 [`distro_home`] 的失败语义一致——调用方当「本次没查到」，不刷日志。
pub fn query_distro_cwd(distro: Option<&str>, token: &str) -> Option<String> {
    let mut command = crate::utils::console::hidden_command("wsl.exe");
    if let Some(distro) = distro {
        command.args(["-d", distro]);
    }
    command.args([
        "--exec",
        "sh",
        "-c",
        CWD_SCAN_SCRIPT,
        "deepterm-cwd-scan",
        token,
    ]);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let cwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // 只收 Linux 绝对路径：wsl.exe 偶尔会往 stdout 混别的输出，
    // 且 Windows 形式的路径对文件树无意义（翻译分支只认 `/...`）。
    (!cwd.is_empty() && cwd.starts_with('/')).then_some(cwd)
}

/// 发行版的展示图标（按已知发行版映射，未知回落 Linux）。
pub fn distro_icon(distro: &str) -> crate::assets::Icon {
    let os = RemoteOS::from_id(distro);
    if os == RemoteOS::Unknown {
        crate::assets::Icon::Linux
    } else {
        os.icon()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_wsl_commands() {
        assert!(is_wsl_command("wsl.exe"));
        assert!(is_wsl_command("WSL.EXE"));
        assert!(is_wsl_command("wsl"));
        // 不能把 wscript / wslpath 之类误判进来
        assert!(!is_wsl_command("wscript.exe"));
        assert!(!is_wsl_command("powershell.exe"));
        // 主干不等于 wsl 的一律不算：`ends_with("wsl")` 会把这两个放进来
        assert!(!is_wsl_command("showsl"));
        assert!(!is_wsl_command("/usr/local/bin/mywsl"));
    }

    /// 绝对路径形式的 `wsl.exe` 也要认出来。
    ///
    /// `is_wsl_command` 取 `Path::file_stem()`，而 components 的切分是平台
    /// 相关的：反斜杠只在 windows 上是分隔符，所以这条断言只在 windows 上
    /// 成立。代码两边都对，因此单独成条而不是混进上面那条。
    #[test]
    #[cfg(windows)]
    fn recognizes_wsl_command_by_absolute_path() {
        assert!(is_wsl_command(r"C:\Windows\System32\wsl.exe"));
    }

    #[test]
    fn parses_distribution_argument_forms() {
        let a = |v: &[&str]| -> Option<String> {
            distro_from_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(a(&["-d", "Ubuntu-24.04"]).as_deref(), Some("Ubuntu-24.04"));
        assert_eq!(
            a(&["--distribution", "archlinux"]).as_deref(),
            Some("archlinux")
        );
        assert_eq!(a(&["-dUbuntu-24.04"]).as_deref(), Some("Ubuntu-24.04"));
        assert_eq!(
            a(&["--distribution=archlinux"]).as_deref(),
            Some("archlinux")
        );
        // 默认发行版：没有 -d
        assert_eq!(a(&["~"]), None);
        assert_eq!(a(&[]), None);
    }

    #[test]
    fn translates_linux_paths_into_wsl_unc() {
        // 期望值只替换分隔符，段结构与前缀仍然写死：`linux_path_to_unc` 内部
        // 用 `PathBuf::push` 逐段拼，接的就是**本平台**的分隔符。写死 `\`
        // 的话这条在 unix 上必然失配，而它本来是个有意义的断言。
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            linux_path_to_unc("Ubuntu-24.04", "/home/you/project").to_string_lossy(),
            format!(r"\\wsl$\Ubuntu-24.04{sep}home{sep}you{sep}project")
        );
        // 根目录
        assert_eq!(
            linux_path_to_unc("Ubuntu-24.04", "/").to_string_lossy(),
            r"\\wsl$\Ubuntu-24.04"
        );
        // 连续斜杠不该产生空段
        assert_eq!(
            linux_path_to_unc("d", "//a//b").to_string_lossy(),
            format!(r"\\wsl$\d{sep}a{sep}b")
        );
    }

    #[test]
    fn leaves_non_linux_paths_untouched() {
        // 已经是 UNC
        let unc = PathBuf::from(r"\\wsl$\Ubuntu-24.04\home");
        assert!(is_unc_wsl_path(&unc));
        assert_eq!(to_browsable_path(None, &unc), Some(unc));
        // Windows 绝对路径原样返回
        let win = PathBuf::from(r"C:\Users\you\project");
        assert_eq!(to_browsable_path(Some("d"), &win), Some(win));
    }

    #[test]
    fn refuses_to_translate_without_a_known_distro() {
        // 发行版未知时给不出可浏览路径：宁缺毋滥。
        assert_eq!(to_browsable_path(None, Path::new("/home/you")), None);
    }

    /// 真机验证：翻译出来的 UNC 路径**真的能列目录**。
    ///
    /// 纯字符串断言只能证明拼对了，证明不了 Windows 认这个 UNC——而这正是
    /// 「WSL 文件树能不能用」的全部。所以这里直接用 `LocalFileSystem` 走一遍
    /// 真实枚举。没装 WSL / 没发行版时自动跳过。
    #[cfg(windows)]
    #[test]
    fn translated_unc_path_is_actually_browsable() {
        let Some(distro) = default_distro() else {
            eprintln!("跳过：未检测到 WSL 发行版");
            return;
        };
        let Some(home) = distro_home(distro) else {
            eprintln!("跳过：取不到 {distro} 的家目录");
            return;
        };
        let unc = linux_path_to_unc(distro, &home);
        assert!(
            unc.is_dir(),
            "翻译出的 UNC 路径应存在且是目录：{}",
            unc.display()
        );
        // 用文件树实际使用的那个文件系统枚举一次。
        let fs = crate::file_manager::filesystem::LocalFileSystem;
        let entries = crate::file_manager::filesystem::FileSystem::read_dir(&fs, &unc);
        assert!(entries.is_ok(), "LocalFileSystem 应能列出 {unc:?}");
    }

    /// 注入必须**同时**给标记和 WSLENV 条目，且不能吃掉用户已有的条目。
    #[test]
    fn inject_cwd_token_sets_the_token_and_appends_wslenv() {
        let mut env = HashMap::new();
        env.insert("WSLENV".to_string(), "WT_SESSION/u".to_string());

        let token = inject_cwd_token(&mut env);

        assert!(!token.is_empty(), "标记不能为空");
        assert_eq!(
            env.get(CWD_TOKEN_ENV).map(String::as_str),
            Some(token.as_str()),
            "标记本体必须进 spawn 环境"
        );
        assert_eq!(
            env.get("WSLENV").map(String::as_str),
            Some("WT_SESSION/u:DEEPTERM_CWD_TOKEN/u"),
            "用户已有条目要保留，新条目追加在后面"
        );
    }

    /// 没有 WSLENV 时不能拼出以 `:` 开头的非法值。
    #[test]
    fn inject_cwd_token_starts_wslenv_clean_when_there_was_none() {
        let mut env = HashMap::new();
        let _ = inject_cwd_token(&mut env);
        let wslenv = env.get("WSLENV").expect("WSLENV 必须被写入");
        assert!(
            !wslenv.starts_with(':'),
            "空 WSLENV 不能带前导冒号: {wslenv}"
        );
        assert!(wslenv.ends_with(CWD_TOKEN_WSLENV_ENTRY), "{wslenv}");
    }

    /// 扫描脚本**不能含双引号**：它整条作为 argv 交给 `wsl.exe`，双引号会把
    /// 转义交给「谁再解析一遍」的浑水（实测会被外层 shell 先展开语法崩掉）。
    #[test]
    fn cwd_scan_script_has_no_double_quotes() {
        assert!(
            !CWD_SCAN_SCRIPT.contains('"'),
            "扫描脚本必须只用单引号，否则 argv 转义不保真:\n{CWD_SCAN_SCRIPT}"
        );
    }

    /// 真机验证：给一个真实 WSL 进程打标记并 `cd`，轮询必须问出新目录。
    ///
    /// 这是整条链路的**决定性**用例——WSLENV 有没有真把变量送进发行版、
    /// `--exec` 有没有绕过默认 shell、`/proc/*/environ` 扫描能不能认出目标，
    /// 三件事缺一条都测不过。没装 WSL / 没发行版时自动跳过。
    #[cfg(windows)]
    #[test]
    fn scans_the_live_cwd_of_a_token_bearing_process() {
        let Some(distro) = default_distro() else {
            eprintln!("跳过：未检测到 WSL 发行版");
            return;
        };

        let token = new_cwd_token();
        let mut child_env = HashMap::new();
        child_env.insert(CWD_TOKEN_ENV.to_string(), token.clone());
        child_env.insert("WSLENV".to_string(), CWD_TOKEN_WSLENV_ENTRY.to_string());

        // 子进程带标记进发行版，cd 到 /tmp 后停住；`exec` 让 sleep 顶替 sh，
        // 进程树里只留一个带标记的进程。
        let mut child = crate::utils::console::hidden_command("wsl.exe")
            .args(["-d", distro, "--exec", "sh", "-c", "cd /tmp; exec sleep 30"])
            .envs(&child_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("启动带标记的 wsl 进程");

        // 冷启动的发行版要几百毫秒才起进程；轮询到超时为止。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            if let Some(cwd) = query_distro_cwd(Some(distro), &token) {
                found = Some(cwd);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(
            found.as_deref(),
            Some("/tmp"),
            "扫描必须问出标记进程 cd 之后的目录"
        );
    }
}

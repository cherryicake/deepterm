//! `workspace/didChangeWatchedFiles` 的路径过滤。
//!
//! 单独成模块而不是塞进 `jsonrpc.rs`：过滤逻辑要能被单测直接调用，而
//! `jsonrpc` 里大部分东西都要真的起一个语言服务器进程才测得到。
//!
//! 两部分职责：
//!
//! - [`glob_match`]：LSP 3.17 `GlobPattern` 的字符串形式（`"**/*.rs"`）。
//!   只在**服务器注册过 glob** 时使用（对照 Zed 的 `GlobSet::is_match`）。
//! - [`is_build_artifact`]：兜底排除构建产物目录，用于服务器没注册 glob 的
//!   情形。理由见 [`super::jsonrpc::Connection::should_notify_watched_file`]。

use std::path::Path;

/// 匹配一个 LSP `GlobPattern`（字符串形式）。
///
/// 支持 `**`（跨任意层目录）与 `?`（单字符，不跨分隔符）。**分隔符一律按 `/`
/// 处理**：LSP 规定 glob 用 `/`，而 Windows 路径是 `\`，不统一就会在 Windows
/// 上一个都匹配不上——这正是 `glob` 参数化路径要解决的问题。
///
/// 语义细节（对齐 LSP 规范与 Zed 的 `glob` crate 行为）：
/// - `**` 单独成段（`a/**/b`）时可匹配**零层**目录，即 `a/b` 也算命中
/// - `**` 出现在段内（`**foo`）时退化为 `*`（不跨分隔符）
/// - 前导 `**/` 等价于去掉前导 `**/` 后的匹配（可匹配任意深度）
pub fn glob_match(pattern: &str, path: &Path) -> bool {
    let normalized = path_to_slashes(path);
    // LSP 的 glob 是**相对模式**：`Cargo.toml` 匹配任意层级下最后一段等于它的
    // 路径（对照 Zed 用的 `globset`：它对不含 `/` 的模式不做锚定）。
    //
    // 因此**不需要**为前导 `**/` 特判 —— 让 `match_segments` 自己处理零层或多层
    // 的 `**` 即可。特判过反而会让「前导 `**/` 之后又出现 `**`」的写法漏掉
    // 中间层数。
    //
    // 唯一锚定的是前导 `/`：那表示相对工作区根的绝对写法。
    match pattern.strip_prefix('/') {
        Some(rooted) => match_segments(rooted, normalized.trim_start_matches('/')),
        None => match_at_any_depth(pattern, &normalized),
    }
}

/// 把路径拆成段列表，分隔符统一按 `/`。
///
/// **必须先拆段再匹配**，不能拿整条路径串去和模式比：LSP 的 `GlobPattern` 语义是
/// 按路径分量比较的，`Cargo.toml` 匹配的是「**最后一段**是 `Cargo.toml`」，
/// 而不是「整条路径字面等于 `Cargo.toml`」。混了整串比较后 `**/Cargo.toml`
/// 这类相对模式一个都匹配不上 —— 这也是最初几版实现全挂的原因。
fn path_to_slashes(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    // UNC 前缀 `//?/C:/...`：去掉开头的 `//?/`，其余保持。
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    s
}

/// 从路径的**任意一段**开始尝试对齐模式。
///
/// 这让 `Cargo.toml` 能匹配 `/ws/sub/Cargo.toml` —— LSP 服务器就是这么用的：
/// rust-analyzer 注册的清单 glob 是 `**/Cargo.toml`，但不少服务器直接写
/// `Cargo.lock`；若要求逐字对齐整条路径，这些模式一个都匹配不上，等于把过滤
/// 功能整个关掉，又回到「全量通知 → reload 循环」的老问题。
///
/// 代价是 `src/*.rs` 也会匹配 `/ws/x/src/lib.rs`。可以接受：过滤的目的是挡住
/// `target/`，误放行一个源码文件只是多一条通知，不会引发 reload 循环。
fn match_at_any_depth(pattern: &str, path: &str) -> bool {
    let mut rest = path;
    loop {
        if match_segments(pattern, rest) {
            return true;
        }
        match rest.find('/') {
            Some(i) => rest = &rest[i + 1..],
            None => return false,
        }
    }
}

/// 按段递归匹配。`path` 是**已经对齐到当前段**的剩余部分。
fn match_segments(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches('/');
    if pattern.is_empty() {
        // 空模式匹配空路径（模式已全部用完）。
        return path.is_empty();
    }

    // 取出 pattern 的第一段（`**` 在这里被特殊处理）。
    let (first, tail) = match pattern.find('/') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => (pattern, ""),
    };

    if first == "**" {
        // `a/**/b` 里的 `**` 可以吃掉**零层**：`a/b` 也命中。
        if match_segments(tail, path) {
            return true;
        }
        // 或者吃掉一层或多层：逐段往后挪。
        let mut rest = path;
        while let Some(i) = rest.find('/') {
            rest = &rest[i + 1..];
            if match_segments(tail, rest) {
                return true;
            }
        }
        // `**` 吃掉全部剩余段（后面已无模式）。
        return tail.is_empty();
    }

    if path.is_empty() {
        return false;
    }
    let (head, rest) = match path.find('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, ""),
    };
    if !segment_match(first, head) {
        return false;
    }
    if tail.is_empty() {
        // 模式用完了：路径也必须正好用完（否则 `a/b` 会匹配 `a/b/c`）。
        return rest.is_empty();
    }
    match_segments(tail, rest)
}

/// 单段匹配：`*` 匹配任意非分隔符字符，`?` 匹配单个非分隔符字符。
fn segment_match(pattern: &str, segment: &str) -> bool {
    // 快速路径：段内没有通配符时走精确比较（绝大多数情况）。
    if !pattern.contains(['*', '?']) {
        return pattern == segment;
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = segment.chars().collect();
    // 经典回溯：`*` 不跨 `/`（此处 segment 已不含分隔符，天然满足）。
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => {
                for i in 0..=s.len() {
                    if go(&p[1..], &s[i..]) {
                        return true;
                    }
                }
                false
            }
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    go(&p, &s)
}

/// 路径是否落在**构建产物**目录里（`target/`、`.git/` 等）。
///
/// 这些目录里的文件改动对语义分析没有价值，却是 reload 循环的燃料：
/// rust-analyzer 自己在跑的 `cargo check` / `cargo metadata` 会持续写
/// `target/`，我们若把每次写入都通知过去，它就重载、再跑 cargo、再写……
///
/// 判定的目录名只在路径**任意一层**出现为下列之一时才算，这样
/// `foo/target/x` 与 `target/x` 都能挡住，而 `src/targets.rs` 这类文件名不受影响。
const BUILD_ARTIFACT_DIRS: &[&str] = &[
    "target",
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".cargo-lock",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".idea",
    ".vscode-test",
];

/// 是否是构建产物 / 版本控制内部文件（应当排除在 `didChangeWatchedFiles` 之外）。
pub fn is_build_artifact(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        BUILD_ARTIFACT_DIRS.contains(&name.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        glob_match(pattern, Path::new(path))
    }

    #[test]
    fn 相对模式匹配任意层级() {
        // LSP 服务器常直接写文件名而不带 `**/`：必须能命中任意深度，
        // 否则过滤形同虚设（会把所有通知都放行，回到 reload 循环）。
        assert!(m("Cargo.toml", "/ws/Cargo.toml"));
        assert!(m("Cargo.toml", "/ws/sub/dir/Cargo.toml"));
        assert!(!m("Cargo.toml", "/ws/Cargo.lock"));
    }

    #[test]
    fn 前导星号跨任意深度() {
        // rust-analyzer 注册的清单 glob 就是这个形状，必须能匹配到任意层。
        assert!(m("**/Cargo.toml", "/ws/Cargo.toml"));
        assert!(m("**/Cargo.toml", "/ws/a/b/Cargo.toml"));
        assert!(!m("**/Cargo.toml", "/ws/Cargo.lock"));
    }

    #[test]
    fn 双星号可以吃掉零层目录() {
        assert!(m("**/target/**", "/ws/target/x/y"));
        assert!(m("src/**/main.rs", "/ws/src/main.rs"));
        assert!(m("src/**/main.rs", "/ws/src/a/b/main.rs"));
    }

    #[test]
    fn 结尾双星号吃掉全部剩余() {
        assert!(m("**", "/ws/a/b/c.rs"));
        assert!(!m("**/Cargo.toml", "/ws/a/b/c.rs"));
    }

    #[test]
    fn 星号不跨分隔符() {
        assert!(m("src/*.rs", "/ws/src/lib.rs"));
        // `*` 不跨 `/`：`src/a/lib.rs` 的第一层多了一段目录，不命中。
        assert!(!m("src/*.rs", "/ws/src/a/lib.rs"));
        // 但模式可以在任意位置起对齐，所以从 `src` 那段开始仍能命中。
        assert!(m("src/*.rs", "/ws/other/src/lib.rs"));
    }

    #[test]
    fn 问号匹配单个字符() {
        assert!(m("a?.txt", "/ws/a1.txt"));
        assert!(!m("a?.txt", "/ws/a12.txt"));
    }

    #[test]
    fn windows_backslash_and_drive_letter_are_normalized() {
        // 之前就是这里在 Windows 上失配：glob 用 `/`，路径给的是 `C:\ws\...`。
        assert!(m("**/Cargo.toml", "C:\\ws\\sub\\Cargo.toml"));
        assert!(m("src/**/*.rs", "C:\\ws\\src\\a\\b.rs"));
    }

    #[test]
    fn 构建产物目录被排除() {
        assert!(is_build_artifact(Path::new(
            "/ws/target/debug/deps/lib.rmeta"
        )));
        assert!(is_build_artifact(Path::new("/ws/.git/index")));
        assert!(is_build_artifact(Path::new("/ws/node_modules/x/i.js")));
    }

    /// Windows 盘符路径里的 `target` 同样要认出来。
    ///
    /// `is_build_artifact` 按 `Path::components()` 逐段比对，而 components 的
    /// 切分是平台相关的：反斜杠只在 windows 上是分隔符。整条断言因此只能在
    /// windows 上成立——但**代码**两边都对，所以不能塞进上面的用例里假装
    /// 跨平台。
    #[test]
    #[cfg(windows)]
    fn windows_盘符路径的构建产物也被排除() {
        assert!(is_build_artifact(Path::new(
            r"C:\ws\target\.rustc_info.json"
        )));
    }

    #[test]
    fn 源码路径不算构建产物() {
        assert!(!is_build_artifact(Path::new("/ws/src/targets.rs")));
        assert!(!is_build_artifact(Path::new("/ws/src/lib.rs")));
        assert!(!is_build_artifact(Path::new("/ws/targets/build.rs")));
    }
}

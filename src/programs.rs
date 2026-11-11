//! 「终端里跑的是什么」：名称、图标与方块底色。
//!
//! 移植自 herdr-app 的 `src/programs.rs` 与 `defaults/programs.json`：同一张表、
//! 同一套判定顺序、同一套配色语义。表内置在 `assets/programs.json`，图标内置在
//! `assets/icons/programs/`，运行期不读用户文件（deepterm 没有 mod / home 概念，
//! 也不会把图标写到磁盘）。
//!
//! 判定顺序（与 herdr 一致，见 [`resolve`]）：
//!
//! 1. **agent 自报**（`app`，如 `claude-code`）优先——它是唯一知道「我到底是谁」
//!    的来源，进程名常被包装成 `node` / `python`。
//! 2. 解释器（`node` / `npx` / `uv` …）往前找它实际跑的那个脚本（`vite` / `pytest`）。
//! 3. 进程名直接查表。
//! 4. shell 本身 → `shell` 条目（绿色方块 + 终端图形）。
//! 5. 都不认识 → `program` 条目 + 由名字哈希出的稳定底色。
//!
//! 与 herdr 的差异只有一处：herdr 会把图标写到用户目录并允许 `programs.json`
//! 覆盖表；deepterm 全部内置，图标路径直接是 `icons/programs/*.svg`。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

/// 内置程序表（与 herdr `defaults/programs.json` 同源）。
const BUILT_IN: &str = include_str!("../assets/programs.json");

#[derive(Deserialize, Default)]
struct Table {
    #[serde(default)]
    shells: Vec<String>,
    #[serde(default)]
    interpreters: Vec<String>,
    #[serde(default)]
    programs: BTreeMap<String, Entry>,
}

#[derive(Deserialize, Default, Clone)]
struct Entry {
    #[serde(default)]
    name: Option<String>,
    /// 图标在 assets 里的路径（`icons/programs/xxx.svg`）。
    #[serde(default)]
    icon: Option<String>,
    /// 单色图形：以 `color`（或文字色）作为 alpha mask 着色；否则当作彩色图片画。
    #[serde(default)]
    tint: Option<bool>,
    /// 方块底色 `#rrggbb`。
    #[serde(default)]
    tile: Option<String>,
    /// 图形的颜色（仅 `tint` 为真时有意义）。
    #[serde(default)]
    color: Option<String>,
    /// 同一个程序的其他进程名（或 agent 的 app 名）。
    #[serde(default, rename = "match")]
    also: Vec<String>,
}

/// 解析好的程序外观：标签方块里要画的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// 表里的键（`claude` / `shell` / `program` …）。用于稳定比较与测试。
    pub key: String,
    /// 显示名（`Claude Code` / `Shell` …）。
    pub name: String,
    /// 图标路径；`None` 表示这张表没给它图标（理论上不会发生）。
    pub icon: Option<String>,
    /// 单色图形（按 `color` 着色）还是彩色图片。
    pub tint: bool,
    /// 方块底色。
    pub tile: Option<u32>,
    /// 单色图形的颜色。
    pub color: Option<u32>,
    /// 是不是 shell 本身（在跑的只是 shell，没有前台程序）。
    pub shell: bool,
    /// 是不是 CLI agent。
    pub agent: bool,
    /// 进程名（agent 自报时的 app 名），`None` 表示未知。
    pub process: Option<String>,
}

impl Program {
    /// 图标是否应按 `color` 着色（单色 mask）。
    pub fn is_tinted(&self) -> bool {
        self.tint && self.icon.is_some()
    }
}

static TABLE: LazyLock<Table> =
    LazyLock::new(|| serde_json::from_str(BUILT_IN).expect("assets/programs.json 是合法的"));

/// 解析一个终端当前跑的程序。
///
/// `app` 是 agent 自报的身份（OSC 上报的 app 名），`argv` 是当前前台进程的
/// 命令行（越完整判定越准；只有进程名也能用）。
pub fn of(app: Option<&str>, argv: &[String]) -> Program {
    resolve(&TABLE, app, argv)
}

fn base(s: &str) -> &str {
    let s = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let s = s.trim_start_matches('-');
    // Windows：`pwsh.exe` 就是 `pwsh`。
    match s.len().checked_sub(4) {
        Some(i) if s.is_char_boundary(i) && s[i..].eq_ignore_ascii_case(".exe") => &s[..i],
        _ => s,
    }
}

fn find<'a>(t: &'a Table, name: &str) -> Option<(&'a str, &'a Entry)> {
    let lower = name.to_lowercase();
    t.programs
        .iter()
        .find(|(k, _)| k.as_str() == name || k.to_lowercase() == lower)
        .or_else(|| {
            t.programs.iter().find(|(_, e)| {
                e.also
                    .iter()
                    .any(|a| a == name || a.to_lowercase() == lower)
            })
        })
        .map(|(k, e)| (k.as_str(), e))
}

fn entry<'a>(t: &'a Table, key: &str) -> &'a Entry {
    static EMPTY: LazyLock<Entry> = LazyLock::new(Entry::default);
    t.programs.get(key).unwrap_or(&EMPTY)
}

fn resolve(t: &Table, app: Option<&str>, argv: &[String]) -> Program {
    // 1. agent 自报。表里没有的 agent 仍然是一个 agent（`agent` 条目）。
    if let Some(app) = app.filter(|a| !a.is_empty()) {
        return match find(t, app) {
            Some((key, e)) => shown(key, e, None, false, true, Some(app)),
            None => shown(
                "agent",
                entry(t, "agent"),
                Some(app),
                false,
                true,
                Some(app),
            ),
        };
    }
    let Some(first) = argv.first().map(|a| base(a)) else {
        return shown("shell", entry(t, "shell"), None, true, false, None);
    };
    // 2. 解释器：`node .../vite.js dev`、`npx vite`、`uv run pytest` —— 认它真正跑的那个。
    if t.interpreters.iter().any(|i| i == first) {
        for arg in argv[1..].iter().filter(|a| !a.starts_with('-')) {
            // 脚本名去掉扩展名（`vite.js`），但绝不认模块路径（`http.server`）。
            let name = base(arg);
            let stem = [".js", ".mjs", ".cjs", ".ts", ".py", ".rb"]
                .iter()
                .find_map(|x| name.strip_suffix(x))
                .unwrap_or(name);
            if let Some((key, e)) =
                find(t, stem).filter(|(k, _)| !t.interpreters.iter().any(|i| i == k))
            {
                return shown(key, e, None, false, false, Some(first));
            }
        }
    }
    // 3. 进程名直接查表。
    if let Some((key, e)) = find(t, first) {
        return shown(key, e, None, false, false, Some(first));
    }
    // 4. shell 自己。
    if t.shells.iter().any(|s| s == first) {
        return shown(
            "shell",
            entry(t, "shell"),
            Some(first),
            true,
            false,
            Some(first),
        );
    }
    // 5. 不认识：`program` + 稳定哈希底色。
    let mut p = shown(
        "program",
        entry(t, "program"),
        Some(first),
        false,
        false,
        Some(first),
    );
    p.tile = Some(hash_color(first));
    p.color = Some(0xFFFFFF);
    p
}

fn shown(
    key: &str,
    e: &Entry,
    name: Option<&str>,
    shell: bool,
    agent: bool,
    process: Option<&str>,
) -> Program {
    Program {
        key: key.to_owned(),
        name: name
            .map(str::to_owned)
            .or_else(|| e.name.clone())
            .unwrap_or_else(|| key.to_owned()),
        icon: e.icon.clone(),
        tint: e.tint.unwrap_or(true),
        tile: e.tile.as_deref().and_then(parse_hex),
        color: e.color.as_deref().and_then(parse_hex),
        shell,
        agent,
        process: process.map(str::to_owned),
    }
}

fn parse_hex(value: &str) -> Option<u32> {
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if hex.len() == 6 {
        u32::from_str_radix(hex, 16).ok()
    } else if hex.len() == 8 {
        // 只取 RGB：方块底色不需要 alpha（表里也没有半透明项）。
        u32::from_str_radix(&hex[..6], 16).ok()
    } else {
        None
    }
}

/// 没被列进表的程序的稳定底色（与 herdr 同一套调色板与哈希）。
fn hash_color(s: &str) -> u32 {
    const PALETTE: [u32; 8] = [
        0x6D5BD0, 0x2F80ED, 0x0F9D8A, 0xD9822B, 0xC2417D, 0x3A7D44, 0x8A5A44, 0x4E5D94,
    ];
    let h = s
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app: Option<&str>, argv: &[&str]) -> String {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        of(app, &argv).key
    }

    #[test]
    fn agents_shells_tools_and_what_interpreters_run() {
        // 与 herdr `programs.rs` 的同名测试一一对应。
        assert_eq!(key(Some("claude-code"), &["/bin/zsh"]), "claude");
        assert_eq!(key(Some("grok-build"), &["/bin/zsh"]), "grok");
        assert_eq!(key(Some("dsh-tui"), &["/bin/zsh"]), "dsh-tui");
        assert_eq!(key(None, &["dsh-tui"]), "dsh-tui");
        assert_eq!(key(Some("newagent"), &[]), "agent");
        assert_eq!(key(None, &["/bin/zsh"]), "shell");
        assert_eq!(key(None, &["-zsh"]), "shell");
        assert_eq!(key(None, &["nvim", "x.rs"]), "nvim");
        assert_eq!(key(None, &["vi"]), "vim");
        assert_eq!(
            key(None, &["node", "/x/node_modules/.bin/vite", "dev"]),
            "vite"
        );
        assert_eq!(key(None, &["npx", "vitest"]), "vitest");
        assert_eq!(key(None, &["node", "server.js"]), "node");
        assert_eq!(
            key(None, &["python3", "-m", "http.server", "8799"]),
            "python"
        );
        assert_eq!(key(None, &["uv", "run", "pytest"]), "pytest");
        assert_eq!(key(None, &["mytool"]), "program");
        assert_eq!(key(None, &[r"C:\Windows\powershell.exe"]), "shell");
    }

    #[test]
    fn unknown_programs_get_a_stable_color() {
        let a = of(None, &["mytool".to_string()]);
        let b = of(None, &["mytool".to_string()]);
        assert_eq!(a.tile, b.tile);
        assert!(a.tile.is_some());
        // 不同名字（通常）落到不同底色；至少不能全是同一个。
        let names = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let colors: std::collections::HashSet<_> = names
            .iter()
            .map(|n| of(None, &[n.to_string()]).tile)
            .collect();
        assert!(colors.len() > 1);
    }

    /// 表里每个条目都必须有图标，且图标文件真的在 assets 里——
    /// herdr 用 `every_entry_has_its_icon` 守这条，漏一个图标会在 UI 上
    /// 变成「方块语义正确、图形空掉」。
    #[test]
    fn every_entry_has_its_icon() {
        for (key, entry) in &TABLE.programs {
            let icon = entry
                .icon
                .as_deref()
                .unwrap_or_else(|| panic!("{key} 没有图标"));
            let file = icon.strip_prefix("icons/").unwrap_or(icon);
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets")
                .join("icons")
                .join(file);
            assert!(path.is_file(), "{key}: {icon} 不在 assets 里（{path:?}）");
        }
    }

    #[test]
    fn shell_and_agent_are_flagged() {
        let shell = of(None, &["/bin/zsh".to_string()]);
        assert!(shell.shell);
        assert!(!shell.agent);
        let agent = of(Some("claude-code"), &[]);
        assert!(agent.agent);
        assert!(!agent.shell);
        assert_eq!(agent.name, "Claude Code");
    }

    /// 表里的 tile 颜色必须都能解析：解析失败会让方块退回主题色，
    /// 表现为「某些程序没有品牌色」这类难查的视觉回归。
    #[test]
    fn every_tile_color_parses() {
        for (key, entry) in &TABLE.programs {
            if let Some(tile) = entry.tile.as_deref() {
                assert!(parse_hex(tile).is_some(), "{key}: 无法解析 tile {tile}");
            }
            if let Some(color) = entry.color.as_deref() {
                assert!(parse_hex(color).is_some(), "{key}: 无法解析 color {color}");
            }
        }
    }
}

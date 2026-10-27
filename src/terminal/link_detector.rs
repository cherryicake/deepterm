//! 终端内容里的链接识别：OSC8 超链接 + 正则/词法混合的网站链接。
//!
//! URL 识别刻意不用「一条大正则 + 尾部裁标点」的老写法。那种写法有两个
//! 结构性缺陷：字符类为了覆盖 URL 语法必须把 `()[]{}` 一起收进来，于是
//! `{"url":"https://a.com/v1"}` 会命中 `https://a.com/v1"}`、`(https://a.com)`
//! 会靠「砍尾」侥幸活下来；而尾部裁剪又无法判断括号配不配对，
//! `(https://…/Foo_(bar))` 会被砍成非法 URL。
//!
//! 现在改成「**锚点正则 + 括号配对尾扫**」：
//! 正则只负责找到「从哪儿开始」（scheme / `www.` / 裸域名 / `localhost`），
//! 之后由 [`scan_tail`] 逐字符吃，遇到**没有配对开启符**的 `)`/`]`/`}` 就停。
//! 收尾标点（`.`/`,`/`;`/`:`/`!`/`?`）在尾扫之后统一剥掉。

use regex::Regex;
use std::cmp::Reverse;
use std::sync::LazyLock;

use crate::terminal::row_text::WrappedText;
use crate::terminal::terminal::IndexedCell;

/// 显式 scheme 锚点。`ftp://` 曾经也收在这里，但 `utils::safe_open` 的
/// http/https 白名单会把点击静默吞掉 —— 与其给出一个点不动的链接，不如不认。
static SCHEME_ANCHOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)https?://").unwrap());

/// `www.` 前缀（后面必须还有域名标签）。
static WWW_ANCHOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)www\.[a-z0-9]").unwrap());

/// `localhost[:port]`——没有点号，裸域名规则覆盖不到。
static LOCALHOST_ANCHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)localhost(?::\d{1,5})?").unwrap());

/// 裸域名的顶级域白名单。
///
/// 这份名单同时承担「排除文件扩展名」的职责：`main.rs` / `Cargo.toml` /
/// `deploy.sh` / `libfoo.so` / `1.2.3` / `0.0.0.0:8080` 的末段都不在名单里
/// （末段必须是字母），所以版本号、IP、带端口的地址、以及 Rust/C/Shell
/// 工程里最常见的一批文件名天然落选；而 `github.com` / `crates.io` /
/// `npmjs.com/crates/gpui` 这类工具输出里的真域名能被认出来。
///
/// **代价是自觉的**：`docs.rs` 这种真实存在的站点因为 `.rs` 冲突而不认，
/// 本项目里 `main.rs` 出现的频率远高于 `docs.rs`。
const TLDS: &[&str] = &[
    // —— 通用商业/通用顶级域 ——
    "com",
    "cn",
    "net",
    "org",
    "edu",
    "gov",
    "mil",
    "int",
    "info",
    "biz",
    "pro",
    "name",
    "mobi",
    "asia",
    "jobs",
    "coop",
    "aero",
    "museum",
    // —— 新通用顶级域 ——
    "dev",
    "app",
    "ai",
    "io",
    "co",
    "cloud",
    "tech",
    "space",
    "website",
    "host",
    "press",
    "zone",
    "today",
    "world",
    "life",
    "live",
    "news",
    "blog",
    "wiki",
    "link",
    "page",
    "site",
    "online",
    "store",
    "shop",
    "tools",
    "codes",
    "digital",
    "network",
    "systems",
    "solutions",
    "services",
    "agency",
    "studio",
    "design",
    "community",
    "social",
    "chat",
    "video",
    "photo",
    "media",
    "center",
    "company",
    "group",
    "team",
    "works",
    "fyi",
    "one",
    "fun",
    "top",
    "vip",
    "icu",
    "best",
    "gg",
    "im",
    "ml",
    "cf",
    "ga",
    "tk",
    // —— 国家和地区 ——
    "to",
    "tv",
    "me",
    "us",
    "uk",
    "de",
    "fr",
    "jp",
    "kr",
    "ru",
    "eu",
    "ca",
    "au",
    "br",
    "it",
    "es",
    "nl",
    "se",
    "no",
    "fi",
    "dk",
    "cz",
    "at",
    "ch",
    "be",
    "pt",
    "gr",
    "tr",
    "ua",
    "nz",
    "za",
    "mx",
    "ar",
    "cl",
    "tw",
    "hk",
    "sg",
    "id",
    "th",
    "ph",
    "vn",
    "my",
    "il",
    "ie",
];

/// 裸域名锚点：`(标签.)+白名单TLD[:端口]`。
static BARE_DOMAIN_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    // 长后缀优先，避免 `co` 抢在 `com` 前面
    let mut alts: Vec<String> = TLDS.iter().map(|t| regex::escape(t)).collect();
    alts.sort_by_key(|t| Reverse(t.len()));
    let alt = alts.join("|");
    Regex::new(&format!(
        r"(?i)(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+(?:{alt})(?::\d{{1,5}})?"
    ))
    .unwrap()
});

/// URL 收尾处要剥掉的标点（句末语气词，不是 URL 的一部分）。
const TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"'];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlRange {
    pub start_col: usize,
    pub end_col: usize,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyperlinkRange {
    pub start_col: usize,
    pub end_col: usize,
    pub uri: String,
    pub id: String,
}

/// 锚点优先级：同一位置命中多种锚点时，scheme 最可信。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AnchorKind {
    Scheme = 0,
    Www = 1,
    Localhost = 2,
    BareDomain = 3,
}

struct Anchor {
    /// URL 在文本中的起始字节。
    start: usize,
    /// 第一个「尾部」字符的字节（scheme 锚点即 `://` 之后）。
    tail_from: usize,
    kind: AnchorKind,
}

/// 收集候选锚点（已按起点、优先级排序）。
fn anchors(text: &str) -> Vec<Anchor> {
    let mut out: Vec<Anchor> = Vec::new();
    for (re, kind) in [
        (&*SCHEME_ANCHOR, AnchorKind::Scheme),
        (&*WWW_ANCHOR, AnchorKind::Www),
        (&*LOCALHOST_ANCHOR, AnchorKind::Localhost),
        (&*BARE_DOMAIN_ANCHOR, AnchorKind::BareDomain),
    ] {
        for m in re.find_iter(text) {
            // 紧跟在 ASCII 字母/数字后的锚点不算（`xhttps://`、`1www.a.com`）；
            // 紧跟在 `@` 后的也不算（邮箱 `user@example.com` 不该被当成网站，
            // 指向 `https://example.com` 是错的）。注意只拦 ASCII：`见https://…`
            // 这种中文正文里紧贴链接的写法必须认出来。
            if let Some(prev) = text[..m.start()].chars().next_back() {
                if prev.is_ascii_alphanumeric() || prev == '@' {
                    continue;
                }
                // scheme 之外的锚点再拦路径分隔符/词内字符：`assets/logo.ai`、
                // `./MyApp.app/Contents/…`、`my_logo.ai` 里的「域名」其实是
                // 文件名的一部分。放行的话 URL 分支会在命中测试里盖过文件
                // 路径，把真实的文件链接遮蔽成 `https://logo.ai` 这种错网站。
                // scheme 锚点不受限：看到 `://` 已是强信号。
                if kind != AnchorKind::Scheme && matches!(prev, '.' | '/' | '\\' | '_') {
                    continue;
                }
            }
            // 裸域名/`localhost` 必须在「词的边界」收尾：TLD 白名单里 `to` 在，
            // 没有这道检查 `Cargo.toml` 会被切成裸域名 `Cargo.to` 再被尾扫吞掉
            // 剩下的 `ml`。regex crate 不支持 lookahead，所以在代码里判。
            // 后面的 `:非数字` 同样拒：`localhost:abc` / `example.com:x` 不是
            // 地址，regex 只吃得到域名部分，放行会得到点开是错的链接。
            if matches!(kind, AnchorKind::BareDomain | AnchorKind::Localhost) {
                let mut next = text[m.end()..].chars();
                if let Some(c) = next.next() {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        continue;
                    }
                    if c == ':' && !next.next().is_some_and(|d| d.is_ascii_digit()) {
                        continue;
                    }
                }
            }
            out.push(Anchor {
                start: m.start(),
                tail_from: m.end(),
                kind,
            });
        }
    }
    out.sort_by_key(|a| (a.start, a.kind));
    out
}

/// 从 `start` 起逐字符吃 URL 主体，返回结束字节（开区间）。
///
/// 停止条件：空白/控制字符、引号尖括号、全角标点，以及**括号不配对**的
/// `)`/`]`/`}`。括号配对时照吃，所以 `…/Foo_(bar)` 不会被腰斩；而
/// `{"url":"https://a.com/v1"}`、`[![b](https://i.io/x)](https://a.com)`
/// 会在第一个不配对的收尾括号处停下，不再把后半个 URL 吞进来。
fn scan_tail(text: &str, start: usize) -> usize {
    // 括号深度位图：bit0 `()`、bit1 `[]`、bit2 `{}`
    let mut open: u8 = 0;
    for (idx, ch) in text[start..].char_indices() {
        let bit = match ch {
            '(' => 0,
            '[' => 1,
            '{' => 2,
            ')' | ']' | '}' => match ch {
                ')' => 0,
                ']' => 1,
                _ => 2,
            },
            _ => {
                if is_hard_stop(ch) {
                    return start + idx;
                }
                continue;
            }
        };
        if matches!(ch, ')' | ']' | '}') {
            if open & (1 << bit) == 0 {
                return start + idx;
            }
            open &= !(1 << bit);
        } else {
            open |= 1 << bit;
        }
    }
    text.len()
}

/// 无条件终止 URL 的字符。
fn is_hard_stop(ch: char) -> bool {
    match ch {
        '<' | '>' | '"' | '\'' | '`' => true,
        // 全角括号与全角标点：中文正文里紧跟链接的这些不是 URL 的一部分
        '\u{FF08}' | '\u{FF09}' | '\u{300C}' | '\u{300D}' | '\u{300E}' | '\u{300F}' => true,
        '\u{FF0C}' | '\u{3001}' | '\u{3002}' | '\u{FF1B}' | '\u{FF1A}' | '\u{FF01}'
        | '\u{FF1F}' => true,
        _ => ch.is_whitespace() || ch.is_control(),
    }
}

/// 剥掉 URL 尾部的句末标点。
fn trim_url_tail(url: &str) -> &str {
    url.trim_end_matches(TRAILING_PUNCT)
}

/// 在（可能跨软换行的）若干行里识别 URL，返回 `(行下标, UrlRange)`。
///
/// 一个 URL 横跨软换行时会按行登记两段：两半各自可点、可高亮，`url` 是
/// 拼接后的完整地址——否则第一行会被识别成 `https://exa` 这种腰斩假链接。
pub fn detect_urls_in_rows<'a>(
    rows: impl IntoIterator<Item = &'a [IndexedCell]>,
) -> Vec<(usize, UrlRange)> {
    let joined = WrappedText::from_rows(rows);
    detect_urls_in_joined(&joined)
}

/// 单行版本的 URL 识别（列号相对该行）。
pub fn detect_urls_in_line(cells: &[IndexedCell]) -> Vec<UrlRange> {
    detect_urls_in_rows([cells])
        .into_iter()
        .map(|(_, range)| range)
        .collect()
}

/// 在已拼好的跨行文本上识别 URL。
pub fn detect_urls_in_joined(joined: &WrappedText) -> Vec<(usize, UrlRange)> {
    let text = &joined.text;
    let mut out = Vec::new();
    let mut consumed = 0usize;
    for anchor in anchors(text) {
        if anchor.start < consumed {
            continue;
        }
        let raw_end = scan_tail(text, anchor.tail_from);
        let raw = &text[anchor.start..raw_end];
        let url = trim_url_tail(raw);
        if !is_complete_url(url, anchor.kind) {
            continue;
        }
        let spans = joined.span_rows(anchor.start, anchor.start + url.len());
        if spans.is_empty() {
            continue;
        }
        // 后面的锚点若落在本次命中内部就跳过（`https://x.com/?q=www.a.com`）
        consumed = anchor.start + url.len();
        let url = normalize_url(url, anchor.kind);
        for (row, start_col, end_col) in spans {
            out.push((
                row,
                UrlRange {
                    start_col,
                    end_col,
                    url: url.clone(),
                },
            ));
        }
    }
    out
}

/// 命中是否是一个可用的 URL（scheme 必须有主机名，裸域名不能被剥空）。
fn is_complete_url(url: &str, kind: AnchorKind) -> bool {
    if url.is_empty() {
        return false;
    }
    match kind {
        AnchorKind::Scheme => {
            let Some((_scheme, rest)) = url.split_once("://") else {
                return false;
            };
            // `https://` 后面必须有主机名，否则只是协议头
            !rest.is_empty() && !rest.starts_with('/')
        }
        AnchorKind::Www => url[4..].contains('.'),
        AnchorKind::Localhost => true,
        AnchorKind::BareDomain => url.contains('.'),
    }
}

/// 裸域名 / `www.` / `localhost` 补上 scheme，让打开动作有个 http(s) 地址可用。
///
/// `localhost` 补 `http://`：本机开发服务器绝大多数是明文 HTTP，补 https 会
/// 直接握手失败（浏览器还会把错误藏成一句「无法建立安全连接」）。
fn normalize_url(url: &str, kind: AnchorKind) -> String {
    if kind == AnchorKind::Scheme || url.contains("://") {
        url.to_string()
    } else if kind == AnchorKind::Localhost {
        format!("http://{url}")
    } else {
        format!("https://{url}")
    }
}

/// Group contiguous cells carrying the same OSC8 hyperlink (`id` + `uri`)
/// into per-row ranges, returning `(行下标, HyperlinkRange)`.
///
/// OSC8 的链接身份存在单元格的 `CellExtra` 里（见 [`IndexedCell::hyperlink`]），
/// 因此本函数只需要单元格的列号连续性，不碰文本。跨软换行时同 `id`/`uri` 的
/// 相邻格仍算同一个链接（列号在换行处从 0 重新开始，只在同一行内要求连续），
/// 否则被折断的长链接在后半截会掉出 OSC8 分支、被正则当成另一个网站。
pub fn detect_hyperlinks_in_rows<'a>(
    rows: impl IntoIterator<Item = &'a [IndexedCell]>,
) -> Vec<(usize, HyperlinkRange)> {
    fn flush(
        current: &mut Option<(String, String, usize, usize, usize)>,
        ranges: &mut Vec<(usize, HyperlinkRange)>,
    ) {
        if let Some((id, uri, row, start, end)) = current.take() {
            ranges.push((
                row,
                HyperlinkRange {
                    start_col: start,
                    end_col: end,
                    uri,
                    id,
                },
            ));
        }
    }

    let mut ranges = Vec::new();
    let mut current: Option<(String, String, usize, usize, usize)> = None;

    for (row, cells) in rows.into_iter().enumerate() {
        let mut at_row_start = true;
        for cell in cells {
            let col = cell.point.column.0;
            let Some(link) = cell.hyperlink.as_ref().filter(|l| !l.uri().is_empty()) else {
                flush(&mut current, &mut ranges);
                at_row_start = false;
                continue;
            };
            // 同一个 `id`/`uri` 且紧邻上一格 → 延长当前 run。
            let cont = match &current {
                Some((cur_id, cur_uri, cur_row, _, cur_end))
                    if cur_id == link.id() && cur_uri == link.uri() =>
                {
                    if *cur_row == row {
                        col == *cur_end
                    } else {
                        // 折行处：只允许在续行的第一格接上（软换行）
                        *cur_row + 1 == row && at_row_start
                    }
                }
                _ => false,
            };
            if cont {
                let cur_row = current.as_ref().map(|c| c.2);
                if cur_row == Some(row) {
                    if let Some((_, _, _, _, cur_end)) = current.as_mut() {
                        *cur_end = col + 1;
                    }
                } else {
                    // 折行处：上一行那段已经结束，登记后从本行新起一段。
                    flush(&mut current, &mut ranges);
                    current = Some((
                        link.id().to_owned(),
                        link.uri().to_owned(),
                        row,
                        col,
                        col + 1,
                    ));
                }
            } else {
                flush(&mut current, &mut ranges);
                current = Some((
                    link.id().to_owned(),
                    link.uri().to_owned(),
                    row,
                    col,
                    col + 1,
                ));
            }
            at_row_start = false;
        }
        // 行尾的连续格跨行续接（软换行），因此不在行末 flush。
    }
    flush(&mut current, &mut ranges);
    ranges
}

/// 单行版本的 OSC8 识别（列号相对该行）。
pub fn detect_hyperlinks_in_line(cells: &[IndexedCell]) -> Vec<HyperlinkRange> {
    detect_hyperlinks_in_rows([cells])
        .into_iter()
        .map(|(_, range)| range)
        .collect()
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::{Cell, Flags};

    use super::*;
    use crate::terminal::row_text::row_text;
    use crate::terminal::terminal::PackedCell;

    /// 按 1 列 1 字符（ASCII）构造单元格。
    fn cells_of(text: &str) -> Vec<IndexedCell> {
        text.chars()
            .enumerate()
            .map(|(col, ch)| {
                let mut packed = PackedCell::from(&Cell::default());
                packed.c = ch;
                IndexedCell {
                    point: Point::new(Line(0), Column(col)),
                    cell: packed,
                    zerowidth: None,
                    hyperlink: None,
                }
            })
            .collect()
    }

    fn url_list(text: &str) -> Vec<String> {
        detect_urls_in_line(&cells_of(text))
            .into_iter()
            .map(|r| r.url)
            .collect()
    }

    fn span_of(text: &str, url: &str) -> (usize, usize) {
        detect_urls_in_line(&cells_of(text))
            .into_iter()
            .find(|r| r.url.contains(url))
            .map(|r| (r.start_col, r.end_col))
            .unwrap_or_else(|| panic!("{url:?} not found in {text:?}"))
    }

    #[test]
    fn basic_scheme_and_www() {
        assert_eq!(url_list("see https://a.com/x now"), vec!["https://a.com/x"]);
        assert_eq!(url_list("www.example.com"), vec!["https://www.example.com"]);
    }

    #[test]
    fn json_closing_brace_not_swallowed() {
        // 旧实现命中 `https://a.com/v1"}`；`}` 不配对，必须停在这里
        assert_eq!(
            url_list(r#"{"url":"https://api.example.com/v1/users"}"#),
            vec!["https://api.example.com/v1/users"]
        );
    }

    #[test]
    fn unbalanced_paren_stops_markdown_badge() {
        let text = "[![b](https://img.shields.io/badge/a-b-c)](https://github.com/zed)";
        assert_eq!(
            url_list(text),
            vec![
                "https://img.shields.io/badge/a-b-c",
                "https://github.com/zed"
            ]
        );
    }

    #[test]
    fn balanced_paren_is_kept() {
        assert_eq!(
            url_list("(https://en.wikipedia.org/wiki/Foo_(bar))"),
            vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn adjacent_markdown_links_are_separate() {
        assert_eq!(
            url_list("[a](https://a.com)[b](https://b.com)"),
            vec!["https://a.com", "https://b.com"]
        );
    }

    #[test]
    fn trailing_sentence_punct_is_trimmed() {
        assert_eq!(url_list("see https://a.com/x!"), vec!["https://a.com/x"]);
        assert_eq!(url_list("see https://a.com/x..."), vec!["https://a.com/x"]);
        assert_eq!(url_list("[https://a.com/x]"), vec!["https://a.com/x"]);
    }

    #[test]
    fn case_insensitive_scheme_and_www() {
        assert_eq!(
            url_list("HTTPS://Example.COM/A"),
            vec!["HTTPS://Example.COM/A"]
        );
        assert_eq!(url_list("WWW.Example.com"), vec!["https://WWW.Example.com"]);
    }

    #[test]
    fn bare_domain_whitelist() {
        assert_eq!(
            url_list("clone github.com/zed-industries/zed"),
            vec!["https://github.com/zed-industries/zed"]
        );
        assert_eq!(
            url_list("cargo add crates.io/crates/gpui"),
            vec!["https://crates.io/crates/gpui"]
        );
        assert_eq!(
            url_list("running on localhost:3000"),
            vec!["http://localhost:3000"]
        );
        assert_eq!(
            url_list("open http://127.0.0.1:8080/x"),
            vec!["http://127.0.0.1:8080/x"]
        );
    }

    #[test]
    fn path_components_are_not_domains() {
        // `assets/logo.ai`、`./MyApp.app/Contents/…`、`my_logo.ai` 里的「域名」
        // 是文件名的一部分：左边界是路径分隔符/词内字符时不该再起锚，否则
        // URL 分支会盖过文件路径，把真实文件链接遮蔽成错网站。
        for text in [
            "assets/logo.ai",
            "./MyApp.app/Contents/MacOS/MyApp",
            "data/foo.io",
            "my_logo.ai",
        ] {
            assert!(url_list(text).is_empty(), "{text:?} 不该被当成网站");
        }
        // 真正的域名不受影响。
        assert_eq!(
            url_list("clone github.com/zed-industries/zed"),
            vec!["https://github.com/zed-industries/zed"]
        );
        assert_eq!(url_list("see logo.ai now"), vec!["https://logo.ai"]);
    }

    #[test]
    fn localhost_port_must_be_numeric() {
        // `localhost:abc` 不是地址：regex 只吃到 `localhost`，由右边界守卫拦下。
        assert!(url_list("connect localhost:abc now").is_empty());
        assert_eq!(
            url_list("serve on localhost:8080/x"),
            vec!["http://localhost:8080/x"]
        );
    }

    #[test]
    fn ftp_links_are_not_detected() {
        // `safe_open` 只放行 http/https，ftp 检测出来也点不动，索性不认。
        assert!(url_list("get ftp://mirror.example.com/x").is_empty());
    }

    #[test]
    fn docs_rs_is_deliberately_not_a_domain() {
        // `.rs` 不在 TLD 白名单里：`main.rs` 在本项目里的出现频率远高于
        // `docs.rs`，误判「文件名为网站」的代价更大。
        assert!(url_list("see docs.rs for details").is_empty());
    }

    #[test]
    fn file_extensions_are_not_domains() {
        for text in [
            "compiling main.rs",
            "reading Cargo.toml",
            "version 1.2.3 installed",
            "listening 0.0.0.0:8080",
            "wrote deploy.sh",
            "linking libfoo.so",
            "building obj/main.o",
            "running 12:30:45",
        ] {
            assert!(url_list(text).is_empty(), "false positive in {text:?}");
        }
    }

    #[test]
    fn email_is_not_a_website() {
        assert!(url_list("mail to user@example.com").is_empty());
    }

    #[test]
    fn non_ascii_url_is_detected() {
        assert_eq!(
            url_list("文档 https://例子.中国/说明 结束"),
            vec!["https://例子.中国/说明"]
        );
    }

    #[test]
    fn column_range_is_exact() {
        // `https://a.com/x` 是 15 列：4..=18，命中区必须是 [4, 19)
        let (start, end) = span_of("see https://a.com/x now", "a.com");
        assert_eq!((start, end), (4, 19));
    }

    #[test]
    fn wide_char_before_url_shifts_columns() {
        let mut cells = Vec::new();
        let mut col = 0;
        for ch in "见".chars() {
            let mut packed = PackedCell::from(&Cell::default());
            packed.c = ch;
            packed.flags = Flags::WIDE_CHAR;
            cells.push(IndexedCell {
                point: Point::new(Line(0), Column(col)),
                cell: packed,
                zerowidth: None,
                hyperlink: None,
            });
            col += 2;
        }
        // 占位格（内容是空格，不该进文本）
        let mut spacer = PackedCell::from(&Cell::default());
        spacer.c = ' ';
        spacer.flags = Flags::WIDE_CHAR_SPACER;
        cells.push(IndexedCell {
            point: Point::new(Line(0), Column(1)),
            cell: spacer,
            zerowidth: None,
            hyperlink: None,
        });
        for (i, mut c) in cells_of("https://a.com").into_iter().enumerate() {
            c.point = Point::new(Line(0), Column(col + i));
            cells.push(c);
        }
        let ranges = detect_urls_in_line(&cells);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].url, "https://a.com");
        assert_eq!((ranges[0].start_col, ranges[0].end_col), (2, 15));
    }

    #[test]
    fn wrapped_url_is_one_complete_link() {
        let first = cells_of("https://exa");
        let second = cells_of("mple.com/docs");
        let rows = detect_urls_in_rows([first.as_slice(), second.as_slice()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[0].1.url, "https://example.com/docs");
        assert_eq!(rows[1].0, 1);
        assert_eq!(rows[1].1.url, "https://example.com/docs");
        assert_eq!((rows[0].1.start_col, rows[0].1.end_col), (0, 11));
        assert_eq!((rows[1].1.start_col, rows[1].1.end_col), (0, 13));
    }

    #[test]
    fn row_text_is_not_needed_for_ascii() {
        // 冒烟：确认 row_text 与本模块的列口径一致
        let row = row_text(&cells_of("ab"));
        assert_eq!(row.span(0, 2), Some((0, 2)));
    }

    /// 给一段文本的每个格子挂上同一个 OSC8 链接。
    fn cells_with_link(text: &str, id: &str, uri: &str) -> Vec<IndexedCell> {
        let link = alacritty_terminal::term::cell::Hyperlink::new(Some(id), uri.to_owned());
        cells_of(text)
            .into_iter()
            .map(|mut c| {
                c.hyperlink = Some(link.clone());
                c
            })
            .collect()
    }

    /// 把单元格序列的列号从 `from` 起重新编号（拼接几段文本时用）。
    fn at_cols(mut cells: Vec<IndexedCell>, from: usize) -> Vec<IndexedCell> {
        for (i, c) in cells.iter_mut().enumerate() {
            c.point = Point::new(Line(0), Column(from + i));
        }
        cells
    }

    #[test]
    fn osc8_runs_stop_at_the_first_bare_cell() {
        // `see` 不带链接 → run 必须从第 3 列起算，不能把整行都算进去
        let mut cells = at_cols(cells_of("see"), 0);
        cells.extend(at_cols(cells_with_link("abc", "1", "https://a.com"), 3));
        let ranges = detect_hyperlinks_in_line(&cells);
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start_col, ranges[0].end_col), (3, 6));
        assert_eq!(ranges[0].uri, "https://a.com");
        assert_eq!(ranges[0].id, "1");
    }

    #[test]
    fn osc8_survives_soft_wrap() {
        // `id`/`uri` 相同的连续格跨软换行仍是同一个链接：两行各登记一段，
        // 否则后半截会掉到正则 URL 分支被当成另一个网站。
        let first = cells_with_link("https://exa", "1", "https://example.com/x");
        let second = cells_with_link("mple.com/x", "1", "https://example.com/x");
        let rows = detect_hyperlinks_in_rows([first.as_slice(), second.as_slice()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 0);
        assert_eq!((rows[0].1.start_col, rows[0].1.end_col), (0, 11));
        assert_eq!(rows[1].0, 1);
        assert_eq!((rows[1].1.start_col, rows[1].1.end_col), (0, 10));
        assert!(rows.iter().all(|(_, r)| r.uri == "https://example.com/x"));
    }

    #[test]
    fn osc8_does_not_merge_across_different_links() {
        let first = cells_with_link("aaa", "1", "https://a.com");
        let second = cells_with_link("bbb", "2", "https://b.com");
        let rows = detect_hyperlinks_in_rows([first.as_slice(), second.as_slice()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.uri, "https://a.com");
        assert_eq!(rows[1].1.uri, "https://b.com");
        assert_eq!((rows[1].1.start_col, rows[1].1.end_col), (0, 3));
    }

    #[test]
    fn osc8_gap_breaks_the_run() {
        // 同一行里 link / 无 / link：两段，不能合并成一段
        let mut cells = at_cols(cells_with_link("aa", "1", "https://a.com"), 0);
        cells.extend(at_cols(cells_of("x"), 2));
        cells.extend(at_cols(cells_with_link("bb", "1", "https://a.com"), 3));
        let ranges = detect_hyperlinks_in_line(&cells);
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].start_col, ranges[0].end_col), (0, 2));
        assert_eq!((ranges[1].start_col, ranges[1].end_col), (3, 5));
    }
}

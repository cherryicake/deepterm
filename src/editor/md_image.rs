//! Markdown 图片：URL 解析 → 加载（本地文件 / http(s) 远程）→ 视图侧缓存。
//!
//! 只读预览（[`super::markdown::Markdown`]）与实时编辑
//! （[`super::markdown_live::view::LiveMarkdownView`]）共用这一份实现，
//! 保证两处对同一个 `![](...)` 有完全相同的解析与加载结果。
//!
//! ## 为什么需要这个模块
//!
//! 此前两条渲染路径各自把 Markdown 里的图片地址**原样**当成
//! `std::path::Path` 去读盘：
//!
//! - 相对路径（`assets/a.png`、`notes.assets/image.png`）被按**进程工作目录**
//!   解析，而不是按**文档所在目录**解析。Typora 式粘贴的图片正是相对路径，
//!   于是打开任何别处的笔记都只看到“加载失败”占位卡片；
//! - 含 `..` 的合法相对路径（`../images/a.png`）被安全门禁直接拒掉；
//! - `file://` URL、百分号转义（`my%20pic.png`）、`~` 一律不支持；
//! - 远程 URL（`http(s)://...`）按“策略”直接不加载。
//!
//! 本模块把这几件事一次性补齐，并把「什么可以加载」收敛到一个函数
//! [`resolve`]：本地文件仍受扩展名/大小限制（避免把任意文件读进解码器），
//! 远程走带超时、限流量、限制重定向的 HTTP 客户端。
//!
//! ## 线程模型
//!
//! 解码和网络请求都在 gpui 后台执行器里跑：渲染路径只做一次 `metadata`
//! （本地）或一次缓存查询（远程），首帧给占位卡片，加载完成后写回缓存并
//! `notify` 重绘。缓存按**解析后的身份**（绝对路径 / 远程 URL）做键，
//! 同一份文档里重复引用同一张图只加载一次。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{Context, Image};

use crate::editor::image_viewer::load_image_from_bytes;

/// 单张图片字节上限。与解码器门禁同源（[`crate::editor::image_viewer::MAX_IMAGE_BYTES`]），
/// 只在这里做一次 `usize → u64` 转换，供 `metadata` 预筛和 HTTP 限流量使用。
pub(crate) const MAX_IMAGE_BYTES: u64 = crate::editor::image_viewer::MAX_IMAGE_BYTES as u64;

/// 远程图片请求总超时（连接 + 重定向 + 读 body）。
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

/// 远程加载失败后的重试间隔：断网时不会每帧重试，网络恢复后也能自愈。
const REMOTE_RETRY: Duration = Duration::from_secs(60);

/// 解析后的图片加载源。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImageSource {
    /// 本地文件（已按文档目录解析成绝对路径）。
    Local(PathBuf),
    /// 远程 http(s) URL。
    Remote(String),
}

impl ImageSource {
    /// 缓存键：解析后的身份，而不是原始字符串。
    ///
    /// 用原始字符串做键时，`./a.png` 与 `a.png` 会各占一份位图，切换文档后
    /// 同一张图还会因相对路径相同而误命中上一份文档的缓存。
    fn cache_key(&self) -> String {
        match self {
            ImageSource::Local(path) => format!("file:{}", path.display()),
            ImageSource::Remote(url) => format!("url:{url}"),
        }
    }
}

/// 一帧内图片卡片的渲染状态。
#[derive(Clone)]
pub(crate) enum SlotState {
    /// 不是可加载的图片源（未知 scheme、非图片扩展名、空地址……）。
    Unsupported,
    /// 正在后台加载（首帧占位）。
    Loading,
    /// 已解码。
    Ready(Arc<Image>, (u32, u32)),
    /// 文件不存在 / 下载失败 / 解码失败。
    Failed,
}

/// 文件指纹 `(mtime, len)`；`None` 表示文件不存在或不是普通文件。
type Stamp = Option<(SystemTime, u64)>;

fn local_stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        meta.len(),
    ))
}

enum Slot {
    Loading,
    Ready {
        image: Arc<Image>,
        size: (u32, u32),
        stamp: Stamp,
    },
    Failed {
        stamp: Stamp,
        at: Instant,
    },
}

/// 图片加载缓存 + 后台加载器，作为一个独立实体挂在文档视图上。
///
/// 用一个独立实体（而不是视图内的 `HashMap`）的原因：只读预览的渲染入口
/// 只拿得到 `&mut App`，拿不到自己的 `Context`，无法在那里 `cx.spawn`；
/// 独立实体既能在渲染期更新，也能在加载完成后 `notify`，视图通过
/// `cx.observe` 收到重绘通知。
pub(crate) struct ImageCache {
    slots: HashMap<String, Slot>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    pub(crate) fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// 清空缓存（`set_text` 时调用：上一份文档的位图不应该继续驻留）。
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        if self.slots.is_empty() {
            return;
        }
        self.slots.clear();
        cx.notify();
    }

    /// 测试探针：已解码条目数（验证渲染路径真的触发了加载）。
    #[cfg(test)]
    pub(crate) fn ready_count(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| matches!(slot, Slot::Ready { .. }))
            .count()
    }

    /// 解析 `url` 并返回当前状态；未命中时启动一次后台加载。
    ///
    /// `doc_dir` 是当前文档所在目录：相对路径以它为基准（`None` 时退回
    /// 进程工作目录）。
    pub(crate) fn slot(
        &mut self,
        url: &str,
        doc_dir: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> SlotState {
        let Some(source) = resolve(url, doc_dir) else {
            return SlotState::Unsupported;
        };
        let key = source.cache_key();
        match source {
            ImageSource::Local(path) => self.local_slot(key, path, cx),
            ImageSource::Remote(url) => self.remote_slot(key, url, cx),
        }
    }

    fn local_slot(&mut self, key: String, path: PathBuf, cx: &mut Context<Self>) -> SlotState {
        let stamp = local_stamp(&path);
        match self.slots.get(&key) {
            // 命中且文件未变：直接用。
            Some(Slot::Ready {
                image,
                size,
                stamp: cached,
            }) if *cached == stamp => return SlotState::Ready(image.clone(), *size),
            // 加载中：继续占位。
            Some(Slot::Loading) => return SlotState::Loading,
            // 已失败且文件指纹未变：不再重试（避免每帧 stat 风暴）。
            Some(Slot::Failed { stamp: cached, .. }) if *cached == stamp => {
                return SlotState::Failed;
            }
            _ => {}
        }
        // 文件不存在：记为失败；文件在之后出现时指纹变化会自动重试。
        if stamp.is_none() {
            log::debug!("Markdown 图片不存在：{}", path.display());
            self.slots.insert(
                key,
                Slot::Failed {
                    stamp,
                    at: Instant::now(),
                },
            );
            return SlotState::Failed;
        }
        self.slots.insert(key.clone(), Slot::Loading);
        self.spawn_load(key, ImageSource::Local(path), stamp, cx);
        SlotState::Loading
    }

    fn remote_slot(&mut self, key: String, url: String, cx: &mut Context<Self>) -> SlotState {
        match self.slots.get(&key) {
            Some(Slot::Ready { image, size, .. }) => {
                return SlotState::Ready(image.clone(), *size);
            }
            Some(Slot::Loading) => return SlotState::Loading,
            // 失败后进入冷却：网络断开时不会每帧发起请求。
            Some(Slot::Failed { at, .. }) if at.elapsed() < REMOTE_RETRY => {
                return SlotState::Failed;
            }
            _ => {}
        }
        self.slots.insert(key.clone(), Slot::Loading);
        self.spawn_load(key, ImageSource::Remote(url), None, cx);
        SlotState::Loading
    }

    /// 后台读盘/下载 + 解码，完成后写回缓存并 `notify`。
    fn spawn_load(
        &mut self,
        key: String,
        source: ImageSource,
        stamp: Stamp,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { load(&source) })
                .await;
            let key_for_write = key.clone();
            if let Some(this) = this.upgrade() {
                this.update(cx, |cache, cx| {
                    // 期间可能 set_text 过（切文档 / 外部重载），缓存已被清空。
                    // 只在该键仍是本次期望的 Loading 时写回，避免上一份文档的
                    // 结果污染新文档、或旧请求覆盖新请求。
                    if matches!(cache.slots.get(&key_for_write), Some(Slot::Loading)) {
                        let slot = match loaded {
                            Some((image, size)) => Slot::Ready { image, size, stamp },
                            None => Slot::Failed {
                                stamp,
                                at: Instant::now(),
                            },
                        };
                        cache.slots.insert(key_for_write, slot);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }
}

/// 把 Markdown 图片目标解析成可加载的源；`None` 表示只能渲染占位卡片。
///
/// 支持：`http(s)://`、协议相对 `//host/x.png`、`file://` URL、`~`、
/// 百分号转义、绝对/相对本地路径（相对 `doc_dir`）、`..`（做词法归一化，
/// 不越出根目录）。拒绝：未知 scheme（`data:`/`javascript:`/`ftp:` ……）、
/// 空地址、非图片扩展名、超长地址。
fn resolve(url: &str, doc_dir: Option<&Path>) -> Option<ImageSource> {
    let raw = strip_angle(url.trim());
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    // 远程：http(s) 与协议相对地址（Markdown 里常见 `//cdn.example.com/a.png`）。
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(ImageSource::Remote(raw.to_string()));
    }
    if lower.starts_with("//") {
        return Some(ImageSource::Remote(format!("https:{raw}")));
    }
    let path_text = if lower.starts_with("file:") {
        file_url_to_path(&raw["file:".len()..])?
    } else if lower.starts_with("data:") || lower.starts_with("javascript:") {
        // 不处理内联数据与脚本协议：既不解码也不当文件名。
        return None;
    } else if lower.contains("://") {
        // 其它协议（ftp://、vscode:// ……）不加载：不是图片传输。
        return None;
    } else {
        // 注意：这里不再把 `foo:bar.png` 当成未知 scheme 拒掉。Unix 上
        // 文件名合法地包含冒号（macOS 截图就有），而除 file/http 外的
        // scheme 即使当成本地路径也只是读不到文件（渲染为占位）。
        percent_decode(raw)
    };
    let path = PathBuf::from(expand_tilde(&path_text));
    let base = match doc_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    let joined = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    let normalized = normalize_path(&joined);
    if !is_supported_extension(&normalized) {
        return None;
    }
    Some(ImageSource::Local(normalized))
}

/// `![](<assets/a b.png>)` 的尖括号写法。
fn strip_angle(url: &str) -> &str {
    url.strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(url)
}

/// 是否为可解码的图片扩展名（口径见
/// `image_viewer::IMAGE_EXTENSIONS`）。
pub(crate) fn is_supported_extension(path: &Path) -> bool {
    crate::editor::image_viewer::is_image_file(path)
}

/// `file://` 的剩余部分 → 本地路径字符串。
///
/// - `file:///home/u/a.png` → `/home/u/a.png`
/// - `file://localhost/home/u/a.png` → `/home/u/a.png`
/// - `file://server/share/a.png` → `\\server\share\a.png`（UNC）
/// - `file:///C:/Pictures/a.png` → `C:/Pictures/a.png`（Windows 盘符）
fn file_url_to_path(rest: &str) -> Option<String> {
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.strip_prefix("//") {
        Some(after) => match after.find('/') {
            Some(i) => (&after[..i], &after[i..]),
            None => (after, ""),
        },
        None => ("", rest),
    };
    if path.is_empty() {
        return None;
    }
    let decoded = percent_decode(path);
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return Some(format!("\\\\{authority}{}", decoded.replace('/', "\\")));
    }
    #[cfg(windows)]
    {
        if let Some(drive_rest) = decoded.strip_prefix('/') {
            let b = drive_rest.as_bytes();
            if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                return Some(drive_rest.to_string());
            }
        }
    }
    Some(decoded)
}

/// `%20` 之类的转义解码；非法转义原样保留（不破坏字面 `%` 文件名）。
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 开头的 `~` 展开成用户主目录。
fn expand_tilde(path: &str) -> String {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => rest,
        _ => return path.to_string(),
    };
    match dirs::home_dir() {
        Some(home) => format!("{}{rest}", home.display()),
        None => path.to_string(),
    }
}

/// 词法归一化：消掉 `.` / `..`，不碰文件系统（路径可能还不存在）。
///
/// `..` 只回退到上一个普通组件为止，已经到根（绝对路径）就忽略，因此
/// 归一化结果永远落在根目录以内。
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// 同步加载：本地读盘或远程下载，然后解码。
fn load(source: &ImageSource) -> Option<(Arc<Image>, (u32, u32))> {
    match source {
        ImageSource::Local(path) => load_local(path),
        ImageSource::Remote(url) => load_remote(url),
    }
}

/// 本地图片：大小/扩展名门禁 → 读盘 → 解码（含解码炸弹防护）。
fn load_local(path: &Path) -> Option<(Arc<Image>, (u32, u32))> {
    if !is_supported_extension(path) {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    match load_image_from_bytes(&bytes, path) {
        Ok(loaded) => Some(loaded),
        Err(e) => {
            log::debug!("Markdown 本地图片解码失败 {}: {e}", path.display());
            None
        }
    }
}

/// 远程图片：带超时/大小上限的 GET，然后按字节嗅探格式解码。
fn load_remote(url: &str) -> Option<(Arc<Image>, (u32, u32))> {
    let bytes = fetch_remote(url)?;
    // 解码器按扩展名判断 SVG，其余格式靠字节嗅探；远程 URL 可能没有扩展名。
    let decode_path = remote_decode_path(url);
    match load_image_from_bytes(&bytes, &decode_path) {
        Ok(loaded) => Some(loaded),
        Err(e) => {
            log::debug!("Markdown 远程图片解码失败 {url}: {e}");
            None
        }
    }
}

fn fetch_remote(url: &str) -> Option<Vec<u8>> {
    let result = ureq::get(url)
        .config()
        .timeout_global(Some(REMOTE_TIMEOUT))
        .max_redirects(5)
        .build()
        .call();
    let mut response = match result {
        Ok(response) => response,
        Err(e) => {
            log::warn!("Markdown 远程图片请求失败 {url}: {e}");
            return None;
        }
    };
    if !response.status().is_success() {
        log::warn!(
            "Markdown 远程图片请求失败 {url}: HTTP {}",
            response.status()
        );
        return None;
    }
    if let Some(len) = response.body().content_length()
        && len > MAX_IMAGE_BYTES
    {
        log::warn!("Markdown 远程图片过大（{len} bytes），已跳过 {url}");
        return None;
    }
    let mut bytes = Vec::new();
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_IMAGE_BYTES)
        .reader();
    match reader.read_to_end(&mut bytes) {
        Ok(_) if !bytes.is_empty() => Some(bytes),
        Ok(_) => None,
        Err(e) => {
            log::warn!("Markdown 远程图片读取失败 {url}: {e}");
            None
        }
    }
}

/// 远程图片的“解码路径”：只用来告诉解码器是否按 SVG 处理。
fn remote_decode_path(url: &str) -> PathBuf {
    let path_part = url.split(['?', '#']).next().unwrap_or(url);
    // 只有 SVG 依赖扩展名（`load_image_from_bytes` 按扩展名走 usvg）；
    // 其余格式都由字节嗅探决定，扩展名随便给一个即可。
    let is_svg = Path::new(path_part)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    if is_svg {
        PathBuf::from("remote.svg")
    } else {
        PathBuf::from("remote.img")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\notes")
        } else {
            PathBuf::from("/notes")
        }
    }

    /// 干净的临时目录（同名残留先删掉，避免上一轮运行的文件干扰断言）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deepterm-md-image-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn local_of(url: &str) -> PathBuf {
        match resolve(url, Some(&doc_dir())) {
            Some(ImageSource::Local(p)) => p,
            other => panic!("expected local source for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn relative_paths_resolve_against_document_dir() {
        // 这是本次修复的核心：Typora 粘贴/相对引用按文档目录解析，
        // 而不是进程工作目录。
        assert_eq!(
            local_of("assets/icon.png"),
            normalize_path(&doc_dir().join("assets/icon.png"))
        );
        assert_eq!(
            local_of("./assets/icon.png"),
            normalize_path(&doc_dir().join("assets/icon.png"))
        );
        assert_eq!(
            local_of("notes.assets/image-1.png"),
            normalize_path(&doc_dir().join("notes.assets/image-1.png"))
        );
    }

    #[test]
    fn parent_dir_references_are_allowed_and_normalized() {
        assert_eq!(
            local_of("../images/a.png"),
            normalize_path(&doc_dir().join("../images/a.png"))
        );
        // 归一化后不应残留 `..`
        assert!(
            !local_of("../images/a.png")
                .components()
                .any(|c| c == Component::ParentDir)
        );
    }

    #[test]
    fn absolute_paths_are_kept() {
        #[cfg(not(windows))]
        {
            assert_eq!(local_of("/tmp/a.png"), PathBuf::from("/tmp/a.png"));
            assert_eq!(
                resolve("/tmp/../tmp/a.png", None),
                Some(ImageSource::Local(PathBuf::from("/tmp/a.png")))
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                local_of("C:\\pics\\a.png"),
                PathBuf::from("C:\\pics\\a.png")
            );
            // 盘符不是 scheme
            assert!(matches!(
                resolve("D:/pics/a.png", None),
                Some(ImageSource::Local(_))
            ));
        }
    }

    #[test]
    fn file_urls_become_paths() {
        #[cfg(not(windows))]
        {
            assert_eq!(local_of("file:///tmp/a.png"), PathBuf::from("/tmp/a.png"));
            assert_eq!(
                local_of("file://localhost/tmp/a.png"),
                PathBuf::from("/tmp/a.png")
            );
            assert_eq!(local_of("file:a.png"), doc_dir().join("a.png"));
        }
        #[cfg(windows)]
        {
            assert_eq!(
                local_of("file:///C:/pics/a.png"),
                PathBuf::from("C:/pics/a.png")
            );
            assert_eq!(
                local_of("file://server/share/a.png"),
                PathBuf::from("\\\\server\\share\\a.png")
            );
        }
    }

    #[test]
    fn percent_encoding_and_tilde_expand() {
        assert_eq!(local_of("my%20pic.png"), doc_dir().join("my pic.png"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(local_of("~/a.png"), normalize_path(&home.join("a.png")));
        }
    }

    #[test]
    fn remote_urls_are_recognized() {
        for url in [
            "https://x.dev/a.png",
            "http://x.dev/a.jpg",
            "HTTPS://x.dev/a.png?raw=1",
        ] {
            assert!(
                matches!(resolve(url, None), Some(ImageSource::Remote(_))),
                "{url}"
            );
        }
        assert_eq!(
            resolve("//cdn.dev/a.png", None),
            Some(ImageSource::Remote("https://cdn.dev/a.png".into()))
        );
    }

    #[test]
    fn unsupported_sources_are_rejected() {
        for url in [
            "",
            "   ",
            "javascript:alert(1)",
            "data:image/png;base64,AAAA",
            "ftp://x.dev/a.png",
            "vscode://file/etc/passwd.png",
            "notes.md",
            "id_rsa",
            "<>",
        ] {
            assert_eq!(resolve(url, None), None, "{url:?}");
        }
    }

    #[test]
    fn colon_in_local_filename_is_not_a_scheme() {
        assert_eq!(local_of("shot:1.png"), doc_dir().join("shot:1.png"));
    }

    #[test]
    fn angle_brackets_are_stripped() {
        assert_eq!(local_of("<assets/a.png>"), doc_dir().join("assets/a.png"));
    }

    #[test]
    fn cache_keys_use_resolved_identity() {
        let a = resolve("assets/a.png", Some(Path::new("/notes"))).unwrap();
        let b = resolve("./assets/a.png", Some(Path::new("/notes"))).unwrap();
        assert_eq!(a.cache_key(), b.cache_key());
        let c = resolve("assets/a.png", Some(Path::new("/other"))).unwrap();
        assert_ne!(a.cache_key(), c.cache_key());
    }

    #[test]
    fn remote_decode_path_keeps_svg() {
        assert_eq!(
            remote_decode_path("https://x.dev/a.svg"),
            PathBuf::from("remote.svg")
        );
        assert_eq!(
            remote_decode_path("https://x.dev/a.svg?v=2"),
            PathBuf::from("remote.svg")
        );
        assert_eq!(
            remote_decode_path("https://x.dev/a"),
            PathBuf::from("remote.img")
        );
    }

    #[test]
    fn local_loader_reads_a_real_image() {
        let dir = temp_dir("load-test");
        let file = dir.join("one.png");
        std::fs::write(&file, PNG).unwrap();
        let loaded = load_local(&file).expect("png should decode");
        assert_eq!(loaded.1, (1, 1));
        // 扩展名门禁：非图片文件不读
        let bad = dir.join("secret.txt");
        std::fs::write(&bad, b"secret").unwrap();
        assert!(load_local(&bad).is_none());
        std::fs::remove_file(&file).ok();
        std::fs::remove_file(&bad).ok();
    }

    /// 本机回环 HTTP 服务：验证远程 URL 真的会走网络取字节并解码，
    /// 不依赖外网（CI / 断网环境也能跑）。
    #[test]
    fn remote_loader_fetches_and_decodes() {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                PNG.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(PNG);
        });

        let url = format!("http://{addr}/a.png");
        let loaded = load_remote(&url).expect("remote png should decode");
        assert_eq!(loaded.1, (1, 1));
        server.join().unwrap();

        // 404 应该安静地变成 None（渲染为占位卡片）。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        });
        assert!(load_remote(&format!("http://{addr}/missing.png")).is_none());
    }

    /// 视图侧状态机：首帧 Loading → 后台加载完成 → Ready；文件缺失直接
    /// Failed，`set_text` 对应的 `clear` 后重新可加载。
    #[gpui::test]
    fn cache_slot_transitions_and_reuses(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        let dir = temp_dir("cache-test");
        std::fs::write(dir.join("one.png"), PNG).unwrap();

        let cache = cx.new(|_| ImageCache::new());
        let first = cache.update(cx, |cache, cx| cache.slot("one.png", Some(&dir), cx));
        assert!(
            matches!(first, SlotState::Loading),
            "first lookup must kick off a background load"
        );
        cx.run_until_parked();
        let second = cache.update(cx, |cache, cx| cache.slot("one.png", Some(&dir), cx));
        assert!(
            matches!(second, SlotState::Ready(_, (1, 1))),
            "loaded image must come from the cache"
        );

        // 不存在的文件：直接 Failed（不反复读盘）。
        let missing = cache.update(cx, |cache, cx| cache.slot("missing.png", Some(&dir), cx));
        assert!(matches!(missing, SlotState::Failed));

        // 非图片源：Unsupported，不占用缓存。
        let unsupported = cache.update(cx, |cache, cx| {
            cache.slot("data:image/png;base64,AA", Some(&dir), cx)
        });
        assert!(matches!(unsupported, SlotState::Unsupported));

        // clear 后重新进入 Loading（切文档/外部重载路径）。
        cache.update(cx, |cache, cx| cache.clear(cx));
        let reload = cache.update(cx, |cache, cx| cache.slot("one.png", Some(&dir), cx));
        assert!(matches!(reload, SlotState::Loading));
        cx.run_until_parked();
    }

    /// 1x1 PNG（本地与远程用例共用）。
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// 端到端：预览（`Markdown`）在真实窗口里渲染 `![](icon.png)`，
    /// 相对路径必须以 `doc_dir` 为基准并最终进入 Ready。
    #[gpui::test]
    fn preview_render_loads_relative_image(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, Entity, IntoElement, Render, Window};

        struct Host(Entity<crate::editor::markdown::Markdown>);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 与 `markdown-showcase.md` 里完全一致的引用：文档在仓库根，图片在
        // `assets/` 子目录（此前相对路径按进程工作目录解析，只有恰好在仓库
        // 根目录启动时才碰巧能显示）。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| {
                crate::editor::markdown::Markdown::new_with_dir(
                    "![DeepTerm Logo](assets/icon.png)\n\n",
                    Some(root.to_path_buf()),
                    cx,
                )
            }))
        });
        let markdown = host.read_with(cx, |host, _| host.0.clone());
        let cache = markdown.read_with(cx, |markdown, _| markdown.image_cache.clone());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            cache.read_with(cx, |cache, _| cache.ready_count()),
            1,
            "preview must load the image via the doc-dir-relative path"
        );
    }

    /// 端到端：实时编辑（`#live`）用 `doc_path` 的**父目录**解析相对图片。
    #[gpui::test]
    fn live_view_loads_relative_image_from_doc_path(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, Entity, IntoElement, Render, Window};

        struct Host(Entity<crate::editor::markdown_live::LiveMarkdownView>);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        let doc = dir.join("demo.md");
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| {
                crate::editor::markdown_live::LiveMarkdownView::with_text("![logo](icon.png)\n", cx)
                    .doc_path(&doc)
            }))
        });
        let view = host.read_with(cx, |host, _| host.0.clone());
        let cache = view.read_with(cx, |view, _| view.image_cache_entity());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            cache.read_with(cx, |cache, _| cache.ready_count()),
            1,
            "live view must resolve the image against the document's parent dir"
        );
    }
}

//! Markdown 图片：URL 解析 → 加载（本地文件 / http(s) 远程）→ 视图侧缓存。
//!
//! hover 浮层（[`super::markdown::Markdown`]）与实时编辑
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
/// 用一个独立实体（而不是视图内的 `HashMap`）的原因：hover 浮层的渲染入口
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
    /// `ctx` 决定相对路径去哪找（见 [`ImageContext`]）。
    pub(crate) fn slot(
        &mut self,
        url: &str,
        ctx: &ImageContext,
        cx: &mut Context<Self>,
    ) -> SlotState {
        let Some(source) = resolve(url, ctx) else {
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

/// 相对图片引用的查找上下文：基准目录按**顺序**依次尝试。
///
/// 这个结构回答「`![](a.png)` 到底该去哪找」。顺序与取舍对照 VS Code：
///
/// | 来源 | VS Code（`markdown-language-features`） | 这里 |
/// |---|---|---|
/// | `http(s):` / `file:` / 绝对路径 | 直接 `asWebviewUri` | 直接加载 |
/// | 文档在某个工作区文件夹里 | **先**拼工作区根，不用文档目录 | 文档目录**优先**，再拼项目根 |
/// | 文档不在工作区 / 没有工作区 | 拼**文档所在目录**（`dirname`） | 同左 |
///
/// VS Code 只用「工作区根」一档，是因为它的预览按工作区建模（`localResourceRoots`
/// 就是文档 + 各工作区根）。deepterm 是 Typora 式笔记工具：同级相对引用
/// （`note.assets/a.png`）必须优先命中文档目录，否则同一个文件名会串到仓库
/// 里另一个 `assets/`。把项目根放在**后面兜底**，同时拿到 VS Code 那档能力
/// ——`docs/foo.md` 引用仓库根的 `assets/icon.png` 也能显示。
#[derive(Debug, Clone, Default)]
pub(crate) struct ImageContext {
    /// 文档所在目录（**目录**，不是文件路径）；相对引用的第一基准。
    pub(crate) doc_dir: Option<PathBuf>,
    /// 项目/工作区根，按由近到远排列；相对引用的兜底基准。
    pub(crate) roots: Vec<PathBuf>,
}

impl ImageContext {
    /// 只按文档目录解析（hover 浮层等没有项目概念的场景）。
    pub(crate) fn from_doc_dir(doc_dir: Option<PathBuf>) -> Self {
        Self {
            doc_dir,
            roots: Vec::new(),
        }
    }

    /// 相对引用的候选基准，按尝试顺序排列。
    fn bases(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::with_capacity(self.roots.len() + 2);
        if let Some(dir) = &self.doc_dir {
            out.push(dir.clone());
        }
        for root in &self.roots {
            if !out.contains(root) {
                out.push(root.clone());
            }
        }
        // 都没有（无路径的临时文档 / hover）：退回进程工作目录，够用即可。
        if out.is_empty()
            && let Ok(cwd) = std::env::current_dir()
        {
            out.push(cwd);
        }
        out
    }
}

/// 把 Markdown 图片目标解析成可加载的源；`None` 表示只能渲染占位卡片。
///
/// 支持：`http(s)://`、协议相对 `//host/x.png`、`file://` URL、`~`、
/// 百分号转义、`?query` / `#fragment`（剥离，不参与本地路径）、绝对路径、
/// 相对路径（按 [`ImageContext`] 依次尝试文档目录 → 项目根 → 工作目录）、
/// `..`（做词法归一化，不越出根目录）。拒绝：未知 scheme
/// （`data:`/`javascript:`/`ftp:` ……）、空地址、非图片扩展名、超长地址。
fn resolve(url: &str, ctx: &ImageContext) -> Option<ImageSource> {
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
        strip_url_suffix(raw)
    };
    // `~` 先展开，否则 `is_absolute()` 判不出来；解码也从展开后的文本算，
    // 否则字面量候选会把 `~` 原样留下（变成相对路径，基准全错）。
    let expanded = expand_tilde(&path_text);
    let path = PathBuf::from(&expanded);
    // 相对引用：按基准逐个探，每个基准里两种读法都试：
    //
    // 1. **解码后**（URI 语义，VS Code 的口径）：`my%20pic.png` → `my pic.png`；
    // 2. **字面量**（Zed/Typora 的口径）：磁盘上真的有个文件叫 `my%20pic.png`。
    //
    // 顺序是「解码命中 > 字面命中 > 解码兜底」：解码在前贴合 VS Code 与
    // Markdown 的 URI 语义；字面量兜住前者不存在的情况，避免把合法文件名
    // 判死。两个都不存在时留给解码结果做占位路径——文件之后被创建时，
    // `ImageCache` 靠文件指纹变化自动重试，而不是永远 `Unsupported`。
    let decoded = PathBuf::from(percent_decode(&expanded));
    let candidates = if decoded == path {
        vec![path]
    } else {
        vec![decoded, path]
    };
    let bases: Vec<PathBuf> = if path_is_absolute(&candidates) {
        vec![PathBuf::new()]
    } else {
        ctx.bases()
    };
    let mut fallback: Option<PathBuf> = None;
    for base in bases {
        for candidate in &candidates {
            let joined = if candidate.is_absolute() {
                candidate.clone()
            } else {
                base.join(candidate)
            };
            let normalized = normalize_path(&joined);
            if !crate::editor::image_viewer::is_image_file(&normalized) {
                continue;
            }
            if normalized.is_file() {
                return Some(ImageSource::Local(normalized));
            }
            fallback.get_or_insert(normalized);
        }
    }
    fallback.map(ImageSource::Local)
}

/// 任一候选是绝对路径（`~` 已展开）就没有基准可换。
fn path_is_absolute(candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|c| c.is_absolute())
}

/// 剥掉 `?query` / `#fragment`：它们是 URI 的一部分，不是文件名的一部分。
///
/// VS Code 用 `Uri.parse("markdown-link:" + e)` 解析，查询串与锚点天然不进入
/// 路径。deepterm 之前把整串当文件名，于是 `a.png?raw=1`（GitHub raw 链接、
/// 各种图床的常用写法）被判成「非图片扩展名」直接放弃，卡片只显示占位。
///
/// `#` 在最前面时视为本地文件名的一部分（极少见，但也别把整段吃掉）。
fn strip_url_suffix(raw: &str) -> String {
    match raw.find(['?', '#']) {
        Some(0) | None => raw.to_string(),
        Some(i) => raw[..i].to_string(),
    }
}

/// `![](<assets/a b.png>)` 的尖括号写法。
fn strip_angle(url: &str) -> &str {
    url.strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(url)
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
    if !crate::editor::image_viewer::is_image_file(path) {
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
pub(crate) mod tests {
    use super::*;

    fn doc_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\notes")
        } else {
            PathBuf::from("/notes")
        }
    }

    /// 干净的临时目录（同名残留先删掉，避免上一轮运行的文件干扰断言）。
    /// 每个测试独占的临时目录。
    ///
    /// 键里带 tag **和**进程内唯一序号：`#[gpui::test]` 默认并发跑，共用
    /// 同名目录时一个测试的 `remove_dir_all` 会把另一个测试刚写好的图片
    /// 删掉——`resolve` 会真的 `is_file()` 探测，于是表现为随机失败。
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "deepterm-md-image-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_of(dir: &Path) -> ImageContext {
        ImageContext::from_doc_dir(Some(dir.to_path_buf()))
    }

    fn local_of(url: &str) -> PathBuf {
        match resolve(url, &ctx_of(&doc_dir())) {
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
                resolve("/tmp/../tmp/a.png", &ImageContext::default()),
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
                resolve("D:/pics/a.png", &ImageContext::default()),
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
                matches!(
                    resolve(url, &ImageContext::default()),
                    Some(ImageSource::Remote(_))
                ),
                "{url}"
            );
        }
        assert_eq!(
            resolve("//cdn.dev/a.png", &ImageContext::default()),
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
            assert_eq!(resolve(url, &ImageContext::default()), None, "{url:?}");
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

    /// `?query` / `#fragment` 是 URI 的一部分，不该被当成文件名。
    ///
    /// 复现的现场：`a.png?raw=1`（GitHub raw、图床常用写法）此前整串进入
    /// 扩展名判断，`png?raw=1` 不在白名单里 → `Unsupported` → 卡片永远
    /// 只显示占位，永远不会读盘。
    #[test]
    fn url_query_and_fragment_are_not_part_of_the_filename() {
        assert_eq!(
            local_of("assets/a.png?raw=1"),
            doc_dir().join("assets/a.png")
        );
        assert_eq!(local_of("assets/a.png#pic"), doc_dir().join("assets/a.png"));
        assert_eq!(
            local_of("assets/a.png?x=1#pic"),
            doc_dir().join("assets/a.png")
        );
        // 真正以 `#` 开头的字面文件名不能被整段吃掉。
        assert_eq!(strip_url_suffix("#a.png"), "#a.png");
    }

    /// 文档目录找不到时，按 `roots` 依次兜底——VS Code 的「工作区根」那档。
    ///
    /// 真实场景：`docs/foo.md` 引用仓库根的 `assets/icon.png`。VS Code 的
    /// 预览会拼 `getWorkspaceFolder(doc)`，所以能显示；deepterm 以前只拼
    /// 文档目录，于是同一个文件在 VS Code 里有图、在这里只有占位。
    #[test]
    fn project_root_is_used_when_doc_dir_misses() {
        let repo = temp_dir("root-fallback");
        let docs = repo.join("docs");
        let assets = repo.join("assets");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(docs.join("local.png"), PNG).unwrap();
        std::fs::write(assets.join("root.png"), PNG).unwrap();

        let ctx = ImageContext {
            doc_dir: Some(docs.clone()),
            roots: vec![repo.clone()],
        };

        // 文档目录命中：优先用它，不回退。
        assert_eq!(
            resolve("local.png", &ctx).and_then(local_path),
            Some(docs.join("local.png"))
        );
        // 文档目录没有 → 回退项目根。
        assert_eq!(
            resolve("assets/root.png", &ctx).and_then(local_path),
            Some(assets.join("root.png"))
        );
        // 文档目录里也有同名 `assets/root.png` 时，文档目录必须赢。
        std::fs::create_dir_all(docs.join("assets")).unwrap();
        std::fs::write(docs.join("assets").join("root.png"), PNG).unwrap();
        assert_eq!(
            resolve("assets/root.png", &ctx).and_then(local_path),
            Some(docs.join("assets").join("root.png"))
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    fn local_path(source: ImageSource) -> Option<PathBuf> {
        match source {
            ImageSource::Local(p) => Some(p),
            ImageSource::Remote(_) => None,
        }
    }

    /// 磁盘上真的叫 `my%20pic.png` 时按字面量命中；写 `my%20pic.png` 引用
    /// 空格文件名时按解码命中。两种写法都要能用。
    #[test]
    fn percent_encoding_tries_literal_then_decoded() {
        let dir = temp_dir("percent");
        std::fs::write(dir.join("my pic.png"), PNG).unwrap();
        std::fs::write(dir.join("literal%20name.png"), PNG).unwrap();
        let ctx = ImageContext::from_doc_dir(Some(dir.clone()));

        assert_eq!(
            resolve("my%20pic.png", &ctx).and_then(local_path),
            Some(dir.join("my pic.png"))
        );
        assert_eq!(
            resolve("literal%20name.png", &ctx).and_then(local_path),
            Some(dir.join("literal%20name.png"))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 端到端复现用户现场：仓库根 `README.md` 引用**同目录**图片，
    /// `docs/` 下的笔记引用**仓库根** `assets/`。
    ///
    /// 两者都必须在真实文件系统上解析成功——只看 `resolve` 的返回值不够，
    /// 因为「路径拼对了」和「这个路径真有文件」是两件事，而用户看到的
    /// 是后者。
    #[test]
    fn repo_relative_references_hit_real_files() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = ImageContext {
            doc_dir: Some(repo.to_path_buf()),
            roots: vec![crate::lsp::project::project_root(repo)],
        };
        // 仓库根文档 → 同目录 assets/
        assert_eq!(
            resolve("assets/icon.png", &ctx).and_then(local_path),
            Some(repo.join("assets").join("icon.png")),
            "同目录相对引用必须命中仓库里的 assets/icon.png"
        );

        // 真实文档：docs/markdown-showcase.md 引用仓库根 assets/icon.png
        let showcase = repo.join("docs").join("markdown-showcase.md");
        assert!(showcase.is_file(), "夹具文档应存在");
        let showcase_ctx = ImageContext {
            doc_dir: showcase.parent().map(std::path::Path::to_path_buf),
            roots: vec![crate::lsp::project::project_root(&showcase)],
        };
        assert_eq!(
            resolve("assets/icon.png", &showcase_ctx).and_then(local_path),
            Some(repo.join("assets").join("icon.png")),
            "markdown-showcase.md 里的 ![](assets/icon.png) 必须能加载"
        );

        // docs/ 下的文档 → 仓库根 assets/（文档目录里没有 assets/）
        let docs_ctx = ImageContext {
            doc_dir: Some(repo.join("docs")),
            roots: vec![crate::lsp::project::project_root(&repo.join("docs"))],
        };
        assert_eq!(
            resolve("assets/icon.png", &docs_ctx).and_then(local_path),
            Some(repo.join("assets").join("icon.png")),
            "docs/ 下的文档应回退到项目根的 assets/"
        );
    }

    #[test]
    fn cache_keys_use_resolved_identity() {
        let a = resolve("assets/a.png", &ctx_of(Path::new("/notes"))).unwrap();
        let b = resolve("./assets/a.png", &ctx_of(Path::new("/notes"))).unwrap();
        assert_eq!(a.cache_key(), b.cache_key());
        let c = resolve("assets/a.png", &ctx_of(Path::new("/other"))).unwrap();
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
        let first = cache.update(cx, |cache, cx| cache.slot("one.png", &ctx_of(&dir), cx));
        assert!(
            matches!(first, SlotState::Loading),
            "first lookup must kick off a background load"
        );
        cx.run_until_parked();
        let second = cache.update(cx, |cache, cx| cache.slot("one.png", &ctx_of(&dir), cx));
        assert!(
            matches!(second, SlotState::Ready(_, (1, 1))),
            "loaded image must come from the cache"
        );

        // 不存在的文件：直接 Failed（不反复读盘）。
        let missing = cache.update(cx, |cache, cx| cache.slot("missing.png", &ctx_of(&dir), cx));
        assert!(matches!(missing, SlotState::Failed));

        // 非图片源：Unsupported，不占用缓存。
        let unsupported = cache.update(cx, |cache, cx| {
            cache.slot("data:image/png;base64,AA", &ctx_of(&dir), cx)
        });
        assert!(matches!(unsupported, SlotState::Unsupported));

        // clear 后重新进入 Loading（切文档/外部重载路径）。
        cache.update(cx, |cache, cx| cache.clear(cx));
        let reload = cache.update(cx, |cache, cx| cache.slot("one.png", &ctx_of(&dir), cx));
        assert!(matches!(reload, SlotState::Loading));
        cx.run_until_parked();
    }

    /// 1x1 PNG（本地与远程用例共用）。
    pub(crate) const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// `Markdown` 实体（hover 浮层用的渲染器）仍按文档目录解析相对图片。
    ///
    /// 只读预览 tab 已删除，但 `Markdown` 实体还在给 hover 浮层渲染 md；
    /// 它的图片解析口径不能因为删预览而失修。
    #[gpui::test]
    fn markdown_entity_resolves_relative_image(cx: &mut gpui::TestAppContext) {
        use gpui::prelude::*;
        use gpui::{Entity, IntoElement, Render, Window};

        struct Host(Entity<crate::editor::markdown::Markdown>);
        impl Render for Host {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                // hover 浮层的真实入口：内联渲染，不套整页外壳。
                let style = crate::editor::markdown::MarkdownStyle::hover_style(
                    window,
                    cx,
                    "Menlo".to_string(),
                );
                let items = crate::editor::markdown::Markdown::render_items_inline(
                    self.0.clone(),
                    style,
                    cx,
                );
                gpui::div().flex().flex_col().children(items)
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
            "Markdown entity must load the image via the doc-dir-relative path"
        );
    }

    /// 端到端：同目录下各种常见写法都必须真的加载出图片。
    #[gpui::test]
    fn live_view_loads_common_forms(cx: &mut gpui::TestAppContext) {
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
        let dir = temp_dir("live-forms");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for (d, n) in [(&dir, "a.png"), (&sub, "b.png")] {
            std::fs::write(d.join(n), PNG).unwrap();
        }
        let doc = dir.join("note.md");
        let text = [
            "![one](a.png)",
            "![two](./a.png)",
            "![three](sub/b.png)",
            "![four](/absolute.png)",
            "![five](a.png?raw=1)",
            "![six](a.png \"title\")",
            "![seven](<a.png>)",
        ]
        .join("\n\n");
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| {
                crate::editor::markdown_live::LiveMarkdownView::with_text(&text, cx).doc_path(&doc)
            }))
        });
        let view = host.read_with(cx, |host, _| host.0.clone());
        let cache = view.read_with(cx, |view, _| view.image_cache_entity());
        cx.refresh().unwrap();
        cx.run_until_parked();
        // 缓存以**解析后的绝对路径**为键：`a.png` 的几种写法共用一个槽位，
        // 所以这里断言的是「几个不同文件」，不是「几次请求」。
        let ready = cache.read_with(cx, |cache, _| cache.ready_count());
        println!("READY = {ready} (expect 2: a.png + sub/b.png)");
        assert_eq!(
            ready, 2,
            "same-dir variants must all resolve to the two real files"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 端到端矩阵：真实世界里会遇到的路径形态与目录布局。
    ///
    /// 重点是**文档不在仓库根**的情形（`docs/note.md` 引用仓库根 `assets/`）：
    /// 这正是 VS Code 靠工作区根能显示、而只拼文档目录会失败的那一档。
    #[gpui::test]
    fn live_view_load_matrix(cx: &mut gpui::TestAppContext) {
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
        let repo = temp_dir("matrix");
        let docs = repo.join("docs");
        let assets = repo.join("assets");
        let spaced = docs.join("my notes");
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::create_dir_all(&assets).unwrap();
        for (d, n) in [
            (&spaced, "pic.png"),
            (&spaced, "大写.PNG"),
            (&spaced, "shot:1.png"),
            (&assets, "logo.png"),
        ] {
            std::fs::write(d.join(n), PNG).unwrap();
        }
        let doc = spaced.join("note.md");

        // (标签, 引用行, 期望 ready 数, 是否给项目根兜底)
        let cases: Vec<(&str, &str, usize, bool)> = vec![
            ("same dir", "![a](pic.png)", 1, false),
            ("dot slash", "![a](./pic.png)", 1, false),
            ("uppercase ext", "![a](大写.PNG)", 1, false),
            ("colon name", "![a](shot:1.png)", 1, false),
            ("query", "![a](pic.png?v=1)", 1, false),
            ("angle", "![a](<pic.png>)", 1, false),
            ("parent dir", "![a](../my%20notes/pic.png)", 1, false),
            // 徽章式一行多图：两张都要出图。
            (
                "two badges one line",
                "![a](pic.png) ![b](pic.png)",
                1,
                false,
            ),
            ("repo root", "![a](assets/logo.png)", 1, true),
            // 没有 roots 时仓库根那份够不到——这是修复前的行为，用来对照。
            ("repo root (no roots)", "![a](assets/logo.png)", 0, false),
        ];
        for (label, ref_line, want, with_roots) in cases {
            let (host, cx2) = cx.add_window_view(|_, cx| {
                Host(cx.new(|cx| {
                    let text = format!("{ref_line}\n");
                    let view = crate::editor::markdown_live::LiveMarkdownView::with_text(&text, cx)
                        .doc_path(&doc);
                    if with_roots {
                        view.image_roots(vec![repo.clone()])
                    } else {
                        view
                    }
                }))
            });
            let view = host.read_with(cx2, |host, _| host.0.clone());
            let cache = view.read_with(cx2, |view, _| view.image_cache_entity());
            cx2.refresh().unwrap();
            cx2.run_until_parked();
            let n = cache.read_with(cx2, |cache, _| cache.ready_count());
            println!("{label:<22} {ref_line:<32} ready={n} (want {want})");
            assert_eq!(n, want, "{label}: {ref_line}");
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 探查：live 视图里**哪些行形态**才会被认成图片卡片。
    ///
    /// `parse_image_line` 要求整行就是 `![alt](url)` 且行尾无其它内容
    /// （见 `markdown_live/view.rs`）。用户把图片写进列表项、和文字同一行、
    /// 或带尾随注释时，卡片根本不生成——这不是路径问题，是行形态问题。
    #[gpui::test]
    fn live_view_image_line_shapes(cx: &mut gpui::TestAppContext) {
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
        let dir = temp_dir("shapes");
        std::fs::write(dir.join("a.png"), PNG).unwrap();
        let doc = dir.join("note.md");

        let cases: Vec<(&str, &str)> = vec![
            ("plain line", "![x](a.png)"),
            ("indented", "    ![x](a.png)"),
            ("nested quote list", "> - ![x](a.png)"),
            ("task item", "- [ ] ![x](a.png)"),
            ("trailing spaces", "![x](a.png)   "),
            ("list item", "- ![x](a.png)"),
            ("ordered item", "1. ![x](a.png)"),
            ("blockquote", "> ![x](a.png)"),
            ("inline with text", "see ![x](a.png) here"),
            ("heading", "## ![x](a.png)"),
            ("title", "![x](a.png \"t\")"),
            ("two images one line", "![x](a.png) ![y](a.png)"),
        ];
        for (label, line) in cases {
            let (host, cx2) = cx.add_window_view(|_, cx| {
                Host(cx.new(|cx| {
                    let text = format!("{line}\n");
                    crate::editor::markdown_live::LiveMarkdownView::with_text(&text, cx)
                        .doc_path(&doc)
                }))
            });
            let view = host.read_with(cx2, |host, _| host.0.clone());
            let cache = view.read_with(cx2, |view, _| view.image_cache_entity());
            cx2.refresh().unwrap();
            cx2.run_until_parked();
            let n = cache.read_with(cx2, |cache, _| cache.ready_count());
            println!("{label:<20} {line:<30} ready={n}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 端到端：`Markdown` 实体（hover 浮层用的渲染器）对同样的行形态是否出图。
    ///
    /// 与 [`live_view_image_line_shapes`] 对照——两边不一致就是缺陷：
    /// 同一个文件在一处有图、在另一处没有。
    #[gpui::test]
    fn markdown_entity_image_line_shapes(cx: &mut gpui::TestAppContext) {
        use gpui::prelude::*;
        use gpui::{IntoElement, Render, Window};

        /// 用 `render_items_inline`（hover 浮层的真实入口）渲染一次。
        struct Host {
            md: gpui::Entity<crate::editor::markdown::Markdown>,
        }
        impl Render for Host {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let style = crate::editor::markdown::MarkdownStyle::hover_style(
                    window,
                    cx,
                    "Menlo".to_string(),
                );
                let items = Self::render_items(&self.md, style, cx);
                gpui::div().flex().flex_col().children(items)
            }
        }
        impl Host {
            fn render_items(
                md: &gpui::Entity<crate::editor::markdown::Markdown>,
                style: crate::editor::markdown::MarkdownStyle,
                cx: &mut gpui::App,
            ) -> Vec<gpui::AnyElement> {
                crate::editor::markdown::Markdown::render_items_inline(md.clone(), style, cx)
            }
        }

        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let dir = temp_dir("md-entity-shapes");
        std::fs::write(dir.join("a.png"), PNG).unwrap();

        let cases: Vec<(&str, &str)> = vec![
            ("plain line", "![x](a.png)"),
            ("indented", "    ![x](a.png)"),
            ("nested quote list", "> - ![x](a.png)"),
            ("task item", "- [ ] ![x](a.png)"),
            ("list item", "- ![x](a.png)"),
            ("ordered item", "1. ![x](a.png)"),
            ("blockquote", "> ![x](a.png)"),
            ("inline with text", "see ![x](a.png) here"),
            ("heading", "## ![x](a.png)"),
            ("title", "![x](a.png \"t\")"),
        ];
        for (label, line) in cases {
            let text = format!("{line}\n");
            let ctx_dir = dir.clone();
            let (host, cx2) = cx.add_window_view(|_, cx| {
                let md = cx.new(|cx| {
                    crate::editor::markdown::Markdown::new_with_dir(
                        text.clone(),
                        Some(ctx_dir.clone()),
                        cx,
                    )
                });
                Host { md }
            });
            let md = host.read_with(cx2, |host, _| host.md.clone());
            let cache = md.read_with(cx2, |md, _| md.image_cache.clone());
            cx2.refresh().unwrap();
            cx2.run_until_parked();
            let n = cache.read_with(cx2, |cache, _| cache.ready_count());
            println!("md-entity {label:<20} ready={n}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 徽章式一行多图：**两张不同的图**都必须进缓存。
    ///
    /// 只读预览删除后实时编辑器是唯一视图，README 徽章那种写法
    /// （`![a](x.svg) ![b](y.svg)`）不能退化成源码文本。
    /// 注意用两张**不同**文件：同图会被缓存按路径去重，测不出「只渲染了第一张」。
    #[gpui::test]
    fn live_view_renders_multiple_images_on_one_line(cx: &mut gpui::TestAppContext) {
        use gpui::prelude::*;
        use gpui::{Entity, IntoElement, Render, Window};

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
        let dir = temp_dir("multi-image-line");
        std::fs::write(dir.join("a.png"), PNG).unwrap();
        std::fs::write(dir.join("b.png"), PNG).unwrap();
        let doc = dir.join("note.md");

        let (host, cx2) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| {
                let text = "![a](a.png) ![b](b.png)\n";
                crate::editor::markdown_live::LiveMarkdownView::with_text(text, cx).doc_path(&doc)
            }))
        });
        let view = host.read_with(cx2, |host, _| host.0.clone());
        let cache = view.read_with(cx2, |view, _| view.image_cache_entity());
        cx2.refresh().unwrap();
        cx2.run_until_parked();
        assert_eq!(
            cache.read_with(cx2, |cache, _| cache.ready_count()),
            2,
            "both images on the same line must be loaded (not just the first)"
        );
        std::fs::remove_dir_all(&dir).ok();
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

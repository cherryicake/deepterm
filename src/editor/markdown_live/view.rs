//! [`LiveMarkdownView`]: the Obsidian-style live-preview markdown editor
//! entity. Owns a [`LiveBuffer`] (single truth), plans rows with reveal,
//! and renders decorations + [`LiveLine`](super::element::LiveLine) text.
//!
//! Rewritten for deepterm's GPUI fork; interaction semantics ported from
//! guise `markdown/editor.rs` (`on_key`/`on_mouse_down`/checkbox/link).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, ClickEvent, ClipboardEntry, ClipboardItem, Context, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, ScrollHandle,
    ShapedLine, SharedString, StatefulInteractiveElement, Styled, StyledText, Subscription,
    TextRun, Window, div, px, rems, svg,
};

use crate::assets::Icon;
use crate::editor::markdown::{Copy, MarkdownStyle, SelectAll};
use crate::editor::markdown_element::MarkdownElementBuilder;
use crate::editor::markdown_live::block::{
    split_table_row, table_cell_spans, table_delimiter_cols, Block,
};
use crate::editor::markdown_live::buffer::LiveBuffer;
use crate::editor::markdown_live::element::{
    apply_find_marks, apply_selection, apply_task_done, row_base_style, runs_for_row, LiveColors,
    LiveLine,
};
use crate::editor::markdown_live::inline::collect_refdefs;
use crate::editor::markdown_live::highlight::HighlightCache;
use crate::editor::markdown_live::mermaid::{MermaidBody, MermaidColors, MermaidState};
use crate::editor::markdown_live::images::{
    MAX_IMAGE_BYTES, assets_dir, format_extension, is_image_file, unique_file_name,
};
use crate::editor::markdown_live::interactions::{
    backspace_marker, classify_alone, insert_link, line_selection, list_cols,
    on_enter, on_tab, pos_of_offset, slash_items, slash_query_for, table_insert_row_below,
    table_nav, toggle_task, toggle_wrap, OrderedCounter, SLASH_COMMANDS,
};
use crate::editor::markdown_live::layout::{
    byte_for_col, col_for_byte, plan_cell, src_for_vis, vis_for_src, RowKind,
};
use crate::editor::markdown_live::model::Pos;
use crate::editor::markdown_live::reveal::{
    classify_document, plan_rows_with_refs, refdefs_for_lines, strip_plan_prefix, CalloutKind,
    Classified, PlannedRow, Reveal,
};
use crate::components::input::{
    blink_cursor::{BlinkCursor, CURSOR_WIDTH},
    cursor::CursorShape,
    search::{SearchPanel, SearchTarget},
};
use crate::theme::ActiveTheme;
use ropey::Rope;

#[derive(Debug, Clone)]
pub enum LiveViewEvent {
    Change(String),
    LinkClick(String),
}

pub struct LiveMarkdownView {
    buffer: LiveBuffer,
    focus: FocusHandle,
    scroll: ScrollHandle,
    hl: HighlightCache,
    tab_size: usize,
    read_only: bool,
    placeholder: SharedString,
    selecting: bool,
    goal_x: Option<f32>,
    planned_version: u64,
    planned_key: (Pos, Option<(Pos, Pos)>, bool, bool, bool),
    planned: Vec<PlannedRow>,
    /// P1: 按 text_version 缓存的文档分类结果（块类型 + 前置语法语言）。
    /// 只有文本变更才重算，纯光标移动时被 `plan_rows` 复用 —— 跳过两个
    /// O(n) 的分类/setext pass。
    planned_classified: Classified,
    planned_classified_version: u64,
    /// P1: 按 text_version 缓存的引用定义（`[label]: dest`）。同样是纯文本相关
    /// 且依赖 `planned_classified.blocks`，光标移动时复用 —— 跳过第三个 O(n)
    /// 的全文扫描。`Rc` 共享避免每次复用拷贝整表。
    planned_refs: Rc<HashMap<String, String>>,
    planned_refs_version: u64,
    /// 表格单元格用的文档级参考链接表（`[t][ref]`）。全文档扫描得来的，
    /// 按「文本版本 + 规划键」缓存，避免每帧重扫一遍整篇文档。
    /// 用 `Rc` 共享：每帧只需自增引用计数而不是整表克隆（见 P4）。
    table_refs: Rc<HashMap<String, String>>,
    /// `table_refs` 对应的缓存键；`None` 表示还没算过。
    table_refs_key: Option<(u64, (Pos, Option<(Pos, Pos)>, bool, bool, bool))>,
    copied_code_blocks: HashSet<usize>,
    /// Mermaid 图渲染缓存 +「预览 | 源码」按钮状态（缓存键 = 块源码 + 主题指纹，
    /// 后台 + 防抖渲染；详见 `mermaid` 模块）。
    mermaid: MermaidState,
    /// Bring the caret's row into view on the next render (set by every
    /// edit/move; consumed by `scroll_to_item` on the rows column).
    scroll_to_cursor: bool,
    /// Source-line range per rendered row element (dead-zone hit fallback:
    /// clicks between/around rows map to the nearest element, then to a
    /// line within it via the recorded layouts).
    row_targets: Vec<(usize, usize)>,
    /// Last frame's per-line text layouts, recorded during `LiveLine`
    /// paint. Powers visual-row (soft-wrap) movement and the dead-zone
    /// click/drag fallbacks.
    row_layouts: std::collections::HashMap<usize, RowLayoutInfo>,
    /// Fingerprint of the style inputs that bake colors into the highlight
    /// cache; a change (theme switch) clears it.
    hl_style_key: u64,
    blink: Entity<BlinkCursor>,
    cursor_shape: CursorShape,
    cursor_blink: bool,
    blink_on: bool,
    /// Typora 风格大纲侧栏是否展开（`Cmd+Shift+O` / 状态条切换）。
    show_outline: bool,
    /// 本文档路径（编辑器传入）：图片粘贴/拖拽按 Typora 存入 `<stem>.assets/`。
    doc_path: Option<PathBuf>,
    /// 源码模式（`Cmd+/` / 状态条切换）：全文档显示源码标记（Typora 源码模式）。
    show_source: bool,
    /// Slash 菜单高亮项；`slash_suppress` 为 Esc 忽略中的查询（内容变化即恢复）。
    slash_index: usize,
    slash_suppress: Option<String>,
    /// 代码编辑器同款 `SearchPanel`（`Cmd+F` 懒建）：匹配引擎、面板 UI、
    /// 跳转与替换逻辑全部复用，不再自实现（`SearchTarget` 桥接行列缓冲）。
    search_panel: Option<Entity<SearchPanel<LiveMarkdownView>>>,
    _subscriptions: Vec<Subscription>,
    /// 行虚拟化（大文档）：每帧只建视口内 group 的元素，其余以占位撑高。
    /// group 粒度保证代码卡片/引用/表格不被拦腰截断；小文档走全量老路。
    /// 上一帧实测的 group 高（`scroll.bounds_for_item` 回填；跨帧保留，
    /// 缺失时用实测均值校准后的行数估算，绘制后逐帧自我修正）。
    group_heights: std::collections::HashMap<usize, f32>,
    heights_version: u64,
    /// 上一帧视口宽：换行随宽度变化，变宽即清空 `group_heights`。
    last_vp_width: f32,
    /// 上一帧渲染的 group 窗口（含占位映射），供本帧回填实测高度。
    last_window: Option<(usize, usize)>,
    /// 上一帧窗口内每个 group 产生的滚动子项数（通常 1；源码态表格为行数）。
    /// 回填实测高度时按此切分 `bounds_for_item`，避免把高度记到错位的 group 上
    /// （错位会导致 prefix 跳变，是底部回滚上下抖动的主因之一）。
    last_window_counts: Vec<usize>,
    /// 本帧虚拟化几何（点击映射用；非虚拟化时为空）：前缀和、每 group
    /// 首末源码行、窗口下界、顶部占位占用的 child 序号偏移。
    virt_prefix: Vec<f32>,
    virt_group_lines: Vec<(usize, usize)>,
    virt_lo: usize,
    virt_hi: usize,
    virt_top_off: usize,
    /// 状态条字数缓存 `(text_version, selection, chars, lines, sel_chars)`：
    /// 全文档字符统计每帧 O(n)，命中缓存后为 O(1)。
    stat_cache: Option<(u64, Option<(Pos, Pos)>, usize, usize, Option<usize>)>,
    /// 已发出的 `Change` 对应的 `text_version`：`edit` 不再直接发射，
    /// 统一由 `after_edit`/`set_text` 按版本去重后发射，避免每键两次同步。
    emitted_version: u64,
    /// 测试探针：上帧 `(groups_total, groups_rendered, virtualized)`。
    #[cfg(test)]
    last_render_stats: (usize, usize, bool),
}

impl EventEmitter<LiveViewEvent> for LiveMarkdownView {}

impl LiveMarkdownView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_text("", cx)
    }

    pub fn with_text(text: &str, cx: &mut Context<Self>) -> Self {
        let blink = cx.new(|_| BlinkCursor::new());
        let _subscriptions = vec![cx.observe(&blink, |_, _, cx| cx.notify())];
        Self {
            buffer: LiveBuffer::new(text),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            hl: HighlightCache::new(),
            tab_size: 4,
            read_only: false,
            placeholder: SharedString::from(""),
            selecting: false,
            goal_x: None,
            planned_version: u64::MAX,
            planned_key: (Pos::default(), None, false, false, false),
            planned: Vec::new(),
            planned_classified: Classified {
                blocks: Vec::new(),
                lang_before: Vec::new(),
            },
            planned_classified_version: u64::MAX,
            planned_refs: Rc::new(HashMap::new()),
            planned_refs_version: u64::MAX,
            table_refs: Rc::new(HashMap::new()),
            table_refs_key: None,
            copied_code_blocks: HashSet::new(),
            mermaid: MermaidState::new(),
            scroll_to_cursor: false,
            row_targets: Vec::new(),
            row_layouts: std::collections::HashMap::new(),
            hl_style_key: 0,
            blink,
            cursor_shape: CursorShape::Bar,
            cursor_blink: true,
            blink_on: false,
            show_outline: false,
            doc_path: None,
            show_source: false,
            slash_index: 0,
            slash_suppress: None,
            search_panel: None,
            _subscriptions,
            group_heights: std::collections::HashMap::new(),
            heights_version: u64::MAX,
            last_vp_width: 0.0,
            last_window: None,
            last_window_counts: Vec::new(),
            virt_prefix: Vec::new(),
            virt_group_lines: Vec::new(),
            virt_lo: 0,
            virt_hi: 0,
            virt_top_off: 0,
            stat_cache: None,
            emitted_version: 0,
            #[cfg(test)]
            last_render_stats: (0, 0, false),
        }
    }

    pub fn value(mut self, text: &str) -> Self {
        self.buffer = LiveBuffer::new(text);
        self
    }

    pub fn placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = text.into();
        self
    }

    pub fn read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }

    /// 文档落盘路径：图片粘贴/拖拽以此定位 `<stem>.assets/`（Typora 布局）。
    pub fn doc_path(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if !path.as_os_str().is_empty() {
            self.doc_path = Some(path.to_path_buf());
        }
        self
    }

    /// 异步加载完成后同步只读态（`Editor::spawn_file_load`）：仅影子
    /// `InputState::disabled` 不足以锁住 live 视图自身编辑。
    pub fn set_read_only(&mut self, ro: bool, cx: &mut Context<Self>) {
        if self.read_only != ro {
            self.read_only = ro;
            cx.notify();
        }
    }

    /// 更新文档落盘路径（预留：重命名/另存后同步 assets 定位）。
    pub fn set_doc_path(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if !path.as_os_str().is_empty() {
            self.doc_path = Some(path.to_path_buf());
        }
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.pause_caret(cx);
        self.hl.clear();
        self.copied_code_blocks.clear();
        self.scroll_to_cursor = true;
        self.emitted_version = self.buffer.text_version();
        if let Some(panel) = self.search_panel.clone() {
            let text = Rope::from(self.buffer.text());
            panel.update(cx, |p, _| p.sync_text(&text));
        }
        cx.emit(LiveViewEvent::Change(self.buffer.text()));
        cx.notify();
    }

    pub fn mark_saved(&mut self) {
        self.buffer.mark_saved();
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    pub fn text_version(&self) -> u64 {
        self.buffer.text_version()
    }

    /// 测试探针：上帧 `(groups_total, groups_rendered, virtualized)`。
    #[cfg(test)]
    pub fn last_virtualization_stats(&self) -> (usize, usize, bool) {
        self.last_render_stats
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape, cx: &mut Context<Self>) {
        if self.cursor_shape != shape {
            self.cursor_shape = shape;
            self.blink.update(cx, |b, cx| b.show_cursor(cx));
            cx.notify();
        }
    }

    pub fn set_cursor_blink(&mut self, blink: bool, cx: &mut Context<Self>) {
        if self.cursor_blink != blink {
            self.cursor_blink = blink;
            self.blink.update(cx, |b, cx| b.set_blink_enabled(blink, cx));
            cx.notify();
        }
    }

    fn pause_caret(&mut self, cx: &mut Context<Self>) {
        if self.blink_on {
            self.blink.update(cx, |b, cx| b.pause_blinking(cx));
        }
    }

    fn show_caret(&self, window: &Window, cx: &App) -> bool {
        !self.read_only
            && self.focus.is_focused(window)
            && window.is_window_active()
            && self.blink.read(cx).visible()
    }

    // ---- image paste / drop (Typora parity) -----------------------------
    // 剪贴板图片字节 / 拖拽或 Finder 拷贝的图片文件落盘到 `<stem>.assets/`，
    // 正文插入 `![](相对路径)`（图片独占行时渲染为图片卡片）。

    /// 图片字节落盘，返回文档相对引用名（无文档路径时用临时目录绝对路径）。
    fn save_image_bytes(&self, bytes: &[u8], ext: &str) -> Option<String> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
            return None;
        }
        let (dir, relative) = match self.doc_path.as_deref() {
            Some(doc) => (assets_dir(doc), true),
            None => (
                std::env::temp_dir().join("deepterm-md-images"),
                false,
            ),
        };
        std::fs::create_dir_all(&dir).ok()?;
        let name = unique_file_name(&dir, "image", ext);
        std::fs::write(dir.join(&name), bytes).ok()?;
        if relative {
            Some(format!(
                "{}/{name}",
                dir.file_name()?.to_str()?
            ))
        } else {
            Some(dir.join(name).to_string_lossy().to_string())
        }
    }

    /// 单个图片文件导入 assets（拖拽 / Finder 拷贝），返回引用名。
    fn import_image_file(&self, src: &Path) -> Option<String> {
        if !is_image_file(src) {
            return None;
        }
        let ext = src
            .extension()?
            .to_str()?
            .to_ascii_lowercase();
        let bytes = std::fs::read(src).ok()?;
        self.save_image_bytes(&bytes, &ext)
    }

    /// 引用名插入正文：空行直接插入，非空行另起一行（Typora 图片独占段落）。
    fn insert_image_refs(&mut self, refs: &[String], cx: &mut Context<Self>) {
        if refs.is_empty() {
            return;
        }
        let md: String = refs
            .iter()
            .map(|r| format!("![]({r})"))
            .collect::<Vec<_>>()
            .join("\n");
        let cur = self.buffer.cursor();
        let blank = self
            .buffer
            .line(cur.line)
            .is_some_and(|l| l.trim().is_empty());
        self.edit(cx, |m| {
            if blank {
                m.insert(&md);
            } else {
                m.insert(&format!("\n{md}"));
            }
        });
    }

    /// 剪贴板图片粘贴：无文本但有图片/图片路径时落盘插入。
    /// 有图片条目即视为消费（返回 true），调用方直接 `after_edit`。
    fn paste_images(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut saw_image = false;
        let mut refs = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(img) => {
                    saw_image = true;
                    if let Some(r) = self.save_image_bytes(&img.bytes, format_extension(img.format))
                    {
                        refs.push(r);
                    }
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    for path in paths.paths() {
                        if !is_image_file(path) {
                            continue;
                        }
                        saw_image = true;
                        if let Some(r) = self.import_image_file(path) {
                            refs.push(r);
                        }
                    }
                }
                _ => {}
            }
        }
        self.insert_image_refs(&refs, cx);
        saw_image
    }

    /// 图片文件拖拽投放（`on_drop::<ExternalPaths>`）：导入并插入。
    fn drop_image_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> bool {
        let refs: Vec<String> = paths.iter().filter_map(|p| self.import_image_file(p)).collect();
        if refs.is_empty() {
            return false;
        }
        self.insert_image_refs(&refs, cx);
        true
    }

    // ---- find / replace: 代码编辑器同款 `SearchPanel` -------------------
    // 经 `SearchTarget` 桥接行列缓冲：匹配引擎、面板 UI、跳转与替换逻辑
    // 全部复用 `components::input::search`，此处只做字节偏移↔行列映射。

    /// 打开 `SearchPanel`（`Cmd+F`，只读也允许查找）：懒建面板、同步全文、
    /// 预填选区首行、聚焦输入框并定位到光标后首个命中（`show` 内完成）。
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_panel.is_none() {
            self.search_panel = Some(SearchPanel::new(cx.entity(), window, cx));
        }
        let text = Rope::from(self.buffer.text());
        let prefill = self.search_prefill();
        if let Some(panel) = self.search_panel.clone() {
            panel.update(cx, |p, cx| {
                p.sync_text(&text);
                p.show(&prefill, window, cx);
            });
            cx.notify();
        }
    }

    /// 查找/替换输入框是否持有焦点（此时回车走面板跳转而非正文换行）。
    fn search_panel_focused(&self, window: &Window, cx: &App) -> bool {
        self.search_panel
            .as_ref()
            .is_some_and(|p| p.read(cx).input_focused(window, cx))
    }

    /// 光标的全文字节偏移（`SearchTarget::search_caret` / `anchor_offset`）。
    fn search_caret_offset(&self) -> usize {
        let cur = self.buffer.cursor();
        let mut off = 0usize;
        for (i, line) in self.buffer.lines().iter().enumerate() {
            if i == cur.line {
                return off + byte_for_col(line, cur.col);
            }
            off += line.len() + 1;
        }
        off
    }


    fn caret_block_for(
        shape: CursorShape,
        ch: Option<char>,
        base: &gpui::TextStyle,
        font_size: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> (Pixels, Option<ShapedLine>) {
        if !matches!(
            shape,
            CursorShape::Block | CursorShape::Hollow | CursorShape::Underline
        ) {
            return (CURSOR_WIDTH, None);
        }
        let fallback = (line_height * 0.6).max(CURSOR_WIDTH);
        let Some(ch) = ch else {
            return (fallback, None);
        };
        if ch == '\n' || ch == '\r' {
            return (fallback, None);
        }
        let editor_bg = cx.theme().editor_background();
        let text_color = if editor_bg.a < 0.75 {
            if cx.theme().appearance.is_dark() {
                gpui::black()
            } else {
                gpui::white()
            }
        } else {
            editor_bg
        };
        let s: SharedString = ch.to_string().into();
        let shaped = window.text_system().shape_line(
            s.clone(),
            font_size,
            &[TextRun {
                len: s.len(),
                font: base.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let width = if ch.is_ascii_whitespace() {
            fallback
        } else {
            fallback.max(shaped.width)
        };
        (width, Some(shaped))
    }

    /// Current vertical scroll offset (0 at top, negative when scrolled
    /// down). Diagnostics/test helper.
    pub fn scroll_offset_y(&self) -> f32 {
        f32::from(self.scroll.offset().y)
    }

    /// Record a row's layout from `LiveLine::paint` (element.rs): powers
    /// visual-row movement and the dead-zone hit fallbacks.
    pub(crate) fn record_row_layout(
        &mut self,
        line: usize,
        layout: gpui::TextLayout,
        text_len: usize,
    ) {
        self.row_layouts
            .insert(line, RowLayoutInfo { layout, text_len });
    }

    /// 内容视口（`#live-markdown-content`）的矩形范围。`LiveLine` 的
    /// 鼠标监听是窗口级全局的（不受命中测试约束），用它判断点击是否真的
    /// 落在本视图横向范围内 —— 右侧终端等兄弟面板的点击超出此范围，必须
    /// 放行，否则会被当成“行尾空白”认领并抢走焦点。
    pub(crate) fn content_viewport_bounds(&self) -> gpui::Bounds<Pixels> {
        self.scroll.bounds()
    }

    /// Mutate + rerender (the `edit` transaction). Never emits `Change`
    /// itself: every text mutation is followed by `after_edit`, which emits
    /// once per transaction (deduped by `emitted_version`). Emitting here
    /// as well doubled shadow-`InputState` sync + `buffer.text()` clones.
    pub fn edit<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut crate::editor::markdown_live::model::EditorModel) -> R) -> R {
        self.scroll_to_cursor = true;
        self.pause_caret(cx);
        let out = self.buffer.edit(f);
        cx.notify();
        out
    }

    fn planned_rows(&mut self, focused: bool) -> &[PlannedRow] {
        let key = (
            self.buffer.cursor(),
            self.buffer.selection(),
            focused,
            self.read_only,
            self.show_source,
        );
        if self.planned_version != self.buffer.text_version() || self.planned_key != key {
            // P1: 只有文本变更才重新分类；仅光标/选区移动时直接复用缓存，
            // 跳过两个 O(n) 的分类（classify）+ setext 反向 pass。
            // 分类结果按文本版本刷新（见 P1）。
            if self.planned_classified_version != self.buffer.text_version() {
                let classified = classify_document(self.buffer.lines());
                self.planned_classified = classified;
                self.planned_classified_version = self.buffer.text_version();
            }
            // 引用定义按文本版本刷新（依赖分类结果，同样纯文本相关，见 P1）。
            if self.planned_refs_version != self.buffer.text_version() {
                let refs = Rc::new(refdefs_for_lines(
                    self.buffer.lines(),
                    &self.planned_classified.blocks,
                ));
                self.planned_refs = refs;
                self.planned_refs_version = self.buffer.text_version();
            }
            // 源码模式：用覆盖全文档的选区迫使 `line_set` 揭示每一行
            //（`Reveal` 不变，所有调用点零改动）。
            let last = self.buffer.lines().len().saturating_sub(1);
            let (cursor, selection) = if self.show_source {
                (
                    self.buffer.cursor(),
                    Some((Pos::new(0, 0), Pos::new(last, usize::MAX))),
                )
            } else {
                (self.buffer.cursor(), self.buffer.selection())
            };
            let reveal = Reveal {
                cursor,
                selection,
                focused,
                read_only: self.read_only,
                show_source: self.show_source,
            };
            let mut planned = plan_rows_with_refs(
                self.buffer.lines(),
                &self.planned_classified,
                &self.planned_refs,
                reveal,
            );
            // Callout openers hide their `[!TAG] ` prefix when not revealed,
            // matching the preview which consumes the tag. Stripped here so
            // paint and hit-testing share one mapping.
            for row in &mut planned {
                let is_opener = row.callout.as_ref().is_some_and(|c| c.is_first);
                if !is_opener || row.plan.revealed {
                    continue;
                }
                let Some(callout) = row.callout.as_ref() else {
                    continue;
                };
                let tag_len = callout.kind.label().len() + 3; // `[!TAG]`
                let src_line = self.buffer.line(row.line).unwrap_or("");
                let content_off = match &row.block {
                    Block::Quote { content, .. } => *content,
                    _ => 0,
                };
                let after_tag = src_line
                    .get(content_off + tag_len..)
                    .unwrap_or("")
                    .strip_prefix(' ')
                    .map(|_| tag_len + 1)
                    .unwrap_or(tag_len);
                if after_tag <= row.plan.visible.len() {
                    let src_off = (content_off + after_tag).min(src_line.len());
                    row.plan = strip_plan_prefix(&row.plan, after_tag, src_off);
                }
            }
            self.planned = planned;
            self.planned_version = self.buffer.text_version();
            self.planned_key = key;
        }
        &self.planned
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        // 搜索/替换等程序化移动可能把光标送进图模式块（源码未渲染）：
        // 统一夹出块外，保证「编辑只能经『源码』tab」不被旁路。
        self.clamp_cursor_out_of_image_blocks(cx);
        // Slash menu highlight resets on every edit (Up/Down move through
        // `after_move`, which preserves it); the query itself re-derives
        // from the buffer each frame.
        self.slash_index = 0;
        let version = self.buffer.text_version();
        if version != self.emitted_version {
            self.emitted_version = version;
            // 查找面板的匹配跟随文档（`InputState::update_search` parity）。
            if let Some(panel) = self.search_panel.clone() {
                let text = Rope::from(self.buffer.text());
                panel.update(cx, |p, _| p.sync_text(&text));
            }
            cx.emit(LiveViewEvent::Change(self.buffer.text()));
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        // 图模式 mermaid 块是移动禁区（源码行未渲染）：Up/Down 越过、
        // 搜索跳转/点击落进来时夹到块外——光标永远不驻留在隐藏源码里。
        self.clamp_cursor_out_of_image_blocks(cx);
        cx.notify();
        cx.stop_propagation();
    }

    /// True when the cursor's row is source-only (code / fence / table /
    /// frontmatter): rich inline toggles (Cmd+B/I/K) must insert literally
    /// there instead of wrapping. Reads the last frame's plan for document
    /// context (fences are only knowable with it).
    fn cursor_is_source_only(&self) -> bool {
        let line = self.buffer.cursor().line;
        self.planned
            .iter()
            .find(|r| r.line == line)
            .is_some_and(|r| crate::editor::markdown_live::interactions::is_source_only(&r.block))
    }

    /// Open slash-command menu `(query, SLASH_COMMANDS indices)` when the
    /// cursor sits in a `<=3 spaces + /query` trigger with no selection
    /// and the query isn't Esc-suppressed. Typora/Notion `/` parity.
    fn slash_menu(&self) -> Option<(String, Vec<usize>)> {
        if self.read_only || self.buffer.selection().is_some() {
            return None;
        }
        let cur = self.buffer.cursor();
        let line_text = self.buffer.line(cur.line)?;
        let query = slash_query_for(line_text, cur.col, false)?;
        if self.slash_suppress.as_deref() == Some(query.as_str()) {
            return None;
        }
        let items = slash_items(&query);
        if items.is_empty() {
            return None;
        }
        Some((query, items))
    }

    /// Apply slash command `cmd_ix`: replace the `/query` before the
    /// cursor with the snippet. Fenced code parks the cursor inside the
    /// block; links park it between the parens.
    fn apply_slash(&mut self, cmd_ix: usize, cx: &mut Context<Self>) {
        let Some(cmd) = SLASH_COMMANDS.get(cmd_ix) else {
            return;
        };
        let (insert, back, middle) = (cmd.insert, cmd.back, cmd.middle);
        let cur = self.buffer.cursor();
        let line_text = self
            .buffer
            .line(cur.line)
            .map(str::to_string)
            .unwrap_or_default();
        let col = cur.col.min(line_text.chars().count());
        let end_byte = byte_for_col(&line_text, col);
        let indent = line_text
            .bytes()
            .take_while(|&b| b == b' ')
            .count()
            .min(end_byte);
        let start = Pos::new(cur.line, col_for_byte(&line_text, indent));
        let end = Pos::new(cur.line, col);
        self.edit(cx, |m| m.replace_range(start, end, insert));
        if middle {
            self.edit(cx, |m| m.move_to(cur.line + 1, usize::MAX, false));
        } else {
            for _ in 0..back {
                self.edit(cx, |m| m.move_left(false));
            }
        }
        self.slash_index = 0;
        self.after_edit(cx);
    }

    /// Auto-pair brackets/quotes (Typora parity): closers skip over an
    /// identical char ahead, opens wrap the selection or insert a pair
    /// with the cursor inside. Quotes pair only at word boundaries so
    /// `don't` stays literal; a backtick after a backtick stays literal
    /// so triple-backtick fences type naturally. True when handled.
    fn try_autopair(&mut self, ch: char, cx: &mut Context<Self>) -> bool {
        const OPENS: [(char, char); 6] = [
            ('(', ')'),
            ('[', ']'),
            ('{', '}'),
            ('"', '"'),
            ('\'', '\''),
            ('`', '`'),
        ];
        const CLOSERS: [char; 3] = [')', ']', '}'];
        if self.buffer.selection().is_some() {
            if let Some((open, close)) = OPENS.iter().find(|(o, _)| *o == ch).copied() {
                self.edit(cx, |m| {
                    m.wrap_selection(open, close);
                });
                return true;
            }
            return false;
        }
        if CLOSERS.contains(&ch) {
            self.edit(cx, |m| {
                m.insert_closer(ch);
            });
            return true;
        }
        let Some((open, close)) = OPENS.iter().find(|(o, _)| *o == ch).copied() else {
            return false;
        };
        if open != close {
            self.edit(cx, |m| m.insert_pair(open, close));
            return true;
        }
        // Symmetric quote/backtick.
        let cur = self.buffer.cursor();
        let src = self.buffer.line(cur.line).unwrap_or("");
        let chars: Vec<char> = src.chars().collect();
        let ahead = chars.get(cur.col.min(chars.len())).copied();
        let behind = cur.col.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if ahead == Some(ch) {
            self.edit(cx, |m| m.move_right(false));
        } else if ch == '`' && behind == Some('`') {
            self.edit(cx, |m| m.insert(&ch.to_string()));
        } else if word(behind) || word(ahead) {
            self.edit(cx, |m| m.insert(&ch.to_string()));
        } else {
            self.edit(cx, |m| m.insert_pair(open, close));
        }
        true
    }

    // ---- dead-zone hit fallback (B4/B5) ---------------------------------

    /// The rendered row element whose band contains (or is nearest to)
    /// window-space `y`, as a source-line range. Uses the scroll handle's
    /// per-child bounds (document space) and the current offset.
    fn nearest_row_for_y(&self, y: Pixels) -> Option<(usize, usize)> {
        let n = self.row_targets.len();
        if n == 0 {
            return None;
        }
        let doc_y = f32::from(y) - f32::from(self.scroll.offset().y);
        // Virtualized: clicks landing on spacers (unrendered territory)
        // map through prefix sums to the group under the cursor instead of
        // snapping to the nearest *rendered* row.
        if !self.virt_prefix.is_empty() && !self.virt_group_lines.is_empty() {
            let g = self.virt_group_lines.len();
            let rendered_top = self.virt_prefix[self.virt_lo.min(g)];
            let rendered_bot = self.virt_prefix[(self.virt_hi + 1).min(g)];
            if doc_y < rendered_top || doc_y > rendered_bot {
                let total = self.virt_prefix[g];
                let y = doc_y.clamp(0.0, total);
                let mut gi = g - 1;
                for (i, w) in self.virt_prefix.windows(2).enumerate() {
                    if y < w[1] || i == g - 1 {
                        gi = i;
                        break;
                    }
                }
                return Some(self.virt_group_lines[gi.min(g - 1)]);
            }
        }
        // Rendered rows live after the top spacer (if any) in the scroll
        // column's child list.
        let base = self.virt_top_off;
        let mut best: Option<(usize, f32)> = None;
        for ix in 0..n {
            let Some(b) = self.scroll.bounds_for_item(ix + base) else {
                continue;
            };
            let (top, bottom) = (f32::from(b.top()), f32::from(b.bottom()));
            if doc_y >= top && doc_y <= bottom {
                return Some(self.row_targets[ix]);
            }
            let dist = if doc_y < top {
                top - doc_y
            } else {
                doc_y - bottom
            };
            if best.is_none_or(|(_, d)| dist < d) {
                best = Some((ix, dist));
            }
        }
        best.map(|(ix, _)| self.row_targets[ix])
    }

    /// Resolve a click to a concrete source line: the row element range
    /// from [`Self::nearest_row_for_y`], then the line within the range
    /// whose recorded layout band contains the click.
    fn line_for_click(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let (a, b) = self.nearest_row_for_y(position.y)?;
        for line in a..=b {
            if let Some(info) = self.row_layouts.get(&line) {
                let lb = info.layout.bounds();
                if position.y >= lb.top() && position.y <= lb.bottom() {
                    return Some(line);
                }
            }
        }
        Some(a)
    }

    /// Column for a fallback click on `line`: through the recorded layout
    /// when one exists (x-precise like a real text click), else line end.
    fn col_for_click(&self, line: usize, position: gpui::Point<Pixels>) -> usize {
        let (Some(info), Some(row), Some(text)) = (
            self.row_layouts.get(&line),
            self.planned.iter().find(|r| r.line == line),
            self.buffer.line(line),
        ) else {
            return usize::MAX;
        };
        let b = info.layout.bounds();
        let y = position.y.clamp(b.top(), b.bottom() - px(1.));
        let x = position.x.max(b.left());
        let vis = match info.layout.index_for_position(gpui::point(x, y)) {
            Ok(ix) | Err(ix) => ix.min(info.text_len),
        };
        let src = src_for_vis(&row.plan.segs, vis);
        col_for_byte(text, src)
    }

    /// Root-level fallback for clicks that no `LiveLine`/row element
    /// claimed: margins between rows, card paddings, areas past EOL.
    /// Mirrors guise's continuous `hit()` — every click in the document
    /// area lands the cursor somewhere sensible.
    fn on_fallback_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only claim clicks inside the document scroll area; everything
        // else (app chrome, other panes) keeps bubbling.
        let sb = self.scroll.bounds();
        if ev.position.y < sb.top() || ev.position.y > sb.bottom() {
            return;
        }
        window.focus(&self.focus, cx);
        self.goal_x = None;
        let Some(line) = self.line_for_click(ev.position) else {
            return;
        };
        // 点击图模式 mermaid 图片区：无文本可落点，不移动光标
        //（仍聚焦并停冒泡——但绝不会把光标放进隐藏源码里）。
        if self.line_in_mermaid_image(line) {
            cx.stop_propagation();
            return;
        }
        let col = self.col_for_click(line, ev.position);
        let shift = ev.modifiers.shift;
        let click_count = ev.click_count;
        self.edit(cx, |m| {
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    m.select_word();
                } else if click_count > 2 {
                    m.select_line();
                }
            }
        });
        self.selecting = click_count <= 1;
        cx.stop_propagation();
        cx.notify();
    }

    /// Fallback for drags through zones no `LiveLine` covers (blank rows,
    /// margins): keep the selection extending to the nearest line.
    pub(crate) fn on_fallback_drag(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let Some(line) = self.line_for_click(position) else {
            return;
        };
        // 拖拽扫过图模式图片区：忽略（保持光标在块外），不扩展进隐藏源码。
        if self.line_in_mermaid_image(line) {
            return;
        }
        let col = self.col_for_click(line, position);
        self.edit(cx, |m| m.move_to(line, col, true));
    }

    // ---- mermaid 图模式禁区（严格 tab-only：图可见时源码不可编辑） -------

    // ---- 表格单元格编辑（点击格内直接改，不退回源码） -------------------

    /// 供 `LiveLine` 的单元格模式取整行文本（映射需要 char 列）。
    pub(crate) fn line_text(&self, line: usize) -> Option<String> {
        self.buffer.line(line).map(str::to_string)
    }

    /// 单元格内的点击：把光标放到格内对应位置（不再整块跳回源码首行）。
    pub(crate) fn on_table_cell_mouse_down(
        &mut self,
        line: usize,
        col: usize,
        click_count: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.goal_x = None;
        self.edit(cx, |m| {
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    m.select_word();
                } else if click_count > 2 {
                    m.select_line();
                }
            }
        });
        self.selecting = click_count <= 1;
        cx.stop_propagation();
        cx.notify();
    }

    /// 单元格内拖拽：把选区终点扩到格内对应位置。
    pub(crate) fn on_table_cell_drag(
        &mut self,
        line: usize,
        col: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        self.edit(cx, |m| m.move_to(line, col, true));
    }

    /// 光标在表格里时，Esc 的目标行：该表格块的最后一行 + 1（贴文档尾则
    /// 表格首行前一行）。光标不在表格里返回 `None`（Esc 维持原行为）。
    fn table_exit_line(&self) -> Option<usize> {
        let cur = self.buffer.cursor().line;
        if !self
            .planned
            .iter()
            .find(|r| r.line == cur)
            .is_some_and(|r| matches!(r.block, Block::Table))
        {
            return None;
        }
        let last = self
            .planned
            .iter()
            .filter(|r| matches!(r.block, Block::Table))
            .map(|r| r.line)
            .max()?;
        let line_count = self.buffer.line_count();
        Some(if last + 1 < line_count {
            last + 1
        } else {
            self.planned
                .iter()
                .filter(|r| matches!(r.block, Block::Table))
                .map(|r| r.line)
                .min()
                .unwrap_or(0)
                .saturating_sub(1)
        })
    }

    /// 表格活动单元格：光标所在行 + 列 + 该格的**内容字节范围**。    ///
    /// 与渲染分支共用同一套切分（`block::table_cell_spans`），所以点哪个格
    /// 就编辑哪个格，不会出现「Grid 画 3 列、Tab 走 2 格」这类错位。
    fn table_active_cell(
        &self,
        rows: &[PlannedRow],
        idxs: &[usize],
        cursor: Pos,
    ) -> Option<(usize, usize, std::ops::Range<usize>)> {
        let cursor_line = cursor.line;
        // 光标行必须真的属于这个表格组（防御：调用点已保证，双保险）。
        idxs.iter().copied().find(|pi| rows[*pi].line == cursor_line)?;
        let line = self.buffer.line(cursor_line)?;
        let col = cursor.col.min(line.chars().count());
        let byte = byte_for_col(line, col);
        let spans = table_cell_spans(line);
        // 光标在格内容里 → 该格；落在管道/空白等「格间」位置 → 取光标
        // **右侧**那一格（与 Tab 前进方向一致，不会莫名跳到最后一格）。
        let found: (usize, (usize, usize)) = spans
            .iter()
            .enumerate()
            .find(|(_, (s, e))| byte >= *s && byte <= *e)
            .map(|(i, (s, e))| (i, (*s, *e)))
            .or_else(|| {
                spans
                    .iter()
                    .enumerate()
                    .find(|(_, (s, _))| *s >= byte)
                    .map(|(i, (s, e))| (i, (*s, *e)))
            })
            .or_else(|| spans.last().map(|(s, e)| (spans.len() - 1, (*s, *e))))
            .expect("table_cell_spans 恒非空");
        let (ci, (s, e)) = found;
        // 内容范围 = 去 padding 后的实际文本（光标落在文字上，不落在空格里）。
        let cell = &line[s..e];
        let lead = cell.len() - cell.trim_start().len();
        let trail = cell.len() - cell.trim_end().len();
        let content = (s + lead)..(e - trail).max(s + lead);
        Some((cursor_line, ci, content))
    }

    /// 「预览 | 源码」按钮的唯一入口：切状态 + 切到预览时把光标夹出块外。
    ///
    /// 夹光标必须在**切换的同一帧**做完：只改状态的话，光标会滞留在下一帧
    /// 就不可见的源码区里（真实点击路径实测复现，见 `live_mermaid_graph_mode`
    /// 第 4 步）。
    fn set_mermaid_showing_code(
        &mut self,
        block_id: usize,
        showing: bool,
        cx: &mut Context<Self>,
    ) {
        if self.mermaid.set_showing_code(block_id, showing) && !showing {
            self.clamp_cursor_out_of_image_blocks(cx);
        }
        cx.notify();
    }

    /// 全文档源码模式开关（`Cmd+/` / 状态条芯片）：关掉源码模式时，原本
    /// 可编辑的 mermaid 块会变回图，光标必须同帧夹出隐藏区。
    fn toggle_show_source(&mut self, cx: &mut Context<Self>) {
        self.show_source = !self.show_source;
        if !self.show_source {
            self.clamp_cursor_out_of_image_blocks(cx);
        }
        cx.notify();
    }

    /// `line` 是否落在某个「正在画图」的 mermaid 块源码区间内。
    fn line_in_mermaid_image(&self, line: usize) -> bool {
        self.mermaid_image_ranges()
            .iter()
            .any(|&(a, b)| line >= a && line <= b)
    }

    /// 图模式下（卡片画着图、源码行未渲染）mermaid 块占用的源码行范围
    /// `(开围栏行, 末行)`。与渲染分支共用 `MermaidState::body` 判定，
    /// 永远与「眼睛看到的」一致：图在画 → 禁区；回退源码 / 源码 tab → 不是。
    fn mermaid_image_ranges(&self) -> Vec<(usize, usize)> {
        if self.mermaid.is_empty() {
            return Vec::new();
        }
        // `planned` 是上一帧规划（帧间判定只关心文本相关范围，光标无关）。
        let rows = &self.planned;
        let mut out = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let Block::Fence { open: true, lang } = &rows[i].block else {
                i += 1;
                continue;
            };
            let is_mermaid = lang.as_deref().is_some_and(|l| l.eq_ignore_ascii_case("mermaid"));
            let open_pi = i;
            i += 1;
            let mut last_code = open_pi;
            while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                last_code = i;
                i += 1;
            }
            let close_pi = if i < rows.len()
                && matches!(rows[i].block, Block::Fence { open: false, .. })
            {
                let c = i;
                i += 1;
                Some(c)
            } else {
                None
            };
            // 非 mermaid / 空块：与渲染分支一致（空块恒显源码）。
            if !is_mermaid || last_code == open_pi {
                continue;
            }
            let block_id = rows[open_pi].line;
            // 与渲染分支同源：mermaid 块不吃「光标停在围栏行」的行级揭示
            //（严格 tab-only），只有全文档源码模式才回源码。
            let fence_revealed = rows[open_pi].plan.revealed
                || close_pi.is_some_and(|c| rows[c].plan.revealed);
            let revealed = fence_revealed && self.show_source;
            if revealed || self.mermaid.is_showing_code(block_id) {
                continue;
            }
            let source = (open_pi + 1..=last_code)
                .map(|pi| self.buffer.line(rows[pi].line).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            if matches!(self.mermaid.body(&source), MermaidBody::Image { .. }) {
                let end = close_pi.unwrap_or(last_code);
                out.push((rows[open_pi].line, rows[end].line));
            }
        }
        out
    }

    /// 光标落进图模式 mermaid 块（隐藏源码区）→ 夹到块外：优先块后一行，
    /// 贴文档尾则块前一行。移动（`after_move`）与编辑（`after_edit`）、
    /// 搜索跳转统一走这里 —— 隐藏源码永远不会被就地编辑。
    fn clamp_cursor_out_of_image_blocks(&mut self, cx: &mut Context<Self>) {
        if self.mermaid.is_empty() {
            return;
        }
        let ranges = self.mermaid_image_ranges();
        if ranges.is_empty() {
            return;
        }
        let cur = self.buffer.cursor();
        let line_count = self.buffer.line_count();
        for (a, b) in ranges {
            if cur.line < a || cur.line > b {
                continue;
            }
            let after = b + 1;
            let target = if after < line_count {
                after
            } else if a > 0 {
                a - 1
            } else {
                // 整篇文档就是这一张图：无处可夹，留在原地（光标在图上
                // 不绘制；on_key 入口的防御分支会吞掉编辑键）。
                return;
            };
            self.edit(cx, |m| m.move_to(target, 0, false));
            return;
        }
    }

    // ---- visual-row movement (B6) ---------------------------------------

    /// Up/Down across *visual* rows (soft-wrapped lines inside a source
    /// line), with a sticky row-relative goal x — guise `move_visual`
    /// parity. Falls back to plain source-line movement when the last
    /// frame's layouts are stale or missing.
    fn move_visual(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        if self.planned_version != self.buffer.text_version() || self.row_layouts.is_empty() {
            self.edit(cx, |m| {
                if down {
                    m.move_down(extend)
                } else {
                    m.move_up(extend)
                }
            });
            self.after_move(cx);
            return;
        }
        let cursor = self.buffer.cursor();
        let fallback = |this: &mut Self, cx: &mut Context<Self>| {
            this.edit(cx, |m| {
                if down {
                    m.move_down(extend)
                } else {
                    m.move_up(extend)
                }
            });
            this.after_move(cx);
        };
        let Some(row) = self.planned.iter().find(|r| r.line == cursor.line) else {
            fallback(self, cx);
            return;
        };
        let Some(info) = self.row_layouts.get(&cursor.line) else {
            fallback(self, cx);
            return;
        };
        let text = self.buffer.line(cursor.line).unwrap_or("").to_string();
        let vis = vis_for_src(&row.plan.segs, byte_for_col(&text, cursor.col));
        let Some(pos) = info.layout.position_for_index(vis.min(info.text_len)) else {
            fallback(self, cx);
            return;
        };
        let origin = info.layout.bounds().origin;
        let line_h = f32::from(info.layout.line_height()).max(1.0);
        let text_h = f32::from(info.layout.bounds().size.height);
        // `position_for_index` has END semantics at wrap boundaries: the
        // caret before a wrapped row's first char reports the PREVIOUS row's
        // end. Detect that (the next byte sits lower) and adopt start
        // semantics — guise's `Row::caret()` does the same with explicit
        // wrap boundaries.
        let pos_y = f32::from(pos.y);
        let next_pos = info
            .layout
            .position_for_index((vis + 1).min(info.text_len));
        let (vrow_f, caret_x_rel) = match next_pos {
            Some(np) if vis < info.text_len && f32::from(np.y) > pos_y => {
                // `vis` starts a new visual row: its row is the next one and
                // its x is the row start.
                (
                    ((f32::from(np.y - origin.y)) / line_h).round(),
                    0.0,
                )
            }
            _ => (
                // Round, not floor: pixel snapping can put a row top at
                // 26.0 with line_height 26.25 and floor would miscount.
                ((f32::from(pos.y - origin.y)) / line_h).round(),
                f32::from(pos.x - origin.x),
            ),
        };
        let vrow = vrow_f as usize;
        let vrow_count = ((text_h / line_h).ceil() as usize).max(1);
        // Sticky goal x, relative to the row's text origin so it survives
        // scroll and inset changes between frames.
        let goal_rel = self.goal_x.unwrap_or(caret_x_rel);
        self.goal_x = Some(goal_rel);

        let target = if down {
            if vrow + 1 < vrow_count {
                Some((cursor.line, vrow + 1))
            } else if cursor.line + 1 < self.buffer.line_count() {
                Some((cursor.line + 1, 0))
            } else {
                None
            }
        } else if vrow > 0 {
            Some((cursor.line, vrow - 1))
        } else if cursor.line > 0 {
            let prev = cursor.line - 1;
            let prev_count = self
                .row_layouts
                .get(&prev)
                .map(|i| {
                    let h = f32::from(i.layout.bounds().size.height);
                    let lh = f32::from(i.layout.line_height()).max(1.0);
                    ((h / lh).ceil() as usize).max(1)
                })
                .unwrap_or(1);
            Some((prev, prev_count.saturating_sub(1)))
        } else {
            None
        };
        let Some((tline, tvrow)) = target else {
            self.edit(cx, |m| {
                if down {
                    m.doc_end(extend)
                } else {
                    m.doc_start(extend)
                }
            });
            self.after_move(cx);
            return;
        };
        let col = if let Some(tinfo) = self.row_layouts.get(&tline) {
            let torigin = tinfo.layout.bounds().origin;
            let tline_h = f32::from(tinfo.layout.line_height()).max(1.0);
            let ty = torigin.y + px((tvrow as f32 + 0.5) * tline_h);
            let tx = torigin.x + px(goal_rel);
            let vis = match tinfo.layout.index_for_position(gpui::point(tx, ty)) {
                Ok(ix) | Err(ix) => ix.min(tinfo.text_len),
            };
            let trow = self.planned.iter().find(|r| r.line == tline);
            let ttext = self.buffer.line(tline).unwrap_or("");
            let src = trow.map_or(vis, |r| src_for_vis(&r.plan.segs, vis));
            col_for_byte(ttext, src)
        } else {
            usize::MAX // blank rows: line end
        };
        self.edit(cx, |m| m.move_to(tline, col, extend));
        self.after_move(cx);
    }

    // ---- mouse ----------------------------------------------------------

    /// Hit raw visible byte `raw` on `line`: link activation first, then
    /// cursor placement / word / line selection.
    #[allow(clippy::too_many_arguments)]
    pub fn on_line_mouse_down(
        &mut self,
        line: usize,
        raw: usize,
        click_count: usize,
        shift: bool,
        platform: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.goal_x = None;
        // 图模式 mermaid 块内不渲染文本行：真实 LiveLine 命中不该出现，
        // 防御性兜底——落进禁区直接忽略，光标留在块外。
        if self.line_in_mermaid_image(line) {
            cx.stop_propagation();
            return;
        }
        let Some(src_line) = self.buffer.line(line).map(str::to_string) else {
            return;
        };
        // Snapshot the plan mapping before the mutable edit borrow.
        let (vis_len, segs, link) = match self.planned.iter().find(|r| r.line == line) {
            Some(row) => {
                let vis = raw.min(row.plan.visible.len());
                let src = src_for_vis(&row.plan.segs, vis);
                (row.plan.visible.len(), row.plan.segs.clone(), row.plan.link_at(src).map(str::to_string))
            }
            None => return,
        };
        let _ = vis_len;
        let vis = raw.min(vis_len);
        let src = src_for_vis(&segs, vis);
        if (platform || self.read_only) && let Some(target) = link {
            cx.emit(LiveViewEvent::LinkClick(target));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let col = col_for_byte(&src_line, src);
        self.edit(cx, |m| {
            // `move_to` clamps, and `extend` anchors a selection even when
            // none exists yet (shift-click starts a selection like guise).
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    m.select_word();
                } else if click_count > 2 {
                    m.select_line();
                }
            }
        });
        self.selecting = click_count <= 1;
        // Claim the event so the root dead-zone fallback doesn't also run.
        cx.stop_propagation();
    }

    pub fn on_line_drag(&mut self, line: usize, raw: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let (vis_len, segs, src_line) = match self.planned.iter().find(|r| r.line == line) {
            Some(row) => (
                row.plan.visible.len(),
                row.plan.segs.clone(),
                self.buffer.line(line).map(str::to_string).unwrap_or_default(),
            ),
            None => return,
        };
        let vis = raw.min(vis_len);
        let col = col_for_byte(&src_line, src_for_vis(&segs, vis));
        self.edit(cx, |m| m.move_to(line, col, true));
        // Claim the drag so the dead-zone fallback doesn't also run.
        cx.stop_propagation();
    }

    pub fn on_line_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    pub fn toggle_task_at(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(text) = self.buffer.line(line).map(str::to_string) else {
            return;
        };
        let changed = self.edit(cx, |m| toggle_task(m, &text, line));
        if changed {
            self.after_edit(cx);
        }
    }

    // ---- keyboard --------------------------------------------------------

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let key = ks.key.as_str();
        if key != "up" && key != "down" {
            self.goal_x = None;
        }
        let shift = ks.modifiers.shift;
        let platform = ks.modifiers.platform;
        let control = ks.modifiers.control;
        let alt = ks.modifiers.alt;
        // 查找面板输入框聚焦时正文快捷键全部让行（`SearchPanel` 自带
        // Enter/Escape/Tab action），仅保留 Cmd+F / Cmd+G / Esc。
        if self.search_panel_focused(window, cx) && !matches!(
            (key, platform),
            ("f", true) | ("g", true) | ("escape", _)
        ) {
            return;
        }

        // 防御：光标若经非常规路径落入图模式 mermaid 块（隐藏源码区），
        // 先夹出块外并吞掉本键——源码可见才可编辑，编辑必须经「源码」tab
        //（严格 tab-only，无任何自动显示源码的行为）。
        if !platform && !control && !alt && self.line_in_mermaid_image(self.buffer.cursor().line) {
            self.clamp_cursor_out_of_image_blocks(cx);
            self.after_move(cx);
            return;
        }

        // Slash `/` command menu (Typora/Notion parity): plain Up/Down
        // cycle, Enter/Tab apply, Esc suppresses until the query changes.
        // Anything else falls through so typing filters the menu live.
        if !platform && !control && !alt
            && self.buffer.selection().is_none()
            && !self.search_panel_focused(window, cx)
            && let Some((query, items)) = self.slash_menu()
        {
            match key {
                "up" => {
                    self.slash_index =
                        (self.slash_index + items.len() - 1) % items.len();
                    self.after_move(cx);
                    return;
                }
                "down" => {
                    self.slash_index = (self.slash_index + 1) % items.len();
                    self.after_move(cx);
                    return;
                }
                "enter" | "tab" => {
                    let ix = self.slash_index.min(items.len() - 1);
                    self.apply_slash(items[ix], cx);
                    return;
                }
                "escape" => {
                    self.slash_suppress = Some(query);
                    self.slash_index = 0;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        match key {
            "left" => {
                self.edit(cx, |m| {
                    if platform {
                        m.home(shift)
                    } else if alt {
                        m.word_left(shift)
                    } else {
                        m.move_left(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "right" => {
                self.edit(cx, |m| {
                    if platform {
                        m.end(shift)
                    } else if alt {
                        m.word_right(shift)
                    } else {
                        m.move_right(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "up" => {
                if platform {
                    self.edit(cx, |m| m.doc_start(shift));
                    self.after_move(cx);
                } else {
                    self.move_visual(false, shift, cx);
                }
                return;
            }
            "down" => {
                if platform {
                    self.edit(cx, |m| m.doc_end(shift));
                    self.after_move(cx);
                } else {
                    self.move_visual(true, shift, cx);
                }
                return;
            }
            "home" => {
                self.edit(cx, |m| {
                    if platform {
                        m.doc_start(shift)
                    } else {
                        m.home(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "end" => {
                self.edit(cx, |m| {
                    if platform {
                        m.doc_end(shift)
                    } else {
                        m.end(shift)
                    }
                });
                self.after_move(cx);
                return;
            }
            "backspace" => {
                if self.read_only {
                    return;
                }
                // Marker strip first (plain backspace, no selection).
                let cur = self.buffer.cursor();
                let sel_none = self.buffer.selection().is_none();
                if sel_none && !platform && !alt {
                    let paired = self.edit(cx, |m| m.delete_pair());
                    if paired {
                        self.after_edit(cx);
                        return;
                    }
                    if let Some(text) = self.buffer.line(cur.line).map(str::to_string) {
                        let stripped = self.edit(cx, |m| backspace_marker(m, &text));
                        if stripped {
                            self.after_edit(cx);
                            return;
                        }
                    }
                }
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        m.delete_selection()
                    } else if platform {
                        m.home(true);
                        m.delete_selection()
                    } else if alt {
                        m.word_left(true);
                        m.delete_selection()
                    } else {
                        m.backspace()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "delete" => {
                if self.read_only {
                    return;
                }
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        m.delete_selection()
                    } else if platform {
                        m.end(true);
                        m.delete_selection()
                    } else if alt {
                        m.word_right(true);
                        m.delete_selection()
                    } else {
                        m.delete()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "enter" => {
                if platform {
                    if self.read_only {
                        return;
                    }
                    let cur = self.buffer.cursor();
                    if let Some(text) = self.buffer.line(cur.line).map(str::to_string) {
                        let toggled = self.edit(cx, |m| toggle_task(m, &text, cur.line));
                        if toggled {
                            self.after_edit(cx);
                        }
                    }
                    return;
                }
                if self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let text = self.buffer.line(cur.line).map(str::to_string).unwrap_or_default();
                let planned_block = self
                    .planned
                    .iter()
                    .find(|r| r.line == cur.line)
                    .map(|r| r.block.clone());
                // Typora Enter inside a table: empty row below (no selection).
                if self.buffer.selection().is_none()
                    && matches!(planned_block, Some(Block::Table))
                    && self.edit(cx, table_insert_row_below)
                {
                    self.after_edit(cx);
                    return;
                }
                let in_code = matches!(
                    planned_block,
                    Some(Block::CodeLine | Block::Fence { .. } | Block::FrontMatter)
                );
                self.edit(cx, |m| {
                    on_enter(m, &text, in_code);
                });
                self.after_edit(cx);
                return;
            }
            "tab" => {
                if platform || self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let text = self.buffer.line(cur.line).map(str::to_string).unwrap_or_default();
                let tab_size = self.tab_size;
                // Typora Tab inside a table: grid navigation (no selection).
                if self.buffer.selection().is_none()
                    && self
                        .planned
                        .iter()
                        .find(|r| r.line == cur.line)
                        .is_some_and(|r| matches!(r.block, Block::Table))
                {
                    let grid: HashSet<usize> = self
                        .planned
                        .iter()
                        .filter(|r| matches!(r.block, Block::Table))
                        .map(|r| r.line)
                        .collect();
                    let forward = !shift;
                    self.edit(cx, |m| {
                        table_nav(m, forward, &|l| grid.contains(&l));
                    });
                    self.after_edit(cx);
                    return;
                }
                // Code/fence/frontmatter keep literal tabs.
                if !matches!(
                    classify_alone(&text),
                    Block::CodeLine | Block::Fence { .. } | Block::FrontMatter
                ) {
                    self.edit(cx, |m| on_tab(m, &text, shift, tab_size));
                    self.after_edit(cx);
                } else if !shift {
                    self.edit(cx, |m| {
                        m.tab();
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "escape" => {
                if let Some(panel) = self.search_panel.clone()
                    && panel.read(cx).is_open()
                {
                    panel.update(cx, |p, cx| p.hide(window, cx));
                    cx.stop_propagation();
                    return;
                }
                // 表格里按 Esc：退出表格（Notion / codemirror-markdown-tables
                // 同款）——光标移到表格后一行，不动任何文本。选区仍优先处理，
                // 这样「先取消选区、再退出表格」是两段式，符合直觉。
                if self.buffer.selection().is_none()
                    && let Some(exit) = self.table_exit_line()
                {
                    self.edit(cx, |m| m.move_to(exit, 0, false));
                    self.after_move(cx);
                    return;
                }
                if self.buffer.selection().is_some() {
                    self.edit(cx, |m| m.clear_selection());
                    cx.notify();
                }
                return;
            }
            "a" if platform => {
                self.edit(cx, |m| m.select_all());
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "b" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("**");
                        } else {
                            toggle_wrap(m, "**");
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "i" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("*");
                        } else {
                            toggle_wrap(m, "*");
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "k" if platform => {
                if !self.read_only {
                    let literal = self.cursor_is_source_only();
                    self.edit(cx, |m| {
                        if literal {
                            m.insert("[]()");
                        } else {
                            insert_link(m);
                        }
                    });
                    self.after_edit(cx);
                }
                return;
            }
            "c" if platform => {
                if let Some(t) = self.selected_source_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t));
                }
                cx.stop_propagation();
                return;
            }
            "x" if platform => {
                if self.read_only {
                    if let Some(t) = self.selected_source_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                    }
                    cx.stop_propagation();
                    return;
                }
                let cut = self.edit(cx, |m| m.cut());
                match cut {
                    Some(t) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                        self.after_edit(cx);
                    }
                    None => cx.stop_propagation(),
                }
                return;
            }
            "v" if platform => {
                if self.read_only {
                    cx.stop_propagation();
                    return;
                }
                if let Some(item) = cx.read_from_clipboard() {
                    // 图片优先：截图/图片字节或 Finder 拷贝的图片文件落盘插入。
                    if self.paste_images(&item, cx) {
                        self.after_edit(cx);
                        return;
                    }
                    if let Some(text) = item.text() {
                        let text = text.replace('\r', "");
                        if !text.is_empty() {
                            self.edit(cx, |m| m.insert(&text));
                            self.after_edit(cx);
                            return;
                        }
                    }
                }
                cx.stop_propagation();
                return;
            }
            "z" if platform => {
                if self.read_only {
                    cx.stop_propagation();
                    return;
                }
                let changed = self.edit(cx, |m| {
                    if shift {
                        m.redo()
                    } else {
                        m.undo()
                    }
                });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                return;
            }
            "f" if platform => {
                // 代码编辑器同款查找面板（只读也允许查找，替换行自动隐藏）。
                self.open_search(window, cx);
                cx.stop_propagation();
                return;
            }
            "g" if platform => {
                if self.search_panel.is_none() {
                    self.open_search(window, cx);
                    if shift
                        && let Some(panel) = self.search_panel.clone()
                    {
                        panel.update(cx, |p, cx| p.jump(-1, window, cx));
                    }
                } else if let Some(panel) = self.search_panel.clone() {
                    panel.update(cx, |p, cx| p.jump(if shift { -1 } else { 1 }, window, cx));
                }
                cx.stop_propagation();
                return;
            }
            "/" if platform => {
                // Typora 源码模式切换（与状态条"源码"芯片同一开关）。
                self.toggle_show_source(cx);
                cx.stop_propagation();
                return;
            }
            "o" if platform && shift => {
                // Typora 大纲侧栏显隐（与状态条按钮同一开关）。
                self.show_outline = !self.show_outline;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            _ => {}
        }
        // Printable input.
        if !self.read_only
            && !platform
            && !control
            && !self.search_panel_focused(window, cx)
            && let Some(text) = ks.key_char.as_deref().filter(|t| !t.is_empty())
        {
            let mut cs = text.chars();
            if let (Some(ch), None) = (cs.next(), cs.next())
                && self.try_autopair(ch, cx)
            {
                self.after_edit(cx);
                return;
            }
            self.edit(cx, |m| {
                m.insert(text);
            });
            self.after_edit(cx);
        }
    }

    /// Selected text as *source* markdown (markers included), for copy.
    fn selected_source_text(&self) -> Option<String> {
        let (s, e) = self.buffer.selection()?;
        let mut out = String::new();
        for line in s.line..=e.line {
            let text = self.buffer.line(line)?;
            let chars: Vec<char> = text.chars().collect();
            let (a, b) = if line == s.line && line == e.line {
                (s.col.min(chars.len()), e.col.min(chars.len()))
            } else if line == s.line {
                (s.col.min(chars.len()), chars.len())
            } else if line == e.line {
                (0, e.col.min(chars.len()))
            } else {
                (0, chars.len())
            };
            if line > s.line {
                out.push('\n');
            }
            out.extend(chars[a..b].iter());
        }
        Some(out)
    }

    /// Copy-button feedback for fenced code cards (mirrors `Markdown`).
    pub fn mark_code_block_copied(&mut self, id: usize, cx: &mut Context<Self>) {
        self.copied_code_blocks.insert(id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |this, cx| {
                this.copied_code_blocks.remove(&id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Fingerprint of the style inputs baked into highlighted runs: a change
/// (theme/appearance switch, font size change) must invalidate the
/// `HighlightCache`, which otherwise returns stale-colored runs.
fn style_fingerprint(style: &MarkdownStyle, is_dark: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    is_dark.hash(&mut h);
    let b = &style.base_text_style;
    b.color.h.to_bits().hash(&mut h);
    b.color.s.to_bits().hash(&mut h);
    b.color.l.to_bits().hash(&mut h);
    b.color.a.to_bits().hash(&mut h);
    b.font_family.hash(&mut h);
    b.font_weight.0.to_bits().hash(&mut h);
    b.font_style.hash(&mut h);
    f32::from(b.font_size.to_pixels(px(16.))).to_bits().hash(&mut h);
    // The code-block mono style feeds `row_base_style` for code runs.
    style.inline_code.font_family.hash(&mut h);
    if let Some(fs) = &style.inline_code.font_size {
        f32::from(fs.to_pixels(px(16.))).to_bits().hash(&mut h);
    }
    h.finish()
}

/// Callout accent per kind (same mapping as the read-only preview).
fn callout_accent(kind: CalloutKind, theme: &crate::theme::AppTheme) -> Hsla {
    let ui = &theme.ui;
    match kind {
        CalloutKind::Note => ui.accent,
        CalloutKind::Tip => ui.success,
        CalloutKind::Important => gpui::rgb(0x8250DF).into(),
        CalloutKind::Warning => ui.warning,
        CalloutKind::Caution => ui.error,
    }
}

/// Outline entries `(source line, level, title)` in document order.
fn outline_entries(rows: &[PlannedRow]) -> Vec<(usize, u8, String)> {
    rows
        .iter()
        .filter_map(|r| match &r.block {
            Block::Heading { level, .. } => Some((r.line, *level, outline_title(&r.plan.visible))),
            _ => None,
        })
        .collect()
}

/// Outline display title: single line, max 42 chars.
fn outline_title(visible: &str) -> String {
    let one = visible
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one.is_empty() {
        return "(空标题)".to_string();
    }
    if one.chars().count() > 42 {
        format!("{}…", one.chars().take(42).collect::<String>())
    } else {
        one
    }
}

/// If the whole line is a single `![alt](url)` image, return `(alt, url)`.
fn parse_image_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let inner = t.strip_prefix("![")?;
    let close = inner.find("](")?;
    let alt = inner[..close].to_string();
    let rest = &inner[close + 2..];
    let end = rest.find(')')?;
    let url = rest[..end].split_whitespace().next().unwrap_or("").to_string();
    if url.is_empty() || !rest[end + 1..].trim().is_empty() {
        return None;
    }
    Some((alt, url))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellAlign {
    Left,
    Center,
    Right,
    None,
}

fn align_of(cell: &str) -> CellAlign {
    let c = cell.trim();
    let l = c.starts_with(':');
    let r = c.ends_with(':');
    match (l, r) {
        (true, true) => CellAlign::Center,
        (true, false) => CellAlign::Left,
        (false, true) => CellAlign::Right,
        _ => CellAlign::None,
    }
}

impl SearchTarget for LiveMarkdownView {
    fn search_text(&self) -> Rope {
        Rope::from(self.buffer.text())
    }

    fn search_prefill(&self) -> Rope {
        let first: String = self
            .selected_source_text()
            .as_deref()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        Rope::from(first)
    }

    fn search_caret(&self) -> usize {
        self.search_caret_offset()
    }

    fn anchor_offset(&self) -> Option<usize> {
        Some(self.search_caret_offset())
    }

    fn reveal_match(&mut self, range: std::ops::Range<usize>, _w: &mut Window, cx: &mut Context<Self>) {
        let lines = self.buffer.lines().to_vec();
        let s = pos_of_offset(&lines, range.start);
        let e = pos_of_offset(&lines, range.end);
        self.edit(cx, |m| {
            m.move_to(s.line, s.col, false);
            m.move_to(e.line, e.col, true);
        });
        // 搜索跳转可能落进图模式块（隐藏源码区）：夹到块外。
        self.clamp_cursor_out_of_image_blocks(cx);
    }

    fn replace_match(
        &mut self,
        range: std::ops::Range<usize>,
        text: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let lines = self.buffer.lines().to_vec();
        let s = pos_of_offset(&lines, range.start);
        let e = pos_of_offset(&lines, range.end);
        self.edit(cx, |m| m.replace_range(s, e, text));
        // 替换后的落点可能在图模式块内：夹到块外再发射变更。
        self.clamp_cursor_out_of_image_blocks(cx);
        self.after_edit(cx);
    }

    fn replace_all_matches(
        &mut self,
        pairs: &[(std::ops::Range<usize>, String)],
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let lines = self.buffer.lines().to_vec();
        let ranges: Vec<(Pos, Pos, String)> = pairs
            .iter()
            .map(|(r, t)| {
                (
                    pos_of_offset(&lines, r.start),
                    pos_of_offset(&lines, r.end),
                    t.clone(),
                )
            })
            .collect();
        if ranges.is_empty() {
            return;
        }
        self.edit(cx, |m| m.replace_ranges_each(&ranges));
        self.after_edit(cx);
    }

    fn scroll_to_offset(
        &mut self,
        _offset: usize,
        _direction: Option<crate::components::input::movement::MoveDirection>,
        _cx: &mut Context<Self>,
    ) {
        // `edit` 已置 `scroll_to_cursor`（光标落在替换处），与旧自实现行为一致。
    }

    fn search_replaceable(&self) -> bool {
        !self.read_only
    }

    fn target_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Focusable for LiveMarkdownView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Per-row painted data, computed once per frame before grouping.
struct RowPaint {
    runs: Vec<TextRun>,
    caret_vis: Option<usize>,
    /// Caret height matching this row's real line height (C4: was
    /// hardcoded per kind and drifted from the rendered metrics).
    caret_h: Pixels,
    caret_block_width: Pixels,
    caret_block_text: Option<ShapedLine>,
}

/// A source line's last-frame text layout, recorded by `LiveLine::paint`.
#[derive(Clone)]
pub(crate) struct RowLayoutInfo {
    pub layout: gpui::TextLayout,
    pub text_len: usize,
}

/// 行虚拟化调参：超过该 group 数即只建视口内元素（`OVERSCAN_GROUPS`
/// 上下各多建一点 cover 滚动）。
const VIRTUALIZE_GROUPS: usize = 200;
const OVERSCAN_GROUPS: usize = 12;
/// 窗口迟滞：视口移动不足该 group 数时复用上帧窗口，避免 1-group 步进
/// 带动顶部占位逐帧跳变（底部回滚上下抖动的直接观感）。
const WIN_HYST_GROUPS: usize = 6;
/// 首帧视口未知时预建的 group 数（滚动后按真实视口修正）。
const FIRST_WINDOW_GROUPS: usize = 80;
/// `group_heights` 每帧只保留当前窗口 ± 该余量的 group，超出即淘汰。
/// 保证跨滚动滚过大量 group 时缓存大小恒定（与文档规模无关）；淘汰的项
/// 读取时回退到估算并由下一帧实测自修正，因此是安全的（见 M3）。
const GROUP_HEIGHT_KEEP_MARGIN: usize = 128;
/// 未实测 group 的高度估算：单行高 + 各容器 chrome。
const EST_LINE_H: f32 = 26.0;
/// Pitch 回填时代的默认行高：含组间 margin（标题 pitch 62.5、空白 38~58、
/// 段落 40 左右），只在尚无任一实测样本的首帧使用。
const EST_PITCH_H: f32 = 40.0;

/// A render group: consecutive rows sharing one chrome container.
enum Group {
    One(usize),
    Fence {
        open: usize,
        code: Vec<usize>,
        close: Option<usize>,
        lang: Option<String>,
    },
    /// Consecutive plain-paragraph rows rendered as ONE block: the preview
    /// joins them into a single paragraph (soft-wrap + hard breaks), so
    /// the live editor must not put inter-row margins between them (C2).
    Para(Vec<usize>),
    Quote(Vec<usize>),
    Table(Vec<usize>),
}

impl Group {
    /// Source-row count (height-estimate input for virtualization).
    fn row_count(&self) -> usize {
        match self {
            Group::One(_) => 1,
            Group::Fence { code, .. } => code.len() + 2,
            Group::Para(v) | Group::Quote(v) | Group::Table(v) => v.len(),
        }
    }

    /// Calibration bucket (20): estimate = rows x rate[bucket], with the
    /// container chrome amortized into the rate. Learning chrome
    /// separately proved unidentifiable (para chrome guessed 20 vs true
    /// ~2 pushed the rate into its clamp floor and biased every unmeasured
    /// para +6px); mixing headings (46px) with blanks (26px) in one bucket
    /// swung the total whenever the window mix changed (bottom-up jitter).
    /// Blanks are bimodal by neighbour (38 after heading vs 58 after para
    /// in the pitch model, margins included), so bucket 9 is split by
    /// predecessor: 10 + prev_base (11 = after blank/unknown-first, ...).
    /// Each sub-bucket is unimodal; median then resists the margin-less
    /// trailing 26px outliers.
    fn cal_bucket(&self, rows: &[PlannedRow]) -> u8 {
        match self {
            Group::Para(_) => 1,
            Group::Quote(_) => 3,
            Group::Table(_) => 6,
            Group::Fence { .. } => 4,
            Group::One(pi) => match rows.get(*pi).map(|r| &r.plan.kind) {
                Some(RowKind::Heading(_)) => 0,
                Some(RowKind::Paragraph) => 1,
                Some(RowKind::Bullet { .. } | RowKind::Ordered { .. } | RowKind::Task { .. }) => 2,
                Some(RowKind::Quote { .. }) => 3,
                Some(RowKind::Fence { .. } | RowKind::Code { .. }) => 4,
                Some(RowKind::Rule) => 5,
                Some(RowKind::Table) => 6,
                Some(RowKind::Math | RowKind::MathDelim { .. }) => 7,
                Some(RowKind::Setext | RowKind::FrontMatter) => 8,
                Some(RowKind::Blank) | None => 9,
            },
        }
    }

    /// (first, last) source line covered (click mapping for virtualization).
    fn line_range(&self, rows: &[PlannedRow]) -> (usize, usize) {
        match self {
            Group::One(pi) => {
                let l = rows.get(*pi).map(|r| r.line).unwrap_or(0);
                (l, l)
            }
            Group::Fence {
                open, code, close, ..
            } => {
                let a = rows.get(*open).map(|r| r.line).unwrap_or(0);
                let last_pi = close.or_else(|| code.last().copied()).unwrap_or(*open);
                let b = rows.get(last_pi).map(|r| r.line).unwrap_or(a);
                (a, b)
            }
            Group::Para(v) | Group::Quote(v) | Group::Table(v) => {
                let a = v
                    .first()
                    .and_then(|pi| rows.get(*pi))
                    .map(|r| r.line)
                    .unwrap_or(0);
                let b = v
                    .last()
                    .and_then(|pi| rows.get(*pi))
                    .map(|r| r.line)
                    .unwrap_or(a);
                (a, b)
            }
        }
    }
}

impl LiveMarkdownView {
    /// This frame's group window for row virtualization (`None` = small doc,
    /// render everything on the legacy path).
    ///
    /// Large markdown docs build thousands of row elements per frame; each
    /// costs layout + paint on the UI thread (~0.1ms/row in debug), so every
    /// blink tick / cursor move / theme refresh freezes the app. Only the
    /// viewport window (+ overscan) becomes elements; spacers preserve total
    /// scroll height. Group granularity keeps code cards, quotes and tables
    /// whole. Heights self-correct: measured child bounds feed back into
    /// `group_heights`, estimates cover the rest.
    fn virtual_window(
        &mut self,
        ginfo: &[(usize, u8, (usize, usize))],
        cursor_line: usize,
        text_version: u64,
        want_caret: bool,
    ) -> Option<(usize, usize, f32, f32)> {
        let g = ginfo.len();
        if g <= VIRTUALIZE_GROUPS || g == 0 {
            self.virt_prefix.clear();
            self.virt_group_lines.clear();
            self.virt_lo = 0;
            self.virt_hi = 0;
            self.virt_top_off = 0;
            self.last_window = None;
            self.last_window_counts.clear();
            return None;
        }
        // Text edits shift group indices, but measured heights are kept:
        // dropping the whole map on every keystroke reverts to estimates
        // for a frame (scroll-height flicker + re-measure churn). Stale
        // entries self-correct below via measured child bounds; only a
        // viewport-width change (rewrap) needs a full clear.
        // `heights_version` still tracks the last text revision for tests.
        let text_changed = text_version != self.heights_version;
        if text_changed {
            self.heights_version = text_version;
        }
        // 上帧几何快照（锚定补偿用）：prefix 逐帧重算会漂，`lo` 越大顶部占位
        // 跳变越大；用同一 anchor 的新旧差修正 scroll offset，视口内容不动。
        let old_prefix = self.virt_prefix.clone();
        let old_window = self.last_window;
        let old_same_shape = old_prefix.len() == g + 1;
        // Wrapping follows viewport width: re-measure after resize.
        let vp = self.scroll.bounds();
        let vp_w = f32::from(vp.size.width);
        if vp_w > 0.0 && (self.last_vp_width - vp_w).abs() > 1.0 {
            self.group_heights.clear();
        }
        self.last_vp_width = vp_w;
        // Feed back last frame's measured heights. Children are
        // [top spacer?] + window groups (each 1 child, except revealed
        // tables which expand to N row children) + [bottom spacer?].
        // The old code assumed 1 group = 1 child, so a revealed table
        // shifted every later group's height onto the wrong index and the
        // prefix/total jumped each frame (bottom-up scroll抖动).
        if let Some((plo, phi)) = self.last_window
            && phi < g
            && self.last_window_counts.len() == phi - plo + 1
        {
            let counts = self.last_window_counts.clone();
            let top_off = usize::from(plo > 0);
            let bot_off = usize::from(phi + 1 < g);
            let expected: usize = counts.iter().sum::<usize>() + top_off + bot_off;
            let mut count = 0usize;
            while count < expected && self.scroll.bounds_for_item(count).is_some() {
                count += 1;
            }
            if count == expected {
                // Pitch-based measurement: group height = distance from its
                // first child's top to the next child's top. Unlike
                // border-box sums this captures the inter-element margins
                // the flex column adds between rows/cards; missing them
                // underestimated the total by hundreds of px and the
                // scrollbar thumb lied about position/size.
                let mut tops: Vec<f32> = Vec::with_capacity(expected + 1);
                for ix in 0..expected {
                    // Guaranteed `Some` by the count check above.
                    match self.scroll.bounds_for_item(ix) {
                        Some(b) => tops.push(f32::from(b.origin.y)),
                        None => break,
                    }
                }
                if tops.len() == expected {
                    let last_bottom = self
                        .scroll
                        .bounds_for_item(expected - 1)
                        .map(|b| f32::from(b.origin.y) + f32::from(b.size.height));
                    let mut first_of = top_off;
                    for (k, c) in counts.iter().enumerate() {
                        let h = if k + 1 < counts.len() || bot_off == 1 {
                            // Next top exists: a group child or the bottom
                            // spacer (its top is the content end, so the
                            // last group's trailing margin is included).
                            tops[first_of + c] - tops[first_of]
                        } else if let Some(bb) = last_bottom {
                            // At the very end: no bottom spacer, and the
                            // trailing margin sits outside content_size
                            // (matches scrollbar max semantics).
                            bb - tops[first_of]
                        } else {
                            first_of += *c;
                            continue;
                        };
                        if h > 0.0 {
                            self.group_heights.insert(plo + k, h);
                        }
                        first_of += *c;
                    }
                }
            }
        }
        // Prefix sums over measured heights, per-row-kind calibrated
        // estimates for the rest (see `Group::cal_bucket`). Rates are
        // derived from the currently measured groups every frame, so no
        // persistent state can drift; unknown buckets fall back to the
        // global mean, then `EST_LINE_H`.
        // Per-bucket median pitch: each measured group contributes one
        // sample `v = h / rows`. The mean proved fragile: two trailing
        // margin-less blank groups (pitch 26) dragged the blank-bucket mean
        // 48.0 -> 46.9, swinging the total by ~600px on next measure.
        // Median over >=4 groups is immune to those outliers and to the
        // 38/58 blank phase mix (median ~= 48, errors cancel in total).
        fn median(vals: &mut [f32]) -> f32 {
            vals.sort_by(|a, b| a.total_cmp(b));
            let n = vals.len();
            if n % 2 == 1 {
                vals[n / 2]
            } else {
                (vals[n / 2 - 1] + vals[n / 2]) * 0.5
            }
        }
        let mut kind_vals: Vec<Vec<f32>> = vec![Vec::new(); 20];
        let mut all_vals: Vec<f32> = Vec::new();
        for (i, (r, b, _)) in ginfo.iter().enumerate() {
            if let Some(h) = self.group_heights.get(&i) {
                // The last group is measured border-box (no next top, no
                // trailing margin) and trailing blanks collapse their
                // margins (26 vs 58 mid-doc): neither is a pitch sample,
                // feeding them in would bias the shared rates.
                if i + 1 >= g && g > 1 {
                    continue;
                }
                let v = (*h / (*r as f32).max(1.0)).clamp(8.0, 96.0);
                kind_vals[*b as usize].push(v);
                all_vals.push(v);
            }
        }
        // Fallback chain: bucket median -> global median -> pitch default.
        // `EST_LINE_H` is the border-box-era row height and no longer a
        // valid pitch fallback (it misses ~12px of margins per group).
        let global_rate = if all_vals.is_empty() {
            EST_PITCH_H
        } else {
            median(&mut all_vals.clone()).clamp(8.0, 96.0)
        };
        let mut kind_rate = [global_rate; 20];
        for k in 0..20 {
            if kind_vals[k].len() >= 4 {
                kind_rate[k] = median(&mut kind_vals[k]).clamp(8.0, 96.0);
            }
        }
        // Trailing-blank prior: the last two groups' blanks render
        // without block margins (measured 26 = line height vs 38/58
        // mid-doc pitch). Estimating them at pitch rates injects ~32px
        // per group of one-time settling jump on first bottom arrival.
        let is_trailing_blank = |i: usize| -> bool {
            if i + 2 < g {
                return false;
            }
            matches!(
                ginfo.get(i).map(|(_, b, _)| *b),
                Some(10..=19)
            )
        };
        let mut prefix = Vec::with_capacity(g + 1);
        prefix.push(0.0f32);
        for (i, (rows, b, _)) in ginfo.iter().enumerate() {
            let h = self.group_heights.get(&i).copied().unwrap_or_else(|| {
                if is_trailing_blank(i) {
                    (*rows as f32) * EST_LINE_H
                } else {
                    (*rows as f32) * kind_rate[*b as usize]
                }
            });
            prefix.push(prefix[i] + h);
        }
        // Viewport window in content pixels (scroll-anchored).
        let vp_h = f32::from(vp.size.height);
        let mut off_y = -f32::from(self.scroll.offset().y);
        // 锚定补偿：锚点取旧空间下视口顶部 group（不是窗口顶部，也不是
        // 内容末端——两者都会把锚点下方新实测 group 的修正漏成视口位移），
        // 组内按比例跟随；视口下方（含底部）的修正由底部占位吸收，视口内容
        // 保持不动，用户滚轮位移被完整保留。
        if !text_changed && !want_caret && vp_h > 0.0 && old_same_shape {
            let mut a = g - 1;
            for i in 0..g {
                if old_prefix[i + 1] > off_y {
                    a = i;
                    break;
                }
                a = i;
            }
            let h_old = (old_prefix[a + 1] - old_prefix[a]).max(1.0);
            let frac = ((off_y - old_prefix[a]) / h_old).clamp(0.0, 1.0);
            let h_new = (prefix[a + 1] - prefix[a]).max(1.0);
            let want = prefix[a] + frac * h_new;
            if (want - off_y).abs() > 0.01 && (want - off_y).abs() < 4000.0 {
                let cur = self.scroll.offset();
                self.scroll.set_offset(gpui::point(cur.x, -px(want)));
                off_y = want;
            }
        }
        let (mut lo, mut hi) = if vp_h <= 0.0 {
            (0, FIRST_WINDOW_GROUPS.min(g - 1))
        } else {
            let mut lo0 = 0;
            for i in 0..g {
                if prefix[i + 1] > off_y {
                    lo0 = i;
                    break;
                }
                lo0 = i;
            }
            let mut hi0 = g - 1;
            for i in (0..g).rev() {
                if prefix[i] < off_y + vp_h {
                    hi0 = i;
                    break;
                }
                hi0 = i;
            }
            (
                lo0.saturating_sub(OVERSCAN_GROUPS),
                (hi0 + OVERSCAN_GROUPS).min(g - 1),
            )
        };
        // 窗口迟滞：小幅滚动复用上帧窗口（overscan 覆盖视口），避免 lo/hi
        // 逐 group 步进带动顶部占位跳变；该跳变是底部回滚上下抖动的直接观感。
        if !text_changed && !want_caret && old_same_shape {
            if let Some((olo, ohi)) = old_window
                && ohi < g
                && olo <= ohi
                && lo.abs_diff(olo) <= WIN_HYST_GROUPS
                && hi.abs_diff(ohi) <= WIN_HYST_GROUPS
                && prefix[olo] <= off_y + 1.0
                && off_y + vp_h <= prefix[ohi + 1] + 1.0
            {
                lo = olo;
                hi = ohi;
            }
        }
        // Caret edits/moves must land inside the window (scroll_to_item
        // consumes the mapped child index in the same frame's prepaint).
        if want_caret {
            let mut gc = g - 1;
            for (i, (_, _, (a, b))) in ginfo.iter().enumerate() {
                if *a <= cursor_line && cursor_line <= *b {
                    gc = i;
                    break;
                }
            }
            if gc < lo || gc > hi {
                lo = gc.saturating_sub(OVERSCAN_GROUPS);
                hi = (gc + OVERSCAN_GROUPS + 64).min(g - 1);
            }
        }
        self.virt_prefix = prefix;
        self.virt_group_lines = ginfo.iter().map(|(_, _, lr)| *lr).collect();
        self.virt_lo = lo;
        self.virt_hi = hi;
        self.virt_top_off = usize::from(lo > 0);
        self.last_window = Some((lo, hi));
        self.prune_group_heights(lo, hi);
        let total = self.virt_prefix[g];
        Some((
            lo,
            hi,
            self.virt_prefix[lo],
            total - self.virt_prefix[hi + 1],
        ))
    }

    /// 滚动窗口化：只保留当前窗口 `lo..=hi` ± 余量内的实测高度，其余淘汰。
    /// 淘汰的 group 读取时回退到估算（`prefix` 与速率采样都用 `unwrap_or_else`
    /// 估算兜底），下一帧进入窗口时实测自修正；因此保留内存从「随滚动遍历
    /// 整个大文档持续增长」变为与文档规模无关的常数窗口（M3）。
    fn prune_group_heights(&mut self, lo: usize, hi: usize) {
        self.group_heights.retain(|&gi, _| {
            hi < lo
                || (gi >= lo.saturating_sub(GROUP_HEIGHT_KEEP_MARGIN)
                    && gi <= hi + GROUP_HEIGHT_KEEP_MARGIN)
        });
    }
}

impl Render for LiveMarkdownView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = theme.ui;
        let is_dark = theme.appearance.is_dark();
        let style = MarkdownStyle::themed(window, cx);
        let colors = LiveColors::resolve(&style, &theme);
        let focused = self.focus.is_focused(window);
        let cursor = self.buffer.cursor();
        let selection = self.buffer.selection().map(|(mut s, mut e)| {
            if e < s {
                std::mem::swap(&mut s, &mut e);
            }
            (s, e)
        });

        // Highlight colors are baked into cached runs; a theme/base-style
        // change (appearance switch, font size change) must invalidate them.
        let hl_key = style_fingerprint(&style, is_dark);
        if self.hl_style_key != hl_key {
            self.hl_style_key = hl_key;
            self.hl.clear();
        }

        // Blink management (Zed BlinkManager parity): enable on focus+active,
        // disable otherwise. `enable` is idempotent; `disable` is guarded by
        // `blink_on` so unfocused frames don't notify-loop.
        let window_active = window.is_window_active();
        let want_blink = focused && window_active && !self.read_only;
        if want_blink && !self.blink_on {
            self.blink.update(cx, |b, cx| b.enable(cx));
            self.blink_on = true;
        } else if !want_blink && self.blink_on {
            self.blink.update(cx, |b, cx| b.disable(cx));
            self.blink_on = false;
        }
        let show_caret = self.show_caret(window, cx);

        // Fresh per-frame hit geometry (LiveLine::paint repopulates).
        self.row_layouts.clear();
        self.row_targets.clear();

        // Plan rows (cached on version+cursor+selection+focus).
        let rows: Vec<PlannedRow> = self.planned_rows(focused).to_vec();
        let last_line = rows.len().saturating_sub(1);
        let entity = cx.entity();

        // ---- grouping (before paint: virtualization window must gate
        // per-row work, otherwise blink ticks stay O(rows)) -----------------
        let mut groups: Vec<Group> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            match &rows[i].block {
                Block::Fence { open: true, lang } => {
                    let lang = lang.clone();
                    let open = i;
                    i += 1;
                    let mut code = Vec::new();
                    while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                        code.push(i);
                        i += 1;
                    }
                    let close = if i < rows.len()
                        && matches!(rows[i].block, Block::Fence { open: false, .. })
                    {
                        let c = Some(i);
                        i += 1;
                        c
                    } else {
                        None
                    };
                    groups.push(Group::Fence {
                        open,
                        code,
                        close,
                        lang,
                    });
                }
                Block::Quote { depth, .. } => {
                    let depth = *depth;
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len()
                        && matches!(rows[i].block, Block::Quote { depth: d, .. } if d == depth)
                    {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Quote(v));
                }
                Block::Paragraph => {
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len() && matches!(rows[i].block, Block::Paragraph) {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Para(v));
                }
                Block::Table => {
                    let mut v = vec![i];
                    i += 1;
                    while i < rows.len() && matches!(rows[i].block, Block::Table) {
                        v.push(i);
                        i += 1;
                    }
                    groups.push(Group::Table(v));
                }
                _ => {
                    groups.push(Group::One(i));
                    i += 1;
                }
            }
        }

        // Row virtualization window, computed BEFORE per-row paint so
        // `code_runs` / `find_marks` / `paints` / `display_numbers` only pay
        // for the viewport (+ overscan). Small docs render everything.
        let mut ginfo: Vec<(usize, u8, (usize, usize))> = Vec::with_capacity(groups.len());
        let mut prev_base: u8 = 9;
        for gr in groups.iter() {
            let base = gr.cal_bucket(&rows);
            let b = if base == 9 {
                10 + prev_base.min(9)
            } else {
                base
            };
            ginfo.push((gr.row_count(), b, gr.line_range(&rows)));
            prev_base = base;
        }
        let text_version = self.buffer.text_version();
        let want_caret = self.scroll_to_cursor;
        let vw = self.virtual_window(&ginfo, cursor.line, text_version, want_caret);
        let virtualized = vw.is_some();
        let (v_lo, v_hi, v_top_px, v_bot_px) =
            vw.unwrap_or((0, groups.len().saturating_sub(1), 0.0, 0.0));
        // Visible row indices for this frame (`None` = everything).
        let visible: Option<Vec<bool>> = if virtualized {
            let mut vis = vec![false; rows.len()];
            for gi in v_lo..=v_hi {
                if let Some(gr) = groups.get(gi) {
                    match gr {
                        Group::One(pi) => {
                            if *pi < vis.len() {
                                vis[*pi] = true;
                            }
                        }
                        Group::Fence { open, code, close, .. } => {
                            if *open < vis.len() {
                                vis[*open] = true;
                            }
                            for pi in code {
                                if *pi < vis.len() {
                                    vis[*pi] = true;
                                }
                            }
                            if let Some(c) = close {
                                if *c < vis.len() {
                                    vis[*c] = true;
                                }
                            }
                        }
                        Group::Para(v) | Group::Quote(v) | Group::Table(v) => {
                            for pi in v {
                                if *pi < vis.len() {
                                    vis[*pi] = true;
                                }
                            }
                        }
                    }
                }
            }
            // Caret row must paint even at window edges (scroll_to_item
            // target + caret height).
            if let Some(pi) = rows.iter().position(|r| r.line == cursor.line) {
                if pi < vis.len() {
                    vis[pi] = true;
                }
            }
            Some(vis)
        } else {
            None
        };
        let is_visible = |pi: usize| -> bool {
            match &visible {
                None => true,
                Some(v) => *v.get(pi).unwrap_or(&false),
            }
        };


        // ---- per-row paint (runs + selection + caret) --------------------
        // (`row.plan` already carries callout tag-stripping from
        // `planned_rows`, shared with hit-testing.)
        //
        // Code rows highlight as whole blocks (one tree-sitter parse per
        // fence, split per line) so multi-line constructs keep their spans;
        // pre-compute the per-line runs here, keyed by source line.
        let mut code_runs: std::collections::HashMap<usize, Vec<TextRun>> =
            std::collections::HashMap::new();
        {
            let mut i = 0;
            while i < rows.len() {
                let lang = match &rows[i].block {
                    Block::Fence { open: true, lang: Some(l) } => l.clone(),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                let start = i;
                i += 1;
                while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                    i += 1;
                }
                let code = start + 1..i;
                if i < rows.len() && matches!(rows[i].block, Block::Fence { open: false, .. }) {
                    i += 1;
                }
                if code.is_empty() {
                    continue;
                }
                // Virtualized: skip highlight (tree-sitter + clones) for
                // fences fully outside the viewport window.
                if virtualized && !(start..i).any(|pi| is_visible(pi)) {
                    continue;
                }
                let base = row_base_style(&rows[code.start].plan, &style);
                let src_lines: Vec<&str> = code
                    .clone()
                    .map(|pi| self.buffer.line(rows[pi].line).unwrap_or(""))
                    .collect();
                let per_line = self.hl.runs_for_block(&lang, &src_lines, &base, &theme);
                for (pi, runs) in code.zip(per_line) {
                    // Only window rows are painted; off-screen runs would
                    // just bloat the map with per-line clones.
                    if is_visible(pi) {
                        code_runs.insert(rows[pi].line, runs);
                    }
                }
            }
        }
        // Find highlights: byte ranges → source lines → visible cols
        // (multi-line matches split per row, hidden-marker spans dropped).
        let find_marks: HashMap<usize, Vec<(usize, usize, bool)>> = {
            let mut marks: HashMap<usize, Vec<(usize, usize, bool)>> = HashMap::new();
            let (match_ranges, current) = self
                .search_panel
                .as_ref()
                .and_then(|p| {
                    p.read(cx)
                        .matcher()
                        .map(|m| (m.match_ranges().to_vec(), m.current_ix()))
                })
                .unwrap_or_default();
            if !match_ranges.is_empty() {
                let row_by_line: HashMap<usize, usize> = rows
                    .iter()
                    .enumerate()
                    .filter(|(pi, _)| is_visible(*pi))
                    .map(|(pi, r)| (r.line, pi))
                    .collect();
                let lines = self.buffer.lines();
                for (ix, range) in match_ranges.iter().enumerate() {
                    let start = pos_of_offset(lines, range.start);
                    let end = pos_of_offset(lines, range.end);
                    for line in start.line..=end.line {
                        let Some(src) = lines.get(line) else {
                            continue;
                        };
                        let bs = if line == start.line {
                            byte_for_col(src, start.col)
                        } else {
                            0
                        };
                        let be = if line == end.line {
                            byte_for_col(src, end.col)
                        } else {
                            src.len()
                        };
                        if bs >= be {
                            continue;
                        }
                        let Some(pi) = row_by_line.get(&line) else {
                            continue;
                        };
                        if virtualized && !is_visible(*pi) {
                            continue;
                        }
                        let segs = &rows[*pi].plan.segs;
                        let (vs, ve) = (vis_for_src(segs, bs), vis_for_src(segs, be));
                        if vs < ve {
                            marks.entry(line).or_default().push((vs, ve, ix == current));
                        }
                    }
                }
            }
            marks
        };
        // Only viewport rows pay for runs/selection/caret shaping;
        // off-screen rows are skipped entirely (no per-row clones).
        let mut paints: HashMap<usize, RowPaint> = HashMap::new();
        for (pi, row) in rows.iter().enumerate() {
            if virtualized && !is_visible(pi) {
                continue;
            }
            let plan = &row.plan;
            let base = row_base_style(plan, &style);
            let mut runs = if let Some(block_runs) = code_runs.get(&row.line) {
                block_runs.clone()
            } else {
                runs_for_row(plan, &base, &colors, &style, &self.hl, &theme)
            };
            if matches!(row.plan.kind, RowKind::Task { checked: true, .. }) {
                runs = apply_task_done(
                    runs,
                    ui.text_muted,
                    ui.text_muted.opacity(0.6),
                );
            }
            // Selection overlay: source cols → bytes → visible bytes.
            let vsel = selection.and_then(|(s, e)| {
                let src_line = self.buffer.line(row.line)?;
                let line_len = src_line.chars().count();
                let (cs, ce, _) = line_selection(s, e, row.line, line_len)?;
                let bs = byte_for_col(src_line, cs);
                let be = byte_for_col(src_line, ce);
                Some((
                    vis_for_src(&plan.segs, bs),
                    vis_for_src(&plan.segs, be),
                ))
            });
            runs = apply_selection(runs, plan.visible.len(), vsel, colors.selection);
            if let Some(marks) = find_marks.get(&row.line) {
                runs = apply_find_marks(
                    runs,
                    plan.visible.len(),
                    marks,
                    colors.find_bg,
                    colors.find_current,
                );
            }
            let caret_vis = if show_caret && cursor.line == row.line {
                let src_line = self.buffer.line(row.line).unwrap_or("");
                Some(vis_for_src(
                    &plan.segs,
                    byte_for_col(src_line, cursor.col),
                ))
            } else {
                None
            };
            // Caret height = this row's real rendered line height (base
            // style × line-height multiplier), not a hardcoded per-kind
            // value (C4).
            let font_size = base.font_size.to_pixels(window.rem_size());
            let caret_h = window.pixel_snap(
                base.line_height
                    .to_pixels(font_size.into(), window.rem_size()),
            );
            let (caret_block_width, caret_block_text) = if caret_vis.is_some() {
                let src_line = self.buffer.line(row.line).unwrap_or("");
                let ch = src_line.chars().nth(cursor.col);
                Self::caret_block_for(
                    self.cursor_shape,
                    ch,
                    &base,
                    font_size,
                    caret_h,
                    window,
                    cx,
                )
            } else {
                (CURSOR_WIDTH, None)
            };
            // Stash paint for the windowed grouping pass below.
            paints.insert(
                pi,
                RowPaint {
                    runs,
                    caret_vis,
                    caret_h,
                    caret_block_width,
                    caret_block_text,
                },
            );
        }
        // ---- render helpers (per-row LiveLine) ------------------------------
        // Ordered display numbers per nesting depth (preview parity: see
        // `OrderedCounter`). Counters depend on document order, so scan
        // from the top to the window end but only retain window rows:
        // top-window frames avoid the O(rows) tail entirely.
        let display_numbers: HashMap<usize, Option<u64>> = {
            let mut ordered = OrderedCounter::new();
            let mut map: HashMap<usize, Option<u64>> = HashMap::new();
            let scan_end = if virtualized {
                let mut max_pi = 0usize;
                if let Some(vis) = &visible {
                    for (pi, on) in vis.iter().enumerate() {
                        if *on {
                            max_pi = max_pi.max(pi);
                        }
                    }
                }
                max_pi.min(rows.len().saturating_sub(1))
            } else {
                rows.len().saturating_sub(1)
            };
            for (pi, row) in rows.iter().enumerate() {
                if pi > scan_end {
                    break;
                }
                let n = ordered.visit(&row.plan.kind, list_cols(&row.plan.kind));
                if is_visible(pi) && n.is_some() {
                    map.insert(pi, n);
                }
            }
            map
        };
        // row_els index of the element containing the caret's source line,
        // for `scroll_to_item` (the rows column registers every direct
        // child's bounds with the scroll handle).
        let mut caret_el_ix: Option<usize> = None;
        let mut row_els: Vec<gpui::AnyElement> = Vec::new();
        // The first row element's LiveLine also carries the dead-zone drag
        // fallback (exactly one handler per frame); the closure consumes
        // the flag so call sites stay unchanged.
        let mut first_row_el = true;
        // `render_line` needs lots of context: build rows inline via closure.
        let cursor_shape = self.cursor_shape;
        let mut live_line = |pi: usize,
                             paints: &HashMap<usize, RowPaint>,
                             entity: &Entity<Self>,
                             colors: &LiveColors|
         -> LiveLine {
            let drag_fallback = first_row_el;
            first_row_el = false;
            let paint = paints.get(&pi).expect("visible row must have paint");
            let plan = &rows[pi].plan;
            LiveLine::new(
                ("live-line", rows[pi].line),
                SharedString::from(plan.visible.clone()),
                paint.runs.clone(),
                rows[pi].line,
                entity.clone(),
                matches!(rows[pi].plan.kind, RowKind::Task { .. }),
                paint.caret_vis,
                colors.caret,
                CURSOR_WIDTH,
                paint.caret_h,
            )
            .with_drag_fallback(drag_fallback)
            .with_caret_shape(cursor_shape)
            .with_caret_block(paint.caret_block_width, paint.caret_block_text.clone())
        };

        if virtualized && v_top_px > 0.0 {
            row_els.push(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .h(px(v_top_px))
                    .into_any_element(),
            );
        }

        // Reference definitions for table cells (`[t][r]` inside the grid
        // resolves document-wide like the preview). 只在真的有表格渲染时才收集，
        // 并且跨帧缓存：这次扫描要遍历整篇文档，边打字边每帧重扫纯属浪费。
        // 跳过规则与 `reveal::plan_document` 一致，源码块里的定义不会生效。
        let table_refs = if groups.iter().any(|g| matches!(g, Group::Table(_))) {
            // 规划键决定每行的 block（进而决定跳过哪些行），所以要一起进缓存键。
            let key = (
                self.buffer.text_version(),
                (
                    self.buffer.cursor(),
                    self.buffer.selection(),
                    focused,
                    self.read_only,
                    self.show_source,
                ),
            );
            if self.table_refs_key != Some(key) {
                let buf_lines = self.buffer.lines();
                self.table_refs = Rc::new(collect_refdefs(buf_lines, &|i| {
                    rows.get(i).is_some_and(|r| {
                        matches!(
                            r.block,
                            Block::CodeLine
                                | Block::Fence { .. }
                                | Block::FrontMatter
                                | Block::Table
                                | Block::Math { .. }
                                | Block::MathDelim { .. }
                        )
                    })
                }));
                self.table_refs_key = Some(key);
            }
            self.table_refs.clone()
        } else {
            Rc::new(HashMap::new())
        };
        // ---- mermaid pass1：对**全部** mermaid 组（含视口外，虚拟化不参与）
        // ensure 渲染缓存（后台 + 200ms 防抖），随后推进缓存帧。
        // 全量而非只扫视口内的原因：「上一帧同位置成品图」回退按文档序对齐，
        // 只扫视口会让窗口滑动时位置错位、把别的块的旧图闪现到这个块上。
        {
            let mm_colors = MermaidColors::resolve(&theme);
            self.mermaid.begin_frame(mm_colors.fingerprint());
            for g in &groups {
                if let Group::Fence { open, code, .. } = g {
                    let lang = match &rows[*open].block {
                        Block::Fence { open: true, lang } => lang.as_deref(),
                        _ => None,
                    };
                    if !code.is_empty()
                        && lang.is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
                    {
                        let block_id = rows[*open].line;
                        let src = code
                            .iter()
                            .map(|c| self.buffer.line(rows[*c].line).unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.mermaid.ensure(block_id, &src, &mm_colors, cx);
                    }
                }
            }
            self.mermaid.end_frame();
        }
        let mut window_counts: Vec<usize> = Vec::new();
        for (gi, group) in groups.iter().enumerate() {
            if virtualized && (gi < v_lo || gi > v_hi) {
                continue;
            }
            let els_before = row_els.len();
            match group {
                Group::One(pi) => {
                    let pi = *pi;
                    let row = &rows[pi];
                    let is_first = row.line == 0;
                    let is_last = row.line == last_line;
                    // Ordered display numbers sequence like the preview
                    // (windowed; see display_numbers).
                    let display_number = display_numbers.get(&pi).copied().flatten();
                    let el: gpui::AnyElement = match &row.plan.kind {
                        RowKind::Blank => {
                            // Blanks have no text hitbox: explicit click moves
                            // the cursor here. The cursor line also paints the
                            // caret inside an (empty) LiveLine.
                            let line = row.line;
                            let blank = div()
                                .id(("live-blank", line))
                                .debug_selector(|| "live-blank-row".into())
                                .w_full()
                                .h(px(26.))
                                // The scroll column is a fixed-height flex
                                // container: without shrink-0 a blank row's
                                // fixed 26px collapses to 0 on any doc taller
                                // than the viewport (unclickable + caret
                                // overlap). Every row element needs this.
                                .flex_shrink_0()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, window, cx| {
                                        window.focus(&this.focus, cx);
                                        // Col clamps to the line end, like
                                        // guise's hit() (whitespace-only
                                        // lines land at their end, not col 0).
                                        this.edit(cx, |m| m.move_to(line, usize::MAX, false));
                                        cx.notify();
                                        cx.stop_propagation();
                                    }),
                                );
                            if paints.get(&pi).is_some_and(|p| p.caret_vis.is_some()) {
                                blank
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                blank.into_any_element()
                            }
                        }
                        RowKind::Rule => {
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .py(px(2.))
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                // Preview parity: 1px soft rule, my_6.
                                // The rule itself has no text: clicks land the
                                // cursor on this source line (revealing it).
                                let line = row.line;
                                let mut d = div()
                                    .id(("live-rule", line))
                                    .flex_shrink_0()
                                    .h(px(1.))
                                    .w_full()
                                    .my_6()
                                    .bg(colors.rule)
                                    .rounded_full()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    );
                                if is_first {
                                    d = d.mt_0();
                                }
                                if is_last {
                                    d = d.mb_0();
                                }
                                d.into_any_element()
                            }
                        }
                        RowKind::Heading(level) => {
                            let level = *level;
                            let mut head = match level {
                                1 => div().mt_8().mb_4().pb_2(),
                                2 => div().mt_7().mb_3().pb_2(),
                                3 => div().mt_6().mb_3(),
                                4 => div().mt_6().mb_2(),
                                _ => div().mt_4().mb_2(),
                            };
                            if is_first {
                                head = head.mt_0();
                            }
                            if is_last {
                                head = head.mb_0();
                            }
                            head = head
                                .w_full()
                                .flex_shrink_0()
                                .flex()
                                .flex_wrap()
                                .items_start()
                                .group("heading");
                            head.style().refine(&style.heading);
                            if let Some(r) = style
                                .heading_level_styles
                                .get(level as usize - 1)
                                .cloned()
                                .flatten()
                            {
                                head.style().refine(&gpui::StyleRefinement {
                                    text: r,
                                    ..Default::default()
                                });
                            }
                            if level <= 2
                                && let Some(color) = style.heading_border_color
                            {
                                head = head.border_b_1().border_color(color);
                            }
                            let anchor = div()
                                .ml_2()
                                .flex_shrink_0()
                                .text_size(px(13.))
                                .text_color(ui.text_muted.opacity(0.35))
                                .invisible()
                                .group_hover("heading", |s| s.visible())
                                .child("#")
                                .into_any_element();
                            head.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(live_line(pi, &paints, &entity, &colors)),
                            )
                            .child(anchor)
                            .into_any_element()
                        }
                        RowKind::Bullet { cols }
                        | RowKind::Ordered { cols, .. }
                        | RowKind::Task { cols, .. } => {
                            // Preview parity: nesting indent 24px per level,
                            // item row flex/gap/mb/lh, preview-exact gutters.
                            let depth = cols / 2;
                            let indent = px(depth as f32 * 24.0);
                            let gutter: gpui::AnyElement = match &row.plan.kind {
                                RowKind::Task { checked, .. } => {
                                    let checked = *checked;
                                    let line = row.line;
                                    let box_el = if checked {
                                        div()
                                            .size(px(16.))
                                            .rounded(px(4.))
                                            .bg(colors.success)
                                            .border_1()
                                            .border_color(colors.success)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                svg()
                                                    .size(px(10.))
                                                    .text_color(gpui::white())
                                                    .path(Icon::Check.path()),
                                            )
                                            .into_any_element()
                                    } else {
                                        div()
                                            .size(px(16.))
                                            .rounded(px(4.))
                                            .border_1()
                                            .border_color(ui.text_muted.opacity(0.35))
                                            .bg(gpui::transparent_black())
                                            .into_any_element()
                                    };
                                    div()
                                        .id(("live-task", line))
                                        .w(px(22.))
                                        .flex_shrink_0()
                                        .mt(px(3.))
                                        .child(
                                            div()
                                                .child(box_el)
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(
                                                        move |this, _e, _w, cx| {
                                                            this.toggle_task_at(line, cx);
                                                            cx.stop_propagation();
                                                        },
                                                    ),
                                                ),
                                        )
                                        .into_any_element()
                                }
                                RowKind::Ordered { number, .. } => {
                                    let n = display_number.unwrap_or(*number);
                                    div()
                                        .w(px(24.))
                                        .flex()
                                        .justify_end()
                                        .flex_shrink_0()
                                        .text_size(px(14.))
                                        .font_weight(gpui::FontWeight(500.))
                                        .pr(px(4.))
                                        .child(format!("{n}."))
                                        .text_color(ui.text_muted)
                                        .into_any_element()
                                }
                                _ => div()
                                    .w(px(16.))
                                    .flex()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .text_size(px(22.))
                                    .child("•")
                                    .text_color(ui.accent)
                                    .into_any_element(),
                            };
                            let mut item = div()
                                .flex()
                                .flex_row()
                                .flex_shrink_0()
                                .items_start()
                                .gap_1()
                                .w_full()
                                .mb_2()
                                .line_height(rems(1.75))
                                .pl(indent);
                            if is_first {
                                item = item.mt_2();
                            }
                            item.child(gutter)
                                .child(
                                    div().flex_1().w_0().min_w_0().child(live_line(pi, &paints, &entity, &colors)),
                                )
                                .into_any_element()
                        }
                        RowKind::Quote { .. } => {
                            // Single quote lines are grouped above; unreachable.
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::Setext => {
                            // A setext underline (`===`/`---` under a heading
                            // line): the preview consumes it into the heading,
                            // so in rendered mode it is a small gap whose
                            // click targets the line; revealed, it shows the
                            // raw marker for editing.
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                let line = row.line;
                                div()
                                    .id(("live-setext", line))
                                    .flex_shrink_0()
                                    .w_full()
                                    .h(px(8.))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .into_any_element()
                            }
                        }
                        RowKind::Math => {
                            // Display formula: centered italic line
                            // (Typora-style; KaTeX approx, no TeX engine).
                            div()
                                .w_full()
                                .flex_shrink_0()
                                .flex()
                                .justify_center()
                                .my_2()
                                .line_height(rems(1.75))
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::MathDelim { .. } => {
                            // A hidden `$$` delimiter: revealed it shows the
                            // raw marker; otherwise a small click gap
                            // (Setext parity) that targets the line.
                            if row.plan.revealed {
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &paints, &entity, &colors))
                                    .into_any_element()
                            } else {
                                let line = row.line;
                                div()
                                    .id(("live-math", line))
                                    .flex_shrink_0()
                                    .w_full()
                                    .h(px(8.))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, window, cx| {
                                            window.focus(&this.focus, cx);
                                            this.edit(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .into_any_element()
                            }
                        }
                        RowKind::Code { .. }
                        | RowKind::Fence { .. }
                        | RowKind::Table
                        | RowKind::FrontMatter => {
                            // Ungrouped source-only rows (revealed blocks,
                            // lone fences, frontmatter): plain mono lines.
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                        RowKind::Paragraph => {
                            // Paragraphs are grouped above; unreachable
                            // (kept as a defensive plain row).
                            div()
                                .flex_shrink_0()
                                .child(live_line(pi, &paints, &entity, &colors))
                                .into_any_element()
                        }
                    };
                    if row.line == cursor.line {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((row.line, row.line));
                    row_els.push(el);
                }
                Group::Fence {
                    open,
                    code,
                    close,
                    lang,
                } => {
                    // Only the fence LINES flip the card to source mode (the
                    // cursor sitting on ```). Code lines stay rendered in
                    // the card while editing them — Obsidian behavior — so
                    // clicking into code never un-renders it.
                    //
                    // mermaid 例外（严格 tab-only）：光标停在围栏行**不再**翻
                    // 源码 —— 否则「光标路过」就等于自动显示源码/可编辑，违背
                    // 「只能点『源码』tab 才能编辑」。只保留全文档源码模式
                    //（`Cmd+/`，plan_document 会把所有行 revealed）这条用户显式
                    // 通道。
                    let is_mermaid =
                        lang.as_deref().is_some_and(|l| l.eq_ignore_ascii_case("mermaid"));
                    let fence_revealed = rows[*open].plan.revealed
                        || close.is_some_and(|c| rows[c].plan.revealed);
                    let revealed = fence_revealed && (!is_mermaid || self.show_source);
                    let block_id = rows[*open].line;
                    if revealed || code.is_empty() {
                        // Source editing mode: mono lines, fences visible.
                        // One horizontal scroll container per block so long
                        // Nowrap lines scroll instead of being clipped
                        // (preview parity: its code card scrolls too).
                        let mut idxs = vec![*open];
                        idxs.extend(code.iter().cloned());
                        idxs.extend(close.iter().cloned());
                        let mut src = div()
                            .id(("live-code-src", block_id))
                            .flex_shrink_0()
                            .overflow_x_scroll();
                        for &pi in &idxs {
                            if rows[pi].line == cursor.line {
                                caret_el_ix = Some(row_els.len());
                            }
                            src = src.child(live_line(pi, &paints, &entity, &colors));
                        }
                        let last_idx = *idxs.last().unwrap();
                        self.row_targets.push((rows[*open].line, rows[last_idx].line));
                        row_els.push(src.into_any_element());
                        window_counts.push(row_els.len() - els_before);
                        continue;
                    }
                    // Preview-parity code card (fences hidden, like preview).
                    let is_first = rows[*open].line == 0;
                    let is_last = close.is_some_and(|c| rows[c].line == last_line);
                    let code_text = code
                        .iter()
                        .map(|c| self.buffer.line(rows[*c].line).unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    // ---- mermaid：预览/源码切换（唯一的状态改变入口） -------
                    let showing_code = is_mermaid && self.mermaid.is_showing_code(block_id);
                    let mm_body = if is_mermaid {
                        Some(self.mermaid.body(&code_text))
                    } else {
                        None
                    };
                    // 图模式且真的画出了图 → 卡体画图；其余（回退源码 /
                    // 源码 tab / 渲染中无旧图）都画源码行。
                    let show_image =
                        !showing_code && matches!(&mm_body, Some(MermaidBody::Image { .. }));
                    let mut outer = div()
                        .group("live-code")
                        .relative()
                        .w_full()
                        .flex_shrink_0()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    outer.style().refine(&style.code_block);
                    if is_first {
                        outer.style().refine(&gpui::StyleRefinement {
                            margin: gpui::EdgesRefinement {
                                top: Some(gpui::Length::Definite(px(0.).into())),
                                ..Default::default()
                            },
                            ..Default::default()
                        });
                    }
                    if is_last {
                        outer.style().refine(&gpui::StyleRefinement {
                            margin: gpui::EdgesRefinement {
                                bottom: Some(gpui::Length::Definite(px(0.).into())),
                                ..Default::default()
                            },
                            ..Default::default()
                        });
                    }
                    let mut inner = if show_image {
                        // 图体：固定尺寸图片，裁剪而非横向滚动条。
                        div()
                            .id(("live-code-inner", block_id))
                            .debug_selector(|| "live-code-inner".into())
                            .overflow_hidden()
                    } else {
                        div()
                            .id(("live-code-inner", block_id))
                            .debug_selector(|| "live-code-inner".into())
                            // Preview parity: the read-only card scrolls long
                            // Nowrap lines horizontally instead of clipping them.
                            .overflow_x_scroll()
                    };
                    if lang.is_some() {
                        inner = inner.pt(px(24.0));
                    }
                    if show_image {
                        if let Some(MermaidBody::Image { image, pending }) = &mm_body {
                            let (dw, dh) = image.display_size();
                            let img = gpui::img(image.image().clone())
                                .w(px(dw))
                                .h(px(dh))
                                .object_fit(gpui::ObjectFit::Contain);
                            // 重渲染中：旧图继续显示 + 呼吸提示（不闪回源码）。
                            let fig = if *pending {
                                img.with_animation(
                                    ("live-mm-pulse", block_id),
                                    Animation::new(Duration::from_millis(1600))
                                        .repeat()
                                        .with_easing(gpui::pulsating_between(0.45, 1.0)),
                                    |el, delta| el.opacity(delta),
                                )
                                .into_any_element()
                            } else {
                                img.into_any_element()
                            };
                            inner = inner.child(
                                div().w_full().flex().justify_center().child(fig),
                            );
                        }
                    } else {
                        for pi in code {
                            inner = inner.child(live_line(*pi, &paints, &entity, &colors));
                        }
                    }
                    let mut card = outer.child(inner);
                    if is_mermaid {
                        // 测试探针：mermaid 卡片当前态。`live-mm-graph` 只在
                        // **真的画着图**（含重渲染中的旧图）时出现，渲染中/失败
                        // 回退源码时是 `live-mm-source` —— 与禁区判定同源，
                        // 避免测试靠 tab 状态猜渲染结果。
                        let is_image = show_image;
                        card = card.debug_selector(move || {
                            if is_image {
                                "live-mm-graph".into()
                            } else {
                                "live-mm-source".into()
                            }
                        });
                    }
                    // mermaid 卡片不渲染语言徽章（左上角让位给「预览 | 源码」
                    // tab）；其他语言保持原样。
                    if !is_mermaid
                        && let Some(lang) = lang
                    {
                        card = card.child(
                            div()
                                .absolute()
                                .top(px(7.0))
                                .left(px(12.0))
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.0))
                                .bg(ui.text.opacity(0.06))
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight(600.))
                                .text_color(ui.text_muted)
                                .child(lang.to_uppercase())
                                .into_any_element(),
                        );
                    }
                    let is_copied = self.copied_code_blocks.contains(&block_id);
                    let view_ent = entity.clone();
                    let copy_btn = div()
                        .id(("live-copy", block_id))
                        .debug_selector(|| "live-copy".into())
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .invisible()
                        .group_hover("live-code", |s| s.visible())
                        .child(if is_copied {
                            svg()
                                .size(px(16.0))
                                .text_color(ui.success)
                                .path(Icon::Check.path())
                        } else {
                            svg()
                                .size(px(16.0))
                                .text_color(ui.text_muted)
                                .path(Icon::Copy.path())
                        })
                        .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                code_text.clone(),
                            ));
                            view_ent.update(cx, |v, cx| {
                                v.mark_code_block_copied(block_id, cx)
                            });
                        })
                        .into_any_element();
                    if is_mermaid {
                        // 「预览」「源码」两个独立圆角矩形按钮，贴左上角
                        //（原语言徽章的位置），中间留一点间隔。tab 是图/源码
                        // 的唯一切换入口；复制按钮仍独占右上角、组级 hover。
                        //
                        // 垂直对齐：复制按钮 = top 6 + py 4 + 16px 图标 → 中心
                        // 18px；这里 top 9 + 高度 18（py 2 + 行高 14）→ 中心也是
                        // 18px，两者文字/图标落在同一条水平线上。
                        let tab_btn = |label: &'static str,
                                       active: bool,
                                       id: (&'static str, usize),
                                       sel: &'static str,
                                       to_source: bool,
                                       ent: Entity<Self>| {
                            div()
                                .id(id)
                                .debug_selector(move || sel.to_string())
                                .px(px(9.0))
                                .py(px(2.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(if active {
                                    ui.text.opacity(0.12)
                                } else {
                                    ui.text.opacity(0.05)
                                })
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .font_weight(gpui::FontWeight(600.))
                                .text_color(if active { ui.text } else { ui.text_muted })
                                .child(label)
                                .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                    ent.update(cx, |v, cx| {
                                        v.set_mermaid_showing_code(block_id, to_source, cx)
                                    });
                                })
                        };
                        card = card.child(
                            div()
                                .absolute()
                                .top(px(9.0))
                                .left(px(10.0))
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .child(tab_btn(
                                    "预览",
                                    !showing_code,
                                    ("live-mm-tab-graph", block_id),
                                    "live-mm-tab-graph",
                                    false,
                                    entity.clone(),
                                ))
                                .child(tab_btn(
                                    "源码",
                                    showing_code,
                                    ("live-mm-tab-src", block_id),
                                    "live-mm-tab-src",
                                    true,
                                    entity.clone(),
                                ))
                                .into_any_element(),
                        );
                    }
                    card = card.child(
                        div()
                            .absolute()
                            .top(px(6.0))
                            .right(px(6.0))
                            .flex()
                            .child(copy_btn)
                            .into_any_element(),
                    );
                    // 图模式回退源码时的 footer：失败给出原因 / 渲染中给出
                    // 呼吸提示。源码 tab（编辑态）不打扰，保持纯净。
                    if is_mermaid && !showing_code && !show_image {
                        let (label, pending) = match &mm_body {
                            Some(MermaidBody::Source { error: Some(msg) }) => (msg.clone(), false),
                            _ => ("渲染中…".to_string(), true),
                        };
                        let foot = div()
                            .px(px(12.0))
                            .py(px(6.0))
                            .text_size(px(11.0))
                            .text_color(ui.text_muted.opacity(0.7))
                            .child(label);
                        let foot_el = if pending {
                            foot.with_animation(
                                ("live-mm-foot", block_id),
                                Animation::new(Duration::from_millis(1600))
                                    .repeat()
                                    .with_easing(gpui::pulsating_between(0.5, 1.0)),
                                |el, delta| el.opacity(delta),
                            )
                            .into_any_element()
                        } else {
                            foot.into_any_element()
                        };
                        card = card.child(foot_el);
                    }
                    let group_last_line = close
                        .map(|c| rows[c].line)
                        .unwrap_or_else(|| rows[*code.last().unwrap()].line);
                    // 图模式不画光标（源码行未渲染，光标不可驻留——移动/
                    // 点击/搜索路径均已在 `mermaid_image_ranges` 处夹取）。
                    if !show_image
                        && rows[*open].line <= cursor.line
                        && cursor.line <= group_last_line
                    {
                        caret_el_ix = Some(row_els.len());
                    }
                    // Fallback target: the first code line (lands the cursor
                    // inside the block without flipping the card).
                    self.row_targets.push((
                        rows[*code.first().unwrap()].line,
                        group_last_line,
                    ));
                    row_els.push(card.into_any_element());
                }
                Group::Para(idxs) => {
                    // Consecutive paragraphs render as ONE block (preview
                    // parity: joined into a single paragraph there, so no
                    // inter-row margins). Image-only lines inside the run
                    // keep their shared card.
                    let first = &rows[idxs[0]];
                    let is_first = first.line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let mut d = div()
                        .w_full()
                        .flex_shrink_0()
                        .my_3()
                        .line_height(rems(1.75))
                        .debug_selector(|| "live-para".into());
                    if is_first {
                        d = d.mt_0();
                    }
                    if is_last {
                        d = d.mb_0();
                    }
                    for pi in idxs {
                        let src = self.buffer.line(rows[*pi].line).unwrap_or("");
                        if !rows[*pi].plan.revealed
                            && let Some((alt, url)) = parse_image_line(src)
                        {
                            d = d.child(MarkdownElementBuilder::image_card(
                                &url, &alt, None, cx,
                            ));
                        } else {
                            d = d.child(live_line(*pi, &paints, &entity, &colors));
                        }
                    }
                    let a = rows[idxs[0]].line;
                    let b = rows[*idxs.last().unwrap()].line;
                    if a <= cursor.line && cursor.line <= b {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((a, b));
                    row_els.push(d.into_any_element());
                }
                Group::Quote(idxs) => {
                    let first = &rows[idxs[0]];
                    let depth = match first.block {
                        Block::Quote { depth, .. } => depth,
                        _ => 1,
                    };
                    let callout = first
                        .callout
                        .as_ref()
                        .filter(|c| c.is_first)
                        .map(|c| c.kind);
                    let is_first = first.line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let (border_color, bg, bw) = match callout {
                        Some(kind) => {
                            let accent = callout_accent(kind, &theme);
                            (
                                accent,
                                accent.opacity(if is_dark { 0.18 } else { 0.13 }),
                                px(4.),
                            )
                        }
                        None => (
                            colors.quote_bar,
                            ui.text.opacity(if is_dark { 0.05 } else { 0.03 }),
                            px(3.),
                        ),
                    };
                    let mut card = div()
                        .flex_shrink_0()
                        .my_3()
                        .py_3()
                        .pr_4()
                        .rounded(px(6.0))
                        .bg(bg);
                    card.style().refine(&gpui::StyleRefinement {
                        padding: gpui::EdgesRefinement {
                            left: Some(gpui::DefiniteLength::Absolute(
                                gpui::AbsoluteLength::Pixels(px(20.)),
                            )),
                            ..Default::default()
                        },
                        border_widths: gpui::EdgesRefinement {
                            left: Some(gpui::AbsoluteLength::Pixels(bw)),
                            ..Default::default()
                        },
                        border_color: Some(border_color),
                        ..Default::default()
                    });
                    if is_first {
                        card = card.mt_0();
                    }
                    if is_last {
                        card = card.mb_0();
                    }
                    if depth > 1 {
                        card = card.ml(px((depth - 1) as f32 * 20.0));
                    }
                    if let Some(kind) = callout {
                        let accent = callout_accent(kind, &theme);
                        card = card.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .mb_2()
                                .text_size(px(12.))
                                .font_weight(gpui::FontWeight(700.))
                                .text_color(accent)
                                .child(div().size(px(10.)).rounded_full().bg(accent))
                                .child(kind.label().to_string())
                                .into_any_element(),
                        );
                    }
                    for pi in idxs {
                        card = card.child(live_line(*pi, &paints, &entity, &colors));
                    }
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((
                        rows[idxs[0]].line,
                        rows[*idxs.last().unwrap()].line,
                    ));
                    row_els.push(card.into_any_element());
                }
                Group::Table(idxs) => {
                    let revealed = idxs.iter().any(|c| rows[*c].plan.revealed);
                    let src_lines: Vec<String> = idxs
                        .iter()
                        .map(|c| self.buffer.line(rows[*c].line).unwrap_or("").to_string())
                        .collect();
                    let grid_rows: Vec<Vec<String>> =
                        src_lines.iter().map(|l| split_table_row(l)).collect();
                    // GFM: the delimiter row must carry the header's column
                    // count, otherwise this is not a table at all (same rule
                    // pulldown-cmark applies in the read-only preview).
                    let has_delimiter = grid_rows.len() > 1
                        && table_delimiter_cols(&src_lines[1]).is_some_and(|c| {
                            c == grid_rows[0].len() && c > 0
                        });
                    if revealed || !has_delimiter {
                        // Whole-block source view (`Cmd+/`) or not really a
                        // table: mono rows.
                        for pi in idxs {
                            if rows[*pi].line == cursor.line {
                                caret_el_ix = Some(row_els.len());
                            }
                            self.row_targets.push((rows[*pi].line, rows[*pi].line));
                            row_els.push(
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(*pi, &paints, &entity, &colors))
                                    .into_any_element(),
                            );
                        }
                        window_counts.push(row_els.len() - els_before);
                        continue;
                    }
                    // Preview-parity grid. The cell under the cursor renders
                    // its own source (markers visible) through `plan_cell`, so
                    // it can be edited in place — the rest stay rendered.
                    let active = self.table_active_cell(&rows, idxs, cursor);
                    // Preview-parity grid.
                    let col_count = grid_rows.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
                    let aligns: Vec<CellAlign> = (0..col_count)
                        .map(|c| {
                            grid_rows
                                .get(1)
                                .and_then(|r| r.get(c))
                                .map(|c| align_of(c))
                                .unwrap_or(CellAlign::None)
                        })
                        .collect();
                    let start_line = rows[idxs[0]].line;
                    let is_first = start_line == 0;
                    let is_last = rows[*idxs.last().unwrap()].line == last_line;
                    let mut outer = div()
                        .id(("live-table", start_line))
                        .debug_selector(|| "live-table-outer".into())
                        .w_full()
                        .flex_shrink_0()
                        .mt_6()
                        .mb_4()
                        .rounded(px(8.0))
                        .border(px(1.0))
                        .border_color(colors.table_border)
                        .bg(colors.panel_bg.opacity(0.5));
                    if is_first {
                        outer = outer.mt_0();
                    }
                    if is_last {
                        outer = outer.mb_0();
                    }
                    // Click the grid to jump the cursor into the block source.
                    outer = outer.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            this.edit(cx, |m| m.move_to(start_line, 0, false));
                            cx.notify();
                        }),
                    );
                    let mut grid = div()
                        .grid()
                        .grid_cols(col_count as u16)
                        .w_full()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    // grid_rows[0] = header, [1] = delimiter (skipped),
                    // [2..] = data. `preview_idx` matches the preview's
                    // row indices (header 0, data 1, 2, …) for dividers/zebra.
                    for (di, _cells) in grid_rows.iter().enumerate() {
                        if di == 1 {
                            continue;
                        }
                        let is_header = di == 0;
                        let preview_idx = di - usize::from(di > 0);
                        for (col, align) in aligns.iter().enumerate() {
                            let mut cell_div = div().px_3().py_2().text_size(px(14.));
                            if col > 0 || preview_idx > 0 {
                                cell_div = cell_div.border_color(colors.table_border);
                                if col > 0 {
                                    cell_div = cell_div.border_l(px(1.0));
                                }
                                if preview_idx > 0 {
                                    cell_div = cell_div.border_t(px(1.0));
                                }
                            }
                            if is_header {
                                cell_div = cell_div.bg(colors.elevated_bg);
                            } else if preview_idx % 2 == 0 {
                                cell_div = cell_div.bg(colors.panel_bg);
                            }
                            match align {
                                CellAlign::Left => cell_div = cell_div.justify_start(),
                                CellAlign::Center => cell_div = cell_div.justify_center(),
                                CellAlign::Right => cell_div = cell_div.justify_end(),
                                CellAlign::None => {}
                            }
                            // 活动格：光标所在的那一格显示自己的源码（标记
                            // 可见），并且用 `LiveLine` 渲染以复用光标绘制 /
                            // 命中测试 / 拖拽选择。其余格保持渲染态。
                            let row_line = rows[idxs[di.min(idxs.len() - 1)]].line;
                            let is_active = active
                                .as_ref()
                                .is_some_and(|(l, c, _)| *l == row_line && *c == col);
                            if is_active {
                                let content = active
                                    .as_ref()
                                    .map(|(_, _, r)| r.clone())
                                    .expect("is_active 蕴含 active 存在");
                                // 探针：活动格（测试断言点击/打字落在格内）。
                                cell_div = cell_div.debug_selector(move || {
                                    format!("live-table-active-{di}-{col}")
                                });
                                let src_line = src_lines
                                    .get(di.min(src_lines.len() - 1))
                                    .map(String::as_str)
                                    .unwrap_or("");
                                let plan = plan_cell(src_line, content.clone(), true, &table_refs);
                                let base = row_base_style(&plan, &style);
                                let runs =
                                    runs_for_row(&plan, &base, &colors, &style, &self.hl, &theme);
                                let caret_vis = show_caret.then(|| {
                                    vis_for_src(
                                        &plan.segs,
                                        byte_for_col(src_line, cursor.col),
                                    )
                                });
                                // 单元格光标高度：与行渲染同一套度量（基础样式
                                // 行高），不另造数字。
                                let cell_base = row_base_style(&plan, &style);
                                let cell_font = cell_base.font_size.to_pixels(window.rem_size());
                                let cell_caret_h = window.pixel_snap(
                                    cell_base
                                        .line_height
                                        .to_pixels(cell_font.into(), window.rem_size()),
                                );
                                cell_div = cell_div.child(
                                    LiveLine::new(
                                        ("live-table-cell", start_line + di * 64 + col),
                                        SharedString::from(plan.visible.clone()),
                                        runs,
                                        row_line,
                                        entity.clone(),
                                        false,
                                        caret_vis,
                                        colors.caret,
                                        CURSOR_WIDTH,
                                        cell_caret_h,
                                    )
                                    .with_cell_segs(plan.segs.clone(), content.clone())
                                    .with_caret_shape(self.cursor_shape),
                                );
                            } else {
                                // 非活动格：同样走 `plan_cell`（reveal=false，
                                // 标记隐藏），只是不上光标。两条路径共用一份
                                // 规划器，光标移进/移出格子时文字不会跳变。
                                let src_line = src_lines
                                    .get(di.min(src_lines.len() - 1))
                                    .map(String::as_str)
                                    .unwrap_or("");
                                let (s, e) = table_cell_spans(src_line)
                                    .get(col)
                                    .copied()
                                    .unwrap_or((0, src_line.len()));
                                let raw = &src_line[s..e];
                                let lead = raw.len() - raw.trim_start().len();
                                let trail = raw.len() - raw.trim_end().len();
                                let content = (s + lead)..(e - trail).max(s + lead);
                                let plan =
                                    plan_cell(src_line, content, false, &table_refs);
                                let cell_base = row_base_style(&plan, &style);
                                let runs = runs_for_row(
                                    &plan, &cell_base, &colors, &style, &self.hl, &theme,
                                );
                                cell_div = cell_div.child(
                                    StyledText::new(SharedString::from(plan.visible.clone()))
                                        .with_runs(runs),
                                );
                            }
                            grid = grid.child(cell_div.into_any_element());
                        }
                    }
                    outer = outer.child(grid);
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets.push((
                        rows[idxs[0]].line,
                        rows[*idxs.last().unwrap()].line,
                    ));
                    row_els.push(outer.into_any_element());
                }
            }
            window_counts.push(row_els.len() - els_before);
        }
        self.last_window_counts = if virtualized {
            window_counts
        } else {
            Vec::new()
        };

        if virtualized && v_bot_px > 0.0 {
            row_els.push(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .h(px(v_bot_px))
                    .into_any_element(),
            );
        }
        #[cfg(test)]
        {
            self.last_render_stats = (groups.len(), v_hi - v_lo + 1, virtualized);
        }

        if rows.is_empty() || self.buffer.is_empty() {
            let show_ph = self.buffer.is_empty() && !focused && !self.placeholder.is_empty();
            if show_ph {
                self.row_targets.push((0, 0));
                row_els.push(
                    div()
                        .text_color(colors.marker)
                        .child(self.placeholder.clone())
                        .into_any_element(),
                );
            } else if rows.is_empty() {
                let ent = entity.clone();
                if show_caret {
                    caret_el_ix = Some(row_els.len());
                }
                self.row_targets.push((0, 0));
                row_els.push(
                    div()
                        .flex_1()
                        .child(
                            LiveLine::new(
                                ("live-line", 0usize),
                                SharedString::from(""),
                                vec![],
                                0,
                                ent,
                                false,
                                show_caret.then_some(0),
                                colors.caret,
                                CURSOR_WIDTH,
                                px(26.),
                            )
                            .with_caret_shape(self.cursor_shape)
                            .with_caret_block(
                                if self.cursor_shape.is_block_like() {
                                    (px(26.) * 0.6).max(CURSOR_WIDTH)
                                } else {
                                    CURSOR_WIDTH
                                },
                                None,
                            ),
                        )
                        .into_any_element(),
                );
            }
        }

        // Scroll-to-caret: the rows column (`live-markdown-content`) tracks
        // every direct child with the scroll handle, so `scroll_to_item`
        // scrolls the minimal amount to bring the caret's element into view
        // (guise `ensure_cursor_visible` parity). Consumed by the column's
        // prepaint in this same frame.
        if self.scroll_to_cursor {
            self.scroll_to_cursor = false;
            if let Some(ix) = caret_el_ix {
                self.scroll.scroll_to_item(ix);
            }
        }


        // Outline (Typora Outline parity) + current section highlight.
        // Hidden panel: skip the O(rows) scan entirely (large docs pay it
        // per frame otherwise).
        let (outline, current_section) = if self.show_outline {
            let outline = outline_entries(&rows);
            let current = outline
                .iter()
                .filter(|(line, _, _)| *line <= cursor.line)
                .last()
                .map(|(line, _, _)| *line);
            (outline, current)
        } else {
            (Vec::new(), None)
        };

        // Status strip (Typora status-bar parity): cursor + counts + outline toggle.
        // Full-document char counting is O(n) per frame: cache on
        // (text_version, selection), recompute only when either changes.
        let stat_version = self.buffer.text_version();
        let stat_sel = self.buffer.selection();
        let (stat_chars, stat_lines, sel_chars) = match &self.stat_cache {
            Some((v, sel, chars, lines, sel_chars)) if *v == stat_version && *sel == stat_sel => {
                (*chars, *lines, *sel_chars)
            }
            _ => {
                let mut chars = 0usize;
                for line in self.buffer.lines() {
                    chars += line.chars().filter(|c| !c.is_whitespace()).count();
                }
                let lines = self.buffer.lines().len();
                let sel_chars = self
                    .selected_source_text()
                    .map(|t| t.chars().filter(|c| !c.is_whitespace()).count());
                self.stat_cache = Some((stat_version, stat_sel, chars, lines, sel_chars));
                (chars, lines, sel_chars)
            }
        };
        let show_outline = self.show_outline;
        let show_source = self.show_source;
        let status = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .mt(px(2.))
            .pt(px(6.))
            .border_t_1()
            .border_color(ui.border.opacity(0.4))
            .text_size(px(11.))
            .text_color(ui.text_muted)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(format!("行 {}, 列 {}", cursor.line + 1, cursor.col + 1))
                    .child(format!("共 {stat_chars} 字 · {stat_lines} 行"))
                    .when_some(sel_chars, |this, n| {
                        this.child(
                            div()
                                .text_color(ui.text_muted.opacity(0.8))
                                .child(format!("已选 {n} 字")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .id("live-source-toggle")
                            .cursor_pointer()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .text_color(if show_source {
                                ui.accent
                            } else {
                                ui.text_muted
                            })
                            .hover(|this| this.bg(ui.tab_hover_background))
                            .child(if show_source { "源码 ✓" } else { "源码" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.toggle_show_source(cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("live-outline-toggle")
                            .cursor_pointer()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .text_color(if show_outline {
                                ui.accent
                            } else {
                                ui.text_muted
                            })
                            .hover(|this| this.bg(ui.tab_hover_background))
                            .child(if show_outline { "大纲 ✓" } else { "大纲" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.show_outline = !this.show_outline;
                                    cx.notify();
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            );

        // Toggleable outline side panel: click jumps (focus kept).
        let outline_panel = show_outline.then(|| {
            div()
                .id("live-outline")
                .w(px(230.))
                .h_full()
                .flex_shrink_0()
                .border_l_1()
                .border_color(ui.border.opacity(0.5))
                .bg(ui.panel_background)
                .flex()
                .flex_col()
                .py_4()
                .child(
                    div()
                        .px(px(12.))
                        .mb_2()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight(600.))
                        .text_color(ui.text_muted)
                        .child("大纲"),
                )
                .child(if outline.is_empty() {
                    div()
                        .px(px(12.))
                        .text_size(px(12.))
                        .text_color(ui.text_muted.opacity(0.6))
                        .child("暂无标题")
                        .into_any_element()
                } else {
                    div()
                        .id("live-outline-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px(px(6.))
                        .children(outline.iter().map(|(line, level, title)| {
                            let line = *line;
                            let current = Some(line) == current_section;
                            div()
                                .id(("live-outline-item", line))
                                .pl(px(6. + (*level as f32 - 1.) * 12.))
                                .pr(px(6.))
                                .py(px(3.))
                                .rounded(px(6.))
                                .text_size(px(12.))
                                .whitespace_nowrap()
                                .truncate()
                                .overflow_hidden()
                                .cursor_pointer()
                                .text_color(if current { ui.accent } else { ui.text })
                                .when(current, |this| this.bg(ui.accent.opacity(0.12)))
                                .hover(|this| this.bg(ui.tab_hover_background))
                                .child(title.clone())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, window, cx| {
                                        window.focus(&this.focus, cx);
                                        this.edit(cx, |m| m.move_to(line, 0, false));
                                        cx.notify();
                                        cx.stop_propagation();
                                    }),
                                )
                                .into_any_element()
                        }))
                        .into_any_element()
                })
        });

        // Find bar (Typora Cmd+F parity): query + count + prev/next/close,
        // option chips, collapsible replace row. Fixed above the content.
        // Slash `/` menu (Typora/Notion parity): an in-flow bar above the
        // status strip listing the filtered block commands. It derives from
        // the buffer every frame, so no open/close state is stored.
        let slash_bar: Option<gpui::AnyElement> = self
            .slash_menu()
            .filter(|_| focused)
            .map(|(_, items)| {
                let sel = self.slash_index.min(items.len() - 1);
                let mut bar = div()
                    .flex_shrink_0()
                    .w_full()
                    .mt(px(6.))
                    .p(px(4.))
                    .rounded(px(8.))
                    .bg(ui.panel_background)
                    .border_1()
                    .border_color(ui.border.opacity(0.5));
                for (i, cmd_ix) in items.iter().copied().enumerate() {
                    let cmd = &SLASH_COMMANDS[cmd_ix];
                    let (title, hint) = (cmd.title, cmd.hint);
                    let active = i == sel;
                    bar = bar.child(
                        div()
                            .id(("live-slash", cmd_ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(active, |this| this.bg(ui.accent.opacity(0.12)))
                            .hover(|this| this.bg(ui.tab_hover_background))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(if active { ui.accent } else { ui.text })
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(ui.text_muted.opacity(0.7))
                                    .child(hint),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.slash_index = i;
                                    this.apply_slash(cmd_ix, cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    );
                }
                bar.into_any_element()
            });

        // Preview-parity container: centered 780px column, no editor chrome.
        div()
            .id("live-markdown")
            .key_context("LiveMarkdown")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            // Dead-zone fallback: clicks no row element claimed (margins,
            // card paddings, past-EOL areas) land the cursor on the nearest
            // line. Runs after every row handler (registration order), so
            // claimed clicks never reach it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.on_fallback_mouse_down(event, window, cx);
                }),
            )
            // OS 文件拖拽投放：图片文件导入 assets 并插入引用（Typora parity）。
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                if this.read_only {
                    return;
                }
                if this.drop_image_files(paths.paths(), cx) {
                    this.after_edit(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Copy, _window, cx| {
                if let Some(t) = this.selected_source_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                this.edit(cx, |m| m.select_all());
                cx.notify();
            }))
            .flex()
            .justify_center()
            .size_full()
            .overflow_hidden()
            .bg(ui.app_background)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .max_w(px(780.))
                            .px_8()
                            .pt_8()
                            .pb_2()
                            .h_full()
                            .overflow_hidden()
                            .when_some(self.search_panel.clone(), |this, panel| {
                                this.child(panel)
                            })
                            .child(
                                div()
                                    .id("live-markdown-content")
                                    .debug_selector(|| "live-markdown-content".into())
                                    .flex()
                                    .flex_col()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .children(row_els),
                            )
                            .when_some(slash_bar, |this, bar| this.child(bar))
                            .child(status),
                    ),
            )
            .when_some(outline_panel, |this, panel| this.child(panel))
    }
}

#[cfg(test)]
mod live_render_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;
    use gpui::{point, size};

    const SAMPLE: &str = "# Title\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\npara text\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    const SHOWCASE: &str = include_str!("../../../markdown-showcase.md");

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    /// Every non-blank planned row (code, table, quote, …) must shape to a
    /// non-zero box with the real theme + fonts + tree-sitter runs.
    #[gpui::test]
    fn live_lines_shape_with_nonzero_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        // 1. Build the exact LiveLines the view would build.
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme().clone();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<String> = SAMPLE.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
            ..Default::default()
        },
            );
            rows.iter()
                .enumerate()
                .filter(|(_, r)| !r.plan.visible.is_empty())
                .map(|(pi, r)| {
                    let base = row_base_style(&r.plan, &style);
                    let runs = runs_for_row(&r.plan, &base, &colors, &style, &hl, &theme);
                    let total: usize = runs.iter().map(|x| x.len).sum();
                    let label = format!("line {} {:?} {:?}", r.line, r.plan.kind, r.plan.visible);
                    assert_eq!(total, r.plan.visible.len(), "runs must cover {label}");
                    for run in &runs {
                        assert!(
                            run.color.a > 0.01,
                            "transparent text run for {label} (run={run:?})"
                        );
                    }
                    let line = LiveLine::new(
                        ("live-test-line", pi),
                        SharedString::from(r.plan.visible.clone()),
                        runs,
                        r.line,
                        view.clone(),
                        false,
                        None,
                        colors.caret,
                        px(2.),
                        px(24.),
                    );
                    Case { label, line }
                })
                .collect()
        });
        assert!(!cases.is_empty(), "sample must produce rows");
        // 2. Shape each line for real: full request_layout → prepaint → paint.
        for case in cases {
            let (styled, _) = cx.draw(
                point(px(0.), px(0.)),
                size(px(700.), px(200.)),
                |_, _| case.line,
            );
            let bounds = styled.layout().bounds();
            assert!(
                bounds.size.width > px(0.),
                "zero width for {}",
                case.label
            );
            assert!(
                bounds.size.height > px(0.),
                "zero height for {}",
                case.label
            );
        }
    }

    /// M3: the measured-height cache stays a constant rolling window, not a
    /// map that grows with how far you scroll. After seeding a large range and
    /// pruning for a mid-document window, only the window ± margin survives
    /// and the out-of-window entries are gone (they were estimators-only).
    #[gpui::test]
    fn group_heights_pruned_to_rolling_window(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        view.update(cx, |v, _| {
            // Simulate having measured every group of a very large doc as we
            // scrolled through it (index 0..50_000).
            for gi in 0..50_000usize {
                v.group_heights.insert(gi, 40.0);
            }
            assert!(v.group_heights.len() >= 50_000);
            // Now "the viewport is" around group 30_000..30_100.
            let lo = 30_000usize;
            let hi = 30_100usize;
            v.prune_group_heights(lo, hi);
            // Bounded to the window + margins, regardless of the doc size.
            assert!(v.group_heights.len() <= hi - lo + 1 + 2 * GROUP_HEIGHT_KEEP_MARGIN);
            // In-window entries survived.
            for gi in lo..=hi {
                assert!(v.group_heights.contains_key(&gi));
            }
            // Far-out entries were dropped.
            assert!(!v.group_heights.contains_key(&0));
            assert!(!v.group_heights.contains_key(&(hi + GROUP_HEIGHT_KEEP_MARGIN + 1)));
            // Margin neighbors survive.
            assert!(v.group_heights.contains_key(&lo.saturating_sub(GROUP_HEIGHT_KEEP_MARGIN)));
        });
    }

    /// Showcase repro: shape every row of the real showcase doc and dump
    /// heights per kind. Code rows must not collapse.
    #[gpui::test]
    fn showcase_rows_keep_sane_heights(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SHOWCASE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme().clone();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<String> = SHOWCASE.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
            ..Default::default()
        },
            );
            rows.iter()
                .enumerate()
                .filter(|(_, r)| !r.plan.visible.is_empty())
                .map(|(pi, r)| {
                    let base = row_base_style(&r.plan, &style);
                    let runs = runs_for_row(&r.plan, &base, &colors, &style, &hl, &theme);
                    let label = format!("L{} {:?} {:?}", r.line, r.plan.kind, r.plan.visible.chars().take(24).collect::<String>());
                    Case {
                        label,
                        line: LiveLine::new(
                            ("live-showcase-line", pi),
                            SharedString::from(r.plan.visible.clone()),
                            runs,
                            r.line,
                            view.clone(),
                            false,
                            None,
                            colors.caret,
                            px(2.),
                            px(24.),
                        ),
                    }
                })
                .collect()
        });
        let mut code_h = Vec::new();
        let mut body_h = Vec::new();
        for case in cases {
            let label = case.label.clone();
            let (styled, _) = cx.draw(
                point(px(0.), px(0.)),
                size(px(700.), px(200.)),
                |_, _| case.line,
            );
            let b = styled.layout().bounds();
            if label.contains("Code {") {
                code_h.push((label, b.size.height, b.size.width));
            } else if label.contains("Paragraph") {
                body_h.push(b.size.height);
            }
        }
        eprintln!("SHOWCASE code rows: {code_h:?}");
        eprintln!("SHOWCASE body heights: {body_h:?}");
        assert!(!code_h.is_empty(), "showcase must have code rows");
        for (label, h, _) in &code_h {
            assert!(*h > px(10.), "collapsed code row {label} h={h:?}");
        }
    }
}


#[cfg(test)]
mod live_frame_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;

    const SAMPLE: &str = "# Title\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\npara text\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Full-frame assertions shared by all window sizes.
    fn assert_frames(cx: &mut gpui::VisualTestContext, view: &gpui::Entity<LiveMarkdownView>) {
        // Frame 1: unfocused full preview → code card + table grid present.
        cx.refresh().unwrap();
        cx.run_until_parked();
        let code = cx.debug_bounds("live-code-inner");
        let table = cx.debug_bounds("live-table-outer");
        eprintln!("F1 CODE bounds: {code:?}");
        eprintln!("F1 TABLE bounds: {table:?}");
        assert!(
            code.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "code card inner missing/collapsed: {code:?}"
        );
        assert!(
            table.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
            "table grid missing/collapsed: {table:?}"
        );
        // Frame 2: focused, cursor on a CODE line → the card stays and the
        // code is edited in place (Obsidian behavior; only fence lines flip
        // a block to source mode).
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(3, 2, false));
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_some(),
            "cursor on a code line must keep the rendered card"
        );
        // Frame 3: cursor on the opening fence line → source mode, the
        // card disappears (fences visible for editing).
        cx.update(|_window, cx| {
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(2, 3, false));
            });
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_none(),
            "cursor on the fence line must drop the card (source mode)"
        );
    }

    /// Dynamic repro: type inside a code block and move the cursor around;
    /// code rows must keep painting after every replan (no stale caches).
    /// Since the card no longer flips while editing code (only fence lines
    /// flip it), the card must stay present the whole time.
    #[gpui::test]
    fn live_typing_keeps_code_painted(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        // Reset the paint log so assertions below see only this test's
        // frames (it is a process-wide static).
        let _ = crate::editor::markdown_live::element::LIVE_LINE_PAINT_LOG
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        // Move cursor into the code block (line 3) and type.
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 2, false));
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        let painted: Vec<(usize, usize)> = crate::editor::markdown_live::element::LIVE_LINE_PAINT_LOG
            .lock()
            .unwrap()
            .drain(..)
            .collect();
        eprintln!("AFTER-TYPE painted: {painted:?}");
        // Card mode: the code lines render inside the card; the fence
        // lines themselves stay hidden.
        for must in [3usize, 4, 5] {
            assert!(
                painted.iter().any(|(l, _)| *l == must),
                "code line {must} missing after typing (painted={painted:?})"
            );
        }
        // (No negative "fence hidden" assertion here: the paint log is a
        // process-wide static and parallel tests pollute it. The card
        // presence below is the semantic check — a flipped block would have
        // dropped `live-code-inner`.)
        assert!(
            cx.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(50.)),
            "code card must stay while editing inside it"
        );
        // Move cursor far away (line 0): the card stays (it never left) and
        // code keeps painting.
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 0, false));
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(50.)),
            "code card missing after leaving the block"
        );
        // The typed char must have stuck in the buffer.
        let text = view.read_with(cx, |v, _| v.text());
        assert!(text.contains("fnx ") || text.contains("fn x"), "typed char lost: {text:?}");
    }

    /// Repro: the real showcase doc, unfocused (full preview) — code cards
    /// and table grids must all be in the render tree with real boxes.
    /// Guards the `:---` delimiter regression (left-aligned columns used to
    /// fail, so every grid fell back to raw source lines).
    /// Large docs virtualize rows: jump the cursor onto each block first so
    /// its group enters the rendered window, then assert (this also covers
    /// virtualized scrolling to far blocks).
    #[gpui::test]
    fn live_showcase_frame_renders_code_and_tables(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = include_str!("../../../markdown-showcase.md");
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        let lines: Vec<String> = DOC.split('\n').map(str::to_string).collect();
        let rows = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
            ..Default::default()
        },
        );
        // Code lines (not fences: the cursor on a fence flips the card to
        // source mode) and table rows to jump onto.
        let code_line = rows
            .iter()
            .find(|r| matches!(r.plan.kind, RowKind::Code { .. }))
            .map(|r| r.line);
        let table_line = rows
            .iter()
            .find(|r| matches!(r.plan.kind, RowKind::Table))
            .map(|r| r.line);
        let jump_to = |cx: &mut gpui::VisualTestContext, line: usize| {
            cx.update(|_, cx| {
                view.update(cx, |v, cx| {
                    v.edit(cx, |m| m.move_to(line, 0, false));
                });
            });
            cx.refresh().unwrap();
            cx.run_until_parked();
        };
        if let Some(line) = code_line {
            jump_to(cx, line);
            let code = cx.debug_bounds("live-code-inner");
            eprintln!("SHOWCASE CODE: {code:?}");
            assert!(
                code.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
                "showcase code card missing: {code:?}"
            );
        }
        if let Some(line) = table_line {
            jump_to(cx, line);
            let table = cx.debug_bounds("live-table-outer");
            eprintln!("SHOWCASE TABLE: {table:?}");
            assert!(
                table.is_some_and(|b| b.size.height > px(0.) && b.size.width > px(0.)),
                "showcase table grid missing: {table:?}"
            );
        }
        assert!(
            code_line.is_some() && table_line.is_some(),
            "showcase must contain code + table rows for this guard"
        );
    }

    /// Mermaid 图模式端到端：卡片默认画图（源码行不渲染）→ 源码 tab 可编辑
    /// → 回图 tab 复原；图模式下光标是禁区（移动越过、编辑被吞）。
    #[gpui::test]
    fn live_mermaid_graph_mode_and_source_tab(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 0:# 标题 1:空 2:```mermaid 3:A-->B 4:``` 5:空 6:尾段
        const DOC: &str = "# T\n\n```mermaid\nflowchart TD\n    A[开始] --> B[结束]\n```\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        // 等后台渲染落地：防抖 200ms 是 `background_executor` 的定时器，
        // 测试里必须推进假时钟才会到期（纯 sleep 不会）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.debug_bounds("live-mm-graph").is_some() {
                ready = true;
                break;
            }
        }
        assert!(ready, "mermaid 图应在若干帧内渲染完成（graph 卡片出现）");
        let graph = cx.debug_bounds("live-mm-graph").expect("图卡片应有实体");
        assert!(graph.size.height > px(40.), "图卡片高度异常：{graph:?}");
        assert!(
            cx.debug_bounds("live-mm-source").is_none(),
            "图模式不该有源码卡片"
        );
        // 布局：两个 tab 与复制图标必须落在同一条水平中心线上
        //（`top 9 + 高 18` 对齐 `top 6 + py 4 + 图标 16` 的中心 18px）。
        let src_tab_b = cx.debug_bounds("live-mm-tab-src").expect("源码按钮");
        let copy_b = cx.debug_bounds("live-copy").expect("复制按钮");
        let tab_cy = src_tab_b.center().y;
        let copy_cy = copy_b.center().y;
        assert!(
            (f32::from(tab_cy) - f32::from(copy_cy)).abs() <= 1.0,
            "按钮与复制图标必须水平对齐：tab_cy={tab_cy:?} copy_cy={copy_cy:?}"
        );

        // 1) 图模式：光标不能驻留在块内（从块前一行向下移动，应直接跳到块后）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(1, 0, false));
        });
        cx.update(|_, cx| {
            view.update(cx, |v, cx| v.move_visual(true, false, cx));
        });
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert!(
            line >= 5,
            "图模式下 Down 必须越过整个 mermaid 块（落点 line={line}）"
        );

        // 2) 图模式：落到块内的编辑（模拟搜索跳转）会被夹出块外，源码不变。
        let before = view.read_with(cx, |v, _| v.text());
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 1, false));
            v.clamp_cursor_out_of_image_blocks(cx);
        });
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert!(line >= 5, "块内落点必须被夹到块外（line={line}）");
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            before,
            "夹取不得改动文本"
        );

        // 3) 真实鼠标点击「源码」按钮：卡片切成源码、图消失，且光标可进入块内编辑。
        //（这是用户唯一允许的切换入口，必须验证点击没被文档级鼠标处理器吃掉。）
        let src_tab = cx
            .debug_bounds("live-mm-tab-src")
            .expect("源码按钮应有实体");
        cx.simulate_click(src_tab.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |v, _| v.mermaid.is_showing_code(2)),
            "点击「源码」按钮必须真的切到源码（tab 是唯一入口）"
        );
        assert!(
            cx.debug_bounds("live-mm-source").is_some(),
            "源码 tab 应显示源码卡片"
        );
        assert!(
            cx.debug_bounds("live-mm-graph").is_none(),
            "源码 tab 不该还在画图"
        );
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 0, false));
            v.after_move(cx);
        });
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert_eq!(line, 3, "源码模式下光标应能进入块内（line={line}）");

        // 4) 真实点击「预览」按钮：图重新出现，光标被夹出块外。
        let graph_tab = cx
            .debug_bounds("live-mm-tab-graph")
            .expect("预览按钮应有实体");
        cx.simulate_click(graph_tab.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |v, _| v.mermaid.is_showing_code(2)),
            "点击「预览」按钮必须切回图模式"
        );
        assert!(
            cx.debug_bounds("live-mm-graph").is_some(),
            "回预览应恢复图卡片"
        );
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert!(
            !(2..=5).contains(&line),
            "切回预览后光标必须被夹出隐藏源码区（line={line}）"
        );

        // 5) 全文档源码模式（Cmd+/ 同一开关）关闭时同样要夹光标：先开源码、
        // 把光标放进块内、再关掉，光标不得滞留在隐藏区。
        view.update(cx, |v, cx| {
            v.toggle_show_source(cx);
            v.edit(cx, |m| m.move_to(3, 0, false));
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert_eq!(line, 3, "全文档源码模式下光标应能进入块内（line={line}）");
        view.update(cx, |v, cx| v.toggle_show_source(cx));
        cx.refresh().unwrap();
        cx.run_until_parked();
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert!(
            !(2..=5).contains(&line),
            "关闭全文档源码模式后光标必须被夹出隐藏区（line={line}）"
        );
    }

    /// 表格格内编辑：点击单元格 → 光标落在格内（不整块跳源码）→ 打字只改
    /// 该格；Tab 跨格、Esc 退出表格；`Cmd+/` 仍可看整块源码。
    #[gpui::test]
    fn live_table_cell_editing_stays_in_grid(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 0:表头 1:分隔 2:数据 3:数据 4:空 5:尾段
        const DOC: &str = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        // 未聚焦光标在第 0 行（表头）→ 网格照常渲染，不退回源码。
        assert!(
            cx.debug_bounds("live-table-outer").is_some(),
            "表格必须渲染成网格"
        );

        // 1) 点击第 2 行第 2 格（`2`）→ 光标落在格内，网格仍在。
        // 先把光标放进目标格（点击需要 bounds，bounds 只在该格是活动格时
        // 才有探针，所以先用编辑接口定位，再断言点击路径本身）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let cell = cx
            .debug_bounds("live-table-active-2-0")
            .expect("第 2 行第 1 格应为活动格");
        cx.simulate_click(cell.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-table-outer").is_some(),
            "点击单元格后网格必须保留（不退回源码行）"
        );
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(line, 2, "光标应落在被点的数据行");
        assert!(
            (1..=11).contains(&col),
            "光标应落在该行某个格内（col={col}）"
        );

        // 2) 打字只改被点的那一格（line 2 的某一格），其他行不动。
        let line_before = view.read_with(cx, |v, _| v.buffer.line(2).unwrap_or("").to_string());
        cx.simulate_input("9");
        cx.run_until_parked();
        let text = view.read_with(cx, |v, _| v.text());
        let line_after = view.read_with(cx, |v, _| v.buffer.line(2).unwrap_or("").to_string());
        assert_ne!(line_before, line_after, "打字必须改到被点的行：{text:?}");
        assert!(line_after.contains('9'), "插入的字符应在该行：{line_after:?}");
        assert!(text.contains("| A | B |"), "表头不得被改动：{text:?}");
        assert!(text.contains("| 3 | 4 |"), "其他数据行不得被改动：{text:?}");
        assert_eq!(
            text.lines().count(),
            6,
            "打字不得破坏表格结构（不新增/删除行）：{text:?}"
        );

        // 3) Tab 跨格：光标移动到下一个格位（同行下一格，或跨到下一行首格）。
        let (line_b, col_b) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        let (line_a, col_a) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert!(
            (line_a, col_a) != (line_b, col_b),
            "Tab 必须把光标移到另一个格位（{line_b},{col_b} → {line_a},{col_a}）"
        );
        assert!(
            line_a <= 3,
            "Tab 不得离开表格（line={line_a}）"
        );

        // 4) Esc 退出表格：光标移到表格后一行（第 4 行），文本不变。
        let before = view.read_with(cx, |v, _| v.text());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert_eq!(line, 4, "Esc 应把光标移出表格（line={line}）");
        assert_eq!(view.read_with(cx, |v, _| v.text()), before, "Esc 不得改动文本");
    }

    /// 表格里「光标在哪个格」的判定必须与 GFM 切分一致：含转义管道的行
    /// 仍然分格正确（旧实现用朴素 split，会把 `\|` 也当边界）。
    #[gpui::test]
    fn live_table_escaped_pipe_keeps_cell_mapping(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        // 光标放在第一格内容里（`a` 上）→ 该格是活动格，且映射到行内正确位置。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-table-active-0-0").is_some(),
            "第一格应为活动格（转义管道不能把它切成两格）"
        );
        assert!(
            cx.debug_bounds("live-table-active-0-1").is_none(),
            "第二格不该同时是活动格"
        );
    }

    /// Image paste/drop (Typora parity): files land in `<stem>.assets/`
    /// with Typora-layout refs, clipboard bytes get unique names.
    #[gpui::test]
    fn live_image_drop_imports_into_assets(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let dir = std::env::temp_dir().join(format!("deepterm-live-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("shot.PNG");
        std::fs::write(&src, b"fake-png-bytes").unwrap();
        let doc = dir.join("notes.md");
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| {
                LiveMarkdownView::with_text("hello\n", cx).doc_path(doc.clone())
            }))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        // 1. Drop an image file → copied into notes.assets, ref inserted.
        view.update(cx, |v, cx| {
            assert!(v.drop_image_files(std::slice::from_ref(&src), cx));
        });
        let text = view.read_with(cx, |v, _| v.text());
        assert!(
            text.contains("![](notes.assets/image.png)"),
            "drop ref missing: {text:?}"
        );
        assert!(dir.join("notes.assets/image.png").exists());
        // 2. Non-image drops are ignored (fall through to text paste).
        let txt = dir.join("data.txt");
        std::fs::write(&txt, b"hi").unwrap();
        view.update(cx, |v, cx| {
            assert!(!v.drop_image_files(std::slice::from_ref(&txt), cx));
        });
        // 3. Clipboard image bytes → unique name alongside the first.
        use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat};
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::Image(Image {
                format: ImageFormat::Png,
                bytes: b"more-bytes".to_vec(),
                id: 7,
            })],
        };
        view.update(cx, |v, cx| {
            assert!(v.paste_images(&item, cx));
        });
        let text = view.read_with(cx, |v, _| v.text());
        assert!(
            text.contains("![](notes.assets/image-1.png)"),
            "pasted ref missing: {text:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Real repo docs: every fenced block / table in the file must come out
    /// as a card in the live editor too, not as raw source lines.
    #[gpui::test]
    fn live_real_docs_render_code_and_tables(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        for (name, doc, want_code, want_table) in [
            (
                "CODE_REVIEW",
                include_str!("../../../docs/CODE_REVIEW.md"),
                false,
                true,
            ),
            (
                "TAB_SPEC",
                include_str!("../../../docs/safari-26-tab-spec.md"),
                true,
                true,
            ),
            (
                "AUDIT",
                include_str!("../../../docs/CODE_AUDIT_2026-09-04.md"),
                false,
                true,
            ),
        ] {
            let (host, cx) = cx.add_window_view(|_, cx| {
                Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx)))
            });
            let view = host.read_with(cx, |h, _| h.0.clone());
            // Large docs virtualize rows: presence must be checked with the
            // block scrolled into the rendered window (jump the cursor onto
            // it); absence is decided at classification level (a grid/card
            // cannot render for rows that were never classified as such).
            let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
            ..Default::default()
        },
            );
            let jump_to = |cx: &mut gpui::VisualTestContext, line: usize| {
                cx.update(|_, cx| {
                    view.update(cx, |v, cx| {
                        v.edit(cx, |m| m.move_to(line, 0, false));
                    });
                });
                cx.refresh().unwrap();
                cx.run_until_parked();
            };
            let code_line = rows
                .iter()
                .find(|r| matches!(r.plan.kind, RowKind::Code { .. }))
                .map(|r| r.line);
            let table_line = rows
                .iter()
                .find(|r| matches!(r.plan.kind, RowKind::Table))
                .map(|r| r.line);
            assert_eq!(
                code_line.is_some(),
                want_code,
                "{name}: doc classifies code={} expected={want_code}",
                code_line.is_some()
            );
            assert_eq!(
                table_line.is_some(),
                want_table,
                "{name}: doc classifies table={} expected={want_table}",
                table_line.is_some()
            );
            if let Some(line) = code_line {
                jump_to(cx, line);
                let code = cx.debug_bounds("live-code-inner");
                assert!(
                    code.is_some_and(|b| b.size.height > px(0.)),
                    "{name}: code card missing after scroll: {code:?}"
                );
            }
            if let Some(line) = table_line {
                jump_to(cx, line);
                let table = cx.debug_bounds("live-table-outer");
                assert!(
                    table.is_some_and(|b| b.size.height > px(0.)),
                    "{name}: table grid missing after scroll: {table:?}"
                );
            }
        }
    }

    /// Repro of the field bug: the view nested inside the editor tab
    /// container (`flex_1 / min_h_0 / overflow_hidden` chain), where code
    /// cards collapsed to a thin pill strip — language badge visible, code
    /// rows gone. The bare-window tests above can't see this.
    #[gpui::test]
    fn live_inside_editor_tab_container_keeps_code_rows(cx: &mut gpui::TestAppContext) {
        struct EditorHost(gpui::Entity<LiveMarkdownView>);
        impl gpui::Render for EditorHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                // Mirrors the `md-live` branch in editor.rs.
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("md-live")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .px(px(6.0))
                            .pb(px(12.0))
                            .child(self.0.clone()),
                    )
            }
        }
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            EditorHost(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let code = cx.debug_bounds("live-code-inner");
        eprintln!("TAB-CONTAINER CODE: {code:?}");
        assert!(
            code.is_some_and(|b| b.size.height > px(50.)),
            "code rows collapsed inside the tab container: {code:?}"
        );
        let _ = view;
    }

    #[gpui::test]
    fn live_frame_maximized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());
        assert_frames(cx, &view);
    }

    #[gpui::test]
    fn live_frame_narrow_pane(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(800.), px(600.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        assert_frames(&mut vc, &view);
    }
}

#[cfg(test)]
mod live_showcase_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;

    const SHOWCASE: &str = include_str!("../../../markdown-showcase.md");

    /// The exact user repro: plan the showcase doc and report every code /
    /// table row whose visible text is empty or suspiciously short.
    #[test]
    fn showcase_code_and_table_rows_have_visible_text() {
        let lines: Vec<String> = SHOWCASE.split('\n').map(str::to_string).collect();
        let rows = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
            ..Default::default()
        },
        );
        assert_eq!(rows.len(), lines.len());
        let mut empties = Vec::new();
        let mut code_count = 0;
        let mut table_count = 0;
        for row in &rows {
            match &row.plan.kind {
                RowKind::Code { lang } => {
                    code_count += 1;
                    let src = &lines[row.line];
                    if row.plan.visible.is_empty() && !src.is_empty() {
                        empties.push((row.line, format!("CODE[{lang:?}] src={src:?}")));
                    }
                }
                RowKind::Table => {
                    table_count += 1;
                    let src = &lines[row.line];
                    if row.plan.visible.is_empty() && !src.is_empty() {
                        empties.push((row.line, format!("TABLE src={src:?}")));
                    }
                }
                RowKind::Fence { .. } => {}
                _ => {}
            }
        }
        eprintln!("code rows: {code_count}, table rows: {table_count}");
        assert!(
            code_count > 10,
            "showcase must contain plenty of code rows"
        );
        assert!(
            empties.is_empty(),
            "code/table rows with empty visible: {empties:?}"
        );
    }

    /// Parity guard: every block the read-only preview (pulldown-cmark)
    /// turns into a code card / table grid must be classified the same way
    /// by the live classifier, or the two renderers drift apart again —
    /// which is exactly how `:---` tables and list-nested fences were lost.
    #[test]
    fn outline_collects_headings_in_order() {
        let doc = "# A\n\ntext\n\n## B\n\n### C\n\n## D\n";
        let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
        let rows = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
            ..Default::default()
        },
        );
        let outline = outline_entries(&rows);
        assert_eq!(
            outline
                .iter()
                .map(|(l, v, t)| (*l, *v, t.clone()))
                .collect::<Vec<_>>(),
            vec![
                (0, 1, "A".to_string()),
                (4, 2, "B".to_string()),
                (6, 3, "C".to_string()),
                (8, 2, "D".to_string()),
            ]
        );
        assert_eq!(outline_title("  a  b  "), "a b");
        assert_eq!(outline_title(""), "(空标题)");
    }

    #[test]
    fn live_blocks_match_preview_blocks() {
        for (name, doc) in [
            ("showcase", SHOWCASE),
            (
                "list_fence",
                "- item\n\n    ```python\n    x = 1\n    ```\n\nafter\n",
            ),
            ("pipeless_table", "\na | b\n--- | ---\n1 | 2\n\nend\n"),
            (
                "aligned_table",
                "| left | center | right |\n| :--- | :---: | ---: |\n| a | b | c |\n",
            ),
        ] {
            let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
            ..Default::default()
        },
            );
            for (kind, a, b) in preview_block_ranges(doc) {
                for row in rows.iter().take(b + 1).skip(a) {
                    let ok = match kind {
                        "code" => matches!(
                            row.block,
                            Block::Fence { .. } | Block::CodeLine
                        ),
                        _ => matches!(row.block, Block::Table),
                    };
                    assert!(
                        ok,
                        "{name}: preview sees {kind} on L{}, live says {:?} (src={:?})",
                        row.line,
                        row.block,
                        lines.get(row.line)
                    );
                }
            }
        }
    }

    /// Source line ranges of code blocks / tables, as pulldown-cmark sees
    /// them (same options as the read-only preview).
    fn preview_block_ranges(doc: &str) -> Vec<(&'static str, usize, usize)> {
        use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
        let mut starts = vec![0usize];
        let mut off = 0usize;
        for l in doc.split('\n') {
            off += l.len() + 1;
            starts.push(off);
        }
        let line_of = |b: usize| starts.iter().rposition(|&s| s <= b).unwrap_or(0);
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        let mut out = Vec::new();
        let mut code: Option<usize> = None;
        let mut table: Option<usize> = None;
        for (ev, range) in Parser::new_ext(doc, opts).into_offset_iter() {
            match ev {
                Event::Start(Tag::CodeBlock(_)) => code = Some(range.start),
                Event::End(TagEnd::CodeBlock) => {
                    if let Some(s) = code.take() {
                        out.push(("code", line_of(s), line_of(range.end.saturating_sub(1))));
                    }
                }
                Event::Start(Tag::Table(_)) => table = Some(range.start),
                Event::End(TagEnd::Table) => {
                    if let Some(s) = table.take() {
                        out.push(("table", line_of(s), line_of(range.end.saturating_sub(1))));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// Manual visual diagnostic: renders the live view offscreen with real Metal
/// and saves `/tmp/live_code.png`.
/// Requires a macOS GUI session + main thread (fails headless):
/// `cargo test -p deepterm --lib live_code_screenshot -- --ignored --test-threads=1`
#[cfg(all(test, target_os = "macos"))]
mod live_screenshot_tests {
    use super::*;

    /// Mirrors markdown-showcase.md §5.1: one fenced block per language.
    const SHOT_SAMPLE: &str = "### 5.1 fence\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n```python\ndef f(x):\n    return x\n```\n";

    struct ShotHost(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for ShotHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    #[test]
    #[ignore]
    fn live_code_screenshot() {
        let mut cx = gpui::VisualTestAppContext::new(gpui_platform::current_platform(false));
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let window = cx
            .open_offscreen_window(gpui::size(px(900.), px(800.)), |_, cx| {
                cx.new(|cx| ShotHost(cx.new(|cx| LiveMarkdownView::with_text(SHOT_SAMPLE, cx))))
            })
            .unwrap();
        cx.run_until_parked();
        let shot = cx.capture_screenshot(window.into()).unwrap();
        shot.save("/tmp/live_code.png").unwrap();
        eprintln!("saved /tmp/live_code.png");
    }
}

#[cfg(test)]
mod layout_shrink_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Regression: `live-markdown-content` is a fixed-height flex column;
    /// every row element must be `flex_shrink_0` or blank/rule rows collapse
    /// to 0px on any document taller than the viewport (unclickable blank
    /// lines, caret overlapping the next row, lost paragraph spacing).
    #[gpui::test]
    fn long_doc_small_window_keeps_row_heights(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..12 {
            doc.push_str(&format!("# Heading {i}\n\n"));
            doc.push_str(
                "```rust\nfn a() {\n    let x = 1;\n    let y = 2;\n    println!(\"{x} {y}\");\n}\n```\n\n",
            );
            doc.push_str("some paragraph text to fill space and wrap around a bit here\n\n");
            doc.push_str("- item one\n- item two\n- item three\n\n");
            doc.push_str("---\n\n");
        }
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(400.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let _view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        let code = vc.debug_bounds("live-code-inner");
        assert!(
            code.is_some_and(|b| b.size.height > px(50.)),
            "code card collapsed on long doc: {code:?}"
        );
        // Blank rows must keep their 26px (they were 0px before the
        // flex_shrink_0 fix): clickable + visible caret row.
        let blank = vc.debug_bounds("live-blank-row");
        assert!(
            blank.is_some_and(|b| b.size.height > px(10.)),
            "blank row collapsed on long doc: {blank:?}"
        );
        // The scroll container must exist with a definite height.
        let content = vc.debug_bounds("live-markdown-content");
        assert!(
            content.is_some_and(|b| b.size.height > px(0.)),
            "content bounds missing: {content:?}"
        );
    }
}

#[cfg(test)]
mod scroll_to_caret_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Moving the caret below the viewport must scroll it into view
    /// (guise `ensure_cursor_visible` parity; the rows column tracks its
    /// children with the scroll handle and we `scroll_to_item` the caret's
    /// element).
    #[gpui::test]
    fn caret_below_viewport_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..30 {
            doc.push_str(&format!("paragraph {i} with some text\n\n"));
        }
        doc.push_str("final line\n");
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(300.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        // Before: at top (offset 0).
        let offset0 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert_eq!(offset0, 0.0, "initial scroll must be at top");

        // Focus + jump the caret to the last line.
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_end(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let offset1 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            offset1 < -50.0,
            "caret at doc end must scroll down (offset {offset1})"
        );

        // Jump back to the top: the caret's element scrolls back up.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_start(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let offset2 = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            offset2 > -50.0,
            "caret back at doc start must scroll up (offset {offset2})"
        );
    }

    /// 大文档行虚拟化：只建视口内 group 的元素（blink/主题刷新不再
    /// 全量布局绘制，主线程不再被大文档卡死）；caret 跳转仍把 caret 组
    /// 纳入窗口并滚动到位。
    #[gpui::test]
    fn large_doc_virtualizes_row_elements(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..300 {
            doc.push_str(&format!("# Heading {i}\n\npara {i} text here\n\n"));
        }
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(400.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();
        let (total, rendered, virtualized) =
            view.read_with(&vc, |v, _| v.last_virtualization_stats());
        assert!(virtualized, "300-heading doc must virtualize");
        assert!(total > 200, "expected hundreds of groups, got {total}");
        assert!(
            rendered < total,
            "only the viewport window may render ({rendered}/{total})"
        );
        assert!(rendered > 0, "viewport must render something");
        // Caret 跳到文末：caret 组必须进窗口，视图随之滚动。
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_end(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let (total2, rendered2, virtualized2) =
            view.read_with(&vc, |v, _| v.last_virtualization_stats());
        assert!(virtualized2, "must stay virtualized after caret jump");
        assert_eq!(total2, total, "group count must be stable");
        assert!(rendered2 < total2);
        let offset = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            offset < -50.0,
            "caret at doc end must scroll down (offset {offset})"
        );
    }

}

#[cfg(test)]
mod scroll_stability_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    struct Snap {
        /// Viewport-relative position of two fixed groups (one near the
        /// bottom window, one far above it): group_top_in_content -
        /// scroll_distance. With no input these must be bit-stable, else
        /// the user sees content jump (bottom-up jitter).
        pos_a: f32,
        pos_b: f32,
    }

    /// Diagnostic: scroll to bottom, then step upward like wheel events.
    /// No-input frames must keep every content point glued; scroll steps
    /// must move content by exactly the input delta.
    #[gpui::test]
    fn bottom_scroll_up_diagnostic(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..300 {
            doc.push_str(&format!("# Heading {i}\n\npara {i} text here\n\n"));
        }
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(400.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(&doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        // Fixed probe groups: A near the bottom window, B far above it.
        let (ga, gb) = (1190usize, 600usize);
        let snap = |view: &gpui::Entity<LiveMarkdownView>, vc: &mut gpui::VisualTestContext| -> Snap {
            view.read_with(vc, |v, _| {
                let off = f32::from(v.scroll.offset().y);
                Snap {
                    pos_a: v.virt_prefix.get(ga).copied().unwrap_or(-99999.0) + off,
                    pos_b: v.virt_prefix.get(gb).copied().unwrap_or(-99999.0) + off,
                }
            })
        };
        let force_frame = |view: &gpui::Entity<LiveMarkdownView>, vc: &mut gpui::VisualTestContext| {
            view.update(vc, |_, cx| cx.notify());
            vc.refresh().unwrap();
            vc.run_until_parked();
        };
        let assert_glued = |prev: &Snap, s: &Snap, ctx: &str| {
            assert!(
                (s.pos_a - prev.pos_a).abs() <= 1.0 && (s.pos_b - prev.pos_b).abs() <= 1.0,
                "content jumped with no input at {ctx}: posA {:.1}->{:.1} posB {:.1}->{:.1}",
                prev.pos_a, s.pos_a, prev.pos_b, s.pos_b
            );
        };
        // Settle onto the true bottom: `max_offset` lags layout by a
        // frame, so a single set-to-max can land above the bottom when the
        // total is still converging. Repeat until the offset reaches max.
        for _ in 0..6 {
            vc.refresh().unwrap();
            vc.run_until_parked();
            let done = view.read_with(&vc, |v, _| {
                let o = v.scroll.offset();
                let m = v.scroll.max_offset();
                (f32::from(o.y) + f32::from(m.y)).abs() <= 1.0
            });
            view.update(&mut vc, |v, _| {
                let m = v.scroll.max_offset();
                v.scroll.set_offset(gpui::point(px(0.), -m.y));
            });
            if done {
                break;
            }
        }
        vc.refresh().unwrap();
        vc.run_until_parked();
        let mut prev = snap(&view, &mut vc);
        view.read_with(&vc, |v, _| {
            let o = f32::from(v.scroll.offset().y);
            let m = f32::from(v.scroll.max_offset().y);
            assert!(
                (o + m).abs() <= 1.0,
                "test setup never reached bottom: off {o:.1} max {m:.1}"
            );
        });
        for k in 0..4 {
            force_frame(&view, &mut vc);
            let s = snap(&view, &mut vc);
            assert_glued(&prev, &s, &format!("bottom+f{k}"));
            prev = s;
        }
        for step in 0..12 {
            view.update(&mut vc, |v, _| {
                let o = v.scroll.offset();
                v.scroll.set_offset(gpui::point(o.x, o.y + px(80.)));
            });
            vc.refresh().unwrap();
            vc.run_until_parked();
            let s = snap(&view, &mut vc);
            // Scroll-up adds +80 to offset.y, so each group's
            // viewport-relative position must grow by exactly +80 (not jump).
            let d = s.pos_a - prev.pos_a;
            assert!(
                (d - 80.0).abs() <= 25.0,
                "step {step}: content moved {d:.1}px for 80px input (jump?)"
            );
            prev = s;
            for k in 0..2 {
                force_frame(&view, &mut vc);
                let f = snap(&view, &mut vc);
                assert_glued(&prev, &f, &format!("up{step}+f{k}"));
                prev = f;
            }
        }
    }
}

#[cfg(test)]
mod interaction_fallback_tests {
    use super::*;
    use gpui::point;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    const DOC: &str = "hello world text\n\nsecond paragraph here";

    fn setup(
        cx: &mut gpui::TestAppContext,
        doc: &str,
    ) -> (gpui::Entity<LiveMarkdownView>, gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(500.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        (view, vc)
    }

    /// B4: a click on live text is claimed by the LiveLine and lands a
    /// precise column.
    #[gpui::test]
    fn click_on_text_is_precise(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, "hello world text");
        let para = vc.debug_bounds("live-para").expect("para group");
        // Click 40px into the first paragraph's text.
        vc.simulate_mouse_down(
            point(para.left() + px(40.), para.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert_eq!(cursor.line, 0, "click in first paragraph");
        assert!(
            (1..=12).contains(&cursor.col),
            "x-precise column expected, got {}",
            cursor.col
        );
    }

    /// B4: clicks in dead zones (margins between rows, the blank row's
    /// padding) still land the cursor on the nearest line — no dead clicks.
    #[gpui::test]
    fn dead_zone_clicks_land_nearest_line(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, DOC);
        let blank = vc.debug_bounds("live-blank-row").expect("blank row");
        // 1) Click inside the blank row itself: its handler targets line 1.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        assert_eq!(view.read_with(&vc, |v, _| v.buffer.cursor()).line, 1);

        // 2) Click in the margin just above the blank (paragraph A's
        // my_3 margin — no row element owns it): the root fallback maps to
        // the nearest line.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.top() - px(6.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert!(
            cursor.line <= 1,
            "margin click should land on the blank or the paragraph above, got line {}",
            cursor.line
        );

        // 3) Click in the margin just below the blank (paragraph B's
        // margin): nearest is the blank or paragraph B.
        vc.simulate_mouse_down(
            point(blank.left() + px(100.), blank.bottom() + px(6.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let cursor = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert!(
            (1..=2).contains(&cursor.line),
            "margin click below the blank should land on line 1 or 2, got {}",
            cursor.line
        );
    }

    /// B5: dragging across the blank row keeps extending the selection
    /// (the drag fallback covers positions no LiveLine band contains).
    #[gpui::test]
    fn drag_across_blank_keeps_selecting(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, DOC);
        let blank = vc.debug_bounds("live-blank-row").expect("blank row");
        // Press on paragraph A, drag into the blank band, then into B.
        vc.simulate_mouse_down(
            point(blank.left() + px(10.), blank.top() - px(40.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.simulate_mouse_move(
            point(blank.left() + px(60.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let mid = view.read_with(&vc, |v, _| v.buffer.selection());
        assert!(
            mid.is_some_and(|(s, e)| s.line == 0 && e.line == 1),
            "drag into the blank should extend the selection to line 1, got {:?}",
            mid
        );
        vc.simulate_mouse_move(
            point(blank.left() + px(60.), blank.bottom() + px(40.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let end = view.read_with(&vc, |v, _| v.buffer.selection());
        assert!(
            end.is_some_and(|(s, e)| s.line == 0 && e.line == 2),
            "drag into paragraph B should extend the selection to line 2, got {:?}",
            end
        );
    }

    /// B6: Up/Down move by visual (soft-wrapped) rows with a sticky goal x.
    #[gpui::test]
    fn visual_row_movement(cx: &mut gpui::TestAppContext) {
        let long = "word ".repeat(60); // wraps to 3+ visual rows at 700px
        let doc = format!("{long}\nsecond line\n");
        let (view, mut vc) = setup(cx, &doc);
        let cursor = |vc: &gpui::VisualTestContext| {
            view.read_with(vc, |v, _| v.buffer.cursor())
        };
        // Start at the beginning of the wrapped paragraph.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 0, false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        // Down twice stays on source line 0 (visual rows 1 → 2).
        view.update(&mut vc, |v, cx| v.move_visual(true, false, cx));
        let c1 = cursor(&vc);
        assert_eq!(c1.line, 0, "first Down stays on the wrapped line");
        assert!(c1.col > 0, "column advanced to visual row 2");
        // Keep moving down within the wrapped line: columns only advance.
        let mut prev = c1.col;
        let mut downs = 0;
        while cursor(&vc).line == 0 && downs < 10 {
            view.update(&mut vc, |v, cx| v.move_visual(true, false, cx));
            downs += 1;
            let c = cursor(&vc);
            if c.line != 0 {
                break; // crossed to the next source line
            }
            assert!(c.col > prev, "Down must advance within the line: {c:?}");
            prev = c.col;
        }
        // It eventually crosses to the next source line.
        let c3 = cursor(&vc);
        assert_eq!(c3.line, 1, "repeated Down crosses to line 1");
        // Up goes back to the LAST visual row of line 0 (sticky goal x).
        view.update(&mut vc, |v, cx| v.move_visual(false, false, cx));
        let c4 = cursor(&vc);
        assert_eq!(c4.line, 0, "Up returns to line 0");
        assert!(c4.col > c1.col, "and lands on its last visual row");
    }

    /// C2: consecutive paragraph lines render as ONE group without
    /// inter-row margins (the preview joins them into one paragraph).
    #[gpui::test]
    fn paragraph_run_renders_as_one_block(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup(cx, "line one here\nline two here");
        let para = vc.debug_bounds("live-para").expect("para group");
        // Two 15px/1.75 rows ≈ 52.5px. The old per-row my_3 layout added a
        // 24px gap between them (≈76.5px); one block must stay near 2 rows.
        assert!(
            para.size.height < px(65.),
            "two joined paragraph lines must not carry inter-row margins: {para:?}"
        );
        assert!(
            para.size.height > px(45.),
            "two lines of text: {para:?}"
        );
        let _ = view;
    }

    /// 回归：点击实时编辑器内容视口之外的右侧兄弟面板（如终端）时，
    /// `LiveLine` 的窗口级全局鼠标监听不得认领该点击 —— 否则会
    /// `window.focus` 把焦点抢回 live 编辑器并 `stop_propagation`，
    /// 导致点击终端无法把焦点切过去。
    #[gpui::test]
    fn click_outside_content_viewport_keeps_sibling_focus(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        struct RightPane {
            focus: gpui::FocusHandle,
        }
        impl RightPane {
            fn new(cx: &mut Context<Self>) -> Self {
                Self {
                    focus: cx.focus_handle(),
                }
            }
        }
        impl gpui::Render for RightPane {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
                    .size_full()
                    .track_focus(&self.focus)
                    .debug_selector(|| "right-pane".into())
            }
        }

        struct SplitHost {
            live: gpui::Entity<LiveMarkdownView>,
            right: gpui::Entity<RightPane>,
        }
        impl gpui::Render for SplitHost {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .child(gpui::div().w(px(600.)).h_full().child(self.live.clone()))
                    .child(gpui::div().flex_1().h_full().child(self.right.clone()))
            }
        }

        let (host, vc) = cx.add_window_view(|_, cx| {
            let live = cx.new(|cx| LiveMarkdownView::with_text(DOC, cx));
            let right = cx.new(RightPane::new);
            SplitHost { live, right }
        });
        let live = host.read_with(vc, |h, _| h.live.clone());
        let right_focus = host.read_with(vc, |h, cx| h.right.read(cx).focus.clone());
        vc.update(|window, cx| {
            window.focus(&live.read(cx).focus_handle(), cx);
        });
        vc.refresh().unwrap();
        vc.run_until_parked();

        let para = vc.debug_bounds("live-para").expect("para group");
        let right = vc.debug_bounds("right-pane").expect("right pane");
        // 点击右侧面板：x 在 live 内容视口之外，y 落在首行带内 —— 修复前
        // LiveLine 的全局监听会把它当成“行尾空白”认领并抢走焦点。
        vc.simulate_mouse_down(
            point(right.left() + px(20.), para.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        vc.update(|window, _| {
            assert!(
                right_focus.is_focused(window),
                "点击兄弟面板必须把焦点切过去，live 编辑器不得抢回"
            );
        });
    }
}

#[cfg(test)]
mod setext_parity_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;
    use crate::editor::markdown_live::reveal::Reveal;

    /// Live setext classification must match pulldown-cmark (the read-only
    /// preview's parser) on the constructs where setext, rules, and tables
    /// interact — the precedence was chosen from pulldown's behavior.
    #[test]
    fn setext_matches_pulldown() {
        use pulldown_cmark::{Event, Options, Parser, Tag};
        for (doc, expect_h) in [
            ("Title\n===\n", vec![(0, 1)]),
            ("Sub\n---\n", vec![(0, 1)]),
            ("a\nb\n===\n", vec![(0, 2)]),
            // Single-column delimiter after a word: setext wins (h2).
            ("a\n---\n", vec![(0, 1)]),
            // Multi-column delimiter row: a table, not a heading.
            ("a | b\n--- | ---\n", vec![]),
            // Blank line between: no setext.
            ("a\n\n---\n", vec![]),
        ] {
            let mut opts = Options::empty();
            opts.insert(Options::ENABLE_TABLES);
            let mut pulldown_ranges: Vec<(usize, usize)> = Vec::new();
            let mut starts = vec![0usize];
            let mut off = 0usize;
            for l in doc.split('\n') {
                off += l.len() + 1;
                starts.push(off);
            }
            let line_of = |b: usize| starts.iter().rposition(|&s| s <= b).unwrap_or(0);
            let mut open: Option<usize> = None;
            for (ev, range) in Parser::new_ext(doc, opts).into_offset_iter() {
                match ev {
                    Event::Start(Tag::Heading { .. }) => open = Some(range.start),
                    Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                        if let Some(s) = open.take() {
                            pulldown_ranges.push((line_of(s), line_of(range.end.saturating_sub(1))));
                        }
                    }
                    _ => {}
                }
            }
            let lines: Vec<String> = doc.split('\n').map(str::to_string).collect();
            let rows = plan_document(
                &lines,
                Reveal {
                    cursor: Pos::new(0, 0),
                    selection: None,
                    focused: false,
                    read_only: false,
            ..Default::default()
        },
            );
            let mut live_ranges: Vec<(usize, usize)> = Vec::new();
            let mut open: Option<usize> = None;
            for row in &rows {
                let is_heading = matches!(row.block, Block::Heading { .. })
                    && !matches!(row.plan.kind, RowKind::Setext);
                let is_underline = matches!(row.block, Block::SetextUnderline { .. });
                if is_heading && open.is_none() {
                    open = Some(row.line);
                }
                if is_underline && let Some(s) = open.take() {
                    live_ranges.push((s, row.line));
                }
                if !is_heading && !is_underline && open.is_some() {
                    live_ranges.push((open.unwrap(), open.unwrap()));
                    open = None;
                }
            }
            if let Some(s) = open.take() {
                live_ranges.push((s, s));
            }
            assert_eq!(
                live_ranges, expect_h,
                "{doc:?}: live setext heading ranges {:?} != expected",
                live_ranges
            );
            assert_eq!(
                pulldown_ranges, expect_h,
                "{doc:?}: pulldown heading ranges {:?} != expected (live drift guard)",
                pulldown_ranges
            );
        }
    }
}

#[cfg(test)]
mod e2e_smoke_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// Combined document exercising every fixed feature at once: setext
    /// headings, paragraph runs, a code card (edited in place), ordered
    /// numbering, blank rows, and scroll-to-caret.
    #[gpui::test]
    fn combined_document_smoke(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let doc = "Title\n===\n\nintro one\nintro two\n\n```rust\nfn main() {}\n```\n\n1. first\n2. second\n\nfinal\n";
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(700.), px(240.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();

        // Everything renders: code card, table-free, setext gap row.
        assert!(
            vc.debug_bounds("live-code-inner").is_some_and(|b| b.size.height > px(20.)),
            "code card renders"
        );
        // Focus + edit inside the code: card stays.
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(7, 11, false)); // inside fn main() {}
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        assert!(
            vc.debug_bounds("live-code-inner").is_some(),
            "card stays while editing code"
        );
        // Jump to the end: scrolls into view.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.doc_end(false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        assert!(
            view.read_with(&vc, |v, _| v.scroll_offset_y()) < -10.0,
            "doc end scrolls into view"
        );
        // Click the blank above "final" via the fallback: lands a line.
        let blank = vc.debug_bounds("live-blank-row").expect("blank");
        vc.simulate_mouse_down(
            gpui::point(blank.left() + px(50.), blank.top() + px(13.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let line = view.read_with(&vc, |v, _| v.buffer.cursor().line);
        assert!(line >= 10, "blank click lands near the end, got line {line}");
    }
}

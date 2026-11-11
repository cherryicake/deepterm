//! [`LiveMarkdownView`]: the Obsidian-style live-preview markdown editor
//! entity. Owns a [`LiveBuffer`] (single truth), plans rows with reveal,
//! and renders decorations + [`LiveLine`](super::element::LiveLine) text.
//!
//! Rewritten for deepterm's GPUI fork; interaction semantics ported from
//! guise `markdown/editor.rs` (`on_key`/`on_mouse_down`/checkbox/link).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, ClickEvent, ClipboardEntry, ClipboardItem, Context, Entity,
    EntityInputHandler, EventEmitter, ExternalPaths, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, ScrollHandle, ShapedLine, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TextRun, UTF16Selection, Window, div, px, rems, svg,
};

use crate::assets::Icon;
use crate::components::input::{
    blink_cursor::{BlinkCursor, CURSOR_WIDTH},
    cursor::CursorShape,
    search::{SearchPanel, SearchTarget},
};
use crate::components::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use crate::editor::image_viewer::is_image_file;
use crate::editor::markdown::{Copy, MarkdownStyle, SelectAll};
use crate::editor::markdown_element::MarkdownElementBuilder;
use crate::editor::markdown_live::block::{
    Block, split_table_row, table_cell_spans, table_delimiter_cols,
};
use crate::editor::markdown_live::buffer::LiveBuffer;
use crate::editor::markdown_live::element::{
    LiveColors, LiveLine, apply_find_marks, apply_selection, apply_task_done, row_base_style,
    runs_for_row,
};
use crate::editor::markdown_live::highlight::HighlightCache;
use crate::editor::markdown_live::images::{assets_dir, format_extension, unique_file_name};
use crate::editor::markdown_live::interactions::{
    OrderedCounter, SLASH_COMMANDS, backspace_marker, cell_content_cols, classify_alone,
    insert_link, line_selection, list_cols, on_enter, on_tab, pos_of_offset, slash_items,
    slash_query_for, table_cell_at, table_cell_bounds, table_goto_cell, table_insert_row_below,
    table_last_row_line, table_nav, table_row_line, toggle_task, toggle_wrap,
};
use crate::editor::markdown_live::layout::{
    RowKind, byte_for_col, col_for_byte, plan_cell, src_for_vis, vis_for_src,
};
use crate::editor::markdown_live::mermaid::{
    MAX_DISPLAY_W, MermaidBody, MermaidColors, MermaidState, ZOOM_MAX, ZOOM_STEP, zoom_label,
    zoomed_size,
};
use crate::editor::markdown_live::model::{Line, Pos};
use crate::editor::markdown_live::reveal::{
    CalloutKind, Classified, PlannedRow, Reveal, plan_range, refdefs_for_lines, reveal_set,
    strip_callout_prefix,
};
use crate::editor::markdown_live::scrollbar::{
    BAR_THICKNESS, ScrollBarMap, ScrollBarState, h_bar_strip, scroll_bar, table_natural_width,
};
use crate::editor::md_image::MAX_IMAGE_BYTES;
use crate::theme::ActiveTheme;
use ropey::Rope;

#[derive(Debug, Clone)]
pub enum LiveViewEvent {
    /// The document text changed. Carries only the new [`TextVersion`] — the
    /// text itself is fetched from the view on demand (`text()`), so a
    /// keystroke no longer pays a full-document `join` just to build an event
    /// nobody may consume. Subscribers that need the text (dirty check, save)
    /// read it explicitly; the shadow-`InputState` sync is debounced.
    Change {
        version: u64,
    },
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
    /// Planned rows, one `Rc` per row. The per-row indirection is what makes
    /// incremental planning possible: re-planning a single row replaces one
    /// `Rc` instead of rebuilding (and re-allocating) the whole plan. The
    /// outer `Rc` lets `render` hold the plan while mutating `self`; because
    /// the previous frame's clone is dropped by then, `Rc::make_mut` is O(1)
    /// on the common path (it only copies the pointer table).
    planned: Rc<Vec<Rc<PlannedRow>>>,
    /// Per-row inputs the current `planned` was computed from, for the
    /// incremental diff: line text (compared by `Arc` pointer), block kind,
    /// and reveal flag. Cheap to keep (`lines × 8 B` for the pointer table)
    /// and turns a keystroke's re-plan into "the rows that actually changed".
    plan_input_lines: Vec<Line>,
    plan_input_blocks: Vec<Block>,
    plan_revealed: std::collections::BTreeSet<usize>,
    /// Reference definitions the current `planned` was built with, compared by
    /// value (refdefs are usually empty, so this is free) — a content change
    /// means link resolution changed and every row may need re-planning.
    plan_refs: HashMap<String, String>,
    /// Cursor-independent document geometry (grouping, buckets, ordered
    /// numbers, mermaid block list), cached by text version. See
    /// [`PlanGeometry`].
    planned_geometry: Rc<PlanGeometry>,
    planned_geometry_version: u64,
    /// Monotonic revision of `planned_geometry`. Bumped only when the geometry
    /// is actually rebuilt; a text-only edit keeps the same group list, so the
    /// O(groups) virtualization prefix/rates can be reused.
    geometry_revision: u64,
    /// `geometry_revision` the last `virtual_window` was built from.
    virt_geometry_revision: u64,
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
    copied_code_blocks: HashSet<usize>,
    /// Mermaid 图渲染缓存 +「预览 | 源码」按钮状态（缓存键 = 块源码 + 主题指纹，
    /// 后台 + 防抖渲染；详见 `mermaid` 模块）。
    mermaid: MermaidState,
    /// 表格 / mermaid 卡片的横向滚动状态（`块首行 → 拖拽态 + 滚动位置`）。
    /// 滚动位置必须跨帧保持，所以存在视图上（见 `scrollbar` 模块）。
    h_scroll: ScrollBarMap,
    /// 编辑器自身的纵向滚动条状态（与代码编辑器同款，自绘常显）。
    v_scroll: Option<Entity<ScrollBarState>>,
    /// 窗口失活监听（惰性注册，见 [`Self::ensure_activation_observer`]）。
    activation_observer: Option<Subscription>,
    /// 「全宽显示」的块集合：表格/mermaid 卡片在 780px 窄栏里太挤时，
    /// 点卡片上的「全宽」按钮即可占据整个编辑器宽度（Typora 的宽表体验）。
    /// 键是块首行号（与 `h_scroll` 同款，文本编辑会让行号漂移，收帧时按
    /// 当前块集合裁剪）。
    wide_blocks: HashSet<usize>,
    /// Bring the caret's row into view on the next render (set by every
    /// edit/move; consumed by the row-precise scroll in `render`).
    scroll_to_cursor: bool,
    /// The caret row's rect **from the last completed frame** (document space),
    /// as `(caret_line, text_version, doc_top, doc_bottom)`, kept across the
    /// per-frame `row_layouts` clear.
    ///
    /// `row_layouts` is emptied at the top of `render` and repopulated during
    /// paint, so it is always empty at the point the scroll decision is made.
    /// This field carries the previous frame's measurement forward, which is
    /// what lets the element-level `scroll_to_item` be skipped when the caret
    /// row is already visible (the common typing case).
    ///
    /// Both stamps matter: the text version catches an edit that reflowed the
    /// row, and the caret line catches a **cursor move**, which changes which
    /// row this rect describes without touching the text at all.
    caret_row_rect: Option<(usize, u64, f32, f32)>,
    /// Remaining post-layout correction passes for the current scroll request.
    /// Bounds `schedule_caret_scroll_correction` to
    /// [`CARET_SCROLL_CORRECTION_PASSES`] frames.
    caret_scroll_passes: u8,
    /// The table whose toolbar was revealed by the gesture currently in
    /// progress, as `(start_line, was_visible_at_press)`.
    ///
    /// A pointer gesture is hit-tested against the layout that existed when it
    /// started. The table toolbar is laid out at the table's **top**, so
    /// revealing it mid-gesture (which happens the instant the cursor enters
    /// the table) makes the table taller and slides every row down — the rows
    /// then move out from under the pointer, and a drag that began on one row
    /// continues against the row above it. Freezing the decision for the
    /// duration of the gesture keeps the geometry the pointer is being mapped
    /// against stable; it re-evaluates on the next gesture.
    toolbar_frozen: Option<(usize, bool)>,
    /// Source-line range per rendered row element (dead-zone hit fallback:
    /// clicks between/around rows map to the nearest element, then to a
    /// line within it via the recorded layouts).
    row_targets: Vec<(usize, usize)>,
    /// Last frame's per-line text layouts, recorded during `LiveLine`
    /// paint. Powers visual-row (soft-wrap) movement and the dead-zone
    /// click/drag fallbacks.
    row_layouts: std::collections::HashMap<usize, RowLayoutInfo>,
    /// 每格的帧内命中记录（键 = 源行号 + 格列，见 [`CellHit`]）：
    /// cell LiveLine 在 paint 时写入，`cell_div` 的元素级按下/移动监听
    /// 用它把窗口坐标映射回本格行列（spec #6 跨格拖拽）。
    cell_hits: std::collections::HashMap<(usize, usize), CellHit>,
    /// Fingerprint of the style inputs that bake colors into the highlight
    /// cache; a change (theme switch) clears it.
    hl_style_key: u64,
    blink: Entity<BlinkCursor>,
    cursor_shape: CursorShape,
    cursor_blink: bool,
    blink_on: bool,
    /// 光标四角弹簧动画状态（跨帧存活；光标行的 `LiveLine::paint` 每帧
    /// 只推进一次）。
    cursor_animator: crate::components::cursor_animation::CursorAnimator,
    /// Typora 风格大纲侧栏是否展开（`Cmd+Shift+O` / 状态条切换）。
    show_outline: bool,
    /// 本文档路径（编辑器传入）：图片粘贴/拖拽按 Typora 存入 `<stem>.assets/`。
    doc_path: Option<PathBuf>,
    /// 相对图片引用的兜底根（项目/工作区根，由近到远）。
    ///
    /// 文档目录始终是**第一**基准；这里只在文档目录找不到时兜底，
    /// 让 `docs/foo.md` 引用仓库根的 `assets/icon.png` 也能显示
    /// （VS Code 靠 `getWorkspaceFolder` 覆盖同一场景）。
    image_roots: Vec<PathBuf>,
    /// 源码模式（`Cmd+/` / 状态条切换）：全文档显示源码标记（Typora 源码模式）。
    show_source: bool,
    /// 文档级宽度：`true` = 全宽（所有行脱离 780px 阅读窄栏、占满编辑器宽度），
    /// `false` = 标宽。状态条「全宽」按钮切换，是**视图偏好**：
    /// 换文档（`set_text`）不重置，与 `show_source` / `show_outline` 一致。
    wide_doc: bool,
    /// 撤销/重做进行中：跳过一次表格自动重排（spec #13，历史保持原样）。
    skip_reformat: bool,
    /// 表格工具栏「尺寸」弹层是否展开（Typora 的 resize popover，spec #14）。
    table_resize_open: bool,
    /// 工具栏 More▾ 手动打开的表格菜单（与右键菜单共用条目，spec #14）。
    table_menu: Option<Entity<PopupMenu>>,
    /// 上述菜单的弹出位置。
    table_menu_position: Option<gpui::Point<Pixels>>,
    /// 上述菜单的关闭订阅（必须持有，否则立刻失效）。
    table_menu_subscription: Option<Subscription>,
    /// 菜单已关、待把焦点还给编辑器（下一帧 render 落实，见 `close_table_menu`）。
    table_menu_restore_focus: bool,
    /// Slash 菜单高亮项；`slash_suppress` 为 Esc 忽略中的查询（内容变化即恢复）。
    slash_index: usize,
    slash_suppress: Option<String>,
    /// 代码编辑器同款 `SearchPanel`（`Cmd+F` 懒建）：匹配引擎、面板 UI、
    /// 跳转与替换逻辑全部复用，不再自实现（`SearchTarget` 桥接行列缓冲）。
    search_panel: Option<Entity<SearchPanel<LiveMarkdownView>>>,
    /// 已登记待同步的文本版本（`Some` = 防抖中）。避免连续击键起多个任务。
    search_sync_version: Option<u64>,
    _subscriptions: Vec<Subscription>,
    /// 行虚拟化（大文档）：每帧只建视口内 group 的元素，其余以占位撑高。
    /// group 粒度保证代码卡片/引用/表格不被拦腰截断；小文档走全量老路。
    /// 上一帧实测的 group 高（`scroll.bounds_for_item` 回填；跨帧保留，
    /// 缺失时用实测均值校准后的行数估算，绘制后逐帧自我修正）。
    group_heights: std::collections::HashMap<usize, f32>,
    heights_version: u64,
    /// Whether `virt_prefix` must be recomputed this frame. The prefix sum
    /// covers every group (O(groups)), so on an idle frame — a blink tick, or
    /// a scroll that did not move the window — the previous one is still
    /// exact and is reused. Set by anything that changes `group_heights` or
    /// the group list.
    prefix_dirty: bool,
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
    /// 文档级图片缓存（本地/远程解析 + 后台加载），与只读预览共用同一实现
    /// （`crate::editor::md_image::ImageCache`）。渲染路径只查缓存，首帧占位，
    /// 加载完 notify 重绘；本地文件 mtime/大小变化时自动失效。
    image_cache: Entity<crate::editor::md_image::ImageCache>,
    /// IME 组字区间（**字节**偏移，非 UTF-16）：组字期间文本恒等于该区间，
    /// 平台按 UTF-16 查询时经 [`Self::byte_to_utf16`] 换算。`None` = 无组字。
    ime_marked_range: Option<std::ops::Range<usize>>,
    /// 本帧是否已向 `Window` 注册 `EntityInputHandler`（`render` 每帧复位；
    /// 光标行每帧认领，其余行仅在未注册时兜底一次）。
    pub(crate) input_registered: bool,
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
        let image_cache = cx.new(|_| crate::editor::md_image::ImageCache::new());
        let _subscriptions = vec![
            cx.observe(&blink, |_, _, cx| cx.notify()),
            // 图片在后台加载完成时由缓存实体 notify，这里转发成整视图重绘。
            cx.observe(&image_cache, |_, _, cx| cx.notify()),
        ];
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
            planned: Rc::new(Vec::new()),
            plan_input_lines: Vec::new(),
            plan_input_blocks: Vec::new(),
            plan_revealed: std::collections::BTreeSet::new(),
            plan_refs: HashMap::new(),
            planned_geometry: Rc::new(PlanGeometry {
                groups: Vec::new(),
                group_of_row: Vec::new(),
                ginfo: Vec::new(),
                display_numbers: Vec::new(),
                has_table: false,
                mermaid_blocks: Vec::new(),
                wide_anchors: Vec::new(),
            }),
            planned_geometry_version: u64::MAX,
            geometry_revision: 0,
            virt_geometry_revision: 0,
            planned_classified: Classified {
                blocks: Vec::new(),
                lang_before: Vec::new(),
                changed: 0..0,
                states: Vec::new(),
            },
            planned_classified_version: u64::MAX,
            planned_refs: Rc::new(HashMap::new()),
            planned_refs_version: u64::MAX,
            copied_code_blocks: HashSet::new(),
            mermaid: MermaidState::new(),
            h_scroll: ScrollBarMap::default(),
            v_scroll: None,
            activation_observer: None,
            wide_blocks: HashSet::new(),
            scroll_to_cursor: false,
            caret_row_rect: None,
            caret_scroll_passes: 0,
            toolbar_frozen: None,
            row_targets: Vec::new(),
            row_layouts: std::collections::HashMap::new(),
            cell_hits: std::collections::HashMap::new(),
            hl_style_key: 0,
            blink,
            cursor_shape: CursorShape::Bar,
            cursor_blink: true,
            blink_on: false,
            cursor_animator: crate::components::cursor_animation::CursorAnimator::new(),
            show_outline: false,
            doc_path: None,
            image_roots: Vec::new(),
            show_source: false,
            wide_doc: false,
            skip_reformat: false,
            table_resize_open: false,
            table_menu: None,
            table_menu_position: None,
            table_menu_subscription: None,
            table_menu_restore_focus: false,
            slash_index: 0,
            slash_suppress: None,
            search_panel: None,
            search_sync_version: None,
            _subscriptions,
            group_heights: std::collections::HashMap::new(),
            heights_version: u64::MAX,
            prefix_dirty: true,
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
            image_cache,
            ime_marked_range: None,
            input_registered: false,
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

    /// 文档落盘路径：图片粘贴/拖拽以此定位 `<stem>.assets/`（Typora 布局），
    /// 同时也是相对图片引用的第一基准（取其父目录）。
    pub fn doc_path(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if !path.as_os_str().is_empty() {
            self.doc_path = Some(path.to_path_buf());
        }
        self
    }

    /// 相对图片引用的兜底根（项目/工作区根，由近到远）。
    ///
    /// 文档目录永远优先；这些目录只在文档目录里找不到同名图片时才尝试，
    /// 用来兜住「笔记在子目录、图片放在仓库根」这类 VS Code 里能显示、
    /// 纯文档目录相对却找不到的引用。
    pub fn image_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.image_roots = roots;
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

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.pause_caret(cx);
        self.hl.clear();
        self.copied_code_blocks.clear();
        // 按块首行记账的 UI 状态只对**上一份文档**有意义：新文本在同一个行号
        // 上可能是另一个块，留着旧状态会把它错误地撑成全宽/缩放。收帧裁剪挡不住
        // 这种情况（行号仍命中某个锚点），只有这里能兜住。
        self.wide_blocks.clear();
        self.mermaid.clear_block_state();
        self.h_scroll.clear();
        // 同理：图片缓存以**解析后的身份**（绝对路径 / 远程 URL）为键持有解码后的
        // 位图（`Arc<gpui::Image>`，单张可达数 MB），键空间是「本文档出现过的每个
        // 图片」——不清则每次 set_text（切文档/外部重载）都往上一份文档的残留里
        // 再叠一份，反复编辑长会话下只增不减。
        //
        // 代价是每次 set_text 后首帧要重新读盘 + 解码本文档的所有图片
        // （外部重载 / stale-while-revalidate 刷新都会触发 set_text）。
        // 只清「新文档里不再出现的 URL」不解决问题：重载时图片大多仍在，
        // 却因内容变化必须重解码；而真正的大头（上一个文档的残留）全都会被清掉。
        self.image_cache.update(cx, |cache, cx| cache.clear(cx));
        self.scroll_to_cursor = true;
        self.emitted_version = self.buffer.text_version();
        if let Some(panel) = self.search_panel.clone() {
            let text = Rope::from(self.buffer.text());
            panel.update(cx, |p, _| p.sync_text(&text));
        }
        cx.emit(LiveViewEvent::Change {
            version: self.buffer.text_version(),
        });
        cx.notify();
    }

    pub fn mark_saved(&mut self) {
        self.buffer.mark_saved();
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    /// O(1) dirty check: `false` means "no text mutation since the last save /
    /// load" (see [`LiveBuffer::is_dirty_fast`]). Used by the editor's
    /// per-keystroke tab-marker update, which must not join the document.
    pub fn is_dirty_fast(&self) -> bool {
        self.buffer.is_dirty_fast()
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
            self.blink
                .update(cx, |b, cx| b.set_blink_enabled(blink, cx));
            cx.notify();
        }
    }

    fn pause_caret(&mut self, cx: &mut Context<Self>) {
        if self.blink_on {
            self.blink.update(cx, |b, cx| b.pause_blinking(cx));
        }
    }

    /// 本帧是否应当绘制光标。
    ///
    /// 比单纯的 `blink.visible()` 宽：淡入窗口内后者仍为 false（相位尚未
    /// 翻转），但透明度已大于 0——此时必须绘制，否则淡入会变成「到点突然
    /// 出现」。
    fn should_paint_caret(&self, window: &Window, cx: &App) -> bool {
        if self.read_only || !self.focus.is_focused(window) || !window.is_window_active() {
            return false;
        }
        let blink = self.blink.read(cx);
        blink.visible() || blink.needs_repaint()
    }

    /// 光标当前的绘制不透明度（0.0 = 完全隐藏，1.0 = 实心）。
    ///
    /// 与终端 / 代码编辑器一致：闪烁相位末尾 100ms 线性淡出 / 淡入。
    /// 只读、失焦、窗口非活动时恒为 1.0（由 [`Self::should_paint_caret`]
    /// 决定是否绘制）。
    fn caret_opacity(&self, window: &Window, cx: &App) -> f32 {
        if self.read_only || !self.focus.is_focused(window) || !window.is_window_active() {
            return 1.0;
        }
        self.blink.read(cx).opacity()
    }

    /// 推进光标弹簧一帧，返回本帧四边形与是否仍在运动。
    ///
    /// 由光标行的 `LiveLine::paint` 调用（那里才有光标几何）。
    pub(crate) fn step_caret_animation(
        &mut self,
        target: crate::components::cursor_animation::CursorTarget,
        reduce_motion: bool,
        now: std::time::Instant,
    ) -> (
        Option<crate::components::cursor_animation::CursorQuad>,
        bool,
    ) {
        if reduce_motion {
            self.cursor_animator.reset();
        }
        let quad = self.cursor_animator.step(Some(target), now);
        (quad, self.cursor_animator.is_animating())
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
            None => (std::env::temp_dir().join("deepterm-md-images"), false),
        };
        std::fs::create_dir_all(&dir).ok()?;
        let name = unique_file_name(&dir, "image", ext);
        std::fs::write(dir.join(&name), bytes).ok()?;
        if relative {
            Some(format!("{}/{name}", dir.file_name()?.to_str()?))
        } else {
            Some(dir.join(name).to_string_lossy().to_string())
        }
    }

    /// 单个图片文件导入 assets（拖拽 / Finder 拷贝），返回引用名。
    fn import_image_file(&self, src: &Path) -> Option<String> {
        if !is_image_file(src) {
            return None;
        }
        let ext = src.extension()?.to_str()?.to_ascii_lowercase();
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
        let refs: Vec<String> = paths
            .iter()
            .filter_map(|p| self.import_image_file(p))
            .collect();
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
            self.search_panel = Some(SearchPanel::new(cx.entity().downgrade(), window, cx));
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

    /// UTF-16 偏移 → 字节偏移：floor 到所在字符（代理对中间 → 代理对起点），
    /// 越界返回 `text.len()`。平台 IME 的 range 全是 UTF-16，落库前必须换算。
    fn utf16_to_byte(text: &str, off: usize) -> usize {
        let mut u = 0usize;
        for (i, ch) in text.char_indices() {
            if off < u + ch.len_utf16() {
                return i;
            }
            u += ch.len_utf16();
        }
        text.len()
    }

    /// 字节偏移 → UTF-16 偏移：floor 到字符边界，越界 clamp 到末尾。
    fn byte_to_utf16(text: &str, off: usize) -> usize {
        let mut off = off.min(text.len());
        while off > 0 && !text.is_char_boundary(off) {
            off -= 1;
        }
        text[..off].encode_utf16().count()
    }

    /// [`Pos`] → 全文字节偏移（[`pos_of_offset`] 的逆，`search_caret_offset`
    /// 的推广）。行号越界时 clamp 到文档末尾。
    fn offset_of_pos(lines: &[Line], pos: Pos) -> usize {
        let mut off = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i == pos.line {
                return off + byte_for_col(line, pos.col);
            }
            off += line.len() + 1;
        }
        // 越界：循环多算了末行的换行符（空文档为 0，saturating 兜住）。
        off.saturating_sub(1)
    }

    /// IME 请求的替换区间 → 全文字节区间 `(start, end)`：显式 UTF-16
    /// 区间（平台权威）→ 组字区间（字节，直接可用）→ 选区/光标。
    fn resolve_input_range(&self, range: Option<std::ops::Range<usize>>) -> (usize, usize) {
        if let Some(r) = range {
            let doc = self.buffer.text();
            let s = Self::utf16_to_byte(&doc, r.start);
            let e = Self::utf16_to_byte(&doc, r.end);
            return (s.min(e), s.max(e));
        }
        if let Some(r) = self.ime_marked_range.clone() {
            return (r.start, r.end);
        }
        match self.buffer.selection() {
            Some((s, e)) => (
                Self::offset_of_pos(self.buffer.lines(), s),
                Self::offset_of_pos(self.buffer.lines(), e),
            ),
            None => {
                let off = self.search_caret_offset();
                (off, off)
            }
        }
    }

    /// 把模型选区预置到全文字节区间 `[start, end)`（IME 替换前定位，让
    /// 后续 `insert`/`delete_selection` 走标准选区语义）。折叠区间只放
    /// 光标；已在目标位置时**不动**（`move_to` 会打断撤销合并，逐字输入
    /// 必须保持一步撤销）。
    fn set_range_selection(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        let s = pos_of_offset(self.buffer.lines(), start);
        if start == end {
            if self.buffer.cursor() != s || self.buffer.selection().is_some() {
                self.edit(cx, |m| m.move_to(s.line, s.col, false));
            }
            return;
        }
        let e = pos_of_offset(self.buffer.lines(), end);
        self.edit(cx, |m| {
            m.move_to(s.line, s.col, false);
            m.move_to(e.line, e.col, true);
        });
    }

    /// IME 候选框/组字下划线定位：某行的**行内字节** → 窗口坐标 bounds。
    /// 表格行走格命中记录（内容包含该字节的格）；普通行走 `row_layouts`；
    /// 跨行或未渲染时退回 `element_bounds`（平台只要求一个锚点）。
    fn ime_point_bounds(
        &self,
        line: usize,
        in_line: usize,
        element_bounds: gpui::Bounds<Pixels>,
    ) -> gpui::Bounds<Pixels> {
        // 表格行：内容区间是闭区间（空格格为退化点），命中即用本格排版。
        if self.cell_hits.keys().any(|(l, _)| *l == line) {
            let hit = self
                .cell_hits
                .iter()
                .find(|((l, _), hit)| {
                    *l == line && in_line >= hit.content.start && in_line <= hit.content.end
                })
                .map(|(_, hit)| hit);
            let Some(hit) = hit else {
                return element_bounds;
            };
            let vis = vis_for_src(&hit.segs, in_line).min(hit.text_len);
            let pos = hit
                .layout
                .position_for_index(vis)
                .or_else(|| (hit.text_len == 0).then_some(hit.text_bounds.origin));
            let Some(pos) = pos else {
                return element_bounds;
            };
            let tb = hit.text_bounds;
            return gpui::Bounds::new(
                gpui::point(pos.x, tb.top()),
                gpui::size((tb.right() - pos.x).max(px(0.)), tb.size.height),
            );
        }
        // 普通行：可见字节 → 字形位置。
        if let (Some(info), Some(row)) = (
            self.row_layouts.get(&line),
            self.planned.iter().find(|r| r.line == line),
        ) {
            let vis = vis_for_src(&row.plan.segs, in_line).min(info.text_len);
            let pos = info
                .layout
                .position_for_index(vis)
                .or_else(|| (info.text_len == 0).then_some(info.layout.bounds().origin));
            if let Some(pos) = pos {
                let b = info.layout.bounds();
                return gpui::Bounds::new(
                    gpui::point(pos.x, b.top()),
                    gpui::size((b.right() - pos.x).max(px(0.)), b.size.height),
                );
            }
        }
        element_bounds
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

    /// Scroll by `dy` pixels (negative scrolls down). Diagnostics/bench helper:
    /// exercises the same `ScrollHandle` offset mutation the wheel listener
    /// performs, so a benchmark measures a real scroll frame.
    pub fn scroll_by_y(&mut self, dy: f32, cx: &mut Context<Self>) {
        let cur = self.scroll.offset();
        self.scroll.set_offset(gpui::point(cur.x, cur.y + px(dy)));
        cx.notify();
    }

    /// Insert `text` at the cursor through the full editing path
    /// (`insert` → `after_edit`: table re-format gate, search sync, `Change`
    /// emission, notify). Bench/diagnostic helper.
    pub fn insert_text_for_bench(&mut self, text: &str, cx: &mut Context<Self>) {
        self.type_text(text, false, cx);
    }

    /// Delete one char before the cursor through the full editing path.
    /// Bench/diagnostic helper (pairs with [`Self::insert_text_for_bench`] to
    /// keep a benchmark's document size stable across iterations).
    pub fn backspace_for_bench(&mut self, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.edit(cx, |m| {
            m.backspace();
        });
        self.after_edit(cx);
    }

    /// Record a row's layout from `LiveLine::paint` (element.rs): powers
    /// visual-row movement and the dead-zone hit fallbacks.
    pub(crate) fn record_row_layout(
        &mut self,
        line: usize,
        layout: gpui::TextLayout,
        text_len: usize,
        doc_top: f32,
        doc_bottom: f32,
    ) {
        // Carry the caret row's rect across frames: `row_layouts` is cleared at
        // the top of `render`, so the scroll decision (which runs before paint)
        // needs a separately retained copy to know whether a scroll is needed.
        // Stamped with the caret line and text version so an edit or a cursor
        // move invalidates it.
        if line == self.buffer.cursor().line {
            self.caret_row_rect = Some((line, self.buffer.text_version(), doc_top, doc_bottom));
        }
        self.row_layouts.insert(
            line,
            RowLayoutInfo {
                layout,
                text_len,
                doc_top,
                doc_bottom,
            },
        );
    }

    /// 记录某格的命中信息（cell LiveLine `paint` 调用；纯字段写入，不
    /// notify —— 与 [`Self::record_row_layout`] 同款约定）。
    pub(crate) fn record_cell_hit(&mut self, key: (usize, usize), hit: CellHit) {
        self.cell_hits.insert(key, hit);
    }

    /// `LiveLine::paint` 认领本帧 `EntityInputHandler` 注册权（中文 IME 等
    /// 平台文本输入的唯一写路径）：焦点在本视图，且**本行是光标行**或
    /// 本帧尚未注册过时返回焦点句柄并置位；`render` 每帧复位
    /// [`Self::input_registered`]。光标行每帧注册，保证 IME 候选框/组字
    /// 下划线贴着光标行；其余行只在首帧兜底一次（输入法在首帧就可输入）。
    pub(crate) fn claim_input_registration(
        &mut self,
        is_caret_line: bool,
        window: &Window,
    ) -> Option<FocusHandle> {
        if !self.focus.is_focused(window) {
            return None;
        }
        if is_caret_line || !self.input_registered {
            self.input_registered = true;
            Some(self.focus.clone())
        } else {
            None
        }
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
    pub fn edit<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::editor::markdown_live::model::EditorModel) -> R,
    ) -> R {
        self.scroll_to_cursor = true;
        self.pause_caret(cx);
        let out = self.buffer.edit(f);
        cx.notify();
        out
    }

    /// [`Self::edit`] for **pointer-driven** caret placement: never scrolls.
    ///
    /// A click or drag can only land on a row that is on screen at that moment
    /// (the pointer has to be over it), so the row is visible by construction
    /// and there is nothing to scroll to. Requesting a scroll anyway is what
    /// produced the reported bug: `scroll_to_item` targets the caret's whole
    /// *group*, and GPUI resolves a group taller than the viewport by pinning
    /// its **top edge** to the viewport top. So clicking the last row of a tall
    /// table/code card yanked the view back up to the container's first row
    /// (the table header, the opening fence) — pushing the clicked row off
    /// screen and making it impossible to edit. The row-precise correction pass
    /// only recovers when the clicked row is still painted; once it falls out
    /// of the virtualization window `row_layouts` has no entry for it and the
    /// view stays stuck at the container top.
    ///
    /// Clearing the request here (rather than teaching `caret_row_in_view`
    /// about the new line) also avoids a wasted `scroll_to_item` +
    /// correction-pass cycle on every single click.
    pub fn edit_no_scroll<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::editor::markdown_live::model::EditorModel) -> R,
    ) -> R {
        let out = self.edit(cx, f);
        self.cancel_caret_scroll();
        out
    }

    /// Drop any pending scroll-to-caret request and stop the correction passes.
    ///
    /// `edit` arms `scroll_to_cursor`; the render loop then consumes it (and
    /// re-arms `caret_scroll_passes`). Both must be cleared together, otherwise
    /// a stale budget lets a later frame's deferred correction move the view
    /// after the user has already positioned the caret themselves.
    fn cancel_caret_scroll(&mut self) {
        self.scroll_to_cursor = false;
        self.caret_scroll_passes = 0;
    }

    /// 统一的可打印文本写入路径（平台 `EntityInputHandler` 与快捷键兜底
    /// 共用；`on_key` 不再直接写文本）：
    /// 控制字符（除 `\n \r \t`）整体丢弃 → spec #6 跨格选区合并 → 单字符
    /// 自动配对（`autopair` 时）→ 常规插入，收尾统一走 [`Self::after_edit`]。
    fn type_text(&mut self, text: &str, autopair: bool, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // 控制字符残留（IME/平台异常输入）：不进 buffer。
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return;
        }
        use crate::editor::markdown_live::table_ops as t;
        // spec #6：跨格选区的覆盖输入 = 合并首尾格并把 typed 写进合并点
        //（一个撤销步）；同格/无选区落空，继续下面的自动配对/常规插入。
        if self.edit(cx, |m| t::merge_cross_cell_selection(m, text)) {
            self.after_edit(cx);
            return;
        }
        if autopair {
            let mut cs = text.chars();
            if let (Some(ch), None) = (cs.next(), cs.next())
                && self.try_autopair(ch, cx)
            {
                self.after_edit(cx);
                return;
            }
        }
        self.edit(cx, |m| {
            m.insert(text);
        });
        self.after_edit(cx);
    }

    /// Plan rows, reusing everything that did not change.
    ///
    /// A keystroke or cursor move changes a contiguous run of rows; the rest
    /// keep their previous `Rc<PlannedRow>` verbatim. Three diffs drive it:
    ///
    /// * **text**: rows whose `Arc<Line>` pointer differs from the previous
    ///   plan's input (line edits replace the `Arc`, so pointer identity is an
    ///   exact change test). A changed line can also move a block boundary, so
    ///   the dirty range is extended to the enclosing block run — bounded by
    ///   the next blank line for paragraphs, or by the fence/math/frontmatter
    ///   run for those constructs.
    /// * **reveal**: rows whose revealed flag differs (cursor/selection move).
    /// * **refdefs**: a content change invalidates link resolution document
    ///   wide, so the whole plan is rebuilt (refdefs are almost always empty,
    ///   making this the rare path).
    ///
    /// Correctness is pinned by a differential test against the full planner
    /// (`incremental_matches_full_plan`) over a random edit stream.
    fn planned_rows(&mut self, focused: bool) -> Rc<Vec<Rc<PlannedRow>>> {
        let key = (
            self.buffer.cursor(),
            self.buffer.selection(),
            focused,
            self.read_only,
            self.show_source,
        );
        if self.planned_version == self.buffer.text_version() && self.planned_key == key {
            return self.planned.clone();
        }
        let text_changed = self.planned_classified_version != self.buffer.text_version();

        // The model's dirty range is *read* here and cleared only once a plan
        // has actually been committed for this text version (see the clears
        // below). `mark_dirty` merges ranges monotonically, so never clearing
        // makes every later keystroke re-scan from the first line ever edited
        // to the last: two distant edits were enough to pin the window at the
        // whole document for the rest of the session (measured: ~8k `Arc` +
        // ~8k block comparisons per keystroke on an 8k-line document), which
        // silently defeated the incremental planner this module exists for.
        //
        // Clearing must not happen here, at read time: `after_edit` can mutate
        // the document again (table auto-format) *after* a plan was computed
        // but before the next one, and a read-time clear would drop the
        // original edit's marks along with it. Clearing on commit keeps the
        // merged range intact until the view has actually caught up.
        //
        // `None` (undo/redo/whole-document replace) means "unknown" and is
        // handled as a full comparison below.
        let dirty_range = self.buffer.dirty_range();

        // ---- classification / refdefs -------------------------------------
        // Classification resumes from a checkpointed state before the dirty
        // range and stops when the state reconverges, so a keystroke only
        // re-classifies the affected run instead of the whole document.
        if text_changed {
            // The whole merged range is passed, not just its start: a single
            // edit can touch several separated lines (replace-all, a coalesced
            // undo), and the classifier must not reuse a tail containing any of
            // them. `None` (unknown / whole-document replace) becomes `0..0`,
            // which disables resumption and forces a full pass.
            let dirty = dirty_range.clone().unwrap_or(0..0);
            let lines = self.buffer.lines();
            if self.planned_classified.blocks.len() == lines.len()
                && self.planned_classified.lang_before.len() == lines.len()
            {
                // Same line count: mutate the old tables in place instead of
                // allocating and copying the unchanged prefix/tail per key.
                crate::editor::markdown_live::reveal::classify_incremental_in_place(
                    &mut self.planned_classified,
                    lines,
                    dirty,
                );
            } else {
                let (classified, _reprocessed) =
                    crate::editor::markdown_live::reveal::classify_incremental(
                        lines,
                        Some(&self.planned_classified),
                        dirty,
                    );
                self.planned_classified = classified;
            }
            self.planned_classified_version = self.buffer.text_version();
        }
        if self.planned_refs_version != self.buffer.text_version() {
            // Refdefs are document-wide, but only a line that *looks like* a
            // definition can change the table. Checking the dirty range on
            // both sides (old text for deleted definitions, new text for
            // added ones) lets ordinary prose/code edits reuse the previous
            // map instead of re-scanning every line on each keystroke — the
            // old path was O(document) per keystroke.
            let refs_may_change = match &dirty_range {
                None => true,
                Some(range) => {
                    let lines = self.buffer.lines();
                    let start = range.start;
                    let end = (range.end + 1).min(lines.len());
                    (start..end).any(|i| {
                        lines.get(i).is_some_and(|line| line_may_be_refdef(line))
                            || self
                                .plan_input_lines
                                .get(i)
                                .is_some_and(|line| line_may_be_refdef(line))
                    })
                }
            };
            if refs_may_change {
                self.planned_refs = Rc::new(refdefs_for_lines(
                    self.buffer.lines(),
                    &self.planned_classified.blocks,
                ));
            }
            self.planned_refs_version = self.buffer.text_version();
        }

        // ---- reveal set for this frame -----------------------------------
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
        let blocks = &self.planned_classified.blocks;
        let lines = self.buffer.lines();
        let revealed = reveal_set(blocks, lines.len(), reveal);

        // ---- decide the dirty range --------------------------------------
        // Structural change (line count, block kinds, refdefs) or the very
        // first plan → rebuild everything. Otherwise splice the changed run.
        let structural = self.planned.is_empty()
            || self.plan_input_lines.len() != lines.len()
            || self.plan_input_blocks.len() != blocks.len()
            || self.plan_refs != *self.planned_refs;

        if structural {
            let mut planned = plan_range(
                lines,
                &self.planned_classified,
                &self.planned_refs,
                &revealed,
                0,
                lines.len(),
                false,
                None,
            );
            // Callout openers hide their `[!TAG] ` prefix unless revealed
            // (preview parity). Applied here, not in the planner, so the
            // planner keeps emitting source-exact segments.
            for row in &mut planned {
                strip_callout_prefix(row, lines.get(row.line).map(|l| l.as_str()).unwrap_or(""));
            }
            self.planned = Rc::new(planned.into_iter().map(Rc::new).collect());
            self.plan_input_lines = lines.to_vec();
            self.plan_input_blocks = blocks.to_vec();
            self.plan_revealed = revealed;
            self.plan_refs = (*self.planned_refs).clone();
            if self.planned_geometry_version != self.buffer.text_version() {
                self.planned_geometry = Rc::new(PlanGeometry::build_owned(&self.planned, lines));
                self.geometry_revision = self.geometry_revision.wrapping_add(1);
                self.planned_geometry_version = self.buffer.text_version();
            }
            self.planned_version = self.buffer.text_version();
            self.planned_key = key;
            return self.planned.clone();
        }

        // Dirty rows: text pointer changed, block kind changed, or the reveal
        // flag changed. Extend the range to a block-safe boundary (see the
        // doc comment): a line edit can flip a neighbouring line's
        // classification (lookahead: setext underlines, GFM tables) or the
        // fence/math state that runs after it.
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        // Whether any row's *block kind* changed. Grouping and the calibration
        // buckets are functions of the block kinds, so an unchanged kind list
        // means the cached geometry is still valid.
        let mut blocks_changed = false;
        let dirty = |i: usize, lo: &mut usize, hi: &mut usize| {
            *lo = (*lo).min(i);
            *hi = (*hi).max(i);
        };
        // Text changes come from the model's tracked dirty range (O(changed)
        // instead of comparing every line's `Arc`), widened by one line on
        // each side because a neighbouring line's classification can depend on
        // the edited text (one-line lookahead).
        if text_changed {
            match dirty_range {
                Some(r) => {
                    let start = r.start.saturating_sub(1);
                    let end = (r.end + 1).min(lines.len());
                    for i in start..end {
                        if !self
                            .plan_input_lines
                            .get(i)
                            .is_some_and(|l| Arc::ptr_eq(l, &lines[i]))
                        {
                            dirty(i, &mut lo, &mut hi);
                        }
                    }
                }
                // Unknown range (undo/redo/whole-document replace): compare all.
                None => {
                    for i in 0..lines.len() {
                        if !self
                            .plan_input_lines
                            .get(i)
                            .is_some_and(|l| Arc::ptr_eq(l, &lines[i]))
                        {
                            dirty(i, &mut lo, &mut hi);
                        }
                    }
                }
            }
        }
        // Which lines' *block kind* changed. This drives both the re-plan
        // window and the geometry gate, and it must be exact.
        //
        // It used to compare only inside the narrow ±1 text window, which is
        // wrong in both directions: the setext restyle rewrites lines far
        // above the edit, and fence/math state propagates far below it, so a
        // genuine structural change could sit outside the window and leave the
        // geometry gate stamping stale grouping as current. (That bug was
        // masked while `mark_dirty` accumulated ranges forever, because the
        // window eventually covered the whole document.)
        //
        // The span to compare comes from the classifier itself
        // ([`Classified::changed`]), which is the only component that knows how
        // far the edit actually propagated. Scanning the whole table instead
        // would be correct but costs a full pass per keystroke — the exact
        // thing the incremental classifier exists to avoid.
        if text_changed {
            let changed = self.planned_classified.changed.clone();
            for i in changed.start.min(self.plan_input_blocks.len())
                ..changed.end.min(self.plan_input_blocks.len())
            {
                if self.plan_input_blocks.get(i) != blocks.get(i) {
                    blocks_changed = true;
                    dirty(i, &mut lo, &mut hi);
                }
            }
        }
        // Reveal changes are enumerated from the two (tiny) sets instead of
        // testing every line: `BTreeSet::contains` is O(log n) per row, which
        // made this loop O(rows × log rows) on a keystroke.
        for l in self.plan_revealed.symmetric_difference(&revealed) {
            dirty(*l, &mut lo, &mut hi);
        }
        if lo > hi {
            // Nothing to re-plan (e.g. a cursor move that changed no line's
            // reveal state, like moving inside an already-revealed line).
            self.planned_key = key;
            return self.planned.clone();
        }
        let (lo, hi) = self.expand_dirty_range(lines, blocks, lo, hi);
        // The window above is deliberately narrow (±1 around the edit), but a
        // text edit can flip the classification of lines outside it: the setext
        // restyle reaches backward over a whole paragraph run, and fence/math
        // state propagates forward. Those lines are inside the *expanded* range
        // that is about to be re-planned, so also compare block kinds there —
        // otherwise `blocks_changed` stays false and the geometry gate stamps
        // the stale grouping/`has_table`/ordered numbers as current.
        for i in lo..=hi.min(lines.len().saturating_sub(1)) {
            if self.plan_input_blocks.get(i) != blocks.get(i) {
                blocks_changed = true;
                break;
            }
        }

        // Callout state entering the range: the row before `lo` carries it.
        let (mut prev_was_quote, mut callout_kind) = if lo == 0 {
            (false, None)
        } else {
            let prev = &self.planned[lo - 1];
            (
                matches!(prev.block, Block::Quote { .. }),
                prev.callout.as_ref().map(|c| c.kind),
            )
        };
        let _ = &mut prev_was_quote;
        let _ = &mut callout_kind;
        let mut replanned = plan_range(
            lines,
            &self.planned_classified,
            &self.planned_refs,
            &revealed,
            lo,
            hi + 1,
            prev_was_quote,
            callout_kind,
        );
        for row in &mut replanned {
            strip_callout_prefix(row, lines.get(row.line).map(|l| l.as_str()).unwrap_or(""));
        }

        // Snapshot the new kinds before consuming the rows, and the old kinds
        // **before** the splice overwrites them. Reading the old kind back out
        // of `self.planned` after splicing compares the new rows against
        // themselves, which made this test vacuously false and left the
        // geometry gate relying on `blocks_changed` alone.
        let replanned_kinds: Vec<RowKind> = replanned.iter().map(|r| r.plan.kind.clone()).collect();
        let previous_kinds: Vec<Option<RowKind>> = (lo..=hi)
            .map(|i| self.planned.get(i).map(|r| r.plan.kind.clone()))
            .collect();

        // Splice in place. `make_mut` copies the pointer table only when the
        // previous frame still holds a clone; it never copies row contents.
        let planned = Rc::make_mut(&mut self.planned);
        for (off, row) in replanned.into_iter().enumerate() {
            if let Some(slot) = planned.get_mut(lo + off) {
                *slot = Rc::new(row);
            }
        }
        // Refresh the input snapshot for the spliced rows only.
        for i in lo..=hi {
            if let Some(l) = self.plan_input_lines.get_mut(i) {
                l.clone_from(&lines[i]);
            }
            if let Some(b) = self.plan_input_blocks.get_mut(i) {
                b.clone_from(&blocks[i]);
            }
        }
        self.plan_revealed = revealed;

        // Geometry inputs are the block kinds, the per-row `RowKind`
        // (ordered-list numbers, list indent columns) and the mermaid block
        // sources. A plain paragraph edit changes none of them, so the cached
        // geometry stays valid — rebuilding it is ~0.8 ms at 20k rows, which
        // would otherwise dominate the remaining keystroke cost.
        // A kind change invalidates the calibration buckets; an ordered-list
        // row in the range can shift the *display* numbers of every later
        // ordered item (the counter is sequential), even though those rows'
        // own kinds are unchanged.
        let kinds_changed = replanned_kinds
            .iter()
            .zip(previous_kinds.iter())
            .any(|(kind, old)| old.as_ref() != Some(kind))
            || replanned_kinds
                .iter()
                .any(|k| matches!(k, RowKind::Ordered { .. }));
        // Mermaid sources live in the geometry, so any edit inside a mermaid
        // block's fence-to-close span invalidates it.
        let mermaid_touched = self.planned_geometry.mermaid_blocks.iter().any(|b| {
            let start = b.block_id;
            let end = b
                .code_rows
                .last()
                .copied()
                .map_or(start, |last| last.max(start))
                + 1;
            lo <= end && hi >= start
        });
        if self.planned_geometry_version != self.buffer.text_version()
            && (blocks_changed || kinds_changed || mermaid_touched)
        {
            self.planned_geometry = Rc::new(PlanGeometry::build_owned(&self.planned, lines));
            self.geometry_revision = self.geometry_revision.wrapping_add(1);
            self.planned_geometry_version = self.buffer.text_version();
        } else if self.planned_geometry_version != self.buffer.text_version() {
            // Inputs unchanged: keep the geometry but re-stamp the version so
            // the next edit still sees a fresh cache.
            self.planned_geometry_version = self.buffer.text_version();
        }
        self.planned_version = self.buffer.text_version();
        self.planned_key = key;
        // Consume the marks: this plan accounted for all of them (the exact
        // block comparison above covers the whole document, and the text scan
        // covered the model's range). `mark_dirty` merges ranges monotonically,
        // so without this the window grows to the span of every edit in the
        // session — measured at ~8k `Arc` + ~8k block comparisons per keystroke
        // on an 8k-line document, which defeats the incremental planner.
        self.buffer.take_dirty_range();
        self.planned.clone()
    }

    /// Widen a dirty row range to the smallest range that can be re-planned
    /// without depending on rows outside it.
    ///
    /// Block classification has one line of lookahead (setext underlines, GFM
    /// table headers) and threads fence/math/frontmatter state forward, so:
    ///
    /// * a **line edit** (`lo..=hi` came from a text change) must extend to the
    ///   enclosing "run" — the maximal span with no blank line and no
    ///   fence/math/frontmatter boundary — because that is the largest region
    ///   whose block kinds can flip from one line's text.
    /// * a **reveal-only** change affects just the flagged rows.
    fn expand_dirty_range(
        &self,
        lines: &[Line],
        blocks: &[Block],
        lo: usize,
        hi: usize,
    ) -> (usize, usize) {
        let text_changed = self.planned_version != self.buffer.text_version();
        if !text_changed {
            // Reveal-only: the block kinds are unchanged, so only the flagged
            // rows need re-planning.
            return (lo, hi);
        }
        let mut lo = lo;
        let mut hi = hi;
        // Backwards over the preceding run.
        while lo > 0 {
            let prev = lo - 1;
            if lines.get(prev).is_none_or(|l| l.trim().is_empty()) {
                break;
            }
            // A fence/math/frontmatter boundary is self-delimiting: stop
            // before it (its own plan does not depend on this run's text).
            if matches!(
                blocks.get(prev),
                Some(
                    Block::Fence { .. }
                        | Block::CodeLine
                        | Block::Math { .. }
                        | Block::MathDelim { .. }
                        | Block::FrontMatter
                )
            ) {
                break;
            }
            lo = prev;
        }
        // Forwards over the following run.
        while hi + 1 < lines.len() {
            let next = hi + 1;
            if lines.get(next).is_none_or(|l| l.trim().is_empty()) {
                break;
            }
            if matches!(
                blocks.get(next),
                Some(
                    Block::Fence { .. }
                        | Block::CodeLine
                        | Block::Math { .. }
                        | Block::MathDelim { .. }
                        | Block::FrontMatter
                )
            ) {
                break;
            }
            hi = next;
        }
        // A partially included self-delimiting run (fence / `$$` math /
        // frontmatter) must be widened to cover the whole run: every line in
        // it depends on the run's *opening* state, so editing the opener
        // changes the code lines' `RowKind::Code { lang }` (which drives
        // tree-sitter) and editing a code line can change where the run ends.
        self.run_bounds(blocks, lo, hi)
    }

    /// Widen `lo..=hi` to cover any fence / math / frontmatter run it touches.
    fn run_bounds(&self, blocks: &[Block], lo: usize, hi: usize) -> (usize, usize) {
        let in_run = |b: Option<&Block>| {
            matches!(
                b,
                Some(
                    Block::Fence { .. }
                        | Block::CodeLine
                        | Block::Math { .. }
                        | Block::MathDelim { .. }
                        | Block::FrontMatter
                )
            )
        };
        // Only widen when the range actually touches such a run.
        let touches = (lo..=hi).any(|i| in_run(blocks.get(i)));
        if !touches {
            return (lo, hi);
        }
        let mut lo = lo;
        let mut hi = hi;
        // A run's boundaries are the nearest lines that are *not* part of it.
        while lo > 0 && in_run(blocks.get(lo - 1)) {
            lo -= 1;
        }
        while hi + 1 < blocks.len() && in_run(blocks.get(hi + 1)) {
            hi += 1;
        }
        (lo, hi)
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
            // spec #13：表格内编辑后自动按 Typora 规则重排本表（列宽/对齐/
            // 分隔行形态），光标重映射到同 (行, 格, 格内偏移)。撤销/重做
            // 不触发（历史保持原样），「源码」模式不触发（保护裸编辑）。
            if !self.skip_reformat {
                self.auto_prettify_table(cx);
            }
            // 重排可能再次改文本：按**最终**版本记账/广播。
            self.emitted_version = self.buffer.text_version();
            // 查找面板的匹配跟随文档（`InputState::update_search` parity）。
            //
            // 面板关着时**完全跳过**：没有面板就没有匹配要维护，全文
            // `Rope::from(text)` + 重扫纯属浪费（这是每击键一次的 O(文档)）。
            // 面板开着时也走防抖：连续打字只在停顿后重扫一次最终文本，
            // 高亮延迟一帧不影响「边打字边看匹配」的体感。
            if let Some(panel) = self.search_panel.clone() {
                let panel_open = panel.read(cx).is_open();
                if panel_open {
                    let version = self.buffer.text_version();
                    if self.search_sync_version != Some(version) {
                        self.search_sync_version = Some(version);
                        let weak = cx.entity().downgrade();
                        cx.spawn(async move |_this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(Self::SEARCH_SYNC_DEBOUNCE_MS))
                                .await;
                            let _ = weak.update(cx, |view, cx| {
                                view.flush_search_sync(cx);
                            });
                        })
                        .detach();
                    }
                }
            }
            cx.emit(LiveViewEvent::Change {
                version: self.buffer.text_version(),
            });
        }
        cx.notify();
        cx.stop_propagation();
    }

    /// spec #13：表格内编辑后的自动重排——调 [`table_ops::prettify_edits`]
    /// 算出目标形态，经 `replace_rows_and_move` 一次写回（撤销与本次打字
    /// 合并为一步），光标重映射到同 (行, 格, 格内偏移)。已是目标格式时
    /// 不动文本、不入撤销栈。
    fn auto_prettify_table(&mut self, cx: &mut Context<Self>) {
        use crate::editor::markdown_live::table_ops as t;
        // IME 组字期间不重排：组字区间按字节记账，重排会挪动区间前的字节。
        if self.read_only
            || self.show_source
            || self.ime_marked_range.is_some()
            || self.buffer.selection().is_some()
        {
            return;
        }
        // 前置门控：先问缓存几何「光标行是不是表格行」。绝大多数文档没有表格，
        // 这一步就把每次击键的全文档克隆 + 全量分类全部省掉。
        // 几何按 text_version 缓存，编辑后版本已变 → 下面用检查点分类重扫
        // 光标附近的表格，而不是整个文档。
        if !self.cursor_line_is_table() {
            return;
        }

        let cur = self.buffer.cursor();
        let dirty_from = self
            .buffer
            .dirty_range()
            .map(|range| range.start)
            .unwrap_or(0);
        let lines = self.buffer.lines();
        let Some(block) =
            t::table_block_range_local(lines, cur.line, Some(&self.planned_classified), dirty_from)
        else {
            return;
        };
        let is_table = |l: usize| l >= block.0 && l <= block.1;
        let Some((_, ci, (cs, _))) = table_cell_at(lines, &is_table, cur.line, cur.col) else {
            return;
        };
        let offset = cur.col.saturating_sub(cs);
        let Some(edits) = t::prettify_edits(lines, block) else {
            return;
        };
        if edits
            .iter()
            .all(|(i, text)| lines.get(*i).map(|l| l.as_str()) == Some(text.as_str()))
        {
            return; // 已是目标格式：不动文本、不入撤销栈。
        }
        // 新列位置：用改写后的该行文本算同 (行, 格) 的内容起点 + 偏移。
        let Some(new_col) = edits
            .iter()
            .find(|(i, _)| *i == cur.line)
            .map(|(_, text)| text.as_str())
            .and_then(|text| cell_content_cols(text, ci))
            .map(|(cs2, ce2)| (cs2 + offset).min(ce2))
        else {
            return;
        };
        self.edit(cx, |m| {
            m.replace_rows_and_move(&edits, Some(Pos::new(cur.line, new_col)));
        });
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        // 光标一动就收起表格尺寸弹层（Typora：点别处/移动即关）。
        // 步进器走 `run_table_op` → `after_edit`，不经这里，弹层保持展开。
        self.table_resize_open = false;
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
        // Pointer-driven: never scroll (see `edit_no_scroll`).
        self.edit_no_scroll(cx, |m| {
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
    ///
    /// Table rows need care. A cell's `cell_div` claims drags over its own box,
    /// but this fallback also fires for pointer positions outside the *first*
    /// row element's band — which includes the whole grid below that first row.
    /// Mapping such a position with [`Self::col_for_click`] uses whole-line
    /// geometry (the rendered row, not the cell), so it lands on column 0 and
    /// **overwrites** the precise column the cell drag just set. The user sees
    /// the caret refuse to follow the mouse and the selection snap back to the
    /// start of the row. So when the drag is already inside a table, defer to
    /// the cell path: extend within the pointer's own cell only.
    pub(crate) fn on_fallback_drag(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        // 正在拖滚动条（表格/图的横向条）：不扩展选区。滚动条的按下监听会
        // `stop_propagation`，但拖拽起点若落在行上（行监听先注册、先跑），
        // `selecting` 已经被置位，这里必须让行。
        if self.h_scroll.any_dragging(cx) {
            return;
        }
        let Some(line) = self.line_for_click(position) else {
            return;
        };
        // 拖拽扫过图模式图片区：忽略（保持光标在块外），不扩展进隐藏源码。
        if self.line_in_mermaid_image(line) {
            return;
        }
        // spec #6：锚点在表内、目标越出该表 → 不延伸（避免跨块选区毁表）。
        if self.drag_cross_table(line) {
            return;
        }
        // In-table: map through the pointer's cell when one owns this position,
        // so the endpoint stays cell-precise instead of collapsing to column 0.
        // (`cell_hits` is written during paint for every rendered cell.)
        if self.line_is_table(line)
            && let Some(col) = self.cell_col_at_any(line, position)
        {
            self.edit_no_scroll(cx, |m| m.move_to(line, col, true));
            return;
        }
        let col = self.col_for_click(line, position);
        self.edit_no_scroll(cx, |m| m.move_to(line, col, true));
    }

    /// The cell column under `position` on `line`, whichever cell of that row
    /// contains it.
    ///
    /// [`Self::cell_col_at`] needs the cell's own `(line, col)` key, which the
    /// fallback path does not know; this finds the owning cell by hit box.
    fn cell_col_at_any(&self, line: usize, position: gpui::Point<Pixels>) -> Option<usize> {
        // Try each recorded cell of this row; their text boxes do not overlap.
        let mut keys: Vec<usize> = self
            .cell_hits
            .keys()
            .filter(|(l, _)| *l == line)
            .map(|(_, c)| *c)
            .collect();
        keys.sort_unstable();
        for col in keys {
            let hit = self.cell_hits.get(&(line, col))?;
            if position.y >= hit.text_bounds.top()
                && position.y <= hit.text_bounds.bottom()
                && position.x >= hit.text_bounds.left()
                && position.x <= hit.text_bounds.right()
            {
                return self.cell_col_at(line, col, position);
            }
        }
        None
    }

    // ---- mermaid 图模式禁区（严格 tab-only：图可见时源码不可编辑） -------

    // ---- 表格单元格编辑（点击格内直接改，不退回源码） -------------------

    /// 供 `LiveLine` 的单元格模式取整行文本（映射需要 char 列）。
    pub(crate) fn line_text(&self, line: usize) -> Option<String> {
        self.buffer.line(line).map(str::to_string)
    }

    /// 窗口坐标 → `(line, col)` 格的行内 char 列（spec #6）。映射走本格
    /// LiveLine 的帧内记录 [`CellHit`]：文本右侧空白 → 文本末、左侧空白
    /// → 文本首、中间 → 字形位置，再经 segs 换回源列并夹进本格内容
    /// （空格 segs 为空时防落回管道上）。格盒本身的认领由 `cell_div`
    /// 的命中框负责（含内边距/未填满区域），这里不做格间判定。
    pub(crate) fn cell_col_at(
        &self,
        line: usize,
        col: usize,
        position: gpui::Point<Pixels>,
    ) -> Option<usize> {
        let hit = self.cell_hits.get(&(line, col))?;
        let raw = if position.x >= hit.text_bounds.right() {
            hit.text_len
        } else if position.x < hit.text_bounds.left() {
            0
        } else {
            match hit.layout.index_for_position(position) {
                Ok(ix) | Err(ix) => ix.min(hit.text_len),
            }
        };
        let src_byte = src_for_vis(&hit.segs, raw).clamp(hit.content.start, hit.content.end);
        let out = col_for_byte(&self.line_text(line)?, src_byte);
        // 空格/映射越界时夹回本格内容（绝不允许光标停在管道上）。
        Some(match self.table_cell_content_at(line, out) {
            Some((cs, ce)) => out.clamp(cs, ce),
            None => out,
        })
    }

    /// 光标所在单元格的**内容 char 列范围**（去 padding），用于把双击选词
    /// / 三击选格夹在格内。光标不在表格格里时返回 `None`。
    fn table_cell_content_at(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        let text = self.buffer.line(line)?;
        let spans = table_cell_spans(text);
        let byte = byte_for_col(text, col);
        let (s, e) = spans
            .iter()
            .copied()
            .find(|(s, e)| byte >= *s && byte <= *e)
            .or_else(|| spans.iter().copied().find(|(s, _)| *s >= byte))
            .or_else(|| spans.last().copied())?;
        let raw = &text[s..e];
        let lead = raw.len() - raw.trim_start().len();
        let trail = raw.len() - raw.trim_end().len();
        let cs = col_for_byte(text, s + lead);
        let ce = col_for_byte(text, (e - trail).max(s + lead));
        Some((cs, ce.max(cs)))
    }

    /// Typora `.md-expand` 的展开范围：光标所在**行内元素**的字节范围
    /// （整格保持渲染，只有这个元素的标记显示）。
    ///
    /// 返回 `None` 表示不展开任何标记（光标不在这格、或落在没有标记的纯
    /// 文本里）。范围换算到**行内绝对偏移**，与 `plan_cell` 的 segs 一致。
    fn table_cell_expand_range(
        &self,
        src_line: &str,
        content: std::ops::Range<usize>,
        cursor_col: usize,
        caret_in_cell: bool,
    ) -> Option<std::ops::Range<usize>> {
        if !caret_in_cell {
            return None;
        }
        let byte = byte_for_col(src_line, cursor_col);
        if byte < content.start || byte > content.end {
            return None;
        }
        // 在**格内容切片**上解析：与 `plan_cell` 的 `inline_range` 同一套
        // 输入，span 边界才与它一致（整行解析会被管道/空格影响）。
        let inline = crate::editor::markdown_live::inline::parse_with_refs(
            &src_line[content.clone()],
            &self.planned_refs,
        );
        let rel = byte - content.start;
        let r = inline.expand_at(rel)?;
        // 换回行内绝对偏移。
        Some(content.start + r.start..content.start + r.end)
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
        // Pin the toolbar's visibility *before* the caret enters the table.
        // This press is hit-tested against the current layout, and revealing
        // the toolbar now would grow the table and slide the rows down under
        // the pointer, so the following drag moves would map to the row above.
        self.freeze_table_toolbar_for_line(line);
        // 双击选词 / 三击选格：都夹在本格内容范围内（Typora 表格语义：
        // 双击选格内的词，三击选整个单元格内容，而不是整行含管道）。
        let cell_range = self.table_cell_content_at(line, col);
        // Pointer-driven: never scroll (see `edit_no_scroll`).
        self.edit_no_scroll(cx, |m| {
            m.move_to(line, col, shift);
            if !shift {
                if click_count == 2 {
                    if let Some((s, e)) = cell_range {
                        m.select_word_within(s, e);
                    } else {
                        m.select_word();
                    }
                } else if click_count > 2 {
                    if let Some((s, e)) = cell_range {
                        m.select_range(s, e);
                    } else {
                        m.select_line();
                    }
                }
            }
        });
        self.selecting = click_count <= 1;
        cx.stop_propagation();
        cx.notify();
    }

    /// 光标落在表格行的**管道或 padding 空白**上时，移到最近的格内容位置。
    ///
    /// 表格里光标只应停在格内容上（Typora 语义）：`|` 与格间空格不是可停靠
    /// 位置。只在光标已落在某个格内容里时保持不动，否则夹到相邻格内容。
    fn clamp_cursor_off_table_pipe(&mut self, cx: &mut Context<Self>) {
        let cur = self.buffer.cursor();
        if !self.line_is_table(cur.line) {
            return;
        }
        let Some((cs, ce)) = self.table_cell_content_at(cur.line, cur.col) else {
            return;
        };
        if cur.col >= cs && cur.col <= ce {
            return;
        }
        // 夹到格内容里（优先光标右侧的格内容，与前进方向一致）。
        let target = if cur.col < cs { cs } else { ce };
        self.edit(cx, |m| m.move_to(cur.line, target, false));
    }

    /// 拖拽延伸目标 `target_line` 是否越出锚点所在的表块（spec #6 的成形
    /// 前提）：表外按下拖进格、表内拖到表外、或拖进另一张表都会形成**跨块**
    /// 选区，而跨块删除会按行拼接、毁掉表格结构 —— 一律不延伸（与「拖到
    /// 无单元格监听的区域时选区自然冻结」的既有表现一致）。普通文字段落间
    /// 的拖拽（两端都不在表里）不受影响。
    fn drag_cross_table(&self, target_line: usize) -> bool {
        // 用上一帧的 `planned` 分类（拖拽是每帧鼠标移动的热路径，不能在这里
        // 重跑 O(n) 全文档分类）；无表格的文档直接零额外开销。
        let flags = self.table_line_flags();
        if !flags.iter().any(|f| *f) {
            return false;
        }
        let lines: Vec<Line> = self.buffer.lines().to_vec();
        // 锚点 = 拖拽起点（退化为光标，覆盖 down 后尚未扩展的首帧）。
        let anchor_line = self
            .buffer
            .model()
            .anchor()
            .map(|p| p.line)
            .unwrap_or_else(|| self.buffer.cursor().line);
        let blk = |l: usize| {
            crate::editor::markdown_live::table_ops::table_block_range_flagged(&lines, &flags, l)
        };
        let anchor_blk = blk(anchor_line);
        let target_blk = blk(target_line);
        anchor_blk != target_blk && (anchor_blk.is_some() || target_blk.is_some())
    }

    /// 单元格内拖拽：把选区终点扩到**指针所在格**的对应位置（spec #6：
    /// 端点由 `cell_div` 的元素级监听按命中格给出，不再夹回起点格 ——
    /// 跨格选区由此成形；跨块延伸由 [`Self::drag_cross_table`] 拦下）。
    pub(crate) fn on_table_cell_drag(&mut self, line: usize, col: usize, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if self.drag_cross_table(line) {
            return;
        }
        self.edit_no_scroll(cx, |m| m.move_to(line, col, true));
    }

    /// Typora：表格里 Cmd/Ctrl+A 只选**当前格**内容（不升级到整表/整文档）；
    /// 表格外返回 `false`，由调用方走整篇全选。返回 `true` 表示已处理。
    fn select_table_cell_contents(&mut self, cx: &mut Context<Self>) -> bool {
        let cur = self.buffer.cursor();
        // 必须真的在表格行上：`table_cell_content_at` 对普通行也会成功
        //（无管道时退化成整行一个 span），不守卫会吞掉表格外的 Cmd+A。
        if !self.line_is_table(cur.line) {
            return false;
        }
        let Some((cs, ce)) = self.table_cell_content_at(cur.line, cur.col) else {
            return false;
        };
        self.edit(cx, |m| m.select_range(cs, ce));
        cx.notify();
        true
    }

    /// Typora spec #15：表格内光标**折叠**时 Cmd/Ctrl+C = `lineWiseCopyCut`
    /// 整行复制——选中该行并把源码写进剪贴板；有选区/表外则原样复制选区。
    fn copy_selection_or_table_row(&mut self, cx: &mut Context<Self>) {
        if self.buffer.selection().is_none() && self.select_table_row(cx) {
            if let Some(t) = self.selected_source_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(t));
            }
        } else if let Some(t) = self.selected_source_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(t));
        }
    }

    /// Typora：Cmd/Ctrl+L 选**整行**（含所有格与管道）。返回 `true` 表示
    /// 已处理（光标在表格里）。
    fn select_table_row(&mut self, cx: &mut Context<Self>) -> bool {
        let cur = self.buffer.cursor();
        if !self.line_is_table(cur.line) {
            return false;
        }
        let len = self
            .buffer
            .line(cur.line)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        self.edit(cx, |m| m.select_range(0, len));
        cx.notify();
        true
    }

    /// Typora spec #12（哨兵退格）：光标在**格内容起点**按退格 ——
    /// - **空**格 → 跳到流序上一格**末**（不删字符、不合并单元格）；
    ///   表格首格且整表为空 → 删除整个表格；首格但表非空 → 原地消费；
    /// - **非空**格 → 命中零宽哨兵，消费按键但**什么都不删**。
    ///
    /// 不在格内容起点、或不在表格 → 返回 `false`，走通用退格。
    fn table_backspace_at_start(&mut self, cx: &mut Context<Self>) -> bool {
        let cur = self.buffer.cursor();
        let lines: Vec<Line> = self.buffer.lines().to_vec();
        // 预先算出「哪些行是表格行」，闭包不再借用 `self`（`self.edit`
        // 需要可变借用，闭包持不可变借用会冲突）。
        let table_lines = self.table_line_flags();
        let is_table = |l: usize| table_lines.get(l).copied().unwrap_or(false);
        let Some((row, col, (cs, _))) = table_cell_at(&lines, &is_table, cur.line, cur.col) else {
            return false;
        };
        if cur.col > cs {
            return false;
        }
        // 该格内容（去 padding）为空吗？
        let cell_empty = lines
            .get(cur.line)
            .map(|l| l.as_str())
            .and_then(|text| {
                table_cell_bounds(text)
                    .get(col)
                    .map(|&(s, e)| text[s..e].trim().is_empty())
            })
            .unwrap_or(false);
        if !cell_empty {
            // 非空格首：Typora 命中零宽哨兵 → 消费但无操作（不跳、不删）。
            return true;
        }
        // 空格 → 流序上一格末：同行前一格，或上一行末格。
        let (prev_row, prev_col) = if col > 0 {
            (row, col - 1)
        } else {
            match row.checked_sub(1) {
                Some(r) => (r, usize::MAX),
                None => {
                    // 表格首格：整表为空 → 删除整张表；否则原地消费。
                    use crate::editor::markdown_live::table_ops as t;
                    if let Some(block) = t::table_block_range(&lines, cur.line)
                        && table_block_all_empty(&lines, block)
                    {
                        self.run_table_op(cx, t::delete_table);
                    }
                    return true;
                }
            }
        };
        let prev_col = if prev_col == usize::MAX {
            let Some(l) = table_row_line(&lines, &is_table, cur.line, prev_row) else {
                return true;
            };
            table_cell_bounds(lines.get(l).map(|l| l.as_str()).unwrap_or(""))
                .len()
                .saturating_sub(1)
        } else {
            prev_col
        };
        let moved = self.edit(cx, |m| {
            table_goto_cell(
                m, &lines, &is_table, cur.line, prev_row, prev_col, true, false,
            )
        });
        if moved {
            self.after_move(cx);
        }
        true
    }

    // ---- 表格结构操作（右键菜单 / 工具栏的入口） -------------------------

    /// Typora 的表格粘贴：文本含 `|` 时按行列分发到从光标格开始的区域
    /// （不够则自动补行/补列）；不含 `|` 返回 `false`，走普通插入。
    /// 打开工具栏 More▾ 的表格菜单（与右键菜单共用 `table_menu_body` 条目）。
    /// 位置 = 点击点，向下展开；点别处/Esc 关闭由菜单自己的 DismissEvent 驱动。
    /// 焦点不在这儿贴：派发期同步 focus 会被丢弃，改由浮层渲染**逐帧贴**
    /// （与 `ContextMenu` 的 paint 内 focus 同款），见 `table_menu_overlay`。
    fn open_table_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_table_menu(cx);
        let ent = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            table_menu_body(menu, ent.clone())
        });
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            this.close_table_menu(cx);
        });
        self.table_menu = Some(menu);
        self.table_menu_position = Some(position);
        self.table_menu_subscription = Some(subscription);
        cx.notify();
    }

    /// 关闭 More▾ 菜单（点菜单项 / 点别处 / 再次打开前）。
    fn close_table_menu(&mut self, cx: &mut Context<Self>) {
        let had = self.table_menu.take();
        self.table_menu_position = None;
        self.table_menu_subscription.take();
        if had.is_some() {
            // 菜单期间焦点被浮层渲染逐帧贴在菜单上；关闭后要还回编辑器
            //（下一帧 render 落实），否则焦点悬空在已卸载的菜单句柄上，
            // 键盘输入失联。
            self.table_menu_restore_focus = true;
            cx.notify();
        }
    }

    /// 执行一个表格菜单动作。
    fn run_table_menu_op(&mut self, op: TableMenuOp, cx: &mut Context<Self>) {
        use crate::editor::markdown_live::table_ops as t;
        match op {
            TableMenuOp::RowAbove => {
                self.run_table_op(cx, |m| t::insert_row(m, true));
            }
            TableMenuOp::RowBelow => {
                self.run_table_op(cx, |m| t::insert_row(m, false));
            }
            TableMenuOp::ColBefore => {
                self.run_table_op(cx, |m| t::insert_column(m, true));
            }
            TableMenuOp::ColAfter => {
                self.run_table_op(cx, |m| t::insert_column(m, false));
            }
            TableMenuOp::RowUp => {
                self.run_table_op(cx, |m| t::move_row(m, true));
            }
            TableMenuOp::RowDown => {
                self.run_table_op(cx, |m| t::move_row(m, false));
            }
            TableMenuOp::ColLeft => {
                self.run_table_op(cx, |m| t::move_column(m, true));
            }
            TableMenuOp::ColRight => {
                self.run_table_op(cx, |m| t::move_column(m, false));
            }
            TableMenuOp::DeleteRow => {
                self.run_table_op(cx, t::delete_row);
            }
            TableMenuOp::DeleteColumn => {
                self.run_table_op(cx, t::delete_column);
            }
            TableMenuOp::CopyTable => {
                // spec #14 菜单「Copy Table」：整表源码进剪贴板（不动文本）。
                let lines = self.buffer.lines().to_vec();
                let cur = self.buffer.cursor();
                if let Some(block) = t::table_block_range(&lines, cur.line) {
                    let text = lines[block.0..=block.1]
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            TableMenuOp::AlignLeft => {
                self.run_table_op(cx, |m| t::set_column_align(m, Some(t::Align::Left)));
            }
            TableMenuOp::AlignCenter => {
                self.run_table_op(cx, |m| t::set_column_align(m, Some(t::Align::Center)));
            }
            TableMenuOp::AlignRight => {
                self.run_table_op(cx, |m| t::set_column_align(m, Some(t::Align::Right)));
            }
            TableMenuOp::AlignNone => {
                self.run_table_op(cx, |m| t::set_column_align(m, None));
            }
            TableMenuOp::Prettify => {
                self.run_table_op(cx, t::prettify_table);
            }
            TableMenuOp::DeleteTable => {
                self.run_table_op(cx, t::delete_table);
            }
        }
    }

    /// 跑一个表格结构操作（纯函数）+ 事务收尾。返回是否真的改了东西。
    fn run_table_op(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&mut crate::editor::markdown_live::model::EditorModel) -> bool,
    ) -> bool {
        if self.read_only {
            return false;
        }
        let changed = self.edit(cx, op);
        if changed {
            self.after_edit(cx);
        }
        changed
    }

    /// 表格里的上下移动（Typora）：按**列索引**换行，不是按像素 x。
    /// 越过表头/末行则退出表格。返回 `true` 表示已处理。
    fn table_move_vertical(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) -> bool {
        let cur = self.buffer.cursor();
        let lines: Vec<Line> = self.buffer.lines().to_vec();
        // 预先算出「哪些行是表格行」，闭包不再借用 `self`（`self.edit`
        // 需要可变借用，闭包持不可变借用会冲突）。
        let table_lines = self.table_line_flags();
        let is_table = |l: usize| table_lines.get(l).copied().unwrap_or(false);
        let Some((row, col, _)) = table_cell_at(&lines, &is_table, cur.line, cur.col) else {
            return false;
        };
        let target_row = if down {
            row + 1
        } else {
            match row.checked_sub(1) {
                Some(r) => r,
                None => return false,
            }
        };
        // Typora: ↑ 落在目标格**末**，↓ 落在**首**。
        let moved = self.edit(cx, |m| {
            table_goto_cell(
                m, &lines, &is_table, cur.line, target_row, col, !down, extend,
            )
        });
        if moved {
            self.after_move(cx);
            return true;
        }
        false
    }

    /// 表格里的左右移动（Typora spec #8）：「行即文本行」——行内横移的换行
    /// 被映射成**同列格的纵向换行**：`←` 在格首折到同列上一格（落在其
    /// **末**），`→` 在格尾折到同列下一格（落在其**首**）。格中间返回
    /// `false`（调用方走字符级移动 + 管道钳制）。表头格首 / 末行格尾无
    /// 目标格，消费按键原地不动。支持 shift 扩展选区。
    fn table_move_horizontal(
        &mut self,
        forward: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let cur = self.buffer.cursor();
        let lines: Vec<Line> = self.buffer.lines().to_vec();
        // 预先算出「哪些行是表格行」，闭包不再借用 `self`（`self.edit`
        // 需要可变借用，闭包持不可变借用会冲突）。
        let table_lines = self.table_line_flags();
        let is_table = |l: usize| table_lines.get(l).copied().unwrap_or(false);
        let Some((row, col, (cs, ce))) = table_cell_at(&lines, &is_table, cur.line, cur.col) else {
            return false;
        };
        let at_edge = if forward {
            cur.col >= ce
        } else {
            cur.col <= cs
        };
        if !at_edge {
            return false;
        }
        let target_row = if forward {
            row + 1
        } else {
            match row.checked_sub(1) {
                Some(r) => r,
                None => {
                    // 表头格首：同列无上一格，不环绕，原地消费。
                    self.after_move(cx);
                    return true;
                }
            }
        };
        let moved = self.edit(cx, |m| {
            table_goto_cell(
                m, &lines, &is_table, cur.line, target_row, col, !forward, extend,
            )
        });
        if !moved {
            // 末行格尾（或目标行列数不足）：无下一格，原地消费。
            self.after_move(cx);
            return true;
        }
        self.after_move(cx);
        true
    }

    /// 某行是否属于表格块（读上一帧的行分类，与 Tab/Enter 分支同源）。
    fn line_is_table(&self, line: usize) -> bool {
        self.planned
            .iter()
            .find(|r| r.line == line)
            .is_some_and(|r| matches!(r.block, Block::Table))
    }

    /// 全文档「哪些行是表格行」的位图。闭包持 `self` 的不可变借用会与
    /// `self.edit` 的可变借用冲突，需要事先物化。
    ///
    /// 表格行分类来自按文本版本缓存的 `planned_geometry`（`Block::Table` 是
    /// 纯文本分类），所以拖拽这条每帧鼠标移动的热路径不再触碰 plan 内容。
    fn table_line_flags(&self) -> Vec<bool> {
        let mut v = vec![false; self.buffer.line_count()];
        let geo = &self.planned_geometry;
        for gi in 0..geo.group_count() {
            if !geo.group_is_table(gi) {
                continue;
            }
            if let Some((a, b)) = geo.group_row_bounds(gi) {
                for r in a..=b {
                    if r < v.len() {
                        v[r] = true;
                    }
                }
            }
        }
        v
    }

    /// 光标行是否**可能**落在表格块里 —— [`Self::auto_prettify_table`] 的廉价
    /// 前置门控。返回 `false` 时可以安全跳过重排。
    ///
    /// 判据分两级：
    /// 1. 缓存几何新鲜（`text_version` 一致）且**没有任何表格 group** → 直接
    ///    否定。分类是纯文本函数，缓存新鲜即权威。
    /// 2. 否则看光标所在的**段落 run**（前后到空行为止的连续行）里有没有 `|`。
    ///    表格不可能跨空行（空行会把 `DocState::in_table` 清掉），而且任何表格
    ///    行都必然与含 `|` 的表头/分隔行同处一个 run —— 所以 run 内无 `|` 就
    ///    一定不是表格。这是**保守否定**：run 内有 `|` 时照常走完整路径。
    ///
    /// 这样非表格文档的每次击键不再克隆全文、也不再全量分类（此前
    /// `table_block_range` 内部无条件 `classify_document`）。
    fn cursor_line_is_table(&self) -> bool {
        if self.planned_geometry_version == self.buffer.text_version() {
            // `has_table` is precomputed in the geometry: scanning every group
            // here was O(groups) on every keystroke, which is exactly the kind
            // of per-edit document-scale work this optimization removes.
            if !self.planned_geometry.has_table {
                return false;
            }
        }
        let lines = self.buffer.lines();
        let cur = self.buffer.cursor().line.min(lines.len().saturating_sub(1));
        let has_pipe = |i: usize| lines.get(i).is_some_and(|l| l.contains('|'));
        // Backwards to the previous blank line.
        let mut i = cur;
        loop {
            if has_pipe(i) {
                return true;
            }
            let blank = lines.get(i).is_none_or(|l| l.trim().is_empty());
            if i == 0 || blank {
                break;
            }
            i -= 1;
        }
        // Forwards to the next blank line.
        let mut i = cur + 1;
        while i < lines.len() {
            let blank = lines.get(i).is_none_or(|l| l.trim().is_empty());
            if blank {
                break;
            }
            if has_pipe(i) {
                return true;
            }
            i += 1;
        }
        false
    }

    /// 光标是否落在表格块里。
    fn cursor_in_table(&self) -> bool {
        self.line_is_table(self.buffer.cursor().line)
    }

    /// 表格里的 Enter（Typora）：移到**下方同列**格；没有下一行则退出表格。
    /// 返回 `true` 表示已处理。
    fn table_enter_down(&mut self, cx: &mut Context<Self>) -> bool {
        let cur = self.buffer.cursor();
        let lines: Vec<Line> = self.buffer.lines().to_vec();
        // 预先算出「哪些行是表格行」，闭包不再借用 `self`（`self.edit`
        // 需要可变借用，闭包持不可变借用会冲突）。
        let table_lines = self.table_line_flags();
        let is_table = |l: usize| table_lines.get(l).copied().unwrap_or(false);
        let Some((row, col, _)) = table_cell_at(&lines, &is_table, cur.line, cur.col) else {
            return false;
        };
        let moved = self.edit(cx, |m| {
            table_goto_cell(m, &lines, &is_table, cur.line, row + 1, col, false, false)
        });
        if moved {
            self.after_move(cx);
            return true;
        }
        // 末行：退出表格（落到表格后一行）。
        if let Some(last) = table_last_row_line(&lines, &is_table, cur.line) {
            let exit = (last + 1).min(lines.len().saturating_sub(1));
            self.edit(cx, |m| m.move_to(exit, 0, false));
            self.after_move(cx);
            return true;
        }
        false
    }

    /// 表格里的 Esc（Typora）：**不退出表格**，只关闭弹窗类 UI。
    /// 这里保留搜索面板关闭，其余情况清掉选区/不做动作。
    fn table_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.cursor_in_table() {
            return false;
        }
        if self.buffer.selection().is_some() {
            self.edit(cx, |m| m.clear_selection());
            cx.notify();
        }
        true
    }

    /// 表格活动单元格：光标所在行 + 列 + 该格的**内容字节范围**。
    ///
    /// 与渲染分支共用同一套切分（`block::table_cell_spans`），所以点哪个格
    /// 就编辑哪个格，不会出现「Grid 画 3 列、Tab 走 2 格」这类错位。
    fn table_active_cell(
        &self,
        rows: &[Rc<PlannedRow>],
        idxs: &[usize],
        cursor: Pos,
    ) -> Option<(usize, usize, std::ops::Range<usize>)> {
        let cursor_line = cursor.line;
        // 光标行必须真的属于这个表格组（防御：调用点已保证，双保险）。
        idxs.iter()
            .copied()
            .find(|pi| rows[*pi].line == cursor_line)?;
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
    fn set_mermaid_showing_code(&mut self, block_id: usize, showing: bool, cx: &mut Context<Self>) {
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

    /// 切换某块（表格 / mermaid 卡片）的「全宽显示」。
    ///
    /// 全宽 = 卡片脱离 780px 阅读窄栏，占满编辑器可用宽度。窄栏对宽表是
    /// 灾难：列数一多每列只剩几十像素，内容全被压成一列一个字。全宽之外
    /// 还有横向滚动（`scrollbar` 模块）兜底。
    pub(crate) fn toggle_wide_block(&mut self, block_id: usize, cx: &mut Context<Self>) {
        if !self.wide_blocks.insert(block_id) {
            self.wide_blocks.remove(&block_id);
        }
        cx.notify();
    }

    /// 全文档宽度切换（状态条「全宽」按钮）：`false` = 780px 阅读窄栏，
    /// `true` = 所有行占满编辑器宽度。
    ///
    /// 宽度变了，每行的换行位置随之改变：上一帧实测的组高全部作废，
    /// 否则虚拟化会拿旧高度估算滚动总高（大文档滚动条跳变）。
    pub(crate) fn toggle_wide_doc(&mut self, cx: &mut Context<Self>) {
        self.wide_doc = !self.wide_doc;
        self.group_heights.clear();
        self.prefix_dirty = true;
        cx.notify();
    }

    /// 收尾所有进行中的滚动条拖拽（窗口失活时调用）。
    ///
    /// 拖拽状态由窗口级 MouseUp 清除，而失活后窗口收不到该事件：拖到一半
    /// Cmd-Tab 会让 `grab` 永久留在 `Some`，`on_fallback_drag` 之后每次拖拽
    /// 都让行，静默禁用该路径下的选区扩展。
    fn end_scroll_drags(&mut self, cx: &mut Context<Self>) {
        self.h_scroll.clear_drags(cx);
        if let Some(v) = self.v_scroll.clone() {
            v.update(cx, |s, _| s.clear_drag());
        }
    }

    /// 注册「窗口失活 → 收尾拖拽」监听（每窗口一次）。
    ///
    /// `observe_window_activation` 需要 `&mut Window`，而视图构造时拿不到窗口，
    /// 所以在 render 里惰性注册。
    fn ensure_activation_observer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.activation_observer.is_some() {
            return;
        }
        self.activation_observer =
            Some(cx.observe_window_activation(window, |view, window, cx| {
                if !window.is_window_active() {
                    view.end_scroll_drags(cx);
                }
            }));
    }

    /// 某块是否处于「全宽显示」。
    pub(crate) fn is_wide_block(&self, block_id: usize) -> bool {
        self.wide_blocks.contains(&block_id)
    }

    /// 某块实际是否按「全宽」渲染：**文档级全宽**下所有块都是全宽，否则按
    /// 逐块状态。渲染分支与缩放基准必须走这个（而不是 `is_wide_block`），
    /// 否则文档全宽时 mermaid 图仍按阅读态 640 上限展示、表格卡片也不撑开。
    pub(crate) fn block_is_wide(&self, block_id: usize) -> bool {
        self.wide_doc || self.is_wide_block(block_id)
    }

    /// 某 mermaid 块的**显式**缩放倍率（`None` = 按适配宽度展示）。
    #[cfg(test)]
    pub(crate) fn mermaid_zoom(&self, block_id: usize) -> Option<f32> {
        self.mermaid.zoom(block_id)
    }

    /// 放大 / 缩小 / 还原某 mermaid 块（`+` / `−` / 百分比按钮）。
    ///
    /// `+` / `−` 从**当前实际显示**的倍率出发，而不是从 100%：默认展示可能是
    /// 「阅读态 640 上限」或「全宽适配宽度」，其等效倍率通常不是 1（大图远小于
    /// 1，小图可能大于 1）。从 100% 出发会让第一次点击跳变（例如 0.6 → 1.25，
    /// 或 4.0 → 5.0 直接被夹住毫无反应），从当前值出发才是连续缩放。
    ///
    /// 基准必须与百分比标签同源（[`Self::mermaid_display_scale`]），否则标签
    /// 显示的和 `+` 实际跳到的位置会对不上。
    pub(crate) fn mermaid_zoom_by(
        &mut self,
        block_id: usize,
        action: MermaidZoom,
        cx: &mut Context<Self>,
    ) {
        match action {
            MermaidZoom::Reset => self.mermaid.clear_zoom(block_id),
            MermaidZoom::In | MermaidZoom::Out => {
                let cur = self.mermaid.zoom(block_id).unwrap_or_else(|| {
                    self.mermaid_display_scale(block_id, self.block_is_wide(block_id))
                });
                let next = match action {
                    MermaidZoom::In => cur * ZOOM_STEP,
                    _ => cur / ZOOM_STEP,
                };
                self.mermaid.set_zoom(block_id, next);
            }
        }
        cx.notify();
    }

    /// **无显式缩放时**图的实际显示倍率（相对自然尺寸）。
    ///
    /// 与渲染分支的取尺寸逻辑一一对应（`display_size` / `fit_width` 都是等比
    /// 缩放），所以百分比标签与图的实际大小永远一致 —— 两处各写一套比例是
    /// 标签撒谎的直接原因。
    pub(crate) fn mermaid_display_scale(&self, block_id: usize, wide: bool) -> f32 {
        let Some(nat_w) = self.mermaid_natural_width(block_id) else {
            return 1.0;
        };
        if nat_w <= 0.0 {
            return 1.0;
        }
        if wide {
            match self.mermaid_viewport_w(block_id) {
                Some(vp) => (vp / nat_w).clamp(1.0 / ZOOM_MAX, ZOOM_MAX),
                None => 1.0,
            }
        } else {
            // 阅读态：`MAX_DISPLAY_W / nat_w`，且只缩不放。
            (MAX_DISPLAY_W / nat_w).min(1.0)
        }
    }

    /// 某块滚动视口的实测宽度（上一帧；`None` = 还没测到）。
    fn mermaid_viewport_w(&self, block_id: usize) -> Option<f32> {
        let w = f32::from(self.h_scroll.handle(block_id)?.bounds().size.width);
        (w > 1.0).then_some(w)
    }

    /// 某块图的自然宽度（渲染未完成时为 `None`）。
    fn mermaid_natural_width(&self, block_id: usize) -> Option<f32> {
        let src = self.planned_geometry.mermaid_source(block_id)?;
        match self.mermaid.body(src) {
            MermaidBody::Image { image, .. } => Some(image.natural_size().0),
            MermaidBody::Source { .. } => None,
        }
    }

    /// 收帧：把「全宽」状态裁剪到本帧仍存在的块。
    ///
    /// 行号会随编辑漂移（在表格上方插一行，整块锚点就变了），不裁剪的话
    /// 一个残留行号可能命中另一个块、把它意外撑成全宽。裁剪是保守的：漂移后
    /// 拿不到原锚点的块回到窄栏，而不是赌它还是同一个块。
    fn prune_wide_blocks(&mut self) {
        if self.wide_blocks.is_empty() {
            return;
        }
        let anchors = &self.planned_geometry.wide_anchors;
        self.wide_blocks
            .retain(|b| anchors.binary_search(b).is_ok());
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
            let is_mermaid = lang
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"));
            let open_pi = i;
            i += 1;
            let mut last_code = open_pi;
            while i < rows.len() && matches!(rows[i].block, Block::CodeLine) {
                last_code = i;
                i += 1;
            }
            let close_pi =
                if i < rows.len() && matches!(rows[i].block, Block::Fence { open: false, .. }) {
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
            let fence_revealed =
                rows[open_pi].plan.revealed || close_pi.is_some_and(|c| rows[c].plan.revealed);
            let revealed = fence_revealed && self.show_source;
            if revealed || self.mermaid.is_showing_code(block_id) {
                continue;
            }
            // 这条路径是光标禁区判定（低频），没有 geometry 的 Arc 可用，
            // 现场 join 后转 `Arc<str>` 供 `body` 查询（`From<String>` 直接
            // 接管缓冲区，不多一次整块拷贝）。
            let source: Arc<str> = (open_pi + 1..=last_code)
                .map(|pi| self.buffer.line(rows[pi].line).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
                .into();
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
        let next_pos = info.layout.position_for_index((vis + 1).min(info.text_len));
        let (vrow_f, caret_x_rel) = match next_pos {
            Some(np) if vis < info.text_len && f32::from(np.y) > pos_y => {
                // `vis` starts a new visual row: its row is the next one and
                // its x is the row start.
                (((f32::from(np.y - origin.y)) / line_h).round(), 0.0)
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
                (
                    row.plan.visible.len(),
                    row.plan.segs.clone(),
                    row.plan.link_at(src).map(str::to_string),
                )
            }
            None => return,
        };
        let _ = vis_len;
        let vis = raw.min(vis_len);
        let src = src_for_vis(&segs, vis);
        if (platform || self.read_only)
            && let Some(target) = link
        {
            cx.emit(LiveViewEvent::LinkClick(target));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let col = col_for_byte(&src_line, src);
        // Pointer-driven: the clicked row is on screen by construction, so
        // never scroll (see `edit_no_scroll`).
        self.edit_no_scroll(cx, |m| {
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

    pub fn on_line_drag(
        &mut self,
        line: usize,
        raw: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        if self.drag_cross_table(line) {
            return;
        }
        let (vis_len, segs, src_line) = match self.planned.iter().find(|r| r.line == line) {
            Some(row) => (
                row.plan.visible.len(),
                row.plan.segs.clone(),
                self.buffer
                    .line(line)
                    .map(str::to_string)
                    .unwrap_or_default(),
            ),
            None => return,
        };
        let vis = raw.min(vis_len);
        let col = col_for_byte(&src_line, src_for_vis(&segs, vis));
        self.edit_no_scroll(cx, |m| m.move_to(line, col, true));
        // Claim the drag so the dead-zone fallback doesn't also run.
        cx.stop_propagation();
    }

    pub fn on_line_mouse_up(&mut self, cx: &mut Context<Self>) {
        // The gesture is over: let the table toolbar re-evaluate from the
        // caret's new position on the next frame.
        self.toolbar_frozen = None;
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    /// Whether to lay out `table_start`'s toolbar on this frame.
    ///
    /// Normally this follows "is the caret inside the table". While a pointer
    /// gesture is in progress the answer is pinned to what it was when the
    /// gesture started, because the toolbar sits at the table's top: revealing
    /// it mid-drag grows the table, slides the rows down, and leaves the
    /// pointer over the wrong row (see [`Self::toolbar_frozen`]).
    fn table_toolbar_visible(&self, table_start: usize, cursor_in_table: bool) -> bool {
        match self.toolbar_frozen {
            Some((frozen_start, was_visible)) if frozen_start == table_start => was_visible,
            _ => cursor_in_table,
        }
    }

    /// Pin the toolbar state of the table containing `line` for the gesture
    /// that just started.
    ///
    /// Must be called **before** the caret moves into the table, so the frozen
    /// value describes the layout the press was hit-tested against. A no-op for
    /// lines that are not in a table.
    fn freeze_table_toolbar_for_line(&mut self, line: usize) {
        if !self.line_is_table(line) {
            return;
        }
        let flags = self.table_line_flags();
        let Some((start, _)) = crate::editor::markdown_live::table_ops::table_block_range_flagged(
            self.buffer.lines(),
            &flags,
            line,
        ) else {
            return;
        };
        let was_visible = self.toolbar_visible_now(start);
        self.toolbar_frozen = Some((start, was_visible));
    }

    /// The toolbar state currently on screen for `table_start`, honouring an
    /// existing freeze.
    fn toolbar_visible_now(&self, table_start: usize) -> bool {
        if let Some((frozen_start, was_visible)) = self.toolbar_frozen
            && frozen_start == table_start
        {
            return was_visible;
        }
        // No freeze: the toolbar is shown exactly when the caret is in this
        // table — the same predicate `render` uses (`cursor_in_table`). The
        // membership test reads the version-cached geometry, so this stays off
        // the classification path.
        let cur = self.buffer.cursor().line;
        if !self.line_is_table(cur) {
            return false;
        }
        let flags = self.table_line_flags();
        crate::editor::markdown_live::table_ops::table_block_range_flagged(
            self.buffer.lines(),
            &flags,
            cur,
        )
        .is_some_and(|(s, _)| s == table_start)
    }

    pub fn toggle_task_at(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(text) = self.buffer.line(line).map(str::to_string) else {
            return;
        };
        let changed = self.edit_no_scroll(cx, |m| toggle_task(m, &text, line));
        if changed {
            self.after_edit(cx);
        }
    }

    // ---- keyboard --------------------------------------------------------

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        use crate::editor::markdown_live::table_ops as t;
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
        if self.search_panel_focused(window, cx)
            && !matches!((key, platform), ("f", true) | ("g", true) | ("escape", _))
        {
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
        if !platform
            && !control
            && !alt
            && self.buffer.selection().is_none()
            && !self.search_panel_focused(window, cx)
            && let Some((query, items)) = self.slash_menu()
        {
            match key {
                "up" => {
                    self.slash_index = (self.slash_index + items.len() - 1) % items.len();
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
                // spec #8/#14：Alt+← 表格里整列左移；普通文本按词左移。
                // （Cmd/Ctrl+方向键绕过表格导航，见下方 gate。）
                if !platform && alt {
                    if self.cursor_in_table() {
                        self.run_table_op(cx, |m| t::move_column(m, true));
                        return;
                    }
                    self.edit(cx, |m| m.word_left(shift));
                    self.clamp_cursor_off_table_pipe(cx);
                    self.after_move(cx);
                    return;
                }
                // 表格格首 → 同列上一格（Typora spec #8 的纵向换行）。
                if !platform && !control && !alt && self.table_move_horizontal(false, shift, cx) {
                    return;
                }
                self.edit(cx, |m| {
                    if platform {
                        m.home(shift)
                    } else {
                        m.move_left(shift)
                    }
                });
                self.clamp_cursor_off_table_pipe(cx);
                self.after_move(cx);
            }
            "right" => {
                // spec #8/#14：Alt+→ 表格里整列右移；普通文本按词右移。
                if !platform && alt {
                    if self.cursor_in_table() {
                        self.run_table_op(cx, |m| t::move_column(m, false));
                        return;
                    }
                    self.edit(cx, |m| m.word_right(shift));
                    self.clamp_cursor_off_table_pipe(cx);
                    self.after_move(cx);
                    return;
                }
                // 表格格尾 → 同列下一格（Typora spec #8 的纵向换行）。
                if !platform && !control && !alt && self.table_move_horizontal(true, shift, cx) {
                    return;
                }
                self.edit(cx, |m| {
                    if platform {
                        m.end(shift)
                    } else {
                        m.move_right(shift)
                    }
                });
                self.clamp_cursor_off_table_pipe(cx);
                self.after_move(cx);
            }
            "up" => {
                if platform {
                    self.edit(cx, |m| m.doc_start(shift));
                    self.after_move(cx);
                } else if alt && self.cursor_in_table() {
                    // spec #14：Alt+↑ 上移整行（表头等不可动情形按键原地消费）。
                    let changed = self.run_table_op(cx, |m| t::move_row(m, true));
                    if !changed {
                        cx.stop_propagation();
                    }
                } else if control {
                    // spec #8：Ctrl+↑ 绕过表格导航。
                    self.move_visual(false, shift, cx);
                } else if self.table_move_vertical(false, shift, cx) {
                    // 表格内：按列索引上移（Typora）。
                } else {
                    self.move_visual(false, shift, cx);
                }
            }
            "down" => {
                if platform {
                    self.edit(cx, |m| m.doc_end(shift));
                    self.after_move(cx);
                } else if alt && self.cursor_in_table() {
                    // spec #14：Alt+↓ 下移整行（末行等不可动情形按键原地消费）。
                    let changed = self.run_table_op(cx, |m| t::move_row(m, false));
                    if !changed {
                        cx.stop_propagation();
                    }
                } else if control {
                    // spec #8：Ctrl+↓ 绕过表格导航。
                    self.move_visual(true, shift, cx);
                } else if self.table_move_vertical(true, shift, cx) {
                    // 表格内：按列索引下移（Typora）。
                } else {
                    self.move_visual(true, shift, cx);
                }
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
            }
            "backspace" => {
                if self.read_only {
                    return;
                }
                // Marker strip first (plain backspace, no selection).
                let cur = self.buffer.cursor();
                let sel_none = self.buffer.selection().is_none();
                // spec #12：表格里 Cmd+Shift+退格 = 删除整行（无选区时）；
                // 表格外保持原来的「删到行首」。
                if sel_none && platform && shift && self.cursor_in_table() {
                    self.run_table_op(cx, t::delete_row);
                    return;
                }
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
                // 表格格首退格（Typora）：光标在该格内容起点时，跳到上一格
                // **末**，不删字符、不合并单元格。必须在通用 backspace 之前
                // 判断，否则行内字符会被先删掉。
                if sel_none && !platform && !alt && self.table_backspace_at_start(cx) {
                    return;
                }
                // spec #12：表格里 Cmd+退格删到**格内容首**（不是行首，避免
                // 把管道和同排格子一起切掉）。
                let cell_cs = if sel_none && platform && !shift && self.cursor_in_table() {
                    self.table_cell_content_at(cur.line, cur.col)
                        .map(|(cs, _)| cs)
                } else {
                    None
                };
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        // spec #6：跨格选区走结构化合并（首尾格合并、中间清空），
                        // 同格选区仍走常规文本删除。
                        t::merge_cross_cell_selection(m, "") || m.delete_selection()
                    } else if platform {
                        match cell_cs {
                            Some(cs) if cur.col > cs => {
                                m.move_to(cur.line, cs, true);
                                m.delete_selection()
                            }
                            Some(_) => false, // 已在格首：原地消费
                            None => {
                                m.home(true);
                                m.delete_selection()
                            }
                        }
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
            }
            "delete" => {
                if self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let sel_none = self.buffer.selection().is_none();
                // spec #12：光标在格内容**末尾** → 命中零宽哨兵，Delete 无操作；
                // 表格外（或格中间）不受影响。同时给 Cmd+Delete 提供格尾边界。
                let cell_ce = if sel_none && self.cursor_in_table() {
                    self.table_cell_content_at(cur.line, cur.col)
                        .map(|(_, ce)| ce)
                } else {
                    None
                };
                if !platform && !alt && cell_ce.is_some_and(|ce| cur.col >= ce) {
                    cx.stop_propagation();
                    return;
                }
                let changed = self.edit(cx, |m| {
                    if m.selection().is_some() {
                        // spec #6：跨格选区合并删除，同格选区常规删除。
                        t::merge_cross_cell_selection(m, "") || m.delete_selection()
                    } else if platform {
                        match cell_ce {
                            Some(ce) if cur.col < ce => {
                                m.move_to(cur.line, ce, true);
                                m.delete_selection()
                            }
                            Some(_) => false, // 已在格尾：原地消费
                            None => {
                                m.end(true);
                                m.delete_selection()
                            }
                        }
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
            }
            "enter" => {
                // Typora 表格内的 Enter 家族优先（否则会被下面的
                // Cmd+Enter 任务切换等分支抢先消费）：
                //  - Shift+Enter → 插入 `<br />`（GFM 单元格不支持换行）
                //  - Cmd/Ctrl+Enter → 在当前行下方插入空行
                //  - Enter → 移到**下方同列**格；末行则退出表格
                if !self.read_only && self.cursor_in_table() {
                    if self.buffer.selection().is_some() {
                        // spec #6：跨格选区先结构化合并（同格走常规删除），
                        // 否则 Enter 会按行拼接、毁掉表格。
                        self.edit(cx, |m| {
                            t::merge_cross_cell_selection(m, "") || m.delete_selection()
                        });
                    }
                    if shift {
                        self.edit(cx, |m| m.insert("<br />"));
                        self.after_edit(cx);
                        return;
                    }
                    if platform || control {
                        if self.edit(cx, table_insert_row_below) {
                            self.after_edit(cx);
                        }
                        return;
                    }
                    if self.table_enter_down(cx) {
                        return;
                    }
                }
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
                let text = self
                    .buffer
                    .line(cur.line)
                    .map(str::to_string)
                    .unwrap_or_default();
                let planned_block = self
                    .planned
                    .iter()
                    .find(|r| r.line == cur.line)
                    .map(|r| r.block.clone());
                let in_code = matches!(
                    planned_block,
                    Some(Block::CodeLine | Block::Fence { .. } | Block::FrontMatter)
                );
                self.edit(cx, |m| {
                    on_enter(m, &text, in_code);
                });
                self.after_edit(cx);
            }
            "tab" => {
                if platform || self.read_only {
                    return;
                }
                let cur = self.buffer.cursor();
                let text = self
                    .buffer
                    .line(cur.line)
                    .map(str::to_string)
                    .unwrap_or_default();
                let tab_size = self.tab_size;
                // Typora Tab inside a table: grid navigation (no selection).
                if self.buffer.selection().is_none() && self.line_is_table(cur.line) {
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
                } else {
                    // 代码块 Shift+Tab：无操作也必须停传播 —— 否则平台
                    // 回退路径会把 key_char（"\t"）当可打印文本再插一遍。
                    cx.stop_propagation();
                }
            }
            "escape" => {
                if let Some(panel) = self.search_panel.clone()
                    && panel.read(cx).is_open()
                {
                    panel.update(cx, |p, cx| p.hide(window, cx));
                    cx.stop_propagation();
                    return;
                }
                // Typora：Esc 在表格里**不退出表格**，只关闭弹窗类 UI
                //（搜索面板已在上方处理）。这里额外清掉选区，其余不动。
                if self.table_escape(cx) {
                    cx.stop_propagation();
                    return;
                }
                if self.buffer.selection().is_some() {
                    self.edit(cx, |m| m.clear_selection());
                    cx.notify();
                }
            }
            "a" if platform => {
                // Typora：表格里 Cmd/Ctrl+A 只选**当前格**内容，不升级到
                // 整表/整文档。
                if self.select_table_cell_contents(cx) {
                    cx.stop_propagation();
                    return;
                }
                self.edit(cx, |m| m.select_all());
                cx.notify();
                cx.stop_propagation();
            }
            // Typora 的表格选择快捷键：Cmd/Ctrl+E 选格、Cmd/Ctrl+L 选整行。
            "e" if platform => {
                if self.select_table_cell_contents(cx) {
                    cx.stop_propagation();
                }
            }
            "l" if platform => {
                // spec #14 快捷键：Cmd+Shift+Alt+L = 删除行——表格里删除本行，
                // 表格外删除光标整行（连同行尾换行）。
                if shift && alt {
                    if !self.read_only {
                        if self.cursor_in_table() {
                            self.run_table_op(cx, t::delete_row);
                        } else {
                            let line = self.buffer.cursor().line;
                            let changed = self.edit(cx, |m| {
                                let len = m.line(line).map(|l| l.chars().count()).unwrap_or(0);
                                if line + 1 < m.line_count() {
                                    // 连同行尾换行一起删。
                                    m.move_to(line, 0, false);
                                    m.move_to(line + 1, 0, true);
                                } else if line > 0 {
                                    // 末行：连同上一行末尾的换行一起删。
                                    let prev_len =
                                        m.line(line - 1).map(|l| l.chars().count()).unwrap_or(0);
                                    m.move_to(line - 1, prev_len, false);
                                    m.move_to(line, len, true);
                                } else {
                                    m.move_to(line, 0, false);
                                    m.move_to(line, len, true);
                                }
                                m.delete_selection()
                            });
                            if changed {
                                self.after_edit(cx);
                            }
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                if self.select_table_row(cx) {
                    cx.stop_propagation();
                }
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
            }
            "c" if platform => {
                self.copy_selection_or_table_row(cx);
                cx.stop_propagation();
            }
            "x" if platform => {
                if self.read_only {
                    if let Some(t) = self.selected_source_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                    }
                    cx.stop_propagation();
                    return;
                }
                let cut = self.edit(cx, |m| {
                    // spec #6：跨格剪切 = 先取选区文本，再结构化合并删除
                    // （m.cut 的 remove_selection 会按行拼接、毁掉表格）。
                    let taken = m.selected_text();
                    if t::merge_cross_cell_selection(m, "") {
                        taken
                    } else {
                        m.cut()
                    }
                });
                match cut {
                    Some(t) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                        self.after_edit(cx);
                    }
                    None => cx.stop_propagation(),
                }
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
                            // spec #6：跨格选区的粘贴 = 合并删除后从首格分发
                            //（typed 先并入首格合并点，随后 paste 再按行列展开），
                            // 合并与写入并成一个撤销步；无选区/同格照旧。
                            let handled = self.run_table_op(cx, |m| {
                                if t::merge_cross_cell_selection(m, &text) {
                                    return true;
                                }
                                t::paste_into_table(m, &text)
                            });
                            if handled {
                                self.after_edit(cx);
                                return;
                            }
                            self.edit(cx, |m| m.insert(&text));
                            self.after_edit(cx);
                            return;
                        }
                    }
                }
                cx.stop_propagation();
            }
            "z" if platform => {
                if self.read_only {
                    cx.stop_propagation();
                    return;
                }
                // spec #13：撤销/重做恢复历史原样，跳过一次自动重排。
                self.skip_reformat = true;
                let changed = self.edit(cx, |m| if shift { m.redo() } else { m.undo() });
                if changed {
                    self.after_edit(cx);
                } else {
                    cx.stop_propagation();
                }
                self.skip_reformat = false;
            }
            "f" if platform => {
                // 代码编辑器同款查找面板（只读也允许查找，替换行自动隐藏）。
                self.open_search(window, cx);
                cx.stop_propagation();
            }
            "g" if platform => {
                if self.search_panel.is_none() {
                    self.open_search(window, cx);
                    if shift && let Some(panel) = self.search_panel.clone() {
                        panel.update(cx, |p, cx| p.jump(-1, window, cx));
                    }
                } else if let Some(panel) = self.search_panel.clone() {
                    panel.update(cx, |p, cx| p.jump(if shift { -1 } else { 1 }, window, cx));
                }
                cx.stop_propagation();
            }
            "/" if platform => {
                // Typora 源码模式切换（与状态条"源码"芯片同一开关）。
                self.toggle_show_source(cx);
                cx.stop_propagation();
            }
            "o" if platform && shift => {
                // Typora 大纲侧栏显隐（与状态条按钮同一开关）。
                self.show_outline = !self.show_outline;
                cx.notify();
                cx.stop_propagation();
            }
            _ => {}
        }
        // 可打印文本不再在这里写：统一走平台 `EntityInputHandler`
        //（`replace_text_in_range` → [`Self::type_text`]）。本函数只处理
        // 具名键；未消费的可打印键由平台回退到 input handler，中文 IME
        // 的 `insertText` 与 ASCII 输入因此走同一条写路径。
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

    /// The source text of the fenced code block whose opening fence is at
    /// source line `block_id` (used by the copy button).
    ///
    /// Reads the buffer at call time: the joined text is only needed on click,
    /// so the render path never builds it. The block's row range comes from the
    /// cached geometry, so this is exact even for nested fences.
    fn code_block_source(&self, block_id: usize) -> String {
        let geo = &self.planned_geometry;
        for gi in 0..geo.group_count() {
            let Some((code, _lang)) = geo.group_fence(gi) else {
                continue;
            };
            if geo.group_fence_open_line(gi) != Some(block_id) {
                continue;
            }
            return code
                .iter()
                .map(|pi| self.buffer.line(*pi).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
        }
        String::new()
    }

    /// 图片卡片：本地文件 / 远程 URL 统一走文档级缓存 + 后台加载。
    fn image_card_cached(
        &mut self,
        url: &str,
        alt: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let cache = self.image_cache.clone();
        // `doc_path` 是**文档文件**路径（粘贴归档用它定位 `<stem>.assets/`）；
        // 相对图片引用要的是**所在目录**，不是文件本身。
        let doc_dir = self
            .doc_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let mut ctx = crate::editor::md_image::ImageContext::from_doc_dir(doc_dir);
        ctx.roots = self.image_roots.clone();
        let state = cache.update(cx, |cache, cx| cache.slot(url, &ctx, cx));
        MarkdownElementBuilder::image_card_slot(url, alt, None, state, cx)
    }

    /// 一行里的多张图片：横向排布（`README` 徽章风格），自动换行。
    ///
    /// 单张也走这里，保证「纯图片行」的间距/宽度上限只有一处口径。
    fn image_strip(
        &mut self,
        images: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut strip = gpui::div()
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_2();
        for (alt, url) in images {
            strip = strip.child(
                gpui::div()
                    .flex_shrink_0()
                    .max_w(px(320.0))
                    .child(self.image_card_cached(url, alt, cx)),
            );
        }
        strip.into_any_element()
    }

    /// 测试探针：图片缓存实体（渲染路径实际使用的那一个）。
    #[cfg(test)]
    pub(crate) fn image_cache_entity(&self) -> Entity<crate::editor::md_image::ImageCache> {
        self.image_cache.clone()
    }

    /// 将当前文本同步进查找面板（防抖落地点）。
    ///
    /// 面板已关时跳过（没有匹配要维护）。`SearchMatcher::update` 内部有
    /// `Rope` 相等性早退，因此这里无需自己判重。
    fn flush_search_sync(&mut self, cx: &mut Context<Self>) {
        self.search_sync_version = None;
        let Some(panel) = self.search_panel.clone() else {
            return;
        };
        if !panel.read(cx).is_open() {
            return;
        }
        let text = Rope::from(self.buffer.text());
        panel.update(cx, |p, _| p.sync_text(&text));
    }

    /// 立即把文本同步进查找面板（打开面板 / 跳转 / 替换前调用，保证匹配
    /// 与当前文本一致）。
    fn sync_search_now(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.search_panel.clone() {
            let text = Rope::from(self.buffer.text());
            panel.update(cx, |p, _| p.sync_text(&text));
        }
    }

    /// 查找面板匹配同步的防抖窗口（面板开着时）。
    const SEARCH_SYNC_DEBOUNCE_MS: u64 = 120;

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
    f32::from(b.font_size.to_pixels(px(16.)))
        .to_bits()
        .hash(&mut h);
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

/// Whether a source line *could* be a CommonMark reference definition.
///
/// This is a conservative pre-filter for the incremental refdef cache: false
/// positives only cause an unnecessary full scan, while a false negative would
/// leave stale link targets. Matches `collect_refdefs`'s shape (≤3 leading
/// spaces, `[label]:`, not a footnote).
fn line_may_be_refdef(line: &str) -> bool {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    rest.starts_with('[') && !rest.starts_with("[^")
}

/// Outline entries `(source line, level, title)` in document order.
fn outline_entries(rows: &[Rc<PlannedRow>]) -> Vec<(usize, u8, String)> {
    rows.iter()
        .filter_map(|r| match &r.block {
            Block::Heading { level, .. } => Some((r.line, *level, outline_title(&r.plan.visible))),
            _ => None,
        })
        .collect()
}

/// Outline display title: single line, max 42 chars.
fn outline_title(visible: &str) -> String {
    let one = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.is_empty() {
        return "(空标题)".to_string();
    }
    if one.chars().count() > 42 {
        format!("{}…", one.chars().take(42).collect::<String>())
    } else {
        one
    }
}

/// mermaid 图卡片的缩放动作（`+` / `-` / 百分比还原按钮）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MermaidZoom {
    In,
    Out,
    Reset,
}

/// 表格拖拽负载：拖动行/列把手时携带（Typora 的 tracker 拖拽重排）。
#[derive(Clone)]
struct TableDrag {
    /// 表格块首行（用于重新定位块）。
    table_start: usize,
    /// 行拖拽 = 源行的**行号**；列拖拽 = 源列索引（`drag_move_row` 收行号、
    /// `drag_move_column` 收列索引）。
    index: usize,
    /// 行还是列。
    is_row: bool,
}

impl gpui::Render for TableDrag {
    /// 拖拽跟随光标的预览：一个小的「行/列」标签。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui;
        // 行的 `index` 是源行行号（对用户没有显示意义），只标「移动行」；
        // 列的 `index` 是列索引，显示 1 基列号。
        let label = if self.is_row {
            "移动行".to_string()
        } else {
            format!("移动列 {}", self.index + 1)
        };
        div()
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .bg(ui.panel_background)
            .border_1()
            .border_color(ui.border)
            .text_size(px(11.0))
            .text_color(ui.text)
            .child(label)
    }
}

/// 给行/列把手挂上拖拽三件套：`on_drag` 预览、`drag_over` 高亮出**插入
/// 标记**（把手本身就是细条，高亮即标记）、`on_drop` 执行行/列重排。
fn attach_table_drag(
    mut g: gpui::Stateful<gpui::Div>,
    payload: TableDrag,
    ent: Entity<LiveMarkdownView>,
    accent: Hsla,
    drop_border: Hsla,
) -> gpui::Stateful<gpui::Div> {
    let p = payload.clone();
    g = g.on_drag(payload.clone(), move |_, _, _, cx| cx.new(|_| p.clone()));
    let drop_payload = payload.clone();
    g = g.drag_over::<TableDrag>(move |style, dragged, _window, _cx| {
        if dragged.table_start == drop_payload.table_start
            && dragged.is_row == drop_payload.is_row
            && dragged.index != drop_payload.index
        {
            style.bg(accent).border_color(drop_border)
        } else {
            style
        }
    });
    let drop_payload2 = payload;
    g = g.on_drop(move |dragged: &TableDrag, _window, cx| {
        let (from, to) = (dragged.index, drop_payload2.index);
        let is_row = drop_payload2.is_row;
        ent.update(cx, |v, cx| {
            use crate::editor::markdown_live::table_ops as t;
            v.run_table_op(cx, |m| {
                if is_row {
                    t::drag_move_row(m, from, to)
                } else {
                    t::drag_move_column(m, from, to)
                }
            });
        });
    });
    g
}

/// 表格右键菜单的动作（Typora 的表格菜单项）。
#[derive(Debug, Clone, Copy)]
enum TableMenuOp {
    RowAbove,
    RowBelow,
    ColBefore,
    ColAfter,
    RowUp,
    RowDown,
    ColLeft,
    ColRight,
    DeleteRow,
    DeleteColumn,
    CopyTable,
    AlignLeft,
    AlignCenter,
    AlignRight,
    AlignNone,
    Prettify,
    DeleteTable,
}

/// 表格菜单条目（右键菜单与工具栏 More▾ **共用一套**，保证两处都与
/// Typora 的 `#table-menu` 一致：行列增删/移动/复制/对齐/重排/删除表）。
fn table_menu_body(menu: PopupMenu, ent: Entity<LiveMarkdownView>) -> PopupMenu {
    let mk = move |label: &'static str, op: TableMenuOp| {
        let ent = ent.clone();
        PopupMenuItem::new(label).on_click(move |_, _, cx| {
            ent.update(cx, |v, cx| v.run_table_menu_op(op, cx));
        })
    };
    menu.item(mk("上方插入行", TableMenuOp::RowAbove))
        .item(mk("下方插入行", TableMenuOp::RowBelow))
        .item(mk("左侧插入列", TableMenuOp::ColBefore))
        .item(mk("右侧插入列", TableMenuOp::ColAfter))
        .separator()
        .item(mk("上移本行", TableMenuOp::RowUp))
        .item(mk("下移本行", TableMenuOp::RowDown))
        .item(mk("左移本列", TableMenuOp::ColLeft))
        .item(mk("右移本列", TableMenuOp::ColRight))
        .separator()
        .item(mk("删除本行", TableMenuOp::DeleteRow))
        .item(mk("删除本列", TableMenuOp::DeleteColumn))
        .item(mk("复制表格", TableMenuOp::CopyTable))
        .separator()
        .item(mk("左对齐", TableMenuOp::AlignLeft))
        .item(mk("居中", TableMenuOp::AlignCenter))
        .item(mk("右对齐", TableMenuOp::AlignRight))
        .separator()
        .item(mk("清除对齐", TableMenuOp::AlignNone))
        .item(mk("重排表格格式", TableMenuOp::Prettify))
        .separator()
        .item(mk("删除表格", TableMenuOp::DeleteTable))
}

/// 表格块内所有数据格都为空吗（分隔行不计）——spec #12 的「空表退格删表」判定。
fn table_block_all_empty(lines: &[Line], block: (usize, usize)) -> bool {
    use crate::editor::markdown_live::table_ops as t;
    for i in block.0..=block.1 {
        let Some(line) = lines.get(i) else {
            continue;
        };
        if t::is_delimiter_row(line) {
            continue; // 分隔行不是内容
        }
        if split_table_row(line).iter().any(|c| !c.trim().is_empty()) {
            return false;
        }
    }
    true
}

/// 整行是否**只由图片组成**（一张或多张，可被空白分隔）。
///
/// 这是 README 徽章那种写法的判据：`![a](x.svg) ![b](y.svg)` 一行两张。
/// 返回全部 `(alt, url)`；只要行里还有别的文字就返回 `None`——那种行交给
/// 文本渲染。
///
/// 已知限制：与文字混排的行内图片（`see ![a](a.png) here`）仍按链接的样子
/// 显示 alt 文本，不做内联图片替换。`LiveLine` 是单一 `StyledText`，插入图片
/// 会打乱光标↔字节偏移映射；要做需要把整行拆成「文本段 + 图片段」的布局，
/// 属于独立改动。
fn parse_image_line_multi(line: &str) -> Option<Vec<(String, String)>> {
    let (body, _) = image_body_span(line)?;
    let mut rest = body.trim();
    if rest.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    while let Some(tail) = rest.strip_prefix("![") {
        let close = tail.find("](")?;
        let alt = tail[..close].to_string();
        let after = &tail[close + 2..];
        let end = after.find(')')?;
        let spec = after[..end].trim();
        // `![](<a b.png>)`：尖括号里可以有空格；`![](a.png "title")`：忽略标题。
        let url = match spec.strip_prefix('<') {
            Some(angle) => angle.split('>').next().unwrap_or(angle).to_string(),
            None => spec.split_whitespace().next().unwrap_or("").to_string(),
        };
        if url.is_empty() {
            return None;
        }
        out.push((alt, url));
        rest = after[end + 1..].trim_start();
    }
    // 剩下的必须为空：还有文字就不是「纯图片行」。
    if rest.is_empty() && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// 行的内容区（剥掉块前缀）及其在源行里的字节偏移。
///
/// 前缀指的是列表符号（`- ` / `* ` / `+ ` / `1. `）、任务框（`- [x] `）、
/// 引用箭头（`> `，可嵌套）与 ATX 标题井号（`## `）。
///
/// 这些前缀在渲染态由**容器**负责画（竖线、圆点、标题字号），正文只负责
/// 内容。图片卡片要贴在正文该在的位置，所以偏移必须一起返回——卡片插进
/// 列表项时仍要缩进到项目符号右侧，而不是顶到行首。
fn image_body_span(line: &str) -> Option<(&str, usize)> {
    let mut at = 0usize;
    loop {
        let rest = &line[at..];
        // 引用箭头（连同它后面那一个空格）
        let after_ws = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        let rest = &rest[after_ws..];
        at += after_ws;
        if rest.starts_with('>') {
            at += 1;
            // `>` 后允许一个空格
            if line[at..].starts_with(' ') {
                at += 1;
            }
            continue;
        }
        // 列表符号：`-` / `*` / `+` 或 `1.` / `1)`
        let trimmed = rest;
        let mut consumed;
        match trimmed.as_bytes().first() {
            Some(b'-' | b'*' | b'+') => {
                consumed = 1;
                // 列表符号后必须是空白，否则 `-foo` 不是列表
                if !trimmed[1..].starts_with([' ', '\t']) {
                    return Some((&line[at..], at));
                }
                consumed += 1;
                // 任务框 `[x]` / `[ ]`
                let after_marker = &trimmed[consumed..];
                if after_marker.starts_with('[')
                    && let Some(close) = after_marker.find(']')
                    && close <= 2
                {
                    consumed += close + 1;
                    if trimmed[consumed..].starts_with(' ') {
                        consumed += 1;
                    }
                }
            }
            Some(b'0'..=b'9') => {
                let digits = trimmed
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(trimmed.len());
                if digits == 0 || digits > 9 {
                    return Some((&line[at..], at));
                }
                consumed = digits;
                match trimmed.as_bytes().get(consumed) {
                    Some(b'.' | b')') => {
                        consumed += 1;
                        if trimmed[consumed..].starts_with([' ', '\t']) {
                            consumed += 1;
                        } else {
                            return Some((&line[at..], at));
                        }
                    }
                    _ => return Some((&line[at..], at)),
                }
            }
            _ => {
                // ATX 标题：1~6 个 `#` 后接空白
                let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
                if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with([' ', '\t']) {
                    consumed = hashes;
                    while trimmed[consumed..].starts_with([' ', '\t']) {
                        consumed += 1;
                    }
                } else {
                    return Some((&line[at..], at));
                }
            }
        }
        at += consumed;
        // 剥完一层前缀再看下一层（`> - ![](a.png)` 这种嵌套）
    }
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

    fn reveal_match(
        &mut self,
        range: std::ops::Range<usize>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 防抖可能让面板的匹配落后于当前文本：跳转前先对齐。
        self.sync_search_now(cx);
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
        // 替换前对齐匹配，避免按过期的偏移改写文本。
        self.sync_search_now(cx);
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
        // 全量替换前对齐匹配（同 `replace_match`）。
        self.sync_search_now(cx);
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

/// 平台文本输入（中文 IME 组字 / `insertText`）的唯一写路径。ASCII 可打印
/// 键也走这里：`on_key` 只处理具名键，未消费的键由平台回退到 input
/// handler（`dispatch_keystroke` → `dispatch_input`）。范围全部是 UTF-16，
/// 落库前经 [`LiveMarkdownView::utf16_to_byte`] 折成字节。
impl EntityInputHandler for LiveMarkdownView {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let doc = self.buffer.text();
        let s = Self::utf16_to_byte(&doc, range.start);
        let e = Self::utf16_to_byte(&doc, range.end);
        let (s, e) = (s.min(e), s.max(e));
        // 回报**实际**取到的区间（floor 后的），平台据此记账。
        *adjusted_range = Some(Self::byte_to_utf16(&doc, s)..Self::byte_to_utf16(&doc, e));
        Some(doc[s..e].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let doc = self.buffer.text();
        let (range, reversed) = match self.buffer.selection() {
            Some((s, e)) => {
                let sb = Self::offset_of_pos(self.buffer.lines(), s);
                let eb = Self::offset_of_pos(self.buffer.lines(), e);
                // 选区终点（光标）在区间起点 = 反向选择。
                let reversed = self.buffer.model().anchor() == Some(e);
                (
                    Self::byte_to_utf16(&doc, sb)..Self::byte_to_utf16(&doc, eb),
                    reversed,
                )
            }
            None => {
                let off = self.search_caret_offset();
                let u = Self::byte_to_utf16(&doc, off);
                (u..u, false)
            }
        };
        Some(UTF16Selection { range, reversed })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        let r = self.ime_marked_range.clone()?;
        let doc = self.buffer.text();
        Some(Self::byte_to_utf16(&doc, r.start)..Self::byte_to_utf16(&doc, r.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.ime_marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 只读/搜索面板聚焦/失焦：静默丢弃（**不**置 stop，避免平台仍在
        // 派发的 `on_key` 被吞）。
        if self.read_only || self.search_panel_focused(window, cx) || !self.focus.is_focused(window)
        {
            return;
        }
        // 控制字符残留整体丢弃（与 type_text 同规），且不动选区。
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return;
        }
        // 提交语义在进入时判定：有组字区间 = 组字内的替换（不做自动配对），
        // 无 = 真正的提交/普通输入（单字符走自动配对）。
        let autopair = self.ime_marked_range.is_none();
        let (start, end) = self.resolve_input_range(range);
        self.ime_marked_range = None;
        if text.is_empty() {
            // 空提交 = 取消组字：删掉区间、光标塌缩在起点（折叠区间为 no-op）。
            self.set_range_selection(start, end, cx);
            self.edit(cx, |m| {
                m.delete_selection();
            });
            self.after_edit(cx);
            return;
        }
        self.set_range_selection(start, end, cx);
        self.type_text(text, autopair, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only || self.search_panel_focused(window, cx) || !self.focus.is_focused(window)
        {
            return;
        }
        if new_text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return;
        }
        let (start, end) = self.resolve_input_range(range);
        if new_text.is_empty() {
            self.ime_marked_range = None;
            self.set_range_selection(start, end, cx);
            self.edit(cx, |m| {
                m.delete_selection();
            });
            self.after_edit(cx);
            return;
        }
        // 预置组字区间（组字期间文本恒等于该区间）：必须在插入**前**置位，
        // 让本次 after_edit 跳过表格自动重排（重排会挪动区间前的字节）。
        // 插入后按光标重算，覆盖跨格合并（写入点 ≠ 请求区间）的情况。
        self.ime_marked_range = Some(start..start + new_text.len());
        self.set_range_selection(start, end, cx);
        self.type_text(new_text, false, cx);
        let caret = self.search_caret_offset();
        let marked_start = caret.saturating_sub(new_text.len());
        self.ime_marked_range = Some(marked_start..caret);
        // 新选区（UTF-16，相对**刚插入的文本**）→ 移动光标；组字态保持。
        if let Some(r) = new_selected_range {
            let cs = marked_start + Self::utf16_to_byte(new_text, r.start);
            let ce = marked_start + Self::utf16_to_byte(new_text, r.end);
            self.set_range_selection(cs, ce, cx);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let doc = self.buffer.text();
        let s = Self::utf16_to_byte(&doc, range_utf16.start);
        let e = Self::utf16_to_byte(&doc, range_utf16.end);
        let (s, e) = (s.min(e), s.max(e));
        let ps = pos_of_offset(self.buffer.lines(), s);
        let pe = pos_of_offset(self.buffer.lines(), e);
        if ps.line != pe.line {
            // 跨行区间没有单一锚点：退回元素盒。
            return Some(element_bounds);
        }
        let text = self.buffer.line(ps.line)?;
        let in_line = byte_for_col(text, ps.col);
        Some(self.ime_point_bounds(ps.line, in_line, element_bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line = self.line_for_click(point)?;
        // 表格行：优先指针所在的格（命中框含整格），否则通用映射兜底。
        let cell = self
            .cell_hits
            .iter()
            .find(|((l, _), hit)| *l == line && hit.text_bounds.contains(&point))
            .map(|(key, _)| *key);
        let col = match cell {
            Some((l, c)) => self
                .cell_col_at(l, c, point)
                .unwrap_or_else(|| self.col_for_click(line, point)),
            None => self.col_for_click(line, point),
        };
        // `col_for_click` 兜底给 usize::MAX（无布局）：`byte_for_col` 会
        // clamp 到行尾。
        let pos = Pos::new(line, col);
        let byte = Self::offset_of_pos(self.buffer.lines(), pos);
        let doc = self.buffer.text();
        Some(Self::byte_to_utf16(&doc, byte))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let doc = self.buffer.text();
        let s = Self::utf16_to_byte(&doc, range_utf16.start);
        let e = Self::utf16_to_byte(&doc, range_utf16.end);
        self.set_range_selection(s.min(e), s.max(e), cx);
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.text().encode_utf16().count())
    }

    fn accepts_text_input(&self, window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.read_only && self.focus.is_focused(window)
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
/// 单元格的帧内命中记录（spec #6）：cell LiveLine 在 `paint` 时写入，
/// `cell_div` 的元素级按下/移动监听用它做坐标 → 格内行列的映射（与
/// LiveLine 窗口监听同一套：文本三段夹取 + segs 可见列 → 源列换算）。
pub(crate) struct CellHit {
    /// 本格文本的排版（窗口坐标系，`position_for_index` 已折入原点）。
    pub layout: gpui::TextLayout,
    /// 本格可见文本字节数。
    pub text_len: usize,
    /// 本格文本的布局盒（≠ 格盒：格盒由 cell_div 命中框覆盖，这里只
    /// 负责「右空白 → 文本末 / 左空白 → 文本首 / 中间 → 字形映射」）。
    pub text_bounds: gpui::Bounds<gpui::Pixels>,
    /// 可见字节 → 行内源字节的映射段（本格内容范围，绝对行内偏移）。
    pub segs: Vec<crate::editor::markdown_live::layout::Seg>,
    /// 本格内容的行内字节范围（去 padding）。
    pub content: std::ops::Range<usize>,
}

pub(crate) struct RowLayoutInfo {
    pub layout: gpui::TextLayout,
    pub text_len: usize,
    /// The row's text rect in **document space** (window space minus the
    /// scroll offset in effect at paint time).
    ///
    /// This is what makes the caret scroll row-precise: it tells the view
    /// whether the caret's *own line* is visible, independent of how tall its
    /// container is (a table grid or code card is a single element holding
    /// many rows).
    pub doc_top: f32,
    pub doc_bottom: f32,
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
/// How many post-layout frames the caret scroll may spend correcting itself.
///
/// The element-level scroll aligns a whole container; the correction then moves
/// the caret's *row* into view. Each pass can shift the layout (revealing a
/// table grid also reveals its toolbar above it, changing the grid's height),
/// so a few passes are allowed to settle. Bounded to keep a pathological
/// layout from looping.
const CARET_SCROLL_CORRECTION_PASSES: u8 = 3;

/// 未实测 group 的高度估算：单行高 + 各容器 chrome。
const EST_LINE_H: f32 = 26.0;
/// Pitch 回填时代的默认行高：含组间 margin（标题 pitch 62.5、空白 38~58、
/// 段落 40 左右），只在尚无任一实测样本的首帧使用。
const EST_PITCH_H: f32 = 40.0;

/// A render group: consecutive rows sharing one chrome container.
pub(crate) enum Group {
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

/// Document-shape geometry derived from the plan: everything the render loop
/// used to rebuild from scratch every frame (grouping, calibration buckets,
/// line ranges, per-row group membership, ordered-list numbers, mermaid
/// blocks).
///
/// This is a pure function of (`planned` blocks, line texts) — i.e. of the
/// text version only, *not* of the cursor/selection/reveal. Caching it here
/// means a scroll-only frame (or a blink tick) stops paying O(document) for
/// work whose answer cannot have changed; only the virtual window itself is
/// per-frame. The reveal-dependent parts (`plan.revealed`, visible text) stay
/// in `planned`.
pub(crate) struct PlanGeometry {
    /// Render groups in document order.
    groups: Vec<Group>,
    /// Group index of every source row (`group_of_row[row]`).
    group_of_row: Vec<u32>,
    /// One entry per group: `(row_count, cal_bucket, (first_line, last_line))`.
    pub(crate) ginfo: Vec<(usize, u8, (usize, usize))>,
    /// Ordered-list display numbers per row (`None` outside ordered lists).
    display_numbers: Vec<Option<u64>>,
    /// Whether any group is a table. Precomputed so the render path doesn't
    /// scan every group to answer this.
    pub(crate) has_table: bool,
    /// Mermaid blocks in document order.
    pub(crate) mermaid_blocks: Vec<MermaidBlockRef>,
    /// 可作为「全宽」目标的块首行（表格组首行 + mermaid 围栏行），升序去重。
    ///
    /// 「全宽」状态按块首行号记账，而行号会随编辑漂移：收帧时用它裁剪，
    /// 保证一个块被删/改类型后，旧行号不会把别的块意外撑成全宽。
    pub(crate) wide_anchors: Vec<usize>,
}

/// A mermaid fence as located by [`PlanGeometry`].
#[derive(Debug, Clone)]
pub(crate) struct MermaidBlockRef {
    /// Source line of the opening fence (the mermaid cache key id).
    pub(crate) block_id: usize,
    /// Row indices of the code lines (used to detect edits inside the block).
    pub(crate) code_rows: Vec<usize>,
    /// The block's joined source text. Cached here so a frame never has to
    /// re-join the code lines just to look the block up in the render cache
    /// (the source only changes when the text does, i.e. when this geometry is
    /// rebuilt). `Arc<str>` shares with the cache key.
    pub(crate) source: Arc<str>,
}

impl PlanGeometry {
    /// Derive geometry from the planned rows. `lines` supplies the code text
    /// for the cached mermaid sources.
    /// [`Self::build`] over `Rc`-wrapped rows (the view's storage).
    fn build_owned(rows: &[Rc<PlannedRow>], lines: &[Line]) -> Self {
        let borrowed: Vec<&PlannedRow> = rows.iter().map(|r| r.as_ref()).collect();
        Self::build_refs(&borrowed, lines)
    }

    fn build_refs(rows: &[&PlannedRow], lines: &[Line]) -> Self {
        // ---- grouping: consecutive rows sharing one chrome container ----
        let mut groups: Vec<Group> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            match &rows[i].block {
                Block::Fence { open: true, lang } => {
                    let lang = lang.as_deref().map(str::to_string);
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

        // ---- per-group info + row -> group membership ----
        let mut ginfo: Vec<(usize, u8, (usize, usize))> = Vec::with_capacity(groups.len());
        let mut group_of_row: Vec<u32> = vec![0; rows.len()];
        let mut prev_base: u8 = 9;
        for (gi, gr) in groups.iter().enumerate() {
            let base = gr.cal_bucket(rows);
            let b = if base == 9 {
                10 + prev_base.min(9)
            } else {
                base
            };
            ginfo.push((gr.row_count(), b, gr.line_range(rows)));
            prev_base = base;
            let gi = gi as u32;
            match gr {
                Group::One(pi) => {
                    if let Some(slot) = group_of_row.get_mut(*pi) {
                        *slot = gi;
                    }
                }
                Group::Fence {
                    open, code, close, ..
                } => {
                    if let Some(slot) = group_of_row.get_mut(*open) {
                        *slot = gi;
                    }
                    for pi in code {
                        if let Some(slot) = group_of_row.get_mut(*pi) {
                            *slot = gi;
                        }
                    }
                    if let Some(c) = close
                        && let Some(slot) = group_of_row.get_mut(*c)
                    {
                        *slot = gi;
                    }
                }
                Group::Para(v) | Group::Quote(v) | Group::Table(v) => {
                    for pi in v {
                        if let Some(slot) = group_of_row.get_mut(*pi) {
                            *slot = gi;
                        }
                    }
                }
            }
        }

        // ---- ordered-list display numbers (document order) ----
        let mut ordered = OrderedCounter::new();
        let mut display_numbers: Vec<Option<u64>> = Vec::with_capacity(rows.len());
        for row in rows {
            display_numbers.push(ordered.visit(&row.plan.kind, list_cols(&row.plan.kind)));
        }

        // ---- mermaid blocks ----
        let mut mermaid_blocks: Vec<MermaidBlockRef> = Vec::new();
        for gr in &groups {
            let Group::Fence { open, code, .. } = gr else {
                continue;
            };
            let lang = match &rows[*open].block {
                Block::Fence { open: true, lang } => lang.as_deref(),
                _ => None,
            };
            if code.is_empty() || !lang.is_some_and(|l| l.eq_ignore_ascii_case("mermaid")) {
                continue;
            }
            mermaid_blocks.push(MermaidBlockRef {
                block_id: rows[*open].line,
                code_rows: code.clone(),
                source: Arc::from(
                    code.iter()
                        .map(|c| lines.get(rows[*c].line).map(|l| l.as_str()).unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .as_str(),
                ),
            });
        }

        let has_table = groups.iter().any(|g| matches!(g, Group::Table(_)));
        // 「全宽」锚点：表格组首行 + mermaid 围栏行（`mermaid_blocks` 的
        // block_id 就是围栏首行）。两者都在文档序里，直接归并去重。
        let mut wide_anchors: Vec<usize> = groups
            .iter()
            .filter_map(|g| match g {
                Group::Table(v) => v.first().map(|pi| rows[*pi].line),
                _ => None,
            })
            .chain(mermaid_blocks.iter().map(|b| b.block_id))
            .collect();
        wide_anchors.sort_unstable();
        wide_anchors.dedup();
        Self {
            groups,
            group_of_row,
            ginfo,
            display_numbers,
            has_table,
            mermaid_blocks,
            wide_anchors,
        }
    }

    /// The group at index `gi` (for window iteration).
    #[inline]
    pub(crate) fn group_at(&self, gi: usize) -> Option<&Group> {
        self.groups.get(gi)
    }

    /// Whether group `gi` is a fence (code block).
    /// The cached joined source of the mermaid block whose opening fence is at
    /// source line `block_id`, if that fence is a non-empty mermaid block.
    ///
    /// Blocks are in document order, so this is a binary search. The render
    /// branch uses it to avoid re-joining the code lines every frame just to
    /// look the block up in the mermaid render cache.
    pub(crate) fn mermaid_source(&self, block_id: usize) -> Option<&Arc<str>> {
        let i = self
            .mermaid_blocks
            .binary_search_by_key(&block_id, |b| b.block_id)
            .ok()?;
        self.mermaid_blocks.get(i).map(|b| &b.source)
    }
    #[inline]
    pub(crate) fn group_fence(&self, gi: usize) -> Option<(&[usize], &str)> {
        match self.groups.get(gi) {
            Some(Group::Fence { code, lang, .. }) if !code.is_empty() => {
                Some((code.as_slice(), lang.as_deref().unwrap_or("")))
            }
            _ => None,
        }
    }

    /// Source line of the opening fence of group `gi` (when it is a fence).
    #[inline]
    pub(crate) fn group_fence_open_line(&self, gi: usize) -> Option<usize> {
        match self.groups.get(gi) {
            // Rows map 1:1 onto source lines, so the row index is the line.
            Some(Group::Fence { open, .. }) => Some(*open),
            _ => None,
        }
    }

    /// Whether group `gi` is a table group.
    #[inline]
    pub(crate) fn group_is_table(&self, gi: usize) -> bool {
        matches!(self.groups.get(gi), Some(Group::Table(_)))
    }

    /// `(first_row, last_row)` covered by group `gi`.
    #[inline]
    pub(crate) fn group_row_bounds(&self, gi: usize) -> Option<(usize, usize)> {
        let g = self.groups.get(gi)?;
        Some(match g {
            Group::One(pi) => (*pi, *pi),
            Group::Fence {
                open, code, close, ..
            } => (
                *open,
                close
                    .or_else(|| code.last().copied())
                    .unwrap_or(*open)
                    .max(*open),
            ),
            Group::Para(v) | Group::Quote(v) | Group::Table(v) => {
                (*v.first().unwrap_or(&0), *v.last().unwrap_or(&0))
            }
        })
    }

    /// Row range covered by groups `lo..=hi` (rows are contiguous in a group,
    /// and groups are contiguous in the document).
    pub(crate) fn window_rows(&self, lo: usize, hi: usize) -> Option<(usize, usize)> {
        let (a, _) = self.group_row_bounds(lo)?;
        let (_, b) = self.group_row_bounds(hi)?;
        Some((a, b.max(a)))
    }

    /// Total group count.
    #[inline]
    pub(crate) fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Whether source row `pi` belongs to a group inside the window.
    #[inline]
    fn row_in_window(&self, pi: usize, lo: usize, hi: usize) -> bool {
        match self.group_of_row.get(pi) {
            Some(&gi) => {
                let gi = gi as usize;
                gi >= lo && gi <= hi
            }
            None => false,
        }
    }
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
    fn cal_bucket(&self, rows: &[&PlannedRow]) -> u8 {
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
    fn line_range(&self, rows: &[&PlannedRow]) -> (usize, usize) {
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
        geometry_revision: u64,
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
            // Only a rebuilt `PlanGeometry` can change the group list / row
            // counts / calibration buckets that feed the prefix sum. A plain
            // text edit usually keeps it identical; the measured-height
            // feedback below still marks the prefix dirty if a visible group
            // actually reflowed. Unconditionally invalidating here made every
            // keystroke O(groups) (≈6.7k at 20k lines) even for a prose edit.
            if geometry_revision != self.virt_geometry_revision {
                self.prefix_dirty = true;
            }
        }
        self.virt_geometry_revision = geometry_revision;
        // 上帧几何快照（锚定补偿用）：prefix 逐帧重算会漂，`lo` 越大顶部占位
        // 跳变越大；用同一 anchor 的新旧差修正 scroll offset，视口内容不动。
        // 缓冲复用：`mem::take` 接管上帧的 prefix（容量留用），避免每帧克隆
        // 整份 `Vec<f32>`。
        let mut old_prefix = std::mem::take(&mut self.virt_prefix);
        let old_window = self.last_window;
        let old_same_shape = old_prefix.len() == g + 1;
        // Wrapping follows viewport width: re-measure after resize.
        let vp = self.scroll.bounds();
        let vp_w = f32::from(vp.size.width);
        if vp_w > 0.0 && (self.last_vp_width - vp_w).abs() > 1.0 {
            self.group_heights.clear();
            self.prefix_dirty = true;
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
            let counts = std::mem::take(&mut self.last_window_counts);
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
                            // Only a *changed* height invalidates the prefix
                            // sum. Re-measuring the same value (the steady
                            // state for every settled group) must not force a
                            // full O(groups) rebuild every frame.
                            match self.group_heights.get(&(plo + k)) {
                                Some(&old) if (old - h).abs() < 0.01 => {}
                                _ => {
                                    self.group_heights.insert(plo + k, h);
                                    self.prefix_dirty = true;
                                }
                            }
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
        // The rate samples only feed the prefix sum below, so they are skipped
        // together with it on an unchanged frame.
        let need_rates = self.prefix_dirty || old_prefix.len() != g + 1;
        let mut kind_vals: Vec<Vec<f32>> = vec![Vec::new(); 20];
        let mut all_vals: Vec<f32> = Vec::new();
        for (i, (r, b, _)) in ginfo.iter().enumerate() {
            if !need_rates {
                break;
            }
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
            median(&mut all_vals).clamp(8.0, 96.0)
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
            matches!(ginfo.get(i).map(|(_, b, _)| *b), Some(10..=19))
        };
        // Reuse the previous prefix sum when nothing that feeds it changed
        // (an idle/blink frame, or a scroll that did not move the window).
        // The scan covers every group, so skipping it is what keeps the frame
        // cost independent of document size. `old_prefix` was `mem::take`n
        // above, so it is ours to reuse or drop.
        // `prefix` is the freshly computed sum; `old_prefix` is last frame's
        // (used by the anchor compensation to detect drift).
        let mut prefix = Vec::with_capacity(g + 1);
        if need_rates {
            prefix.clear();
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
            self.prefix_dirty = false;
        } else {
            // Unchanged inputs: last frame's sum is still exact. Take the
            // buffer instead of copying all groups (the anchor compensation
            // below only runs when the prefix was rebuilt).
            prefix = std::mem::take(&mut old_prefix);
        }
        // Viewport window in content pixels (scroll-anchored).
        let vp_h = f32::from(vp.size.height);
        let mut off_y = -f32::from(self.scroll.offset().y);
        // 锚定补偿：锚点取旧空间下视口顶部 group（不是窗口顶部，也不是
        // 内容末端——两者都会把锚点下方新实测 group 的修正漏成视口位移），
        // 组内按比例跟随；视口下方（含底部）的修正由底部占位吸收，视口内容
        // 保持不动，用户滚轮位移被完整保留。
        if need_rates && !text_changed && !want_caret && vp_h > 0.0 && old_same_shape {
            // First group whose *end* is past the viewport top: binary search
            // instead of a linear scan (O(log g) per frame).
            let k = old_prefix.partition_point(|&p| p <= off_y);
            let a = k.saturating_sub(1).min(g - 1);
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
            // lo0: first group ending past the viewport top.
            let lo0 = prefix
                .partition_point(|&p| p <= off_y)
                .saturating_sub(1)
                .min(g - 1);
            // hi0: last group starting before the viewport bottom.
            let hi0 = prefix
                .partition_point(|&p| p < off_y + vp_h)
                .saturating_sub(1)
                .min(g - 1);
            (
                lo0.saturating_sub(OVERSCAN_GROUPS),
                (hi0 + OVERSCAN_GROUPS).min(g - 1),
            )
        };
        // 窗口迟滞：小幅滚动复用上帧窗口（overscan 覆盖视口），避免 lo/hi
        // 逐 group 步进带动顶部占位跳变；该跳变是底部回滚上下抖动的直接观感。
        if !text_changed
            && !want_caret
            && old_same_shape
            && let Some((olo, ohi)) = old_window
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
        // Caret edits/moves must land inside the window: the caret row has to
        // be *built* so the scroll logic can measure and reveal it (and so the
        // caret paints). `ginfo` is ordered by line range, so the containing
        // group is a binary search (the old linear scan ran every caret frame).
        if want_caret {
            let gc = match ginfo.binary_search_by(|(_, _, (a, b))| {
                if cursor_line < *a {
                    std::cmp::Ordering::Greater
                } else if cursor_line > *b {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            }) {
                Ok(i) => i,
                Err(_) => g - 1,
            };
            if gc < lo || gc > hi {
                lo = gc.saturating_sub(OVERSCAN_GROUPS);
                hi = (gc + OVERSCAN_GROUPS + 64).min(g - 1);
            }
        }
        self.virt_prefix = prefix;
        // `virt_group_lines` mirrors the cached geometry's line ranges; only
        // copy it when the geometry changed (otherwise the existing copy is
        // already exact, and this loop is O(groups) per frame).
        if self.prefix_dirty || self.virt_group_lines.len() != g {
            self.virt_group_lines.clear();
            self.virt_group_lines
                .extend(ginfo.iter().map(|(_, _, lr)| *lr));
        }
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

    /// Whether the caret's own row is already inside the viewport.
    ///
    /// Uses [`Self::caret_row_rect`] — the rect recorded by the **previous**
    /// frame's paint. `row_layouts` cannot be consulted here: it is cleared at
    /// the top of `render` and only repopulated during paint, i.e. after this
    /// decision is made, so looking it up always returned `None` and the
    /// caller's "already visible → do not scroll" fast path never engaged.
    ///
    /// The rect is accepted whenever it describes the **same caret line**,
    /// including after an edit changed the text version. Requiring a matching
    /// version was a mistake: it invalidated the rect on every single keystroke,
    /// so typing in a tall container re-armed the element-level scroll and
    /// jumped to the container's top edge on each character — the same
    /// jump-to-header bug as clicking, just triggered by the keyboard. The
    /// rect being one frame old is fine, because staleness is exactly what
    /// [`Self::correct_caret_scroll`] is for: it re-measures the caret row from
    /// the freshly painted layout and nudges by the minimal amount, so an edit
    /// that genuinely reflowed the row still ends up visible. Suppressing the
    /// element-level scroll only ever removes the *gross* jump; the precise
    /// correction still runs.
    ///
    /// `None` when no rect has been recorded yet, or when it describes a
    /// different caret line — a cursor move (arrow keys, Enter, a long jump)
    /// genuinely needs the element-level scroll to reach the new row.
    fn caret_row_in_view(&self) -> Option<bool> {
        let (line, _version, doc_top, doc_bottom) = self.caret_row_rect?;
        if line != self.buffer.cursor().line {
            return None;
        }
        let vp_h = f32::from(self.scroll.bounds().size.height);
        if vp_h <= 0.0 {
            return None;
        }
        let vp_top = -f32::from(self.scroll.offset().y);
        let vp_bottom = vp_top + vp_h;
        Some(doc_top >= vp_top - 1.0 && doc_bottom <= vp_bottom + 1.0)
    }

    /// Re-measure the caret row after the frame's layout and correct the scroll
    /// if it ended up outside the viewport.
    ///
    /// This is the row-precise half of the caret scroll (see the call site in
    /// `render`): the element-level scroll can only align a whole container, so
    /// a caret on a late row of a tall table/code card would be left off-screen.
    /// Here the container's measured child bounds are available, so the caret
    /// row's own rect can be derived and the view nudged by the smallest amount.
    ///
    /// Bounded to [`CARET_SCROLL_CORRECTION_PASSES`] frames, one pass each:
    /// moving the scroll changes the row's rect, so a second pass in the same
    /// frame would compute from stale geometry. Re-arming per frame lets the
    /// layout settle (a table toolbar appearing above the grid changes its
    /// height) while keeping each decision exact.
    fn schedule_caret_scroll_correction(&mut self, cx: &mut Context<Self>) {
        // Budget is per scroll request, re-armed by `scroll_to_cursor`; without
        // this the re-arm would loop for as long as the row stayed off-screen.
        if self.caret_scroll_passes == 0 {
            return;
        }
        self.caret_scroll_passes -= 1;
        let weak = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |view, cx| {
                if !view.scroll_to_cursor {
                    return;
                }
                if view.correct_caret_scroll() {
                    view.schedule_caret_scroll_correction(cx);
                }
            });
        });
    }

    /// One correction pass over the freshly measured caret row.
    ///
    /// Returns `true` when another pass (next frame) could help.
    fn correct_caret_scroll(&mut self) -> bool {
        let line = self.buffer.cursor().line;
        let vp = self.scroll.bounds();
        let vp_h = f32::from(vp.size.height);
        if vp_h <= 0.0 {
            return false;
        }
        // The caret row's rect, in the same space the scroll handle uses:
        // derive it from the row's layout (recorded during paint, which ran
        // with the *current* offset) rather than from stale element bounds.
        let Some(info) = self.row_layouts.get(&line) else {
            return false;
        };
        let (row_top, row_bottom) = (info.doc_top, info.doc_bottom);
        let vp_top = -f32::from(self.scroll.offset().y);
        let vp_bottom = vp_top + vp_h;
        let row_h = (row_bottom - row_top).max(1.0);
        let target_top = if row_h >= vp_h || row_top < vp_top {
            row_top
        } else if row_bottom > vp_bottom {
            row_bottom - vp_h
        } else {
            self.scroll_to_cursor = false;
            return false; // Already fully visible: never scroll.
        };
        let want = -target_top;
        let cur = self.scroll.offset();
        if (f32::from(cur.y) - want).abs() <= 0.5 {
            self.scroll_to_cursor = false;
            return false;
        }
        self.scroll.set_offset(gpui::point(cur.x, px(want)));
        true
    }

    /// 滚动窗口化：只保留当前窗口 `lo..=hi` ± 余量内的实测高度，其余淘汰。
    /// 淘汰的 group 读取时回退到估算（`prefix` 与速率采样都用 `unwrap_or_else`
    /// 估算兜底），下一帧进入窗口时实测自修正；因此保留内存从「随滚动遍历
    /// 整个大文档持续增长」变为与文档规模无关的常数窗口（M3）。
    fn prune_group_heights(&mut self, lo: usize, hi: usize) {
        let before = self.group_heights.len();
        self.group_heights.retain(|&gi, _| {
            hi < lo
                || (gi >= lo.saturating_sub(GROUP_HEIGHT_KEEP_MARGIN)
                    && gi <= hi + GROUP_HEIGHT_KEEP_MARGIN)
        });
        if self.group_heights.len() != before {
            self.prefix_dirty = true;
        }
    }
}

/// 代码 / mermaid 卡片左上角胶囊的基础样式（mermaid「预览 | 源码」等 tab 与
/// 语言徽章共用同一套数值：10px 字号、600 字重、14px 行高、圆角 5、上下 2
/// 左右 9 内边距）。
///
/// 调用方补颜色与交互：tab 挂 `cursor_pointer`/`on_click`，语言徽章只复用
/// 样式、不可点击。
fn card_pill(bg: Hsla, fg: Hsla) -> gpui::Div {
    div()
        .px(px(9.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .bg(bg)
        .text_size(px(10.0))
        .line_height(px(14.0))
        .font_weight(gpui::FontWeight(600.))
        .text_color(fg)
}

impl Render for LiveMarkdownView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme_shared();
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
        self.ensure_activation_observer(window, cx);
        let want_blink = focused && window_active && !self.read_only;
        if want_blink && !self.blink_on {
            self.blink.update(cx, |b, cx| b.enable(cx));
            self.blink_on = true;
        } else if !want_blink && self.blink_on {
            self.blink.update(cx, |b, cx| b.disable(cx));
            self.blink_on = false;
        }
        let show_caret = self.should_paint_caret(window, cx);
        let caret_opacity = self.caret_opacity(window, cx);
        // 闪烁淡入淡出窗口内需要连续帧（与光标动画同一套「按需续帧」约定：
        // 相位中间 `needs_repaint()` 为 false，静止即停、不空转）。
        if self.blink.read(cx).needs_repaint() {
            let view_id = cx.entity_id();
            window.on_next_frame(move |_, cx| cx.notify(view_id));
        }
        // 光标动画的**名义**度量：一律用基础正文行高，与行类型无关。
        // 标题/代码/表格行的真实行高各不相同，若把本行实际行高当动画度量，
        // 跨块移动会被误判成「布局变化」而瞬移（动画退化成跳变）。
        let caret_metric_height = {
            let font_size = style.base_text_style.font_size.to_pixels(window.rem_size());
            window.pixel_snap(
                style
                    .base_text_style
                    .line_height
                    .to_pixels(font_size.into(), window.rem_size()),
            )
        };
        // 文档级宽度：本帧全宽（所有行脱离 780px 窄栏）。逐块「全宽」按钮在
        // 此状态下隐藏（见 mermaid 卡片 / 表格工具栏）。
        let wide_doc = self.wide_doc;

        // Fresh per-frame hit geometry (LiveLine::paint repopulates).
        self.row_layouts.clear();
        self.cell_hits.clear();
        self.row_targets.clear();
        // 本帧的输入法处理器注册权（LiveLine::paint 认领；光标行每帧注册）。
        self.input_registered = false;

        // Plan rows (cached on version+cursor+selection+focus). `Rc` clone:
        // no per-frame deep copy of the document's planned rows.
        let rows: Rc<Vec<Rc<PlannedRow>>> = self.planned_rows(focused);
        let last_line = rows.len().saturating_sub(1);
        let entity = cx.entity();

        // ---- geometry (cached by text version; NOT rebuilt per frame) -----
        // Grouping, calibration buckets, ordered-list numbers and the mermaid
        // block list are pure functions of the text, so a scroll-only frame or
        // blink tick must not recompute them. `group_of_row` also makes the
        // visibility test O(1) instead of an O(rows) bitmap rebuild.
        let geometry: Rc<PlanGeometry> = self.planned_geometry.clone();
        let ginfo = &geometry.ginfo;

        // Row virtualization window, computed BEFORE per-row paint so
        // `code_runs` / `find_marks` / `paints` / `display_numbers` only pay
        // for the viewport (+ overscan). Small docs render everything.
        let text_version = self.buffer.text_version();
        let want_caret = self.scroll_to_cursor;
        let vw = self.virtual_window(
            ginfo,
            cursor.line,
            text_version,
            self.geometry_revision,
            want_caret,
        );
        let virtualized = vw.is_some();
        let (v_lo, v_hi, v_top_px, v_bot_px) =
            vw.unwrap_or((0, ginfo.len().saturating_sub(1), 0.0, 0.0));
        // Visibility is a group-membership test (O(1) per row) — no per-frame
        // `vec![false; rows.len()]` and no `position()` scan for the caret row.
        let is_visible = |pi: usize| -> bool {
            if !virtualized {
                return true;
            }
            geometry.row_in_window(pi, v_lo, v_hi)
        };
        // The caret's row must paint even when it sits just outside the window
        // (it is the scroll target and carries the caret).
        // `planned` has exactly one row per source line, in document order
        // (`plan_range` pushes `PlannedRow { line: i, .. }` for every `i`), so
        // the cursor's row index *is* its source line. Scanning all rows with
        // `position()` here made every caret frame O(document).
        let caret_row: Option<usize> = if virtualized && cursor.line < rows.len() {
            Some(cursor.line)
        } else {
            None
        };
        let row_visible = |pi: usize| -> bool { is_visible(pi) || caret_row == Some(pi) };

        // ---- per-row paint (runs + selection + caret) --------------------
        // (`row.plan` already carries callout tag-stripping from
        // `planned_rows`, shared with hit-testing.)
        //
        // Code rows highlight as whole blocks (one tree-sitter parse per
        // fence, split per line) so multi-line constructs keep their spans;
        // pre-compute the per-line runs here, keyed by source line.
        //
        // Only window groups are walked: off-screen fences skip tree-sitter
        // entirely, and the walk itself no longer touches every row.
        let mut code_runs: std::collections::HashMap<usize, Vec<TextRun>> =
            std::collections::HashMap::new();
        for gi in v_lo..=v_hi {
            let Some((code, lang)) = geometry.group_fence(gi) else {
                continue;
            };
            let base = row_base_style(&rows[code[0]].plan, &style);
            let src_lines: Vec<&str> = code
                .iter()
                .map(|pi| self.buffer.line(rows[*pi].line).unwrap_or(""))
                .collect();
            let per_line = self.hl.runs_for_block(lang, &src_lines, &base, &theme);
            for (pi, runs) in code.iter().copied().zip(per_line) {
                if is_visible(pi) {
                    code_runs.insert(rows[pi].line, runs);
                }
            }
        }
        // Find highlights: byte ranges → source lines → visible cols
        // (multi-line matches split per row, hidden-marker spans dropped).
        let find_marks: HashMap<usize, Vec<(usize, usize, bool)>> = {
            let mut marks: HashMap<usize, Vec<(usize, usize, bool)>> = HashMap::new();
            let search = self.search_panel.as_ref().map(|p| p.read(cx));
            if let Some(matcher) = search.as_ref().and_then(|p| p.matcher()) {
                // Borrow the matcher's ranges instead of cloning the whole
                // match list on every frame while the find panel is open.
                let match_ranges = matcher.match_ranges();
                let current = matcher.current_ix();
                if !match_ranges.is_empty() {
                    let row_by_line: HashMap<usize, usize> = rows
                        .iter()
                        .enumerate()
                        .filter(|(pi, _)| row_visible(*pi))
                        .map(|(pi, r)| (r.line, pi))
                        .collect();
                    let lines = self.buffer.lines();
                    for (ix, range) in match_ranges.iter().enumerate() {
                        // O(log lines) offset → Pos instead of a linear scan per
                        // match (was O(matches × lines) per frame with the find
                        // panel open — the worst hidden hotspot on large docs).
                        let start = self.buffer.pos_of_offset(range.start);
                        let end = self.buffer.pos_of_offset(range.end);
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
            }
            marks
        };
        // Only viewport rows pay for runs/selection/caret shaping;
        // off-screen rows are skipped entirely (no per-row clones). The loop
        // walks the window's row range instead of every row in the document.
        let mut paints: HashMap<usize, RowPaint> = HashMap::new();
        let paint_rows: (usize, usize) = geometry
            .window_rows(v_lo, v_hi)
            .unwrap_or((0, rows.len().saturating_sub(1)));
        for pi in paint_rows.0..=paint_rows.1.min(rows.len().saturating_sub(1)) {
            let row = &rows[pi];
            if virtualized && !row_visible(pi) {
                continue;
            }
            let plan = &row.plan;
            let base = row_base_style(plan, &style);
            let mut runs = if let Some(block_runs) = code_runs.remove(&row.line) {
                block_runs
            } else {
                runs_for_row(plan, &base, &colors, &style, &self.hl, &theme)
            };
            if matches!(row.plan.kind, RowKind::Task { checked: true, .. }) {
                runs = apply_task_done(runs, ui.text_muted, ui.text_muted.opacity(0.6));
            }
            // Selection overlay: source cols → bytes → visible bytes.
            let vsel = selection.and_then(|(s, e)| {
                let src_line = self.buffer.line(row.line)?;
                let line_len = src_line.chars().count();
                let (cs, ce, _) = line_selection(s, e, row.line, line_len)?;
                let bs = byte_for_col(src_line, cs);
                let be = byte_for_col(src_line, ce);
                Some((vis_for_src(&plan.segs, bs), vis_for_src(&plan.segs, be)))
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
                Some(vis_for_src(&plan.segs, byte_for_col(src_line, cursor.col)))
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
                Self::caret_block_for(self.cursor_shape, ch, &base, font_size, caret_h, window, cx)
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
        // Ordered display numbers come from the cached geometry (computed
        // once per text version, in document order — counters must see every
        // preceding list item to number correctly). The per-frame cost is a
        // `Vec<Option<u64>>` lookup instead of a scan from line 0.
        let display_numbers = &geometry.display_numbers;
        // `row_els` index of the element containing the caret's source line.
        // Used only as the *fallback* scroll target when the caret row's own
        // rect is unknown (see `scroll_caret_row_into_view`); the precise path
        // never scrolls by element, because that mis-handles tall containers.
        let mut caret_el_ix: Option<usize> = None;
        let mut row_els: Vec<gpui::AnyElement> = Vec::new();
        // 「全宽」块的下标：这些行元素要脱离 780px 阅读窄栏、占满编辑器宽度。
        // 下标是 `row_els` 内的下标，与「滚动列的子元素」一一对应（顶部占位
        // 已经先 push 进 `row_els`，所以下标无需再加偏移）。
        let mut wide_rows: HashSet<usize> = HashSet::new();
        // The first row element's LiveLine also carries the dead-zone drag
        // fallback (exactly one handler per frame); the closure consumes
        // the flag so call sites stay unchanged.
        let mut first_row_el = true;
        // `render_line` needs lots of context: build rows inline via closure.
        let cursor_shape = self.cursor_shape;
        let mut live_line = |pi: usize,
                             paints: &mut HashMap<usize, RowPaint>,
                             entity: &Entity<Self>,
                             colors: &LiveColors|
         -> LiveLine {
            let drag_fallback = first_row_el;
            first_row_el = false;
            // Each visible row is built exactly once; move its paint data out
            // instead of deep-cloning the run vector into the element (and
            // `LiveLine::request_layout` takes it from there into StyledText).
            let mut paint = paints.remove(&pi).expect("visible row must have paint");
            let plan = &rows[pi].plan;
            LiveLine::new(
                ("live-line", rows[pi].line),
                plan.visible.clone(),
                std::mem::take(&mut paint.runs),
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
            .with_caret_block(paint.caret_block_width, paint.caret_block_text.take())
            .with_caret_opacity(caret_opacity)
            .with_caret_metric_height(caret_metric_height)
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
        // resolves document-wide like the preview). 跳过规则与 `refdefs_for_lines`
        // 完全一致（表格 / 源码块 / frontmatter / 数学块里的定义不生效），所以
        // 直接复用 `planned_rows` 已经算好的引用表，不再按 text_version 重扫
        // 整篇文档（旧实现每次击键都多一次 O(n) 扫描）。
        let table_refs = self.planned_refs.clone();
        // ---- mermaid pass1：对**全部** mermaid 块（含视口外，虚拟化不参与）
        // ensure 渲染缓存（后台 + 200ms 防抖），随后推进缓存帧。
        // 全量而非只扫视口内的原因：「上一帧同位置成品图」回退按文档序对齐，
        // 只扫视口会让窗口滑动时位置错位、把别的块的旧图闪现到这个块上。
        // 块列表来自按文本版本缓存的 geometry：不再每帧扫全部 group、也不再
        // 每帧 join 一遍每块源码。
        {
            let mm_colors = MermaidColors::resolve(&theme);
            self.mermaid.begin_frame(mm_colors.fingerprint());
            for block in &geometry.mermaid_blocks {
                self.mermaid
                    .ensure(block.block_id, &block.source, &mm_colors, cx);
            }
            self.mermaid.end_frame();
        }
        // 横向滚动条（表格 / 图）与纵向滚动条的状态帧：`slot` 记录本帧用到的
        // 块，`end_frame` 淘汰其余（删块后不留垃圾）。
        self.h_scroll.begin_frame();
        let v_state = self
            .v_scroll
            .get_or_insert_with(|| cx.new(|_| ScrollBarState::default()))
            .clone();
        let mut window_counts: Vec<usize> = Vec::new();
        // Virtualized frames only build the window's groups. Walking
        // `0..group_count()` and skipping the rest made every frame O(groups)
        // (≈6.7k iterations at 20k lines) even when the window did not move.
        let group_count = geometry.group_count();
        let (group_lo, group_hi) = if virtualized {
            (
                v_lo.min(group_count),
                v_hi.saturating_add(1).min(group_count),
            )
        } else {
            (0, group_count)
        };
        for gi in group_lo..group_hi {
            let Some(group) = geometry.group_at(gi) else {
                continue;
            };
            let els_before = row_els.len();
            match group {
                Group::One(pi) => {
                    let pi = *pi;
                    let row = &rows[pi];
                    let is_first = row.line == 0;
                    let is_last = row.line == last_line;
                    // Ordered display numbers sequence like the preview
                    // (windowed; see display_numbers).
                    let display_number = display_numbers.get(pi).copied().flatten();
                    let el: gpui::AnyElement =
                        match &row.plan.kind {
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
                                            this.edit_no_scroll(cx, |m| {
                                                m.move_to(line, usize::MAX, false)
                                            });
                                            cx.notify();
                                            cx.stop_propagation();
                                        }),
                                    );
                                if paints.get(&pi).is_some_and(|p| p.caret_vis.is_some()) {
                                    blank
                                        .child(live_line(pi, &mut paints, &entity, &colors))
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
                                        .child(live_line(pi, &mut paints, &entity, &colors))
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
                                                this.edit_no_scroll(cx, |m| {
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
                                let src = self.buffer.line(row.line).unwrap_or("");
                                let inner: gpui::AnyElement = match (!row.plan.revealed)
                                    .then(|| parse_image_line_multi(src))
                                    .flatten()
                                {
                                    Some(images) => self.image_strip(&images, cx),
                                    None => live_line(pi, &mut paints, &entity, &colors)
                                        .into_any_element(),
                                };
                                head.child(div().flex_1().min_w_0().child(inner))
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
                                            .child(div().child(box_el).on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _e, _w, cx| {
                                                    this.toggle_task_at(line, cx);
                                                    cx.stop_propagation();
                                                }),
                                            ))
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
                                // 图片独占列表项时渲染成卡片（预览同款）；否则
                                // 照常渲染文本行。`- ![](a.png)` 在预览里有图，
                                // 实时编辑不能只画一个 `![](…)` 文本。
                                let src_line = self.buffer.line(row.line).unwrap_or("");
                                let body: gpui::AnyElement = match (!row.plan.revealed)
                                    .then(|| parse_image_line_multi(src_line))
                                    .flatten()
                                {
                                    Some(images) => self.image_strip(&images, cx),
                                    None => live_line(pi, &mut paints, &entity, &colors)
                                        .into_any_element(),
                                };
                                item.child(gutter)
                                    .child(div().flex_1().w_0().min_w_0().child(body))
                                    .into_any_element()
                            }
                            RowKind::Quote { .. } => {
                                // Single quote lines are grouped above; unreachable.
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &mut paints, &entity, &colors))
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
                                        .child(live_line(pi, &mut paints, &entity, &colors))
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
                                                this.edit_no_scroll(cx, |m| {
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
                                    .child(live_line(pi, &mut paints, &entity, &colors))
                                    .into_any_element()
                            }
                            RowKind::MathDelim { .. } => {
                                // A hidden `$$` delimiter: revealed it shows the
                                // raw marker; otherwise a small click gap
                                // (Setext parity) that targets the line.
                                if row.plan.revealed {
                                    div()
                                        .flex_shrink_0()
                                        .child(live_line(pi, &mut paints, &entity, &colors))
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
                                                this.edit_no_scroll(cx, |m| {
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
                                    .child(live_line(pi, &mut paints, &entity, &colors))
                                    .into_any_element()
                            }
                            RowKind::Paragraph => {
                                // Paragraphs are grouped above; unreachable
                                // (kept as a defensive plain row).
                                div()
                                    .flex_shrink_0()
                                    .child(live_line(pi, &mut paints, &entity, &colors))
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
                    let is_mermaid = lang
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"));
                    let fence_revealed =
                        rows[*open].plan.revealed || close.is_some_and(|c| rows[c].plan.revealed);
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
                            src = src.child(live_line(pi, &mut paints, &entity, &colors));
                        }
                        let last_idx = *idxs.last().unwrap();
                        self.row_targets
                            .push((rows[*open].line, rows[last_idx].line));
                        row_els.push(src.into_any_element());
                        window_counts.push(row_els.len() - els_before);
                        continue;
                    }
                    // Preview-parity code card (fences hidden, like preview).
                    let is_first = rows[*open].line == 0;
                    let is_last = close.is_some_and(|c| rows[c].line == last_line);
                    // ---- mermaid：预览/源码切换（唯一的状态改变入口） -------
                    let showing_code = is_mermaid && self.mermaid.is_showing_code(block_id);
                    // The mermaid source is cached in the geometry (rebuilt on
                    // text change only) — no per-frame join of the code lines.
                    // Non-mermaid fences never need the joined text unless the
                    // copy button is clicked, so it is built lazily below.
                    let mm_body = if is_mermaid {
                        geometry
                            .mermaid_source(block_id)
                            .map(|src| self.mermaid.body(src))
                    } else {
                        None
                    };
                    // 图模式且真的画出了图 → 卡体画图；其余（回退源码 /
                    // 源码 tab / 渲染中无旧图）都画源码行。
                    let show_image =
                        !showing_code && matches!(&mm_body, Some(MermaidBody::Image { .. }));
                    // 「全宽」：图卡片脱离 780px 窄栏。结构图（flowchart 等）
                    // 在窄栏里被压到 640px 上限，节点文字挤成一团 —— 全宽让图
                    // 用满编辑器宽度（**只缩不放**，见下），横向滚动再兜底超宽图。
                    let wide = self.block_is_wide(block_id);
                    // 缩放：`None` = 没按过 `+`/`−`，按「适配宽度」展示。
                    let zoom = self.mermaid.zoom(block_id);
                    // 百分比标签显示**实际显示**的倍率：没设过显式倍率时，展示
                    // 是「适配宽度」（或阅读态 640 上限），其等效倍率就是标签值，
                    // 否则用户看到的数字会与图的实际大小不符。
                    let zoom_pct = zoom_label(zoom.unwrap_or_else(|| {
                        if show_image {
                            self.mermaid_display_scale(block_id, wide)
                        } else {
                            1.0
                        }
                    }));
                    let (mm_h_state, mm_h_handle) = if show_image {
                        let (s, h) = self.h_scroll.slot(block_id, cx);
                        (Some(s), Some(h))
                    } else {
                        (None, None)
                    };
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
                        // 图体：图片保持自然尺寸，视口装不下时左右滚动查看。
                        // gpui 的滚轮在「只开 x 轴」时会把纵向增量喂给 x 轴，
                        // 所以限制到实际轴（纵向继续冒泡给文档滚动）。
                        div()
                            .id(("live-code-inner", block_id))
                            .debug_selector(|| "live-code-inner".into())
                            .overflow_x_scroll()
                            .restrict_scroll_to_axis()
                    } else if is_mermaid {
                        // mermaid 源码同样限制到 x 轴：纵向滚动不该被卡片截胡。
                        div()
                            .id(("live-code-inner", block_id))
                            .debug_selector(|| "live-code-inner".into())
                            .overflow_x_scroll()
                            .restrict_scroll_to_axis()
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
                    if let Some(h_handle) = &mm_h_handle {
                        inner = inner.track_scroll(h_handle);
                    }
                    if show_image {
                        if let Some(MermaidBody::Image { image, pending }) = &mm_body {
                            let (nat_w, nat_h) = image.natural_size();
                            // 展示尺寸：
                            // - 有显式缩放 → 自然尺寸 × 倍率（100% = 自然尺寸）；
                            // - 无显式缩放 → 全宽时**适配卡片宽度**（图占据整个
                            //   宽度，既不溢出也不放大到自然尺寸）；否则沿用阅读
                            //   态尺寸（最多 640）。
                            //
                            // 视口宽取滚动句柄的实测值（gpui 在 prepaint 回填，
                            // 与 `max_offset` 同源）：宽度只在窗口 resize 时变，
                            // 一帧滞后看不出来；首帧为 0 → 回退阅读尺寸，下一帧
                            // 收敛。
                            let (dw, dh) = match zoom {
                                Some(z) => zoomed_size((nat_w, nat_h), z),
                                None if wide => match self.mermaid_viewport_w(block_id) {
                                    Some(vp) => image.fit_width(vp),
                                    None => image.display_size(),
                                },
                                None => image.display_size(),
                            };
                            let img = gpui::img(image.image().clone())
                                .w(px(dw))
                                .h(px(dh))
                                .flex_shrink_0()
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
                            // 图比视口窄时居中；比视口宽时 `min_w` 顶开内容宽度
                            //（`justify_center` 在溢出时自然退化为从左开始 ——
                            //  flex 的居中只在有剩余空间时生效），滚动才有意义。
                            inner = inner.child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .min_w(px(dw))
                                    .justify_center()
                                    .child(fig),
                            );
                        }
                    } else {
                        for pi in code {
                            inner = inner.child(live_line(*pi, &mut paints, &entity, &colors));
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
                    // tab）；其他语言保留徽章，样式与那些 tab 完全一致 ——
                    // 同一个胶囊（[`card_pill`]），区别只是不可点击。
                    if !is_mermaid && let Some(lang) = lang {
                        card = card.child(
                            div()
                                .debug_selector(|| "live-code-lang".into())
                                .absolute()
                                .top(px(9.0))
                                .left(px(10.0))
                                .child(
                                    card_pill(ui.text.opacity(0.05), ui.text_muted)
                                        .child(lang.to_uppercase()),
                                )
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
                            // The joined text is only needed when the button is
                            // actually pressed, so read it then (no per-frame
                            // join of every code block).
                            view_ent.update(cx, |v, cx| {
                                let text = v.code_block_source(block_id);
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
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
                            card_pill(
                                if active {
                                    ui.text.opacity(0.12)
                                } else {
                                    ui.text.opacity(0.05)
                                },
                                if active { ui.text } else { ui.text_muted },
                            )
                            .id(id)
                            .debug_selector(move || sel.to_string())
                            .cursor_pointer()
                            .child(label)
                            .on_click(
                                move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                    ent.update(cx, |v, cx| {
                                        v.set_mermaid_showing_code(block_id, to_source, cx)
                                    });
                                },
                            )
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
                                // 「全宽」：图卡片撑满编辑器宽度。结构图在
                                // 780px 窄栏里被压到 640px 上限，节点文字挤成
                                // 一团；全宽让图用满可用宽度，超宽图再横向滚动。
                                // 文档级全宽（状态条「全宽」芯片）时该按钮没有
                                // 可切换的效果（已全宽），隐藏；按块状态保留，
                                // 切回标宽后原样恢复。
                                .when(!wide_doc, |bar| {
                                    bar.child({
                                        let wide_ent = entity.clone();
                                        div()
                                            .id(("live-mm-tab-wide", block_id))
                                            .debug_selector(|| "live-mm-tab-wide".into())
                                            .px(px(9.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .cursor_pointer()
                                            .bg(if wide {
                                                ui.accent.opacity(0.18)
                                            } else {
                                                ui.text.opacity(0.05)
                                            })
                                            .text_size(px(10.0))
                                            .line_height(px(14.0))
                                            .font_weight(gpui::FontWeight(600.))
                                            .text_color(if wide {
                                                ui.accent
                                            } else {
                                                ui.text_muted
                                            })
                                            .child(if wide { "全宽 ✓" } else { "全宽" })
                                            .on_click(
                                                move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                                    wide_ent.update(cx, |v, cx| {
                                                        v.toggle_wide_block(block_id, cx)
                                                    });
                                                },
                                            )
                                    })
                                })
                                // 缩放：`−` / 百分比（点=还原默认展示）/ `+`。
                                // 没按过时标签显示**当前实际**的显示倍率
                                // （适配宽度 / 阅读尺寸），不是 100%。
                                .child({
                                    let zoom_ent = entity.clone();
                                    let step = |glyph: &'static str,
                                                action: MermaidZoom,
                                                sel: &'static str| {
                                        let ent = zoom_ent.clone();
                                        div()
                                            .id(SharedString::from(format!(
                                                "live-mm-zoom-{sel}-{block_id}"
                                            )))
                                            .debug_selector(move || sel.to_string())
                                            .px(px(6.0))
                                            .py(px(2.0))
                                            .rounded(px(5.0))
                                            .cursor_pointer()
                                            .bg(ui.text.opacity(0.05))
                                            .text_size(px(10.0))
                                            .line_height(px(14.0))
                                            .font_weight(gpui::FontWeight(600.))
                                            .text_color(ui.text_muted)
                                            .hover(|s| {
                                                s.bg(ui.text.opacity(0.12)).text_color(ui.text)
                                            })
                                            .child(glyph)
                                            .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                                ent.update(cx, |v, cx| {
                                                    v.mermaid_zoom_by(block_id, action, cx)
                                                });
                                            })
                                    };
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(3.0))
                                        .child(step("−", MermaidZoom::Out, "live-mm-zoom-out"))
                                        .child({
                                            let ent = zoom_ent.clone();
                                            div()
                                                .id(SharedString::from(format!(
                                                    "live-mm-zoom-pct-{block_id}"
                                                )))
                                                .debug_selector(|| "live-mm-zoom-pct".into())
                                                .px(px(5.0))
                                                .py(px(2.0))
                                                .rounded(px(5.0))
                                                .cursor_pointer()
                                                .text_size(px(10.0))
                                                .line_height(px(14.0))
                                                .font_weight(gpui::FontWeight(600.))
                                                .text_color(ui.text_muted)
                                                // 点百分比 = 还原（回到适配宽度 /
                                                // 阅读尺寸）。
                                                .hover(|s| {
                                                    s.bg(ui.text.opacity(0.12)).text_color(ui.text)
                                                })
                                                .child(zoom_pct)
                                                .on_click(
                                                    move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                                        ent.update(cx, |v, cx| {
                                                            v.mermaid_zoom_by(
                                                                block_id,
                                                                MermaidZoom::Reset,
                                                                cx,
                                                            )
                                                        });
                                                    },
                                                )
                                        })
                                        .child(step("+", MermaidZoom::In, "live-mm-zoom-in"))
                                })
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
                    // 图体的横向滚动条：独立条带（排在滚动容器之后 —— 轨道挂
                    // 在视口坐标上，不跟着内容滚走）。只在画图时出现。
                    if let (Some(h_state), Some(h_handle)) = (mm_h_state, mm_h_handle) {
                        card = card.child(h_bar_strip(
                            ("live-mm-hbar", block_id),
                            h_state,
                            h_handle.clone(),
                            px(6.0),
                            ui.border.opacity(0.75),
                            ui.border_strong,
                            h_handle.max_offset().x > px(0.5),
                        ));
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
                    self.row_targets
                        .push((rows[*code.first().unwrap()].line, group_last_line));
                    if wide {
                        wide_rows.insert(row_els.len());
                    }
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
                        // 纯图片行（一张或多张）渲染成图片；含文字的行走文本。
                        match (!rows[*pi].plan.revealed)
                            .then(|| parse_image_line_multi(src))
                            .flatten()
                        {
                            // 徽章式一行多图横排；单图也走这条路。
                            Some(images) => d = d.child(self.image_strip(&images, cx)),
                            None => d = d.child(live_line(*pi, &mut paints, &entity, &colors)),
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
                        let src = self.buffer.line(rows[*pi].line).unwrap_or("");
                        // 引用里的独占图片行同样渲染成卡片（预览同款）。
                        let el: gpui::AnyElement = match (!rows[*pi].plan.revealed)
                            .then(|| parse_image_line_multi(src))
                            .flatten()
                        {
                            Some(images) => self.image_strip(&images, cx),
                            None => {
                                live_line(*pi, &mut paints, &entity, &colors).into_any_element()
                            }
                        };
                        card = card.child(el);
                    }
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets
                        .push((rows[idxs[0]].line, rows[*idxs.last().unwrap()].line));
                    row_els.push(card.into_any_element());
                }
                Group::Table(idxs) => {
                    let revealed = idxs.iter().any(|c| rows[*c].plan.revealed);
                    let src_lines: Vec<Line> = idxs
                        .iter()
                        .map(|c| {
                            Arc::new(self.buffer.line(rows[*c].line).unwrap_or("").to_string())
                        })
                        .collect();
                    let grid_rows: Vec<Vec<String>> =
                        src_lines.iter().map(|l| split_table_row(l)).collect();
                    // GFM: the delimiter row must carry the header's column
                    // count, otherwise this is not a table at all (same rule
                    // pulldown-cmark applies in the read-only preview).
                    let has_delimiter = grid_rows.len() > 1
                        && table_delimiter_cols(&src_lines[1])
                            .is_some_and(|c| c == grid_rows[0].len() && c > 0);
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
                                    .child(live_line(*pi, &mut paints, &entity, &colors))
                                    .into_any_element(),
                            );
                        }
                        window_counts.push(row_els.len() - els_before);
                        continue;
                    }
                    // Preview-parity grid. The cell under the cursor renders
                    // its own source (markers visible) through `plan_cell`, so
                    // it can be edited in place — the rest stay rendered.
                    let active = self.table_active_cell(rows.as_slice(), idxs, cursor);
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
                    // Typora 的表格工具栏：光标在表内时浮在表格上方。
                    // 左侧列×行计数、中间对齐按钮、右侧 More（同右键菜单）。
                    let cursor_in_table = idxs.iter().any(|pi| rows[*pi].line == cursor.line);
                    // 「全宽」状态：卡片脱离 780px 窄栏、占满编辑器宽度。
                    // 宽表（列多/内容长）在窄栏里每列只剩几十像素，必须能撑开。
                    let wide = self.block_is_wide(start_line);
                    // 横向滚动：表格列是 `flex_1`（无自然宽度），这里给内容一个
                    // 「列数 × 每列可读最小宽」的下界 —— 视口更宽时列平分不滚动，
                    // 更窄时才横向滚动，而不是把每列压到不可读。
                    let (h_state, h_handle) = self.h_scroll.slot(start_line, cx);
                    let table_min_w = table_natural_width(col_count);
                    let mut outer = div()
                        .id(("live-table", start_line))
                        .debug_selector(|| "live-table-outer".into())
                        // 全宽：脱离窄栏（由 `wide_blocks` 驱动，见 render 尾部）。
                        .when(!wide, |this| this.w_full())
                        .relative()
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
                    // Pinned for the duration of a pointer gesture: revealing
                    // the toolbar mid-drag slides the rows down under the
                    // pointer (see `toolbar_frozen`).
                    if self.table_toolbar_visible(start_line, cursor_in_table) {
                        // 当前列的对齐（高亮对应按钮）。
                        let cur_col = crate::editor::markdown_live::table_ops::column_at(
                            &src_lines,
                            cursor.line.saturating_sub(start_line),
                            cursor.col,
                        )
                        .unwrap_or(0);
                        let cur_align = aligns.get(cur_col).copied().unwrap_or(CellAlign::None);
                        let align_btn =
                            |label: &'static str,
                             op: TableMenuOp,
                             want: CellAlign,
                             ent: Entity<Self>| {
                                let on = cur_align == want;
                                div()
                                    .id(SharedString::from(format!(
                                        "live-table-tb-align-{start_line}-{label}"
                                    )))
                                    .debug_selector(move || format!("live-table-toolbar-{label}"))
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(if on { ui.accent } else { ui.text_muted })
                                    .hover(|s| s.bg(ui.text.opacity(0.08)))
                                    .child(label)
                                    .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                        ent.update(cx, |v, cx| v.run_table_menu_op(op, cx));
                                    })
                            };
                        // Typora 工具栏（spec #14）：左端「尺寸」按钮开 resize
                        // 弹层（行/列数步进器）、中间对齐 ×3、右端 More▾（= 右键
                        // 菜单同一套条目）。原 `+行/+列/重排/删除表` 平铺按钮
                        // 移入弹层/菜单。
                        let resize_open = self.table_resize_open;
                        let resize_ent = entity.clone();
                        outer = outer.child(
                            div()
                                .id(("live-table-toolbar", start_line))
                                .debug_selector(|| "live-table-toolbar".into())
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .px(px(6.0))
                                .py(px(3.0))
                                .border_b(px(1.0))
                                .border_color(colors.table_border)
                                // 工具栏内部点击不得冒泡到外层「点表格跳块首」
                                // 监听：按下时被打回块首会让工具栏操作作用到
                                // 错误的行列上。
                                .on_mouse_down(MouseButton::Left, |_, _, cx: &mut gpui::App| {
                                    cx.stop_propagation()
                                })
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "live-table-tb-resize-{start_line}"
                                        )))
                                        .debug_selector(|| "live-table-toolbar-resize".into())
                                        .px(px(6.0))
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .text_size(px(11.0))
                                        .text_color(if resize_open {
                                            ui.accent
                                        } else {
                                            ui.text_muted
                                        })
                                        .hover(|s| s.bg(ui.text.opacity(0.08)).text_color(ui.text))
                                        .child("尺寸")
                                        .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                            resize_ent.update(cx, |v, cx| {
                                                v.table_resize_open = !v.table_resize_open;
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(div().w(px(1.0)).h(px(12.0)).bg(colors.table_border))
                                .child(align_btn(
                                    "左对齐",
                                    TableMenuOp::AlignLeft,
                                    CellAlign::Left,
                                    entity.clone(),
                                ))
                                .child(align_btn(
                                    "居中",
                                    TableMenuOp::AlignCenter,
                                    CellAlign::Center,
                                    entity.clone(),
                                ))
                                .child(align_btn(
                                    "右对齐",
                                    TableMenuOp::AlignRight,
                                    CellAlign::Right,
                                    entity.clone(),
                                ))
                                .child(div().w(px(1.0)).h(px(12.0)).bg(colors.table_border))
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "live-table-tb-more-{start_line}"
                                        )))
                                        .debug_selector(|| "live-table-toolbar-more".into())
                                        .px(px(6.0))
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .text_size(px(11.0))
                                        .text_color(ui.text_muted)
                                        .hover(|s| s.bg(ui.text.opacity(0.08)).text_color(ui.text))
                                        .child("更多▾")
                                        .on_click(cx.listener(
                                            move |this, event: &ClickEvent, window, cx| {
                                                let position = match event {
                                                    ClickEvent::Mouse(e) => e.down.position,
                                                    _ => return,
                                                };
                                                this.open_table_menu(position, window, cx);
                                            },
                                        )),
                                )
                                // 「全宽」：占满编辑器宽度（窄栏里的宽表只能看到
                                // 前几列）。放在最右、与 More▾ 同款样式，激活时
                                // 用 accent 高亮 —— 状态可见，再点一次还原。
                                // 文档级全宽时隐藏（已全宽，无可切换效果）；
                                // 按块状态保留，切回标宽后原样恢复。
                                .when(!wide_doc, |bar| {
                                    bar.child(div().w(px(1.0)).h(px(12.0)).bg(colors.table_border))
                                        .child({
                                            let wide_ent = entity.clone();
                                            div()
                                                .id(SharedString::from(format!(
                                                    "live-table-tb-wide-{start_line}"
                                                )))
                                                .debug_selector(|| "live-table-toolbar-wide".into())
                                                .px(px(6.0))
                                                .py(px(2.0))
                                                .rounded(px(4.0))
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(if wide {
                                                    ui.accent
                                                } else {
                                                    ui.text_muted
                                                })
                                                .hover(|s| {
                                                    s.bg(ui.text.opacity(0.08)).text_color(ui.text)
                                                })
                                                .child(if wide { "全宽 ✓" } else { "全宽" })
                                                .on_click(
                                                    move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                                        wide_ent.update(cx, |v, cx| {
                                                            v.toggle_wide_block(start_line, cx)
                                                        });
                                                    },
                                                )
                                        })
                                }),
                        );
                    }
                    // Click the grid to jump the cursor into the block source.
                    outer = outer.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            this.edit_no_scroll(cx, |m| m.move_to(start_line, 0, false));
                            cx.notify();
                        }),
                    );
                    // Typora 的 tracker 骨架（spec #14）：不再是扁平 CSS grid，
                    // 改为「列把手条 + 逐行 flex 容器」——行把手 8px 在每行最左，
                    // 列把手条在表头上方，格子在行内 `flex_1` 平分。
                    let mut grid = div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .rounded(px(8.0))
                        .overflow_hidden();
                    // 列把手条：左侧 8px 留白与行把手对齐，之后每段 `flex_1`
                    // 与一列同宽；拖动即整列重排（insert marker = 高亮这段）。
                    let mut col_strip = div()
                        .flex()
                        .w_full()
                        .h(px(6.0))
                        .child(div().w(px(8.0)).flex_shrink_0());
                    for col in 0..col_count {
                        let cg = div()
                            .id(SharedString::from(format!(
                                "live-table-col-gutter-{start_line}-{col}"
                            )))
                            .debug_selector(move || format!("live-table-col-gutter-{col}"))
                            .flex_1()
                            .cursor_grab()
                            .hover(|s| s.bg(ui.accent.opacity(0.35)));
                        let cg = attach_table_drag(
                            cg,
                            TableDrag {
                                table_start: start_line,
                                index: col,
                                is_row: false,
                            },
                            entity.clone(),
                            ui.accent,
                            ui.drop_target_border,
                        );
                        col_strip = col_strip.child(cg);
                    }
                    grid = grid.child(col_strip);
                    // grid_rows[0] = header, [1] = delimiter (skipped),
                    // [2..] = data. `preview_idx` matches the preview's
                    // row indices (header 0, data 1, 2, …) for dividers/zebra.
                    for (di, _cells) in grid_rows.iter().enumerate() {
                        if di == 1 {
                            continue;
                        }
                        let is_header = di == 0;
                        let preview_idx = di - usize::from(di > 0);
                        let row_line = rows[idxs[di.min(idxs.len() - 1)]].line;
                        // —— 行把手（最左 8px）：点击 = 光标落到本行首格内容起点；
                        // 数据行可上下拖拽重排（表头行固定，不挂拖拽）。
                        let mut gutter = div()
                            .id(SharedString::from(format!(
                                "live-table-row-gutter-{start_line}-{di}"
                            )))
                            .debug_selector(move || format!("live-table-row-gutter-{di}"))
                            .w(px(8.0))
                            .flex_shrink_0()
                            .cursor_pointer()
                            .hover(|s| s.bg(ui.text.opacity(0.15)));
                        if preview_idx > 0 {
                            gutter = gutter.border_t(px(1.0)).border_color(colors.table_border);
                        }
                        if is_header {
                            gutter = gutter.bg(colors.elevated_bg);
                        } else if preview_idx % 2 == 0 {
                            gutter = gutter.bg(colors.panel_bg);
                        }
                        let g_line = row_line;
                        gutter = gutter.on_click(cx.listener(move |this, _event, _window, cx| {
                            if let Some((cs, _)) = this.table_cell_content_at(g_line, 0) {
                                this.edit_no_scroll(cx, |m| m.move_to(g_line, cs, false));
                                this.after_move(cx);
                            }
                        }));
                        if !is_header {
                            gutter = attach_table_drag(
                                gutter,
                                TableDrag {
                                    table_start: start_line,
                                    index: row_line,
                                    is_row: true,
                                },
                                entity.clone(),
                                ui.accent,
                                ui.drop_target_border,
                            );
                        }
                        let mut row_div = div().flex().w_full().child(gutter);
                        for (col, align) in aligns.iter().enumerate() {
                            let mut cell_div = div()
                                .flex_1()
                                .min_w(px(0.))
                                .px_3()
                                .py_2()
                                .text_size(px(14.));
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
                                    .map(|l| l.as_str())
                                    .unwrap_or("");
                                // Typora `.md-expand`：整格保持渲染，只把光标
                                // 所在的那个行内元素的标记展开（`**粗体**` 里的
                                // `**`），而不是整格翻成源码。
                                let expand = self.table_cell_expand_range(
                                    src_line,
                                    content.clone(),
                                    cursor.col,
                                    row_line == cursor.line,
                                );
                                let plan =
                                    plan_cell(src_line, content.clone(), expand, &table_refs);
                                let base = row_base_style(&plan, &style);
                                let mut runs =
                                    runs_for_row(&plan, &base, &colors, &style, &self.hl, &theme);
                                // 选区高亮：与普通行同一套映射（源列 → 源字节
                                // → 可见字节），但**夹在本格内容范围内** —— 拖过
                                // 格边界时不会把 `|` 和相邻格染上。
                                if let Some((s, e)) = selection {
                                    let line_len = src_line.chars().count();
                                    if let Some((cs, ce, _)) =
                                        line_selection(s, e, row_line, line_len)
                                    {
                                        let bs = byte_for_col(src_line, cs).max(content.start);
                                        let be = byte_for_col(src_line, ce).min(content.end);
                                        if be > bs {
                                            let vsel = (
                                                vis_for_src(&plan.segs, bs),
                                                vis_for_src(&plan.segs, be),
                                            );
                                            runs = apply_selection(
                                                runs,
                                                plan.visible.len(),
                                                Some(vsel),
                                                colors.selection,
                                            );
                                        }
                                    }
                                }
                                let caret_vis = show_caret.then(|| {
                                    vis_for_src(&plan.segs, byte_for_col(src_line, cursor.col))
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
                                        plan.visible.clone(),
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
                                    .with_caret_shape(self.cursor_shape)
                                    .with_caret_opacity(caret_opacity)
                                    .with_caret_metric_height(caret_metric_height)
                                    .with_cell_key(row_line, col),
                                );
                            } else {
                                // 非活动格：与活动格同一套 LiveLine 规划（reveal=false，
                                // 标记隐藏），只是不上光标。两条路径共用一份
                                // 规划器，光标移进/移出格子时文字不会跳变；
                                // 选区高亮同样渲染（spec #6 跨格选区要可见），
                                // 且与活动格一致夹在本格内容范围内。
                                let src_line = src_lines
                                    .get(di.min(src_lines.len() - 1))
                                    .map(|l| l.as_str())
                                    .unwrap_or("");
                                let (s, e) = table_cell_spans(src_line)
                                    .get(col)
                                    .copied()
                                    .unwrap_or((0, src_line.len()));
                                let raw = &src_line[s..e];
                                let lead = raw.len() - raw.trim_start().len();
                                let trail = raw.len() - raw.trim_end().len();
                                let content = (s + lead)..(e - trail).max(s + lead);
                                let plan = plan_cell(src_line, content.clone(), None, &table_refs);
                                let cell_base = row_base_style(&plan, &style);
                                let mut runs = runs_for_row(
                                    &plan, &cell_base, &colors, &style, &self.hl, &theme,
                                );
                                // 选区高亮：与活动格同一套映射，夹在本格内容内
                                //（选区外的格 line_selection 落空，不染色）。
                                if let Some((s, e)) = selection {
                                    let line_len = src_line.chars().count();
                                    if let Some((cs, ce, _)) =
                                        line_selection(s, e, row_line, line_len)
                                    {
                                        let bs = byte_for_col(src_line, cs).max(content.start);
                                        let be = byte_for_col(src_line, ce).min(content.end);
                                        if be > bs {
                                            let vsel = (
                                                vis_for_src(&plan.segs, bs),
                                                vis_for_src(&plan.segs, be),
                                            );
                                            runs = apply_selection(
                                                runs,
                                                plan.visible.len(),
                                                Some(vsel),
                                                colors.selection,
                                            );
                                        }
                                    }
                                }
                                // 探针：非活动格（测试断言点击落在该格）。
                                cell_div = cell_div
                                    .debug_selector(move || format!("live-table-cell-{di}-{col}"));
                                cell_div = cell_div.child(
                                    LiveLine::new(
                                        ("live-table-cell", start_line + di * 64 + col),
                                        plan.visible.clone(),
                                        runs,
                                        row_line,
                                        entity.clone(),
                                        false,
                                        None, // 光标只在活动格
                                        colors.caret,
                                        CURSOR_WIDTH,
                                        px(0.0),
                                    )
                                    .with_cell_segs(plan.segs.clone(), content.clone())
                                    .with_caret_shape(self.cursor_shape)
                                    .with_cell_key(row_line, col),
                                );
                            }
                            // spec #6：元素级按下/移动认领 —— 命中框 = 整格
                            //（含内边距/未填满区域），只有指针所在的格触发；
                            // 坐标 → 行列的映射走本格 LiveLine 的帧内记录
                            //（cell_hits；双击/Shift 语义仍走
                            // on_table_cell_mouse_down）。
                            let hit_key = (row_line, col);
                            cell_div = cell_div.on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        let Some(c) =
                                            this.cell_col_at(hit_key.0, hit_key.1, event.position)
                                        else {
                                            return;
                                        };
                                        this.on_table_cell_mouse_down(
                                            hit_key.0,
                                            c,
                                            event.click_count,
                                            event.modifiers.shift,
                                            window,
                                            cx,
                                        );
                                    },
                                ),
                            );
                            cell_div = cell_div.on_mouse_move(cx.listener(
                                move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                                    if !matches!(
                                        event.pressed_button,
                                        Some(gpui::MouseButton::Left)
                                    ) {
                                        return;
                                    }
                                    let Some(c) =
                                        this.cell_col_at(hit_key.0, hit_key.1, event.position)
                                    else {
                                        return;
                                    };
                                    this.on_table_cell_drag(hit_key.0, c, cx);
                                },
                            ));
                            row_div = row_div.child(cell_div.into_any_element());
                        }
                        grid = grid.child(row_div);
                    }
                    // 横向滚动：grid 用 `overflow_x_scroll` 承载溢出（gpui 负责
                    // 钳制与滚轮），内容下界 `table_min_w` 保证列不会被压到不可读。
                    // 滚动条是**兄弟节点**、排在滚动容器之后：轨道挂在视口坐标上，
                    // 不跟着内容滚走（否则拖到右端就摸不到 thumb）。
                    //
                    // `restrict_scroll_to_axis` 必须开：gpui 的
                    // `paint_scroll_listener` 在「只开 x 轴」时会把纵向滚轮增量
                    // 喂给 x 轴，普通上下滚动会被表格截胡。
                    let grid_scroll = div()
                        .id(("live-table-scroll", start_line))
                        .debug_selector(|| "live-table-scroll".into())
                        .w_full()
                        .overflow_x_scroll()
                        .track_scroll(&h_handle)
                        .restrict_scroll_to_axis()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_shrink_0()
                                .min_w(table_min_w)
                                .child(grid),
                        );
                    outer = outer.child(grid_scroll);
                    // 滚动条条带：独立占位（不是覆盖层），永远不会压住最后一行。
                    outer = outer.child(h_bar_strip(
                        ("live-table-hbar", start_line),
                        h_state,
                        h_handle.clone(),
                        px(6.0),
                        ui.border.opacity(0.75),
                        ui.border_strong,
                        h_handle.max_offset().x > px(0.5),
                    ));
                    // 尺寸弹层（Typora 的 resize popover，spec #14）：行/列数
                    // 步进器（行 2..=99、列 1..=40，超限由 `set_*_count` 内部
                    // 夹紧 → 点到头就是无变化）。必须排在 grid **之后**（否则
                    // 被后画的兄弟盖住），并用 `.absolute()` 挂到工具栏下方。
                    if cursor_in_table && self.table_resize_open {
                        let rc = crate::editor::markdown_live::table_ops::table_data_rows(
                            &src_lines,
                            (0, src_lines.len().saturating_sub(1)),
                        )
                        .len();
                        // 一个步进按钮（−/+）：按当前光标块算目标值再走 op。
                        let sb = |probe: &'static str, glyph: &'static str, ent: Entity<Self>| {
                            div()
                                .id(SharedString::from(format!(
                                    "live-table-tb-step-{start_line}-{probe}"
                                )))
                                .debug_selector(move || format!("live-table-resize-{probe}"))
                                .px(px(7.0))
                                .py(px(1.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .text_size(px(12.0))
                                .text_color(ui.text_muted)
                                .hover(|s| s.bg(ui.text.opacity(0.10)).text_color(ui.text))
                                .child(glyph)
                                .on_click(move |_: &ClickEvent, _, cx: &mut gpui::App| {
                                    ent.update(cx, |v, cx| {
                                        use crate::editor::markdown_live::table_ops as t;
                                        let lines = v.buffer.lines().to_vec();
                                        let cur = v.buffer.cursor();
                                        let Some(block) = t::table_block_range(&lines, cur.line)
                                        else {
                                            return;
                                        };
                                        match probe {
                                            "row-plus" | "row-minus" => {
                                                let n = t::table_data_rows(&lines, block).len();
                                                if n == 0 {
                                                    return;
                                                }
                                                let target =
                                                    if probe == "row-plus" { n + 1 } else { n - 1 };
                                                v.run_table_op(cx, |m| t::set_row_count(m, target));
                                            }
                                            _ => {
                                                let c = t::table_col_count(&lines, block);
                                                let target = if probe == "col-plus" {
                                                    c + 1
                                                } else {
                                                    c.saturating_sub(1)
                                                };
                                                v.run_table_op(cx, |m| t::set_col_count(m, target));
                                            }
                                        }
                                    });
                                })
                        };
                        let stepper = |label: &'static str, value: usize, base: &'static str| {
                            let ent_row = entity.clone();
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(12.0))
                                .text_color(ui.text)
                                .child(label)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(sb(
                                            // 'static 探针名按按钮拼接：
                                            // base = "row" / "col"。
                                            if base == "row" {
                                                "row-minus"
                                            } else {
                                                "col-minus"
                                            },
                                            "-",
                                            ent_row.clone(),
                                        ))
                                        .child(
                                            div()
                                                .w(px(34.0))
                                                .text_size(px(12.0))
                                                .child(format!("{value}")),
                                        )
                                        .child(sb(
                                            if base == "row" {
                                                "row-plus"
                                            } else {
                                                "col-plus"
                                            },
                                            "+",
                                            ent_row,
                                        )),
                                )
                        };
                        outer = outer.child(
                            div()
                                .id(SharedString::from(format!(
                                    "live-table-resize-{start_line}"
                                )))
                                .debug_selector(|| "live-table-resize-panel".into())
                                .absolute()
                                .top(px(30.0))
                                .left(px(6.0))
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .px(px(10.0))
                                .py(px(8.0))
                                .rounded(px(6.0))
                                .border(px(1.0))
                                .border_color(ui.border)
                                .bg(colors.elevated_bg)
                                // 弹层内部点击同样不得冒泡到「跳块首」监听。
                                .on_mouse_down(MouseButton::Left, |_, _, cx: &mut gpui::App| {
                                    cx.stop_propagation()
                                })
                                .child(stepper("行数", rc, "row"))
                                .child(stepper("列数", col_count, "col")),
                        );
                    }
                    if idxs.iter().any(|pi| rows[*pi].line == cursor.line) {
                        caret_el_ix = Some(row_els.len());
                    }
                    self.row_targets
                        .push((rows[idxs[0]].line, rows[*idxs.last().unwrap()].line));
                    // 右键菜单（Typora 的表格菜单）：与工具栏 More▾ 共用
                    // `table_menu_body` 一套条目，两处行为天然一致；`context_menu`
                    // 会改变元素类型，所以必须是 `outer` 的最后一步。
                    let menu_ent = entity.clone();
                    if wide {
                        wide_rows.insert(row_els.len());
                    }
                    row_els.push(
                        outer
                            .context_menu(
                                ("live-table-menu", start_line),
                                move |menu, _window, _cx| table_menu_body(menu, menu_ent.clone()),
                            )
                            .into_any_element(),
                    );
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
            self.last_render_stats = (geometry.group_count(), v_hi - v_lo + 1, virtualized);
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
                            .with_caret_opacity(caret_opacity)
                            .with_caret_metric_height(caret_metric_height)
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

        // Scroll-to-caret: bring the caret's *own row* into view, minimally.
        //
        // The rows column registers one child per group with the scroll
        // handle, and a group can be far taller than the viewport (a table
        // grid, a code card, a quote/paragraph run). GPUI's element-level
        // `scroll_to_item` resolves that case by pinning the child's **top**
        // edge to the viewport top (`scroll_handle_aligns_tall_children_to_top_edge`),
        // which yanks the view up to the block's first row whenever the caret
        // sits on a later row — pushing the row being edited off-screen and
        // making it permanently unreachable. That is the bug this fixes.
        //
        // Two steps, because a scroll can only be computed from bounds that
        // already exist:
        //
        // 1. *here* (before layout): if the caret row's element is known, ask
        //    the scroll handle to bring that element into view — but only when
        //    the caret row is not already inside the viewport, so typing in a
        //    tall table never scrolls at all;
        // 2. *next frame* (`correct_caret_scroll`): re-measure the caret row's
        //    own rect (now exact, post-layout) and, if the block-level scroll
        //    of step 1 left it outside the viewport, nudge by the minimal
        //    amount. This is what makes a tall container behave like a normal
        //    line: the container's top edge is never the final resting place.
        if self.scroll_to_cursor {
            match caret_el_ix {
                Some(ix) => {
                    // Skip the element-level scroll when the caret row is
                    // already visible (the common typing case: no movement at
                    // all). Unknown geometry (row not painted yet) must still
                    // scroll, otherwise a long jump never reaches its target.
                    if self.caret_row_in_view() != Some(true) {
                        self.scroll.scroll_to_item(ix);
                    }
                    self.caret_scroll_passes = CARET_SCROLL_CORRECTION_PASSES;
                    self.schedule_caret_scroll_correction(cx);
                }
                None => self.scroll_to_cursor = false,
            }
        }

        // Outline (Typora Outline parity) + current section highlight.
        // Hidden panel: skip the O(rows) scan entirely (large docs pay it
        // per frame otherwise).
        let (outline, current_section) = if self.show_outline {
            let outline = outline_entries(rows.as_slice());
            let current = outline
                .iter()
                .rfind(|(line, _, _)| *line <= cursor.line)
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
            .id("live-status-bar")
            .debug_selector(|| "live-status".into())
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
                    // 全宽：单个按钮切换整个文档的渲染宽度（未选中 = 标宽
                    // 780px 阅读窄栏，选中 = 所有行铺满编辑器宽度）。选中态
                    // 高亮，与「源码」「大纲」同款样式。
                    .child(
                        div()
                            .id("live-width-toggle")
                            .debug_selector(|| "live-width-toggle".into())
                            .cursor_pointer()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .text_color(if wide_doc { ui.accent } else { ui.text_muted })
                            .hover(|this| this.bg(ui.tab_hover_background))
                            .child("全宽")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.toggle_wide_doc(cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("live-source-toggle")
                            .debug_selector(|| "live-source-toggle".into())
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
                            .child("源码")
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
                            .debug_selector(|| "live-outline-toggle".into())
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
                            .child("大纲")
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
        let slash_bar: Option<gpui::AnyElement> =
            self.slash_menu().filter(|_| focused).map(|(_, items)| {
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

        // 工具栏 More▾ 的手动表格菜单浮层（右键菜单由 ContextMenu 自带浮层，
        // 这里是 `open_table_menu` 手动模式的挂载点）。
        let table_menu_overlay =
            self.table_menu
                .clone()
                .zip(self.table_menu_position)
                .map(|(menu, position)| {
                    // 与 ContextMenu 一致：菜单可见期间**在帧内**把焦点贴到菜单
                    // 上（派发期同步 focus 会被丢弃），Esc/↑↓/Enter 才走得到菜单。
                    let menu_focus = menu.focus_handle(cx);
                    if !menu_focus.contains_focused(window, cx) {
                        menu_focus.focus(window, cx);
                    }
                    gpui::deferred(
                        gpui::anchored()
                            .position(position)
                            .anchor(gpui::Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.0))
                            .child(menu),
                    )
                    .with_priority(100)
                    .into_any_element()
                });

        // 菜单刚关闭（Esc / 点菜单项 / 点别处）：焦点还悬在已卸载的菜单句柄
        // 上，这里把它还回编辑器，键盘输入才不会失联。
        if self.table_menu_restore_focus && self.table_menu.is_none() {
            self.table_menu_restore_focus = false;
            window.focus(&self.focus, cx);
        }

        // 横向滚动条收帧：淘汰文档里已不存在的块（`wide_anchors` 就是全部
        // 可能带横向条的块）。滚出窗口/切源码视图的块保留句柄与滚动位置。
        self.h_scroll.end_frame(&geometry.wide_anchors);
        // 「全宽」状态按块首行记账，行号会随编辑漂移 —— 收帧时裁到仍存在的块。
        self.prune_wide_blocks();

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
                            .pt_8()
                            .pb_2()
                            .h_full()
                            .overflow_hidden()
                            // 查找面板 / slash 菜单 / 状态条跟随阅读窄栏
                            //（780px 居中）；正文行各自决定宽度，见下面的
                            // 「全宽」包装。文档级全宽时三者同样铺满。
                            .when_some(self.search_panel.clone(), |this, panel| {
                                this.child(if wide_doc {
                                    wide_row(center_narrow(panel.into_any_element()))
                                } else {
                                    center_narrow(panel.into_any_element())
                                })
                            })
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .flex()
                                    .flex_col()
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
                                            // 每行一个包装：默认居中在 780px
                                            // 阅读窄栏里，「全宽」块（`wide_rows`）
                                            // 占满编辑器宽度。逐行包装而不是整列定宽，
                                            // 是为了让宽表/大图真正撑开而不拉伸正文。
                                            // 文档级全宽时**所有行**都走全宽包装。
                                            .children(row_els.into_iter().enumerate().map(
                                                |(ix, el)| {
                                                    if wide_doc || wide_rows.contains(&ix) {
                                                        wide_row(el)
                                                    } else {
                                                        center_narrow(el)
                                                    }
                                                },
                                            )),
                                    )
                                    // 纵向滚动条：贴内容区右缘的覆盖层（与代码编辑器
                                    // 同款）。挂在滚动容器**外面** —— 挂进内容里会跟着
                                    // 内容一起滚走。
                                    .child(
                                        div()
                                            .id("live-vscroll-bar")
                                            .debug_selector(|| "live-vscroll-bar".into())
                                            .absolute()
                                            .top_0()
                                            .bottom_0()
                                            .right_0()
                                            .w(BAR_THICKNESS)
                                            .child(scroll_bar(
                                                gpui::Axis::Vertical,
                                                v_state,
                                                self.scroll.clone(),
                                                ui.border.opacity(0.7),
                                                ui.border_strong,
                                                // 只认 thumb：轨道压在内容右缘，
                                                // 轨道点击会把「点行尾」吃掉。
                                                false,
                                            )),
                                    ),
                            )
                            .when_some(slash_bar, |this, bar| {
                                this.child(if wide_doc {
                                    wide_row(bar.into_any_element())
                                } else {
                                    center_narrow(bar.into_any_element())
                                })
                            })
                            .child(if wide_doc {
                                wide_row(status.into_any_element())
                            } else {
                                center_narrow(status.into_any_element())
                            }),
                    ),
            )
            .when_some(outline_panel, |this, panel| this.child(panel))
            .when_some(table_menu_overlay, |this, menu| this.child(menu))
    }
}

/// 把一行元素居中放进 780px 阅读窄栏（正文的默认版式）。
///
/// 与只读预览（`Markdown` 的 `max_w(px(780.))`）保持同一套宽度，两处渲染
/// 切换时正文不会跳版。
fn center_narrow(el: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .justify_center()
        .child(div().w_full().max_w(px(780.)).px_8().child(el))
        .into_any_element()
}

/// 把一行元素铺满编辑器宽度（「全宽」版式）。
///
/// 与逐块「全宽」（表格 / mermaid 卡片）同一套包装：`px_4` 留出与窄栏
/// `px_8` 相称的呼吸空间。文档级全宽时所有行都走这里。
fn wide_row(el: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .w_full()
        .flex_shrink_0()
        .px_4()
        .debug_selector(|| "live-wide-row".into())
        .child(el)
        .into_any_element()
}

#[cfg(test)]
mod live_render_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;
    use gpui::{point, size};

    const SAMPLE: &str = "# Title\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\npara text\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    const SHOWCASE: &str = include_str!("../../../docs/markdown-showcase.md");

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
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        // 1. Build the exact LiveLines the view would build.
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme_shared();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(SAMPLE);
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
                        r.plan.visible.clone(),
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
            let (styled, _) = cx.draw(point(px(0.), px(0.)), size(px(700.), px(200.)), |_, _| {
                case.line
            });
            let bounds = styled.layout().bounds();
            assert!(bounds.size.width > px(0.), "zero width for {}", case.label);
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
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx))));
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
            assert!(
                !v.group_heights
                    .contains_key(&(hi + GROUP_HEIGHT_KEEP_MARGIN + 1))
            );
            // Margin neighbors survive.
            assert!(
                v.group_heights
                    .contains_key(&lo.saturating_sub(GROUP_HEIGHT_KEEP_MARGIN))
            );
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
        let (host, cx) = cx
            .add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(SHOWCASE, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        struct Case {
            label: String,
            line: LiveLine,
        }
        let cases: Vec<Case> = cx.update(|window, cx| {
            let theme = cx.theme_shared();
            let style = MarkdownStyle::themed(window, cx);
            let colors = LiveColors::resolve(&style, &theme);
            let hl = HighlightCache::new();
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(SHOWCASE);
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
                    let label = format!(
                        "L{} {:?} {:?}",
                        r.line,
                        r.plan.kind,
                        r.plan.visible.chars().take(24).collect::<String>()
                    );
                    Case {
                        label,
                        line: LiveLine::new(
                            ("live-showcase-line", pi),
                            r.plan.visible.clone(),
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
            let (styled, _) = cx.draw(point(px(0.), px(0.)), size(px(700.), px(200.)), |_, _| {
                case.line
            });
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

    /// 语言徽章必须和 mermaid 卡片左上角「预览 | 源码」tab 同款：共用
    /// [`card_pill`] 的胶囊样式（同高、左偏移一致），区别只是不可点击。
    #[gpui::test]
    fn code_language_badge_matches_mermaid_tab_pill(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str =
            "```rust\nfn main() {}\n```\n\n```mermaid\nflowchart TD\n    A --> B\n```\n";
        let (_host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        cx.refresh().unwrap();
        cx.run_until_parked();
        let lang = cx.debug_bounds("live-code-lang").expect("语言徽章");
        let tab = cx.debug_bounds("live-mm-tab-graph").expect("预览 tab");
        assert!(
            (f32::from(lang.size.height) - f32::from(tab.size.height)).abs() <= 0.5,
            "语言徽章与 mermaid tab 必须同高：{lang:?} vs {tab:?}"
        );
        assert!(
            (f32::from(lang.origin.x) - f32::from(tab.origin.x)).abs() <= 0.5,
            "语言徽章与 mermaid tab 必须左边对齐：{lang:?} vs {tab:?}"
        );
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
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx))));
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
        let painted: Vec<(usize, usize)> =
            crate::editor::markdown_live::element::LIVE_LINE_PAINT_LOG
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
            cx.debug_bounds("live-code-inner")
                .is_some_and(|b| b.size.height > px(50.)),
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
            cx.debug_bounds("live-code-inner")
                .is_some_and(|b| b.size.height > px(50.)),
            "code card missing after leaving the block"
        );
        // The typed char must have stuck in the buffer.
        let text = view.read_with(cx, |v, _| v.text());
        assert!(
            text.contains("fnx ") || text.contains("fn x"),
            "typed char lost: {text:?}"
        );
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
        const DOC: &str = include_str!("../../../docs/markdown-showcase.md");
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        let lines: Vec<Line> = DOC.split('\n').map(|l| Arc::new(l.to_string())).collect();
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
    /// 该格；Tab 跨格（末格加行）、Esc 不退出表格；`Cmd+/` 仍可看整块源码。
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
        assert!(
            line_after.contains('9'),
            "插入的字符应在该行：{line_after:?}"
        );
        assert!(
            text.contains("| A    | B    |"),
            "表头结构不变，仅随 spec #13 自动重排列宽对齐：{text:?}"
        );
        assert!(
            text.contains("| 3    | 4    |"),
            "其他数据行结构不变，仅随 spec #13 对齐：{text:?}"
        );
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
        assert!(line_a <= 3, "Tab 不得离开表格（line={line_a}）");

        // 4) Esc：Typora 在表格里**不退出表格**（只关弹窗/清选区），光标
        // 留在格内、文本不变。
        let before = view.read_with(cx, |v, _| v.text());
        let line_before = view.read_with(cx, |v, _| v.buffer.cursor().line);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let line = view.read_with(cx, |v, _| v.buffer.cursor().line);
        assert_eq!(line, line_before, "Esc 不该把光标移出表格（Typora 语义）");
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            before,
            "Esc 不得改动文本"
        );

        // 5) Enter：移到**下方同列**格（Typora），不是插入新行。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        let rows_before = view.read_with(cx, |v, _| v.buffer.line_count());
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before,
            "Enter 不该改变行数（Typora：移到下方同列格）"
        );
        assert!(line > 2, "Enter 应移到下方单元格（line={line}, col={col}）");

        // 6) Cmd/Ctrl+Enter：在当前行下方插入空行（Typora）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        let rows_before = view.read_with(cx, |v, _| v.buffer.line_count());
        cx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before + 1,
            "Cmd+Enter 应插入一行（Typora）"
        );
    }

    /// Typora 表格键盘语义：Cmd+A 只选当前格、格首退格跳到上一格末（不合并）、
    /// 格边界上左右键纵向换行（spec #8）、上下键按列索引。
    #[gpui::test]
    fn live_table_typora_keyboard_semantics(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 0:表头 1:分隔 2:数据 3:数据
        const DOC: &str = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });

        // 1) Cmd+A：只选当前格内容（`2`），不是整行/整文档。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 8, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("cmd-a");
        cx.run_until_parked();
        let sel = view.read_with(cx, |v, _| v.buffer.selection());
        let text = view.read_with(cx, |v, _| v.selected_source_text());
        assert!(sel.is_some(), "Cmd+A 应产生选区");
        assert_eq!(
            text.as_deref(),
            Some("2"),
            "Cmd+A 在表格里只选当前格内容（Typora）"
        );

        // 2) spec #12 哨兵退格：**非空**格首按退格 → 消费但什么都不做
        //    （不删字符、不合并、也不跳格）。
        let before = view.read_with(cx, |v, _| v.text());
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            before,
            "格首退格不得删字符/合并单元格"
        );
        assert_eq!(
            (line, col),
            (2, 6),
            "非空格首退格应原地消费（line={line}, col={col}）"
        );

        // 3) spec #8：「行即文本行」—— 格尾按右 → 同列下一格**首**；
        //    格首按左 → 同列上一格**末**（不再是流序相邻格）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 7, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        let (line_r, col_r) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(
            (line_r, col_r),
            (3, 6),
            "格尾右键应到同列下一格首（line={line_r}, col={col_r}）"
        );
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 6, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        let (line_l, col_l) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(
            (line_l, col_l),
            (2, 7),
            "格首左键应到同列上一格末（line={line_l}, col={col_l}）"
        );

        // 3b) 表头格首按左键：同列无上一格 → 消费按键，原地不动。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        let (line_h, col_h) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(
            (line_h, col_h),
            (0, 2),
            "表头格首按左键应原地消费（line={line_h}, col={col_h}）"
        );

        // 3c) Shift+右（格尾）→ 选区扩展进同列下一格（extend 生效）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 7, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("shift-right");
        cx.run_until_parked();
        let sel = view.read_with(cx, |v, _| v.buffer.selection());
        assert_eq!(
            sel,
            Some((
                crate::editor::markdown_live::model::Pos::new(2, 7),
                crate::editor::markdown_live::model::Pos::new(3, 6)
            )),
            "Shift+右应把选区从格尾扩展到同列下一格首"
        );

        // 4) 上下键按列索引：从第 2 行第 2 格向下 → 第 3 行第 2 格首。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 8, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(line, 3, "下键应到下一行（line={line}）");
        assert_eq!(col, 6, "下键应保持**列索引**并落在下一格首（col={col}）");
    }

    /// spec #12（哨兵退格/删除）：**空**格首退格跳到流序上一格末；**非空**格
    /// 首与格尾 Delete 原地消费；Cmd+退格 / Cmd+Delete 分别删到格内容首/尾，
    /// 不切掉管道。
    #[gpui::test]
    fn live_table_sentinel_backspace_delete_spec12(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 0:表头 1:分隔 2:数据(第1格空) 3:数据 4:空行 5:尾段
        const DOC: &str = "| A | B |\n|---|---|\n|   | 2 |\n| 3 | 4 |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 1) **空**格首退格 → 跳到流序上一格（表头 B 格）**末**，文本不动。
        let before = view.read_with(cx, |v, _| v.text());
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!(
            (c.line, c.col),
            (0, 7),
            "空格首退格应到流序上一格末（line={}, col={}）",
            c.line,
            c.col
        );
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            before,
            "跳格不得改文本"
        );

        // 2) **非空**格首退格 → 原地消费。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 2), "非空格首退格原地消费");

        // 3) 格内容**末尾**按 Delete → 哨兵命中，无操作。
        let before = view.read_with(cx, |v, _| v.text());
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 7, false)); // `2` 的内容末
            v.after_move(cx);
        });
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            before,
            "格尾 Delete 不得删字符"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 7), "格尾 Delete 原地消费");

        // 4) Cmd+退格 → 删到格内容首（管道与同排格子保留）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 7, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).map(str::to_string));
        // 删除后触发 spec #13 自动重排：结构（3 根管道）保留，列宽补齐为 4。
        assert_eq!(
            line2.as_deref(),
            Some("|      |      |"),
            "Cmd+退格只删格内容，保留管道（随后自动重排对齐）"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 14), "空格内容起点 = 右管道前");

        // 5) Cmd+Delete → 删到格内容尾。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 2, false)); // `3` 的内容首
            v.after_move(cx);
        });
        cx.simulate_keystrokes("cmd-delete");
        cx.run_until_parked();
        let line3 = view.read_with(cx, |v, _| v.buffer.line(3).map(str::to_string));
        // 删掉 `3` 后自动重排（spec #13）：col0 变空、补齐为宽 4。
        assert_eq!(
            line3.as_deref(),
            Some("|      | 4    |"),
            "Cmd+Delete 只删到格内容尾（随后自动重排对齐）"
        );
    }

    /// spec #12 续：**整表为空**时，表格首格按退格 → 删除整张表。
    #[gpui::test]
    fn live_table_empty_table_backspace_deletes_table(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "|  |  |\n|---|---|\n|  |  |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "\ntail\n",
            "空表首格退格应删除整张表"
        );
    }

    /// spec #13：表格内打字后自动重排（列宽/对齐/分隔行），光标留在同
    /// (行, 格, 格内偏移)；「打字 + 重排」合并成**一步**撤销；撤销/重做
    /// 恢复历史原样、不再次重排。
    #[gpui::test]
    fn live_table_auto_prettify_after_typing_spec13(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "| a | bb |\n|---|---|\n| 1 | 2 |\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 1) 在第 1 条数据行第 1 格内容首输入 `x` → 整表按 Typora 规则重排，
        //    光标保持 (行, 格, 格内偏移) = (2, 0格, 偏移1) → (2, 3)。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        let lines: Vec<String> = view.read_with(cx, |v, _| {
            (0..3)
                .filter_map(|i| v.buffer.line(i).map(str::to_string))
                .collect()
        });
        assert_eq!(lines[0], "| a    | bb   |", "表头列宽补齐");
        assert_eq!(lines[1], "| ---- | ---- |", "分隔行统一形态");
        assert_eq!(lines[2], "| x1   | 2    |", "数据行对齐");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 3), "光标留在同格同偏移（x 与 1 之间）");

        // 2) 再输入 `y`：内容变宽（仍在列宽内）→ 光标跟随。
        cx.simulate_keystrokes("y");
        cx.run_until_parked();
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).map(str::to_string));
        assert_eq!(line2.as_deref(), Some("| xy1  | 2    |"));
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 4), "光标继续跟在 xy 之后");

        // 3) **一步**撤销跨过「打字 + 两次重排」→ 回到原始文档。
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            DOC,
            "打字+重排必须是一步撤销"
        );

        // 4) 重做恢复到重排后的形态（撤销/重做不被自动重排破坏）。
        cx.simulate_keystrokes("cmd-shift-z");
        cx.run_until_parked();
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).map(str::to_string));
        assert_eq!(
            line2.as_deref(),
            Some("| xy1  | 2    |"),
            "重做恢复重排后状态"
        );
    }

    /// spec #13 续：「源码」模式里裸编辑**不**自动重排（保护手写格式）；
    /// 退出源码模式后恢复正常。
    #[gpui::test]
    fn live_table_source_mode_skips_prettify_spec13(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "| a | bb |\n|---|---|\n| 1 | 2 |\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 源码模式里输入 → 保持裸源码，不重排。
        view.update(cx, |v, cx| v.toggle_show_source(cx));
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        let line0 = view.read_with(cx, |v, _| v.buffer.line(0).map(str::to_string));
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).map(str::to_string));
        assert_eq!(line0.as_deref(), Some("| a | bb |"), "源码模式：表头不动");
        assert_eq!(line2.as_deref(), Some("| x1 | 2 |"), "源码模式：不自动重排");

        // 退出源码模式后再输入 → 恢复自动重排。
        view.update(cx, |v, cx| v.toggle_show_source(cx));
        cx.simulate_keystrokes("y");
        cx.run_until_parked();
        let line0 = view.read_with(cx, |v, _| v.buffer.line(0).map(str::to_string));
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).map(str::to_string));
        assert_eq!(
            line0.as_deref(),
            Some("| a    | bb   |"),
            "退出源码模式恢复重排"
        );
        assert_eq!(line2.as_deref(), Some("| xy1  | 2    |"));
    }

    /// spec #15/#14：表格内光标折叠 Cmd+C = 整行复制（选中该行 + 源码进剪贴板）；
    /// 菜单「复制表格」复制整表源码；Cmd+Shift+Alt+L 删行（表内删本行、表外删整行）。
    #[gpui::test]
    fn live_table_copy_and_delete_line_spec15(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 1) 折叠光标 Cmd+C → 复制**整行源码**，该行进入选中态。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("cmd-c");
        cx.run_until_parked();
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("| 1 | 2 |"), "折叠光标复制整行源码");
        assert!(
            view.read_with(cx, |v, _| v.buffer.selection()).is_some(),
            "复制后该行应处于选中态（Typora lineWiseCopyCut）"
        );

        // 2) 菜单「复制表格」→ 整表源码，且不动文本。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        view.update(cx, |v, cx| v.run_table_menu_op(TableMenuOp::CopyTable, cx));
        cx.run_until_parked();
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(
            clip.as_deref(),
            Some("| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |"),
            "复制表格 = 整表源码"
        );
        assert_eq!(view.read_with(cx, |v, _| v.text()), DOC, "复制不得改动文本");

        // 3) Cmd+Shift+Alt+L 在表格里 → 删除本行（行数 -1）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        let rows_before = view.read_with(cx, |v, _| v.buffer.line_count());
        cx.simulate_keystrokes("cmd-shift-alt-l");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before - 1,
            "表格里 Cmd+Shift+Alt+L 应删除本行"
        );
        let text = view.read_with(cx, |v, _| v.text());
        assert!(!text.contains('1'), "被删行的文本不应残留：{text:?}");

        // 4) Cmd+Shift+Alt+L 在表格外 → 删除光标整行（连同行尾换行）。
        let rows_before = view.read_with(cx, |v, _| v.buffer.line_count());
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(4, 2, false));
            v.after_move(cx);
        });
        cx.simulate_keystrokes("cmd-shift-alt-l");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before - 1,
            "表格外应删除整行"
        );
        let text = view.read_with(cx, |v, _| v.text());
        assert!(!text.contains("tail"), "行文本应被删除：{text:?}");
    }

    /// 回归：菜单「下方插入行」出来的空白行，点击**非首格**应把光标落在
    /// 被点格的内容里（此前空白格 `segs` 为空，`src_for_vis` 兜底 0 →
    /// 经内容夹取后落回**首格**，只有 Tab 能过去）。
    #[gpui::test]
    fn live_table_blank_row_click_lands_in_clicked_cell(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 光标进数据行 → 「下方插入行」→ 新空白行（第 3 行，可能已被重排
        // 补齐 padding，列位置按内容范围动态取）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        view.update(cx, |v, cx| v.run_table_menu_op(TableMenuOp::RowBelow, cx));
        cx.refresh().unwrap();
        cx.run_until_parked();

        let blank = view.read_with(cx, |v, _| v.buffer.line(3).unwrap_or("").to_string());
        let (cs, ce) = cell_content_cols(&blank, 1).expect("新行应是表格行");
        let nb = cx
            .debug_bounds("live-table-cell-3-1")
            .expect("新空白行的第 2 格应有探针与命中框");
        cx.simulate_click(nb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();

        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!(c.line, 3, "点击应落在新插入的空白行");
        assert!(
            (cs..=ce).contains(&c.col),
            "光标应落在被点格（第 2 格）的内容内：col={}，格=[{cs},{ce}]，行={blank:?}",
            c.col
        );
    }

    /// spec #6：跨格选区全链路 —— ①点非活动格由该格认领（不再被活动格
    /// 串认领整行后夹回）；②拖拽端点跟随指针所在格成形跨格选区；③覆盖
    /// 输入 = 合并首尾格 + 写入 typed（一个动作）；④退格 = 同样的合并
    /// 删除、中间格清空、结构不动；⑤粘贴 = 先合并删除、再按 `|` 分发。
    #[gpui::test]
    fn live_table_cross_cell_selection_merge_spec6(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // 0:表头 1:分隔 2:数据 3:数据 4:空 5:尾段
        const DOC: &str = "| aa | bb |\n|---|---|\n| cc | dd |\n| ee | ff |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let line_count_before = view.read_with(cx, |v, _| v.buffer.line_count());

        /// 某格的内容（去 padding）。`ci` 用探测列定位：0 → 第一格，
        /// 其余 → 最后一格（映射兜底取末格），对重排后的宽度不敏感。
        fn cell_text(v: &LiveMarkdownView, line: usize, ci: usize) -> String {
            let text = v.buffer.line(line).unwrap_or("").to_string();
            let (cs, ce) = v
                .table_cell_content_at(line, if ci == 0 { 0 } else { 999 })
                .expect("该行应是表格格");
            let chars: Vec<char> = text.chars().collect();
            chars
                .get(cs..ce)
                .map(|s| s.iter().collect())
                .unwrap_or_default()
        }
        /// 某格的内容起点 char 列（空格 = 去 padding 后的空位）。
        fn cell_cs(v: &LiveMarkdownView, line: usize, ci: usize) -> usize {
            v.table_cell_content_at(line, if ci == 0 { 0 } else { 999 })
                .expect("该行应是表格格")
                .0
        }
        /// 拖出跨格选区：锚 = `(la, ca)` 格、终点 = `(lb, cb)` 格。
        fn drag_range(
            host: &gpui::Entity<Host>,
            cx: &mut gpui::TestAppContext,
            la: usize,
            ca: usize,
            lb: usize,
            cb: usize,
        ) {
            let view = host.read_with(cx, |h, _| h.0.clone());
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(la, ca, false));
                v.after_move(cx);
                v.selecting = true;
                v.on_table_cell_drag(lb, cb, cx);
            });
            cx.refresh().unwrap();
            cx.run_until_parked();
        }

        // 1) 点非活动格：元素级认领 → 光标落进被点的格（line2 cell1），
        //    而不是被活动格（cell0）的窗口监听夹回。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let nb = cx
            .debug_bounds("live-table-cell-2-1")
            .expect("非活动格应有探针与命中框");
        cx.simulate_click(nb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(line, 2, "点击应落在被点的数据行");
        let cs1 = view.read_with(cx, |v, _| cell_cs(v, 2, 1));
        let ce1 = view.read_with(cx, |v, _| {
            v.table_cell_content_at(2, 999).map(|(_, e)| e).unwrap()
        });
        assert!(
            (cs1..=ce1).contains(&col),
            "光标应落在被点的非活动格内容内（col={col}，格=[{cs1},{ce1}]）"
        );

        // 2) 覆盖输入：拖 (2, cs0+1) → (3, cell1 内容首)，输入 `z`
        //    = 合并首尾格 + typed 写进合并点。
        let prefix_col = view.read_with(cx, |v, _| cell_cs(v, 2, 0) + 1); // 前缀 = "c"
        let end_col = view.read_with(cx, |v, _| cell_cs(v, 3, 1)); // 后缀 = "ff"
        drag_range(&host, cx, 2, prefix_col, 3, end_col);
        let sel = view.read_with(cx, |v, _| v.buffer.selection());
        assert_eq!(
            sel,
            Some((Pos::new(2, prefix_col), Pos::new(3, end_col))),
            "拖拽端点应跟随指针所在格（跨格选区成形）"
        );
        cx.simulate_input("z");
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 0)),
            "czff",
            "覆盖输入：首格 ← 前缀 + typed + 尾格后缀"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 1)),
            "",
            "同排后格应清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 0)),
            "",
            "尾格前的格应清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 1)),
            "",
            "尾格应清空"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!(
            (c.line, c.col),
            (2, view.read_with(cx, |v, _| cell_cs(v, 2, 0)) + 2), // "c" + "z" 之后
            "光标应落在首格合并点"
        );
        assert!(view.read_with(cx, |v, _| v.buffer.selection()).is_none());

        // 3) 退格：表头里拖同排跨格选区 → 合并删除（首格保留前缀 +
        //    尾格后缀），数据行与结构不动。
        let h0 = view.read_with(cx, |v, _| cell_cs(v, 0, 0) + 1); // 前缀 = "a"
        let h1 = view.read_with(cx, |v, _| cell_cs(v, 0, 1)); // 后缀 = "bb"
        drag_range(&host, cx, 0, h0, 0, h1);
        cx.simulate_keystrokes("backspace");
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 0, 0)),
            "abb",
            "退格合并：首格 ← 前缀 + 尾格后缀"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 0, 1)),
            "",
            "同排尾格应清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 0)),
            "czff",
            "选区外的数据行不受影响"
        );

        // 4) 粘贴：跨格选区 + 含 `|` 的文本 = 先合并删除，再从首格分发。
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("P|Q".to_string()));
        let p0 = view.read_with(cx, |v, _| cell_cs(v, 2, 0));
        let p1 = view.read_with(cx, |v, _| cell_cs(v, 3, 1));
        drag_range(&host, cx, 2, p0, 3, p1);
        cx.simulate_keystrokes("cmd-v");
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 0)),
            "P",
            "粘贴分发：首格 ← 第一列"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 1)),
            "Q",
            "粘贴分发：同排次格 ← 第二列"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 0)),
            "",
            "选区内尾行应已清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 1)),
            "",
            "选区内尾行应已清空"
        );

        // 5) 结构始终完好：行数不变、表格块范围不变（分隔行仍是结构行）。
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            line_count_before,
            "合并/分发都不得增删行"
        );
        let lines: Vec<Line> = view.read_with(cx, |v, _| v.buffer.lines().to_vec());
        assert_eq!(
            crate::editor::markdown_live::table_ops::table_block_range(&lines, 0),
            Some((0, 3)),
            "表格块范围（含分隔行）应保持不变"
        );
    }

    /// spec #14：工具栏 = 尺寸弹层（行 2..=99 / 列 1..=40 步进器）+ 对齐 ×3 +
    /// More▾（与右键菜单同款条目）；点工具栏不挪光标；行把手点击落到本行首格
    /// 内容起点；光标一动弹层即收起。
    #[gpui::test]
    fn live_table_toolbar_resize_and_gutter_spec14(cx: &mut gpui::TestAppContext) {
        use crate::editor::markdown_live::table_ops as t;
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
            // 生产入口（main.rs）注册的组件按键绑定：PopupMenu 的
            // escape→Cancel / enter→Confirm / ↑↓ 都在这儿，不注册则
            // 菜单吃不到 Esc（测试 setup 默认不跑 components::init）。
            crate::components::init(cx);
        });
        const DOC: &str = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 0) 光标在表内 → 新三件套可见，旧平铺按钮已移除，把手骨架存在。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-table-toolbar-resize").is_some(),
            "尺寸按钮应在"
        );
        assert!(
            cx.debug_bounds("live-table-toolbar-more").is_some(),
            "More▾ 按钮应在"
        );
        assert!(
            cx.debug_bounds("live-table-toolbar-重排").is_none(),
            "旧「重排」平铺按钮已移入菜单"
        );
        assert!(
            cx.debug_bounds("live-table-toolbar-+行").is_none(),
            "旧「+行」平铺按钮已移除"
        );
        assert!(
            cx.debug_bounds("live-table-row-gutter-2").is_some(),
            "行把手应在"
        );
        assert!(
            cx.debug_bounds("live-table-col-gutter-1").is_some(),
            "列把手条应在"
        );

        // 1) 点「尺寸」展开弹层，且不挪光标（外层「点表格跳块首」被工具栏拦下）。
        let b = cx
            .debug_bounds("live-table-toolbar-resize")
            .expect("尺寸按钮 bounds");
        cx.simulate_click(b.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-table-resize-panel").is_some(),
            "尺寸弹层应展开"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 6), "点工具栏不得挪动光标");

        // 2) 行步进器：+1 → 多一行；−1 → 回到原样（弹层保持展开）。
        let rows_before = view.read_with(cx, |v, _| v.buffer.line_count());
        let pb = cx
            .debug_bounds("live-table-resize-row-plus")
            .expect("行 + 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before + 1,
            "行步进器 +1 应新增一行"
        );
        assert!(
            cx.debug_bounds("live-table-resize-panel").is_some(),
            "步进后弹层保持展开"
        );
        let pb = cx
            .debug_bounds("live-table-resize-row-minus")
            .expect("行 − 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            rows_before,
            "行步进器 −1 应删掉刚加的行"
        );

        // 3) 列步进器：+1 → 列数 +1；−1 → 回到原样。
        let counts = |v: &LiveMarkdownView| -> (usize, usize) {
            let lines = v.buffer.lines().to_vec();
            let cur = v.buffer.cursor();
            let Some(block) = t::table_block_range(&lines, cur.line) else {
                return (0, 0);
            };
            (
                t::table_data_rows(&lines, block).len(),
                t::table_col_count(&lines, block),
            )
        };
        let (_, c0) = view.read_with(cx, |v, _| counts(v));
        let pb = cx
            .debug_bounds("live-table-resize-col-plus")
            .expect("列 + 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (_, c1) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(c1, c0 + 1, "列步进器 +1 应新增一列");
        let pb = cx
            .debug_bounds("live-table-resize-col-minus")
            .expect("列 − 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (_, c2) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(c2, c0, "列步进器 −1 应删掉刚加的列");

        // 4) 上限由 `set_*_count` 夹紧（行 ≤99、列 ≤40）：直接顶到上限后，
        // 再点 + 也不再变化（spec #14：99 行 / 40 列）。
        view.update(cx, |v, cx| {
            v.run_table_op(cx, |m| t::set_row_count(m, 500));
        });
        cx.run_until_parked();
        let (r, _) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(r, 99, "行数上限 99");
        let pb = cx
            .debug_bounds("live-table-resize-row-plus")
            .expect("行 + 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (r, _) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(r, 99, "顶到 99 行后再点 + 应无变化");
        view.update(cx, |v, cx| {
            v.run_table_op(cx, |m| t::set_row_count(m, 3));
        });
        cx.run_until_parked();
        view.update(cx, |v, cx| {
            v.run_table_op(cx, |m| t::set_col_count(m, 500));
        });
        cx.run_until_parked();
        let (_, c) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(c, 40, "列数上限 40");
        let pb = cx
            .debug_bounds("live-table-resize-col-plus")
            .expect("列 + 按钮 bounds");
        cx.simulate_click(pb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (_, c) = view.read_with(cx, |v, _| counts(v));
        assert_eq!(c, 40, "顶到 40 列后再点 + 应无变化");
        view.update(cx, |v, cx| {
            v.run_table_op(cx, |m| t::set_col_count(m, 2));
        });
        cx.run_until_parked();

        // 5) 光标一动（↓）→ 弹层收起。
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("live-table-resize-panel").is_none(),
            "光标移动后弹层应收起"
        );

        // 6) 行把手点击 → 光标落到本行首格内容起点（`| 1 …` → col 2）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 6, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let gb = cx
            .debug_bounds("live-table-row-gutter-2")
            .expect("行把手 bounds");
        cx.simulate_click(gb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 2), "行把手点击 = 本行首格内容起点");

        // 7) More▾ → 手动打开表格菜单（与右键菜单共用条目），光标不动；
        //    Esc 关闭。
        let mb = cx
            .debug_bounds("live-table-toolbar-more")
            .expect("More▾ 按钮 bounds");
        cx.simulate_click(mb.center(), gpui::Modifiers::none());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |v, _| v.table_menu.is_some()),
            "More▾ 应打开表格菜单"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (2, 2), "点 More▾ 不得挪动光标");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |v, _| v.table_menu.is_none()),
            "Esc 应关闭菜单"
        );
        // 关闭后焦点还回编辑器（否则焦点悬空在菜单句柄上，键入失联）。
        let view_focus = view.read_with(cx, |v, _| v.focus_handle());
        cx.update(|window, _cx| {
            assert!(view_focus.is_focused(window), "菜单关闭后焦点应还给编辑器");
        });
    }

    /// 表格里方向键不应把光标停在管道或格间空格上（Typora 语义：只有格
    /// 内容是可停靠位置）。
    #[gpui::test]
    fn live_table_arrow_keys_skip_pipes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // `| foo | bar |`：内容分别在第 2..5 与 8..11 列。
        const DOC: &str = "| foo | bar |\n|---|---|\n| baz | qux |\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        // 光标放到第一格内容末尾（`foo` 之后，col 5）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 5, false));
            v.after_move(cx);
        });
        // 再按 Right：源码上会走到 col 6（空格）或 7（管道），必须被夹回
        // 某个格内容里（要么本格末，要么下一格内容首）。
        for _ in 0..3 {
            cx.update(|_, cx| {
                view.update(cx, |v, cx| {
                    v.edit(cx, |m| m.move_right(false));
                    v.clamp_cursor_off_table_pipe(cx);
                    v.after_move(cx);
                });
            });
        }
        let (line, col) = view.read_with(cx, |v, _| {
            let c = v.buffer.cursor();
            (c.line, c.col)
        });
        assert_eq!(line, 0, "Right 不该跨行（line={line}）");
        assert!(
            (2..=5).contains(&col) || (8..=11).contains(&col),
            "光标必须停在某个格内容里，不能停在管道/空格上（col={col}）"
        );
        // Left 同理：从第二格内容首往回退，不能停在管道上。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 8, false));
            for _ in 0..3 {
                v.edit(cx, |m| m.move_left(false));
                v.clamp_cursor_off_table_pipe(cx);
            }
            v.after_move(cx);
        });
        let col = view.read_with(cx, |v, _| v.buffer.cursor().col);
        assert!(
            (2..=5).contains(&col) || (8..=11).contains(&col),
            "Left 后光标必须停在格内容里（col={col}）"
        );
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
            Host(cx.new(|cx| LiveMarkdownView::with_text("hello\n", cx).doc_path(doc.clone())))
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
                "DESIGN",
                include_str!("../../../docs/DESIGN_LSP_REMOTE.md"),
                true,
                true,
            ),
            (
                "GHOSTTY_REVIEW",
                include_str!("../../../docs/GHOSTTY_1.4_DEEPTERM_REVIEW_2026-11-02.md"),
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
            let (host, cx) =
                cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx))));
            let view = host.read_with(cx, |h, _| h.0.clone());
            // Large docs virtualize rows: presence must be checked with the
            // block scrolled into the rendered window (jump the cursor onto
            // it); absence is decided at classification level (a grid/card
            // cannot render for rows that were never classified as such).
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(&doc);
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
                div().size_full().flex().flex_col().child(
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
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(SAMPLE, cx))));
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
mod live_caret_animation_tests {
    use super::*;

    const SAMPLE: &str = "# Title\n\npara one\n\npara two\n\n```rust\nfn main() {}\n```\n";

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// 开一个测试窗口、激活并聚焦（测试窗口默认不活跃，光标不会绘制）。
    fn setup_with(
        cx: &mut gpui::TestAppContext,
        text: &str,
    ) -> (gpui::Entity<LiveMarkdownView>, gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(gpui::size(px(800.), px(600.)), |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(text, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.update(|window, cx| {
            window.activate_window();
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        (view, vc)
    }

    /// 推进一帧：消费 `on_next_frame` 回调 → 推进模拟时钟 → 重绘。
    ///
    /// 动画的续帧靠 `on_next_frame`，测试里必须显式 `simulate_next_frame`
    /// 消费（`refresh` 只标脏、不跑绘制）。
    fn tick(vc: &mut gpui::VisualTestContext) {
        vc.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        vc.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        vc.refresh().unwrap();
        vc.run_until_parked();
    }

    /// 光标移动应产生弹簧动画并收敛。
    #[gpui::test]
    fn caret_move_animates_and_settles(cx: &mut gpui::TestAppContext) {
        let (view, mut vc) = setup_with(cx, SAMPLE);
        tick(&mut vc);

        // 首帧：光标就位、无动画。
        assert!(
            !view.read_with(&vc, |v, _| v.cursor_animator.is_animating()),
            "首帧不应处于动画中"
        );

        // 光标下移到下一段（跨行）：应起动画。
        vc.update(|_, cx| {
            view.update(cx, |v, cx| {
                v.edit(cx, |m| m.move_to(2, 0, false));
            });
        });
        tick(&mut vc);
        assert!(
            view.read_with(&vc, |v, _| v.cursor_animator.is_animating()),
            "光标移动应产生动画"
        );

        // 推进到收敛。
        let mut frames = 0;
        for _ in 0..300 {
            if !view.read_with(&vc, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
            tick(&mut vc);
            frames += 1;
        }
        assert!(frames > 1, "应有多帧中间态，实际 {frames} 帧");
        assert!(
            !view.read_with(&vc, |v, _| v.cursor_animator.is_animating()),
            "动画应收敛"
        );
    }

    /// 滚动不应产生光标动画（光标应钉在内容上）。
    #[gpui::test]
    fn caret_scroll_does_not_animate(cx: &mut gpui::TestAppContext) {
        // 长文档才滚得动。
        let long: String = (0..200)
            .map(|i| format!("paragraph number {i} with some filler text\n\n"))
            .collect();
        let (view, mut vc) = setup_with(cx, &long);

        // 就位并收敛。
        for _ in 0..300 {
            tick(&mut vc);
            if !view.read_with(&vc, |v, _| v.cursor_animator.is_animating()) {
                break;
            }
        }

        let before = view.read_with(&vc, |v, _| v.cursor_animator.last_quad());

        // 只滚动，不移动光标。
        vc.update(|_, cx| {
            view.update(cx, |v, cx| {
                let cur = v.scroll.offset();
                v.scroll.set_offset(gpui::point(cur.x, cur.y - px(120.0)));
                cx.notify();
            });
        });
        tick(&mut vc);

        assert!(
            !view.read_with(&vc, |v, _| v.cursor_animator.is_animating()),
            "滚动不应产生光标动画（光标应钉在内容上）"
        );

        // 滚动量必须真的被动画器看到：四边形应整体上移 120px。
        let after = view.read_with(&vc, |v, _| v.cursor_animator.last_quad());
        match (before, after) {
            (Some(b), Some(a)) => {
                let dy = a.corners[0].1 - b.corners[0].1;
                assert!(
                    (dy - (-120.0)).abs() < 2.0,
                    "滚动应把光标整体平移 -120px，实际 {dy}（before={:?} after={:?}）",
                    b.corners[0],
                    a.corners[0]
                );
            }
            other => panic!("两侧都应有四边形: {other:?}"),
        }
    }
}

#[cfg(test)]
mod live_showcase_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_document;

    const SHOWCASE: &str = include_str!("../../../docs/markdown-showcase.md");

    /// The exact user repro: plan the showcase doc and report every code /
    /// table row whose visible text is empty or suspiciously short.
    #[test]
    fn showcase_code_and_table_rows_have_visible_text() {
        let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(SHOWCASE);
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
        assert!(code_count > 10, "showcase must contain plenty of code rows");
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
        let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(&doc);
        let rows: Vec<Rc<PlannedRow>> = plan_document(
            &lines,
            Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: false,
                read_only: false,
                ..Default::default()
            },
        )
        .into_iter()
        .map(Rc::new)
        .collect();
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
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(&doc);
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
                        "code" => matches!(row.block, Block::Fence { .. } | Block::CodeLine),
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

    /// 人工验收用：宽表 + 结构图在窄栏 / 全宽两种状态下的对照截图。
    ///
    /// `cargo test --lib live_wide_and_graph_screenshot -- --ignored --nocapture`
    const SHOT_WIDE: &str = "\
## 宽表（12 列，窄栏里应能横向滚动）

| 环境 | 区域 | 实例 | 规格 | 镜像 | 状态 | 负责人 | 创建时间 | 到期时间 | 成本 | 标签 | 备注 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| prod | cn-north | i-0a1b2c3d | 8C16G | registry/app:1.4.2 | running | 张三 | 2026-01-04 | 2026-12-31 | ¥1,240 | team-a | 主力 |
| stage | cn-south | i-4e5f6a7b | 4C8G | registry/app:1.4.2-rc1 | stopped | 李四 | 2026-02-11 | 2026-06-30 | ¥420 | team-b | 灰度 |
| dev | cn-east | i-8c9d0e1f | 2C4G | registry/app:dev | running | 王五 | 2026-03-02 | — | ¥96 | team-c | 临时 |

## 结构图（flowchart，窄栏里节点会挤）

```mermaid
flowchart TD
    A[\"改 my_policy/policy.py<br/>或 v1-SKILL.md\"] --> B
    B[\"探活端点<br/>probe_endpoint.py\"] --> C{模型可用?}
    C -->|\"否\"| B1[\"换端点 / 等配额<br/>改 .env.local\"]
    B1 --> B
    C -->|\"是\"| D[\"离线自检<br/>test_my_policy.py\"] --> E{通过?}
    E -->|\"否\"| A
    E -->|\"是\"| F[\"构建 pack<br/>trajectoryrl-miner build\"] --> G[\"同步到服务器\"]
    G --> H[\"真场景评测<br/>eval_local.py\"] --> I{分数提升?}
    I -->|\"否\"| K[\"看 transcript<br/>定位 agent 死因\"]
    I -->|\"是\"| L[\"保留 · 更新 README\"]
    K --> A
```

尾段文字，用于确认正文仍居中在阅读窄栏里。
";

    #[test]
    #[ignore]
    fn live_wide_and_graph_screenshot() {
        let mut cx = gpui::VisualTestAppContext::new(gpui_platform::current_platform(false));
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let window = cx
            .open_offscreen_window(gpui::size(px(1200.), px(900.)), |_, cx| {
                let v = cx.new(|cx| LiveMarkdownView::with_text(SHOT_WIDE, cx));
                *cell.borrow_mut() = Some(v.clone());
                cx.new(|_| ShotHost(v))
            })
            .unwrap();
        let view = cell.into_inner().unwrap();
        let handle: gpui::AnyWindowHandle = window.into();
        // 等 mermaid 后台渲染：防抖 200ms 走假时钟，必须推进；渲染完成的
        // 判据取自视图状态本身（`MermaidState::body`），不依赖窗口内部字段。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            cx.update_window(handle, |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
            ready = cx.read(|cx| {
                view.read(cx)
                    .planned_geometry
                    .mermaid_blocks
                    .iter()
                    .any(|b| {
                        matches!(
                            view.read(cx).mermaid.body(&b.source),
                            MermaidBody::Image { pending: false, .. }
                        )
                    })
            });
            if ready {
                break;
            }
        }
        eprintln!("mermaid ready: {ready}");
        cx.update_window(handle, |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        let shot = cx.capture_screenshot(handle).unwrap();
        shot.save("/tmp/live_wide_narrow.png").unwrap();
        eprintln!("saved /tmp/live_wide_narrow.png");

        // 切全宽：表格首行 = 2（标题 + 空行），mermaid 围栏行 = 14。
        cx.update(|cx| {
            view.update(cx, |v, cx| {
                v.toggle_wide_block(2, cx);
                v.toggle_wide_block(14, cx);
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        let shot = cx.capture_screenshot(handle).unwrap();
        shot.save("/tmp/live_wide_full.png").unwrap();
        eprintln!("saved /tmp/live_wide_full.png");

        // 文档级全宽（状态条「全宽」按钮）：全部行铺满，状态条同样铺满，
        // 表格工具栏与 mermaid 卡片的单块「全宽」按钮隐藏。
        cx.update(|cx| {
            view.update(cx, |v, cx| {
                v.toggle_wide_block(2, cx);
                v.toggle_wide_block(14, cx);
                v.toggle_wide_doc(cx);
            });
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        let shot = cx.capture_screenshot(handle).unwrap();
        shot.save("/tmp/live_doc_wide.png").unwrap();
        eprintln!("saved /tmp/live_doc_wide.png");
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

    /// Editing a **tall container's later rows** must not scroll the view to
    /// the container's top.
    ///
    /// A table grid / code card / quote run is ONE rendered element holding
    /// many rows. GPUI's `ScrollHandle::scroll_to_item` pins an oversized
    /// child's top edge to the viewport top, so a caret on the last table row
    /// used to yank the view up to the table header — pushing the row being
    /// edited off-screen and making it permanently unreachable.
    ///
    /// This asserts the caret's own row stays visible (and that the view did
    /// not jump to the block's top) for a table taller than the viewport.
    #[gpui::test]
    /// Every container taller than the viewport must leave the scroll alone
    /// when the user clicks a row that is already on screen.
    ///
    /// `scroll_to_item` targets the caret's whole *group*, and GPUI resolves a
    /// group taller than the viewport by pinning its **top edge** to the
    /// viewport top. So a click on a late row of any such container used to
    /// yank the view back to the container's first row — pushing the clicked
    /// row off screen and making it uneditable. The correction pass only
    /// rescues the case where the clicked row is still painted; once it falls
    /// out of the virtualization window the view stays stuck at the top.
    ///
    /// The group kinds that can exceed the viewport are exactly
    /// [`Group::Table`] (grid), [`Group::Fence`] (code card), [`Group::Quote`]
    /// and [`Group::Para`] (unbroken runs); [`Group::One`] is a single row and
    /// cannot. This covers all four.
    #[gpui::test]
    fn clicking_visible_row_of_any_tall_container_does_not_scroll(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        // Each fixture: (label, document, first line of the tall container).
        // 60 rows/lines is comfortably taller than the 300px window.
        let mut table = String::from("intro paragraph\n\n| col a | col b |\n|---|---|\n");
        for i in 0..40 {
            table.push_str(&format!("| row {i} | value {i} |\n"));
        }
        table.push_str("\nafter\n");

        let mut fence = String::from("intro paragraph\n\n```rust\n");
        for i in 0..60 {
            fence.push_str(&format!("let x{i} = {i};\n"));
        }
        fence.push_str("```\n\nafter\n");

        // One quote run and one unbroken paragraph run: no blank lines inside,
        // so each collapses into a single group.
        let mut quote = String::from("intro paragraph\n\n");
        for i in 0..60 {
            quote.push_str(&format!("> quoted line {i}\n"));
        }
        quote.push_str("\nafter\n");

        let mut para = String::from("intro paragraph\n\n");
        for i in 0..60 {
            para.push_str(&format!("paragraph line {i} with some prose text\n"));
        }
        para.push_str("\nafter\n");

        let mut failures: Vec<String> = Vec::new();
        for (label, doc) in [
            ("table", table),
            ("code card", fence),
            ("quote run", quote),
            ("paragraph run", para),
        ] {
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
            vc.update(|window, cx| {
                window.focus(&view.read(cx).focus_handle(), cx);
            });

            // Park the caret on line 0, then scroll well past the container's
            // top so its first line is off screen — the state the bug needs.
            view.update(&mut vc, |v, cx| {
                v.edit(cx, |m| m.move_to(0, 0, false));
            });
            vc.refresh().unwrap();
            vc.run_until_parked();
            view.update(&mut vc, |v, _| {
                v.scroll.set_offset(gpui::point(px(0.), px(-900.)));
            });
            for _ in 0..2 {
                vc.refresh().unwrap();
                vc.run_until_parked();
            }

            // Precondition for the bug: the caret's own row is off screen, so
            // the "already visible → do not scroll" guard does *not* suppress
            // the element-level scroll. (A container's individual rows may not
            // have a recorded layout — a code card paints as one element — so
            // this is checked on the caret row, which always has one.)
            let (offset_before, guard) =
                view.read_with(&vc, |v, _| (v.scroll_offset_y(), v.caret_row_in_view()));
            assert_eq!(
                guard,
                Some(false),
                "{label}: setup: the caret row must be off screen so the guard does not \
                 suppress the scroll (offset {offset_before:.1})"
            );

            // Click a window point inside the viewport (and therefore over the
            // visible part of the tall container).
            let vp_win = view.read_with(&vc, |v, _| v.scroll.bounds());
            let click_at = gpui::point(
                vp_win.origin.x + px(100.),
                vp_win.origin.y + vp_win.size.height / 2.0,
            );
            let cursor_before = view.read_with(&vc, |v, _| v.buffer.cursor().line);
            vc.simulate_click(click_at, gpui::Modifiers::none());
            for _ in 0..6 {
                vc.refresh().unwrap();
                vc.run_until_parked();
            }
            let (cursor_after, offset_after) =
                view.read_with(&vc, |v, _| (v.buffer.cursor().line, v.scroll_offset_y()));

            assert_ne!(
                cursor_after, cursor_before,
                "{label}: setup: the click must move the caret into the container"
            );
            if (offset_after - offset_before).abs() > 1.0 {
                failures.push(format!(
                    "{label}: click moved the view: offset {offset_before:.1} -> \
                     {offset_after:.1} (caret line {cursor_before} -> {cursor_after})"
                ));
            }

            // ...and typing the first character must not scroll either. This is
            // the same requirement one frame later: a keystroke re-arms
            // `scroll_to_cursor`, and if the clicked row's rect was never
            // recorded (a code card paints as one element, so its inner rows
            // may have no layout entry) the guard degrades to "unknown" and the
            // element-level scroll fires again — jumping to the container top
            // exactly when the user starts editing.
            view.update(&mut vc, |v, cx| {
                v.insert_text_for_bench("x", cx);
            });
            for _ in 0..6 {
                vc.refresh().unwrap();
                vc.run_until_parked();
            }
            let offset_typed = view.read_with(&vc, |v, _| v.scroll_offset_y());
            if (offset_typed - offset_after).abs() > 1.0 {
                failures.push(format!(
                    "{label}: typing after the click moved the view: offset \
                     {offset_after:.1} -> {offset_typed:.1}"
                ));
            }
        }

        // Report every failing container, not just the first: they exercise
        // different group kinds and can regress independently.
        assert!(
            failures.is_empty(),
            "clicking an already-visible row must not scroll the view, but these \
             containers moved:\n  {}",
            failures.join("\n  ")
        );
    }

    /// Clicking a visible row of a tall table must not scroll the view.
    ///
    /// The reported bug: with the table's header scrolled out of view, clicking
    /// a late row scrolls the view to the container's **first** row (GPUI's
    /// element-level `scroll_to_item` pins a tall child's top edge to the
    /// viewport top). That pushes the clicked row off-screen, so the user can
    /// never edit the data they clicked on. Code cards and every other
    /// container taller than the viewport behave the same way.
    #[gpui::test]
    fn clicking_visible_row_of_tall_table_does_not_scroll(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::from("intro paragraph\n\n| col a | col b |\n|---|---|\n");
        for i in 0..40 {
            doc.push_str(&format!("| row {i} | value {i} |\n"));
        }
        doc.push_str("\nafter\n");

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
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });

        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 0, false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        view.update(&mut vc, |v, _| {
            v.scroll.set_offset(gpui::point(px(0.), px(-900.)));
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }

        let (offset_before, vp_h, header_top) = view.read_with(&vc, |v, _| {
            (
                v.scroll_offset_y(),
                f32::from(v.scroll.bounds().size.height),
                v.row_layouts.get(&2).map(|i| i.doc_top).unwrap_or(f32::MAX),
            )
        });
        let vp_top_before = -offset_before;
        assert!(
            header_top < vp_top_before,
            "setup: the table header (top {header_top:.1}) must be scrolled out of \
             view (viewport top {vp_top_before:.1})"
        );

        let table = vc
            .debug_bounds("live-table-outer")
            .expect("table grid must be rendered");
        let vp_win = view.read_with(&vc, |v, _| v.scroll.bounds());
        let click_at = gpui::point(
            vp_win.origin.x + px(100.),
            vp_win.origin.y + vp_win.size.height / 2.0,
        );
        let (click_y, tbl_top, tbl_bottom) = (
            f32::from(click_at.y),
            f32::from(table.origin.y),
            f32::from(table.origin.y + table.size.height),
        );
        assert!(
            click_y >= tbl_top && click_y <= tbl_bottom,
            "the click point must be over the table grid (y {click_y:.1}, \
             table {tbl_top:.1}..{tbl_bottom:.1})"
        );

        let cursor_before = view.read_with(&vc, |v, _| v.buffer.cursor().line);
        vc.simulate_click(click_at, gpui::Modifiers::none());
        for _ in 0..6 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let (cursor_after, offset_after) =
            view.read_with(&vc, |v, _| (v.buffer.cursor().line, v.scroll_offset_y()));

        assert_ne!(
            cursor_after, cursor_before,
            "setup: the click must actually move the caret into the table"
        );
        assert!(
            (offset_after - offset_before).abs() <= 1.0,
            "clicking a visible row of a tall table must not scroll the view: \
             offset {offset_before:.1} -> {offset_after:.1} (caret line {cursor_before} -> \
             {cursor_after}, viewport height {vp_h:.1})"
        );
    }

    #[gpui::test]
    fn tall_table_last_row_stays_visible(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // A table with many rows: the grid is far taller than the 300px window.
        let mut doc = String::from("intro paragraph\n\n");
        doc.push_str("| col a | col b |\n|---|---|\n");
        for i in 0..40 {
            doc.push_str(&format!("| row {i} | value {i} |\n"));
        }
        doc.push_str("\nafter\n");

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
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });

        // Put the caret on the table's LAST data row (line index: 2 intro
        // lines + 2 header lines + 40 rows → the last row is line 43).
        let last_row = 2 + 2 + 39;
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(last_row, 3, false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        // The caret row must be inside the viewport.
        let (offset, vp_h, row_top, row_bottom) = view.read_with(&vc, |v, _| {
            let line = v.buffer.cursor().line;
            let info = v
                .row_layouts
                .get(&line)
                .unwrap_or_else(|| panic!("caret row {line} has no recorded layout"));
            (
                v.scroll_offset_y(),
                f32::from(v.scroll.bounds().size.height),
                info.doc_top,
                info.doc_bottom,
            )
        });
        let vp_top = -offset;
        let vp_bottom = vp_top + vp_h;
        assert!(
            row_top >= vp_top - 1.0 && row_bottom <= vp_bottom + 1.0,
            "caret row must be visible: row {row_top:.1}..{row_bottom:.1} \
             viewport {vp_top:.1}..{vp_bottom:.1}"
        );
        // And the view must NOT have jumped to the table's first line: the
        // header (line 2) must be above the viewport top.
        let header_bottom = view.read_with(&vc, |v, _| {
            v.row_layouts
                .get(&2)
                .map(|i| i.doc_bottom)
                .unwrap_or(f32::MAX)
        });
        assert!(
            header_bottom <= vp_top + 1.0,
            "view must not scroll back to the table header \
             (header bottom {header_bottom:.1} vs viewport top {vp_top:.1})"
        );
    }

    /// Same guarantee for a code card: editing a late code line must not jump
    /// the view to the card's first line.
    #[gpui::test]
    fn tall_code_card_late_line_stays_visible(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::from("intro\n\n```rust\n");
        for i in 0..60 {
            doc.push_str(&format!("let x{i} = {i};\n"));
        }
        doc.push_str("```\n\nafter\n");

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
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });

        // A late code line (inside the card).
        let late = 2 + 1 + 55;
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(late, 3, false));
        });
        vc.refresh().unwrap();
        vc.run_until_parked();

        let (offset, vp_h, row_top, row_bottom) = view.read_with(&vc, |v, _| {
            let line = v.buffer.cursor().line;
            let info = v
                .row_layouts
                .get(&line)
                .unwrap_or_else(|| panic!("caret row {line} has no recorded layout"));
            (
                v.scroll_offset_y(),
                f32::from(v.scroll.bounds().size.height),
                info.doc_top,
                info.doc_bottom,
            )
        });
        let vp_top = -offset;
        let vp_bottom = vp_top + vp_h;
        assert!(
            row_top >= vp_top - 1.0 && row_bottom <= vp_bottom + 1.0,
            "caret row must be visible: row {row_top:.1}..{row_bottom:.1} \
             viewport {vp_top:.1}..{vp_bottom:.1}"
        );
        let first_code_bottom = view.read_with(&vc, |v, _| {
            v.row_layouts
                .get(&3)
                .map(|i| i.doc_bottom)
                .unwrap_or(f32::MAX)
        });
        assert!(
            first_code_bottom <= vp_top + 1.0,
            "view must not scroll back to the card's first line \
             (first line bottom {first_code_bottom:.1} vs viewport top {vp_top:.1})"
        );
    }

    /// The same guarantee for the remaining multi-row containers: a tall
    /// blockquote run and a tall paragraph run. Both are one element holding
    /// many rows, so the element-level scroll would pin their first row to the
    /// viewport top and push a late row's caret off-screen.
    #[gpui::test]
    fn tall_quote_and_paragraph_late_rows_stay_visible(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        // `(label, doc, caret line)` — each container is far taller than the
        // 300px window.
        let mut quote = String::from("intro\n\n");
        for i in 0..40 {
            quote.push_str(&format!("> quoted line {i}\n"));
        }
        quote.push_str("\nafter\n");

        // A single paragraph run (no blank lines) is one `Group::Para`.
        let mut para = String::from("intro\n\n");
        for i in 0..40 {
            para.push_str(&format!("paragraph line {i}\n"));
        }
        para.push_str("\nafter\n");

        for (label, doc, caret_line) in [("quote", quote, 2 + 39), ("paragraph", para, 2 + 39)] {
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
            vc.update(|window, cx| {
                window.focus(&view.read(cx).focus_handle(), cx);
            });

            view.update(&mut vc, |v, cx| {
                v.edit(cx, |m| m.move_to(caret_line, 2, false));
            });
            vc.refresh().unwrap();
            vc.run_until_parked();

            let (offset, vp_h, row_top, row_bottom) = view.read_with(&vc, |v, _| {
                let line = v.buffer.cursor().line;
                let info = v
                    .row_layouts
                    .get(&line)
                    .unwrap_or_else(|| panic!("{label}: caret row {line} has no layout"));
                (
                    v.scroll_offset_y(),
                    f32::from(v.scroll.bounds().size.height),
                    info.doc_top,
                    info.doc_bottom,
                )
            });
            let vp_top = -offset;
            let vp_bottom = vp_top + vp_h;
            assert!(
                row_top >= vp_top - 1.0 && row_bottom <= vp_bottom + 1.0,
                "{label}: caret row must be visible: row {row_top:.1}..{row_bottom:.1} \
                 viewport {vp_top:.1}..{vp_bottom:.1}"
            );
            // The container's first row must be scrolled past (not pinned to
            // the viewport top).
            let first_row_bottom = view.read_with(&vc, |v, _| {
                v.row_layouts
                    .get(&2)
                    .map(|i| i.doc_bottom)
                    .unwrap_or(f32::MAX)
            });
            assert!(
                first_row_bottom <= vp_top + 1.0,
                "{label}: view must not scroll back to the container's first row \
                 (first row bottom {first_row_bottom:.1} vs viewport top {vp_top:.1})"
            );
        }
    }

    /// The "caret row already visible → do not scroll at all" fast path must
    /// actually engage, and must be reachable when the scroll decision is made.
    ///
    /// `caret_row_in_view` used to read `row_layouts`, which `render` clears at
    /// its top and only repopulates during paint — so the lookup always returned
    /// `None`, the guard was dead, and the element-level `scroll_to_item` (the
    /// very call that yanks a tall container's top edge to the viewport top)
    /// ran on every caret frame. The rect now comes from the previous frame, so
    /// this asserts the state the guard depends on is genuinely available at the
    /// point of use, not just that the end position is right.
    #[gpui::test]
    fn caret_row_rect_is_available_for_the_scroll_decision(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        // Tall table: a single element containing many rows.
        let mut doc = String::from("intro paragraph\n\n| col a | col b |\n|---|---|\n");
        for i in 0..40 {
            doc.push_str(&format!("| row {i} | value {i} |\n"));
        }
        doc.push_str("\nafter\n");

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
        vc.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });

        // Put the caret on a middle table row and let the scroll settle.
        let caret_line = 2 + 2 + 20;
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(caret_line, 3, false));
        });
        for _ in 0..6 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }

        // The retained rect must exist, match the caret row, and be stamped
        // with the current text version — otherwise the guard degrades to
        // `None` and the fast path is dead again.
        //
        // Crucially this is asserted with `row_layouts` **cleared**, which is
        // the state `render` leaves it in when the scroll decision is made
        // (`render` empties it at the top; `LiveLine::paint` refills it later
        // in the same frame). Asserting from outside a frame without clearing
        // would observe the post-paint map and pass even with the dead lookup.
        let (rect, in_view) = view.update(&mut vc, |v, _| {
            v.row_layouts.clear();
            (v.caret_row_rect, v.caret_row_in_view())
        });
        let (_line, _version, doc_top, doc_bottom) =
            rect.expect("caret row rect must be retained across the per-frame clear");
        assert!(
            doc_bottom > doc_top,
            "retained rect must be a real measurement ({doc_top}..{doc_bottom})"
        );
        assert_eq!(
            in_view,
            Some(true),
            "with `row_layouts` cleared (the state at the scroll decision), the guard \
             must still report caret row {caret_line} visible from the retained rect \
             {doc_top:.1}..{doc_bottom:.1} — `Some(true)` is what suppresses the \
             element-level scroll on the next typing frame"
        );

        // The guard must survive an intervening edit. Keying it on the text
        // version made every keystroke invalidate the rect, so typing in a tall
        // container re-armed `scroll_to_item` on each character and jumped to
        // the container's top edge — the same jump-to-header bug as clicking,
        // from the keyboard. Staleness is handled by the correction pass, which
        // re-measures and nudges minimally.
        // Deliberately NO refresh between the edit and the check: `render` makes
        // this decision *before* paint, so at that moment the text version has
        // already advanced while `caret_row_rect` still carries the previous
        // frame's stamp. That is precisely the state a typing frame sees.
        let (stale_version, in_view_after_edit) = view.update(&mut vc, |v, cx| {
            v.insert_text_for_bench("y", cx);
            v.row_layouts.clear();
            let stale = v.caret_row_rect.map(|(_, ver, _, _)| ver) != Some(v.buffer.text_version());
            (stale, v.caret_row_in_view())
        });
        assert!(
            stale_version,
            "setup: the rect must still carry the pre-edit version at this point"
        );
        assert_eq!(
            in_view_after_edit,
            Some(true),
            "an edit that did not move the caret must not invalidate the rect: the \
             guard has to stay `Some(true)` so typing does not re-arm the scroll"
        );

        // Typing one character must not move the view at all: the caret row is
        // visible, so the fast path suppresses `scroll_to_item`.
        let before = view.read_with(&vc, |v, _| v.scroll_offset_y());
        view.update(&mut vc, |v, cx| {
            v.insert_text_for_bench("x", cx);
        });
        for _ in 0..6 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let after = view.read_with(&vc, |v, _| v.scroll_offset_y());
        assert!(
            (after - before).abs() <= 1.0,
            "typing in a visible tall container must not scroll: {before:.1} -> {after:.1}"
        );
    }

    /// Starting to edit must not scroll the view, for every tall container.
    ///
    /// This is the keyboard half of the reported bug. A keystroke goes through
    /// `edit`, which arms `scroll_to_cursor`; the render loop then resolves it
    /// with the element-level `scroll_to_item`, and GPUI pins a group taller
    /// than the viewport by its **top edge** — so the first character typed on
    /// a late row of a tall table/code card/quote/paragraph jumped the view
    /// back to the container's first row, off the row being edited. Typing on
    /// an already-visible row must move nothing at all.
    #[gpui::test]
    fn typing_in_visible_tall_container_does_not_scroll(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });

        let mut table = String::from("intro paragraph\n\n| col a | col b |\n|---|---|\n");
        for i in 0..40 {
            table.push_str(&format!("| row {i} | value {i} |\n"));
        }
        table.push_str("\nafter\n");

        let mut fence = String::from("intro paragraph\n\n```rust\n");
        for i in 0..60 {
            fence.push_str(&format!("let x{i} = {i};\n"));
        }
        fence.push_str("```\n\nafter\n");

        let mut quote = String::from("intro paragraph\n\n");
        for i in 0..60 {
            quote.push_str(&format!("> quoted line {i}\n"));
        }
        quote.push_str("\nafter\n");

        let mut para = String::from("intro paragraph\n\n");
        for i in 0..60 {
            para.push_str(&format!("paragraph line {i} with some prose text\n"));
        }
        para.push_str("\nafter\n");

        let mut failures: Vec<String> = Vec::new();
        for (label, doc, caret_line) in [
            ("table", table, 2 + 2 + 30usize),
            ("code card", fence, 2 + 1 + 40),
            ("quote run", quote, 2 + 40),
            ("paragraph run", para, 2 + 40),
        ] {
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
            vc.update(|window, cx| {
                window.focus(&view.read(cx).focus_handle(), cx);
            });

            // Put the caret on a late row of the container and let the scroll
            // settle, then confirm the row is on screen and the container's
            // first line is not.
            view.update(&mut vc, |v, cx| {
                v.edit(cx, |m| m.move_to(caret_line, 2, false));
            });
            for _ in 0..4 {
                vc.refresh().unwrap();
                vc.run_until_parked();
            }
            let (offset_before, guard) =
                view.read_with(&vc, |v, _| (v.scroll_offset_y(), v.caret_row_in_view()));
            assert_eq!(
                guard,
                Some(true),
                "{label}: setup: the caret row must be visible before typing"
            );

            // The decision that matters is made in `render`, *before* paint:
            // `scroll_to_item` runs only when this guard is not `Some(true)`.
            // Check it immediately after the edit, with no repaint in between —
            // that is exactly the state a typing frame sees. (Sampling the
            // offset alone cannot detect the defect: the element-level scroll
            // pins the container's top edge for one prepaint and the deferred
            // correction pass restores it inside the same refresh, so the
            // observable offset barely moves. In a large document the
            // correction cannot recover at all — the jumped-away row is
            // virtualized out, so `row_layouts` has no entry and the view stays
            // stuck at the container top.)
            let guard_during_typing = view.update(&mut vc, |v, cx| {
                v.insert_text_for_bench("x", cx);
                v.caret_row_in_view()
            });
            if guard_during_typing != Some(true) {
                failures.push(format!(
                    "{label}: typing re-armed the element-level scroll (guard \
                     {guard_during_typing:?} instead of Some(true)), which pins the \
                     container's top edge to the viewport top (caret line {caret_line})"
                ));
            }
            // ...and the view must stay put across every frame of the repaint.
            let mut worst = 0.0f32;
            for _ in 0..6 {
                vc.refresh().unwrap();
                vc.run_until_parked();
                let off = view.read_with(&vc, |v, _| v.scroll_offset_y());
                worst = worst.max((off - offset_before).abs());
            }
            if worst > 1.0 {
                failures.push(format!(
                    "{label}: typing moved the view by up to {worst:.1}px \
                     (settled at {offset_before:.1}, caret line {caret_line})"
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "typing on a visible row must not scroll the view, but these containers \
             moved:\n  {}",
            failures.join("\n  ")
        );
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
        let snap =
            |view: &gpui::Entity<LiveMarkdownView>, vc: &mut gpui::VisualTestContext| -> Snap {
                view.read_with(vc, |v, _| {
                    let off = f32::from(v.scroll.offset().y);
                    Snap {
                        pos_a: v.virt_prefix.get(ga).copied().unwrap_or(-99999.0) + off,
                        pos_b: v.virt_prefix.get(gb).copied().unwrap_or(-99999.0) + off,
                    }
                })
            };
        let force_frame = |view: &gpui::Entity<LiveMarkdownView>,
                           vc: &mut gpui::VisualTestContext| {
            view.update(vc, |_, cx| cx.notify());
            vc.refresh().unwrap();
            vc.run_until_parked();
        };
        let assert_glued = |prev: &Snap, s: &Snap, ctx: &str| {
            assert!(
                (s.pos_a - prev.pos_a).abs() <= 1.0 && (s.pos_b - prev.pos_b).abs() <= 1.0,
                "content jumped with no input at {ctx}: posA {:.1}->{:.1} posB {:.1}->{:.1}",
                prev.pos_a,
                s.pos_a,
                prev.pos_b,
                s.pos_b
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

    /// Dragging inside one table cell must track the pointer, even though the
    /// table's toolbar appears as the gesture starts.
    ///
    /// The toolbar is laid out at the table's **top**, so revealing it (which
    /// happens as soon as the cursor enters the table) makes the table taller
    /// and slides every row down. When that happens between the press and the
    /// drag moves, the rows move out from under the pointer: the endpoint lands
    /// on the row above and the drag spans two rows — reported as "the caret
    /// doesn't follow the mouse, it selects a whole table row".
    #[gpui::test]
    fn dragging_in_a_cell_tracks_the_pointer_despite_the_toolbar(cx: &mut gpui::TestAppContext) {
        const D: &str = "intro paragraph above\n\n| alpha bravo charlie | second cell |\n|---|---|\n| delta echo foxtrot | golf hotel |\n| india juliet | kilo lima |\n\ntail\n";
        let (view, mut vc) = setup(cx, D);
        // Park the caret OUTSIDE the table so the toolbar starts hidden.
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 3, false));
            v.after_move(cx);
        });
        vc.refresh().unwrap();
        vc.run_until_parked();

        // `di` is the `grid_rows` index: 0 = header, 1 = delimiter,
        // 2 = source line 4, 3 = source line 5 (the last data row).
        let cell = vc
            .debug_bounds("live-table-cell-3-0")
            .expect("the last data row's cell must render");
        let y = cell.center().y;
        let x0 = cell.left() + px(30.);
        vc.simulate_mouse_down(
            point(x0, y),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vc.run_until_parked();
        let pressed = view.read_with(&vc, |v, _| v.buffer.cursor());
        assert_eq!(
            pressed.line, 5,
            "setup: the press must land on the intended row, got {pressed:?}"
        );

        // Drag rightwards across the same cell, sampling the endpoint.
        let mut ends = Vec::new();
        for step in 1..=6 {
            let x = x0 + px(step as f32 * 15.);
            vc.simulate_mouse_move(
                point(x, y),
                gpui::MouseButton::Left,
                gpui::Modifiers::default(),
            );
            vc.run_until_parked();
            let (cursor, sel) =
                view.read_with(&vc, |v, _| (v.buffer.cursor(), v.buffer.selection()));
            // Sample the selection's *end* when there is one (the drag's moving
            // end), else the caret.
            ends.push((step, sel.map(|(_, e)| e).unwrap_or(cursor)));
        }
        for (step, end) in &ends {
            assert_eq!(
                end.line, 5,
                "step{step}: the drag endpoint must stay on the pressed row, got {end:?} \
                 (all samples: {ends:?})"
            );
        }
        let cols: Vec<usize> = ends.iter().map(|(_, e)| e.col).collect();
        assert!(
            cols.windows(2).all(|w| w[0] <= w[1]) && cols.last() > cols.first(),
            "the endpoint must follow the pointer across the cell text, got {cols:?}"
        );
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
        let cursor = |vc: &gpui::VisualTestContext| view.read_with(vc, |v, _| v.buffer.cursor());
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
        assert!(para.size.height > px(45.), "two lines of text: {para:?}");
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
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
mod live_ime_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    /// 建视图 + 聚焦 + 首帧（输入处理器注册与命中记录都发生在 paint）。
    fn setup<'a>(
        cx: &'a mut gpui::TestAppContext,
        doc: &str,
    ) -> (
        gpui::Entity<LiveMarkdownView>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        (view, cx)
    }

    /// 某格内容文本（去 padding）。
    fn cell_text(v: &LiveMarkdownView, line: usize, ci: usize) -> String {
        let text = v.buffer.line(line).unwrap_or("").to_string();
        let (cs, ce) = cell_content_cols(&text, ci).expect("该行应是表格行");
        text.chars().skip(cs).take(ce - cs).collect()
    }

    /// 中文 IME 全链路：拼音组字两帧 + 提交中文 —— 组字区间（UTF-16）与
    /// 光标逐帧跟随，提交后标记清除、光标在提交文本之后。
    #[gpui::test]
    fn live_ime_composition_and_commit_cjk(cx: &mut gpui::TestAppContext) {
        let (view, cx) = setup(cx, "héllo\n");
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 5, false)); // 行尾（byte 6）
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        // 第 1 帧：平台无显式区间，插入拼音 `n`，相对插入文本的选区 1..1。
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "héllon\n",
            "组字文本应入 buffer"
        );
        let marked =
            cx.update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx)));
        assert_eq!(marked, Some(5..6), "组字区间 = 刚插入的 n（UTF-16）");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 6), "光标应在组字文本末尾");

        // 第 2 帧：显式区间（旧标记 5..6）替换为整段拼音 `nihao`。
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(Some(5..6), "nihao", Some(5..5), window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |v, _| v.text()), "héllonihao\n");
        let marked =
            cx.update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx)));
        assert_eq!(marked, Some(5..10), "组字区间跟随整段拼音");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 10), "光标在拼音末尾");

        // 提交：显式区间 → 中文；标记清除，光标在提交文本后。
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(Some(5..10), "你好", window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "héllo你好\n",
            "提交文本应替换拼音"
        );
        let marked =
            cx.update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx)));
        assert_eq!(marked, None, "提交后无组字标记");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 7), "光标在提交文本之后");
    }

    /// 跨行 UTF-16：`中` 占 1 个 UTF-16 单元，偏移 2 应落在换行后的 `文` 前。
    #[gpui::test]
    fn live_ime_utf16_multiline_commit(cx: &mut gpui::TestAppContext) {
        let (view, cx) = setup(cx, "中\n文");
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(Some(2..2), "好", window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |v, _| v.text()), "中\n好文");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!(
            (c.line, c.col),
            (1, 1),
            "跨行 UTF-16 定位：换行后第 1 个字符"
        );
    }

    /// 未聚焦 / 只读：IME 写路径静默丢弃（不落文本、不动选区）。
    #[gpui::test]
    fn live_ime_unfocused_and_read_only_drop(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        const DOC: &str = "abc\n";
        // 不聚焦：连处理器注册都不该认领。
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(DOC, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |v, _| v.input_registered),
            "未聚焦不得认领输入处理器注册权"
        );
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(None, "x", window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |v, _| v.text()), DOC, "未聚焦不得写入");

        // 聚焦 + 只读：处理器注册但写路径丢弃。
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |v, _| v.input_registered),
            "聚焦后光标行应认领注册权"
        );
        view.update(cx, |v, cx| v.set_read_only(true, cx));
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(None, "x", window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |v, _| v.text()), DOC, "只读不得写入");
    }

    /// 跨格选区 + IME 写路径（等价 `simulate_input("z")` 的平台回退）：
    /// 合并首尾格、typed 写进合并点，与 spec #6 同款结果。
    #[gpui::test]
    fn live_ime_cross_cell_merge_via_handler(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "| aa | bb |\n|---|---|\n| cc | dd |\n| ee | ff |\n\ntail\n";
        let (view, cx) = setup(cx, DOC);
        // 拖出跨格选区：(2, cell0 内容首 + 1) → (3, cell1 内容首)。
        let (a_col, b_col) = view.read_with(cx, |v, _| {
            let l2 = v.buffer.line(2).unwrap().to_string();
            let l3 = v.buffer.line(3).unwrap().to_string();
            (
                cell_content_cols(&l2, 0).unwrap().0 + 1,
                cell_content_cols(&l3, 1).unwrap().0,
            )
        });
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, a_col, false));
            v.after_move(cx);
            v.selecting = true;
            v.on_table_cell_drag(3, b_col, cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(None, "z", window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 0)),
            "czff",
            "覆盖输入：首格 ← 前缀 + typed + 尾格后缀"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 2, 1)),
            "",
            "同排后格应清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 0)),
            "",
            "尾格前的格应清空"
        );
        assert_eq!(
            view.read_with(cx, |v, _| cell_text(v, 3, 1)),
            "",
            "尾格应清空"
        );
    }

    /// 代码块里 Tab / Shift+Tab：普通 Tab 缩进一次（不得经 handler 再插一遍
    /// 制表符）；Shift+Tab 无操作但**必须**停传播（否则平台回退把 "\t" 当
    /// 可打印文本插入）。同时覆盖 Enter 不双插换行。
    #[gpui::test]
    fn live_ime_code_block_tab_and_enter_no_double_write(cx: &mut gpui::TestAppContext) {
        let (view, cx) = setup(cx, "```\ncode\n```\n");
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(1, 0, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.simulate_keystrokes("tab");
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line(1).unwrap().to_string()),
            "    code",
            "代码块 Tab 只缩进一次（on_key 消费，handler 不得再插）"
        );

        // Shift+Tab：代码行无操作（on_tab 的 outdent 对非列表不缩进）。
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line(1).unwrap().to_string()),
            "    code",
            "代码块 Shift+Tab 无操作，且不得插入字面制表符"
        );

        // Enter 换行一次（on_key 的 enter 分支消费，handler 不得再插 \n）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(1, 8, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let lines_before = view.read_with(cx, |v, _| v.buffer.line_count());
        cx.simulate_keystrokes("enter");
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            lines_before + 1,
            "Enter 只换一行（不得双插）"
        );
    }

    /// 空提交 = 取消组字：删掉标记文本、标记清除、光标塌缩回起点。
    #[gpui::test]
    fn live_ime_empty_commit_cancels_marked_text(cx: &mut gpui::TestAppContext) {
        let (view, cx) = setup(cx, "héllo\n");
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 5, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |v, _| v.text()), "héllon\n");

        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(Some(5..6), "", None, window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "héllo\n",
            "取消组字应删掉标记文本"
        );
        let marked =
            cx.update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx)));
        assert_eq!(marked, None, "取消后无组字标记");
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 5), "光标塌缩回组字起点");
    }

    /// ASCII 输入走同一条写路径且只写一次；单字符自动配对仍生效。
    #[gpui::test]
    fn live_ime_ascii_typing_single_write_with_autopair(cx: &mut gpui::TestAppContext) {
        let (view, cx) = setup(cx, "ab\n");
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.simulate_input("x");
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "abx\n",
            "ASCII 应经 handler 单次写入（on_key 不再直写）"
        );
        cx.simulate_input("(");
        assert_eq!(
            view.read_with(cx, |v, _| v.text()),
            "abx()\n",
            "handler 路径的单字符自动配对仍生效"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 4), "光标应在配对括号内");
    }

    /// 表格内组字不触发自动重排（组字区间按字节记账），提交后恢复重排。
    #[gpui::test]
    fn live_ime_table_composition_defers_prettify(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (view, cx) = setup(cx, DOC);
        let cs = view.read_with(cx, |v, _| {
            let l = v.buffer.line(2).unwrap().to_string();
            cell_content_cols(&l, 0).unwrap().0
        });
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, cs, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line(0).unwrap().to_string()),
            "| a | b |",
            "组字期间不得重排表格（区间会错位）"
        );

        let marked = cx
            .update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx)))
            .expect("应有组字区间");
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_text_in_range(Some(marked), "你", window, cx);
            });
        });
        let line0 = view.read_with(cx, |v, _| v.buffer.line(0).unwrap().to_string());
        assert!(
            line0.len() > "| a | b |".len(),
            "提交后应恢复自动重排：{line0:?}"
        );
        let line2 = view.read_with(cx, |v, _| v.buffer.line(2).unwrap().to_string());
        assert!(line2.contains('你'), "提交文本应在格内：{line2:?}");
    }

    /// IME 锚点：普通行 `bounds_for_range` 有行高；格内点映射回该格内容。
    #[gpui::test]
    fn live_ime_bounds_and_point_mapping(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "hello\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (view, cx) = setup(cx, DOC);
        let fallback = gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(10.), px(10.)));
        let b = cx
            .update(|window, cx| {
                view.update(cx, |v, cx| v.bounds_for_range(0..0, fallback, window, cx))
            })
            .expect("普通行应有 bounds");
        assert!(b.size.height > px(0.), "bounds 应有行高：{b:?}");

        // 表格第 2 格内的一点 → 应映射到该格内容的 UTF-16 区间内。
        let nb = cx.debug_bounds("live-table-cell-2-1").expect("第 2 格探针");
        let idx = cx
            .update(|window, cx| {
                view.update(cx, |v, cx| {
                    v.character_index_for_point(nb.center(), window, cx)
                })
            })
            .expect("格内点应可映射");
        let expect = view.read_with(cx, |v, _| {
            let doc = v.text();
            let l = v.buffer.line(4).unwrap().to_string();
            let (cs, ce) = cell_content_cols(&l, 1).unwrap();
            let sb = LiveMarkdownView::offset_of_pos(v.buffer.lines(), Pos::new(4, cs));
            let eb = LiveMarkdownView::offset_of_pos(v.buffer.lines(), Pos::new(4, ce));
            (
                LiveMarkdownView::byte_to_utf16(&doc, sb),
                LiveMarkdownView::byte_to_utf16(&doc, eb),
            )
        });
        assert!(
            (expect.0..=expect.1).contains(&idx),
            "格内点应映射到该格内容：idx={idx}，期望 {expect:?}"
        );
    }
}

#[cfg(test)]
mod live_table_regression_tests {
    use super::*;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    fn setup<'a>(
        cx: &'a mut gpui::TestAppContext,
        doc: &str,
    ) -> (
        gpui::Entity<LiveMarkdownView>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus_handle(), cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        (view, cx)
    }

    /// 回归（评审发现）：表格**外** Cmd+A 必须选中整篇文档 —— 修复前
    /// `select_table_cell_contents` 没有表格守卫，`table_cell_content_at`
    /// 对普通行也返回整行范围，于是 Cmd+A 只选当前行并吞掉按键。
    #[gpui::test]
    fn live_cmd_a_outside_table_selects_document(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "第一段\n\n第二段\n";
        let (view, cx) = setup(cx, DOC);
        // 光标放在第一段中间（非表格行）。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        cx.simulate_keystrokes("cmd-a");
        cx.run_until_parked();
        let sel = view.read_with(cx, |v, _| v.selected_source_text());
        assert_eq!(
            sel.as_deref(),
            Some(DOC),
            "表格外 Cmd+A 应选中整篇文档（不得只选当前行）"
        );
    }

    /// 回归：表格外 Cmd+E 不得吞键（表内才拦截选格）。
    #[gpui::test]
    fn live_cmd_e_outside_table_not_consumed(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "第一段\n\n第二段\n";
        let (view, cx) = setup(cx, DOC);
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(0, 2, false));
            v.after_move(cx);
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-e");
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |v, _| v.buffer.selection()).is_none(),
            "表格外 Cmd+E 不得产生整行选区"
        );
        let c = view.read_with(cx, |v, _| v.buffer.cursor());
        assert_eq!((c.line, c.col), (0, 2), "光标不应被移动");
    }

    /// 回归（评审发现）：紧邻表格前、只是恰好含 `|` 的段落行不得被表格
    /// 结构操作改写 —— 修复前块范围按「行内含 `|`」扩张，删列会把该段
    /// 落当成表格行切割。
    #[gpui::test]
    fn live_table_op_does_not_rewrite_pipe_paragraph(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "用 `a | b` 表示或运算\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let (view, cx) = setup(cx, DOC);
        // 光标进表格数据行 → 删除本列。
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(3, 4, false));
            v.after_move(cx);
        });
        view.update(cx, |v, cx| {
            v.run_table_menu_op(TableMenuOp::DeleteColumn, cx)
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line(0).unwrap().to_string()),
            "用 `a | b` 表示或运算",
            "段落行必须原样保留（不得被当成表格行改写）"
        );
        assert_eq!(
            view.read_with(cx, |v, _| {
                let l = v.buffer.line(1).unwrap().to_string();
                split_table_row(&l)
            }),
            vec!["b"],
            "表头应只删掉被删列（宽度由自动重排补齐）"
        );
    }

    /// 回归（评审发现）：文件末尾无换行时「删除本行」不得静默无操作。
    #[gpui::test]
    fn live_table_delete_row_at_eof_without_newline(cx: &mut gpui::TestAppContext) {
        const DOC: &str = "| a | b |\n|---|---|\n| 1 | 2 |";
        let (view, cx) = setup(cx, DOC);
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 4, false));
            v.after_move(cx);
        });
        view.update(cx, |v, cx| v.run_table_menu_op(TableMenuOp::DeleteRow, cx));
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.buffer.line_count()),
            2,
            "末行（文件无尾换行）应被真正删除"
        );
        let kept = view.read_with(cx, |v, _| v.text());
        assert!(kept.contains("a"), "表头应保留：{kept:?}");
        assert!(!kept.contains('1'), "被删行不得残留：{kept:?}");
    }
}

#[cfg(test)]
mod setext_parity_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::Reveal;
    use crate::editor::markdown_live::reveal::plan_document;

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
                            pulldown_ranges
                                .push((line_of(s), line_of(range.end.saturating_sub(1))));
                        }
                    }
                    _ => {}
                }
            }
            let lines: Vec<Line> = crate::editor::markdown_live::model::lines_from_text(&doc);
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
            vc.debug_bounds("live-code-inner")
                .is_some_and(|b| b.size.height > px(20.)),
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
        assert!(
            line >= 10,
            "blank click lands near the end, got line {line}"
        );
    }
}

#[cfg(test)]
mod perf_diagnostics {
    use super::*;
    use crate::editor::markdown_live::reveal::classify_document;
    use std::time::Instant;

    /// 诊断（非断言）：按文档规模拆解「每次击键」各阶段耗时，用于定位
    /// 大文档下打字变慢的残余 O(文档) 环节。
    ///
    /// 需要显式运行：`cargo test --lib typing_phase_breakdown -- --ignored --nocapture`
    ///
    /// 判读（release 口径，20k 行）：
    /// - `plan` / `classify` 是**全量**口径，只用于对照；
    /// - `incr_classify` 是击键实际走的增量分类，应与文档规模基本无关；
    /// - `geometry` 是几何全量重建（仅在块结构变化时发生）。
    #[test]
    #[ignore = "diagnostic: run with --ignored --nocapture"]
    fn typing_phase_breakdown() {
        for lines in [1_000usize, 5_000, 20_000] {
            let mut doc = String::new();
            let mut i = 0;
            while i < lines {
                doc.push_str(&format!("# H{i}\n\npara {i} **b** `c`\n\n"));
                i += 3;
            }
            let mut buf = LiveBuffer::new(&doc);
            let classified = classify_document(buf.lines());
            let refs = refdefs_for_lines(buf.lines(), &classified.blocks);
            let reveal = Reveal {
                cursor: Pos::new(0, 0),
                selection: None,
                focused: true,
                read_only: false,
                show_source: false,
            };

            let t = Instant::now();
            for _ in 0..10 {
                let _ = buf.text();
            }
            let join_ms = t.elapsed().as_secs_f64() * 100.0;

            let t = Instant::now();
            let _ = classify_document(buf.lines());
            let classify_ms = t.elapsed().as_secs_f64() * 1000.0;

            let t = Instant::now();
            let _ = crate::editor::markdown_live::reveal::plan_rows_with_refs(
                buf.lines(),
                &classified,
                &refs,
                reveal,
            );
            let plan_ms = t.elapsed().as_secs_f64() * 1000.0;

            let t = Instant::now();
            buf.edit(|m| m.insert("x"));
            let edit_ms = t.elapsed().as_secs_f64() * 1000.0;

            let planned: Vec<Rc<PlannedRow>> =
                crate::editor::markdown_live::reveal::plan_rows_with_refs(
                    buf.lines(),
                    &classified,
                    &refs,
                    reveal,
                )
                .into_iter()
                .map(Rc::new)
                .collect();
            let t = Instant::now();
            let _ = PlanGeometry::build_owned(&planned, buf.lines());
            let geo_ms = t.elapsed().as_secs_f64() * 1000.0;

            // The path a keystroke actually takes: incremental classification
            // from the model's tracked dirty range.
            let t = Instant::now();
            let (_, repro) = crate::editor::markdown_live::reveal::classify_incremental(
                buf.lines(),
                Some(&classified),
                buf.dirty_range().unwrap_or(0..0),
            );
            let incr_classify_ms = t.elapsed().as_secs_f64() * 1000.0;

            println!(
                "lines={lines:6} join={join_ms:7.3}ms full_classify={classify_ms:7.3}ms \
                 full_plan={plan_ms:8.3}ms edit={edit_ms:6.3}ms \
                 geometry={geo_ms:7.3}ms incr_classify={incr_classify_ms:6.3}ms \
                 (reproc={repro})"
            );
        }
    }
}

#[cfg(test)]
mod incremental_plan_tests {
    use super::*;
    use crate::editor::markdown_live::reveal::plan_rows_with_refs;

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    /// A document exercising every construct whose block kind depends on
    /// neighbouring lines (the reason the dirty range must be widened):
    /// setext underlines, GFM tables with and without outer pipes, fences,
    /// `$$` math runs, frontmatter, callouts, quotes and ordered lists.
    ///
    /// It is deliberately **longer than `CLASSIFY_CHECKPOINT`** (padded with
    /// prose runs) so the incremental classifier's checkpoint-resume path is
    /// actually entered. The original fixture was ~30 lines, i.e. below the
    /// checkpoint granularity, which meant every edit took the full-pass path
    /// and the resume logic was never exercised by this differential test.
    fn tricky_doc() -> String {
        let mut d = String::new();
        d.push_str("---\ntitle: x\n---\n\n");
        // Prose runs long enough to straddle checkpoint boundaries.
        for i in 0..60 {
            d.push_str(&format!("prose line {i} of a paragraph run\n"));
            if i % 4 == 3 {
                d.push('\n');
            }
        }
        d.push_str("Setext title\n===\n\n");
        d.push_str("multi line\nsetext run\n---\n\n");
        d.push_str("```rust\nfn a() {}\n```\n\n");
        d.push_str("| h1 | h2 |\n|---|---|\n| a | b |\n\n");
        d.push_str("no pipes a | b\n--- | ---\n1 | 2\n\n");
        d.push_str("$$\nx = 1\n$$\n\n");
        d.push_str("> [!NOTE]\n> card body\n> more\n\n");
        d.push_str("> plain quote\n\n");
        d.push_str("1. one\n2. two\n\n");
        d.push_str("tail paragraph\n");
        d
    }

    /// Build the expected plan from scratch for the view's current state.
    ///
    /// The classification and refdefs are recomputed with a genuine full pass
    /// ([`classify_document`]) rather than reused from `v.planned_classified`.
    /// That reuse made this oracle blind by construction: it compared the
    /// incremental plan against a plan built from the *same* incrementally
    /// derived classification, so any error in the classification itself
    /// cancelled out and the differential test stayed green.
    fn full_plan(v: &LiveMarkdownView, focused: bool) -> Vec<PlannedRow> {
        let last = v.buffer.lines().len().saturating_sub(1);
        let (cursor, selection) = if v.show_source {
            (
                v.buffer.cursor(),
                Some((Pos::new(0, 0), Pos::new(last, usize::MAX))),
            )
        } else {
            (v.buffer.cursor(), v.buffer.selection())
        };
        let reveal = Reveal {
            cursor,
            selection,
            focused,
            read_only: v.read_only,
            show_source: v.show_source,
        };
        let lines = v.buffer.lines();
        let fresh = crate::editor::markdown_live::reveal::classify_document(lines);
        let refs = crate::editor::markdown_live::reveal::refdefs_for_lines(lines, &fresh.blocks);
        let mut expected = plan_rows_with_refs(lines, &fresh, &refs, reveal);
        for row in &mut expected {
            strip_callout_prefix(row, v.buffer.line(row.line).unwrap_or(""));
        }
        expected
    }

    /// Compare the view's incrementally-maintained plan against a from-scratch
    /// plan for the same text and reveal state, and the cached geometry
    /// against a from-scratch build.
    fn assert_matches_full(v: &mut LiveMarkdownView, round: usize, what: &str) {
        let incremental = v.planned_rows(false);
        let expected = full_plan(v, false);
        // The view's own classification is maintained incrementally, so it must
        // be compared against the full pass explicitly — otherwise a stale
        // classification would show up only as a mysterious plan difference
        // (or not at all, if the oracle reuses it).
        let truth = crate::editor::markdown_live::reveal::classify_document(v.buffer.lines());
        assert_eq!(
            v.planned_classified.blocks, truth.blocks,
            "classification diverged from a full pass after round {round} edit {what:?}"
        );
        assert_eq!(
            v.planned_classified.lang_before, truth.lang_before,
            "classification lang_before diverged after round {round} edit {what:?}"
        );
        // Geometry is cached separately and gated on "did the inputs change";
        // a wrong gate would render stale grouping/ordered numbers/mermaid
        // blocks while the plan itself looked right.
        let expected_geo = PlanGeometry::build_owned(
            &expected.iter().cloned().map(Rc::new).collect::<Vec<_>>(),
            v.buffer.lines(),
        );
        assert_eq!(
            v.planned_geometry.ginfo, expected_geo.ginfo,
            "geometry ginfo diverged after round {round} edit {what:?}"
        );
        assert_eq!(
            v.planned_geometry.group_of_row, expected_geo.group_of_row,
            "geometry group_of_row diverged after round {round} edit {what:?}"
        );
        assert_eq!(
            v.planned_geometry.display_numbers, expected_geo.display_numbers,
            "geometry display_numbers diverged after round {round} edit {what:?}"
        );
        assert_eq!(
            v.planned_geometry.has_table, expected_geo.has_table,
            "geometry has_table diverged after round {round} edit {what:?}"
        );
        let got_mermaid: Vec<(usize, &str)> = v
            .planned_geometry
            .mermaid_blocks
            .iter()
            .map(|b| (b.block_id, b.source.as_ref()))
            .collect();
        let want_mermaid: Vec<(usize, &str)> = expected_geo
            .mermaid_blocks
            .iter()
            .map(|b| (b.block_id, b.source.as_ref()))
            .collect();
        assert_eq!(
            got_mermaid, want_mermaid,
            "geometry mermaid blocks diverged after round {round} edit {what:?}"
        );
        assert_eq!(
            incremental.len(),
            expected.len(),
            "row count diverged after round {round} edit {what:?}"
        );
        for (i, (got, want)) in incremental.iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                got.line, want.line,
                "row {i} line index diverged after round {round} edit {what:?}"
            );
            assert_eq!(
                got.block,
                want.block,
                "row {i} block diverged after round {round} edit {what:?}\n\
                 text: {:?}",
                v.buffer.line(got.line).unwrap_or("")
            );
            assert_eq!(
                got.plan,
                want.plan,
                "row {i} plan diverged after round {round} edit {what:?}\n\
                 text: {:?}",
                v.buffer.line(got.line).unwrap_or("")
            );
            assert_eq!(
                got.callout, want.callout,
                "row {i} callout diverged after round {round} edit {what:?}"
            );
        }
    }

    /// Editing a fence's *info string* changes every code line's
    /// `RowKind::Code { lang }` (which drives tree-sitter highlighting) even
    /// though those lines' `Block` stays `CodeLine`. The dirty range must
    /// therefore cover the whole fence, not just the edited fence line.
    #[gpui::test]
    fn fence_language_edit_replans_its_code_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let doc = "para\n\n```\nfn a() {}\nfn b() {}\n```\n\ntail\n";
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(doc, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        // Warm the plan.
        view.update(cx, |v, _| {
            v.planned_rows(false);
        });
        // Type the language into the opening fence (line 2).
        view.update(cx, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 3, false));
        });
        view.update(cx, |v, cx| {
            v.insert_text_for_bench("rust", cx);
        });
        view.update(cx, |v, _| {
            assert_matches_full(v, 0, "fence lang");
            // The code rows must carry the new language.
            let row = v
                .planned
                .iter()
                .find(|r| matches!(r.plan.kind, RowKind::Code { .. }))
                .expect("code row present");
            match &row.plan.kind {
                RowKind::Code { lang } => assert_eq!(
                    lang.as_deref(),
                    Some("rust"),
                    "code row kept a stale language"
                ),
                other => panic!("unexpected kind {other:?}"),
            }
        });
    }

    /// The incremental planner must produce *exactly* the same rows as a full
    /// re-plan, for an arbitrary edit stream.
    ///
    /// This is the correctness contract of the whole optimization: the frame
    /// path now reuses untouched rows, and any divergence (a stale block kind,
    /// a missed callout transition, a stale reveal flag) would show up here.
    /// The comparison is on the full `PlannedRow` (block, plan, callout), not
    /// just the rendered text.
    #[gpui::test]
    fn incremental_matches_full_plan(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            Host(cx.new(|cx| LiveMarkdownView::with_text(&tricky_doc(), cx)))
        });
        let view = host.read_with(cx, |h, _| h.0.clone());

        let edits: &[&str] = &[
            "x",
            "# ",
            "\n",
            "===\n",
            "| a | b |\n",
            "|---|---|\n",
            "```\n",
            "\n",
            "```\n",
            "rust", // fence info string: changes code rows' RowKind
            "```\n",
            "$$\n",
            "y = 2\n",
            "$$\n",
            "[!TIP]\n",
            "> [!WARNING]\n", // callout kind change inside a quote run
            "1. item\n",
            "    indented\n",
        ];

        for round in 0..3 {
            for text in edits {
                view.update(cx, |v, cx| {
                    let n = v.buffer.line_count();
                    // Sweep the target across a full checkpoint period so the
                    // resume path is entered at every offset, including the
                    // exact multiples of `CLASSIFY_CHECKPOINT` that used to
                    // freeze the line above the edit.
                    let target = (round * 7 + n / 3) % n.max(1);
                    v.edit(cx, |m| m.move_to(target, 0, false));
                    v.insert_text_for_bench(text, cx);
                    assert_matches_full(v, round, text);
                    // Pure cursor moves (reveal-only changes).
                    let n = v.buffer.line_count();
                    v.edit(cx, |m| m.move_to(n / 2, 0, false));
                    assert_matches_full(v, round, "<cursor>");
                });
            }
        }
    }

    /// Drive the view through **every line of a full checkpoint period**,
    /// including the exact multiples of `CLASSIFY_CHECKPOINT`.
    ///
    /// The general stream above samples a single target per round, so the
    /// boundary offsets were only hit by luck. This test walks the whole
    /// period with a fresh view per offset and asserts the rendered plan still
    /// matches a genuine full pass — the case that silently froze the line
    /// above the edit before the resume point was pulled back one checkpoint.
    ///
    /// Two details make this actually exercise the resume path:
    /// * each edit is a **pure in-line replacement** (select the line's
    ///   content, type over it). Inserting a `\n` dirties the range to
    ///   end-of-document and disables tail reuse entirely, which would hide
    ///   the bug;
    /// * the document is a single unbroken prose run, so the line *above*
    ///   every checkpoint multiple is a paragraph. A blank line there would
    ///   end the setext run and make the bug unreachable.
    #[gpui::test]
    fn checkpoint_boundary_edits_match_full_plan(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let period = crate::editor::markdown_live::reveal::CLASSIFY_CHECKPOINT;
        // One unbroken paragraph run (no blank lines) several periods long.
        let mut text = String::new();
        for i in 0..(period * 4) {
            text.push_str(&format!("prose line {i}\n"));
        }
        text.push_str("tail\n");
        for off in 0..period {
            let (host, cx) = cx
                .add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(&text, cx))));
            let view = host.read_with(cx, |h, _| h.0.clone());
            // Warm the cached plan/classification: a real screen has always
            // rendered before the user types, and `classify_incremental` only
            // takes the resume path when a previous classification exists.
            view.update(cx, |v, _| {
                let _ = v.planned_rows(false);
            });
            view.update(cx, |v, cx| {
                let n = v.buffer.line_count();
                // Land the edit on `off` modulo the checkpoint period. Start
                // past line 0 so the line above is an ordinary prose line.
                let target = period + off;
                if target >= n {
                    return;
                }
                let len = v
                    .buffer
                    .line(target)
                    .map(|l| l.chars().count())
                    .unwrap_or(0);
                v.edit(cx, |m| {
                    m.move_to(target, 0, false);
                    m.move_to(target, len, true);
                });
                v.insert_text_for_bench("===", cx);
                assert_matches_full(v, off, "checkpoint-boundary setext edit");
            });
        }
    }

    /// The model's dirty accumulator must stay bounded to the *current* edit.
    ///
    /// `mark_dirty` merges ranges monotonically and nothing else clears them,
    /// so before this was fixed two distant edits pinned the per-keystroke
    /// window at the span between them — eventually the whole document — which
    /// silently turned the incremental planner back into a full scan while
    /// every functional test still passed.
    #[gpui::test]
    fn dirty_accumulator_stays_bounded_across_distant_edits(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let mut doc = String::new();
        for i in 0..4000 {
            doc.push_str(&format!("paragraph line {i} with some text\n\n"));
        }
        let (host, cx) =
            cx.add_window_view(|_, cx| Host(cx.new(|cx| LiveMarkdownView::with_text(&doc, cx))));
        let view = host.read_with(cx, |h, _| h.0.clone());
        view.update(cx, |v, cx| {
            let _ = v.planned_rows(false);

            let mut spans = Vec::new();
            for &line in &[2usize, 7000, 6] {
                let n = v.buffer.line_count();
                let target = line.min(n - 2);
                v.edit(cx, |m| m.move_to(target, 3, false));
                v.insert_text_for_bench("A", cx);
                let _ = v.planned_rows(false);
                if let Some(r) = v.buffer.dirty_range() {
                    spans.push((target, r.end - r.start));
                }
            }
            // A settled plan consumes its marks; anything still pending must at
            // least be scoped to the edit that produced it, never to the union
            // of every edit so far.
            for (line, span) in &spans {
                assert!(
                    *span < 200,
                    "dirty range after the edit at line {line} spans {span} lines; \
                     the accumulator is not being consumed (spans so far: {spans:?})"
                );
            }
        });
    }
}

#[cfg(test)]
mod hscroll_and_wide_tests {
    use super::*;
    use crate::editor::markdown_live::scrollbar::{
        scroll_for_drag, table_natural_width, thumb_metrics,
    };
    use gpui::{point, size};

    struct Host(gpui::Entity<LiveMarkdownView>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    fn setup<'a>(
        cx: &'a mut gpui::TestAppContext,
        window: gpui::Size<Pixels>,
        doc: &str,
    ) -> (gpui::Entity<LiveMarkdownView>, gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let cell = std::cell::RefCell::new(None);
        let handle = cx.open_window(window, |_, cx| {
            let view = cx.new(|cx| LiveMarkdownView::with_text(doc, cx));
            *cell.borrow_mut() = Some(view.clone());
            Host(view)
        });
        let view = cell.into_inner().unwrap();
        let mut vc = gpui::VisualTestContext::from_window(handle.into(), cx);
        vc.refresh().unwrap();
        vc.run_until_parked();
        (view, vc)
    }

    /// thumb 几何：内容不溢出 → 无条；溢出 → 条长按比例、位置随偏移走。
    #[test]
    fn thumb_metrics_scale_and_follow_offset() {
        // 视口 1000、内容 1000：不溢出。
        assert!(thumb_metrics(px(1000.), px(1000.), px(0.)).is_none());
        // 内容 2000（可滚 1000）：条长 = 轨道一半。
        let (start, len) = thumb_metrics(px(1000.), px(2000.), px(0.)).expect("溢出应有条");
        assert_eq!(start, px(2.)); // TRACK_INSET
        assert!(
            (f32::from(len) - (1000.0 - 4.0) / 2.0).abs() < 1.0,
            "条长应约为轨道一半，实际 {len:?}"
        );
        // 滚到底：条贴轨道右端。
        let (start_bottom, _) =
            thumb_metrics(px(1000.), px(2000.), px(-1000.)).expect("溢出应有条");
        assert!(
            f32::from(start_bottom) > f32::from(start),
            "滚到底后条应右移（{start:?} → {start_bottom:?}）"
        );
        // 极长内容：条不小于 MIN_THUMB（抓得住）。
        let (_, tiny) = thumb_metrics(px(1000.), px(100_000.), px(0.)).expect("溢出应有条");
        assert!(f32::from(tiny) >= 28.0 - 0.01, "条长有下限，实际 {tiny:?}");
    }

    /// 窄视口：轨道比 MIN_THUMB 还短时条长压到轨道，不得 panic。
    ///
    /// `Pixels::clamp` 是 `Ord::clamp`（断言 `min <= max`），纵向条在矮分屏里
    /// 会走到这条路径。
    #[test]
    fn thumb_metrics_survives_track_shorter_than_min_thumb() {
        for viewport in [px(6.), px(20.), px(31.), px(34.)] {
            let (_, len) = thumb_metrics(viewport, px(2000.), px(0.))
                .unwrap_or_else(|| panic!("视口 {viewport:?} 溢出时应有条"));
            let track = viewport - px(4.);
            let min_thumb = px(28.).min(track);
            assert!(
                len >= min_thumb - px(0.01) && len <= track,
                "条长应落在 [min(MIN_THUMB, 轨道), 轨道]，视口 {viewport:?} 实际 {len:?}"
            );
        }
    }

    /// 拖动换算：thumb 位移按 `max_scroll / travel` 放大，并夹在 `[0, max]`。
    #[test]
    fn drag_maps_thumb_travel_to_scroll_range() {
        // 视口 1000、内容 2000、条长 500 → travel 496、max_scroll 1000。
        let half = scroll_for_drag(px(0.), px(248.), px(1000.), px(2000.), px(500.));
        assert!(
            (f32::from(half) - 500.0).abs() < 2.0,
            "拖到轨道中点应滚到一半，实际 {half:?}"
        );
        // 超程夹紧。
        assert_eq!(
            scroll_for_drag(px(0.), px(9999.), px(1000.), px(2000.), px(500.)),
            px(1000.)
        );
        assert_eq!(
            scroll_for_drag(px(100.), px(-9999.), px(1000.), px(2000.), px(500.)),
            px(0.)
        );
        // 不可滚：不动。
        assert_eq!(
            scroll_for_drag(px(0.), px(50.), px(1000.), px(1000.), px(500.)),
            px(0.)
        );
    }

    /// 表格自然宽度下界随列数增长（列多 → 下界大 → 更容易触发横向滚动）。
    #[test]
    fn table_natural_width_grows_with_columns() {
        assert!(table_natural_width(1) < table_natural_width(4));
        assert!(table_natural_width(4) < table_natural_width(12));
        // 单列也要有可读下界（不是 0）。
        assert!(f32::from(table_natural_width(1)) >= 96.0);
    }

    /// 窄窗口里的宽表：横向滚动容器存在、可滚（`max_offset.x > 0`），
    /// 且滚动条条带画出来了。
    #[gpui::test]
    fn wide_table_in_narrow_pane_scrolls_horizontally(cx: &mut gpui::TestAppContext) {
        // 12 列 × 每列 96px ≈ 1168px 下界，窗口只有 700px。
        let mut doc = String::from("intro\n\n|");
        for c in 0..12 {
            doc.push_str(&format!(" 列{c} 标题 |"));
        }
        doc.push_str("\n|");
        for _ in 0..12 {
            doc.push_str("---|");
        }
        doc.push('\n');
        for r in 0..3 {
            doc.push('|');
            for c in 0..12 {
                doc.push_str(&format!(" r{r}c{c} |"));
            }
            doc.push('\n');
        }
        let (view, mut vc) = setup(cx, size(px(700.), px(600.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let outer = vc.debug_bounds("live-table-outer").expect("表格应渲染");
        assert!(
            f32::from(outer.size.width) > 600.0,
            "表格卡片应占据可用宽，实际 {outer:?}"
        );
        // 横向可滚：容器的 max_offset.x > 0（内容比视口宽）。
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(2) // 表格首行 = 2（intro + 空行）
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            max_x > 1.0,
            "12 列宽表在 700px 窗口里应横向可滚，实际 max_offset.x = {max_x}"
        );
        // 滚动条条带（独立占位）应存在。
        assert!(
            vc.debug_bounds("live-hscroll-bar").is_some(),
            "横向滚动条应画出来"
        );
    }

    /// 窄栏里的表格在「全宽」后占满编辑器宽度（比窄栏宽）。
    #[gpui::test]
    fn wide_toggle_breaks_table_out_of_narrow_column(cx: &mut gpui::TestAppContext) {
        let mut doc = String::from("|");
        for c in 0..6 {
            doc.push_str(&format!(" 列{c} |"));
        }
        doc.push_str("\n|");
        for _ in 0..6 {
            doc.push_str("---|");
        }
        doc.push_str("\n|");
        for c in 0..6 {
            doc.push_str(&format!(" v{c} |"));
        }
        doc.push_str("\n\ntail\n");
        let (view, mut vc) = setup(cx, size(px(1400.), px(600.)), &doc);
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let narrow = vc.debug_bounds("live-table-outer").expect("表格应渲染");
        // 窄栏 = 780 - px_8(32) 两侧 = 716 内容宽。
        assert!(
            f32::from(narrow.size.width) <= 720.0,
            "默认应落在 780px 阅读窄栏里，实际 {narrow:?}"
        );
        // 切「全宽」：直接调状态（等价于点按钮），再断言宽度撑开。
        view.update(&mut vc, |v, cx| v.toggle_wide_block(0, cx));
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let wide = vc.debug_bounds("live-table-outer").expect("表格应仍在");
        assert!(
            f32::from(wide.size.width) > 1000.0,
            "「全宽」后表格应占满 1400px 窗口（含边距），实际 {wide:?}"
        );
        assert!(
            f32::from(wide.size.width) > f32::from(narrow.size.width) + 200.0,
            "全宽应显著宽于窄栏：{narrow:?} → {wide:?}"
        );
        // 再点一次还原。
        view.update(&mut vc, |v, cx| v.toggle_wide_block(0, cx));
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let back = vc.debug_bounds("live-table-outer").expect("表格应仍在");
        assert!(
            f32::from(back.size.width) <= 720.0,
            "取消全宽应回到窄栏，实际 {back:?}"
        );
    }

    /// 状态条「全宽」按钮：整个文档脱离 780px 窄栏（含正文段落），
    /// 再点一次回到标宽。走真实点击链路（不是直接调状态）。
    #[gpui::test]
    fn clicking_width_chip_toggles_doc_wide(cx: &mut gpui::TestAppContext) {
        let doc = "第一段正文，用来量宽度。\n\n第二段正文，同样要脱离窄栏。\n";
        let (view, mut vc) = setup(cx, size(px(1400.), px(600.)), doc);
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        // 标宽：正文落在 780px 窄栏里（px_8 两侧 → 716）。
        let narrow = vc.debug_bounds("live-para").expect("段落应渲染");
        assert!(
            f32::from(narrow.size.width) <= 720.0,
            "默认标宽应落在 780px 阅读窄栏里，实际 {narrow:?}"
        );
        // 按钮文案恒定「全宽」（不随状态变），顺序为 全宽 < 源码 < 大纲。
        let chip = vc
            .debug_bounds("live-width-toggle")
            .expect("「全宽」按钮应渲染");
        let src = vc.debug_bounds("live-source-toggle").expect("源码芯片");
        let outline = vc.debug_bounds("live-outline-toggle").expect("大纲芯片");
        assert!(
            chip.left() < src.left() && src.left() < outline.left(),
            "三个芯片应依次为 全宽 / 源码 / 大纲：{chip:?} {src:?} {outline:?}"
        );
        // 点一下 → 全宽。
        vc.simulate_click(chip.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, _| v.wide_doc),
            "点击「全宽」按钮应置位文档级全宽"
        );
        let wide = vc.debug_bounds("live-para").expect("段落应仍在");
        assert!(
            f32::from(wide.size.width) > 1000.0,
            "全宽后正文应占满 1400px 窗口（含边距），实际 {wide:?}"
        );
        assert!(
            f32::from(wide.size.width) > f32::from(narrow.size.width) + 200.0,
            "全宽应显著宽于窄栏：{narrow:?} → {wide:?}"
        );
        // 状态条本身也铺满（两端贴内容区）。
        let status = vc.debug_bounds("live-status").expect("状态条");
        assert!(
            f32::from(status.size.width) > 1000.0,
            "全宽时状态条应铺满，实际 {status:?}"
        );
        // 再点一次 → 标宽。
        let chip = vc.debug_bounds("live-width-toggle").expect("按钮仍在");
        vc.simulate_click(chip.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, _| v.wide_doc),
            "再点一次应回到标宽"
        );
        let back = vc.debug_bounds("live-para").expect("段落应仍在");
        assert!(
            f32::from(back.size.width) <= 720.0,
            "取消全宽应回到窄栏，实际 {back:?}"
        );
    }

    /// 文档级全宽时，逐块「全宽」按钮（表格工具栏 / mermaid 卡片）隐藏 ——
    /// 已全宽，单块按钮没有可切换的效果；切回标宽后按钮恢复，按块状态保留。
    #[gpui::test]
    fn doc_wide_hides_per_block_wide_button(cx: &mut gpui::TestAppContext) {
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
        let (view, mut vc) = setup(cx, size(px(1200.), px(600.)), doc);
        // 光标进表 → 工具栏出现（含「全宽」按钮）。
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            vc.debug_bounds("live-table-toolbar-wide").is_some(),
            "setup：标宽时表格工具栏的「全宽」按钮应在"
        );
        // 先置位按块全宽：切到文档全宽再切回来，状态必须保留。
        view.update(&mut vc, |v, cx| v.toggle_wide_block(0, cx));
        view.update(&mut vc, |v, cx| v.toggle_wide_doc(cx));
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            vc.debug_bounds("live-table-toolbar-wide").is_none(),
            "文档全宽时按块「全宽」按钮应隐藏"
        );
        view.update(&mut vc, |v, cx| v.toggle_wide_doc(cx));
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            vc.debug_bounds("live-table-toolbar-wide").is_some(),
            "切回标宽后按钮应恢复"
        );
        assert!(
            view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "切回标宽后按块全宽状态应保留"
        );
    }

    /// 换文档（`set_text`）：文档级宽度是视图偏好，保留；按块状态照旧清空。
    #[gpui::test]
    fn set_text_keeps_doc_wide_but_resets_block_state(cx: &mut gpui::TestAppContext) {
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
        let (view, mut vc) = setup(cx, size(px(1000.), px(500.)), doc);
        view.update(&mut vc, |v, cx| {
            v.toggle_wide_doc(cx);
            v.toggle_wide_block(0, cx);
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        view.update(&mut vc, |v, cx| {
            v.set_text("| x | y |\n|---|---|\n| 3 | 4 |\n\n别的文档\n", cx)
        });
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, _| v.wide_doc),
            "文档级宽度是视图偏好，换文档后应保留"
        );
        assert!(
            !view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "换文档后不得保留上一个文档的按块全宽状态"
        );
    }

    /// 文档级全宽下 mermaid 图按全宽适配（不再是阅读态 640 上限），且不溢出。
    #[gpui::test]
    fn doc_wide_mermaid_fits_width(cx: &mut gpui::TestAppContext) {
        let mut doc = String::from("```mermaid\nflowchart LR\n");
        for i in 0..14 {
            doc.push_str(&format!("    N{i}[\"节点{i}\"] --> N{}\n", i + 1));
        }
        doc.push_str("```\n");
        let (view, mut vc) = setup(cx, size(px(1200.), px(700.)), &doc);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            vc.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vc.refresh().unwrap();
            vc.run_until_parked();
            ready = vc.debug_bounds("live-mm-graph").is_some();
            if ready {
                break;
            }
        }
        assert!(ready, "mermaid 图应在若干帧内渲染完成");
        let narrow = vc.debug_bounds("live-code-inner").expect("图卡片内层");
        view.update(&mut vc, |v, cx| v.toggle_wide_doc(cx));
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let wide = vc.debug_bounds("live-code-inner").expect("图卡片内层");
        assert!(
            f32::from(wide.size.width) > f32::from(narrow.size.width) + 50.0,
            "文档全宽后图的视口应明显变宽：{narrow:?} → {wide:?}"
        );
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            max_x <= 0.5,
            "全宽适配不应溢出到需要横向滚动（max_x={max_x}）"
        );
    }

    /// 编辑器自身的纵向滚动条：长文档下出现，且贴内容区右侧。
    #[gpui::test]
    fn editor_has_vertical_scrollbar_on_long_doc(cx: &mut gpui::TestAppContext) {
        let mut doc = String::new();
        for i in 0..200 {
            doc.push_str(&format!("paragraph line {i}\n\n"));
        }
        let (view, mut vc) = setup(cx, size(px(900.), px(400.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let bar = vc
            .debug_bounds("live-vscroll-bar")
            .expect("长文档应有纵向滚动条");
        let content = vc
            .debug_bounds("live-markdown-content")
            .expect("内容区应有实体");
        // 条贴在内容区右侧。
        assert!(
            f32::from(bar.right()) >= f32::from(content.right()) - 2.0,
            "滚动条应贴内容区右缘：bar={bar:?} content={content:?}"
        );
        assert!(bar.size.height > px(50.), "滚动条应占满内容区高度");
        // 文档确实可滚（否则上面的断言没有意义）。
        let max_y = view.read_with(&vc, |v, _| f32::from(v.scroll.max_offset().y));
        assert!(max_y > 100.0, "长文档应可纵向滚动，实际 {max_y}");
    }

    /// 换文档（`set_text`）必须丢弃按行号记账的 UI 状态。
    ///
    /// 新文本在同一行号上可能是另一个块：留着旧的全宽/缩放/滚动位置会把它们
    /// 错误地套到无关的块上，而收帧裁剪挡不住（行号仍命中某个锚点）。
    #[gpui::test]
    fn set_text_resets_per_block_ui_state(cx: &mut gpui::TestAppContext) {
        // 表格（行 0）+ mermaid（行 4）：两类状态各有一个真实锚点。
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\n```mermaid\nflowchart LR\n    A --> B\n```\n";
        let (view, mut vc) = setup(cx, size(px(1000.), px(500.)), doc);
        view.update(&mut vc, |v, cx| {
            v.toggle_wide_block(0, cx);
            v.mermaid.set_zoom(4, 2.0);
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, _| v.is_wide_block(0)
                && v.mermaid_zoom(4).is_some()),
            "setup：全宽与缩放状态应已置位"
        );

        // 换文档：同样的行号上仍是表格 + mermaid，但都是**另一个**文档的块。
        view.update(&mut vc, |v, cx| {
            v.set_text(
                "| x | y |\n|---|---|\n| 3 | 4 |\n\n```mermaid\nflowchart LR\n    C --> D\n```\n",
                cx,
            )
        });
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "换文档后不得保留上一个文档的全宽状态"
        );
        assert_eq!(
            view.read_with(&vc, |v, _| v.mermaid_zoom(4)),
            None,
            "换文档后不得保留上一个文档的缩放倍率"
        );
    }

    /// 滚出虚拟化窗口的块：横向滚动位置必须留住（滚回来不能跳回最左）。
    ///
    /// 句柄被 `end_frame` 淘汰就等于把偏移归零 —— 这跟「删块后不留垃圾」
    /// 是两件事，判据必须是「文档里还有没有这个块」。
    #[gpui::test]
    fn hscroll_survives_virtualization_window_exit(cx: &mut gpui::TestAppContext) {
        // 宽表 + 大量填充行：窗口足够矮时表格会被虚拟化排除在窗口外。
        let mut doc = String::from("|");
        for c in 0..12 {
            doc.push_str(&format!(" 列{c} 标题 |"));
        }
        doc.push_str("\n|");
        for _ in 0..12 {
            doc.push_str("---|");
        }
        doc.push('\n');
        for r in 0..3 {
            doc.push('|');
            for c in 0..12 {
                doc.push_str(&format!(" r{r}c{c} |"));
            }
            doc.push('\n');
        }
        for i in 0..300 {
            doc.push_str(&format!("填充段落 {i}\n\n"));
        }
        let (view, mut vc) = setup(cx, size(px(700.), px(400.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let groups = view.read_with(&vc, |v, _| v.planned_geometry.group_count());
        assert!(
            groups > 200,
            "setup：文档分组数应超过虚拟化阈值（{groups}）"
        );
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(max_x > 1.0, "setup：宽表应可横向滚动（max_x={max_x}）");
        // 直接改偏移（模拟用户横向滚动后离开）。
        view.update(&mut vc, |v, cx| {
            if let Some(h) = v.h_scroll.handle(0) {
                let mut off = h.offset();
                off.x = px(-80.);
                h.set_offset(off);
            }
            cx.notify();
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let scrolled = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        assert!(scrolled < -1.0, "setup：横向偏移应已生效（{scrolled}）");

        // 滚到文档底部：表格滚出虚拟化窗口。
        view.update(&mut vc, |v, cx| {
            v.scroll_by_y(-100_000.0, cx);
        });
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let offscreen = vc.debug_bounds("live-table-scroll").is_none();
        assert!(offscreen, "setup：表格应已滚出虚拟化窗口");
        let kept = view.read_with(&vc, |v, _| {
            v.h_scroll.handle_for(0).map(|h| f32::from(h.offset().x))
        });
        assert_eq!(
            kept.map(|x| (x - scrolled).abs() < 1.0),
            Some(true),
            "滚出窗口后横向滚动位置必须保留（{scrolled} → {kept:?}）"
        );
    }

    /// 窗口失活：进行中的滚动条拖拽必须收尾。
    ///
    /// 拖拽状态由窗口级 MouseUp 清除，而失活后窗口收不到该事件；不清的话
    /// `on_fallback_drag` 之后每次都让行，静默禁用该路径下的选区扩展。
    #[gpui::test]
    fn window_deactivation_ends_scroll_drag(cx: &mut gpui::TestAppContext) {
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
        let (view, mut vc) = setup(cx, size(px(1000.), px(500.)), doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        // 直接置位拖拽状态（等价于按下 thumb 后的状态）。
        view.update(&mut vc, |v, cx| {
            let (state, _) = v.h_scroll.slot(0, cx);
            state.update(cx, |s, _| s.set_drag_for_test(px(0.), px(0.)));
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, cx| v.h_scroll.any_dragging(cx)),
            "setup：拖拽状态应已置位"
        );
        // `deactivate_window` 只对「当前活跃窗口」生效，测试窗口默认不活跃：
        // 先激活，再失活，走完整的平台回调链路。
        vc.update(|window, _| window.activate_window());
        vc.run_until_parked();
        vc.deactivate_window();
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, cx| v.h_scroll.any_dragging(cx)),
            "窗口失活后不得残留拖拽状态"
        );
    }

    /// 全宽状态裁剪：块被删掉后，残留的行号不得把别的块意外撑开。
    #[gpui::test]
    fn wide_state_pruned_when_block_disappears(cx: &mut gpui::TestAppContext) {
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
        let (view, mut vc) = setup(cx, size(px(1000.), px(500.)), doc);
        view.update(&mut vc, |v, cx| v.toggle_wide_block(0, cx));
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "全宽状态应在"
        );
        // 把表格整块删掉（首行 + 后三行）：状态必须被裁剪，不留悬空行号。
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| {
                m.move_to(0, 0, false);
                m.select_all();
                m.insert("plain text\n");
            });
            v.after_edit(cx);
        });
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "表格删除后全宽状态应被裁剪"
        );
    }

    /// 横向滚轮（触控板双指左右滑）能滚宽表，且**纵向**滚轮不被表格截胡
    /// （`restrict_scroll_to_axis` 的意义所在）。
    #[gpui::test]
    fn table_scroll_container_is_x_only(cx: &mut gpui::TestAppContext) {
        // 12 列 ≈ 1168px 下界，窗口 700px → 可横向滚动；正文够长 → 可纵向滚动。
        let mut doc = String::from("|");
        for c in 0..12 {
            doc.push_str(&format!(" 列{c} 标题 |"));
        }
        doc.push_str("\n|");
        for _ in 0..12 {
            doc.push_str("---|");
        }
        doc.push('\n');
        for r in 0..3 {
            doc.push('|');
            for c in 0..12 {
                doc.push_str(&format!(" r{r}c{c} |"));
            }
            doc.push('\n');
        }
        for i in 0..80 {
            doc.push_str(&format!("尾巴行 {i}\n"));
        }
        let (view, mut vc) = setup(cx, size(px(700.), px(500.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let grid = vc.debug_bounds("live-table-scroll").expect("表格滚动容器");
        let (max_x, h_before, v_before) = view.read_with(&vc, |v, _| {
            (
                v.h_scroll
                    .handle_for(0)
                    .map(|h| f32::from(h.max_offset().x))
                    .unwrap_or(0.0),
                v.h_scroll
                    .handle_for(0)
                    .map(|h| f32::from(h.offset().x))
                    .unwrap_or(0.0),
                v.scroll_offset_y(),
            )
        });
        assert!(max_x > 1.0, "setup：宽表应可横向滚动（max_x={max_x}）");
        assert!(
            v_before.abs() < 1.0,
            "setup：文档应从顶部开始（offset.y={v_before}）"
        );

        // 纵向滚轮落在表格网格上：必须滚的是文档，不是表格的 x 轴
        // （`restrict_scroll_to_axis` 的全部意义）。指针必须落在**窗口内**：
        // 表格高度是内容高度（远超视口），`grid.center()` 会落到窗口外、命中失败。
        vc.simulate_event(gpui::ScrollWheelEvent {
            position: point(grid.center().x, grid.top() + px(20.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-60.))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let (h_after, v_after) = view.read_with(&vc, |v, _| {
            (
                v.h_scroll
                    .handle_for(0)
                    .map(|h| f32::from(h.offset().x))
                    .unwrap_or(0.0),
                v.scroll_offset_y(),
            )
        });
        assert!(
            v_after < v_before - 1.0,
            "纵向滚轮应滚文档（offset.y {v_before} → {v_after}）"
        );
        assert!(
            (h_after - h_before).abs() < 1.0,
            "纵向滚轮不得改动表格横向偏移（offset.x {h_before} → {h_after}）"
        );
    }

    /// 拖动横向滚动条 thumb：内容真的跟着滚，松手后停住。
    ///
    /// 这是「过宽内容能左右滚动查看」的核心交互 —— 只断言容器可滚是不够的，
    /// 必须验证按下/拖动/松开整条链路真的改到了滚动偏移。
    #[gpui::test]
    fn dragging_hscroll_thumb_moves_table_content(cx: &mut gpui::TestAppContext) {
        // 12 列 ≈ 1168px 下界，窗口 700px → 可滚。
        let mut doc = String::from("|");
        for c in 0..12 {
            doc.push_str(&format!(" 列{c} 标题 |"));
        }
        doc.push_str("\n|");
        for _ in 0..12 {
            doc.push_str("---|");
        }
        doc.push('\n');
        for r in 0..3 {
            doc.push('|');
            for c in 0..12 {
                doc.push_str(&format!(" r{r}c{c} |"));
            }
            doc.push('\n');
        }
        let (view, mut vc) = setup(cx, size(px(700.), px(600.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let table_start = 0usize;
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(table_start)
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(max_x > 1.0, "setup：宽表应可横向滚动（max_x={max_x}）");

        // 条带（含 thumb）的窗口坐标。
        let bar = vc.debug_bounds("live-hscroll-bar").expect("滚动条应画出来");
        let before = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(table_start)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        // 从条带左侧按下并向右拖 → 内容右移（offset.x 变小 / 更负）。
        let start = point(bar.left() + px(10.), bar.center().y);
        let end = point(bar.left() + px(120.), bar.center().y);
        vc.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        vc.simulate_mouse_move(end, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        vc.simulate_mouse_move(
            point(end.x + px(60.), end.y),
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        vc.run_until_parked();
        let during = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(table_start)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            during < before - 1.0,
            "向右拖 thumb 应让内容右移（offset.x {before} → {during}）"
        );
        vc.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
        vc.run_until_parked();
        let after = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(table_start)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            (after - during).abs() < 1.0,
            "松开后不应继续滚动（{during} → {after}）"
        );
        // 拖拽结束后不能还挂着「拖动中」状态（否则点击/选择会被永久让行）。
        assert!(
            !view.read_with(&vc, |v, cx| v.h_scroll.any_dragging(cx)),
            "松开后拖拽状态必须清空"
        );
    }

    /// mermaid 图卡片的「全宽」：图脱离 780px 窄栏、占满编辑器宽度。
    ///
    /// 全宽**不**把图放大到自然尺寸：图按视口宽度等比适配，因此全宽后反而
    /// 不出现横向滚动（自然宽大于视口时也不溢出）。
    #[gpui::test]
    fn mermaid_wide_toggle_enlarges_graph_and_scrolls(cx: &mut gpui::TestAppContext) {
        // 宽图：flowchart 横向铺开，自然宽度明显超过 640。
        let mut doc = String::from("```mermaid\nflowchart LR\n");
        for i in 0..14 {
            doc.push_str(&format!("    N{i}[\"节点{i}\"] --> N{}\n", i + 1));
        }
        doc.push_str("```\n");
        let (view, mut vc) = setup(cx, size(px(900.), px(700.)), &doc);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            vc.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vc.refresh().unwrap();
            vc.run_until_parked();
            ready = view.read_with(&vc, |v, _cx| {
                v.planned_geometry.mermaid_blocks.iter().any(|b| {
                    matches!(
                        v.mermaid.body(&b.source),
                        MermaidBody::Image { pending: false, .. }
                    )
                }) && v.planned_geometry.mermaid_blocks.len() == 1
            }) && vc.debug_bounds("live-mm-graph").is_some();
            if ready {
                break;
            }
        }
        assert!(ready, "mermaid 图应在若干帧内渲染完成");
        // `live-mm-graph` 是**卡片**；图的滚动视口是 `live-code-inner`，
        // 全宽与否要对比同一元素。
        let narrow = vc.debug_bounds("live-code-inner").expect("图卡片内层");
        // 阅读态：图最多 640px 宽（`MAX_DISPLAY_W`）。
        let (img_narrow, img_natural) = view.read_with(&vc, |v, _| {
            v.planned_geometry
                .mermaid_blocks
                .iter()
                .find_map(|b| match v.mermaid.body(&b.source) {
                    MermaidBody::Image { image, .. } => {
                        Some((image.display_size().0, image.natural_size().0))
                    }
                    _ => None,
                })
                .unwrap_or((0.0, 0.0))
        });
        assert!(
            img_narrow <= 640.0 + 0.5,
            "阅读态图宽应不超过 640 上限，实际 {img_narrow}"
        );
        assert!(
            img_natural > 640.0 + 50.0,
            "setup：本用例的图自然宽应明显超过 640（{img_natural}），否则断言没有意义"
        );
        // 切全宽 → 图占据整个宽度。**不放大到自然尺寸**：自然宽比全宽视口还
        // 大，若按自然尺寸展示就会溢出并可横向滚动 —— 现在应当「只占据宽度、
        // 不溢出」。
        view.update(&mut vc, |v, cx| v.toggle_wide_block(0, cx));
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let wide = vc.debug_bounds("live-code-inner").expect("图卡片内层");
        assert!(
            f32::from(wide.size.width) > f32::from(narrow.size.width) + 50.0,
            "全宽后图的视口应明显变宽：{narrow:?} → {wide:?}"
        );
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            max_x <= 0.5,
            "全宽只让图占据整个宽度，不应溢出到需要横向滚动（max_x={max_x}）"
        );
    }

    /// 缩放：`+` 放大、`−` 缩小、点百分比还原 100%，且长宽比不变。
    #[gpui::test]
    fn mermaid_zoom_in_out_and_reset(cx: &mut gpui::TestAppContext) {
        let doc = "```mermaid\nflowchart LR\n    A[开始] --> B[结束]\n```\n";
        let (view, mut vc) = setup(cx, size(px(900.), px(700.)), doc);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            vc.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vc.refresh().unwrap();
            vc.run_until_parked();
            if vc.debug_bounds("live-mm-zoom-in").is_some() {
                ready = true;
                break;
            }
        }
        assert!(ready, "缩放控件应在图渲染后出现");
        // 初始：没设过缩放（按适配宽度 / 阅读尺寸展示）。
        assert_eq!(
            view.read_with(&vc, |v, _| v.mermaid_zoom(0)),
            None,
            "初始不应有显式缩放倍率"
        );
        let (nat_w, nat_h) = view.read_with(&vc, |v, _| {
            v.planned_geometry
                .mermaid_blocks
                .iter()
                .find_map(|b| match v.mermaid.body(&b.source) {
                    MermaidBody::Image { image, .. } => Some(image.natural_size()),
                    _ => None,
                })
                .unwrap_or((1.0, 1.0))
        });

        // 点两次 `+`：从**当前显示倍率**出发连续放大（不是从 100% 跳变），
        // 每次 × 1.25。当前显示倍率 = 阅读态尺寸 / 自然尺寸。
        let base = view.read_with(&vc, |v, _| v.mermaid_display_scale(0, false));
        for _ in 0..2 {
            let btn = vc.debug_bounds("live-mm-zoom-in").expect("+ 按钮");
            vc.simulate_click(btn.center(), gpui::Modifiers::none());
            for _ in 0..2 {
                vc.refresh().unwrap();
                vc.run_until_parked();
            }
        }
        let z = view
            .read_with(&vc, |v, _| v.mermaid_zoom(0))
            .expect("放大后应有倍率");
        let want = (base * 1.25 * 1.25).min(4.0);
        assert!(
            (z - want).abs() < 0.01,
            "两次放大应为当前倍率 × 1.5625（{base} → {want}），实际 {z}"
        );
        // 尺寸按倍率走，且长宽比不变。
        let (dw, dh) = zoomed_size((nat_w, nat_h), z);
        assert!(
            (dw / dh - nat_w / nat_h).abs() < 0.01,
            "缩放必须保持长宽比：({dw}, {dh}) vs ({nat_w}, {nat_h})"
        );

        // 点百分比 → 还原：清掉显式倍率，回到「适配宽度 / 阅读尺寸」。
        let pct = vc.debug_bounds("live-mm-zoom-pct").expect("百分比按钮");
        vc.simulate_click(pct.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert_eq!(
            view.read_with(&vc, |v, _| v.mermaid_zoom(0)),
            None,
            "点百分比应还原（清除显式倍率）"
        );
        // 点 `−`：从当前显示倍率缩小一档。
        let base = view.read_with(&vc, |v, _| v.mermaid_display_scale(0, false));
        let btn = vc.debug_bounds("live-mm-zoom-out").expect("− 按钮");
        vc.simulate_click(btn.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let z = view
            .read_with(&vc, |v, _| v.mermaid_zoom(0))
            .expect("缩小后应有倍率");
        let want = (base / 1.25).max(0.25);
        assert!(
            (z - want).abs() < 0.01,
            "一次缩小应为当前倍率 ÷ 1.25（{base} → {want}），实际 {z}"
        );
    }

    /// 点击表格工具栏的「全宽」按钮：走真实点击链路（不是直接调状态），
    /// 并且再点一次能还原。
    #[gpui::test]
    fn clicking_table_wide_button_toggles_full_width(cx: &mut gpui::TestAppContext) {
        let doc = "| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
        let (view, mut vc) = setup(cx, size(px(1200.), px(600.)), doc);
        // 光标进表 → 工具栏出现。
        view.update(&mut vc, |v, cx| {
            v.edit(cx, |m| m.move_to(2, 2, false));
            v.after_move(cx);
        });
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let btn = vc
            .debug_bounds("live-table-toolbar-wide")
            .expect("「全宽」按钮应随工具栏出现");
        let before = vc.debug_bounds("live-table-outer").expect("表格");
        vc.simulate_click(btn.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        // 表格的块 id 是**块首行**（0），不是光标所在行。
        assert!(
            view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "点击「全宽」按钮应置位该表格块的全宽状态"
        );
        let after = vc.debug_bounds("live-table-outer").expect("表格");
        assert!(
            f32::from(after.size.width) > f32::from(before.size.width) + 200.0,
            "点击后表格应明显变宽：{before:?} → {after:?}"
        );
        // 再点一次还原。
        let btn = vc
            .debug_bounds("live-table-toolbar-wide")
            .expect("按钮仍在");
        vc.simulate_click(btn.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "再点一次应取消全宽"
        );
        let back = vc.debug_bounds("live-table-outer").expect("表格");
        assert!(
            f32::from(back.size.width) < f32::from(after.size.width) - 200.0,
            "取消后应回到窄栏：{after:?} → {back:?}"
        );
    }

    /// 横向滚轮（触控板双指左右滑）能滚宽表。
    ///
    /// 「纵向滚轮不被表格截胡」由 `table_scroll_container_is_x_only` 覆盖。
    #[gpui::test]
    fn horizontal_wheel_scrolls_table_without_hijacking_vertical(cx: &mut gpui::TestAppContext) {
        let mut doc = String::from("|");
        for c in 0..12 {
            doc.push_str(&format!(" 列{c} 标题 |"));
        }
        doc.push_str("\n|");
        for _ in 0..12 {
            doc.push_str("---|");
        }
        doc.push('\n');
        for r in 0..3 {
            doc.push('|');
            for c in 0..12 {
                doc.push_str(&format!(" r{r}c{c} |"));
            }
            doc.push('\n');
        }
        let (view, mut vc) = setup(cx, size(px(700.), px(600.)), &doc);
        for _ in 0..3 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        let max_x = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.max_offset().x))
                .unwrap_or(0.0)
        });
        assert!(max_x > 1.0, "setup：宽表应可横向滚动（max_x={max_x}）");

        // 指针落在表格网格上，发一个横向滚轮事件。
        let grid = vc.debug_bounds("live-table-scroll").expect("滚动容器");
        let at = grid.center();
        let before = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        vc.simulate_event(gpui::ScrollWheelEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(point(px(-60.), px(0.))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        vc.refresh().unwrap();
        vc.run_until_parked();
        let after = view.read_with(&vc, |v, _| {
            v.h_scroll
                .handle_for(0)
                .map(|h| f32::from(h.offset().x))
                .unwrap_or(0.0)
        });
        assert!(
            after < before - 1.0,
            "横向滚轮应让表格内容右移（offset.x {before} → {after}）"
        );
    }

    /// 点击 mermaid 卡片的「全宽」按钮：同样走真实点击链路。
    #[gpui::test]
    fn clicking_mermaid_wide_button_toggles_full_width(cx: &mut gpui::TestAppContext) {
        let doc = "```mermaid\nflowchart TD\n    A[开始] --> B[结束]\n```\n";
        let (view, mut vc) = setup(cx, size(px(1200.), px(700.)), doc);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            vc.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vc.refresh().unwrap();
            vc.run_until_parked();
            if vc.debug_bounds("live-mm-tab-wide").is_some() {
                ready = true;
                break;
            }
        }
        assert!(ready, "mermaid 卡片（含「全宽」tab）应在若干帧内出现");
        let btn = vc.debug_bounds("live-mm-tab-wide").expect("全宽按钮");
        vc.simulate_click(btn.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "点击 mermaid「全宽」应置位状态"
        );
        let wide = vc.debug_bounds("live-mm-graph").expect("图卡片");
        assert!(
            f32::from(wide.size.width) > 900.0,
            "全宽后图卡片应占满 1200px 窗口，实际 {wide:?}"
        );
        let btn = vc.debug_bounds("live-mm-tab-wide").expect("全宽按钮");
        vc.simulate_click(btn.center(), gpui::Modifiers::none());
        for _ in 0..2 {
            vc.refresh().unwrap();
            vc.run_until_parked();
        }
        assert!(
            !view.read_with(&vc, |v, _| v.is_wide_block(0)),
            "再点一次应取消"
        );
    }
}

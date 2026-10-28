use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Hsla, IntoElement, KeyBinding, ListAlignment,
    ListState, Render, SharedString, StyleRefinement, TextRun, TextStyleRefinement, WhiteSpace,
    Window, actions, div, list, px, rems,
};

use crate::editor::syntax::SyntaxHighlighter;
use crate::sidebar_view::font_settings::{
    platform_default_fallbacks, platform_default_font_family,
};
use crate::theme::{ActiveTheme, AppTheme};

use super::markdown_element::MarkdownElementBuilder;
use super::markdown_parser::{MarkdownEvent, parse_markdown};

/// 代码块高亮缓存：`(代码块源码, 源码在整篇文档中的字节范围) → TextRun 列表`。
///
/// 同一篇 Markdown 里的重复代码块可直接复用，避免每次渲染都重跑语法高亮。
pub(crate) type HighlightCache = Rc<RefCell<HashMap<(String, Range<usize>), Vec<TextRun>>>>;

#[derive(Clone)]
pub struct MarkdownStyle {
    pub base_text_style: gpui::TextStyle,
    pub container_style: StyleRefinement,
    pub code_block: StyleRefinement,
    /// 代码块是否带卡片附带的交互 chrome：左上角语言徽章 + 右上角复制按钮。
    ///
    /// `true`（预览 tab / live 卡片）；`false`（hover 浮层）—— 浮层里只留
    /// 代码本身的高亮，不标语言、不给复制入口。
    pub code_block_chrome: bool,
    pub inline_code: TextStyleRefinement,
    pub block_quote: TextStyleRefinement,
    pub link: TextStyleRefinement,
    pub rule_color: Hsla,
    pub block_quote_border_color: Hsla,
    pub heading: StyleRefinement,
    pub heading_level_styles: [Option<TextStyleRefinement>; 6],
    pub heading_border_color: Option<Hsla>,
}

impl MarkdownStyle {
    pub fn themed(window: &Window, cx: &gpui::App) -> Self {
        let theme = cx.theme();
        let ui = &theme.ui;
        let is_dark = theme.appearance.is_dark();

        // Typora / Notion 阅读体验：正文使用系统无衬线栈（非等宽），
        // 15px / 1.75 行高，段间距靠容器 margin 统一控制，
        // 参考 github-markdown-css + Notion 的柔和留白。
        let mut base_text_style = window.text_style();
        base_text_style.refine(&TextStyleRefinement {
            color: Some(ui.text),
            font_size: Some(px(15.).into()),
            line_height: Some(rems(1.75).into()),
            white_space: Some(WhiteSpace::Normal),
            ..Default::default()
        });

        // 代码块卡片：editor_background 背景，淡边框，圆角 8px
        let code_border = ui.border.opacity(if is_dark { 0.28 } else { 0.20 });
        let code_block = StyleRefinement {
            padding: gpui::EdgesRefinement {
                top: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(14.)),
                )),
                left: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(16.)),
                )),
                right: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(16.)),
                )),
                bottom: Some(gpui::DefiniteLength::Absolute(
                    gpui::AbsoluteLength::Pixels(px(14.)),
                )),
            },
            margin: gpui::EdgesRefinement {
                top: Some(gpui::Length::Definite(px(16.).into())),
                left: Some(gpui::Length::Definite(px(0.).into())),
                right: Some(gpui::Length::Definite(px(0.).into())),
                bottom: Some(gpui::Length::Definite(px(20.).into())),
            },
            border_style: Some(gpui::BorderStyle::Solid),
            border_widths: gpui::EdgesRefinement {
                top: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                left: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                right: Some(gpui::AbsoluteLength::Pixels(px(1.))),
                bottom: Some(gpui::AbsoluteLength::Pixels(px(1.))),
            },
            border_color: Some(code_border),
            background: Some(theme.editor_background().into()),
            text: TextStyleRefinement {
                font_family: Some(platform_default_font_family()),
                font_fallbacks: Some(platform_default_fallbacks()),
                font_size: Some(px(13.5).into()),
                line_height: Some(rems(1.6).into()),
                ..Default::default()
            },
            ..Default::default()
        };

        // 行内代码：淡底胶囊，亮 6% / 暗 12%
        let inline_code = TextStyleRefinement {
            font_family: Some(platform_default_font_family()),
            font_fallbacks: Some(platform_default_fallbacks()),
            font_size: Some(px(13.).into()),
            background_color: Some(ui.text.opacity(if is_dark { 0.12 } else { 0.06 })),
            ..Default::default()
        };

        let link = TextStyleRefinement {
            color: Some(ui.accent),
            underline: Some(gpui::UnderlineStyle {
                color: Some(ui.accent.opacity(0.35)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 标题层级：逐级缩小，H1-H4 加粗，H5-H6 600，H1/H2 带分割线（行高 > 字号，避免重叠）
        let heading = |size: f32, weight: gpui::FontWeight, height: f32| TextStyleRefinement {
            font_size: Some(rems(size).into()),
            font_weight: Some(weight),
            line_height: Some(rems(height).into()),
            ..Default::default()
        };
        let mut h6 = heading(0.85, gpui::FontWeight(600.), 1.55);
        h6.color = Some(ui.text_muted);
        let heading_level_styles = [
            Some(heading(1.875, gpui::FontWeight::BOLD, 2.35)),
            Some(heading(1.5, gpui::FontWeight::BOLD, 1.95)),
            Some(heading(1.25, gpui::FontWeight::BOLD, 1.75)),
            Some(heading(1.08, gpui::FontWeight::BOLD, 1.55)),
            Some(heading(0.95, gpui::FontWeight(600.), 1.5)),
            Some(h6),
        ];

        // 引用边框：左重边框
        let block_quote_border_color = ui.border.opacity(if is_dark { 0.9 } else { 0.65 });
        Self {
            base_text_style,
            container_style: StyleRefinement::default(),
            code_block,
            code_block_chrome: true,
            inline_code,
            block_quote: TextStyleRefinement {
                color: Some(ui.text.opacity(0.78)),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
            link,
            rule_color: ui.border.opacity(if is_dark { 0.5 } else { 0.45 }),
            block_quote_border_color,
            heading: StyleRefinement {
                ..Default::default()
            },
            heading_level_styles,
            heading_border_color: Some(ui.border.opacity(if is_dark { 0.35 } else { 0.22 })),
        }
    }
    /// hover 浮层专用的紧凑样式。
    ///
    /// 与 [`Self::themed`]（整页阅读，15px / 1.75 行高 / 780px 窄栏）刻意不同：
    /// hover 卡片只有 680×320，要塞进签名、文档段落和代码块，字号与行高都得
    /// 收紧，否则几行就被撑到折行、滚动条立刻出现。
    ///
    /// 对照 Zed `hover_popover.rs::hover_markdown_style`：它同样是独立的一套
    /// `MarkdownStyle`，而不是复用 preview 的样式。
    pub fn hover_style(window: &Window, cx: &App, code_font: String) -> Self {
        let theme = cx.theme();
        let ui = &theme.ui;
        let is_dark = cx.theme().appearance.is_dark();

        let mut base_text_style = window.text_style();
        base_text_style.refine(&TextStyleRefinement {
            color: Some(ui.text),
            font_size: Some(px(12.).into()),
            line_height: Some(rems(1.45).into()),
            white_space: Some(WhiteSpace::Normal),
            ..Default::default()
        });

        // 代码块：hover 里**不画框**——不铺背景、不描边、不留内边距，只保留
        // 等宽高亮的代码本身（语言徽章与复制按钮由 `code_block_chrome = false`
        // 关掉）。上下留 4px 让它与正文段落分开即可。
        //
        // 字段形状照 [`Self::themed`] 的 `code_block`：`StyleRefinement` 用的是
        // `background` / `border_widths` / `border_radius`，没有
        // `background_color` / `border_width` 这些名字。
        let code_block = StyleRefinement {
            margin: gpui::EdgesRefinement {
                top: Some(gpui::Length::Definite(px(4.).into())),
                left: Some(gpui::Length::Definite(px(0.).into())),
                right: Some(gpui::Length::Definite(px(0.).into())),
                bottom: Some(gpui::Length::Definite(px(4.).into())),
            },
            // **行高必须显式给且大于字号**：卡片有 `max_h`，行高塌陷会让
            // 相邻两行叠在一起（用户看到的「多行字符堆叠」）。
            text: TextStyleRefinement {
                font_family: Some(code_font.clone().into()),
                font_size: Some(px(11.5).into()),
                line_height: Some(rems(1.55).into()),
                white_space: Some(WhiteSpace::Normal),
                ..Default::default()
            },
            ..Default::default()
        };

        // 行内代码用**编辑器等宽字体**：hover 里出现的多半是签名与类型，
        // 拿 UI 无衬线渲染会丢掉对齐关系。
        //
        // `white_space: Normal` 是为了长类型路径能换行：`` `Vec<impl Iterator<Item = ...>>` ``
        // 这类写法在 680px 宽的卡片里放不下。
        let inline_code = TextStyleRefinement {
            font_family: Some(code_font.into()),
            font_size: Some(px(11.5).into()),
            white_space: Some(WhiteSpace::Normal),
            background_color: Some(ui.text.opacity(if is_dark { 0.12 } else { 0.06 })),
            ..Default::default()
        };

        let link = TextStyleRefinement {
            color: Some(ui.accent),
            underline: Some(gpui::UnderlineStyle {
                color: Some(ui.accent.opacity(0.35)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        };

        // hover 里几乎不会出现标题，但真出现时行高必须大于字号，否则相邻两行
        // 会叠在一起（这正是原先纯文本渲染的毛病）。
        let heading = |size: f32, weight: gpui::FontWeight, height: f32| TextStyleRefinement {
            font_size: Some(px(size).into()),
            font_weight: Some(weight),
            line_height: Some(px(size * height).into()),
            ..Default::default()
        };

        Self {
            base_text_style,
            container_style: StyleRefinement::default(),
            code_block,
            // hover 里代码块只渲染高亮本身（见上方 `code_block` 注释）。
            code_block_chrome: false,
            inline_code,
            block_quote: TextStyleRefinement {
                color: Some(ui.text.opacity(0.78)),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
            link,
            rule_color: ui.border.opacity(if is_dark { 0.5 } else { 0.45 }),
            block_quote_border_color: ui.border.opacity(if is_dark { 0.9 } else { 0.65 }),
            heading: StyleRefinement::default(),
            heading_level_styles: [
                Some(heading(15., gpui::FontWeight::BOLD, 1.5)),
                Some(heading(14., gpui::FontWeight::BOLD, 1.5)),
                Some(heading(13., gpui::FontWeight::BOLD, 1.5)),
                Some(heading(12.5, gpui::FontWeight::BOLD, 1.5)),
                Some(heading(12., gpui::FontWeight(600.), 1.5)),
                Some(heading(12., gpui::FontWeight(600.), 1.5)),
            ],
            heading_border_color: None,
        }
    }
}

actions!(markdown, [Copy, SelectAll]);

#[derive(Clone, Debug, Default)]
pub struct MarkdownSelection {
    pub anchor_fragment: usize,
    pub anchor_offset: usize,
    pub focus_fragment: usize,
    pub focus_offset: usize,
    pub pending: bool,
    pub has_selection: bool,
}

impl MarkdownSelection {
    pub fn is_empty(&self) -> bool {
        !self.has_selection
            || (self.anchor_fragment == self.focus_fragment
                && self.anchor_offset == self.focus_offset)
    }

    pub fn set_anchor(&mut self, fragment: usize, offset: usize) {
        self.anchor_fragment = fragment;
        self.anchor_offset = offset;
        self.has_selection = true;
    }

    pub fn set_focus(&mut self, fragment: usize, offset: usize) {
        self.focus_fragment = fragment;
        self.focus_offset = offset;
        self.has_selection = true;
    }

    pub fn clear(&mut self) {
        self.has_selection = false;
        self.pending = false;
    }

    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_fragment, self.anchor_offset);
        let b = (self.focus_fragment, self.focus_offset);
        if a <= b { (a, b) } else { (b, a) }
    }
}

pub struct Markdown {
    source: SharedString,
    events: Vec<(Range<usize>, MarkdownEvent)>,
    item_ranges: Vec<Range<usize>>,
    pub(crate) focus_handle: FocusHandle,
    copied_code_blocks: std::collections::HashSet<usize>,
    highlight_cache: HighlightCache,
    list_state: ListState,
    pub(crate) selection: MarkdownSelection,
    /// fragment_id -> text, populated lazily during render for copy. Wrapped in Rc<RefCell> so builder can write without Entity borrow during render.
    pub(crate) fragment_texts: Rc<RefCell<std::collections::HashMap<usize, String>>>,
    /// 文档级图片缓存：后台加载 + 按解析后的身份缓存位图（见 [`md_image`]）。
    pub(crate) image_cache: Entity<crate::editor::md_image::ImageCache>,
    /// 文档所在目录：相对图片路径以它为基准（`None` = 进程工作目录）。
    pub(crate) doc_dir: Option<std::path::PathBuf>,
    /// 图片加载完成后由 `image_cache` 触发重绘，订阅必须持有。
    _subscriptions: Vec<gpui::Subscription>,
}

impl Markdown {
    pub fn init(cx: &mut gpui::App) {
        crate::keymap::bind(
            cx,
            [
                KeyBinding::new("cmd-c", Copy, Some("Markdown")),
                KeyBinding::new("ctrl-c", Copy, Some("Markdown")),
                KeyBinding::new("cmd-a", SelectAll, Some("Markdown")),
                KeyBinding::new("ctrl-a", SelectAll, Some("Markdown")),
            ],
        );
    }

    pub fn new(source: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self::new_with_dir(source, None, cx)
    }

    /// 与 [`Self::new`] 相同，但带上文档所在目录：`![](assets/a.png)` 这类
    /// 相对引用以 `doc_dir` 为基准解析（不传则退回进程工作目录）。
    pub fn new_with_dir(
        source: impl Into<SharedString>,
        doc_dir: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = source.into();
        let focus_handle = cx.focus_handle();
        let image_cache = cx.new(|_| crate::editor::md_image::ImageCache::new());
        let _subscriptions = vec![cx.observe(&image_cache, |_, _, cx| cx.notify())];
        let mut this = Self {
            source: SharedString::default(),
            events: Vec::new(),
            item_ranges: Vec::new(),
            focus_handle,
            copied_code_blocks: std::collections::HashSet::new(),
            highlight_cache: Rc::new(RefCell::new(HashMap::new())),
            list_state: ListState::new(0, ListAlignment::Top, px(200.0)),
            selection: MarkdownSelection::default(),
            fragment_texts: Default::default(),
            image_cache,
            doc_dir,
            _subscriptions,
        };
        this.set_text(source, cx);
        this
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if self.source == text {
            return;
        }
        self.source = text;
        self.copied_code_blocks.clear();
        self.highlight_cache.borrow_mut().clear();
        self.selection.clear();
        self.fragment_texts.borrow_mut().clear();
        // 上一份文档的位图（含远程下载结果）不应继续驻留：键空间是「本文档
        // 出现过的每个 URL」，不清则会随切文档/外部重载只增不减。
        self.image_cache.update(cx, |cache, cx| cache.clear(cx));
        self.parse();
        cx.notify();
    }

    fn parse(&mut self) {
        self.events = parse_markdown(&self.source);
        self.item_ranges = compute_item_ranges(&self.events);
        self.list_state.reset(self.item_ranges.len());
    }

    pub(crate) fn local_selection_for_fragment(
        &self,
        fragment_id: usize,
        fragment_len: usize,
    ) -> Option<Range<usize>> {
        if self.selection.is_empty() {
            return None;
        }
        let ((s_frag, s_off), (e_frag, e_off)) = self.selection.ordered();
        if fragment_id < s_frag || fragment_id > e_frag {
            return None;
        }
        if s_frag == e_frag && s_frag == fragment_id {
            let start = s_off.min(e_off).min(fragment_len);
            let end = s_off.max(e_off).min(fragment_len);
            if start < end {
                return Some(start..end);
            } else {
                return None;
            }
        }
        if fragment_id == s_frag {
            let start = s_off.min(fragment_len);
            if start < fragment_len {
                return Some(start..fragment_len);
            } else {
                return None;
            }
        }
        if fragment_id == e_frag {
            let end = e_off.min(fragment_len);
            if end > 0 {
                return Some(0..end);
            } else {
                return None;
            }
        }
        // Fully inside selection
        Some(0..fragment_len)
    }

    pub fn selected_plain_text(&self, _cx: &App) -> String {
        if self.selection.is_empty() {
            return String::new();
        }
        let ((s_frag, s_off), (e_frag, e_off)) = self.selection.ordered();
        // Collect fragments in order
        let mut ids: Vec<usize> = self.fragment_texts.borrow().keys().cloned().collect();
        ids.sort_unstable();

        fn snap_forward(text: &str, mut offset: usize) -> usize {
            while offset < text.len() && !text.is_char_boundary(offset) {
                offset += 1;
            }
            offset.min(text.len())
        }
        fn snap_backward(text: &str, mut offset: usize, min: usize) -> usize {
            while offset > min && !text.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        }

        let mut out = String::new();
        let mut first = true;
        for &frag_id in &ids {
            if frag_id < s_frag || frag_id > e_frag {
                continue;
            }
            let Some(text) = self.fragment_texts.borrow().get(&frag_id).cloned() else {
                continue;
            };
            let slice = if s_frag == e_frag {
                let start = snap_forward(&text, s_off.min(e_off).min(text.len()));
                let end = snap_backward(&text, s_off.max(e_off).min(text.len()), start);
                &text[start..end]
            } else if frag_id == s_frag {
                let start = snap_forward(&text, s_off.min(text.len()));
                &text[start..]
            } else if frag_id == e_frag {
                let end = snap_backward(&text, e_off.min(text.len()), 0);
                &text[..end]
            } else {
                text.as_str()
            };
            if slice.is_empty() {
                continue;
            }
            if !first {
                let prev_item = (frag_id - 1) >> 20;
                let cur_item = frag_id >> 20;
                let sep = if prev_item != cur_item { "\n\n" } else { "\n" };
                out.push_str(sep);
            }
            out.push_str(slice);
            first = false;
        }
        out
    }

    pub fn copy_selection(&self, _window: &mut Window, cx: &mut App) {
        let text = self.selected_plain_text(cx);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.fragment_texts.borrow().is_empty() {
            return;
        }
        let mut ids: Vec<usize> = self.fragment_texts.borrow().keys().cloned().collect();
        ids.sort_unstable();
        if let (Some(first), Some(last)) = (ids.first(), ids.last()) {
            let last_len = self
                .fragment_texts
                .borrow()
                .get(last)
                .map(|s| s.len())
                .unwrap_or(0);
            self.selection.anchor_fragment = *first;
            self.selection.anchor_offset = 0;
            self.selection.focus_fragment = *last;
            self.selection.focus_offset = last_len;
            self.selection.has_selection = true;
            self.selection.pending = false;
            cx.notify();
        }
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.has_selection {
            self.selection.clear();
            cx.notify();
        }
    }

    pub(crate) fn register_fragment(&self, fragment_id: usize, text: String) {
        self.fragment_texts.borrow_mut().insert(fragment_id, text);
    }

    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn events(&self) -> &[(Range<usize>, MarkdownEvent)] {
        &self.events
    }

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

crate::impl_focusable!(Markdown);

impl Render for Markdown {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let markdown = cx.entity();
        let list_state = self.list_state.clone();
        let style = MarkdownStyle::themed(_window, cx);
        // Register actions for copy/select-all when focused
        let markdown_for_copy = markdown.clone();
        let markdown_for_select_all = markdown.clone();

        // Notion/Typora 版式：居中窄栏，最大 780px (≈72ch)，
        // 外层负责背景与居中，内层窄栏承载 list，实现优雅的阅读宽度。
        div()
            .id("markdown")
            .key_context("Markdown")
            .track_focus(&self.focus_handle)
            .on_action({
                let markdown = markdown_for_copy.clone();
                move |_: &Copy, window, cx| {
                    markdown.update(cx, |m, cx| m.copy_selection(window, cx));
                }
            })
            .on_action({
                let markdown = markdown_for_select_all.clone();
                move |_: &SelectAll, _window, cx| {
                    markdown.update(cx, |markdown, cx| {
                        markdown.select_all(cx);
                    });
                }
            })
            .flex()
            .flex_col()
            .items_center()
            .size_full()
            .overflow_hidden()
            .bg(theme.app_background)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    if !this.selection.pending && this.selection.has_selection {
                        this.clear_selection(cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(780.))
                    .px_8()
                    .py_8()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        list(list_state, move |ix, _window, cx| {
                            Self::render_item(markdown.clone(), ix, cx, style.clone())
                        })
                        .w_full()
                        .h_full(),
                    ),
            )
    }
}

impl Markdown {
    /// 渲染单个条目。**不要 `window` 参数**：这里只用 `cx` 取样式与资源，
    /// 多带一个 `&mut Window` 会让内联调用方（hover 卡片，绘制阶段拿不到
    /// `Window`）不得不伪造窗口。
    fn render_item(
        markdown: Entity<Markdown>,
        ix: usize,
        cx: &mut gpui::App,
        style: MarkdownStyle,
    ) -> gpui::AnyElement {
        let (copied, events, highlight_cache, image_cache, doc_dir) =
            markdown.update(cx, |markdown, _cx| {
                let range = markdown.item_ranges[ix].clone();
                (
                    markdown.copied_code_blocks.clone(),
                    markdown.events[range].to_vec(),
                    markdown.highlight_cache.clone(),
                    markdown.image_cache.clone(),
                    markdown.doc_dir.clone(),
                )
            });

        let elements = MarkdownElementBuilder::new_with_item(
            style,
            copied,
            markdown,
            highlight_cache,
            ix,
            image_cache,
            doc_dir,
        )
        .build(&events, cx);

        div().w_full().children(elements).into_any_element()
    }

    /// 把全部条目**内联**渲染成元素列表（不带整页外壳）。
    ///
    /// [`Render::render`] 给的是整页版式：居中 780px 窄栏 + 自己的背景色，
    /// 那是 markdown 预览 tab 用的。hover 卡片要保留自己的外观，只借用
    /// markdown 的解析与语法高亮，所以必须走这个入口。
    ///
    /// 对照 Zed `hover_popover.rs`：它也是 `MarkdownElement::new(...)` 内联
    /// 放进浮层，而不是把 markdown 实体整个渲染出来。
    pub fn render_items_inline(
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &mut App,
    ) -> Vec<gpui::AnyElement> {
        let count = markdown.read(cx).item_ranges.len();
        (0..count)
            .map(|ix| Self::render_item(markdown.clone(), ix, cx, style.clone()))
            .collect()
    }

    /// 条目数量（0 表示内容是空的，不该渲染任何东西）。
    pub fn item_count(&self) -> usize {
        self.item_ranges.len()
    }
}

fn compute_item_ranges(events: &[(Range<usize>, MarkdownEvent)]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    for (i, (_, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(_) => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            MarkdownEvent::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    ranges.push(start..i + 1);
                }
            }
            MarkdownEvent::Rule if depth == 0 => {
                ranges.push(i..i + 1);
            }
            _ => {}
        }
    }
    ranges
}

pub fn highlight_code_runs(
    content: &str,
    language: &str,
    base_style: &gpui::TextStyle,
    theme: &AppTheme,
) -> Vec<TextRun> {
    let mut highlighter = SyntaxHighlighter::new(language);
    let rope = ropey::Rope::from_str(content);
    highlighter.update(None, &rope, None);
    let styles = highlighter.styles(&(0..content.len()), &theme.highlight_theme);

    let mut runs = Vec::new();
    let mut offset = 0;
    for (range, style) in &styles {
        if offset < range.start {
            runs.push(base_style.to_run(range.start - offset));
        }
        let mut run_style = base_style.clone();
        run_style = run_style.highlight(*style);
        runs.push(run_style.to_run(range.len()));
        offset = range.end;
    }
    if offset < content.len() {
        runs.push(base_style.to_run(content.len() - offset));
    }
    runs
}

#[cfg(test)]
mod chrome_tests {
    use super::*;

    /// 一段带语言的代码块：预览样式要画卡片 chrome，hover 样式只留高亮。
    const SAMPLE: &str = "```rust\nfn main() {}\n```\n";

    #[derive(Clone, Copy, PartialEq)]
    enum Look {
        Preview,
        Hover,
    }

    struct Host {
        markdown: Entity<Markdown>,
        look: Look,
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let style = match self.look {
                Look::Preview => MarkdownStyle::themed(window, cx),
                Look::Hover => MarkdownStyle::hover_style(window, cx, "Menlo".to_string()),
            };
            div().size_full().flex().flex_col().children(
                Markdown::render_items_inline(self.markdown.clone(), style, cx)
                    .into_iter()
                    .map(|item| div().w_full().child(item)),
            )
        }
    }

    /// hover 浮层里的 markdown 代码块：**不画框、不标语言、不给复制按钮**，
    /// 只渲染代码本身的高亮；预览（整页 / live）仍保留卡片 chrome。
    #[gpui::test]
    fn hover_code_block_renders_without_card_chrome(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::sidebar_view::font_settings::FontState::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            markdown: cx.new(|cx| Markdown::new(SAMPLE, cx)),
            look: Look::Hover,
        });
        cx.refresh().unwrap();
        cx.run_until_parked();

        let code = cx
            .debug_bounds("md-code-block")
            .expect("代码块本体仍应渲染");
        assert!(
            code.size.height > px(0.),
            "hover 代码块高度应大于 0：{code:?}"
        );
        assert!(
            cx.debug_bounds("md-code-lang").is_none(),
            "hover 代码块不该渲染语言徽章"
        );
        assert!(
            cx.debug_bounds("md-code-copy").is_none(),
            "hover 代码块不该渲染复制按钮"
        );

        // 预览样式：卡片 chrome 照旧（语言徽章 + 复制按钮都在）。
        host.update(cx, |h, cx| {
            h.look = Look::Preview;
            cx.notify();
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("md-code-block").is_some(),
            "预览代码块本体应渲染"
        );
        assert!(
            cx.debug_bounds("md-code-lang").is_some(),
            "预览代码块必须有语言徽章"
        );
        assert!(
            cx.debug_bounds("md-code-copy").is_some(),
            "预览代码块必须有复制按钮"
        );
    }
}

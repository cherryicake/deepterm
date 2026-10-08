//! 实时 markdown 编辑器（`LiveMarkdownView`）帧路径基准。
//!
//! 优化目标：大文档下的**滚动帧**与**空转帧**开销必须与文档规模无关
//! （虚拟化 + 几何缓存后只应随视口规模增长）。本基准按文档规模
//! （1k / 5k / 20k 行）分别测量同一操作：**斜率就是回归指标**。
//!
//! 每个场景通过 [`gpui::BenchAppContext::bench_renderer`] 走真实帧管线
//! （`LiveMarkdownView::render` → 元素构建 → 排版 → 绘制提交）。
//! 文档内容确定性生成，迭代之间输入可复现。
//!
//! 运行：
//! ```text
//! cargo bench --bench markdown_live
//! cargo bench --bench markdown_live -- frame_blink
//! ```
//! HTML 报告：`target/criterion/report/index.html`
//!
//! 判读方式：把三档规模的读数并列比较。
//! - `frame_blink_*`：空转重绘（blink / 闲置）。**必须近似常数** ——
//!   它不含任何视口变化，若随行数增长说明又有 O(文档) 工作回到帧路径。
//! - `frame_scroll_*`：滚动一帧。只应随视口行数增长，不应随文档总行数增长。
//! - `frame_type_*`：单字符插入。只应随改动规模增长。
//!
//! 反例（优化前）：`planned_rows().to_vec()` 每帧深拷贝整篇 plan、
//! 分组/校准桶/有序号/mermaid 扫描逐帧重建、`is_visible` 位图 O(rows)，
//! 使这些读数都随文档规模线性上升。

use deepterm::editor::markdown_live::LiveMarkdownView;
use deepterm::sidebar_view::font_settings::FontState;
use deepterm::theme;
use gpui::prelude::*;
use gpui::{BenchAppContext, Entity};

/// 初始化主题与字体（`MarkdownStyle::themed` 依赖主题；排版依赖字体状态）。
fn init_bench_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        theme::init(cx);
        FontState::init(cx);
    });
}

/// 生成 `lines` 行源文本的确定性 markdown：标题 / 段落 / 列表 / 代码块 /
/// 表格 / 引用混排。刻意**不含图片行**（图片走后台加载，会把磁盘抖动带进
/// 读数）。
fn make_doc(lines: usize) -> String {
    let mut doc = String::with_capacity(lines * 48);
    let mut i = 0usize;
    while i < lines {
        match i % 8 {
            0 => doc.push_str(&format!("# Heading {i}\n\n")),
            1 => doc.push_str(&format!("Paragraph {i} with **bold** and `code`.\n\n")),
            2 => doc.push_str(&format!("- bullet {i}\n- bullet {}\n\n", i + 1)),
            3 => doc.push_str(&format!("1. ordered {i}\n2. ordered {}\n\n", i + 1)),
            4 => doc.push_str(&format!("```rust\nfn f{i}() -> u32 {{ {i} }}\n```\n\n")),
            5 => doc.push_str(&format!("| a | b |\n|---|---|\n| {i} | {i} |\n\n")),
            6 => doc.push_str(&format!("> quote {i} with a [link](https://x.dev/{i})\n\n")),
            _ => doc.push_str(&format!(
                "Plain line {i} with filler text to be realistic.\n\n"
            )),
        }
        i += 3;
    }
    doc
}

/// 挂载实时 markdown 视图到无头窗口（作为窗口根节点参与渲染）。
///
/// 返回视图实体本身：`bench_renderer` 要求传入的实体位于窗口渲染树中，
/// 而 `LiveMarkdownView` 本身不实现 `Render`（它是 `Editor` 的子视图），
/// 因此外面包一层 [`ViewHost`] 作为根节点。
fn make_view(cx: &mut BenchAppContext, doc: &str) -> Entity<LiveMarkdownView> {
    let mut window = cx.add_empty_window();
    let doc = doc.to_string();
    let view = cx.update(|cx| cx.new(|cx| LiveMarkdownView::with_text(&doc, cx)));
    let inner = view.clone();
    window.update(|window, cx| {
        window.replace_root(cx, move |_window, _cx| ViewHost(inner.clone()));
    });
    view
}

/// 极简宿主：把视图当作窗口根节点渲染。
struct ViewHost(Entity<LiveMarkdownView>);

impl gpui::Render for ViewHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        gpui::div().size_full().child(self.0.clone())
    }
}

// ---------------------------------------------------------------------------
// 空转帧：blink / 闲置重绘 —— 必须与文档规模无关
// ---------------------------------------------------------------------------

fn frame_blink(cx: &mut BenchAppContext, lines: usize) {
    init_bench_context(cx);
    let doc = make_doc(lines);
    let view = make_view(cx, &doc);
    view.update(cx, |_, cx| cx.notify());
    cx.bench_renderer(view, move |_view, _window, cx| {
        cx.notify();
    });
}

/// 空转重绘一帧（1k 行）。
#[gpui::bench]
fn frame_blink_1k(cx: &mut BenchAppContext) {
    frame_blink(cx, 1_000);
}

/// 空转重绘一帧（5k 行）。
#[gpui::bench]
fn frame_blink_5k(cx: &mut BenchAppContext) {
    frame_blink(cx, 5_000);
}

/// 空转重绘一帧（20k 行）。
#[gpui::bench]
fn frame_blink_20k(cx: &mut BenchAppContext) {
    frame_blink(cx, 20_000);
}

// ---------------------------------------------------------------------------
// 滚动帧：视口位移 —— 只应随视口规模增长
// ---------------------------------------------------------------------------

fn frame_scroll(cx: &mut BenchAppContext, lines: usize) {
    init_bench_context(cx);
    let doc = make_doc(lines);
    let view = make_view(cx, &doc);
    // 先渲染一帧，让虚拟化窗口与几何缓存就位。
    view.update(cx, |_, cx| cx.notify());
    let mut down = true;
    cx.bench_renderer(view, move |view, _window, cx| {
        // 上下交替滚动一行：视口内容位移最小，但每次都是完整 render。
        let dy = if down { -26.0 } else { 26.0 };
        down = !down;
        view.scroll_by_y(dy, cx);
    });
}

/// 滚动一帧（1k 行）。
#[gpui::bench]
fn frame_scroll_1k(cx: &mut BenchAppContext) {
    frame_scroll(cx, 1_000);
}

/// 滚动一帧（5k 行）。
#[gpui::bench]
fn frame_scroll_5k(cx: &mut BenchAppContext) {
    frame_scroll(cx, 5_000);
}

/// 滚动一帧（20k 行）。
#[gpui::bench]
fn frame_scroll_20k(cx: &mut BenchAppContext) {
    frame_scroll(cx, 20_000);
}

// ---------------------------------------------------------------------------
// 击键帧：单字符插入 —— 只应随改动规模增长
// ---------------------------------------------------------------------------

fn frame_type(cx: &mut BenchAppContext, lines: usize) {
    init_bench_context(cx);
    let doc = make_doc(lines);
    let view = make_view(cx, &doc);
    view.update(cx, |_, cx| cx.notify());
    // 光标停在首行：插入单字符（走 `insert` + `after_edit` 全链路），
    // 下一次迭代回退一格，保持文档规模稳定。
    let mut typed = false;
    cx.bench_renderer(view, move |view, _window, cx| {
        if typed {
            view.backspace_for_bench(cx);
        } else {
            view.insert_text_for_bench("x", cx);
        }
        typed = !typed;
    });
}

/// 单字符插入一帧（1k 行）。
#[gpui::bench]
fn frame_type_1k(cx: &mut BenchAppContext) {
    frame_type(cx, 1_000);
}

/// 单字符插入一帧（5k 行）。
#[gpui::bench]
fn frame_type_5k(cx: &mut BenchAppContext) {
    frame_type(cx, 5_000);
}

/// 单字符插入一帧（20k 行）。
#[gpui::bench]
fn frame_type_20k(cx: &mut BenchAppContext) {
    frame_type(cx, 20_000);
}

gpui::bench_group!(
    benches,
    frame_blink_1k,
    frame_blink_5k,
    frame_blink_20k,
    frame_scroll_1k,
    frame_scroll_5k,
    frame_scroll_20k,
    frame_type_1k,
    frame_type_5k,
    frame_type_20k
);
gpui::bench_main!(benches);

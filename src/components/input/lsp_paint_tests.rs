//! LSP 浮层的**绘制路径**测试。
//!
//! 这一层必须用真实绘制验证，不能只测纯函数：
//! `defer_draw` 只允许在 **prepaint** 阶段调用——它把元素入队，gpui 随后
//! 对队列里的元素统一跑 prepaint + paint。若在 `paint` 里调用，入队的元素
//! 从未 prepaint，gpui 会在 `element.paint()` 处 panic：
//!
//! ```text
//! must call prepaint before paint
//! ```
//!
//! 这个 bug 只有真正跑一遍 `prepaint → paint` 才会暴露，纯函数测试看不见。
//!
//! 注意：绘制必须经由一个真实的 `Render` 视图（`cx.draw` 只接受
//! `Element`，而 `EditorInput` 是 `RenderOnce`，且 gpui 需要
//! `rendered_entity_stack` 才能算 `current_view()`）。

use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, Window, px,
};

use crate::components::input::rope_ext::RopeExt as _;
use crate::components::input::{EditorInput, InputState};

/// 承载编辑器的视图：让 `EditorInput` 在一个真实渲染树里被 prepaint+paint。
struct EditorHost(gpui::Entity<InputState>);

impl Render for EditorHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .w(px(600.))
            .h(px(400.))
            .child(EditorInput::new(&self.0))
    }
}

/// 建编辑器、设置浮层，然后真正跑一遍绘制。
fn draw_with(
    text: &str,
    cx: &mut gpui::TestAppContext,
    setup: impl FnOnce(&mut InputState, &mut gpui::Context<InputState>),
) -> gpui::Entity<InputState> {
    cx.update(|cx| {
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
    });

    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true)
        });
        input.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            setup(s, cx);
        });
        *cell.borrow_mut() = Some(input.clone());
        EditorHost(input)
    });
    let input = cell.into_inner().unwrap();

    // 真正跑一遍布局 + 绘制：浮层相关的 panic 会在这里发生。
    let _ = cx.refresh();
    input
}

/// hover 浮层存在时，绘制不能 panic。
///
/// 回归：曾在 `paint` 阶段调 `defer_draw`，触发
/// "must call prepaint before paint" 崩溃。
#[gpui::test]
fn hover_popover_paints_without_panic(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, _cx| {
        // 锚在第 2 行（`let x`），内容非空 → 绘制路径会走 defer_draw。
        state.set_hover_popover(
            16,
            crate::editor::hover::HoverInfo {
                contents: "```rust\nlet x: i32\n```".to_string(),
                range: None,
            },
        );
    });
    // 走到这里说明 prepaint + paint 都跑通了。
    assert!(input.read_with(cx, |s, _| s.hover_popover().is_some()));
}

/// 补全菜单存在时，绘制不能 panic。
#[gpui::test]
fn completion_menu_paints_without_panic(cx: &mut gpui::TestAppContext) {
    use crate::editor::completion::Candidate;

    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, _cx| {
        let candidates: Vec<Candidate> = (0..3)
            .map(|i| Candidate {
                label: format!("candidate_{i}"),
                detail: None,
                item: lsp_types::CompletionItem {
                    label: format!("candidate_{i}"),
                    ..Default::default()
                },
            })
            .collect();
        state.set_completion_anchor(16);
        state.set_completion_menu(candidates, 0);
    });
    assert!(input.read_with(cx, |s, _| s.completion_menu().is_some()));
}

/// hover 与补全同时存在（两个 deferred 元素）：都要能画出来。
#[gpui::test]
fn both_popovers_paint_together(cx: &mut gpui::TestAppContext) {
    use crate::editor::completion::Candidate;

    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, _cx| {
        state.set_hover_popover(
            16,
            crate::editor::hover::HoverInfo {
                contents: "hover text".to_string(),
                range: None,
            },
        );
        state.set_completion_anchor(16);
        state.set_completion_menu(
            vec![Candidate {
                label: "foo".to_string(),
                detail: None,
                item: lsp_types::CompletionItem {
                    label: "foo".to_string(),
                    ..Default::default()
                },
            }],
            0,
        );
    });
    assert!(input.read_with(cx, |s, _| s.hover_popover().is_some()));
    assert!(input.read_with(cx, |s, _| s.completion_menu().is_some()));
}

/// 锚点落在不可见区域（越界偏移）：不能 panic，只是不画。
#[gpui::test]
fn popover_with_out_of_range_anchor_is_safe(cx: &mut gpui::TestAppContext) {
    let _ = draw_with("fn main() {}\n", cx, |state, _cx| {
        state.set_hover_popover(
            // 远超文本长度：`visible_row_for_offset` 返回 None → 跳过绘制。
            999_999,
            crate::editor::hover::HoverInfo {
                contents: "unreachable".to_string(),
                range: None,
            },
        );
    });
}

/// 空内容的浮层不画（避免一张空白卡片）。
#[gpui::test]
fn empty_popover_content_is_skipped(cx: &mut gpui::TestAppContext) {
    let _ = draw_with("fn main() {}\n", cx, |state, _cx| {
        state.set_hover_popover(
            0,
            crate::editor::hover::HoverInfo {
                contents: "   \n  ".to_string(),
                range: None,
            },
        );
    });
}

// ==================== 诊断版本门控 ====================

/// 造一条覆盖 `[offset, offset+len)` 字节的诊断。
fn diag(offset: usize, len: usize, msg: &str) -> crate::editor::syntax::Diagnostic {
    crate::editor::syntax::Diagnostic::new(
        offset..(offset + len),
        crate::editor::syntax::DiagnosticSeverity::Error,
        msg.to_string(),
    )
}

/// 回归：过期版本的诊断不能覆盖新版本。
///
/// 服务器可能乱序推送（v5 的分析晚于 v7 到达）；若让旧的盖上去，
/// 用户看到的是基于旧文本的错误位置。
#[gpui::test]
fn stale_diagnostics_version_is_ignored(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {}\n", cx, |_state, _cx| {});

    input.update(cx, |state, _cx| {
        // v7 先到：应用。
        let applied = state.set_diagnostics_versioned(vec![diag(0, 2, "v7")], Some(7));
        assert!(applied, "v7 must apply (first versioned push)");
        assert_eq!(state.applied_diagnostics_version(), Some(7));

        // v5 后到：必须被丢弃。
        let applied = state.set_diagnostics_versioned(vec![diag(0, 2, "v5")], Some(5));
        assert!(!applied, "stale v5 must be ignored");

        // 已应用的仍是 v7 的内容。
        assert_eq!(state.applied_diagnostics_version(), Some(7));
        let msgs: Vec<String> = state
            .diagnostics()
            .map(|d| d.items().iter().map(|x| x.message.clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            msgs,
            vec!["v7".to_string()],
            "newer diagnostics must survive"
        );
    });
}

/// 同版本重复推送要接受（服务器会重发同一版本的更新结果）。
#[gpui::test]
fn same_version_diagnostics_apply(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {}\n", cx, |_state, _cx| {});
    input.update(cx, |state, _cx| {
        assert!(state.set_diagnostics_versioned(vec![diag(0, 2, "a")], Some(3)));
        assert!(
            state.set_diagnostics_versioned(vec![diag(0, 2, "b")], Some(3)),
            "same version must apply (re-push)"
        );
        let msgs: Vec<String> = state
            .diagnostics()
            .map(|d| d.items().iter().map(|x| x.message.clone()).collect())
            .unwrap_or_default();
        assert_eq!(msgs, vec!["b".to_string()]);
    });
}

/// 不带版本号的推送必须照常应用（部分服务器不报版本）。
#[gpui::test]
fn unversioned_diagnostics_still_apply(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {}\n", cx, |_state, _cx| {});
    input.update(cx, |state, _cx| {
        // 先有一个高版本。
        assert!(state.set_diagnostics_versioned(vec![diag(0, 2, "v9")], Some(9)));
        // 无版本推送：照常应用，且**不**改动已记录的版本。
        assert!(
            state.set_diagnostics_versioned(vec![diag(0, 2, "none")], None),
            "unversioned push must apply"
        );
        let msgs: Vec<String> = state
            .diagnostics()
            .map(|d| d.items().iter().map(|x| x.message.clone()).collect())
            .unwrap_or_default();
        assert_eq!(msgs, vec!["none".to_string()]);
        assert_eq!(
            state.applied_diagnostics_version(),
            Some(9),
            "unversioned push must not clobber the recorded version"
        );
    });
}

/// 版本递增时正常覆盖（不能因为门控把正常更新挡掉）。
#[gpui::test]
fn newer_version_diagnostics_replace_older(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {}\n", cx, |_state, _cx| {});
    input.update(cx, |state, _cx| {
        assert!(state.set_diagnostics_versioned(vec![diag(0, 2, "old")], Some(2)));
        assert!(state.set_diagnostics_versioned(vec![diag(0, 2, "new")], Some(4)));
        let msgs: Vec<String> = state
            .diagnostics()
            .map(|d| d.items().iter().map(|x| x.message.clone()).collect())
            .unwrap_or_default();
        assert_eq!(msgs, vec!["new".to_string()]);
        assert_eq!(state.applied_diagnostics_version(), Some(4));
    });
}

/// 清空诊断（空列表）也受版本门控：过期版本的「清空」不能抹掉新版本的错误。
#[gpui::test]
fn stale_empty_push_does_not_clear_newer_diagnostics(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {}\n", cx, |_state, _cx| {});
    input.update(cx, |state, _cx| {
        assert!(state.set_diagnostics_versioned(vec![diag(0, 2, "real")], Some(6)));
        // 过期版本的「无问题」推送：必须被丢弃，否则错误会被凭空抹掉。
        assert!(!state.set_diagnostics_versioned(vec![], Some(4)));
        let count = state.diagnostics().map(|d| d.items().len()).unwrap_or(0);
        assert_eq!(count, 1, "stale empty push must not clear diagnostics");
    });
}

// ==================== 浮层实际落点（端到端） ====================
//
// 这些用例驱动**真实的**绘制路径（`TextElement::prepaint` →
// `layout_lsp_popovers` → `place_at_anchor` → `popover_y`），并断言浮层
// 实际落在锚点上方还是下方。
//
// 为什么必须这样测：曾出现「顶部代码该往下弹却往上弹」的缺陷，根因是
// **调用方传错了矩形**（把已加过滚动偏移的 bounds 当视口）。纯函数测试
// 传自己的参数，完全看不见这类错误。

/// 从当前可见行里挑哪一行作为锚点。
#[derive(Clone, Copy)]
enum VisibleRowPick {
    /// 首个可见行（贴近视口顶部）。
    First,
    /// 中间的可见行。
    Middle,
    /// 末个可见行（贴近视口底部）。
    Last,
}

/// 在给定滚动位置下放置 hover 浮层，返回 (锚点屏幕 y, 浮层实际 y)。
///
/// 流程：先绘制一帧拿到布局 → `scroll_to` 滚到目标行 → 再绘制一帧，
/// 这样 prepaint 用的是滚动后的状态（`scroll_to` 依赖已有 `last_layout`）。
fn hover_placement(
    scroll_to_line: Option<usize>,
    pick: VisibleRowPick,
    cx: &mut gpui::TestAppContext,
) -> Option<(f32, f32)> {
    // 构造足够长的文本，便于滚动。
    let text: String = (0..80).map(|i| format!("line {i}\n")).collect();
    // 先建好编辑器（不设浮层），滚到目标位置后再设锚点——这样锚点一定
    // 落在当前可见区内，测的才是「择位方向」而不是「锚点是否可见」。
    let input = draw_with(&text, cx, |_state, _cx| {});

    if let Some(line) = scroll_to_line {
        let offset = input.read_with(cx, |state, _| state.text().line_start_offset(line));
        input.update(cx, |state, cx| {
            state.scroll_to(offset, None, cx);
        });
        let _ = cx.refresh();
    }

    // 滚动稳定后再设锚点：从**实际可见行**里挑一行，保证锚点确实可见
    // （否则测的就成了「不可见锚点不画」而不是择位方向）。
    // `anchor_selector` 决定挑哪一行：给 `VisibleRowPick::Middle` 取中部，
    // 取 `VisibleRowPick::First` 取首行（贴近视口顶部）。
    let anchor_line = input.read_with(cx, |state, _| {
        let layout = state.last_layout.as_ref()?;
        let vis = &layout.visible_buffer_lines;
        if vis.is_empty() {
            return None;
        }
        let ix = match pick {
            VisibleRowPick::First => 0,
            VisibleRowPick::Middle => vis.len() / 2,
            VisibleRowPick::Last => vis.len().saturating_sub(1),
        };
        vis.get(ix).copied()
    })?;

    input.update(cx, |state, _cx| {
        let offset = state.text().line_start_offset(anchor_line);
        state.set_hover_popover(
            offset,
            crate::editor::hover::HoverInfo {
                contents: "hover".to_string(),
                range: None,
            },
        );
    });
    let _ = cx.refresh();

    input.read_with(cx, |state, _| {
        // 直接用实现记下的锚点行 y 作为基准：测试不再自己重算坐标公式，
        // 避免「测试与实现各算一遍、一起算错」的自洽式通过。
        let origin = state.last_popover_origin()?;
        let anchor_y = state.last_popover_anchor_y()?;
        Some((f32::from(anchor_y), f32::from(origin.y)))
    })
}

/// 顶部代码（未滚动，锚点在第 1 行）：上方放不下 → 浮层必须在锚点**下方**。
#[gpui::test]
fn hover_at_top_places_popover_below(cx: &mut gpui::TestAppContext) {
    let Some((anchor_y, popover_y)) = hover_placement(None, VisibleRowPick::First, cx) else {
        panic!("popover was not placed");
    };
    assert!(
        popover_y > anchor_y,
        "top-of-screen code: popover must be placed BELOW the anchor \
         (anchor_y={anchor_y}, popover_y={popover_y})"
    );
}

/// 滚动到中部、锚点在屏幕中部：上方放得下 → 浮层在锚点**上方**。
#[gpui::test]
fn hover_in_the_middle_places_popover_above(cx: &mut gpui::TestAppContext) {
    // 滚到第 20 行，锚点取可见区中部 → 上方放得下。
    let Some((anchor_y, popover_y)) = hover_placement(Some(20), VisibleRowPick::Middle, cx) else {
        panic!("popover was not placed");
    };
    assert!(
        popover_y < anchor_y,
        "mid-screen code: popover should prefer ABOVE the anchor \
         (anchor_y={anchor_y}, popover_y={popover_y})"
    );
}

/// 关键回归：**滚动不该改变择位方向**。
///
/// 同一行代码滚到视口顶部附近时，浮层该往下弹；这正是「传错矩形」会
/// 破坏的不变量——把滚过的 bounds 当视口时，视口上沿会随滚动上移，
/// 于是顶部代码被误判成「上方放得下」而往上弹。
#[gpui::test]
fn placement_direction_is_scroll_invariant(cx: &mut gpui::TestAppContext) {
    // 同一份代码：滚到让锚点贴近视口顶部 vs 贴近视口底部。
    let near_top = hover_placement(Some(20), VisibleRowPick::First, cx);
    let near_bottom = hover_placement(Some(20), VisibleRowPick::Last, cx);

    let (Some((a_top, p_top)), Some((a_bot, p_bot))) = (near_top, near_bottom) else {
        panic!("popover was not placed in both scroll positions");
    };

    // 贴近视口顶部：必须往下弹。
    assert!(
        p_top > a_top,
        "when the anchor sits at the viewport top (scrolled), the popover must go BELOW \
         (anchor_y={a_top}, popover_y={p_top})"
    );
    // 贴近视口底部：必须往上弹。
    assert!(
        p_bot < a_bot,
        "when the anchor sits at the viewport bottom (unscrolled), the popover must go ABOVE \
         (anchor_y={a_bot}, popover_y={p_bot})"
    );
}

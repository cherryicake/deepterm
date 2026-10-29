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
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, Window, px,
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
    draw_with_probe(text, cx, setup, |_| ()).0
}

/// 同 [`draw_with`]，并在绘制完成后把 `VisualTestContext` 交给 `probe`
/// 查询 `debug_bounds` 之类的绘制探针（`VisualTestContext` 是
/// `TestAppContext` 的借用，不能带出函数）。
fn draw_with_probe<R>(
    text: &str,
    cx: &mut gpui::TestAppContext,
    setup: impl FnOnce(&mut InputState, &mut gpui::Context<InputState>),
    probe: impl FnOnce(&mut gpui::VisualTestContext) -> R,
) -> (gpui::Entity<InputState>, R) {
    cx.update(|cx| {
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
    });

    let cell = std::cell::RefCell::new(None);
    let (_host, vcx) = cx.add_window_view(|window, cx| {
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
    let _ = vcx.refresh();
    let probed = probe(vcx);
    (input, probed)
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
            crate::editor::hover::HoverInfo::from_plain_text("```rust\nlet x: i32\n```", _cx),
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
                detail: Some("fn() -> i32".to_string()),
                kind: Some(lsp_types::CompletionItemKind::FUNCTION),
                deprecated: false,
                filter_text: format!("candidate_{i}"),
                sort_text: format!("candidate_{i}"),
                insert_text: format!("candidate_{i}()"),
                replace_range: 16..16,
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

/// 回归：补全菜单可见时按 Enter，只能触发补全确认，不能再插入换行。
///
/// 真实击键路径（`simulate_keystrokes`）会同时匹配补全键与 `Input` 的
/// `editor::Enter`。补全 handler 必须 `stop_propagation()`，否则两个 action
/// 都会执行：先插入候选、再换行。
#[gpui::test]
fn enter_on_visible_completion_menu_does_not_leak_to_newline(cx: &mut gpui::TestAppContext) {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use crate::editor::CompletionConfirm;
    use crate::editor::completion::Candidate;

    cx.update(|cx| {
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::keymap::init(cx);
    });

    struct KeyHost {
        input: gpui::Entity<InputState>,
        completions: Rc<Cell<usize>>,
    }
    impl Render for KeyHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .key_context("EditorPanel")
                .size_full()
                .on_action(cx.listener(|this, _: &CompletionConfirm, _window, cx| {
                    this.completions.set(this.completions.get() + 1);
                    // 与生产代码的 `completion_confirm` 相同：补全接管本次
                    // 按键后必须阻止普通编辑键继续派发。
                    cx.stop_propagation();
                }))
                .child(EditorInput::new(&self.input))
        }
    }

    let cell = RefCell::new(None);
    let hits = Rc::new(Cell::new(0));
    let (host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true);
            state.set_value("pri", window, cx);
            state.selected_range = (3..3).into();
            state.set_completion_anchor(3);
            state.set_completion_menu(
                vec![Candidate {
                    label: "println".into(),
                    detail: None,
                    kind: Some(lsp_types::CompletionItemKind::FUNCTION),
                    deprecated: false,
                    filter_text: "println".into(),
                    sort_text: "println".into(),
                    insert_text: "println!".into(),
                    replace_range: 0..3,
                    item: lsp_types::CompletionItem {
                        label: "println".into(),
                        ..Default::default()
                    },
                }],
                0,
            );
            state
        });
        *cell.borrow_mut() = Some(input.clone());
        KeyHost {
            input,
            completions: hits.clone(),
        }
    });
    let input = cell.into_inner().unwrap();

    cx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.focus_handle.clone().focus(window, cx);
        });
    });
    // 生成一帧：`EditorInput` 的键位上下文必须带上 `showing_completions`。
    let _ = cx.refresh();
    cx.simulate_keystrokes("enter");

    assert_eq!(
        host.read_with(cx, |host, _| host.completions.get()),
        1,
        "菜单可见时 Enter 必须派发 CompletionConfirm"
    );
    assert_eq!(
        input.read_with(cx, |state, _| state.text().to_string()),
        "pri",
        "补全确认后不能继续触发 editor::Enter 插入换行"
    );
}

/// hover 与补全同时存在（两个 deferred 元素）：都要能画出来。
#[gpui::test]
fn both_popovers_paint_together(cx: &mut gpui::TestAppContext) {
    use crate::editor::completion::Candidate;

    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, _cx| {
        state.set_hover_popover(
            16,
            crate::editor::hover::HoverInfo::from_plain_text("hover text", _cx),
        );
        state.set_completion_anchor(16);
        state.set_completion_menu(
            vec![Candidate {
                label: "foo".to_string(),
                detail: None,
                kind: Some(lsp_types::CompletionItemKind::VARIABLE),
                deprecated: false,
                filter_text: "foo".to_string(),
                sort_text: "foo".to_string(),
                insert_text: "foo".to_string(),
                replace_range: 16..16,
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
            crate::editor::hover::HoverInfo::from_plain_text("unreachable", _cx),
        );
    });
}

/// 空内容的浮层不画（避免一张空白卡片）。
#[gpui::test]
fn empty_popover_content_is_skipped(cx: &mut gpui::TestAppContext) {
    let _ = draw_with("fn main() {}\n", cx, |state, _cx| {
        state.set_hover_popover(
            0,
            crate::editor::hover::HoverInfo::from_plain_text("   \n  ", _cx),
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
            crate::editor::hover::HoverInfo::from_plain_text("hover", _cx),
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

// ==================== 补全：additionalTextEdits（自动导入） ====================

/// 接受候选时必须一并应用 `additionalTextEdits`（自动导入的 `use` 语句）。
///
/// 回归：此前只插主文本，`additionalTextEdits` 被完全忽略——接受 `HashMap`
/// 得到的是「未导入」的错误，而不是可用的代码。
#[gpui::test]
fn completion_applies_additional_text_edits(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {\n    let m = HashMap\n}\n", cx, |_s, _cx| {});

    input.update(cx, |state, cx| {
        // 光标在 `HashMap` 之后（行 1 偏移 18 = `    let m = HashMap` 末尾）。
        let cursor = state.text().line_start_offset(1) + "    let m = HashMap".len();
        state.selected_range = (cursor..cursor).into();

        // 主插入：把 `HashMap` 换成 `HashMap::new()`；
        // 附加编辑：在文件开头插入 `use std::collections::HashMap;\n`。
        let additional = vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            ),
            new_text: "use std::collections::HashMap;\n".to_string(),
        }];
        let range = (cursor - "HashMap".len())..cursor;
        let ok = state.apply_completion_with_edits(range, "HashMap::new()", &additional, cx);
        assert!(ok, "completion should apply");

        let text = state.text().to_string();
        assert!(
            text.starts_with("use std::collections::HashMap;\n"),
            "the `use` line must be inserted at the top; got:\n{text}"
        );
        assert!(
            text.contains("let m = HashMap::new()"),
            "the main insert must land correctly after the shifted offset; got:\n{text}"
        );
    });
}

/// 附加编辑落在主插入**之后**时，主插入区间不应被平移。
#[gpui::test]
fn additional_edit_after_insert_does_not_shift_it(cx: &mut gpui::TestAppContext) {
    let input = draw_with("let x = foo\n\n\n\n", cx, |_s, _cx| {});

    input.update(cx, |state, cx| {
        let cursor = "let x = foo".len();
        state.selected_range = (cursor..cursor).into();
        let additional = vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(3, 0),
                lsp_types::Position::new(3, 0),
            ),
            new_text: "// trailing\n".to_string(),
        }];
        let range = 8..11; // `foo`
        assert!(state.apply_completion_with_edits(range, "bar()", &additional, cx));

        let text = state.text().to_string();
        assert!(text.starts_with("let x = bar()"), "got:\n{text}");
        assert!(text.contains("// trailing"), "got:\n{text}");
    });
}

/// 越界的附加编辑**被钳制**而不是 panic，也不会写坏缓冲区。
///
/// `position_to_offset` 对越界行返回文本末尾（`clip_offset` 语义），
/// 所以这类编辑退化成「在文件末尾插入」——安全但无意义。关键是：
/// 不 panic、不越界、不破坏已有内容。
#[gpui::test]
fn out_of_range_additional_edit_is_clamped_not_fatal(cx: &mut gpui::TestAppContext) {
    let input = draw_with("ab\n", cx, |_s, _cx| {});

    input.update(cx, |state, cx| {
        state.selected_range = (2..2).into();
        let additional = vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(99, 0),
                lsp_types::Position::new(99, 5),
            ),
            new_text: "junk".to_string(),
        }];
        let ok = state.apply_completion_with_edits(0..2, "XY", &additional, cx);
        assert!(ok, "must not panic on an out-of-range additional edit");
        // 原有内容仍然完整（`ab` 被 `XY` 替换，末尾追加了 junk）。
        let text = state.text().to_string();
        assert!(text.starts_with("XY"), "main insert applied; got {text:?}");
        assert!(text.len() <= 16, "no runaway growth; got {text:?}");
    });
}

/// 附加编辑区间落在字符中间（多字节字符内部）时安全跳过该编辑。
#[gpui::test]
fn additional_edit_on_non_char_boundary_is_safe(cx: &mut gpui::TestAppContext) {
    let input = draw_with("中文x\n", cx, |_s, _cx| {});
    input.update(cx, |state, cx| {
        state.selected_range = (0..0).into();
        // 列 1 落在 `中` 的 UTF-16 中间 → 换算后被吸附，不应 panic。
        let additional = vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 1),
                lsp_types::Position::new(0, 2),
            ),
            new_text: "Y".to_string(),
        }];
        let _ = state.apply_completion_with_edits(0..0, "", &additional, cx);
        // 只要不 panic 即通过；文本仍应是合法 UTF-8。
        let _ = state.text().to_string();
    });
}

/// 没有附加编辑时行为与普通插入一致（不能因为新路径引入回归）。
#[gpui::test]
fn completion_without_additional_edits_still_works(cx: &mut gpui::TestAppContext) {
    let input = draw_with("let v = fo\n", cx, |_s, _cx| {});
    input.update(cx, |state, cx| {
        state.selected_range = (10..10).into();
        assert!(state.apply_completion_with_edits(8..10, "foo", &[], cx));
        assert_eq!(state.text().to_string(), "let v = foo\n");
    });
}

/// hover 卡片里的长段落必须换行，且卡片宽度不超过上限。
///
/// 回归：测量只给 `MinContent` / `MaxContent` 时 gpui 的文本一律不换行
/// （`TextLayout::layout` 的 `wrap_width` 只在 `AvailableSpace::Definite` 下
/// 才有值），卡片被自己的 `max_w` 夹成 680、里面却只有一行高，文字与代码块
/// 全溢出到卡片外面（用户报的「文本不换行 / 代码块比浮窗宽」）。
#[gpui::test]
fn hover_card_wraps_long_paragraph(cx: &mut gpui::TestAppContext) {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(40);
    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, cx| {
        let info = crate::editor::hover::HoverInfo::from_plain_text(long, cx);
        state.set_hover_popover(7, info);
    });
    let bounds = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    assert!(
        bounds.size.width <= px(crate::editor::hover::MAX_WIDTH),
        "卡片宽度不能超过上限: {bounds:?}"
    );
    assert!(
        bounds.size.height > px(100.),
        "长段落必须换行（不换行只有一行高，内容溢出卡片）: {bounds:?}"
    );
    assert!(
        bounds.size.height <= px(crate::editor::hover::MAX_HEIGHT),
        "高度仍受 max_h 约束: {bounds:?}"
    );
}

/// 代码块同样要在卡片宽度内换行，不能横向溢出到卡片外面。
#[gpui::test]
fn hover_card_code_block_stays_within_card_width(cx: &mut gpui::TestAppContext) {
    let line = "let x: HashMap<String, Vec<Vec<HashMap<String, u8>>>> = HashMap::new(); ";
    let contents = format!("```rust\n{}\n```", line.repeat(6));
    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, cx| {
        let info = crate::editor::hover::HoverInfo::from_plain_text(contents, cx);
        state.set_hover_popover(7, info);
    });
    let bounds = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    assert!(
        bounds.size.width <= px(crate::editor::hover::MAX_WIDTH),
        "代码块不能把卡片撑宽: {bounds:?}"
    );
    assert!(
        bounds.size.height > px(120.),
        "长代码行必须在卡片内换行（不换行时只有 81px 高）: {bounds:?}"
    );
}

/// 回归：hover 卡片里的长文档注释必须在**卡内折行**，不能被右侧裁掉。
///
/// 根因：卡片宽度曾由内容撑开（auto），markdown 段落的
/// `w_full`/`max_w_full` 百分比链解析不出定宽 —— gpui 的文本只有在
/// `known_dimensions.width` / `AvailableSpace::Definite` 时才设置
/// `wrap_width`，于是长段落按自然宽度（实测 4000+ px）排成一行，再被卡片的
/// `overflow_x_hidden` 裁掉。现在卡片在最终渲染时带显式宽度（两趟测量），
/// 文本片段宽度必须落在卡片内，且高度随折行增长。
#[gpui::test]
fn hover_card_paragraph_wraps_inside_card(cx: &mut gpui::TestAppContext) {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(40);
    let (input, frag) = draw_with_probe(
        "fn main() {\n    let x = 1;\n}\n",
        cx,
        |state, cx| {
            let info = crate::editor::hover::HoverInfo::from_plain_text(long, cx);
            state.set_hover_popover(7, info);
        },
        |cx| cx.debug_bounds("md-frag"),
    );
    let card = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    // 编辑区只有 600px 宽：卡片必须夹到「视口 - 两侧留白」= 584，
    // 不能按 `MAX_WIDTH`（680）排版——那会让卡片右侧落到视口外。
    assert!(
        card.size.width <= px(584.),
        "卡片宽度必须夹到编辑区视口内: card={card:?}"
    );
    let frag = frag.expect("长段落应渲染出文本片段");
    assert!(
        frag.size.width <= card.size.width,
        "文本片段不能比卡片宽（不换行时是 4000+ px）: frag={frag:?} card={card:?}"
    );
    assert!(
        frag.size.height > px(40.),
        "长段落必须在卡内折行（不换行只有一行高）: frag={frag:?}"
    );
}

/// 回归：链接文本同样要在卡内折行。裸 `StyledText` 是 flex item，默认按
/// max-content 取宽，长链接会整行溢出卡片被裁掉。
#[gpui::test]
fn hover_card_link_text_wraps_inside_card(cx: &mut gpui::TestAppContext) {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(20);
    let contents = format!("[{long}](https://example.com)");
    let (input, link) = draw_with_probe(
        "fn main() {\n    let x = 1;\n}\n",
        cx,
        |state, cx| {
            let info = crate::editor::hover::HoverInfo::from_plain_text(contents, cx);
            state.set_hover_popover(7, info);
        },
        |cx| cx.debug_bounds("md-link"),
    );
    let card = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    let link = link.expect("链接元素应渲染");
    assert!(
        link.size.width <= card.size.width,
        "链接块不能比卡片宽: link={link:?} card={card:?}"
    );
    assert!(
        link.size.height > px(40.),
        "长链接文本必须在卡内折行（不换行只有一行高）: link={link:?}"
    );
}

/// 回归：列表不能把整套 hover 卡片塌成一条窄柱。
///
/// 根因：列表项正文容器是 `flex_1().w_0()`（flex-basis 0），**宽度测量**
/// 里拿不到文本的自然宽度，整张卡片只会量到「项目符号 + 内边距」（实测
/// 66px），正文被压成逐字换行。改成 `flex_basis: auto` 后卡片应接近正文宽度。
#[gpui::test]
fn hover_card_list_keeps_usable_width(cx: &mut gpui::TestAppContext) {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(20);
    let contents = format!("- {long}");
    let (input, frag) = draw_with_probe(
        "fn main() {\n    let x = 1;\n}\n",
        cx,
        |state, cx| {
            let info = crate::editor::hover::HoverInfo::from_plain_text(contents, cx);
            state.set_hover_popover(7, info);
        },
        |cx| cx.debug_bounds("md-frag"),
    );
    let card = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    assert!(
        card.size.width > px(200.),
        "列表内容不能把卡片塌成窄柱（此前只有 66px）: card={card:?}"
    );
    let frag = frag.expect("列表项文本应渲染");
    assert!(
        frag.size.width <= card.size.width && frag.size.width > px(100.),
        "列表文本应在卡内换行而不是塌成 0 宽: frag={frag:?} card={card:?}"
    );
}

/// 短内容的卡片仍然只占内容宽，不被撑满上限。
///
/// 定宽测量不会把浮层撑满 `max_w`：浮层是 flex 容器，宽度取子内容的
/// cross size（taffy `determine_container_cross_size`），可用宽度只作为对
/// 子内容的约束往下传。
#[gpui::test]
fn hover_card_shrinks_to_short_content(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, cx| {
        let info = crate::editor::hover::HoverInfo::from_plain_text("hover", cx);
        state.set_hover_popover(7, info);
    });
    let bounds = input
        .read_with(cx, |s, _| s.hover_popover_bounds())
        .expect("hover card must be laid out");
    assert!(
        bounds.size.width < px(200.),
        "短内容不该被撑到上限宽度: {bounds:?}"
    );
}

/// 各种长度下都要成立：宽度不超上限、高度随内容增长（= 换行确实在生效）。
///
/// 单看一个长度不够 —— 「不换行」与「换行」在超长内容上都可能撞到同一个
/// 宽度上限，只有高度的**增长**能区分二者。
#[gpui::test]
fn hover_card_wraps_at_every_length(cx: &mut gpui::TestAppContext) {
    let mut heights = Vec::new();
    for n in [10usize, 20, 40] {
        let long = "the quick brown fox jumps over the lazy dog ".repeat(n);
        let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |state, cx| {
            let info = crate::editor::hover::HoverInfo::from_plain_text(long, cx);
            state.set_hover_popover(7, info);
        });
        let bounds = input
            .read_with(cx, |s, _| s.hover_popover_bounds())
            .expect("hover card must be laid out");
        assert!(
            bounds.size.width <= px(crate::editor::hover::MAX_WIDTH),
            "n={n}: 卡片宽度不能超过上限: {bounds:?}"
        );
        // 不换行时整张卡片只有一行高（46px）。
        assert!(
            bounds.size.height > px(70.),
            "n={n}: 段落必须换行（不换行时只有 46px 高）: {bounds:?}"
        );
        heights.push(bounds.size.height);
    }
    assert!(
        heights[2] > heights[0],
        "内容翻四倍后卡片应当更高（换行在生效）: {heights:?}"
    );
}

/// 指针压在行号 gutter 上时取不到 hover 目标。
///
/// Zed 靠 `text_hitbox.is_hovered` 分流：gutter 上走 `hover_at(..., None, ...)`
/// （作废在飞请求 / 排延迟收起），正文上才走 `hover_at(Some(anchor))`。我们
/// gutter 与正文共用一个 `on_mouse_move`，没有独立 hitbox，改用
/// `content_left_x` 复现同一条分界线。
///
/// 少了这一支：`index_for_mouse_position` 会把 gutter 上的 x 钳回正文左缘、
/// 当成「正在 hover 第一个字符」，于是一张卡片弹在行号栏上并一直挂着。
#[gpui::test]
fn hover_target_is_none_over_the_line_numbers(cx: &mut gpui::TestAppContext) {
    let input = draw_with("fn main() {\n    let x = 1;\n}\n", cx, |_, _| {});
    let (content_left, top) = input.read_with(cx, |state, _| {
        (state.content_left_x(), state.input_bounds.origin.y)
    });
    // 前提：行号栏确实有宽度，否则下面的断言退化成「输入框外」的判断。
    assert!(
        input.read_with(cx, |state, _| {
            state
                .last_layout
                .as_ref()
                .map(|layout| layout.line_number_width)
                .unwrap_or(px(0.))
        }) > px(0.),
        "测试前提：代码编辑器必须渲染行号栏"
    );

    let over_gutter = input.read_with(cx, |state, _| {
        state.hover_offset_for_mouse(gpui::point(content_left - px(1.), top + px(30.)))
    });
    assert_eq!(
        over_gutter, None,
        "行号栏上不该有 hover 目标（正文左缘以左一律算 gutter）"
    );

    // 正文上必须取得到，且不随 y 挑剔（行上方钳到行首、行下方钳到文末，
    // 都仍是「在正文上」）。
    for y in [0., 10., 30., 60., 120., 400.] {
        let offset = input.read_with(cx, |state, _| {
            state.hover_offset_for_mouse(gpui::point(content_left, top + px(y)))
        });
        assert!(offset.is_some(), "正文左缘（y = {y}）必须能取到 hover 目标");
    }
}

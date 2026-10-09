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

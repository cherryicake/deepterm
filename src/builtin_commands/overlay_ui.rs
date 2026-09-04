//! 内建命令浮层（overlay）的 UI 渲染。
//!
//! 从 mod.rs 拆出：mod.rs 保留状态机与业务逻辑，本文件只负责浮层
//! DOM 构建（key hints / 历史行 / 命令列表）。

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, IntoElement, MouseButton, ParentElement, Pixels, ScrollHandle,
    StatefulInteractiveElement, Styled, Transformation, div, ease_in_out, percentage, px, svg,
};

use crate::builtin_commands::registry::BuiltinCommand;
use crate::builtin_commands::history::HistoryEntry;
use crate::builtin_commands::path_completion::{CompletionKind, PathCompletionItem};
use crate::theme::UiTheme;

fn render_key_hint(key: &str, label: Option<&str>, theme: UiTheme) -> impl IntoElement {
    let key = key.to_string();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .px(px(7.0))
                .py(px(2.0))
                .bg(theme.elevated_background)
                .rounded(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.text)
                .child(key),
        )
        .when_some(label, |this, label| {
            let label = label.to_string();
            this.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(label),
            )
        })
}

/// 浮窗垂直锚点：贴住光标的哪一边。
///
/// 终端覆盖光标在父容器顶部 → 用 `Top` 让浮窗紧贴光标行下方，列表向下展开；
/// 终端覆盖光标在父容器底部 → 用 `Bottom` 让浮窗底端紧贴光标行上方，
/// 列表从下往上增长。两种锚点都保证「靠近光标的一边」始终贴合光标。
#[derive(Clone, Copy, Debug)]
pub enum OverlayAnchor {
    /// 距父容器顶端的偏移（`.top(px)`）
    Top(Pixels),
    /// 距父容器底端的偏移（`.bottom(px)`）
    Bottom(Pixels),
}

/// 把锚点应用到一个绝对定位的 `div`。
fn apply_overlay_anchor<E: gpui::Styled>(element: E, anchor: OverlayAnchor) -> E {
    match anchor {
        OverlayAnchor::Top(top) => element.top(top),
        OverlayAnchor::Bottom(bottom) => element.bottom(bottom),
    }
}

/// 把 `last_used`（unix 秒）转成人类可读的相对时间，用于历史条目右侧次要文字。
fn relative_time_label(last_used: i64, now: i64) -> String {
    let diff = (now - last_used).max(0);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86_400)
    }
}

/// 渲染「最近使用」历史分组的行集合，置于补全列表顶部。
///
/// `history_selected` 是历史段内的高亮下标（`None` ⇒ 历史段无高亮）。
/// `on_pick_history(i)` 由外层提供，响应鼠标点击历史项第 `i` 条。
fn render_history_rows<F>(
    entries: &[HistoryEntry],
    history_selected: Option<usize>,
    theme: UiTheme,
    is_dark: bool,
    on_pick_history: Rc<F>,
) -> Vec<gpui::AnyElement>
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    if entries.is_empty() {
        return Vec::new();
    }
    let now = crate::builtin_commands::history::current_unix_secs();
    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(entries.len() + 1);

    // 分组标题
    rows.push(
        div()
            .px(px(8.0))
            .py(px(2.0))
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child("最近使用")
            .into_any_element(),
    );

    for (index, entry) in entries.iter().enumerate() {
        let on_pick = on_pick_history.clone();
        let is_selected = Some(index) == history_selected;
        let time_label = relative_time_label(entry.last_used, now);
        rows.push(
            div()
                .w_full()
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(if is_selected {
                    theme.accent.opacity(if is_dark { 0.18 } else { 0.12 })
                } else {
                    gpui::transparent_black()
                })
                .when(!is_selected, |this| {
                    this.hover(move |style| {
                        style.bg(theme.accent.opacity(if is_dark { 0.10 } else { 0.06 }))
                    })
                })
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    on_pick(index, window, cx);
                })
                .child(
                    div()
                        .w(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            svg()
                                .size(px(12.0))
                                .text_color(theme.icon_muted)
                                .path(crate::assets::Icon::Clock.path()),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .truncate()
                        .font_family("monospace")
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(entry.command_line.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(time_label),
                )
                .into_any_element(),
        );
    }

    // 分隔线
    rows.push(
        div()
            .h(px(1.0))
            .my(px(2.0))
            .bg(theme.border)
            .into_any_element(),
    );
    rows
}

/// 渲染内建命令建议浮层。
///
/// `selected_index` 为 `None` 时浮层弹出但所有项无高亮（用户尚未按 ↓/Tab）。
/// `on_pick` 由外层提供，用来响应鼠标选中某一条建议。
#[allow(clippy::too_many_arguments)]
pub fn render_overlay<F, H>(
    suggestions: Rc<[Arc<BuiltinCommand>]>,
    selected_index: Option<usize>,
    history: &[HistoryEntry],
    history_selected: Option<usize>,
    anchor: OverlayAnchor,
    theme: UiTheme,
    is_dark: bool,
    on_pick: F,
    on_pick_history: H,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
    H: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);
    let anchored_at_bottom = matches!(anchor, OverlayAnchor::Bottom(_));

    let history_rows = render_history_rows(
        history,
        history_selected,
        theme,
        is_dark,
        Rc::new(on_pick_history),
    );

    let items_list = div().max_h(px(420.0)).children(history_rows).children(
        suggestions
            .iter()
            .cloned()
            .enumerate()
            .map(move |(index, cmd)| {
                let on_pick = on_pick.clone();
                let is_selected = Some(index) == selected_index;
                div()
                    .w_full()
                    .h_6()
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(if is_selected {
                        theme.accent.opacity(if is_dark { 0.16 } else { 0.09 })
                    } else {
                        theme.panel_background
                    })
                    .when(!is_selected, |this| {
                        this.hover(move |style| style.bg(theme.tab_hover_background))
                    })
                    .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        on_pick(index, window, cx);
                    })
                    .child(
                        div()
                            .w(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                svg()
                                    .size(px(10.0))
                                    .text_color(theme.icon_muted)
                                    .path(cmd.icon.path()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(390.0))
                            .text_size(px(13.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(cmd.name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(cmd.description),
                    )
            }),
    );

    let key_hints_row = div()
        .px(px(6.0))
        .flex()
        .items_center()
        .gap_2()
        .child(render_key_hint("↑↓", Some("navigate"), theme))
        .child(render_key_hint("esc", Some("dismiss"), theme));

    let container = apply_overlay_anchor(div().absolute().left(px(0.0)).right(px(0.0)), anchor)
        .flex()
        .flex_col()
        .gap_0()
        // 浮窗整体使用不透明面板背景，避免终端文本透过来。
        .bg(theme.panel_background)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        });

    if anchored_at_bottom {
        // 浮窗从光标向上展开 ⇒ 按键提示行挪到浮窗底部，紧贴光标行
        container
            .child(items_list)
            .child(div().h(px(1.0)).bg(theme.border))
            .child(key_hints_row)
            .child(div().h(px(1.0)).bg(theme.border))
    } else {
        container
            .child(div().h(px(1.0)).bg(theme.border))
            .child(key_hints_row)
            .child(div().h(px(1.0)).bg(theme.border))
            .child(items_list)
    }
}

/// 渲染路径补全浮层。
///
/// `selected_index` 为 `None` 时浮层弹出但所有项无高亮。
/// `on_pick` 由外层提供，用来响应鼠标选中某一条路径建议。
#[allow(clippy::too_many_arguments)]
pub fn render_path_overlay<F, H>(
    completions: &[PathCompletionItem],
    selected_index: Option<usize>,
    history: &[HistoryEntry],
    history_selected: Option<usize>,
    anchor: OverlayAnchor,
    left: Pixels,
    theme: UiTheme,
    is_dark: bool,
    scroll_handle: &ScrollHandle,
    on_pick: F,
    on_pick_history: H,
) -> impl IntoElement
where
    F: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
    H: Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let on_pick = Rc::new(on_pick);
    let overlay_width = estimate_overlay_width(completions);

    let history_rows = render_history_rows(
        history,
        history_selected,
        theme,
        is_dark,
        Rc::new(on_pick_history),
    );

    // 紧贴命令行光标的圆角弹层。当 `anchor` 为 `Bottom` 时浮窗底端贴近光标，
    // 列表从下往上贴着光标增长。宽度按最长建议项估算（夹紧到 320–900px）。
    apply_overlay_anchor(div().absolute().left(left), anchor)
        .w(px(overlay_width))
        .rounded(px(12.0))
        .bg(theme.notification_background)
        .shadow_lg()
        .flex()
        .flex_col()
        .py(px(4.0))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .id("path-completion-list")
                .max_h(px(272.0))
                .overflow_y_scroll()
                .track_scroll(scroll_handle)
                .px(px(6.0))
                .flex()
                .flex_col()
                .children(history_rows)
                .children(
                    completions
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(move |(index, item)| {
                            let on_pick = on_pick.clone();
                            let is_selected = Some(index) == selected_index;
                            // 占位项（读取中 / 读取失败）不可点击、不悬停高亮
                            let interactive = !item.is_placeholder();
                            let icon_path =
                                item.icon
                                    .map(|i| i.path())
                                    .unwrap_or_else(|| match item.kind {
                                        CompletionKind::Directory => {
                                            crate::assets::Icon::Folder.path()
                                        }
                                        CompletionKind::Host => crate::assets::Icon::Host.path(),
                                        CompletionKind::File => crate::assets::Icon::File.path(),
                                    });
                            div()
                                .w_full()
                                .h(px(28.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .rounded(px(6.0))
                                .when(interactive, |this| this.cursor_pointer())
                                .bg(if is_selected {
                                    theme.accent.opacity(if is_dark { 0.18 } else { 0.12 })
                                } else {
                                    gpui::transparent_black()
                                })
                                .when(!is_selected && interactive, |this| {
                                    this.hover(move |style| {
                                        style.bg(theme.accent.opacity(if is_dark {
                                            0.10
                                        } else {
                                            0.06
                                        }))
                                    })
                                })
                                .when(interactive, |this| {
                                    this.on_mouse_up(MouseButton::Left, move |_, window, cx| {
                                        cx.stop_propagation();
                                        on_pick(index, window, cx);
                                    })
                                })
                                .child({
                                    let svg_element = svg()
                                        .size(px(16.0))
                                        .text_color(theme.text_muted)
                                        .path(icon_path);
                                    if item.icon == Some(crate::assets::Icon::ProgressIndicator) {
                                        div()
                                            .w(px(16.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                svg_element.with_animation(
                                                    "progress-rotate",
                                                    Animation::new(
                                                        std::time::Duration::from_millis(1000),
                                                    )
                                                    .repeat()
                                                    .with_easing(ease_in_out),
                                                    |this, delta| {
                                                        this.with_transformation(
                                                            Transformation::rotate(percentage(
                                                                delta,
                                                            )),
                                                        )
                                                    },
                                                ),
                                            )
                                    } else {
                                        div()
                                            .w(px(16.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(svg_element)
                                    }
                                })
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(13.5))
                                        .text_color(theme.text)
                                        .child(item.name.clone()),
                                )
                                // 仅 Host 条目附带 nickname / 描述（"This machine"、保存的别名等）；
                                // 文件 / 目录条目只显示文件名
                                .when_some(host_description(&item), |this, desc| {
                                    this.child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .truncate()
                                            .text_size(px(12.5))
                                            .text_color(theme.text_muted)
                                            .child(desc),
                                    )
                                })
                        }),
                ),
        )
}

/// 估算路径补全浮层应当占用的宽度，让最长的"name + 描述"在 UI 边界以内完整展示。
///
/// 以字符宽度粗算（monospace 13.5px ≈ 8.5px/char、proportional 12.5px ≈ 7px/char），
/// 再加上 padding/icon/gap 的固定开销。算出的值会被 [`OVERLAY_MIN_WIDTH`] /
/// [`OVERLAY_MAX_WIDTH`] 夹紧，避免极端情况撑爆窗口。
fn estimate_overlay_width(items: &[PathCompletionItem]) -> f32 {
    // 横向布局：padding(10*2) + icon(16) + icon→name gap(10) + name→desc gap(10) + 余量(12)
    const FIXED_CHROME: f32 = 10.0 + 10.0 + 16.0 + 10.0 + 12.0;
    const NAME_CHAR: f32 = 8.5;
    const DESC_CHAR: f32 = 7.0;

    // 用 unicode-width 计算显示宽度：CJK 等双宽字符占 2 列，
    // 简单 chars().count() 会低估导致浮窗过窄、长条目被裁剪
    let text_cells = |s: &str| unicode_width::UnicodeWidthStr::width(s) as f32;

    let needed = items
        .iter()
        .map(|item| {
            let name_w = text_cells(&item.name) * NAME_CHAR;
            let desc_w = host_description(item)
                .map(|d| 10.0 + text_cells(&d) * DESC_CHAR)
                .unwrap_or(0.0);
            FIXED_CHROME + name_w + desc_w
        })
        .fold(OVERLAY_MIN_WIDTH, f32::max);

    needed.min(OVERLAY_MAX_WIDTH)
}

/// Host 条目展示在 IP 右侧的次要文字（nickname / `Local` 标签）；
/// 文件 / 目录条目返回 `None`，让 UI 只渲染文件名。
fn host_description(item: &PathCompletionItem) -> Option<String> {
    if item.kind != CompletionKind::Host {
        return None;
    }
    item.description.clone()
}

const OVERLAY_MIN_WIDTH: f32 = 320.0;
const OVERLAY_MAX_WIDTH: f32 = 900.0;

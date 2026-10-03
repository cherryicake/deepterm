//! LSP 自动补全。
//!
//! 补全有两块状态，分开放：
//! - [`CompletionState`] 挂在 `Editor` 上：候选列表、选中项、可见性。它需要
//!   `Window`（定位、重绘）与发起请求的能力；
//! - `InputState` 上的 pending 槽：后台请求的结果先落那里，由 Editor 取走。
//!   这样后台任务不必持有 UI 实体的强引用。
//!
//! 触发方式（与 VSCode 一致）：
//! - **自动**：输入标识符字符（字母/数字/`_`）时按防抖请求；
//! - **手动**：`ctrl-space` 立即请求。
//!
//! 插入时优先用服务器给的 `textEdit`（它会精确替换掉用户已输入的前缀），
//! 没有则替换光标前的整个词。

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div, px};
use lsp_types::{CompletionItem, Url};

use crate::components::input::InputState;
use crate::lsp::client::LspClient;
use crate::lsp::features;
use crate::theme::ActiveTheme as _;

/// 自动触发的防抖窗口。
const AUTO_DELAY: Duration = Duration::from_millis(120);

/// 菜单最大可见条目数，超出滚动。
const MAX_VISIBLE: usize = 12;

/// 一条候选。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 主标签（左栏）。
    pub label: String,
    /// 右侧的补充信息（类型/所属），可为 None。
    pub detail: Option<String>,
    /// 插入后光标相对插入文本末端的偏移调整。
    /// 服务器不要求时保持默认（光标落在插入文本末尾）。
    pub item: CompletionItem,
}

/// 补全菜单状态。
pub struct CompletionState {
    /// 当前候选列表。
    candidates: Vec<Candidate>,
    /// 选中下标。
    selected: usize,
    /// 补全要替换的字节区间（服务器给的或按词推断的）。
    replace_range: Range<usize>,
    /// 是否可见。
    visible: bool,
    /// 防抖任务。
    task: Option<gpui::Task<()>>,
    /// 触发时用的 uri（结果回来时校验文件没被切走）。
    uri: Option<Url>,
}

impl Default for CompletionState {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionState {
    pub fn new() -> Self {
        Self {
            candidates: Vec::new(),
            selected: 0,
            replace_range: 0..0,
            visible: false,
            task: None,
            uri: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 隐藏并清空（不清 `InputState`，见 [`Self::hide_in`]）。
    pub fn hide(&mut self) {
        self.visible = false;
        self.candidates.clear();
        self.selected = 0;
        self.task.take();
    }

    /// 隐藏并清掉 `InputState` 上的快照（绘制读的是那边）。
    pub fn hide_in(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        self.hide();
        editor.update(cx, |state, _cx| state.clear_completion_menu());
    }

    /// 选中项。
    pub fn selected(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// 上移 / 下移（循环）。
    pub fn move_selection(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(len)) as usize;
    }

    /// 请求补全（手动触发，无防抖）。
    pub fn trigger(
        &mut self,
        editor: &Entity<InputState>,
        client: Option<Arc<LspClient>>,
        uri: Url,
        cx: &mut App,
    ) {
        // 手动触发（ctrl-space）：不带触发字符。
        self.fetch(editor, client, uri, None, None, cx);
    }

    /// 输入后触发（带防抖）。
    pub fn trigger_auto(
        &mut self,
        editor: &Entity<InputState>,
        client: Option<Arc<LspClient>>,
        uri: Url,
        trigger_character: Option<String>,
        cx: &mut App,
    ) {
        self.fetch(editor, client, uri, trigger_character, Some(AUTO_DELAY), cx);
    }

    fn fetch(
        &mut self,
        editor: &Entity<InputState>,
        client: Option<Arc<LspClient>>,
        uri: Url,
        trigger_character: Option<String>,
        delay: Option<Duration>,
        cx: &mut App,
    ) {
        let Some(client) = client else {
            return;
        };
        self.task.take();
        self.uri = Some(uri.clone());

        let editor = editor.downgrade();
        let offset = editor
            .update(cx, |state, _| state.cursor())
            .unwrap_or(0);

        self.task = Some(cx.spawn(async move |cx| {
            if let Some(delay) = delay {
                smol::Timer::after(delay).await;
            }
            // 防抖后重读光标：期间用户可能继续打字或移开了光标。
            let Some(position) = editor
                .update(cx, |state, _| {
                    features::offset_to_position(state.text(), state.cursor())
                })
                .ok()
            else {
                return;
            };
            let _ = offset;

            // 把触发字符原样传给服务器（对照 LSP 的
            // `CompletionContext.triggerCharacter`）：服务器据此决定返回哪一
            // 类候选（例如 `:` 后只给属性名）。此前恒传 `None`，服务器无法
            // 区分「用户敲了触发字符」与「手动请求补全」。
            match client
                .completion(&uri, position, trigger_character)
                .await
            {
                Ok((items, _incomplete)) => {
                    editor
                        .update(cx, |state, _| {
                            state.set_pending_completions(items, position);
                        })
                        .ok();
                }
                Err(e) => {
                    // 降级：补全失败只是没有候选，不影响编辑。
                    log::debug!("lsp completion failed: {e}");
                }
            }
        }));
    }

    /// 取走后台结果（Editor 每帧调用）。
    pub fn take_pending(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        let Some((items, _position)) = editor.update(cx, |state, _| state.take_pending_completions())
        else {
            return;
        };
        if items.is_empty() {
            self.hide_in(editor, cx);
            return;
        }
        let (replace_range, cursor) =
            editor.update(cx, |state, _| (state.completion_replace_range_for(&items), state.cursor()));
        self.candidates = items
            .into_iter()
            .map(|item| Candidate {
                label: features::completion_label(&item),
                detail: item.detail.clone().or(item.label_details.as_ref().and_then(|d| d.detail.clone())),
                item,
            })
            .collect();
        self.replace_range = replace_range;
        self.selected = 0;
        self.visible = true;

        // 把锚点与候选快照写进 `InputState`：绘制发生在 `TextElement` 里，
        // 那里只拿得到 `Entity<InputState>`。
        let candidates = self.candidates.clone();
        editor.update(cx, |state, _cx| {
            state.set_completion_anchor(cursor);
            state.set_completion_menu(candidates, 0);
        });
    }

    /// 同步选中项到 `InputState`（上下移动后绘制要跟着变）。
    pub fn sync_selection(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        if !self.visible {
            return;
        }
        let candidates = self.candidates.clone();
        let selected = self.selected;
        editor.update(cx, |state, _cx| {
            state.set_completion_menu(candidates, selected);
        });
    }

    /// 应用选中的候选：替换区间并写入文本。
    pub fn confirm(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        let Some(candidate) = self.selected().cloned() else {
            self.hide();
            return;
        };
        let insert = features::completion_insert_text(&candidate.item);
        let range = self.replace_range.clone();
        editor.update(cx, |state, cx| {
            if !state.apply_completion(range, &insert, cx) {
                // 区间失效（文本被改短）：放弃本次补全，不写坏缓冲区。
                log::debug!("lsp: stale completion range, discarding");
            }
        });
        self.hide();
    }

    /// 当前 uri（用于校验结果来源）。
    pub fn uri(&self) -> Option<&Url> {
        self.uri.as_ref()
    }
}

/// 补全菜单的只读快照（绘制用）。
///
/// 绘制发生在 `TextElement` 里，那里只拿得到 `Entity<InputState>`；
/// 用一个不含任务/句柄的纯数据视图，避免把整个 `CompletionState` 暴露出去。
pub struct CompletionMenu {
    pub candidates: Vec<Candidate>,
    pub selected: usize,
}

/// 渲染补全菜单**内容**（不含定位），从 `InputState` 取状态。
///
/// 定位由 `TextElement::paint_completion_menu` 在绘制阶段算：那里才有
/// 当前帧的行布局与滚动偏移（照抄 Zed 的 `layout_hover_popovers` 模型）。
pub fn render_completion_menu_from_state(
    state: &crate::components::input::InputState,
    cx: &App,
) -> Option<gpui::AnyElement> {
    let menu = state.completion_menu()?;
    render_completion_rows(menu, cx)
}

/// 渲染补全菜单**内容**（不含定位）。
///
/// 条目数超过 [`MAX_VISIBLE`] 时按窗口滚动。
pub fn render_completion_rows(
    menu: &CompletionMenu,
    cx: &App,
) -> Option<gpui::AnyElement> {
    let theme = cx.theme().ui;

    // 窗口滚动：保证选中项始终可见。
    let start = if menu.selected >= MAX_VISIBLE {
        menu.selected + 1 - MAX_VISIBLE
    } else {
        0
    };
    let end = (start + MAX_VISIBLE).min(menu.candidates.len());

    let mut el = div()
        .id("lsp-completion-menu")
        .min_w(px(240.0))
        .max_w(px(420.0))
        .max_h(px(280.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .py(px(4.0))
        .rounded(px(8.0))
        .bg(theme.panel_background)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_size(px(12.0));

    for (i, candidate) in menu.candidates[start..end].iter().enumerate() {
        let index = start + i;
        let is_selected = index == menu.selected;
        let mut row = div()
            .id(("lsp-completion-item", index))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(2.0))
            .whitespace_nowrap()
            .rounded(px(4.0))
            .child(
                div()
                    .text_color(if is_selected { theme.text } else { theme.text_muted })
                    .child(candidate.label.clone()),
            );

        if is_selected {
            row = row.bg(theme.accent.opacity(0.18));
        }
        if let Some(detail) = &candidate.detail {
            row = row.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .overflow_hidden()
                    .child(detail.clone()),
            );
        }
        el = el.child(row);
    }

    Some(el.into_any_element())
}

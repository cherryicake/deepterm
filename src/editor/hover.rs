//! LSP 悬浮提示浮层。
//!
//! 鼠标停在标识符上一段时间后，向服务器请求 `textDocument/hover`，把返回的
//! markdown / 纯文本渲染成一张小卡片。
//!
//! **定位模型照抄 Zed**（`editor/src/hover_popover.rs` +
//! `editor/src/element.rs::layout_hover_popovers`）：
//!
//! 1. 浮层状态只记录**文本锚点**（命中字符的字节偏移），不记录像素坐标。
//!    像素坐标在编辑器每次布局时都会变（滚动、软换行、行号栏宽度变化），
//!    存下来必然过期——这正是之前浮层位置错乱的根因。
//! 2. 真正绘制时由 `TextElement::paint_inner` 用**当前帧的**行布局把锚点
//!    换算成 `(x, y)`：`x` 由该行 `ShapedLine::x_for_index` 给出，`y` 由
//!    行号 × 行高给出，再减去滚动偏移、加上内容原点。
//! 3. 用 `window.defer_draw(element, origin, 2, None)` 画到绝对坐标上。
//!    `defer_draw` 的坐标就是**窗口坐标**，与鼠标事件同一空间，不再需要
//!    在两种坐标系之间换算。
//! 4. 优先画在命中行**上方**；上方放不下才画到下方（Zed 的
//!    `can_place_above` / `can_place_below` 判定）。
//!
//! 交互约定：
//! - **防抖**：鼠标移动是连续事件，每次都发请求会把服务器打挂，停稳
//!   [`HOVER_DELAY`] 才发；
//! - **不抢焦点**：浮层不能聚焦，否则鼠标一进浮层编辑器就失焦。

use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, Entity, IntoElement, ParentElement as _, Styled as _, div, px,
};
use lsp_types::Url;
use ropey::Rope;

use crate::components::input::InputState;
use crate::lsp::features::{self, hover_contents};
use crate::theme::ActiveTheme as _;

/// 鼠标停稳多久才发 hover 请求。
pub const HOVER_DELAY: Duration = Duration::from_millis(300);

/// 浮层最大宽度（px）。
pub const MAX_WIDTH: f32 = 520.0;
/// 浮层最大高度（px）；超出后截断。
pub const MAX_HEIGHT: f32 = 320.0;

/// 一次 hover 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct HoverInfo {
    /// 展示文本（可能是 markdown，含 ``` 代码块）。
    pub contents: String,
    /// 命中的源区间，用于在编辑器里高亮。
    pub range: Option<Range<usize>>,
}

/// 鼠标移出编辑区后，延迟多久收起 hover 浮层。
///
/// 对照 Zed 的 `hover_popover_hiding_delay`（默认 300ms 左右）。之所以要延迟
/// 而不是立刻消失：用户常把鼠标往浮层方向移去点里面的链接/按钮，立刻收起会让
/// 目标在指针到达前就没了。
const HOVER_HIDING_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// hover 浮层的状态机。
///
/// 挂在 `Editor` 上：请求需要 `InputState`（文本）与 LSP 会话，
/// 绘制需要 `Window`。
pub struct HoverState {
    /// 锚点：命中字符的字节偏移。
    ///
    /// **只存文本偏移，不存像素**——像素坐标随滚动/换行/行号栏宽度变化，
    /// 存下来会过期（Zed 同样只存 `Anchor`）。
    anchor_offset: usize,
    /// 防抖任务：新的一次移动会替换（取消）它。
    task: Option<gpui::Task<()>>,
    /// 延迟隐藏任务：鼠标移到空白处时启动，到期才真正收起。
    ///
    /// 对照 Zed 的 `hiding_delay_task`（`hover_popover.rs`）。收起得「延迟」而不是
    /// 「立刻」，是为了让指针有机会移到浮层上操作里面的内容。
    hiding_delay_task: Option<gpui::Task<()>>,
    /// 上次请求的目标位置，用于避免鼠标在同一字符内微动时重复请求。
    last_offset: Option<usize>,
    /// 请求代号：每次 `on_mouse_moved` 自增。
    ///
    /// **用来作废在飞的后台任务**。GPUI 的 `Task` 被 drop 只是 detach、**不会
    /// 取消**（同一个坑见 `TerminalView::wsl_scan_seq`），所以 `self.task.take()`
    /// 拦不住旧任务：它照样会跑完、照样会回写结果。
    ///
    /// 不作废的后果很具体：鼠标从符号划到空白，那条「无内容 → 收起浮层」的
    /// 旧任务若晚于新请求返回，就会把**刚弹出**的卡片擦掉；任务越积越多，
    /// 最后一个返回的总是过期的，于是浮层再也不出现。
    request_generation: u64,
}

impl Default for HoverState {
    fn default() -> Self {
        Self::new()
    }
}

impl HoverState {
    pub fn new() -> Self {
        Self {
            anchor_offset: 0,
            task: None,
            hiding_delay_task: None,
            last_offset: None,
            request_generation: 0,
        }
    }

    /// 浮层的文本锚点（字节偏移）。
    pub fn anchor_offset(&self) -> usize {
        self.anchor_offset
    }

    /// 隐藏浮层并取消在途请求（不清 `InputState`，见 [`Self::hide_in`]）。
    pub fn hide(&mut self) {
        self.task.take();
        self.hiding_delay_task = None;
        self.last_offset = None;
    }

    /// 安排一次**延迟隐藏**（鼠标移到空白处时调用）。
    ///
    /// 已在隐藏中就不重复起计时器——否则连续扫过一片空白会攒出大量任务。
    ///
    /// `owner` 是发起隐藏的那个实体（`Editor`）的弱引用：`HoverState` 只是它
    /// 的字段，自己拿不到自己的实体句柄。延迟到期后由 `on_expired` 回调完成
    /// 真正的收起 —— 回调由 `Editor` 侧提供，那里才有权改`InputState` 上的
    /// 浮层状态（绘制读的是那边，见 [`Self::hide_in`]）。
    pub fn schedule_hide<F>(&mut self, cx: &mut gpui::App, on_expired: F)
    where
        F: FnOnce(&mut gpui::AsyncApp) + Send + 'static,
    {
        if self.hiding_delay_task.is_some() {
            return;
        }
        self.hiding_delay_task = Some(cx.spawn(async move |cx| {
            cx.background_executor().timer(HOVER_HIDING_DELAY).await;
            on_expired(cx);
        }));
    }

    /// 立即收起浮层（延迟到期，或调用方明确要求立刻隐藏时）。
    pub fn hide_now(&mut self) {
        self.hiding_delay_task = None;
        self.task.take();
        self.last_offset = None;
        log::info!("lsp: hover 浮层已收起（延迟到期）");
    }

    /// 鼠标移动：重置防抖，停稳后发请求。
    ///
    /// `offset` 是鼠标位置换算出的字节偏移；同一偏移内的重复移动不会
    /// 重新请求（鼠标抖动不应产生流量）。
    ///
    /// `client` 为 `None`（该语言无服务器）时是空操作——调用方不需要
    /// 先判断有没有 LSP。
    pub fn on_mouse_moved(
        &mut self,
        offset: usize,
        editor: &Entity<InputState>,
        client: Option<std::sync::Arc<crate::lsp::client::LspClient>>,
        uri: Url,
        cx: &mut App,
    ) {
        let Some(client) = client else {
            return;
        };
        // 同一字符内重复移动：不重发（鼠标抖动不该产生流量）。
        if self.last_offset == Some(offset) {
            return;
        }
        // 锚点立刻更新：这样请求回来后浮层就落在当前字符上。
        self.anchor_offset = offset;
        self.last_offset = Some(offset);
        // 替换即取消上一次的防抖。注意这**只是 detach**（GPUI 语义），
        // 真正作废靠 `request_generation` —— 任务回写前会比对代号。
        self.task.take();
        self.request_generation = self.request_generation.wrapping_add(1);
        let generation = self.request_generation;
        // 新一轮查询开始，撤销上一轮的延迟隐藏（否则正在显示的卡片会在
        // 鼠标还在符号上时被收走）。对照 Zed `hover_popover.rs` 的
        // `show_hover`：它进来就会把 `hiding_delay_task` 置空。
        self.hiding_delay_task = None;

        let editor = editor.downgrade();
        // 另留一份弱引用：收起浮层后要 notify 才会重绘，而 `WeakEntity`
        // 本身没有 `notify`（那是 `Context` 的方法），得回到实体上调用。
        let editor_for_notify = editor.clone();
        // 代号要比对，放在任务够得到的 `InputState` 上。
        editor
            .update(cx, |state, _| state.set_hover_request_generation(generation))
            .ok();
        self.task = Some(cx.spawn(async move |cx| {
            smol::Timer::after(HOVER_DELAY).await;

            // 代号已变 = 期间又发了新请求（鼠标继续移动）：这次的结果过期，
            // 必须整体丢弃。`smol::Timer` 醒来不会自动取消，所以这个检查点
            // 不可省 —— 漏掉它就会用旧位置的响应覆盖新位置的状态。
            let still_current = editor
                .update(cx, |state, _| {
                    state.hover_request_generation() == generation
                })
                .unwrap_or(false);
            if !still_current {
                log::info!("lsp: hover 请求已过期（防抖后），丢弃");
                return;
            }

            // 防抖结束后重新读文本：期间用户可能又打了字，位置会漂移。
            let Some(position) = editor
                .update(cx, |state, _| {
                    features::offset_to_position(state.text(), offset)
                })
                .ok()
            else {
                return;
            };

            let result = client.hover(&uri, position).await;

            // 响应回来时再验一次代号：等待期间可能又发了新请求。
            // 代号存在 `InputState` 上（任务拿得到弱引用），`HoverState` 自身
            // 在后台任务里不可达。
            let still_current = editor
                .update(cx, |state, _| {
                    state.hover_request_generation() == generation
                })
                .unwrap_or(false);
            if !still_current {
                log::info!(
                    "lsp: hover 响应已过期，丢弃 pos={}:{}",
                    position.line,
                    position.character
                );
                return;
            }

            let Ok(Some(hover)) = result else {
                // **空白处（服务器无内容）必须把已显示的浮层收掉。**
                //
                // 此前这里只是 `return`，于是鼠标从符号移到空白行/空格时，
                // 旧卡片就一直挂着 —— 用户看到的是「弹窗不会自动关闭」。
                let hidden = editor
                    .update(cx, |state, _cx| state.clear_hover_popover())
                    .is_ok();
                if hidden {
                    log::info!(
                        "lsp: hover 无内容 -> 收起已显示浮层 pos={}:{}",
                        position.line, position.character
                    );
                    // 收起发生在 `InputState` 上，重绘由 `Editor` 的 render 读那边
                    // 的状态，所以要显式 notify 让下一帧跑起来。
                    editor_for_notify.update(cx, |_, cx| cx.notify()).ok();
                }
                return;
            };
            log::info!(
                "lsp: hover 返回内容 pos={}:{}",
                position.line,
                position.character
            );

            editor
                .update(cx, |state, cx| {
                    let info = hover_info(state.text(), &hover);
                    // 结果先落进 `InputState` 的槽，由 Editor 取走
                    // （后台任务拿不到 `Editor` 上的 `HoverState`）。
                    state.set_pending_hover(info);
                    // **必须 notify**：`Editor::drain_lsp_results`（真正把结果
                    // 搬到浮层状态的地方）只在 `render` 里调用。没有这次
                    // notify 就不会产生新的一帧，render 不跑，hover 永远
                    // 搬不出来。补全那边曾有同一个缺陷。
                    cx.notify();
                })
                .ok();
        }));
    }

    /// 把后台算好的结果落到 `InputState` 的浮层槽里（由 Editor 每帧调用）。
    ///
    /// `InputState` 是浮层状态的唯一真相源：`TextElement` 只持有
    /// `Entity<InputState>`，绘制时要同时拿到锚点与内容才能定位排版。
    pub fn take_pending(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        let anchor = self.anchor_offset;
        editor.update(cx, |state, _cx| {
            if let Some(info) = state.take_pending_hover() {
                state.set_hover_popover(anchor, info);
            }
        });
    }

    /// 隐藏浮层：同时清掉 `InputState` 上的状态（绘制读的是那边）。
    pub fn hide_in(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        self.hide();
        editor.update(cx, |state, _cx| state.clear_hover_popover());
    }

}

/// 渲染 hover 浮层的**内容**（不含定位），从 `InputState` 取状态。
///
/// 定位交给 `TextElement::paint_inner`：那里才有当前帧的行布局与滚动偏移。
/// 这里只负责把文本排成卡片，并限制最大尺寸。
pub fn render_hover_content_from_state(state: &InputState, cx: &App) -> Option<gpui::AnyElement> {
    let (_, info) = state.hover_popover()?;
    render_hover_card(info, cx)
}

/// 渲染 hover 浮层的**内容**（不含定位）。
///
/// 文本按行渲染，不解析 markdown——hover 内容里最常见的是带 ``` 的
/// 代码块，用等宽字体整块呈现已经足够可读，引入完整的 markdown 渲染
/// 会为此付出不必要的依赖与布局复杂度。
pub fn render_hover_card(info: &HoverInfo, cx: &App) -> Option<gpui::AnyElement> {
    if info.contents.trim().is_empty() {
        return None;
    }
    let theme = cx.theme().ui;

    // 等宽字体：与代码编辑器同一套设置（用户改了字号/字体，hover 里的
    // 代码块也跟着变）。
    let code_font = cx
        .try_global::<crate::sidebar_view::font_settings::FontState>()
        .map(|s| s.settings.editor_font_family.clone())
        .unwrap_or_else(|| "Menlo".to_string());

    let mut card = div()
        .max_w(px(MAX_WIDTH))
        .max_h(px(MAX_HEIGHT))
        .overflow_hidden()
        .flex()
        .flex_col()
        .py(px(8.0))
        .px(px(10.0))
        .rounded(px(8.0))
        .bg(theme.panel_background)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_size(px(12.0))
        .text_color(theme.text);

    // 按行渲染：``` 包裹的段落用等宽字体区分代码块。
    let mut in_code = false;
    for line in info.contents.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        let line_owned = line.to_string();
        if in_code {
            card = card.child(
                div()
                    .font_family(code_font.clone())
                    .whitespace_nowrap()
                    .text_color(theme.text)
                    .child(line_owned),
            );
        } else {
            card = card.child(div().child(line_owned));
        }
    }

    Some(card.into_any_element())
}

/// 从 `Hover` 响应构造展示信息。
pub fn hover_info(rope: &Rope, hover: &lsp_types::Hover) -> HoverInfo {
    HoverInfo {
        contents: hover_contents(hover),
        range: features::hover_range(rope, hover),
    }
}

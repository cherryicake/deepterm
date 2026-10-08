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
//! 交互约定（延迟模型**照抄 Zed** `hover_popover.rs`）：
//! - **防抖按「最后一次鼠标移动」计**：每一次 mouse move 都作废在途任务、
//!   重新起表（Zed 每次 `hover_at` 都 `info_task = Some(task)` 把旧任务顶掉）。
//!   只在偏移**变化**时重置是不够的 —— 指针在同一个字、行尾空白、行号区里
//!   移动时偏移不变，计时器照走，卡片会在鼠标**还在动**的时候弹出来；
//! - **半程发请求**：[`HOVER_DELAY`] 一劈两半，前半段等完就发 LSP 请求，
//!   后半段与请求往返重叠，最后把总时长补足到 [`HOVER_DELAY`]。于是卡片
//!   **一定不早于** [`HOVER_DELAY`] 出现，同时请求往返不会叠加在它之后
//!   （Zed `show_hover` 里的 `lsp_request_early = hover_popover_delay / 2`）；
//! - **同一符号不重启**：卡片已显示、且新偏移落在它覆盖的区间内就直接返回
//!   （Zed `same_info_hover`），鼠标在同一个符号里抖动时卡片稳稳待着；
//! - **不抢焦点**：浮层不能聚焦，否则鼠标一进浮层编辑器就失焦；
//! - **不穿透**：卡片根节点 `.occlude()`（Zed `InfoPopover::render` 同款），
//!   指针压在卡片上时它下面的 hitbox 一律不算 hovered，编辑区的
//!   `on_mouse_move` 根本不会触发，自然也不会再把卡片下的坐标当字符去发
//!   hover 请求。
//!
//! **一处刻意偏离 Zed**：它的 `show_hover` 在新锚点与当前卡片不同时会
//! `hide_hover()` 立刻收起旧卡片，我们**不**这么做 —— 卡片画在锚点行上方
//! 10px，从符号往卡片去的路上必然扫过其它行，Zed 那套会让卡片在半路消失
//! （指针还没到就跑了）。我们的等价物是「无内容 / 移出编辑区 → 延迟
//! [`HOVER_HIDING_DELAY`] 收起」，见 `request_at` 里的空白响应分支与
//! [`HoverState::schedule_hide`]。

use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use lsp_types::Url;
use ropey::Rope;

use crate::components::input::InputState;
use crate::lsp::features::{self, hover_contents};
use crate::theme::ActiveTheme as _;

/// 鼠标停稳多久才让卡片出现。
///
/// 与 Zed 的 `hover_popover_delay` 默认值一致（`assets/settings/default.json`：
/// 「Time to wait in milliseconds before showing the informational hover box」
/// = 300）。注意它的含义是「**从最后一次鼠标移动算起，卡片至少等这么久**」，
/// 而不是「等这么久才发请求」—— 请求在前半段（[`HOVER_DELAY`] / 2）就发出去，
/// 与后半段重叠，见 `HoverState::request_at`。
///
/// 键盘触发（`K`）跳过整个延迟：Zed 的 `show_hover(..., ignore_timeout = true)`
/// 同样如此。
pub const HOVER_DELAY: Duration = Duration::from_millis(300);

/// 浮层最大宽度（px）。
///
/// 取 680 而不是原先的 520：hover 里最常见的是**函数签名**与 trait 声明，
/// 一行放不下时若不能换行就只能横向滚动，而卡片上没有可见的横向滚动条 ——
/// 等于内容看不全。配合 [`crate::editor::markdown::MarkdownStyle::hover_style`]
/// 里的 `white_space: Normal`（换行），宽一点 + 换行比「窄 + 截断」实用得多。
///
/// 上限仍留着：卡片宽到 680 已占去小屏的相当一部分，再宽会让下面的代码被挡太多。
pub const MAX_WIDTH: f32 = 680.0;
/// 浮层最大高度（px）；超出后可滚动查看。
pub const MAX_HEIGHT: f32 = 320.0;

/// 一次 hover 的结果。
#[derive(Debug, Clone)]
pub struct HoverInfo {
    /// 展示文本（可能是 markdown，含 ``` 代码块）。
    pub contents: String,
    /// 命中的源区间，用于在编辑器里高亮。
    pub range: Option<Range<usize>>,
    /// 已解析的 markdown 实体，绘制时**内联**渲染（见
    /// [`crate::editor::markdown::Markdown::render_items_inline`]）。
    ///
    /// **在后台任务里就建好，不要在绘制阶段建**：markdown 解析 + tree-sitter
    /// 高亮不是廉价操作，每帧重跑一次会让 hover 卡住。实体建好后每帧只做
    /// 排版。对照 Zed `hover_popover.rs` 的 `parse_blocks(&blocks, ..., cx)`：
    /// 它同样在 `show_hover` 的异步任务里建 `Entity<Markdown>` 存进
    /// `InfoPopover.parsed_content`。
    pub markdown: Option<Entity<crate::editor::markdown::Markdown>>,
    /// 浮层自己的滚动句柄。
    ///
    /// 浮层每帧都是新建的元素，若句柄也每帧新建，滚动位置会被重置成 0，
    /// 于是「滚一点就弹回顶部」，滚动根本用不了。
    pub scroll_handle: gpui::ScrollHandle,
}

/// 鼠标移出编辑区后，延迟多久收起 hover 浮层。
///
/// 对照 Zed 的 `hover_popover_hiding_delay`（默认 300ms 左右）。之所以要延迟
/// 而不是立刻消失：用户常把鼠标往浮层方向移去点里面的链接/按钮，立刻收起会让
/// 目标在指针到达前就没了。
pub(crate) const HOVER_HIDING_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

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
    /// 防抖任务：**每一次**鼠标移动都会替换（作废）它。
    ///
    /// 注意「替换」只是 detach（见 [`Self::request_generation`]），真正拦住
    /// 旧任务的是代号比对。
    task: Option<gpui::Task<()>>,
    /// 延迟隐藏任务：鼠标移到空白处时启动，到期才真正收起。
    ///
    /// 对照 Zed 的 `hiding_delay_task`（`hover_popover.rs`）。收起得「延迟」而不是
    /// 「立刻」，是为了让指针有机会移到浮层上操作里面的内容。
    hiding_delay_task: Option<gpui::Task<()>>,
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

    /// 鼠标移动：**每一次移动都重新起防抖**，停稳 [`HOVER_DELAY`] 后才让
    /// 卡片出现。
    ///
    /// `offset` 是鼠标位置换算出的字节偏移。注意「同一偏移内移动」也照样
    /// 重置计时器 —— 这正是 Zed 的做法（每次 `hover_at` 都把 `info_task`
    /// 换成新任务），也是「鼠标还在动卡片就弹出来」的修法：只在偏移**变化**
    /// 时重置的话，指针在同一个字、行尾空白、行号区里移动时偏移不变，计时器
    /// 照走，300ms 一到卡片就在移动中冒出来。
    ///
    /// 唯一不重启的情形是「卡片已经显示、且这次仍在它覆盖的区间内」——
    /// 见 [`InputState::hover_popover_covers`]（Zed `same_info_hover`）。
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
        self.request_at(offset, editor, client, uri, true, cx)
    }

    /// 键盘触发（`K`）：立刻请求，不等防抖。
    ///
    /// 防抖存在的理由是「鼠标移动是连续事件」，而按键是一次性的——等 300ms
    /// 才出浮层会让 `K` 显得失灵。对照 Zed 的 `hover` action：它调
    /// `show_hover(editor, head, /* ignore_timeout */ true, ...)`，
    /// 同样跳过等待。
    ///
    /// `debounce = false` 同时也跳过 `same_info_hover` 判定（Zed 在
    /// `ignore_timeout` 分支里连同 `hide_hover` 一起整段跳过），否则
    /// 「卡片已经在了」会让第二次按 K 什么都没发生。
    pub fn on_keyboard_triggered(
        &mut self,
        offset: usize,
        editor: &Entity<InputState>,
        client: Option<std::sync::Arc<crate::lsp::client::LspClient>>,
        uri: Url,
        cx: &mut App,
    ) {
        self.request_at(offset, editor, client, uri, false, cx)
    }

    /// 发一次 hover 请求。
    ///
    /// `debounce` 为 true（鼠标路径）时按 [`HOVER_DELAY`] 走 Zed 的「半程
    /// 发请求」；false（键盘路径）时立即发、不设任何等待。
    fn request_at(
        &mut self,
        offset: usize,
        editor: &Entity<InputState>,
        client: Option<std::sync::Arc<crate::lsp::client::LspClient>>,
        uri: Url,
        debounce: bool,
        cx: &mut App,
    ) {
        let Some(client) = client else {
            return;
        };

        // 新一轮查询开始，撤销上一轮的延迟隐藏 —— 对照 Zed `hover_at`：
        // 它在调 `show_hover` **之前**就置空 `hiding_delay_task`，所以这一步
        // 必须在 `same_info_hover` 早退**之前**做：鼠标回到已显示的符号上时，
        // 挂着的收起计划同样该作废。
        self.hiding_delay_task = None;
        editor.update(cx, |state, _| state.cancel_hover_hide());

        // 对照 Zed `same_info_hover`：卡片已经显示、且这次的偏移仍落在它
        // 覆盖的区间里 → 不重新排防抖（否则每动一下都把任务往后推，卡片
        // 可能永远不出现）。鼠标在同一个符号里抖动时卡片必须稳稳待着。
        let same_card = debounce && editor.read(cx).hover_popover_covers(offset);

        // 对照 Zed `show_hover`：`same_info_hover` 不成立时它**立刻**
        // `hide_hover()` 收起旧卡片，然后才建新任务 —— 卡片一直停在旧符号上、
        // 让人以为指针还压在那儿，正是 `hover_popover.rs` 里这段的含义。
        // 位置放在排新任务**之前**（`hide_in` 自己会抬代号，必须排在下面抬
        // 代号之前，否则新任务一出生就带着过期代号）。
        //
        // 只走鼠标路径：键盘触发在 Zed 里是 `ignore_timeout = true`，整段跳过，
        // 旧卡片留着、等新响应换内容（见 `on_keyboard_triggered`）。
        if debounce && !same_card {
            self.hide_in(editor, cx);
        }

        // 锚点立刻更新：这样请求回来后浮层就落在当前字符上。
        self.anchor_offset = offset;
        // 作废在途任务。注意这**只是 detach**（GPUI 语义），真正拦住旧任务的
        // 是代号比对 —— 任务在每个检查点都会比对代号，对不上就整体丢弃。
        //
        // **每次移动（哪怕偏移没变）都要走到这里**，见 `on_mouse_moved` 的
        // 文档：这是「鼠标还在动就弹出卡片」的根因所在。
        self.task.take();
        self.request_generation = self.request_generation.wrapping_add(1);
        let generation = self.request_generation;

        let editor = editor.downgrade();
        // 代号与目标偏移都要比对，放在任务够得到的 `InputState` 上。
        editor
            .update(cx, |state, _| {
                state.set_hover_request_generation(generation);
                state.set_hover_offset(offset);
                state.cancel_hover_hide();
            })
            .ok();

        if same_card {
            // 卡片已覆盖这个偏移，不必再排一次防抖。但**在途请求照样作废**：
            // 那是上一个位置的查询，回来会把当前这张卡片顶掉（序列
            // 「看 A → 掠过 B（请求已发）→ 回到 A」里，B 的响应必须丢）。
            // Zed 靠 `info_task` 残留会有这个瑕疵，我们这里顺手修掉。
            return;
        }

        self.task = Some(cx.spawn(async move |cx| {
            // 任务起表的时刻 = 最后一次鼠标移动的时刻（新移动会换掉本任务，
            // 能跑到这里说明期间没有新移动）。
            let started = std::time::Instant::now();

            // Zed `show_hover` 的「半程发请求」：前半段等完就发 LSP 请求，
            // 后半段（与请求往返重叠）用来把**总时长**补足到 `HOVER_DELAY`。
            if debounce {
                smol::Timer::after(HOVER_DELAY / 2).await;
            }

            // 代号已变 = 期间又发了新请求（鼠标继续移动）：这次的结果过期，
            // 必须整体丢弃。`smol::Timer` 醒来不会自动取消，所以这个检查点
            // 不可省 —— 漏掉它就会用旧位置的响应覆盖新位置的状态。
            //
            // **鼠标每动一下都会换掉本任务**（见 `on_mouse_moved`），所以能
            // 走到这里就说明前半段 150ms 里指针是停着的。
            let still_current = editor
                .update(cx, |state, _| {
                    state.hover_request_generation() == generation
                })
                .unwrap_or(false);
            if !still_current {
                log::info!("lsp: hover 请求已过期（前半段延迟后），丢弃");
                return;
            }

            // 前半段结束后重新读文本：期间用户可能又打了字，位置会漂移。
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

            // 代号对上了，但**鼠标可能已经移到别的字符**：代号只在
            // `on_mouse_moved` 里递增，而请求是在前半段延迟之后才发的 —— 这
            // 期间鼠标一动代号就变了，本该被上面拦下；可若鼠标是**移出编辑
            // 区**（没有新的 `on_mouse_moved`），代号不会变，响应就会落到一
            // 个已经不该显示的位置上。
            let anchor_moved = editor
                .update(cx, |state, _| state.hover_offset())
                .ok()
                .flatten()
                .is_some_and(|recorded| recorded != offset);
            if anchor_moved {
                log::info!(
                    "lsp: hover 锚点已漂移（请求于 offset={offset}），丢弃 pos={}:{}",
                    position.line,
                    position.character
                );
                return;
            }

            // **补齐总时长（Zed 的后半段）**：前半段 + 请求往返 + 这里的补足
            // 必须 ≥ `HOVER_DELAY`，否则「服务器一快卡片就秒出」—— 那正是
            // 用户说的「立即弹出」。请求已经在前半段发出去了，所以这段等待
            // 与往返重叠，不会让卡片更晚出现。
            if debounce {
                let rest = HOVER_DELAY.saturating_sub(started.elapsed());
                if !rest.is_zero() {
                    smol::Timer::after(rest).await;
                    let still_current = editor
                        .update(cx, |state, _| {
                            state.hover_request_generation() == generation
                        })
                        .unwrap_or(false);
                    if !still_current {
                        log::info!("lsp: hover 请求已过期（补齐延迟后），丢弃");
                        return;
                    }
                }
            }

            let Ok(Some(hover)) = result else {
                // **空白处（服务器无内容）只安排延迟隐藏，不立刻收。**
                //
                // 立刻收是错的：从符号把指针挪进卡片的路上必然经过编辑区的
                // 其它字符，每一步都会发一次 hover 请求 —— 只要其中一次先
                // 返回「无内容」并立刻清掉，卡片就会在指针到达之前消失。
                // 这正是「鼠标移进卡片、卡片大概率直接关闭」的时序根因
                // （几何守卫修的是「编辑器把卡片上的坐标当成字符」那一半，
                // 这里修的是「空白响应把卡片提前清掉」那一半）。
                //
                // 对照 Zed `hover_popover.rs` 的 `hiding_delay_task`：无内容
                // 时只安排、到期才收；期间新内容到来、指针进入卡片、新请求
                // 发出都会取消它（见 `cancel_hover_hide` 的各调用点）。
                // 记下 deadline 并返回它：睡醒后凭它认领 —— 期间若有新的
                // None 响应把 deadline 推后了，这次睡眠对应的 watcher 就
                // 不再负责（新的 watcher 会接管），避免旧任务误杀新安排。
                let deadline = editor
                    .update(cx, |state, _cx| {
                        if state.hover_popover().is_none() {
                            // 本来就没浮层：无事可做，也别为了这事专门刷一帧。
                            return None;
                        }
                        if state.hover_keyboard_takeover() {
                            // 接管中：用户正用 hjkl 看内容，在飞的旧请求返回
                            // 「无内容」也不能安排隐藏 —— 否则退出接管的瞬间
                            // 就会因为这个过期安排被立即收走。
                            return None;
                        }
                        let deadline = std::time::Instant::now() + HOVER_HIDING_DELAY;
                        state.set_hover_hide_deadline(deadline);
                        Some(deadline)
                    })
                    .ok()
                    .flatten();
                let Some(deadline) = deadline else {
                    return;
                };
                log::info!(
                    "lsp: hover 无内容 -> 安排延迟隐藏 pos={}:{}",
                    position.line,
                    position.character
                );
                // 睡到期再检查：deadline 必须还是当初记的那个（没被新内容 /
                // 进卡片 / 新请求取消或推后），指针不在卡片上，没被键盘接管，
                // 才真正收起。
                //
                // 睡在**当前任务**里而不是等下一帧：鼠标静止时不会产生新帧，
                // 光靠 `take_pending`（只在 render 里跑）这个 deadline 永远
                // 等不到检查它的那一帧。
                cx.background_executor().timer(HOVER_HIDING_DELAY).await;
                let hid = editor
                    .update(cx, |state, cx| {
                        if state.hover_hide_deadline() != Some(deadline) {
                            return false;
                        }
                        if !state.hover_hide_expired() {
                            return false;
                        }
                        state.clear_hover_popover();
                        // 收起发生在 `InputState` 上，重绘由 `Editor` 的
                        // render 读那边，所以要显式 notify 让下一帧跑起来。
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if hid {
                    log::info!("lsp: hover 浮层已收起（延迟到期）");
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
                    // `state.text()` 借了 state，而 `hover_info` 要 `cx.new`
                    // 建 markdown 实体（会再借一次 state），所以先把 rope 克隆
                    // 出来断开借用。
                    let snapshot = state.text().clone();
                    let info = hover_info(&snapshot, &hover, cx);
                    // 结果先落进 `InputState` 的槽，由 Editor 取走
                    // （后台任务拿不到 `Editor` 上的 `HoverState`）。
                    // 偏移一起存：取出时必须用它当锚点。
                    state.set_pending_hover(info, offset);
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
        editor.update(cx, |state, _cx| {
            // **锚点必须来自槽里存的那个偏移**（请求发出时的位置），不能用
            // `HoverState` 上的「最后一次鼠标移动」—— 两者之间只要有请求在
            // 飞（≥ `HOVER_DELAY` 的延迟 + 往返），它们就指向不同字符，卡片内容（A 的）
            // 和位置（B 的）就会错位。
            if let Some((offset, info)) = state.take_pending_hover() {
                state.set_hover_popover(offset, info);
            }
            // 延迟隐藏到期（且指针不在卡片上、没被键盘接管）就收起。
            //
            // 这是后台任务里那个「睡到期检查」的帧驱动备份：正常情况下
            // 睡醒的任务会处理，但任务可能先于 deadline 被丢弃（例如
            // `hide()` 把 `task` take 掉了），这里兜底。
            if state.hover_hide_expired() {
                state.clear_hover_popover();
            }
        });
    }

    /// 隐藏浮层：同时清掉 `InputState` 上的状态（绘制读的是那边）。
    ///
    /// **指针正落在浮层上时不隐藏**：否则用户没法把指针移进卡片去点链接或
    /// 滚动内容 —— 卡片会在指针到达之前就没了。这正是「浮窗不能滚动查看」
    /// 的另一半原因（另一半是它压根没有滚动容器）。
    ///
    /// 对照 Zed `hide_hover`：它没有这个判断，但浮层上挂了
    /// `stop_propagation`，所以编辑区的 mouse_move 根本不会走到隐藏逻辑。
    /// 我们的事件路径不同，用显式状态表达同一意图。
    pub fn hide_in(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        // 浮层被键盘接管时（`K` 进了浮层）不隐藏：用户正要拿 hjkl 看内容。
        // 判定要 `Window`，而这里只有 `App`；`App` 拿不到 window，所以焦点
        // 这一路由**调用方**（`Editor`，它有 `&mut Window`）提前挡掉，
        // 见 `Editor::toggle_hover` 与 `schedule_hover_hide`。
        let pointer_inside = editor.read(cx).hover_pointer_inside();
        if pointer_inside {
            // 只取消在途请求，浮层与滚动位置都留着。
            self.task.take();
            return;
        }
        // 作废在途请求：`self.hide()` 只能把 `task` take 掉，而 GPUI 的 Task
        // 一 drop 就是 detach、**不会取消**；代号不变的话旧任务醒来后照样会把
        // 响应写进 pending 槽，表现就是「点一下收起卡片，几百毫秒后它又自己
        // 弹回来」。抬代号才能让那些任务在检查点上自己退出。
        self.request_generation = self.request_generation.wrapping_add(1);
        let generation = self.request_generation;
        editor.update(cx, |state, _| {
            state.set_hover_request_generation(generation)
        });
        self.hide();
        editor.update(cx, |state, _cx| state.clear_hover_popover());
    }
}

/// 浮层被键盘接管时用来滚动内容的按键（按键名，不是事件）。
///
/// 只收「滚动语义」的那几个：`hjkl` 与方向键、`pageup`/`pagedown`。
/// `Keystroke.key` 是**字符串**（gpui 的 `platform::keystroke::Keystroke`），
/// 方向键给的是 `"up"` / `"pagedown"` 这类名字，字母键给的是字符本身。
/// 字母键要**同时**匹配大小写：按住 Shift 时 `K`/`J` 送上来的是大写。
///
/// 刻意**不**收 `home`/`end`/`g`/`G`：浮层里没有「跳到首行/末行」的语义，
/// 而 `g` 在 vim normal 模式下是操作符前缀，吃掉它会让用户以为按了没用。
fn hover_scroll_key(key: &str) -> bool {
    // 注意没有大写 `K`：它是接管的总开关，走 keymap 的 `editor::Hover`，
    // 拦截器必须原样放行（见 `init_keystroke_interceptor` 里的提前返回）。
    matches!(
        key,
        "up" | "down" | "pageup" | "pagedown" | "h" | "H" | "j" | "J" | "k" | "l" | "L"
    )
}

/// Esc 在浮层接管中：退出接管并收起。
///
/// **不进键位表**：键位表里的 `escape` 已经绑给 `editor::Escape`，在这里再加
/// 一条覆盖，会连编辑器里「Esc 退出插入模式」一起弄坏。而拦截器跑在 keymap
/// 之前，就地判断最直接。
const HOVER_ESC: &str = "escape";

/// 按键名 → 滚动偏移。
///
/// 翻页用句柄自己记录的 `bounds` 高度算，所以卡片多高就翻多少，不需要把高度
/// 从绘制阶段传进来。
fn scroll_hover_by(handle: &gpui::ScrollHandle, key: &str, line_height: gpui::Pixels) {
    let page = handle.bounds().size.height;
    // 逐键步长取行高，让 hjkl 的滚动粒度与文本行对齐；翻页则整屏。
    let step = line_height;
    // y 轴向上为负（内容向上滚 = 偏移变负），所以「向下」取负。
    let (dx, dy) = match key {
        "up" => (px(0.), step),
        "down" => (px(0.), -step),
        "pageup" => (px(0.), page),
        "pagedown" => (px(0.), -page),
        "h" | "H" => (step, px(0.)),
        "l" | "L" => (-step, px(0.)),
        "j" | "J" => (px(0.), -step),
        "k" => (px(0.), step),
        _ => return,
    };
    let mut offset = handle.offset();
    offset.y += dy;
    offset.x += dx;
    handle.set_offset(offset);
}

/// 注册全局按键拦截器：键盘接管中的 hover 浮层靠它滚动。
///
/// **为什么必须是全局拦截器**（`cx.intercept_keystrokes`，跑在 keymap 之前），
/// 而不能是卡片上的 `on_key_down` / `window.focus`：
///
/// 卡片是用 `defer_draw` 画到窗口层上的，它从不经过 prepaint，也就进不了
/// GPUI 的 dispatch 树。`window.focus()` 在它身上是空操作 ——
/// `focus_node_id_in_rendered_frame` 查不到节点就回退到根节点，按键照样走
/// 编辑器的路径（vim 的 `j` 照样移光标），用户看到的就是「焦点还在编辑器里」。
/// 而卡片自己的 `on_key_down` 根本不在 dispatch 路径上，永远收不到事件。
///
/// 拦截器只在**有编辑器正处于接管中**时才动手，其它时候原样放行，
/// 对编辑器/终端/其它面板的按键零影响。
///
/// 语义：
/// - 滚动键（hjkl/方向键/翻页）：滚卡片，吃掉按键（vim 不再移光标）；
/// - Esc：退出接管并收起浮层，吃掉按键；
/// - 其它键：退出接管，**不吃** —— 按键继续走正常路径（例如 `w` 照样跳词，
///   只是浮层不再被接管）。
pub fn init_keystroke_interceptor(cx: &mut App) {
    // `Subscription` drop 即退订，所以必须留着 —— 它是应用生命周期的全局
    // 拦截器，`forget` 是故意的（进程退出时一并回收）。
    std::mem::forget(cx.intercept_keystrokes(|event, window, cx| {
        let key = event.keystroke.key.as_str();

        // 找到接管中的那一个编辑器。绝大多数按键发生时根本没有接管，
        // 先只读扫描，找到才动手 —— 终端里打字不受任何影响。
        let mut target: Option<gpui::Entity<crate::components::input::InputState>> = None;
        for input in crate::components::input::state::all_input_states() {
            if input.read(cx).hover_keyboard_takeover() {
                target = Some(input);
                break;
            }
        }
        let Some(input) = target else {
            return;
        };
        // `App::notify` 要显式给 `EntityId`（和 `Context::notify` 不同）。
        let id = input.entity_id();

        // `K` 交给 keymap 的 `editor::Hover`（即 `toggle_hover` 的三态机）：
        // 拦截器若在这里把接管清掉，`toggle_hover` 看到的永远是「未接管」，
        // 第三次按 K 就永远到不了「退出接管」那一态 —— 接管一旦打开就关不掉。
        //
        // 注意大小写：小写 `k` 是滚动键（往上滚一行），大写 `K` 才是开关。
        // 这个耦合是故意的 —— `K` 绑死在 Rust 侧（`Editor::init`），
        // 用户改键位表也改不掉它（见该处的注释）。
        if key == "K" {
            return;
        }

        if !hover_scroll_key(key) && key != HOVER_ESC {
            // 不是接管关心的键：退出接管（例如浮层显示中用户直接打字，
            // 接管本来就不该继续），但按键本身放行 —— `w` 照样跳词。
            input.update(cx, |state, _| {
                state.set_hover_keyboard_takeover(false);
            });
            cx.notify(id);
            log::info!("lsp: hover 退出键盘接管（其它按键）");
            return;
        }

        if key == HOVER_ESC {
            input.update(cx, |state, _| {
                state.clear_hover_popover();
            });
            cx.notify(id);
            log::info!("lsp: hover 退出键盘接管（Esc）");
            cx.stop_propagation();
            return;
        }

        // 滚动键：滚卡片并吃掉按键（vim 不再移光标）。
        let line_height = window.line_height();
        input.update(cx, |state, _| {
            if let Some((_, info)) = state.hover_popover() {
                scroll_hover_by(&info.scroll_handle, key, line_height);
            }
        });
        cx.notify(id);
        cx.stop_propagation();
    }));
}

/// 渲染 hover 浮层的**内容**（不含定位）。
///
/// 用仓库里已有的 markdown 渲染器**内联**渲染，而不是把文本按行塞进 div。
/// 后者是此前的实现，带来两个用户可见的问题：
///
/// 1. **没有高亮**：hover 内容里最常见的 ``` 代码块就是函数签名，纯 div
///    渲染下它与正文长得一模一样，等宽字体是唯一的区分手段。
///    markdown 渲染器带 tree-sitter 高亮（`highlight_code_runs`），签名里的
///    类型、字段、生命周期参数才会被上色。
/// 2. **多行字符叠在一起**：卡片是 `flex_col` + `max_h`，而 flex 子项默认
///    `flex_shrink: 1`。内容超过 `max_h` 时 GPUI 不是裁掉，而是把每个子项
///    压扁，行高塌陷 -> 文字互相重叠。这也是「格式错乱」的真正原因，与
///    换行符无关。
///
/// 现在：markdown 条目内联渲染 + 外层 `overflow_y_scroll`，行高由
/// [`crate::editor::markdown::MarkdownStyle::hover_style`] 的 `line_height`
/// 明确给定（必须大于字号），并给每个子项 `flex_shrink_0()` 兜底。
///
/// 对照 Zed `hover_popover.rs::InfoPopover::render`：它同样是
/// `MarkdownElement` 内联 + `.overflow_y_scroll()` + `.track_scroll(...)`，
/// 样式取自独立的 `hover_markdown_style`。
pub fn render_hover_card(
    info: &HoverInfo,
    window: &mut gpui::Window,
    editor_state: gpui::WeakEntity<InputState>,
    cx: &mut App,
) -> Option<gpui::AnyElement> {
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

    // 内容区：可纵向滚动，并绑定**稳定**的句柄（浮层元素每帧重建，句柄不能
    // 每帧新建，否则滚动位置永远被重置回顶部）。
    let scroll_handle = info.scroll_handle.clone();
    let body = div().flex().flex_col().w_full().children(
        info.markdown
            .as_ref()
            .map(|md| {
                let style = crate::editor::markdown::MarkdownStyle::hover_style(
                    window,
                    cx,
                    code_font.clone(),
                );
                crate::editor::markdown::Markdown::render_items_inline(md.clone(), style, cx)
            })
            .unwrap_or_default()
            .into_iter()
            // 每个条目 `flex_shrink_0`：卡片有 `max_h`，flex 子项默认
            // `flex_shrink: 1`，内容超高时会被压扁 —— 行高塌陷就是之前
            // 「多行字符叠在一起」的直接原因。
            .map(|item| div().flex_shrink_0().w_full().child(item)),
    );

    let card = div()
        // `.id(...)` 不是装饰：它把 `Div` 变成 `Stateful<Div>`，而
        // `overflow_y_scroll` / `track_scroll` 只在 `StatefulInteractiveElement`
        // 上有实现。少这一行整个滚动容器就编译不过。
        .id("hover-card")
        // **挡住底下的一切鼠标交互**（Zed `InfoPopover::render` 同款
        // `.occlude()`，`hover_popover.rs`）。卡片是 `defer_draw` 的，它的
        // hitbox 比编辑器内容**后**插入，`occlude` 让命中测试在卡片处就停下：
        // 卡片**下面**的 hitbox 一律 `is_hovered() == false`，于是指针压在卡片
        // 上时
        //   * 编辑器容器的 `on_mouse_move` 根本不会触发 → 不会再把卡片下的
        //     坐标当成字符去发 LSP hover（用户报的「事件穿透」）；
        //   * 底下文字的 hover 样式、链接 tooltip、点击（改光标位置）一并挡住。
        // 卡片自己的 hitbox 就是「挡住别人的那一个」，仍在命中列表里，所以它
        // 自己的滚动 / `on_mouse_move` / `on_hover` 照常工作。
        //
        // 编辑器侧还留了一份几何判断（`hover_popover_contains`）做兜底：
        // 卡片 hitbox 要到它自己那一帧的 prepaint 才存在，两帧之间靠它拦。
        .occlude()
        // 宽度上限 + 换行的**唯一**来源。两件事必须由同一条约束提供：
        //
        //   * 夹住卡片盒子 —— 段落是 `flex().flex_wrap()` 的行容器，它的主轴
        //     尺寸在 taffy 的内在尺寸算法里取**内容贡献**（不看可用宽度），
        //     会把卡片顶到内容的自然宽度（实测 1599 / 3183 / …）。
        //   * 给子内容一个定宽 —— 文本只有拿到 `AvailableSpace::Definite`
        //     才换行（见 `place_at_anchor::wrap_at` 的注释），可用宽度定下来后
        //     正好是 `MAX_WIDTH - padding`，段落与代码块都按它折行。
        //
        // 老代码为什么坏：`max_w` 把第一遍测量的结果夹成 680，于是「自然宽度
        // 超上限才改用定宽」的判断 `680 > 680` 恒为假、定宽那一遍从不执行 ——
        // 卡片 680 宽、里面只有一行高，文字与代码块全溢出到卡片外面。现在
        // 只量一遍、直接给定宽。
        .max_w(px(MAX_WIDTH))
        .max_h(px(MAX_HEIGHT))
        .flex()
        .flex_col()
        .overflow_y_scroll()
        // 横向兜底裁剪：换行之后仍可能有**不可断**的长串（长 URL、无空格的
        // 类型路径）超出卡片；`overflow_y_scroll` 只管纵轴，横轴不裁就会画到
        // 卡片外面去。
        .overflow_x_hidden()
        .track_scroll(&scroll_handle)
        .py(px(8.0))
        .px(px(10.0))
        .rounded(px(8.0))
        .bg(theme.panel_background)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_size(px(12.0))
        .text_color(theme.text)
        .child(body)
        // 键盘接管（`K` 第二次按下）**不走真焦点**：卡片是 `defer_draw` 到
        // 窗口层上的，从不经过 prepaint，也就进不了 GPUI 的 dispatch 树 ——
        // `window.focus()` 在它身上是空操作（查不到节点就回退到根，按键照样
        // 走编辑器的路径）。所以接管是虚拟的：焦点物理上还在编辑器里，
        // `hjkl` 等键由全局按键拦截器提前拿走（见 `init_keystroke_interceptor`）。
        .on_mouse_move({
            let editor = editor_state.clone();
            move |_, _, cx| {
                editor
                    .update(cx, |state, cx| {
                        // 指针进入卡片：之前安排的延迟隐藏作废。
                        state.set_hover_pointer_inside(true);
                        state.cancel_hover_hide();
                        cx.notify();
                    })
                    .ok();
                cx.stop_propagation();
            }
        })
        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
            // 不让点击穿透到编辑区（否则会移动光标、把浮层锚点带跑）。
            cx.stop_propagation();
        })
        // 指针离开卡片：复位 `hover_pointer_inside`。
        //
        // 只靠编辑器侧的 mouse_move 复位是不够的：指针从卡片直接移到编辑区
        // 之外（侧边栏、其它面板）时，编辑器的 `on_mouse_move` 根本不会触发，
        // 标记会永远是 true，此后所有隐藏都被跳过 —— 浮层赖着不走。
        // `on_hover(false)` 是 GPUI 在悬停状态变化时给的回调，走哪都触发。
        .on_hover({
            let editor = editor_state.clone();
            move |hovered: &bool, _window, cx| {
                if !hovered {
                    editor
                        .update(cx, |state, cx| {
                            state.set_hover_pointer_inside(false);
                            // 离开卡片：之前安排的延迟隐藏如果还挂着，
                            // 含义已经过时（它是「指针在卡片上时」安排的）——
                            // 取消它，后续的编辑器 mouse_move 会按新位置
                            // 重新安排。否则离开后会因旧安排被立即收走。
                            state.cancel_hover_hide();
                            cx.notify();
                        })
                        .ok();
                }
            }
        });
    //
    // 浮层自己的滚动由 GPUI 为 `overflow: Scroll` 容器自动注册的 listener
    // （`div.rs::paint_scroll_listener`）完成，它**不会** `stop_propagation`，
    // 所以事件会继续冒泡到 `InputState::on_scroll_wheel` 把编辑器滚走。
    // 拦这件事放在**编辑器侧**做（`InputState::hover_popover_contains` +
    // `on_scroll_wheel`）：那里有浮层的实际矩形，判断可靠。
    //
    // 在卡片上挂 listener 反而会坏事：用户 listener 与 GPUI 内置 listener
    // 谁先跑取决于 paint 顺序，一旦自己先 `stop_propagation`，浮层就滚不动了
    // —— 变成「浮层和编辑器都不滚」，比穿透更糟。

    Some(card.into_any_element())
}

/// 已经是纯文本的结果（签名帮助走这条）。
impl HoverInfo {
    /// 直接从一段文本构造，顺带建好 markdown 实体。
    ///
    /// 签名帮助复用 hover 浮层，内容同样可能是 ``` 代码块，因此与
    /// [`hover_info`] 走完全相同的渲染路径（解析 + 高亮 + 滚动），而不是
    /// 塞一段无样式的纯文本进去。
    pub fn from_plain_text(contents: impl Into<String>, cx: &mut App) -> Self {
        let contents = contents.into();
        let markdown = if contents.trim().is_empty() {
            None
        } else {
            let text = contents.clone();
            Some(cx.new(|cx| crate::editor::markdown::Markdown::new(text, cx)))
        };
        HoverInfo {
            contents,
            range: None,
            markdown,
            scroll_handle: gpui::ScrollHandle::new(),
        }
    }
}

/// 从 `Hover` 响应构造展示信息。
///
/// markdown 实体在这里**一次性**建好（后台任务里），绘制阶段只做排版。
/// `cx` 是 `&mut App`：需要它来 `cx.new` 出 markdown 实体。
pub fn hover_info(rope: &Rope, hover: &lsp_types::Hover, cx: &mut App) -> HoverInfo {
    let contents = hover_contents(hover);
    // 纯空白就别建 markdown 实体了：`Markdown::new` 会跑一遍解析，
    // 而这种结果绘制时本来就会被丢掉。
    let markdown = if contents.trim().is_empty() {
        None
    } else {
        let text = contents.clone();
        Some(cx.new(|cx| crate::editor::markdown::Markdown::new(text, cx)))
    };
    HoverInfo {
        contents,
        range: features::hover_range(rope, hover),
        markdown,
        scroll_handle: gpui::ScrollHandle::new(),
    }
}

//! LSP 自动补全。
//!
//! 结构对照 Zed 的 `editor/src/completions.rs` + `code_context_menus.rs`：
//!
//! - [`Candidate`]：一条补全的**可视化快照**。把服务器返回的
//!   [`lsp_types::CompletionItem`] 在响应到达时一次性解析成菜单需要的字段
//!   （显示文本、详情、过滤文本、插入文本、替换区间、类型），后续绘制与
//!   确认不再反复解析协议对象。
//! - [`CompletionState`]：挂在 `Editor` 上。持有「服务器返回的全量候选」与
//!   「当前过滤后的可见候选」两层，用户继续输入时先**本地过滤**（Zed 的
//!   `set_filter_results`），再发新的 LSP 请求；这样菜单不会在新响应回来
//!   之前一直展示与当前前缀不匹配的旧候选。
//! - `InputState` 只保存绘制快照（[`CompletionMenu`]）与后台结果槽
//!   （[`PendingCompletions`]）。`TextElement` 绘制时只拿得到
//!   `Entity<InputState>`，这是两边的边界。
//!
//! 触发方式与 Zed/VSCode 一致：
//! - **自动**：输入标识符字符或服务器声明的触发字符时按防抖请求；
//! - **手动**：`ctrl-space` 立即请求。
//!
//! 键盘语义（见 `assets/keymaps/completion.json`）：所有补全键无条件绑在
//! `Input` 上并先于普通编辑键；handler 先看补全是否可见，不可见时
//! `cx.propagate()` 放行，可见时才消费。`↑/↓`（`ctrl-p/ctrl-n`）选择、
//! `enter/tab` 确认、`esc` 关闭、`pageup/pagedown` 首尾。快捷键**不能**
//! 绑在 `EditorPanel` 上：`Input` 上已经有 `enter`/`escape`/方向键。

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Entity, EntityId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Styled as _, div, px,
};
use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemTag, Position, Url};

use crate::components::button::{Icon as ButtonIcon, Size as IconSize};
use crate::components::input::InputState;
use crate::lsp::client::LspClient;
use crate::lsp::features;
use crate::theme::ActiveTheme as _;

/// 自动触发的防抖窗口。
const AUTO_DELAY: Duration = Duration::from_millis(120);

/// 菜单最大可见条目数，超出滚动。
const MAX_VISIBLE: usize = 12;

/// 单次响应最多保留多少条候选（防止服务器返回上万条时卡住 UI）。
///
/// 500 对成员很多的类型不够：rust-analyzer 在 `value.` 上可能返回上千条，
/// 截断后用户会看到「存在的方法没出现」。这里只保留一个防病态响应的上限，
/// 显示层仍然只渲染 [`MAX_VISIBLE`] 行，不会因此变慢。
const MAX_CANDIDATES: usize = 10_000;

/// 一条候选。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 主标签（左栏）。
    pub label: String,
    /// 右侧的补充信息（类型/所属），可为 `None`。
    pub detail: Option<String>,
    /// 补全项类型；决定左侧图标与颜色。
    pub kind: Option<CompletionItemKind>,
    /// 是否已弃用（显示删除线、降低对比度）。
    pub deprecated: bool,
    /// 过滤文本（`filterText` 优先，否则 label）。
    pub filter_text: String,
    /// 服务器排序键（仅用于诊断/稳定排序）。
    pub sort_text: String,
    /// 接受时写入缓冲区的文本。
    pub insert_text: String,
    /// 接受时替换的字节区间（响应到达时按当时文本计算）。
    pub replace_range: Range<usize>,
    /// 原始协议项（`completionItem/resolve` 与 `additionalTextEdits` 要用）。
    pub item: CompletionItem,
}

impl Candidate {
    /// 从协议项构造候选。
    ///
    /// `text` 是响应到达时的缓冲区，`cursor` 是发请求时的光标偏移：
    /// 两者必须同源，否则替换区间会按新位置算到旧候选上。
    fn from_item(text: &ropey::Rope, cursor: usize, item: CompletionItem) -> Option<Self> {
        let label = features::completion_label(&item);
        if label.is_empty() {
            return None;
        }
        let filter_text = features::completion_filter_text(&item).trim().to_string();
        let filter_text = if filter_text.is_empty() {
            // `label` 为空、内容全在 `textEdit.newText` 里时，显示文本有回退，
            // 过滤文本也必须用同一个回退值，否则候选会被上面的空检查丢掉。
            label.clone()
        } else {
            filter_text
        };
        let insert_text = features::completion_insert_text(&item);
        let replace_range = features::completion_replace_range(text, cursor, &item);
        if replace_range.start > cursor {
            log::warn!(
                "lsp: completion 候选 {label:?} 的区间 {replace_range:?} 起点越过光标 {cursor}，text_edit={:?}",
                item.text_edit
            );
        }
        let deprecated = item.deprecated.unwrap_or(false)
            || item
                .tags
                .as_ref()
                .is_some_and(|tags| tags.contains(&CompletionItemTag::DEPRECATED));
        Some(Self {
            label,
            detail: features::completion_detail(&item),
            kind: item.kind,
            deprecated,
            filter_text,
            sort_text: features::completion_sort_key(&item).to_string(),
            insert_text,
            replace_range,
            item,
        })
    }

    /// 选中项的身份：用于刷新候选后恢复选中位置。
    fn identity(&self) -> (String, String, Option<CompletionItemKind>) {
        (self.label.clone(), self.insert_text.clone(), self.kind)
    }

    /// 该候选的过滤文本是否命中当前前缀。
    fn score(&self, query: &str) -> Option<u32> {
        // `filterText` 是服务器的过滤建议，不一定等于展示 label（例如
        // 函数 label 是 `foo(…)`、filterText 只有 `foo`）。两边都试，
        // 避免合法候选因为 filterText 过窄而被本地过滤掉。
        fuzzy_score(&self.filter_text, query).or_else(|| fuzzy_score(&self.label, query))
    }

    /// 用户继续输入/退格后，把响应时期的替换区间跟着光标平移。
    ///
    /// 我们不能用 Zed 的 `Anchor`（`InputState` 没有 anchor 体系），但补全
    /// 的编辑几乎都发生在光标处：插入时区间终点跟着右移，退格时跟着左移。
    /// 不调整的话会按**旧光标**的位置替换——例如在 `pri|` 收到 `println`
    /// （区间 `0..3`），用户再敲 `n` 变成 `prin|`，确认时仍只替换前 3 个
    /// 字符，结果是 `printlnn`。
    fn adjust_range_for_edit(&mut self, text: &ropey::Rope, old_cursor: usize, cursor: usize) {
        if old_cursor == cursor {
            return;
        }
        let word = features::word_range_before(text, cursor);
        let old = self.replace_range.clone();
        let (mut start, mut end) = (old.start, old.end);

        if cursor > old_cursor {
            let delta = cursor - old_cursor;
            if end == old_cursor {
                end += delta;
            }
            // 响应给的是空区间（插入点）：新输入的字符必须一起被替换。
            if start == old_cursor {
                start = word.start;
            }
        } else {
            let delta = old_cursor - cursor;
            if end >= old_cursor {
                end = cursor;
            } else {
                end = end.saturating_sub(delta);
            }
            if start <= old_cursor {
                start = start.saturating_sub(delta);
            }
            // 连续退格可能把区间起点推到当前词首之前（例如
            // `foo.bar|` 退到 `foo.|`：旧区间 4..7、delta=3，左移后
            // 变成 1..4，会误删 `oo.`）。服务器原本就从词首开始替换时，
            // 起点不能越过现在的词首。
            if start < word.start && old.start >= word.start {
                start = word.start;
            }
        }

        let next = if start <= end
            && end <= text.len()
            && text.is_char_boundary(start)
            && text.is_char_boundary(end)
        {
            start..end
        } else {
            word
        };
        if next.start > cursor || next.end < cursor {
            log::warn!(
                "lsp: completion 平移后区间 {next:?} 越过光标 {cursor}（old={old:?} old_cursor={old_cursor}），确认时会退回词区间"
            );
        }
        self.replace_range = next;
    }
}

/// 把明显越过光标的替换区间拉回当前词。
///
/// 正常 LSP `textEdit` 的起点不会在光标之后；一旦出现（旧响应被平移、
/// 服务器把空区间给到下一行行首、行尾差一），继续按原区间插入就会把补全
/// 写到下一行/光标后面。此时退回 `word_range_before(cursor)`，至少替换掉
/// 用户刚输入的前缀，而不是在新行追加一份。
fn clamp_replacement_range(
    range: std::ops::Range<usize>,
    text: &ropey::Rope,
    cursor: usize,
) -> std::ops::Range<usize> {
    if range.start > cursor || range.end < cursor {
        crate::lsp::features::word_range_before(text, cursor)
    } else {
        range
    }
}

/// 补全菜单状态（挂在 `Editor` 上）。
pub struct CompletionState {
    /// 服务器最近一次返回的全量候选（已按响应位置算好替换区间）。
    all: Vec<Candidate>,
    /// 当前可见候选（按查询过滤、排序后的子集）。
    candidates: Vec<Candidate>,
    /// 选中下标（相对 `candidates`）。
    selected: usize,
    /// 菜单锚点（请求/刷新时的光标字节偏移）。
    anchor: usize,
    /// 候选对应的文本版本；确认时用它判断区间是否仍然有效。
    text_version: u64,
    /// 当前查询（光标前的标识符前缀），仅用于日志/诊断。
    query: String,
    /// 是否可见。
    visible: bool,
    /// 防抖/请求任务。
    task: Option<gpui::Task<()>>,
    /// 请求代号：每次触发自增，用来作废在飞的旧任务。
    ///
    /// **必须有这个**。GPUI 的 `Task` 被 drop 只是 detach、**不会取消**
    /// （同一个坑见 `TerminalView::wsl_scan_seq` 与
    /// `HoverState::request_generation`），所以 [`Self::task`].take() 拦不住
    /// 旧任务：它照样会跑完、照样会回写。
    request_generation: u64,
    /// 触发时用的 uri（结果回来时校验文件没被切走）。
    uri: Option<Url>,
    /// 候选所属的编辑器；切 tab 后旧菜单不能被新 tab 确认。
    owner: Option<EntityId>,
}

impl Default for CompletionState {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionState {
    pub fn new() -> Self {
        Self {
            all: Vec::new(),
            candidates: Vec::new(),
            selected: 0,
            anchor: 0,
            text_version: 0,
            query: String::new(),
            visible: false,
            task: None,
            request_generation: 0,
            uri: None,
            owner: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 菜单锚点（光标字节偏移）。供 Editor 的观察者判断「只是移了光标」。
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// 当前候选对应的文本版本。
    pub fn text_version(&self) -> u64 {
        self.text_version
    }

    /// 选中项。
    pub fn selected(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// 当前可见候选数量（日志/测试）。
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// 隐藏并清空本地状态（不清 `InputState`，见 [`Self::hide_in`]）。
    ///
    /// 同时抬升请求代号：GPUI 的 `Task` drop 只是 detach，不抬代号的话
    /// 在飞响应回来还会把菜单重新弹出来（按 Esc / 确认后「弹窗又出现」）。
    pub fn hide(&mut self) {
        self.visible = false;
        self.all.clear();
        self.candidates.clear();
        self.selected = 0;
        self.query.clear();
        self.task.take();
        self.owner = None;
        self.uri = None;
        self.request_generation = self.request_generation.wrapping_add(1);
    }

    /// 隐藏并清掉 `InputState` 上的快照（绘制读的是那边）。
    ///
    /// 除了清快照，还要同步抬升 `InputState` 上的代号，拦住在飞任务：
    /// 只抬本地代号没用，后台任务校验的是 `InputState` 上的那一份。
    pub fn hide_in(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        self.hide();
        let generation = self.request_generation;
        editor.update(cx, |state, _cx| {
            state.set_completion_request_generation(generation);
            state.clear_completion_menu();
            state.clear_pending_completions();
        });
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

    /// 选中第一项 / 最后一项。
    pub fn select_first(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = 0;
        }
    }

    pub fn select_last(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = self.candidates.len() - 1;
        }
    }

    /// 请求补全（手动触发，无防抖）。
    pub fn trigger(
        &mut self,
        editor: &Entity<InputState>,
        client: Option<Arc<LspClient>>,
        uri: Url,
        cx: &mut App,
    ) {
        log::info!("lsp: completion 手动触发 (ctrl-space)");
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
        log::info!(
            "lsp: completion 自动触发 trigger={:?} delay={:?}",
            trigger_character,
            AUTO_DELAY
        );
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
            // 服务器没就绪时收起旧菜单：继续展示一份不会再更新的候选，
            // 用户选中后很可能插入失败。
            log::warn!("lsp: completion 无 client（服务器未就绪？）");
            self.hide_in(editor, cx);
            return;
        };
        log::info!(
            "lsp: completion 客户端就绪，is_closed={}",
            client.is_closed()
        );
        if client.is_closed() {
            log::warn!("lsp: completion 客户端已断开，取消本次请求");
            self.hide_in(editor, cx);
            return;
        }
        // `take()` 只是 detach（GPUI 的 Task drop 不取消），真正作废靠代号。
        self.task.take();
        self.request_generation = self.request_generation.wrapping_add(1);
        let generation = self.request_generation;
        self.uri = Some(uri.clone());
        self.owner = Some(editor.entity_id());

        // 代号必须在 **spawn 之前**同步写到 `InputState` 上，不能等防抖醒来
        // 再写。写在任务里会留下一个真空期：防抖期间 `InputState` 上仍是**上一
        // 次**的代号，此时若有更早的任务醒来，它会拿旧代号通过校验并把过期
        // 候选写进 pending 槽 —— 用户看到的就是「候选内容和光标处不匹配」。
        editor.update(cx, |state, _| {
            state.set_completion_request_generation(generation);
            // 上一轮请求可能已经落地但还没被 `drain_lsp_results` 取走；
            // 新一轮请求开始后它就是过期的，不能抢在新结果前面显示。
            state.clear_pending_completions();
        });

        let editor = editor.downgrade();
        self.task = Some(cx.spawn(async move |cx| {
            if let Some(delay) = delay {
                smol::Timer::after(delay).await;
            }
            // 防抖醒来先验代号：期间用户又敲了字符，这一份就已经过期，
            // 连请求都不必发。
            let still_current = editor
                .update(cx, |state, _| {
                    state.completion_request_generation() == generation
                })
                .unwrap_or(false);
            if !still_current {
                log::info!("lsp: completion 请求已过期（防抖后），不发请求");
                return;
            }
            // 防抖后重读光标/版本：期间用户可能继续打字或移开了光标。
            let Some((position, cursor_at_request, version_at_request)) = editor
                .update(cx, |state, _| {
                    (
                        features::offset_to_position(state.text(), state.cursor()),
                        state.cursor(),
                        state.text_version(),
                    )
                })
                .ok()
            else {
                return;
            };

            // 把触发字符原样传给服务器（对照 LSP 的
            // `CompletionContext.triggerCharacter`）：服务器据此决定返回哪一
            // 类候选（例如 `:` 后只给属性名）。
            let t0 = std::time::Instant::now();
            log::info!(
                "lsp: → textDocument/completion pos={}:{} offset={cursor_at_request} trigger={:?} uri={uri}",
                position.line,
                position.character,
                trigger_character
            );
            match client.completion(&uri, position, trigger_character).await {
                Ok((items, incomplete)) => {
                    // **await 之后必须再验代号**：等待期间用户可能又敲了几个
                    // 字符，那几轮的任务也会发请求。旧响应回来就写 pending 槽，
                    // 会把新位置的结果覆盖掉（或反过来让菜单迟迟不出现）。
                    let still_current = editor
                        .update(cx, |state, _| {
                            state.completion_request_generation() == generation
                        })
                        .unwrap_or(false);
                    if !still_current {
                        log::info!("lsp: completion 响应已过期，丢弃 {} 个候选", items.len());
                        return;
                    }
                    log::info!(
                        "lsp: ← completion 返回 {} 个候选 (incomplete={incomplete}) 耗时={:?}",
                        items.len(),
                        t0.elapsed()
                    );
                    editor
                        .update(cx, |state, cx| {
                            state.set_pending_completions_from_request(
                                items,
                                position,
                                cursor_at_request,
                                version_at_request,
                                uri.clone(),
                                incomplete,
                            );
                            // **必须 notify**：候选只是先存进 `InputState` 的
                            // pending 槽，真正搬到浮层状态要等
                            // `Editor::drain_lsp_results`，而它只在 `render`
                            // 里调用。没有这次 notify 就不会产生新的一帧，
                            // `render` 不跑，候选永远搬不出来 —— 表现为
                            // 「服务器明明返回了候选，却没有弹窗」。
                            cx.notify();
                        })
                        .ok();
                }
                Err(e) => {
                    // 降级：补全失败只是没有候选，不影响编辑。
                    log::warn!("lsp: ← completion 失败: {e}");
                }
            }
        }));
    }

    /// 用服务器最新结果替换候选。
    ///
    /// 返回 `true` 表示菜单可见；`false` 表示没有可用候选（已收起）。
    fn install(
        &mut self,
        state: &mut InputState,
        items: Vec<CompletionItem>,
        cursor: usize,
        version: u64,
        incomplete: bool,
    ) -> bool {
        // 刷新候选时保留用户已经移动到的那一项（Zed 同语义）；只有首次
        // 打开或该项被过滤掉时才退回 `preselect` / 第一项。
        let previous = self.selected().map(Candidate::identity);

        // 1) 协议项 → 可视化候选（替换区间按**响应时**的文本算）。
        //
        // 不做按 label/detail 的去重：同一类型可能从多个 trait/impl 返回同名
        // 但语义不同的方法，去重会让其中一部分「凭空消失」。Zed 的 LSP
        // 补全也不在这里去重，重复项交给菜单/用户判断。
        let all: Vec<Candidate> = {
            let text = state.text();
            items
                .into_iter()
                .filter_map(|item| Candidate::from_item(text, cursor, item))
                .take(MAX_CANDIDATES)
                .collect()
        };

        self.all = all;
        self.text_version = version;
        self.anchor = cursor;

        // 2) 过滤 + 排序。
        let visible = {
            let text = state.text();
            let query = query_before(text, cursor);
            self.query = query.clone();
            filter_candidates(&self.all, &query)
        };
        if visible.is_empty() {
            log::info!(
                "lsp: completion 无可显示候选（收到 {} 条，query={:?}）",
                self.all.len(),
                self.query
            );
            self.visible = false;
            self.candidates.clear();
            self.selected = 0;
            state.clear_completion_menu();
            return false;
        }

        // 3) 优先保留原选中项；否则服务器声明的 `preselect`；否则第一项。
        let selected = previous
            .and_then(|identity| {
                visible
                    .iter()
                    .position(|candidate| candidate.identity() == identity)
            })
            .or_else(|| {
                visible
                    .iter()
                    .position(|candidate| candidate.item.preselect == Some(true))
            })
            .unwrap_or(0);
        self.candidates = visible;
        self.selected = selected;
        self.visible = true;

        log::info!(
            "lsp: completion 菜单已显示，{} 条可见 / {} 条返回，query={:?} anchor={cursor} incomplete={incomplete}",
            self.candidates.len(),
            self.all.len(),
            self.query
        );

        state.set_completion_anchor(cursor);
        state.set_completion_menu(self.candidates.clone(), self.selected);
        true
    }

    /// 取走后台结果（Editor 每帧调用）。
    pub fn take_pending(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        // 结果只属于发起请求的那个编辑器：切 tab 后旧槽不能被新 tab 取走。
        if self.owner.is_some() && self.owner != Some(editor.entity_id()) {
            return;
        }
        let Some(pending) = editor.update(cx, |state, _| state.take_pending_completions()) else {
            return;
        };
        if self.uri.as_ref() != Some(&pending.uri) {
            log::info!("lsp: completion 结果来自已切走的会话，丢弃");
            return;
        }
        let cursor = pending.cursor_offset;
        let version = pending.text_version;
        let incomplete = pending.incomplete;
        let items = pending.items;

        let installed = editor.update(cx, |state, _cx| {
            self.install(state, items, cursor, version, incomplete)
        });
        if !installed {
            self.hide_in(editor, cx);
        }
    }

    /// 每次文本变化时先按当前前缀过滤已有候选。
    ///
    /// 必须在发起新请求**之前**调用：LSP 往返通常几十到几百毫秒，
    /// 只等新结果会让菜单在这段时间里持续展示与当前输入不匹配的旧候选。
    /// Zed 的做法同样是「本地过滤 + 后台刷新」（`set_filter_results`）。
    pub fn refilter(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        if !self.visible || self.all.is_empty() {
            return;
        }
        editor.update(cx, |state, cx| {
            let cursor = state.cursor();
            let version = state.text_version();
            // 替换区间基于旧文本；文本变了就跟着光标平移（见
            // `Candidate::adjust_range_for_edit`）。
            if version != self.text_version {
                let old_anchor = self.anchor;
                let text = state.text();
                for candidate in &mut self.all {
                    candidate.adjust_range_for_edit(text, old_anchor, cursor);
                }
                self.text_version = version;
            }
            self.anchor = cursor;

            let previous = self.selected().map(Candidate::identity);
            let visible = {
                let text = state.text();
                let query = query_before(text, cursor);
                self.query = query.clone();
                // 查询为空（刚敲了触发字符 `.` / `:` / 空格）时，旧候选
                // 与新上下文无关。Zed 在 `query.is_none()` 时同样直接收起
                // 菜单，等服务器针对触发字符返回新结果；继续展示旧列表正是
                // 「候选内容错误」的另一个来源。
                if query.is_empty() {
                    Vec::new()
                } else {
                    filter_candidates(&self.all, &query)
                }
            };
            if visible.is_empty() {
                log::info!("lsp: 本地过滤后无候选，收起菜单（query={:?}）", self.query);
                self.visible = false;
                self.candidates.clear();
                self.selected = 0;
                state.clear_completion_menu();
                state.clear_pending_completions();
                self.request_generation = self.request_generation.wrapping_add(1);
                state.set_completion_request_generation(self.request_generation);
                cx.notify();
                return;
            }

            self.selected = previous
                .and_then(|identity| {
                    visible
                        .iter()
                        .position(|candidate| candidate.identity() == identity)
                })
                .unwrap_or(0);
            self.candidates = visible;
            state.set_completion_anchor(cursor);
            state.set_completion_menu(self.candidates.clone(), self.selected);
            cx.notify();
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

    /// 每帧校验菜单是否仍然对得上当前编辑器/光标。
    ///
    /// 覆盖键盘路径之外的关闭条件：点鼠标、方向键移动光标、切 tab、
    /// 关闭文件。文本输入路径的「继续过滤」由 [`Self::refilter`] 负责，
    /// 它会在本函数之前把锚点更新到新光标。
    pub fn validate(&mut self, editor: &Entity<InputState>, cx: &mut App) {
        if !self.visible {
            // 防御：状态已收起但 InputState 上还留着一份快照（历史上
            // `confirm` 只调了 `hide()`，于是浮窗会永远留在屏幕上）。
            let stale = editor.read(cx).completion_menu().is_some();
            if stale {
                editor.update(cx, |state, _cx| state.clear_completion_menu());
            }
            return;
        }
        let (cursor, version) =
            editor.update(cx, |state, _| (state.cursor(), state.text_version()));
        let has_snapshot = editor.read(cx).completion_menu().is_some();
        if !has_snapshot
            || self.owner != Some(editor.entity_id())
            || self.anchor != cursor
            || self.text_version != version
        {
            log::info!(
                "lsp: completion 编辑器/光标/文本已变化（anchor={} cursor={cursor} version={version} snapshot={has_snapshot}），收起菜单",
                self.anchor
            );
            self.hide_in(editor, cx);
        }
    }

    /// 应用选中的候选：替换区间并写入文本。
    ///
    /// 返回 `Some((item, range))` 表示**调用方需要先 resolve 再应用**：
    /// LSP 允许服务器把 `additionalTextEdits`（自动导入的 `use` 语句）
    /// 延到用户选中时才给（`resolveSupport`）。不 resolve 就拿不到那些编辑，
    /// 表现为「自动导入不生效」。
    ///
    /// 之所以不在这里直接 spawn：本函数只有 `&mut App`，无法把 `cx` move 进
    /// async 块；`Editor` 有 `Context<Editor>`，由它 spawn 更自然。
    /// 本地已有 `additionalTextEdits` 时直接应用并返回 `None`（省一次往返）。
    pub fn confirm(
        &mut self,
        editor: &Entity<InputState>,
        client: Option<Arc<LspClient>>,
        cx: &mut App,
    ) -> Option<(CompletionItem, Range<usize>)> {
        let Some(candidate) = self.selected().cloned() else {
            self.hide_in(editor, cx);
            return None;
        };

        // 文本在响应之后变过（用户继续输入/删字符/按了换行）时，响应里的
        // 区间**整体作废**。
        //
        // 这里曾经退回「当前光标前的词区间」——但当变化是 Enter 插入的换行
        // 时，光标已经在新行行首，词区间是空区间 `cursor..cursor`，补全
        // 就会被写到下一行开头。正确做法是：只要版本对不上，就不应用旧
        // 候选，等下一轮请求（用户继续输入会自然触发）。
        let (cursor, version) =
            editor.update(cx, |state, _| (state.cursor(), state.text_version()));
        if version != self.text_version {
            log::warn!(
                "lsp: completion 响应后文本已变化（anchor={} state_version={} cursor={cursor} text_version={version}），放弃旧区间 {:?}",
                self.anchor,
                self.text_version,
                candidate.replace_range
            );
            self.hide_in(editor, cx);
            return None;
        }
        let range = editor.update(cx, |state, _| {
            clamp_replacement_range(candidate.replace_range.clone(), state.text(), cursor)
        });
        if range != candidate.replace_range {
            log::warn!(
                "lsp: completion 候选区间 {:?} 越过光标 {cursor}，退回词区间 {range:?}；text_edit={:?}",
                candidate.replace_range,
                candidate.item.text_edit
            );
        }
        log::info!(
            "lsp: completion 确认 range={range:?} anchor={} cursor={cursor} version={version}",
            self.anchor
        );

        // **必须用 hide_in**：只调 `hide()` 不会清 `InputState` 上的绘制
        // 快照，浮窗会留在屏幕上（确认后「菜单不消失」的根因）。
        self.hide_in(editor, cx);

        // 服务器支持 resolve 时，缺省/空数组都可能只是「还没给」：
        // 有些服务器把自动导入的 `additionalTextEdits` 延到 resolve 才返回。
        // 已有非空附加编辑时才跳过这次往返。
        let needs_resolve = client
            .as_ref()
            .is_some_and(|client| client.supports_completion_resolve())
            && candidate
                .item
                .additional_text_edits
                .as_ref()
                .is_none_or(|edits| edits.is_empty());
        if !needs_resolve {
            apply_item_with_range(&candidate.item, editor, range, cx);
            return None;
        }
        Some((candidate.item, range))
    }

    /// 当前 uri（用于校验结果来源）。
    pub fn uri(&self) -> Option<&Url> {
        self.uri.as_ref()
    }

    /// 测试入口：把选中候选的替换区间改成指定值，模拟服务器/平移后的
    /// 「区间越过光标」状态。
    #[cfg(test)]
    pub(crate) fn break_selected_range_for_test(&mut self, range: std::ops::Range<usize>) {
        if let Some(candidate) = self.candidates.get_mut(self.selected) {
            candidate.replace_range = range;
        }
    }

    /// 测试入口：只设置 owner/uri，不装候选（菜单不可见）。
    #[cfg(test)]
    pub(crate) fn show_none_for_test(&mut self, owner: EntityId, uri: Url) {
        self.owner = Some(owner);
        self.uri = Some(uri);
    }

    /// 测试入口：不经过 LSP，直接把候选装进菜单。
    #[cfg(test)]
    pub(crate) fn show_for_test(
        &mut self,
        state: &mut InputState,
        owner: EntityId,
        uri: Url,
        items: Vec<CompletionItem>,
        cursor: usize,
    ) -> bool {
        self.owner = Some(owner);
        self.uri = Some(uri);
        let version = state.text_version();
        self.install(state, items, cursor, version, false)
    }
}

/// 后台补全结果在 `InputState` 里的落地形态。
pub struct PendingCompletions {
    pub items: Vec<lsp_types::CompletionItem>,
    /// 发请求时的位置，仅用于日志与诊断。
    pub position: Position,
    /// 发请求时的光标字节偏移；取出时拿它和当前光标比对。
    pub cursor_offset: usize,
    /// 发请求时的文本版本；取出时比对，防止「文本变了但光标恰好没变」。
    pub text_version: u64,
    /// 发请求的 uri；切换会话后旧结果不得进入新菜单。
    pub uri: Url,
    /// 服务器是否声明 `isIncomplete`。
    pub incomplete: bool,
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
/// 定位由 `TextElement::layout_lsp_popovers` 在 prepaint 阶段算：那里才有
/// 当前帧的行布局与滚动偏移（照抄 Zed 的 `layout_hover_popovers` 模型）。
pub fn render_completion_menu_from_state(state: &InputState, cx: &App) -> Option<gpui::AnyElement> {
    let menu = state.completion_menu()?;
    render_completion_rows(menu, cx)
}

/// 渲染补全菜单**内容**（不含定位）。
///
/// 条目数超过 [`MAX_VISIBLE`] 时按窗口滚动；选中项始终可见。
pub fn render_completion_rows(menu: &CompletionMenu, cx: &App) -> Option<gpui::AnyElement> {
    if menu.candidates.is_empty() {
        return None;
    }
    let theme = cx.theme().ui;
    let highlight_theme = &cx.theme().highlight_theme;

    // 窗口滚动：保证选中项始终可见。
    let start = if menu.selected >= MAX_VISIBLE {
        menu.selected + 1 - MAX_VISIBLE
    } else {
        0
    };
    let end = (start + MAX_VISIBLE).min(menu.candidates.len());

    let mut el = div()
        .id("lsp-completion-menu")
        .min_w(px(300.0))
        .max_w(px(560.0))
        .max_h(px(320.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_y(px(1.0))
        .p(px(4.0))
        .rounded(px(10.0))
        .bg(theme.menu_background)
        .border_1()
        .border_color(theme.menu_border)
        .shadow_lg()
        // 菜单画在正文之上：挡住底下的点击，避免用户点候选时把光标
        // 移到菜单下面的位置（补全项本身暂不支持鼠标点击）。
        .occlude()
        .text_size(px(12.5))
        .line_height(px(18.0))
        .text_color(theme.text);

    for (i, candidate) in menu.candidates[start..end].iter().enumerate() {
        let index = start + i;
        let is_selected = index == menu.selected;
        let kind_color = if candidate.deprecated {
            theme.text_muted
        } else {
            kind_color(candidate.kind, cx, highlight_theme)
        };

        let mut label = div()
            .flex_shrink(1.0)
            .max_w(px(400.0))
            .truncate()
            .whitespace_nowrap()
            .text_color(if candidate.deprecated {
                theme.text_muted
            } else {
                theme.text
            })
            .child(candidate.label.clone());
        if candidate.deprecated {
            label = label.line_through();
        }

        let mut row = div()
            .id(("lsp-completion-item", index))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(5.0))
            .whitespace_nowrap()
            // 左侧类型图标（对齐 Zed `render_completion_kind_letter`：
            // 固定宽度的图标槽，没有 kind 时也占位，文字不会左右跳）。
            .child(
                div()
                    .w(px(16.0))
                    .h(px(16.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        ButtonIcon::new(kind_icon_path(candidate.kind))
                            .size(IconSize::Size(px(14.0)))
                            .color(kind_color),
                    ),
            )
            .child(label);

        if let Some(detail) = &candidate.detail {
            row = row.child(div().flex_grow_1()).child(
                div()
                    .flex_shrink(1.0)
                    .max_w(px(260.0))
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.85))
                    .child(detail.clone()),
            );
        }

        if is_selected {
            row = row.bg(theme.accent.opacity(0.16));
        }
        el = el.child(row);
    }

    // 条目超过窗口时给一个数量提示（不实现滚动条：菜单本身是
    // `defer_draw` 出来的，滚轮事件穿透给正文更符合预期）。
    if menu.candidates.len() > MAX_VISIBLE {
        el = el.child(
            div()
                .px(px(8.0))
                .pt(px(2.0))
                .text_size(px(10.0))
                .text_color(theme.text_subtle)
                .child(format!(
                    "{} / {}",
                    menu.selected.saturating_add(1),
                    menu.candidates.len()
                )),
        );
    }

    Some(el.into_any_element())
}

/// 补全类型 → SVG 图标路径。
///
/// 与 `assets/icons/completion/*.svg` 一一对应；未知 kind 落到通用符号。
pub fn kind_icon_path(kind: Option<CompletionItemKind>) -> &'static str {
    match kind {
        Some(CompletionItemKind::METHOD) => "icons/completion/method.svg",
        Some(CompletionItemKind::FUNCTION) => "icons/completion/function.svg",
        Some(CompletionItemKind::CONSTRUCTOR) => "icons/completion/constructor.svg",
        Some(CompletionItemKind::FIELD) => "icons/completion/field.svg",
        Some(CompletionItemKind::VARIABLE) => "icons/completion/variable.svg",
        Some(CompletionItemKind::CLASS) => "icons/completion/class.svg",
        Some(CompletionItemKind::INTERFACE) => "icons/completion/interface.svg",
        Some(CompletionItemKind::MODULE) => "icons/completion/module.svg",
        Some(CompletionItemKind::PROPERTY) => "icons/completion/property.svg",
        Some(CompletionItemKind::UNIT) => "icons/completion/unit.svg",
        Some(CompletionItemKind::VALUE) => "icons/completion/value.svg",
        Some(CompletionItemKind::ENUM) => "icons/completion/enum.svg",
        Some(CompletionItemKind::KEYWORD) => "icons/completion/keyword.svg",
        Some(CompletionItemKind::SNIPPET) => "icons/completion/snippet.svg",
        Some(CompletionItemKind::COLOR) => "icons/completion/color.svg",
        Some(CompletionItemKind::FILE) => "icons/completion/file.svg",
        Some(CompletionItemKind::REFERENCE) => "icons/completion/reference.svg",
        Some(CompletionItemKind::FOLDER) => "icons/completion/folder.svg",
        Some(CompletionItemKind::ENUM_MEMBER) => "icons/completion/enum_member.svg",
        Some(CompletionItemKind::CONSTANT) => "icons/completion/constant.svg",
        Some(CompletionItemKind::STRUCT) => "icons/completion/struct.svg",
        Some(CompletionItemKind::EVENT) => "icons/completion/event.svg",
        Some(CompletionItemKind::OPERATOR) => "icons/completion/operator.svg",
        Some(CompletionItemKind::TYPE_PARAMETER) => "icons/completion/type_parameter.svg",
        _ => "icons/completion/text.svg",
    }
}

/// 补全类型 → 语法高亮名（颜色取自主题的 syntax token）。
fn kind_highlight_name(kind: CompletionItemKind) -> Option<&'static str> {
    Some(match kind {
        CompletionItemKind::CLASS => "type",
        CompletionItemKind::CONSTANT => "constant",
        CompletionItemKind::CONSTRUCTOR => "constructor",
        CompletionItemKind::ENUM => "enum",
        CompletionItemKind::ENUM_MEMBER => "variant",
        CompletionItemKind::FIELD => "property",
        CompletionItemKind::FUNCTION => "function",
        CompletionItemKind::INTERFACE => "type",
        CompletionItemKind::METHOD => "function.method",
        CompletionItemKind::MODULE => "namespace",
        CompletionItemKind::OPERATOR => "operator",
        CompletionItemKind::PROPERTY => "property",
        CompletionItemKind::STRUCT => "type",
        CompletionItemKind::TYPE_PARAMETER => "type",
        CompletionItemKind::VARIABLE => "variable",
        CompletionItemKind::KEYWORD => "keyword",
        CompletionItemKind::SNIPPET => "string",
        CompletionItemKind::COLOR => "string.special",
        CompletionItemKind::REFERENCE => "link_text",
        _ => return None,
    })
}

fn kind_color(
    kind: Option<CompletionItemKind>,
    cx: &App,
    highlight_theme: &crate::theme::syntax_theme::HighlightTheme,
) -> Hsla {
    kind.and_then(kind_highlight_name)
        .and_then(|name| highlight_theme.style(name))
        .and_then(|style| style.color)
        .unwrap_or(cx.theme().ui.icon_muted)
}

/// 当前前缀（光标前的标识符）。
fn query_before(text: &ropey::Rope, cursor: usize) -> String {
    let range = features::word_range_before(text, cursor);
    text.slice(range.start..range.end).to_string()
}

/// 过滤并排序候选。
///
/// 没有查询时保持服务器返回顺序（LSP 的 `sortText` 已经体现在这个顺序里）；
/// 有查询时按匹配质量排序，同分时保持服务器顺序。
fn filter_candidates(all: &[Candidate], query: &str) -> Vec<Candidate> {
    if query.is_empty() {
        return all.iter().take(MAX_CANDIDATES).cloned().collect();
    }
    let mut matched: Vec<(u32, usize, &Candidate)> = all
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            candidate
                .score(query)
                .map(|score| (score, index, candidate))
        })
        .collect();
    matched.sort_by_key(|(score, index, _)| (*score, *index));
    matched
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, _, candidate)| candidate.clone())
        .collect()
}

/// 大小写不敏感的模糊匹配，分数越小越好。
///
/// 分层与 Zed 的 `sort_string_matches` 同思路（精确 > 前缀 > 词首 >
/// 包含 > 子序列），但只保留菜单真正需要的区分度。
fn fuzzy_score(haystack: &str, needle: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(|ch| ch.to_lowercase()).collect();
    let ned: Vec<char> = needle.chars().flat_map(|ch| ch.to_lowercase()).collect();
    if ned.len() > hay.len() {
        return None;
    }
    if hay == ned {
        return Some(0);
    }
    if hay.starts_with(&ned) {
        return Some(1);
    }
    if starts_at_word_boundary(&hay, &ned) {
        return Some(2);
    }
    if hay.windows(ned.len()).any(|window| window == ned) {
        return Some(3);
    }

    // 子序列匹配：字符按顺序出现即可。用跳跃长度当罚分，
    // 让 `camel` 比散落在长签名里的同名子序列更靠前。
    let mut hay_index = 0;
    let mut gaps = 0u32;
    for wanted in &ned {
        while hay_index < hay.len() && hay[hay_index] != *wanted {
            hay_index += 1;
            gaps = gaps.saturating_add(1);
        }
        if hay_index >= hay.len() {
            return None;
        }
        hay_index += 1;
    }
    Some(100 + gaps.min(90))
}

/// `needle` 是否出现在 camelCase / snake_case / 分隔符之后的词首。
fn starts_at_word_boundary(hay: &[char], ned: &[char]) -> bool {
    for index in 0..=hay.len().saturating_sub(ned.len()) {
        let boundary = index == 0
            || !hay[index - 1].is_alphanumeric()
            || hay[index - 1] == '_'
            || (hay[index - 1].is_lowercase() && hay[index].is_uppercase());
        if boundary && hay[index..].starts_with(ned) {
            return true;
        }
    }
    false
}

/// 把一条补全候选写进缓冲区（含 `additionalTextEdits`）。
///
/// 抽成自由函数：`confirm` 的同步路径与 Editor 的 resolve 回调都要用它，
/// 两处必须行为一致（尤其「先附加编辑、再插主文本」的顺序）。
pub fn apply_completion_item(
    item: &lsp_types::CompletionItem,
    editor: &Entity<InputState>,
    range: Range<usize>,
    cx: &mut App,
) {
    apply_item_with_range(item, editor, range, cx);
}

fn apply_item_with_range(
    item: &lsp_types::CompletionItem,
    editor: &Entity<InputState>,
    range: Range<usize>,
    cx: &mut App,
) {
    let edit = features::completion_insert_edit(item);
    let insert = edit.text;
    let additional = item.additional_text_edits.clone().unwrap_or_default();
    // 诊断日志：用户反馈「补全被写到下一行」时，看 range 的起点与 insert
    // 是否带前导换行即可定位是区间问题还是服务器文本问题。
    log::info!(
        "lsp: completion 应用 range={:?} insert={:?} cursor={:?} selection={:?} 附加编辑={:?}",
        range,
        insert.escape_debug().to_string(),
        edit.cursor,
        edit.selection,
        additional
            .iter()
            .map(|edit| format!(
                "{:?}..{:?}={:?}",
                edit.range.start,
                edit.range.end,
                edit.new_text.escape_debug().to_string()
            ))
            .collect::<Vec<_>>()
    );
    editor.update(cx, |state, cx| {
        if !state.apply_completion_with_edits_and_metadata(
            range,
            &insert,
            &additional,
            edit.cursor,
            edit.selection,
            cx,
        ) {
            log::debug!("lsp: stale completion range, discarding");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// `filterText` 不匹配时仍要尝试展示 label：函数的 label 是
    /// `activate(…)`、filterText 可能只有 `activate` 或更窄的别名，不能
    /// 因为 filterText 过窄就把合法方法丢掉。
    #[test]
    fn filter_falls_back_to_label_when_filter_text_misses() {
        let mut item = candidate("activate", Some(CompletionItemKind::METHOD), "fn()");
        item.filter_text = "zzz".into();
        let visible = filter_candidates(&[item], "act");
        assert_eq!(visible.len(), 1, "label 命中时应保留候选");
    }

    /// 候选区间起点在光标之后/终点在光标之前时，必须退回当前词区间；
    /// 这正是「补全被写到下一行开头」的防线。
    #[test]
    fn clamp_replacement_range_pulls_back_to_word() {
        let text = ropey::Rope::from("pri");
        assert_eq!(
            clamp_replacement_range(4..4, &text, 3),
            0..3,
            "起点越过光标的空区间必须替换当前词，而不是在光标后插入"
        );
        assert_eq!(
            clamp_replacement_range(0..1, &text, 3),
            0..3,
            "终点在光标之前说明旧候选只覆盖了部分前缀，也要退回当前词"
        );
        // 精确复现用户日志：`builder|\n`，候选区间却给到换行后的 8..8。
        let with_newline = ropey::Rope::from("builder\nnext");
        assert_eq!(
            clamp_replacement_range(8..8, &with_newline, 7),
            0..7,
            "区间落在换行之后时必须替换光标前的 builder，不能写到下一行"
        );
        assert_eq!(
            clamp_replacement_range(0..3, &text, 3),
            0..3,
            "正常区间保持不变"
        );
    }

    fn solution() -> Vec<Candidate> {
        vec![
            candidate(
                "HashMap",
                Some(CompletionItemKind::STRUCT),
                "std::collections",
            ),
            candidate(
                "HashSet",
                Some(CompletionItemKind::STRUCT),
                "std::collections",
            ),
            candidate("hash_brown", Some(CompletionItemKind::VARIABLE), "let"),
            candidate("Vec", Some(CompletionItemKind::STRUCT), "std"),
        ]
    }

    fn candidate(label: &str, kind: Option<CompletionItemKind>, detail: &str) -> Candidate {
        Candidate {
            label: label.to_string(),
            detail: Some(detail.to_string()),
            kind,
            deprecated: false,
            filter_text: label.to_string(),
            sort_text: label.to_string(),
            insert_text: label.to_string(),
            replace_range: 0..0,
            item: CompletionItem {
                label: label.to_string(),
                kind,
                ..Default::default()
            },
        }
    }

    #[test]
    fn filter_keeps_only_matching_candidates() {
        let all = solution();
        let filtered = filter_candidates(&all, "hash");
        assert_eq!(
            filtered
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["HashMap", "HashSet", "hash_brown"]
        );
        let filtered = filter_candidates(&all, "hs");
        assert!(
            filtered.iter().any(|c| c.label == "HashMap"),
            "子序列应命中 HashMap"
        );
        assert!(!filtered.iter().any(|c| c.label == "Vec"));
    }

    #[test]
    fn empty_query_preserves_server_order() {
        let all = solution();
        let filtered = filter_candidates(&all, "");
        assert_eq!(
            filtered
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["HashMap", "HashSet", "hash_brown", "Vec"]
        );
    }

    #[test]
    fn prefix_ranks_before_subsequence() {
        let all = vec![
            candidate("my_hash_map", Some(CompletionItemKind::VARIABLE), ""),
            candidate("hash", Some(CompletionItemKind::VARIABLE), ""),
            candidate("HashMap", Some(CompletionItemKind::STRUCT), ""),
        ];
        let filtered = filter_candidates(&all, "hash");
        assert_eq!(filtered[0].label, "hash", "精确前缀必须排第一");
        assert_eq!(filtered[1].label, "HashMap");
    }

    #[test]
    fn filter_is_case_insensitive() {
        let all = solution();
        assert!(
            filter_candidates(&all, "hashmap")
                .iter()
                .any(|c| c.label == "HashMap")
        );
    }

    #[test]
    fn kind_icons_are_distinct_for_common_kinds() {
        let kinds = [
            CompletionItemKind::FUNCTION,
            CompletionItemKind::METHOD,
            CompletionItemKind::VARIABLE,
            CompletionItemKind::STRUCT,
            CompletionItemKind::ENUM,
            CompletionItemKind::FIELD,
            CompletionItemKind::PROPERTY,
            CompletionItemKind::MODULE,
            CompletionItemKind::KEYWORD,
            CompletionItemKind::SNIPPET,
            CompletionItemKind::CONSTANT,
            CompletionItemKind::INTERFACE,
            CompletionItemKind::CLASS,
            CompletionItemKind::TYPE_PARAMETER,
            CompletionItemKind::FILE,
            CompletionItemKind::FOLDER,
            CompletionItemKind::COLOR,
            CompletionItemKind::EVENT,
            CompletionItemKind::OPERATOR,
            CompletionItemKind::REFERENCE,
            CompletionItemKind::ENUM_MEMBER,
            CompletionItemKind::CONSTRUCTOR,
            CompletionItemKind::UNIT,
            CompletionItemKind::VALUE,
            CompletionItemKind::TEXT,
        ];
        let paths: HashSet<&'static str> = kinds
            .iter()
            .map(|kind| kind_icon_path(Some(*kind)))
            .collect();
        assert_eq!(
            paths.len(),
            kinds.len(),
            "每个 CompletionItemKind 都应有独立图标"
        );
        // 图标是 `rust_embed` 打进二进制的资源；路径写错时运行时只会静默
        // 不画，必须在测试里挡住。顺便让 usvg 解析一遍，坏 XML/SVG 也不放过。
        let usvg_options = usvg::Options::default();
        for path in paths {
            let asset = crate::assets::Assets::get(path)
                .unwrap_or_else(|| panic!("补全图标资源未嵌入：{path}"));
            assert!(
                usvg::Tree::from_data(&asset.data, &usvg_options).is_ok(),
                "补全图标不是合法 SVG：{path}"
            );
        }
    }

    #[test]
    fn selection_wraps_and_clamps() {
        let mut state = CompletionState::new();
        state.candidates = solution();
        state.move_selection(-1);
        assert_eq!(state.selected, 3, "向上越界应循环到最后一项");
        state.move_selection(1);
        assert_eq!(state.selected, 0, "向下越界应循环回第一项");
        state.select_last();
        assert_eq!(state.selected, 3);
        state.select_first();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn replace_range_follows_typing_at_cursor() {
        let mut candidate = candidate("println", Some(CompletionItemKind::FUNCTION), "");
        candidate.replace_range = 0..3;

        let text = ropey::Rope::from_str("prin");
        candidate.adjust_range_for_edit(&text, 3, 4);
        assert_eq!(
            candidate.replace_range,
            0..4,
            "继续输入后区间终点要跟着光标右移，否则会插入 `printlnn`"
        );

        let text = ropey::Rope::from_str("pri");
        candidate.adjust_range_for_edit(&text, 4, 3);
        assert_eq!(candidate.replace_range, 0..3, "退格后区间终点跟着左移");
    }

    #[test]
    fn empty_text_edit_range_expands_to_typed_word() {
        let mut candidate = candidate("bar", Some(CompletionItemKind::FIELD), "");
        candidate.replace_range = 4..4;
        let text = ropey::Rope::from_str("foo.ba");
        candidate.adjust_range_for_edit(&text, 4, 6);
        assert_eq!(
            candidate.replace_range,
            4..6,
            "空区间只覆盖新输入的两个字符，不能把已输入的 `ba` 原样留下"
        );
    }

    #[test]
    fn backspacing_the_whole_word_does_not_eat_the_prefix() {
        let mut candidate = candidate("bar", Some(CompletionItemKind::FIELD), "");
        candidate.replace_range = 4..7;
        // `foo.bar` 退格到 `foo.`：待替换的词已经为空，区间必须收成
        // `4..4`，不能左移到 `1..4` 把 `oo.` 一起删掉。
        let text = ropey::Rope::from_str("foo.");
        candidate.adjust_range_for_edit(&text, 7, 4);
        assert_eq!(candidate.replace_range, 4..4);
    }

    #[test]
    fn empty_label_falls_back_to_text_edit_for_filtering() {
        let item = CompletionItem {
            label: String::new(),
            text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 3),
                ),
                new_text: "println!".into(),
            })),
            ..Default::default()
        };
        let text = ropey::Rope::from_str("pri");
        let candidate = Candidate::from_item(&text, 3, item).expect("空 label 不应丢掉候选");
        assert_eq!(candidate.label, "println!");
        assert_eq!(
            candidate.filter_text, "println!",
            "过滤文本必须和显示文本用同一个回退值，否则本地过滤会把它筛掉"
        );
    }

    /// 服务器返回的候选先按当前前缀过滤，再组装绘制快照；`preselect`
    /// 必须决定默认选中项，否则结构体字段补全的默认项永远是第一项。
    #[gpui::test]
    fn install_filters_and_honors_preselect(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                div()
            }
        }

        let cell = std::cell::RefCell::new(None);
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
            *cell.borrow_mut() = Some(input.clone());
            Host(input)
        });
        let input = cell.into_inner().unwrap();
        input.update(cx, |state, cx| {
            state.set_text("pri", cx);
            let items = vec![
                CompletionItem {
                    label: "println".into(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    ..Default::default()
                },
                CompletionItem {
                    label: "printf".into(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    preselect: Some(true),
                    ..Default::default()
                },
                CompletionItem {
                    label: "Vec".into(),
                    kind: Some(CompletionItemKind::STRUCT),
                    ..Default::default()
                },
            ];
            let version = state.text_version();
            let mut completion = CompletionState::new();
            assert!(completion.install(state, items, 3, version, false));
            assert_eq!(completion.len(), 2, "`Vec` 不匹配前缀 `pri`，必须被过滤");
            assert_eq!(
                completion
                    .selected()
                    .map(|candidate| candidate.label.as_str()),
                Some("printf"),
                "preselect 项必须成为默认选中项"
            );
            assert_eq!(state.completion_anchor(), Some(3));
        });
    }

    /// 只是移动光标（没有编辑文本）时，菜单必须收起并清掉绘制快照。
    ///
    /// 回归：`confirm` 曾只调 `hide()`，`InputState` 上的快照永远留着，
    /// 浮窗会一直挂在屏幕上；`validate` 就是每帧兜这条路径的。
    #[gpui::test]
    fn validate_hides_menu_when_cursor_moves(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                div()
            }
        }

        let cell = std::cell::RefCell::new(None);
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
            *cell.borrow_mut() = Some(input.clone());
            Host(input)
        });
        let input = cell.into_inner().unwrap();
        let mut completion = CompletionState::new();
        input.update(cx, |state, cx| {
            state.set_text("pri", cx);
            let version = state.text_version();
            assert!(completion.install(
                state,
                vec![CompletionItem {
                    label: "println".into(),
                    ..Default::default()
                }],
                3,
                version,
                false
            ));
        });
        assert!(input.read_with(cx, |state, _| state.completion_menu().is_some()));
        // `install` 通常由 `take_pending` 调用，owner 已由 `fetch` 设好；
        // 这里直接模拟同样的状态，确保测的是「锚点 != 光标」而不是 owner。
        completion.owner = Some(input.entity_id());

        // 光标从锚点 3 移到 2：没有文本变化，只有光标移动。
        input.update(cx, |state, _| {
            state.selected_range = (2..2).into();
        });
        cx.update(|_, cx| completion.validate(&input, cx));
        assert!(!completion.is_visible(), "光标移开后菜单必须收起");
        assert!(
            input.read_with(cx, |state, _| state.completion_menu().is_none()),
            "绘制快照必须同时清掉，否则浮窗会留在屏幕上"
        );
    }
}

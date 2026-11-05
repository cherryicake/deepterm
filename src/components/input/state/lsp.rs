use super::*;

impl InputState {
    #[inline]
    pub fn diagnostics(&self) -> Option<&DiagnosticSet> {
        match &self.mode {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    #[inline]
    pub fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        self.mode.diagnostics_mut()
    }

    /// 当前编辑器语言名（仅 [`InputMode::CodeEditor`]），如 "rust"、"python"。
    /// 记录浮层实际落点与锚点行屏幕 y（由 `TextElement` 在 prepaint 里调用）。
    ///
    /// 同时记下锚点行的屏幕 y，测试就能直接比较「浮层在锚点上方还是下方」，
    /// 不必在测试里重算一遍坐标公式——重算容易与实现漂移，那样测试会
    /// 自洽地通过而真实行为是错的。
    pub fn record_popover_placement(
        &mut self,
        origin: gpui::Point<gpui::Pixels>,
        anchor_y: gpui::Pixels,
    ) {
        self.last_popover_origin = Some(origin);
        self.last_popover_anchor_y = Some(anchor_y);
    }

    /// 记录 hover 浮层在**窗口坐标系**里的实际矩形（`defer_draw` 之后的 bounds）。
    ///
    /// 有了它才能回答「滚轮事件是不是发生在浮层上」。这是必要的，因为：
    /// - GPUI 为每个 `overflow: Scroll` 容器注册的滚动 listener
    ///   （`div.rs::paint_scroll_listener`）**不会** `stop_propagation`，
    ///   所以事件会继续冒泡到 `InputState::on_scroll_wheel`，把编辑器滚走；
    /// - 但那套 listener 是按 paint 顺序注册的，靠「谁先 stop_propagation」
    ///   去和它抢顺序并不可靠。
    ///
    /// 于是改成在**编辑器侧**用几何判断：滚轮落在浮层矩形内就不滚编辑器。
    /// 浮层自身的滚动仍由 GPUI 那套 listener 完成，两者互不干扰。
    ///
    /// 对照 Zed：它的浮层由 `Editor` 自己绘制在独立的 popover 元素里，
    /// 滚轮不会走到编辑器容器上，所以不需要这个判断。
    pub fn record_hover_popover_bounds(&mut self, bounds: gpui::Bounds<gpui::Pixels>) {
        self.hover_popover_bounds = Some(bounds);
    }

    /// 上一次记录的 hover 浮层矩形（见 [`Self::record_hover_popover_bounds`]）。
    pub fn hover_popover_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.hover_popover_bounds
    }

    /// 某个窗口坐标是否落在当前 hover 浮层内。
    ///
    /// 只在**确实有浮层**时才算命中。浮层消失时 `hover_popover_bounds` 必须
    /// 一并清掉 —— 否则指针恰好落在上一张卡片的位置上时，编辑器会莫名其妙
    /// 地滚不动。
    pub fn hover_popover_contains(&self, position: gpui::Point<gpui::Pixels>) -> bool {
        if self.hover_popover.is_none() {
            return false;
        }
        self.hover_popover_bounds
            .is_some_and(|b| b.contains(&position))
    }

    /// 浮层当前是否被键盘接管（`K` 第二次按下）。
    ///
    /// 隐藏逻辑要先问它：被接管期间鼠标移动与点击外部都**不该**收起浮层，
    /// 否则用户刚用 `K` 打开、还没来得及滚动就没了。
    pub fn hover_keyboard_takeover(&self) -> bool {
        self.hover_popover.is_some() && self.hover_keyboard_takeover
    }

    /// 设置键盘接管标志（见字段文档）。
    pub fn set_hover_keyboard_takeover(&mut self, takeover: bool) {
        // 只在有浮层时才有意义：没浮层时置位只会让一个「不存在的接管」
        // 挡住下一次浮层的正常隐藏。
        self.hover_keyboard_takeover = takeover && self.hover_popover.is_some();
    }

    /// 安排一次延迟隐藏：`delay` 之后到期（见字段文档）。
    ///
    /// 调用方是「服务器返回无内容」的后台任务。注意它**不负责**真正收起，
    /// 只记时刻 —— 到期由 [`Self::hover_hide_expired`] 判定，检查点有两处：
    /// 后台任务睡醒后，以及 Editor 每帧的 `take_pending`。
    pub fn schedule_hover_hide(&mut self, delay: std::time::Duration) {
        self.hover_hide_deadline = Some(std::time::Instant::now() + delay);
    }

    /// 取消待执行的延迟隐藏。
    ///
    /// 新内容到来、新请求发出、指针进入卡片时调用 —— 对照 Zed 在 `show_hover`
    /// 里把 `hiding_delay_task` 置空。
    pub fn cancel_hover_hide(&mut self) {
        self.hover_hide_deadline = None;
    }

    /// 直接指定隐藏时刻（供后台任务认领用，见 `hover.rs` 的延迟隐藏）。
    pub fn set_hover_hide_deadline(&mut self, deadline: std::time::Instant) {
        self.hover_hide_deadline = Some(deadline);
    }

    /// 当前安排的隐藏时刻（`None` = 没安排）。
    pub fn hover_hide_deadline(&self) -> Option<std::time::Instant> {
        self.hover_hide_deadline
    }

    /// 延迟隐藏是否已到期（且浮层还在、指针不在卡片上、没被键盘接管）。
    ///
    /// 三个「不隐藏」的条件缺一不可：
    /// - 指针在卡片上：用户正要操作内容；
    /// - 键盘接管中：用户正用 hjkl 看内容；
    /// - 浮层已经没了：无事可做。
    pub fn hover_hide_expired(&self) -> bool {
        self.hover_popover.is_some()
            && !self.hover_pointer_inside
            && !self.hover_keyboard_takeover
            && self
                .hover_hide_deadline
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    }

    /// 上一次浮层落点（测试用观测点）。
    pub fn last_popover_origin(&self) -> Option<gpui::Point<gpui::Pixels>> {
        self.last_popover_origin
    }

    /// 上一次浮层锚点行的屏幕 y（测试用观测点）。
    pub fn last_popover_anchor_y(&self) -> Option<gpui::Pixels> {
        self.last_popover_anchor_y
    }

    /// 语言服务器实时状态（`None` = 该文件没有 LSP）。
    pub fn lsp_status(&self) -> Option<&crate::lsp::status::LspStatus> {
        self.lsp_status.as_ref()
    }

    /// 设置语言服务器状态。
    pub fn set_lsp_status(&mut self, status: Option<crate::lsp::status::LspStatus>) {
        self.lsp_status = status;
    }

    /// 应用一条 `$/progress` 通知，并据此刷新状态。
    ///
    /// 有进行中的进度 → `Working`；全部结束 → 回到 `Ready`
    /// （诊断计数沿用上次 `publishDiagnostics` 的结果）。
    ///
    /// 返回 false 表示状态没变（重复送达的同一条进度），调用方据此
    /// **跳过 `cx.notify()`**：rust-analyzer 索引期每几百毫秒推一条，多数
    /// 内容不变，而 `notify` 的代价是整棵视图树重建。
    pub fn apply_lsp_progress(
        &mut self,
        token: &lsp_types::ProgressToken,
        value: &lsp_types::WorkDoneProgress,
    ) -> bool {
        if !self.lsp_progress.apply(token, value) {
            return false;
        }
        use crate::lsp::status::LspStatus;
        let next = match self.lsp_progress.representative() {
            Some((title, message, percentage)) => Some(LspStatus::Working {
                title,
                message,
                percentage,
            }),
            // 没有进行中的进度：回到就绪。诊断计数沿用上次 `publishDiagnostics`
            // 的结果——进度结束不等于诊断清空。
            None => {
                let (errors, warnings) = match self.lsp_status.as_ref() {
                    Some(LspStatus::Ready { errors, warnings }) => (*errors, *warnings),
                    _ => (0, 0),
                };
                Some(LspStatus::Ready { errors, warnings })
            }
        };
        let changed = self.lsp_status != next;
        if changed {
            self.lsp_status = next;
        }
        changed
    }

    /// 记录诊断计数（`Ready` 状态的「实时状态」部分）。
    ///
    /// 正在工作时只记数不切状态——进度条还在跑，不该被诊断覆盖。
    ///
    /// 返回 false 表示状态没变：计数与上一轮相同（服务器每次改动都重发一遍
    /// 同样的诊断是常态），或本来就在 `Working` 且没有可覆盖的 `Ready`。
    /// 调用方据此跳过重绘。
    pub fn set_lsp_diagnostic_counts(&mut self, errors: usize, warnings: usize) -> bool {
        use crate::lsp::status::LspStatus;
        let next = Some(LspStatus::Ready { errors, warnings });
        // 进度还在跑，且当前态不是 `Ready`（那是 `Working`/`Starting`）：
        // 只记数不切态，状态一字未动。
        if !self.lsp_progress.is_empty()
            && !matches!(self.lsp_status, Some(LspStatus::Ready { .. }))
        {
            return false;
        }
        if self.lsp_status == next {
            return false;
        }
        self.lsp_status = next;
        true
    }

    /// 设置悬浮浮层（锚点 + 内容）。
    ///
    /// `InputState` 是浮层状态的**唯一真相源**：`TextElement` 只持有
    /// `Entity<InputState>`，绘制时要同时拿到锚点与内容才能定位和排版。
    ///
    /// 锚点存**字节偏移而非像素**：像素坐标随滚动/软换行/行号栏宽度变化，
    /// 存下来必然过期（这正是之前浮层位置错乱的原因）。
    pub fn set_hover_popover(
        &mut self,
        anchor_offset: usize,
        info: crate::editor::hover::HoverInfo,
    ) {
        self.hover_popover = Some((anchor_offset, info));
        // 新内容落定：之前安排的延迟隐藏作废；上一份内容量出的卡片宽度
        // 也随之失效（下一帧重新两趟测量）。
        self.hover_hide_deadline = None;
        self.hover_card_width = None;
    }

    /// 已缓存的 hover 卡片宽度（仅当排版可用宽度一致时命中）。
    ///
    /// 见 [`Self::set_hover_card_width`]。
    pub fn hover_card_width(&self, wrap_at: gpui::Pixels) -> Option<gpui::Pixels> {
        self.hover_card_width
            .filter(|(wrapped, _)| *wrapped == wrap_at)
            .map(|(_, width)| width)
    }

    /// 记录 hover 卡片的定宽（`wrap_at` 为本次排版可用宽度，用于失效判断）。
    pub fn set_hover_card_width(&mut self, wrap_at: gpui::Pixels, width: gpui::Pixels) {
        self.hover_card_width = Some((wrap_at, width));
    }

    /// 当前悬浮浮层（锚点偏移 + 内容）。
    pub fn hover_popover(&self) -> Option<(usize, &crate::editor::hover::HoverInfo)> {
        self.hover_popover.as_ref().map(|(o, i)| (*o, i))
    }

    /// 正显示的 hover 卡片是否已经覆盖 `offset`。
    ///
    /// 对照 Zed `hover_popover.rs::same_info_hover`：命中区间的
    /// `start..=end` 包含新偏移，或锚点完全相同（服务器没给命中区间时的
    /// 兜底 —— 签名、纯文本 hover 都是 `range: None`）。
    ///
    /// `HoverState::request_at` 用它决定「要不要重新起防抖」：已经显示在
    /// 覆盖同一区间的卡片上时直接返回，既不重启计时器也不重新发请求 ——
    /// 否则鼠标在同一个符号里抖动会把任务一直往后推，卡片可能永远不出现。
    ///
    /// 用**闭区间** `start..=end`：LSP 常把 hover 结果挂在整个符号区间上，
    /// 而鼠标正好停在符号末字符（该字节属于下一个偏移）时必须算「还在里面」。
    /// Zed 这里同样是 `hover_range.start..=hover_range.end`。
    pub fn hover_popover_covers(&self, offset: usize) -> bool {
        let Some((anchor, info)) = self.hover_popover() else {
            return false;
        };
        if anchor == offset {
            return true;
        }
        info.range
            .as_ref()
            .is_some_and(|range| (range.start..=range.end).contains(&offset))
    }

    /// 清空悬浮浮层。
    pub fn clear_hover_popover(&mut self) {
        self.hover_popover = None;
        // 矩形必须同步清掉：否则 `hover_popover_contains` 会拿一个已经不存在的
        // 卡片去判断命中，导致该区域内的滚动被无谓地吞掉。
        self.hover_popover_bounds = None;
        self.hover_card_width = None;
        // 浮层没了，「指针在浮层上」这件事也随之失效。不重置的话，下一次
        // 浮层弹出时 `hover_pointer_inside` 仍是 true，`hide` 会被无谓地跳过，
        // 表现为「鼠标明明已经移开，浮层却赖着不走」。
        self.hover_pointer_inside = false;
        // 浮层都没了，待执行的延迟隐藏与键盘接管一并作废。
        self.hover_hide_deadline = None;
        self.hover_keyboard_takeover = false;
    }

    /// 标记指针是否正落在 hover 浮层上。
    ///
    /// 由浮层自己的 `on_mouse_move` / `on_mouse_down` 置位，由编辑区的
    /// `on_mouse_move` 复位。隐藏逻辑（`HoverState::hide_in` 与延迟隐藏）
    /// 要先问它：指针在卡片上时不能收起，否则用户根本没法把指针移进去
    /// 点链接或滚动内容 —— 卡片会在指针到达之前就消失。
    ///
    /// 对照 Zed `hover_popover.rs:1068`：它在浮层上挂 `on_mouse_move` 并
    /// `cx.stop_propagation()`，顺带把 `closest_mouse_distance` 归零、
    /// 清掉 `hiding_delay_task`。我们没有独立的 `closest_mouse_distance`，
    /// 用这个布尔量表达同一件事。
    pub fn set_hover_pointer_inside(&mut self, inside: bool) {
        self.hover_pointer_inside = inside;
    }

    /// 指针是否正落在 hover 浮层上（见 [`Self::set_hover_pointer_inside`]）。
    pub fn hover_pointer_inside(&self) -> bool {
        self.hover_pointer_inside
    }

    /// 补全菜单的文本锚点（字节偏移）。
    ///
    /// 与 hover 同一套模型：**只存文本偏移，像素在绘制时算**（照抄 Zed）。
    /// 菜单位置依赖当前帧行布局与滚动偏移，预先算好会过期。
    pub fn set_completion_anchor(&mut self, offset: usize) {
        self.completion_anchor = Some(offset);
    }

    /// 当前补全菜单的文本锚点。
    pub fn completion_anchor(&self) -> Option<usize> {
        self.completion_anchor
    }

    /// 清空补全菜单锚点。
    pub fn clear_completion_anchor(&mut self) {
        self.completion_anchor = None;
    }

    /// 设置补全菜单快照（绘制用）。
    ///
    /// 与锚点一起构成绘制所需的全部信息；两者都由 `Editor` 在候选到达时写入。
    pub fn set_completion_menu(
        &mut self,
        candidates: Vec<crate::editor::completion::Candidate>,
        selected: usize,
    ) {
        self.completion_menu = Some(crate::editor::completion::CompletionMenu {
            candidates,
            selected,
        });
    }

    /// 当前补全菜单快照。
    pub fn completion_menu(&self) -> Option<&crate::editor::completion::CompletionMenu> {
        self.completion_menu.as_ref()
    }

    /// 清空补全菜单（锚点 + 快照）。
    pub fn clear_completion_menu(&mut self) {
        self.completion_anchor = None;
        self.completion_menu = None;
        self.completion_accept_requested = false;
    }

    /// 取走「Enter 希望确认补全」的兜底请求（Editor 每帧调用）。
    pub fn take_completion_accept_request(&mut self) -> bool {
        std::mem::take(&mut self.completion_accept_requested)
    }

    /// 设置 LSP 折叠范围（供显示层合并进折叠状态）。
    pub fn set_pending_folds(&mut self, folds: Vec<crate::lsp::features::FoldRange>) {
        self.pending_folds = folds;
    }

    /// 取走 LSP 折叠范围。
    pub fn take_pending_folds(&mut self) -> Option<Vec<crate::lsp::features::FoldRange>> {
        if self.pending_folds.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending_folds))
    }

    /// 设置 LSP Inlay Hint（按 buffer 行号分组）。
    pub fn set_inlay_hints(&mut self, hints: Vec<crate::lsp::features::InlayHintData>) {
        self.inlay_hints.clear();
        for hint in hints {
            // hint 的偏移 → 行号：inlay 画在它所属那一行。
            let row = self
                .text
                .byte_to_line_idx(hint.offset.min(self.text.len()), ropey::LineType::LF);
            self.inlay_hints.entry(row).or_default().push(hint);
        }
    }

    /// 按 buffer 行号索引的 Inlay Hint（渲染用）。
    pub fn inlay_hints_by_line(
        &self,
    ) -> &std::collections::HashMap<usize, Vec<crate::lsp::features::InlayHintData>> {
        &self.inlay_hints
    }

    /// LSP 文档高亮的落地槽：`Some(vec)` 时优先于本地的「同词高亮」。
    ///
    /// 服务器知道作用域，比按词文本匹配准（同名局部变量与全局变量不会被
    /// 混在一起）。为 `None` 时回退本地逻辑。
    pub fn set_lsp_highlights(&mut self, ranges: Option<Vec<Range<usize>>>) {
        self.lsp_highlights = ranges;
    }

    /// 当前可见区间对应的 LSP `Range`（Inlay Hint 等「只算可见部分」的请求用）。
    ///
    /// 未布局过时返回 `None`。行号取可见行范围（不含折叠隐藏的行）。
    pub fn visible_lsp_range(&self) -> Option<lsp_types::Range> {
        let layout = self.last_layout.as_ref()?;
        let first = *layout.visible_buffer_lines.first()?;
        let last = *layout.visible_buffer_lines.last()?;
        let start = crate::lsp::features::offset_to_position(
            &self.text,
            self.text.line_start_offset(first),
        );
        let end = crate::lsp::features::offset_to_position(
            &self.text,
            self.text.line_content_end_offset(last),
        );
        Some(lsp_types::Range::new(start, end))
    }

    /// 存放后台文档符号请求的结果。
    pub fn set_pending_symbols(&mut self, response: lsp_types::DocumentSymbolResponse) {
        self.pending_symbols = Some(response);
    }

    /// 取走后台文档符号结果。
    pub fn take_pending_symbols(&mut self) -> Option<lsp_types::DocumentSymbolResponse> {
        self.pending_symbols.take()
    }

    /// 光标下的词（命名面板的初始值、重命名前的校验都用它）。
    ///
    /// 光标不在词内时返回空串——调用方据此判断「这里没有可重命名的符号」。
    pub fn word_at_cursor(&self) -> String {
        use crate::components::input::rope_ext::RopeExt as _;
        let offset = self.cursor();
        let range = crate::lsp::features::word_range_before(&self.text, offset);
        // 光标右侧也纳入：光标停在词首时 `word_range_before` 得到空区间。
        //
        // 按字符下标前进并吸附起点：`end += 1` 按字节走会停在多字节
        // 字符中间，随后的 `slice` 会 panic。
        let end = {
            let clamped = self
                .text
                .clip_offset(offset.min(self.text.len()), sum_tree::Bias::Left);
            let len = self.text.chars().count();
            let mut ci = self.text.offset_to_char_index(clamped);
            while ci < len {
                let byte_off = self.text.char_index_to_offset(ci);
                let ch = self.text.char(byte_off);
                if ch.is_alphanumeric() || ch == '_' {
                    ci += 1;
                } else {
                    break;
                }
            }
            self.text.char_index_to_offset(ci)
        };
        if range.start >= end {
            return String::new();
        }
        self.text.slice(range.start..end).to_string()
    }

    /// 该编辑器对应的文件路径（LSP 用）。
    pub fn lsp_file_path(&self) -> Option<&str> {
        self.lsp_file_path.as_deref()
    }

    /// 设置 LSP 用的文件路径。
    pub fn set_lsp_file_path(&mut self, path: impl Into<String>) {
        self.lsp_file_path = Some(path.into());
    }

    /// 光标处的 LSP `Position`（发起 hover / 补全 / 跳转请求时用它）。
    pub fn lsp_cursor_position(&self) -> lsp_types::Position {
        crate::lsp::features::offset_to_position(&self.text, self.cursor())
    }

    /// 光标在内容坐标系里的像素位置（上一次布局的结果）。
    pub fn cursor_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.last_layout.as_ref()?.cursor_bounds
    }

    /// 存放后台 hover 请求的结果（供浮层取走）。
    ///
    /// `offset` 是**这次请求**的目标字符偏移，必须和结果一起存 ——
    /// 取出时拿它当卡片锚点。若只存内容、取出时用「当前鼠标/光标位置」，
    /// 而两者之间又有新请求发出，卡片内容（A 的）和位置（B 的）就会错位。
    /// 这正是之前 `HoverState::last_result_offset` 那个镜像字段想解决的，
    /// 但镜像和槽是两处状态，照样有竞态；合在一起才是原子的。
    pub fn set_pending_hover(&mut self, info: crate::editor::hover::HoverInfo, offset: usize) {
        self.pending_hover = Some((offset, info));
    }

    /// 记录当前 hover 请求的代号（见 `HoverState::request_generation`）。
    ///
    /// 存在这里而不是只留在 `HoverState` 上，是为了让后台任务能比对：
    /// 任务持有 `WeakEntity<InputState>`，够不到 `HoverState`。
    /// 用途是让**过期响应**自我丢弃 —— GPUI 的 `Task` drop 只是 detach，
    /// 旧任务照样会跑完并回写。
    pub fn set_hover_request_generation(&mut self, generation: u64) {
        self.hover_request_generation = generation;
    }

    /// 当前 hover 请求代号（供后台任务判断自己的响应是否过期）。
    pub fn hover_request_generation(&self) -> u64 {
        self.hover_request_generation
    }

    /// 记录当前 hover 请求的目标字符偏移。
    ///
    /// 代号只随 `on_mouse_moved` 递增，所以鼠标**移出编辑区**时代号不变 ——
    /// 只靠代号拦不住那种过期响应。这个偏移是第二道闸门：后台任务比对它，
    /// 不一致就丢弃。
    pub fn set_hover_offset(&mut self, offset: usize) {
        self.hover_offset = Some(offset);
    }

    /// 当前 hover 请求的目标字符偏移（见 [`Self::set_hover_offset`]）。
    pub fn hover_offset(&self) -> Option<usize> {
        self.hover_offset
    }

    /// 记录当前补全请求代号（见 [`Self::completion_request_generation`]）。
    pub fn set_completion_request_generation(&mut self, generation: u64) {
        self.completion_request_generation = generation;
    }

    /// 当前补全请求代号（供后台任务判断自己的响应是否过期）。
    pub fn completion_request_generation(&self) -> u64 {
        self.completion_request_generation
    }

    /// 取走后台 hover 结果（取走即清空，避免重复弹出同一张卡片）。
    ///
    /// 返回 `(请求时的偏移, 内容)` —— 调用方必须用这个偏移当锚点。
    pub fn take_pending_hover(&mut self) -> Option<(usize, crate::editor::hover::HoverInfo)> {
        self.pending_hover.take()
    }

    /// 存放后台补全请求的结果（含来源 uri 与 `isIncomplete`）。
    ///
    /// `cursor_offset` / `text_version` 都是**发请求时**的值，不是收到结果时
    /// 的。
    ///
    /// 必须记下来：`drain_lsp_results` 在 `render` 里跑，而后台任务结束与
    /// 下一帧之间用户可能又敲了字符。那时若直接用「当前光标」去算
    /// `replace_range` 与菜单锚点，就会拿 A 位置的候选按 B 位置替换 ——
    /// 用户看到的正是「候选内容和光标处该有的东西对不上」。同理
    /// [`Self::take_pending_completions`] 要靠它们丢弃已过期的结果。
    pub fn set_pending_completions_from_request(
        &mut self,
        items: Vec<lsp_types::CompletionItem>,
        position: lsp_types::Position,
        cursor_offset: usize,
        text_version: u64,
        uri: Url,
        incomplete: bool,
    ) {
        self.pending_completions = Some(crate::editor::completion::PendingCompletions {
            items,
            position,
            cursor_offset,
            text_version,
            uri,
            incomplete,
        });
    }

    /// 取走后台补全结果，**但光标/文本已经变化时返回 `None`**。
    ///
    /// 任何一处变化都说明候选是按旧文档取的：显示它会让用户看到与光标处
    /// 不匹配的内容，按下去还会替换错区间。与其展示一个错的菜单，不如让
    /// 下一次请求的结果直接接管（`isIncomplete` 时服务器会继续补全）。
    pub fn take_pending_completions(
        &mut self,
    ) -> Option<crate::editor::completion::PendingCompletions> {
        let pending = self.pending_completions.take()?;
        if pending.cursor_offset != self.cursor() || pending.text_version != self.text_version {
            log::debug!(
                "lsp: completion 结果已过期（请求于 offset={}/v{}，当前 offset={}/v{}），丢弃 {} 个候选",
                pending.cursor_offset,
                pending.text_version,
                self.cursor(),
                self.text_version,
                pending.items.len()
            );
            return None;
        }
        Some(pending)
    }

    /// 当前文本版本（补全确认时用于判断替换区间是否仍然有效）。
    pub fn text_version(&self) -> u64 {
        self.text_version
    }

    /// 丢弃尚未取走的补全结果。
    ///
    /// 收起菜单时必须调用：后台任务可能在收起**之前**就把结果写进槽里，
    /// 只抬代号拦不住已经落地的 pending；下一帧 `drain_lsp_results` 会把它
    /// 重新装进菜单（用户按 Esc / 确认后弹窗又出现）。
    pub fn clear_pending_completions(&mut self) {
        self.pending_completions = None;
    }

    /// 应用一条补全：主插入 + `additionalTextEdits`（如自动导入）。
    ///
    /// LSP 的 `CompletionItem.additionalTextEdits` 是**接受该候选时必须一并
    /// 应用**的编辑——rust-analyzer 用它插入 `use` 语句（自动导入）。
    /// 不处理的话，接受 `HashMap` 会得到「未导入」的错误而不是可用的代码。
    ///
    /// 顺序很重要：**先应用附加编辑，再插入主文本**。附加编辑的 range 是
    /// 基于「插入前」的文档坐标（通常落在文件顶部），若先插入主文本，顶部
    /// 之后的偏移会整体平移，附加编辑就会打到错误位置。
    ///
    /// 返回 false 表示主插入的区间失效（文本被并发改短），调用方应放弃本次
    /// 补全——此时附加编辑也不该生效。
    pub fn apply_completion_with_edits(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        additional: &[lsp_types::TextEdit],
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_completion_with_edits_and_metadata(range, new_text, additional, None, None, cx)
    }

    /// 带 snippet 光标/占位符选择的补全应用。
    ///
    /// `cursor` / `selection` 是相对 `new_text` 的**字节**偏移；插入完成后
    /// 再换算成绝对位置。这样 `foo(${1:a})$0` 会选中参数 `a`，
    /// `println!($0)` 会把光标放进括号，而不是停在整段文本末尾。
    pub fn apply_completion_with_edits_and_metadata(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        additional: &[lsp_types::TextEdit],
        cursor: Option<usize>,
        selection: Option<std::ops::Range<usize>>,
        cx: &mut Context<Self>,
    ) -> bool {
        // 先把附加编辑换算成字节区间（基于当前文本），并从后往前排序。
        let additional_bytes =
            crate::lsp::features::to_byte_edits(self.text(), additional.to_vec());

        // 逐条应用（已按起点降序）。任一区间失效就整体放弃，避免留下
        // 「导入加了但名字没插」的半成品。
        for edit in &additional_bytes {
            if !self.apply_completion(edit.range.clone(), &edit.new_text, cx) {
                log::debug!("lsp: stale additionalTextEdit range, aborting completion");
                return false;
            }
        }

        // 主插入的区间是基于**原始**文档算的；附加编辑若落在它之前，
        // 偏移会平移。按平移量修正后再插入。
        let shift: i64 = additional_bytes
            .iter()
            .filter(|e| e.range.start <= range.start)
            .map(|e| e.new_text.len() as i64 - (e.range.end - e.range.start) as i64)
            .sum();
        let shifted = if shift == 0 {
            range
        } else {
            let start = (range.start as i64 + shift).max(0) as usize;
            let end = (range.end as i64 + shift).max(0) as usize;
            start..end
        };
        let base = shifted.start;

        if !self.apply_completion(shifted, new_text, cx) {
            return false;
        }

        // `apply_completion` 默认把光标放到插入文本末尾；snippet 有 `$0`
        // 或第一个 tabstop 时覆盖它。
        let text_len = self.text.len();
        if let Some(selection) = selection {
            let start = (base + selection.start).min(text_len);
            let end = (base + selection.end).min(text_len);
            if start <= end && self.text.is_char_boundary(start) && self.text.is_char_boundary(end)
            {
                self.selected_range = (start..end).into();
            }
        } else if let Some(cursor) = cursor {
            let pos = (base + cursor).min(text_len);
            if self.text.is_char_boundary(pos) {
                self.selected_range = (pos..pos).into();
            }
        }
        self.update_preferred_column();
        true
    }

    /// 应用一条 LSP 补全：按字节区间替换文本并把光标放到插入文本之后。
    ///
    /// 单独成方法而不是复用 `replace_text_in_range`：后者要 `Window` 且收
    /// UTF-16 区间，而补全结果本来就是字节区间（在 `features` 里换算过），
    /// 再转一次 UTF-16 只会引入偏移错误的机会。
    ///
    /// 返回 false 表示区间非法（文本在请求期间被改短了），调用方应放弃本次
    /// 补全而不是写坏缓冲区。
    pub fn apply_completion(
        &mut self,
        range: std::ops::Range<usize>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let len = self.text.len();
        if range.start > range.end || range.end > len {
            return false;
        }
        // 区间必须落在字符边界上，否则 `Rope` 的切片/渲染会 panic。
        if !self.text.is_char_boundary(range.start) || !self.text.is_char_boundary(range.end) {
            return false;
        }
        if self.disabled {
            return false;
        }

        // 撤销记录需要**编辑前**的文本：只克隆受影响的那一段。
        let old_text: String = self.text.slice(range.clone()).to_string();
        let new_offset = range.start + new_text.len();
        let old_selection = self.selected_range;

        // 折叠要先按这次编辑调整：折叠存的是字节偏移，不调整的话它们会
        // 指向新文本里无关的位置（甚至越界）。顺序与
        // `replace_text_in_range_inner` 一致。
        self.display_map.adjust_folds_for_edit(&range, new_text);
        self.adjust_git_diff_for_edit(&range, new_text);

        self.text.remove(range.clone());
        self.text.insert(range.start, new_text);

        self.display_map
            .on_text_changed(&self.text, &range, &Rope::from(new_text), cx);

        self.history.push(
            Change::new(range.clone(), &old_text, range.start..new_offset, new_text)
                .with_selections(
                    old_selection.start..old_selection.end,
                    new_offset..new_offset,
                ),
        );
        self.history.end_grouping();
        self.bump_text_version();
        self.selected_range = (new_offset..new_offset).into();
        self.ime_marked_range.take();
        self.update_preferred_column();
        self.mode.update_auto_grow(&self.display_map);
        self.update_search(cx);

        // 语法高亮走同步路径：这里没有 `Window`，起不了后台解析任务。
        let _ = self
            .mode
            .update_highlighter(&range, &self.text, new_text, true, cx);

        cx.emit(InputEvent::Change);
        cx.notify();
        true
    }

    /// 整体替换诊断集合（LSP 的主要入口）。
    ///
    /// 与 [`Self::diagnostics_mut`] 的区别：LSP 诊断要先按当前文本换算成
    /// 字节区间，调用方手上只有 `&mut self`，拿不到第二个不可变借用。
    /// 所以把「算 + 写」收进一个方法里，由内部决定借用顺序。
    ///
    /// 返回 false 表示这不是代码编辑器。
    pub fn set_diagnostics(&mut self, items: Vec<Diagnostic>) -> bool {
        let Some(set) = self.mode.diagnostics_mut() else {
            return false;
        };
        set.set(items, &self.text);
        true
    }

    /// 带版本门控的诊断替换（`publishDiagnostics` 的入口）。
    ///
    /// 服务器可能乱序推送（先发 v5 的分析、后发 v7 的），若让旧的覆盖新的，
    /// 用户会看到基于旧文本的错误位置。规则：
    /// - `version` 为 `Some(v)` 且 `v < 已应用版本` → **丢弃**，返回 `false`；
    /// - `version` 为 `None` → 照常应用（部分服务器不报版本，不能因此拒收）；
    /// - 否则应用并记下版本。
    ///
    /// 返回值表示「是否应用了」；`false` 既可能是版本过期，也可能是该编辑器
    /// 不是代码编辑器（无诊断集合）。
    pub fn set_diagnostics_versioned(
        &mut self,
        items: Vec<Diagnostic>,
        version: Option<i32>,
    ) -> bool {
        if let Some(v) = version
            && let Some(applied) = self.applied_diagnostics_version
            && v < applied
        {
            log::debug!("lsp: dropping stale diagnostics (version {v} < applied {applied})");
            return false;
        }
        if !self.set_diagnostics(items) {
            return false;
        }
        if version.is_some() {
            self.applied_diagnostics_version = version;
        }
        true
    }

    /// 已应用诊断的文档版本（测试与调试用）。
    pub fn applied_diagnostics_version(&self) -> Option<i32> {
        self.applied_diagnostics_version
    }

    /// 清空 LSP 诊断及其版本门控（服务器重连/停止时用）。
    ///
    /// 新会话的 `didOpen` 从 version 0 重新计数。如果保留旧会话的
    /// `applied_diagnostics_version`（例如 42），新服务器推出的 v0/v1
    /// 会被门控当成过期诊断全部丢掉，表现为“重启后诊断永远不出现”。
    /// 因此重连/停止必须同时清掉诊断、版本号和状态栏计数。
    pub fn reset_lsp_diagnostics(&mut self) {
        self.applied_diagnostics_version = None;
        self.set_diagnostics(Vec::new());
        self.set_lsp_diagnostic_counts(0, 0);
    }

    /// 滚到某个偏移并居中（LSP 跳转定位用）。
    pub fn reveal_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.scroll_to_center(offset, cx);
    }

    /// 应用 LSP 给的折叠范围。
    ///
    /// 只在**当前没有任何折叠**时应用：用户手动折过/展开过之后不再覆盖，
    /// 否则服务器每推一次范围就把用户的操作重置回去。
    pub fn apply_lsp_folds(
        &mut self,
        folds: Vec<crate::lsp::features::FoldRange>,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_folding() || folds.is_empty() {
            return;
        }
        // `folds()` 为空才算「用户没折过」：一旦有过折叠就交回用户控制。
        if !self.display_map.folds().is_empty() {
            return;
        }
        let text = self.text.clone();
        let mut ranges = Vec::with_capacity(folds.len());
        for fold in folds {
            let start = text.line_start_offset(fold.start_line as usize);
            // 结束行用「下一行行首」而不是本行行尾：折起来后要连带吃掉
            // 行尾换行，否则折叠区与下一行之间会留一条空缝。
            let end = text
                .line_start_offset(fold.end_line as usize + 1)
                .min(text.len());
            if start < end {
                ranges.push(start..end);
            }
        }
        if ranges.is_empty() {
            return;
        }
        self.display_map.fold_ranges(&text, ranges);
        self.after_fold_change(cx);
    }
}

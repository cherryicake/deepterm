# 性能与内存审计报告（2026-09-12）

对 DeepTerm 源码（约 10.9 万行 Rust，`src/` 共 174 个 `.rs` 文件）做的一轮
针对 **内存占用与性能** 的审计。重点覆盖热路径：终端渲染、Markdown 实时预览
编辑、代码编辑器（含语法高亮）、输入 display map、文件树加载/监听。

**前提结论**：终端渲染管线（`TerminalElement`）在既有
`docs/PERF_2026-09-06.md` 中已做过一轮非常扎实的优化（选区 O(1) 区间求交、
消除 FlattenedRenderCache 双份存储、行缓存 take/restore）；本地/远端文件加载
均在后台线程；代码编辑器撤销用**增量 Change**（不是整文档快照）。以下是本轮
新发现的、仍可优化的真实问题。

---

## 内存问题

### M1. Markdown 实时预览的撤销栈按「整文档快照」增长，且无上限（高）

`src/editor/markdown_live/model.rs` 的 `EditorModel`：

```rust
struct Snapshot { lines: Vec<String>, cursor: Pos }   // 一行注释：整文档快照
fn push_undo(&mut self, coalesce: bool) {
  if !(coalesce && self.coalescing) {
    self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
  }
  ...
}
```

- 每次**非合并**编辑（多字符插入、替换选区、删除整行、包裹、查找/替换、回车
  分页等都会 `push_undo(false)`）都会 `self.lines.clone()` —— 拷贝**整个文档**。
- 只有「连续单字符击键」会合并在一步；任何移动/其它编辑都打断合并。
- `undo` / `redo` 两个 `Vec<Snapshot>` **没有深度上限**，只在 `set_text` 时清空。
- 对比：代码编辑器撤销用的是 `History<Change>`（`change.rs` 只存改动 range 的
  old/new 文本），是增量的。Live 编辑器用的是全量快照，两者不一致。

**后果**：一个 5 千行的大型 md，用户做 50 次独立编辑（即使每次只改 1 行），
撤销栈就持有 50 份整文档 ≈ 50× 文档字节数。长期编辑大型文档时内存显著膨胀。
这是一处容易在运行时持续回填的内存大户。

**建议**：
- 复制代码编辑器的 `Change` 增量模型 —— 撤销步只存 `(range, old_text, new_text)`
  + 光标，`undo()` 反向应用。Live 文档本来就是逐行的 `Vec<String>`，行级/区间
  增量实现成本低。
- 若保持快照式，至少给 `undo.len()` 设上限（如历史 100 步），并在入栈时对
  `lines` 用 `Vec::with_capacity(len)` 精确分配避免反复扩容；或把已删除的远端
  快照从 `redo` 中移项时复用容量。
- 对超大文档，可在 `push_undo` 前做「复制开销 > 阈值则截断最老历史」的保护。

### M2. `HighlightCache` 是无界 LRU 缺失的 HashMap，长会话慢泄漏（中-高）

`src/editor/markdown_live/highlight.rs`：

```rust
pub struct HighlightCache { inner: Rc<RefCell<CacheMap>> }  // HashMap<(String,String), CacheValue>
```

- 缓存键是 `(language, content.to_string())` —— 代码块的**逐键内容**。
- 每次在代码块里敲一个字符，`content` 就变一次 → **新键永久插入**，旧键从不
  淘汰。只在 `set_text` / 主题切换时 `clear()`（`view.rs:249/2266`）。
- 值里还存 `Vec<TextRun>`（或块级 `Vec<Vec<TextRun>>`），随文档持续增长。

**后果**：在长会话里编辑一个带代码块的 md，历史每个中间版本的整块高亮 runs
都残留，内存无限增长。

**建议**：
- 给缓存加一个容量上限（如 64~256 键），插入时若超限驱逐最久未用（简易 FIFO
  或 `VecDeque` LRU）。键只用于命中，驱逐后下次重算即可（本来就有 miss 重算）。
- 更优：按「行号/块」失效而非「内容字符串」—— 代码块内容只在块内编辑时变，
  命中时用文本版本号做二次确认。
- 键避免 `content.to_string()` 冗余分配：用 `Rc<str>` 或把整块用 ropey/hash 指纹
  做键，减少每次查键的字符串分配。

### M3. `group_heights` / `row_layouts` 等 HashMap 跨滚动无界保留（中）

`src/editor/markdown_live/view.rs`：
- `group_heights: HashMap<usize, f32>` 每访问一个 group（滚动经过）就
  `insert(plo+k, h)`（`view.rs:2067`），只在**视口宽变化**时整表清空
  （`view.rs:2009`）。滚过整篇大文档后，`group_heights` 保留所有访问过的 group。
- `row_layouts: HashMap<usize, RowLayoutInfo>` 每帧绘制后回填上次布局，但有
  `row_layouts.clear()`（`view.rs:2284`）所以每帧大小有界 —— 这一点 OK。

`group_heights` 虽以文档规模为界（不算纯泄漏），但远比需要的窗口大，且滚动
遍历大文档会保留大量已离开视口的项。建议改成有界窗口（只保留当前视口窗口
内的 group），随滚动滑动删除。

### M4. 终端 scrollback 固定 10 000 行 ≈ 15 MB/终端（已知，产品决策）

已在 `docs/PERF_2026-09-06.md` O3 量化（80 列默认约 15 MB/终端），且已定为
不引入设置项。若未来在意多终端内存，可考虑默认调低或按需增长；此处仅记录，
不做改动建议。

---

## 性能问题

### P1. Live 预览每次击键对**整篇文档**重新 plan + 重新分层（高，大文档）

`src/editor/markdown_live/view.rs::planned_rows`：

```rust
let key = (cursor, selection, focused, read_only, show_source);
if self.planned_version != self.buffer.text_version() || self.planned_key != key {
    // 每次 text_version 变化（即每次击键）都会执行整个 plan_document
    let mut planned = plan_document(self.buffer.lines(), reveal);
    ...
}
```

`plan_document`（`reveal.rs:125`）对**全部行**做 O(n) 全量计算：
1. 逐行 classify（带 lookahead）构造 `Vec<Block>`；
2. 反向 setext 扫描；
3. 计算全部行的 reveal 集合；
4. `collect_refdefs` 整篇扫描引用定义；
5. 逐行 `plan_with_refs`。

渲染端已经有行虚拟化（只建视口内元素），但**规划端没有** —— 渲染虚拟化只省了
元素构建，`planned` 每一击键仍重建整篇。对大 md（数千行）每秒会造成多次 O(n)
数组分配 + 分层。

**建议**：
- **增量规划**：击键只影响编辑点所在行的 block 分类及其所在的连续块
  （fence/frontmatter/table 的运行边界），从编辑行向后直到遇到不可受影响的块
  边界即可局部重算，其余行复用上一版 `planned`。
- refdefs 已有跨帧缓存的先例（`table_refs` 按 text_version+key 缓存），可把
  `plan_document` 里的 `collect_refdefs` 也纳入同款缓存，避免每击键重扫全文。
- 大文档可考虑把 planning 移到后台任务（与代码编辑器 tree-sitter 后台解析同款
  调度），规划完成再回填。

### P2. 代码编辑器每次击键在主线程克隆整份 Rope 并做同步 tree-sitter 解析（高）

链路：`element.rs::prepare_if_needed` → `mode.rs::update_highlighter` →
`simple.rs::Highlighter::update`：

```rust
// mode.rs, force=true, 每次文本变更
let completed = h.update(Some(edit), text, Some(30ms 超时));   // 主线程同步
// simple.rs
self.text = text.clone();                                     // 克隆整份 Rope
self.parser.parse_with_options(..., timeout);                  // 同步解析，上限 30ms
```

- `self.text = text.clone()`：`ropey::Rope::clone` 拷贝整棵树（叶片数≈行数），
  无论改动多小 —— 每次击键都 O(n) 分配。
- 主线程同步解析带 **30ms 超时**：命中超时则转后台；但大文件即便增量解析也可能
  吃掉这段 CPU，导致键入时 UI 短暂卡顿（在 30ms 内完成则每次都白付）。

**建议**：
- 避免整份 Rope 克隆：把高亮器的文本改成共享引用语义（`Arc<Rope>`/`Rc<Rope>`，
  或 tree-sitter 需要的 `&mut [u8]` 用一份持有，变更时只更新编辑区间）。这样每次
  击键不再整体复制文件。
- 把同步解析预算压得更低（如 5~10ms）或直接走后台解析 + 版本号去重（现有
  `dispatch_background_parse` 已经具备 stale 丢弃与 debounce 取消逻辑），让主线程
  几乎不阻塞，仅靠上一棵树顶到后台结果落地。

### P3. `WrapMap::rebuild_cache` 每次编辑全量重建前缀和（中，长文本）

`src/components/input/display_map/wrap_map.rs`：

```rust
pub fn on_text_changed(...) {
  self.wrapper.update(...);       // wrapper 是增量的
  self.rebuild_cache();           // O(总行数) 全量重建 buffer_line_starts
}
fn rebuild_cache(&mut self) {
  // 仅当行数与 wrap 行数都与缓存一致时 early-exit；
  // 中段编辑（行数变化）时必然全量重走所有 LineItem 累计前缀和
  for line_item in &self.wrapper.lines { self.buffer_line_starts.push(...); }
}
```

- 编辑发生在长文件**中段**时，`on_text_changed` → 行数/总 wrap 行数都会变 →
  前缀和从第 0 行重算到末尾，O(n)。注释也明说 `rebuild_cache` 是 O(总行数)。
- `buffer_line_starts` 是 `buffer line → 该行首个 wrap_row` 的前缀和。

**建议**：把前缀和改成**后缀式增量** —— 编辑点之后的行，其 `buffer_line_starts`
只是整体平移（编辑行之后每行的 wrap_row 起点都 += 该行的 wrap 增量），可
`O(编辑行至文件尾)` 而非「前段 + 尾段」全量；或按行缓存增量标记 + 惰性修正
（查询时若行号 < dirty 区则正确，否则按 dirty 区起重算）。至少把 early-exit
条件放宽：当只有**文件末尾之后**的行数变化时直接就地追加而不重扫前段。

### P4. `self.table_refs.clone()` 每个渲染帧克隆整张 HashMap（低）

`view.rs:2743`，只要视口有表格就每帧 `self.table_refs.clone()` 整表拷贝（该表
是全文引用定义，可能大）。改成 `Rc<HashMap<..>>` 共享引用，仅在重算时重建。

---

## 其它观察

- 事件枚举做了装箱（`FileTreeEvent` 用 `Box<SshConnectionConfig>` 缩体积）；
  watcher 批量 `buffer_unordered(8)` 有界并发 —— 远程监听风暴已有抑制，做得好。
- 文件树监听有 content-hash / meta 变更检测与「首轮基线不重复 emit」，事件风暴
  抑制总体健康。
- 撤销/重做中代码编辑器与 Live 编辑器模型不一致（见 M1），建议统一。

## 优先级建议

| 项 | 类型 | 严重度 | 影响场景 |
|---|---|---|---|
| M1 撤销整文档快照 | 内存 | 高 | 大 md 长期编辑 |
| P1 plan_document 全量重算 | 性能 | 高 | 大 md 逐键卡顿 |
| P2 主线程 Rope 克隆+同步解析 | 性能+内存 | 高 | 大源码逐键卡顿 |
| M2 HighlightCache 无界 | 内存 | 中-高 | 长会话编辑代码块 |
| P3 WrapMap 全量前缀重建 | 性能 | 中 | 长文本中段编辑 |
| M3 group_heights 无界滚动保留 | 内存 | 中 | 滚动遍历大文档 |
| M4 scrollback 15MB/终端 | 内存 | 中(已知) | 多终端 |
| P4 table_refs 每帧克隆 | 性能 | 低 | 含表格的文档 |

建议先做 M1（撤销增量）与 P1（plan 增量），它们对「大型 md 文档」是同一处
瓶颈的两面；再做 P2 的 Rope 共享与解析预算下调。

---

## 实装结果（2026-09-12 当日落地，均已通过测试）

按「低风险 · 可充分测试」原则，本轮实装了 4 项优化，全部有单元测试支撑；
高风险的 P1（plan 增量）、P2（Rope 共享 + 主线程解析预算）留作下一步。

### 已实装并验证

| 项 | 改动文件 | 验证方式 | 结果 |
|---|---|---|---|
| **M2** HighlightCache 有界 | `markdown_live/highlight.rs` | 新增 `store_stays_bounded_and_evictions_recompute`、`reinsert_after_eviction_keeps_bounded` 两个测试 | ✓ 缓存键数硬上限 128，驱逐后自动重算，`clear` 同时清空 FIFO 队列 |
| **M1** 撤销历史有界 | `markdown_live/model.rs` | 保留全部 49 个既有测试；新增 `undo_history_is_bounded_and_drops_oldest` | ✓ 从「无上限整文档快照」改为 `MAX_UNDO_STEPS=200` 上限 + `VecDeque` O(1) 丢弃最旧；输入 2200 次编辑后仍有界，LIFO 语义不变（见下表） |
| **P3** WrapMap 前缀和增量重建 | `display_map/text_wrapper.rs`, `display_map/wrap_map.rs` | 新增 `incremental_prefix_matches_full` 随机平价测试 | ✓ `rebuild_cache` 改为从受影响起始行向后重建，跳过干净前缀；编辑位于长文件尾部时接近 O(1)，等价于旧的全量重建 |
| **P4** `table_refs` 改 Rc 共享 | `markdown_live/view.rs` | 全库测试 | ✓ 每帧 `clone()` 从整表拷贝变为引用计数自增 |

**关于 M1 的说明**：本轮交付的是「**有界撤销历史**」这一低风险、可验证的部分
（把最坏情况内存从无界降为 `MAX × 文档`），不是完整的「变更增量」撤销。完整方案
（每步只存 `(range, 前后文本)`，undo 反向应用）需要重构全部编辑原语与合并/多行
语义，侵入大、风险高，故列为下一步。

**验证证据**（`cargo test --lib` 全量 767 通过；`markdown_live` 188 通过；
`display_map` 21 通过；`model` 50 通过）：

```
undo_history_is_bounded...  ok   // n=2200 次编辑后仍只保留 200 步
incremental_prefix_matches_full ok // 增量结果 == 全量结果
store_stays_bounded...       ok   // 2000+ 键后缓存 ≤ 128
```

### 正向效果

- **M2**：长会话编辑代码块不再残留每个历史版本的整块 runs —— 缓存内存从「随会话
  无限增长」变为硬上限，且驱逐后下次 miss 自动重算（成本与原来无缓存时相同）。
- **M1**：撤销栈最坏内存从「随编辑次数线性增长」降为固定 200 × 文档大小，大文档
  长期编辑的内存显著收敛。
- **P3**：长文本**尾部**编辑（最常见的键入/追加位置）从 O(总行数) 降到 O(脏尾)；
  已用随机平价测试证明与旧全量重建输出完全一致（正确性成立，且严格 ≤ 旧成本）。
- **P4**：含表格文档每帧少一次整表 `HashMap` 拷贝。

### 仍待实装（高风险，建议拆小任务逐个做）

- **P1** `plan_document` 全量重算 → 增量/refdefs 缓存（大 md 逐键卡顿，渲染已虚拟化，
  规划端未做；需精确定义「不可被编辑影响的块」增量边界）。
- **P2** 代码编辑器主线程 `Rope::clone` + 同步 tree-sitter 30ms 超时 → Rope 共享引用
  /下调同步预算（涉及 `simple.rs` 高亮器与后台解析调度）。
- **M3** `group_heights` 跨滚动无界 → 有界滚动窗口。
- **M1（完整版）** 快照撤销 → 变更增量撤销（见上）。

---

## 追加实装（同日晚些）：M3 + P2 结论修正

### M3：`group_heights` 滚动窗口化（已实装并测试）

`view.rs` 的 `group_heights: HashMap<usize,f32>` 原先只在视口宽变化时清空；滚动
遍历大文档时，每个访问过的 group 高度永久留存 → 随滚动距离增长。

- 新增 `prune_group_heights(lo, hi)`：每帧只保留当前窗口 `lo..=hi` ±
  `GROUP_HEIGHT_KEEP_MARGIN(128)` 内的实测高度，其余淘汰。
- 安全性：淘汰的 group 读取时回退到估算（`prefix` 与速率采样都有
  `unwrap_or_else` 估算兜底），下一帧进入窗口时实测自修正；窗口迟滞
  `WIN_HYST_GROUPS=6` 远小于 128 余量，不干扰既有高度反馈。
- 保留内存从「随滚动遍历整个大文档增长」变为**与文档规模无关的常数窗口**。
- 验证：新增 `group_heights_pruned_to_rolling_window`（种入 0..50k 条高度，
  按 `lo=30000,hi=30100` 剪枝，断言 ≤ 窗口+2×margin、窗口内保留、远处淘汰、
  边缘保留）。`cargo test --lib` 全量 **768 通过**（live 189 通过；滚动稳定性
  与虚拟化测试均不受影响）。

### P2 结论修正（重要：原报告对 Rope clone 的成本判断有误）

原报告 P2 声称「代码编辑器每次击键在主线程 `Rope::clone` 整份」是 O(n)。经核查
ropey-2.0.0-beta.1：`Rope` 内部 `Node` 由 `Arc<Text>` / `Arc<Children>` 支撑，
`#[derive(Clone)]` 的 `Rope::clone` 只是**引用计数自增（O(1)）**，不是 O(n)。
因此 `simple.rs` 的 `self.text = text.clone()` 是廉价的；`self.text` 确实需要在
`styles()`（tree-sitter `RopeTextProvider`）与 offset 裁剪中继续持有，不能省。

**修正后的 P2 实况**：主线程的真实成本是**同步 tree-sitter 解析**（30ms 超时
预算，超时转后台）。但由于是**增量解析**（`tree.edit` 只重解析编辑区），大文件
上单次小编辑的重解析通常远低于 30ms；仅当编辑迫使大范围重解析时才吃满预算。
而把 30ms 预算下调会把中等文件从「同步即时」推到「后台稍迟」，高亮延迟反而变差
——是调参取舍，没有明确正向收益。**结论：P2 不再改动**，保留原设计（增量解析 +
后台兜底已是合理方案）。

### 仍待实装（准确范围）

- **P1** `plan_document` 每次文本/光标变化对整篇文档 O(n)：分类 pass + setext
  反向 + reveal 集合 + 逐行 plan。唯一干净的中间优化是「把**分类结果**按
  text_version 缓存，光标移动时只重算 reveal+逐行 plan（省去 ~1/4 的 O(n)）」；
  但增量 reveal 与逐行 plan 仍依赖光标位，无法跳过 → 完整增量规划需重新设计
  reveal 表示。属实质工程，另立任务做。

---

## P1 中间优化：分类结果缓存（实装并测试）

`plan_document` 被拆成两部分（`reveal.rs`）：
- `classify_document(lines) -> Classified`：**仅文本相关**（doc-order 分类 + setext 反向 restyle）。
- `plan_rows(lines, &Classified, reveal) -> Vec<PlannedRow>`：reveal 集合 + refdefs + 逐行 plan（光标相关，无法跳过）。
- `plan_document` 保留为 `plan_rows(classify_document(...))` 的聚合（所有既有调用点零改动）。

`view.rs::planned_rows` 新增 `planned_classified` + `planned_classified_version`：
**只有文本变更才重新分类；纯光标/选区移动直接复用缓存**，跳过两个 O(n) 的分类/setext pass。

**验证**：
- 新增平价测试 `plan_document_equals_classify_then_plan_rows`：对同一文档遍历每个光标行，
  断言 `plan_document == plan_rows(classify_document(...))`（分类缓存喂回的 rows 与全新全量 plan 一致）——正确性成立（P1 缓存不改变任何渲染结果）。
- 全量 `cargo test --lib` 769 通过；`markdown_live` 190 通过；无编译告警。
- 正向效果：大 md 文档**仅移动光标/选区**（不编辑）时，每次 plan 从 ~5 个 O(n) pass 减到 ~3 个
  （省 classify + setext 两个 pass）。

**说明**：这是 P1 的中间优化（省文本无关的两个 O(n) pass）。完整增量 reveal（光标相关的
reveal 集合 + 逐行 plan 也跳过）仍需重新设计 reveal 表示，留作后续。

---

## P1 续：引用定义缓存（refdefs，实装并测试）—— 完成 P1 的「纯文本」部分

`plan_document` 的光标相关 O(n) pass 构成已确认：
- **纯文本相关（可缓存）**：classify + setext 反向 + `collect_refdefs` 全文扫描。
- **光标相关（需增量 reveal，见下）**：run-expansion + 逐行 `plan_with_refs`。

本轮把第三个纯文本 O(n) pass —— 引用定义扫描 —— 也纳入缓存：

- `reveal.rs` 新增 `refdefs_for_lines(lines, blocks)`（块级跳过规则集中为
  `is_refdef_skipped`）与 `plan_rows_with_refs(lines, classified, refs, reveal)`
  变体；`plan_rows`/`plan_document` 行为不变（内部调 `refdefs_for_lines`）。
- `view.rs::planned_rows` 新增 `planned_refs`（`Rc<HashMap>`）+ version：仅文本
  变更重算 refs，纯光标移动与分类一起复用。
- 平价测试扩展：`plan_rows_with_refs(classify_document + 缓存 refs)` 与
  `plan_document` 逐行一致。

**效果**：纯光标/选区移动时，plan 从 ~5 个 O(n) pass 减到 ~2 个（分类 2 个 +
refs 全扫已缓存，只余 run-expansion 与逐行 plan）——约 **60%** 的移动成本下降。

验证：`cargo test --lib` 769 通过，无告警；`markdown_live` 190 通过。

### 仍留（结构性重写，建议单独决策）

- **P1 余下**：增量 reveal（run-expansion 与逐行 plan 都从「整篇重算」改为
  「仅光标邻域重算 + 复用缓存行的 reveal 态」）。需把 `planned` 改成逐行可失效、
  与 `planned_classified` 并行维护的缓存，并引入 per-line reveal 指纹 —— 中等偏大且
  触及渲染正确性，不宜尾段一次落地。
- **M1 完整**：把「有界整文档快照撤销」改为「变更增量撤销」—— 需要将模型底层
  `Vec<String>` 换成可结构共享的数据结构（持久向量 / 行 rope），并重写 `push_undo/
  undo/redo` 与打字合并语义。这是对撤销子系统的大改，风险最高；当前「有界快照」
  已把所有内存压到硬上限（200 × 文档），对常见文档已足够。

**建议**：这两项（尤其 M1 的持久化底层改动）拆成独立任务、在建基线后再小步做；
不在本轮一次性强行合入，以免破坏既有的 50+ 撤销/合并测试或渲染平价。

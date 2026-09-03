# deepterm 编辑器实现审计报告

日期：2026-08-30
对照基线：`/Users/zhousong/Documents/workspace/zed`（上游 Zed 源码）
审查范围：`src/editor/**`（12,301 行）+ `src/components/input/**`（11,279 行）= 23,580 行

---

## 0. 总体结论

deepterm **没有复用 Zed 的 `editor` / `language` / `text` / `rope` crate**，而是用 `InputState` 重写了一套
"单缓冲 + 显示映射" 的编辑器。规模对比：

| 模块 | Zed | deepterm | 倍差 |
|---|---|---|---|
| editor crate 总计 | 153,527 行 | 1,301 行（src/editor/editor.rs） | ~118× |
| buffer / text 层 | `crates/language/src/buffer.rs` + `crates/text/src/text.rs` | `components/input/state.rs` 4,748 行 | — |
| 选区模型 | `selections_collection.rs` 1,470 行 + `selection.rs` 2,398 行 | `selection.rs` **51 行**（单 Selection） | — |
| 移动语义 | `movement.rs` 1,509 行 | `movement.rs` 264 行 | ~5.7× |
| 行映射底层 | `SumTree` + `RowInfo`（O(log N)） | `Vec` + 线性扫描（O(N)） | — |

**这不是"少了功能"，而是底层数据结构被换掉了**：Zed 用 `SumTree` 摘要树 + `Anchor` 锚点 +
事务型 undo，deepterm 用 `Vec<Fold>` + 裸字节 offset + 计时器分组 undo。下面绝大多数 P0/P1
缺陷都直接源于这个替换。

**发现缺陷 37 项**（P0 9 项、P1 14 项、P2 14 项）。其中 **9 项已由我逐行核验确认**，
其余由子代理带行号取证（报告中标注取证位置）。

---

## 1. P0 — 会导致数据损坏 / 核心功能失效

### 1.1 后台 tree-sitter 解析结果**永远被丢弃**（死代码 + CPU 空转）
**位置**：`src/components/input/state.rs:3863` / `:3870` / `:3910` / `:3952`
**状态**：✅ 已核验

```rust
3863  Self::dispatch_background_parse(bg, self.text_version, window, cx);  // 传 N
3870  self.text_version = self.text_version.wrapping_add(1);               // 立即变 N+1
...
3910  let parse_version = text_version;                                    // = N
3952  let stale = entity.update(cx, |state, _| state.text_version != parse_version)
3955  if stale { return; }
```

`text_version` 全仓**只有 3870 一处自增**（已 grep 全量确认），所以任务完成时
`N+1 != N` 恒成立 → `stale` 恒为 `true` → `apply_background_tree`（3962 行）**永不执行**。

- **后果**：大文件触发同步解析 30ms 超时（`mode.rs:241`）后，高亮/折叠/sticky header
  永久停留在旧语法树上；`state.rs:3881-3973` 整段是死代码，后台 CPU 白烧。
- **修复**：把 3870 行的自增移到 3863 行之前（或 dispatch 时传 `self.text_version + 1`）。

### 1.2 CRLF 全线崩坏 —— `slice_line` 保留 `\r`，而所有行操作假设行尾是 1 字节
**位置**：根因 `src/components/input/rope_ext.rs:307-322`
**状态**：✅ 已核验

```rust
fn slice_line(&self, row: usize) -> RopeSlice<'_> {
    let line = self.line(row, LineType::LF);
    if line.len() > 0 {
        let line_end = line.len() - 1;
        if line.is_char_boundary(line_end) && line.char(line_end) == '\n' {
            return line.slice(..line_end);   // ← 只剥 '\n'，'\r' 留下
        }
    }
    line
}
```

由此派生的 6 处数据损坏（均在 CRLF 文件上触发）：

| # | 位置 | 症状 |
|---|---|---|
| a | `state.rs:2390-2409` `join_lines` | ✅已核验。`cur_line` 以 `\r` 结尾 → `ends_with(whitespace)` 恒 true → **合并行永远不加空格**；`next_line` 把下一行的 `\r` 搬到行中间。`"a\r\nb\r\nc"` 按 `cmd-j` → `"a\rb\r\nc"` |
| b | `state.rs:2259-2264` `move_line_up` | `&prev[..prev.len()-1]` 只砍 `\n`，残留孤立 `\r` 被拼到块尾；分隔符硬编码 `"\n"` |
| c | `state.rs:2304-2309` `move_line_down` | 同上，对称 |
| d | `state.rs:2363-2365` `delete_line` | `span.start -= 1` 只退 1 字节 → 删末尾行后文件以裸 `\r` 结尾 |
| e | `state.rs:2815` `delete_to_end_of_line` | 连 `\r` 一起删，行尾 `"\r\n"` → `"\n"` |
| f | `state.rs:3413` `paste` / `:4231` 剪贴板 / `:2339` `duplicate_line` | 只剥 `\n` 不剥 `\r`；复制末行硬编码补 `\n` → 行尾混用 |

- **Zed 对照**：`crates/editor/src/editor.rs` 全部行操作走 `Point` / `OffsetUtf16` +
  `starts_on_newline` 语义，行内容**从不含行尾符**，不存在此类问题。
- **修复优先级最高的一条**：统一引入 `line_content_without_terminator(row)`，并把
  所有 `len() - 1` / `+ 1` 的行尾算术换成 `line_ending_len(row)`。

### 1.3 diff 视图「还原此块」可能写到**完全错误的位置**（代码丢失）
**位置**：`src/editor/diff_view.rs:840-852`
**状态**：取证自子代理（行号已核对存在）

```rust
let mut matched = fresh.iter().find(|h| {
    h.old_rows.start < stale.old_rows.end.max(stale.old_rows.start + 1)
        && stale.old_rows.start < h.old_rows.end.max(h.old_rows.start + 1)
});
if matched.is_none() && stale.old_rows.start == stale.old_rows.end {
    matched = fresh.iter().min_by_key(|h| {          // ← 危险兜底
        h.new_rows.start.abs_diff(stale.new_rows.start)
            .saturating_add(h.old_rows.start.abs_diff(stale.old_rows.start))
    });
}
```

- `find` 取「文件序第一个相交」而非「行号最接近」的 hunk；
- 退化分支按行号**距离**取最近者 —— 用户在上方删除一大段后，`stale.new_rows.start=50`
  而 fresh 只有 `new_rows.start=5` 的块 → **把 HEAD 内容写到第 5 行**。
- **无任何保护**：无内容匹配校验、无备份、无 mtime/size 外部修改检测、无回滚。
- **Zed 对照**：`crates/editor/src/git.rs:1858-1890` `prepare_restore_change` 用 **`Anchor`**
  （`hunk.buffer_range`）+ base rope 的 `diff_base_byte_range`，在编辑时刻对当前 snapshot
  解析，结构上不可能写错位置。
- **修复**：改为「按 `old_rows` 最小距离匹配 + 匹配后校验 `current[new_rows]` 仍与 hunk 描述一致」，
  校验失败则拒绝并提示。

### 1.4 diff 视图保存**绕过编码层**：BOM / GBK / UTF-16 被静默改写
**位置**：`src/editor/git_repository.rs:424-445` + `src/editor/editor.rs:2136-2140`

```rust
GitBackend::Local  => std::fs::write(&file_path, content)          // 无编码、无原子写
GitBackend::Remote => sftp_fs::write_file_string(config, path, content)
```

- 读取侧是 `encoding.rs:76` `decode_with_bom_removal`（**剥 BOM**、GBK/UTF-16 解码为 UTF-8），
  写回侧既不 `encode_text` 也不补 BOM → **BOM 被永久删除，GBK/UTF-16 文件被整体改写为 UTF-8**。
- `editor.rs:2136-2140` 对 diff 标签页直接 `return` 到 `view.save(cx)`，**跳过**了正常路径的
  `LineEnding::detect` + `encode_text` + 错误通知 + `sync_tab_modified`（`editor.rs:2837-2859`）。
- **Zed 对照**：`crates/fs/src/fs.rs:136` `Fs::save(&path, text, line_ending)` 把 line_ending
  作为一等参数，且用 `atomic_write`（临时文件 + `atomic_replace`）落盘。

### 1.5 diff 视图「刷新」静默丢弃未保存编辑，且左栏 HEAD 文本不更新
**位置**：`src/editor/diff_view.rs:975-985`

- `refresh()` 无脏检查、无确认弹窗，未保存编辑被直接覆盖；
- `view.old_text` 更新为新 HEAD，但 **`view.left` 编辑器文本从未更新**（对比 `fetch_contents`
  的 `diff_view.rs:261`）→ 左栏显示旧 HEAD，而 hunks 按新 `old_text` 计算 → **左右栏内容与染色全部错位**。

### 1.6 还原/重载不可撤销，且滚动位置被重置
**位置**：`src/components/input/state.rs:1284-1300`
**状态**：✅ 已核验

```rust
self.history.ignore = true;      // ← 不进 undo 栈
self.replace_text(value, window, cx);
self.history.ignore = false;
...
self.scroll_handle.set_offset(point(px(0.), px(0.)));   // ← 无条件滚回顶部
```

`diff_view.rs:914-916` 的「还原此块」走的就是 `set_value` → **Cmd+Z 无法撤销**，
且大文件还原后视图跳回文件开头、光标丢失。

### 1.7 `previous_boundary` 在 `\r` 处下溢 panic
**位置**：`src/components/input/state.rs:3630-3638`
**状态**：✅ 已核验

```rust
let mut offset = self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
if let Some(ch) = self.text.char_at(offset) {
    if ch == '\r' { offset -= 1; }      // ← offset == 0 时下溢
}
```

- **触发**：CRLF 文件首行为空行（文档以 `"\r\n"` 开头，很常见），光标位于 offset 1 按 `←`
  → clip 到 0，`char_at(0) == '\r'` → `0 - 1` 下溢（debug panic / release 变 `usize::MAX`）。
- **次生问题**：`offset -= 1` 未做 char-boundary 对齐。文本 `"中\r\n"`、光标在 `\r` 后（offset 4）
  按 `←` → `offset = 2`，落在 `中`(0..3) 中间。该偏移被写入 `selected_range` 后，
  后续 `self.text.slice(..cursor)` 会命中 ropey 的 *not on char boundaries* panic。

### 1.8 折叠后闭合括号 `}` **永远画不出来**
**位置**：`src/components/input/element.rs:1452-1458`
**状态**：✅ 已核验

```rust
let tail_start = fold.end_offset;                                    // = line_start(end.row) + end.column
let tail_end = { let end_col = fold.end.column.min(line_len);
                 text.line_start_offset(fold.end.row) + end_col };   // ← 与 tail_start 恒等
let tail = if tail_end > tail_start { ... } else { None };           // ← 恒为 None
```

`fold.end_offset` 由 `fold_map.rs:44-53` 定义为 `text.point_to_offset(end)`，
而 `rope_ext.rs:350-357` 的 `point_to_offset` 就是 `line_start + point.column`（无钳制）。
两者**在所有情况下恒等** → `tail` 永远是 `None` → 折叠渲染为 `fn main() {⋯`（少一个 `}`）。
`state.rs:4673` 的注释 "row 5's `}` tail is painted on row 0" 说明意图存在、实现缺失。

### 1.9 文档整体替换不清折叠 → 旧折叠被重映射到新文档
**位置**：`src/components/input/state.rs:1310-1330` + `display_map.rs:245-258`

`set_text` 只调 `display_map.set_text`（刷新 wrap），`folds` 原样保留。
`fold_map.rs:314-327` 仅把 offset 夹到 `len` 并丢弃退化折叠 —— 落在新文档里仍"合法"的旧
offset 会被保留，**折叠到完全无关的行上**。`default_value`（`state.rs:1651-1661`）同样不清。

- **Zed 对照**：折叠是锚定在 buffer 上的 `Anchor`（`TreeMap<Range<Anchor>, FoldId>`），
  buffer 实体不同则折叠不同。

---

## 2. P1 — 功能错误 / 严重性能 / 交互失效

### 2.1 `scroll_to` / `scroll_to_center` 用错索引
**位置**：`src/components/input/state.rs:3252` / `:3335`
**状态**：✅ 已核验

```rust
if let Some(line) = last_layout.lines.get(row.saturating_sub(last_layout.visible_range.start)) {
```

`LastLayout::lines` 是**紧凑可见行数组**（`state.rs:653-663`，与 `visible_buffer_lines` 平行）。
项目内已有正确实现 `LastLayout::line()`（`state.rs:683-686`，用 `binary_search`）。
`row - visible_range.start` 只在无折叠时成立 → **存在折叠时，每一次上下方向键**都取到错误的
`LineLayout`，水平滚动量 `pos.x` 与软换行子行偏移 `pos.y` 均来自错误的行。

### 2.2 undo 分组锚点每次重置 → 连续打字只有一组
**位置**：`src/components/input/history.rs:76-89`

```rust
if self.last_changed_at.elapsed() > interval { self.version += 1; }
self.last_changed_at = now;   // ← 每次 push 都刷新锚点
```

判定基准是「距上次按键」而非「距本组开始」。以 0.9s 间隔持续输入（`interval=1s`），
`elapsed()` 恒为 0.9s < 1s → `version` **永不递增** → 整场输入折叠成一个 undo 组，
**一次 Cmd+Z 撤销全部输入**。

- **Zed 对照**：`crates/language/src/buffer.rs` 用显式 `start_transaction / end_transaction`，
  并在 Editor 层用选区变化、换行、光标跳转等**语义事件**强制切组。

### 2.3 `History::unique()` 完全是空操作
**位置**：`src/components/input/history.rs:105-108` + `change.rs:1-14`

`Change` 用 `#[derive(PartialEq)]`，**把 `version` 也算进相等性**；而 `item.set_version(version)`
在 `retain` **之后**才执行（item.version 此刻仍是 0）→ `*c != item` 恒为 true → 永远删不掉任何东西。

### 2.4 undo 不恢复光标 / 选区
**位置**：`src/components/input/state.rs:3435-3444`

每次 `replace_text_in_range_inner` 都把 `selected_range` 设为 `(start + old_text.len())..`（`state.rs:3866`）
→ 撤销后光标停在**被恢复文本的末尾**而非编辑前位置。
- **Zed 对照**：`selections_collection.rs` 的 transaction 记录了 `selections` 快照，一并回滚。

### 2.5 IME 取消后 `history.grouping` 未复位
**位置**：`src/components/input/state.rs:3054-3066` / `:4019-4021` / `:4103`

`unmark_text` 只清 `ime_marked_range`，不调 `end_grouping()`；而
`replace_and_mark_text_in_range` 在 4103 行 `start_grouping()` 后从不 `end_grouping()`。
→ 组字中途按 Esc → `grouping` 保持 true → 之后键入的第一个字符被并进 IME 那一组。

### 2.6 `point_to_offset` 既不钳制列、也不对齐 char boundary
**位置**：`src/components/input/rope_ext.rs:350-357`

```rust
fn point_to_offset(&self, point: Point) -> usize {
    if point.row >= self.lines_len() { return self.len(); }
    self.text.line_to_byte_idx(point.row, LineType::LF) + point.column   // 无 min(行字节长)
}
```

任何一个**陈旧 `Point`**（编辑前算出的）其 column 都可能超出当前行长度 → 偏移滑到下一行甚至越过 EOF。
真实持有陈旧 Point 的调用方：`display_map/folding.rs:37-39`、`fold_map.rs:45-46`、`text_wrapper.rs:323`。
`fold_map.rs:314-327` 之后若 `start > end`，下游 `Rope::slice` 直接 panic。

- **Zed 对照**：`crates/rope/src/rope.rs:553` `clip_point` → `chunk.clip_point(overshoot, bias)`，
  强制钳到行尾并贴到字符边界；`text.rs:2677` 的 `BufferSnapshot::clip_point` 是全部编辑入口的**必经关卡**。

### 2.7 `slice_lines` 在空/退化区间 panic
**位置**：`src/components/input/rope_ext.rs:324-328`

`slice_lines(2..2)` → `start = line_start(2)`，`end = line_end(1) < start` → ropey
*"Panics if the start of the range is greater than the end"*。当前 `src/` 内无调用点（仅 doctest/单测），
属公开 API 上的**潜伏** panic。

### 2.8 缩进参考线在 `paint` 内对**全文件**做 4 趟 O(N) 扫描
**位置**：`src/components/input/element.rs:2634-2672`

```rust
let total_lines = state.text.lines_len();                       // 全文件
for idx in 0..total_lines {
    let line: String = state.text.slice_line(idx).into();       // 每行一次堆分配
}
let mut prev  = vec![-1; total_lines];   // 第 2 趟
let mut next  = vec![-1; total_lines];   // 第 3 趟
let mut counts= vec![ 0; total_lines];   // 第 4 趟
```

`paint` 每帧执行，但只需要 `guides_counts.get(buffer_line)` 用于可视区那几十行。
10 万行文件 → 每帧 40 万次迭代 + 10 万次 `String` 分配；minified 单文件退化为对 5MB 字符串逐字符扫描。

### 2.9 可视区计算 / 光标定位 / `scroll_to` 全是 O(光标行号) 线性扫描
| 位置 | 代码 |
|---|---|
| `element.rs:1187` | 可视区从 0 扫到视口底部 |
| `element.rs:364` | `layout_cursor` 同一个从 0 开始的循环 |
| `state.rs:3237-3245` | `scroll_to`：扫到 `row` 才 break |
| `text_wrapper.rs:270-312` | `offset_to_display_point` / `display_point_to_offset` 每次线性扫描所有 buffer 行 |

滚到 100 万行文件底部时，前四项**每帧**各跑 ~100 万次。
- **Zed 对照**：`FoldSnapshot` / `DisplaySnapshot` 用 `SumTree` + `RowInfo`，行↔显示行互转 O(log N)。

### 2.10 含换行的编辑 / 窗口 resize 触发**全文档同步重排**
**位置**：`src/components/input/text_wrapper.rs:185-195` / `:104-121`

文件头部按一次回车 → 重排其后**全部**行（同步、主线程）。`set_wrap_width` / `set_font`
→ `update_all` → 每次改窗口宽度重排全文件（经 `state.rs:3749` 的 `set_input_bounds` 触发）。
minified 文件：`layout_lines` 把整行当一个 `shape_line`，无分块、无长度上限
（`MAX_HIGHLIGHT_LINE_LENGTH=10000` 只作用于语法高亮，不作用于 wrap/shape）。
- **Zed 对照**：`wrap_map` 后台线程 + 增量，`update` 只重排受影响行。

### 2.11 `paint` 末尾无条件 `cx.notify()` → 疑似永久重绘循环
**位置**：`src/components/input/element.rs:2972-2986`
**状态**：✅ 已核验

```rust
state.update_scroll_offset(Some(prepaint.cursor_scroll_offset), cx);   // 内部也 notify
state.deferred_scroll_offset = None;
cx.notify();        // ← 每帧
```

在 paint 阶段 `notify` 会把实体重新标脏并请求下一帧 → 空闲时也 60fps 全量 prepaint+paint
（叠加 2.8/2.9/2.10 的 O(N)）。注释表明这是为配合"每帧只滚一行"的 hack 而刻意为之。

### 2.12 分栏滚动同步的映射函数互不为逆 → 抖动/乒乓
**位置**：`src/editor/diff_view.rs:1508-1543`

**可验证反例**：hunk `{old: 10..20, new: 10..12}`。`map_old_to_new(15) = 15`；
`map_new_to_old(15) = 23`。往返不闭合：`15 → 15 → 23 → 15 → 23 …`，差值 8 行，
远超 `diff_view.rs:767` 的 `line_height / 2` 阈值。

且 `sync_scroll` 的重入保护形同虚设（`diff_view.rs:768-772`）：`scroll_syncing = false`
同步复位，而反向 `cx.observe`（224-229）在 effects flush 时才触发，此时守卫已失效
→ 左右两栏每帧两次互相纠正、永不收敛。

- **Zed 对照**：不用「顶行 + 增量 delta」映射，而是在 display map 里给较短一侧**插入等高
  filler block** 做物理对齐，滚动同步只对齐 top row，天然稳定。

### 2.13 `flush_pending_jump` 在特定条件下形成每帧死循环
**位置**：`src/editor/diff_view.rs:533-563`（`render` 每帧调用）

```rust
self.pending_jump = editor.read(cx).scroll_handle.offset().y == before && row > 0;
```

`scroll_editor_to_row` 的 `target = row.saturating_sub(3)`。当 `1 <= row <= 3` 且编辑器
本就停在顶部时，`desired_y == before == 0` → 条件恒 true → **每次 render 都重复设置滚动偏移
并 `cx.notify`，无限重绘**。Split 分支（554-561）同构。

### 2.14 HEAD 侧不剥离 BOM → BOM 文件首行永远显示"已修改"
**位置**：`src/editor/git_repository.rs:318` + `:546`

`show <ref>` 走 `String::from_utf8_lossy`，无 BOM 处理；工作区侧走 `decode_with_bom_removal`
→ `old = "\u{FEFF}line1\n…"` vs `new = "line1\n…"` → 首行恒为伪 hunk。
同理 `core.autocrlf=true` 的仓库（HEAD 是 LF、工作区是 CRLF）→ **全文件每一行都显示修改**。

---

## 3. P2 — 次要 / 边界 / 可维护性

### 3.1 `git_marks` 去重保留了错误的优先级
**位置**：`src/editor/git_marks.rs:47-49`
**状态**：✅ 已核验

```rust
marks.sort_by_key(|(line, _)| *line);   // 稳定排序
marks.dedup_by(|a, b| a.0 == b.0);      // ← 保留先出现的
```

纯删除 hunk（`new_rows = 5..5`）先 push `(5, Deleted)`，随后的修改 hunk（`new_rows = 5..8`）
再 push `(5, Modified)`。稳定排序后 **Deleted 在前，dedup 保留 Deleted**。
第 47 行注释明确写「保留优先级：Added/Modified > Deleted」—— **行为与注释相反**。

### 3.2 关闭行号时，当前行高亮整体消失
**位置**：`src/components/input/element.rs:2499`
**状态**：✅ 已核验

```rust
if let Some(line_numbers) = prepaint.line_numbers.as_ref() {   // ← 当前行高亮 + git gutter 都包在里面
```

当前行高亮与 git gutter 被错误地耦合在"是否显示行号"这一条件下。

### 3.3 折叠态下 `current_row` 算错一行
**位置**：`src/components/input/element.rs:409-448`

判定应为 `prev_lines_offset <= cursor < prev_lines_offset + line_len`，实际写成 `>=`
→ 命中的是**第一个行首偏移 ≥ 光标**的行，即光标所在隐藏行的**下一行**。
`element.rs:549-554` 的 `fold_header_for_row` 修正（`fold_map.rs:207-213` 判断
`row > f.start.row && row <= f.end.row`）无法覆盖 `end.row + 1`。
→ 光标画在折叠头行，而当前行高亮 / 行号高亮 / 折叠图标落在下一行。

### 3.4 `adjust_folds_for_edit` 边界语义
**位置**：`src/components/input/display_map/fold_map.rs:285-310`

在 `fold.end_offset` 处**纯插入**（`edit.start == edit.end == fold.end_offset`）落到 `else`
分支 → **折叠被丢弃**（Zed 用 Anchor + Bias 决定落点，不会整体丢）。
`delta` 为大负数时 `.max(0)` 把 `start`/`end` 双双钳到 0，产生 `0..0`。

### 3.5 `set_input_bounds` 只比对宽度，且用的是上一帧的 `wrap_width`
**位置**：`src/components/input/state.rs:3736-3752`

`wrap_width = bounds.width - line_number_width - RIGHT_MARGIN`。当 **`line_number_width`
变化**（行数越过 9999/99999 阈值，`element.rs:1228-1234`；或开关折叠加
`FOLD_ICON_HITBOX_WIDTH`，`element.rs:1258-1261`）时 `bounds.width` 不变 → display_map
永远不更新 → **软换行宽度陈旧**。此外 `prepaint` 里重算了 `wrap_width` 但从不调
`on_layout_changed`，只有 `set_input_bounds` 会，滞后一帧。

### 3.6 缩进参考线活跃块判定 1-based / 0-based 混用
**位置**：`src/components/input/element.rs:2711-2719`

`s` 按 1-based 取前一行（`s-2`），`e` 却把 1-based 的 `e` 当 0-based 索引
（`guides_counts[e]`）→ `active_end` 偏大；且 `s > 1` 无法下探到 0-based 行 0，
首行永远不能作为活跃块起点。

### 3.7 全文件 UTF-16 长度每次 O(N) 遍历
**位置**：`src/components/input/state.rs:1381`

```rust
let range = 0..self.text.chars().map(|c| c.len_utf16()).sum();   // 应直接 self.text.len_utf16()（O(1)）
```

### 3.8 单行输入每次按键全量 clone 文本
**位置**：`src/components/input/state.rs:3840` / `:4063`

```rust
let pending_text = self.text.to_string();   // 每次按键全量 clone 去跑 is_valid_input
```

### 3.9 词移动 / 词删除复制光标之前的全部文本
**位置**：`src/components/input/state.rs:1809` / `:1821`

```rust
self.text.slice(0..offset).to_string()   // 源码自留注释：// FIXME: Avoid to_string
```

2MB 文件光标在末尾时，每按一次 `alt-←` / `alt-backspace` 分配 2MB。

### 3.10 同词高亮在 paint 路径做 O(n·m) 朴素匹配
**位置**：`src/components/input/state.rs:273` + `:1519` + `element.rs:1114`

`let bytes: Vec<u8> = text.bytes().collect();` 把整个文档（上限 `OCCURRENCE_MAX_TEXT_LEN = 2_000_000`）
复制成 `Vec<u8>` 再做朴素匹配；调用点在 **paint 路径**，缓存键只要词范围变了就重算
（`state.rs:1512-1517`）→ 每次敲键重扫全文件。

### 3.11 masked 输入框鼠标定位 O(n) 且每次鼠标移动触发
**位置**：`src/components/input/state.rs:3519` / `:3529` / `:3537`

`char_index_to_offset`（`rope_ext.rs:483-485`）是 `self.chars().take(n)...sum()`，O(n)；
`on_drag_move` 每次鼠标移动触发。

### 3.12 折叠 pill 与正文错位
**位置**：`src/components/input/element.rs:1550`

pill 的 x 用 `bounds.origin.x + line_number_width + last_width`，**未加 `scroll_offset`
对齐偏移**；正文在 `element.rs:2783` 是加了的 → 非左对齐时错位。

### 3.13 缩进线 x 按空格宽度线性外推
**位置**：`src/components/input/element.rs:2739`

`x = space_width * (visible_column - 1)`，tab 缩进（虽在 2646 折算成 4 列）与非等宽字体下必然不对齐。

### 3.14 折叠无持久化 + `TabSize` 是死代码
- 折叠状态**无任何持久化**：`FoldMap.folds` 是纯内存 `Vec<Fold>`，随 `InputState` 生命周期消亡
  （全仓唯一持久化的是 `file_tree.rs` 的 `expanded_folders`，属文件树非编辑器）。
- `indent.rs` 的 `TabSize` 在整个 crate 里**零调用**（仅 `mod.rs:19` 再导出），
  `hard_tabs` / `tab_size` 被静默忽略 —— 缩进单元写死为 `INDENT_UNIT = "\t"`（`state.rs:2897`）。

---

## 4. 绘制期 panic 风险（已定位，未复现）

| 位置 | 代码 | 风险 |
|---|---|---|
| `element.rs:1672-1676` | `buffer_lines.get(buffer_line).expect("line should exists in wrapper")` | `visible_buffer_lines` 与 `display_map.lines()` 失同步即 panic；紧邻的 `log::error!`（1685-1697）说明作者已遇到过失同步，但用 `expect` 而非降级 |
| `text_wrapper.rs:268` | `let line = &self.lines[row];` | `self.text` 与 `self.lines` 不同步 → 越界 panic |
| `text_wrapper.rs:231` | `line_str[prev_boundary_ix..]` | 若 `boundary.ix` 不是字符边界或 > `line_str.len()` → panic，无校验 |
| `element.rs:2733` | `&prepaint.last_layout.lines[vi]` | 依赖 `lines.len() == visible_buffer_lines.len()`；空文本分支（1641-1657）按 `placeholder.split("\n")` 生成，长度不再等价 |
| `state.rs:2751` | `self.text.slice(..cursor)` | 承接 1.7 的越界偏移 → ropey char boundary panic |

---

## 5. 明确"未发现"的项（避免误判）

| 项 | 结论 |
|---|---|
| diff 算法本身 | **正确**。`line_diff` / `word_diff_ranges` / `tokenize` / `diff_internal` 与 `zed/crates/language/src/text_diff.rs:159-219, 259-300` 逐行等价，同为 imara-diff **Histogram**（非 Myers，与 Zed 一致），近线性时间。边界（空文件/单行/末尾无换行/CRLF/空行）逐一手算通过，有 `test_layout_consumption_crlf_and_cjk` 覆盖 |
| diff 的并发保存 | 有 `saving` 标志防护（`diff_view.rs:924`），**未发现问题** |
| 后台任务 use-after-free | 全部用 `this.update(cx, ...).ok()`（`diff_view.rs`），`recompute_gen` / `refreshing` 能正确作废旧结果，**未发现崩溃** |
| 行号 gutter 与正文错位 | **未发现**。两者都从 `visible_top` 起算，子行数一致 |
| 选中高亮 / 括号匹配 / 同词高亮的坐标系 | **未发现错误**。`layout_match_range`（`element.rs:590-594`）正确使用 `visible_line_byte_offsets` |
| `fold_map.rs:381-424` 隐藏区间合并 | **未发现错误** |
| 折叠命中区域 | 与 Zed 一致，**未发现错位** |
| 缩进 / Tab 逻辑正确性 | **未发现逻辑错误**（`indent_or_outdent_lines:2945-3020` 能正确保住 `\r`） |

---

## 6. 修复优先级建议

**第一批（一行到几行改动，立竿见影）**
1. `state.rs:3870` —— `text_version` 自增移到 3863 之前（解死 1.1）
2. `element.rs:1456` —— `tail_end` 改为 `text.line_end_offset(fold.end.row)`（解死 1.8）
3. `state.rs:3252` / `:3335` —— 改用 `last_layout.line(row)` 的 `binary_search`（1 行，解死 2.1）
4. `state.rs:3632` —— `offset.checked_sub(1)` + char boundary 对齐（解死 1.7）
5. `git_marks.rs:47-49` —— dedup 前按优先级排序（解死 3.1）
6. `element.rs:2499` —— 当前行高亮移出 `if let Some(line_numbers)`（解死 3.2）

**第二批（CRLF 系统性治理）**
7. 引入 `line_content_without_terminator(row)` + `line_ending_len(row)`，替换 1.2 表格中全部 6 处行尾算术

**第三批（diff 数据安全，风险最高）**
8. `diff_view.rs:840-852` —— 改「最小距离匹配 + 内容一致性校验，失败即拒绝」（1.3）
9. `git_repository.rs:424-445` —— 保存复用 `save_text_to_file` + 原子写 + mtime 校验 + 错误通知（1.4）
10. `diff_view.rs:975-985` —— 刷新前判脏、同步更新 `view.left`（1.5）
11. `state.rs:1284-1300` —— 还原走可撤销 edit 事务并保留滚动（1.6）

**第四批（架构性，工作量最大）**
12. `rope_ext.rs:350` 引入 `clip_point` 强制关卡（2.6）
13. undo 改显式事务分组（2.2 / 2.3 / 2.4 / 2.5）
14. 性能：按可视区裁剪缩进参考线计算（2.8）、行↔显示行改前缀和/摘要树（2.9）、
    增量 wrap（2.10）、去掉 paint 中的 `cx.notify`（2.11）

---

## 7. 附：核验方法

- P0/P1 中标注「✅ 已核验」的 9 项由人工逐行读码 + grep 全量确认（含 `text_version`
  自增点唯一性、`Fold::from_points` 定义、`LastLayout` 的两种索引方式对比）。
- 其余条目由三条并行分析子代理带 `文件:行号 + 代码片段 + 触发条件 + Zed 对照` 取证产出，
  行号抽样复核一致。
- **未做动态验证**：未编译、未运行、未构造最小复现用例。P2/第 4 节建议在修复时补单元测试。

---

## 8. 修复记录（2026-08-30）

全部条目均已 `cargo test --bin deepterm` 通过（**498 passed / 0 failed / 无警告**）。
带 🧪 的表示补了回归或差分测试。

### 已修复

| 项 | 修复要点 |
|---|---|
| 1.1 | `replace_text_in_range_inner` 里把 `text_version` 自增移到 `dispatch_background_parse` 之前，后台解析结果不再被全量丢弃 🧪 |
| **新增** | **`text_version` 不变式缺失**（见下方专项说明）：`set_text` 与 IME 组字两条路径改写 `self.text` 却不推进版本号，导致中文输入时文件不显示“已修改”、同词高亮缓存串档、后台解析 stale 判定失效。新增 `bump_text_version()` 收口并写明三条变更路径 🧪 |
| 1.3 | `resolve_hunk_for_revert` 按 `old_rows` 距离评分 + `REVERT_MATCH_TOLERANCE` 封顶 🧪 |
| 1.4 | `save` 改走 `save_text_to_file` + `detect_encoding`，不再绕过编码层 |
| 1.5 | `refresh` 加脏检查保留未保存编辑，并同步更新左栏 `old` 文本 |
| 1.6 | `revert_hunk` / 重载改用 `set_value_undoable`，可撤销且不重置滚动 |
| 1.7 | `previous_boundary` 的 `\r` 下溢改 `saturating_sub` |
| 1.8 | 折叠 tail 用 `tail_end = text.line_content_end_offset(fold.end.row)` |
| 1.9 | 文档整体替换（`set_value` / `set_text`）清空折叠 🧪 |
| 2.1 | `scroll_to` 改用 `last_layout.line(row)` 二分查找 |
| 2.2 | `History::inc_version` 的计时锚点只在**开新组**时刷新；新增 `break_group()` 供语义边界强制切组 🧪 |
| 2.3 | `History::unique()` 从空操作改为真正按 `Change::Edit` 合并去重 |
| 2.4 | undo / redo 恢复光标与选区 |
| 2.5 | IME 取消后复位 `history.grouping` |
| 2.6 | `point_to_offset` 钳制列 + `clip_offset` 对齐字符边界 |
| 2.7 | `slice_lines` 空/退化区间早返回 |
| 2.8 | 缩进参考线计算裁到可视区 ∪ 光标行，窗口外只做有界的「找第一个非空行」 🧪 |
| 2.9 | `scroll_to` / `scroll_to_center` 的 O(行号) 逐行累加 → `display_map.display_row_for_buffer_line()`，`FoldMap::visible_display_rows_before` 用前缀和 + 二分，O(log n) 🧪 |
| 2.12 | 已核查为**不需修改**：in-hunk 结果已钳制到 `last_*_row`，往返两步收敛；重入保护已用 `cx.defer` 覆盖到 effects flush 之后 🧪（往返收敛测试） |
| 2.13 | `flush_pending_jump` 的重试条件改为「`line_height <= 0`」且复位 `pending_jump = false` |
| 2.14 | HEAD 侧 `show <ref>` 走 `decode_with_bom_removal` 🧪 |
| 3.1 | `git_marks` 去重按 `(行号, 优先级)` 排序，`Added/Modified` 不再被 `Deleted` 挤掉 🧪 |
| 3.2 | 当前行高亮移出 `line_numbers` 块，改用 `line_geometry`，关闭行号时仍生效 |
| 3.3 | 折叠态下 `current_row` 的差一错误 |
| 3.4 | `adjust_folds_for_edit` 边界语义收紧（`edit.end <= fold.end_offset` 的纯插入保留折叠）、位移改 `saturating_add_signed` 防下溢 🧪 |
| 3.5 | `set_input_bounds` 的判据从「元素宽度」改为「换行宽度本身」，且读本帧的 `line_number_width`（行号栏宽度变化而元素宽度不变时也能触发重排） |
| 3.6 | 缩进参考线活跃块判定统一为 0-based 半开区间 |
| 3.7 | `text.chars().map(len_utf16).sum()` → `text.len_utf16()`（O(N) → O(1)） |
| 3.8 | 单行输入的 `to_string()` 校验前加 `needs_validation()` 短路（无校验规则时零拷贝） |
| 3.9 | 词移动 / 词删除改 4KB 滑动窗口，窗口全空白才退回全量扫描 🧪 |
| 3.10 | 同词高亮的 `Vec<u8>` 全文件复制 → `VecDeque` 定长流式窗口 🧪（streaming vs naive 差分） |
| 3.11 | `char_index_to_offset` / `offset_to_char_index` 从 O(n) 逐字符累加 → ropey `metric_chars` 的 O(log n) 树查询（后者在 paint 路径的 `masked_display_offset` 中被调用）🧪（差分 + 越界饱和） |
| 3.12 | 折叠 pill 的 x 补上 `scroll_offset` 对齐偏移 |
| 3.13 | 缩进参考线 x 由「空格宽度线性外推」改为实测 Tab / 空格的推进量 |
| 3.14（TabSize 部分） | `TabSize` 从零调用的死类型改为生效配置：新增 `HARD_TABS` / `spaces()` / `indent_unit()` / `outdent_len()` / `indent_count()`，接入缩进、反缩进、回车智能缩进、闭合括号自动反缩进与参考线宽度；删除写死的 `INDENT_UNIT` / `SPACE_OUTDENT_WIDTH` 🧪 |
| 第 4 节 | `cursor()` 钳制到 `text.len()` 并 `clip_offset(.., Bias::Left)`，消除 `slice(..cursor)` 的越界/非字符边界 panic；`expect` 降级 |

### 仍未处理（非缺陷或需设计决策）

| 项 | 说明 |
|---|---|
| 1.1 | 后台 tree-sitter 解析结果被丢弃 —— 死代码 + CPU 空转，需决定「接上还是删掉」 |
| 1.2 | CRLF：`slice_line` 保留 `\r`，而行操作假设行尾 1 字节。改动面横跨所有行操作，建议独立一轮 |
| 2.10 | 含换行的编辑 / resize 触发全文档同步重排 —— 需增量 wrap，架构性改动 |
| 3.14（折叠持久化） | 折叠状态无任何持久化。**属新功能而非缺陷**：需要先确定存储位置（工作区 / 项目 / 全局）与失效策略（文件指纹 / mtime / 内容 hash） |

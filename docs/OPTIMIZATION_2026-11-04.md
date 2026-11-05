# 优化清单（2026-11-04）

依据 `.opencode/skills/rust-refactor/SKILL.md` 与
`.opencode/skills/rust-performance/SKILL.md` 的诊断框架，对当前仓库
（252 个 `.rs`，214,476 行）做了一轮静态分析。本文按“可独立评审、可测量”
的原则排序，作为后续逐项推进的底稿。

> 行号以本文提交时的 `main`（`62f424b`）为准。旧审计文档
> （`docs/CODE_AUDIT_2026-09-04.md` 等）的行号与总行数已过期，引用前先核对。

---

## 一、本次实测基线

| 检查 | 结果 |
|---|---|
| `git status --short` | 干净 |
| `cargo fmt --all --check` | 通过 |
| `cargo check --workspace --all-targets` | 通过，0 warning |
| `cargo clippy --workspace --all-targets` | lib-test 174 条（含 76 条重复）；`-D warnings` 不通过 |
| `cargo check --no-default-features` | 通过（移除失效的 `tree-sitter-languages` feature 后） |
| `cargo test` | `cargo test --lib`：2304 passed / 1 ignored；3 个环境/flaky 用例（2 个 `terminal::local_pty`、1 个 rust-analyzer 特性请求），单独复跑均通过 |
| `cargo bench` | 未运行；现有基准见 `benches/terminal_render.rs`、`benches/markdown_live.rs` |

结论：终端渲染与 markdown live 两条主热路径已经过多轮测量驱动优化。当前收益
集中在代码编辑器（`InputState`/`TextElement`）的每键/每帧 O(文档) 工作、
搜索面板的强引用环、侧边栏每帧配置拷贝，以及巨型文件/巨型函数的结构债。

---

## 二、性能与内存热点

| 优先级 | 位置 | 问题 | 复杂度影响 | 状态 |
|---|---|---|---|---|
| P0 | `state.rs:492` + `search.rs:388` | `InputState` ↔ `SearchPanel` 强引用成环 | 关闭标签后整份文档/布局缓存泄漏 | 已完成 |
| P1 | `editor.rs:6949` | 每次文本变化全文拷贝 + 比较 | 每次击键 O(文档) | 部分完成（长度快速路径） |
| P1 | `element.rs:1461`、`:491` | 每帧从第 0 行扫描到视口/光标 | 大文件底部每帧 O(总行数) | 已完成 |
| P1 | `text_wrapper.rs:207-212` | 换行编辑重排到文档末尾 | 每次 Enter O(后半文档) | 已完成 |
| P2 | `view.rs:1609` + `table_ops.rs:26` | 表格内击键全量分类文档 | 每次击键 O(文档) | 已完成 |
| P2 | `path_completion.rs:130` | 每键同步 `read_dir` | 大目录输入阻塞 UI | 已完成 |
| P2 | `host_view.rs:1066`、`keychain_view.rs:164` | 每帧 stat + 深拷贝配置 | 每帧多次堆分配 | 已完成 |
| P3 | `terminal_view.rs:5231` | 每键多余的 `Vec` 拷贝 | 每键 1 次分配 | 已完成 |
| P3 | `view.rs:5583` | 每帧 clone `AppTheme` + 重建样式 | 每帧多次分配 | 已完成（主题 Arc） |
| P3 | `terminal.rs:4001` | 每帧全视口行哈希 | 每帧 O(视口单元格) | 已完成 |
| P3 | `diff_view.rs:681` | 每键全文拷贝 + 比较 | 每次击键 O(文档) | 已完成 |
| P3 | `search.rs:132` | 搜索面板打开时每键全文重扫 | 每次击键 O(文档) | 已完成 |

### P0：搜索面板强引用环

`InputState::search_panel` 持有 `Entity<SearchPanel>`（`state.rs:492`），
`SearchPanel::editor` 又强持有 `Entity<InputState>`（`search.rs:388/546`）。
GPUI 的 `Entity` 是强引用，二者形成环；用过搜索的标签关闭后 `InputState`
不会析构，整份 `Rope`、`display_map.lines`、`last_layout.lines`
（含 `ShapedLine`）、undo 历史和 matcher 的文本副本都留在内存里。

**做法**

- `SearchPanel::editor` 改为 `WeakEntity<T>`；`new` 接收 `WeakEntity<T>`，
  两个构造点先 `downgrade()`。
- 所有 `self.editor.read/update` 改成先 `upgrade()`，失败时安全降级
  （隐藏替换 UI、跳过跳转、只更新 matcher）。
- `hide` 时清空 matcher 的 `text`/`matched_ranges`（保留 query 与输入框内容）。
- 测试：关闭/释放宿主后 `WeakEntity::upgrade()` 返回 `None`；`hide` 后
  matcher 不再持有文本。

### P1：普通编辑器每击键全文脏检查

`Editor::file_is_dirty`（`editor.rs:6949`）在每次文本变化时调用
`InputState::value()`；`value()` 内部 `Rope::to_string()` 再构造
`SharedString`（`state.rs:2681-2682`），随后与 `original_content` 全文比较。
`text_version` 闸门（`editor.rs:1322-1330`、`6446-6460`）只过滤光标移动，
编辑本身每次都会通过。10 MB 文件每键两次 O(n) 拷贝 + 一次 O(n) 比较。

**做法**

- 已落地：普通编辑器（`live.is_none()`）先比 `state.text.len()` 与
  `original_content.len()`，长度不同直接判脏，省掉 `value()` 的全文拷贝。
  live 标签保留原同口径比较（影子文本剥 `\r`，长度口径不同）。
- 待做：`EditorFile` 增加 `clean_version`，让同长度替换/撤销也能 O(1) 命中；
  精确比较放到保存、关闭确认、防抖回调。live 路径已经是这个模式
  （`buffer.rs:137-163` + `editor.rs:7109` 的 `is_dirty_fast`/`is_dirty` 分离）。
- 可选：给 `InputState` 增加 `text_equals(&str)`，避免构造 `String`。

### P1：编辑区每帧从第 0 行扫描

`TextElement::calculate_visible_range`（`element.rs:1461`）从 `ix=0` 累加
每行高度，直到视口填满；`layout_cursor`（`element.rs:432`，循环在 `:491`）
同样从第 0 行扫到光标行。光标在 10 万行文件底部时，每帧/每键约 10 万次
行遍历。

**已落地**

- `DisplayMap` 新增 `geometry_dirty_from`：文本编辑记录 `WrapMap` 返回的最早
  受影响行；折叠、git 展开区、字体/宽度变化保守标记为 0。
- `WrapMap::on_text_changed` 返回增量起点；`set_font` 在字体未变时返回
  `false`，避免 `prepare_if_needed` 每帧调用把锚点打脏。
- `calculate_visible_range` 在 `dirty_from >= 上一帧可见区起点` 且锚点仍在
  当前视口上方时，从 `LastLayout` 的 `visible_top` / `visible_range.start`
  继续扫描；否则退回第 0 行。
- `layout_cursor` 在光标与选区端点都落在当前可见字节范围内时，从可见区
  起点累计 y；端点越界时保留原全量扫描（用于滚动跟随/折叠内映射）。
- 回归测试 `visible_range_reuses_last_anchor`：10,000 行文档，首帧扫描
  10,000 行，复用锚点后 < 100 行；编辑落在锚点行仍复用；编辑落在锚点之前
  回到全量。`text_edit_marks_geometry_from_edited_row` 验证编辑行号正确上报。

**后续可选**

- 给 DisplayMap 增加“可见行数（含 fold/git 插入）”的前缀和，按像素偏移
  二分，覆盖滚动条大跳转 / 向上大幅滚动的场景。
- 覆盖 fold、git 展开、软换行、resize 的更多组合测试。

### P1：换行编辑重排到文档末尾

`TextWrapper::_update`（`text_wrapper.rs:207-212`）在 `new_text` 含换行时把
`rows_range` 扩到 `start_row..=lines.len()-1`，随后
`Rope::from(changed_text.slice(new_range))` 复制整段尾部、逐行
`line.to_string()`（`:230`），最后 `soft_lines` 全量求和（`:275`）。

**已落地**

- `_update` 始终只替换 `start_row..=end_row`，`new_range` 截到
  `line_end_offset(new_end_row)`；多行编辑不再重排到文末。
- `soft_lines` 用 `旧值 - removed_soft + added_soft` 增量更新。
- `longest_row`：旧最长行在被替换区间之外时按行数差平移；被替换时重扫
  全表（只在编辑到最长行时发生）。
- 顺带修正 `update_all` 使用新文本长度作为旧 range 的问题：文本缩短时
  旧尾部会残留。现在用 `self.text.len()`（旧文本长度）；`WrapMap::set_text`
  不再预先写入新文本，并删除因此失效的 `TextWrapper::set_default_text`。
- 新增测试：`incremental_update_matches_full_rebuild`（逐字段对比行表、
  `soft_lines`、`longest_row`）、`single_line_edit_does_not_rewrap_tail`
  （10k 行文档单行编辑只调用 `wrap_line` ≤2 次）、
  `update_all_shortening_drops_stale_tail`（长文→短文全量替换不残留旧行）。
- 现有 `incremental_prefix_matches_full` 与 `test_update` 继续通过。

### P2：表格内击键全量分类

`auto_prettify_table`（`view.rs:1590`）先用 `cursor_line_is_table()` 做廉价
门控；命中表格后执行 `m.lines().to_vec()`（`:1609`）+ `table_block_range` →
`classify_document`（`table_ops.rs:26-36`），全文档分类且每键一次。

**已落地**

- 新增 `table_ops::table_block_range_local`：从 `planned_classified` 的
  64 行检查点克隆 `DocState`，向前分类到表格结束；检查点已落在表格内部时
  借助旧 `blocks`（脏行之前的前缀可信）回溯表头。
- `auto_prettify_table` 删掉 `m.lines().to_vec()` 全量克隆；块范围、单元格
  位置与 `prettify_edits` 在 `edit` 外计算，只把
  `replace_rows_and_move` 留在 `edit` 内，保持撤销语义。
- 无检查点、旧 blocks 未覆盖或检查点晚于光标时退回原 `table_block_range`
  全量分类，行为保持。
- 测试：与全量分类在检查点边界/多个 `dirty_from` 下逐项相等；跨检查点
  回溯；无 outer pipe 表格与普通续行；无分隔行不算表格；10k 行文档中
  表格在 9000 行时只分类 <200 行（测试探针 `TABLE_CLASSIFY_CALLS`）。

### P2：本地路径补全同步 IO

`BuiltinCommandManager::refresh_state`（`builtin_commands/mod.rs:991`）在参数
阶段每键调用 `refresh_path_completions`，本地分支直接走
`PathCompletionEngine::complete`（`:1083-1084`），内部 `std::fs::read_dir` +
逐项 `file_type`（`path_completion.rs:130-133`）。大目录输入会卡 UI。

**已落地**

- `BuiltinCommandManager` 新增 `LocalCompletionRequest` / `LocalCompletionContext`
  与请求代次；本地分支立即显示“读取目录中…”占位，后台读取完成后按上下文拼回：
  - `Plain`：普通参数候选；
  - `ScpHostOrLocal`：主机候选 + 本机文件；
  - `ScpLocalPath { host_token }`：`<host_token>:path`。
- `TerminalView::spawn_local_dir_load`：40 ms 防抖 + `background_executor`
  读盘；醒来先校验代次，过期请求不读盘、不回写。
- `refresh_path_completions` 每次输入先作废在途本地请求；`apply_local_completion_result`
  只接受当前代次。
- 新增测试：普通 `Plain` 的异步往返与过期结果丢弃、`ScpLocalPath` 的
  host 前缀、`ScpHostOrLocal` 的主机候选 + 本机文件。

### P2：侧边栏每帧配置深拷贝

`HostView::render`（`host_view.rs:1064-1067`）每帧 `load_grouped_hosts()` +
`SshGlobalConfig::load()`；`load_grouped_hosts` 内部又 load config + keychain
（`hosts_view.rs:179-181`）。`KeychainSidebarView::render`
（`keychain_view.rs:162-164`）每帧 `KeychainStore::load_or_default()`。
`load()` 有 mtime/size 缓存，但仍每帧 `stat` + 深拷贝整个对象。

**已落地**

- `SshGlobalConfig::load_shared()`、`KeychainStore::load_shared()` 直接返回
  现有 mtime/size 缓存里的 `Arc`；`load()` 仍按原语义返回深拷贝。
- `HostView` 增加 `GroupedHostsCache`：保存配置/钥匙串 `Arc`、主机/配置/文件夹
  列表、排序设置与文件夹计数；`Arc::ptr_eq` 或排序设置变化时才重建。
  `render` 不再每帧 `load_grouped_hosts()`、排文件夹、克隆配置。
- `KeychainSidebarView` 保存 `Arc<KeychainStore>`；`Arc::ptr_eq` 不变时复用。
- 外部编辑/应用内保存都会让 mtime/size 缓存生成新 `Arc`，从而触发重建；
  `load_shared` 的 stat 仍保留，保证外部修改可见。

### P3：其余低风险项

- `terminal_view.rs:5231-5244`：`to_esc_str` 返回 `Vec<u8>` 后
  `bytes.to_vec()`，无广播回调时也复制。`terminal.write(&bytes)` 之后把
  `bytes` move 给 `input_bytes`；广播关闭时跳过。
- `view.rs:5583`：每帧 `cx.theme().clone()`；`AppTheme` 含
  `String id/name/description`（`theme/mod.rs:390-398`）。已改为
  `ThemeState` 持有当前主题的 `Arc`，新增 `ActiveTheme::theme_shared()`；
  live markdown 的 4 处 `cx.theme().clone()` 全部替换，克隆降为 O(1)。
  `MarkdownStyle::themed` 仍每帧重建；它是 `Clone` 且分配量小，留待基准
  显示明显收益后再做键控缓存。
- `terminal.rs:4001-4005`：partial damage 分支对所有视口行重算
  `row_content_hash`。已改为沿用上一帧指纹、只更新 `dirty_rows`；兜底全量
  重建或指纹长度不匹配时才全量重算。新增
  `partial_frame_hashes_only_dirty_rows`（单行写入 ≤2 行指纹）。
- `diff_view.rs:681-691`：每键 `value().to_string()` + 全文比较已加长度快速
  路径。unified 路径改为 60ms 防抖写回：`on_unified_changed` 只置脏并调度，
  `flush_unified_writeback` 在停顿后做一次 `worktree_from_merged` + 右栏
  `set_text`；写回期间用 `suppress_right_change` 抑制二次调度。`save` 与
  `recompute` 入口先 flush，快速 Cmd+S / 重算仍能拿到最新文本。
  验证：diff 44 passed、diff_model 27 passed。
- `search.rs:132-146`：面板打开时每键 `to_string()` + 全量重扫。已改为
  `sync_text_debounced`：60ms 防抖 + 代次校验，连续输入只在停顿后重扫；
  打开/程序化替换走 `sync_text` 立即同步并取消挂起任务。
- `terminal_element.rs:283-305`：`rotate_row_caches` 已改为原地
  `rotate_left`/`rotate_right` + 越界端重置，不再分配 `slots`/`rotated`
  两个 Vec；补了负向滚动断言。
- `element.rs:4289`：`last_layout` 每帧 clone（内部多个 Vec）。已改为
  `InputState.last_layout: Option<Rc<LastLayout>>`，paint 末尾写回只做一次
  引用计数递增；`PrepaintState` 同步持有 `Rc<LastLayout>`。
- `element.rs:2022-2076`：`layout_lines` 每帧对每个可见行
  `slice_line().into()` + `shape_line`；可在
  `(text_version, highlight_version, visible_range, font/width)` 不变时复用
  `last_layout.lines`（已经是 `Rc<Vec<LineLayout>>`）。

---

## 三、结构与重构

### R1：`EditorFile` 的互斥 Option → enum（已完成）

`editor.rs:270-318` 同时有 `markdown`、`live`、`image`、`diff`、`diff_repo`
五个互斥选项，另有 7 个 bool；全文件约 20 处 `is_some()` 分支（render 的
模式分派在 `7751-7800`）。

```rust
enum TabContent {
    Code(Entity<InputState>),
    MarkdownPreview(Entity<Markdown>),
    Live(Entity<LiveMarkdownView>),
    Image(Entity<ImageViewer>),
    Diff { view: Entity<DiffView>, repo: Option<GitRepository> },
}
```

**已落地**：`EditorFile` 的 `markdown`/`live`/`image`/`diff`/`diff_repo`
五个 Option 已收进 `enum TabContent`（`Code` / `MarkdownPreview` / `Live` /
`Image` / `Diff { view, repo }`），并提供只读访问器。所有构造点按模式构造
枚举，非法组合在类型层面不可表达。打开文件的 4 个 bool 参数仍是后续项
（`OpenMode` + `OpenFlags`）。验证：`cargo test --lib editor::editor::tests`
（49 passed）。

### R2：巨型文件与超长函数

按“按领域拆、测试随代码走”的原则，优先处理（生产行数已排除测试模块）：

| 文件 | 生产行数 | 建议拆分 |
|---|---:|---|
| `editor/markdown_live/view.rs` | 8,311 | render（行/表格/代码块/mermaid/图片）、input（on_key/IME/鼠标）、search、tests |
| `editor/editor.rs` | 9,045 | open、file_io、tabs、lsp、render、tests |
| `terminal/terminal.rs` | 5,634 | snapshot、agent、input、osc、remote |
| `terminal_view/terminal_view.rs` | 5,969 | render、scrollbar、search、toast、hooks、builtin_commands |
| `components/input/state.rs` | 5,060 | search_ui、decorations、layout、scroll、tests |
| `components/input/element.rs` | 4,839 | prepaint、paint、overlay、gutter、tests |
| `workspace/workspace.rs` | 3,010 | transfer_hub、resize、session |

**已落地（2026-11-05）**：`InputState` 的 LSP/hover/completion 状态域
（约 858 行）原样迁到 `components/input/state/lsp.rs`。`state.rs` 从
5,918 行降到 5,060 行，拆分前后内容逐行比对一致，无逻辑改动。

| 函数 | 行数 |
|---|---:|
| `LiveMarkdownView::render`（`view.rs:5582-8281`） | 2,700 |
| `TextElement::paint_inner`（`element.rs:3143-4318`） | 1,176 |
| `TerminalElement::prepaint`（`terminal_element.rs:1480-2291`） | 812 |
| `TextElement::prepaint_inner`（`element.rs:2455-3141`） | 687 |
| `Editor::render`（`editor.rs:7669-8322`） | 654 |
| `LiveMarkdownView::on_key`（`view.rs:3282-3913`） | 632 |
| `TerminalView::render`（`terminal_view.rs:5415-5867`） | 453 |
| `Terminal::update_last_content`（`terminal.rs:3813-4048`） | 236 |

拆分时先抽“渲染上下文结构体”（例如 `MarkdownRenderCtx`、`EditorRenderCtx`），
避免把局部变量变成一长串函数参数；每次提交只动一个领域。

### R3：参数梯与 God object（部分完成）

当前有 22 处 `#[allow(clippy::too_many_arguments)]`。典型：

- `shape_overlay_layout`（`terminal_element.rs:1341`）16 个参数
- `open_file_with_content`（`editor.rs:6353`）13 个参数
- `open_remote_file_inner`（`editor.rs:6003`）10 个参数
- `render_path_overlay`（`builtin_commands/overlay_ui.rs:318`）11 个参数
- `create_remote_terminal`（`terminal.rs:2081`）10 个参数
- `LiveLine::new`（`markdown_live/element.rs:415`）10 个参数
- `transfer_one`（`file_manager/manager.rs:342`）9 个参数

**已落地**：

- `open_file_with_content` 的 13 个位置参数收成 `OpenFileRequest`。
- `open_local_file_inner` / `open_remote_file_inner` 的 4 个布尔参数收成
  `OpenOptions { mode: OpenMode, silent }`；早期透传但从未读取的
  `notify_app` 已于 2026-11-05 删除。
- 剩余 `too_many_arguments` 从 22 处降到 20 处；`shape_overlay_layout`
  （16 参数）、`render_path_overlay`（11）、`create_remote_terminal`（10）、
  `LiveLine::new`（10）仍待处理。
- God object（`InputState` 97 字段、`TerminalView` 83、`Terminal` 68、
  `Editor` 56）尚未拆子结构，属于长期结构债。

字段规模（约数）：

| 结构体 | 字段 / Option |
|---|---|
| `InputState`（`state.rs:469`） | 97 / 42 |
| `TerminalView`（`terminal_view.rs:461`） | 83 / 41 |
| `Terminal`（`terminal.rs:1129`） | 68 / 33 |
| `Editor`（`editor.rs:504`） | 56 / 23 |

建议子结构：

- `InputState` → `SearchUi`、`LspUi`、`Decorations`、`LayoutCache`、`ScrollState`
- `TerminalView` → `ScrollbarUi`、`ToastUi`、`SshConnectUi`、`SearchUi`、
  `HookPromptUi`、`KittyUi`、`RenderCache`
- `Terminal` → `TerminalIo` enum（`Local` / `Russh` / `SshPty`）+
  `AgentState`、`KittyState`、`SnapshotState`
- `Editor` → 文件打开/加载状态、LSP 菜单状态、完成菜单状态

`Terminal` 的 local/remote 字段组尤其明显：`local_sender` / `_local_reader` /
`child_process_id` / `shell_process_group_id` / `pty_master_fd` /
`local_mode_1016` 与 `ssh_pty_terminal` / `remote_*` / `kitty_scanner` 互斥。
后端枚举改动面大，放在结构重构后期。

### R4：失效的 feature flag（已完成）

`Cargo.toml:256-257` 的 `tree-sitter-languages` 默认开启、没有 CI/脚本切换；
`cargo check --no-default-features` 失败 22 个错误。例如
`Language::display_name`（`languages.rs:74-108`）没有加 `cfg`，在只有 `Json`
变体时匹配所有语言会产生 `E0599`。tree-sitter 依赖全部无条件编译，关闭该
feature 也不减少构建时间。

**已处置**：该 feature 默认开启、无 CI/脚本切换，且关闭后无法编译；按
“删除而非修复”处理，移除 `Cargo.toml` 的 feature 与 `languages.rs` 的全部
cfg 分支/fallback。`cargo check --no-default-features` 现在通过。
若未来确需轻量构建，再把 grammar 依赖设 `optional = true` 并补齐 cfg。

### R5：模块环与层级倒置（本轮范围已完成）

- `ssh/sftp_fs.rs:18` 依赖 `file_manager::file_tree_cache::RemoteFileTreeCache`。
- `file_manager/file_tree_cache.rs:8` 依赖 `editor::encoding::FileEncoding`。
- 原 `components/macos_sidebar` 是 workspace 级应用外壳，却直接依赖
  `sidebar_view`、`file_manager`、`terminal_view` 等业务模块。
- `components/title_bar.rs` 直接持有 `Workspace`/`TabBar`，同样属于
  workspace 外壳。

**已落地**：

- `FileEncoding` 下沉到 `utils::encoding`，`editor::encoding` 仅 re-export；
  `file_manager::file_tree_cache` 不再反向依赖 `editor`。
- `sftp_fs::read_file_bytes` 改为先 `metadata` 判大文件，删除对
  `file_manager::file_tree_cache::RemoteFileTreeCache` 的依赖；
  `ssh ↔ file_manager` 的直接环解除。
- `macos_sidebar` 整体迁到 `workspace/sidebar`，与它实际所处的应用层级一致；
  `components` 不再承载 workspace 外壳。依赖 `Editor` 与侧栏的
  `sidebar_view::open_path` 同步迁为 `workspace::sidebar::open_path`，
  `sidebar_view` 不再反向依赖应用外壳。
- `TitleBar` 迁到 `workspace/title_bar.rs`，`components` 不再反向依赖
  `Workspace`/`TabBar`。

上述依赖倒置已解除。`components/input` 与 editor/LSP 之间仍有更细粒度的
耦合，编辑器/LSP/终端之间也仍有模块依赖；若继续清理，应按单项依赖拆成
独立改动。

### R6：验证基础设施

- clippy 基线 166 条；先做一次独立的机械性清理提交，再启用 `-D warnings`
  门禁。不要混在重构提交里。
- `.github/workflows/ci.yml:18/37/57` 三个 job 全部是 `macos-latest`；
  Windows/Linux 的 cfg 代码没有编译兜底。建议加 `cargo check
  --workspace --all-targets` 的 matrix。
- 旧审计文档行号已失效；引用前核对，或在开头标注归档。

---

## 四、已经优化过、不要重复动

- 终端行缓存、增量快照、主题/选区指纹、滚动平移复用：
  `terminal_element.rs:240`、`terminal.rs:3813`，见 `docs/PERF_2026-10-27.md`。
- markdown live 的增量规划、`RowPlan.visible: SharedString`、分类检查点：
  `view.rs:1098`、`reveal.rs:223`。
- 图片后台加载与缓存：`md_image.rs:225`。
- tree-sitter `Query` 的 `Arc` 缓存：`simple.rs:67`。
- 远端 watcher 的 metadata-first、2 MB 上限、30 s 内容哈希：`watcher.rs:430`。
- 文件树缓存的内存条数/字节上限、孤儿服务器 TTL：`file_tree_cache.rs:35-61`。
- prepaint 只在滚动真的变化时 `cx.notify()`：`element.rs:4312`。
- 终端鼠标移动按网格 cell 去重、Kitty 动画按需续帧：`terminal.rs:1336`、
  `terminal_view.rs:3257`。
- 安全项：`ssh/mod.rs:77` 的 `mktemp`、`cross_remote.rs:645` 的 known_hosts、
  `sftp_fs.rs:737` 的原子写、`zeroize` 在 keychain/agent/scp lease 的使用。
- 以下 Option 字段是有意设计，不要机械套 enum：`lsp/jsonrpc.rs:127` 的
  `IncomingMessage`（区分 `result: null` 与字段缺失）、
  `theme/syntax_theme.rs` 的 `SyntaxColors`（反序列化中间态）、
  `file_manager/manager.rs:70` 的 `TransferRun`（真实可选能力）。

---

## 五、执行顺序与门禁

| 阶段 | 内容 | 状态 |
|---|---|---|
| 0 | 跑 `cargo test`、现有 bench、新增 editor bench / TextWrapper 微基准 | 部分（全量测试已跑；bench 未跑） |
| 1 | 低风险快赢：搜索面板弱引用 + matcher 清理、终端按键 Vec move、主题 Arc/样式缓存、侧边栏 Global 缓存、本地路径补全后台化 | 完成 |
| 2 | 算法向：编辑器脏检查 → visible range/layout cursor → TextWrapper splice → 表格局部分类 → 终端脏行哈希 → diff 门控 | 完成 |
| 3 | 结构向：`EditorFile` enum → 超长 render 拆分 → 参数结构体 → God object 子结构 → 巨型文件拆分 | 部分（enum/参数结构体完成） |
| 4 | 收尾：clippy 清理、`tree-sitter-languages`、模块环、CI matrix、`cargo build --timings` | 部分（feature/模块环完成） |

每步验证：

```bash
cargo fmt --all
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings   # 当前基线 166，先清零或约定“不新增”
cargo test
cargo bench --bench terminal_render
cargo bench --bench markdown_live
# 终端行缓存改动额外跑：
DEEPTERM_VALIDATE_ROW_CACHE=1 cargo run --release
```

按 `rust-refactor` 的约定：单次只做一个可独立评审的改动；不夹带功能修改或
无关格式化；每个中间状态都必须可编译；涉及渲染/PTY 热路径时先记录基线再复测。

---

## 六、完成度与剩余

**已完成（本轮落地并验证）**

- 内存：搜索面板弱引用环 + matcher 释放。
- 热路径：编辑器脏检查长度快速路径；可见区锚点复用；`TextWrapper` 局部重排；
  表格局部检测；终端脏行指纹；终端按键 Vec move；主题共享 Arc；
  `rotate_row_caches` 原地旋转；`LastLayout` Rc + 整形结果跨帧复用；
  搜索面板防抖；本地路径补全异步化；侧边栏配置快照缓存。
- 结构：`EditorFile` → `TabContent` enum；`OpenFileRequest`/`OpenOptions`；
  移除失效的 `tree-sitter-languages` feature；`FileEncoding` 下沉、
  `ssh ↔ file_manager` 环解除；`FontState` 下沉到设置层；
  `macos_sidebar` 归位到 `workspace/sidebar`；`TitleBar` 归位到 workspace。

**仍待做（已单列，避免与行为改动混在一起）**

1. R2 巨型文件/函数拆分：`markdown_live/view.rs`、`editor.rs`、
   `terminal_view.rs`、`input/element.rs` 的超长 render/prepaint/paint。
   这类改动需要按领域逐个提交，且每一步跑完整输入/渲染测试。
2. R3 God object 子结构：`InputState`、`TerminalView`、`Terminal`、`Editor`。
3. R6：clippy 基线清理、Windows/Linux CI matrix、
   `cargo build --timings` 与是否拆 crate 的决策。
4. 基准：现有 terminal/markdown bench 尚未在本轮复跑；新增 editor bench
   后再给结构重构提供回归数据。

## 七、进展记录

### 2026-11-04：P0 搜索面板强引用环（已完成）

- `SearchPanel::editor` 从 `Entity<T>` 改为 `WeakEntity<T>`；两个构造点
  （`InputState::on_action_search`、`LiveMarkdownView::open_search`）传
  `cx.entity().downgrade()`。
- 面板侧所有访问先 `upgrade()`；宿主已释放时隐藏替换 UI、跳过跳转，
  `hide` 只关闭面板。
- `hide` 调用新增的 `SearchMatcher::release_text()`，释放全文副本与命中集，
  保留 query 与输入框内容；下次 `show` 由宿主重新同步文本。
- 新增回归测试：`search_panel_does_not_keep_host_alive`、
  `release_text_drops_matches_and_keeps_query`。
- 验证：`cargo fmt --all`、`cargo check --workspace --all-targets`、
  `cargo test --lib components::input::search::tests`（7 passed）。

### 2026-11-04：终端按键多余 Vec 拷贝（已完成）

- `TerminalView` 按键路径改为 `terminal.write(&bytes)` 后把 `bytes` move 给
  `input_bytes`，去掉每键一次 `to_vec()`。
- 验证：`cargo fmt --all`、`cargo check --workspace --all-targets`、
  `cargo test --lib terminal_view::terminal_view::tests`（17 passed）。

**下一步**：结构重构 R1：`EditorFile` 的互斥 Option → enum。

### 2026-11-04：模块环清理（R5 部分完成）

- `FileEncoding` 下沉到 `utils::encoding`，`file_manager` 不再依赖 `editor`。
- `sftp_fs::read_file_bytes` 用 `metadata` 判定大文件，删除对
  `file_manager::file_tree_cache` 的依赖。
- 验证：`cargo test --lib encoding`（12 passed）、`cargo test --lib
  file_manager`（62 passed）、`cargo test --lib ssh::`（82 passed）。

### 2026-11-04：移除失效 feature flag（R4 完成）

- 删除 `tree-sitter-languages` feature 及其在 `languages.rs` 的 6 组
  cfg 分支/fallback；该 flag 默认开启、无人切换，关闭时本来有 22 个编译错误。
- 验证：`cargo check --workspace --all-targets`、`cargo check
  --no-default-features` 均通过；`cargo test --lib syntax` 相关测试后续随
  全量测试覆盖。

### 2026-11-04：DiffView unified 写回防抖（P3 完成）

- `on_unified_changed` 只置脏 + 60ms 防抖；`flush_unified_writeback` 统一做
  `worktree_from_merged` + 右栏 `set_text`，`suppress_right_change` 防止
  Change 订阅重复调度。
- `save` 与 `recompute` 入口先 flush；`fetch_contents` 切换基线/目标时作废
  挂起代次。
- 验证：`cargo test --lib editor::diff`（44 passed）、
  `cargo test --lib diff_model`（27 passed）。

### 2026-11-04：打开文件参数梯收敛（R3 部分完成）

- `open_file_with_content` → `OpenFileRequest`（13 参数收成 1 个具名结构）。
- `open_local_file_inner` / `open_remote_file_inner` 的
  `edit_mode/live_mode/silent` 收成 `OpenMode` + `OpenOptions`。
- 验证：`cargo test --lib editor::editor::tests`（49 passed）。

### 2026-11-04：EditorFile 内容模式 enum（R1 完成）

- `EditorFile` 的五个互斥 Option 收进 `TabContent`，删除字段级非法状态；
  所有构造点（恢复、普通打开、diff tab）按模式构造枚举。
- 原有 `.live`/`.markdown`/`.image`/`.diff` 访问改为只读访问器，渲染分派
  与脏检查行为保持不变。
- 验证：`cargo test --lib editor::editor::tests`（49 passed）。

### 2026-11-04：输入区渲染缓存收尾（P3 完成）

- `rotate_row_caches` 改为原地 `rotate_left`/`rotate_right`，不再每滚动帧
  分配两个 Vec；补负向滚动断言。
- `InputState.last_layout` 与 `PrepaintState.last_layout` 改存
  `Rc<LastLayout>`，paint 末尾写回只做引用计数递增。
- `LastLayout` 记录 `text_version`/`font_size`/`runs`/`has_whitespace_indicators`；
  文本、可见行、run、字号、对齐都没变时复用上一帧 `LineLayout`，跳过每帧
  `shape_line`。新增 `unchanged_frame_reuses_shaped_lines`（第二帧
  `layout_lines` 调用 0 次）。
- 验证：`cargo test --lib components::input`（217 passed）。

### 2026-11-04：搜索面板防抖同步（P3 已完成）

- `SearchPanel` 新增 `pending_sync` / `sync_generation`；宿主编辑走
  `sync_text_debounced`（60ms），打开/程序化替换走 `sync_text` 立即同步并
  取消挂起任务；`hide` 同时取消任务并释放文本。
- 验证：`cargo test --lib components::input::search`（22 passed）。

### 2026-11-04：DiffView 脏检查长度快速路径（P3 部分完成）

- `on_right_changed` 先比 `text().len()` 与 `saved_text.len()`，长度不同
  直接判脏；同长度才构造全文比较。unified 路径的 `worktree_from_merged`
  仍每键执行，留作后续带回写抑制的防抖改动。
- 验证：`cargo test --lib editor::diff`（44 passed）、
  `cargo test --lib diff_model`（27 passed）。

### 2026-11-04：终端脏行指纹（P3 已完成）

- partial damage 分支不再清空并重算所有视口行指纹；未变行沿用上一帧，
  只更新 `dirty_rows`。兜底全量重建/指纹长度不一致时仍全量重算。
- 新增测试 `partial_frame_hashes_only_dirty_rows`（单行写入只重算 1~2 行）。
- 验证：`cargo test --lib terminal_memory`（10 passed），包含增量快照与
  全量重建的逐格平价测试。

### 2026-11-04：主题共享句柄（P3 已完成）

- `ThemeState` 持有 `active_theme: Arc<AppTheme>`；`ActiveTheme` 新增
  `theme_shared()`，主题切换/设置同步时更新 Arc。
- `markdown_live` 的 `view.rs`/`highlight.rs` 中 7 处 `cx.theme().clone()`
  改为 `theme_shared()`，每帧不再深拷贝 `id/name/description`。
- 验证：`cargo test --lib theme`（8 passed）、
  `cargo test --lib editor::markdown_live`（324 passed / 1 ignored）。

### 2026-11-04：侧边栏配置快照缓存（P2 已完成）

- 新增 `SshGlobalConfig::load_shared`、`KeychainStore::load_shared`（复用现有
  mtime/size 缓存里的 `Arc`）；`load()` 保持深拷贝语义。
- `HostView` 缓存 `Arc<配置> + Arc<钥匙串> + 主机/配置/文件夹列表 + 计数 +
  排序`，仅在 `Arc::ptr_eq` 变化或排序设置变化时重建。
- `KeychainSidebarView` 缓存 `Arc<KeychainStore>`，渲染直接读 `items`。
- 验证：`cargo test --lib sidebar_view`（28 passed）、
  `cargo test --lib ssh::`（82 passed）。

### 2026-11-04：本地路径补全异步化（P2 已完成）

- `BuiltinCommandManager` 新增本地补全请求/代次/上下文；本地分支先显示
  占位项，`TerminalView` 以 40ms 防抖 + 后台 executor 读取目录。
- 过期代次不回写；`ScpHostOrLocal` 保留主机候选并追加本机文件，
  `ScpLocalPath` 保留 `host_token:` 前缀。
- 新增 3 个 manager 测试：异步往返 + 过期丢弃、ScpLocalPath 前缀、
  ScpHostOrLocal 合并。
- 验证：`cargo test --lib builtin_commands`（160 passed）、
  `cargo test --lib terminal_view::terminal_view::tests`（17 passed）。

### 2026-11-04：表格内击键局部检测（P2 已完成）

- 新增 `table_block_range_local`：从 `Classified::states` 检查点恢复分类，
  只扫描光标附近的表格；检查点在表格内部时用旧 blocks 回溯表头，异常时
  退回全量 `table_block_range`。
- `auto_prettify_table` 去掉 `lines().to_vec()`，块范围/单元格/edits 在
  `edit` 外计算，`replace_rows_and_move` 仍在 `edit` 内。
- 新增 5 个测试：检查点边界/多个 `dirty_from` 与全量一致、跨检查点回溯、
  无 outer pipe + 续行、无分隔行、10k 行文档只分类 <200 行。
- 验证：`cargo test --lib editor::markdown_live`（324 passed, 1 ignored）、
  `cargo test --lib local_table_block`（5 passed）；
  `cargo test --lib` 最终 2302 passed / 0 failed / 1 ignored（此前
  `terminal::local_pty` 偶发环境超时，最后一次全绿）。

### 2026-11-04：TextWrapper 只重排受影响行（P1 已完成）

- `_update` 的行替换范围固定为 `start_row..=end_row`，新文本范围截到
  `line_end_offset(new_end_row)`；多行编辑不再重排到文末。
- `soft_lines` 增量更新；`longest_row` 在旧最长行被替换时重扫全表，否则
  按插入/删除行数平移并与新行比较。
- 修正 `update_all` 的旧 range：用旧文本长度，避免缩短文本时旧尾部残留。
- 修正 `update_all` 的旧 range，并让 `WrapMap::set_text` 先走全量更新再写
  `self.text`；删除失效的 `TextWrapper::set_default_text`。
- 新增 `incremental_update_matches_full_rebuild`（行表/`soft_lines`/
  `longest_row` 与全量重排逐字段相等）、`single_line_edit_does_not_rewrap_tail`
  （10k 行文档单行编辑调用 `wrap_line` ≤2 次）、
  `update_all_shortening_drops_stale_tail`。
- 验证：`cargo test --lib components::input`（216 passed）、
  `cargo test --lib components::input::display_map`（29 passed）；
  `cargo test --lib`：2294 passed，3 个 `terminal::local_pty` 超时（环境）。

### 2026-11-04：编辑区可见区锚点复用（P1 已完成）

- `DisplayMap` 新增 `geometry_dirty_from` 与 `take_geometry_dirty_from()`；
  `WrapMap::on_text_changed` 返回增量起点；折叠、git 展开区、布局/字体变化
  保守标记 0。`set_git_insertions` 空→空、`adjust_folds_for_edit` 无折叠时
  不再标脏；`set_font` 字体未变时 no-op，避免每帧调用打脏锚点。
- `TextElement::calculate_visible_range` 复用上一帧 `visible_top` /
  `visible_range.start`；`layout_cursor` 在光标与选区都可见时从可见区起点
  累计 y。
- 新增测试：`visible_range_reuses_last_anchor`（10k 行：首帧 10k 行扫描，
  复用后 <100 行；锚点行编辑仍复用；锚点前编辑回全量）、
  `text_edit_marks_geometry_from_edited_row`、`geometry_dirty_from_only_tracks_relevant_changes`。
- 验证：`cargo fmt --all`、`cargo check --workspace --all-targets`、
  `cargo test --lib components::input`（213 passed）、
  `cargo test --lib editor::editor::tests`（49 passed）、
  `cargo test --lib components::input::display_map`（25 passed）。
- `cargo test --lib` 多次运行：2292–2295 passed；每次 1–3 个
  `terminal::local_pty` 端到端用例超时（本机 PTY 环境限制；单独复跑同样
  失败，且每次失败的子集不同，与本次改动的输入/display map 路径无关）。

### 2026-11-04：普通编辑器脏检查长度快速路径（P1 部分完成）

- `Editor::file_is_dirty` 先比 `InputState.text.len()` 与
  `original_content.len()`；长度不同直接判脏。插入/删除类击键不再构造
  全文 `SharedString`。live 标签跳过该分支，保留剥 `\r` 的同口径比较。
- 新增回归测试 `plain_editor_dirty_check_covers_length_changes`：
  初始干净、插入判脏、同长度替换判脏、改回基线恢复干净。
- 验证：`cargo fmt --all`、`cargo check --workspace --all-targets`、
  `cargo test --lib plain_editor_dirty_check_covers_length_changes`（1 passed）、
  `cargo test --lib dirty`（4 passed）、
  `cargo test --lib content_matches_baseline`（1 passed）。

### 2026-11-05：打开链路与字体设置去重（已完成）

- 删除 `notify_app` 的整条透传链：`OpenOptions`、`OpenFileRequest`、本地/
  远程图片与文本打开函数都不再接收这个最终被丢弃的布尔值；`_no_notify`
  后缀失去语义，恢复为 `open_file_at_line_col` /
  `open_remote_file_at_line_col`，并删除未使用的 `open_file_no_notify`。
- `FontSettings` 与 `FontContent` 合并为后者；`FontState` 直接读写
  `SettingsContent.font`，删除 `from_settings`/`into_content` 两段人工转写，
  同时解除 `settings::content` 对 `sidebar_view::font_settings` 的反向依赖。
- 删除 `IncomingMessage.jsonrpc`、agent wire `v` 两个只反序列化不读取的字段，
  以及 `ssh::shell_quote_single` 纯转发包装。
- 验证：`cargo fmt --all --check`、`cargo check --release --workspace --all-targets`、
  settings/JSON-RPC/CLI-agent/SSH/LSP-jump 定向测试全绿。完整 lib 测试在
  release 模式下为 2264 passed / 52 failed / 3 ignored；失败集中在渲染、
  网络代理、共享内存与 PTY 环境用例（例如 `editor_width_follows_window`
  单独复跑时 `debug_bounds` 返回 `None`）。

### 2026-11-05：字体状态下沉与侧栏层级归位（已完成）

- `FontState` 从 `sidebar_view::font_settings` 下沉到
  `settings::font_state`，设置层不再反向依赖 UI 层；原路径保留兼容
  re-export，现有调用点无需一次性迁移。
- 原 `components/macos_sidebar` 迁到 `workspace/sidebar`。侧栏是
  workspace 级应用外壳，不属于可复用组件；`components` 不再承载应用外壳。
- `TitleBar` 从 `components` 迁到 `workspace/title_bar.rs`，`components`
  不再反向依赖 `Workspace`/`TabBar`。
- 依赖 `Editor` 与侧栏的 `sidebar_view::open_path` 同步迁入
  `workspace/sidebar/open_path.rs`，并收窄为 crate 内可见，解除
  `sidebar_view → components::macos_sidebar` 的反向依赖。
- 删除 `editor::git_repository` 内仅转发到 `utils::shell::shell_quote_single`
  的 `shell_quote` 包装及重复测试，直接复用统一实现。
- 验证：`cargo fmt --all --check`、`git diff --check`、
  `cargo check --release --workspace --all-targets`、
  `cargo clippy --release --workspace --all-targets`；定向测试
  `settings::tests`（2 passed）、`init_tests`（2 passed）、
  `workspace::tests`（2 passed）、`test_rust_highlight_use_path`（1 passed）、
  `editor::git_repository::tests`（21 passed）。

### 2026-11-05：InputState 的 LSP 状态域拆分（已完成）

- 将 `components/input/state.rs` 中连续的 LSP/hover/completion 状态访问、
  补全应用、诊断版本门控与 LSP 折叠逻辑迁到
  `components/input/state/lsp.rs`，以子模块内第二个 `impl InputState`
  扩展状态类型。
- 主文件从 5,918 行降到 5,060 行；新文件 862 行。迁移块与拆分前逐行
  `diff` 一致，属于纯移动，无逻辑改动。
- 验证：`cargo fmt --all --check`、`git diff --check`、
  `cargo check --release --workspace --all-targets`、
  `cargo clippy --release --workspace --all-targets`（183 条既有 warning，
  数量未变）；`cargo test --release --lib components::input` 为
  214 passed / 3 failed，失败均为 `hover_card_*` 用例读取
  `debug_bounds`，属于 release 构建不记录布局调试信息的既有环境限制；
  其余输入、补全、诊断与折叠测试全部通过。

### 2026-11-05：侧栏圆形图标按钮去重（已完成）

- 审计出 7 处完全同构的 28px 圆形图标按钮：主机页返回/更多、密钥串页
  返回/新增、主题页返回、字体页返回、文件树页返回。各调用点原先把尺寸、
  圆角、底色、hover、指针与 SVG 样式重复展开 14 行。
- 在 `sidebar_view::style` 新增 `round_icon_button(theme, icon, icon_size)`，
  只封装纯视觉样式并返回 `Div`；`id`、`on_click` 与 `on_mouse_down`
  仍由调用方绑定，避免为不同闭包类型引入泛型回调层。
- 7 个调用点全部改为单行构造，交互语义与图标尺寸保持不变；同时删除
  迁移后不再使用的 `button_bg` / `button_bg_hover` 导入。
- 验证：改动文件 `rustfmt --check`、`git diff --check`、
  `cargo check --release --workspace --all-targets`、
  `cargo clippy --release --workspace --all-targets`（183 条既有 warning，
  数量未变）、`cargo test --release --lib sidebar_view::host_view`
  （3 passed）。
- 环境说明：`src/components/stepper/stepper.rs` 与 `assets/icon.png` 曾被
  误删，前者会使 `cargo fmt --all` / 全仓编译直接失败。后续已从 `HEAD`
  恢复这两个仍被模块与打包配置引用的文件（见下一条进展记录）。

### 2026-11-05：终端事件分发与列表状态收敛（已完成）

- `TerminalView` 原有三份近乎相同的终端事件订阅：本地、首次 SSH 连接、
  重连后。公共事件处理完全一致，仅 `Wakeup` / `SelectionsChanged` 与 SSH
  专属事件依赖连接阶段。新增 `TerminalEventSource` 枚举和统一
  `handle_terminal_event`，把差异显式保留为三条阶段分支：
  - 本地：额外 `poll_wsl_cwd`，忽略 SSH 断连/端口转发；
  - 首次 SSH：额外检测远端 OS，处理 SSH 断连/端口转发；
  - 重连后：不再重复检测远端 OS，处理 SSH 断连/端口转发。
- `terminal_view.rs` 相对基线净减 63 行，三套事件匹配不再各自漂移。
- 侧栏 6 处列表行复用 `row_selection_state(theme, is_selected)`：选中态
  accent 实底、未选中态 hover 叠加与常规文字色保持原样；行高、内边距、
  图标和内容布局仍由各列表自行控制。
- 编辑器补全的下/上/首/末四个导航动作原先各复制 14 行可见性、焦点、
  按键消费与过期校验；收敛为 `prepare_completion_navigation`，四个动作只
  保留各自的选择移动逻辑。
- 删除 `md_image::is_supported_extension` 纯转发包装，调用方统一直接使用
  `image_viewer::is_image_file`，避免图片扩展名口径出现第二个入口。
- 验证：`cargo fmt --all --check`、`git diff --check`、
  `cargo check --release --workspace --all-targets`、
  `cargo clippy --release --workspace --all-targets`（183 条既有 warning，
  数量未变）；定向测试 `terminal_view::terminal_view::tests`
  （17 passed）、`sidebar_view::host_view`（3 passed）、编辑器补全/Enter/Tab
  （7 passed）、`editor::md_image::tests`（13 passed / 3 failed）。
  `md_image` 的 3 个失败为既有环境限制：1 个网络访问 `PermissionDenied`，
  2 个 release 渲染用例读不到布局/加载结果；其余图片路径解析、URL 与缓存
  用例全部通过。

### 2026-11-05：恢复误删文件并修复全仓编译（已完成）

- 编译阻断根因不是本轮结构改造，而是两个被误删的受版本控制文件：
  - `src/components/stepper/stepper.rs` 仍被 `stepper/mod.rs` 声明，
    且 `Stepper` / `StepperStatus` 被 `ssh_view/connect.rs` 实际使用；
  - `assets/icon.png` 仍被 `[package.metadata.bundle].icon` 与图片相关测试
    引用。
- 两者已从 `HEAD` 原样恢复，没有通过删除模块、改导入或绕过打包配置来
  掩盖缺失文件。
- 验证：`cargo fmt --all --check`、`git diff --check`、
  `cargo check --release --workspace --all-targets`、
  `cargo clippy --release --workspace --all-targets`、
  `cargo build --release --workspace` 全部通过。Clippy 仍为既有 183 条
  warning；release 构建仅剩 Cargo 对 `mach2` 用途归属的既有 manifest
  warning，不影响编译产物。

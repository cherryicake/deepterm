# deepterm 编辑器 vs Zed 功能对齐差距审计

日期：2026-09-27
对照基线：`/Users/zhousong/Documents/workspace/zed`（上游 Zed，`40d20036af`）
审查范围：`src/editor/**` + `src/components/input/**`（62,086 行）及周边（sidebar / workspace / theme / git）

---

## 0. 总体结论

deepterm 的代码编辑器是一个**语法层编辑器**，Zed 是一个**语义层编辑器**。差距不是零散功能，
而是三层结构性底座缺失，绝大多数具体功能都是这三层的下游：

| 底座 | Zed | deepterm | 后果 |
|---|---|---|---|
| **LSP 客户端** | `crates/lsp`(2,542) + `project/lsp_store.rs`(15,288) | **0 命中** | 补全 / hover / 跳转 / 诊断 / 重命名 / 格式化 全部不存在 |
| **project + buffer 注册表** | `crates/project`(73,416) + `multi_buffer`(16,329) + `text`(6,570) | **无**（每 tab 自带 `Entity<InputState>`） | 无跨文件能力、无 BufferId、无 Anchor 锚点 |
| **settings + keymap + action 注册表** | `settings_store.rs`(3,058) + `keymap_file.rs`(2,609) + `all_action_names()` | **均无**，177 条键位硬编码 | 用户零可配；动作不可发现、不可按名派发 |

> 这不是"少了几个功能"，而是底层数据结构被换掉了。deepterm 自己的
> `docs/EDITOR_AUDIT_2026-08-30.md:27` 已经指出同一结论。

**规模对照**

| 模块 | Zed | deepterm |
|---|---|---|
| editor crate | 153,527 行 | 代码编辑器 ≈ 22k 行（`editor.rs` 5,022 + `input/**` ≈17k） |
| 选区模型 | `selections_collection.rs` 1,470 + `selection.rs` 2,398 | `selection.rs` **51 行**（单选区） |
| 移动语义 | `movement.rs` 1,509 | `movement.rs` 264 |
| 折叠 | `folding_ranges.rs` 1,192（LSP + 语法树） | 缩进启发式 + 3 个硬编码括号 |
| 搜索 | `crates/search` 11,973 | 单文件查找条 1,381 |
| Vim | `crates/vim` 47,384 | 无 |

---

## 1. P0 — 根因级缺口（不补则下游无从落地）

### 1.1 完全没有 LSP 客户端
- **证据**：`grep -rIni 'lsp' src/` 全仓仅 2 处命中，且都无关——`src/components/input/element.rs:1537`
  是转述 Zed 的注释，`src/editor/markdown_live/table_ops.rs:920` 是中文里的 "colspan"。
  `language_server|jsonrpc|Content-Length|textDocument` → 各 0。`Cargo.toml` 无 lsp 依赖，
  `Cargo.lock` 无 lsp 包。
- **现状替代**：tree-sitter 语法高亮（22 种语言，真实 grammar crate + 21 个 vendored `.scm`），
  驱动 `SimpleHighlighter`（`src/editor/syntax/simple.rs:75/123/349`）。
- **顺带发现**：仓库根的 `.zed/settings.json`（写着 `format_on_save: "off"`）**是死配置**，
  `grep -rIn '\.zed|language_overrides|format_on_save' src/` → 0，没有任何代码读它。
- **连带缺失**（全部 0 命中）：补全、hover 文档、转到定义/声明/类型定义/实现、查找引用、
  文档符号/工作区符号、重命名符号、代码动作、签名帮助、内联提示、语义 token、code lens、
  文档格式化、链接编辑、编辑预测、文档颜色、on-type formatting。

### 1.2 无 project / buffer 注册表，且选区无锚点
- **证据**：`Editor { open_files: Vec<EditorFile> }`（`src/editor/editor.rs:275-340`），
  每个 `EditorFile` 独享 `Entity<InputState>`（`:100-134`）。`MultiBuffer|multi_buffer` → 0，
  `worktree` 命中全在 `src/agent/cli_agent.rs`（指 Claude 的目录名，非 git worktree）。
- **关键风险**：`Selection { start: usize, end: usize }`（`src/components/input/selection.rs:5-8`）
  是**裸字节偏移，没有 Anchor**。任何异步回包（诊断/补全/hover）到达时文本可能已漂移，
  裸 offset 会指向错误位置 → 错位高亮甚至错误编辑。
- **结论**：LSP 不是"加一个模块"。必须先有「稳定文件身份 + 版本号 + 跨文件 buffer 注册表 +
  锚点快照」，否则每一项语义能力都只能退化成语法启发式。

### 1.3 诊断系统是空壳（但**渲染管线已铺好**，是投入产出比最高的一项）
- **证据**：`src/editor/syntax/mod.rs:19-41` —— `pub struct DiagnosticSet;`（单元结构体，
  无任何字段），`for_offset()` 恒返 `None`，`styles_for_range()` 恒返 `Vec::new()`。
  调用点 `state.rs:1047/1773/4305/4539` 全是 no-op。
- **关键**：渲染侧**已经接通**——`element.rs:1952-1957` 从 `InputMode::CodeEditor` 取出
  `diagnostics`，`:2063` 调 `diagnostics.styles_for_range(&visible_byte_range, cx)`，
  `:2064` 用 `combine_highlights_stable` 并入高亮。**只是数据源恒为空。**
- **含义**：补上 `DiagnosticSet` 的数据结构 + 任意一个产出方（哪怕先支持外部注入 JSON），
  就能一次点亮四处能力：inline 高亮、gutter 标记、滚动条诊断标记、go-to-diagnostic。
- **注意**：tree-sitter 本身能产出 `ERROR`/`MISSING` 节点，但
  `is_error|is_missing|has_error` → **各 0**，从未被查询。这条**不依赖 LSP 就能补**。

### 1.4 无 settings 系统
- **证据**：`SettingsStore|settings_store|settings_file` → 0。配置是 6 个各自手写 load/save 的
  JSON：`session_state.json`（`workspace/persistence.rs:127`）、`theme.json`（`theme/mod.rs:621`）、
  `font.json`（`sidebar_view/font_settings.rs:114`）、`ssh.json`、`keychain.json`、
  `command_history.json`。无分层覆盖、无 watcher、无 schema、无 per-language、无 per-project。
- **连带**：`tab_size|hard_tabs|format_on_save|autosave` → 0。

### 1.5 无 keymap 抽象，177 条键位硬编码
- **证据**：`KeyBinding::new` 共 **177 处**，散在 8 个 `init()` 里
  （`workspace.rs:94`、`input/state.rs:273`、`editor/editor.rs:60/66`、`main.rs:58`、
  `terminal_view.rs:148`、`popup_menu.rs:21`、`modal.rs:26`、`markdown.rs:222`）。
  无 keymap 文件、无用户自定义、**0 处** `&&`/`||`/`==` 上下文谓词
  （gpui 的 `KeyBindingContextPredicate` 就在依赖树里，未用）。

### 1.6 无 action 注册表 / 无命令面板
- **证据**：`register_action|all_action_names|available_actions|action_registry` → **0**。
  18 个 `actions!` 模块、127 个 `on_action` handler，但只能由写死的键位触发，无法按名调用。
- **为什么是 P0**：Zed 的 keymap 值就是 `"editor::Copy"` 这样的字符串。没有按名派发层，
  1.5 的 keymap 文件化在结构上不可能实现。
- **命令面板**：`command_palette|CommandPalette` → 0。`builtin_commands/registry.rs:137-157`
  只注册了 `/scp` 和 `/open` 两条**终端斜杠命令**，与编辑器动作无关。

---

## 2. P1 — 高频日常能力缺口

| # | 能力 | Zed 位置 | deepterm 证据 |
|---|---|---|---|
| 2.1 | **多光标 / 多选区** | `selections_collection.rs`(1,470) | `multi_cursor\|add_selection\|SelectNextOccurrence\|column_select` → **0**；`Selection` 单区间。**改动面**：`selected_range` 全仓 **149 处引用**，全按单选区语义写；反转态还是外挂的 `selection_reversed: bool`（`state.rs:571`），无 anchor/head 分离。**这是唯一"改数据结构"而非"加功能"的缺口** |
| 2.2 | **项目级搜索** | `crates/search`(11,973) | `project_search\|ripgrep\|WalkDir\|globset` → **0**；侧栏"搜索" tab 是**占位符**（`components/macos_sidebar/file_tree.rs:419-425` 只渲染一行文字"搜索"）。**前置缺口**：无全量文件遍历层——现有 `read_dir` 是逐目录懒加载（`file_manager/filesystem.rs:193/288`），且**不读 `.gitignore`**（`gitignore` 仅命中图标映射 `assets.rs:415`）。匹配引擎可复用（已有 `regex` + `aho-corasick`，`input/search.rs:99-251`） |
| 2.3 | **模糊文件查找器** | `crates/file_finder` + `picker` | `fuzzy` → **0**；无 nucleo/fuzzy-matcher 依赖。只有文件树，无按路径模糊跳转 |
| 2.4 | **编辑器右键菜单** | `mouse_context_menu.rs`(400) + `code_context_menus.rs`(2,040) | `state.rs:3400-3402` 右键直接 `return`；ContextMenu 设施只用于 tab 栏/文件树/live markdown |
| 2.5 | **语法级扩选/缩选** | `SelectLargerSyntaxNode` 等 | `SelectLargerSyntaxNode\|SelectInsideDelimiters\|SelectEnclosingSymbol` → **0**。语法树已在手（`simple.rs:467 tree()`），纯缺功能 |
| 2.6 | **选择下一个/全部出现** | `SelectNext`/`SelectAllMatches` | → **0**（仅同词高亮渲染，不可选中） |
| 2.7 | **编辑器状态跨会话持久化** | `editor/persistence.rs:103-320`（SQLite：selections/folds） | `FileTreeEditorState` 无 serde（`editor.rs:266-273`），只在同进程内切组时换手；`session_state.json` 的 `PanelContentSessionState` **只有 `Terminal` 变体**。重启即丢光标/滚动/折叠 |
| 2.8 | **保存期语义** | `format_on_save` / trim / final newline | `format_on_save\|trim_trailing\|insert_final_newline` → **0**；无 rustfmt/prettier |
| 2.9 | **导航历史 前进/后退** | `pane.rs:470-483` NavHistory | `nav_history\|GoForward\|back_stack` → **0** |
| 2.10 | **Vim 模式** | `crates/vim`(47,384) | 10 处 "vim" 全是终端注释/测试 |
| 2.11 | **Snippets / tab stop** | `crates/snippet` | 唯一命中是注释（`markdown_live/view.rs:1635`） |
| 2.12 | **内联 git blame** | `editor/src/git/blame.rs`(1,298) | `blame` 全仓（含非 rs）→ **0** |
| 2.13 | **书签** | `editor/src/bookmarks.rs`(402) | `bookmark` 仅命中 emoji 表（`markdown_live/emoji.rs:58`） |
| 2.14 | **任务系统 / runnable 检测** | `crates/task`(3,003) + `tasks_ui`(2,038) | `runnable\|task_template\|TaskTemplate` → **0**（`task` 命中全是 `gpui::Task` 异步句柄） |
| 2.15 | **编辑器标签页关闭不可撤销** | `ReopenClosedItem` 覆盖编辑器 item | `closed_tabs` 只收 `SplitContainer`（终端 panel），`ClosedTab` 结构无编辑器字段（`workspace.rs:247-256`）；`close_open_path`（`editor.rs:3373`）无撤销栈。**终端 tab 可 Cmd+Shift+T 恢复，编辑器 tab 不能** |
| 2.16 | **文件树内无过滤框** | 文件查找器即入口 | `grep -n "InputState\|EditorInput" src/sidebar_view/file_tree.rs` → **0**；1833 行文件树没有任何过滤输入框 |
| 2.17 | **无标签切换器（Ctrl-Tab）** | `crates/tab_switcher`(896) | `TabSwitcher\|tab_switcher` → **0**；只有 `cmd-shift-[/]` 顺序轮换（`workspace.rs:111-118`） |
| 2.18 | **无最近项目 / 欢迎页** | `crates/recent_projects`(8,737) | `recent_projects\|RecentProjects\|welcome` → **0** |
| 2.19 | **无「编辑器→终端发送文本」** | `terminal_view::SendText` | `SendText\|send_text` → **0**（反方向「终端→编辑器」路径跳转是有的） |
| 2.20 | **无行/选区变换命令** | `actions.rs:449-477, 505, 738-850` | `SortLines\|UniqueLines\|ReverseLines\|ShuffleLines\|ToggleCase\|ConvertTo*Case\|ConvertIndentationTo*\|DuplicateSelection\|NewlineAbove\|NewlineBelow\|Transpose\|AutoIndent\|Rewrap` → **各 0**。有移动/复制/删除行、行注释、join lines |
| 2.21 | **无光标定位命令** | `ScrollCursorTop/Center/Bottom/CenterTopBottom`、`HalfPageUp/Down`、`LineUp/Down`、`NextScreen`、`SelectLine`、`SaveLocation` | → **各 0**。内部已有 `scroll_to_center`（`state.rs:3611`）但无 action/键位 |
| 2.22 | **无 Git hunk 导航** | `GoToHunk`/`GoToNextChange`/`GoToPreviousChange` | → **0**（有 hunk 展开，但不能在 hunk 间跳转） |
| 2.23 | **无显示开关类 action** | `ToggleSoftWrap`/`ToggleLineNumbers`/`ToggleRelativeLineNumbers`/`ToggleIndentGuides`/`ToggleDiagnostics`/`ToggleBreadcrumb`/`ToggleGitBlameInline` | → **0**。`set_soft_wrap`/`set_line_number`/`set_show_whitespaces`/`set_cursor_shape` 等 setter 在编辑器侧**无调用点** |

---

## 3. P2 — 体验与可定制面

**编辑器内已实现但用户够不到**（builder/setter 在 `state.rs` 之外零调用点，实测）：

| builder / setter | 外部调用点 |
|---|---|
| `tab_size` / `indent_guides` / `show_whitespaces` / `rainbow_brackets` | 0 |
| `occurrences` / `scroll_beyond_last_line` / `cursor_surrounding_lines` / `sticky_scroll` | 0 |
| `git_gutter` / `scrollbar` / `soft_wrap` / `folding` | 0 |
| `set_soft_wrap` / `set_line_number` / `set_show_whitespaces` / `set_cursor_shape` / `set_cursor_blink` | 0 |

代码编辑器构造只写 `InputState::new(window, cx).code_editor(lang).multi_line(true)`
（`editor/editor.rs:2626-2628`），**上述全部走默认值、用户无法调整**。状态栏的 `"缩进: Tab"`
是硬编码字符串（`editor.rs:1505`），而 `TabSize` 默认是 `HARD_TABS` 4 列
（`input/indent.rs:19-24`，与 Zed 默认空格不同）。

> **这是横切问题**：在补任何显示类能力之前，应先落一层最小设置系统，否则每加一项都要接线两次。

**其余缺口**

| 能力 | 证据 |
|---|---|
| 编辑器字体不可配（仅终端字体可配） | `buffer_font\|editor_font` → 0；编辑器用 `window.text_style().font()`（`element.rs:301`）继承 UI 字体。fallback 平台硬编码（`font_settings.rs:150-214`），无连字 |
| 主题不可导入 | 31 个主题 `include_str!` 编译期嵌入（`theme/mod.rs:653`）；`read_dir\|themes_dir\|user_theme` → 0 |
| 折叠仅缩进启发式 | `display_map/folding.rs:21` 硬编码 `["}", "]", ")"]`；无语法树 fold query、无 `FoldRecursive`/`FoldFunctionBodies`、无折叠持久化；**层级折叠只到 7 级**（Zed 到 9） |
| 折叠缺 action | `FoldRecursive`/`UnfoldRecursive`/`FoldFunctionBodies`/`UnfoldLines`/按 buffer 折叠 → **各 0**（`unfold_lines` 是私有 fn，无 action） |
| 滚动条无标记 | 悬浮滚动条可拖拽（`element.rs:3705-3738`），但 `ScrollbarGeometry` 只有 4 个几何字段，无 diagnostics/git/search/cursor 标记 |
| 无 minimap | `minimap` → 0。**注意：Zed 确实有 minimap**（`editor_settings.rs:107 struct Minimap`、`element.rs:1352 layout_minimap`、`ToggleMinimap` action） |
| 无相对行号 / 无行尾符显示 | → 0。行尾**处理**正确（`state.rs:2363` 按行探测 LF/CRLF），但无指示/切换 |
| 无中键粘贴 / 无拖放选区 / 无滚轮缩放 | `middle_click_paste\|drag_and_drop_selection\|mouse_wheel_zoom` → 0 |
| 无选区外观配置 | `rounded_selection\|selection_highlight` → 0；选区色硬编码主题色（`element.rs:2932`） |
| 查找条缺选项 | 有正则/大小写/全字/替换（`input/search.rs:189/737/774`），缺**选区限定搜索**（`ToggleSelection` → 0）、**搜索历史**（`SearchHistory` → 0）、include-ignored、one-match-per-line、backwards |
| 替换无预览/确认 | `replace_all` 直接批量写回（`input/search.rs:774-812`），无预览层（有 undo 兜底） |
| 代码侧无大纲 | live markdown 有（`markdown_live/view.rs:4057-4068`），代码编辑器没有 |
| 无面包屑 | `breadcrumb` → **0** |
| 无 LSP document highlights | `document_highlight` → 0；同词高亮是本地扫描（仅光标空选区时生效，`state.rs:1624`，上限 2MB/2000 条） |
| 无预览标签页 / 无固定标签页 | `is_preview\|pinned` → 0；文件树单击即常驻打开 |
| 无 zen mode / 无 dock / 全屏无动作绑定 | `zen_mode\|ToggleFullScreen` → 0；标题栏样式硬编码 `WindowDecorations::Client` |
| 无标签自动闭合 / JSX / Emmet | `jsx_tag\|emmet\|rename_tag` → 0；只有全局 6 对硬编码 `AUTO_PAIRS`（`state.rs:1110-1117`），无按语言开关 |
| 无未配对括号指示 | `unmatched_bracket\|bracket_error` → 0 |
| 无扩展系统 | 无 wasmtime/wasmparser；仅从 zed 仓库引 gpui 系列 |
| **无障碍为零** | `Role::\|aria_label\|aria_role` → **0**（Zed 452 处）→ macOS VoiceOver 完全不可用 |
| 无 dead keys 处理 | `dead_key` → 0（影响欧洲键盘布局） |
| 无 telemetry / 自动更新 | → 0 |

**Git 侧缺口**（`git_panel.rs` 2,774 行已具备 提交/amend/文件级+**hunk 级**暂存/丢弃/贮藏 push-pop/冲突三选一/ahead-behind 徽标）：

| 缺失 | 证据 |
|---|---|
| 分支切换/创建 UI | `switch_branch\|checkout_branch\|branch_picker` → 0（只有只读 `branch_status()`，`git_repository.rs:968`） |
| fetch / pull / push | 仅出现在 `#[cfg(test)]` 测试里（`git_repository.rs:2158-2224`） |
| 历史 / 日志图 | `reflog\|git_log\|log_view` → 0 |
| worktree | 无 |
| stash 仅 push/pop | 无 list/apply/drop（`stash_list\|stash_apply\|stash_drop` → 0） |
| 无冲突解决视图 | 只有右键三选一（`git_panel.rs:1145-1192`） |
| **hunk 级取消暂存** | `stage_hunk` 在非 Worktree 目标直接返回（`diff_view.rs:520`），staged 页只读（`:422`）。Zed 的 `ToggleStaged` 是双向的 |
| 面板无排序/分组/树视图 | 只有分段（暂存/更改），`SetSortBy\|GroupBy\|TreeView` → 0 |

**根因**：`GitRepository` 只封装了**本地索引与工作区操作**，没有任何远端与引用遍历命令。

---

## 4. 已对齐，不要误报为缺失

- **IME 组合输入**：4 处 `EntityInputHandler` 实现（`state.rs:4447`、`markdown_live/view.rs:4395`、
  `terminal_view.rs:4604`、`keychain_editor.rs:144`），含中文 IME 测试。质量不低。
- **编辑器内查找/替换**：`code_editor()` 默认 `searchable=true`/`replaceable=true`
  （`state.rs:885-886`），有正则、大小写、全字、replace_all。只有 `PlainText` 模式默认关。
- **撤销/重做**：计时器分组（1s），undo 恢复光标与选区，`break_group()` 语义边界。
- **括号匹配高亮 + 彩虹括号 + 自动配对 + 智能缩进 + 行注释 + 移动/复制/删除行 + 折叠 + sticky scroll + git gutter（含 hunk 展开）+ 主题选择 UI（搜索 + ANSI 预览 + 持久化）+ 多窗口 + 分屏 + 终端 tab 恢复 + 标签拖拽重排 + 跳转到行列（含 `行:列` 解析）**：均已存在。
- **终端→编辑器路径跳转**：`file_link_detector.rs` 检测 `file:line:col` 并可跳转。
- **工作区持久化工程质量高**：原子写 + `.bak` 回退 + 旧路径迁移（`workspace/persistence.rs:109-193`）——问题只在**范围**（只存终端树），不在实现。
- **宏录制**：Zed 也没有（`RecordMacro|ReplayMacro` 全仓 0），只有 vim 的 `q`/`@` 与 `.` 重复。
  **记为"双方均缺"，不是 deepterm 落后。**
- **RTL/bidi**：Zed 也基本没有。
- **`cmd-[` / `cmd-]` 已被占用**：deepterm 用它们做编辑器缩进/反缩进（`input/state.rs:305/309`），
  与 Zed 的导航前进/后退冲突。补 2.9 时需重新分配键位。

---

## 5. 建议补齐顺序

**第 0 步（前置，其余全部依赖）**
建立 `settings.json`（user + project 两层）+ keymap 文件 + action 按名派发注册表。
这三者互相咬合，且是"用户可配"的唯一入口。

**第 1 步（不依赖 LSP，收益最直接）**
1. **诊断数据源**（把空壳 `DiagnosticSet` 填上）—— **投入产出比最高**：渲染管线已接通
   （`element.rs:2063`），补数据结构 + 任意产出方即可一次点亮 inline 高亮 / gutter 标记 /
   滚动条诊断标记 / go-to-diagnostic 四处。首个产出方可以先用 tree-sitter 的
   `ERROR`/`MISSING` 节点，零外部依赖。
2. **全量文件遍历层**（尊重 `.gitignore` 的并行 walker）—— 项目级搜索与模糊查找器的共同前置。
   做完后：
3. 项目级搜索（侧栏"搜索" tab 已是占位符，UI 骨架可复用 `builtin_commands/overlay_ui.rs`）
4. 模糊文件查找器 + 命令面板（命令面板依赖第 0 步的注册表）
5. 编辑器右键菜单（复用已有 `menu/context_menu.rs`，代码编辑器主体目前零使用）
6. 语法级扩选/缩选（语法树已在手，纯功能层）
7. 编辑器状态持久化（光标/滚动/折叠）+ 编辑器 tab 纳入 `closed_tabs` 撤销栈
   —— 落盘机制已完备（`persistence.rs:111 atomic_write` + `.bak` + 损坏恢复测试），
   只需给 `EditorOpenFileInfo` 加字段并接进 `AppSessionState`

**第 2 步（结构性，工作量最大）**
1. **多选区模型重构**：`Selection` 升级为 `{ anchor, head, id }` + `Vec<Selection>` 容器，
   再逐点改成"对每个选区做一遍"。**改动面 149 处引用**，是唯一"改数据结构"而非"加功能"的缺口。
   `AddSelectionAbove/Below`、`SelectNext`、列选择、`SelectAllMatches`、多光标渲染、
   多选区 undo 全部依赖它。
2. buffer 身份 + 版本号 + 跨文件注册表 + **Anchor 锚点**，然后才是 LSP 客户端。
   **这一步不做，LSP 接上去会产生错位编辑。**
   （多缓冲区 excerpt 模型应与本步同步设计——它是搜索结果/诊断列表/引用列表的统一渲染载体。）

**第 3 步（依赖 LSP）**
补全 → hover → 跳转/引用 → 重命名 → 代码动作 → 格式化 → 语义 token / inlay hints / code lens
/ document highlights。

**可并行、互不阻塞**
保存期语义（format-on-save / trim / final newline）、移动语义接线（段落除外，subword 算法已有
只需 action 化）、折叠增强（语法树 fold ranges + 递归/函数体 + 8/9 级）、行变换命令
（排序/去重/反转/大小写）、Vim、snippets、编辑器字体可配、主题可导入、
无障碍（`Role::`/`aria_*`）、内联 blame（`GitRepository` 骨架已在，成本低感知强）、
分支切换 → fetch/pull/push → 历史图、预览/固定标签页、最近项目、标签切换器。

---

## 6. 核验方法

- 全部"缺失"结论均以精确 `grep -rniE` 在 `deepterm/src`（Rust 源码）全量搜索，
  确认 0 命中或仅无关命中；无关命中已显式列出（如 `vim` → 终端注释、`snippet` → 文档注释、
  `settings.json` → agent hooks 里 Claude/Qoder 的配置、`worktree` → Claude 的目录名、
  `Transpose` → `Iterator::transpose`、`Format` → `std::fmt::Formatter`/kitty 图像格式、
  `paragraph` → markdown 渲染、`rename` → 文件系统重命名、`reference` → Rust 借用）。
- 全部"存在"结论均给出 `文件:行`。
- 四路并行子代理（编辑器核心 / 语言智能 / 项目导航 / 编辑 UX）独立取证，关键结论由主代理二次复核。
- **未做**：未编译、未运行、未构造最小复现。所有结论为静态代码审计。

**两处对基线前提的修正**（审计过程中发现）：
1. **Zed 确实有 minimap** —— `editor_settings.rs:107 struct Minimap`、
   `element.rs:1352 layout_minimap`、`editor.rs:1143 minimap: Option<Entity<Self>>`、
   `actions.rs:836 ToggleMinimap`。deepterm 没有。
2. **`crates/text/src/history.rs` 不存在** —— Zed 的文本历史内联在
   `crates/text/src/text.rs`（`HistoryEntry:127`、`Transaction:135`、`struct History:160`、
   `start_transaction:1313`、`undo_to_transaction:1372`），编辑器侧选区历史在
   `crates/editor/src/editor.rs:7498` 的 `selection_history`。

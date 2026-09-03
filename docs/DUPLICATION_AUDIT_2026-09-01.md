# DeepTerm 重复/冗余代码审计报告

- 审计日期：2026-09-01
- 范围：`src/**/*.rs`，146 个文件，83,923 行，解析出 **3,260 个函数**
- 方法：自研扫描器（`.workbuddy/dupscan*.py`），四轮检测
  1. 函数体精确哈希（去注释/字符串归一化）
  2. 函数体结构归一化（遮蔽标识符、字面量，只留语法骨架）
  3. 10 行滑窗跨文件块匹配 + 重叠区间合并
  4. 20 行大窗口长块匹配
- 所有结论均已**逐处打开源码人工复核**，误报单独列出（见 C 类）

---

## 结论速览

| 等级 | 含义 | 条目数 | 估算冗余行数 |
|---|---|---|---|
| A | 真实逻辑重复，建议重构 | 9 | ~290 |
| B | 样板 / UI 复制粘贴，量大 | 5 | ~300 |
| C | 低价值或误报，不建议动 | 9 | — |

---

## A 类：真实逻辑冗余（建议重构）

### A1. `watcher.rs` — 两个 `impl WatcherContext` 逐字节重复 ★最高优先级
**文件**：`src/file_manager/watcher.rs:613-651` 与 `:653-691`
**冗余**：约 **78 行**（8 个方法 × 2 份，函数体完全一致）

`impl WatcherContext for App` 与 `impl<'a, T> WatcherContext for Context<'a, T>` 的 8 个方法体完全相同：

| 方法 | App 版 | Context 版 |
|---|---|---|
| `fs_watcher` | :614 | :654 |
| `remote_fs_watcher` | :618 | :658 |
| `watch_path` | :622 | :662 |
| `watch_recursive_path` | :627 | :667 |
| `unwatch_path` | :632 | :672 |
| `unwatch_recursive_path` | :637 | :677 |
| `watch_remote_path` | :642 | :682 |
| `unwatch_remote_path` | :647 | :687 |

8 个方法全部是 `let watcher = self.fs_watcher(); watcher.update(...)` 的三行同构体。

**修法**：在 `trait WatcherContext`（:602）中为后 6 个方法提供 default method，两个 impl 只保留 `fs_watcher` / `remote_fs_watcher` 两个真实差异点。

---

### A2. `check_server_key` 跨文件完全重复
**文件**：`src/ssh/russh_terminal.rs:43-53`、`src/ssh/sftp_fs.rs:68-78`
**冗余**：11 行 × 2

两个文件的函数体逐字相同：
```rust
crate::ssh::host_key::check_and_trust_host_key(
    &self.host, self.port, server_public_key, self.host_key_policy,
)
```
**修法**：`host_key` 模块已有实现，两处都改为直接调用；或抽一个 `HostKeyChecker` trait。

---

### A3. `sftp_fs.rs` — `block_on` 异步包装器重复 22 处
**文件**：`src/ssh/sftp_fs.rs`
**冗余**：约 60 行

`(1)` **完全同形的 5 处**（只差被调用的 sftp 方法名）：

| 方法 | 行号 |
|---|---|
| `canonicalize` | :117-124 |
| `exists` | :168-175 |
| `rename` | :201-208 |
| `remove_file` | :210-217 |
| `remove_dir` | :219-226 |

全部是：
```rust
crate::ssh::shared_runtime::shared_ssh_runtime().block_on(async {
    self.sftp.<METHOD>(path).await.map_err(anyhow::Error::from)
})
```

`(2)` `metadata`（:126-136）与 `symlink_metadata`（:140-150），11 行，仅 `metadata` / `symlink_metadata` 一词之差。

**修法**：
```rust
fn sftp_call<T, F>(&mut self, fut: F) -> anyhow::Result<T>
where F: Future<Output = Result<T, russh_sftp::Error>>
{ shared_ssh_runtime().block_on(fut).map_err(anyhow::Error::from) }
```
或用一个 `sftp_call!` 宏，5 处缩成 5 行。

---

### A4. `terminal.rs` — 6 个滚动方法结构完全相同
**文件**：`src/terminal/terminal.rs:2387-2435`
**冗余**：约 40 行

| 方法 | 行号 | 唯一差异 |
|---|---|---|
| `scroll_line_up` | :2387 | `AlacScroll::Delta(1)` |
| `scroll_line_down` | :2395 | `AlacScroll::Delta(-1)` |
| `scroll_page_up` | :2403 | `AlacScroll::Delta(lines)` |
| `scroll_page_down` | :2412 | `AlacScroll::Delta(-lines)` |
| `scroll_to_top` | :2421 | `AlacScroll::Top` |
| `scroll_to_bottom` | :2429 | `AlacScroll::Bottom` |

6 个方法的尾部三行**完全一致**：
```rust
drop(term);
self.content_dirty = true;
self.last_scroll_time.set(Instant::now());
```
`set_display_offset`（:2438）也复制了同一段尾部。

**修法**：
```rust
fn scroll_by(&mut self, scroll: AlacScroll) {
    self.term.lock().scroll_display(scroll);
    self.content_dirty = true;
    self.last_scroll_time.set(Instant::now());
}
```
6 个方法各缩成 1 行调用。（历史 `CODE_REVIEW.md:142` 已标记过此项，尚未收敛。）

---

### A5. `history.rs` — `undo` / `redo` 镜像重复
**文件**：`src/components/input/history.rs:161-176` 与 `:179-194`
**冗余**：16 行

两个 16 行函数仅 `undos` / `redos` 两个字段名互换，逻辑完全对称。

**修法**：抽 `fn pop_group(from: &mut VecDeque<I>, to: &mut VecDeque<I>) -> Option<Vec<I>>`，两者各缩成 1 行。

---

### A6. `split_manager.rs` — 两个完全相同的递归函数（死重复）
**文件**：`src/workspace/split_manager.rs:180-189` 与 `:198-207`
**冗余**：10 行

`collect_panels`（被 `collect_panels_vec` 前的旧路径使用）与 `collect_panels_vec_recursive` 是**签名与函数体完全相同的两份实现**：
```rust
match self {
    SplitContainer::Panel(panel) => panels.push(panel.clone()),
    SplitContainer::Split { children, .. } => {
        for child in children { child.<SELF>(panels); }
    }
}
```
**修法**：直接删除其一，统一调用点。（`collect_panels_vec`:191 是 `collect_panels_vec_recursive` 的包装器，保留。）

---

### A7. `builtin_commands/mod.rs` — 三个 clamp 索引函数同构
**文件**：`src/builtin_commands/mod.rs:231-237`、`:988-996`、`:997-1005`
**冗余**：14 行

`history_selected_index` / `selected_index` / `selected_path_index` 结构完全一致：
```rust
let total = self.<LIST>.len();
if total == 0 { return None; }
self.<IDX>.map(|i| i.min(total - 1))
```

**修法**：`fn clamp_index(list_len: usize, idx: Option<usize>) -> Option<usize>`。

---

### A8. `file_manager` — 递归目录传输循环体跨 3 文件重复
**文件**：
- `src/file_manager/remote_ops.rs:169-237`（`copy_remote_dir_to_remote_path`）
- `src/file_manager/upload_download.rs:275-335`（`download_remote_dir_to_local_path`）
- `src/file_manager/cross_remote.rs:1048+`（`copy_remote_dir_between_sessions`）

**冗余**：约 40 行（核心循环块，各约 20 行）

三处共享同一段结构：冲突策略解析 → `Some(path)` 分支递归 → `None` 分支 `TransferSummary::one_skipped()` → `else` 分支 `log::warn!("跳过不支持的远程条目…")`。

其中 `remote_ops.rs:218-236` 与 `upload_download.rs:316-334` 几乎逐行相同（连注释都只差几个字）。

**修法**：把「遍历条目 → 判定支持性 → 冲突解决 → 汇总」抽象成一个泛型 `transfer_dir(entries, resolve, copy_one)`。

---

### A9. `git_panel.rs` — `stage_file` / `unstage_file` 26 行重复
**文件**：`src/sidebar_view/git_panel.rs:456-481` 与 `:483-508`
**冗余**：26 行

仅两处差异：`repo.stage(&[..])` vs `repo.unstage(&[..])`，以及 `GitPanelEvent::StageFile` vs `UnstageFile`。

**修法**：抽 `fn run_git_op(&self, path, op: fn(&Repo, &[&str]) -> Result<..>, event: fn(String) -> GitPanelEvent, cx)`。

---

## B 类：样板 / UI 复制粘贴（量大，建议用宏或公共组件收敛）

### B1. `impl Focusable` 样板 — 18 处完全相同 ★量最大
**冗余**：约 72 行

```rust
impl Focusable for <T> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
```

18 处位置：

| 文件 | 行 | 文件 | 行 |
|---|---|---|---|
| `components/macos_select.rs` | 271 | `terminal_view/terminal_view.rs` | 2580 |
| `workspace/panel.rs` | 630 | `sidebar_view/file_tree.rs` | 1094 |
| `editor/image_viewer.rs` | 130 | `sidebar_view/folder_dialog.rs` | 98 |
| `workspace/workspace.rs` | 2418 | `sidebar_view/keychain_view.rs` | 159 |
| `workspace/split_manager.rs` | 542 | `sidebar_view/font_view.rs` | 251 |
| `ssh_view/picker.rs` | 617 | `sidebar_view/host_view.rs` | 968 |
| `workspace/tab_bar.rs` | 699 | `sidebar_view/keychain_editor.rs` | 119 |
| `components/input/state.rs` | 4517 | `sidebar_view/keychain_editor.rs` | 623 |
| `sidebar_view/host_form.rs` | 673 | `sidebar_view/git_panel.rs` | 669 |

**修法**：`macro_rules! impl_focusable { ($t:ty) => { ... } }`，18 处各缩成 1 行。

---

### B2. GPUI `Element` trait 样板三件套
**文件**：`src/components/input/element.rs`、`editor/editor.rs:2922+`、`editor/image_viewer.rs:181+`、`editor/markdown_selectable.rs:194+`、`workspace/workspace.rs:2499+`、`sidebar_view/file_tree.rs:1258+`、`terminal_view/terminal_element.rs:1102+`、`terminal_view/terminal_scrollbar.rs:118`、`components/menu/context_menu.rs:147`、`components/macos_sidebar/resize.rs`
**冗余**：约 60 行

- `fn source_location(...) -> ... { None }` — **10 处**，其中 6 处与 `request_layout` 连成 10 行整块重复
- `fn request_layout(...)` — **10 处**，单行版 `(window.request_layout(gpui::Style::default(), [], cx), ())` 在 resize.rs:15-22 / editor.rs:2926 / workspace.rs:2508 / image_viewer.rs / file_tree.rs / terminal_element.rs:1103 重复出现
- `fn prepaint` — 10 处

**修法**：抽一个 `struct PassthroughElement(AnyElement)` 统一提供默认 `source_location` / `request_layout`，或提供 `default_element_impl!` 宏。

---

### B3. 侧边栏「列表行 + 选中/悬停态」builder 链
**文件**：`components/macos_sidebar/content.rs:333/456`、`components/macos_sidebar/file_tree.rs:311`、`sidebar_view/host_view.rs:461/494/728/1300`、`sidebar_view/keychain_view.rs:208/241/469/434`、`sidebar_view/theme_view.rs:34/127`、`sidebar_view/file_tree.rs`
**冗余**：约 80 行

重复模式（约 10 行/处，跨 5+ 文件出现 7 次以上）：
```rust
.when(is_x, |this| this.bg(theme.accent).text_color(theme.accent_foreground))
.when(!is_x, |this| this.hover(|style| style.bg(hover_bg(theme))).text_color(theme.text))
```

**修法**：抽 `fn selected_row(theme, is_selected) -> Div` 或 `fn row_state(theme, is_active) -> impl Fn(Div) -> Div`。

---

### B4. 圆形图标按钮 builder 链 — 7 处跨 5 文件
**位置**：`components/macos_sidebar/content.rs:82/461`、`components/macos_sidebar/file_tree.rs:316`、`sidebar_view/host_view.rs:466/499`、`sidebar_view/keychain_view.rs:213/246`、`sidebar_view/theme_view.rs:39/174`
**近似变体**：`workspace/tab_bar.rs:1118`、`workspace/transfer_bell.rs:249`、`editor/diff_view.rs:1322/1407`
**冗余**：约 70 行

重复模式（`button_bg(theme)` 共 7 次）：
```rust
.w(px(28.0)).h(px(28.0)).flex().items_center().justify_center()
.rounded(px(999.0))
.bg(button_bg(theme))
.cursor_pointer()
.hover(|style| style.bg(button_bg_hover(theme)))
.child(gpui::svg().size(px(8.0)).path(Icon::X.path()).text_color(theme.text))
```

**修法**：`fn round_icon_button(theme: UiTheme, icon: Icon) -> Div`。

---

### B5. `Focusable` + `Render` 开头样板组合
**文件**：`components/macos_select.rs:269`、`editor/image_viewer.rs:128`、`sidebar_view/folder_dialog.rs:96`、`sidebar_view/font_view.rs:249`、`sidebar_view/host_form.rs:671`、`sidebar_view/host_view.rs:966`、`sidebar_view/keychain_editor.rs:117/621`、`sidebar_view/keychain_view.rs:157`、`ssh_view/picker.rs:615`、`workspace/split_manager.rs:540`
**冗余**：约 40 行

`impl Focusable` 紧接 `impl Render`（开头统一 `let theme = cx.theme().ui;`）的 10 行组合块重复 11 次。属于 B1 的子集，随 B1 一并收敛。

---

## C 类：低价值 / 误报（列出但不建议动）

| # | 位置 | 说明 | 判定 |
|---|---|---|---|
| C1 | `button.rs`、`input/state.rs:1560-1640`、`modal.rs`、`stepper.rs`、`ssh/config.rs`、`menu_item.rs` 等 | builder setter 样板 `pub fn x(mut self, v: T) -> Self { self.x = v; self }`，约 100+ 处 | Rust 惯例，可用 `bon`/`typed-builder` 但收益有限 |
| C2 | `input/state.rs:46-105`、`sidebar_view/hosts_view.rs:7-21`、`input/element.rs:2469-2482`、`terminal_view/terminal_element.rs:1727-1738` | `actions!` 宏列表与长标识符列表 | **误报**（结构归一化后全塌成 `V, V, V`） |
| C3 | `file_icons.rs:13-94` vs `:100-409`、`terminal_view/box_drawing.rs:34-117`、`theme/mod.rs:32-70`、`editor/syntax/registry.rs` | 纯数据表（`map.insert` / `enc()` 常量表） | **误报**，数据而非代码 |
| C4 | `editor/encoding.rs:200/208`、`scp_completion.rs:541/552`、`path_resolver.rs:176/192`、`overlay_wrap.rs:468-513`、`syntax/simple.rs:977/1029` | 单元测试的镜像重复 | 测试重复通常可接受，可用 rstest 参数化收敛 |
| C5 | `builtin_commands/mod.rs:588-619` | 6 个 `move_cursor_*` 单行委托包装器 | 薄委托，可保留 |
| C6 | `components/macos_sidebar/content.rs:528-542` | `render_host_content` / `render_keychain_content` 6 行相同 | 可合并，但仅 2 处 |
| C7 | `file_manager/file_tree_cache.rs:373-392` | `is_dir_fresh` / `is_file_fresh` 结构相同 | 可合并，仅 2 处 |
| C8 | `components/stepper/mod.rs:26-43`、`components/spinner.rs:31-38` | 四分支 `match => px(N)` 尺寸表 | 数据表，可接受 |
| C9 | `ssh/platform.rs:124-159` | `display_name` / `to_id` 两个 17 行 13 分支 match 表 | 数据表，但可用 `&[(&str, &str)]` 静态表驱动 |

---

## 建议处理顺序

1. **A6（split_manager 死重复）** — 改动最小，纯删除，零风险
2. **A1（watcher 78 行）** — default method，收益最大
3. **B1 + B5（Focusable 18 处）** — 一个宏解决 ~110 行
4. **A4（terminal 6 个滚动方法）** — 历史已标记，本次给出具体形态
5. **A3（sftp 22 处 block_on）** — 宏/helper，风险低收益高
6. **A2、A5、A7、A9** — 各自 10-26 行，独立可提
7. **A8（file_manager 三文件）** — 涉及传输主流程，需单独立项 + 回归测试
8. **B2、B3、B4** — UI 样板，建议与侧边栏组件重构一并做

---

## 复现方法

扫描脚本保留在 `.workbuddy/`：
```bash
PY=/Users/zhousong/.workbuddy/binaries/python/versions/3.13.12/bin/python3
$PY .workbuddy/dupscan.py    # 精确重复 + 跨文件同名函数
$PY .workbuddy/dupscan2.py   # 结构归一化（函数级 + 10 行滑窗）
$PY .workbuddy/dupscan3.py 20  # 20 行大窗口长块
```

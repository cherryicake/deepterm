# deepterm 代码审查报告

- 日期：2026-08-28
- 范围：`src/` 全部 167 个 Rust 文件 / 78,322 行
- 基线：`main` @ `28be09a`，工作区干净
- 手段：`cargo check`（15 warning）+ `cargo clippy`（271 warning）+ 人工热点扫描 + 1 个临时探针测试（已撤销）

---

## 修复进度（同日更新）

P0、P1 全部修完，P2 清理项除两项较大重构外也已处理。改动 **15 个文件，+183 / -231**，
新增 `src/ssh/host_key.rs`。

| 项目 | 状态 |
|---|---|
| P0-1 SFTP 会话池失效 | 已修 |
| P0-2 SFTP 主机密钥校验缺失 | 已修（新增共用模块） |
| P0-3 git 面板 stage/unstage 未接线 | 已修（新增 `GitIndexChanged` 事件） |
| P1-1 diff 尾部折叠取错行 | 已修 + 补行序回归测试 |
| P1-2 `with_cached_session` 变量遮蔽 | 已修 |
| P1-3 高亮合并全量重排 | 已修 |
| P2 死代码与冗余逻辑 | 5 / 7 已修，另 2 项重构待定 |

验证：**409 个测试全部通过**；`cargo check` 警告 15 → **0**；clippy 271 → 253。
改动尚未提交，等待审阅。

### 实施中值得记录的判断

- **P0-1 与 P1-2 必须同批修**：单独修好会话池后，P1-2 的遮蔽 bug 会把刚判定为坏的
  连接又塞回池里。
- **P1-3 只能省跨段重排**：`syntax/simple.rs` 的 `styles()` 返回**按起点排序但可能重叠**
  的区间（tree-sitter captures 天然嵌套），段内归并不能省。
- **跨平台代码不能照当前平台的警告删**：`ssh_view/picker.rs` 的 `is_command_available`
  在 macOS 上报「never used」，实际被 `#[cfg(windows)]` 的终端扫描调用，应加条件编译
  而非删除。
- `DiffFile::build` 与 `build_unified_collapsed` 各有一个同名 `old_line_count`，
  前者仍在使用，删前须在函数内确认。

---

## 结论速览

| 级别 | 数量 | 说明 |
|---|---|---|
| P0 | 3 | 功能失效或安全风险，建议尽快修 |
| P1 | 3 | 逻辑错误 / 性能退化，影响体验 |
| P2 | 7 | 死代码、冗余逻辑、可维护性 |

编译零错误。生产代码的 panic 风险极低、命令注入防护到位、远程 git 已正确后台化 —— 这几处做得好，不要回退。

---

## P0-1　SFTP 会话池完全失效，每次操作都新建 SSH 连接

**位置**：`src/ssh/sftp_fs.rs:746-948`

`CacheEntry` 从来没有被引擎创建过。`checkin_session` 只在 entry 已存在时才 push：

```rust
// sftp_fs.rs:907
fn checkin_session(key: &str, session: SharedBlockingSftpSession) {
    let mut cache = session_cache().lock();
    if let Some(entry) = cache.get_mut(key) {   // entry 不存在 → 直接丢弃
        if entry.idle.len() < MAX_IDLE_SESSIONS_PER_HOST {
            entry.idle.push(session);
        }
    }
}
```

而全文件（全仓）不存在任何 `cache.insert(...)` 或 `cache.entry(key).or_insert(...)` —— 已用
`rg 'or_insert|entry\('` 确认无命中。`get_or_connect_session` 在 903 行新建连接时也明确不入池：

```rust
// 新建连接：直接交给调用方独占使用（不入池）。
Ok(Arc::new(Mutex::new(BlockingSftpSession::connect(config)?)))
```

**后果**：`MAX_IDLE_SESSIONS_PER_HOST = 8` 的池永远是空的。每一次 `read_dir` / `metadata` /
`read_file_bytes` 都要重走 TCP 建连 + SSH 握手 + 认证。浏览远端目录树是 N 次 `read_dir`，
代价被直接放大 N 倍。第 752 行注释宣称的「UI 的读目录 / 统计等高频小操作始终能复用池中会话」不成立。

**修法**：把 `checkin_session` 改成 `cache.entry(key).or_insert_with(...)`，让首次归还时创建条目。

---

## P0-2　SFTP 通道未做主机密钥校验（MITM 风险）

**位置**：`src/ssh/sftp_fs.rs:55-60`

```rust
struct SftpClientHandler;
impl client::Handler for SftpClientHandler {
    async fn check_server_key(&mut self, _server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)          // ← 无条件接受任何主机密钥
    }
}
```

同一个 crate 里的终端通道做了正确校验（`src/ssh/russh_terminal.rs:42-60`，走
`russh::keys::known_hosts::check_known_hosts` + accept-new 写入）。

**后果**：所有 SFTP 流量 —— 远端文件树、上传下载、远程文件编辑 —— 都不验证对端身份，
中间人可静默接管。终端侧有防护而文件侧没有，属于防护缺口而非有意设计。

**修法**：把 `russh_terminal.rs` 的 known_hosts 校验逻辑抽成公共函数，`SftpClientHandler` 复用。

---

## P0-3　Git 面板的 stage / unstage 按钮没有接线

**位置**：`src/sidebar_view/git_panel.rs:38-39`（定义）、`494`、`521`（emit）
**订阅方**：`src/components/macos_sidebar/file_tree.rs:133-146`

事件被 emit 了，但唯一的订阅方只匹配两个 OpenDiff 变体：

```rust
|this, _, event: &GitPanelEvent, cx| match event {
    GitPanelEvent::OpenDiff { path, diff, repo } => { /* ... */ }
    GitPanelEvent::OpenDiffLoading { path, repo } => { /* ... */ }
    // StageFile / UnstageFile 落空，编译器因非穷尽 match 也不会报错
}
```

**后果**：点击暂存 / 取消暂存没有任何反应。编译器没报警是因为 `match` 里没有通配分支却…
实际上是因为这两个变体带了未使用的 `path` 字段（clippy 的 `field path is never read` 正是这个信号的源头）。

**附带**：`GitPanelEvent::OpenDiff` 变体从未被构造（457 行只 emit `OpenDiffLoading`），是死代码，
可从枚举中移除。

---

## P1-1　Diff 视图尾部折叠：取错了行，占位条位置也错了

**位置**：`src/editor/diff_model.rs:417-436`（`push_gap` 的 `leading` 分支）

```rust
if leading {
    let visible = CONTEXT_LINES.min(gap_len);
    let hidden  = gap_len - visible;
    let hidden_id = new_from;
    if expanded.contains(&hidden_id) { push_context(.. new_from, new_to ..); return; }
    push_context(text, lines, new_text, new_starts, new_from + hidden, new_to, old_from + hidden);
    push_separator(text, lines, separators, hidden_id, hidden);
}
```

`new_from + hidden == new_to - visible`，即取的是 **gap 末尾**若干行；尾部 gap 的末尾就是文件末尾。
随后 `push_separator` 排在后面，占位条被渲染到文件最后一行的下方。

**实测确认**（临时探针测试，跑完已撤销，工作区保持干净）：

```
  0 SEP                       ← 头部折叠，正确
  1 C ctx16   new=16
  2 C ctx17   new=17
  3 C ctx18   new=18
  4 C ctx19   new=19          ← hunk 前 4 行上下文，正确
  5 - old line old=20
  6 + new line A new=20
  7 + new line B new=21
  8 C tail36  new=38          ← 错：这是文件末 4 行
  9 C tail37  new=39
 10 C tail38  new=40
 11 C tail39  new=41
 12 SEP                       ← 错：占位条跑到文件最后一行下面了
```

期望：8-11 行应为 `tail20..tail23`（紧贴最后一个 hunk 的上下文），SEP 在其后。

**修法**：`leading` 分支改为
`push_context(.. new_from, new_from + visible, old_from)`，再 `push_separator(...)`。

**测试缺口**：`collapsed_tests` 现有 5 个用例只断言 `separators` 的数量、id、
`hidden_lines` 和总行数，没有一行断言行序或行内容，所以漏掉了这个 bug。建议补一个行序断言。

---

## P1-2　`with_cached_session` 重试路径变量遮蔽，坏连接回池、新连接被丢

**位置**：`src/ssh/sftp_fs.rs:937-947`

```rust
let session = get_or_connect_session(config, &key)?;
let result = match f(&mut session.lock()) {
    Ok(result) => Ok(result),
    Err(_) => {
        evict_session(&key, &session);
        let session = get_or_connect_session(config, &key)?;   // ← 遮蔽了外层 session
        f(&mut session.lock())                                 // ← 这个新连接用完即丢
    }
};
checkin_session(&key, session);   // ← 归还的是外层那个已被 evict 的坏会话
```

**后果**：(a) 重试新建的（昂贵的）连接随遮蔽绑定出作用域被 Drop；(b) 已被判定为坏的会话被归还。
在 P0-1 修好之前 (b) 不会真正生效（池是空的），但两者必须一起修，否则修好池的当天就会把坏连接灌进去。

**修法**：用单一可变绑定承接重试结果，例如
`let mut session = ...; ... Err(_) => { evict_session(&key, &session); session = get_or_connect(...)?; f(&mut session.lock()) }`。

---

## P1-3　语法高亮合并：每个可见段都全量 clone + 重排

**位置**：`src/components/input/element.rs:1540-1556`

```rust
let mut flush_range = |start_line, end_line, skip, styles: &mut Vec<_>| {
    let range_styles = /* highlighter.styles(...) */;
    *styles = combine_highlights_stable(styles.clone(), range_styles);   // ← 每次全量
};
```

`combine_highlights_stable`（同文件 32-77 行）把所有 endpoint 收集后 `sort_by`，是 O(n log n)，
且 `styles.clone()` 每次拷贝整个累积结果。`flush_range` 按可见行的**连续段**调用，段数 = 1 +
超长行数量 + 折叠区间数量。新增的折叠功能（`acf4d6d`）会显著增加段数，退化到 O(K·N log N)。

各段严格递增且互不重叠，`combine` 在这里没有实际归并作用。

**修法**：改为线性 append（可选再做一次相邻同样式合并），把每段成本从 O(N log N) 降到 O(段长)。
同文件 1592 / 1608 行的 diagnostics 与 diff 着色合并是真实归并，保留即可。

---

## P2　清理项

| # | 位置 | 问题 |
|---|---|---|
| 1 | `editor/git_marks.rs:19` | `.max(if new_text.is_empty() { 0 } else { 0 })` 两分支同值，`max(0)` 对 usize 恒等 → 整个表达式等价于 `new_text.lines().count()` |
| 2 | `ssh/keychain.rs:595-601` | `certificate_path.is_some()` 分支与 `else` 都返回 `Russh`，证书判断形同虚设 |
| 3 | `components/input/element.rs:301-307` | 前两个分支同值，`visible_range.len() < BOTTOM_MARGIN_ROWS * 8` 是死条件 |
| 4 | `components/input/search.rs:145-151` | 前两个分支同值 |
| 5 | `theme/mod.rs:226-268` | `editor_fg` 等 6 个变量先包 `Some(..)` 再 `.unwrap()`，纯噪音；dark / light 两版 `SyntaxColors` 各约 45 行字面量几乎全同，可抽公共基底 + 覆盖表 |
| 6 | `workspace/workspace.rs:46`、`file_manager/file_tree_event.rs:6` | 枚举达 208 字节（内嵌 `SshConnectionConfig`：多个 `String` + `Option<Vec<String>>`）。事件 clone 频繁，建议 `Box` 化 |
| 7 | 死代码 | `git_panel.rs` 的 `status_label` 与 `OpenDiff` 变体、`ssh_view/picker.rs` 的 `decode_wsl_output` / `is_command_available`、`components/input/search.rs` 的 `on_action_search_previous` / `prev_scroll_direction`、2 个 unused import、4 个 unused variable |

**clippy 271 条警告**里 49 条是 `very complex type`（集中在 `workspace.rs` / `tab_bar.rs`），
抽 `type` 别名即可批量消掉；其余多为 `needless_borrow` / `useless_conversion` / `unit value`
之类的机械项，`cargo fix` 可自动处理大半。

---

## 审查中确认做得好的地方（请勿回退）

- **生产代码 panic 风险极低**：`unwrap` / `expect` / `panic!` 几乎全部落在 `#[cfg(test)]` 内。
  例：`builtin_commands/executor.rs` 40 处 `panic!` 全是测试断言，非测试代码 0 处。
- **无 shell 注入**：git 调用一律 `Command::new("git").args([...])` 传参，无字符串拼 shell。
- **远程 git 已正确后台化**：`editor/diff_view.rs` 走 `cx.background_executor()`；
  `editor/git_repository.rs:300-332` 用 `scope.spawn` 并行拉 HEAD / `git show` / stat。
- **本地与远端文件操作有统一抽象**：`file_manager/filesystem.rs` 的 `FileSystem` trait
  + 本地 / 远端两实现。`sftp_fs.rs` 的 bulk 通道与常规元数据通道分离，避免 request id 竞争。
- **测试覆盖不错**：468 个 test 标记，`diff_model` / `search` / `rope_ext` /
  `overlay_wrap` / `fold_map` 等核心模块齐备。
- **缓存有界**：`MAX_PERSISTED_DIRS_PER_HOST` / `MAX_CACHED_FILE_SIZE` / `MAX_ORPHAN_SERVERS` 均有淘汰。

---

## 建议的修复顺序

1. P0-1 会话池 `or_insert_with` （一行改动，收益最大）
2. P0-2 复用 known_hosts 校验
3. P1-2 `with_cached_session` 遮蔽（与 1 同批，避免坏连接回池）
4. P0-3 stage / unstage 接线
5. P1-1 diff 尾部折叠 + 补行序测试
6. P1-3 高亮合并改线性
7. P2 批量清理（`cargo fix` + 手工删死代码）

## 附注

- 上游依赖 `block v0.1.6`、`proc-macro-error2 v2.0.1` 触发 future-incompat 告警，
  来自 gpui 依赖树，当前无需处理，待上游更新。
- 增量跑单测约 15s：`cargo test --bin deepterm <test_name> -- --nocapture`，验证成本很低。

---

## Phase 3 修复记录（2026-08-28 第二轮）

前三项（theme 去重、大枚举装箱、clippy 批量清理）已全部完成：

### 大枚举装箱
- `WorkspaceEvent::OpenPath`、`FileTreeEvent::OpenFile`：`SshConnectionConfig`
  （184 字节）装箱，枚举 208 → 32 字节。
- 根因修复：`GitBackend::Remote { config }` 改为 `Box<SshConnectionConfig>`，
  `GitRepository` 208 → 32 字节，连带让 `SidebarEvent`（232 字节，比 clippy
  标记的两个枚举还大）瘦身。
- `sidebar_view/file_tree.rs`、`main.rs`、`macos_sidebar/mod.rs` 同步调整取值方式。

### clippy 清理（271 → 33）
- `cargo clippy --fix` 清掉全部机械项（needless_borrow / useless_conversion /
  let_unit_value / needless_return 等）。
- 新增 `src/callbacks.rs` 集中管理 GPUI 回调别名（ClickCallback / HoverCallback /
  StrCallback / AppCallback 等），并在各组件内补充局部别名，**49 处
  type_complexity 全部清零**。
- 其余手工修复：7 处 `let...else` → `?`、3 处 `sort_by_key`、3 处文档列表缩进、
  `rainbow_cache` 元组改 `RainbowCache` 结构体、`Language::from_str` 改名
  `from_id`（避免与 `std::str::FromStr` 混淆）、`StepperItem::is_last` 改名
  `last`、`split_manager::collect_panels` 移除纯透传的 `cx` 参数等。

### 验证
- `cargo test --bin deepterm`：**409 passed / 0 failed**。
- `cargo check`：无错误、无警告。
- clippy 剩 33 条，全部为架构级遗留项（25 条 `too_many_arguments`、
  8 条 `module_inception`），需引入参数结构体 / 重命名模块，改动面大，
  单独立项处理。

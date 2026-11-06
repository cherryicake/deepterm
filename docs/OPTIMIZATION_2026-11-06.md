# 优化记录（2026-11-06）

本轮延续 `docs/OPTIMIZATION_2026-11-04.md` 的选题标准：**热点路径、根因明确、
单点可改、行为不变、可用现有测试/分配探针复验**。关注三条线：代码结构、
每帧/每键的热路径、以及由「整份文档拷贝」带来的内存抖动。

> 行号以本文提交时的 `main`（`db8e7bc`）为基准。

---

## 零、环境：Metal 工具链缺失会阻断一切构建

本轮开工时 `cargo check` 直接失败：

```text
error: gpui_apple@0.1.0: metal shader compilation failed:
error: cannot execute tool 'metal' due to missing Metal Toolchain;
       use: xcodebuild -downloadComponent MetalToolchain
```

Xcode 27 起 Metal 编译器不再随 Xcode 本体分发，而是可下载组件
（`Metal Toolchain 27B47`，约 839 MB）。`xcrun -f metal` 仍能解析出路径，
但那个二进制只是个会报上述错误的桩，所以**只看 `xcrun -f` 会误判为「工具在」**。

修复（一次性，机器级）：

```bash
xcodebuild -downloadComponent MetalToolchain
```

下载后组件挂载在
`/var/run/com.apple.security.cryptexd/mnt/com.apple.MetalToolchain.../`，
`gpui_apple/build.rs` 通过 `xcrun -sdk macosx metal` 调用它。之后再没有
构建期问题。**这是本轮唯一一处环境改动，没有改仓库代码来绕过它**——绕法
（伪造 metallib / 打开 `runtime_shaders`）都会把坏产物带进发布包。

---

## 一、本轮基线

| 检查 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 通过（46 s，热缓存） |
| `cargo clippy --workspace --all-targets` | lib 90 条 / lib-test 184 条（90 条重复）；**与改动前完全一致，无新增** |
| `cargo test --lib` | 2316 passed / 3 ignored / 0 failed |
| `cargo fmt --all --check` | 通过 |

---

## 二、改动清单

| 优先级 | 位置 | 问题 | 结果 |
|---|---|---|---|
| P1 | `editor.rs:7090`、`diff_view.rs:742`、`:1545`、`editor.rs:7350` | 脏检查每键 `Rope::to_string()` 整份拷贝后再比较 | 已改为不复制文档的比较（≤128 B 固定开销） |
| P1 | `sidebar_view/host_view.rs:792` | 每个可见主机行、每一帧深拷贝整份分组表 + 连接配置 | 已改为共享句柄 + 惰性克隆 |
| P2 | `sidebar_view/theme_view.rs:24` | 主题侧栏每帧 `themes().to_vec()` 深拷贝整张主题表 | 已改为只读借用 |
| P3 | `terminal_element.rs:1461` | `PrepaintState` 是 11 元位置元组，构造点与解构点相隔数百行 | 已改为命名字段结构体 |
| P3 | `terminal_element.rs:1356` | `shape_overlay_layout` 15 个位置参数混着文本/几何/配色三组语义 | 已收成 `OverlayShapeParams` |
| P3 | `search.rs:918`、`terminal_view.rs:4267` | 每帧构造 query 的 `SharedString` 只为判断「空不空」 | 已改为 `text().len()` |

### P1：脏检查复制整份文档

`Editor::file_is_dirty` 在每次 `text_version` 变化（也就是每次击键）时调用。
旧实现把 `InputState::value()`（内部 `Rope::to_string()` + `SharedString::new`）
与 `original_content` 全文比较；`last_checked_text_version` 闸门只能过滤
「只移动光标」，过滤不掉真实编辑。

**做法**

- 新增 `InputState::text_equals(&str)`：直接走 `Rope: PartialEq<&str>`，
  按 chunk 逐段比较字节，不构造 `String`。
- `Editor::file_is_dirty` 的普通编辑器分支改为逐字节比较；live 分支保留
  `content_matches_baseline`（live 的影子文本剥了 `\r`，字节口径不同，
  不能混用）。
- `DiffView` 的两处脏检查、`flush_live_shadow` 的兜底比较同步改用
  `text_equals`（原先长度快速路径只是把「不同长度」的拷贝省掉，同长度
  替换/撤销回原样仍会全文拷贝）。
- `search.rs` 与 `terminal_view.rs` 的「query 是否为空」改为 `text().len()`。

**取舍**：先写过一版用 `get_chunk` 逐段查树的「真·零分配」实现，实测比
ropey 的 `chunks()` 迭代器慢 1.8 倍（10 MB 文档 debug 构建 5.6 ms vs
3.2 ms）——每个 chunk 一次 O(log N) 树查询。迭代器的代价是 `ChunkCursor`
为节点栈分配一个小 `Vec`（128 B，固定大小）。脏检查在击键热路径上，
用 128 B 换 1.8× 更快是对的选择，已在注释里写清。

**实测（10 MB 文档，`cargo test --lib` 分配探针，debug 构建）**

| 路径 | 耗时 | 分配次数 | 分配字节 |
|---|---:|---:|---:|
| `value()` + 比较（旧） | 5.3 ms | 18 | **44,039,584** |
| `text_equals(&doc)`（新） | 3.2 ms | ≤1 | **≤128** |

分配流量降了约 34 万倍（44 MB → 128 B），且 `text_equals` 的分配与文档
大小无关；耗时在 debug 下也已低于旧路径（release 下拷贝的劣势只会更明显）。

**回归测试**：`components::input::state::tests::text_equals_does_not_copy_the_document`
——64 KiB 多字节文档上验证语义（相等 / 长度不同 / 同长度不同内容 / 空串）
与分配口径（`text_equals` 的分配 ≤ 1 KiB 且不随文档增长，`value()` 至少
分配一份全文）。断言的是**恒定量级**而不是精确次数，避免被 ropey 内部
实现变化误伤。

### P1：主机列表每行每帧的深拷贝

`HostView::render_host_row` 在每次重绘时，对**每一个可见行**做：

- `configs.iter().find(|c| c.id() == host_id).cloned()`：深拷贝一份
  `SshConnectionConfig`（多个 `String` + `Vec`），而它只在双击连接时才用；
- `groups.to_vec()`：为右键菜单闭包整份克隆 `Vec<HostGroup>`（每个分组
  带 id/name 字符串），而菜单几乎从不打开。

20 个可见主机、10 个分组就是每帧 200+ 字符串分配，纯属为「可能永远不
触发」的交互付钱。

**做法**

- `render_host_row` 的 `configs` / `groups` 参数改为 `&Rc<Vec<..>>`
  （`GroupedHostsCache` 里本来就是 `Rc`，是一次引用计数 +1）。
- 双击连接改为在 `on_click` 里按 id 现查现克隆（`Rc` 已在闭包里）。
- 右键菜单闭包持有 `Rc` 而不是 `Vec` 的深拷贝。

行为不变：点击时用的是该行渲染时那一帧的同一份快照。

### P2：主题侧栏每帧克隆整张主题表

`render_theme_sidebar` 里 `ThemeState::themes(cx).to_vec()` 只是为了遍历，
而每个 `AppTheme` 都带 `id/name/description` 字符串与完整配色。改为直接
借用 `&[AppTheme]`（`cx` 在整个函数里是 `&App`，不存在借用冲突）。

### P3：`TerminalElement` 的两处参数梯

- `type PrepaintState` 原本是 11 元位置元组，构造点（prepaint 末尾）与
  解构点（paint 开头）相隔数百行，改字段顺序要两处同步数下标。改成
  `pub struct TerminalPrepaintState`，字段带注释；paint 侧用
  `..` 忽略不消费的字段。同时删掉两个从未被读取的字段
  （`cursor_row` / `cursor_in_viewport`，原先在解构处就是 `_` 前缀）。
- `shape_overlay_layout` 的 15 个位置参数按语义收成 `OverlayShapeParams`
  （文本 + 光标偏移 / 网格几何 / 配色三组），`#[allow(clippy::too_many_arguments)]`
  随之删除，全仓该类 allow 从 18 处降到 17 处。

两处都是纯结构改动：字段/参数名与原来一致，行为零变化（2316 条测试全绿）。

---

## 三、没做 / 后续可做

1. **主机列表虚拟化**。`SidebarFileTree` 用 `uniform_list` 只渲染可见行，
   而 `HostView` 的列表是 `div` + 全量子节点：主机一多，每帧都在建全量
   元素树。座位表里同时有文件夹行（52px）、分隔线（1px）、「未分组」标签
   （约 20px）三种高度，要走 `gpui::list`（可变高度）而不是 `uniform_list`，
   需要先拍平成一个行模型。改动面较大、UI 回归风险高，留待单独一轮。
2. **`clean_version` 让脏检查 O(1)**。现在每次编辑仍是 O(文档) 比较（只是
   不再复制）。给 `EditorFile` 存「保存时的 `text_version`」，版本相同直接
   判干净，可把「连续输入」的每键成本降到 O(1)。难点是**撤销回原样**时
   要能重新判干净：需要 undo 路径上报或额外记录，单独设计。
3. **clippy 基线 184 条**。本轮没有新增，但也没清零；其中 71 条是机
   械可修的 suggestion。建议单独一个「纯 lint」提交后用 `-D warnings`
   卡住新增，避免和逻辑改动混在一起。
4. **bench 未复跑**。`benches/terminal_render.rs`、`benches/markdown_live.rs`
   在本轮没有改动，也未重新测量；编辑器脏检查的收益目前由单测里的分配
   探针守护（比墙钟时间稳定）。若要加 `benches/editor_dirty.rs`，建议用
   `AllocObserver` 的分配字节数作为主指标。

---

## 四、验证记录

```bash
cargo fmt --all                                   # 通过
cargo check --workspace --all-targets             # 通过
cargo clippy --workspace --all-targets            # lib 90 / lib-test 184，与基线一致
cargo test --lib                                  # 2316 passed / 3 ignored / 0 failed
```

改动文件（9 个，+255 / −99）：

| 文件 | 改动 |
|---|---|
| `src/components/input/state.rs` | 新增 `text_equals` |
| `src/components/input/state_tests.rs` | 新增分配口径回归测试 |
| `src/components/input/search.rs` | query 判空不再构造 `SharedString` |
| `src/editor/editor.rs` | 脏检查、live 影子兜底比较改用 `text_equals` |
| `src/editor/diff_view.rs` | 两处脏检查改用 `text_equals` |
| `src/sidebar_view/host_view.rs` | 行渲染改共享句柄，配置惰性克隆 |
| `src/sidebar_view/theme_view.rs` | 去掉每帧主题表克隆 |
| `src/terminal_view/terminal_view.rs` | 搜索框判空不再构造 `SharedString` |
| `src/terminal_view/terminal_element.rs` | `PrepaintState` / 覆盖层参数改结构体 |

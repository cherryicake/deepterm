# 远程（SSH）LSP 支持方案

> 调研对象：Zed 源码 `~/Documents/workspace/zed`
> 状态：**已实施**（2026-10-13 完成阶段 1-5，见文末实施记录）

## 1. 需求

- 本地与远程 LSP **默认关闭**。
- 打开代码文件时，编辑器状态栏显示该文件对应语言服务器的状态。
- 用户可通过菜单手动开启 / 关闭 / 重启。

这与 Zed 的默认策略相反（Zed 默认自动启动），但**启停机制可以照搬**。

## 2. Zed 的架构（调研结论）

### 2.1 远程 LSP 走的是「远端完整实例」，不是 stdio 转发

Zed 没有把语言服务器的 stdio 通过 SSH channel 转发过来。它的做法是：

1. SSH 连接建立后，把 `zed` 二进制上传到远端（`remote_binary_path`）。
2. 启动 `ssh <remote> env VAR=val <remote-zed> proxy --identifier <id>`，
   即远端起一个**完整的 Zed 进程**（`crates/remote/src/transport/ssh.rs:439`
   `start_proxy`）。
3. 本地与这个远端进程之间走 protobuf RPC（`Envelope`，长度前缀帧），
   桥接在 SSH 子进程的 stdio 上（`crates/remote/src/transport.rs:128`
   `handle_rpc_messages_over_child_process_stdio`）。
4. 语言服务器由**远端的那个 Zed 实例**启动，
   进程 spawn 代码就是普通的 `Command::spawn`
   （`crates/lsp/src/lsp.rs:390` `LanguageServer::new`）。

关键证据：`crates/remote/` 整个 crate 里 **`lsp` 字样出现 0 次**。
remote 层完全不知道 LSP 的存在。

### 2.2 LspStore 的双模设计

`crates/project/src/lsp_store.rs`（15,288 行）分成：

| 类型 | 运行位置 | 职责 |
|---|---|---|
| `LocalLspStore` | 主机侧（本地工作区或 SSH 主机侧） | 服务器生命周期、二进制解析、启动 |
| `RemoteLspStore` | guest 侧 | **只透传**：`upstream_client` 发 RPC，不决定用哪个服务器 |
| `LspStore` | 统一门面 | 对 UI 暴露一致接口 |

`RemoteLspStore` 只有两个字段（`lsp_store.rs:4002`）：

```rust
pub struct RemoteLspStore {
    upstream_client: Option<AnyProtoClient>,
    upstream_project_id: u64,
}
```

guest 侧发 `proto::StartLanguageServer`，host 侧真正起进程后再回
`StartLanguageServer` 通知 guest 更新状态（`lsp_store.rs:8659`、`12005`）。

### 2.3 传输层泛型（值得抄的部分）

`LanguageServer::new_internal`（`lsp/src/lsp.rs:465`）对 stdin/stdout
完全泛型：

```rust
fn new_internal<Stdin, Stdout, Stderr, F>(...)
where
    Stdin: AsyncWrite + Unpin + Send + 'static,
      Stdout: AsyncRead + Unpin + Send + 'static,
      ...
```

所以 Zed 能用 `async_pipe::pipe()` 造出成对的假服务器做测试
（`LanguageServer::new`，`lsp.rs:1831`），也可以用真实子进程。
**DeepTerm 的 `Connection` 应该照这个思路改。**

### 2.4 远端 PATH 问题：Zed 的解法是「登录 shell 环境」

Zed 不给服务器进程猜 PATH。它在启动前先捕获远端登录 shell 的环境：

- `LocalLspAdapterDelegate::shell_env()`（`lsp_store.rs:14935`）
- → `load_directory_shell_environment`（`project/src/environment.rs:313`）
- → `util::shell_env::capture`（`util/src/shell_env.rs:30`）

`capture_unix` 的做法是跑 `shell -l -i -c '<command that dumps env as JSON>'`，
并把输出重定向到一个不会与 shell 启动噪音混淆的 fd
（`>&0` / `[1=0]` / `o>e`，按 shell 类型分支）。

拿到的 env 直接作为服务器进程的 `binary.env`
（`lsp_store.rs:726`），`which` 也用这份 PATH 解析（`lsp_store.rs:14984`）。

### 2.5 启停机制（直接照搬）

`LocalLspStore` 两个字段（`lsp_store.rs:342`）：

```rust
all_language_servers_stopped: bool,
stopped_language_servers: HashSet<LanguageServerName>,
```

- 默认都是「未停止」（`lsp_store.rs:4364` 初始化为 `false` / 空集）。
- `stop_all_language_servers`（`lsp_store.rs:11452`）置位全局标志；
  `stop_language_server` 按名字加入集合。
- **防复活的判据**在 `refresh_server_tree`（`lsp_store.rs:5443`）：
  全局标志置位时整个函数直接 return；单个服务器按名字跳过。
  这与 DeepTerm 现有的 `ServerTable::is_stopped` / `accepts_start` 是同一设计。
- `restart_all_language_servers`（`lsp_store.rs:11490`）清标志 + 清集合。

DeepTerm 已经在 `git log` 里修过同一个 bug（「关掉服务器后换标签再切回来
会把它重新拉起」），机制等价，只是 Zed 多一层 `all_language_servers_stopped`
的批量开关。

### 2.6 状态模型

Zed 分两个维度（`language_core/src/lsp_adapter.rs`）：

```rust
pub enum ServerHealth { Ok, Warning, Error }        // 健康度
pub enum BinaryStatus {                              // 进程/二进制状态
    None, CheckingForUpdate, Downloading, Starting,
    Stopping, Stopped, Failed { error: String },
}
```

DeepTerm 的 `ServerState`（`src/lsp/project.rs`）已经合成了一版
（`Running` / `Starting` / `Stopped` / `Failed`），够用，无需拆成两个维度。

## 3. DeepTerm 的方案

### 3.1 为什么不能照搬「上传 zed 二进制 + proxy」

Zed 的方案要求远端有一个**完整的 Zed 二进制**，因为它要在远端跑
buffer store、worktree store、project 等一整套东西，LSP 只是其中一环。

DeepTerm 是终端 + 编辑器，没有 RPC 层、没有 proto、没有 guest/host 分离。
照搬意味着先造一套 Zed 那样的 remote runtime，代价与本项目体量不匹配。

DeepTerm 已经有 russh 直连与 SFTP 会话池，可以直接在 SSH channel 上
承载语言服务器的 stdio。**这是同一目的下的轻量解法。**

### 3.2 方案 A：远端起服务器，stdio 走 SSH channel

在远端执行 LSP 服务器进程，把它的 stdin/stdout 接到 SSH channel 上。

- 服务器跑在源码旁边：索引、`cargo metadata`、sysroot、工具链全部命中。
- 路径语义天然正确：`file:///remote/path`，`project_root` 探测也在远端做。
- 与 Zed 的 `LanguageServer::new_internal` 泛型思路一致，
  DeepTerm 的 `Connection` 只需把「拿 stdio」那一步换掉。

### 3.3 传输层改造

`src/lsp/jsonrpc.rs` 的 `Connection` 当前直接持 `ChildStdin` /
`ChildStdout` / `Child`。改成泛型 +  trait 对象两种能力：

```rust
// 读循环只需要读
async fn read_loop<R, W>(stdout: R, stdin: W, ...)
where R: AsyncRead + Unpin + Send + 'static,
      W: AsyncWrite + Unpin + Send + 'static,
```

`spawn`（本地）保持现状；新增 `spawn_over_channel`（远端）传入
SSH channel 的 reader/writer。

**trait 家族差异（已在源码核对）**：russh 的 `make_reader()` /
`make_writer()` 返回 `tokio::io::AsyncRead/AsyncWrite`，而 `Connection`
基于 `futures` 的同名 trait。桥接用 `tokio-util` 的 `compat` feature：

```toml
tokio-util = { version = "0.7", features = ["compat"] }
```

该 crate 已在依赖树（`russh-sftp` 传递引入 0.7.18，只开了 `rt`），
加 feature 不引入新 crate。另已确认 `ChannelRx` 只用
`tokio::sync::mpsc::poll_recv`，不依赖 runtime 句柄，可以在 smol 里 poll。

### 3.4 关闭路径

本地 `shutdown_gracefully` 靠 `child.try_status()` / `child.kill()`。
远端没有 pid 可杀，改为：

1. 发 `shutdown` 请求 + `exit` 通知（与本地相同）。
2. 关 SSH channel（`Channel::close`）。
3. 超时后断开 SSH 连接。

`Connection` 的 `child: Arc<Mutex<Option<Child>>>` 需要抽象成
「可选的本地子进程」+「可选的 channel 句柄」。

### 3.5 远端 PATH（照搬 Zed 的思路，简化实现）

不做完整的「登录 shell env 捕获 + JSON 解析 + fd 重定向分支」。
分两步：

1. **启动前探测**：经 SFTP 会话执行 `command -v <server>`，拿到绝对路径。
2. **探测失败时**，用 `bash -lc 'command -v <server>'` 再试一次
   （覆盖 `~/.cargo/bin` 只在登录 shell 里可见的情况）。
3. 仍失败则状态栏报「远端未找到 <命令>」，与本地「启动失败」区分。

避免一次登录 shell 交互的开销，同时覆盖绝大多数情况。

### 3.6 默认关闭 + 状态栏 + 菜单

**默认关闭**：`LspContent` 增加 `remote` 段，默认 `enabled: false`；
本地 `lsp.enabled` 改为默认 `false`。

改动要点：`LspContent::default()` 现在是显式 `enabled: true`
（注释说明 derive 出来的 false 会被 serde 覆盖，这是个已踩过的坑），
改成 false 时要同步 `assets/default_settings.json`。

**状态栏**：`start_lsp_for_tab` 的门控从「本地且未加载」改为
「本地或远程，且远端已开启」。远端文件也要建 `LspSession`，
uri 仍是 `file:///remote/path`。

远端标签要显示**未启动**状态（不是「无 LSP」），用户据此知道可以开启。

**菜单**：`src/editor/lsp_menu.rs` 已实现启停/重启/日志，
只需让 `ServerKey` 带主机维度（见下）。

### 3.7 `ServerKey` 扩展

```rust
pub struct ServerKey {
    pub root: PathBuf,
    pub language: String,
    pub ssh_config: Option<SshConnectionConfig>,  // None = 本地
}
```

`SshConnectionConfig` **没有 `Hash` 实现**（`PartialEq`/`Eq` 是手写的，
有意排除 `use_system_proxy` 与 `created_at`）。补 `Hash` 时字段口径
必须与那份 `PartialEq` 完全一致，否则同一台机器会被判成两个键。

日志桶（`src/lsp/log.rs` 的 key）同样要含主机维度。

### 3.8 远端 project_root

当前用 `std::fs::exists()` 逐级向上找标记文件。远端版要经 SFTP `metadata`。
为避免深度 D 次往返，MVP 阶段先用「文件所在目录」作根，
后续再优化为带缓存的逐级探测。

### 3.9 文件监视

`handle_fs_event` 的 `didChangeWatchedFiles` 现在只对本地事件发
（`editor.rs:1170` 的 `is_from_backend(None)`）。远端会话要放开为
匹配当前 backend。`RemoteFsWatcher` 已经在轮询远端路径并发事件，
数据是现成的。

## 4. 分阶段实施

| 阶段 | 内容 | 可验证性 |
|---|---|---|
| 1 | `Connection` 泛型化 + `spawn_over_channel` + 关闭路径抽象 | 单测：替身传输覆盖帧编解码 |
| 2 | 远端 channel 传输 + PATH 探测 | 集成测试：真连一台机器 |
| 3 | 默认关闭 + 状态栏显示远端状态 | UI 验证 |
| 4 | `ServerKey` 带 `ssh_config` + 编辑器放开门控 | 端到端：远程 Rust 文件有诊断/hover/跳转 |
| 5 | 远端 `project_root` + `didChangeWatchedFiles` | 端到端 |

阶段 1-4 即为 MVP：远程代码文件有诊断、hover、跳转、重命名。

## 5. 风险

| 项 | 说明 |
|---|---|
| `SshConnectionConfig` 无 `Hash` | 需手写补，字段口径必须与既有 `PartialEq` 一致 |
| SFTP 会话池不能复用给 LSP | `with_cached_session` 错误路径会重连重跑，长连接不合适 |
| 远端 PATH | 见 3.5，分两步探测 |
| 空闲断连 | 本地进程无此问题；远端靠 SSH keepalive（30s×3，约 90s 判定） |
| 进程回收 | 远端无 `child.kill()` 对应物，只能关 channel + disconnect |
| 多项目并发 | 同一主机多项目根会起多个服务器进程，需评估资源 |
| 默认关闭的 UX | 用户可能不知道有 LSP；状态栏要给可发现的开启入口 |

---

# 附：远程 LSP 能做到什么程度

按「传输层改造后是否需要额外工作」分三档。结论基于对
`src/lsp/` 与 `src/editor/` 现有代码路径的逐项核对。

## 第一档：完美支持，零额外工作

这些功能的数据流只经过 `LspClient` → `Connection` → 编辑器内存结构，
**从不碰本地磁盘**。换成 SSH channel 传输后行为完全一致。

| 功能 | 代码位置 | 为什么天然成立 |
|---|---|---|
| 诊断（错误/警告波浪线 + 计数） | `session.rs:339` `spawn_diagnostic_listener` | 服务器推 `publishDiagnostics`，按 uri 过滤后写 `InputState`。uri 是 `file:///remote/path`，服务器在远端，两边一致 |
| Hover 浮窗 | `client.rs:336` | 纯请求-响应，结果只进浮层 |
| 补全 + resolve | `client.rs:361` / `404` | 同上 |
| 签名帮助 | `client.rs:668` | 同上 |
| Inlay Hint | `client.rs:864` | 同上，只按可见区间请求 |
| 同文件内跳转定义/声明/类型定义/实现 | `editor.rs:1897` `JumpInPlace` | 判定「同文件」走 `uri_eq`，只移光标不开文件 |
| 文档符号列表 | `client.rs:526` | 结果进 UI 列表面板 |
| 代码动作（作用于当前文件） | `editor.rs:2382` | `edits_for_uri` 按 uri 取编辑，只打当前 buffer |
| 格式化 / 选区格式化 | `client.rs:608` / `634` | `apply_text_edits` 只改当前 buffer |
| 同文件重命名 | `client.rs:541` + `edits_for_uri` | WorkspaceEdit 里属于当前 uri 的那部分 |
| 文档高亮 | `client.rs:821` | 只回字节区间 |
| 折叠范围 | `client.rs:846` | 只回行号 |
| 选区范围 | `client.rs:882` | 纯位置换算 |
| 语义令牌 | `client.rs:908` | 纯 token 流 |
| `$/progress` 索引进度 | `session.rs:358` | 推送到状态栏 |
| 启停/重启/报文日志 | `lsp_menu.rs`、`log.rs` | 与传输无关 |

这一档覆盖了日常 90% 的使用：写代码时看到的错误、补全、hover、格式化、
跳转。**改造传输层后它们自动就能用。**

一个附带收益：因为服务器跑在远端，`cargo metadata`、sysroot、工具链
都在远端本地磁盘上，rust-analyzer 的索引质量与用户在远端终端里
手跑 `rust-analyzer` 完全一致 —— 这是本地服务器 + 远程文件做不到的。

## 第二档：需要补一个「路径归属」判断

跨文件跳转是唯一的结构性缺口。

`editor.rs:1905` 的 `OpenFile` 分支调 `open_file_at_line_col_no_notify(path, ...)`，
它内部走 `open_local_file_at`（`editor.rs:3155`）—— **按本地路径打开**。
远程 LSP 跳转到 `/remote/project/src/lib.rs` 时，这个路径会被当成
本地路径，结果是在本地文件系统里找一个不存在的文件。

修复很小：`open_lsp_location` 需要知道当前会话是不是远程的，
是则改调 `open_remote_file_at_line_col_no_notify`
（`editor.rs:3159`，**已存在，签名正好匹配**）。

```rust
LspJumpAction::OpenFile { path, line, col } => {
    match self.active_lsp_session().and_then(|s| s.ssh_config()) {
        Some(config) => self.open_remote_file_at_line_col_no_notify(
            path.to_string_lossy().to_string(), config, Some(line), Some(col), window, cx),
        None => self.open_file_at_line_col_no_notify(path, Some(line), Some(col), window, cx),
    }
}
```

同类需要这个判断的：

- `find_references`（`editor.rs:1915`）：跳到第一个结果，与跳转定义同管线，
  改一处即同时生效。
- **跨文件重命名**：`edits_for_uri` 只取当前 uri 的编辑，其余文件的编辑
  会被丢弃。远端场景下「改另一个文件」需要打开它、改它、保存它。
  这是第二档里唯一有真实工作量的项。
- `document_symbols` / `find_references` 的结果如果带文件路径，点进去时
  同样要走归属判断。

## 第三档：需要额外实现，且语义上打折

| 功能 | 状态 | 原因 |
|---|---|---|
| 远端 `project_root` 探测 | 需重写 | 现在用 `std::fs::exists()` 逐级向上（`project.rs:50`），远端要走 SFTP `metadata`。MVP 可先用「文件所在目录」作根 |
| `didChangeWatchedFiles` | 需放开门控 | `editor.rs:1170` 只对本地事件发（`is_from_backend(None)`）。远端 `RemoteFsWatcher` 已在轮询并发事件，数据是现成的，改的是过滤条件 |
| 远端保存后 `didSave` | 零成本 | `save_text_to_file` 已经写远端，`on_saved` 加个分支即可 |
| 状态栏区分本地/远端 | 需加标识 | 两台机器同语言同项目根时，用户要能分清是哪个服务器的状态 |

## 明确做不到的

| 功能 | 原因 |
|---|---|
| 远端服务器二进制缺失时的自动下载安装 | 需要往远端写文件并执行，权限与平台差异大。只做「探测 + 报错」，与 Zed 的下载逻辑不同量级 |
| 在未打开文件时对整棵远端目录树做诊断 | 需要遍历远端目录并逐个 didOpen，SFTP 往返成本高。只服务用户打开的文件 |
| 服务器内存占用显示 | `lsp_button.rs` 的 `ProcessMemoryCache` 靠本地 `sysinfo` 读 pid；远端没有本地 pid 可读 |

## 与本地 LSP 的体验差异

会有差距的地方，但都不是功能缺失：

1. **冷启动慢**。SSH 握手 + 远端 `cargo metadata`，大项目可能十几秒到几十秒。
   状态栏会显示「启动中」，`$/progress` 会显示索引进度。
   `REQUEST_TIMEOUT`（30s）可能需要按远端放大。
2. **断线即失效**。SSH 断开则服务器一起没（它跑在远端那条连接上）。
   本地场景服务器是独立进程，网络抖动不影响。恢复连接后需要重启服务器，
   这是可以接受的降级 —— 且因为我们默认关闭，用户本就预期这是手动操作。
3. **空闲 90s 可能被 keepalive 判定断连**。LSP 本身没有心跳，
   长时间不动文件时靠 SSH keepalive 兜。这是 russh 的既有配置
   （`keepalive_interval: 30s` × `keepalive_max: 3`）。

## 结论

**第一档的功能（诊断、hover、补全、跳转、格式化、代码动作、重命名同文件部分）
在传输层改造完成后自动可用，不需要任何额外代码。** 这是主体价值。

第二档只需要在 `open_lsp_location` 加一个路径归属判断，工作量很小
（`open_remote_file_at_line_col_no_notify` 已经写好了）。
跨文件重命名是第二档里唯一有实际工作量的项。

所以方案 A 落地后，远程代码文件的体验与本地基本齐平，
差距集中在冷启动速度与断线恢复，而不是功能缺失。

---

# 实施记录（2026-10-13）

方案已按阶段落地。与上面「方案」部分的差异按实际实现记录于此，
以代码为准。

## 已完成的五个阶段

| 阶段 | 落地内容 | 关键文件 |
|---|---|---|
| 1 | `Connection` 传输层泛型化：`ServerProcess` 枚举（本地 `Child` / 远端 `RemoteLspProcess`），读写端统一为 `Box<dyn futures::Async{Read,Write}>` | `src/lsp/jsonrpc.rs` |
| 2 | 远端传输：`RemoteLspProcess` 经 russh channel `exec` 起服务器，`ChannelStream` + `tokio-util compat` 桥接两套 trait 家族；两级 PATH 探测 | `src/ssh/lsp_channel.rs` |
| 3 | 默认关闭：`LspContent::default().enabled = false` + `default_settings.json` 同步；状态栏新增 `LspStatus::NotStarted`，chip 可点开菜单启动 | `src/lsp/registry.rs`、`src/lsp/status.rs`、`src/editor/editor.rs` |
| 4 | `ServerKey` 增加 `ssh_config` 维度（含手写 `Hash`，字段口径与既有 `PartialEq` 一致）；编辑器放开远程门控 | `src/lsp/project.rs`、`src/ssh/config.rs` |
| 5 | 跨文件跳转按会话归属分流（远端走 `open_remote_file_at_line_col_no_notify`）；远端 `project_root` 经 SFTP 逐级探测；`didChangeWatchedFiles` 放开为匹配当前 backend | `src/editor/editor.rs`、`src/lsp/project.rs` |

## 与方案的偏差

1. **远端 `project_root` 用了同步逐级探测**，不是方案里写的「MVP 先用文件所在目录」。
   上限 12 级，只在「首次为该远端语言起服务器」与「用户点开菜单」时发生。
2. **`didChangeWatchedFiles` 直接放开**，没有按方案单独排期——过滤条件改成
   `is_from_backend(session.ssh_config())` 即可，本地/远端共用一条路径。
3. **`didSave` 无需改**：远端保存走的仍是同一条 `on_saved`，uri 一致即生效。

## 未做（明确不在本次范围）

- 跨文件重命名（第二档里唯一有实际工作量的项：需要打开远端文件、改、保存）。
- 远端服务器二进制的自动下载安装。
- 状态栏区分本地/远端的视觉标识（当前靠菜单元信息行与日志区分）。
- 远端服务器内存占用显示（远端没有本地 pid 可读）。

## 复审与修复（同日，第二次）

上面「阶段 3/4/5」落地后逐条复审，发现问题按 P0/P1/P2 修完。逐条记录，
以免下一个人把同一条改回去。

### P0：功能其实没通

1. **chip 在「从未启动过」时点不动**。`active_lsp_snapshot` 只在服务器表里
   有条目时才返回快照，而表里没有条目恰恰是默认关闭时的常态 → 菜单开不了，
   用户唯一的开启入口不存在。改为：表里没有就补一份 `Stopped` 的最小快照，
   chip 可点。
2. **菜单「Start」起来之后不接已打开的标签**。`start_lsp_server_for_key`
   只管进程，标签还挂在旧的 `None` 上。补 `tabs_for_server_key` + 逐标签
   `start_lsp_for_tab`；`server_for_key_with_override` 里 `starting` 分支
   改为**并入**已在握手的那个进程（原来会再起一个）。
3. **异步加载完成的远程文件从不接 LSP**。加载回调用 `is_local` 把远端挡掉
   了，而远端文件**只有**这一条路径（没有同步加载）。去掉该门控。
4. **渲染路径与开文件路径上有同步 SSH 往返**。项目根探测逐目录 `stat`
   （实测一个工程约 200 次往返），候选可用性探测每次渲染都问一次远端。
   两处都改成「只读缓存，缺失就后台补探 + `notify`」：
   - `registry`：`Resolve::{Blocking, Cached}` 分流，UI 一律走 `Cached`，
     新增 `pending_chain_probe` / `probe_commands_pending` 给补探用；
     探测结果按 `Probe::Settled(bool)` / `Probe::RetryAt(Instant)` 存，
     **「问不到」绝不落成「没有」**（30s 后重试）。
   - `remote_project_root`：一次 `sh` 脚本走完 12 级，加会话期缓存，
     键为 `user@host:port|目录`。
   - **开标签这条也挪走**：`start_lsp_for_tab` 在「这个目录的根还不知道」时
     直接把标签挂起（`Editor::pending_root_attach`），探测丢给
     `background_spawn`，落地后重走一次本函数。留在 UI 线程上的最后一次往返
     就是它——`session.connect` → `server_for_remote_file` → `root_for`，
     主机卡住时是 15s 的界面冻结，而且发生在「双击文件」这个动作上，
     比卡顿更难解释。
     终止性由 `remote_project_root` 保证：每条出路要么缓存根、要么记下
     「问过但没问到」，两者都让 `remote_root_is_known` 返回真。
     失败记的是**时刻**而不是结论，静默 `ROOT_PROBE_RETRY`（30s，与候选链
     同一口径）后允许再探——把失败当事实缓存，主机恢复后这个项目就永远
     探不回真根了。菜单/停止那两条路径（`active_lsp_snapshot`、
     `tabs_for_server_key`）仍走 `root_for`：它们只在用户点击时跑，
     且此时开标签已经探过，命中缓存。

### P1：正确性与失败可查

5. **`ServerKey` 的主机维度改为「主机身份」**：`host/port/username`，
   手写 `PartialEq`/`Hash` 都按这一口径。原来整份 `SshConnectionConfig`
   参与比较，而 `os` 是**连上之后探测写回**的（`workspace.rs`），于是同一个
   进程空间有「探测前/后」两把键：查不到已在跑的进程 → 起第二个服务器；
   标签归属判断同样失效。`Editor::tabs_for_server_key` 共用
   `project::host_identity`，两处口径必须一致。
6. **区分「命令没装」与「主机连不上」**：`resolve_remote_binary` 拆出
   `RemoteUnreachable` 类型，启动失败时按它落 `finish_retryable`
   （进 `starting` 清理但不进 `failed`），下次打开文件还能重试——原来一次
   网络抖动会把这门语言永久标记为失败。
7. 远端项目根探测的缓存同上（见 4）。
8. **登录 shell 探测的输出容忍 rc 文件噪声**：取第一条以 `/` 开头的行，
   而不是整段 stdout（`~/.bashrc` 里一句 echo 就会让解析出绝对路径失败）。
9. **跨文件跳转的归属在发请求前捕获**（`LspJumpOrigin`）。原来在响应到达后
   才问 `active_lsp_session()`，用户在等待期间切了标签 → 远端文件被当本地
   打开（或反之）。
10. **远端标签 id 带主机**：`editor-remote-user@host:port-<path>`。原来只有
    路径，两台主机的同名路径会撞同一个 id。
11. **`initialize` 的 `processId` 对远端传 `null`**。传本地 pid 会让服务器
    （按规范）监视一个它看不见的进程。
12. **读循环不再自己去拿写锁**：应答服务器的主动请求改为投递到无界队列，
    由 `reply_writer` 专职落笔。远端 channel 写满时，另一个任务正持着
    `stdin` 锁阻塞在 `write_all` 上，而它等的数据要靠读循环腾出来——
    读循环一停在等锁上就是实打实的死锁。

### P2

- `Content-Length` 的边界从一句恒真的 `.filter(|n| *n > 0 || true)` 改成
  真实校验：`0 < n <= MAX_FRAME_BYTES`（64MiB）。畸形大长度若被判成「半包」，
  读循环会照着它一直往缓冲里攒字节、永远等不齐，连接无声挂死。
- `initialize` 之后按目标主机显示服务器名（`configured_command_on`
  不再被全局开关挡回 `None`）：chip 上写的是那台机器上真的会起的那个。
- **一个远端服务器一条专用 SSH 连接**（`RemoteLspProcess::connect` 不走
  `sftp_fs` 的会话池），核对过三点：
  - 条数 = 存活的带 `ssh_config` 的 `ServerKey` 数（项目根 × 语言），
    不是按标签数；`sftp_fs` 的 `MAX_IDLE_SESSIONS_PER_HOST`（8）管的是
    **空闲池**，套不到长寿命连接上，所以这里没有现成的上限可借。
  - 回收：`Connection::Drop → shutdown → shutdown_gracefully`，远端分支
    等 `is_finished` 后交出 `RemoteLspProcess`（`Arc<Handle>` 随之释放），
    超时则显式 `close()` 断开——两条出路都不留活连接。
    唯一的漏点是 `spawn_over_channel` 里 `take_io()?` 之前失败，那时
    `process` 被裸 drop；而 `take_io` 只在「io 已被取走」时失败，
    这条路径上进程刚 new 出来，走不到。
  - 为什么**不该**并进会话池：池按「一次调用」借还，而 exec 起来的服务器
    占的是整条连接的寿命，借出去还不回来是池的语义事故。
  结论：不加客户端限流。真按 `MaxSessions`/内存算，先撑不住的是一台机器上
  同时跑四个 rust-analyzer，而那是用户一个个点「Start」点出来的显式操作，
  替他决定「该踢掉哪个服务器」是产品决策，不是这次修复的范围。

### 测试

本机新增的关键一条：

- `project::tests::deferred_attach_ends_even_when_the_probe_fails`——用
  `127.0.0.1:1`（必然连不上）钉住「挂起等根」的两条不变量：问过就必须给挂起的
  标签一个尽头（含问不到的情况），而尽头只买一段静默、过期后重新变成「不知道」。
  第二条靠把记录的时刻拨旧来触发，不睡 30 秒。

真机用例（`DEEPTERM_LSP_REMOTE_HOST` 才跑）新增：

| 用例 | 守的东西 |
|---|---|
| `detecting_the_os_must_not_change_the_key` | 键在 `os` 写回后仍是同一个 |
| `after_menu_start_another_file_in_the_project_gets_diagnostics` | 菜单 Start → 第二个文件走自动路径拿到**同一个进程** → didOpen 后真的收到诊断 |
| `a_running_server_is_reused_even_when_globally_disabled` | 默认关闭时自动路径要复用已跑起来的进程 |
| `chip_names_the_server_even_when_globally_disabled` | chip 显示按目标主机挑出来的服务器名 |
| `not_started_placeholder_follows_the_target_host` | 占位与否按**那台机器**上有没有候选 |
| `remote_python_candidate_chain_picks_what_the_host_has` | 候选链按目标主机挑，不钉死名字 |

真机跑法与两个坑（都是这次实际踩到的）：

```sh
DEEPTERM_LSP_REMOTE_HOST=192.168.194.4 DEEPTERM_LSP_REMOTE_USER=zhousong \
  cargo test --lib lsp::remote -- --test-threads=1 --nocapture
```

- **别把「终局拒绝」当「还没好」等**。`server_for_key*` 返回的 channel 里
  `Ok(None)` 是答案（这台主机上选不出命令 / 握手失败），此后队列永远为空。
  第一版 `after_menu_start_...` 用 `if let Ok(Some(_))` 轮询，白等满 90s
  再报一句看不出原因的「server never became ready」。现在
  `wait_until_ready` 把 `Ok(None)`、超时与 `Closed` 分开，并把
  `lsp::log::messages(key)` 的握手报文一起打进 panic 消息——真机故障
  重跑一遍去猜不如一次看清。
- **超时预算按真机给**：`/tmp` 下现写的空工程，rust-analyzer 冷启动要跑
  `cargo metadata`，测试机负载 1.8/14 个登录用户时 90s 不够（同一条用例
  120s 稳过）。单独重跑必过——是环境，不是代码。

## `.pyi` 存根：认不出来，以及认出来之后惹的祸（同日，第三次）

用户报的现象：「py 函数跳转到了 `.pyi` 的文件中，但当前代码编辑器不识别 `.pyi`」。

两层原因，第二层是第一层的修复**引入**的：

1. **认不出来**：`guess_language_from_extension` 的表里只有 `py`，于是
   `typing.pyi` 得到 `text`。掉的不只是高亮——`#` 注释切换没了，LSP 也
   根本不接（`start_lsp_for_tab` 见 language 是 `text` 就 return，连状态栏
   chip 都不出现），落点看着就像一个打不开的文件。存根的语法就是 Python，
   语言 id 保持 `python`（pyright/basedpyright 的 `didOpen` 也这么报，
   它们把存根当 python 文档处理）。图标同样复用 `python.svg`。

2. **认出来之后**：`.pyi` 一变成 python，跳转的落点就开始接 LSP 了——而它
   **不在项目树里**（typeshed、stub 包、site-packages）。`ServerKey.root` 原本
   由「这个文件自己的路径」算，于是每次跳进一个存根就在存根目录旁边另起一个
   Python 服务器；pyright 冷启动要索引整个 typeshed，体感是「跳一次卡一次」，
   还多占一个进程。所以这一条必须和识别一起修，不能留给以后。

修法：**服务器归属由发起跳转的会话传下去**，落点不再自己算根。

- `LspSession` 记住自己挂的根（`server_root`，`connect` 时定下来）。为此把
  原来两个几乎重复的入口 `server_for_file` / `server_for_remote_file` 收敛成
  `LspManager::key_for_file`——包装函数只发请求，给不出**算出来的那个根**。
  根的选择本身抽成纯函数 `session::server_key_for`（`connect` 要 `App` 且真起
  进程，那条路上没法断言「显式根优先」）。
- `LspJumpOrigin` 多带一个 `root`：归属在发请求**之前**捕获，跟 uri/主机同一个
  理由（await 之后用户可能已经切了标签）。
- `Editor::lsp_jump_roots` 记下「目标文件（路径 + 主机）← 发起跳转的根」，
  `start_lsp_for_tab` 命中它就直接 `set_server_root`。用表而不是函数参数：
  跨文件打开要经过 `open_*_at_line_col_no_notify`（终端 Cmd+Click 也用它，
  那边没有会话概念），而远端文件还是异步加载的——真正接 LSP 的时刻在
  `finish_file_load` 里，中间没有传参的通道。表按标签数收敛，免得长会话里
  每个落点留一条。
- 附带好处：远端的跳转落点**带着根来**，就不需要去那台机器上探项目根，
  因此不走 P0#4 那条「等根探测先挂起」的路（少一次往返，也少一次延迟）。

本机钉住的三条：`type_stubs_are_python_not_text`（含反向：`.py`/`.rs` 不受影响）、
`jump_root_key_keeps_the_host_in_it`（同路径不同主机是两个键，与 `ServerKey` 同源）、
`session::tests::explicit_root_wins_over_the_path_derived_one`（显式根优先，
没指定才按路径算）。

## Cmd+Click，以及「跳过去」到底要跳到什么（同日，第四次）

用户报两条：

1. 「使用 cmd + 光标点击也能触发函数跳转，就和 vscode 一样」——本仓此前
   **完全没有**鼠标跳转（`event.modifiers` 在输入框里只用于 shift 扩选）。
2. 「函数跳转一定要跳到这个函数上，现在是直接打开代码文件就完事了，光标
   焦点也没有跳转过去」。

### VSCode 是怎么做的

github 直连不通，取的是 jsDelivr 上 `microsoft/vscode@main` 的原文，并在本机
安装的 VSCode 包里核对了默认值。

- **触发键**由 `editor.multiCursorModifier` 决定，默认 `"altKey"`（安装包里的
  默认值同样是 alt），于是 macOS = Cmd+Click、其余平台 = Ctrl+Click。
  这个跨平台语义正好等于 gpui 的 `Modifiers::secondary()`。
- **守卫**（`clickLinkGesture.ts`）：左键、`event.detail <= 1`（双击选词、
  三击选行，都不能顺手跳一次转会打断选择）、触发键必须在**按下那一刻**就
  按住（它专门记 `_hasTriggerModifierOnMouseDown`）。
- **命中必须是正文**：`target.type === MouseTargetType.CONTENT_TEXT`，点在
  行号栏不算。
- **先放光标，再发请求**：`gotoDefinition()` 的第一行就是
  `this.editor.setPosition(position)`，之后走与 F12 **同一条**命令路径
  （`goToCommands.ts` 读的是 `editor.getPosition()`）。
- **落点**（`goToCommands._openReference` → `applyTextEditorOptions`）：
  selection 折叠到 `range.start`，先 setSelection 再 reveal；同文件与跨文件
  走同一条路。
- **焦点**：`codeEditorService.openCodeEditor` 用
  `openEditor(input, ACTIVE_GROUP)`，不带 `preserveFocus`，于是
  `editorGroupView` 的 `activateGroup` 把目标分组激活——跳完键盘就在新文件上。

### 本机为什么「打开了但没跳」

`jump_to_line_col` 设了 `selected_range`、也 `switch_to_file` 了，但**从不聚焦**：

- gpui 的焦点跟着**物理点击**走（`Div::track_focus`），程序化换标签不会带动
  它，`switch_to_file` 只改 `active_file_id` 与标签栏；
- 而选区与光标的**绘制**都以 focus 为门（`components/input/element.rs`：
  `layout_selections` 非聚焦返回 `None`、`should_paint_cursor` 同）。

两条合起来就是用户看到的：文件开了、落点看不见、接着敲键还打在原文件里。
已有的两条断言（光标行列跳对了）抓不到这个缺陷——它们只看 `selected_range`，
不看焦点。

### 改动

- `Editor::focus_editor_tab`：跳转的收尾把键盘焦点交给目标编辑器。
  diff / 图片 / markdown 预览 / 同屏实时这几种标签**不聚焦**——那里渲染的不是
  `InputState`，聚焦一个屏幕上不存在的输入框等于把键盘焦点从界面上拿走
  （判定与 `active_text_editor` 同口径）。
- `jump_to_line_col` 因此要收 `&mut Window`。异步落地的 pending jump 同样要
  聚焦，所以 `spawn_file_load` 的收尾从 `update` 换成 `update_in`。
- Cmd+Click：`is_goto_definition_click`（纯判定，照抄上面三条守卫）+
  `Editor::goto_definition_at_click`，挂在编辑器**容器**的左键按下上，转发给
  `goto_kind(Definition)`——于是鼠标跳转与 F12 共用整条管线（跳转历史、
  `.pyi` 的服务器归属、远程 uri 走远程打开）。容器上再补一条
  `InputState::click_is_in_text`（正文才算）。
- 顺序天然对齐 VSCode：输入框自己的按下监听在更深一层，先跑完并把光标移到
  点击处（= `setPosition`），我们随后按当前光标发请求（= 读 `getPosition`）。

### 与 VSCode 的有意差异

VSCode 在 **mouse up** 才执行，并要求 release 与按下在同一行（防拖选途中按下
修饰键把选区当成跳转）；我们在按下时执行——macOS 上 Cmd+拖动不是文本选择
手势，风险低于多养一份按下状态。VSCode 的按住高亮下划线、Alt+Click
（declaration）、中键开侧边、多结果 peek 也还没做：这些键位走的是既有的
动作路径，鼠标只补了最常用的那一条。

### 钉住的测试

`jump_focuses_the_target_editor`（跨文件跳转后焦点必须在目标编辑器上，含反向
断言：发起跳转的文件必须失去焦点）、`secondary_click_triggers_goto_definition`、
`ordinary_clicks_do_not_trigger_goto_definition`（普通点击 / 右键 / 双击 / 三击）、
`other_modifiers_do_not_trigger_goto_definition`（单按 Alt、Shift 不跳）。

焦点这一层能在测试里断言，是因为 gpui 的 `Window::focus` 直接写
`self.focus`（`window.rs:2050`，不校验句柄在不在渲染树里）；「鼠标点得到、
浮层不抢事件」这种还是要人眼在界面上确认。


## 状态灯的规矩：谁点灯、谁转圈、谁都不画（同日，第五次）

用户的问题：**「为什么 py lsp 状态栏上没有像 rust lsp 前面有一个状态圆点标识」**。
同一个 chip 组件，Rust 有点、Python 没有——差别不在远程/本地，也不在语言，
而在**诊断计数**。

### 证据（用户自己的日志）

```
04:03:30  lsp-status: basedpyright-langserver   (raw=Ready { errors: 0, warnings: 0 })
04:03:32  lsp: publishDiagnostics 59 items (version Some(0)) for .../controller.py
（此后没有任何 lsp-status 行）
```

`Ready` 时 chip 的文案就是服务器名，计数变了文案没变——所以那 59 条诊断落地
时日志一言不发。而 Rust 侧最后一条是 `Ready { errors: 0, warnings: 0 }`。
两边唯一的区别由此确定：**errors 是否大于 0**。

### 根因

`render_lsp_status_chip` 把「画不画灯」挂在**颜色分档**上：

```rust
LspChipTone::Error => (theme.error, false),   // ← 灯没了
```

`Ready { errors > 0 }` 归 `Error` 档，于是**不画灯**；同一个档又被 `Failed`
复用，而 `Failed` 确实不该画灯（没有进程）。一个档承担了两件不相干的事，
结果是「有错误」这个最需要被看见的状态成了唯一没有灯的状态——用户读成
「Python 的 LSP 根本没起来」。`render_lsp_status_chip` 自己的文档写着
「灯芯的颜色即状态：绿=干净、黄=有警告、**红=有错误**」，实现与文档相反。

### 改法

灯按**状态**决定，颜色按**分档**决定，两件事分开：

- `lsp_chip_shows_lamp(status)`：只有 `Ready` / `Stopped` 点灯——灯的意思
  是「有个进程活着」；
- `Failed` / `NotStarted` 不点（那时确实没有进程），`Starting` / `Working`
  画转圈；
- 颜色仍由 tone 给：`Ready { errors > 0 }` 现在是**红灯**，不再是「红字、
  什么都没有」。

### 顺带修的四条同源问题

1. **空内容的 `$/progress` 会把 chip 永久钉在转圈态**（同样表现为「没有
   点」）。只有 `end` 会删条目，而没发过 `begin` 的令牌永远等不到自己的
   `end`：一条 `{}` report 建出一个空条目后谁也清不掉它。现在
   - 内容全空（message/percentage 都没有）且令牌未知 → **不建条目**；
   - `representative()` 与 `is_empty()` 统一口径（标题、消息、百分比全空
     的条目不算「有可显示的进度」），否则一边说「有进度」、另一边说
     「没内容」，chip 停在既没文字也没灯的态上。
2. **监听任务把「有没有变化」和「实体还在不在」混成同一个布尔**。
   `weak.update(...).unwrap_or(false)` + `if !alive { break }`：一条被版本
   门控丢弃的诊断（`applied == false`）或一条内容没变的进度，都会被读成
   「编辑器已关」而**终止监听**——此后这个文件的诊断与进度再也不更新，chip
   停在最后一次变化的样子。现在存活判断只用 `is_err()`。
3. **重复送达的同一条进度不再触发整棵视图树重建**。rust-analyzer 索引期
   一轮推 800 条，其中不少 message/percentage 一个字节没变；`apply` 现在
   返回「是否真的变了」，`set_lsp_diagnostic_counts` 同（计数相同就是
   no-op），两边都据此跳过 `cx.notify()`。
4. **chip 日志的去重键从「文案」改成「文案 + 状态」**。`NotStarted` /
   `Stopped` / `Ready` 的文案都是服务器名，只比文案的话「用户点了 Stop」
   和「起来了但有 59 个错」在日志里都不留痕迹——本次排查就是先被这个盲区
   带偏了一次。

### 审计结论里被否掉的一条

外部审计说「诊断计数写进了**当前活动文件**的槽，两个文件都会收到对方的
重绘」。这条不成立，也**不要**按它的建议改：`spawn_diagnostic_listener`
拿到的是**该标签自己的** `Entity<InputState>`（`editor.rs` 里
`let editor = this.open_files[idx].editor_state.clone()`），`lsp_status`
是每个标签一份，状态栏读的也是活动标签自己那份。把计数塞进事件、再在
`drain_lsp_results` 里比 tab id，会把一份本来就正确的实现改坏。

### 钉住的测试

- `errors_still_light_the_lamp`——`Ready` 有错也点灯；`Failed` / `NotStarted`
  不点；`Starting` / `Working` 走转圈。
- `an_empty_report_does_not_create_a_stuck_progress`、
  `a_blank_title_is_not_displayable`——空进度不留条目、不可显示。
- `an_identical_report_reports_no_change`、
  `an_end_for_an_unknown_token_changes_nothing`——通知的「变化」口径。
- `identical_diagnostic_counts_are_a_no_op`、
  `a_content_less_progress_report_changes_nothing`——状态层的两个 no-op。

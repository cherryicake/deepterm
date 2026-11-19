# 终端持久化方案（移植 herdr-app）

> 调研对象：`~/Documents/herdr-app` + `herdr-core`（`hc` 0.1.20）
> 状态：设计中（2026-11-12 起）

## 1. 需求

- 关闭 deepterm 窗口后，**本地与远程（SSH）终端里跑的程序继续运行**。
- 重新打开 deepterm，**立刻恢复退出前的样子**：同样的标签、面板布局、
  每个终端的屏幕内容与回滚历史，程序仍在原处继续跑。
- 不是"重新执行一遍命令"，是**接回同一个进程**。

## 2. herdr 的做法（调研结论）

### 2.1 三进程模型

herdr-app 自己**不持有任何 PTY**。终端全部跑在独立常驻的
`herdr-core`（可执行名 `hc`）里：

```
herdr-app (GPUI 窗口)
    │  每次启动 fork 一个 `hc connect` 子进程，
    │  在它的 stdin/stdout 上走长度前缀帧
    ▼
herdr-core  (常驻 daemon，持终端注册表 terminals.json)
    │
    └── holder 进程 (每个 holder 管一批终端)
            ├── /bin/bash   ← PTY master 在 holder 里
            └── /bin/bash
```

关键点：**窗口关闭 → `hc connect` 子进程退出 → core 存活 → 终端不受影响**。
core 甚至可以在更新时 `server.restart` 而"programs keep running"
（旧的 holder 继续跑，新 core 接管记录）。

### 2.2 恢复靠"接回"，不靠"重放"

启动时：

1. `events.subscribe` 拿到快照：所有 live 终端的记录
   （`address` / `argv` / `cwd` / `pid` / `keep` / `labels` ...）。
2. 对每个要显示的终端调 `terminal.attach`：

```json
{"terminal": "t7", "cols": 174, "rows": 66,
 "history": 2000, "display": {...}}
```

core 回复一个**专属 channel**，然后：

- 先把当前屏幕（+ 最多 2000 行回滚）以**重绘序列**推一遍；
- 之后持续推新输出。

客户端只是"重新渲染"，程序根本不知道有人接过来了。

### 2.3 会话状态的持久化分三层

| 层 | 存放位置 | 内容 | 写入方 |
| --- | --- | --- | --- |
| 终端 | core 的 `terminals.json` + `screens/*.snap` | argv、cwd、env、pid、`keep`、退出码 | core |
| 布局 | core 的 `doc`（`doc.get`/`doc.set`，CAS 版本号） | space / tab / 切分树 | herdr-app |
| 视图 | `~/.config/herdr-app/view.json` | 焦点、zoom、最近记录 | herdr-app |

布局文档放在 **core 里**而不是本地文件，这样换一台机器的 core 也能读到同一份
布局；CAS 版本号解决多窗口并发写。

### 2.4 `keep` 语义

`terminal.run` 默认 `keep: true`。没开 `keep` 的终端在 core 重启后变成
`finished`；开了的会被换成一个新 shell 原地接上（`--keep` 明确说
"never re-run the original command"）。

## 3. deepterm 现状

**单进程**。`Terminal` 用 `alacritty_terminal::Pty` 直接 fork 子进程，
PTY master fd 存在 UI 进程里（`Terminal::pty_master_fd`）。
窗口一关，SIGHUP，整棵树陪葬。

已有的、可以直接复用的：

- **字节管线与来源无关**。SSH 那条路已经是"外部字节流"形态：
  `handle_remote_data` → `process_ssh_data` → `process_pty_data` → `advance_vte`。
  新的守护进程字节源可以完全走这条链。
- **输入/尺寸已经是回调形态**。远程终端的
  `remote_input_callback` / `remote_resize_callback` 正是挂接式终端需要的接口。
- **kitty 图形、OSC 扫描、agent 识别**都在 `Terminal` 内部，与字节来源解耦。
- **会话状态已有**（`session_state.rs`），但语义是"重建"，不是"重连"。

缺的：

1. 常驻进程 + IPC。
2. `Terminal` 的"挂接式"字节源（目前本地路径是 `LocalReader` 直接持有 PTY）。
3. 服务器的 VT 状态（见 4.1）。

## 4. 设计

### 4.1 屏幕状态放在哪：服务器持有 VT

herdr 用 Ghostty，它的屏幕是 `ghostty_terminal`，可以导出 `GHOSTSNP`
（你机器上 `~/.local/state/herdr-core/default/screens/*.snap` 就是）。

deepterm 用 `alacritty_terminal::Term`，**`Term` 没有实现 `Serialize`**
（只有 `grid::Grid` / `storage` / `index` 等零件有 `serde` feature，
`Term` 本体没有）。所以有两条路：

- **(A) 守护进程也跑一份 `Term`**，attach 时把它渲染成重绘序列发给客户端。
  与 herdr 等价，客户端不需要知道服务器的内部状态。
- **(B) 只存原始字节流**，attach 时把历史字节重放给客户端。

**选 (A)。** 理由：

- (B) 的重放代价随历史线性增长，一个跑了三小时的 `make` 就是几十 MB，
  打开窗口要卡很久；而且重放期间 shell 提示符会闪。
- (A) 的重绘是 `O(屏幕格数)`，固定几十 KB。
- (A) 还能顺带解决"客户端 resize 后如何重新排版"——服务器知道完整状态，
  可以按新尺寸重新折行后再渲染。

`Term` 在守护进程里本来就是必需的：要回答 `CSI 14 t`（窗口像素尺寸）、
`CSI 18 t`（字符尺寸）、查询光标位置、识别 OSC 7 cwd 等等。

**重绘序列的格式**：普通 ANSI。逐行输出
`CSI {row};1H` + 该行所有 cell 的 SGR 转义 + 文字，结束时恢复光标位置与
当前 SGR。这样客户端收到后喂给现有 vte 就行，零新代码。
kitty 图形单独处理：守护进程自己跑一份扫描器维护共享存储，挂接时按当前
存储**重新编码**（`a=t` 传像素 + `a=p` 摆位置），而不是回放原始 APC 字节。
重新编码天然只包含"现在还在的图"，不用背整条历史；代价是动画只恢复当前帧、
回滚区里的放置不恢复（那些已经滚出视口）。

**两种重绘不是一回事**，混用会出具体问题：

| 入口 | 带多少回滚 | 用途 |
| --- | --- | --- |
| `snapshot()` | 0 | resize 后的重绘对齐。每次拖窗口都触发；带回滚会让客户端的历史翻倍 |
| `attach_paint(history)` | 客户端请求的（上限 2000） | 新窗口挂接。本地 VT 是空的，要看到关窗前的输出 |
| 原地重连 | 0 | 本地 VT 里已经有回滚，再要一份会翻倍（断一次翻一倍） |

### 4.2 进程与 IPC

```
deepterm (GUI)
    │ 启动子进程，读写它的 stdin/stdout
    ▼
deepcore connect ── Unix socket / Named Pipe ──▶ deepcore (core)
    │                                              │
    └──────── 9 字节头分帧，原样中继 ──────────────┘
                                                   │
                         ┌─────────────────────────┴─────────────────────────┐
                         ▼                                                   ▼
              deepcore --holder --socket holder.<id>.sock      holder.sock（旧版）
                   └── Pty + Term + poller                         └── Pty + Term + poller
```

GUI 不直接持有 socket：它启动 `deepcore connect`，只面对两根管道。这与
herdr 的 `hc connect` 同构，也为后续“通过 SSH channel 执行远端
`deepcore connect`”留下同一条传输路径。`connect` 只做字节中继，不解析业务
帧，也不替 GUI 完成 `hello`。

**两个独立二进制**，源码在 `src/bin/deepterm.rs` 与 `src/bin/deepcore.rs`：

| | `deepterm` | `deepcore` |
| --- | --- | --- |
| 角色 | GUI 客户端 | 终端守护进程 |
| 链接图形栈 | 是 | **否**（产物小一半） |
| 生命周期 | 跟着窗口 | 独立，活过 GUI |

选独立二进制而不是 `deepterm __daemon` 的三个理由：

1. **生命周期独立**。GUI 崩溃、升级重装、或被 `pkill deepterm` 之类按名字
   杀进程的操作命中时，都不该牵动守护进程——而"活过 GUI 退出"正是它存在
   的全部意义。同一个二进制时进程管理器会把它们当一回事。
2. **不链接图形栈**。共用二进制意味着守护进程也带着 GPUI/wgpu/字体栈，
   它永远用不到，却要为此付出启动时间与常驻内存。
3. **权限与打包边界清楚**。单独一个文件便于设 capability（Windows）
   或放进 systemd user unit（Linux）。

代价是版本一致性要靠约定，所以 `hello` 里**必须**交换协议版本
（`protocol::PROTOCOL_VERSION`）：GUI 与守护进程是两个能分别升级的二进制，
帧格式不变但语义会演进（`attach` 的 `history`、快照里的图像回放）。旧守护
进程会**静默忽略**新字段，用户只看到"回滚没了 / 图片没了"，日志里没有错误。
版本对不上就明确拒绝，并把两边的版本号带回客户端，提示用户完全退出后重开。

**如何找到 `deepcore`**（`daemon::client::daemon_binary_path`）：
`$DEEPTERM_DEEPCORE` → 与 GUI 同目录 → `PATH`。发布包里两个二进制始终并排
（deb 的 `/usr/bin`、macOS 的 `Contents/MacOS/`、Windows 的安装目录），
所以"同目录"覆盖绝大多数情况。

启动时自动拉起：GUI 启动 `deepcore connect`；桥连不上 core 时就拉起
`deepcore`，等 socket 就绪后转发。core 第一次需要终端时再拉起 holder。

**协议**：9 字节头（channel `u32` + kind `u8` + payload 长度 `u32`），与
herdr 的 framed 模式同构。控制面是 channel 0 上的 JSON，终端字节走独立
channel 与 `Data` 帧，避免 base64 膨胀。

### 4.2.1 多 holder、构建指纹与 drain（对齐 herdr）

早期实现固定复用 `holder.sock`，core 升级后仍会连回旧 holder。旧 holder
没有 `Meta` 能力，于是 GUI 重启后终端画面能回来，但 Tab 的程序名、图标和
agent 状态回不来。这与 herdr 的 holder 升级模型不一致：**旧 holder 可以
继续持有旧终端，但新终端必须进入新构建的 holder。**

现在的规则：

- core 启动时扫描运行时目录里的 `holder.sock` 与所有 `holder.<id>.sock`，
  逐个鉴权接管。新 holder 使用随机唯一 socket；旧版固定 socket 仍可接管。
- holder 的 `hello` 返回 `build` / `pid` / `draining`。`build` 是当前可执行
  文件 SHA-256 的前 32 位（同一台机器内比较），因此同一版本号的不同构建
  也能区分。旧 holder 没有 `build`，按空字符串处理。
- 只有 `build == 当前 core 的 build` 且未 drain 的 holder 是 current。新
  终端只交给 current holder；同一构建出现多个 current 时只保留一个，其余
  收到 drain。
- 旧构建或空构建的 holder 仍被接管：它的终端可以 `list`/`attach`/输入，
  但永远不再接收新终端。drain 不结束现有程序，只关闭“接新终端”的入口。
- `tN` 地址由 core 全局分配，而不是每个 holder 从 `t1` 开始。core 保存
  `地址 → holder` 映射与已用编号高水位，所有 `attach`/`write`/`resize`/
  `close` 先查映射再转发；接管旧 holder 时也会据其终端清单抬高水位。

因此升级后的状态是“新旧 holder 并存”：旧终端原地继续跑，新开的终端落到
新 holder 并具备完整 Meta。等旧终端全部退出、旧 holder 空闲退出后，运行
时目录里只剩新 holder。

**能力边界**：旧 holder 的既有终端在升级前没有保存过前台程序与 agent
快照，新 core 无法事后补出这段信息。它们仍能恢复画面和交互，但 Tab 的
程序/状态可能退回 shell 默认值；升级后新开的终端不受影响。

### 4.3 客户端挂接

新增 `Terminal::new_attached(cx, address)`：

- 建立一个挂接会话，拿到字节流；
- 输入走 `remote_input_callback` 那套（已存在）；
- 尺寸变更发 `resize` 帧；
- 断线时进入"重连中"状态而不是"终端已退出"。

**断线语义必须与"程序退出"区分开**——这是 herdr `Stream::Lost` vs
`Stream::Closed` 的区别，也是现在的 `DriverEvent::AbnormalDisconnect` 想表达
但没表达清楚的。挂接式终端的 `Lost` 只是"客户端看不到"，程序还在跑。

### 4.4 会话恢复

`session_state.json` 里每个 `PanelSessionState` 增加一个
`daemon_address: Option<String>`：

- 有它且守护进程里该终端还活着 → 挂接回去，**不重新执行命令**；
- 没有或已死 → 走现在的"按 command/cwd 重建"逻辑，向后兼容。

标签/面板的布局照旧从 `session_state.json` 读；与 herdr 把布局放进 core
不同，deepterm 单机使用，本地文件已经够了，且能省掉 CAS 那一套。

### 4.4.1 Tab 程序与 agent 状态的快照重放

挂接式终端没有 PTY master fd，GUI 自己无法执行 `tcgetpgrp` 或读取前台
argv。这部分状态必须由 holder 持有并通过 `Kind::Meta` 下发。`TerminalMeta`
包含：

- `foreground`：前台进程名；
- `argv`：前台进程完整 argv，用于图标与 agent 识别；
- `agent`：argv 识别出的 CLI agent（识别不到时用 hook 自报兜底）；
- `agent_session`：agent 的 `Working` / `Waiting` / `Done` 等回合状态。

holder 每 500ms 用纯 syscall 探测一次前台进程，只在变化时广播；同时旁路
扫描 PTY 输出里的 OSC 777 hook 事件。**即使没有任何 GUI 订阅者，holder 也
会更新并保存这份状态。** 新订阅者 attach 后，`subscribe` 会先补发一帧
完整 `Meta` 快照，再发送屏幕重绘与增量输出；因此 GUI 重启不需要等 agent
下一次 hook，Tab 就能立刻恢复程序图标和状态点。前台确认回到 shell 时会
同时清空 agent 身份与状态，避免留下永远不灭的状态点。

### 4.5 SSH 远程终端（默认远端 core，旧路径为显式降级）

默认路径与 herdr 同构：本地 core 经 russh 建立 SSH channel，在远端执行
`deepcore connect`，帧协议跑在 stdin/stdout 上。PTY、屏幕历史与程序都由
远端 core/holder 持有，因此本机 GUI/core 重启或 SSH 传输断开后，远端任务
仍在；line 恢复后按原 `<machine>/tN` 地址重新 attach。

- 认证继续使用 DeepTerm 现有的 Keychain/密码、host-key 校验、代理与超时
  配置，不改为系统 OpenSSH 的交互登录。
- 远端 core 由 GitHub Release 的 Linux `x86_64`/`aarch64` musl 产物安装：
  manifest + SHA-256 → SFTP 临时文件 → `--version` 探针 → 原子替换。版本
  不一致自动更新，只停旧 core、不碰 holder。
- Linux 有 systemd user manager 时用 `systemd-run --user` 托管 core/holder，
  并通过 `loginctl enable-linger` 与状态上报处理 SSH 登出后的保活；没有
  user manager 时回退 `setsid`，此时无法证实登出后仍存活，状态上报 `null`。

旧的“本地 holder 的 PTY 里跑 `ssh`”保留为**显式降级选项**。它仍然复用
`SshPtyTerminal::build_ssh_command` 的同一份参数构造（StrictHostKeyChecking
策略、超时、保活、代理、端口转发）；两边各写一份迟早会漂移。该路径的
spawn 请求带 `env`/`remote`：

- `env` 传 `SSHPASS` 等**凭据**。这些值只随这一次请求发出，守护进程交给
  子进程环境后就不再保留，**绝不写进终端记录**（`terminals.json` 是同机
  可读的）。
- `remote: true` 表示"别套本地 shell integration"——远程主机上没有 deepterm
  的集成脚本，注入 `ZDOTDIR`/`ENV` 只会把对方 shell 弄坏。
- 会话状态里 `Ssh` 分支带 `daemon_address`，恢复时优先挂接；接不上才重连
  （旧路径重连会让远端的 `htop` 退出、编译中断）。

远端 core 失败时不会静默切到旧路径：默认路径显示错误，旧路径只能由用户在
标签菜单中显式选择“以旧 SSH 模式重连（不保活）”。

### 4.6 Windows（已实现，未验证）

没有 Unix socket，用**命名管道**（`\\.\pipe\deepterm-<用户名>`）。
选管道而不是本地 TCP 端口：管道可以按用户 SID 设 ACL，别的用户连不上；
`127.0.0.1:port` 对同机所有用户开放，等于把终端交给全机。

PTY 侧不用做任何事：`alacritty_terminal` 的 `Pty` 抽象已覆盖 ConPTY
（现有代码本来就这么跑）。守护进程只为 IPC 换一层传输，见
`daemon/mod.rs` 的 `transport` 模块——两端都实现 `Read + Write`，
分帧代码 (`protocol.rs`) 完全共用。这是刻意的：分帧逻辑跨平台一致，
才不会出现"Linux 上好好的、Windows 上粘包"这种只在一边出现的 bug。

> ⚠️ **`src/daemon/windows.rs` 一次都没有被 rustc 编译过。**
> 开发环境是 Linux，没有 Windows 目标也没有机器。已按 `windows` 0.52
> 的源码逐个对齐 API 签名与 feature 清单，但首次在 Windows 上构建
> 预计还要修几处编译错误（文件头列了具体位置）。这些都是编译期问题，
> 不会静默产生错误行为——跑一次 `cargo check --target x86_64-pc-windows-msvc`
> 就能全部暴露。

一个已知差异：Windows 的命名管道是**阻塞** accept，没有"非阻塞轮询"
这条路，所以那边守护进程不会像 Unix 那样空闲超时退出。对一个"活着才有
意义"的常驻进程来说可以接受，但不能把"空闲退出"当成跨平台行为来依赖。

## 5. 分阶段实施

| 阶段 | 内容 | 可独立验证 |
| --- | --- | --- |
| 1 | 守护进程骨架 + 终端注册 + Unix socket IPC | 起 daemon、跑 shell、cat 回显 |
| 2 | 客户端连接层（自动拉起、重连、协议） | 连上/断线/重连 |
| 3 | `Terminal::new_attached` 挂接式终端 | UI 里能打字、能看输出 |
| 4 | attach 时整屏 ANSI 重绘 + 回滚 | 重开窗口，屏幕原样 |
| 5 | 会话状态外置与自动恢复 | 关 app 再开，标签自动回来 |
| 6 | SSH 终端纳入守护进程 | 远程终端关窗后继续跑 |
| 7 | Windows（Named Pipe） | 平台验证 |

每个阶段结束跑一次全量测试，保持可回退。

## 6. 明确不做的事

- **不做多窗口共享同一终端的接管（attach 冲突）**：herdr 允许多个客户端
  同时 attach 同一个终端（只读 `--observe`）。deepterm 当前阶段先做单客户端，
  写冲突留到后面。
- **不引入 herdr-core 那份 RPC 兼容层**：deepterm 只有自家客户端，
  不需要 generation/method 协商。
- **不改 `session_state.json` 的旧字段含义**：新增字段全部
  `#[serde(default)]`，旧文件照常加载。

## 6.5 一次真实冻结的根因与修复（务必不要再引入）

现象：在 TUI 程序（opencode / nvim）里滚动滚轮，界面**整个冻住**，程序其实
还在跑，日志里也没有错误。

根因不在渲染，而在守护进程的终端线程结构：旧实现把「读 PTY / 喂 VT / 广播」
与「把用户输入写进 PTY」放在**同一条线程**，写用的是无限重试（`WouldBlock`
时 `sleep(1ms)` 再来）。只要进程一时不读 stdin——TUI 正在整屏重绘、`grep`
在跑、`less` 刚启动还没进读取循环——PTY 内核写缓冲就满，这条线程卡在写里，
**连带**它本该做的读也停了：输出再也不被读走，客户端屏幕停在最后一帧。

采样证据（修复前，holder 里的 `daemon-t1` 线程）：

```
777 个样本：703 停在 nanosleep（那个 1ms 重试间隔），72 停在 write
同一时刻 PTY 里还有输出没被读走
```

herdr 从不共享这个结构：holder 里每终端有独立的输入通道 + 写线程，读线程
只 `read`。修复对齐同一分工，但**没有**拆线程——`tty::Pty` 在 Windows 上把
读写端点绑在同一个 `Pty` 里，跨平台拆线程要处理句柄复制。改成：

- 读循环只 `read` + 喂 VT + 广播，**永不阻塞在写上**；
- 输入进 `pending_write`，每轮用 `write` 尽力写一次，`WouldBlock` 就留着；
  有积压时把 poll 超时从 100ms 压到 5ms，保证手感；
- 缓冲上限 256 KiB，程序彻底不读时丢**最旧**的输入而不是输出（输出才决定
  屏幕内容，且这种场景下用户真正在意的是能看见并 Ctrl-C）。

回归测试：`daemon::registry::tests::a_blocked_pty_write_never_blocks_the_reader`
用一个永远 `WouldBlock` 的写端断言函数立刻返回且字节不丢。

## 6.6 滚动 TUI 卡死/错乱的真正机制（第二层）

修完 6.5 的"输入写阻塞输出读"之后，用户复测仍然报告：连续上下滚动
opencode 的文本，界面**完全卡死**，或者**内容错乱**。原因是两个独立的
客户端状态问题，都出在"丢帧"这条路径上。

### 6.6.1 卡死：vte 的同步更新不会自己超时

opencode 每次滚轮重绘会发 **36 个** `CSI ?2026h … CSI ?2026l` 块（实测
一帧约 40 KB）。这是正常的 TUI 做法；问题在于我们的流控在客户端稍慢时
丢的是**按字节量**的一段，很容易正好丢掉收尾的 `?2026l`。

客户端（vte 0.15）一旦停在同步更新里：

- **不解析任何字节**，全部囤进 `sync_state.buffer`；
- `StdSyncHandler::pending_timeout()` 判的是"超时是否被**设置**"，而不是
  "是否已到期"——`set_timeout` 之后它永远返回 true，**这个超时不会自己
  结束**；
- 唯一出口是缓冲里出现 ESU，或缓冲涨到 2 MiB（`SYNC_BUFFER_SIZE`）被动
  溢出。

所以症状正是"界面完全不动，等好一会儿（攒满 2 MiB）突然跳一下"。

**修法**：重绘序列以 `CSI ?2026l` 开头，把积压冲出来。

### 6.6.2 错乱：半截转义序列不会自愈

丢帧是按字节量丢的，可能正好切在 `ESC[38;2` 与 `...m` 中间。之后补发的
整屏快照若直接以 `ESC[!p` 开头，客户端会把两者拼成 `ESC[38;2` + `ESC[!p`
——参数里混进转义，整条序列被吞，后面的重绘内容就错位或残缺。

vte 的 `anywhere` 状态里**只有 `CAN`(0x18) / `SUB`(0x1A) 会无条件把状态机
拉回 Ground**；`ESC` 只是进入 Escape 状态，救不了"参数写到一半"。

**修法**：`?2026l` 之后紧跟 `CAN`。顺序不能反——放在冲刷之前的话，`CAN`
只会跟着一起进 sync 缓冲，冲刷时半截序列照样污染后面的字节。
herdr 的 `paint_state`（`viewers.rs:496`）出于同样理由以 `vec![0x18]` 开头。

### 6.6.3 放大：欠着整屏重绘时不能继续推增量

旧语义是"额度不足 → 丢掉这一段 → 记个标记 → 有额度时补整屏"，但**中间
继续照常推增量**。后果有两个：

- 客户端一边收到基于新状态画的局部更新，一边缺着丢掉的那一块 → 画面长期
  错位；
- 每段被跳过的增量又记一次重绘 → 额度一恢复就连续生成好几帧整屏快照。

而整屏快照是在**连接线程**上序列化的（`grant_credit` 在 `serve_connection`
里），滚轮输入就排在同一线程的读循环后面——于是"卡顿"再叠一层。

**修法**（对齐 herdr 的 `repaint_pending`）：`Subscriber::repaint_pending`
置位期间**增量一律跳过**，只攒"该重绘"这个事实；一帧完整快照成功发出后
才清位、恢复推增量。

### 6.6.4 重绘本身的代价：每格一次 `format!`

采样（holder 的连接线程）：33% 的时间在 `render_screen`，其中大半是
`format_inner` / `String::write_str` —— 旧实现给每个单元格调一次返回
`String` 的 `sgr_for`，一屏 181×43 ≈ 7800 次堆分配。

改成"样式变化时才生成 SGR" + `sgr_into` 直接写 `Vec<u8>`（自己十进制写
`u8`，不分配）。同一份内容的 A/B：

```
旧：单次 1.662 ms
新：单次 0.165 ms   （快 10 倍）
```

回归测试与验证脚本：

- `daemon::terminal::tests::a_repaint_can_rescue_a_client_stuck_mid_sequence`
  ——钉住 `?2026l` 与 `CAN` 的存在与顺序；
- `the_repaint_recovers_both_stuck_states`——用真实 `Term`+`Processor`
  扮演两种坏状态的客户端，断言画面真的恢复；
- `daemon::pump::tests::an_overdrawn_subscriber_gets_no_more_deltas`——钉住
  "欠重绘期间一个字节都不发、额度恢复后补的是快照、之后恢复增量"；
- `scripts/check-scroll-consistency.py`——真实 TUI 上跑三种场景。
  实测**旧代码在第 2 个场景就失败**（卡在 `csi` 状态），新代码全过。

## 6.7 滚动手感的最后一层：输入被 poll 节拍量化了

上一节修完之后，滚动的**正确性**没问题了（不再卡死/错乱），但用户复测
仍然说"非常卡顿"。实测把它量化了出来。

### 6.7.1 症状：与输出大小无关的固定延迟

用一个"收到输入就 echo 一行"的极简程序测端到端延迟：

```
输入 → 首字节输出    中位 67.26 ms（最好 20.11，最差 68.57）
```

这个数字**与输出大小完全无关**——所以不是解析、不是渲染、不是带宽。它
的形状（几十毫秒、抖动大、下限也有 20ms）说明问题在**节拍**上。

### 6.7.2 根因：`write_input` 不唤醒读线程

终端读线程的主体是：

```rust
loop {
    poller.wait(&mut events, Some(timeout))?;   // 空闲时 timeout = 100ms
    while let Ok(msg) = input_rx.try_recv() { ... 写进 PTY ... }
    // 读 PTY 输出并广播
}
```

而 `write_input` 只是往 `input_rx` 里 `try_send` 一下——**它不会唤醒正在
`poller.wait` 的那条线程**。于是每条输入都要等 `wait` 自己超时（空闲
终端 100ms）才被看到。滚轮是高频事件，每一次都被量化成几十毫秒，手感
就是"卡"。

更糟的是这条延迟在整条链上**叠加**：GUI → core 是一跳写，core → holder
又是一跳（`HolderClient::send` 持锁同步 `write_to`，结尾还 `flush`），
两跳都在各自的连接读线程上排队。

### 6.7.3 修法

1. **输入到达即唤醒**：`TerminalHandle` 持有 `Arc<polling::Poller>`，
   `write_input` / `resize` / `request_close` 投递成功后调
   `Poller::notify()`。这是 `polling` 专门为"唤醒 wait 但不产生事件"提供
   的机制，跨平台，不用自己维护 self-pipe。
   有了它，100ms 的空闲超时就与输入响应**完全解耦**——空闲终端照样
   100ms 醒一次省 CPU，输入却立刻被处理。
2. **core→holder 改异步队列**：与 GUI→core 同一套分工（投递进无界队列，
   专用写线程负责写）。旧实现让 core 的**连接读线程**持锁同步写
   holder，holder 一忙就把后面所有输入堵住。

修后同一测量：

```
完整三进程链路（GUI→core→holder→PTY）  中位 0.42 ms（最好 0.28，最差 0.61）
```

**67 ms → 0.42 ms，约 150 倍。**

回归测试：`daemon::registry::tests::writing_input_wakes_the_terminal_thread_immediately`
把 poller 的 wait 放在一条线程上、主线程投递输入，断言 wait 在 60ms 内
返回（去掉唤醒时实测会失败）。这条用例做过双向验证——有修复通过、
把 `self.wake()` 删掉就失败。

## 6.8 "关窗再重开就卡死"：控制回复按 id 分发

用户报的现象很具体：**手动关掉 deepterm 窗口再重开，TUI 界面直接卡死**。
取证结果把范围压得很小：

| 观测 | 结果 |
| --- | --- |
| GUI 主线程 | 停在正常的 `mach_msg` 事件等待，CPU 0.6% —— 没死锁 |
| 守护进程里的画面 | 完整、最新（能看到 opencode 完整 UI 文本） |
| 守护进程是否在输出 | 3 秒 92 KB / 80 帧 —— 一直在输出 |
| GUI 的 `daemon-client-read` 线程 | **整整 2 秒停在 `__recvfrom`，一个字节都没到** |
| GUI 采样里的终端调用 | **0 次** —— 渲染路径根本没被调用过 |

结论：程序活着、守护进程在发，但 **GUI 那条连接收不到数据**。

### 根因：握手回复没有带请求 id

控制回复的分发方式有两处相同缺陷：

```rust
// client.rs（GUI→core）与 core_holder.rs（core→holder）都是这个形状
loop {
    let frame = recv_blocking();            // 共享队列
    if reply.id == my_id { return reply; }
    // 否则：**丢掉**
}
```

两个问题：

1. **并发的 call 互相吞回复**。打开窗口时 `get`/`list`/`attach` 是并发发的，
   B 会把 A 的回复吃掉并丢弃，A 等到超时。
2. **`attach` 超时的后果不是"报个错"**：拿不到 `channel` 就不会调
   `attach_channel` 登记路由。之后 reader 线程收到该 channel 的数据时查不到
   路由，全塞进 `pending`，接收端永远空着 —— 界面冻在最后一屏，与观测完全
   一致（连接在、守护进程在发、GUI 收不到）。

修法：**按请求 id 分发**（`pending_replies: id → 一次性通道`），只保留一个
分发器（reader 线程本身），谁也不会吃谁的回复。

### 修这个洞时踩到的第二个坑（值得记下来）

改成按 id 分发之后，`hello` 的回复**必须带 id** —— 而握手回复原本不带。
于是新 core 连不上一个**正在跑的旧 holder**（它带的旧代码不回 id），报
"鉴权时与 holder 的连接断开"。holder 持有用户的终端，绝不能因为版本差异
被抛弃。

所以分发器还要**兼容不带 id 的回复**：没有 id 就交给唯一/最早的等待者。
控制面是低频请求，同一时刻通常只有一个在等；即使并发，最坏是某次请求拿到
别家的回复并被上层判为无效字段，而不是整条连接不可用。

两处最终形态：
- 服务端：`hello`（成功/版本不匹配/需要先 hello）**全部回填请求 id**；
- 客户端：按 id 分发，**缺 id 时回退到最早等待者**（兼容旧对端）。

实测（真实三进程 + 正在跑的旧 holder）：

```
hello:    {'ok': True, 'protocol': 2, 'id': 0}
已（重新）连接 holder
attach 已由 holder 处理: "t1"        ← 接回了旧 holder 里的终端
t1 history=0:    1.1 ms
t1 history=2000: 1.0 ms
t5 history=0:    0.4 ms
```

回归测试：`hello_replies_must_carry_the_request_id` 与
`concurrent_calls_never_steal_each_others_replies`。

## 6.9 TUI 卡死的最后一层：同步更新没有超时兜底

前几节修的都是"数据没送到"。这一节是**数据送到了、也解析了，但画面不刷新**。

### 症状

重启后打开 opencode，界面直接僵住，但：

- 守护进程在持续输出（实测 2 秒 102 KB）
- 客户端 `daemon-client-read` 正常收到字节
- GUI 主线程停在正常的 `mach_msg` 事件等待，CPU 0.6%
- 采样里 **没有任何 terminal 渲染调用**

也就是"有数据、在解析，但永远不重绘"。

### 根因：vte 的同步更新不会自己超时

opencode 每次滚轮重绘会发 **51 对** `CSI ?2026h … CSI ?2026l`（实测）。
vte 收到 `?2026h` 后**停止解析**，把后续字节全囤进 `sync_state.buffer`，
直到看见 `?2026l`。

问题在于 `StdSyncHandler` 的 `Timeout` 实现：

```rust
fn pending_timeout(&self) -> bool { self.timeout.is_some() }   // 只看"设了没"
```

它判的是"超时**是否被设置**"而不是"是否已到期"——`set_timeout` 之后恒为
true，**这个超时永远不会自己结束**。唯一的内建出口是缓冲涨到 2 MiB
（`SYNC_BUFFER_SIZE`）被动溢出。

所以只要收尾的那个 `?2026l` 丢了（流控丢帧、程序崩溃、写了一半），画面就
永久冻住，后续输出也全看不见。

### herdr 怎么做

`herdr-app/src/vt.rs`：

```rust
const MAX_HOLD: Duration = Duration::from_millis(1000);

pub fn render_hold_deadline(&self) -> Option<Instant> {
    self.shared.held_since.map(|since| since + MAX_HOLD)
}

pub fn frame(&mut self) -> Frame {
    if let Some(since) = self.shared.held_since && since.elapsed() > MAX_HOLD {
        // 强制关掉 SYNC_OUTPUT
        ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_MODE, /* value: false */);
        self.shared.held_since = None;
    }
    ...
}
```

并且 `ingest()` 里给这个截止时刻排了一个定时器，到点 `cx.notify()`。

### deepterm 的修法

三层都要，缺一层就有地方会冻住：

| 层 | 位置 | 做法 |
| --- | --- | --- |
| 客户端（挂接/SSH） | `Terminal::watch_sync_hold` | 每次喂数据后检查 `sync_bytes_count()`；卡住超过 `MAX_SYNC_HOLD`（1s）就 `stop_sync` + 标记脏 + `emit(Wakeup)` |
| 客户端（本地 PTY） | `local_pty::parse_loop` | `recv_timeout(50ms)` 心跳里做同样检查 |
| 守护进程 | `registry::run_terminal` | 每轮轮询里检查；解冻后 `mark_repaint()`，否则 attach 拿到的 `snapshot` 永远是旧的 |

`stop_sync` 会把积压字节**真正解析掉**（它们本来就是程序画的内容，只是被
`?2026h` 拦住了），所以解冻后画面立即正确，不需要等下一次重绘。

取 1 秒的理由：正常 TUI 一帧是毫秒级，1s 足够宽松不会打断合法帧；真卡住时
用户最多等 1 秒，而不是永久。

### 验证

- `a_stuck_synchronized_update_is_released_after_the_hold_limit`：构造半帧
  （只有 `?2026h`），断言超过上限后 sync 缓冲清零**且内容真的画出来**。
  做过双向验证——去掉强制解冻就失败。
- `a_complete_synchronized_update_is_not_disturbed`：成对的 `?2026h/?2026l`
  正常收尾，不留空转计时器。
- 端到端：起一个只发 `?2026h`、永不发 `?2026l` 的程序，等 2 秒后
  `snapshot` 里能看到那段内容（修复前永远是旧画面）。

## 6.10 core 重连 holder 会让所有 viewer 静默失聪

### 症状

重启后打开 opencode，界面卡死。但这次的取证结果与之前几节都不同：

| 观测 | 结果 |
| --- | --- |
| 守护进程是否在发数据 | 2 秒 76 KB，正常 |
| 同步更新（`?2026h`） | 50 对 51 个，**配对正常**，没卡 |
| `snapshot` 内容 | 完整，`2026h` 残留 0 |
| 直接经 core attach 探测 | 2 秒 75 KB，**转发完全正常** |
| GUI 采样里的 terminal 调用 | **0**（4 秒内一次渲染都没有） |
| GUI 的 `daemon-client-read` | 停在 `__recvfrom`，零字节 |

也就是：链路、数据、转发全都正常，**只有 GUI 那条连接收不到**。

### 根因：holder 重连后 channel 号从头开始，而 relay 还在等旧号

core 与 holder 之间只有**一条**连接，而 holder 那条连接上的 channel 号是它
自己 `Connection::next_channel` 分配的——**每条新连接都从 1 重新开始**。

core 里的 `relay_*` 线程握着一个 channel 号在 `recv_blocking` 上等：

```rust
let incoming = holder.attach_channel(holder_channel);   // 等 holder 的 1、2、3…
```

一旦 holder 重连（换了 `HolderClient`，`routes` 是新的 map），旧 relay 等的
那个号在新连接上**要么不存在、要么属于别的终端**。于是它**永远收不到数据**，
GUI 那边就是"界面冻在最后一帧"——而日志里每一步都是 `ok`。

实测这不是边角场景：core 重连 holder 达 **51 次**（holder 忙、空闲退出、
socket 抖动都会触发）。每重连一次，所有已挂接的 viewer 就永久失聪一次。

### herdr 怎么避免

herdr 的重连是按 **holder 实例**管理的（`Link { instance, ... }`），重连会
替换整个 `Link`，viewer 的订阅跟着 holder 一起重建（`runtime.rs` 的
`relink`）。没有"跨重连复用同一个 channel 号"这种做法。

### 修法

早期实现是"每个 viewer 一条 relay 线程 + 全局 generation"，已被 §6.11 的
架构重构取代。现在的机制：

1. core→holder 只有**一条**连接，读取由 `holder-reader` 线程独占；
2. 转发在**这条 reader 线程里就地做**：按 holder 的 channel 号查路由表，
   改写成本连接的 `local_channel` 后投进目标连接的 `outgoing`；
3. holder 重连（换 `HolderClient`）时，`holder()` 调用
   `HolderClient::recover_routes()`：按**地址**把已有挂接全部重新 `attach`
   一遍，拿到新编号后**就地更新路由表**。对上层（GUI）来说 channel 号
   完全不变，只是底层换了条通道。

不再需要 generation：路由表本身随连接对象一起换，没有"旧线程握着旧号"
这种跨连接的残留状态。相应地，曾经的 `holder_generation` 机制与其测试已删除。

### 验证

- 端到端（长连接 + 中途 `kill -9` holder）：

```text
[长连接] attach t1 channel=1
[长连接] 每秒字节数: {0: 4069, 1: 3780, ..., 6: 3570, 7: 3780, ...}
[长连接] holder 重连后（第 6 秒起）收到 22470 字节
```

数据流在 holder 被杀前后**完全连续**——修复前是从那一刻起永久归零。

## 6.11 架构重构：流控收归一处，relay 线程归零

前几节修的都是具体 bug。但它们能反复出现，是因为**同一件事有三个副本**。
这一节是结构性收口。

### 重构前的问题

投递状态散在三处，各自维护：

| 位置 | 状态 | 谁改 |
| --- | --- | --- |
| `Subscriber::credit` | 字节额度 | 终端读线程 |
| `Subscriber::repaint_pending` | 终端线程决定"停发增量" | 终端读线程 |
| `TerminalState::repaint_ids` | 连接层"没额度，等信用再补" | 连接读线程 |

三个标记描述同一件事的三个阶段，却在两条线程上分别置位/清除。这正是
"画面永久停住但日志全绿"的来源：某个阶段忘了清，或者两个阶段的判断
不一致。

此外 `attach` 每次都 spawn 一条 `relay-*` 线程（core 里常驻十几条），
每条都持有一份 `outgoing` 克隆——连接断开时无法靠 sender 计数判断"该退
了没有"，只能等下一次 `try_send` 失败；如果那个 channel 再不产生数据，
线程就永久挂着。

### 重构后

**1. 投递状态只有一个副本**（新模块 `daemon/pump.rs`）

```rust
enum Delivery { Live, NeedsPaint }
```

一个订阅者一个状态机，四个不变量：

- `NeedsPaint` 期间**一个字节都不发**（不是"少发"）；
- 只有整屏快照能把它带回 `Live`（增量补不回丢掉的那段）；
- 状态与前向数据在同一把锁下变更；
- N 个订阅者**只序列化一次**快照（旧实现是每人一份）。

`TerminalHandle` 只持有一个 `Arc<Hub>`，`Subscriber` / `repaint_ids` /
两套重绘判断全部删除。

**2. relay 线程归零**

holder 的 reader 线程本来就阻塞在读 socket 上，让它**顺手完成 channel
改写与转发**，不再为每个 attach 起线程。`Route` 结构里带
`local_channel`（改写成 GUI 认得的号）和 `address`（重连后按它恢复）。

**3. holder 重连的恢复集中到一处**

`holder()` 换连接时调用 `recover_routes()`：按地址重新 attach 一遍，就地
更新路由表。GUI 侧的 channel 号**完全不变**，只是底层换了条通道。

（之前是每条 relay 线程各自发现 `generation` 变了再自己重试，N 条线程
各自试探，且上限是每条 8 次。）

**4. 开闸机制（对齐 herdr 的 `open_paused` / `resume`）**

`attach` 的回复与整屏快照走同一条连接。客户端必须先拿到回复、登记路由，
才能处理数据帧。旧实现靠"客户端自己缓冲"（上限 8 MiB，超了要丢）。

现在服务端在 attach 时把 channel 记进 `Connection::paused`，转发层看到就
把帧攒在 `Route::held`；回复写进 outgoing 之后立刻 `resume` 按序放行。
**数据流在回复之后才开始**，客户端不需要任何缓冲。

这与 herdr 的做法一致（`server.rs:385`：先 `out.control(reply)` 再
`out.resume(channel)`）。

**5. 客户端缓冲收到 256 KiB**

`client.rs::MAX_PENDING` 从 8 MiB 降到 256 KiB（与初始流控额度同量级，
一屏 180×50 的完整重绘约 200 KB 刚好容得下）。超限**整段丢弃**而不是
截断——截断会留下半截转义序列，让后续数据全部错位。

### 收益

| 指标 | 重构前 | 重构后 |
| --- | --- | --- |
| 每 attach 的常驻线程 | 1 条 `relay-*` | **0** |
| 10 个挂接时 core 线程数 | 14+ | **8**（含 accept / holder 读 / holder 写 / 各连接读写） |
| 10 个挂接时 core RSS | — | **9 MB** |
| 欠重绘时的整屏序列化次数 | 每个订阅者一次 | **一次** |
| attach 首屏的客户端缓冲 | 最多 8 MiB | **0**（开闸后直发） |
| 重绘状态副本 | 3 处 | **1 处** |

### 验证

- `daemon::pump` 6 条单测覆盖状态机的每个不变量（含"停发期间零字节"、
  "只序列化一次"、"恢复后立刻推增量"）；
- `scripts/check-daemon.py` 全过（含 history 回放、resize 重绘、额度恢复）；
- `scripts/check-scroll-consistency.py` 全过；
- 长连接 + 中途 `kill -9` holder：每秒字节数**完全连续**
  （`{0:7721, 1:6601, ..., 5:6762, ...}`），旧连接自动恢复。

## 6.12 重启后"标签变成 bash"：PTY 在守护进程里，客户端没有 fd

### 症状

关掉 deepterm 再打开，正在跑 `opencode` 的标签页显示 `bash`，图标也是
shell 的绿色终端方块；程序本身活得好好的（接回来能继续用）。

### 根因

判断"前台跑的是什么"只有一个来源：`tcgetpgrp(master_fd)` 拿到前台进程组，
再读它的 argv。**挂接式终端没有 master fd**——PTY 在 holder 手里。

```text
本地终端:   Terminal.pty_master_fd = Some(fd)   → 查得到
挂接式终端: Terminal.pty_master_fd = None       → foreground_process_name() 恒为 None
                                                  → 标签退回 shell_name() = "bash"
```

而 `foreground_program()`（图标）走的是另一条数据源 `foreground_argv`，
本地探测会填它、挂接式终端没人填——于是文字和图标**各自错**，还互相
不一致。

### 修法：守护进程探，经 `Kind::Meta` 推给客户端

守护进程有 fd，所以由它探。三件事必须同时成立，缺一条标签就还是错的：

1. **探测侧**（`daemon/terminal.rs::probe_terminal_meta`）：
   `tcgetpgrp` → 与 `shell_pgid` 比对 → 读 argv。**只读一次 argv**，
   名字从同一份里取：读两次之间进程可能 exec 成别的程序，于是文字说
   `opencode`、argv 说 `bash`。
2. **传输侧**（`Kind::Meta = 6`）：不消耗流控额度、不受 `NeedsPaint`
   影响——元信息是"标签该显示什么"，与画面错位无关。
3. **客户端侧**（`Terminal::apply_daemon_meta`）：写进与本地探测**同一组
   字段**，于是标签、图标、agent 状态点全都不用区分终端类型。

### 三个容易漏的细节

| 细节 | 不这么做的后果 |
| --- | --- |
| 探测必须在 attach 前就发生过，且快照要**存**在 handle 上、`subscribe` 时补发 | 元信息只在"变化时"广播，新挂接者可以永远等不到第一帧 → 重启后标签一直是 bash（**这就是本次的 bug**） |
| 缓冲（`pending`）必须**整帧**保留，不能只留 payload | 早到的 `Meta` 帧被当成终端数据补发，VT 里直接画出 `{"foreground":...}` |
| 探测**不能 fork** | `ps -o comm=` 在 macOS 上是 fork+exec，五个终端每 500ms 一轮 = 每秒十几次 fork，拖慢读循环。改用 `terminal::foreground_argv`（`KERN_PROCARGS2` / `/proc`，纯 syscall）。`foreground_process_name` 原本在渲染路径里 fork `/bin/ps`，一并换掉 |

另外，挂接式终端上 `TerminalView::refresh_foreground_agent` 直接早退：
客户端没有 fd，派后台任务只会拿到 `Unavailable`，白扫一遍 `sysctl`。

### 验证

- `t4` 前台跑 opencode，接回来后 `Kind::Meta` 报
  `foreground='opencode' argv=['opencode']`，与 `ps` 的
  `ttys003` 前台进程组 23495 一致；
- `daemon::terminal::tests::a_late_subscriber_gets_the_current_meta_snapshot`
  钉住"晚到的订阅者也能拿到快照"；
- `daemon::{terminal,client}::tests::buffered_frames_keep_their_kind`
  钉住"缓冲不能把 Meta 压成 Data"。

agent 状态也属于同一份快照：holder 在无 GUI 时继续解析 OSC 777，
`subscribe` 时把 `agent` 与 `agent_session` 连同 `foreground`/`argv` 一起
补发。回归测试
`a_late_subscriber_gets_agent_identity_and_turn_status` 钉住了
`Working`/`Waiting` 状态不会等到下一次 hook 才出现。

旧 holder 不具备这套 Meta/OSC 记录能力，因此升级前已经在旧 holder 里运行
的终端无法事后恢复准确的程序名与 agent 状态；它们仍可接回并继续运行。
必须新开终端，才能进入新当前构建的 holder 并获得完整恢复能力。

## 6.13 关窗再开就卡死：订阅者从不退订，额度只减不增

§6.11 的开闸改造解决了"首屏能不能到"，这一节是它的**下半场**：首屏到了，
之后就没有了。

### 症状

打开着的窗口一切正常（TUI 滚动、重绘、输入都对）。**关掉窗口再打开**，
屏幕停在最后一屏，进度条纹丝不动；而守护进程侧那个程序活得好好的。

### 复现（隔离目录，不依赖 GUI）

```text
attach history=2000 → 收到 269816 字节（那份快照）
再 0.5 秒          → 0 字节
之后每一轮         → 0 字节
```

快照的大小和内容都正确，所以"快照发过来了"，断的是**之后的增量**。

### 根因：连接关闭时没人退订

`Hub` 里每个订阅者有一份**只减不增**的额度（`INITIAL_CREDIT`）。额度耗尽
时订阅者进入 `Delivery::NeedsPaint`，而**这个状态由 holder 独占**——客户端
根本不知道它存在，发再多 `Credit` 也救不回来（`grant_credit` 找不到那个 id）。

唯一的活路是 `unsubscribe`。而连接关闭时的清理只做了：

```rust
for (address, id) in attachments {
    if let Some(handle) = registry.get(&address) { handle.unsubscribe(id); }
}
```

**`registry` 在 core 里是空的**（PTY 全在 holder）。`attachments` 也是空的
——core 的 attach 分支走的是 `dispatch_with` 的直接转发，从不往里面 push。
于是每次关窗都是在 holder 里留下一具额度已尽的 `Sink`，永不清除。

### 为什么它会让"新"窗口也卡住

重开窗口时 holder 给同一个终端分配**新的 channel 号**（1 → 2 → 3 …），
新订阅者本身是干净的。但：

1. 转发表 `Route` 按 **holder channel** 索引，旧条目没人删；
2. `conn.holder_channels` 是**每条连接**的映射，local 号又都从 1 起；
3. 信用回执按 local → holder 反查，于是全部发往旧 channel。

实测探针（`REGISTER` / `SEND` / `CREDIT` 三类埋点）：

```text
REGISTER holder=1 local=1     ← 第一次开窗
REGISTER holder=2 local=1     ← 重开，holder 换了号
SEND holder=1 ... n=20500     ← 旧通道一直在推
SEND holder=2 ... n=1         ← 新通道只推了 1 帧
CREDIT local=1 → holder=1     ← 信用全发给了旧通道！
```

新通道拿不到信用 → 额度耗尽 → 停在 `NeedsPaint` → 屏幕静止。

### 修法

| 位置 | 改动 |
| --- | --- |
| `connection.rs` 收尾 | `Backend::Holder` 时遍历 `conn.holder_channels`，对每个 holder channel 调 `detach_from_holder` |
| `detach_from_holder` | 三件事一起做：① 发 `detach` 给 holder（它才 `unsubscribe`）；② `drop_route` 剔本地转发表；③ 剔 local→holder 映射 |
| `dispatch_with` 的 `detach` 分支 | 同样走 `drop_route`，不再只删映射（只删映射会让 holder 继续往没人读的 channel 推，额度照耗） |
| `HolderClient::drop_route` | 新增，同时清 `routes` 与 `pending` |
| `Hub::repaint_all` | 顺手修掉 `if !any && sinks.is_empty()` 这个死代码：没有订阅者时不该做整屏序列化 |

### 验证

- 隔离目录连开关 5 次窗口：**每次都稳定 20 MB / 3 秒**（修复前第 2 次归零）；
- 真实路径下对同一终端重开 5 次：每次都 6518 字节且
  `foreground='opencode'`；
- 探针确认信用回执现在跟随新 channel（`local 1 → holder 1/3/4/5`）；
- `daemon/pump` 新增 `an_unsubscribed_sink_stops_being_a_candidate`，
  钉住"额度耗尽且卡在 NeedsPaint 的订阅者，退订后不该再参与任何广播"。

### 教训

`attachments` 那张表**看起来**是订阅台账，实际在 core 侧恒为空——因为
core 不建 PTY、只转发。任何"按台账清理"的代码都必须先确认那张表在
**这个进程**里真的有人写。否则清理是一句漂亮的空话，而泄漏完全静默。

## 7. 实施记录

### 验证脚本

`scripts/` 下每个脚本都对着一个**产品承诺**，不是按函数写的单测。改协议或
改流控时先跑对应的那个，比翻单元测试快。

| 脚本 | 验的是什么 | 需要什么 |
| --- | --- | --- |
| `check-daemon.py` | 关掉客户端程序仍在跑、重连能恢复屏幕、resize 重绘、额度耗尽后能恢复 | 真实 `deepcore` + 真实 PTY |
| `check-scroll-consistency.py` | 三种滚动场景下画面最终收敛（不错位、不残留） | 真实 `deepcore` |
| `check-blocked-input.py` | 程序不读 stdin 时（`yes` 卡住）输出不能被拖停 | 真实 `deepcore` |
| `check-core-upgrade.py` | **只重启 core、不碰 holder** 的升级路径（旧 core + 旧 holder + 活终端 → 换新 core） | 两个真实版本的二进制 |
| `check-kitty-replay.py` | 守护进程侧存下的图像能在 attach 快照里回放，客户端喂进本地 VT 也出现 | 真实 `deepcore` |
| `measure-link-latency.py` | 「输入 → 首字节输出」延迟，把我们的链路成本与 TUI 自身重绘开销分开 | 真实 `deepcore` |
| `measure-scroll-latency.py` | 滚轮报文后守护进程多久开始吐字节、连续滚动时输出是否持续流动 | 真实 `deepcore` + `TARGET` |

统一入口形态：

```sh
cargo build --release --bin deepcore
DEEPTERM_RUNTIME_DIR=/tmp/dt-check ./target/release/deepcore &
DEEPTERM_RUNTIME_DIR=/tmp/dt-check python3 scripts/check-daemon.py
```

### 验证矩阵

| 能力 | Linux | macOS | Windows |
| --- | --- | --- | --- |
| 关掉 GUI 后本地终端存活 | ✅ 实测 | ⬜ 代码同源，未跑 | ⬜ 未编译 |
| 重启后接回同一终端、屏幕保留 | ✅ 实测 | ⬜ | ⬜ |
| 关标签真正结束程序 | ✅ 实测 | ⬜ | ⬜ |
| **core 重启（含 kill -9）后终端存活** | ✅ 实测 | ⬜ | ⬜ |
| 守护进程崩溃后自动重连 | ✅ 实测（kill -9） | ⬜ | ⬜ |
| SSH 远程终端持久化 | ⚠️ 机制实测通过，未连真机 | ⬜ | ⬜ |
| 命名管道 IPC | — | — | ⬜ 未编译过 |

"⚠️ 机制实测"指：在真实 `deepcore` 里用 `remote: true` + env 起了一个
与 `ssh` 同形态的长期进程，验证了输出回传、断开客户端后存活、屏幕恢复、
凭据不落盘。**没有**连过真实 SSH 服务器：本机起的临时 sshd 在沙箱里
拒绝入站连接（`Connection reset`），所以端到端的 SSH 握手没能跑通。

### 阶段 1（守护进程骨架）— 完成

已落地：

- `src/daemon/protocol.rs`：9 字节头分帧（channel/kind/len），channel 0 是
  控制面 JSON，其余是各终端的字节流。带长度上限保护，避免坏帧头撑爆内存。
- `src/daemon/terminal.rs`：单终端状态（VT + 挂接者 + 流控信用）与
  **ANSI 整屏重绘**（`render_screen`）。
- `src/daemon/registry.rs`：终端注册表、地址分配（`t1`、`t2`…，只增不复用）、
  每终端一个 poller 线程（读写 + 子进程事件，照抄 alacritty 自己的模型——
  拆成读写两线程会让阻塞读把写饿死）。
- `src/daemon/connection.rs` / `server.rs`：控制请求分发。
- `src/daemon/client.rs`：客户端连接，含"没在跑就拉起"。
- `src/daemon/listen.rs`：accept 循环。
- `src/main.rs`：隐藏子命令 `deepterm __daemon`，在日志初始化之后、
  GPUI 启动之前拦截。

**端到端验证**（`scripts/check-daemon.py`，真实 PTY + 真实 shell）：

```
spawned t1
shell responded (30 bytes)
client disconnected (simulating window close)
terminal survived the disconnect
screen recovered on reattach
reattached terminal still interactive
ALL CHECKS PASSED
```

即：断开客户端后终端仍 live，重连时 `snapshot` 能还原断开前的屏幕，
且程序依然接受输入——这正是需求里"关掉 deepterm 终端不死、重开恢复"
的最小闭环。

### 一个验证陷阱（记录以免重复踩）

在受限沙箱里让守护进程与子进程"跨调用存活"是**验证不了**的：沙箱用
`bwrap --unshare-pid --die-with-parent` 跑每条命令，工具调用结束时
整棵进程树被回收，看起来像"守护进程死了子进程也跟着死"。

实际用 `/proc/<pid>/stat` 核对过子进程归属：

```
child : ppid=11 pgrp=329 session=11
daemon: ppid=2  pgrp=5   session=5
```

子进程的父进程是 alacritty 建的那层 shell（pid 11），**不是守护进程**，
且 `session` 与守护进程不同。所以真实环境下守护进程崩掉不会带走终端，
这也是与 herdr 的 holder 模型一致的性质。

要在沙箱里验证"守护进程被 SIGKILL 后终端仍在"，需要把守护进程与测试
放进**同一次**工具调用里跑。

## 7.5 core / holder 两进程（对齐 herdr）

最初的实现让 core 亲自建 PTY，于是**core 就是终端进程的父进程**——core
一重启（升级、崩溃恢复），终端跟着死。实测 `ppid` 指向 core，core 退出
后程序立刻消失，这与 herdr 的 "programs keep running" 正好相反。

herdr 的解法是两个进程：

```
deepcore                  core：协调、跑 UI 协议、转发
  └─ deepcore --holder    holder：PTY 的父进程，只负责"别死"
       └─ 终端程序
```

core 把 `spawn`/`attach`/`close` 转给 holder，输入、尺寸、输出原样双向
转发。**core 在这里只是一根管子**：不做缓冲、不改内容——终端字节本来就
该端到端保持原样（任何"顺手处理"都可能破坏转义序列）。

holder 用 `pre_exec(setsid)` 脱离 core 的会话。只脱离会话还不够，父子
关系还在；实测 `kill -9` core 后 holder 与终端都活着，这正是要的性质。

两个进程各用各的 socket 与日志：

| | core | holder |
| --- | --- | --- |
| socket | `daemon.sock` | 旧版 `holder.sock`；新版 `holder.<id>.sock` |
| pid | `daemon.pid` | 与 socket 同名，如 `holder.<id>.pid` |
| token | `daemon.token` | 与 socket 同名，如 `holder.<id>.token` |
| 日志 | `deepcore.log` | `holder.log` |

这三个"按 socket 名区分"的文件都曾经是共用的，每一个都造成过真实故障：

- **pid** 共用：后启动的覆盖先启动的，按那个文件去 kill 杀的是另一个
  进程（实测中招：以为在杀 core，实际杀的是 holder）。
- **token** 共用：holder 启动时写入自己的 token 覆盖 core 的，core 拿被
  覆盖前的旧值去连 holder 必然被拒。
- 连带 **holder 客户端漏了鉴权握手**：连接建立后直接发 `spawn`，服务端
  要求第一帧是带 token 的 `hello`，于是每条请求都被拒——而连接本身是
  "成功"的。

第三个问题的表现最阴险：`spawn` 返回成功、`list` 也能看到终端，但
**holder 里是空的**——终端其实还是 core 自己建的，core 一重启照样死。
"对齐 herdr"只是看起来像。要发现它，必须直接问 holder "你有几个终端"。

Linux 实测：

```
core=5  holder=10  prog=14 (ppid=10)
kill -9 core  → holder 存活 YES，程序存活 YES
新 core 启动  → 看到 t1 (live) → 旧屏幕在 → 仍可交互
```

## 8. 生命周期语义（重要）

这一节记录两个**必须分开**的概念，混在一起的代价是数据丢失。

| 事件 | 语义 | 对守护进程里的程序 |
| --- | --- | --- |
| 关闭标签 / 关闭面板 | 用户**不要这个终端了** | 结束它（`close` 操作） |
| 退出 deepterm（视图全部销毁） | 只是**不再观看** | 什么都不做，继续跑 |
| 连接断开（守护进程崩溃） | 我们看不到了 | 尝试重连；接不上才提示 |

代码上的落点：

- `Terminal::kill()` = 用户不要它了 → 挂接式终端调 `daemon::session::close_terminal`。
- `Terminal::drop()` = 视图销毁 → 挂接式终端**直接返回**，不碰程序。

为什么不能只留一个：`kill` 一律结束程序的话，退出应用时每个视图都会
drop，等于每次退出都杀掉用户所有终端，功能整个作废；反过来 `kill`
一律不管的话，关标签后终端永远留在守护进程里，跑着的程序再也没人管。

`remove` 与 `close` 也是刻意分开的：`remove` 只清**已经结束**的记录
（打扫），拿它杀活终端会失败——一次误调用就干掉用户的编译任务。

## 9. 故障恢复

守护进程崩溃或被更新时：

1. reader 线程退出 → **清空路由表**，让挂接方的 `recv()` 立即返回
   `Err`（只置标志唤不醒阻塞中的 `recv`，终端会静默冻在最后一屏）。
2. 泵任务看到断开 → 等 300ms（给守护进程起来的时间）→ 重新 `attach`。
3. `client()` 发现连接已关闭 → 重建连接，顺带把守护进程拉起来。
   （这里曾用 `OnceLock`，它只写一次，导致"守护进程重启了却永远接不
   回来"——现在换成 `Mutex<Option<..>>`。）
4. 重连后地址不存在（守护进程重启过，终端记录随进程一起没了）→
   明确报"没有这个终端"，并在屏幕写一行提示，而不是静默冻住。

`handle_connection_lost` 与 `handle_program_exit` 是**相反**的语义：
前者是"我们看不到了，程序可能还活着"，后者是"程序真的退出了"。
搞混会让用户以为任务死了从而重启一遍，而原来的还在后面跑。

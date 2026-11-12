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
唯一需要额外处理的是 kitty 图形（`Term` 里存的是图像 id，要单独发）。

### 4.2 进程与 IPC

```
deepterm (GUI)                    deepcore (守护进程)
    │  Unix socket: $XDG_RUNTIME_DIR/deepterm/daemon.sock
    │  Windows: \\.\pipe\deepterm-daemon
    ▼
    └──────── 9 字节头分帧 ────────▶ 终端注册表
                                       └── 每终端一个 Pty + Term + poller 线程
```

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

代价是版本一致性要靠约定：GUI 连上时用 `ping` 比对版本，对不上就重拉。

**如何找到 `deepcore`**（`daemon::client::daemon_binary_path`）：
`$DEEPTERM_DEEPCORE` → 与 GUI 同目录 → `PATH`。发布包里两个二进制始终并排
（deb 的 `/usr/bin`、macOS 的 `Contents/MacOS/`、Windows 的安装目录），
所以"同目录"覆盖绝大多数情况。

启动时自动拉起：连不上就 `Command::new(current_exe).arg("__daemon")` 后台起，
再重试连接。

**协议**：长度前缀帧（4 字节小端长度 + JSON），与 herdr 的 framed 模式同构。
每个终端一条逻辑 channel，二进制数据走独立的帧类型避免 base64 膨胀。

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

### 4.5 SSH 远程终端（已实施）

远程终端本质就是"PTY 里跑 `ssh`"，所以**不需要新机制**：守护进程照常
起一条 PTY，只不过里面跑的是 `ssh` 而不是 shell。

- `SshPtyTerminal::build_ssh_command` 被抽成独立函数，进程内路径与守护
  进程路径共用同一份参数构造（StrictHostKeyChecking 策略、超时、保活、
  代理、端口转发）。两边各写一份迟早会漂移，表现成"同一台主机在持久化
  模式下连法不一样"。
- spawn 请求新增 `env` 与 `remote` 两个字段：
  - `env` 传 `SSHPASS` 等**凭据**。这些值只随这一次请求发出，守护进程
    交给子进程环境后就不再保留，**绝不写进终端记录**（`terminals.json`
    是同机可读的）。
  - `remote: true` 表示"别套本地 shell integration"——远程主机上没有
    deepterm 的集成脚本，注入 `ZDOTDIR`/`ENV` 只会把对方 shell 弄坏。
- 会话状态里 `Ssh` 分支也带 `daemon_address`，恢复时优先挂接；接不上
  才重连（重连会让远端的 `htop` 退出、编译中断）。

**没有做的事：向远端部署 `deepcore`。** herdr 的做法是远端跑它自己的
core（`hc machine add` 会用 `curl | sh` 装一个），那样终端进程才真正在
远端机器上。deepterm 现阶段不这么做，理由：

1. 没有发布站，`curl | sh` 这条路不通；
2. 传自编译二进制要面对 glibc 兼容（本机 2.39 vs 远端 2.35 起不来），
   得先做 musl 静态构建；
3. 收益只在"本机断网后远端进程仍要活着"时才体现。

本方案下关闭 deepterm 不中断 SSH 连接，远端的 `htop`、长编译照常跑——
"关掉 GUI 终端不死"这条需求已经满足。

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

## 7. 实施记录

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

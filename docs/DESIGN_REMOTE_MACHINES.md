# 远程机器方案（对齐 herdr：远端跑自己的 core）

> 调研对象：`~/Documents/workspace/herdr-core`（`hc`，protocol generation 1）、`~/Documents/workspace/herdr-app`
> 现状基线：deepterm `68dee25`（core/holder 两进程、本地持久化已实测）
> 状态：**阶段 0–5 已完成，阶段 6 的 Linux musl 发布构建已落地，并在
> `lingsi-ubuntu` 真机通过完整 E2E**（2026-11-17）。后续章节保留立项分析，
> 与本节冲突时以本节为准。

## 0. 已批准决策与当前进度

用户已批准以下约束，优先级高于本文后面形成的早期建议：

| 项目 | 最终决策 |
| --- | --- |
| SSH 认证 | **保留 DeepTerm 现有 Keychain / 密码 / host-key 校验路径**，复用 `src/ssh` 的 russh 栈；不改用 herdr 的系统 OpenSSH 交互登录 |
| 发布与更新 | **与 herdr 一致，依托 GitHub Releases**：release manifest、SHA-256、按平台构建的 core artifact、更新检查与下载 |
| Linux 产物 | 至少提供 `x86_64-unknown-linux-musl` 与 `aarch64-unknown-linux-musl`，避免 Ubuntu glibc 差异 |
| 旧 SSH 路径 | **默认走远端 core**；旧“本地 PTY 里跑 ssh”的路径保留为显式降级选项，失败时不得静默回退 |
| 远端保活 | 按 herdr 方案：core 重启不杀 holder；处理 SSH 断线重连、systemd logout、`loginctl enable-linger` 与 `keeps_running_after_logout` 上报 |
| 传输 | app 只面对 `deepcore connect` 的 stdin/stdout；本地与远端共用同一帧协议 |
| token | 本地阶段 `connect` 保持透明中继，不替 app 完成 `hello`；远端阶段如何在 SSH 已认证会话内传递 token 单独实现，不能用“跳过鉴权”换便利 |

### 已完成（阶段 0–5，本地与真实远端均已验证）

- `hello` 增加并校验 `version` / `core_id` / `generation` / `hostname` /
  `methods` / `capabilities` / `keeps_running_after_logout` /
  `logout_linger_user`；协议号仍为 `2`，因此新 core 仍能接上旧 holder。
- 新增 `deepcore connect`：在 stdin/stdout 与本地 core socket 之间原样转发
  帧，stdout 不输出任何日志或提示；stdin EOF 时半关闭写端。
- app 的 `Client` 改为启动 `deepcore connect`，复用原有 reader/writer、
  pending-by-id、channel 路由与重连逻辑；`connect_existing()` 保留直连
  socket 路径给测试和显式兼容场景。
- core 在 `bind` 前写 token；app 握手在桥启动窗口内等待 token，修掉冷启动
  竞态。
- 验证：`cargo test --lib daemon::` 115 项通过；`tests/deepcore_bridge_e2e.rs`
  真实拉起隔离 core，经桥完成 `hello` + `ping` + `list` + `spawn` +
  `attach` + 终端输出，随后断开桥、重连并确认同一终端仍在；`cargo check
  --bin deepcore --bin deepterm` 通过。
- 机器表 `machines.json` 与每台机器一条 SSH line 已落地。line 复用
  `src/ssh` 的 russh 认证、Keychain/密码、代理与 host-key 校验，不调用系统
  `ssh`。断线按 250ms→30s 退避重拨；认证失败走 120s/300s/600s 慢重试。
- **core 身份落在持久目录**（`$XDG_STATE_HOME/deepterm/core-id`，兜底
  `~/.local/state/deepterm/core-id`），不再放 `$XDG_RUNTIME_DIR`。后者在
  Linux 上是 tmpfs，重启 / `RemoveIPC` 会清空，core 换了 id 之后本机记着的
  `remote_core_id` 永远对不上，机器永久卡在“另一个 core”。升级时若旧位置
  还有身份文件，会沿用它的值迁移过去，不换 id。socket 仍留在运行时目录。
- **身份变化不再和版本不兼容混为一谈**：`hello` 的 `core_id` 与记录不一致时
  上报 `kind: rebuilt`（而不是 `incompatible`），监督线程立刻返回而不是等
  满 30 秒超时；客户端据此清掉同目标的旧机器条目再重新添加，新条目从
  `remote_core_id: None` 开始，首次握手即记下对端当前身份。`incompatible`
  仍然保留给“版本不一致，自动更新远端 core”这条路径。
- 远端地址映射为 `<machine>/tN`；`spawn/list/attach/resize/write/close/remove`
  都经 line 路由。远端 attach 的 channel 在本机按 generation 重新登记，
  core 重启、line 重拨后会自动重挂同一地址。
- 默认 SSH 标签走远端 core：PTY、屏幕历史与程序都在远端 holder；本地只
  中转帧。`hello` 的 `core_id/generation/hostname/methods/capabilities/
  keeps_running_after_logout/logout_linger_user` 会落进机器表。
- 协议兼容按能力拆分：基础终端方法仍是必需项，`machines.v1`、`machine.*`
  是可选能力；旧 holder 没有这些能力仍能连接（有单测钉住）。
- 旧“本地 core 的 PTY 里跑 ssh”路径保留为**显式降级**：新增
  `TerminalView::new_persistent_legacy_ssh`，会话文件记录 `legacy_ssh`，
  标签右键菜单提供“以旧 SSH 模式重连（不保活）”。默认路径失败只显示错误页，
  不静默切换；错误页的 Reconnect 只重试远端 core，旧路径断线也只会按旧路径
  重连。首次建立机器连接（SSH 握手、认证、安装/启动远端 core）最多等待 60
  秒，app→本地 core 的 spawn 请求等待 70 秒；20 秒的协议握手超时只约束已经
  建立的 line，不能拿来判断首次启动失败。
- macOS `SshReconnect` 事件增加模式守卫：远端 core 会话的异常断开交给机器
  supervisor，不会被误当成旧 SSH 标签切走。
- GitHub Release 增加 Linux `x86_64` / `aarch64` musl core 产物、
  `deepcore-manifest.json` 与总 `SHA256SUMS.txt`。CI 使用 Rust 自带 musl
  self-contained 库链接、`strip --strip-all`、`readelf` 静态链接门禁与
  128 MiB 大小门禁；manifest 由 release job 根据实际归档 SHA-256 生成。
- 远端安装走完整发布链路：读取 manifest → 按 `uname -sm` 选 target →
  下载归档 → 校验 SHA-256 → 严格解包唯一 `deepcore` → SFTP 上传临时文件
  → 远端 `--version` 探针 → 原子 `mv -f`。失败不覆盖旧二进制；更新时只停
  旧 core，不碰 holder，同一终端在新 core 起来后继续 attach。
- `core_missing` 自动安装、握手版本不一致自动更新，以及显式
  `machine.install` / `machine.update` 都已接入。自动安装/更新失败停在
  `install_failed` / `incompatible`，不会静默回退旧 SSH。
- **多 holder 接管（2026-11-17）**：core 启动时扫描 `holder.sock` 与所有
  `holder.<id>.sock`，全部鉴权接管。holder `hello` 带
  `build`/`pid`/`draining`；`build` 是同一台机器内可执行文件 SHA-256 的
  前 32 位。只有同 build 且未 drain 的 holder 接新终端，旧 build / 旧版
  空 build 只继续服务已有终端。`tN` 编号由 core 全局分配，地址到 holder
  的映射统一路由，避免多个 holder 同时生成 `t1`。
- **Tab Meta 快照重放**：holder 保存 `foreground` / `argv` / `agent` /
  `agent_session`。前台进程每 500ms 探测一次；OSC 777 在 PTY 输出路径上
  旁路记录，即使 GUI 已关闭也不会丢。新订阅者 `attach` 时先收到完整 Meta
  快照，再收到屏幕重绘与增量输出，因此 GUI 重启后程序图标、agent 身份与
  `Working`/`Waiting` 状态无需等待下一次 hook 即可恢复。前台回到 shell
  时清空 agent 状态。
- Linux 有 systemd user manager 时，core 与 holder 都由
  `systemd-run --user --collect --property=KillMode=process` 托管；否则回退
  `setsid`。linger 通过 marker / `loginctl` 探测，首次不满足时后台尝试
  `loginctl enable-linger`；状态每 60 秒刷新，连接期间由 `server.status`
  每 3 秒同步到机器表。无法证实时上报 `null`，不误报 `false`。
- Linux keyring 后端改为纯 Rust `async-secret-service`（zbus + crypto-rust），
  保留现有同步 `Entry` API、Keychain/密码语义，同时让静态 musl core 不再
  依赖系统 `libdbus`。
- 最终验证：`cargo test --lib` 2484 passed / 3 ignored，`cargo check --bin
  deepterm --bin deepcore` 通过；`remote_machine_e2e` 1 passed（79.52s）。
- 真实 `lingsi-ubuntu`（`192.168.194.4`，`zhousong`）测试覆盖：
  1. 远端输出正常；
  2. 无 core 时从测试发布站自动安装，manifest / SHA-256 / SFTP 全链路通过；
  3. 强制更新：远端 core PID 变化、holder PID 不变，同一终端仍可 attach；
  4. 故意破坏 manifest SHA-256：安装失败，旧 core 校验和与终端不受影响；
  5. 杀掉远端 core、保留 holder：新 core 自动拉起，holder PID 不变，
     同一 `<machine>/tN` 可重新 attach；
  6. 切断并恢复 SSH TCP 传输：line 自动重拨，同一终端可重新 attach；
  7. 杀掉本地 core/holder 再启动：机器表仍在，远端 core/holder 与终端仍在。
  测试用 `session=e2e-*` 隔离运行时，不触碰用户已有的生产 core/holder；
  `remote_machine_e2e` 最终结果为 1 passed，耗时 79.52 秒。
- 多 holder + Meta 重放另用真实旧 holder 二进制在 `lingsi-ubuntu` 的隔离
  运行时完成 E2E：旧 holder 持有 `t1`，新 core 接管后旧屏幕可恢复；新 core
  另起唯一 socket 的 holder 并创建 `t2`；`t2` 前台运行 `codex` 时，
  `attach` 收到
  `foreground="codex"`、`argv=["codex","30"]`、`agent="Codex"`、
  `agent_session.status="working"`、`session_id="s-remote"`。模拟 GUI 关闭
  后重新 attach，同一 Meta 快照再次重放；同时存在 `holder.sock` 与
  `holder.<id>.sock`，两个 holder 均存活。测试脚本为
  `/tmp/dt_multi_holder_test.py`，输出为 `/tmp/dt_multi_holder.out`。
- 测试入口：

  ```bash
  ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes \
    -L 127.0.0.1:22222:192.168.194.4:22 zhousong@192.168.194.4

  DEEPTERM_TEST_SSH_HOST=127.0.0.1 \
  DEEPTERM_TEST_SSH_PORT=22222 \
  DEEPTERM_TEST_SSH_USER=zhousong \
  DEEPTERM_TEST_SSH_REMOTE_HOST=192.168.194.4 \
  DEEPTERM_TEST_SSH_REMOTE_PORT=22 \
  DEEPTERM_TEST_CORE_ARTIFACT=/tmp/deepcore-linux-x86_64.tar.gz \
  cargo test --test remote_machine_e2e -- --ignored --nocapture
  ```

  测试内部还会再起一个可控 TCP 中继，并在中途切断它来验证 SSH 传输恢复。
  macOS 对未签名测试二进制的局域网直连有限制，所以测试进程通过系统
  `ssh -L` 隧道访问；russh 仍在隧道上完成真实 SSH 握手与主机密钥校验。

### 当前边界

- 机器表已返回 `keeps_running_after_logout` / `logout_linger_user`，但机器
  列表 UI 的保活警示样式仍属后续界面工作。
- 升级前已存在于旧 holder 的终端没有历史 Meta/OSC 状态，无法事后补出准确
  的程序名、图标与 agent 回合状态；它们仍可接回、恢复屏幕并继续交互。要
  获得完整 Tab 恢复，必须在升级后的新 holder 中新开终端。
- 多 holder 并存期间旧 holder 只 drain、不接新终端；等旧终端自然结束且
  holder 空闲退出后，运行时目录才会回到单 holder 状态。
- 没有 systemd user manager 时只能回退 `setsid`；该路径能活过 core 重启，
  但无法仅靠程序保证活过用户登出，此时 linger 状态保持 `null`（未知）。
- Windows 远端、远端文件/LSP/Git/端口转发的跨机路由仍不在本期范围。
- GitHub Actions 的发布流水线已实现；只有推送 `v*` tag 后才会产生真实
  Release 资产，日常开发与测试可用 `DEEPTERM_RELEASES_URL` 指向夹具。

### 下一步

阶段 5、musl 发布构建、多 holder 升级与 Meta 快照重放已完成。后续是机器
列表 UI 的保活提示，以及远端文件/LSP/Git/端口转发等跨机能力。

## 1. 需求与验收口径

用户原话："deepterm 连接打开远程 linux 服务器方案也要和 herdr 保持一致。"

把这句话落成可验收的四条（优先级从高到低）：

| # | 验收项 | 判定方式 |
| --- | --- | --- |
| R1 | 远端的终端程序**跑在远端**，本地断网/关 app/本机重启都不影响它 | 本机 `kill -9` GUI 与 core 后，远端 `htop`/编译继续；重连能接回同一进程 |
| R2 | 同一台机器上的多个终端标签**共享一条连接**，不是每个标签一条 SSH | 远端 `ps` 只有一个到该机的传输进程；开 10 个标签仍是 1 条 |
| R3 | 远端终端出现在**同一个列表/同一套事件流**里，地址形如 `box/t3` | 本地 `list` 一次返回本地 + 各机器终端；增删改按序推送，不靠轮询全量 |
| R4 | 本机**不因为远端慢/掉线而卡住**：掉线只是"看不到了"，不是"终端没了" | 远端 `SIGSTOP` 时本地 UI 仍响应；`list` < 3s 返回，掉线机器显示 unreachable 而不是等待 |

明确**不做**（与 herdr 对齐后仍属于其能力、但本期不做）：agent 跨机父子关系与 `die_with_parent`、`--via` 自定义连接命令、Windows 远端、远端文件 API（`file.read/write/edit`）、`terminal.put`、模板。

## 2. herdr 的做法（逐层拆解 + 证据）

### 2.1 三侧结构

```
herdr-app (GPUI 窗口)
   │   fork 一个 `hc connect` 子进程，在其 stdin/stdout 上走 framed 协议
   ▼
本地 core (hc serve)                     ← 持"机器表 + 每台机器一条 line + 远端镜像"
   │   ssh <target> sh -c 'exec herdr-core connect'
   ▼
远端 core (hc serve)  ← 持"自己的终端、屏幕、文件"
   └── holder ── PTY ── 程序
```

关键点（herdr-core `README.md:1-6`、`docs/machines.md:139-146`）：**本地 core 只保留连接与镜像，远端 core 拥有程序、历史、文件。** 窗口关闭 → `hc connect` 退出 → 本地 core 与远端 core 都活着 → 程序不受影响。

### 2.2 传输：所有连接都是"同一帧协议"，只是承载不同

herdr 只有**一套** wire 格式（`crates/herdr-core/src/wire.rs:1-33`）：`[channel u32 LE][kind u8][len u32 LE][payload]`，`kind` 为 `DATA/CREDIT/CLOSE/RESIZE/FOCUS/FACTS/AUTOMATIC_INPUT`，`WINDOW=256KiB`、`MAX_PENDING=4MiB`、`MAX_CONTROL=64MiB`。

三种承载：
1. 本地 Unix socket / 命名管道（`paths.rs:26-35`）；
2. **`hc connect`：把同一协议跑在 stdin/stdout 上**（`cli.rs:104-110`、`cli.rs:697-726`）——这是跨机最关键的一环，它让"远端 core"变成"一条管道后面的 core"；
3. 远端：`ssh <target> sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; … exec "$p" connect'`（`machines.rs:245-266`）。

`connect` 的职责极薄：连本地 socket，然后**原样双向拷贝字节**，只在 plain 模式下数字符（`cli.rs:697-750`）。它不理解业务。

### 2.3 加机器 = 一次握手 + 落盘

`machine.add`（`api.rs:636-697`）：
1. 归一化 target（`can@host:2222` → `ssh://can@host:2222`，`machines.rs:110-122`）；
2. 名字：缺省从主机名清洗而来，截 32、冲突加 `-2`（`machines.rs:127-149`）；
3. **拨一次号并 `hello`**：拿到远端 core 的 `core.id`（32 hex）、`core.name`（主机名清洗）、`version`、`keeps_running_after_logout`、`logout_linger_user`（`machines.rs:296-424`）；
4. 落盘 `machines.json`（`machines.rs:100-105`）；
5. 拒绝"加到自己"（`id == core.info.id`）、拒绝重复 id（另一个 target 应答同一 core → `already_added`，提示"像是一份拷贝"）（`api.rs:672-691`）。

### 2.4 line 的生命周期（这是整个方案的骨架）

`remote.rs::supervise()`（`remote.rs:266-328`）：

- `dial()` → `hold()` → 掉线后**立刻重拨**（多数掉线是远端重启，等待中的调用应该等新 line）；
- 退避：250ms 起 ×2，上限 `HC_RECONNECT_MS`（默认 30s），连上重置（`remote.rs:224-226`、`remote.rs:326-327`）；
- `needs_login`：**120s → 300s → 600s**，不走普通退避（防 fail2ban/CrowdSec 封人；agent 后来解锁能自愈）（`remote.rs:311-321`）；
- `incompatible`（`unsupported_generation`）→ `needs_update`，提示 `machine update`（`remote.rs:323`、`machines.rs:407-412`）；
- **静默判据**：`silence = machine_timeout(默认30s) + heartbeat_every(默认15s)`，且用 `wire::Heard` 在 `poll_read` 层按"真实字节"计时——**一个 16MB 大帧在慢链路上慢慢流不算静默**（`wire.rs:65-114`、`config.rs:69-71`、`server.rs:274-277`）。对应测试 `tests/remote.rs:242`（320KB/s + `machine_timeout=2` 下 1MB 输出完整到达）。

掉线时对已有 viewer 的处理：**不关闭 viewer**，只往它的本地 channel 写 `"\x18\r\n[line to <m> lost; reconnecting...]\r\n"`——CAN 在前，因为断线可能落在转义序列中间（`remote.rs:454-470`）。同时 `line.close()` 让所有 pending 调用立刻以 "lost the line" 失败（不是等到 deadline）。子进程用 `start_kill()` 而不 `wait()`（一个 mac core 曾因等旧 ssh 变成永久 connecting，`remote.rs:462-464`）。

### 2.5 镜像：远端终端如何进入本地同一个列表

- line 连上就发 `events.subscribe`，reply 里有 `channel` + `terminals` 快照（`remote.rs:359-404`）；
- `mirror_replace()`：逐条 `localize_in()`（`t1` → `rohan/t1`），与旧镜像 diff，消失的发 `removed`、新增/变化的发 `created`（`remote.rs:472-490`）；
- 之后按事件增量更新（`created/restarted/removed/status/turn/title/labels/keep/size/foreground/finished/lost`）（`remote.rs:535-590`）；
- 远端若丢事件会发 `missed`，本地转成"重放镜像快照 + 给文档订阅者发本地 `missed`"（`remote.rs:513-533`）；
- 为什么不在 `list` 时去问远端：本地事件流需要**一条有序的本地 seq**，订阅过滤（terminal/labels/children/docs）本来就在本地求值，且掉线机器不能拖慢 `list`（`api.rs:279-296` 对每台机器并行且单台 3s 上限）。

### 2.6 viewer 中继：跨机的 attach 怎么不出错

`remote/viewers.rs::attach()`（`viewers.rs:88-124`）：
1. 本地先开一个 channel 并 `open_paused`——**在远端 reply 装好映射之前，先到的输出被排队而不是丢弃/错投**，reply 送出后才 `resume`；
2. 调远端 `terminal.attach`，把回复里的 `channel` **改写成本地 channel**；
3. 记录 `Relayed{line, there, conn, params, active_at}`；
4. 能力协商：`resize_notifications` 要 `observe && 请求了 && 远端答应` 三者都成立；`automatic_input` 要 line 的 feature 与回包都为真。

断线重连后 `reattach()`（`viewers.rs:23-73`）：对每个 `machine == m.id` 的 `Relayed`，**沿用原本地 channel** 重新 attach，`history=0`（屏幕会重绘，viewer 已有历史）、`resumed=true`（"回来"不算活跃，不抢尺寸），按 `active_at` 升序使"最后活跃的 viewer"成为远端 leader。

`RESIZE_LOST` 必须 `close` 而不是 `finish`（`viewers.rs:64-66`）：降级路径下已排队的画帧是按旧尺寸契约生成的，送出去就是错尺寸脏画面。

### 2.7 远端进程托管：怎么活过"最后一次登录结束"

`platform/systemd.rs`：只在真有 user manager 时（`XDG_RUNTIME_DIR == /run/user/<uid>` 且 `/run/user/<uid>/systemd/private` 存在）用 `systemd-run --user` 起 core/holder（`systemd.rs:58`、`systemd.rs:103`、`systemd.rs:244-277`）。unit 名 `hc-serve-<8hex>`，`--collect`、`KillMode=process`、`RuntimeDirectory=<unit>` 0700、`EnvironmentFile` 0600、`StandardError=append:<log>`。

linger：先看 `/var/lib/systemd/linger/<user>` marker；没有就**每进程一次**在独立线程里 `loginctl --no-ask-password enable-linger -- <user>`（polkit 可能卡 25s，不能挡启动）（`systemd.rs:137-141`、`systemd.rs:426-457`）。状态 `(Option<bool>, Option<String>)` 每 60s 重算：只有能证实"linger 关 **且** `UserStopDelayUSec` 是有限值"才敢断言 `false`，读到 `18446744073709551615`（无限）返回 `None`（`systemd.rs:330-341`）。本机把这两个字段带进 `hello`/`server.status`，`machine.add` 抄进 `machines.json`，连上后每 3s 刷新（`api.rs:186-201`、`api.rs:669-670`、`remote.rs:331-341`）。

### 2.8 安装/更新：把"当前这个二进制"推过去

`machines.rs::install()`（`machines.rs:471-552`）：
1. `uname -sm` → `platform()` 归一化成 `linux x86_64` 这种（`machines.rs:418-433`）；
2. 与本机一致 → 直接推**当前这个可执行文件**；不一致 → 取"本版本该 target 的 release 构建"（target 名 `linux-x86_64`，`releases::manifest(Some(version), …)`）；
3. `via` 管道上传（**不是 scp**）：远端 `head -c <n>` 精确收字节；
4. 四条校验一条原子替换：字节数相等 → `chmod 755` → `--version` 能跑 → `mv -f` 覆盖 `~/.local/bin/herdr-core`；
5. 只在没有 `hc` 时补软链；
6. 最后 `{session}<bin> server restart`——**计划内重启**，holder 保留 PTY 等新 core 收编，终端不死（`restart.rs:7-9`、`runtime.rs:963-998`）。

Windows 分支才用 `scp` + `install.ps1`（`machines.rs:598-622`）。

### 2.9 API 层面的跨机语义

- 两类寻址：`on` 参数（`terminal.run`/`machine.exec`/`file.*`/`doc.*`）与 `box/t7` 前缀（`ON_A_TERMINAL` 12 个方法）（`remote.rs:796-862`）；
- `caller` 跨机时加 core **id** 前缀而非 name（name 可能被对端改名），到对端再翻回本地视角（`remote.rs:867-871`、`remote.rs:682-695`）；
- `localize()` 只改 `address/parent/terminal/from/removed` 五个 key（`remote.rs:1048-1077`）；
- 对端（peer，拨进来的 core）只能反向调 `terminal.send/get/read/wait` 四个（`CALLS_BACK`，`remote.rs:593`）；
- 互相 add 的两台机器**不递归**：从远端来的调用只返回它自己的（`api.rs:261-296`，测试 `tests/remote.rs:270`、`291`）。

### 2.10 测试手法（决定 deepterm 能不能照抄）

`HC_SSH` 环境变量让 `ssh()` 直接返回指定可执行文件（`machines.rs:180-183`）。`fake_ssh` 脚本按 target 分派：命中某台机器就 `exec env -i HOME=<该 session 的 home> … sh -c "$*"`，**真的在本机起一个隔离 core**，而不是 mock 协议（`tests/common/mod.rs:238-262`）。故障注入靠文件（`down` 文件 → `Connection refused` + 255）与 pid 文件 + `SIGKILL` 精确切断 line（`tests/remote.rs:25-40`）。

---

## 3. 立项时的 deepterm 差距（现已补齐）

本节保留立项时的差距分析，用于解释设计取舍；当前实现状态以 §0 为准。

### 3.1 已经有的（可直接复用）

| 能力 | 位置 | 说明 |
| --- | --- | --- |
| core/holder 两进程 | `src/daemon/{listen,holder,core_holder}.rs` | 与 herdr 同构，已实测 kill -9 core 终端不死 |
| framed 协议 | `src/daemon/protocol.rs:32-67` | 9 字节头，`Json/Data/Close/Resize/Credit/Meta`，`PROTOCOL_VERSION=2` |
| 流控状态机 | `src/daemon/pump.rs:33-78` | `Live/NeedsPaint` + `INITIAL_CREDIT=256KiB`，与 herdr `WINDOW` 同量级 |
| 单连接多路复用 | `src/daemon/client.rs:221-345` | 读线程按 channel 分发、`pending` 缓冲、`pending_replies` 按 id 分发 |
| attach 先压后放 | `src/daemon/connection.rs:331-371` | `paused` + 全局 ticket，等价 herdr `open_paused`/`resume` |
| 断线≠结束 | `src/daemon/session.rs:1-24`、`terminal.rs:6022-6038` | `Close` 帧=程序退出；socket 断=只是看不到 |
| 尺寸先于快照 | `src/daemon/connection.rs:463-520` | 与 herdr `apply_size` → `first_paint` 同序 |
| 远端 exec 长期流 | `src/ssh/lsp_channel.rs:139-180` | 已能把远端进程 stdio 架在 SSH channel 上（LSP 在用） |

### 3.2 缺的（本方案要补）

1. **deepcore 没有 `connect` 子命令**：它只监听 socket（`src/bin/deepcore.rs:76-82`、`listen.rs:94-104`）。没有它，跨机就成了"远端 core 只能被本机 core 用 socket 连"，而 socket 不能过 SSH。
2. **客户端写死本地 socket**：`connect_or_start()` 直接 `transport::connect(&socket_path())`（`client.rs:75-130`），`from_stream` 虽然接受 `Read+Write`，但调用链上没有注入点；`handshake()` 还必须读本地 `token` 文件（`client.rs:408-420`）。
3. **没有"机器"概念**：没有 `machines.json`、没有每台机器一条 line、没有镜像、没有 `box/t7` 寻址。`remote_deploy.rs` 是死代码（唯一引用 `ssh/mod.rs:21`），且原设计走 TCP 监听（`daemon/mod.rs:123-127` 注释设想、代码不存在），与本方案方向相反。
4. **没有跨机调用路由**：`op` 是自研 JSON，`id` 每连接从 1 递增（`client.rs:515-533`），两跳之后必然撞号。
5. **没有 plan/orphan/missed 语义**：远端重启后 `daemon_address` 失效只能重建（`panel.rs:725-830` 已有这条回退），但"机器掉线"这个中间态不存在。
6. **Linux 无 musl 静态构建**：`.github/workflows/release.yml:20-28` 只有 `ubuntu-22.04` glibc 构建。这正是 `remote_deploy.rs:19-32` 当年放弃的根因。

---

## 4. 方案总体设计

### 4.1 目标拓扑

```
deepterm (GPUI)
   │  本地 socket（不变）
   ▼
deepcore (core) ── 新增：machines.json + 每台机器一条 Line + 远端镜像
   │   ssh <host> deepcore connect   ← 新增子命令，帧协议跑在 stdio
   ▼
deepcore (远端，同一二进制) ── holder ── PTY ── 程序
```

**与 herdr 完全同构**，差别只在命名与范围：

| herdr | deepterm |
| --- | --- |
| `hc connect` | `deepcore connect` |
| `machines.json`、`Machine{name,target,id,host,version,…}` | `Machine{name,ssh,session,id,hostname,version,…}`，复用 DeepTerm SSH 配置 |
| `Line`（每机器一条 framed 连接） | 同名，改动最小 |
| `mirror`（`box/t7` → 记录） | 同名 |
| `Relayed`（本地 channel ↔ 远端 channel） | 同名 |
| `session`（`HC_SESSION`） | `DEEPTERM_SESSION`（本期只用于测试隔离，不加 UI） |

### 4.2 为什么不复用 `remote_deploy.rs` 的 TCP 方案

原设计是"远端 core 监听 `127.0.0.1:port`，本机经 SSH 转发 TCP"。这条路要额外引入：端口分配、token 分发、`-L` 转发生命周期、以及"端口被占"的一整类故障。herdr 的 stdin/stdout 方案把这些全部消掉：

- 鉴权由 SSH 本身承担，**不需要 token**（远端 `connect` 只连本机 socket，socket 目录 0700）；
- 不需要端口、不需要 `-L`、不需要接受入站连接；
- 一条 SSH 连接就是一个会话，天然与"机器"一一对应。

**结论：删掉 `src/ssh/remote_deploy.rs`，按 herdr 重做。**（它的 `--version` 探针、指纹路径、`DeployOutcome` 三条测试里"不要静默降级"的精神保留到新实现。）

### 4.3 关键设计决定

#### D1. 只加一个子命令，不加新传输

`deepcore connect` 的行为与 herdr `hc connect` 一致（`cli.rs:697-750`）：

```
stdin  ──原样──▶ 本地 socket
stdout ◀─原样── 本地 socket
```

- 不做任何解析、不改写字节（`Kind::Credit`、`Meta` 都必须原样过去）；
- 唯一逻辑是"输入结束 → 半关写端"（`shutdown(Write)`），让 core 看到 EOF 退出连接；
- **远端 core 侧不需要任何新代码**：`serve_connection` 原样复用。

#### D2. Token：本地 socket 校验保留，`connect` 自己取

`authenticate()` 要求首帧 `hello` 带 token（`connection.rs:835-875`）。`connect` 在**本机**读 `token_path_for(socket)` 并自动补上 `hello`——但客户端（GUI）本来也要发 `hello`（`client.rs:408-464`）。

这里有个必须二选一的冲突：**GUI 的 `hello` 会在 `connect` 里被看到，如果 `connect` 什么都不做，远端 core 会收到两次 `hello`。**

选 **D2-a：`connect` 做"透明中继 + 首次注入"**：读到第一帧若为 `hello` 且没有 `token`，补上 token 再转发；之后完全透明。这样 GUI 侧代码一行不改，`connect` 只是"替远端调用者拿到本地凭据"，语义与 SSH 的 agent forwarding 同类。

风险写在方案里：`connect` 会把 token 暴露给"能把帧写到它 stdin 的人"——但那正是 SSH 会话本身，等价于 herdr 的模型（`docs/security.md:35-40`：能连 API 就能跑命令，SSH 负责信任）。

#### D3. 调用寻址：`box/t3` 前缀 + `caller` 加 id

完全照抄 `remote.rs:796-873`：

- `spawn`/`attach`/`close`/`get`/`list`/`snapshot`/`detach`/`remove`/`reap`（现有 `holder_op` 白名单，`connection.rs:408-413`）——`attach` 走"地址前缀"分支，其余走 `on` 或地址前缀；
- `address` 带 `/` 时按前缀查机器表：命中本机 id/name → 本地；命中机器 → 转发并把地址改写成裸 `t3`；
- 返回时 `localize()`：裸 `t3` → `box/t3`，本机 id/name 前缀还原成裸地址。

**`id` 撞号问题**：herdr 的 `id` 只在**一条连接**内唯一（`wire.rs:118-133` 的 `next_channel` 同理）。两跳之后：

- GUI→本地 core 的 `id` 是 GUI 发的；
- 本地 core→远端 core 的 `id` 由本地 core 重新分配（`Line::call` 自己 `next.fetch_add`）；
- 回复时本地 core 把 `id` 改回 GUI 的那个。

这与 herdr `Line::take_reply` 按 id 分发（`remote.rs:64-90`）完全一致。**不能直接透传 GUI 的 id**：GUI 重连后从 1 重新开始，会与远端在途请求撞号（这与 deepterm 已经踩过的 channel 号坑是同一类，见 `connection.rs:35-51` 的教训）。

#### D4. 掉线与"终端已结束"必须继续分开

现有三态：`Close` 帧 = 程序退出；socket 断 = 看不到（`session.rs:1-24`）。新增第四态：**机器掉线**。

| 状态 | 来源 | UI |
| --- | --- | --- |
| 程序退出 | `Kind::Close` | 标签显示"已退出"，可重启 |
| 本机 core 掉线 | 客户端 socket 断 | 屏幕写"重连中"，不清终端 |
| **机器掉线** | line 断 | 屏幕写 `[line to box lost; reconnecting...]`（CAN 前缀），**终端记录保留** |
| 机器需要登录/更新 | line 状态 | 机器列表标 `needs_login` / `needs_update`，终端只读 |

#### D5. 远端进程托管：systemd + linger，照抄但有取舍

herdr 用 `systemd-run --user`（`systemd.rs:103`）。deepterm 本期采用**更简单但语义等价**的两级策略：

1. 有 user manager → `systemd-run --user --collect --property=KillMode=process --unit=deepterm-<role>-…`；
2. 没有 → `Command::pre_exec(setsid)`，stdin/stdout/stderr 全部指向 `/dev/null`。

core 与 holder 使用各自独立的 transient unit，所以停 core 的 unit 不会连带
停 holder。linger 探测读 marker + `UserStopDelayUSec`，每 60s 刷新；只有
“marker 不存在、Linger=no、UserStopDelayUSec 有限”三者同时成立才报
`false`，否则无法证实就是 `null`。**不做** herdr 的关停顺序 unit：holder
本身独立托管，且 core 退出不发“请退出”信号，顺序风险远小于 herdr。

#### D6. 安装：直推当前二进制 + 静态构建兜底

- 发布站是 GitHub Release：`deepcore-manifest.json` 给出应用版本、两个
  Linux target 的归档 URL 与 SHA-256；归档根目录只能有一个 `deepcore`；
- 远端 `uname -sm` 选 `linux-x86_64` 或 `linux-aarch64`。manifest 版本必须
  等于当前 app 版本，不从远端猜版本；
- 下载后先做 SHA-256 与大小校验，再严格解包到本地临时文件；上传走现有
  SFTP（`upload_file_from_local_with_permissions`），不新增 SCP/端口；
- 远端校验顺序：字节数 → `chmod 700` → `--version` 能跑 → 原子 `mv -f`；
- 固定路径是 `~/.local/share/deepterm/deepcore`；测试 session 使用
  `~/.local/share/deepterm/sessions/<session>/deepcore`；
- 已有 core 更新时不新增 `restart` 子命令：只停 `daemon.pid` 指向的旧 core，
  supervisor 重拨 `deepcore connect`，由它拉起新 core；holder 全程不动。

#### D7. 先做"一条 line"，不做 peer

herdr 还有一个"远端 core 反向拨回本地"的 peer 模式（`remote.rs:640-700`），用于远端子进程回调 parent、跨机 agent 关系。本期**不做**：deepterm 的 agent 状态是本地标签属性，没有跨机父子需求。留着口子：`Line` 的 `hello` 里带 `calls_you`，将来加 peer 不改协议。

---

## 5. 实施阶段（每阶段可独立验收、可回退）

### 阶段 0：准备（0.5 天）

- 加 `DEEPTERM_SESSION` 运行时隔离（照 `HC_SESSION`，`paths.rs:80-93` 的校验逻辑），只用于测试与并行安装；
- 删 `src/ssh/remote_deploy.rs`（连带 `ssh/mod.rs:21`）；
- 抽出 `src/daemon/link.rs`：把 `Client` 里"读线程 + 写线程 + routes + pending + pending_replies"那段（`client.rs:221-345`）抽成与传输无关的 `Link`，`Client` 组合 `Link + handshake`。这是本方案唯一的重构，必须先做，否则后面每处都要复制一遍。

**验收**：`cargo test --lib` 全绿；`scripts/check-daemon.py` 全过（证明重构无行为变化）。

### 阶段 1：`deepcore connect`（1 天）

- `deepcore connect [--session S]`：连本地 socket，双向透传，首帧 `hello` 补 token（D2-a）；
- 输入 EOF → `shutdown(Write)`；
- 不新增任何远端代码。

**验收**：本机 `deepcore | deepcore connect` 能跑通 `scripts/check-daemon.py`（把它指向 stdout 管道而不是 socket）；手工 `ssh localhost deepcore connect` 后灌帧能拿到 `list`。

### 阶段 2：机器表 + line 监督（2 天）

- `machines.json`（`Machine{name,target,session,id,host,version,keeps_running_after_logout,logout_linger_user}`）；
- `Line`：`ssh` spawn（`-T`、`ControlMaster`、`BatchMode` 可配）、`dial()` + `hello`、`hold()` 读循环、`take_reply` 按 id 分发；
- `supervise()`：250ms→30s 退避、`needs_login` 120/300/600s、`needs_update`、`Heard` 静默判据（`machine_timeout=30s + heartbeat=15s`）；
- `op`：`machine.add` / `machine.list` / `machine.rm` / `machine.login` / `machine.update`。

**验收**：`tests` 里的 fake `ssh`（照 `tests/common/mod.rs:238-262`：命中 target 就 `exec env -i HOME=<隔离 home> … sh -c "$*"`，真的起一个隔离 `deepcore`）；`down` 文件断开；pid 文件 + `SIGKILL` 精确断线；断言 `machine ls` 在远端 `SIGSTOP` 时 < 6s 返回 `unreachable`。

### 阶段 3：镜像 + 跨机路由（2 天）

- `events.subscribe` 等价物：现有协议没有事件流，**新增 `op: "subscribe"`**：reply 带 `channel` + 当前 `list` 快照，之后在 channel 0 上推 `{type, address, data}`；
- `mirrors: HashMap<machine_id, BTreeMap<address, record>>`，`mirror_replace`/`mirror_events` 照抄；
- `route()`：`address` 前缀 / `on` 参数 → `To::Machine`；`caller` 加本机 `core.id`；`localize()` 改 `address`/`parent`/`cwd` 外的五个 key（deepterm 只有 `address`）。
- `list` 合并本地 + 各镜像；掉线机器不等待（返回缓存 + `state`）。

**验收**：两台隔离 core 互加后 `list` 都在 2s 内返回且不递归；事件按序、`seq` 单调。

### 阶段 4：viewer 中继 + 面板接线（3 天）

- `attach`：本地先开 channel 并 `paused`，调远端 `attach`，把 channel 改写成本地号，`Relayed` 记账；
- `reattach`：`history=0`、`resumed=true`、沿用原本地 channel、按 `active_at` 排序；
- 掉线时向 viewer 写 CAN + 提示，**不 close**；
- UI：`Panel::with_ssh_terminal` 改为"有机器 → 远端 spawn/attach；没有 → 现状（进程内 core）"；`session_state` 的 `daemon_address` 变成 `box/t3`（已 `#[serde(default)]`，旧文件不受影响）；
- 侧栏"添加机器"：调 `machine.add`，失败按 kind 提示（`hc_missing`/`needs_login`/`already_added`/`self`）。

**验收**：R1–R4 四条逐条脚本化；真机 Ubuntu 22.04 / 24.04 各一台。

### 阶段 5：安装/更新 + linger 上报（2 天）

**已实现并纳入 `tests/remote_machine_e2e.rs`。**

- `uname -sm` → target；GitHub Release manifest + SHA-256 + 严格解包；
- SFTP 上传 + 大小/权限/`--version` 探针 + 原子替换；
- `core_missing` 自动安装、版本不一致自动更新、显式 install/update；
- 更新只停旧 core，不杀 holder；失败时旧 core 与终端保持不变；
- systemd-run / setsid 双路径、linger 后台启用、60s 刷新、3s 状态同步。

**验收**：远端无 core 时自动安装并连上；强制更新后远端 core PID 变化但
holder PID 不变且终端可 attach；SHA-256 损坏时安装失败且不覆盖旧 core。

### 阶段 6：Linux musl 静态构建（1.5 天，可与 4/5 并行）

**已实现。**

- `x86_64-unknown-linux-musl` + `aarch64-unknown-linux-musl` 进入 release workflow；
- 链接器包装把 Rust sysroot 的 musl self-contained libc 放在系统库之前，
  并用 `RUST_FONTCONFIG_DLOPEN=1` 避免静态链接 fontconfig 的 dlopen 问题；
- release job 下载两个 core 归档，生成 `deepcore-manifest.json` 和总
  `SHA256SUMS.txt` 后与桌面安装包一起发布。

---

## 6. 关键取舍（已决）

- **异构/glibc**：选择 GitHub Releases 发布 musl 静态 core，不把 Linux
  二进制塞进桌面安装包。架构差异由 manifest target 选择解决，glibc 差异由
  musl 静态链接解决。
- **鉴权**：保留 DeepTerm 现有 Keychain / 密码 / host-key 校验路径；传输
  信任边界是已认证 SSH 会话。`deepcore connect` 只给首帧 `hello` 注入当前
  socket 的 token，不跳过鉴权，也不让远端 core 监听额外网络端口。
- **上传**：选择 SFTP，复用现有进度、权限与取消设施；不采用 herdr 的
  exec + `head -c` 管道。

---

## 7. 风险与已知陷阱（从 herdr 的 CHANGELOG 反推）

| 风险 | herdr 踩过的证据 | deepterm 的对应措施 |
| --- | --- | --- |
| 一台机器 SSH 认证反复失败被 fail2ban 封 | `CHANGELOG.md:17`，120/300/600s 退避 | 阶段 2 直接照抄这三档 |
| 慢链路上大帧被判成"卡死" | `tests/remote.rs:242` | 用"最后读到字节的时刻"而不是"最后一次完整帧" |
| 掉线重连后 viewer 静默失聪 | deepterm 自己踩过（`connection.rs:35-51`、`core_holder.rs` 的 channel 号错配） | `reattach` 必须沿用原本地 channel，且**每台机器独立编号** |
| 远端重启后旧的 `daemon_address` 指向不存在的终端 | 现有 `panel.rs:759` 已有回退 | 保持"接不上就按会话重建"，但**机器还在、只是 line 断**时不重建 |
| 登录 banner 混进协议流 | `tests/machines.rs:487` | `dial` 只认"第一条 id 匹配的 JSON 行"，其余忽略 |
| 两份快照叠加导致回滚翻倍 | 现有 `attach` 默认 `history=0` + 设计文档 §4.1 | `reattach` 一律 `history=0` |
| 同机另一个会话被当成同一台机器 | `tests/machines.rs:354` | `machine.id` 用远端 core 的随机 id，`session` 参与身份 |
| 互相 add 导致事件回声/递归 | `tests/remote.rs:291` | 本期不做 peer，风险天然不存在；为将来留 `calls_you` |

---

## 8. 工作量与顺序

| 阶段 | 内容 | 人日 | 依赖 |
| --- | --- | --- | --- |
| 0 | `Link` 重构 + 删死代码 + session 隔离 | 0.5 | — |
| 1 | `deepcore connect` | 1 | 0 |
| 2 | 机器表 + line 监督 | 2 | 1 |
| 3 | 镜像 + 跨机路由 | 2 | 2 |
| 4 | viewer 中继 + 面板 | 3 | 3 |
| 5 | 安装/更新 + linger | 2 | 2 |
| 6 | musl 静态构建（若选 B） | 1.5 | — |

合计 **12 人日**（含 6）。可先做到阶段 3 就停下来验收一次（那时"能看到远端终端、能读能发"已经成立，只是关掉 app 会断开），再决定是否继续。

---

## 9. 与 herdr 的逐项对照（审阅用）

| 能力 | herdr | 本方案 | 差异理由 |
| --- | --- | --- | --- |
| 跨机传输 | `hc connect` over stdin/stdout | `deepcore connect` | 一致 |
| 鉴权 | SSH + 本地 socket 权限 | 同 + `connect` 自动补 token | 一致（安全模型等价） |
| 每机器一条 line | ✅ | ✅ | 一致 |
| 断线退避 | 250ms→30s，login 120/300/600s | 同 | 一致 |
| 静默判据 | 字节级 `Heard` | 同 | 一致 |
| 镜像 + 事件流 | ✅ | ✅（新增 `subscribe` op） | deepterm 协议原本没有事件流，必须补 |
| viewer 中继 | `open_paused` + `reattach` | 同 | 一致 |
| 远端进程托管 | systemd-run + linger | systemd-run / setsid + linger | 不做 shutdown-order unit |
| 安装 | 推当前二进制 / release 下载 | GitHub Release manifest + SHA-256 + SFTP | 与最终 herdr 发布方式对齐 |
| peer（反向拨入） | ✅ | 本期不做 | 无跨机 agent 父子需求 |
| 文件/exec API | ✅ | 不做（deepterm 用 SFTP） | 已有设施不同 |
| Windows 远端 | ✅ | 不做 | 范围控制 |

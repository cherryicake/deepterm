# vtebench 终端吞吐基准报告（2026-09-25）

用 Alacritty 官方的 [vtebench](https://github.com/alacritty/vtebench) 对
DeepTerm 的终端做了一轮 PTY 读取吞吐基准，并与 Alacritty（vtebench 的出处）、
Ghostty 1.3.1 三方对照，定位出 4 个可独立落地的优化点。

**vtebench 测的是什么**：它把基准脚本的 stdout 灌进被测终端的 PTY，测
「终端从 PTY 读走这些字节并把它们解析进网格」所需的时间（写入端阻塞时长）。
它**不测**帧率、延迟、渲染质量——上游 README 明确说明单看这个数字不足以
判断终端整体性能。本轮结论也只在「PTY 读取与解析吞吐」这个口径上成立。

---

## 一、环境与工具

| 项目 | 值 |
|---|---|
| 机器 | macOS 26.4.1 arm64（Apple Silicon），load ~9（用户 DeepTerm.app 在跑） |
| 被测 | `target/release/deepterm`（release，rustc 1.98.0-nightly） |
| 对照 | Alacritty（本仓库 `../alacritty` 构建）、Ghostty 1.3.1 |
| 工具 | vtebench 0.3.1，`--max-secs 10 --min-bytes 1048576 --warmup 1` |
| 网格 | deepterm 126×36 / alacritty 126×36 / ghostty 129×37（面积差 +5%） |

### 复现

```sh
# 1. 构建 vtebench
git clone https://github.com/alacritty/vtebench.git
cd vtebench && cargo build --release

# 2. macOS 兼容修补：vtebench 的基准脚本用 `ps -o tty=` 取终端设备，
#    而 BSD ps 的输出带尾随空格（Linux 不带），导致 `/dev/ttys006 ` 打不开、
#    尺寸取不到、基准输出为空。7 个脚本需加 `| tr -d " "`。
#    （见「附录 A」，这是本轮唯一对 vtebench 的改动，未改测量逻辑。）

# 3. 让 deepterm 把 vtebench 当 shell 启动（终端读取 $SHELL）
SHELL=/path/to/run-vtebench.sh ./target/release/deepterm
```

驱动脚本 `/tmp/vtebench-run/run-vtebench.sh` 在开跑前 `sleep 4` 等窗口布局
把 PTY resize 到最终尺寸——否则 vtebench 会在初始 55×18 的网格上测完整轮，
与对照组的 126×36 不可比（第一轮数据就是这样废掉的）。

---

## 二、结果

单位毫秒，取各基准全部样本的**中位数**（vtebench 自身按 10 秒/项采样，
样本数 200~1100 不等）。

| benchmark | deepterm | alacritty | ghostty | dt/alac | 判定 |
|---|---:|---:|---:|---:|---|
| cursor_motion | **15** | 7 | 16 | **2.14x** | 慢 |
| dense_cells | 12 | 11 | 20 | 1.09x | — |
| light_cells | 8 | 8 | 14 | 1.00x | — |
| medium_cells | **14** | 9 | 16 | **1.56x** | 慢 |
| scrolling | **25** | 65 | 29 | **0.38x** | 快 |
| scrolling_bottom_region | **30** | 24 | 33 | **1.25x** | 慢 |
| scrolling_bottom_small_region | **44** | 38 | 36 | **1.16x** | 慢 |
| scrolling_fullscreen | **12** | 10 | 13 | **1.20x** | 慢 |
| scrolling_top_region | **45** | 34 | 28 | **1.32x** | 慢 |
| scrolling_top_small_region | **34** | 21 | 27 | **1.62x** | 慢 |
| sync_medium_cells | **22** | 13 | 18 | **1.69x** | 慢 |
| unicode | **13** | 8 | 10 | **1.62x** | 慢 |
| **几何平均** | | | | **1.25x** | |

deepterm 只在 1/12 项（`scrolling`）上比 Alacritty 快，整体慢 25%。
**与 Ghostty 相比则是 7 快 5 慢、几何平均 0.98x（基本持平）**——所以问题
不是「deepterm 的终端整体差」，而是特定路径上有可定位的开销。

三个终端的差异不只在被测代码：alacritty 与 deepterm 共用同一个
`alacritty_terminal` 0.26 解析内核，**两者之差就是 deepterm 在解析前后额外
加的那层代码**；deepterm 与 ghostty 之差则是两个独立实现之差。

---

## 三、根因（按影响排序）

数据来自 `sample(1)` 对各线程做的 1ms 采样（本地 `--max-secs 20` 聚焦轮）。

### R1. 应用层扫描器每次查找都重建 memchr searcher（最大项）

**证据**：`cursor_motion + unicode` 聚焦轮里，读线程 self-time 的
**38%** 落在 `memchr` 上，其中**重建 searcher 占约 27%**（远大于实际搜索）：

```
18.6%  memmem::FinderBuilder::build_forward_with_ranker   ← 重建 searcher
 8.7%  memmem::find                                       ← 入口（其子节点即上一行）
11.1%  memmem::searcher::searcher_kind_neon               ← 实际搜索
 1.7%  escape_scan::find_private_mode                     ← 调用方
 1.2%  escape_scan::find_dec_rqm_private                  ← 调用方
```

`memchr::memmem::find(haystack, needle)` 是便捷函数，内部每次都执行
`Finder::new(needle)`（`memchr-2.8.1/src/memmem/mod.rs:185-191`）。它适合
「一次性搜索」，不适合 deepterm 这种**在同一个 needle 上反复搜索**的用法。

deepterm 的用法恰恰是后者，而且**每次搜索的 haystack 只有几 KiB**：

- `escape_scan.rs:167` `find_csi_16_t` → `memmem::find(data, b"\x1b[16t")`
- `escape_scan.rs:192` `find_private_mode` → **循环内**每轮
  `memmem::find(&data[search_from..], CSI_START)`（数据里有 N 条 CSI 就重建 N 次）
- `escape_scan.rs:289` `find_dec_rqm_private` → 同上
- `kitty/scanner.rs:473-474` 与 `terminal.rs:2613-2615` → OSC/APC/CSI 各一次

`handle_size_sequences`（`kitty/scanner.rs:303`）对**每一个** PTY 读块都无条件
跑 4 组查找循环（`16t` / `2048` / `1016` / `dec_rqm`），即使块里根本没有 ESC。

**微基准**（`/tmp/memchr-bench`，两个改法各自量化）：

| 负载 | 现状 | 改后 | 加速 |
|---|---:|---:|---:|
| 复刻 `find_private_mode` 形态，2000 条 CSI × 400 轮（**复用 Finder**） | 32.7 ms | 19.7 ms | **1.66x** |
| 纯文本块（无 ESC）× 2000 轮（**ESC 门控 + 早退**） | 6.78 ms | 0.56 ms | **12.0x** |

**改法**（两处独立，可分别落地）：

1. **把 needle 提成复用 Finder**。所有 needle 都是编译期常量，可以
   `static`/`LazyLock` 或在函数内建一次复用，消除每块的重建。
2. **给扫描器加单字节 ESC 门控**。`memchr::memchr(0x1b, chunk)` 一次扫描
   （NEON 向量化，比 memmem 快得多）就能判定「这块没有任何转义序列」，
   是则整段透传、跳过全部 6 次 memmem。纯文本块上实测 **12.0x**
   （1696 ns/块 → 141 ns/块）。

纯文本输出（`cat` 大文件、日志刷屏、`yes`）在真实使用中占比很高，而
`handle_size_sequences` 在这些块上做的是纯无用功。

### R2. UI 线程在 Wakeup 路径上阻塞等网格锁

**证据**：`scrolling_top_region` 聚焦轮里，主线程 9120 个采样中有
**4244 个（46.5%）停在 `__psynch_cvwait`**，调用链是：

```
Terminal::build_local_terminal::{closure#2}   ← reader_notice_task
  └ WeakEntity<Terminal>::update → App::update_entity
      └ Terminal::check_focus_mode_enabled    (terminal.rs:2179)
          └ RawMutex::lock_slow → pthread_cond_wait   ← 3680 采样卡在这
```

`check_focus_mode_enabled` 用的是**公平锁** `self.term.lock()`：

```rust
fn check_focus_mode_enabled(&mut self) {
    let focus_mode_now = {
        let term = self.term.lock();        // ← 会排队等读线程的 lease
        term.mode().contains(TermMode::FOCUS_IN_OUT)
    };
    ...
}
```

它被 `handle_local_notice` 的 `LocalNotice::Wakeup` 分支（`terminal.rs:2224`）
和 `process_event` 的 `AlacTermEvent::Wakeup` 分支（`terminal.rs:3046`）调用
——**每条读线程通知都会走一遍**。而读线程在 `pty_read` 里持
`term.lease()` + `data` 锁直到喂完 64 KiB（`local_pty.rs:535-589`），
所以 UI 线程每次都排在这个 lease 后面。

`Terminal::update_last_content`（`terminal.rs:3184`）的注释里已经把这套
「渲染侧不排队等网格锁」的取舍讲得很清楚（`try_lock_unfair` + 跳过本帧），
但**这条 Wakeup 路径没享受到同一待遇**——它绕过了 `update_last_content`，
直接 `lock()`。

**影响**：一次通知 = 一次潜在的长阻塞。读线程吞吐越高、UI 线程越容易被拖住；
反过来 UI 线程占着锁也会让读线程的 `try_lock_unfair` 落空。这在
`scrolling_top_region`（`printf "y\n"` 高频小写入）上最明显：通知极密集。

**改法**：`check_focus_mode_enabled` 改成与 `update_last_content` 同款的
非阻塞尝试。具体地：`try_lock_unfair()` 拿不到就**不阻塞**，把「待检查」
标记置位，等下一次 `update_last_content` 已经持锁时顺手比对一次
（那里本来就把 `content.mode` 写进 `last_content.mode`，多一次比较即可）。
这样 Wakeup 路径完全不碰锁。

注意不能简单地「拿不到就跳过」——`FOCUS_IN_OUT` 的**跳变沿**必须被捕获
（xterm 约定：应用开启该模式时要立即上报一次基线），漏掉跳变沿会让
终端不响应焦点变化。所以要用「置脏 + 在持锁点补检」而不是丢弃。

顺带：`report_focus_state`（`terminal.rs:2074`）**自己也再取一次锁**读
`term.mode()`。补检时模式位已经拿在手上，这个二次加锁可以一并省掉
（把 mode 当参数传进去），否则刚出锁又进锁。

### R3. `Full` damage 强制整视口快照重建，滚动场景每帧全量

**证据**：`scrolling_top_region` 读线程 self-time 的 **56%** 在
`Grid::scroll_up`，其中 `Row::reset` 占 7.3%。这是 alacritty 内部的正常
成本（对照组同样要付），差异在**deepterm 的 UI 侧**：

alacritty 把任何滚动都标成 `mark_fully_damaged()`（`term/mod.rs:764/791`），
于是 deepterm 的 `update_last_content` 每帧走 `rebuild_all` 分支
（`terminal.rs:3291`），对整个视口（126×36 = 4536 格 × 56 B）重建
`IndexedCell`；随后渲染侧拿到的 `dirty_rows` 全是 `true`
（`terminal_element.rs:1536-1546`），于是**每帧每行都重新排版**——行缓存
（`RowRenderCache`）在滚动场景下完全不命中。滚动是逐行位移，**增量覆盖在
语义上是可行的**（行号由 `display_offset` 唯一确定），但当前实现把「滚动」
与「清屏/resize」同等对待。

`docs/PERF_2026-09-19.md` 记录了上一轮的取舍：当时论证「`Full` damage 意味着
视口没有一行可信任」是**保守且安全**的。这在当时是对的——但滚动场景
（`scrolling_*` 五项里有四项偏慢）说明这笔开销值得单独处理。

**改法**（有难度，建议先做 R1/R2）：把「整区滚动 N 行」识别成
「视口内容整体位移 N 行 + 末尾 N 行新增」，用 `Vec::copy_within` 移动
`cells`/`row_ranges` 后只重建新增行。前提是要能区分「滚动型 Full」与
「清屏/resize 型 Full」——alacritty 的 damage API 不区分，需要自己记账
（例如在 `process_event` 里观察 `display_offset` 变化 + 网格行数不变）。

### R4. `rebuild_viewport_snapshot` 每格一次 `Arc` 克隆 + 无条件全量

`rebuild_viewport_snapshot`（`terminal.rs:3144`）对每个格子做
`IndexedCell { cell: PackedCell::from(...), zerowidth: packed_zerowidth(...),
hyperlink: ic.cell.hyperlink() }`。`hyperlink()` 是 `Arc` 克隆（无字符串
分配，注释已说明），但**每格一次引用计数增减**在 4536 格上仍有可观成本，
且它与 R3 叠加：滚动时每帧 4536 次。

这一项与 R3 是同一个改动的两面（R3 落地后 R4 自然只在新增行上发生），
单列出来是因为它也是 `resize` / 换主题等 `force_full_damage` 路径的常数项。

---

## 四、附带发现：一个可稳定复现的崩溃（非性能）

跑基准时终端子进程（vtebench）退出，deepterm **必崩**（SIGABRT）：

```
thread 'main' panicked:
cannot read deepterm::workspace::workspace::Workspace while it is already being updated
```

栈（`RUST_BACKTRACE=full`，完整栈见 `/tmp/vtebench-run/crash.log`）：

```
Terminal::build_local_terminal::{closure#2}     ← reader_notice_task 处理 ChildExit
  └ WeakEntity<Terminal>::update → App::update_entity → App::flush_effects
      └ Workspace::handle_panel_close            (workspace.rs:1364)
          └ Workspace::do_panel_close            (workspace.rs:1424)
              └ Workspace::do_tab_close          (workspace.rs:2145)
                  └ Workspace::before_window_remove   (workspace.rs:659)
                      └ AppWindow::flush_pending_save (lib.rs:873)
                          └ AppWindow::save_session   (lib.rs:796)
                              └ AppWindow::session_state (lib.rs:754)
                                  └ self.default_workspace.read(cx)   ← 重入读 Workspace
```

**机制**：`WeakEntity<Terminal>::update` 持有 `Workspace` 的更新借用；事件
flush 期间 `Workspace` 自身在 `do_tab_close` 里被 `&mut` 持有，而
`before_window_remove` → `save_session` → `session_state` 又要
`default_workspace.read(cx)`——同一个 `Workspace` 实体的读借用，gpui 的
`EntityMap::read` 直接 panic（`entity_map.rs:164`）。

触发条件是「**关闭窗口里最后一个标签页**」——`do_tab_close` 只在
`active_tab_id == tab_id` 且 tab bar 已无下一个 tab 时才走
`before_window_remove` + `remove_window`（`workspace.rs:2171-2181`；
`activate_next_tab` 里还有同款的第二处，`workspace.rs:724-729`）。
实测确认（隔离 HOME，`/tmp/vtebench-run/`）：

| 场景 | 结果 |
|---|---|
| 单标签页，终端里 `exit` | **必崩**（exit 134，复跑两次一致） |
| 双标签页，退出的那个是活动 tab，另一个存活 | 不崩（走切换分支） |
| 双标签页，退出的那个不是活动 tab | 不崩 |
| 双标签页，两个终端都退出 | **必崩**（第二个退出时已无剩余 tab） |

即**窗口里最后一个终端消失时必崩**：单标签页敲 `exit`、或按 `cmd-w`
关掉最后一个标签页。复现脚本 `/tmp/vtebench-run/crash-exit.sh`
（2 秒内 100% 复现）。

**改法**：`before_window_remove` 里的落盘不能同步读 `Workspace`。两条路：
- 让 `save_session` 接受一份**已取好的**会话快照（调用方在还没进入
  `&mut Workspace` 更新时先算出来），而不是自己去 `read`；
- 或把落盘挪到 `cx.defer` / `spawn` 里，脱离当前的更新借用。

---

## 五、优先级建议

| 优先级 | 项 | 预期收益 | 风险 |
|---|---|---|---|
| P0 | **崩溃修复**（第四节） | 正确性；用户丢标签页时必崩 | 低 |
| P1 | **R1 memchr searcher 复用** | 读线程 29% → 个位数；纯文本块 12x | 低（纯等价改写） |
| P1 | **R2 Wakeup 路径去阻塞锁** | UI 线程 46% 的等待消除 | 中（要保住跳变沿语义） |
| P2 | R3/R4 滚动增量快照 | `scrolling_*` 四项（1.16~1.62x） | 高（要自建 damage 记账） |

R1 与 R2 都是**纯路径改写、不改语义**，且各自有明确的微基准背书，建议先做。
R3 需要在 alacritty 的 damage API 之外自己区分「滚动型 Full」，改动面大、
正确性风险高，适合在有回归测试保护（`incremental_snapshot_matches_full_rebuild`
那套逐格平价测试正是为此准备的）之后单独一轮做。

---

## 附录 A：对 vtebench 的唯一改动

macOS 的 BSD `ps -o tty=` 输出带尾随空格（Linux 不带）：

```
$ ps -o tty= -p $$ | cat -A
ttys006 $              ← 末尾有空格
```

于是 `tty="/dev/$(ps -o tty= -p $$)"` 得到 `/dev/ttys006 `，`tput cols < $tty`
失败、`columns` 为空，基准脚本产出 0 字节。**7 个脚本**受影响（含
`ps -o tty=` 的全部脚本）：
`cursor_motion`、`dense_cells`、`light_cells`、`scrolling_fullscreen` 的
`benchmark`，以及 `scrolling_bottom_region`、`scrolling_bottom_small_region`、
`scrolling_top_small_region` 的 `setup`。

改动只有一处：`ps -o tty= -p $$` → `ps -o tty= -p $$ | tr -d " "`。
未触碰 vtebench 的测量逻辑（`src/` 一行未改）。

## 附录 B：产物位置

| 内容 | 路径 |
|---|---|
| vtebench 源码 | `../vtebench`（与本仓库同级的 checkout） |
| deepterm 全量结果（126×36） | `/tmp/vtebench-run/results-deepterm-full.dat` |
| alacritty 结果（126×36） | `/tmp/vtebench-run/results-alacritty.dat` |
| ghostty 结果（129×37） | `/tmp/vtebench-run/results-ghostty-full.dat` |
| 三方汇总 | `/tmp/vtebench-run/final-comparison.json` |
| CPU 采样（全量负载） | `/tmp/vtebench-run/deepterm-sample.txt` |
| CPU 采样（scrolling_top_region） | `/tmp/vtebench-run/slow-sample.txt` |
| CPU 采样（cursor_motion + unicode） | `/tmp/vtebench-run/cm-sample.txt` |
| memchr 微基准 | `/tmp/memchr-bench` |
| 崩溃复现 | `/tmp/vtebench-run/crash-repro.sh` + `crash.log` |

## 附录 C：测量口径说明

- 中位数而非均值：vtebench 自身输出均值 + p90，但本机 load ~9
  （用户 DeepTerm.app 常驻），个别样本可达中位数的 3~10 倍（如
  `scrolling_bottom_small_region` 出现 140ms 离群值），均值会被拖偏。
- 网格尺寸：deepterm 与 alacritty 都是 126×36（完全一致）；ghostty 用
  `--window-width=126 --window-height=36` 请求后实际落在 129×37（+5% 面积），
  对比时已计入这一点。
- 第一轮数据已废弃：那时没等窗口布局把 PTY resize 完，vtebench 在初始
  55×18 网格上跑完整轮，与对照组不可比（deepterm 那轮数字普遍比第二轮
  高一倍以上）。
- `scrolling` 一项 deepterm 显著快于 alacritty（0.38x），与其余滚动项方向
  相反，未深究——该基准的 `setup` 里先灌 10 万行 `printf "y\n"`，负载形态
  与其它滚动项不同。

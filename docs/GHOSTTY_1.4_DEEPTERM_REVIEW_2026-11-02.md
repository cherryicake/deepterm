# Ghostty 1.4 调研与 DeepTerm 终端优化建议

日期：2026-11-02  
范围：Ghostty `v1.4.0-92-g7cd6fa021`（上级目录 `../ghostty`）对比 DeepTerm `0.1.0`（当前仓库）  
方法：发布说明/社区文章检索 + Ghostty 提交与源码逐项核对 + DeepTerm 代码审计 + 现有基准与崩溃报告复核

> 证据标记：✅ = 当前代码或本机实测可直接确认；◐ = 上游提交/发布说明/已有报告；○ = 结构性推断，需后续实测。

---

## 0. 一页结论

Ghostty 1.4 的“大版本提升”不是单点魔法，而是四条线同时推进：

1. **I/O 与解析解耦**：PTY 读取改成 `io-gather` + `io-reader` 两级流水（4×64 KiB 环形缓冲），解决 macOS 单次 `read` 只有约 1 KiB、解析期间不 drain 内核队列的问题；端到端吞吐 +25%～55%。
2. **解析器批量化 + SIMD**：ASCII/UTF-8 解码走 SIMD，普通文本以 `print_slice` 成批填行，减少每字符重复判断；ASCII 128→725 MB/s，真实 asciinema 语料约 +25%。
3. **内存页化、压缩、可归还**：8 B/cell 的页式存储；空闲时用 LZ4 压缩不可见 scrollback（物理内存降 70%～90%）；空闲页用 `madvise` 归还物理页；空终端页内存 128 KiB→16 KiB；默认 scrollback 从 10 MB 提到 50 MB。
4. **长跑稳定性工程**：修掉“跑久了每帧全量重建”“关闭窗口泄漏”“读终端文本泄漏”“隐藏标签不释放 GPU”“计数器/缓存生命周期不一致”等一批只在长 uptime 下暴露的问题。

DeepTerm 的差距则集中在三处：

- **稳定性**：至少两类可复现/有崩溃报告的 panic 仍然存在——最后一个标签关闭时的 GPUI 实体重入 panic；GPUI `layout_line` 的 `split_at` 越界 panic（本机 10/29、10/30 两份 `.ips`）。此外 PTY 读线程缺少 panic 兜底、关闭信号可能被丢弃、粘贴大文本会在读线程里阻塞写 5 秒并丢数据；PTY 模式 SSH 关闭时没有调用 `close()`，事件循环可能在后台线程 panic。
- **性能**：9/25 vtebench 报告里的 R1（每次 `memmem::find` 重建 searcher）、R2（每条 Wakeup 公平锁）审计时未修（本轮已修，见第 8 节）；滚动时 alacritty 每行输出都标 `Full`，DeepTerm 每帧全量重建快照 + 全行重排，`frame_all_rows_dirty` 2.51 ms，而空转 0.76 ms。
- **内存**：alacritty 的 `Cell` 是 24 B（Ghostty 8 B），每行还有 32 B 的 `Row` 头；scrollback 按“行”固定 10 000 行、无压缩、无字节上限。10k 行在 80/200/300 列分别约 **19.5/48/72 MB**；用户当前 250×59 窗口满历史约 **60 MB/终端**；Ghostty 同等历史压缩后通常只有 **2–6 MB**。另外“关闭的 tab 进 undo 栈”最多保活 10 个完整终端（PTY 线程 + 回滚 + 图片），这是“关了很多标签内存也不降”的第一解释。

**最值得先做的 6 件事**（详见第 4 节）：

1. ✅ 修最后一个标签关闭的实体重入 panic（P0，已落）。
2. ✅ 在 `shape_line` 前强制把 runs 归一化到文本/char 边界，堵住 `split_at` 崩溃（P0，已落）。
3. ✅ PTY gather→parse 两级流水已落；关停原子标志 + 双线程 join、4×64 KiB 有界环（P0）。
4. ✅ 关闭 PTY 模式 SSH 时显式 `close()`，并把 `SshPtyTerminal` 存进 `Terminal` 字段（P0，已落）。
5. ✅ `Finder` 复用 + ESC 门控；Wakeup 路径去公平锁（P1，已落）。
6. ✅ scrollback 32 MB 字节预算 + 死配置接入、undo 栈 10→3、隐藏标签释放视口快照/行缓存（P1，已落）。

---

## 1. 调研来源

Ghostty 1.4.0 于 **2026-10-27** 发布，距 1.3.1 约 7 个月、2 889 个提交、172 位贡献者。

- 官方发布说明（含完整 changelog、PR 号、基准数字）：  
  <https://ghostty.org/docs/install/release-notes/1-4-0>
- Linuxiac：*Ghostty 1.4 Terminal Emulator Cuts Memory Usage, Boosts Performance*（2026-10-27）  
  <https://linuxiac.com/ghostty-1-4-terminal-emulator-cuts-memory-usage-boosts-performance/>
- OMG! Ubuntu：*Ghostty 1.4 is out with big improvements on Linux*（2026-10）  
  <https://www.omgubuntu.co.uk/2026/10/ghostty-1-4-released>
- Hacker News 讨论（1 355 分、443 评论，含真实用户对长跑内存/Claude Code 负载的反馈）：  
  <https://news.ycombinator.com/item?id=50261998>
- Mitchell Hashimoto：*Finding and Fixing Ghostty's Largest Memory Leak*（2026-01-10，长跑泄漏的经典案例）  
  <https://mitchellh.com/writing/ghostty-memory-leak-fix>
- Ghostty 仓库提交：`0535770e3`(#13209)、`2da015cd6`(#13220)、`98a7c0f6f`(#13227)、`ad692f1e8`(#13245)、`bc6ca3ff5`(#13240)、`7e02af879`(#13264)、`07bccf7a3`(#14137)、`31bdcd5a7`(#14130)、`683d8db64`(#14017)、`e69dc2bee`(#13585) 等。

社区侧的两个重要背景：

- 1.4 之前 Ghostty 曾被报告“10 天涨到 37 GB”，根因是 PageList 在 scrollback 裁剪时复用非标准页，元数据被重置为标准页大小、底层 mmap 却未 `munmap`；修法是“非标准页不复用，直接销毁并从池里取标准页”。这说明**长跑内存问题的根因往往不是“缓存太大”，而是对象生命周期/记账不一致**，对 DeepTerm 同样适用。
- 1.4 发布说明中专门列了“长 uptime 后每帧重建终端状态”（#13585）的修复：渲染态计数器到 100 000 帧后 deinit，但计数器没清零，之后每帧都 deinit + rebuild。这类“每 N 帧重置”的计数器是长跑退化的高发点。

---

## 2. Ghostty 1.4 的关键机制

### 2.1 内存：页、压缩、归还

**页布局**（`../ghostty/src/terminal/page.zig`）：

- 标准页容量 `cols=rows=215`，布局后 409 600 B（400 KiB），一次页对齐 mmap；页内顺序为 `[Row headers][Cell 数组][styles/graphemes/strings/hyperlinks 元数据]`。
- `Cell` 是 `packed struct(u64)`，**8 B**；颜色/字体等外置到页内 `StyleSet`，cell 只存 `style_id`。
- `Row` 也是 8 B（cells 偏移 u32 + wrap/grapheme/hyperlink/kitty 等标志位）。

**空终端 128 KiB→16 KiB 的三段式**：

1. `#14130` 把页池 free list 从“侵入式链表（写池项头部）”改成独立的 `UntouchedPool`，池项在真正使用前不被触碰，减少缺页。
2. `#14137` 把所有页内数据结构改成“零即空”：`initBuf` 只写行头，styles/strings/graphemes/hyperlinks/bitmap 都假设零初始化，不再 `memset` 整页。
3. `#13240` 修了 bitmap allocator 的区域尺寸 bug：旧代码把“字节容量”又乘了一次 `chunk_size`，导致每页多预留约 180 KiB（grapheme 131 136→8 256 B，string 65 544→2 056 B），标准页 576 KiB→400 KiB。顺带修了一个“可分配数少于位图标记 free 数”的越界发放风险。

**scrollback 压缩**（`#13264`，`../ghostty/src/terminal/compress/`、`PageList.zig`）：

- 自研 LZ4 block 编码器（为保持 libghostty-vt 的 no-libc 约束），压缩对象是整页 backing memory。
- 节点状态 `resident | compressed`；只压缩“活动页之前、且不与 viewport 相交”的整页。
- 渲染线程 250 ms 空闲计时器触发；每步最多检查 8 个候选、只压 1 页，成功后 1 ms 再步进；任何终端活动都会重置计时器。
- 压缩后保留虚拟映射，只让 OS 丢弃物理页；解压时 `recommit` + 原地解码，**解压不可失败**（失败即视为内存损坏）。
- 搜索用临时 buffer 解码，不改变节点状态；滚动只读元数据，压缩页进入活动区也不立即解压。
- **限额仍按未压缩逻辑大小记账**（默认 50 MB），所以“压缩不会让你留更多历史”，只是让同样的历史占更少物理内存。
- 实测：7.3 MB 语料、121 页，编码后 3.03 MB（6.11%），压缩 30.3 ms（约 101 µs/页），解压约 26 µs/页；稳态 I/O 吞吐在噪声内。

**归还 OS**（`#13245`，`../ghostty/src/terminal/mem.zig`）：

- 不 `munmap`，只 `madvise`：Linux `MADV_DONTNEED`、Darwin `MADV_FREE_REUSABLE/FREE_REUSE`、Windows `DiscardVirtualMemory`、FreeBSD `MAP_FIXED` 重映射。
- 页销毁时先 decommit 再回池；取用时 recommit。页粒度 + 映射常驻避免 mmap/munmap 抖动。
- 这解决了“清 scrollback 后 RSS 永远停在高水位”的问题。

**隐藏标签 GPU 释放**（`#14017`）：不可见 surface 释放 swapchain/RT/字体纹理，显示时重建（0.43 ms）；1 可见 + 20 隐藏标签，GPU 内存 385 MiB→18 MiB。

### 2.2 I/O：两级流水线

`#13209`（`../ghostty/src/termio/Exec.zig:1269-1318`）：

- 发现：macOS 内核对 pty master 的单次 `read` 上限约 1 KiB，跟缓冲区多大无关。旧架构“读 1 KiB → 解析 → 再读”会在解析期间停止 drain，写者被内核队列卡住。
- 新架构：`io-gather` 线程把 PTY 读进 4 个 64 KiB 的旋转缓冲；`io-reader` 线程按批取走、加终端锁、VT 解析、更新渲染态。锁只护环元数据，缓冲内容零拷贝。
- 背压：gather 在环满时阻塞 → 停止读 → 内核队列满 → 子进程 write 阻塞，不丢字节；每 surface 上限 256 KiB。
- 延迟：累计 ≥1 KiB 才进入 bridge（最多 16 次自旋、1 ms poll、整批 3 ms 预算）；小于 1 KiB 在首次 EAGAIN 立即投递。`#13237` 又修了“解析器空闲时 gather 仍在 bridge，导致写者等自己的应答”约 1.2 ms 的问题。
- 结果：macOS 端到端 +25%～55%；`cat 150 MB` 1.5 s→566 ms。

### 2.3 解析与渲染

**解析器**（`#13220`）：

- SIMD UTF-8 解码到 ESC/控制序列为止。
- `print_slice` 把一段普通文本成批交给 `Terminal.printSlice`；把显示/插入/wrap/charset/hyperlink 等不变量提到循环外；连续“简单格”用掩码比较 + 无分支存储批量填行，复杂格退回逐字符。
- CSI 终结字节直接派发，省掉每字节拷贝约 240 B 的 action 数组；SGR 空操作跳过；OSC 大载荷整段扫描 + 批量 append（OSC 52 约 30×）。
- 结果：ASCII 128→725 MB/s，CJK 3.4×，真实语料约 +25%。

**渲染锁**（`#13227`）：

- 不是新增双缓冲，而是把 `RenderState.update` 拆成 `beginUpdate`（持锁，按页块连续迭代、记录 pending styles）和 `endUpdate`（出锁，做样式反归一化、用 `applied_styles` 跳过未变样式）。
- 持锁时间降 2.7×～11×；clean 202→19 ns、单脏行 290→54 ns、全量 6.9→2.5 µs。
- 配套 `lockDemand/yieldToDemand`：解析线程在每批边界主动让锁，避免不公平 mutex 把渲染饿死。

**空闲能耗**：display link 只在“可见且有 cell/image/animation 需要重绘”时运行；不可见标签停 link 并释放 GPU 资源；非聚焦但脏改为按 vsync 画，而不是“每次 PTY 唤醒提交一帧”。

### 2.4 长跑稳定性修复（可直接借鉴的教训）

| 问题 | 根因 | 修法 |
|---|---|---|
| 长 uptime 后每帧全量重建（#13585） | 每 100 000 帧 deinit 渲染态，但计数器没清零，条件此后恒真 | 计数器清零（一行） |
| 关闭窗口不释放、WindowServer CPU 渐涨（#13601） | titlebar KVO 自失效/重绑，已关闭窗口仍注册在 AppKit | 延后到下一个 main-queue turn 重绑并跟踪 observed group |
| 每次读终端文本泄漏（#12020） | C 头与 Zig 实现的 `free_text` 签名不一致，真实分配永不释放 | 签名对齐 |
| 脚本快速开关终端泄漏 IOSurface（#13640） | 关闭时 window 恰好为 nil，surface 泄漏在 split tree | weak controller map + 所有权校验 |
| 每次写 screen file 漏 2 个 fd（#13644） | 保留的 tempdir parent handle 未关 | exhaustive close |
| 图像驱逐后内存慢涨（#13656） | placement 未 deinit，pin 残留 screen | 驱逐前 deinit |
| 行回收残留 wrap/prompt 元数据（#14010） | 清 cell 未清行标志，复用被快路径继承 | 回收点统一清 row 元数据 |
| 单行屏幕 + 行数限额下 UAF（#14686） | prune 掉 cursor 所在页，cursor 仍走同页快路径 | 换页时重建 cursor 的 style/hyperlink ID |

共同模式：**计数器/缓存与其描述的状态生命周期不一致**，以及**丢弃路径缺析构**。

---

## 3. DeepTerm 现状：代码与实测

### 3.1 架构与数据路径

- Rust + GPUI（Zed 框架）+ `alacritty_terminal 0.26`。终端核心 `src/terminal/terminal.rs`（约 7 700 行），本地 PTY `src/terminal/local_pty.rs`（约 2 800 行），渲染 `src/terminal_view/terminal_element.rs`（约 3 200 行）。
- 线程：GPUI 主线程；每个本地终端一个 `local-pty-reader` 线程（poll + read + 扫描 + vte 解析）；GPUI Metal 渲染。
- 读线程：`poll` → `read` 进 1 MiB 缓冲 → 取 `term.lease()` + `try_lock_unfair` → 扫描器（先 `handle_probe_sequences`，再 OSC/APC 扫描）→ 普通文本 `Processor::advance` 给 vte；每 64 KiB 让出锁；`try_send(Wakeup)` 通知 UI。
- UI：`handle_local_notice` → `note_output` → `check_focus_mode_enabled`（公平锁）→ `TerminalView` 订阅者做 `check_remote_os/check_tab_label_changed/poll_wsl_cwd` → `cx.notify`；prepaint 时 `Terminal::sync` 做 damage/快照，行缓存按 dirty 行重排，paint 遍历缓存。

### 3.2 内存：实测与构成

**实测**（`cargo test --lib terminal_scrollback_footprint -- --nocapture`，本机 2026-11-02）：

| scrollback | footprint 增量 | 每 1000 行 |
|---:|---:|---:|
| 1 000 | +1.84 MB | 1.84 MB |
| 5 000 | +5.53 MB | 1.11 MB |
| 10 000 | +15.69 MB | 1.57 MB |

默认无头几何是 31×33（500×300 逻辑像素、16×9 cell），所以这个数字包含大量 malloc size-class 取整与分配器驻留；但线性趋势明确：**scrollback 是单终端最大可控内存项**。

**构成**：

- `alacritty_terminal::Cell` = 24 B（`cell.rs` 自带 `EXPECTED_CELL_SIZE = 24` 断言）：`char` 4 + `fg/bg` 16 + `flags` 2 + `Option<Arc<CellExtra>>` 8。Ghostty 是 8 B。
- 每行一个 `Row`：32 B 头（`Vec<Cell>` 24 B + `occ` 8 B）+ 按列数满额分配的 `cols × 24 B`。也就是说每行至少 `32 + 24×cols` 字节，与行内实际内容无关。
- 宽度放大：alacritty 的 resize/reflow 会对**全部历史行** `row.grow(columns)`（`grid/resize.rs:233`），所以窗口变宽/字号变小会把所有回滚行重分配到新列数。80/200/300 列、10k 行的回滚部分分别约 **19.5/48.3/72.3 MB**；用户当前 `250×59` 窗口约 **60 MB**（malloc 取整后约 61–65 MB）。
- 超链接/组合字符放大：`CellExtra` 约 40 B + Arc 头 16 B = **56 B/格**；OSC 8 密集输出（`eza`、`ls --hyperlink`、agent 日志）会把单格从 24 B 推到约 80 B，**3.3×**。这是“普通 shell 还好，一跑 agent/彩色日志就涨”的隐藏放大项。
- 视口快照 `TerminalContent`：`IndexedCell` 约 56 B × 250×59 ≈ **0.8 MB/终端**（复用缓冲，不随帧增长）。
- 行渲染缓存：每可见行 `Vec<BatchedTextRun>` + `Arc<ShapedLine>`（单个 ShapedLine 约 3 KB）+ 选区/背景/盒绘制 span；50 行约 **0.3–1 MB/视图**。
- Kitty：存储上限 `KITTY_IMAGE_LIMIT = 128 * 1000 * 1000`（`terminal.rs:235`），**每终端 128 MB**；单图 64 MB、分片 base64 累积 85 MB、单命令 65 MB，峰值可到约 340 MB/终端。更关键的是同一图像同时保留 CPU RGBA、CPU BGRA `RenderImage` 和 GPU 纹理 **三份**（`kitty/render.rs:570-590`）；动画每帧按整幅画布 RGBA 存。
- 读线程固定 1 MB 缓冲；跨 chunk APC 残尾上限 65 MB（`kitty/scanner.rs:105`）。
- 搜索：快照按行建 `String + Vec<u32> + Vec<u8>`，80×10k 约 **4.8 MB**；命中 `Vec<SearchMatch>` 每字符 16 B，极端搜索可到十几 MB。
- **undo 栈保活**：`MAX_CLOSED_TABS = 10`（`workspace.rs:271`），关闭的“活终端”tab 会保留整个 `SplitContainer`（终端实体、PTY 线程、scrollback、Kitty 图片）以便重开。10 个 60 MB 的终端就是 **600 MB 级**保留；这是“关了很多标签但内存不降”的第一解释，不是传统泄漏，但用户感知与泄漏相同。
- 固定 GPU 成本：本机运行中的 DeepTerm 336 MB footprint 中，**172 MB IOSurface + 67 MB graphics + 44 MB IOAccelerator + 11 MB graphics** ≈ 294 MB 是 5K 窗口的 drawable/GPU 表面；这不是 Ghostty 的 scrollback 优化能消除的，但它解释了“看起来比 Ghostty 大很多”的一部分。

> 口径提醒：Ghostty 的“空终端 16 KiB”是**终端页的物理内存**，不含 GPU/框架；不要拿它和 DeepTerm 的进程 footprint 直接比。活动监视器里应先扣掉 5K 窗口的 GPU 表面，剩下的“随标签数/历史增长”的部分，才是 scrollback、undo 栈、Kitty 图片这些可控项。

**换算到 Ghostty**：250 列 × 10 000 行 × 8 B = 20 MB 逻辑；按 10%～30% 编码比，物理约 2–6 MB。即使 Ghostty 保留 50 MB 逻辑上限（约 25 000 行），压缩后通常也低于 DeepTerm 的 10 000 行。**差距主要来自 24 B vs 8 B 的 cell 布局 + 每行 32 B 头 + 无压缩**，不是渲染缓存。

**配置缺口**：`src/settings/content.rs:135` 有 `scrollback_lines` 设置，`Settings` 也注册了，但全仓库没有任何消费点；`Terminal::default_terminal_config()` 永远用 10 000（`terminal.rs:1194`）。`MAX_SCROLLBACK_LINES = 100_000` 也永远达不到。用户改这个值不会生效。

### 3.3 性能：9/25 的四个根因（审计时状态）

> 说明：R1/R2/R3 已在第 8 节落地（Finder 复用 + ESC 门控、Wakeup 去公平锁、行内容指纹 + 滚动平移）；R4 仍未修。以下保留审计时的原始证据，便于复核收益。

**R1：每次 `memmem::find` 重建 searcher** ✅

- `memchr::memmem::find(data, needle)` 是便捷函数，内部每次 `Finder::new(needle)`。
- 当前调用点：`escape_scan.rs:291/360/395/488`、`kitty/scanner.rs:500/501/511/512`、`terminal.rs:2858-2860/2883-2884`。
- `KittyGraphicsScanner::scan` 每个数据块无条件先跑 `handle_probe_sequences`（6 组查找：16t/2048/1016/dec_rqm/XTVERSION/DA2），再 2 次 OSC/APC 查找；纯文本块也要付 8 次 Finder 构建。
- 9/25 `sample(1)`：读线程 self-time 38% 在 `memchr`，其中约 27% 是重建 searcher；纯文本块 ESC 门控微基准 12×。
- 现状：**未修**，而且探测项从 4 组增到 6 组。

**R2：Wakeup 路径阻塞等网格锁** ✅

- `check_focus_mode_enabled` 仍用 `self.term.lock()`（`terminal.rs:2370-2373`），被 `handle_local_notice::Wakeup`（`:2440`）、`process_event::Wakeup`（`:3295-3301`）、`handle_pty_data`（`:2363-2365`）调用。
- `report_focus_state` 自己再取一次锁（`:2266-2270`）。
- 读线程在 `pty_read` 里持 lease 直到喂完 64 KiB（`local_pty.rs:527-583`），所以 UI 每次都排在后面。
- 9/25 采样：`scrolling_top_region` 主线程 46.5% 采样停在 `__psynch_cvwait`。
- 现状：**未修**，另有 `scroll_by`（`:4849`）、`scroll_wheel`（`:4758`）、搜索快照（`:4900`）同类公平锁。

**R3：滚动 `Full` → 每帧全量快照 + 全行重排** ✅

- alacritty 的 `scroll_up_relative` / `scroll_down_relative` 每次都 `mark_fully_damaged()`（`alacritty_terminal/src/term/mod.rs:788`），而 `linefeed` 在滚动区底部会调用它（`:1426-1427`）。所以**屏幕写满后每一行输出都是 Full**，不是只有用户滚动才 Full。
- DeepTerm `update_last_content` 对 Full 走 `rebuild_viewport_snapshot`（`terminal.rs:3499/3553`），并把整个视口（默认 126×36 时 36 行，用户当前 250×59 时 59 行）全部标脏（`:3385-3391`），下游每行 `take()` 旧缓存并整行重排（`terminal_element.rs:1637-1716`）。
- 现有 bench：空转 0.76 ms、单行脏 1.00 ms、滚动 1.20 ms、全脏 2.51 ms（criterion 中位数，含 present）。
- 现状：**本轮已修**（行内容指纹 + `scroll_shift` 平移复用，见第 8 节 P1-R3）。但要注意：Ghostty 1.4 在滚动/viewport pin 变化时**也**会全量重建渲染态（`../ghostty/src/terminal/render.zig:1018-1021`），差别是它重建扁平 cell 数组、按页块迭代、样式反归一化在锁外、排版有跨帧缓存。**所以正确方向是“让全量重建变便宜”，不是先做滚动 damage 记账。**

**R4：每格 Arc 克隆** — 部分修 ✅

- `Cell::hyperlink()` 是 `self.extra.as_ref()?.hyperlink.clone()`，只有带 OSC 8/组合字符的格子才增引用计数；普通格返回 `None`。
- 真正每帧付的是每个视口格一次 `PackedCell::from` + 写入 + `packed_zerowidth`（默认 126×36 时 4 536 格，用户 250×59 时 14 750 格）。增量路径（`terminal.rs:3567-3587`）只为脏行重写，9/20 已落地；10/27 的 `3a43436` 只把 run 合并线性化并去掉整行克隆。

**其它每帧/每通知成本**：

- `TerminalView` 每条 Wakeup 都跑 `check_remote_os` / `check_tab_label_changed`（含防抖计时器、后台进程 cwd 刷新）/ `poll_wsl_cwd` / `sync_agent_streaming`（`terminal_view.rs:1176-1190`）。
- `kitty_placeholder_cells()` 每帧扫全视口并分配（`terminal.rs:4279`）；`placed_images` 每帧重建并排序（`terminal_element.rs:2153-2187`）。
- 搜索打开时每 150 ms 抓全 scrollback 快照，主线程公平锁 + 约 3 次分配/行（`terminal.rs:4900`、`5059-5097`），10k 行 = 3 万次分配 + 全量字符拷贝。
- 光标 blink 淡出窗口每 500 ms 连续排帧约 100 ms（`blink_cursor.rs:82-84`），空闲常驻约 12 fps。

### 3.4 稳定性：本机证据

**（1）GPUI 文本整形 `split_at` panic** ✅

本机 `~/Library/Logs/DiagnosticReports/` 有两份 DeepTerm 崩溃（2026-10-29 15:29、2026-10-30 14:16），都是 `SIGABRT`，主线程：

```
rust_panic → core::str::slice_error_fail_rt → core::str::slice_error_fail
→ gpui_apple::text_system::AppleTextSystem::layout_line
→ gpui::text_system::line_layout::LineLayoutCache::layout_line
→ deepterm::components::input::element::TextElement::paint_inner
```

`AppleTextSystem::layout_line` 对每个 `TextRun` 做 `text.split_at(run.len)`；当 runs 的累计长度超过文本字节数时，`split_at` 越界 panic。

DeepTerm 自己在 `src/components/input/element.rs:2051-2064` 已经能检测到 `runs_len != sub_line.len()`，但**只打日志，仍然调用 `shape_line`**：

```rust
if runs_len != sub_line.len() {
    log::error!("layout_lines run length mismatch: ...");
}
let shaped_line = window
    .text_system()
    .shape_line(sub_line, font_size, &line_runs, None);
```

这是明确的“已知不变量被破坏但仍继续”的路径。另一个高风险点是 `layout_sticky_lines`（`:1183-1216`）用语法高亮 span 拼 runs，没有做“累计长度 ≤ 文本长度”的最后校验。

**（2）最后一个标签关闭时的 GPUI 实体重入 panic** ✅（代码路径仍在；9/25 已 100% 复现，本次未重跑，建议先补回归测试确认当前 GPUI rev）

调用链：

```
Terminal reader_notice_task → WeakEntity<Terminal>::update
→ Terminal::process_event(ChildExit/Exit) → cx.emit(CloseTerminal)
→ TerminalView → Workspace::handle_panel_close → do_panel_close → do_tab_close
→ before_window_remove (workspace.rs:670)
→ on_last_tab_close callback (lib.rs:770-778)
→ AppWindow::flush_pending_save / save_session (lib.rs:871)
→ AppWindow::session_state (lib.rs:829)
→ self.default_workspace.read(cx) (lib.rs:837)   ← 同一个 Workspace 正在 &mut update
→ GPUI: "cannot read ... while it is already being updated"
```

触发条件：关闭窗口里最后一个标签（单标签 `exit`、`cmd-w` 关最后一个标签、双标签两个都退出）。`before_window_remove` 仍在 `&mut Workspace` 更新过程中同步回调落盘；回调里又读同一个 Workspace 实体。9/25 文档记录“单标签 exit 必崩、2 秒内 100% 复现”，当前 HEAD 的代码路径没有变化。

**（3）PTY 读线程生命周期与写入阻塞** ✅

- `LocalReader` 没有 `Drop`，`_thread: Option<JoinHandle>` 直接 drop 即 detach；`Terminal::drop` 调 `kill()`，但 `LocalSender::shutdown()` 走 `try_send`（`local_pty.rs:202-211`）。通道满时 Shutdown 被丢弃，读线程永远阻塞在 poller，PTY fd/子进程/线程泄漏。
- `LocalSender::input` 同样 `try_send`（`:190-194`），通道满时**静默丢用户输入**；粘贴是单次写入（`terminal.rs:4960-4975`），大粘贴可能一次塞满 256 槽。
- `write_all_retry`（`local_pty.rs:70-101`）在 PTY 写缓冲满时在读线程里 sleep 重试，最长 5 秒；超时返回错误，剩余字节整段丢失。日志中 **125 次** `pty write failed: Resource temporarily unavailable (os error 35)`。读线程在写的时候不能读，写大粘贴时可能形成“写等读、读等写”的放大。
- 读线程没有 `catch_unwind`；vte/扫描器/自定义 APC 解析任何 panic 都会让线程静默死亡，终端冻结（进程不一定崩）。
- `LocalNotice` 通道容量 256，`try_send` 满时丢 Wakeup（`local_pty.rs:592-596`）；若最后一次输出恰好被丢，画面可能停在旧内容，直到下一次输出或 8 ms 补帧定时器（后者只在 `content_dirty` 为真时排）。

**（4）PTY 模式 SSH 关闭路径没有 `close()`** ✅ 链路确认 / ◐ 触发时序

- `SshPtyTerminal` 只保存一个 `sender`（`ssh_pty.rs:241-255`），`close()`（`:263-266`）没有任何调用点；`Terminal::kill()` 只处理本地 PTY 和 russh（`terminal.rs:4099-4102`）。
- `event_loop.spawn()` 的 `JoinHandle` 被 `_io_thread` 立即丢弃（detach）。`Terminal` 析构 → 回调闭包析构 → sender 断开 → alacritty `EventLoop` 的 `recv()` 收到 `Disconnected` 时 `panic!("event loop channel closed")`（`alacritty_terminal-0.26.0/src/event_loop.rs:481`）。
- 若远端仍有输出，关闭 SSH tab 可能在后台线程触发 panic；若远端静默，线程会永久阻塞，`ssh` 子进程 + Term 一起挂着。这是比本地 PTY 更隐蔽的长跑资源泄漏/崩溃源。

**（5）Kitty 解析路径的进程级崩溃风险** ✅ 路径确认 / ◐ 触发条件

- kitty 存储锁的 `expect("kitty 图像存储锁被投毒")` 分布在 `terminal.rs:956/964`、`terminal_element.rs:2376`、`kitty/exec.rs:315/321/332`。读线程只要在持锁的 decode/execute 路径 panic 一次，锁中毒，之后 UI 每次 paint 都会再 panic，终端窗口直接不可用。
- `RgbaImage::from_raw(...).expect`（`kitty/render.rs:630-634`）在帧数据长度与 `image.width/height` 不一致时让 UI 线程 panic；动画换帧/损坏帧可达。
- 修法：锁改 `lock().unwrap_or_else(PoisonError::into_inner)`；`from_raw` 返回可恢复错误并丢弃坏帧；读线程 `catch_unwind` 把 panic 转成 `Error` 并关闭终端。

**（6）长跑内存增长/OOM 风险** ◐

- 单终端 60–65 MB（250×59 满 10k 历史）；6 个标签约 360 MB，加上固定 GPU 表面 294 MB，总 footprint 可到 650 MB+。历史继续增长不会再涨（有 10k 上限），但多开标签/Kitty 图片会线性叠加。
- **undo 栈保活**：最多 10 个关闭的活终端 tab 保持完整实体（`workspace.rs:271`、`push_closed_tab`），每个 60 MB 级，合计可到 600 MB；且进入 undo 栈时不清理 Kitty 纹理。这是“关闭标签后内存不降”的第一解释。
- Kitty 每终端 128 MB 上限；10 个用图标签 = 1.28 GB 潜在；同一图像 CPU RGBA + CPU BGRA + GPU 三份。
- 隐藏标签只停绘制，不释放 `row_render_cache`、`last_content.cells`、Kitty GPU 纹理；Ghostty 1.4 的 385→18 MiB 对比说明这里有多少可回收空间。
- 大 APC 残尾：`pending + data` 每次全量 concat（`kitty/scanner.rs:486-497`），65 MB 上限在 1 MB 读缓冲下约 64 次全量拷贝，累计 GB 级 memcpy，分配器高水位可被顶到 128 MB。
- 日志文件 87 MB、无轮转；release 仍默认 `Debug`（`src/main.rs:52-56`），日志中 414 417 条 DEBUG、20 650 条 `vte::ansi` unhandled OSC。不是 RSS 主因，但会持续吃磁盘，且同步日志格式化会占用 UI/读线程时间。

**（7）其他观察**

- 2 次 `could not create display link, code: -6661`（睡眠/显示器变化后），可能导致渲染停摆；对应 Ghostty #14691“唤醒后罕见永久冻结”的同类风险。
- 远端文件树大量 `Handle limit reached`（2 214 次刷新失败），说明 SFTP 会话/句柄在长跑中累积；`failed_remote_keys` 的失败路径不写 LRU，近似无界（`builtin_commands/mod.rs:288-302`）。虽属文件管理子系统，但会放大进程资源压力。

---

## 4. 可借鉴清单

优先级：P0 = 正确性/崩溃；P1 = 高性价比性能/内存；P2 = 结构性优化；P3 = 战略选项。

| # | 优先级 | Ghostty 1.4 的做法 | DeepTerm 的改法 | 代码位置 | 预期收益 | 成本/风险 |
|---|---|---|---|---|---|---|
| 1 | P0 | 长跑生命周期修复 | 最后一个标签关闭：把 `before_window_remove` 的落盘改成 `cx.defer` 或在进入 `&mut Workspace` 前取好会话快照；禁止回调内 `default_workspace.read` | `workspace.rs:670/2174`、`lib.rs:770-778/829-871` | 消除 9/25 100% 复现的关标签崩溃 | 低；注意窗口移除后 deferred 任务的生命周期 |
| 2 | P0 | — | 所有 `shape_line` 前把 runs 归一化：累计长度 > 文本长度则截断，< 则补默认 run；`layout_lines` 检测到 mismatch 时不要继续 | `components/input/element.rs:2051-2064`、`:1183-1216`、`:1960/1979` | 堵住两份 `.ips` 的 `split_at` panic | 低；纯防御性 |
| 3 | P0 | 丢弃路径必须析构（#13574/#13931） | 本地 PTY：Shutdown 改原子标志 + `poller.notify()`，`Drop` 里 join（带超时/退出确认）；输入改独立非阻塞写队列；读线程 `catch_unwind` 并把 panic 转成 Error + 关闭 | `local_pty.rs:202-211/334-451/515-600` | 消除线程/fd/子进程泄漏、输入丢失、5 秒卡顿 | 中；需要重构消息循环 |
| 4 | P0 | 关闭路径必须显式终止 | `SshPtyTerminal` 存进 `Terminal` 字段，`kill()` 调用 `close()`；`close()` 发 `Msg::Shutdown` 并等待/确认事件循环退出 | `terminal.rs:4099-4102`、`ssh_pty.rs:241-266` | 消除 SSH tab 关闭时的后台 panic/线程+ssh 泄漏 | 低 |
| 5 | P0 | 长跑可观测性 | release 默认 `Info`，`vte::ansi=warn`、`keyring=warn`；日志按大小轮转（如 16 MB×3）；panic hook 把消息+backtrace 写日志 | `main.rs:52-84` | 87 MB→可控；崩溃可诊断 | 低 |
| 6 | P1 | 解析快速路径 | `Finder` 提成 `LazyLock<Finder<'static>>` 复用；scanner 入口 `memchr(0x1b)` 门控，纯文本整段透传；远程路径同样门控 | `escape_scan.rs:291/360/395/488`、`kitty/scanner.rs:495-512`、`terminal.rs:2858-2860` | 纯文本块 12×；读线程 memchr 38%→<5% | 低；纯等价改写 |
| 7 | P1 | 渲染让锁（#13265/#13227） | `check_focus_mode_enabled` 改 `try_lock_unfair`，拿不到置 `focus_check_pending`，在 `update_last_content` 持锁点补检；`report_focus_state` 传 mode 免二次锁；`scroll_by/scroll_wheel/search_snapshot` 同样去公平锁 | `terminal.rs:2370-2373/2266-2270/4849/4758/4900` | 消除 46.5% 主线程 cvwait | 低–中；要保住 FOCUS_IN_OUT 跳变沿 |
| 8 | P1 | 两级 PTY 流水（#13209） | Step 1：把 `term.lease()` 从读循环开头移到“累积够 64 KiB 后解析”处，读期间不持 lease；Step 2：加 gather 线程 + 4×64 KiB 环，parse 线程按批解析 | `local_pty.rs:515-600` | macOS 吞吐 +10%～30%（对照 Ghostty +25%～55%）；UI 锁等待大降 | 中；先做 Step 1 低风险 |
| 9 | P1 | 按字节限额 | scrollback 改字节预算（如 32 MB，按 `32 + cols*24B` 折算行数，resize 时重算）；接上 `scrollback_lines` 死配置并用 `MAX_SCROLLBACK_LINES` 夹取 | `terminal.rs:1194`、`settings/content.rs:135` | 满历史 60–65 MB→≤32 MB | 中；resize 时 `set_options` 会全量 damage，需用现有 25 ms 合并器 |
| 10 | P1 | 资源生命周期审计（#13585 的教训） | undo 栈从“保活 10 个完整终端”改为“挂起”：释放 grid/Kitty/GPU，保留 cwd/command 重建；或把 `MAX_CLOSED_TABS` 降到 3 | `workspace.rs:271/2269-2279` | 关闭标签后内存立刻回落，最多省 600 MB 级 | 中；需产品确认“可恢复”语义 |
| 11 | P1 | 隐藏标签释放 GPU（#14017） | 隐藏 tab 释放 `row_render_cache`/`last_content.cells`/Kitty 纹理；Kitty 上限可配、设全局总预算；进 undo 栈时也清理纹理 | `terminal_view.rs:1758`、`workspace.rs:1414-1426` | 隐藏 tab 数百 MB 可回收 | 中；重建一帧，需测切换延迟 |
| 12 | P1 | 让全量重建变便宜（#13227 的思路） | Full damage 时按行内容指纹复用旧 `RowRenderCache`（滚动/清屏/批量输出都受益）；行内 row 字段改为相对值或重定位；可配合 GPUI `shape_line_by_hash` 避免每行 `SharedString` | `terminal.rs:3499/3553`、`terminal_element.rs:1637-1716` | 滚动 1.20 ms→0.85–0.95 ms；全脏 2.51 ms→约 1.5–1.8 ms | 中；需要平价测试 |
| 13 | P2 | 大载荷解析 | APC 残尾改增量 append（独立 `Vec` 只追加新 chunk，避免 `pending + data` 全量 concat）；超大提前丢弃并回错误 | `kitty/scanner.rs:486-497/545-560`、`terminal.rs:2849-2860` | 去掉 O(n²) memcpy 与 128 MB 峰值 | 中；局部可落地 |
| 14 | P2 | 图片资源约束 | Kitty placements 加上限（4096/图、8192/终端）；逐图固定开销计入预算；上传后释放 CPU 像素，避免 RGBA+BGRA+GPU 三份 | `kitty/storage.rs:271/420-432/562-568`、`kitty/render.rs:570-590` | 峰值从 ~340 MB 降到 ~128 MB 内 | 中；需改 GPUI 侧纹理持有 |
| 15 | P2 | 搜索不阻塞 | 搜索快照移到后台线程，主线程只取一次锁做增量；命中 `SearchMatch` 压缩为 u32 行列对并设上限 | `terminal.rs:4900/5059-5097`、`terminal_view.rs:409` | 去掉每 150 ms 的 3 万次分配/全量拷贝 | 中 |
| 16 | P2 | 丢弃路径缺析构的教训 | kitty 锁改 `unwrap_or_else(PoisonError::into_inner)`；`RgbaImage::from_raw` 换可恢复错误；读线程 panic 转 Error | `terminal.rs:956/964`、`terminal_element.rs:2376`、`kitty/render.rs:630-634` | 消除锁中毒级联崩溃 | 低 |
| 17 | P2 | 有界队列 | `failed_remote_keys` 纳入 LRU/TTL；SSH-PTY 输入加背压（有界队列 + 写缓冲上限） | `builtin_commands/mod.rs:288-302`、`ssh_pty.rs:254-258` | 去掉近似无界增长与粘贴 OOM | 低–中 |
| 18 | P3 | 页式压缩（#13264） | fork/vendor `alacritty_terminal`，把冷 scrollback 按 256 行分段压缩（LZ4，`lz4_flex`），显式解压点（scroll/resize/search/copy）；逻辑字节限额 | 新 `vendor/alacritty_terminal` | 2–6× 内存下降（对照 Ghostty 70%–90%） | 高；需改 `Storage`/`Grid`，`&self` 索引是硬约束 |
| 19 | P3 | 8 B cell/页式存储 | 同上 fork 才能做；或长期评估 `libghostty-vt` FFI 替换核心 | — | 3× cell 密度 + 页式压缩 | 很高；重写渲染/选择/搜索集成 |
| 20 | P3 | 空闲休眠/隐藏停渲染 | 非活动 tab 的 Wakeup 走轻量路径（不跑 cwd/agent/tab label）；blink 在不可见/非聚焦时停或离散化 | `terminal_view.rs:1176-1190`、`blink_cursor.rs:82-84` | 空闲 12 fps→0；后台 CPU 下降 | 低–中 |

> 第 8 节列出了本轮已经落地的 P0 与 R1/R2；表中其余项仍是后续工作。

### 4.1 为什么“滚动 damage 记账”不是第一优先

9/25 报告建议 R3 做“滚动型 Full”识别，但对照 Ghostty 1.4 后要修正：

- Ghostty 在 viewport pin 变化/滚动时也会 `redraw` 全量重建渲染态（`render.zig:1018-1021`），它没有为滚动单独做 damage 记账。
- 它的全量重建便宜，因为：扁平 cell 数组、按页块连续迭代、`beginUpdate` 只记录 pending styles、`endUpdate` 锁外反归一化、跨帧 shaper cache。
- DeepTerm 的全量重建贵，贵在“每行重建 `BatchedTextRun`/`Vec`/`SharedString`/span”而不是“复制 4 536 个 cell”。

所以更稳的路径是**内容指纹复用行缓存**：滚动 k 行时，新视口的绝大多数行内容与上一帧某行完全相同，只是行号变了；按内容 hash 命中即可复用排版结果，只重建新进入的 k 行。这样不依赖 alacritty 的 damage 语义，也能覆盖 `cat`、构建日志、tmux 重绘等“Full 但内容大量重复”的场景。

### 4.2 内存：先做“预算”，再做“压缩”

- **今天就能做**：按字节预算折算 `scrolling_history`，把 60–65 MB/终端压到 32 MB 以内；resize 时按新列数重算，用现有 25 ms 合并器避免频繁 `set_options`。
- **优先处理 undo 栈**：它把“关闭的 tab”当成完整活终端保留，最多 10 个，每个 60 MB 级；这比任何缓存都更直接地解释“关了很多标签内存不降”。改成“挂起”或把上限降到 3，收益立竿见影。
- **顺手修**：`settings/content.rs` 的 `scrollback_lines` 目前是死配置；要么接上，要么删掉，避免用户以为改了就生效。
- **隐藏标签**：切走时释放快照/行缓存/Kitty 纹理；Ghostty 重建 0.43 ms，DeepTerm 重建一帧快照也在毫秒级，值得。
- **Kitty**：同一图像不要同时留 CPU RGBA + CPU BGRA + GPU 三份；placements 要有上限；逐图固定开销要计入预算，否则 128 MB 预算会被小图放大。
- **结构性**：fork `alacritty_terminal` 做“冷段压缩”。不要照抄 Ghostty 的 per-page lazy restore（需要页对齐 mmap + `madvise` + `&mut` 解压点）；Rust 侧更现实的方案是 256 行一段、显式解压、2–4 段 LRU。
- **不建议**：为了省内存把默认历史直接砍到 1k–2k 行；这会显著伤害“长时间构建/日志”场景，且没有解决宽窗口的线性放大。

### 4.3 性能：先做 R1/R2，再做 PTY 流水

- R1/R2 都是纯路径改写、不改语义，且有 9/25 微基准和采样背书；先做这两个，能立刻改善 `cursor_motion`/`unicode`/`scrolling_top_region`。
- PTY 流水是唯一“结构性但可控”的大项，建议先做“读期间不持 lease”的低风险 Step 1，再做 gather 线程。
- 行缓存内容指纹是滚动/批量输出的通用解，优先级高于 damage 记账。
- 不建议现在跟进 SIMD parser / `print_slice`：vte 0.15 是逐字符回调，要批量化必须 fork 整个 grid 写入路径，投入产出比低。

---

## 5. 不建议照搬的部分

| Ghostty 做法 | 为什么不建议直接搬到 DeepTerm |
|---|---|
| 8 B packed Cell + 页式 PageList | 需要重写 `alacritty_terminal` 的 Cell/Grid/Selection/Search/Serde 全部消费点；工作量以月计，且会长期背 fork |
| “零即空”初始化 | 依赖 mmap 按需缺页；Rust 全局分配器不保证零页，照搬没有收益 |
| 保留虚拟映射 + `madvise` 原地解压 | 需要自己管页对齐 mmap，且解压失败按 Ghostty 语义要 panic；与 Rust 生态/DeepTerm 的稳定性目标冲突 |
| GTK EGL/DMABUF 三缓冲 | DeepTerm 用 GPUI/Metal，平台不同；GPU 表面成本是 5K 窗口固有 |
| SIMD UTF-8 + `print_slice` | 要换解析器/grid 写入路径；DeepTerm 当前的瓶颈在读线程扫描和锁，不在 vte 的字节循环 |
| “滚动也全量重建”本身 | 这点 Ghostty 也做；要学的是“让全量便宜”，不是照抄“全量” |

---

## 6. 分阶段落地建议

> 阶段 0 的 6 项与 P1-R1/R2 已于 2026-11-02 落地，见第 8 节；下面保留原始计划作为后续排期参考。

### 阶段 0：止血（1–2 天）

1. 修最后一个标签关闭的重入 panic（P0-1）。
2. `shape_line` runs 归一化（P0-2）。
3. release 日志级别 + 轮转 + panic hook（P0-5）。
4. 读线程 Shutdown 原子标志 + `Drop` join + `catch_unwind`（P0-3 的最小版）。
5. SSH-PTY 显式 `close()` + `SshPtyTerminal` 入 `Terminal` 字段（P0-4）。
6. 把 `scrollback_lines` 死配置接上或删除。

验收：单标签 `exit` 不崩；SSH tab 关闭无后台 panic，`ssh` 子进程被回收；连续开关 100 次标签后线程/fd 数不增长；日志 24 h 不超过 32 MB；`cargo test` 全绿。

### 阶段 1：速赢（1–2 周）

1. R1：Finder 复用 + ESC 门控（本地/远程两条路径）。
2. R2：Wakeup 去公平锁 + `report_focus_state` 免二次锁。
3. PTY Step 1：lease 移出读循环。
4. 字节预算 scrollback + undo 栈瘦身/上限下调。
5. 隐藏标签释放缓存/纹理；Kitty 预算与 placements 上限。
6. blink/后台 tab 轻量路径。

验收：vtebench 几何平均从 1.25×（对 alacritty）降到 ≤1.10×；`scrolling_top_region` 主线程 cvwait <5%；250×59 满历史 ≤32 MB；空闲帧率 0。

### 阶段 2：结构性（3–6 周）

1. PTY 两级流水（gather + 4×64 KiB ring）。
2. 行缓存内容指纹复用（滚动/全脏）。
3. 搜索快照后台化 + 命中上限。
4. APC 残尾增量化；Kitty 上传后释放 CPU 像素。
5. 可选：fork `alacritty_terminal` 做冷段压缩原型（先 256 行/段 + LZ4，只压 `Line <= -(screen+1)` 的段）。

验收：`cat 100 MB` 端到端提升 ≥20%；`frame_all_rows_dirty` ≤1.8 ms；压缩原型在 10k 行语料上物理内存降 ≥50%，解压延迟 <1 ms/次滚动。

### 阶段 3：战略选项（季度级）

- 评估 `libghostty-vt` FFI 替换终端核心：能直接获得 8 B cell、页式压缩、SIMD parser、Ghostty 的 emulation 兼容性；代价是渲染/选择/搜索/配置全部重接，且 C API 仍在 WIP。建议先做 spike（一个只读终端 pane），不要一次性替换。

---

## 7. 验证与观测

### 7.1 现有基准

```sh
# 终端吞吐（需 ../vtebench；9/25 文档附录 A 的 macOS ps 修补仍适用）
cargo build --release
SHELL=/path/to/run-vtebench.sh ./target/release/deepterm

# 渲染帧
cargo bench --bench terminal_render

# scrollback 内存
cargo test --lib terminal_scrollback_footprint -- --nocapture --test-threads=1
```

### 7.2 建议新增

1. **250×59 固定几何的内存测试**：现有 footprint 测试用 31×33 默认几何，不能直接反映用户窗口；参数化 `TerminalBounds`，输出 MB/1000 行 @ 250 列。
2. **24 h soak**：以 10 MB/s 灌入 + 每 10 分钟开关一个标签 + 定期 Kitty 图片，记录 RSS、线程数、fd 数、`malloc` 区；断言 RSS 斜率 <1 MB/h、线程/fd 不增长。
3. **崩溃回归**：
   - 单标签 `exit`、双标签全退出、`cmd-w` 最后一个标签；
   - 构造 `runs_len > text.len()` 的 `TextElement` 单测，断言不 panic；
   - 读线程注入 panic，断言 UI 收到 Error 且终端可关闭。
4. **性能门禁**：把 vtebench 几何平均、`frame_all_rows_dirty`、`frame_scroll_one_line` 纳入 CI 阈值（允许 ±10%）。
5. **内存统计**：给每个 Terminal 暴露 `scrollback_bytes / snapshot_bytes / kitty_bytes / row_cache_bytes`，在 inspector 或日志中周期输出；这是发现“长跑慢涨”的前提。

### 7.3 验收指标（建议）

| 指标 | 当前 | 目标 |
|---|---|---|
| 最后一个标签关闭 | 9/25 100% panic | 0 |
| `layout_line` panic | 2 份 `.ips`/月 | 0 |
| SSH-PTY tab 关闭 | 无显式 close，后台可能 panic/泄漏 | 无 panic、`ssh` 子进程回收 |
| 关闭 10 个重标签后内存 | 保活约 600 MB 级 | 立即回落到基线 |
| 250×59 满 10k 行内存 | 60–65 MB/终端 | ≤32 MB（预算）或 ≤10 MB（压缩后） |
| 空闲帧率 | ~12 fps（blink） | 0（不可见/非聚焦） |
| `scrolling_top_region` 主线程 cvwait | 46.5% | <5% |
| vtebench vs alacritty 几何平均 | 1.25× | ≤1.10× |
| `frame_all_rows_dirty` | 2.51 ms | ≤1.8 ms |
| 24 h RSS 斜率 | 未测 | <1 MB/h |
| 24 h 线程/fd 数 | 未测 | 不增长 |

---

## 8. 本轮已实装（2026-11-02）

按第 6 节“阶段 0”先落了 5 个 P0，随后补齐 9 个 P1（R1/R2、M1–M3、R3、P1、K1、A1），均不改变终端语义：

| 项 | 实装内容 | 文件 |
|---|---|---|
| P0-1 关标签重入 panic | `Workspace::before_window_remove` 先算好 `WorkspaceSessionState` 再回调；`AppWindow` 新增 `session_state_with_default` / `save_session_with_default` / `flush_pending_save_with_default`，回调不再 `default_workspace.read(cx)` | `src/workspace/workspace.rs`、`src/lib.rs` |
| P0-2 `shape_line` 越界 | 新增 `normalize_runs_for_text`：累计长度不超过文本、每段对齐 char boundary；接入 sticky 行、折叠 tail、单行/空文本、主 `layout_lines`；顺手修正 inlay hint 把 `chars().count()` 当字节长度的 bug | `src/components/input/element.rs` |
| P0-3 本地 PTY 生命周期 | `LocalSender::shutdown` 改原子标志 + `poller.notify()`，不再依赖可能满的通道；`LocalReader::Drop` 发关停并 join；`run_loop` 在 poll 前后/消息循环检查标志，关闭时不再消费积压输入 | `src/terminal/local_pty.rs` |
| P0-4 SSH-PTY 关闭 | `Terminal` 新增 `ssh_pty_terminal: Option<SshPtyTerminal>`；`kill()` 显式 `close()`，避免 sender 断开后事件循环 panic / ssh 子进程挂着 | `src/terminal/terminal.rs` |
| P0-5 日志 | release 默认 `Info`（debug 保持 `Debug`）；`vte::ansi=Warn`；单文件 `CappedLog`，上限 1 MiB、超限原地裁到末尾 256 KiB，不再生成 `.1`，并清理旧备份 | `src/main.rs` |
| P1-R1 解析快路径 | `OSC/APC/CSI` Finder 改为静态复用（不再每块 `Finder::new`）；本地 scanner 与远程 `process_pty_data` 增加 ESC 门控，纯文本块直接透传；`find_csi_with_intermediate` 的 Finder 提到循环外 | `src/terminal/escape_scan.rs`、`src/terminal/kitty/scanner.rs`、`src/terminal/terminal.rs` |
| P1-R2 Wakeup 去公平锁 | `check_focus_mode_enabled` 改 `try_lock_unfair`，拿不到置 `focus_check_pending`，在 `update_last_content` 释放锁后补检；`report_focus_state` 改读 `last_content.mode`，不再二次加锁 | `src/terminal/terminal.rs` |
| P1-M1 scrollback 字节预算 | 新增 `SCROLLBACK_BUDGET_BYTES=32MB`，按 `32+24*cols` 折算行数；接入此前完全没消费的 `scrollback_lines` 设置；宽窗口在 reflow **之前**缩历史，避免先扩到 60 MB 再缩；只缩不涨 | `src/terminal/terminal.rs` |
| P1-M2 undo 栈上限 | 关闭标签的保活栈 `MAX_CLOSED_TABS` 10 → 3，把最多 ~600 MB 的隐藏终端降到 ~180 MB 级 | `src/workspace/workspace.rs` |
| P1-M3 隐藏标签释放 | 切标签时对旧标签清行缓存/扁平缓存，释放视口快照 `cells/row_ranges/row_hashes`，并通过 `Window::drop_image` 归还 Kitty GPU 纹理；切回由 `content_dirty`/按需上传重建 | `src/workspace/workspace.rs`、`src/terminal_view/terminal_view.rs`、`src/terminal/terminal.rs` |
| P1-R3 滚动行缓存复用 | 每行内容指纹（字符/颜色/flags/下划线/zerowidth/hyperlink）；全量帧检测 `new[i]==old[i+shift]` 的滚动位移，平移行缓存并只重排新进入视口的行；同时比对 `palette_hash`，OSC 改色后强制重建；不依赖 alacritty 提供滚动 damage | `src/terminal/terminal.rs`、`src/terminal_view/terminal_element.rs` |
| P1-P1 PTY 两级流水 | gather 线程独占 `Pty`/`Poller`，只读字节、处理输入/尺寸/子进程事件；parse 线程独占 `Term`+vte+扫描器；4×64 KiB 有界环（PARSE_QUEUE_DEPTH=4）；扫描器应答经 reply channel + `poller.notify()` 回到 gather 写回 | `src/terminal/local_pty.rs` |
| P1-K1 Kitty 健壮性 | 存储锁中毒改 `into_inner()` 恢复；`RgbaImage::from_raw` 失败退化为 1×1 透明帧而非 panic；单图像 placement 上限 4096 | `src/terminal/kitty/storage.rs`、`src/terminal/kitty/render.rs`、`src/terminal/kitty/exec.rs` |
| P1-A1 APC 残尾增量 | 待续 APC 不再每次 `take+extend+全量重扫`；只追加新字节、只在新增尾部找 ST，完整后把余下字节递归走正常扫描；2 字节分片 × 26 块的测试覆盖 | `src/terminal/kitty/scanner.rs`、`src/terminal/escape_scan.rs` |

验证：

- `cargo test --lib normalize_runs_never_exceeds_text_len`：1 passed。
- `cargo test --lib terminal_memory_tests`：10 passed。
- `cargo test --lib tests_runs_for_range`：5 passed。
- `cargo test --lib escape_scan`：37 passed。
- `cargo test --lib kitty::scanner`：26 passed。
- `cargo test --lib focus_check_never_blocks_on_grid_lock`：1 passed（持锁时 Wakeup 检查不阻塞、置 pending，释放后恢复）。
- `cargo test --lib scrollback_budget_shrinks_wide_windows_only`：1 passed（250 列 ≈ 5.5k 行；窄窗口不放大）。
- `cargo test --lib detect_scroll_shift_recognizes_one_line_and_clear`：1 passed（上滚/下滚/清屏三态）。
- `cargo test --lib rotate_row_caches`：1 passed（平移行缓存并重标 span 行号）。
- `cargo test --lib apc_pending_incremental_path_handles_many_tiny_chunks`：1 passed（2 字节分片走增量残尾路径）。
- `cargo test --lib placements_are_capped_per_image`：1 passed（第 4097 个 placement 被淘汰）。
- `cargo test --lib user_scroll_exposes_positive_shift`：1 passed（`scroll_line_up` 端到端识别出 -1 位移）。
- `cargo test --lib run_paint_origin_uses_passed_row_not_cached_row`：1 passed（滚动复用后文本 y 坐标必须取当前行）。
- `cargo test --lib palette_hash_changes_when_osc_override_changes`：1 passed（OSC 改色改变调色板指纹）。
- `cargo test --lib local_pty::sink_tests`：2 passed。
- `cargo test --lib local_terminal_keeps_an_input_channel`：1 passed（UI 持 listener/notice 发送端，避免工作线程关通道唤醒 GPUI 调度器）。
- `cargo test --lib parse_batch_feeds_bytes_into_grid_without_pty`：1 passed（不依赖 PTY，验证 parse 线程的持锁解析路径）。
- `cargo test --lib`（真实 PTY，完整访问权限）：**2266 passed、0 failed、3 ignored**。
- `cargo test --lib local_pty`：20 passed（含 16 条真实 PTY 端到端：输入回环、OSC/APC、Kitty 分片、CSI 16 t/DA2/XTVERSION、2048/1016、子进程退出、grok 流）。
- 说明：`workspace-write` 沙箱会拒绝 `posix_openpt`，同一套用例会报 17 个 `Operation not permitted`；这是环境限制，不是代码失败。
- `cargo test --bin deepterm --no-run`：通过（13 个改动文件全部进入二进制）。
- `cargo bench --bench terminal_render -- frame_scroll_one_line --quick`：行缓存指纹生效后指令数 17.9M → 13.5M、foreground 11.1M → 7.3M、每迭代分配 1927 → 175 次；墙钟受本机 GPU 负载噪声影响大（p50 4.1 ms，均值 19 ms），以指令/分配为准。
- `rustfmt --check`：13 个改动文件全部通过；`git diff --check` 无空白错误。


未做：SSH-PTY 输入背压。输入走 alacritty `EventLoopSender` 的无界 `std::sync::mpsc`，外部拿不到队列长度；要真正限流只能替换 sender 或 fork event loop，属于第三方接口改造。其余结构性项（PTY 两级流水、搜索、APC、资源生命周期）已落地。

### 8.1 发布后 TUI 重影回归（2026-11-02）

用户实测 pi agent 等 TUI 偶尔把同一个 shell 提示符渲染成两条。全面复核行缓存与终端管线后，确认并修复三处遗漏：

1. **`BatchedTextRun` 缓存了绝对行号（主因）**
   滚动时 P1-R3 会把整行缓存平移到新视口下标，并重标 `BackgroundSpan` / `BoxDrawingCell` / 选区 span 的 `row`，却漏了 `BatchedTextRun.start_row`。`paint` 用它算 y 坐标，于是平移过来的文字仍画在旧行位置：旧位置一条、新位置一条，正好表现为“提示符出现两次”。
   修复：删除 run 里的 `start_row`，`paint` 由调用方显式传入当前遍历 `row`；新增 `run_paint_origin_uses_passed_row_not_cached_row` 回归用例。

2. **滚动帧未强制所有行过指纹**
   `scroll_shift != 0` 但 damage 没把某行标脏时，平移后的缓存可能既不重标也不重建。现在只要发生整体滚动，所有行都强制走一遍指纹/重标或重建。

3. **行缓存复用漏掉调色板**
   行指纹只覆盖 cell 的 `Color` 枚举，OSC 4/10/11/12 改色不改变枚举、却改变最终颜色；复用会保留旧色。新增 `TerminalPalette::hash_u64()`，把 `TerminalContent.palette_hash` 纳入行缓存复用条件。

复核但未再发现同类问题的区域：PTY gather→parse 的顺序/尺寸应答/Wakeup、APC 残尾增量、隐藏标签快照与 GPU 纹理释放、scrollback 预算裁剪、search 后台化。PTY 20 条真实端到端用例仍全绿。

## 附录 A：关键代码位置速查

| 主题 | 位置 |
|---|---|
| scrollback 默认 10k / 32 MB 预算 / 上限 100k | `src/terminal/terminal.rs:58-83` |
| damage → 快照（Full/Partial） | `src/terminal/terminal.rs:3441-3600` |
| 全量快照构造 | `src/terminal/terminal.rs:3399-3438` |
| Wakeup 焦点锁 | `src/terminal/terminal.rs:2370-2373/3295-3301/2440` |
| 读线程 read/parse/lease | `src/terminal/local_pty.rs:515-600` |
| 读线程控制消息/丢弃 | `src/terminal/local_pty.rs:190-211/334-451` |
| PTY 写重试 | `src/terminal/local_pty.rs:70-101` |
| 扫描器 8 次 memmem | `src/terminal/kitty/scanner.rs:495-512` |
| 远程扫描 memmem | `src/terminal/terminal.rs:2858-2860` |
| 行缓存 dirty 处理 | `src/terminal_view/terminal_element.rs:1637-1716` |
| runs 长度不匹配仍 shape | `src/components/input/element.rs:2051-2064` |
| 最后一个标签关窗 | `src/workspace/workspace.rs:670/2174`、`src/lib.rs:770-778/829-871` |
| undo 栈保活 | `src/workspace/workspace.rs:261-273/2269-2279` |
| SSH-PTY 关闭 | `src/terminal/terminal.rs:4099-4102`、`src/ssh/ssh_pty.rs:241-266` |
| Kitty 上限 / 锁中毒 | `src/terminal/terminal.rs:235/956/964`、`src/terminal_view/terminal_element.rs:2376` |
| `RgbaImage::from_raw` | `src/terminal/kitty/render.rs:630-634` |
| APC 残尾 concat | `src/terminal/kitty/scanner.rs:486-497` |
| 日志默认 Info + 1 MiB 单文件裁尾 | `src/main.rs:47-125` |
| scrollback 死配置 | `src/settings/content.rs:135` |

## 附录 B：Ghostty 关键提交

| 主题 | 提交 / PR |
|---|---|
| PTY 两级流水 | `0535770e3` (#13209) |
| VT 处理优化 | `2da015cd6` (#13220) |
| 渲染态锁优化 | `98a7c0f6f` (#13227) |
| 空闲页归还 OS | `ad692f1e8` (#13245) |
| bitmap allocator 页尺寸 | `bc6ca3ff5` (#13240) |
| scrollback 页压缩 | `7e02af879` (#13264) |
| 零即空 / 不触碰池项 | `07bccf7a3` (#14137)、`31bdcd5a7` (#14130) |
| 隐藏标签释放 GPU | `683d8db64` (#14017) |
| 长 uptime 每帧重建 | `e69dc2bee` (#13585) |
| 关闭窗口泄漏 | `594ee212b` (#13601) |
| 读终端文本泄漏 | `4803d58bb` (#12020) |

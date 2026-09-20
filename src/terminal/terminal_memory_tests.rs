//! 终端内存占用测量（`cargo test -- --nocapture terminal_memory` 可见数据输出）。
//!
//! 这些测试是优化工作的**证据链**：断言用宽松阈值保证稳定，真实数值通过
//! `--nocapture` 打印进测试日志，供 `docs/PERF_*.md` 引用。
//!
//! 测量口径：
//! - footprint：进程 `phys_footprint` 差值（Xcode 内存仪表口径），仅 macOS；
//! - 分配抖动：`AllocObserver` 统计订阅线程上的堆分配次数 / 字节。

use crate::terminal::Terminal;
use crate::terminal::terminal::MAX_SNAPSHOT_LOCK_SKIPS;
use crate::terminal_view::TerminalView;
use crate::utils::alloc_probe::AllocObserver;
use gpui::{AppContext as _, TestAppContext};

/// 灌入 `lines` 行确定性 ASCII 文本。
fn feed(terminal: &mut Terminal, lines: usize, seq: usize, cx: &mut gpui::Context<Terminal>) {
    let mut bytes = Vec::with_capacity(lines * 96);
    for i in 0..lines {
        let text =
            format!("s{i}q{seq} The quick brown fox jumps over the lazy dog 0123456789\r\n");
        bytes.extend_from_slice(text.as_bytes());
    }
    terminal.handle_remote_data(&bytes, cx);
}

/// 比较不同 scrollback 配置下、写满历史后单个 Terminal 的内存占用。
///
/// 写入 2×scrollback 行迫使网格历史真正分配到配置上限。
/// 证据（打印到测试日志）：各配置的差值与「每 1000 行」斜率。
///
/// **对并发敏感**：footprint 是进程级的量（见 [`Footprint`]），并发跑的测试
/// 只要在分配大块内存就会污染差值。本测试目前是仓库里唯一基于 footprint 的
/// 测量，所以这里的进程级互斥量并不隔离别的 footprint 测试（没有别的）；
/// 它挡不住的是**普通测试**的分配，那些不受这把锁约束。
#[cfg(target_os = "macos")]
#[gpui::test]
fn terminal_scrollback_footprint_scales_linearly(cx: &mut TestAppContext) {
    use crate::utils::alloc_probe::Footprint;
    use std::sync::{Mutex, OnceLock};

    fn footprint_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    let _guard = footprint_lock().lock().unwrap_or_else(|e| e.into_inner());

    // 预热：先建一个临时终端并完整喂满，把「首个样本要付的首次分配」（字体
    // 表、网格骨架、会话骨架等一次性成本）从 1k 样本的基线里剥离，避免 1k
    // 档的读数被一次性成本放大。
    {
        let warmup = cx.update(|cx| {
            let t = Terminal::new_headless(cx, 10_000);
            t.update(cx, |t, cx| feed(t, 1_000, 999, cx));
            t
        });
        let _ = Footprint::capture();
        drop(warmup);
        cx.run_until_parked();
    }

    let samples: &[(usize, &str)] = &[(1_000, "1k"), (5_000, "5k"), (10_000, "10k")];
    let mut results: Vec<(usize, u64)> = Vec::new();

    for &(scrollback, label) in samples {
        let baseline = Footprint::capture();
        let entity = cx.update(|cx| {
            let terminal = Terminal::new_headless(cx, scrollback);
            terminal.update(cx, |terminal, cx| feed(terminal, scrollback * 2, 0, cx));
            terminal
        });
        let after_feed = Footprint::capture();
        let delta = baseline.delta_bytes(after_feed);
        results.push((scrollback, delta));
        println!(
            "[footprint] scrollback={label:>4}: +{:.2} MB ({:.2} MB / 1000 行)",
            delta as f64 / 1024.0 / 1024.0,
            delta as f64 / 1024.0 / 1024.0 / (scrollback as f64 / 1000.0),
        );
        // 引用计数归零后 gpui 释放实体；跑完剩余任务避免跨样本噪声。
        drop(entity);
        cx.run_until_parked();
    }

    // scrollback 是容量参数：写入量相同时更大的上限只应多占历史网格。
    //
    // 这个断言只证明**方向**，不证明精确值。`phys_footprint` 是进程级的量，
    // 读数取决于分配器此刻的页复用状态与并发测试的瞬时占用：实测全量套件下
    // 1k 档在 0.8 ~ 4.3 MB 之间跳、5k 档在 5.4 ~ 26.8 MB 之间跳，于是相邻两
    // 档的差值经常读成负数。**不要**改回「相邻档斜率必须为正」——那个断言在
    // 本机全量套件下 6 轮里 5 轮失败，是必然 flaky 的。
    //
    // 真正守得住的是「更大的上限不会占得更少」：它与分配器状态无关，只在
    // scrollback 容量真的没生效时才会失败。精确的每行成本看单独运行
    // （`cargo test --lib terminal_scrollback_footprint -- --nocapture`）的日志。
    let small = results[0].1;
    let large = results[2].1;
    assert!(
        large >= small,
        "10k scrollback ({large} B) 不应低于 1k ({small} B)"
    );

    // 斜率仅供参考：页复用会把它压成 0 或负数，所以只打印、不断言。
    let slope = (results[2].1 as i64 - results[1].1 as i64) / 5;
    println!(
        "[footprint] 斜率（5k→10k，仅供参考，受页复用影响）: {:.2} MB / 1000 行",
        slope as f64 / 1024.0 / 1024.0
    );
}

/// 稳态「写一行 → 快照」的每帧分配次数（快照缓冲复用的证据）。
///
/// `update_last_content` 应复用 `cells` / `row_ranges` / `dirty_rows` 的
/// 既有容量：预热后单行新增的分配只剩 dirty 记账与 row_ranges 追加，
/// 不应重新分配整个视口缓冲。
#[gpui::test]
fn terminal_per_frame_snapshot_allocations(cx: &mut TestAppContext) {
    let terminal = cx.update(|cx| {
        let terminal = Terminal::new_headless(cx, 1_000);
        terminal.update(cx, |terminal, cx| feed(terminal, 200, 0, cx));
        // 预热：分配一次全量快照缓冲容量。
        terminal.update(cx, |terminal, cx| terminal.sync_for_test(cx));
        terminal
    });

    let mut per_frame = Vec::new();
    for seq in 1..=5 {
        let mut cx2 = cx.clone();
        let terminal = terminal.clone();
        let (_, stats) = AllocObserver::scope(move || {
            terminal.update(&mut cx2, |terminal, cx| {
                feed(terminal, 1, seq, cx);
                terminal.sync_for_test(cx);
            });
        });
        per_frame.push(stats.allocations);
    }
    println!("[alloc] 稳态单行写入+快照分配次数: {per_frame:?}");
    let steady = *per_frame.last().unwrap();
    // 宽松阈值：视口缓冲不应整帧重建（那会是数百次分配）。
    assert!(
        steady < 20,
        "稳态单帧快照分配 {steady} 次过高，疑似快照缓冲未复用"
    );
}

/// 内容完全未变时 sync 的分配次数（应为 0：走 fill(false) 短路分支）。
#[gpui::test]
fn terminal_unchanged_sync_zero_alloc(cx: &mut TestAppContext) {
    let terminal = cx.update(|cx| {
        let terminal = Terminal::new_headless(cx, 1_000);
        terminal.update(cx, |terminal, cx| feed(terminal, 200, 0, cx));
        terminal.update(cx, |terminal, cx| terminal.sync_for_test(cx));
        terminal
    });

    // 预热：吸收 entity.update 机制与 TLS 的任何一次性分配。
    terminal.update(cx, |terminal, cx| terminal.sync_for_test(cx));

    let mut cx2 = cx.clone();
    let terminal = terminal.clone();
    let (_, stats) = AllocObserver::scope(move || {
        terminal.update(&mut cx2, |terminal, cx| terminal.sync_for_test(cx));
    });
    println!("[alloc] 内容未变的 sync 分配次数: {}", stats.allocations);
    // 允许 gpui update 机制的常数级记账（<=2），快照缓冲本身不应重建。
    assert!(
        stats.allocations <= 2,
        "内容未变的 sync 分配 {} 次过高，疑似快照缓冲未复用",
        stats.allocations
    );
}

/// prepaint 行缓存管线的每帧分配次数（O2 的基线证据）。
///
/// 用 `VisualTestContext::draw` 跑真实的 TerminalElement prepaint
/// （NoopTextSystem 排版，行缓存管线与缓存重建逻辑与生产一致）：
/// - 稳态帧（无 damage）：行缓存全命中 → 仅剩 draw 框架常量分配；
/// - 单行 dirty 帧：O2 前 flattened 全量重建（实测 1932 次分配 / 8.1MB），
///   O2 后 paint 直接遍历行缓存，只剩 1 行增量更新的分配（实测 44 次）。
#[gpui::test]
fn terminal_view_prepaint_frame_allocations(cx: &mut TestAppContext) {
    use crate::sidebar_view::font_settings::FontState;
    use crate::terminal_view::TerminalElement;
    use crate::theme;
    use gpui::{px, size, point};

    struct Host {
        view: gpui::Entity<TerminalView>,
        terminal: gpui::Entity<Terminal>,
    }
    impl gpui::Render for Host {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    cx.update(|cx| {
        theme::init(cx);
        FontState::init(cx);
    });

    let (host, cx) = cx.add_window_view(|_window, cx| {
        let terminal = Terminal::new_headless(cx, 1_000);
        terminal.update(cx, |terminal, cx| feed(terminal, 200, 0, cx));
        let view = cx.new(|cx| TerminalView::build_headless(cx, terminal.clone()));
        Host { view, terminal }
    });

    // 预热帧：建立行缓存（首次全量排版 + 缓存分配不计数）。
    let (view, terminal) = host.read_with(cx, |h, _| (h.view.clone(), h.terminal.clone()));
    let focus_handle = view.read_with(cx, |v, _| v.focus_handle_for_test());
    let make_element =
        || TerminalElement::new(terminal.clone(), focus_handle.clone(), true, true, view.clone());
    cx.draw(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)), |_, _| {
        make_element()
    });

    // 场景 A：稳态帧（无 damage、无选区变化、无悬停变化）。
    let (_, stats_clean) = AllocObserver::scope(|| {
        cx.draw(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)), |_, _| {
            make_element()
        });
    });
    println!("[alloc] 稳态 prepaint 分配: {} 次", stats_clean.allocations);

    // 场景 B：单行 dirty（覆盖光标单元格 + 左移复位，不产生滚动 ——
    // 与 bench 的 frame_single_row_dirty 同口径）。
    let (_, stats_dirty) = AllocObserver::scope(|| {
        terminal.update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"X\x1b[1D", cx);
        });
        cx.draw(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)), |_, _| {
            make_element()
        });
    });
    println!(
        "[alloc] 单行 dirty prepaint 分配: {} 次 / {}（O2 前基线：1932 次 / 8.1 MB）",
        stats_dirty.allocations,
        crate::utils::alloc_probe::format_bytes(stats_dirty.bytes),
    );

    // 稳态帧的常量分配（draw 框架记账 + 光标块字符每帧 shape 一次等）
    // 与视口规模无关：断言 55±常数区间，后续优化此值只降不升。
    // DEEPTERM_VALIDATE_ROW_CACHE=1 时 assert_row_consistency 会为每行做
    // 全量参考重算（本身大量分配），仅提高上限不做精确断言。
    let clean_limit = if std::env::var_os("DEEPTERM_VALIDATE_ROW_CACHE").is_some() {
        1400
    } else {
        80
    };
    assert!(
        stats_clean.allocations <= clean_limit,
        "稳态 prepaint 分配 {} 次过高，行缓存复用疑似失效",
        stats_clean.allocations
    );
    // O2 之后单行 dirty 帧不应再出现全视口级的重排版分配（实测 44 次，
    // 基线 1932 次）；上限取 2 倍余量防退化。
    let dirty_limit = if std::env::var_os("DEEPTERM_VALIDATE_ROW_CACHE").is_some() {
        1400
    } else {
        120
    };
    assert!(
        stats_dirty.allocations < dirty_limit,
        "单行 dirty prepaint 分配 {} 次过高，flattened 全量重建疑似回归",
        stats_dirty.allocations
    );
}


/// 增量快照与整体重建的**逐格平价**。
///
/// `update_last_content` 在 `Partial` damage 下只覆盖变过的行（增量路径），
/// 其余行沿用上一帧的 `IndexedCell`。这个优化的正确性完全取决于「增量结果
/// == 整体重建结果」，所以这里用一条参考实现（每次都对整个视口重建）做
/// 随机平价：每步随机编辑后，先走生产路径（按真实 damage 决定增量/全量），
/// 再强制走整体重建，逐格比对 `(point, PackedCell)`。
///
/// 参考侧用的是 `force_full_damage = true` 分支——与增量路径是同一函数里的
/// 两条真实分支，因此断言的是等价性，而不是复制一份逻辑的自我印证。
#[gpui::test]
fn incremental_snapshot_matches_full_rebuild(cx: &mut TestAppContext) {
    let terminal = cx.update(|cx| Terminal::new_headless(cx, 2_000));

    // 固定种子的 XorShift：失败可复现。
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };

    for step in 0..1500usize {
        // 随机一条编辑指令，覆盖「只动几格」「整行重写」「换行滚动」三类。
        //
        // 第 5、6 类专门给 `IndexedCell` 的**非 PackedCell 字段**制造非 None
        // 取值：OSC8 超链接（`hyperlink`）与组合变音符（`zerowidth`）。没有
        // 它们时这两个字段恒为 None，整格比较再严也证明不了增量路径有没
        // 有把它们带上——实测把增量分支的 `hyperlink` 写死成 `None`，不含
        // 这两类的随机流照样全绿。
        let bytes: Vec<u8> = match next() % 7 {
            0 => format!("x{}", step % 10).into_bytes(),
            1 => b"\r\n".to_vec(),
            2 => format!("\x1b[{};{}H", (next() % 20) + 1, (next() % 60) + 1).into_bytes(),
            3 => format!("line {step} 0123456789 abcdefghijklmnopqrstuvwxyz\r\n").into_bytes(),
            4 => format!("\x1b[{}K", next() % 3).into_bytes(),
            // OSC8 链接：开链接 → 写文本 → 关链接。
            5 => format!(
                "\x1b]8;;https://example.com/{step}\x1b\\link{step}\x1b]8;;\x1b\\"
            )
            .into_bytes(),
            // 组合变音符（U+0301）落在前一个基字符格上，产生非 None zerowidth。
            _ => format!("e\u{0301}{step}").into_bytes(),
        };

        let (incremental, full) = cx.update(|cx| {
            terminal.update(cx, |t, cx| {
                t.handle_remote_data(&bytes, cx);
                // 生产路径：按真实 damage 决定增量 / 全量。
                t.sync_for_test(cx);
                let incremental = t.last_content().cells.clone();
                // 参考路径：强制整体重建。
                t.full_snapshot_for_test();
                let full = t.last_content().cells.clone();
                (incremental, full)
            })
        });

        assert_eq!(
            incremental.len(),
            full.len(),
            "step {step}: 增量与全量的单元格数不同（{} vs {}）",
            incremental.len(),
            full.len()
        );
        for (i, (a, b)) in incremental.iter().zip(full.iter()).enumerate() {
            // 整格比较（`IndexedCell: PartialEq`），覆盖 point / cell /
            // zerowidth / hyperlink。手写字段列表会漏掉后来新增的字段——
            // `zerowidth` 正是 Kitty `U=1` 占位符所依赖的那个。
            assert_eq!(
                a, b,
                "step {step}: 第 {i} 格不一致（增量 {a:?} vs 全量 {b:?}）"
            );
        }
    }

    // 没有这一条，测试在增量路径整体失效（永远走全量重建）时也会全绿——
    // 那就成了「全量 vs 全量」的自我印证，失去平价的意义。
    let hits = cx.update(|cx| terminal.read(cx).incremental_snapshot_count_for_test());
    assert!(
        hits > 0,
        "1500 步随机编辑里增量快照分支一次都没走到，平价测试没有覆盖到被优化的路径"
    );
    println!("[alloc] 增量快照分支命中 {hits}/1500 步");
}

/// 网格锁被占住时，快照刷新既不阻塞也不永久饿死。
///
/// `update_last_content` 先 `try_lock_unfair()`：拿不到就跳过本帧（返回
/// false、`content_dirty` 保持置位），连续跳过达到
/// [`MAX_SNAPSHOT_LOCK_SKIPS`] 后退回公平锁。两个失败模式各由一条断言钉住，
/// **且都发生在锁仍被占住的时刻**（这是关键：若等到占锁线程释放后再断言，
/// `try_lock_unfair` 自己就会成功，回退分支根本走不到，测试会在实现退化时
/// 依然全绿——那种写法只证明了「跳过」，没证明「防饿死」）：
///
/// 1. **阻塞**（旧行为：每次都公平 `lock()`）：阶段 1 会一直等到占锁线程
///    放行，届时 `content_dirty` 已被清掉，第一条断言失败。
/// 2. **饿死**（只跳过、永不退回公平锁）：阶段 3 在锁仍被占住时立刻返回
///    false，快照没刷新，第三条断言失败。
///
/// 占锁线程在收到「即将走回退分支」的信号后再等一小段才放行，因此阶段 3
/// 必然发生在锁被占住期间；同时它带 5 秒硬上限，任何实现下都不会把测试挂死。
#[gpui::test]
fn snapshot_refresh_yields_to_grid_lock_without_starving(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // 阶段划分依赖「跳过上限至少为 2」：阶段 1 消耗第一次跳过，阶段 2 把它顶
    // 到上限，阶段 3 才落到回退分支。上限为 1 时阶段 1 就会去阻塞，断言失真。
    assert!(
        MAX_SNAPSHOT_LOCK_SKIPS >= 2,
        "本测试的阶段性依赖 MAX_SNAPSHOT_LOCK_SKIPS >= 2（当前 {MAX_SNAPSHOT_LOCK_SKIPS}）"
    );

    let terminal = cx.update(|cx| {
        let t = Terminal::new_headless(cx, 200);
        t.update(cx, |t, cx| feed(t, 60, 0, cx));
        t
    });

    let term_lock = cx.update(|cx| terminal.read(cx).term_lock_for_test());

    // 另一个线程公平地占住网格锁，模拟「读线程正在喂一个大 chunk」。
    //
    // 收到放行信号后**再**持锁 1 秒才释放：信号由阶段 3 之前发出，此时跳过
    // 计数已顶到上限，所以阶段 3 一定落在「锁仍被占住」的窗口里。1 秒相对
    // 阶段 3 那次 `cx.update` 的分派开销是极大余量；反过来，占锁线程也带
    // 5 秒硬上限，任何实现下都不会把测试挂死。
    let held = Arc::new(AtomicBool::new(false));
    let held_for_thread = held.clone();
    let (armed_tx, armed_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = term_lock.lock();
        held_for_thread.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while armed_rx.try_recv().is_err() {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(1000));
    });

    // 等它真的拿到锁再继续，避免把「还没抢到」误判成「抢到了」。
    let deadline = Instant::now() + Duration::from_secs(5);
    while !held.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "占锁线程 5 秒内没拿到锁，测试环境异常"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 阶段 1：锁被占住期间 UI 侧照常请求重绘，必须立刻返回且**不谎报**已刷新。
    let still_dirty = cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.mark_content_dirty_for_test();
            t.sync_for_test(cx);
            t.content_dirty_for_test()
        })
    });
    assert!(
        still_dirty,
        "网格锁被占住时 sync 不该阻塞等待，也不该把 content_dirty 清掉"
    );

    // 阶段 2：继续跳过，把连续跳过计数顶到上限。这些调用同样必须立刻返回，
    // 且不能清掉 content_dirty（锁明明还被占着）。
    let skipped_more = cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            for _ in 1..MAX_SNAPSHOT_LOCK_SKIPS {
                t.sync_for_test(cx);
                if !t.content_dirty_for_test() {
                    return false;
                }
            }
            true
        })
    });
    assert!(
        skipped_more,
        "跳过计数未达上限前 sync 就清了 content_dirty：锁还被占着，不该拿到快照"
    );

    // 阶段 3：再跳一次就达到上限，必须退回公平锁——阻塞到占锁线程放行，然后
    // 真正刷新快照。只跳过、永不回退的实现会在这里立刻返回 false。
    armed_tx.send(()).expect("放行信号应送达占锁线程");
    let caught_up = cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.sync_for_test(cx);
            !t.content_dirty_for_test()
        })
    });
    assert!(
        caught_up,
        "达到跳过上限后 sync 仍未退回公平锁：占锁期间快照永远刷不了，画面被饿住"
    );

    holder.join().expect("占锁线程不应 panic");
}

/// 跳过快照刷新后，**必须有东西再排一帧**把快照追平。
///
/// 这是「跳过本帧」策略最容易漏掉的一半：`sync` 的唯一调用点是元素
/// `prepaint`，GPUI 又不按 vsync 空转。所以如果一次跳过之后不再有新输出
/// （读线程 drain 完退出、SSH 断连、`ChildExit` 这类「最后一次唤醒」正好撞上
/// 被占住的锁），就再没有任何东西会驱动下一帧——`content_dirty` 永久置位，
/// 画面停在旧内容上直到用户碰鼠标。
///
/// 本测试不依赖任何真实计时：占锁线程持有锁，直到补帧任务自己把快照刷完
/// （或超时）。没有 `schedule_snapshot_retry` 时，`sync_for_test` 之后不再有
/// 任何推进，`content_dirty` 会一直为真，断言失败。
#[gpui::test]
fn skipped_snapshot_schedules_a_retry_frame(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let terminal = cx.update(|cx| {
        let t = Terminal::new_headless(cx, 200);
        t.update(cx, |t, cx| feed(t, 60, 0, cx));
        t
    });
    let term_lock = cx.update(|cx| terminal.read(cx).term_lock_for_test());

    // 占锁线程：先拿到锁，然后在「被要求放行」后立刻退出。测试的推进方式是把
    // 虚拟时钟往前拨，所以这里用真实时间短睡，配合有界轮询。
    let held = Arc::new(AtomicBool::new(false));
    let held_for_thread = held.clone();
    let release = Arc::new(AtomicBool::new(false));
    let release_for_thread = release.clone();
    let holder = std::thread::spawn(move || {
        let _guard = term_lock.lock();
        held_for_thread.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !release_for_thread.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while !held.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "占锁线程 5 秒内没拿到锁，测试环境异常"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 锁被占住：这一帧必须跳过，并因此排下一个补帧任务。
    let skipped = cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.mark_content_dirty_for_test();
            t.sync_for_test(cx);
            t.content_dirty_for_test()
        })
    });
    assert!(skipped, "锁被占住时首帧应跳过");

    // 放锁。此后**不再调用 sync_for_test**——只有补帧任务能把快照追平。
    release.store(true, Ordering::SeqCst);

    // 推进虚拟时钟 + 泵事件循环，让补帧定时器到点并跑完它的 update。
    let caught_up = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.background_executor
                .advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            if !cx.update(|cx| terminal.read(cx).content_dirty_for_test()) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    assert!(
        caught_up,
        "跳过快照后没有任何补帧机制：content_dirty 永久置位，画面会停在旧内容上"
    );

    holder.join().expect("占锁线程不应 panic");
}

/// 行切片与视口行数不一致时，兜底分支必须**真的**退化成全量重建。
///
/// `update_last_content` 的第三条分支是防御性的：生产路径上不可达（几何
/// 变化必然伴随 `Full` damage，已被 `rebuild_all` 拦下），但它的存在意义
/// 正是「上游一旦改了 damage 语义，退化成正确但慢的全量重建，而不是切片
/// 越界 panic」。防御性代码从不执行就无法证明它能兜住，所以这里用
/// `corrupt_row_ranges_for_test` 人为破坏不变量走进去，断言两件事：
///
/// 1. 兜底分支确实被命中（`snapshot_fallback_count_for_test` 增长）；
/// 2. 重建结果与「本来就全量重建」逐格一致——即它没有把 `row_ranges` 留在
///    损坏状态，也没有漏掉任何一行。
#[gpui::test]
fn inconsistent_row_ranges_fall_back_to_full_rebuild(cx: &mut TestAppContext) {
    let terminal = cx.update(|cx| {
        let t = Terminal::new_headless(cx, 500);
        t.update(cx, |t, cx| feed(t, 80, 0, cx));
        t
    });

    // 先跑一次正常快照，建立自洽的 row_ranges。
    cx.update(|cx| {
        terminal.update(cx, |t, cx| t.sync_for_test(cx));
    });

    let (before_count, expected, corrupted_len, rows) = cx.update(|cx| {
        terminal.update(cx, |t, _cx| {
            let rows = t.last_content().rows;
            let expected = t.last_content().cells.clone();
            let before = t.snapshot_fallback_count_for_test();
            t.corrupt_row_ranges_for_test();
            let corrupted = t.row_range_count_for_test();
            (before, expected, corrupted, rows)
        })
    });
    assert_eq!(
        corrupted_len,
        rows - 1,
        "破坏钩子应让行切片数比视口行数少 1（{corrupted_len} vs {rows}）"
    );

    // 制造一次 **Partial** damage，逼 update_last_content 走到
    // 「row_ranges.len() != num_rows」那条分支。
    //
    // 这里必须只写一个字符、不能带换行：视口早已被 80 行喂满，任何换行都会
    // 触发 `scroll_up` → `mark_fully_damaged()` → `rebuild_all`，那样就直接
    // 走全量重建了，根本到不了兜底分支（实测如此）。
    let (after_count, rebuilt) = cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.handle_remote_data(b"X", cx);
            t.sync_for_test(cx);
            (
                t.snapshot_fallback_count_for_test(),
                t.last_content().cells.clone(),
            )
        })
    });

    assert!(
        after_count > before_count,
        "行切片与视口行数不一致时没有走兜底分支（计数 {before_count} -> {after_count}），         说明不变量被打破后会直接掉进增量路径的切片越界"
    );
    assert_eq!(
        rebuilt.len(),
        expected.len(),
        "兜底重建后的单元格数应与正常全量一致（{} vs {}）",
        rebuilt.len(),
        expected.len()
    );
    assert_eq!(
        cx.update(|cx| terminal.read(cx).row_range_count_for_test()),
        rows,
        "兜底重建后 row_ranges 应恢复到与视口行数一致"
    );
}

/// 增量快照在**几何变化与滚动**下的平价（增量路径最危险的边界）。
///
/// 前一个平价测试只做「编辑」，走不到 `Full` damage 的几种触发条件。这里
/// 专门把它们拉进来：
///
/// - **滚动**（`scroll_display`）：视口整体错位，增量路径若被误用，行号与
///   内容会整体串位——这是最容易出错、也最容易被漏测的一类；
/// - **resize**（行列变化，触发 reflow）：行切片长度会变，增量覆盖必须退回
///   整体重建；
/// - **滚回原位**：错位再还原，检验行号换算在往返后仍然自洽。
///
/// 每一步之后同样比对「生产路径（按真实 damage 决定增量/全量）」与
/// 「强制整体重建」的逐格结果。
#[gpui::test]
fn incremental_snapshot_matches_full_rebuild_across_scroll_and_resize(
    cx: &mut TestAppContext,
) {
    let terminal = cx.update(|cx| {
        let t = Terminal::new_headless(cx, 500);
        t.update(cx, |t, cx| feed(t, 120, 0, cx));
        t
    });

    // 每步做一件事，然后比对两种快照路径。
    let compare = |cx: &mut TestAppContext, label: &str| {
        let (incremental, full) = cx.update(|cx| {
            terminal.update(cx, |t, cx| {
                t.sync_for_test(cx);
                let incremental = t.last_content().cells.clone();
                t.full_snapshot_for_test();
                let full = t.last_content().cells.clone();
                (incremental, full)
            })
        });
        assert_eq!(
            incremental.len(),
            full.len(),
            "{label}: 增量与全量的单元格数不同（{} vs {}）",
            incremental.len(),
            full.len()
        );
        for (i, (a, b)) in incremental.iter().zip(full.iter()).enumerate() {
            // 整格比较，理由同上：不遗漏 zerowidth / hyperlink。
            assert_eq!(
                a, b,
                "{label}: 第 {i} 格不一致（增量 {a:?} vs 全量 {b:?}）"
            );
        }
    };

    // 1) 向上滚入 scrollback，逐步加深。
    for n in 1..=6 {
        cx.update(|cx| {
            terminal.update(cx, |t, cx| {
                let _ = cx;
                t.scroll_line_up();
            });
        });
        compare(cx, &format!("向上滚动第 {n} 次"));
    }

    // 2) 滚回底部（视口错位再还原）。
    cx.update(|cx| {
        terminal.update(cx, |t, _cx| {
            t.scroll_to_bottom();
        });
    });
    compare(cx, "滚回底部");

    // 3) 滚到底部之后继续产生输出：新行进入 scrollback，视口内容上移。
    for n in 1..=4 {
        cx.update(|cx| {
            terminal.update(cx, |t, cx| {
                t.handle_remote_data(b"after-scroll output line\r\n", cx);
            });
        });
        compare(cx, &format!("滚动后继续输出第 {n} 次"));
    }

    // 4) 滚动过程中改内容：先滚上去，再写（写会落在网格底部，可能不可见）。
    cx.update(|cx| {
        terminal.update(cx, |t, _cx| {
            t.scroll_page_up();
        });
    });
    compare(cx, "滚动后未写");
    cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.handle_remote_data(b"written-while-scrolled\r\n", cx);
        });
    });
    compare(cx, "滚动中写入");

    // 5) resize：直接改网格几何（等价于 `apply_pending_resize` 里
    //    `term.resize` + 强制全量的那一步），检验增量覆盖在行切片长度变化时
    //    会退回整体重建。
    cx.update(|cx| {
        terminal.update(cx, |t, cx| {
            t.scroll_to_bottom();
            let (cols, rows) = {
                let c = t.last_content();
                (c.cols, c.rows)
            };
            for (new_cols, new_rows) in [(cols + 10, rows), (cols, rows + 5), (cols - 7, rows + 3)] {
                t.resize(
                    crate::terminal::terminal::TerminalBounds::new(
                        gpui::px(16.0),
                        gpui::px(9.0),
                        gpui::Bounds::new(
                            gpui::point(gpui::px(0.0), gpui::px(0.0)),
                            gpui::size(gpui::px(9.0 * new_cols as f32), gpui::px(16.0 * new_rows as f32)),
                        ),
                    ),
                    1.0,
                    cx,
                );
                t.apply_pending_resize_for_test(cx);
                let _ = (new_cols, new_rows);
            }
        });
    });
    compare(cx, "resize 之后");
}

/// 增量快照的**确定性**收益度量：每帧少重建多少格。
///
/// wall-clock 基准在本机噪声太大（实测同一二进制相邻两轮可差 30%，连不受
/// 本轮改动影响的 `frame_no_damage` 都会漂移），所以这里改用不受负载影响的
/// 计数口径：`IndexedCell` 的**重建个数**。
///
/// 用 bench 同款的「单行写入 + 光标复位」稳态负载跑 N 帧，比较两种策略的
/// 总重建格数：
///
/// - 生产路径（增量）：每帧只重建真正变过的那些格；
/// - 参考路径（全量）：每帧重建整个视口。
///
/// 断言增量 ≤ 全量的一半——打字/回显是终端最常见的稳态场景，收益应当是
/// 数量级的，而不是几个百分点。
#[gpui::test]
fn incremental_snapshot_rebuilds_far_fewer_cells(cx: &mut TestAppContext) {
    const FRAMES: u64 = 200;

    // 单行写入 + 左移复位：只脏光标所在的那一格，不产生滚动
    // （与 benches/terminal_render.rs 的 frame_single_row_dirty 同口径）。
    let drive = |cx: &mut TestAppContext, full: bool| -> u64 {
        let terminal = cx.update(|cx| {
            let t = Terminal::new_headless(cx, 1_000);
            t.update(cx, |t, cx| feed(t, 60, 0, cx));
            t
        });
        // 预热：让行切片长度稳定到视口行数。
        cx.update(|cx| {
            terminal.update(cx, |t, cx| t.sync_for_test(cx));
        });

        let before = cx.update(|cx| terminal.read(cx).snapshot_cells_built_for_test());
        for _ in 0..FRAMES {
            cx.update(|cx| {
                terminal.update(cx, |t, cx| {
                    t.handle_remote_data(b"X\x1b[1D", cx);
                    if full {
                        t.full_snapshot_for_test();
                    } else {
                        t.sync_for_test(cx);
                    }
                });
            });
        }
        let after = cx.update(|cx| terminal.read(cx).snapshot_cells_built_for_test());
        after - before
    };

    let incremental = drive(cx, false);
    let full = drive(cx, true);

    println!(
        "[alloc] {FRAMES} 帧稳态打字：增量重建 {incremental} 格，全量重建 {full} 格（省 {:.1}%）",
        (1.0 - incremental as f64 / full as f64) * 100.0
    );

    assert!(
        full > 0,
        "全量参考路径没有重建任何格，度量口径失效"
    );
    assert!(
        incremental * 2 <= full,
        "增量快照 {FRAMES} 帧重建 {incremental} 格，未达全量 {full} 格的一半，\
         增量路径疑似退化为全量"
    );
}

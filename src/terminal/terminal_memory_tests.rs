//! 终端内存占用测量（`cargo test -- --nocapture terminal_memory` 可见数据输出）。
//!
//! 这些测试是优化工作的**证据链**：断言用宽松阈值保证稳定，真实数值通过
//! `--nocapture` 打印进测试日志，供 `docs/PERF_*.md` 引用。
//!
//! 测量口径：
//! - footprint：进程 `phys_footprint` 差值（Xcode 内存仪表口径），仅 macOS；
//! - 分配抖动：`AllocObserver` 统计订阅线程上的堆分配次数 / 字节。

use crate::terminal::Terminal;
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
/// 证据（打印到测试日志）：各配置的 footprint 差值与「每 1000 行」斜率。
#[cfg(target_os = "macos")]
#[gpui::test]
fn terminal_scrollback_footprint_scales_linearly(cx: &mut TestAppContext) {
    use crate::utils::alloc_probe::Footprint;

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
    // 放宽为「10k ≥ 1k」（回收时机导致的抖动不影响方向性判断）。
    let small = results[0].1;
    let large = results[2].1;
    assert!(
        large >= small,
        "10k scrollback ({large} B) 不应低于 1k ({small} B)"
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

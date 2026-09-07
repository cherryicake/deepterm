//! 终端渲染路径基准（criterion + gpui `#[gpui::bench]`）。
//!
//! 每个场景通过 [`gpui::BenchAppContext::bench_renderer`] 测量**真实**的
//! TerminalElement 帧管线：prepaint（damage 消费、行缓存、CoreText 排版）、
//! paint（quad/字形绘制）与 present（无头 Metal 渲染器提交）。
//! 终端内容用确定性的 ANSI 字节流驱动（[`Terminal::new_headless`]，
//! 无 PTY / 子进程），保证迭代之间输入可复现。
//!
//! 运行：`cargo bench --bench terminal_render`
//! HTML 报告：`target/criterion/report/index.html`

use deepterm::sidebar_view::font_settings::FontState;
use deepterm::terminal::Terminal;
use deepterm::terminal_view::TerminalView;
use deepterm::theme;
use gpui::{BenchAppContext, MouseButton, MouseDownEvent, MouseMoveEvent, Point};

fn init_bench_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        theme::init(cx);
        FontState::init(cx);
    });
}

/// 创建挂在窗口根节点上的无驱动终端视图。
fn make_terminal_view(cx: &mut BenchAppContext) -> gpui::Entity<TerminalView> {
    let mut window = cx.add_empty_window();
    let terminal = cx.update(|cx| Terminal::new_headless(cx, 10_000));
    window.update(|window, cx| {
        window.replace_root(cx, |_window, cx| {
            TerminalView::build_headless(cx, terminal)
        })
    })
}

/// 向终端灌入 `lines` 行确定性输出。`cx` 为 App 上下文。
fn feed_lines(view: &gpui::Entity<TerminalView>, lines: usize, seq: usize, cx: &mut gpui::App) {
    let terminal = view.read(cx).terminal().clone();
    terminal.update(cx, |terminal, cx| feed_bytes(terminal, lines, seq, cx));
}

/// 灌入 `lines` 行确定性的 ASCII 文本（行内容随 `seq` 变化，迫使真实重排版）。
fn feed_bytes(terminal: &mut Terminal, lines: usize, seq: usize, cx: &mut gpui::Context<Terminal>) {
    let mut bytes = Vec::with_capacity(lines * 96);
    for i in 0..lines {
        let text = format!("s{i}q{seq} The quick brown fox jumps over the lazy dog 0123456789\r\n");
        bytes.extend_from_slice(text.as_bytes());
    }
    terminal.handle_remote_data(&bytes, cx);
}

/// 全行脏帧：每次迭代灌入整屏新输出（典型场景：`cat` 大文件、批量日志）。
///
/// 预灌 4 屏后进入稳态：视口已满，每迭代 40 行导致滚动 + 全行重排版。
#[gpui::bench]
fn frame_all_rows_dirty(cx: &mut BenchAppContext) {
    init_bench_context(cx);
    let view = make_terminal_view(cx);
    cx.update(|cx| feed_lines(&view, 160, 0, cx));

    let mut seq = 1usize;
    cx.bench_renderer(view, move |view, _window, cx| {
        view.terminal().update(cx, |terminal, cx| {
            feed_bytes(terminal, 40, seq, cx);
        });
        seq += 1;
    });
}

/// 单行脏帧：模拟 shell 回显 / 稳态打字（只重排光标所在行）。
///
/// 每次迭代写入 `X` 后左移光标复位，下一迭代覆盖同一单元格。
#[gpui::bench]
fn frame_single_row_dirty(cx: &mut BenchAppContext) {
    init_bench_context(cx);
    let view = make_terminal_view(cx);
    cx.update(|cx| feed_lines(&view, 160, 0, cx));

    cx.bench_renderer(view, move |view, _window, cx| {
        view.terminal().update(cx, |terminal, cx| {
            terminal.handle_remote_data(b"X\x1b[1D", cx);
        });
    });
}

/// 无 damage 帧：光标闪烁 / 闲置重绘（行缓存全命中，不应有排版工作）。
#[gpui::bench]
fn frame_no_damage(cx: &mut BenchAppContext) {
    init_bench_context(cx);
    let view = make_terminal_view(cx);
    cx.update(|cx| feed_lines(&view, 160, 0, cx));

    cx.bench_renderer(view, move |_view, _window, cx| {
        cx.notify();
    });
}

/// 滚动一帧：display_offset 上/下交替移动一行（全视口内容位移重绘）。
#[gpui::bench]
fn frame_scroll_one_line(cx: &mut BenchAppContext) {
    init_bench_context(cx);
    let view = make_terminal_view(cx);
    cx.update(|cx| feed_lines(&view, 160, 0, cx));

    let mut scroll_up = true;
    cx.bench_renderer(view, move |view, _window, cx| {
        view.terminal().update(cx, |terminal, _cx| {
            if scroll_up {
                terminal.scroll_line_up();
            } else {
                terminal.scroll_line_down();
            }
        });
        scroll_up = !scroll_up;
        cx.notify();
    });
}

/// 拖拽选区一帧：按住左键每次移动一个单元格（选区变化 → 行缓存选择失效）。
///
/// 等 prepaint 完成首次 resize（terminal_bounds 有效）后按下鼠标，
/// 然后在行 2 上左右交替拖动。
#[gpui::bench]
fn frame_selection_drag(cx: &mut BenchAppContext) {
    init_bench_context(cx);
    let view = make_terminal_view(cx);
    cx.update(|cx| feed_lines(&view, 160, 0, cx));

    let mut drag_col: usize = 2;
    let mut started = false;
    let mut going_right = true;
    cx.bench_renderer(view, move |view, _window, cx| {
        view.terminal().update(cx, |terminal, cx| {
            let bounds = terminal.terminal_bounds;
            if bounds.num_columns() < 10 {
                // 首帧 prepaint 尚未设置 bounds，跳过（后续迭代进入稳态）。
                return;
            }
            let origin = bounds.bounds.origin;
            let cell_w = bounds.cell_width;
            let cell_h = bounds.line_height;
            if !started {
                started = true;
                let pos = origin + gpui::point(cell_w * 2.0, cell_h * 2.0);
                terminal.mouse_down(
                    &MouseDownEvent {
                        position: pos,
                        button: MouseButton::Left,
                        ..MouseDownEvent::default()
                    },
                    cx,
                );
                drag_col = 2;
            }

            // 边界内左右交替拖动，避免无限延伸选区。
            if going_right {
                drag_col += 1;
                if drag_col >= bounds.num_columns() - 2 {
                    going_right = false;
                }
            } else {
                drag_col -= 1;
                if drag_col <= 2 {
                    going_right = true;
                }
            }
            let pos = origin + Point::new(cell_w * drag_col as f32, cell_h * 2.0);
            terminal.mouse_drag(
                &MouseMoveEvent {
                    position: pos,
                    pressed_button: Some(MouseButton::Left),
                    ..MouseMoveEvent::default()
                },
                cx,
            );
        });
        cx.notify();
    });
}

gpui::bench_group!(
    benches,
    frame_all_rows_dirty,
    frame_single_row_dirty,
    frame_no_damage,
    frame_scroll_one_line,
    frame_selection_drag
);
gpui::bench_main!(benches);

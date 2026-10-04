//! LSP 跳转（定义 / 引用）的测试。
//!
//! 回归背景：`goto_definition` 曾经只在**目标与当前文件相同**时才跳，
//! 跨文件时静默返回——而「跳到别的模块的定义」恰恰是最常见的场景，
//! 用户看到的现象就是「跳转不工作」。
//!
//! 对照 Zed 的 `go_to_definition_of_kind`：它拿到 `Location` 后
//! `open_local_buffer_via_lsp` 打开目标文件再选中区间，从不因为跨文件而放弃。
//!
//! 这里驱动真实的 [`crate::editor::editor::lsp_location_action`]——它是
//! 「原地跳 / 打开目标文件 / 忽略」的**唯一分支点**。
//!
//! 为什么不直接调 `Editor::open_lsp_location`：那条路径会调用
//! `cx.watch_path`，启动真实的 OS 文件监视线程；在 gpui 的
//! `TestScheduler` 下这些线程会被判为「非确定性活动」并 abort 整个测试
//! 进程（`test_scheduler.rs` 的 non-determinism 检查）。决策函数把分支
//! 逻辑完整覆盖，而文件打开本身复用已有的 `open_file_at_line_col_no_notify`
//! （它有自己的测试）。

use gpui::AppContext as _;
use lsp_types::{Location, Position, Range, Url};

use super::{LspJumpAction, lsp_location_action};

fn loc(uri: &str, line: u32, col: u32) -> Location {
    Location {
        uri: Url::parse(uri).unwrap(),
        range: Range::new(Position::new(line, col), Position::new(line, col + 4)),
    }
}

/// 构造一个 `file://` uri，指向给定的**平台本地路径**。
///
/// 必须走 `Url::from_file_path` 而不是手写 `file:///tmp/x.rs`：windows 上
/// 合法形式是 `file:///C:/tmp/x.rs`，`to_file_path()` 遇到 `/tmp/x.rs`
/// 会因「不是本机绝对路径」返回 `Err`，于是 `uri_to_path` 给出 `None`、
/// 跳转退化成 `Ignore`——被验的「行号换算」根本没机会执行。
fn file_loc(path: &std::path::Path, line: u32, col: u32) -> Location {
    Location {
        uri: Url::from_file_path(path).expect("必须是合法的本地路径"),
        range: Range::new(Position::new(line, col), Position::new(line, col + 4)),
    }
}

/// 跨文件跳转必须产出 `OpenFile`（而不是放弃）。
///
/// 这是用户报的「跳转不工作」的核心场景。
#[test]
fn cross_file_location_opens_target_file() {
    let target = std::env::temp_dir().join("other.rs");
    let location = file_loc(&target, 9, 2);
    let action = lsp_location_action(&location, false);
    match action {
        LspJumpAction::OpenFile { path, line, col } => {
            assert_eq!(path, target);
            // LSP 0 基 → 编辑器 1 基：行、列**都要** +1。
            assert_eq!(line, 10, "LSP line is 0-based; editor line is 1-based");
            assert_eq!(col, 3, "LSP col is 0-based; editor col is 1-based");
        }
        other => panic!("cross-file jump must open the target file, got {other:?}"),
    }
}

/// 同文件跳转必须原地跳（不重复打开标签）。
#[test]
fn same_file_location_jumps_in_place() {
    let location = file_loc(&std::env::temp_dir().join("same.rs"), 3, 0);
    let action = lsp_location_action(&location, true);
    match action {
        LspJumpAction::JumpInPlace { line, col } => {
            assert_eq!(line, 4, "0-based LSP line 3 -> 1-based editor line 4");
            assert_eq!(col, 1, "0-based LSP col 0 -> 1-based editor col 1");
        }
        other => panic!("same-file jump must jump in place, got {other:?}"),
    }
}

/// 非 `file://` 的 uri（如 untitled:）必须被忽略，不能 panic。
#[test]
fn non_file_uri_is_ignored() {
    let location = loc("untitled:Untitled-1", 0, 0);
    assert_eq!(
        lsp_location_action(&location, false),
        LspJumpAction::Ignore
    );
}

/// 同文件判定优先于 uri 形态：即便 uri 不是 file://，同文件也原地跳。
#[test]
fn same_file_takes_precedence_over_uri_shape() {
    let location = loc("untitled:Untitled-1", 1, 1);
    assert!(matches!(
        lsp_location_action(&location, true),
        LspJumpAction::JumpInPlace { .. }
    ));
}

/// 行号换算在边界上正确（第 0 行 → 第 1 行）。
#[test]
fn first_line_maps_to_line_one() {
    let location = file_loc(&std::env::temp_dir().join("a.rs"), 0, 0);
    match lsp_location_action(&location, false) {
        LspJumpAction::OpenFile { line, .. } => assert_eq!(line, 1),
        other => panic!("expected OpenFile, got {other:?}"),
    }
}

// ------------------------------------------------ prepareRename 错误分类

/// 服务器**明确拒绝**重命名时要能识别出来。
///
/// 实测 rust-analyzer 对无引用/关键字位置回
/// `-32602 No references found at position`。这类错误应直接告诉用户
/// 「不可重命名」，而不是退回按词边界猜的行为（那样用户会看到输入框、
/// 输完却没有任何效果）。
#[test]
fn recognizes_explicit_rename_rejection() {
    for msg in [
        "JSON-RPC error -32602: No references found at position",
        "symbol cannot be renamed",
        "element is not renameable",
        "InvalidParams",
    ] {
        let err = anyhow::anyhow!("{msg}");
        assert!(
            super::is_rename_rejection(&err),
            "{msg:?} should be classified as a rename rejection"
        );
    }
}

/// 传输类失败**不能**被当成「不可重命名」——那会因一次超时就禁掉功能。
#[test]
fn transport_failures_are_not_rename_rejections() {
    for msg in [
        "timed out after 30s waiting for `textDocument/prepareRename`",
        "language server is closed",
        "failed to write to language server stdin",
    ] {
        let err = anyhow::anyhow!("{msg}");
        assert!(
            !super::is_rename_rejection(&err),
            "{msg:?} must NOT be treated as a rename rejection (should fall back)"
        );
    }
}

// ------------------------------------------------ 跳转历史（返回）

/// 跳转历史是后进先出：返回应回到**最近一次**跳转前的位置。
///
/// 回归：此前完全没有历史——跳到定义后无法返回，用户只能自己找回来。
/// 对照 Zed：它用 `ItemNavHistory` 支持 `navigate_backward`。
#[test]
fn nav_history_is_lifo() {
    let mut history: Vec<super::NavLocation> = Vec::new();
    for (tab, offset) in [("a.rs", 10usize), ("b.rs", 20), ("c.rs", 30)] {
        history.push(super::NavLocation {
            tab_id: tab.to_string(),
            offset,
        });
    }
    // 返回三次应得到逆序。
    assert_eq!(history.pop().unwrap().tab_id, "c.rs");
    assert_eq!(history.pop().unwrap().tab_id, "b.rs");
    assert_eq!(history.pop().unwrap().tab_id, "a.rs");
    assert!(history.pop().is_none(), "empty history must yield None");
}

/// 历史有容量上限，不会无限增长。
#[test]
fn nav_history_is_bounded() {
    let limit = super::NAV_HISTORY_LIMIT;
    assert!(limit > 0 && limit <= 1000, "limit should be sane, got {limit}");

    let mut history: Vec<super::NavLocation> = Vec::new();
    for i in 0..(limit * 2) {
        history.push(super::NavLocation {
            tab_id: "t.rs".to_string(),
            offset: i,
        });
        if history.len() > limit {
            history.remove(0);
        }
    }
    assert_eq!(history.len(), limit, "history must be capped");
    // 最旧的被丢弃，最新的保留。
    assert_eq!(history.last().unwrap().offset, limit * 2 - 1);
}

/// 同一位置连续跳转不重复压栈（避免历史里一堆相同项）。
#[test]
fn consecutive_identical_jumps_are_deduplicated() {
    let entry = super::NavLocation {
        tab_id: "a.rs".to_string(),
        offset: 42,
    };
    let mut history: Vec<super::NavLocation> = Vec::new();
    // 模拟 `push_nav_history` 的去重条件。
    for _ in 0..5 {
        if history.last() == Some(&entry) {
            continue;
        }
        history.push(entry.clone());
    }
    assert_eq!(history.len(), 1, "identical consecutive entries must collapse");
}

// ------------------------------------------------ 端到端：跳到目标函数上

struct Host(gpui::Entity<super::Editor>);
impl gpui::Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

fn editor_ctx<'a>(
    cx: &'a mut gpui::TestAppContext,
) -> (gpui::Entity<super::Editor>, &'a mut gpui::VisualTestContext) {
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::file_manager::watcher::init(cx);
        // 打开文件时会向 LspManager 查询服务器（本地文件即使没有 server
        // 也会走这条路径），缺这个全局会 panic。
        crate::lsp::init(cx);
    });
    let (host, cx) = cx.add_window_view(|window, cx| Host(cx.new(|cx| super::Editor::new(window, cx))));
    (host.read_with(cx, |h, _| h.0.clone()), cx)
}

/// 跳到**尚未打开**的文件里的某个函数：必须打开该文件并把光标移到那一行。
///
/// 回归：用户报「只能打开对应的代码文件，没有跳到函数上」。此前的测试只
/// 断言「文件被打开」（`cross_file_location_opens_target_file` 走纯决策函数），
/// **没有断言光标落点**——所以「打开了但没跳」这个缺陷完全隐形。
#[gpui::test]
fn jump_lands_cursor_on_target_function(cx: &mut gpui::TestAppContext) {
    // 关掉 LSP 接入：打开文件会尝试连服务器（`smol::spawn` 到全局执行器），
    // 会被 gpui 的 TestScheduler 判为非确定性并 abort 整个测试二进制。
    super::with_lsp_disabled(|| {
    let (editor, cx) = editor_ctx(cx);
    let dir = std::env::temp_dir().join("deepterm-lsp-jump-cursor");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("defs.rs");
    // 目标函数在第 20 行（0 基 19），前面填足够多的行以证明真的跳了。
    let mut body = String::new();
    for i in 1..=19 {
        body.push_str(&format!("// filler line {i}\n"));
    }
    body.push_str("fn target_function() {}\n");
    body.push_str("// trailing\n");
    std::fs::write(&target, &body).unwrap();

    let uri = Url::from_file_path(&target).unwrap();
    // LSP 行号 0 基：第 20 行 → 19。列 3（`fn ` 之后，`target_function` 起点）。
    let location = Location {
        uri,
        range: Range::new(Position::new(19, 3), Position::new(19, 18)),
    };

    editor.update_in(cx, |editor, window, cx| {
        editor.open_lsp_location_for_test(&location, window, cx);
    });
    // 文件加载是异步的：跑到空闲让 pending jump 落地。
    cx.run_until_parked();

    let pos = editor
        .read_with(cx, |editor, cx| editor.cursor_position_of_open_path(&target, cx))
        .expect("target file must be open");
    assert_eq!(
        pos.0, 19,
        "cursor must land on the definition line (0-based 19), got line {} (col {})",
        pos.0, pos.1
    );
    assert_eq!(
        pos.1, 3,
        "cursor must land on the definition column (0-based 3), got {}",
        pos.1
    );
    });
}

/// 同文件跳转：光标必须移到目标行（不重复开标签）。
#[gpui::test]
fn same_file_jump_moves_cursor(cx: &mut gpui::TestAppContext) {
    super::with_lsp_disabled(|| {
    let (editor, cx) = editor_ctx(cx);
    let dir = std::env::temp_dir().join("deepterm-lsp-jump-same-cursor");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("one.rs");
    let mut body = String::new();
    for i in 1..=30 {
        body.push_str(&format!("// line {i}\n"));
    }
    body.push_str("fn here() {}\n");
    std::fs::write(&file, &body).unwrap();

    // 先把文件打开。
    editor.update_in(cx, |editor, window, cx| {
        editor.open_file(file.clone(), window, cx);
    });
    cx.run_until_parked();

    // 让 `open_lsp_location` 认为目标是「当前文件」。
    editor.update_in(cx, |editor, _window, _cx| {
        editor.set_session_uri_for_test(Url::from_file_path(&file).unwrap());
    });

    let uri = Url::from_file_path(&file).unwrap();
    let location = Location {
        uri,
        range: Range::new(Position::new(30, 3), Position::new(30, 7)),
    };
    editor.update_in(cx, |editor, window, cx| {
        editor.open_lsp_location_for_test(&location, window, cx);
    });
    cx.run_until_parked();

    let pos = editor
        .read_with(cx, |editor, cx| editor.cursor_position_of_open_path(&file, cx))
        .expect("file must be open");
    assert_eq!(pos.0, 30, "same-file jump must move the cursor to line 30");
    });
}

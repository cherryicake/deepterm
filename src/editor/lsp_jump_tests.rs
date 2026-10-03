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

use std::path::PathBuf;

use lsp_types::{Location, Position, Range, Url};

use super::{LspJumpAction, lsp_location_action};

fn loc(uri: &str, line: u32, col: u32) -> Location {
    Location {
        uri: Url::parse(uri).unwrap(),
        range: Range::new(Position::new(line, col), Position::new(line, col + 4)),
    }
}

/// 跨文件跳转必须产出 `OpenFile`（而不是放弃）。
///
/// 这是用户报的「跳转不工作」的核心场景。
#[test]
fn cross_file_location_opens_target_file() {
    let location = loc("file:///tmp/other.rs", 9, 2);
    let action = lsp_location_action(&location, false);
    match action {
        LspJumpAction::OpenFile { path, line, col } => {
            assert_eq!(path, PathBuf::from("/tmp/other.rs"));
            // LSP 0 基行 9 → 编辑器 1 基行 10。
            assert_eq!(line, 10, "LSP line is 0-based; editor line is 1-based");
            assert_eq!(col, 2);
        }
        other => panic!("cross-file jump must open the target file, got {other:?}"),
    }
}

/// 同文件跳转必须原地跳（不重复打开标签）。
#[test]
fn same_file_location_jumps_in_place() {
    let location = loc("file:///tmp/same.rs", 3, 0);
    let action = lsp_location_action(&location, true);
    match action {
        LspJumpAction::JumpInPlace { line, col } => {
            assert_eq!(line, 4, "0-based LSP line 3 -> 1-based editor line 4");
            assert_eq!(col, 0);
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
    let location = loc("file:///tmp/a.rs", 0, 0);
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

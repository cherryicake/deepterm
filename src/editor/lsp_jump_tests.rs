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

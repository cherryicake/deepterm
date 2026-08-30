//! Git 行变更标记：把 HEAD→工作区 diff 转成每个新行的变更类型。
//!
//! 数据流：Editor 侧在后台线程调用 [`compute_git_line_marks`]，把结果写入
//! `InputState::set_git_line_marks`，由编辑器渲染层画行号区色条。

use crate::components::input::GitLineMark;
use crate::editor::diff_model::line_diff;

/// 新行号（0 起）上限保护：超大文件放弃标记。
const MAX_NEW_LINES: usize = 200_000;

/// 由 old(HEAD) 与 new(工作区) 文本计算每个新行的变更标记。
///
/// - hunk 只有新行 → `Added`
/// - hunk 新旧行都有 → 新行标 `Modified`
/// - hunk 只有旧行（删除）→ 在新文本中紧随其后的行（或末行）标 `Deleted`，
///   渲染层画一个红色小三角/短条表示“此处上方有删除”。
pub fn compute_git_line_marks(old_text: &str, new_text: &str) -> Vec<(u32, GitLineMark)> {
    let new_line_count = new_text.lines().count();
    if new_line_count > MAX_NEW_LINES {
        return Vec::new();
    }
    // 用行数而不是字符数粗略对齐：imara 的行号是“带终止符的行”编号。
    // 空文本没有行；行数按 lines() 计（不含末尾空行）。
    let mut marks: Vec<(u32, GitLineMark)> = Vec::new();

    for (old_rows, new_rows) in line_diff(old_text, new_text) {
        let old_count = old_rows.end - old_rows.start;
        let new_count = new_rows.end - new_rows.start;
        if new_count == 0 {
            // 纯删除：挂在 hunk 之后的第一行（行号 = new_rows.start）。
            // new_rows 为空区间时 start 即插入点之后的新行号。
            let anchor = new_rows.start;
            marks.push((anchor, GitLineMark::Deleted));
            continue;
        }
        let kind = if old_count == 0 {
            GitLineMark::Added
        } else {
            GitLineMark::Modified
        };
        for line in new_rows.start..new_rows.end {
            marks.push((line, kind));
        }
    }

    // 同一行可能拿到多个标记：纯删除 hunk 的锚点挂在「其后第一行」，当它后面
    // 紧跟着一个修改 hunk 时，两者会落在同一个新行号上。
    // 按 (行号, 优先级) 排序，让 Added/Modified 排在同组最前 —— `dedup_by` 只
    // 保留每组第一个，优先级高的才会胜出。
    // （此前仅按行号做稳定排序，而 Deleted 是先 push 的，反而把 Added/Modified
    // 挤掉了，与注释声明的优先级相反。）
    marks.sort_by_key(|(line, mark)| (*line, mark_priority(*mark)));
    marks.dedup_by(|a, b| a.0 == b.0);
    marks
}

/// 去重优先级，数值越小越优先保留。
///
/// `Deleted` 的三角只是「此处上方有删除」的提示，应让位于 `Added` / `Modified`
/// 这类描述当前行自身状态的标记。
fn mark_priority(mark: GitLineMark) -> u8 {
    match mark {
        GitLineMark::Deleted => 1,
        GitLineMark::Added | GitLineMark::Modified => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_for_simple_edit() {
        // 中间行被修改
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let marks = compute_git_line_marks(old, new);
        assert_eq!(marks, vec![(1, GitLineMark::Modified)]);
    }

    #[test]
    fn marks_for_added_and_deleted() {
        // 顶部插入两行 → 0,1 Added；中间删一行 → 后续行 Deleted
        let old = "a\nb\nc\n";
        let new = "x\ny\na\nc\n";
        let marks = compute_git_line_marks(old, new);
        assert!(marks.contains(&(0, GitLineMark::Added)));
        assert!(marks.contains(&(1, GitLineMark::Added)));
        // b 被删：锚到其后第一行（新文本行 3，即 c）
        assert!(marks.contains(&(3, GitLineMark::Deleted)));
        // a/c 应无标记
        assert!(!marks.iter().any(|(l, _)| *l == 2));
    }

    #[test]
    fn marks_empty_and_identical() {
        assert!(compute_git_line_marks("", "").is_empty());
        assert!(compute_git_line_marks("a\n", "a\n").is_empty());
    }

    #[test]
    fn marks_new_file_all_added() {
        let marks = compute_git_line_marks("", "a\nb\n");
        assert_eq!(
            marks,
            vec![(0, GitLineMark::Added), (1, GitLineMark::Added)]
        );
    }

    #[test]
    fn marks_deleted_at_end_anchors_last_line() {
        let old = "a\nb\n";
        let new = "a\n";
        let marks = compute_git_line_marks(old, new);
        // b 删除后 new_rows 空区间 start = 1（超出最后行 0）→ 锚点 1 不存在，
        // 渲染层按“超出末尾”处理为不显示（或画在最后一行下方）。
        assert_eq!(marks, vec![(1, GitLineMark::Deleted)]);
    }
}

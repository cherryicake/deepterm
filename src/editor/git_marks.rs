//! Git 行变更标记：把 HEAD→工作区 diff 转成每个新行的变更类型与 hunk 列表。
//!
//! 数据流：Editor 侧在后台线程调用 [`compute_git_diff_summary`]，把结果写入
//! `InputState::set_git_diff`：行标记由编辑器渲染层画行首色条；hunk 列表
//! 支撑行首色条点击就地展开（红色删除行 + 绿色新增行底色）。

use crate::components::input::{GitDeletedLine, GitDiffSummary, GitHunkInfo, GitLineMark};
use crate::editor::diff_model::line_diff;

/// 新行号（0 起）上限保护：超大文件放弃标记。
const MAX_NEW_LINES: usize = 200_000;

/// 由 old(HEAD) 与 new(工作区) 文本计算行标记与 hunk 列表。
///
/// 行标记规则：
/// - hunk 只有新行 → `Added`
/// - hunk 新旧行都有 → 新行标 `Modified`
/// - hunk 只有旧行（删除）→ 在新文本中紧随其后的行（或末行）标 `Deleted`，
///   渲染层画一个红色短条表示“此处上方有删除”。
///
/// hunk 列表与行标记同源：`before_line` 是展开区的插入锚点，`deleted`
/// 携带被删行的旧行号与文本（1 起行号，对齐 git diff 旧侧）。
pub fn compute_git_diff_summary(old_text: &str, new_text: &str) -> GitDiffSummary {
    let new_line_count = new_text.lines().count();
    if new_line_count > MAX_NEW_LINES {
        return GitDiffSummary::default();
    }
    let mut marks: Vec<(u32, GitLineMark)> = Vec::new();
    let mut hunks: Vec<GitHunkInfo> = Vec::new();

    // 被删行文本按旧行号取自 old_text；hunk 按旧侧行序升序产出，
    // 一次性物化行切片避免逐 hunk `lines().nth()` 的 O(n²)。
    let old_lines: Vec<&str> = old_text.lines().collect();

    for (old_rows, new_rows) in line_diff(old_text, new_text) {
        let old_count = old_rows.end - old_rows.start;
        let new_count = new_rows.end - new_rows.start;
        if new_count == 0 {
            // 纯删除：锚点挂在 hunk 之后的第一行（行号 = new_rows.start）。
            // new_rows 为空区间时 start 即插入点之后的新行号；若指向末尾
            // 之外（尾部删除）则钳到末行，保证渲染层总能画出（此前越界行号
            // 永不可见，尾删无任何提示）。空文件无行可挂，直接丢弃。
            if new_line_count == 0 {
                continue;
            }
            let last = (new_line_count - 1) as u32;
            let anchor = new_rows.start.min(last);
            marks.push((anchor, GitLineMark::Deleted));
            hunks.push(GitHunkInfo {
                before_line: anchor,
                new_rows: new_rows.clone(),
                deleted: deleted_lines(&old_lines, old_rows.clone()),
            });
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
        hunks.push(GitHunkInfo {
            before_line: new_rows.start,
            new_rows: new_rows.clone(),
            deleted: deleted_lines(&old_lines, old_rows.clone()),
        });
    }

    // 同一行可能拿到多个标记：纯删除 hunk 的锚点挂在「其后第一行」，当它后面
    // 紧跟着一个修改 hunk 时，两者会落在同一个新行号上。
    // 按 (行号, 优先级) 排序，让 Added/Modified 排在同组最前 —— `dedup_by` 只
    // 保留每组第一个，优先级高的才会胜出。
    // （此前仅按行号做稳定排序，而 Deleted 是先 push 的，反而把 Added/Modified
    // 挤掉了，与注释声明的优先级相反。）
    marks.sort_by_key(|(line, mark)| (*line, mark_priority(*mark)));
    marks.dedup_by(|a, b| a.0 == b.0);

    GitDiffSummary { marks, hunks }
}

/// 收集 `old_rows`（0 起行区间）对应的被删行，行号 1 起。
fn deleted_lines(old_lines: &[&str], old_rows: std::ops::Range<u32>) -> Vec<GitDeletedLine> {
    old_rows
        .map(|row| {
            let text = old_lines
                .get(row as usize)
                .copied()
                .unwrap_or_default()
                .to_string();
            GitDeletedLine {
                old_lineno: row + 1,
                text,
            }
        })
        .collect()
}

/// 去重优先级，数值越小越优先保留。
///
/// `Deleted` 的短条只是「此处上方有删除」的提示，应让位于 `Added` / `Modified`
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
        let summary = compute_git_diff_summary(old, new);
        assert_eq!(summary.marks, vec![(1, GitLineMark::Modified)]);
        // 可展开：被删旧行 b（旧行号 2）
        assert_eq!(summary.hunks.len(), 1);
        let hunk = &summary.hunks[0];
        assert_eq!(hunk.before_line, 1);
        assert_eq!(hunk.new_rows, 1..2);
        assert_eq!(hunk.deleted.len(), 1);
        assert_eq!(hunk.deleted[0].old_lineno, 2);
        assert_eq!(hunk.deleted[0].text, "b");
    }

    #[test]
    fn marks_for_added_and_deleted() {
        // 顶部插入两行 → 0,1 Added；中间删一行 → 后续行 Deleted
        let old = "a\nb\nc\n";
        let new = "x\ny\na\nc\n";
        let summary = compute_git_diff_summary(old, new);
        assert!(summary.marks.contains(&(0, GitLineMark::Added)));
        assert!(summary.marks.contains(&(1, GitLineMark::Added)));
        // b 被删：锚到其后第一行（新文本行 3，即 c）
        assert!(summary.marks.contains(&(3, GitLineMark::Deleted)));
        // a/c 应无标记
        assert!(!summary.marks.iter().any(|(l, _)| *l == 2));

        // 纯删除 hunk 可展开，锚点行 3，旧行号 2
        let deleted_hunk = summary
            .hunks
            .iter()
            .find(|h| h.new_rows.is_empty())
            .expect("应有纯删除 hunk");
        assert_eq!(deleted_hunk.before_line, 3);
        assert_eq!(deleted_hunk.deleted[0].old_lineno, 2);
        assert_eq!(deleted_hunk.deleted[0].text, "b");
    }

    #[test]
    fn marks_empty_and_identical() {
        assert!(compute_git_diff_summary("", "").marks.is_empty());
        assert!(compute_git_diff_summary("a\n", "a\n").marks.is_empty());
        assert!(compute_git_diff_summary("a\n", "a\n").hunks.is_empty());
    }

    #[test]
    fn marks_new_file_all_added() {
        let summary = compute_git_diff_summary("", "a\nb\n");
        assert_eq!(
            summary.marks,
            vec![(0, GitLineMark::Added), (1, GitLineMark::Added)]
        );
        // 纯新增 hunk：无可展开内容
        assert_eq!(summary.hunks.len(), 1);
        assert!(summary.hunks[0].deleted.is_empty());
        assert!(!summary.hunks[0].expandable());
    }

    #[test]
    fn marks_deleted_at_end_anchors_last_line() {
        let old = "a\nb\n";
        let new = "a\n";
        let summary = compute_git_diff_summary(old, new);
        // b 删除后 new_rows 空区间 start = 1（超出最后行 0）→ 钳到末行 0，
        // 保证尾删可见（此前锚点 1 永不可见）。
        assert_eq!(summary.marks, vec![(0, GitLineMark::Deleted)]);
        assert_eq!(summary.hunks[0].before_line, 0);
        assert_eq!(summary.hunks[0].deleted[0].text, "b");
    }

    /// 相邻的「修改 + 删除」会被 line_diff 合并成一个 hunk：
    /// 新行标 Modified，被删的多行旧文本一并保留（一次展开全可见）。
    #[test]
    fn adjacent_modify_and_delete_merge_into_one_hunk() {
        // old: a b c；new: a B —— b 修改 + c 删除合并为单个 hunk。
        let old = "a\nb\nc\n";
        let new = "a\nB\n";
        let summary = compute_git_diff_summary(old, new);
        assert_eq!(summary.marks, vec![(1, GitLineMark::Modified)]);
        assert_eq!(summary.hunks.len(), 1);
        let hunk = &summary.hunks[0];
        assert_eq!(hunk.before_line, 1);
        assert_eq!(hunk.new_rows, 1..2);
        assert_eq!(
            hunk.deleted,
            vec![
                GitDeletedLine {
                    old_lineno: 2,
                    text: "b".into()
                },
                GitDeletedLine {
                    old_lineno: 3,
                    text: "c".into()
                },
            ]
        );
    }
}

use std::sync::Arc;

/// Git status code for index/worktree
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusCode {
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmodified,
}

impl StatusCode {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'M' => Some(StatusCode::Modified),
            b'T' => Some(StatusCode::TypeChanged),
            b'A' => Some(StatusCode::Added),
            b'D' => Some(StatusCode::Deleted),
            b'R' => Some(StatusCode::Renamed),
            b'C' => Some(StatusCode::Copied),
            b' ' => Some(StatusCode::Unmodified),
            _ => None,
        }
    }
}

/// Tracked file status (index + worktree)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrackedStatus {
    pub index_status: StatusCode,
    pub worktree_status: StatusCode,
}

/// Unmerged (conflict) status
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnmergedStatus {
    pub first_head: UnmergedStatusCode,
    pub second_head: UnmergedStatusCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnmergedStatusCode {
    Added,
    Deleted,
    Updated,
}

impl UnmergedStatusCode {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'A' => Some(UnmergedStatusCode::Added),
            b'D' => Some(UnmergedStatusCode::Deleted),
            b'U' => Some(UnmergedStatusCode::Updated),
            _ => None,
        }
    }
}

/// Complete file status
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Untracked,
    Ignored,
    Unmerged(UnmergedStatus),
    Tracked(TrackedStatus),
}

impl FileStatus {
    fn from_bytes(bytes: [u8; 2]) -> Option<Self> {
        let status = match bytes {
            [b'?', b'?'] => FileStatus::Untracked,
            [b'!', b'!'] => FileStatus::Ignored,
            [b'A', b'A'] => UnmergedStatus {
                first_head: UnmergedStatusCode::Added,
                second_head: UnmergedStatusCode::Added,
            }
            .into(),
            [b'D', b'D'] => UnmergedStatus {
                first_head: UnmergedStatusCode::Deleted,
                second_head: UnmergedStatusCode::Deleted,
            }
            .into(),
            [x, b'U'] => UnmergedStatus {
                first_head: UnmergedStatusCode::from_byte(x)?,
                second_head: UnmergedStatusCode::Updated,
            }
            .into(),
            [b'U', y] => UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::from_byte(y)?,
            }
            .into(),
            [x, y] => TrackedStatus {
                index_status: StatusCode::from_byte(x)?,
                worktree_status: StatusCode::from_byte(y)?,
            }
            .into(),
        };
        Some(status)
    }

    pub fn is_conflicted(&self) -> bool {
        matches!(self, FileStatus::Unmerged { .. })
    }

    pub fn is_created(&self) -> bool {
        match self {
            FileStatus::Tracked(tracked) => matches!(
                (tracked.index_status, tracked.worktree_status),
                (StatusCode::Added, _) | (_, StatusCode::Added)
            ),
            FileStatus::Untracked => true,
            _ => false,
        }
    }

    pub fn is_modified(&self) -> bool {
        match self {
            FileStatus::Tracked(tracked) => matches!(
                (tracked.index_status, tracked.worktree_status),
                (StatusCode::Modified, _) | (_, StatusCode::Modified)
            ),
            _ => false,
        }
    }

    pub fn is_untracked(&self) -> bool {
        matches!(self, FileStatus::Untracked)
    }
}

impl From<UnmergedStatus> for FileStatus {
    fn from(value: UnmergedStatus) -> Self {
        FileStatus::Unmerged(value)
    }
}

impl From<TrackedStatus> for FileStatus {
    fn from(value: TrackedStatus) -> Self {
        FileStatus::Tracked(value)
    }
}

/// Stage status derived from index/worktree combination
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StageStatus {
    Staged,
    Unstaged,
    PartiallyStaged,
}

impl StageStatus {
    pub fn has_staged(&self) -> bool {
        matches!(self, StageStatus::Staged | StageStatus::PartiallyStaged)
    }
}

impl FileStatus {
    pub fn staging(&self) -> StageStatus {
        match self {
            FileStatus::Untracked | FileStatus::Ignored | FileStatus::Unmerged { .. } => {
                StageStatus::Unstaged
            }
            FileStatus::Tracked(tracked) => match (tracked.index_status, tracked.worktree_status) {
                (StatusCode::Unmodified, _) => StageStatus::Unstaged,
                (_, StatusCode::Unmodified) => StageStatus::Staged,
                _ => StageStatus::PartiallyStaged,
            },
        }
    }

    /// 右侧单字母徽标（VSCode 风格）：M 修改 / A 新增 / D 删除 / U 冲突 / R 改名 / T 类型变更。
    pub fn badge_letter(&self) -> char {
        match self {
            FileStatus::Untracked => 'U',
            FileStatus::Ignored => 'I',
            FileStatus::Unmerged { .. } => 'C',
            FileStatus::Tracked(t) => {
                // 优先展示工作区状态（未暂存的更需要引起注意），全已暂存时展示暂存区状态。
                let code = if t.worktree_status != StatusCode::Unmodified {
                    t.worktree_status
                } else {
                    t.index_status
                };
                match code {
                    StatusCode::Modified => 'M',
                    StatusCode::Added => 'A',
                    StatusCode::Deleted => 'D',
                    StatusCode::Renamed => 'R',
                    StatusCode::Copied => 'C',
                    StatusCode::TypeChanged => 'T',
                    StatusCode::Unmodified => 'M',
                }
            }
        }
    }

    /// 该文件是否还有可暂存的工作区改动（含未跟踪文件）。
    pub fn has_unstaged_changes(&self) -> bool {
        match self {
            FileStatus::Untracked => true,
            FileStatus::Ignored | FileStatus::Unmerged { .. } => false,
            FileStatus::Tracked(t) => t.worktree_status != StatusCode::Unmodified,
        }
    }

    /// 该文件是否有已暂存的改动。
    pub fn has_staged_changes(&self) -> bool {
        match self {
            FileStatus::Tracked(t) => t.index_status != StatusCode::Unmodified,
            _ => false,
        }
    }
}

/// Section for grouping files in the git panel (VSCode 模型)。
/// 排序：合并冲突 < 已暂存 < 更改（未跟踪文件也归入更改，U 徽标区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Conflict,
    Staged,
    Changes,
}

impl Section {
    pub fn title(&self) -> &'static str {
        match self {
            Section::Conflict => "合并更改",
            Section::Staged => "已暂存的更改",
            Section::Changes => "更改",
        }
    }

    /// 分组排序键（VSCode 顺序）。
    pub fn order(&self) -> u8 {
        match self {
            Section::Conflict => 0,
            Section::Staged => 1,
            Section::Changes => 2,
        }
    }
}

/// A single file status entry
#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: String,
    pub status: FileStatus,
    pub section: Section,
}

/// Diff stat for a file (added/deleted lines)
#[derive(Clone, Copy, Debug, Default)]
pub struct DiffStat {
    pub added: u32,
    pub deleted: u32,
}

/// Parsed git status output
#[derive(Clone, Debug, Default)]
pub struct GitStatus {
    pub entries: Arc<[StatusEntry]>,
}

impl GitStatus {
    /// Parse `git status --porcelain=v1 -z` output
    pub fn parse(output: &str) -> Self {
        let mut entries: Vec<StatusEntry> = output
            .split('\0')
            .filter_map(|entry| {
                // 关键安全说明：`get(2..3)` 成功即保证 entry 至少 3 字节且切分点在字符边界，
                // 因此后续 `as_bytes()[0]/[1]` 与 `&entry[3..]` 不会越界或 panic；
                // 畸形行直接返回 None 跳过，保证解析永不崩溃。
                let sep = entry.get(2..3)?;
                if sep != " " {
                    return None;
                }
                let path = &entry[3..];
                let status_bytes: [u8; 2] = [entry.as_bytes()[0], entry.as_bytes()[1]];
                let status = FileStatus::from_bytes(status_bytes)?;
                // 以 `/` 结尾的条目是子模块/嵌套仓库：`?? sub/`（未跟踪）
                // 是现存 git 的形态，旧版 git 的跟踪子模块同样带后缀。
                // VSCode 会展示它们，这里剥掉后缀、路径与文件树/暂存操作
                // 保持一致。纯未跟踪目录不会以目录形态出现——本应用的
                // status 固定 `--untracked-files=all`，目录总是被展开。
                let path = path.strip_suffix('/').unwrap_or(path);
                let section = classify_section(&status);
                Some(StatusEntry {
                    path: path.to_string(),
                    status,
                    section,
                })
            })
            .collect();

        entries.sort_by(|a, b| a.path.cmp(&b.path));

        // Merge duplicate entries: D? + ?? -> DA (deleted in index, added in worktree)
        entries.dedup_by(|a, b| {
            if a.path != b.path {
                return false;
            }
            match (a.status, b.status) {
                (
                    FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Unmodified,
                    }),
                    FileStatus::Untracked,
                )
                | (
                    FileStatus::Untracked,
                    FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Unmodified,
                    }),
                ) => {
                    b.status = TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Added,
                    }
                    .into();
                    b.section = classify_section(&b.status);
                    true
                }
                _ => a.status == b.status,
            }
        });

        Self {
            entries: entries.into(),
        }
    }
}

fn classify_section(status: &FileStatus) -> Section {
    if status.is_conflicted() {
        Section::Conflict
    } else {
        // VSCode 模型：未跟踪文件同样归入“更改”（U 徽标区分），
        // 不再单独分组。
        match status.staging() {
            StageStatus::Staged => Section::Staged,
            // 部分暂存（MM/AM 等）同时有已暂存与未暂存改动：
            // 列表中只出现一次，归入“更改”，避免同一文件在两组重复出现
            // 导致暂存/取消暂存按钮语义混乱。
            StageStatus::Unstaged | StageStatus::PartiallyStaged => Section::Changes,
        }
    }
}

/// Parse `git diff --numstat` output
pub fn parse_numstat(output: &str) -> Vec<(String, DiffStat)> {
    let mut entries: Vec<(String, DiffStat)> = output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, '\t');
            let added_str = parts.next()?;
            let deleted_str = parts.next()?;
            let path = parts.next()?;
            // Skip binary files (shown as "-\t-")
            let added = added_str.parse::<u32>().ok()?;
            let deleted = deleted_str.parse::<u32>().ok()?;
            Some((path.to_string(), DiffStat { added, deleted }))
        })
        .collect();

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.0 == b.0);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_simple() {
        let input = "M  src/main.rs\0?? src/new_file.rs\0";
        let status = GitStatus::parse(input);
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].path, "src/main.rs");
        assert!(status.entries[0].status.is_modified());
        // "M " = 仅暂存区有改动 → 已暂存组
        assert_eq!(status.entries[0].section, Section::Staged);
        assert_eq!(status.entries[1].path, "src/new_file.rs");
        assert!(status.entries[1].status.is_untracked());
        // 未跟踪文件归入“更改”（VSCode 模型，U 徽标区分）。
        assert_eq!(status.entries[1].section, Section::Changes);
    }

    #[test]
    fn test_parse_status_sections() {
        //  M = 仅工作区修改 → 更改；MM = 部分暂存 → 更改；A  = 新增已暂存 → 已暂存
        let input = " M a.rs\0MM b.rs\0A  c.rs\0";
        let status = GitStatus::parse(input);
        assert_eq!(status.entries[0].section, Section::Changes);
        assert_eq!(status.entries[1].section, Section::Changes);
        assert_eq!(status.entries[2].section, Section::Staged);
        assert_eq!(status.entries[0].status.badge_letter(), 'M');
        assert_eq!(status.entries[2].status.badge_letter(), 'A');
        assert!(status.entries[0].status.has_unstaged_changes());
        assert!(status.entries[2].status.has_staged_changes());
    }

    #[test]
    fn test_parse_status_conflict() {
        let input = "UU conflict.txt\0";
        let status = GitStatus::parse(input);
        assert_eq!(status.entries.len(), 1);
        assert!(status.entries[0].status.is_conflicted());
        assert_eq!(status.entries[0].section, Section::Conflict);
    }

    /// 子模块/嵌套仓库条目以 `/` 结尾（`?? sub/` 未跟踪；旧版 git 的
    /// 跟踪子模块 `M sub/` 同形）——必须保留且剥掉后缀，否则面板看不到
    /// 新增子模块、无法暂存。
    #[test]
    fn test_parse_status_submodule_kept() {
        let input = "?? sub2/\0 M sub/\0M  .gitmodules\0";
        let status = GitStatus::parse(input);
        assert_eq!(status.entries.len(), 3);
        let sub2 = status.entries.iter().find(|e| e.path == "sub2").unwrap();
        assert!(sub2.status.is_untracked());
        assert_eq!(sub2.section, Section::Changes);
        let sub = status.entries.iter().find(|e| e.path == "sub").unwrap();
        assert!(sub.status.is_modified());
        // " M" = 仅工作区改动 → 更改组。
        assert_eq!(sub.section, Section::Changes);
        assert!(
            status.entries.iter().all(|e| !e.path.ends_with('/')),
            "路径后缀必须剥掉，与文件树/暂存操作的路径一致"
        );
    }

    #[test]
    fn test_parse_numstat() {
        let input = "10\t5\tsrc/main.rs\n3\t1\tREADME.md\n";
        let entries = parse_numstat(input);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "README.md");
        assert_eq!(entries[0].1.added, 3);
        assert_eq!(entries[0].1.deleted, 1);
    }
}

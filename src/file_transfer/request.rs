use crate::file_transfer::FileEndpoint;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferOperation {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPolicy {
    AutoRename,
    Overwrite,
    Skip,
    Ask,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransferRequest {
    pub sources: Vec<FileEndpoint>,
    pub destination: FileEndpoint,
    pub operation: TransferOperation,
    pub conflict_policy: ConflictPolicy,
    /// 单源文件场景下的目标文件名。`Some` 时将 `destination` 视为目录，
    /// 并用该名称覆盖源文件名；多源或源为目录时忽略。
    pub rename_to: Option<String>,
}

impl TransferRequest {
    /// 所有源文件是否已在目标位置（无需实际传输）
    pub fn is_noop(&self) -> bool {
        if self.sources.is_empty() {
            return true;
        }
        self.sources.iter().all(|source| {
            if !source.is_same_device(&self.destination) {
                return false;
            }
            // 单源 + rename_to：只有 rename_to 等于源文件名时才可能是 noop
            if self.sources.len() == 1 {
                if let Some(rename_to) = self.rename_to.as_deref() {
                    let src_name = match source {
                        FileEndpoint::Local { path } => {
                            path.file_name().map(|n| n.to_string_lossy().into_owned())
                        }
                        FileEndpoint::Remote { path, .. } => path
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .filter(|n| !n.is_empty())
                            .map(|n| n.to_string()),
                    };
                    if src_name.as_deref() != Some(rename_to) {
                        return false;
                    }
                }
            }
            match (source, &self.destination) {
                (FileEndpoint::Local { path: src }, FileEndpoint::Local { path: dst }) => {
                    src == dst || src.parent().is_some_and(|parent| parent == dst)
                }
                (
                    FileEndpoint::Remote { path: src, .. },
                    FileEndpoint::Remote { path: dst, .. },
                ) => {
                    let src = src.trim_end_matches('/');
                    let dst = dst.trim_end_matches('/');
                    if src == dst {
                        return true;
                    }
                    if let Some(pos) = src.rfind('/') {
                        let parent = if pos == 0 { "/" } else { &src[..pos] };
                        parent == dst
                    } else {
                        false
                    }
                }
                _ => false,
            }
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn local(path: &str) -> FileEndpoint {
        FileEndpoint::Local {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn is_noop_when_source_sits_in_destination_without_rename() {
        let req = TransferRequest {
            sources: vec![local("/tmp/a.txt")],
            destination: local("/tmp"),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: None,
        };
        assert!(req.is_noop());
    }

    #[test]
    fn is_noop_when_rename_matches_source_name() {
        let req = TransferRequest {
            sources: vec![local("/tmp/a.txt")],
            destination: local("/tmp"),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: Some("a.txt".to_string()),
        };
        assert!(req.is_noop());
    }

    #[test]
    fn is_not_noop_when_rename_differs_from_source() {
        let req = TransferRequest {
            sources: vec![local("/tmp/a.txt")],
            destination: local("/tmp"),
            operation: TransferOperation::Copy,
            conflict_policy: ConflictPolicy::Ask,
            rename_to: Some("b.txt".to_string()),
        };
        assert!(!req.is_noop());
    }
}

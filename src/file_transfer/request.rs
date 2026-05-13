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
pub struct FileDragPayload {
    pub sources: Vec<FileEndpoint>,
    pub operation_hint: TransferOperation,
}

impl FileDragPayload {
    pub fn copy_one(source: FileEndpoint) -> Self {
        Self {
            sources: vec![source],
            operation_hint: TransferOperation::Copy,
        }
    }

    pub fn move_one(source: FileEndpoint) -> Self {
        Self {
            sources: vec![source],
            operation_hint: TransferOperation::Move,
        }
    }

    pub fn preview_label(&self) -> String {
        match self.sources.as_slice() {
            [] => "No files".to_string(),
            [source] => source.display_name(),
            sources => format!("{} items", sources.len()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransferRequest {
    pub sources: Vec<FileEndpoint>,
    pub destination: FileEndpoint,
    pub operation: TransferOperation,
    pub conflict_policy: ConflictPolicy,
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

    pub fn from_drag_payload(payload: FileDragPayload, destination: FileEndpoint) -> Self {
        let operation = if payload
            .sources
            .iter()
            .all(|source| source.is_same_device(&destination))
        {
            payload.operation_hint
        } else {
            TransferOperation::Copy
        };

        Self {
            sources: payload.sources,
            destination,
            operation,
            conflict_policy: ConflictPolicy::Ask,
        }
    }
}

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
            conflict_policy: ConflictPolicy::AutoRename,
        }
    }
}

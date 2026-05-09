mod conflict;
mod endpoint;
mod manager;
mod request;
mod view;

pub use conflict::{
    ConflictPrompt, ConflictResolution, ConflictResolver, SharedConflictResolver,
    is_apply_to_all, set_apply_to_all, wait_for_resolution,
};
pub use endpoint::FileEndpoint;
pub use manager::{TRANSFER_CANCELLED_MSG, TransferManager, TransferSummary};
pub use request::{ConflictPolicy, FileDragPayload, TransferOperation, TransferRequest};
pub use view::{
    TransferTaskHandler, TransferTaskState, TransferTaskStatus, endpoint_label, quick_total_size,
    render_transfer_overlay, transfer_label, transfer_status_label,
};

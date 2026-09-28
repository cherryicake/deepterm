mod conflict;
mod cross_remote;
mod endpoint;
pub mod file_tree_cache;
pub mod file_tree_event;
pub mod file_tree_loader;
pub mod filesystem;
mod local_ops;
mod manager;
mod remote_ops;
mod request;
mod scp_key_lease;
pub mod project_search;
pub mod types;
mod upload_download;
mod utils;
mod view;
pub mod walk;
pub mod watcher;

pub use conflict::{
    ConflictPrompt, ConflictResolution, ConflictResolver, SharedConflictResolver, is_apply_to_all,
    set_apply_to_all, wait_for_resolution,
};
pub use cross_remote::{NoopReporter, TransferStatusReporter};
pub use endpoint::FileEndpoint;
pub use filesystem::{
    FileMetadata, FileOpError, FileSystem, LocalFileSystem, RemoteFileSystem,
};
pub use manager::{TransferManager, TransferRun};
pub use request::{ConflictPolicy, TransferOperation, TransferRequest};
pub use utils::{TRANSFER_CANCELLED_MSG, TransferSummary, validate_rename_to};
pub use view::{
    TransferHoverAction, TransferTaskHandler, TransferTaskState, TransferTaskStatus,
    endpoint_label, quick_total_size, render_transfer_panel, transfer_item_name, transfer_label,
};
pub use watcher::{FsChangeEvent, FsChangeKind, FsWatcher, RemoteFsWatcher, WatcherContext};
pub use project_search::{
    FileMatch, MatchLine, MAX_SEARCH_FILE_BYTES, ProjectMatcher, ProjectSearchOptions,
    ProjectSearchOutcome, SearchMode, search_project,
};
pub use walk::{
    FileEntry, MAX_WALK_ENTRIES, WalkCancellation, WalkOptions, WalkOutcome, walk, walk_streaming,
};

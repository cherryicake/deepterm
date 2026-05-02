use crate::file_transfer::TransferRequest;

pub mod remote_view;
pub mod shared;
mod target;
pub mod view;

#[derive(Clone, Debug)]
pub enum FileBrowserEvent {
    Close,
    TransferRequested(TransferRequest),
}

pub use remote_view::RemoteFileBrowserView;
pub use shared::FileBrowserViewMode;
pub use target::FileBrowserTarget;
pub use view::FileBrowserView;

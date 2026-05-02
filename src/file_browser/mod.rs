pub mod remote_view;
pub mod shared;
mod target;
pub mod view;

#[derive(Clone, Debug)]
pub enum FileBrowserEvent {
    Close,
}

pub use remote_view::RemoteFileBrowserView;
pub use shared::FileBrowserViewMode;
pub use target::FileBrowserTarget;
pub use view::FileBrowserView;

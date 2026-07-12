use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Entity, Window};

use crate::components::macos_sidebar::MacosSidebar;
use crate::editor::Editor;
use crate::file_manager::file_tree_cache::RemoteFileTreeCache;
use crate::file_manager::filesystem::{FileSystem, LocalFileSystem, RemoteFileSystem};
use crate::sidebar_view::file_tree::SidebarFileTree;
use crate::ssh::SshConnectionConfig;

pub fn handle_open_path(
    editor: Entity<Editor>,
    sidebar: Entity<MacosSidebar>,
    path: String,
    ssh_config: Option<SshConnectionConfig>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(ssh_config) = ssh_config {
        let backend = Arc::new(RemoteFileSystem::new(ssh_config.clone())) as Arc<dyn FileSystem>;
        let remote_path = PathBuf::from(&path);
        let is_dir = backend
            .metadata(&remote_path)
            .map(|m| m.is_dir)
            .unwrap_or(false);
        if is_dir {
            let file_tree = cx.new(|cx| {
                let mut tree =
                    SidebarFileTree::new(cx, backend, Some(remote_path.clone()));
                tree.open_path(Some(remote_path), cx);
                tree
            });
            // 为新树注册共享缓存引用
            RemoteFileTreeCache::global().lock().unwrap().acquire(&ssh_config);
            sidebar.update(cx, |s, cx| {
                s.set_file_tree_view(file_tree, "Files".to_string(), None, cx);
                if !s.is_visible() {
                    s.toggle();
                }
            });
        } else {
            editor.update(cx, |e, cx| {
                e.open_remote_file_no_notify(path, ssh_config, window, cx);
            });
        }
    } else {
        let local_path = PathBuf::from(&path);
        if local_path.is_dir() {
            let backend = Arc::new(LocalFileSystem) as Arc<dyn FileSystem>;
            let file_tree = cx.new(|cx| {
                let mut tree =
                    SidebarFileTree::new(cx, backend, Some(local_path.clone()));
                tree.open_path(Some(local_path), cx);
                tree
            });
            sidebar.update(cx, |s, cx| {
                s.set_file_tree_view(file_tree, "Files".to_string(), None, cx);
                if !s.is_visible() {
                    s.toggle();
                }
            });
        } else {
            editor.update(cx, |e, cx| {
                e.open_file_no_notify(local_path, window, cx);
            });
        }
    }
}

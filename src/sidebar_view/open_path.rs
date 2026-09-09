use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Entity, Window};

use crate::components::macos_sidebar::MacosSidebar;
use crate::editor::Editor;
use crate::file_manager::filesystem::{FileSystem, LocalFileSystem, RemoteFileSystem};
use crate::sidebar_view::file_tree::SidebarFileTree;
use crate::ssh::SshConnectionConfig;

/// Open a terminal-clicked path.
///
/// 不变式：文件树 root 恒等于发起终端的工作目录（`terminal_cwd`）。
/// 点击子目录只在当前树内 reveal/展开，永不把子目录提升为 root。
/// - 文件 → 编辑器打开（带可选 `line:col` 跳转）+ 树内 reveal 定位；
/// - 目录 → 树内 reveal 展开；
/// - 点击路径落在当前 root 之外（如绝对路径）→ 树保持不动（仍为终端 cwd），
///   文件照常在编辑器打开。
pub fn handle_open_path(
    editor: Entity<Editor>,
    sidebar: Entity<MacosSidebar>,
    path: String,
    line: Option<u32>,
    col: Option<u32>,
    ssh_config: Option<SshConnectionConfig>,
    terminal_cwd: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(ssh_config) = ssh_config {
        let backend = Arc::new(RemoteFileSystem::new(ssh_config.clone())) as Arc<dyn FileSystem>;
        let remote_path = PathBuf::from(&path);
        let desired_root = terminal_cwd.map(PathBuf::from);
        let window_handle = window.window_handle();
        cx.spawn(async move |cx| {
            let is_dir = cx
                .background_executor()
                .spawn({
                    let backend = backend.clone();
                    let remote_path = remote_path.clone();
                    async move {
                        backend
                            .metadata(&remote_path)
                            .map(|m| m.is_dir)
                            .unwrap_or(false)
                    }
                })
                .await;
            cx.update_window(window_handle, |_view, window, cx| {
                // 先把树根对齐到终端 cwd（ remote cwd），再 reveal，绝不以点击路径为根。
                // 退化兜底：cwd 完全未知且无现有树时才以点击目录建树（旧行为），
                // 避免点了没反应；只要 cwd 已知就严格遵守 root == cwd。
                let no_tree_no_cwd = sidebar.read(cx).file_tree_view().is_none()
                    && desired_root.is_none();
                let tree =
                    ensure_tree_for_cwd(&sidebar, backend.clone(), desired_root, Some(&ssh_config), cx);
                if is_dir {
                    if let Some(tree) = tree {
                        let remote_path_clone = remote_path.clone();
                        tree.update(cx, |t, cx| {
                            t.reveal(remote_path_clone, cx);
                        });
                    } else if no_tree_no_cwd {
                        let file_tree = cx.new(|cx| {
                            let mut tree =
                                SidebarFileTree::new(cx, backend, Some(remote_path.clone()));
                            tree.open_path(Some(remote_path), cx);
                            tree
                        });
                        sidebar.update(cx, |s, cx| {
                            s.set_file_tree_view(file_tree, "Files".to_string(), None, cx);
                        });
                    }
                    sidebar.update(cx, |s, _cx| {
                        if !s.is_visible() {
                            s.toggle();
                        }
                    });
                } else {
                    let ssh_for_open = ssh_config.clone();
                    editor.update(cx, |e, cx| {
                        e.open_remote_file_at_line_col_no_notify(
                            path, ssh_for_open, line, col, window, cx,
                        );
                    });
                    if let Some(tree) = tree {
                        tree.update(cx, |t, cx| {
                            t.reveal(remote_path, cx);
                        });
                    }
                }
            })
            .ok();
        })
        .detach();
    } else {
        let local_path = PathBuf::from(&path);
        let desired_root = terminal_cwd.map(PathBuf::from);
        let backend = Arc::new(LocalFileSystem) as Arc<dyn FileSystem>;
        if local_path.is_dir() {
            // 目录点击：只 reveal/展开，root 保持为终端 cwd。
            // 退化兜底：cwd 未知且无现有树时才以点击目录建树（旧行为）。
            let no_tree_no_cwd =
                sidebar.read(cx).file_tree_view().is_none() && desired_root.is_none();
            if let Some(tree) =
                ensure_tree_for_cwd(&sidebar, backend.clone(), desired_root, None, cx)
            {
                tree.update(cx, |t, cx| {
                    t.reveal(local_path, cx);
                });
            } else if no_tree_no_cwd {
                let file_tree = cx.new(|cx| {
                    let mut tree = SidebarFileTree::new(cx, backend, Some(local_path.clone()));
                    tree.open_path(Some(local_path), cx);
                    tree
                });
                sidebar.update(cx, |s, cx| {
                    s.set_file_tree_view(file_tree, "Files".to_string(), None, cx);
                });
            }
            sidebar.update(cx, |s, _cx| {
                if !s.is_visible() {
                    s.toggle();
                }
            });
        } else {
            editor.update(cx, |e, cx| {
                e.open_file_at_line_col_no_notify(local_path.clone(), line, col, window, cx);
            });
            if let Some(tree) = ensure_tree_for_cwd(&sidebar, backend, desired_root, None, cx) {
                tree.update(cx, |t, cx| {
                    t.reveal(local_path, cx);
                });
            }
        }
    }
}

/// Ensure the sidebar tree exists, matches the backend, and is rooted at the
/// terminal cwd. Returns the tree entity, or `None` when there is neither a
/// usable current tree nor a desired root to create one from.
///
/// 永远不会以点击路径为根：调用方传 `desired_root = terminal_cwd`。
fn ensure_tree_for_cwd(
    sidebar: &Entity<MacosSidebar>,
    backend: Arc<dyn FileSystem>,
    desired_root: Option<PathBuf>,
    ssh_config: Option<&SshConnectionConfig>,
    cx: &mut App,
) -> Option<Entity<SidebarFileTree>> {
    if let Some(tree) = sidebar.read(cx).file_tree_view() {
        let (backend_ok, root_ok) = {
            let t = tree.read(cx);
            let backend_ok = if ssh_config.is_some() {
                // `Option<&T> == Option<&T>` 按值比对；`SshConnectionConfig`
                // 的 `PartialEq` 已排除 `created_at`，同一连接不同实例仍复用。
                !t.is_local() && t.ssh_config() == ssh_config
            } else {
                t.is_local()
            };
            let root_ok = desired_root
                .as_ref()
                .is_none_or(|want| t.root_path() == want.as_path());
            (backend_ok, root_ok)
        };
        if backend_ok && root_ok {
            return Some(tree);
        }
        // 后端一致但 root 漂移（如终端 cd 后树未跟上）：原地切回终端 cwd，
        // 而不是切到点击的子目录。
        if backend_ok {
            if let Some(root) = desired_root.clone() {
                tree.update(cx, |t, cx| {
                    t.open_path(Some(root), cx);
                });
                return Some(tree);
            }
            // 无 cwd 信息：保留现有树（调用方 reveal 可能失败，但绝不乱切根）。
            return Some(tree);
        }
    }
    // 无可用树：仅当知道终端 cwd 时才建树，且以 cwd 为根。
    let root = desired_root?;
    let file_tree = cx.new(|cx| {
        let mut tree = SidebarFileTree::new(cx, backend, Some(root.clone()));
        tree.open_path(Some(root), cx);
        tree
    });
    sidebar.update(cx, |s, cx| {
        s.set_file_tree_view(file_tree.clone(), "Files".to_string(), None, cx);
    });
    Some(file_tree)
}

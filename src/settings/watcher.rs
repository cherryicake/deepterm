//! 配置文件的热重载。
//!
//! 监听 `settings.json` 与 `keymap.json` 的外部修改。应用自身的写入走
//! [`SettingsStore::update_user`] 等路径，不依赖本模块；这里只负责
//! 「用户在外部编辑器里改了文件」这条路径。
//!
//! 监听父目录而非文件本身：写入用 `atomic_write`（临时文件 + rename），
//! inode 会变，直接监听文件在某些平台上会随 rename 丢失句柄。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, BorrowAppContext as _};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

use super::store::SettingsStore;

/// 监听一个配置文件，内容变化时调用 `on_change`。
///
/// 事件按 80ms 窗口防抖：编辑器保存往往是「写临时文件 + rename」两步，
/// 事件会连着来，折叠后只触发一次重载。
pub fn watch_config_file(
    path: &Path,
    label: &'static str,
    cx: &mut App,
    on_change: impl Fn(&mut App) + 'static,
) {
    let Some(parent) = path.parent().map(Path::to_path_buf) else {
        log::warn!("{label} 没有父目录，热重载未启用");
        return;
    };

    if let Err(err) = std::fs::create_dir_all(&parent) {
        log::warn!("failed to create config directory {}: {err}", parent.display());
    }

    let (tx, rx) = async_channel::unbounded::<()>();
    let file_name = path.file_name().map(|name| name.to_owned());

    let watcher = match RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };
            // 只关心目标文件的事件：配置目录里的其它写入不应触发重载。
            let touches_target = event
                .paths
                .iter()
                .any(|changed| changed.file_name().map(|name| name.to_owned()) == file_name);
            if touches_target {
                let _ = tx.try_send(());
            }
        },
        notify::Config::default().with_poll_interval(Duration::from_millis(500)),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            log::error!("failed to create watcher for {label}: {err} — 热重载未启用");
            return;
        }
    };

    let watcher = Arc::new(Mutex::new(watcher));
    if let Err(err) = watcher.lock().watch(&parent, RecursiveMode::NonRecursive) {
        log::error!("failed to watch {}: {err} — 热重载未启用", parent.display());
        return;
    }

    cx.spawn(async move |cx| {
        // 持有句柄，防止 watcher 被 drop 后停止监听。
        let _keep_alive = watcher;

        while rx.recv().await.is_ok() {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            while rx.try_recv().is_ok() {}

            // 窗口可能已全部关闭；此时静默跳过即可。
            cx.update(|cx| on_change(cx));
        }
    })
    .detach();
}

/// 监听用户设置文件。
pub fn watch_user_settings(cx: &mut App) {
    let Some(path) = super::store::user_settings_path() else {
        log::warn!("无法解析 settings.json 路径 — 热重载未启用");
        return;
    };

    let watched = path.clone();
    watch_config_file(&path, "settings.json", cx, move |cx| {
        let Ok(text) = std::fs::read_to_string(&watched) else {
            // 文件被删除或正在被替换：保留当前设置，等下一次事件。
            return;
        };
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.set_user_document(&text, cx);
        });
    });
}

//! 用户键位表与按名派发。
//!
//! 按名派发本身由 gpui 提供：`actions!` 宏内部走 `#[derive(gpui::Action)]`，
//! 该 derive 通过 `inventory` 把每个 action 注册进 `ActionRegistry`
//! （`gpui/src/action.rs:287`），因此 `App::build_action(name, params)`
//! 对全部已声明 action 直接可用。本模块只负责「定位用户文件 + 解析」。

use std::path::PathBuf;

use gpui::{App, KeyBinding};

use super::file;

/// 用户键位表路径：`<配置目录>/keymap.json`。
pub fn user_keymap_path() -> Option<PathBuf> {
    Some(crate::workspace::persistence::PersistenceManager::get_config_dir().join("keymap.json"))
}

/// 读取并解析用户键位表。
///
/// 文件不存在时返回空绑定（正常情况，不是错误）。
pub fn load_user_keymap(cx: &App) -> Result<(Vec<KeyBinding>, Vec<String>), String> {
    let Some(path) = user_keymap_path() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(err) => return Err(format!("{}：{err}", path.display())),
    };

    Ok(file::parse(&source, cx))
}

/// 监听用户键位表并热重载。
pub fn watch_user_keymap(cx: &mut App) {
    let Some(path) = user_keymap_path() else {
        return;
    };
    let Some(parent) = path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&parent) {
        log::warn!("failed to create config directory {}: {err}", parent.display());
        return;
    }

    crate::settings::watcher::watch_config_file(&path, "keymap.json", cx, |cx| {
        super::reload(cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_user_keymap_is_not_an_error() {
        // 用一个不存在的路径验证「文件缺失 → 空绑定且无错误」。
        let source = "";
        let sections = file::sections_of(source);
        assert!(sections.is_err(), "空字符串不是合法 JSON 数组");

        // 真正的空文档（合法 JSON）应当解析出零条绑定。
        let sections = file::sections_of("[]").unwrap();
        assert!(sections.is_empty());
    }
}

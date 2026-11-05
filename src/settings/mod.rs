//! 用户设置的统一入口。
//!
//! 与 Zed 同构的两阶段模型：先把默认文档与用户文档合并成一份
//! [`SettingsContent`]，再对每个已注册的设置类型跑一遍
//! [`Settings::from_settings`]。合并语义见 [`merge`]：结构体递归合并、
//! `Option::None` 跳过、标量与数组覆盖。
//!
//! 分工原则：`settings.json` 只承载**用户手写可编辑的设置**；机器状态
//! （会话布局、命令历史、sftp 缓存）与凭据（keychain、ssh 密钥）留在各自的
//! 文件里，避免把「可手改」与「程序自管」混在一处。

pub mod content;
pub mod font_state;
pub mod store;
pub mod watcher;

pub use content::*;
pub use font_state::FontState;
pub use store::{Settings, SettingsStore};

use gpui::App;

/// 初始化设置系统：读取默认文档与用户文件、完成首轮求值、启动热重载。
///
/// 必须在 `theme::init` 与 [`FontState::init`] 之前调用——这两者改为从
/// [`SettingsStore`] 取值，而不是各自读文件。
pub fn init(cx: &mut App) {
    SettingsStore::init(cx);
    observe(cx);
    watcher::watch_user_settings(cx);
}

/// 让各子系统跟随设置变化。
///
/// 这里集中订阅而不是让每个子系统自己 `observe_global`：设置的消费者数量会
/// 随阶段推进增长，集中一处能避免遗漏，也让「改了设置谁会响应」一目了然。
fn observe(cx: &mut App) {
    cx.observe_global::<SettingsStore>(|cx| {
        crate::theme::ThemeState::sync_from_settings(cx);
        FontState::sync_from_settings(cx);
        // vim 开关决定是否加载 vim 键位表，所以设置一变就要重建 keymap。
        crate::keymap::reload(cx);
        // 关闭 vim 时清掉各编辑器残留的模式状态：否则 Normal 模式会继续
        // 吞掉按键，而 vim 键位表已卸载——编辑器在重启前无法输入。
        crate::vim::disable_all(cx);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};

    /// 设置变化后，字体与主题快照必须跟着更新——这是热重载生效的实际判据。
    #[gpui::test]
    fn settings_change_propagates_to_font_and_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            SettingsStore::init_for_test(cx);
            crate::theme::init(cx);
            FontState::init(cx);
            observe(cx);
        });

        let before_font = cx.update(|cx| FontState::global(cx).settings.editor_font_size);
        let before_theme = cx.update(|cx| {
            crate::theme::ThemeState::global(cx)
                .active_theme_id()
                .to_string()
        });

        // 模拟用户在 settings.json 里改了字号与主题。
        let other_theme = cx.update(|cx| {
            crate::theme::ThemeState::themes(cx)
                .iter()
                .map(|theme| theme.id.clone())
                .find(|id| *id != before_theme)
                .expect("内置主题不止一个")
        });

        let other_theme_for_store = other_theme.clone();
        cx.update(|cx| {
            // 走真实入口：`set_user_document` 会重算并触发 `observe_global` 链。
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_document(
                    &serde_json::json!({
                        "font": { "editor_font_size": 33.0 },
                        "theme": { "id": other_theme_for_store },
                    })
                    .to_string(),
                    cx,
                );
            });
        });

        let after_font = cx.update(|cx| FontState::global(cx).settings.editor_font_size);
        let after_theme = cx.update(|cx| {
            crate::theme::ThemeState::global(cx)
                .active_theme_id()
                .to_string()
        });

        assert_eq!(
            before_font, 16.0,
            "默认字号来自 assets/default_settings.json"
        );
        assert_eq!(after_font, 33.0, "字号变更必须传到 FontState");
        assert_eq!(after_theme, other_theme, "主题变更必须传到 ThemeState");
    }

    /// 未初始化 store 时，字体与主题仍须可用（测试与独立渲染组件的路径）。
    #[gpui::test]
    fn font_and_theme_work_without_settings_store(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(cx.try_global::<SettingsStore>().is_none());
            crate::theme::init(cx);
            FontState::init(cx);
        });

        cx.update(|cx| {
            assert!(crate::theme::ThemeState::themes(cx).len() > 1);
            let font = FontState::global(cx);
            assert!(font.settings.editor_font_size > 0.0);
        });
    }
}

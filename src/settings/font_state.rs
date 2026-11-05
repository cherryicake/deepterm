use gpui::{App, Global};

use super::content::FontContent;
use super::store::SettingsStore;

/// 字体设置的只读快照。
///
/// 数据的唯一真源是 [`SettingsStore`]；本类型不再自己读写磁盘，只作为
/// 「渲染路径便捷取用」的门面。这样字体的读写与其它设置走同一条路径
/// （热重载、差异写回、未知键保留都自动生效）。
#[derive(Clone, Debug)]
pub struct FontState {
    pub settings: FontContent,
}

impl Global for FontState {}

impl FontState {
    /// 安装字体快照。
    ///
    /// 设置 store 缺席时退回内置默认值，而不是 panic：测试与独立渲染
    /// 组件会只初始化本类型。
    pub fn init(cx: &mut App) {
        let settings = cx
            .try_global::<SettingsStore>()
            .map(|store| store.content().font.clone())
            .unwrap_or_default();
        cx.set_global(FontState { settings });
    }

    /// 从设置 store 刷新快照。`SettingsStore` 变化后由订阅方调用。
    pub fn sync_from_settings(cx: &mut App) {
        let settings = cx.global::<SettingsStore>().content().font.clone();
        // 先比较再取可变引用：`global_mut` 会无条件通知观察者，
        // 设置里改的是别的段落时不该惊动字体的订阅方。
        if cx.global::<FontState>().settings == settings {
            return;
        }
        cx.global_mut::<FontState>().settings = settings;
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// 修改字体设置：写回 `settings.json`，并同步本快照。
    pub fn update<F>(cx: &mut App, f: F)
    where
        F: FnOnce(&mut FontContent),
    {
        if cx.try_global::<SettingsStore>().is_none() {
            // 无 store 时只改快照，不落盘（测试路径）。
            let state = cx.global_mut::<FontState>();
            f(&mut state.settings);
            cx.refresh_windows();
            return;
        }

        SettingsStore::update_user(cx, |content| f(&mut content.font));
        Self::sync_from_settings(cx);
    }
}

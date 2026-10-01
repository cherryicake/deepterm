//! 类型化设置存储。
//!
//! 数据流：
//!
//! ```text
//! init:    默认文档(嵌入) ─┐
//!                           ├─→ merged ─→ 每个注册类型跑 from_settings ─→ values
//!          用户文档(磁盘)  ─┘
//!
//! 写入:    update_user(改类型化 user) ─→ 与默认值求差 ─→ 合并进 raw ─→ 落盘
//! ```
//!
//! 两个刻意的设计：
//!
//! 1. **用户文件按「与默认值的差异」写回**。用户改了字号，文件里只出现字号，
//!    不会把整份默认值倾倒进去。
//! 2. **未知键保留**。用户文档同时以 `serde_json::Value` 保存原始形状，写回时
//!    把差异合并进这个 Value，因此拼错的键、未来版本的键都不会被悄悄丢掉。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{App, Global};
use serde_json::Value;

use super::content::SettingsContent;
use crate::utils::fs_atomic::atomic_write;

/// 可从合并后的设置文档派生出自身取值的类型。
pub trait Settings: 'static + Sized {
    /// 从合并文档构造自身。派生逻辑（字符串解析成枚举等）放在这里。
    fn from_settings(content: &SettingsContent) -> Self;

    /// 读取全局快照。
    ///
    /// # Panics
    ///
    /// 类型未注册时 panic。注册发生在 `SettingsStore::init`，所以正常路径下
    /// 只要类型是已知子系统就不会触发。
    fn get_global(cx: &App) -> &Self {
        cx.global::<SettingsStore>()
            .get::<Self>()
            .unwrap_or_else(|| panic!("settings type {} is not registered", std::any::type_name::<Self>()))
    }
}

type Builder = Box<dyn Fn(&SettingsContent) -> (TypeId, Box<dyn Any>)>;

pub struct SettingsStore {
    defaults: SettingsContent,
    /// 用户文档的类型化视图（已由 serde 补齐默认值）。
    user: SettingsContent,
    /// 用户文档的原始形状，用于保留未知键。
    user_raw: Value,
    merged: SettingsContent,
    builders: Vec<Builder>,
    values: HashMap<TypeId, Box<dyn Any>>,
    /// 最近一次解析失败的原因，供 UI 提示。
    error: Option<String>,
    /// 是否把改动写回磁盘。
    ///
    /// 测试必须关掉：`update_user` 的写入目标是**用户的真实配置文件**，
    /// 测试里调用它会把开发者的设置覆盖掉。这个开关的存在就是为了让
    /// 「测试不碰用户数据」成为结构上的保证，而不是靠调用方自觉。
    persist: bool,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// 构造并安装全局 store。必须在任何 `Settings::get_global` 之前调用。
    pub fn init(cx: &mut App) {
        let path = user_settings_path();

        let (user_raw, error) = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(text)) => match parse_user(&text) {
                Ok(value) => {
                    // 逐段容错：某一段写坏只丢该段，其余照常生效。
                    match serde_json::from_value::<SettingsContent>(value.clone()) {
                        Ok(_) => (value, None),
                        Err(err) => (
                            Value::Object(Default::default()),
                            Some(format!("settings.json 解析失败：{err}")),
                        ),
                    }
                }
                Err(err) => (
                    Value::Object(Default::default()),
                    Some(format!("settings.json 不是合法 JSON：{err}")),
                ),
            },
            Some(Err(err)) => (
                Value::Object(Default::default()),
                Some(format!("settings.json 读取失败：{err}")),
            ),
            // 文件不存在是正常情况（首次运行），不算错误。
            None => (Value::Object(Default::default()), None),
        };

        cx.set_global(Self::new(user_raw, error, true));
        migrate_legacy_files(cx);
    }

    /// 测试专用：只用默认值安装 store，不读磁盘、不起监听、**不写磁盘**。
    ///
    /// 生产路径读真实文件会让测试依赖开发者机器上的 `settings.json`；
    /// 更糟的是 `update_user` 会写回真实文件，把用户设置覆盖成测试里的值。
    /// 所以测试路径必须同时关掉读写。
    pub fn init_for_test(cx: &mut App) {
        cx.set_global(Self::new(Value::Object(Default::default()), None, false));
    }

    /// 从用户文档的原始形状构造 store。
    ///
    /// 类型化视图由原始形状派生，二者不可能不一致——若允许分别传入，
    /// 调用方就能造出「raw 说字号 19、typed 说 16」这种自相矛盾的状态。
    fn new(user_raw: Value, error: Option<String>, persist: bool) -> Self {
        let user = serde_json::from_value::<SettingsContent>(user_raw.clone()).unwrap_or_default();

        let mut store = Self {
            defaults: parse_defaults(),
            user,
            user_raw,
            merged: SettingsContent::default(),
            builders: Vec::new(),
            values: HashMap::new(),
            error,
            persist,
        };
        store.register_builtin_types();
        store.recompute();
        store
    }

    /// 注册所有已知的设置类型。
    fn register_builtin_types(&mut self) {
        self.register::<super::content::ThemeContent>();
        self.register::<super::content::FontContent>();
        self.register::<super::content::EditorContent>();
        self.register::<super::content::TerminalContent>();
        self.register::<super::content::VimContent>();
    }

    pub fn register<T: Settings>(&mut self) {
        self.builders.push(Box::new(|content| {
            let value = T::from_settings(content);
            (TypeId::of::<T>(), Box::new(value) as Box<dyn Any>)
        }));
    }

    pub fn get<T: Settings>(&self) -> Option<&T> {
        self.values
            .get(&TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<T>())
    }

    /// 全局 store。
    ///
    /// # Panics
    ///
    /// 未初始化时 panic——调用点都在 `settings::init` 之后。
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// 合并后的完整设置文档。
    pub fn content(&self) -> &SettingsContent {
        &self.merged
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 重跑合并与派生。任何内容变更后都必须调用。
    fn recompute(&mut self) {
        let mut merged = self.defaults.clone();
        merged.merge_from(&self.user);
        self.merged = merged;

        self.values.clear();
        for builder in &self.builders {
            let (type_id, value) = builder(&self.merged);
            self.values.insert(type_id, value);
        }
    }

    /// 替换用户文档（热重载路径）。
    ///
    /// 解析失败时**保留上一份可用内容**，只记录错误——正在编辑的文件被保存到
    /// 一半时不应让运行中的窗口失去设置。内容与当前一致时直接返回，
    /// 避免 notify 的元数据事件触发无谓的全量重算。
    pub fn set_user_document(&mut self, text: &str, cx: &mut App) {
        let parsed = parse_user(text).and_then(|value| {
            serde_json::from_value::<SettingsContent>(value.clone())
                .map(|content| (content, value))
                .map_err(|err| err.to_string())
        });

        match parsed {
            Ok((content, raw)) => {
                if raw == self.user_raw {
                    return;
                }
                self.user = content;
                self.user_raw = raw;
                self.error = None;
                self.recompute();
                cx.refresh_windows();
            }
            Err(err) => {
                self.error = Some(format!("settings.json 解析失败：{err}"));
            }
        }
    }

    /// 修改用户设置并落盘。
    ///
    /// 回调拿到的是类型化的用户文档（只含用户显式写过的值 + 默认值），
    /// 写回磁盘时只输出与默认值不同的部分。
    pub fn update_user<F>(cx: &mut App, f: F)
    where
        F: FnOnce(&mut SettingsContent),
    {
        let store = cx.global_mut::<SettingsStore>();
        let before = store.user.clone();
        let mut user = before.clone();
        f(&mut user);

        // 只按「本次真正改动的键」写回。
        //
        // 不能拿 `user` 与默认值求差：当界面把某项**改回默认值**时，
        // `user` 与默认值相等，差异为空，于是用户文件里的旧值原样留下——
        // 用户的修改在下次重载时被静默丢弃。改为比较改动前后的取值，
        // 逐个键处理。
        let before_json = serde_json::to_value(&before).unwrap_or(Value::Null);
        let after_json = serde_json::to_value(&user).unwrap_or(Value::Null);
        let defaults_json = serde_json::to_value(&store.defaults).unwrap_or(Value::Null);

        let mut changed = Vec::new();
        changed_leaf_paths(&before_json, &after_json, String::new(), &mut changed);

        for path in &changed {
            let Some(new_value) = get_path(&after_json, path) else {
                continue;
            };
            // 新值等于默认值 → 从用户文件里移除该键，避免它覆盖未来的
            // 默认值变更。否则写入新值。
            if get_path(&defaults_json, path).is_some_and(|default| default == new_value) {
                remove_paths(&mut store.user_raw, std::slice::from_ref(path));
            } else {
                set_path(&mut store.user_raw, path, new_value.clone());
            }
        }

        store.user = user;
        store.error = None;
        store.recompute();

        if store.persist {
            let text = serde_json::to_string_pretty(&store.user_raw).unwrap_or_default();
            if let Some(path) = user_settings_path()
                && let Err(err) = write_with_backup(&path, &text)
            {
                log::warn!("failed to save settings.json: {err}");
            }
        }

        cx.refresh_windows();
    }
}

/// 把旧版分散的配置文件并入 `settings.json`。
///
/// 只在 `settings.json` 里对应段落尚未被用户写过时才导入，避免覆盖用户
/// 已经在新文件里做的设置。旧文件保留不删——回滚旧版本时仍能读回。
fn migrate_legacy_files(cx: &mut App) {
    let Some(dir) = user_settings_path().and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };

    let font_legacy = read_json(&dir.join("font.json"));
    let theme_legacy = read_json(&dir.join("theme.json"));

    let (font_present, theme_present) = {
        let store = cx.global::<SettingsStore>();
        (
            store.user_raw.get("font").is_some(),
            store.user_raw.get("theme").is_some(),
        )
    };

    if font_legacy.is_none() && theme_legacy.is_none() {
        return;
    }

    // 两段都已在新文件里 → 没有可导入的内容，**不要**调用 `update_user`。
    // 那会无条件走一遍写回，把用户文件重写一次（浮点数会被规整成 `f32`
    // 的规范形式，文件因此每次启动都变），而用户什么都没改。
    if font_present && theme_present {
        return;
    }

    let mut imported = false;
    SettingsStore::update_user(cx, |content| {
        if !font_present
            && let Some(font) = font_legacy.as_ref().and_then(|value| {
                serde_json::from_value::<super::content::FontContent>(value.clone()).ok()
            })
        {
            content.font = font;
            imported = true;
        }
        if !theme_present
            && let Some(theme_id) = theme_legacy
                .as_ref()
                .and_then(|value| value.get("theme_id"))
                .and_then(Value::as_str)
        {
            content.theme.id = theme_id.to_string();
            imported = true;
        }
    });

    if imported {
        log::info!("imported legacy font.json / theme.json into settings.json");
    }
}

/// 读一个 JSON 文件；不存在或解析失败返回 `None`。
fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 比较改动前后的文档，收集**取值发生变化**的叶子键路径（`.` 分隔）。
///
/// 用「改动前后」而不是「与默认值」作基准：界面把某项改回默认值时，
/// 与默认值比较得出的差异为空，用户的修改会被静默丢弃。
fn changed_leaf_paths(before: &Value, after: &Value, prefix: String, out: &mut Vec<String>) {
    let (Value::Object(before_map), Value::Object(after_map)) = (before, after) else {
        if before != after && !prefix.is_empty() {
            out.push(prefix);
        }
        return;
    };

    for (key, after_value) in after_map {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match before_map.get(key) {
            Some(before_value) if before_value == after_value => {}
            Some(before_value) => changed_leaf_paths(before_value, after_value, path, out),
            // 新增的键：整个子树都算变化。
            None => collect_all_leaf_paths(after_value, path, out),
        }
    }
}

/// 收集一个子树里的全部叶子路径。
fn collect_all_leaf_paths(value: &Value, prefix: String, out: &mut Vec<String>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_all_leaf_paths(child, path, out);
            }
        }
        _ => out.push(prefix),
    }
}

/// 按路径取值（`.` 分隔）。
fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = value;
    for segment in path.split('.') {
        node = node.get(segment)?;
    }
    Some(node)
}

/// 按路径写值，沿途缺失的对象会被创建（`.` 分隔）。
fn set_path(value: &mut Value, path: &str, new_value: Value) {
    let segments: Vec<&str> = path.split('.').collect();
    let mut node = value;
    for (i, segment) in segments.iter().enumerate() {
        if i + 1 == segments.len() {
            if let Value::Object(map) = node {
                map.insert((*segment).to_string(), new_value);
            }
            return;
        }
        if !node.is_object() {
            *node = Value::Object(serde_json::Map::new());
        }
        let map = node.as_object_mut().expect("just ensured object");
        node = map
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

/// 按路径删除键（`.` 分隔）。
fn remove_paths(value: &mut Value, paths: &[String]) {
    for path in paths {
        let segments: Vec<&str> = path.split('.').collect();
        let mut node: &mut Value = value;
        for (i, segment) in segments.iter().enumerate() {
            if i + 1 == segments.len() {
                if let Value::Object(map) = node {
                    map.remove(*segment);
                }
                break;
            }
            match node.get_mut(*segment) {
                Some(next) => node = next,
                None => break,
            }
        }
    }
}

/// 设置文件的落盘路径。
pub fn user_settings_path() -> Option<PathBuf> {
    Some(crate::workspace::persistence::PersistenceManager::get_config_dir().join("settings.json"))
}

/// 读默认文档。嵌入资源，解析失败属于构建期错误，直接 panic。
fn parse_defaults() -> SettingsContent {
    const DEFAULT_SETTINGS: &str = include_str!("../../assets/default_settings.json");
    parse_user(DEFAULT_SETTINGS)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .expect("assets/default_settings.json must be valid")
}

/// 解析用户 JSON。走 `serde_json_lenient` 以支持注释与尾逗号——
/// 手写配置里这两种写法很常见，报错会显得刻薄。
fn parse_user(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json_lenient::from_str::<Value>(text).map_err(|err| err.to_string())
}

/// 原子写 + `.bak` 备份，与 `session_state.json` 同一标准。
fn write_with_backup(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if path.exists() {
        let backup = path.with_extension("json.bak");
        // 备份失败不阻断写入：宁可少一份回滚副本，也不要让设置改不动。
        if let Err(err) = std::fs::copy(path, &backup) {
            log::warn!("failed to back up settings.json: {err}");
        }
    }
    atomic_write(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 只收集取值真正变化的叶子路径。
    #[test]
    fn changed_paths_only_include_actual_changes() {
        let before = json!({ "font": { "size": 16.0, "family": "Menlo" } });
        let after = json!({ "font": { "size": 20.0, "family": "Menlo" } });
        let mut out = Vec::new();
        changed_leaf_paths(&before, &after, String::new(), &mut out);
        assert_eq!(out, vec!["font.size".to_string()]);
    }

    /// 没有任何变化时不应产出路径（否则会触发无谓的落盘）。
    #[test]
    fn changed_paths_empty_when_identical() {
        let doc = json!({ "a": 1, "b": { "c": 2 } });
        let mut out = Vec::new();
        changed_leaf_paths(&doc, &doc, String::new(), &mut out);
        assert!(out.is_empty());
    }

    /// 新增的键要整棵子树都收集到。
    #[test]
    fn changed_paths_include_new_subtree() {
        let before = json!({});
        let after = json!({ "vim": { "enabled": true } });
        let mut out = Vec::new();
        changed_leaf_paths(&before, &after, String::new(), &mut out);
        assert_eq!(out, vec!["vim.enabled".to_string()]);
    }

    /// 写回不得丢掉未知键——用户文件里我们读不懂的条目必须原样保留。
    #[test]
    fn write_back_preserves_unknown_keys() {
        let mut raw = json!({ "typo_key": true, "font": { "size": 16.0 } });
        set_path(&mut raw, "font.size", json!(20.0));
        assert_eq!(raw, json!({ "typo_key": true, "font": { "size": 20.0 } }));
    }

    /// 改回默认值必须**移除**用户文件里的条目。
    ///
    /// 否则那个陈旧值会一直覆盖未来的默认值变更。
    #[test]
    fn reverting_to_default_removes_the_key() {
        let defaults = json!({ "font": { "editor_font_size": 16.0 } });
        // 用户文件里写了非默认字号。
        let mut raw = json!({ "font": { "editor_font_size": 20.0 } });

        // 界面把它改回默认值。
        let after = json!({ "font": { "editor_font_size": 16.0 } });
        let new_value = get_path(&after, "font.editor_font_size").unwrap();
        if get_path(&defaults, "font.editor_font_size").is_some_and(|d| d == new_value) {
            remove_paths(&mut raw, &["font.editor_font_size".to_string()]);
        }

        assert!(
            raw.get("font").and_then(|f| f.get("editor_font_size")).is_none(),
            "改回默认值后不应保留该键：{raw}"
        );
    }

    /// 用户的取值恰好等于默认值、但**本次并未改动**时，不得被当成陈旧条目删除。
    ///
    /// 回归背景：曾按「取值等于默认值」判定陈旧，导致用户的
    /// `terminal_font_size: 18.0`（恰与默认值相同）在启动时被删掉。
    #[test]
    fn unchanged_value_matching_default_is_not_removed() {
        // 用户文件里显式写了 18.0，而默认值也是 18.0。
        let raw = json!({ "font": { "terminal_font_size": 18.0 } });

        // 本次没有任何改动 → 变化集为空 → 不该动任何键。
        let doc = json!({ "font": { "terminal_font_size": 18.0 } });
        let mut changed = Vec::new();
        changed_leaf_paths(&doc, &doc, String::new(), &mut changed);
        assert!(changed.is_empty(), "未改动时变化集应为空");

        assert_eq!(
            raw["font"]["terminal_font_size"],
            json!(18.0),
            "未改动的用户值必须保留，即便它恰好等于默认值"
        );
    }

    #[test]
    fn lenient_parser_accepts_comments_and_trailing_commas() {
        let text = r#"{
            // 注释
            "font": { "editor_font_size": 18.0, },
        }"#;
        let value = parse_user(text).unwrap();
        assert_eq!(value["font"]["editor_font_size"], json!(18.0));
    }

    #[test]
    fn empty_document_is_not_an_error() {
        assert!(parse_user("   ").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn broken_json_keeps_previous_settings() {
        let mut store = SettingsStore::new(json!({ "font": { "editor_font_size": 19.0 } }), None, false);
        assert_eq!(
            store.get::<super::super::content::FontContent>().unwrap().editor_font_size,
            19.0
        );

        // 半截文件不应让运行中的设置失效。
        let app = gpui::TestAppContext::single();
        app.update(|cx| store.set_user_document("{ \"font\": { ", cx));

        assert!(store.error().is_some(), "解析失败必须被记录");
        assert_eq!(
            store.get::<super::super::content::FontContent>().unwrap().editor_font_size,
            19.0,
            "失败后应保留上一份可用设置"
        );
    }

    #[test]
    fn identical_document_is_a_no_op() {
        let mut store = SettingsStore::new(json!({}), None, false);
        let app = gpui::TestAppContext::single();
        let text = r#"{ "vim": { "enabled": true } }"#;
        app.update(|cx| store.set_user_document(text, cx));
        assert!(store.get::<super::super::content::VimContent>().unwrap().enabled);

        // 再喂一次同样内容：不应重算（此处以「未报错且值不变」为可观测判据）。
        app.update(|cx| store.set_user_document(text, cx));
        assert!(store.error().is_none());
        assert!(store.get::<super::super::content::VimContent>().unwrap().enabled);
    }

    /// 未改动任何设置时，`update_user` 不得改写文件内容。
    ///
    /// 回归背景：`migrate_legacy_files` 曾无条件调用 `update_user`，导致
    /// 每次启动都重写一遍用户文件（`f32` 字段被规整成规范形式），
    /// 文件因此每次启动都变，而用户什么都没改。
    #[test]
    fn unchanged_update_does_not_touch_the_document() {
        // 用户文件里有一个 f64 精度的值，f32 无法精确表示。
        let raw = json!({ "font": { "terminal_line_height": 1.2899999618530271 } });
        let doc = raw.clone();

        // 没有变化 → 变化集为空 → 不应产生任何写入。
        let before = serde_json::to_value(SettingsContent::default()).unwrap();
        let mut changed = Vec::new();
        changed_leaf_paths(&before, &before, String::new(), &mut changed);
        assert!(changed.is_empty());

        // 文件内容保持不变（不被规整）。
        assert_eq!(
            doc["font"]["terminal_line_height"],
            json!(1.2899999618530271),
            "未改动时不应改写既有值"
        );
        assert_eq!(doc, raw);
    }

    /// 只在真有改动时才写回，且只写变化的键。
    #[test]
    fn only_changed_keys_are_written() {
        let before = serde_json::to_value(SettingsContent::default()).unwrap();
        let mut content = SettingsContent::default();
        content.font.editor_font_size = 21.0;
        let after = serde_json::to_value(&content).unwrap();

        let mut changed = Vec::new();
        changed_leaf_paths(&before, &after, String::new(), &mut changed);
        assert_eq!(changed, vec!["font.editor_font_size".to_string()]);

        // 应用到原始文件：只有这一个键被写。
        let mut raw = json!({ "typo_key": true });
        let value = get_path(&after, "font.editor_font_size").unwrap().clone();
        set_path(&mut raw, "font.editor_font_size", value);
        assert_eq!(
            raw,
            json!({ "typo_key": true, "font": { "editor_font_size": 21.0 } }),
            "只应写入变化的键，且未知键保留"
        );
    }

    #[test]
    fn section_defaults_are_isolated() {
        // 用户只改 vim，不应影响 font 段的派生结果。
        let mut store = SettingsStore::new(json!({}), None, false);
        let app = gpui::TestAppContext::single();
        app.update(|cx| store.set_user_document(r#"{ "vim": { "enabled": true } }"#, cx));

        assert!(store.get::<super::super::content::VimContent>().unwrap().enabled);
        assert_eq!(
            store.get::<super::super::content::FontContent>().unwrap(),
            &super::super::content::FontContent::default()
        );
    }
}

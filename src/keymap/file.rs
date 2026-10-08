//! `keymap.json` 的解析。
//!
//! 格式与 Zed 的 keymap 一致：一个 section 数组，每段含 `context` 与
//! `bindings`。逐条绑定独立解析，**单条失败不影响其余**——一个手写错误的
//! 键位不该让整份键位表失效。

use gpui::{App, KeyBinding};
use serde::Deserialize;
use serde_json::Value;

/// 一个上下文段。
#[derive(Debug, Clone, Deserialize)]
pub struct KeymapSection {
    /// 上下文谓词，如 `"Input && vim_mode == normal"`。省略表示无上下文。
    #[serde(default)]
    pub context: Option<String>,
    /// 键序列 → action。action 可以是字符串（无参数）或
    /// `[名称, 参数对象]`（带参数）。
    #[serde(default)]
    pub bindings: Bindings,
}

/// 保留插入顺序的键值对列表。
///
/// `serde_json::Map` 默认按 key 排序（未启用 `preserve_order`），而键位表里
/// 同上下文下**后写者胜**，顺序会影响语义，所以不能借用它。
#[derive(Debug, Clone, Default)]
pub struct Bindings(pub Vec<(String, Value)>);

impl Bindings {
    pub fn iter(&self) -> impl Iterator<Item = &(String, Value)> {
        self.0.iter()
    }
}

impl<'de> Deserialize<'de> for Bindings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct BindingsVisitor;

        impl<'de> Visitor<'de> for BindingsVisitor {
            type Value = Bindings;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("键序列到 action 的映射")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Bindings, M::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry()? {
                    entries.push((key, value));
                }
                Ok(Bindings(entries))
            }
        }

        deserializer.deserialize_map(BindingsVisitor)
    }
}

/// 解析文档结构（不做 action 派发，不需要 `App`）。
pub fn sections_of(source: &str) -> Result<Vec<KeymapSection>, String> {
    serde_json_lenient::from_str(source).map_err(|err| format!("JSON 解析失败：{err}"))
}

/// 解析并构造绑定；action 通过 `App` 的注册表按名派发。
pub fn parse(source: &str, cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let sections = match sections_of(source) {
        Ok(sections) => sections,
        Err(err) => return (Vec::new(), vec![err]),
    };

    let mut bindings = Vec::new();
    let mut errors = Vec::new();

    for (index, section) in sections.into_iter().enumerate() {
        let context = section.context.clone();
        for (keystrokes, action) in section.bindings.iter() {
            match build_binding(keystrokes, action, context.as_deref(), cx) {
                Ok(binding) => bindings.push(binding),
                Err(err) => errors.push(format!("第 {} 段 `{keystrokes}`：{err}", index + 1)),
            }
        }
    }

    (bindings, errors)
}

/// 把 JSON 里的 action 值拆成（名称, 参数）。
pub fn split_action(action: &Value) -> Result<(String, Option<Value>), String> {
    match action {
        Value::String(name) => Ok((name.clone(), None)),
        Value::Array(items) => {
            let name = items
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "数组形式的首项必须是 action 名称字符串".to_string())?
                .to_string();
            Ok((name, items.get(1).cloned()))
        }
        other => Err(format!(
            "action 必须是字符串或 [名称, 参数]，实际是 {other}"
        )),
    }
}

/// 构造一条真实绑定：action 走 `App` 的按名派发注册表。
fn build_binding(
    keystrokes: &str,
    action: &Value,
    context: Option<&str>,
    cx: &App,
) -> Result<KeyBinding, String> {
    let (name, params) = split_action(action)?;

    let action = cx
        .build_action(&name, params)
        .map_err(|err| format!("无法构造 action `{name}`：{err}"))?;

    let context_predicate = context
        .map(|context| {
            gpui::KeyBindingContextPredicate::parse(context)
                .map(std::rc::Rc::new)
                .map_err(|err| format!("上下文 `{context}` 非法：{err}"))
        })
        .transpose()?;

    KeyBinding::load(
        keystrokes,
        action,
        context_predicate,
        false,
        None,
        cx.keyboard_mapper().as_ref(),
    )
    .map_err(|err| format!("键序列非法：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_parameterized_actions() {
        let source = r#"[
            { "context": "Input", "bindings": { "h": "vim::Left" } },
            { "bindings": { "secondary-enter": ["editor::Enter", { "secondary": true }] } }
        ]"#;
        let sections = sections_of(source).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].context.as_deref(), Some("Input"));
        assert!(sections[1].context.is_none());

        let entries: Vec<_> = sections[1].bindings.iter().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "secondary-enter");
        let (name, params) = split_action(&entries[0].1).unwrap();
        assert_eq!(name, "editor::Enter");
        assert_eq!(params.unwrap()["secondary"], serde_json::json!(true));
    }

    #[test]
    fn preserves_declaration_order() {
        // 顺序即语义（同上下文下后写者胜），不能被排序打乱。
        let source = r#"[{ "bindings": { "z": "vim::A", "a": "vim::B", "m": "vim::C" } }]"#;
        let sections = sections_of(source).unwrap();
        let keys: Vec<&str> = sections[0]
            .bindings
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, ["z", "a", "m"]);
    }

    #[test]
    fn malformed_json_reports_error() {
        assert!(sections_of("{ not json").is_err());
    }

    #[test]
    fn bad_action_shape_is_reported() {
        let source = r#"[{ "bindings": { "x": 42 } }]"#;
        let sections = sections_of(source).unwrap();
        let (_, action) = sections[0].bindings.iter().next().unwrap();
        assert!(split_action(action).is_err());
    }

    #[test]
    fn lenient_parser_accepts_comments() {
        let source = r#"[
            // 编辑器键位
            { "bindings": { "h": "vim::Left" } }
        ]"#;
        assert_eq!(sections_of(source).unwrap().len(), 1);
    }

    /// 多键序列（`g g`）里的每一段击键都必须可解析。
    ///
    /// 拆分由 `KeyBinding::load` 按空白完成，所以这里逐段校验。
    #[test]
    fn multi_keystroke_sequences_parse() {
        for source in ["g g", "secondary-k z", "ctrl-w h", "d w"] {
            for keystroke in source.split_whitespace() {
                assert!(
                    gpui::Keystroke::parse(keystroke).is_ok(),
                    "键序列 `{source}` 的 `{keystroke}` 段应可解析"
                );
            }
        }
    }

    /// `secondary` 修饰键必须被识别（macOS 上是 cmd，其它平台是 ctrl）。
    #[test]
    fn secondary_modifier_resolves_per_platform() {
        let parsed = gpui::Keystroke::parse("secondary-enter").unwrap();
        if cfg!(target_os = "macos") {
            assert!(parsed.modifiers.platform, "macOS 上 secondary 应为 cmd");
        } else {
            assert!(parsed.modifiers.control, "非 macOS 上 secondary 应为 ctrl");
        }
    }
}

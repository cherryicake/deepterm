use gpui::SharedString;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use crate::editor::syntax::languages;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageConfig {
    pub name: SharedString,
    pub language: tree_sitter::Language,
    pub injection_languages: Vec<SharedString>,
    pub highlights: SharedString,
    pub injections: SharedString,
    pub locals: SharedString,
}

impl LanguageConfig {
    pub fn new(
        name: impl Into<SharedString>,
        language: tree_sitter::Language,
        injection_languages: Vec<SharedString>,
        highlights: &str,
        injections: &str,
        locals: &str,
    ) -> Self {
        Self {
            name: name.into(),
            language,
            injection_languages,
            highlights: SharedString::from(highlights.to_string()),
            injections: SharedString::from(injections.to_string()),
            locals: SharedString::from(locals.to_string()),
        }
    }
}

/// Registry for code highlighter languages.
pub struct LanguageRegistry {
    languages: Mutex<HashMap<SharedString, LanguageConfig>>,
}

impl LanguageRegistry {
    /// Returns the singleton instance of the `LanguageRegistry` with default languages and themes.
    pub fn singleton() -> &'static LazyLock<LanguageRegistry> {
        static INSTANCE: LazyLock<LanguageRegistry> = LazyLock::new(|| LanguageRegistry {
            languages: Mutex::new(
                languages::Language::all()
                    .map(|language: languages::Language| {
                        (language.name().into(), language.config())
                    })
                    .collect::<HashMap<SharedString, LanguageConfig>>(),
            ),
        });
        &INSTANCE
    }

    /// Registers a new language configuration to the registry.
    pub fn register(&self, lang: &str, config: &LanguageConfig) {
        // 关键防御：后台高亮线程 panic 可能毒化锁，不得 unwrap 拖垮 UI，
        // 毒化时取内部数据继续服务，保证语法高亮永不因锁问题崩溃。
        self.languages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(lang.to_string().into(), config.clone());
    }

    /// Returns a list of all registered language names.
    pub fn languages(&self) -> Vec<SharedString> {
        self.languages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Returns the language configuration for the given language name.
    pub fn language(&self, name: &str) -> Option<LanguageConfig> {
        // Try to get by name first, there may have a custom language registered
        // Then try to get built-in language to support short language names, e.g. "js" for "javascript"
        let languages = self
            .languages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        languages
            .get(name)
            .or_else(|| languages.get(languages::Language::from_id(name).name()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::syntax::LanguageConfig;

    #[test]
    fn test_registry() {
        use super::LanguageRegistry;
        let registry = LanguageRegistry::singleton();

        registry.register(
            "foo",
            &LanguageConfig::new("foo", tree_sitter_json::LANGUAGE.into(), vec![], "", "", ""),
        );

        assert!(registry.language("foo").is_some());
        assert!(registry.language("rust").is_some());
        assert!(registry.language("rs").is_some());
        assert!(registry.language("javascript").is_some());
        assert!(registry.language("js").is_some());
    }
}

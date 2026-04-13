mod simple;
pub use simple::*;

use gpui::{HighlightStyle, Hsla};
use ropey::Rope;
use std::ops::Range;

/// Stub DiagnosticSet for compatibility
#[derive(Clone, Default)]
pub struct DiagnosticSet;

impl DiagnosticSet {
    pub fn new(_text: &Rope) -> Self {
        Self
    }
}

/// Stub SyntaxHighlighter alias pointing to our SimpleHighlighter
pub use simple::SimpleHighlighter as SyntaxHighlighter;

/// Stub HighlightTheme
#[derive(Clone, Default)]
pub struct HighlightTheme;

impl HighlightTheme {
    pub fn style(&self, _name: &str) -> Option<HighlightStyle> {
        None
    }
}

/// Stub LanguageRegistry
pub struct LanguageRegistry;

impl LanguageRegistry {
    pub fn singleton() -> &'static std::sync::LazyLock<LanguageRegistry> {
        static INSTANCE: std::sync::LazyLock<LanguageRegistry> =
            std::sync::LazyLock::new(|| LanguageRegistry);
        &INSTANCE
    }

    pub fn language(&self, _lang: &str) -> Option<LanguageConfig> {
        None
    }
}

/// Stub LanguageConfig
#[derive(Clone)]
pub struct LanguageConfig {
    pub language: tree_sitter::Language,
}

/// Stub Language enum
#[derive(Clone)]
pub enum Language {}

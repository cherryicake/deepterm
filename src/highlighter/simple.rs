use gpui::{HighlightStyle, Hsla};
use ropey::Rope;
use std::ops::Range;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::theme::{ActiveTheme, ThemeAppearance};

pub struct SimpleHighlighter {
    _language: String,
    parser: Parser,
    query: Option<Query>,
}

impl SimpleHighlighter {
    pub fn new(lang: &str) -> Self {
        let mut parser = Parser::new();
        let query = match lang {
            "rust" => Self::setup_language(
                &mut parser,
                tree_sitter_rust::LANGUAGE.into(),
                RUST_QUERY,
            ),
            "javascript" | "js" => Self::setup_language(
                &mut parser,
                tree_sitter_javascript::LANGUAGE.into(),
                JS_QUERY,
            ),
            "typescript" | "ts" => Self::setup_language(
                &mut parser,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                TS_QUERY,
            ),
            "python" | "py" => Self::setup_language(
                &mut parser,
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
            ),
            "json" => Self::setup_language(
                &mut parser,
                tree_sitter_json::LANGUAGE.into(),
                JSON_QUERY,
            ),
            "go" => Self::setup_language(
                &mut parser,
                tree_sitter_go::LANGUAGE.into(),
                GO_QUERY,
            ),
            "html" => Self::setup_language(
                &mut parser,
                tree_sitter_html::LANGUAGE.into(),
                HTML_QUERY,
            ),
            "css" => Self::setup_language(
                &mut parser,
                tree_sitter_css::LANGUAGE.into(),
                CSS_QUERY,
            ),
            "c" => Self::setup_language(
                &mut parser,
                tree_sitter_c::LANGUAGE.into(),
                C_QUERY,
            ),
            "cpp" | "c++" => Self::setup_language(
                &mut parser,
                tree_sitter_cpp::LANGUAGE.into(),
                CPP_QUERY,
            ),
            "bash" | "sh" => Self::setup_language(
                &mut parser,
                tree_sitter_bash::LANGUAGE.into(),
                BASH_QUERY,
            ),
            "markdown" | "md" => Self::setup_language(
                &mut parser,
                tree_sitter_md::LANGUAGE.into(),
                MD_QUERY,
            ),
            _ => None,
        };
        Self {
            _language: lang.to_string(),
            parser,
            query,
        }
    }

    fn setup_language(
        parser: &mut Parser,
        language: tree_sitter::Language,
        query: &str,
    ) -> Option<Query> {
        parser.set_language(&language).ok()?;
        Query::new(&language, query).ok()
    }

    pub fn update(&mut self, text: &Rope) {
        let text_str = text.to_string();
        let _tree = self.parser.parse(&text_str, None);
    }

    pub fn styles(
        &mut self,
        text: &Rope,
        cx: &gpui::App,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let query = match &self.query {
            Some(q) => q,
            None => return Vec::new(),
        };

        let theme = cx.theme().ui;
        let is_dark = cx.theme().appearance == ThemeAppearance::Dark;

        let text_str = text.to_string();
        let tree = match self.parser.parse(&text_str, None) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), text_str.as_bytes());

        let mut styles = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = query.capture_names()[capture.index as usize];
                let range = capture.node.byte_range();
                if let Some(style) = style_for_name(name, theme, is_dark) {
                    styles.push((range, style));
                }
            }
        }
        styles.sort_by(|a, b| a.0.start.cmp(&b.0.start));
        styles
    }
}

fn style_for_name(
    name: &str,
    theme: crate::theme::UiTheme,
    is_dark: bool,
) -> Option<HighlightStyle> {
    let color = match name {
        "comment" | "doc-comment" => Hsla::from(gpui::rgb(0x6A9955)),
        "string" => Hsla::from(gpui::rgb(0xCE9178)),
        "number" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xB5CEA8))
            } else {
                Hsla::from(gpui::rgb(0x098658))
            }
        }
        "function" => {
            if is_dark {
                Hsla::from(gpui::rgb(0xDCDCAA))
            } else {
                Hsla::from(gpui::rgb(0x795E26))
            }
        }
        "type" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4EC9B0))
            } else {
                Hsla::from(gpui::rgb(0x267F99))
            }
        }
        "field" => theme.text,
        "tag" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x569CD6))
            } else {
                Hsla::from(gpui::rgb(0x800000))
            }
        }
        "property" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x9CDCFE))
            } else {
                Hsla::from(gpui::rgb(0x001080))
            }
        }
        "constant" => {
            if is_dark {
                Hsla::from(gpui::rgb(0x4FC1FF))
            } else {
                Hsla::from(gpui::rgb(0x0070C1))
            }
        }
        _ => return None,
    };

    Some(HighlightStyle {
        color: Some(color),
        ..Default::default()
    })
}

const RUST_QUERY: &str = r#"
(line_comment) @comment
(block_comment) @comment
(string_literal) @string
(raw_string_literal) @string
(integer_literal) @number
(float_literal) @number
(boolean_literal) @number
(char_literal) @string
(function_item name: (identifier) @function)
(function_signature_item name: (identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
(primitive_type) @type
(field_identifier) @field
(macro_invocation macro: (identifier) @function)
"#;

const JS_QUERY: &str = r#"
(comment) @comment
(string) @string
(template_string) @string
(regex) @string
(number) @number
(true) @number
(false) @number
(class_declaration name: (identifier) @function)
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
(predefined_type) @type
"#;

const TS_QUERY: &str = r#"
(comment) @comment
(string) @string
(template_string) @string
(regex) @string
(number) @number
(true) @number
(false) @number
(class_declaration name: (identifier) @function)
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
(predefined_type) @type
"#;

const PYTHON_QUERY: &str = r#"
(comment) @comment
(string) @string
(integer) @number
(float) @number
(true) @number
(false) @number
(function_definition name: (identifier) @function)
(call function: (identifier) @function)
(type) @type
"#;

const JSON_QUERY: &str = r#"
(string) @string
(number) @number
(true) @number
(false) @number
(null) @constant
(pair key: (string) @property)
"#;

const GO_QUERY: &str = r#"
(comment) @comment
(string_literal) @string
(interpreted_string_literal) @string
(int_literal) @number
(float_literal) @number
(true) @number
(false) @number
(function_declaration name: (identifier) @function)
(method_declaration name: (field_identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
"#;

const HTML_QUERY: &str = r#"
(comment) @comment
(doctype) @tag
(tag_name) @tag
(attribute_name) @property
(quoted_attribute_value) @string
"#;

const CSS_QUERY: &str = r#"
(comment) @comment
(string_value) @string
(integer_value) @number
(float_value) @number
(property_name) @property
(class_name) @type
(id_name) @constant
(tag_name) @tag
"#;

const C_QUERY: &str = r#"
(comment) @comment
(string_literal) @string
(system_lib_string) @string
(number_literal) @number
(true) @number
(false) @number
(function_declarator declarator: (identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
(primitive_type) @type
(field_identifier) @field
"#;

const CPP_QUERY: &str = r#"
(comment) @comment
(string_literal) @string
(raw_string_literal) @string
(number_literal) @number
(true) @number
(false) @number
(function_declarator declarator: (identifier) @function)
(call_expression function: (identifier) @function)
(type_identifier) @type
(primitive_type) @type
(field_identifier) @field
"#;

const BASH_QUERY: &str = r#"
(comment) @comment
(string) @string
(number) @number
(function_definition name: (word) @function)
(command name: (command_name) @function)
(variable_name) @property
"#;

const MD_QUERY: &str = r#"
(code_span) @string
(fenced_code_block) @string
(link_text) @string
(link_destination) @property
"#;

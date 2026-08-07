//! Go-to-definition and hover definition underline.
//!
//! Ported from gpui-component's `input/lsp/definitions.rs`, adapted so the
//! actual LSP round-trip happens in the session task (which owns the async
//! client); the editor sends [`LspChange::Definition`](crate::components::input::lsp::LspChange)
//! through the change channel and updates [`HoverDefinition`] when the reply
//! arrives.

use anyhow::Result;
use gpui::{
    App, Context, HighlightStyle, MouseDownEvent, Task, UnderlineStyle, Window, px,
};
use ropey::Rope;
use std::{ops::Range, rc::Rc};

use crate::components::input::{GoToDefinition, InputState, RopeExt as _, element::TextElement};
use crate::theme::ActiveTheme;

use super::LspChange;

/// Definition provider.
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_definition
pub trait DefinitionProvider {
    /// textDocument/definition
    fn definitions(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Vec<lsp_types::LocationLink>>>;
}

#[derive(Clone, Default)]
pub(crate) struct HoverDefinition {
    /// The range of the symbol that triggered the hover.
    symbol_range: Range<usize>,
    pub(crate) locations: Rc<Vec<lsp_types::LocationLink>>,
    last_location: Option<(Range<usize>, Rc<Vec<lsp_types::LocationLink>>)>,
}

impl HoverDefinition {
    pub(crate) fn update(
        &mut self,
        symbol_range: Range<usize>,
        locations: Vec<lsp_types::LocationLink>,
    ) {
        self.clear();
        self.symbol_range = symbol_range;
        self.locations = Rc::new(locations);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        if !self.locations.is_empty() {
            self.last_location = Some((self.symbol_range.clone(), self.locations.clone()));
        }

        self.symbol_range = 0..0;
        self.locations = Rc::new(vec![]);
    }

    pub(crate) fn is_same(&self, offset: usize) -> bool {
        self.symbol_range.contains(&offset)
    }
}

impl InputState {
    /// Send a definition request to the LSP session and update the hover
    /// underline when the reply arrives.
    pub(crate) fn handle_hover_definition(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tx) = self.lsp.change_tx.clone() else {
            return;
        };

        if self.hover_definition.is_same(offset) {
            return;
        }

        let (reply_tx, reply_rx) = async_channel::bounded(1);
        if tx.try_send(LspChange::Definition {
            offset,
            reply: reply_tx,
        })
        .is_err()
        {
            return;
        }

        let mut symbol_range = self.text.word_range(offset).unwrap_or(offset..offset);
        let editor = cx.entity();
        self.lsp._hover_task = cx.spawn_in(window, async move |_, cx| {
            let locations = match reply_rx.recv().await {
                Ok(Ok(locations)) => locations,
                _ => Vec::new(),
            };

            let _ = editor.update(cx, |editor, cx| {
                if locations.is_empty() {
                    editor.hover_definition.clear();
                } else {
                    if let Some(location) = locations.first() {
                        if let Some(range) = location.origin_selection_range {
                            let start = editor.text.position_to_offset(&range.start);
                            let end = editor.text.position_to_offset(&range.end);
                            symbol_range = start..end;
                        }
                    }

                    editor
                        .hover_definition
                        .update(symbol_range.clone(), locations.clone());
                }
                cx.notify();
            });

            Ok(())
        });
    }

    pub(crate) fn on_action_go_to_definition(
        &mut self,
        _: &GoToDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.cursor();
        if let Some((symbol_range, locations)) = self.hover_definition.last_location.clone() {
            if !(symbol_range.start..=symbol_range.end).contains(&offset) {
                return;
            }

            if let Some(location) = locations.first().cloned() {
                self.go_to_definition(&location, window, cx);
            }
        }
    }

    /// Return true if handled (Cmd+click on a hovered symbol).
    pub(crate) fn handle_click_hover_definition(
        &mut self,
        event: &MouseDownEvent,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> bool {
        if !event.modifiers.secondary() {
            return false;
        }

        if self.hover_definition.is_empty() || !self.hover_definition.is_same(offset) {
            return false;
        }

        let Some(location) = self.hover_definition.locations.first().cloned() else {
            return false;
        };

        self.go_to_definition(&location, window, cx);

        true
    }

    pub(crate) fn go_to_definition(
        &mut self,
        location: &lsp_types::LocationLink,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let uri = location.target_uri.to_string();
        let external = uri.starts_with("https://") || uri.starts_with("http://");

        if external {
            cx.open_url(&uri);
            return;
        }

        // Same document (or an external file deepterm cannot open yet — the
        // LSP session is per-document, so a target in another file is shown
        // in a toast rather than silently ignored).
        if uri != self.lsp.document_uri() {
            log::info!("go-to-definition target in another file: {uri}");
            return;
        }

        let target_range = location.target_selection_range;
        let start = self.text.position_to_offset(&target_range.start);
        let end = self.text.position_to_offset(&target_range.end);

        self.move_to(start, None, cx);
        self.select_to(end, cx);
    }
}

impl TextElement {
    /// Highlight style for the hovered symbol (underline), composed into the
    /// render pipeline like the search matches.
    pub(crate) fn layout_hover_definition(&self, cx: &App) -> Option<(Range<usize>, HighlightStyle)> {
        let editor = self.state.read(cx);
        if !editor.mode.is_code_editor() {
            return None;
        }

        if editor.hover_definition.is_empty() {
            return None;
        }

        let mut highlight_style: HighlightStyle = cx
            .theme()
            .highlight_theme
            .link_text
            .map(|style| style.into())
            .unwrap_or_default();

        highlight_style.underline = Some(UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        });

        Some((
            editor.hover_definition.symbol_range.clone(),
            highlight_style,
        ))
    }
}

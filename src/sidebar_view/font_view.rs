use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render, SharedString,
    Window, div, px,
};

use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::macos_select::MacosSelect;
use crate::sidebar_view::font_settings::{
    FontState, clean_font_choice, format_font_choice, platform_default_font_family,
};
use crate::theme::ActiveTheme;

const WEIGHT_OPTIONS: &[(FontWeight, &str)] = &[
    (FontWeight::THIN, "Thin"),
    (FontWeight::EXTRA_LIGHT, "Extra Light"),
    (FontWeight::LIGHT, "Light"),
    (FontWeight::NORMAL, "Normal"),
    (FontWeight::MEDIUM, "Medium"),
    (FontWeight::SEMIBOLD, "Semibold"),
    (FontWeight::BOLD, "Bold"),
    (FontWeight::EXTRA_BOLD, "Extra Bold"),
    (FontWeight::BLACK, "Black"),
];

pub struct FontSettingsView {
    focus_handle: FocusHandle,
    font_select: Entity<MacosSelect>,
    weight_select: Entity<MacosSelect>,
    font_size_input: Entity<InputState>,
    line_height_input: Entity<InputState>,
    _subscriptions: Vec<gpui::Subscription>,
    font_size_valid: bool,
    line_height_valid: bool,
}

impl FontSettingsView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        let terminal_font_size = FontState::global(cx).settings.terminal_font_size;
        let terminal_line_height = FontState::global(cx).settings.terminal_line_height;
        let current_family = FontState::global(cx).settings.terminal_font_family.clone();

        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            let default_font = platform_default_font_family().to_string();
            let hardcoded = vec![
                "Menlo",
                "Monaco",
                "Consolas",
                "Courier New",
                "JetBrains Mono",
                "Fira Code",
                "Source Code Pro",
                "Hack",
                "DejaVu Sans Mono",
                "SF Mono",
            ];
            let default_fonts: Vec<SharedString> =
                std::iter::once(format_font_choice(&default_font, true).into())
                    .chain(
                        hardcoded
                            .into_iter()
                            .filter(|f| *f != default_font.as_str())
                            .map(|f| f.into()),
                    )
                    .collect();

            let selected_label = if current_family == default_font {
                format_font_choice(&default_font, true)
            } else {
                current_family.clone()
            };

            let font_select = MacosSelect::new(window, cx, default_fonts, selected_label);
            font_select.update(cx, |select, _cx| {
                select.on_select(move |family, _index, _window, cx| {
                    let family = clean_font_choice(&family).to_string();
                    FontState::update(cx, |settings| {
                        settings.terminal_font_family = family;
                    });
                });
            });

            let current_weight = FontState::global(cx).settings.terminal_font_weight;
            let weight_labels: Vec<SharedString> = WEIGHT_OPTIONS
                .iter()
                .map(|(_, label)| (*label).into())
                .collect();
            let selected_weight_label = WEIGHT_OPTIONS
                .iter()
                .find(|(w, _)| w.0 == current_weight)
                .map(|(_, label)| (*label).to_string())
                .unwrap_or("Normal".to_string());

            let weight_select = MacosSelect::new(window, cx, weight_labels, selected_weight_label);
            weight_select.update(cx, |select, _cx| {
                select.on_select(move |label, _index, _window, cx| {
                    if let Some((weight, _)) =
                        WEIGHT_OPTIONS.iter().find(|(_, l)| *l == label.as_ref())
                    {
                        FontState::update(cx, |settings| {
                            settings.terminal_font_weight = weight.0;
                        });
                    }
                });
            });

            let font_size_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("18")
                    .placeholder_font_size(px(14.0))
            });
            font_size_input.update(cx, |input, cx| {
                input.set_text(format!("{}", terminal_font_size), cx);
            });

            let line_height_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("1.3")
                    .placeholder_font_size(px(14.0))
            });
            line_height_input.update(cx, |input, cx| {
                input.set_text(format!("{}", terminal_line_height), cx);
            });

            let mut view = Self {
                focus_handle,
                font_select,
                weight_select,
                font_size_input,
                line_height_input,
                _subscriptions: Vec::new(),
                font_size_valid: true,
                line_height_valid: true,
            };

            view.load_system_fonts(window, cx);
            view.subscribe_to_inputs(cx);
            view
        })
    }

    fn subscribe_to_inputs(&mut self, cx: &mut Context<Self>) {
        let size_subscription = cx.subscribe(
            &self.font_size_input.clone(),
            |this: &mut Self, _, event: &InputEvent, cx| {
                let text = this.font_size_input.read(cx).value().to_string();
                let (valid, size) = match text.parse::<f32>() {
                    Ok(size) if size >= 6.0 && size <= 72.0 => (true, Some(size)),
                    _ => (false, None),
                };
                match event {
                    InputEvent::Change => {
                        if valid != this.font_size_valid {
                            this.font_size_valid = valid;
                            cx.notify();
                        }
                    }
                    InputEvent::Blur => {
                        if valid != this.font_size_valid {
                            this.font_size_valid = valid;
                            cx.notify();
                        }
                        if let Some(size) = size {
                            FontState::update(cx, |settings| {
                                settings.terminal_font_size = size;
                            });
                        }
                    }
                    _ => {}
                }
            },
        );
        self._subscriptions.push(size_subscription);

        let lh_subscription = cx.subscribe(
            &self.line_height_input.clone(),
            |this: &mut Self, _, event: &InputEvent, cx| {
                let text = this.line_height_input.read(cx).value().to_string();
                let (valid, lh) = match text.parse::<f32>() {
                    Ok(lh) if lh >= 0.8 && lh <= 3.0 => (true, Some(lh)),
                    _ => (false, None),
                };
                match event {
                    InputEvent::Change => {
                        if valid != this.line_height_valid {
                            this.line_height_valid = valid;
                            cx.notify();
                        }
                    }
                    InputEvent::Blur => {
                        if valid != this.line_height_valid {
                            this.line_height_valid = valid;
                            cx.notify();
                        }
                        if let Some(lh) = lh {
                            FontState::update(cx, |settings| {
                                settings.terminal_line_height = lh;
                            });
                        }
                    }
                    _ => {}
                }
            },
        );
        self._subscriptions.push(lh_subscription);
    }

    fn load_system_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.font_select.update(cx, |select, cx| {
            let default_font = platform_default_font_family().to_string();
            let mut fonts: Vec<_> = window
                .text_system()
                .all_font_names()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            fonts.sort();
            fonts.dedup();
            fonts.retain(|f| f != &default_font);
            let mut final_fonts: Vec<SharedString> =
                vec![format_font_choice(&default_font, true).into()];
            final_fonts.extend(fonts.into_iter().map(|s| s.into()));
            if !final_fonts.is_empty() {
                select.set_items(final_fonts, cx);
            }
        });
    }

    fn render_compact_row(&self, label: &str, control: impl IntoElement, cx: &App) -> gpui::Div {
        let theme = cx.theme().ui;
        let label = label.to_string();
        div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                div()
                    .w(px(80.))
                    .flex()
                    .child(div().text_xs().text_color(theme.text_muted).child(label)),
            )
            .child(control)
    }
}

impl Focusable for FontSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FontSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let error_color = gpui::rgb(0xef4444);

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_compact_row("终端字体", self.font_select.clone(), cx))
            .child(self.render_compact_row("字重", self.weight_select.clone(), cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div().w(px(80.)).flex().child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child("字体大小 (px)"),
                        ),
                    )
                    .child(
                        div()
                            .w(px(140.))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(EditorInput::new(&self.font_size_input))
                            .when(!self.font_size_valid, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(error_color)
                                        .child("Value must be between 6 and 72"),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div().w(px(80.)).flex().child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child("行高 (倍率)"),
                        ),
                    )
                    .child(
                        div()
                            .w(px(140.))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(EditorInput::new(&self.line_height_input))
                            .when(!self.line_height_valid, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(error_color)
                                        .child("Value must be between 0.8 and 3.0"),
                                )
                            }),
                    ),
            )
    }
}

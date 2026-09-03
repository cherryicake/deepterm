use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, px};

use crate::components::button::{Button, ButtonVariants, Sizable, Size as ButtonSize};
use crate::components::input::{EditorInput, InputState};
use crate::theme::ActiveTheme;

pub enum FolderDialogMode {
    Create,
    #[allow(dead_code)]
    Rename {
        id: String,
        old_name: String,
    },
}

pub struct FolderNameDialog {
    focus_handle: FocusHandle,
    input: Entity<InputState>,
    mode: FolderDialogMode,
    error_message: Option<String>,
    on_cancel: Option<Box<dyn Fn(&mut App) + 'static>>,
    on_confirm: Option<Box<dyn Fn(String, &mut App) + 'static>>,
}

impl FolderNameDialog {
    pub fn new(window: &mut Window, cx: &mut App, mode: FolderDialogMode) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let (placeholder, initial) = match &mode {
                FolderDialogMode::Create => ("文件夹名称".to_string(), String::new()),
                FolderDialogMode::Rename { old_name, .. } => (old_name.clone(), old_name.clone()),
            };
            let input = cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .placeholder(placeholder)
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0));
                if !initial.is_empty() {
                    state.set_text(&initial, cx);
                    state.selected_range = (0..initial.len()).into();
                }
                state
            });
            // auto focus handled by track_focus in render
            Self {
                focus_handle,
                input,
                mode,
                error_message: None,
                on_cancel: None,
                on_confirm: None,
            }
        })
    }

    pub fn on_cancel<F>(&mut self, cb: F)
    where
        F: Fn(&mut App) + 'static,
    {
        self.on_cancel = Some(Box::new(cb));
    }

    pub fn on_confirm<F>(&mut self, cb: F)
    where
        F: Fn(String, &mut App) + 'static,
    {
        self.on_confirm = Some(Box::new(cb));
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let trimmed = self.input.read(cx).value().trim().to_string();
        if trimmed.is_empty() {
            self.error_message = Some("文件夹名称不能为空".to_string());
            cx.notify();
            return;
        }
        if trimmed.chars().count() > 20 {
            self.error_message = Some("文件夹名称不能超过20字符".to_string());
            cx.notify();
            return;
        }
        self.error_message = None;
        if let Some(callback) = self.on_confirm.take() {
            callback(trimmed, cx);
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_cancel.take() {
            cb(cx);
        }
    }

    pub fn set_error(&mut self, err: String, cx: &mut Context<Self>) {
        self.error_message = Some(err);
        cx.notify();
    }
}

impl Focusable for FolderNameDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FolderNameDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let title = match &self.mode {
            FolderDialogMode::Create => "新建文件夹",
            FolderDialogMode::Rename { .. } => "重命名文件夹",
        };
        let subtitle = match &self.mode {
            FolderDialogMode::Create => "输入文件夹名称（1-20字符）",
            FolderDialogMode::Rename { .. } => "修改文件夹名称",
        };

        div()
            .id("folder-dialog")
            .track_focus(&self.focus_handle)
            .w(px(400.))
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(div().text_sm().text_color(theme.text_muted).child(subtitle))
            .when_some(self.error_message.clone(), |this, err| {
                this.child(div().text_sm().text_color(theme.error).child(err))
            })
            .child(EditorInput::new(&self.input))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .justify_end()
                    .child(
                        Button::new("folder-cancel")
                            .label("取消")
                            .rounded(px(10.))
                            .with_size(ButtonSize::Small)
                            .px(px(14.))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Button::new("folder-confirm")
                            .label("确定")
                            .success()
                            .rounded(px(10.))
                            .with_size(ButtonSize::Small)
                            .px(px(14.))
                            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                    ),
            )
    }
}

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, Context, CursorStyle, Element, ElementId, Entity,
    GlobalElementId, IntoElement, KeyBinding, ParentElement, Pixels, Render, SharedString, Styled,
    WeakEntity, Window, actions, div, px,
};

use crate::components::input::{EditorInput, InputState};
use crate::components::tab_bar::{DraggedTab, TabBar, TabItem};
use crate::ssh::SshConnectionConfig;
use crate::theme::ActiveTheme;

const EDITOR_DEFAULT_WIDTH: f32 = 680.0;
const EDITOR_MIN_WIDTH: f32 = 200.0;


actions!(editor_panel, [CloseActiveEditorTab]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseActiveEditorTab, Some("EditorPanel")),
    ]);
}

struct EditorFile {
    id: SharedString,
    #[allow(dead_code)]
    path: String,
    #[allow(dead_code)]
    label: String,
    editor_state: Entity<InputState>,
}

pub struct Editor {
    tab_bar: Entity<TabBar>,
    open_files: Vec<EditorFile>,
    active_file_id: Option<SharedString>,
    width: f32,
    resize_drag_x: Option<Pixels>,
    app_window: Option<WeakEntity<crate::AppWindow>>,
    tab_bar_pad_epoch: u32,
    tab_bar_pad_start: f32,
    tab_bar_pad_end: f32,
    tab_bar_pad_duration_ms: u64,
    content_left_padding: f32,
    content_bottom_padding: f32,
}

impl Editor {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tab_bar = TabBar::new("editor-tab-bar", cx);

        let mut this = Self {
            tab_bar,
            open_files: Vec::new(),
            active_file_id: None,
            width: EDITOR_DEFAULT_WIDTH,
            resize_drag_x: None,
            app_window: None,
            tab_bar_pad_epoch: 0,
            tab_bar_pad_start: 6.0,
            tab_bar_pad_end: 6.0,
            tab_bar_pad_duration_ms: 0,
            content_left_padding: 0.0,
            content_bottom_padding: 0.0,
        };

        this.setup_tab_bar_handlers(cx);
        this
    }

    pub fn bind_app_window(&mut self, app_window: WeakEntity<crate::AppWindow>) {
        self.app_window = Some(app_window);
    }

    pub fn is_empty(&self) -> bool {
        self.open_files.is_empty()
    }

    pub fn is_dragging(&self) -> bool {
        self.resize_drag_x.is_some()
    }

    pub fn start_resize(&mut self, pos: Pixels) {
        self.resize_drag_x = Some(pos);
    }

    pub fn update_resize(&mut self, pos: Pixels) {
        if let Some(start) = self.resize_drag_x {
            let delta: f32 = (pos - start).into();
            self.width = (self.width + delta).max(EDITOR_MIN_WIDTH);
            self.resize_drag_x = Some(pos);
        }
    }

    pub fn end_resize(&mut self) {
        self.resize_drag_x = None;
    }

    pub fn set_paddings(
        &mut self,
        content_left: f32,
        content_bottom: f32,
        cx: &mut Context<Self>,
    ) {
        if (self.content_left_padding - content_left).abs() > 0.5
            || (self.content_bottom_padding - content_bottom).abs() > 0.5
        {
            self.content_left_padding = content_left;
            self.content_bottom_padding = content_bottom;
            cx.notify();
        }
    }

    pub fn set_tab_bar_pad_animation(
        &mut self,
        epoch: u32,
        start: f32,
        end: f32,
        duration_ms: u64,
        cx: &mut Context<Self>,
    ) {
        if self.tab_bar_pad_epoch != epoch
            || (self.tab_bar_pad_start - start).abs() > 0.5
            || (self.tab_bar_pad_end - end).abs() > 0.5
            || self.tab_bar_pad_duration_ms != duration_ms
        {
            self.tab_bar_pad_epoch = epoch;
            self.tab_bar_pad_start = start;
            self.tab_bar_pad_end = end;
            self.tab_bar_pad_duration_ms = duration_ms;
            cx.notify();
        }
    }

    pub fn notify_app_window(&self, cx: &mut App) {
        if let Some(aw) = &self.app_window {
            aw.update(cx, |_, cx| cx.notify()).ok();
        }
    }

    pub fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = format!("editor-{}", path_str);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        self.open_file_with_content(
            tab_id,
            path_str,
            content,
            path,
            None,
            window,
            cx,
            true,
        );
    }

    pub fn open_remote_file(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, true);
    }

    pub fn open_remote_file_no_notify(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_file_inner(path, ssh_config, window, cx, false);
    }

    fn open_remote_file_inner(
        &mut self,
        path: String,
        ssh_config: SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        let tab_id = format!("editor-remote-{}", path);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let content = match crate::ssh::sftp_fs::read_text_preview(&ssh_config, &path) {
            Ok(Some(text)) => text,
            Ok(None) => {
                log::warn!("远程文件为二进制或空，无法打开编辑: {}", path);
                String::new()
            }
            Err(e) => {
                log::warn!("读取远程文件失败 {}: {}", path, e);
                String::new()
            }
        };

        self.open_file_with_content(
            tab_id,
            path.clone(),
            content,
            PathBuf::from(&path),
            Some(ssh_config),
            window,
            cx,
            notify_app,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn open_file_with_content(
        &mut self,
        tab_id: String,
        path_str: String,
        content: String,
        path: PathBuf,
        _ssh_config: Option<SshConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
        notify_app: bool,
    ) {
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path_str)
            .to_string();
        let language = guess_language_from_extension(&path);

        let editor_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true);
            state.set_value(&content, window, cx);
            state
        });

        self.open_files.push(EditorFile {
            id: tab_id.clone().into(),
            path: path_str,
            label: label.clone(),
            editor_state,
        });

        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.add_tab(
                TabItem::new(&tab_id, &label)
                    .with_icon(Some(crate::assets::Icon::File)),
                cx,
            );
        });

        self.switch_to_file(&tab_id, cx);
        cx.notify();
        if notify_app {
            self.notify_app_window(cx);
        }
    }

    pub fn open_file_no_notify(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let tab_id = format!("editor-{}", path_str);

        if self.open_files.iter().any(|f| f.id == tab_id) {
            self.switch_to_file(&tab_id, cx);
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        self.open_file_with_content(
            tab_id,
            path_str,
            content,
            path,
            None,
            window,
            cx,
            false,
        );
    }

    fn switch_to_file(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active_file_id = Some(id.into());
        self.tab_bar.update(cx, |tab_bar, cx| {
            tab_bar.set_active_tab(id, cx);
        });
        cx.notify();
    }

    fn close_file(&mut self, id: &str, cx: &mut Context<Self>) {
        let pos = self.open_files.iter().position(|f| f.id.as_ref() == id);
        if let Some(index) = pos {
            self.open_files.remove(index);
            self.tab_bar.update(cx, |tab_bar, cx| {
                tab_bar.remove_tab(id, cx);
            });

            let was_active = self.active_file_id.as_ref().map(|s| s.as_ref()) == Some(id);
            if was_active {
                let next_id = if index < self.open_files.len() {
                    Some(self.open_files[index].id.clone())
                } else if !self.open_files.is_empty() {
                    Some(self.open_files[self.open_files.len() - 1].id.clone())
                } else {
                    None
                };
                if let Some(next) = next_id {
                    self.switch_to_file(&next, cx);
                } else {
                    self.active_file_id = None;
                }
            }
        }
        cx.notify();
        self.notify_app_window(cx);
    }

    fn handle_tab_reorder(&mut self, dragged_tab: DraggedTab, to_index: usize, cx: &mut Context<Self>) {
        let dragged_id = dragged_tab.tab_id.as_ref();
        let from_index = self.open_files.iter().position(|f| f.id.as_ref() == dragged_id);
        if let Some(from) = from_index {
            if from != to_index {
                let file = self.open_files.remove(from);
                let insert_idx = if to_index > from { to_index.saturating_sub(1) } else { to_index };
                let capped = insert_idx.min(self.open_files.len());
                self.open_files.insert(capped, file);
                cx.notify();
                self.notify_app_window(cx);
            }
        }
    }

    fn setup_tab_bar_handlers(&mut self, cx: &mut Context<Self>) {
        let editor = cx.entity();
        self.tab_bar.update(cx, |tab_bar, _cx| {
            tab_bar.on_tab_click({
                let editor = editor.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    editor.update(cx, |_, cx| {
                        cx.defer_in(window, move |this, _window, cx| {
                            this.switch_to_file(&id, cx);
                        });
                    });
                }
            });

            tab_bar.on_tab_close({
                let editor = editor.clone();
                move |tab_id, window, cx| {
                    let id = tab_id.to_string();
                    editor.update(cx, |_, cx| {
                        cx.defer_in(window, move |this, _window, cx| {
                            this.close_file(&id, cx);
                        });
                    });
                }
            });

            tab_bar.on_tab_reorder({
                let editor = editor.clone();
                move |dragged, to_index, cx| {
                    editor.update(cx, |this, cx| {
                        this.handle_tab_reorder(dragged, to_index, cx);
                    });
                }
            });
        });
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.open_files.is_empty();
        let theme = cx.theme().ui;

        let inner = self.tab_bar.clone();

        let tab_bar: gpui::AnyElement = if is_empty {
            div()
                .w(px(0.0))
                .h(px(44.0))
                .overflow_hidden()
                .child(inner)
                .into_any_element()
        } else {
            let epoch = self.tab_bar_pad_epoch;
            let start = self.tab_bar_pad_start;
            let end = self.tab_bar_pad_end;
            let dur = self.tab_bar_pad_duration_ms;

            if epoch > 0 {
                let id = ("editor-tab-pad", epoch);
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .child(inner)
                    .with_animation(
                        id,
                        Animation::new(Duration::from_millis(dur))
                            .with_easing(gpui::ease_in_out),
                        move |this, delta| this.pl(px(start + (end - start) * delta)),
                    )
                    .into_any_element()
            } else if end > 0.0 {
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .pl(px(end))
                    .child(inner)
                    .into_any_element()
            } else {
                div()
                    .h(px(44.0))
                    .w(px((self.width - 8.0).max(0.0)))
                    .pl(px(6.0))
                    .child(inner)
                    .into_any_element()
            }
        };

        let content: gpui::AnyElement = if !is_empty {
            if let Some(active_id) = &self.active_file_id {
                if let Some(file) = self.open_files.iter().find(|f| f.id == *active_id) {
                    div()
                        .w_full()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .bg(theme.app_background)
                        .when(self.content_left_padding > 0.0, |this| {
                            this.pl(px(self.content_left_padding))
                        })
                        .when(self.content_bottom_padding > 0.0, |this| {
                            this.pb(px(self.content_bottom_padding))
                        })
                        .child(EditorInput::new(&file.editor_state))
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            } else {
                div().into_any_element()
            }
        } else {
            div().into_any_element()
        };

        let resize_handle: gpui::AnyElement = if !is_empty {
            div()
                .absolute()
                .right(px(0.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .w(px(2.0))
                .cursor_col_resize()
                .bg(theme.border.opacity(0.5))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.start_resize(event.position.x);
                        cx.notify();
                        this.notify_app_window(cx);
                    }),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        div()
            .key_context("EditorPanel")
            .on_action(cx.listener(|this, _: &CloseActiveEditorTab, _window, cx| {
                if let Some(active_id) = this.active_file_id.clone() {
                    this.close_file(active_id.as_ref(), cx);
                }
            }))
            .w(px(if is_empty { 0.0 } else { self.width }))
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .items_start()
            .child(tab_bar)
            .child(content)
            .child(resize_handle)
            .when(!is_empty, |this| {
                this.child(EditorResizeElement {
                    editor: cx.entity().downgrade(),
                })
            })
            .into_any_element()
    }
}

fn guess_language_from_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "json" => "json",
            "py" => "python",
            "go" => "go",
            "cpp" | "cc" | "cxx" | "hpp" => "cpp",
            "c" | "h" => "c",
            "html" | "htm" | "vue" | "svelte" => "html",
            "css" => "css",
            "scss" | "sass" => "scss",
            "md" | "markdown" => "markdown",
            "sh" | "bash" | "zsh" => "bash",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "xml" => "xml",
            "sql" => "sql",
            "rb" => "ruby",
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "swift" => "swift",
            "php" => "php",
            "pl" | "pm" => "perl",
            "lua" => "lua",
            "zig" => "zig",
            "gradle" => "groovy",
            _ => "text",
        })
        .unwrap_or("text")
        .to_string()
}

struct EditorResizeElement {
    editor: WeakEntity<Editor>,
}

impl Element for EditorResizeElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint_state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };

        if editor.read(cx).is_dragging() {
            window.set_window_cursor_style(CursorStyle::ResizeColumn);
        }

        let ed = editor.clone();
        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, _, _, cx| {
            ed.update(cx, |this, cx| {
                if this.is_dragging() {
                    this.update_resize(event.position.x);
                    cx.notify();
                    this.notify_app_window(cx);
                }
            });
        });

        let ed = editor.clone();
        window.on_mouse_event(move |_: &gpui::MouseUpEvent, _, _, cx| {
            ed.update(cx, |this, cx| {
                if this.is_dragging() {
                    this.end_resize();
                    cx.notify();
                    this.notify_app_window(cx);
                }
            });
        });
    }
}

impl IntoElement for EditorResizeElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

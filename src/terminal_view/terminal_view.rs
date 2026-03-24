//! Terminal view component - Full featured

use std::time::Duration;

use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EntityInputHandler, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString, Task, UTF16Selection,
    Window, actions, div, point, px,
};
use gpui::{KeyBinding, prelude::*};

/// Events emitted by TerminalView
#[derive(Clone, Debug)]
pub enum Event {
    /// Terminal process exited - container should close this view
    CloseTerminal,
}

impl EventEmitter<Event> for TerminalView {}

/// ASCII control character for Ctrl+V (SYN - Synchronous Idle)
pub const CTRL_V: u8 = 0x16;

/// IME state for tracking marked (pre-edit) text during composition
pub struct ImeState {
    pub marked_text: String,
}

use alacritty_terminal::term::TermMode;

use crate::ShowSshPicker;
use crate::ssh::SshConnectionConfig;
use crate::ssh_view::SshConnectView;
use crate::terminal::mappings::keys::to_esc_str;
use crate::terminal::terminal::{Event as TerminalEvent, Terminal};
use crate::terminal_view::terminal_element::{RowLayoutEntry, TerminalElement};
use crate::terminal_view::terminal_scrollbar::TerminalScrollbar;

// Define terminal actions
actions!(terminal, [Copy, Paste, Clear, SelectAll]);
const CONTEXT: &str = "Terminal";
pub(crate) fn init(cx: &mut App) {
    // Bind keyboard shortcuts
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-k", ShowSshPicker, Some(CONTEXT)),
    ]);
}

/// 广播输入回调类型 - 用于将输入广播给其他终端
pub type BroadcastInputCallback = Box<dyn Fn(&[u8])>;

pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
    cursor_visible: bool,
    title: SharedString,
    selecting: bool,
    /// IME composition state for CJK input methods
    ime_state: Option<ImeState>,
    /// 广播输入回调 - 当终端输入需要广播给其他终端时使用
    broadcast_callback: Option<BroadcastInputCallback>,
    /// Pending scrollbar hide task - prevents spawning a new timer every render frame
    scrollbar_hide_task: Option<Task<()>>,
    /// SSH 连接进度视图 - 在连接建立期间覆盖终端内容
    ssh_connect_view: Option<Entity<SshConnectView>>,
    /// Per-row layout cache: indexed by viewport row (0 = top).
    /// Clean rows are reused across frames; dirty rows are recomputed and the cache is updated.
    pub row_layout_cache: Vec<Option<RowLayoutEntry>>,
}

impl TerminalView {
    /// 创建新的本地终端
    pub fn new(cx: &mut App) -> Entity<Self> {
        let terminal = Terminal::new(cx, dirs::home_dir());
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let terminal_clone = terminal.clone();

            cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, _terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                },
            )
            .detach();

            let mut this = Self {
                terminal: terminal_clone,
                focus_handle,
                cursor_visible: true,
                title: "Terminal".into(),
                selecting: false,
                ime_state: None,
                broadcast_callback: None,
                scrollbar_hide_task: None,
                ssh_connect_view: None,
                row_layout_cache: Vec::new(),
            };
            this.update_title(cx);
            this
        })
    }

    /// 使用指定命令创建新的本地终端
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `command` - 要执行的命令（如 "powershell.exe", "cmd.exe", "wsl.exe"）
    /// * `args` - 命令参数
    pub fn new_with_command(cx: &mut App, command: &str, args: &[String]) -> Entity<Self> {
        let command_string = command.to_string();
        let args_vec: Vec<String> = args.to_vec();
        // WSL 不传递工作目录，让它默认进入 home 目录
        let working_dir = if command.contains("wsl") {
            None
        } else {
            dirs::home_dir()
        };
        let terminal = Terminal::new_with_command(cx, working_dir, &command_string, &args_vec);
        let focus_handle = cx.focus_handle();

        cx.new(|cx| {
            let terminal_clone = terminal.clone();

            cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, _terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                },
            )
            .detach();

            let mut this = Self {
                terminal: terminal_clone,
                focus_handle,
                cursor_visible: true,
                title: command_string.into(),
                selecting: false,
                ime_state: None,
                broadcast_callback: None,
                scrollbar_hide_task: None,
                ssh_connect_view: None,
                row_layout_cache: Vec::new(),
            };
            this.update_title(cx);
            this
        })
    }

    /// 创建新的 SSH 远程终端
    ///
    /// 优先使用 russh 连接，如果失败则回退到 PTY + ssh 命令方式
    ///
    /// # 参数
    ///
    /// * `cx` - GPUI 上下文
    /// * `ssh_config` - SSH 连接配置
    pub fn new_ssh(cx: &mut App, ssh_config: &SshConnectionConfig) -> Entity<Self> {
        log::info!("=== 创建 SSH 远程终端（优先 russh，失败回退到 ssh 命令）===");
        log::info!(
            "目标: {}@{}:{}",
            ssh_config.username,
            ssh_config.host,
            ssh_config.port
        );

        let focus_handle = cx.focus_handle();
        let ssh_config_clone = ssh_config.clone();

        cx.new(|cx| {
            // 首先尝试 russh 方式创建终端
            let terminal = Terminal::new_ssh(cx, &ssh_config_clone);
            let terminal_clone = terminal.clone();

            // 创建 SSH 连接进度视图（在连接成功前覆盖在终端上方）
            let connect_view = cx.new(|cx| SshConnectView::new(ssh_config_clone.clone(), cx));

            cx.subscribe(
                &terminal_clone,
                |this: &mut TerminalView, terminal, event: &TerminalEvent, cx| match event {
                    TerminalEvent::TitleChanged => {
                        this.update_title(cx);
                    }
                    TerminalEvent::Wakeup | TerminalEvent::SelectionsChanged => {
                        // 当 SSH 连接完成时更新进度视图
                        if let Some(connect_view) = this.ssh_connect_view.clone() {
                            enum SshOutcome {
                                Connected,
                                Failed(String),
                                Pending,
                            }
                            let outcome = {
                                let term = terminal.read(cx);
                                if term.is_ssh_connected() {
                                    SshOutcome::Connected
                                } else if term.is_ssh_failed() {
                                    SshOutcome::Failed(
                                        term.ssh_error_message()
                                            .unwrap_or_else(|| "Unknown error".to_string()),
                                    )
                                } else {
                                    SshOutcome::Pending
                                }
                            };
                            match outcome {
                                SshOutcome::Connected => {
                                    // 先标记为已连接，然后移除视图
                                    connect_view.update(cx, |view, cx| view.set_connected(cx));
                                    // 延迟移除连接视图，让用户看到完成状态
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(Duration::from_millis(500))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            this.ssh_connect_view = None;
                                            cx.notify();
                                        })
                                        .ok();
                                    })
                                    .detach();
                                }
                                SshOutcome::Failed(msg) => {
                                    connect_view.update(cx, |view, cx| view.set_failed(msg, cx));
                                }
                                SshOutcome::Pending => {}
                            }
                        }
                        cx.notify();
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::CloseTerminal => {
                        cx.emit(Event::CloseTerminal);
                    }
                },
            )
            .detach();

            let mut this = Self {
                terminal: terminal_clone,
                focus_handle,
                cursor_visible: true,
                title: ssh_config_clone.host_label().into(),
                selecting: false,
                ime_state: None,
                broadcast_callback: None,
                scrollbar_hide_task: None,
                ssh_connect_view: Some(connect_view),
                row_layout_cache: Vec::new(),
            };
            this.update_title(cx);
            this
        })
    }

    /// 更新终端标题
    fn update_title(&mut self, cx: &mut Context<Self>) {
        self.title = self.terminal.read(cx).remote_info().map_or_else(
            || "Terminal".into(),
            |(host, username, port)| {
                if port == 22 {
                    format!("{}@{}", username, host)
                } else {
                    format!("{}@{}:{}", username, host, port)
                }
                .into()
            },
        );
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// 设置广播输入回调
    /// 当终端有输入时，会调用此回调将输入数据广播给其他终端
    pub fn set_broadcast_callback(&mut self, callback: BroadcastInputCallback) {
        self.broadcast_callback = Some(callback);
    }

    /// 清除广播输入回调
    pub fn clear_broadcast_callback(&mut self) {
        self.broadcast_callback = None;
    }

    /// 检查是否是远程终端
    pub fn is_remote(&self, cx: &App) -> bool {
        self.terminal.read(cx).is_remote()
    }

    /// Check if the scrollbar should be visible
    /// Scrollbar is visible when:
    /// 1. There's scrollable content (total_lines > viewport_lines)
    /// 2. User is dragging the scrollbar, OR
    /// 3. Less than 2 seconds have passed since the last scroll
    pub fn should_show_scrollbar(&self, cx: &App) -> bool {
        let term = self.terminal.read(cx);
        let content = term.last_content();
        let total_lines = content.total_lines;
        let viewport_lines = content.viewport_lines;

        // No scrollbar if no scrollable content
        if total_lines <= viewport_lines {
            return false;
        }

        // Always show scrollbar while dragging
        if term.scrollbar_drag_offset.get().is_some() {
            return true;
        }

        // Show scrollbar if scrolled within the last 2 seconds
        let last_scroll = term.last_scroll_time.get();
        let elapsed = last_scroll.elapsed();
        elapsed < Duration::from_secs(2)
    }

    // IME handling methods for CJK input methods
    /// Sets the marked (pre-edit) text from the IME.
    /// During composition, this is called repeatedly to update the candidate text.
    /// The text is NOT sent to the terminal - it's only for display purposes.
    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Returns true if IME composition is currently active.
    pub fn has_ime_composition(&self) -> bool {
        self.ime_state
            .as_ref()
            .map_or(false, |s| !s.marked_text.is_empty())
    }

    /// Returns a reference to the current IME state, if any.
    pub fn ime_state(&self) -> Option<&ImeState> {
        self.ime_state.as_ref()
    }

    /// Clears the marked (pre-edit) text state.
    /// Called when composition is cancelled or completed.
    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commits (sends) the given text to the PTY.
    /// This is called when the user confirms the input (e.g., pressing Space to select).
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.write_with_broadcast(text.as_bytes(), cx);
        }
    }

    /// 写入数据到终端，并可选地广播给其他终端
    fn write_with_broadcast(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _| {
            terminal.write(data);
        });
        // 如果有广播回调，调用它来广播输入
        if let Some(ref callback) = self.broadcast_callback {
            callback(data);
        }
    }

    /// 将拖拽的文件路径写入终端
    /// 参考 alacritty 的实现，将路径格式化后粘贴到终端
    fn add_paths_to_terminal(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        use itertools::Itertools;

        // 构建路径字符串，参考 alacritty 的实现：path + " "
        let text = paths
            .iter()
            .map(|path| {
                let path_str = path.to_string_lossy().into_owned();
                format!("{path_str} ")
            })
            .join("");

        // 过滤掉 \x1b 和 \x03，防止恶意输入破坏终端转义序列
        let filtered = text.replace(['\x1b', '\x03'], "");

        self.terminal.update(cx, |terminal, _| {
            let bracketed = terminal
                .last_content()
                .mode
                .contains(TermMode::BRACKETED_PASTE);

            if bracketed {
                // 使用 bracketed paste 模式
                terminal.write(b"\x1b[200~");
                terminal.write(filtered.as_bytes());
                terminal.write(b"\x1b[201~");
            } else {
                // 普通模式
                terminal.write(filtered.as_bytes());
            }
        });
    }

    // Actions
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.copy(cx);
        });
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        // Check if clipboard contains an image
        // If so, forward Ctrl+V to let the terminal application handle it
        if clipboard.entries.iter().any(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => !image.bytes.is_empty(),
            _ => false,
        }) {
            self.forward_ctrl_v(cx);
            return;
        }

        // Otherwise, paste text content
        if let Some(text) = clipboard.text() {
            self.write_with_broadcast(text.as_bytes(), cx);
        }
    }

    /// Forward Ctrl+V to the terminal as raw input
    /// This lets TUI applications handle clipboard images themselves
    fn forward_ctrl_v(&mut self, cx: &mut Context<Self>) {
        self.write_with_broadcast(&[CTRL_V], cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.clear();
        });
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.select_all();
        });
        cx.notify();
    }

    // Scrolling
    pub fn scroll_line_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_up();
        });
        cx.notify();
    }

    pub fn scroll_line_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_line_down();
        });
        cx.notify();
    }

    pub fn scroll_page_up(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_up();
        });
        cx.notify();
    }

    pub fn scroll_page_down(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_page_down();
        });
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_top();
        });
        cx.notify();
    }

    pub fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.scroll_to_bottom();
        });
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If user has scrolled up, pressing any key should scroll back to bottom
        let was_scrolled = self.terminal.update(cx, |terminal, _cx| {
            let display_offset = terminal.last_content().display_offset;
            if display_offset > 0 {
                terminal.scroll_to_bottom();
                true
            } else {
                false
            }
        });

        if was_scrolled {
            // Scroll back to bottom, then process the key
            cx.notify();
        }

        let mut input_bytes: Option<Vec<u8>> = None;
        let handled = self.terminal.update(cx, |terminal, _cx| {
            let mode = terminal.last_content().mode;
            if let Some(bytes) = to_esc_str(&event.keystroke, &mode, false) {
                input_bytes = Some(bytes.to_vec());
                terminal.write(&bytes);
                true
            } else {
                false
            }
        });

        // 如果有输入数据且广播回调已设置，调用广播回调
        if let Some(ref bytes) = input_bytes {
            if let Some(ref callback) = self.broadcast_callback {
                callback(bytes);
            }
        }

        if handled {
            // Stop propagation only if we handled the key
            cx.stop_propagation();
        }
        // Otherwise, let the event propagate to EntityInputHandler
        // which will handle regular character input (including uppercase letters)
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    /// Called when text is committed (e.g., user pressed Space to confirm CJK input).
    /// This sends the text to the terminal and clears any marked text state.
    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_marked_text(cx);
        self.commit_text(text, cx);
    }

    /// Called during IME composition to update the pre-edit text.
    /// This does NOT send text to the terminal - it only updates the marked text state
    /// for display purposes (showing the candidate text while typing pinyin).
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_marked_text(new_text.to_string(), cx);
    }

    /// Called when IME composition is cancelled (e.g., user pressed Escape).
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Only return bounds during active IME composition
        if self.ime_state.is_none() {
            return None;
        }
        self.terminal.update(cx, |terminal, _cx| {
            let content = terminal.last_content();
            let bounds = terminal.terminal_bounds();

            let cursor_line_in_viewport = content.cursor.line.0 + content.display_offset as i32;

            let viewport_lines = bounds.num_lines() as i32;
            if cursor_line_in_viewport < 0 || cursor_line_in_viewport >= viewport_lines {
                return None;
            }

            let cursor_x = element_bounds.left()
                + px(4.0)
                + px((content.cursor.column.0 as f32 * bounds.cell_width.as_f32()).floor());
            let cursor_y = element_bounds.top()
                + px(cursor_line_in_viewport as f32 * bounds.line_height.as_f32());

            Some(Bounds {
                origin: point(cursor_x, cursor_y),
                size: gpui::Size {
                    width: bounds.cell_width,
                    height: bounds.line_height,
                },
            })
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self.terminal.clone();
        let focus_handle = self.focus_handle.clone();
        let view_entity = cx.entity().clone();
        let is_focused = focus_handle.is_focused(window);
        let show_scrollbar = self.should_show_scrollbar(cx);

        // Schedule a refresh to hide the scrollbar after it fades out.
        // Only spawn a new timer task if one isn't already running, to avoid
        // accumulating one task per frame (~60/s) while the scrollbar is visible.
        if show_scrollbar && self.scrollbar_hide_task.is_none() {
            self.scrollbar_hide_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                this.update(cx, |this, cx| {
                    this.scrollbar_hide_task = None;
                    cx.notify();
                })
                .ok();
            }));
        }

        div()
            .id("terminal-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(|this, _: &Copy, _window, cx| {
                this.copy(cx);
            }))
            .on_action(cx.listener(|this, _: &Paste, _window, cx| {
                this.paste(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.selecting = true;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_down(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.selecting = false;
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.mouse_up(event, cx);
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                this.terminal.update(cx, |terminal, cx| {
                    // Always send mouse move events in mouse mode (e.g., neovim)
                    // so the application can handle selection in real-time
                    if terminal.mouse_mode(event.modifiers.shift) {
                        terminal.mouse_move(event, cx);
                    }
                    // Also handle terminal selection if we're dragging
                    if this.selecting {
                        terminal.mouse_drag(event, cx);
                    }
                });
                cx.notify();
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.terminal.update(cx, |terminal, _cx| {
                    terminal.scroll_wheel(event, 3.0);
                });
                cx.stop_propagation();
                cx.notify();
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.add_paths_to_terminal(paths.paths(), cx);
            }))
            .child(
                // Use relative positioning: terminal fills container, scrollbar floats on top
                div()
                    .relative()
                    .size_full()
                    .child(
                        // Terminal content fills entire container
                        TerminalElement::new(
                            terminal.clone(),
                            focus_handle,
                            is_focused,
                            self.cursor_visible,
                            view_entity,
                        ),
                    )
                    .when(show_scrollbar, |this| {
                        // Scrollbar positioned absolutely on the right
                        // No background color - only the thumb is visible (Zed style)
                        this.child(
                            div()
                                .absolute()
                                .right(px(2.0))
                                .top_0()
                                .bottom_0()
                                .w(px(8.0))
                                .child(TerminalScrollbar::new(terminal, cx)),
                        )
                    })
                    // SSH 连接进度视图覆盖层 - 在连接建立期间覆盖终端内容
                    .when_some(self.ssh_connect_view.clone(), |this, connect_view| {
                        this.child(div().absolute().inset_0().child(connect_view))
                    }),
            )
    }
}

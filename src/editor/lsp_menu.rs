//! 语言服务器控制菜单：只操作**当前这一个**服务器。
//!
//! 对照 Zed 的 `crates/language_tools/src/lsp_button.rs`：它的状态栏按钮点开
//! 是全部服务器的列表，每个服务器再弹子菜单。这里刻意**不做列表**——DeepTerm
//! 的状态栏 chip 本身就是「当前文件的服务器」，用户点它的时候想知道的是
//! 「这个服务器怎么了、我能不能把它重启」，不是「我机器上还有哪些服务器」。
//! 列一堆当前文件用不到的服务只会让人多选一次。
//!
//! 菜单结构（悬停才出现的子菜单在这里不需要——只有一个服务）：
//!
//! ```text
//! ●  rust-analyzer                ← 标题行，灯 + 名字
//!    View Logs                    ← 报文面板
//!    Restart Server
//!    Stop Server  /  Start Server
//!    ────────────────────────────
//!    ● Running — v1.98.0 — 3.7 GB  ← 只读的元信息行
//! ```
//!
//! 状态色全走 [`crate::editor::lsp_lamp`]：菜单里的灯与状态栏 chip 上的灯
//! 是同一个组件、同一种动画——用户才可能把两者联系起来。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, IntoElement, Render, Styled, Window,
    actions, div, px,
};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::components::menu::MenuItemElement;
use crate::editor::editor::Editor;
use crate::editor::lsp_lamp;
use crate::lsp::project::{ServerSnapshot, ServerState};
use crate::theme::ActiveTheme as _;

/// 菜单上的三个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    Restart,
}

actions!(lsp_menu, [Cancel]);

const CONTEXT: &str = "LspMenu";

/// 注册 Escape 键：菜单开着时要能一键关掉。
pub fn init(cx: &mut App) {
    crate::keymap::bind(cx, [gpui::KeyBinding::new("escape", Cancel, Some(CONTEXT))]);
}

/// 一行条目的高度（与 `MenuItemElement` 一致，保证视觉对齐）。
const ROW_H: f32 = 22.0;

/// 面板内边距（与 `PopupMenu` 一致）。
const PANEL_PAD: f32 = 6.0;

pub struct LspMenu {
    /// 菜单只服务这一个服务器。
    snapshot: ServerSnapshot,
    /// 宿主编辑器：所有动作都要它去驱动会话（didOpen / didClose）。
    host: gpui::WeakEntity<Editor>,
    focus_handle: FocusHandle,
    /// 服务器进程内存缓存（见 [`ProcessMemoryCache]）。
    mem: ProcessMemoryCache,
}

impl LspMenu {
    /// 构造菜单内容。
    ///
    /// 返回 `Self` 而不是 `Entity<Self>`：调用方（`Editor`）只有
    /// `Context<Editor>`，得由它 `cx.new(...)` 才能拿到 `Entity<LspMenu>`。
    pub fn new(
        host: gpui::WeakEntity<Editor>,
        snapshot: ServerSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            snapshot,
            host,
            focus_handle: cx.focus_handle(),
            mem: ProcessMemoryCache::new(),
        }
    }

    fn dismiss(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    // ---------------------------------------------------------------- 动作

    /// 执行一个动作，然后关掉菜单。
    ///
    /// 「点完即关」与 Zed / VSCode 的菜单一致；留着菜单让人再点一次外面只是
    /// 多一步。执行结果从状态栏 chip 上看（灯的颜色 / 文字会跟着变）。
    fn act(&mut self, what: Action, cx: &mut Context<Self>) {
        let key = self.snapshot.key.clone();
        self.host
            .update(cx, |editor, cx| match what {
                Action::Start => editor.start_lsp_server_for_key(&key, cx),
                Action::Stop => editor.stop_lsp_server_for_key(&key, cx),
                Action::Restart => editor.restart_lsp_server_for_key(&key, cx),
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn view_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.snapshot.key.clone();
        self.host
            .update(cx, |editor, cx| editor.open_lsp_log_view(&key, window, cx))
            .ok();
        // 打开日志面板后把自己关掉：两块浮层叠在一起只会互相挡。
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for LspMenu {}
crate::impl_focusable!(LspMenu);

impl LspMenu {
    /// 焦点句柄。
    ///
    /// 给宿主抢焦点用：Escape 的 keybinding 挂在菜单的 key context 上，
    /// 不聚焦就收不到。`track_focus` 只是「跟踪」，不等于「获得」。
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LspMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let snapshot = &self.snapshot;
        let running = snapshot.is_running();
        // 元信息行的灯色（标题行已去掉，这里是唯一用色的地方）。
        let color = state_color(snapshot.state, &theme);

        // 元信息行：状态 + 版本 + 内存。
        let mut meta = match snapshot.state {
            ServerState::Running => "Running".to_string(),
            ServerState::Starting => "Starting".to_string(),
            ServerState::Stopped => "Stopped".to_string(),
            ServerState::Failed => "Failed".to_string(),
        };
        if let Some(version) = &snapshot.version {
            meta.push_str(&format!(" — v{version}"));
        }
        if let Some(bytes) = snapshot.pid.map(|pid| self.mem.usage_of(pid)) {
            meta.push_str(&format!(" — {}", format_memory(bytes)));
        }

        div()
            .id("lsp-menu")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .flex()
            .flex_col()
            .p(px(PANEL_PAD))
            .min_w(px(248.0))
            .bg(theme.menu_background)
            .text_color(theme.text)
            .border(px(0.5))
            .border_color(theme.menu_border)
            .rounded(px(12.0))
            .occlude()
            .child(
                MenuItemElement::new("lsp-view-logs")
                    .child("View Logs")
                    .on_click(cx.listener(|this, _, window, cx| this.view_logs(window, cx))),
            )
            .child(
                MenuItemElement::new("lsp-restart")
                    .child("Restart Server")
                    .on_click(cx.listener(|this, _, _, cx| this.act(Action::Restart, cx))),
            )
            // 启动 / 停止按状态二选一。这里**不**两个都放（置灰一个）：
            // 菜单只服务一个服务器，「另一个操作」对当前状态毫无意义，
            // 摆着只会让人以为点了能有反应。
            .when(running, |this| {
                this.child(
                    MenuItemElement::new("lsp-stop")
                        .child("Stop Server")
                        .on_click(cx.listener(|this, _, _, cx| this.act(Action::Stop, cx))),
                )
            })
            .when(!running, |this| {
                this.child(
                    MenuItemElement::new("lsp-start")
                        .child("Start Server")
                        .on_click(cx.listener(|this, _, _, cx| this.act(Action::Start, cx))),
                )
            })
            .child(separator(&theme))
            // ── 元信息行 ─────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(ROW_H))
                    .px(px(8.0))
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child(lsp_lamp::lamp(color))
                    .child(meta),
            )
    }
}

/// 面板内的分隔线（与 `PopupMenu` 里的同款）。
fn separator(theme: &crate::theme::UiTheme) -> impl IntoElement {
    div().h(px(9.0)).flex().items_center().child(
        div()
            .h(px(1.0))
            .mx(px(8.0))
            .bg(theme.menu_separator.opacity(0.6))
            .w_full(),
    )
}

/// 状态 → 灯的颜色。
fn state_color(state: ServerState, theme: &crate::theme::UiTheme) -> gpui::Hsla {
    match state {
        ServerState::Running => theme.success,
        ServerState::Starting => theme.accent,
        ServerState::Failed => theme.error,
        // 停止是用户的选择，不是故障：用最淡的一档，不报警。
        ServerState::Stopped => theme.text_muted,
    }
}

/// 字节数 → "3.7 GB" / "128 MB"。
fn format_memory(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    }
}

/// 进程内存缓存。
///
/// 与 Zed `lsp_button.rs` 的 `ProcessMemoryCache` 同构：**语言服务器的内存
/// 要算整棵进程树**（rust-analyzer 会 fork 子进程，只算根进程会少一大截）。
struct ProcessMemoryCache {
    system: System,
    usage: HashMap<u32, u64>,
    last_refresh: Option<Instant>,
}

/// 缓存有效期：5s，与 Zed 同值。
const MEMORY_CACHE_TTL: Duration = Duration::from_secs(5);

impl ProcessMemoryCache {
    fn new() -> Self {
        Self {
            system: System::new(),
            usage: HashMap::new(),
            last_refresh: None,
        }
    }

    /// 某进程（含其子进程）的内存占用（字节）。
    fn usage_of(&mut self, pid: u32) -> u64 {
        let expired = self
            .last_refresh
            .map(|t| t.elapsed() >= MEMORY_CACHE_TTL)
            .unwrap_or(true);
        if expired {
            self.system.refresh_specifics(
                RefreshKind::new().with_processes(ProcessRefreshKind::new().with_memory()),
            );
            self.usage.clear();
            self.last_refresh = Some(Instant::now());
        }
        if let Some(&bytes) = self.usage.get(&pid) {
            return bytes;
        }
        let root = Pid::from_u32(pid);
        let parents: HashMap<Pid, Pid> = self
            .system
            .processes()
            .iter()
            .filter_map(|(&pid, process)| Some((pid, process.parent()?)))
            .collect();
        let total: u64 = self
            .system
            .processes()
            .iter()
            // `filter` 收的是 `&Item`，比 `filter_map` 多一层引用。
            .filter(|(pid, _)| is_descendant_of(**pid, root, &parents))
            .map(|(_, process)| process.memory())
            .sum();
        self.usage.insert(pid, total);
        total
    }
}

/// `pid` 是否在 `root` 的进程树里。
///
/// `visited` 是防环的：pid 理论上不会成环，但系统在某些沙盒环境下会给出
/// 自指的 parent，没有这个集合就是死循环。
fn is_descendant_of(pid: Pid, root: Pid, parents: &HashMap<Pid, Pid>) -> bool {
    let mut current = pid;
    let mut visited = std::collections::HashSet::new();
    while current != root {
        if !visited.insert(current) {
            return false;
        }
        match parents.get(&current) {
            Some(&parent) => current = parent,
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格式化按 GB / MB 分档。
    #[test]
    fn memory_is_human_readable() {
        assert_eq!(format_memory(0), "0 MB");
        assert_eq!(format_memory(512 * 1024 * 1024), "512 MB");
        assert_eq!(
            format_memory((3.7 * 1024.0 * 1024.0 * 1024.0) as u64),
            "3.7 GB"
        );
    }

    /// 自指的 parent 不能让遍历转起来没完。
    #[test]
    fn cyclic_parent_chain_terminates() {
        let mut parents = HashMap::new();
        parents.insert(Pid::from_u32(1), Pid::from_u32(2));
        parents.insert(Pid::from_u32(2), Pid::from_u32(3));
        parents.insert(Pid::from_u32(3), Pid::from_u32(1));
        assert!(!is_descendant_of(
            Pid::from_u32(1),
            Pid::from_u32(9),
            &parents
        ));
    }

    /// 自己就是 root 时命中（内存统计要把根进程算进去）。
    #[test]
    fn root_is_its_own_descendant() {
        let parents = HashMap::new();
        assert!(is_descendant_of(
            Pid::from_u32(42),
            Pid::from_u32(42),
            &parents
        ));
    }

    /// 顺着链条能走通；反向不行。
    #[test]
    fn walks_up_the_chain() {
        let mut parents = HashMap::new();
        parents.insert(Pid::from_u32(10), Pid::from_u32(20));
        parents.insert(Pid::from_u32(20), Pid::from_u32(30));
        assert!(is_descendant_of(
            Pid::from_u32(10),
            Pid::from_u32(30),
            &parents
        ));
        assert!(!is_descendant_of(
            Pid::from_u32(30),
            Pid::from_u32(10),
            &parents
        ));
    }
}

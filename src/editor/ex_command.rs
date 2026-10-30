//! Vim `:` 命令行（Ex 命令）。
//!
//! 解析与执行分开：
//! - [`parse`] 只把输入拆成「行范围 + 命令名 + bang + 参数」，不碰编辑器；
//! - [`ExCommandPanel`] 负责输入/历史/Tab 补全/错误显示；
//! - `Editor::execute_ex_command` 负责真正执行（保存、切 tab、改文本）。
//!
//! 第一阶段覆盖与 Zed `crates/vim/src/command.rs` 对齐的常用命令，
//! 不实现 `:g`、`:normal`、`:!` 这类需要二次解析或 shell 的命令。

use gpui::prelude::*;
use gpui::{App, Context, Entity, Subscription, WeakEntity, Window, div, px};
use std::cell::RefCell;
use std::rc::Rc;

use super::{ExCancel, ExComplete, ExHistoryNext, ExHistoryPrev, ExSubmit};
use crate::components::input::{EditorInput, InputState};
use crate::theme::ActiveTheme;

/// 行地址。范围端点可以是当前行、末行、数字、标记或整个缓冲区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineExpr {
    Current,
    Last,
    Number(usize),
    Mark(char),
    All,
}

/// 解析后的 Ex 命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExCommand {
    /// `None` 表示没有显式范围，由具体命令决定默认范围。
    pub range: Option<(LineExpr, LineExpr)>,
    /// 命令名；为空表示「地址即跳转」（`:42` / `:$`）。
    pub name: String,
    pub bang: bool,
    /// 命令名之后、已 trim 的参数原文。
    pub args: String,
}

impl ExCommand {
    pub(crate) fn is_goto(&self) -> bool {
        self.name.is_empty()
    }
}

/// 解析 `:` 后面的输入（允许调用方连 `:` 一起传入）。
pub(crate) fn parse(input: &str) -> Result<ExCommand, String> {
    let mut rest = input.trim();
    if let Some(stripped) = rest.strip_prefix(':') {
        rest = stripped.trim_start();
    }
    if rest.is_empty() {
        return Ok(ExCommand {
            range: None,
            name: String::new(),
            bang: false,
            args: String::new(),
        });
    }

    let mut range = None;
    let bytes = rest.as_bytes();
    let mut i = 0;

    if bytes[i] == b'%' {
        range = Some((LineExpr::All, LineExpr::All));
        i += 1;
    } else if let Some((expr, next)) = parse_line_expr(rest, i) {
        i = skip_spaces(rest, next);
        if i < bytes.len() && bytes[i] == b',' {
            i = skip_spaces(rest, i + 1);
            let Some((end, next)) = parse_line_expr(rest, i) else {
                return Err("范围缺少结束行".into());
            };
            i = next;
            range = Some((expr, end));
        } else {
            range = Some((expr, expr));
        }
    }

    i = skip_spaces(rest, i);
    if i >= bytes.len() {
        return Ok(ExCommand {
            range,
            name: String::new(),
            bang: false,
            args: String::new(),
        });
    }

    // `s/old/new/` 没有空格：命令名只有 `s`，其余全是参数。
    if bytes[i] == b's'
        && bytes
            .get(i + 1)
            .is_some_and(|b| !b.is_ascii_alphanumeric() && !b.is_ascii_whitespace())
    {
        let args = rest[i + 1..].trim().to_string();
        return Ok(ExCommand {
            range,
            name: "s".into(),
            bang: false,
            args,
        });
    }

    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut name = rest[start..i].to_string();
    let mut bang = false;
    if let Some(stripped) = name.strip_suffix('!') {
        name = stripped.to_string();
        bang = true;
    }
    if name.is_empty() {
        return Err("缺少命令名".into());
    }
    let args = rest[i..].trim().to_string();

    Ok(ExCommand {
        range,
        name,
        bang,
        args,
    })
}

fn skip_spaces(input: &str, mut i: usize) -> usize {
    while input
        .as_bytes()
        .get(i)
        .is_some_and(|b| b.is_ascii_whitespace())
    {
        i += 1;
    }
    i
}

fn parse_line_expr(input: &str, i: usize) -> Option<(LineExpr, usize)> {
    let bytes = input.as_bytes();
    match *bytes.get(i)? {
        b'.' => Some((LineExpr::Current, i + 1)),
        b'$' => Some((LineExpr::Last, i + 1)),
        b'\'' => {
            let mark = input[i + 1..].chars().next()?;
            Some((LineExpr::Mark(mark), i + 1 + mark.len_utf8()))
        }
        b'0'..=b'9' => {
            let start = i;
            let mut end = i;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let number = input[start..end].parse::<usize>().ok()?;
            Some((LineExpr::Number(number), end))
        }
        _ => None,
    }
}

/// 全局 Ex 命令历史（跨面板、跨文件保留）。
#[derive(Default)]
struct ExHistory {
    entries: Vec<String>,
}

impl gpui::Global for ExHistory {}

impl ExHistory {
    fn entries(cx: &App) -> Vec<String> {
        cx.try_global::<Self>()
            .map(|history| history.entries.clone())
            .unwrap_or_default()
    }

    fn push(cx: &mut App, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if cx.try_global::<Self>().is_none() {
            cx.set_global(Self::default());
        }
        let history = cx.global_mut::<Self>();
        if history.entries.last().map(String::as_str) == Some(line) {
            return;
        }
        history.entries.push(line.to_string());
        if history.entries.len() > 200 {
            history.entries.remove(0);
        }
    }
}

/// 底部的 `:` 命令行面板。
pub(crate) struct ExCommandPanel {
    pub(crate) input: Entity<InputState>,
    editor: WeakEntity<super::Editor>,
    pub(crate) error: Option<String>,
    pub(crate) message: Option<String>,
    history: Rc<RefCell<Vec<String>>>,
    history_index: Option<usize>,
    /// 打开历史前保存的当前输入，用于 Down 回到未提交内容。
    history_draft: String,
    completion: Vec<String>,
    completion_index: usize,
    _subscription: Subscription,
}

impl ExCommandPanel {
    pub(crate) fn new(
        editor: WeakEntity<super::Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .placeholder("输入 Vim 命令，例如 w / %s/foo/bar/g / 42")
                .submit_on_enter(true);
            input.set_extra_key_context(Some("ExCommandLine"));
            input
        });

        let history = Rc::new(RefCell::new(ExHistory::entries(cx)));
        let panel = cx.new(|_cx| Self {
            input: input.clone(),
            editor,
            error: None,
            message: None,
            history,
            history_index: None,
            history_draft: String::new(),
            completion: Vec::new(),
            completion_index: 0,
            _subscription: Subscription::new(|| {}),
        });

        let subscription = panel.update(cx, |_this, cx| {
            cx.subscribe(&input, |this: &mut Self, _, event, cx| {
                if let crate::components::input::InputEvent::Change = event {
                    this.error = None;
                    this.message = None;
                    this.history_index = None;
                    this.refresh_completion(cx);
                }
            })
        });
        panel.update(cx, |this, _| this._subscription = subscription);

        panel
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.read(cx).focus_handle.clone().focus(window, cx);
    }

    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let raw = self.input.read(cx).value().to_string();
        self.completion = complete(&raw);
        self.completion_index = 0;
    }

    fn apply_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion.is_empty() {
            self.refresh_completion(cx);
        }
        let Some(candidate) = self
            .completion
            .get(self.completion_index % self.completion.len().max(1))
            .cloned()
        else {
            return;
        };
        self.completion_index = self.completion_index.wrapping_add(1);
        let raw = self.input.read(cx).value().to_string();
        let replaced = replace_last_token(&raw, &candidate);
        self.input.update(cx, |input, cx| {
            input.set_value(replaced, window, cx);
        });
        cx.notify();
    }

    fn cycle_history(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let history_len = self.history.borrow().len();
        if history_len == 0 {
            return;
        }
        let len = history_len as isize;
        let next = match self.history_index {
            None => {
                self.history_draft = self.input.read(cx).value().to_string();
                if delta < 0 { len - 1 } else { -1 }
            }
            Some(index) => (index as isize + delta).rem_euclid(len),
        };
        if next < 0 {
            self.history_index = None;
            let draft = self.history_draft.clone();
            self.input.update(cx, |input, cx| {
                input.set_value(draft, window, cx);
            });
            cx.notify();
            return;
        }
        self.history_index = Some(next as usize);
        let value = self
            .history
            .borrow()
            .get(next as usize)
            .cloned()
            .unwrap_or_default();
        self.input.update(cx, |input, cx| {
            input.set_value(value, window, cx);
        });
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.input.read(cx).value().to_string();
        let line = line.trim().to_string();
        if line.is_empty() {
            self.close(window, cx);
            return;
        }
        let Some(editor) = self.editor.upgrade() else {
            return;
        };

        let result = editor.update(cx, |editor, cx| {
            editor.execute_ex_command(&line, window, cx)
        });
        match result {
            Ok(outcome) => {
                ExHistory::push(cx, &line);
                {
                    let mut history = self.history.borrow_mut();
                    if history.last().map(String::as_str) != Some(line.as_str()) {
                        history.push(line.clone());
                        if history.len() > 200 {
                            history.remove(0);
                        }
                    }
                }
                self.history_index = None;
                match outcome {
                    ExOutcome::Close => self.close(window, cx),
                    ExOutcome::Message(message) => {
                        self.error = None;
                        self.message = Some(message);
                        self.input.update(cx, |input, cx| {
                            input.set_value("", window, cx);
                        });
                        self.focus(window, cx);
                        cx.notify();
                    }
                }
            }
            Err(error) => {
                self.error = Some(error);
                self.message = None;
                cx.notify();
            }
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| editor.close_ex_command(window, cx));
        }
    }

    fn on_submit(&mut self, _: &ExSubmit, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.submit(window, cx);
    }

    fn on_cancel(&mut self, _: &ExCancel, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.close(window, cx);
    }

    fn on_complete(&mut self, _: &ExComplete, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.apply_completion(window, cx);
    }

    fn on_history_prev(&mut self, _: &ExHistoryPrev, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.cycle_history(-1, window, cx);
    }

    fn on_history_next(&mut self, _: &ExHistoryNext, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.cycle_history(1, window, cx);
    }
}

impl gpui::Render for ExCommandPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        let mut panel = div()
            .id("ex-command-panel")
            .w(px(640.0))
            .max_w(px(820.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(10.0))
            .bg(theme.menu_background)
            .border_1()
            .border_color(theme.menu_border)
            .shadow_lg()
            .text_size(px(12.5))
            .text_color(theme.text)
            .occlude()
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_complete))
            .on_action(cx.listener(Self::on_history_prev))
            .on_action(cx.listener(Self::on_history_next));

        if let Some(error) = &self.error {
            panel = panel.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.error_foreground)
                    .child(error.clone()),
            );
        } else if let Some(message) = &self.message {
            panel = panel.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(message.clone()),
            );
        }

        let mut row = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child(":"),
            )
            .child(
                div()
                    .flex_grow_1()
                    .child(EditorInput::new(&self.input).transparent().small()),
            );
        if !self.completion.is_empty() {
            row = row.child(
                div()
                    .max_w(px(240.0))
                    .truncate()
                    .text_size(px(10.5))
                    .text_color(theme.text_subtle)
                    .child(format!(
                        "Tab: {}",
                        self.completion
                            .get(self.completion_index % self.completion.len())
                            .cloned()
                            .unwrap_or_default()
                    )),
            );
        }
        panel.child(row)
    }
}

/// `:marks` / `:registers` 这类命令的结果：保持面板打开并显示一段文本。
pub(crate) enum ExOutcome {
    Close,
    Message(String),
}

/// 命令名 + 参数补全。
fn complete(raw: &str) -> Vec<String> {
    const COMMANDS: &[&str] = &[
        "w",
        "wq",
        "x",
        "q",
        "e",
        "bn",
        "bp",
        "tabn",
        "tabp",
        "d",
        "y",
        "j",
        "s",
        "sort",
        "noh",
        "set",
        "marks",
        "registers",
    ];
    let trimmed_start = raw.trim_start();
    if let Some(rest) = trimmed_start.strip_prefix("set ") {
        let prefix = rest.trim();
        return ["nu", "nonu", "rnu", "nornu", "wrap", "nowrap", "ic", "noic"]
            .into_iter()
            .filter(|option| option.starts_with(prefix))
            .map(|option| format!("set {option}"))
            .collect();
    }
    if trimmed_start.contains(' ') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|command| command.starts_with(trimmed_start))
        .map(|command| (*command).to_string())
        .collect()
}

/// 把最后一个空格之后的部分替换成补全候选。
fn replace_last_token(raw: &str, candidate: &str) -> String {
    if candidate.starts_with("set ") {
        return candidate.to_string();
    }
    match raw.rfind(' ') {
        Some(index) => format!("{}{}", &raw[..=index], candidate),
        None => candidate.to_string(),
    }
}

/// 解析 `s/pat/repl/[gi]` 的参数（第一个字符是分隔符，不要求是 `/`）。
pub(crate) fn parse_substitute(args: &str) -> Result<(String, String, String), String> {
    let mut chars = args.chars();
    let delimiter = chars.next().ok_or_else(|| "缺少替换分隔符".to_string())?;
    if delimiter.is_alphanumeric() || delimiter.is_whitespace() {
        return Err("替换分隔符不能是字母/空白".into());
    }
    let rest = &args[delimiter.len_utf8()..];
    let (pattern, rest) =
        split_unescaped(rest, delimiter).ok_or_else(|| "替换缺少结束分隔符".to_string())?;
    let (replacement, flags) = match split_unescaped(&rest, delimiter) {
        Some((replacement, flags)) => (replacement, flags),
        None => (rest, String::new()),
    };
    let flags = flags.trim().to_string();
    for flag in flags.chars() {
        if !matches!(flag, 'g' | 'i') {
            return Err(format!("不支持的替换标志：{flag}"));
        }
    }
    Ok((pattern, replacement, flags))
}

fn split_unescaped(input: &str, delimiter: char) -> Option<(String, String)> {
    let mut out = String::new();
    let mut chars = input.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch == '\\' {
            if let Some((_, next)) = chars.next() {
                if next == delimiter {
                    out.push(delimiter);
                } else {
                    out.push('\\');
                    out.push(next);
                }
                continue;
            }
            out.push('\\');
            continue;
        }
        if ch == delimiter {
            return Some((out, input[index + ch.len_utf8()..].to_string()));
        }
        out.push(ch);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_commands() {
        assert_eq!(parse("w").unwrap().name, "w");
        assert_eq!(parse("w!").unwrap().bang, true);
        assert_eq!(parse("q!").unwrap().name, "q");
        assert_eq!(parse("42").unwrap().name, "");
        assert_eq!(
            parse("$").unwrap().range,
            Some((LineExpr::Last, LineExpr::Last))
        );
        assert_eq!(
            parse("1,5d").unwrap().range,
            Some((LineExpr::Number(1), LineExpr::Number(5)))
        );
        assert_eq!(
            parse("%s/foo/bar/g").unwrap().range,
            Some((LineExpr::All, LineExpr::All))
        );
        assert_eq!(parse("%s/foo/bar/g").unwrap().args, "/foo/bar/g");
        assert_eq!(
            parse("'a,'bd").unwrap().range,
            Some((LineExpr::Mark('a'), LineExpr::Mark('b')))
        );
        assert_eq!(parse("set nu").unwrap().args, "nu");
        assert_eq!(parse("s/foo/bar/").unwrap().name, "s");
    }

    #[test]
    fn parses_substitute() {
        assert_eq!(
            parse_substitute("/foo/bar/g").unwrap(),
            ("foo".into(), "bar".into(), "g".into())
        );
        assert_eq!(
            parse_substitute("#a/b#c/d#").unwrap(),
            ("a/b".into(), "c/d".into(), "".into())
        );
        assert!(parse_substitute("x").is_err());
    }

    #[test]
    fn rejects_missing_range_end() {
        assert!(parse("1,").is_err());
    }

    #[test]
    fn completes_commands_and_set_options() {
        assert!(complete("w").contains(&"wq".to_string()));
        assert!(complete("set r").contains(&"set rnu".to_string()));
    }
}

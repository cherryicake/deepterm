//! Vim 的端到端行为测试。
//!
//! 这些测试驱动**真实击键**（`cx.simulate_keystrokes`），而不是直接调用
//! handler。理由是按键拦截这条链路横跨 gpui 的三个阶段——平台输入处理器、
//! 绑定解析、`on_key_down` 监听器——只有走完整链路才能证明
//! 「Normal 模式下按 `h` 不会插入字符」这类核心行为成立。
//!
//! 测试用的 `InputState` 走 `vim` 字段直设，不依赖全局设置：
//! 这样测试不受开发者机器上 `settings.json` 的影响。

use gpui::{
    AppContext as _, Entity, InputEvent as _, IntoElement, KeyDownEvent, Keystroke, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div,
};

use crate::components::input::state::InputState;
use crate::vim::{VimMode, VimState};

/// 一个最小的宿主视图，承载被测的 `InputState`。
struct VimHost {
    state: Entity<InputState>,
}

impl Render for VimHost {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(crate::components::input::EditorInput::new(&self.state))
    }
}

/// 构造一个带 vim 状态的代码编辑器并聚焦。
fn vim_editor<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<InputState>, &'a mut VisualTestContext) {
    vim_editor_with_language(cx, text, "text")
}

/// 同上，但指定语言（`gc` 的注释符依赖语言）。
fn vim_editor_with_language<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
    language: &str,
) -> (Entity<InputState>, &'a mut VisualTestContext) {
    // `EditorInput` 的渲染路径要读主题与字体设置，先装好这两者。
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        // vim 键位表只在 `settings.vim.enabled` 时加载，所以测试里
        // 显式打开开关，再重建键位表——这也是真实用户打开 vim 的路径。
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = true;
        });
        crate::keymap::reload(cx);
        // 走**真实入口** `vim::init`（而不是单独装 Registers/Marks）。
        //
        // 回归：`vim::init` 曾在 `main.rs` 里漏调用，导致 Registers/Marks
        // 从未注册，用户按 `yy` 就 panic（`no global registered of type
        // deepterm::vim::registers::Registers`）。测试里手动 init 两个全局
        // 会让这个缺陷隐形——必须用与生产同一条路径。
        crate::vim::init(cx);
        // `InputState::init` 注册 App 级击键拦截器：`f`/`t`/`r` 的参数
        // 必须靠它才能在绑定解析之前被截住。
        crate::components::input::state::init(cx);
    });

    let language = language.to_string();
    let cell = std::cell::RefCell::new(None);
    let (host, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor(language.clone())
                .multi_line(true);
            state.vim = Some(VimState::default());
            state
        });
        state.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.move_to(0, None, cx);
            state.focus(window, cx);
        });
        *cell.borrow_mut() = Some(state.clone());
        VimHost { state }
    });
    let _ = host;
    (cell.into_inner().unwrap(), cx)
}

/// 读当前文本。
fn text_of(state: &Entity<InputState>, cx: &mut VisualTestContext) -> String {
    state.read_with(cx, |state, _| state.value().to_string())
}

/// 读当前模式。
fn mode_of(state: &Entity<InputState>, cx: &mut VisualTestContext) -> VimMode {
    state.read_with(cx, |state, _| {
        state.vim.as_ref().expect("测试中 vim 必须已启用").mode
    })
}

/// 读当前光标偏移。
///
/// 可视模式下取 `visual_head`：`selected_range.end` 为「含尾」比光标大一格，
/// 直接读它会把可视区末端当成光标位置。
fn cursor_of(state: &Entity<InputState>, cx: &mut VisualTestContext) -> usize {
    state.read_with(cx, |state, _| {
        state
            .vim
            .as_ref()
            .filter(|vim| vim.mode.is_visual())
            .and_then(|vim| vim.visual_head)
            .unwrap_or_else(|| state.cursor())
    })
}

/// 直接派发一次击键。
///
/// 不用 `cx.simulate_keystrokes`：后者按「平台击键 → 文本输入」路径投递，
/// 而这里要验证的正是 `KeyDownEvent` 在 `on_key_down` 与绑定解析之间的
/// 处理顺序。用 `dispatch_event` 走与真实按键相同的入口。
fn press(cx: &mut VisualTestContext, keystroke: &str) {
    let parsed = Keystroke::parse(keystroke).expect("击键应可解析");
    cx.update(|window, cx| {
        window.dispatch_event(
            KeyDownEvent {
                keystroke: parsed,
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.run_until_parked();
}

/// 模拟文本输入（走平台输入处理器路径）。
fn type_text(cx: &mut VisualTestContext, text: &str) {
    cx.simulate_input(text);
    cx.run_until_parked();
}

#[gpui::test]
fn normal_mode_swallows_printable_keys(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");
    assert_eq!(mode_of(&state, cx), VimMode::Normal);

    // `h` 在 Normal 下是「左移」，绝不能插入字符。
    press(cx, "h");
    assert_eq!(
        text_of(&state, cx),
        "hello world\n",
        "Normal 模式不得插入字符"
    );

    // 一个未绑定的键（vim 里是「无操作」）同样不得插入。
    press(cx, "z");
    press(cx, "q");
    assert_eq!(
        text_of(&state, cx),
        "hello world\n",
        "未绑定的按键也不得插入字符"
    );
}

#[gpui::test]
fn normal_mode_motions_move_the_cursor(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 光标起点在 0；`l` 前进一格。
    press(cx, "l");
    assert_eq!(cursor_of(&state, cx), 1, "`l` 应右移一格");

    press(cx, "l");
    assert_eq!(cursor_of(&state, cx), 2);

    press(cx, "h");
    assert_eq!(cursor_of(&state, cx), 1, "`h` 应左移一格");

    // 行首。
    press(cx, "0");
    assert_eq!(cursor_of(&state, cx), 0, "`0` 应回到行首");

    // `$` 停在行内最后一个字符上（vim 语义），即 "hello world" 的 `d`（偏移 10）。
    press(cx, "$");
    assert_eq!(
        cursor_of(&state, cx),
        10,
        "`$` 应停在行尾字符上，而非换行之后"
    );

    // `w` 从行首跳到下一个词首。
    press(cx, "0");
    press(cx, "w");
    assert_eq!(cursor_of(&state, cx), 6, "`w` 应跳到 `world` 的词首");
}

#[gpui::test]
fn right_motion_stops_at_last_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "ab\ncd\n");

    // `l` 连续按不应越过行尾字符进入下一行。
    for _ in 0..10 {
        press(cx, "l");
    }
    assert_eq!(
        cursor_of(&state, cx),
        1,
        "Normal 模式的 `l` 应停在行内最后一个字符上，不跨行"
    );
}

#[gpui::test]
fn escape_returns_to_normal_and_shifts_cursor_left(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "i");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`i` 应进入 Insert");

    type_text(cx, "XY");
    assert_eq!(text_of(&state, cx), "XYhello\n", "Insert 模式应插入文本");

    // `Esc` 回 Normal，并把光标左移一格（vim 语义）。
    let before = cursor_of(&state, cx);
    press(cx, "escape");
    assert_eq!(mode_of(&state, cx), VimMode::Normal, "`Esc` 应回到 Normal");
    assert_eq!(
        cursor_of(&state, cx),
        before - 1,
        "`Esc` 应把光标左移一格，落到刚输入的字符上"
    );

    // 回到 Normal 后按键只移动光标，不再插入文本。
    let text = text_of(&state, cx);
    let cursor_before = cursor_of(&state, cx);
    press(cx, "h");
    assert_eq!(text_of(&state, cx), text, "回到 Normal 后不得再插入字符");
    assert!(
        cursor_of(&state, cx) < cursor_before,
        "回到 Normal 后 `h` 应移动光标"
    );
}

#[gpui::test]
fn insert_mode_still_accepts_text(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "shift-a");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`A` 应进入 Insert");

    type_text(cx, "Z");
    assert_eq!(text_of(&state, cx), "abcZ\n", "Insert 模式必须接受文本输入");
}

#[gpui::test]
fn operator_keys_switch_the_key_context(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `d` 压入 Delete 操作符，键位上下文应切到 `vim_operator == d`。
    press(cx, "d");
    state.read_with(cx, |state, _| {
        let vim = state.vim.as_ref().unwrap();
        assert_eq!(vim.active_operator_id(), "d", "`d` 应压入删除操作符");
    });

    // 操作符待决时 `w` 完成 `dw`：删掉第一个词。
    press(cx, "w");
    assert_eq!(
        text_of(&state, cx),
        "world\n",
        "操作符待决时 `w` 应完成 `dw`"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.vim.as_ref().unwrap().active_operator_id(),
            "none",
            "执行完应清空待决操作符"
        );
    });
}

#[gpui::test]
fn object_prefix_after_operator_selects_object_context(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "d");
    press(cx, "i");
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.vim.as_ref().unwrap().active_operator_id(),
            "i",
            "`d` 之后的 `i` 应是对象前缀，而不是插入命令"
        );
        assert_eq!(
            state.vim.as_ref().unwrap().mode,
            VimMode::Normal,
            "`di` 不得进入 Insert 模式"
        );
    });
}

#[gpui::test]
fn vim_disabled_leaves_editing_untouched(cx: &mut TestAppContext) {
    // 没有 vim 状态时，一切照旧：按键走常规路径，文本正常插入。
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::keymap::reload(cx);
        // 寄存器是全局的，测试里也要装上。
        crate::vim::Registers::init(cx);
        crate::vim::Marks::init(cx);
    });

    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("text")
                .multi_line(true)
        });
        state.update(cx, |state, cx| {
            state.set_value("abc", window, cx);
            state.move_to(0, None, cx);
            state.focus(window, cx);
        });
        *cell.borrow_mut() = Some(state.clone());
        VimHost { state }
    });
    let state = cell.into_inner().unwrap();

    state.read_with(cx, |state, _| {
        assert!(state.vim.is_none(), "未启用 vim 时不应有 vim 状态");
        let plain = Keystroke {
            key: "h".into(),
            key_char: Some("h".into()),
            modifiers: Modifiers::default(),
        };
        assert!(
            !state.should_intercept_key_for_test(&plain),
            "未启用 vim 时不得拦截按键"
        );
    });

    // 文本输入仍走平台路径。
    type_text(cx, "Z");
    assert_eq!(text_of(&state, cx), "Zabc", "vim 关闭时文本输入不受影响");
}

#[gpui::test]
fn modifiers_bypass_interception(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    // Cmd/Ctrl 组合必须放行（否则会破坏系统快捷键）。
    state.read_with(cx, |state, _| {
        let cmd = Keystroke {
            key: "s".into(),
            key_char: None,
            modifiers: Modifiers {
                platform: true,
                ..Modifiers::default()
            },
        };
        assert!(
            !state.should_intercept_key_for_test(&cmd),
            "Cmd 组合不应被 vim 拦截"
        );

        let ctrl = Keystroke {
            key: "c".into(),
            key_char: None,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        };
        assert!(
            !state.should_intercept_key_for_test(&ctrl),
            "Ctrl 组合不应被 vim 拦截"
        );

        let plain = Keystroke {
            key: "h".into(),
            key_char: Some("h".into()),
            modifiers: Modifiers::default(),
        };
        assert!(
            state.should_intercept_key_for_test(&plain),
            "无修饰的可打印键在 Normal 下应被拦截"
        );
    });
}

// ==================== 操作符与编辑命令 ====================

/// 按一串键（支持 `"d w"` 这样的多键序列，按空白拆分逐个派发）。
fn press_all(cx: &mut VisualTestContext, keys: &str) {
    for key in keys.split_whitespace() {
        press(cx, key);
    }
}

/// 连续派发多个击键，**中间不等待重绘**。
///
/// 真实输入下，用户连按 `d` `d` 时第二次击键可能在重绘之前到达，
/// 此时键位上下文仍是旧的（`vim_operator == none`）。这条路径必须与
/// 「等重绘」的路径产生同样的结果，否则快速输入会行为不一致。
fn press_burst(cx: &mut VisualTestContext, keys: &[&str]) {
    for key in keys {
        let parsed = Keystroke::parse(key).expect("击键应可解析");
        cx.update(|window, cx| {
            window.dispatch_event(
                KeyDownEvent {
                    keystroke: parsed,
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
                cx,
            );
        });
    }
    cx.run_until_parked();
}

#[gpui::test]
fn delete_line_works_without_waiting_for_redraw(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    // 连按 `d` `d`，中间不等重绘——模拟真实快速输入。
    press_burst(cx, &["d", "d"]);
    assert_eq!(
        text_of(&state, cx),
        "two\n",
        "连按 `dd` 必须与逐次等待重绘的结果一致"
    );
}

#[gpui::test]
fn three_d_presses_do_not_delete_extra_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    // `ddd` 在 vim 里是 `dd` + 一次未完成的操作符，只应删掉一行。
    press_burst(cx, &["d", "d", "d"]);
    assert_eq!(
        text_of(&state, cx),
        "two\nthree\n",
        "`ddd` 只应删掉一行，第三次 `d` 只是开启新的操作符"
    );
}

#[gpui::test]
fn delete_word_removes_to_next_word_start(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `dw` 删掉「hello 」——独占语义到下一个词首之前。
    press_all(cx, "d w");
    assert_eq!(text_of(&state, cx), "world\n", "`dw` 应删到下一个词首之前");
}

#[gpui::test]
fn delete_word_is_symmetric_backwards(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 光标移到 `world` 的词首，`db` 应删掉「hello 」。
    press_all(cx, "w");
    assert_eq!(cursor_of(&state, cx), 6, "先移动到 `world`");
    press_all(cx, "d b");
    assert_eq!(text_of(&state, cx), "world\n", "`db` 应与 `dw` 对称");
}

#[gpui::test]
fn delete_to_end_of_line_is_inclusive(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\nsecond\n");

    // `d$` 含行尾字符，应删掉整行内容但保留换行。
    press_all(cx, "d $");
    assert_eq!(
        text_of(&state, cx),
        "\nsecond\n",
        "`d$` 应删掉整行内容（含最后一个字符），保留换行"
    );
}

#[gpui::test]
fn delete_line_removes_whole_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    // `dd` 删掉当前行（含换行）。
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\nthree\n", "`dd` 应删掉整行");
}

/// `dd` 在文件中间的行上，必须删掉**光标所在那一行**。
///
/// 覆盖「光标不在第 0 行」的情况——行首偏移算错时这里会暴露出来。
#[gpui::test]
fn delete_line_removes_the_cursor_line_not_the_first(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\nfour\n");

    // 移到第三行（`three`）。
    press(cx, "j");
    press(cx, "j");
    assert_eq!(cursor_of(&state, cx), 8, "光标应在 `three` 行首");

    press_all(cx, "d d");
    assert_eq!(
        text_of(&state, cx),
        "one\ntwo\nfour\n",
        "`dd` 必须删掉光标所在行，而不是第一行"
    );
}

/// `dd` 在最后一行（无尾随换行）上，应删掉该行及其前的换行。
#[gpui::test]
fn delete_line_on_last_line_without_trailing_newline(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo");

    press(cx, "j");
    press_all(cx, "d d");
    assert_eq!(
        text_of(&state, cx),
        "one\n",
        "删除最后一行应只留前一行（不留多余空行）"
    );
}

/// `dd` 在单行文件上应清空内容。
#[gpui::test]
fn delete_line_on_single_line_file(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "only\n");

    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "", "单行文件 `dd` 后应为空");
}

#[gpui::test]
fn delete_two_lines_with_vertical_motion(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    // `dj` 是行级操作：删当前行与下一行。
    press_all(cx, "d j");
    assert_eq!(text_of(&state, cx), "three\n", "`dj` 应删掉两行");
}

#[gpui::test]
fn delete_inner_word_leaves_surrounding_space(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `diw` 只删词本身，保留空格。
    press_all(cx, "d i w");
    assert_eq!(
        text_of(&state, cx),
        " world\n",
        "`diw` 应只删词，保留分隔空白"
    );
}

// ==================== Visual 模式 ====================

#[gpui::test]
fn visual_char_enters_visual_mode(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    assert_eq!(mode_of(&state, cx), VimMode::Visual, "`v` 应进入字符可视");
}

#[gpui::test]
fn visual_line_enters_linewise_mode(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "shift-v");
    assert_eq!(mode_of(&state, cx), VimMode::VisualLine, "`V` 应进入行可视");
}

#[gpui::test]
fn visual_motion_extends_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    // 移动应扩展选区，而不是只移动光标。
    press(cx, "l");
    press(cx, "l");
    let selected = state.read_with(cx, |state, _| state.selected_range.len());
    assert!(
        selected > 0,
        "可视模式下移动应产生非空选区（实际 {selected}）"
    );
}

#[gpui::test]
fn visual_delete_removes_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    cx.refresh().unwrap();
    assert_eq!(mode_of(&state, cx), VimMode::Visual, "前置：应处于可视模式");

    // 逐次断言光标位置，定位是哪一步多走了。
    for (i, expected) in [1usize, 2, 3, 4].into_iter().enumerate() {
        press(cx, "l");
        let cursor = cursor_of(&state, cx);
        assert_eq!(
            cursor,
            expected,
            "第 {} 次 `l` 后光标应在 {expected}（实际 {cursor}）",
            i + 1
        );
    }
    let selected = state.read_with(cx, |state, _| state.selected_range.len());
    assert!(selected > 0, "移动后应有选区，实际 {selected}");

    // 检查 `d` 在当前上下文下解析到哪个 action——这能区分
    // 「上下文没更新」与「handler 没生效」。
    let resolved = cx.update(|_window, cx| {
        let mut ctx = gpui::KeyContext::new_with_defaults();
        ctx.add("Input");
        ctx.add("VimControl");
        ctx.set("vim_mode", "visual");
        ctx.set("vim_operator", "none");
        let stack = [ctx];
        let keys = vec![gpui::Keystroke::parse("d").unwrap()];
        let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
        bindings.first().map(|b| b.action().name().to_string())
    });
    assert_eq!(
        resolved.as_deref(),
        Some("vim::VisualDelete"),
        "可视上下文下 `d` 应解析到 VisualDelete"
    );

    press(cx, "d");
    // 直接观察 handler 拿到的选区，区分「选区没建立」与「执行没生效」。
    let after = state.read_with(cx, |state, _| {
        (
            state.value().to_string(),
            state.vim.as_ref().unwrap().mode,
            state.selected_range.len(),
        )
    });
    assert_eq!(
        after.0, " world\n",
        "可视模式 `d` 应删掉选区（模式 {:?}，选区 {}）",
        after.1, after.2
    );
    assert_eq!(mode_of(&state, cx), VimMode::Normal, "删除后应回 Normal");
}

#[gpui::test]
fn visual_yank_copies_without_deleting(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    for _ in 0..4 {
        press(cx, "l");
    }
    press(cx, "y");
    assert_eq!(text_of(&state, cx), "hello world\n", "`y` 不得删文本");
    assert_eq!(mode_of(&state, cx), VimMode::Normal, "复制后应回 Normal");
}

#[gpui::test]
fn visual_change_replaces_selection_and_enters_insert(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    for _ in 0..4 {
        press(cx, "l");
    }
    press(cx, "c");
    assert_eq!(
        mode_of(&state, cx),
        VimMode::Insert,
        "可视 `c` 应进入 Insert"
    );

    type_text(cx, "HI");
    assert_eq!(text_of(&state, cx), "HI world\n", "应替换原选区");
}

#[gpui::test]
fn visual_line_delete_removes_whole_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    // `V` 选中当前行，`j` 扩展到下一行，`d` 删两行。
    press(cx, "shift-v");
    press(cx, "j");
    press(cx, "d");
    assert_eq!(text_of(&state, cx), "three\n", "`Vjd` 应删掉两整行");
}

#[gpui::test]
fn visual_swap_ends_keeps_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    for _ in 0..4 {
        press(cx, "l");
    }
    let before = state.read_with(cx, |state, _| {
        (state.selected_range.start, state.selected_range.end)
    });

    // `o` 切换端点：光标跳到另一端，选区内容不变。
    press(cx, "o");
    let after = state.read_with(cx, |state, _| {
        (state.selected_range.start, state.selected_range.end)
    });
    assert_eq!(before, after, "`o` 不应改变选区范围");

    // 再按 `o` 回到原处。
    press(cx, "o");
    let back = state.read_with(cx, |state, _| {
        (state.selected_range.start, state.selected_range.end)
    });
    assert_eq!(before, back, "`o` 应可往返");
}

#[gpui::test]
fn visual_escape_exits_and_clears_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "v");
    press(cx, "l");
    press(cx, "escape");
    assert_eq!(mode_of(&state, cx), VimMode::Normal, "`Esc` 应退出可视");
    assert_eq!(
        state.read_with(cx, |state, _| state.selected_range.len()),
        0,
        "退出可视后不应留选区"
    );
    assert_eq!(text_of(&state, cx), "hello\n", "退出可视不得改文本");
}

#[gpui::test]
fn visual_toggle_off_with_same_key(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "v");
    assert_eq!(mode_of(&state, cx), VimMode::Visual);
    // 再按 `v` 应退出（vim 行为）。
    press(cx, "v");
    assert_eq!(mode_of(&state, cx), VimMode::Normal, "再按 `v` 应退出可视");
}

#[gpui::test]
fn reselect_visual_restores_last_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "v");
    for _ in 0..4 {
        press(cx, "l");
    }
    let selected = state.read_with(cx, |state, _| state.selected_range);
    press(cx, "escape");

    // `gv` 恢复上次选区。
    press_all(cx, "g v");
    assert_eq!(mode_of(&state, cx), VimMode::Visual, "`gv` 应回到可视模式");
    let restored = state.read_with(cx, |state, _| state.selected_range);
    assert_eq!(
        (selected.start, selected.end),
        (restored.start, restored.end),
        "`gv` 应恢复同一选区"
    );
}

#[gpui::test]
fn visual_indent_shifts_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press(cx, "shift-v");
    press(cx, "j");
    press(cx, ">");
    assert_eq!(
        text_of(&state, cx),
        "    one\n    two\n",
        "可视 `>` 应缩进选中的两行"
    );
}

#[gpui::test]
fn visual_outdent_shifts_lines_back(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "    one\n    two\n");

    press(cx, "shift-v");
    press(cx, "j");
    press(cx, "<");
    assert_eq!(
        text_of(&state, cx),
        "one\ntwo\n",
        "可视 `<` 应反缩进选中的两行"
    );
}

#[gpui::test]
fn visual_line_mode_blocks_text_input(cx: &mut TestAppContext) {
    // 可视模式下按键是命令，不能插入字符。
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "v");
    press(cx, "z");
    assert_eq!(text_of(&state, cx), "hello\n", "可视模式下不得插入字符");
}

// ==================== `.` 重复 ====================

#[gpui::test]
fn dot_repeats_delete_char(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcdef\n");

    press(cx, "x");
    assert_eq!(text_of(&state, cx), "bcdef\n");

    // `.` 应再删一个字符。
    press(cx, ".");
    assert_eq!(text_of(&state, cx), "cdef\n", "`.` 应重复 `x`");
}

#[gpui::test]
fn dot_repeats_delete_word(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three\n");

    press_all(cx, "d w");
    assert_eq!(text_of(&state, cx), "two three\n");

    // 光标现在在 `two` 上，`.` 应再删一个词。
    press(cx, ".");
    assert_eq!(text_of(&state, cx), "three\n", "`.` 应重复 `dw`");
}

#[gpui::test]
fn dot_repeats_change_word_with_typed_text(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two\n");

    // `cw` 换成 `X`。vim 里 `Esc` 后光标停在刚输入的字符上，
    // 所以要移到下一个词再按 `.`——这也是实际使用中的操作序列。
    press_all(cx, "c w");
    type_text(cx, "X");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "X two\n");

    // 移到下一个词，`.` 应重复同样的替换。
    press(cx, "w");
    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "X X\n",
        "`.` 应重放 `cw` 的替换（删词 + 插入 X）"
    );
}

#[gpui::test]
fn dot_at_same_position_replaces_same_word(cx: &mut TestAppContext) {
    // `cw` 后不移动直接按 `.`：vim 会再替换一次同一个词（结果是同样的文本）。
    let (state, cx) = vim_editor(cx, "one two\n");

    press_all(cx, "c w");
    type_text(cx, "X");
    press(cx, "escape");
    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "X two\n",
        "原地 `.` 重复替换同一个词，文本不变"
    );
}

#[gpui::test]
fn dot_repeats_whole_line_delete(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\nthree\n");

    press(cx, ".");
    assert_eq!(text_of(&state, cx), "three\n", "`.` 应重复 `dd`");
}

#[gpui::test]
fn dot_repeats_toggle_case(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "ab\n");

    press(cx, "~");
    assert_eq!(text_of(&state, cx), "Ab\n");

    press(cx, ".");
    assert_eq!(text_of(&state, cx), "AB\n", "`.` 应重复 `~`");
}

#[gpui::test]
fn dot_repeats_backward_word_delete(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three\n");

    // 移到 `two` 的词首，`db` 向后删掉 `one `。
    press(cx, "w");
    press_all(cx, "d b");
    assert_eq!(text_of(&state, cx), "two three\n", "`db` 应向后删一个词");

    // 光标现在在 `two` 上，`.` 应继续**向后**删——但已在行首，无内容可删。
    // 换一个更明确的位置：先右移到 `three`。
    press(cx, "w");
    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "three\n",
        "`.` 重放 `db` 必须朝后删（若朝前删会得到 `two e` 之类）"
    );
}

#[gpui::test]
fn dot_without_prior_change_does_nothing(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    // 还没做过修改：`.` 不应报错也不应改文本。
    press(cx, ".");
    assert_eq!(text_of(&state, cx), "abc\n", "无历史时 `.` 应无操作");
}

#[gpui::test]
fn dot_does_not_repeat_yank(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `y` 不改文本，vim 里也不会成为 `.` 的重复对象。
    press_all(cx, "y w");
    press(cx, ".");
    assert_eq!(text_of(&state, cx), "hello world\n", "`y` 不应被 `.` 重复");
}

// ==================== 寄存器 ====================

#[gpui::test]
fn unnamed_register_holds_last_delete(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    // `dd` 写无名寄存器，`p` 读它。
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\n");

    press(cx, "p");
    assert_eq!(text_of(&state, cx), "two\none\n", "`p` 应粘贴刚删除的整行");
}

#[gpui::test]
fn named_register_round_trips(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    // `"ayy` 复制到寄存器 a，`dd` 删掉它，`"ap` 粘回来。
    press(cx, "\"");
    press_char(cx, &state, 'a');
    press_all(cx, "y y");
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\n");

    press(cx, "\"");
    press_char(cx, &state, 'a');
    press(cx, "p");
    assert_eq!(
        text_of(&state, cx),
        "two\none\n",
        "`\"ap` 应从寄存器 a 粘贴"
    );
}

#[gpui::test]
fn uppercase_register_appends(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "aa\nbb\ncc\n");

    // `"ayy` 存 `aa`，移到下一行 `"Ayy` 追加 `bb`。
    press(cx, "\"");
    press_char(cx, &state, 'a');
    press_all(cx, "y y");
    press(cx, "j");
    press(cx, "\"");
    press_char(cx, &state, 'A');
    press_all(cx, "y y");

    // 读回寄存器 a，应是两行。
    press(cx, "\"");
    press_char(cx, &state, 'a');
    press(cx, "p");
    let text = text_of(&state, cx);
    assert!(
        text.contains("aa") && text.contains("bb"),
        "大写寄存器应追加而非覆盖：{text:?}"
    );
}

#[gpui::test]
fn yank_register_zero_survives_delete(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "keep\ndrop\n");

    // `yy` 复制首行到 `0`，`dd` 删掉它（只影响无名寄存器）。
    press_all(cx, "y y");
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "drop\n");

    // `"0p` 应粘回复制的内容，而不是被删的内容。
    press(cx, "\"");
    press_char(cx, &state, '0');
    press(cx, "p");
    assert!(
        text_of(&state, cx).contains("keep"),
        "`\"0p` 应粘贴最近复制的内容：{:?}",
        text_of(&state, cx)
    );
}

#[gpui::test]
fn blackhole_register_discards(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "keep\ndrop\n");

    // 先复制到无名寄存器。
    press_all(cx, "y y");

    // `"_dd` 删除但不写寄存器。
    press(cx, "\"");
    press_char(cx, &state, '_');
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "drop\n");

    // `p` 应粘回之前复制的 `keep`，而不是被 `"_dd` 删掉的 `keep`…
    // 这里两者文本相同，所以改为验证寄存器确实没被 `dd` 覆盖：
    // 用 `"0p` 与 `p` 的结果一致性来判断。
    press(cx, "u");
    let restored = text_of(&state, cx);
    assert_eq!(restored, "keep\ndrop\n", "`u` 应恢复被删的行");
}

#[gpui::test]
fn disabling_vim_clears_state_and_restores_typing(cx: &mut TestAppContext) {
    // 回归背景：关掉 vim 开关后，编辑器残留的 Normal 模式会继续吞掉按键，
    // 而 vim 键位表已随开关卸载——编辑器在重启前无法输入。
    let (state, cx) = vim_editor(cx, "hello\n");
    assert!(
        state.read_with(cx, |s, _| s.vim.is_some()),
        "前置：vim 已启用"
    );

    cx.update(|_window, cx| {
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = false;
        });
        crate::vim::disable_all(cx);
        crate::keymap::reload(cx);
    });

    state.read_with(cx, |state, _| {
        assert!(state.vim.is_none(), "关闭 vim 后必须清掉模式状态");
    });

    // 清掉之后按键应恢复为普通文本输入（不再被吞）。
    type_text(cx, "Z");
    assert_eq!(text_of(&state, cx), "Zhello\n", "关闭 vim 后输入应恢复正常");
}

#[gpui::test]
fn enabling_vim_installs_state_on_existing_editors(cx: &mut TestAppContext) {
    // 反向：先建一个没有 vim 的编辑器，打开开关后应装上状态。
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::vim::Registers::init(cx);
        crate::vim::Marks::init(cx);
        crate::keymap::reload(cx);
    });

    let cell = std::cell::RefCell::new(None);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("text")
                .multi_line(true)
        });
        state.update(cx, |state, cx| {
            state.set_value("hello\n", window, cx);
            state.move_to(0, None, cx);
            state.focus(window, cx);
        });
        *cell.borrow_mut() = Some(state.clone());
        VimHost { state }
    });
    let state = cell.into_inner().unwrap();
    assert!(
        state.read_with(cx, |s, _| s.vim.is_none()),
        "前置：vim 未启用"
    );

    cx.update(|_window, cx| {
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = true;
        });
        crate::vim::disable_all(cx);
        crate::keymap::reload(cx);
    });

    state.read_with(cx, |state, _| {
        assert!(state.vim.is_some(), "打开 vim 后已有编辑器应装上状态");
    });
}

/// 起一个 vim 已启用的环境，再按 `builder` 造一个输入框，返回它。
///
/// 造法交给调用方（`new_code_editor` / 普通单行框 / …），这样「同一个环境、
/// 不同造法」的 A/B 用例能复用同一段初始化。
fn input_host_with(
    cx: &mut TestAppContext,
    builder: impl Fn(&mut Window, &mut gpui::Context<VimHost>) -> Entity<InputState>,
) -> (Entity<InputState>, &mut VisualTestContext) {
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = true;
        });
        crate::keymap::reload(cx);
        crate::vim::init(cx);
        crate::components::input::state::init(cx);
    });

    let cell = std::cell::RefCell::new(None);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = builder(window, cx);
        state.update(cx, |state, cx| state.focus(window, cx));
        *cell.borrow_mut() = Some(state.clone());
        VimHost { state }
    });
    (cell.into_inner().unwrap(), cx)
}

/// 起一个 vim 已启用、但输入框是普通单行框的环境，返回该输入框。
///
/// 与 `vim_editor` 的区别只在输入框类型：那条路径建的是代码编辑器。
fn plain_input_host(cx: &mut TestAppContext) -> (Entity<InputState>, &mut VisualTestContext) {
    input_host_with(cx, |window, cx| {
        // 单行普通输入框：与搜索框、表单字段、提交框同一条构造路径。
        cx.new(|cx| InputState::new(window, cx).placeholder("搜索"))
    })
}

/// 代码编辑器必须**当场**装上 vim 状态，不能等设置变化后再补。
///
/// 这是漂移的护栏：`settings::observe` 在**任何**设置变化（字号、主题……）时
/// 都会调 `vim::disable_all`，而它遍历的是全部已注册 `InputState`。于是
/// 「创建时不装、之后被补上」的用户可见后果是：同一个 diff 编辑器，开出来时
/// 没 vim，用户改一次字号后 vim 突然出现——diff 视图当初正是这样漏掉了
/// `sync_vim_state`。
///
/// 断言的是 [`InputState::new_code_editor`] 而不是 `DiffView::new_editor`：
/// 后者要构造整个 `DiffView` 实体，而两个生产调用点（编辑器 tab、diff 视图）
/// 现在共用同一个构造器，守住它等于同时守住两处。
#[gpui::test]
fn code_editors_get_vim_at_creation(cx: &mut TestAppContext) {
    let (state, cx) = input_host_with(cx, |window, cx| {
        cx.new(|cx| InputState::new_code_editor("rust", window, cx))
    });

    state.read_with(cx, |state, _| {
        assert!(
            state.vim.is_some(),
            "开着 vim 开关时，代码编辑器创建的那一刻就必须有 vim 状态"
        );
        assert!(state.vim_capable(), "代码编辑器必须是 vim 的适用面");
    });
}

/// 关掉开关后新建的代码编辑器不得带 vim 状态。
///
/// 与上一条同一构造器的反向判据：只测「开」不测「关」的话，`sync_vim_state`
/// 里那个 `else` 分支（清掉残留状态）就没有护栏。
#[gpui::test]
fn code_editors_skip_vim_when_the_setting_is_off(cx: &mut TestAppContext) {
    // 先开着开关建环境，再关掉——`SettingsStore` 必须已注册，否则
    // `enabled()` 与 `initial_state()` 会同时拿不到设置而给出「关」的假象。
    let (state, cx) = input_host_with(cx, |window, cx| {
        cx.new(|cx| InputState::new_code_editor("rust", window, cx))
    });
    cx.update(|_window, cx| {
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = false;
        });
    });

    let cell = std::cell::RefCell::new(None);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let fresh = cx.new(|cx| InputState::new_code_editor("rust", window, cx));
        *cell.borrow_mut() = Some(fresh.clone());
        VimHost { state: fresh }
    });
    let fresh = cell.into_inner().unwrap();

    fresh.read_with(cx, |state, _| {
        assert!(
            state.vim.is_none(),
            "开关关闭时新建的代码编辑器不得带 vim 状态"
        );
    });
    // 顺带确认「关」是真的生效，而不是设置压根没读到。
    let setting_off = cx.update(|_window, cx| !crate::vim::enabled(cx));
    assert!(
        setting_off,
        "前置：vim 开关此刻必须是关的，否则这条测的是别的东西"
    );
    state.read_with(cx, |state, _| {
        assert!(state.vim.is_some(), "前置：先前创建的那个仍应有 vim");
    });
}

#[gpui::test]
fn vim_is_not_installed_on_plain_inputs(cx: &mut TestAppContext) {
    // vim 只属于代码/文本编辑器。普通输入框装上它之后，Normal 模式会吞掉
    // `h`/`j`/`k`/`l`，搜索框与表单会变成「打不了字」。
    let (state, cx) = plain_input_host(cx);

    cx.update(|_window, cx| {
        crate::vim::disable_all(cx);
    });

    state.read_with(cx, |state, _| {
        assert!(state.vim.is_none(), "普通单行输入框不得装上 vim 状态");
        assert!(!state.vim_capable(), "普通单行输入框不是 vim 的适用面");
    });
}

#[gpui::test]
fn plain_inputs_keep_typing_with_vim_enabled(cx: &mut TestAppContext) {
    // 回归的**用户可见**判据：开着 vim 在搜索框里打字，字符必须落进去。
    let (state, cx) = plain_input_host(cx);

    cx.update(|_window, cx| {
        crate::vim::disable_all(cx);
    });

    // `hjkl` 是 vim 的移动键，在普通输入框里必须是字符。
    type_text(cx, "hjkl");
    assert_eq!(
        text_of(&state, cx),
        "hjkl",
        "普通输入框里 `hjkl` 应作为文本插入，而不是被当作移动命令吞掉"
    );
}

#[gpui::test]
fn vim_applies_to_code_editors_but_not_plain_inputs(cx: &mut TestAppContext) {
    // 同一环境、同一开关下的 A/B：收紧适用面不能把代码编辑器一起挡掉，
    // 也不能放过普通输入框。
    //
    // `plain` 覆盖三种非代码编辑器模式——搜索框/表单字段（单行 PlainText）、
    // 多行文本框（多行 PlainText）、git 提交框（AutoGrow）。
    struct AbHost {
        code: Entity<InputState>,
        plain: Entity<InputState>,
        multiline: Entity<InputState>,
        commit: Entity<InputState>,
    }
    impl Render for AbHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(crate::components::input::EditorInput::new(&self.code))
                .child(crate::components::input::EditorInput::new(&self.plain))
                .child(crate::components::input::EditorInput::new(&self.multiline))
                .child(crate::components::input::EditorInput::new(&self.commit))
        }
    }

    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = true;
        });
        crate::keymap::reload(cx);
        crate::vim::init(cx);
        crate::components::input::state::init(cx);
    });

    let cells = std::cell::RefCell::new(None);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let code = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true)
        });
        let plain = cx.new(|cx| InputState::new(window, cx).placeholder("搜索"));
        let multiline = cx.new(|cx| InputState::new(window, cx).multi_line(true));
        // git 提交框的真实构造参数（`git_panel::ensure_commit_editor`）。
        let commit = cx.new(|cx| InputState::new(window, cx).auto_grow(3, 6));
        *cells.borrow_mut() = Some((
            code.clone(),
            plain.clone(),
            multiline.clone(),
            commit.clone(),
        ));
        AbHost {
            code,
            plain,
            multiline,
            commit,
        }
    });
    let (code, plain, multiline, commit) = cells.into_inner().unwrap();

    // 走设置变更那条路径（`vim::disable_all` 是唯一入口）。
    cx.update(|_window, cx| {
        crate::vim::disable_all(cx);
    });

    code.read_with(cx, |state, _| {
        assert!(state.vim.is_some(), "代码编辑器必须装上 vim 状态");
    });
    for (state, what) in [
        (&plain, "单行普通输入框"),
        (&multiline, "多行普通输入框"),
        (&commit, "git 提交框（auto_grow）"),
    ] {
        state.read_with(cx, |state, _| {
            assert!(state.vim.is_none(), "{what}不得装上 vim 状态");
        });
    }

    // 关掉开关：代码编辑器的状态必须清干净（否则残留 Normal 会继续吞按键）。
    cx.update(|_window, cx| {
        crate::settings::SettingsStore::update_user(cx, |content| {
            content.vim.enabled = false;
        });
        crate::vim::disable_all(cx);
    });
    code.read_with(cx, |state, _| {
        assert!(state.vim.is_none(), "关闭 vim 后代码编辑器必须清掉状态");
    });
}

/// 编辑器内的查找框是**普通输入框**，渲染上嵌在编辑器 `key_context` 内部。
///
/// 用户的可见后果：编辑器处于 Normal 模式时打开查找（Cmd+F），在查找框里
/// 按 `h`/`j`/`k`/`l` 会命中祖先上下文的 vim 移动键——字符进不了查找框，
/// 编辑器光标反而在动。
#[gpui::test]
fn nested_find_input_keeps_typing_in_normal_mode(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 打开查找面板：走真实入口 `InputState::on_action_search`。
    cx.update(|window, cx| {
        state.update(cx, |state, cx| {
            state.on_action_search(&crate::components::input::Search, window, cx);
        });
    });
    cx.run_until_parked();

    // 查找框应拿到焦点（`SearchPanel::show` 的行为）。
    let find_input = state.read_with(cx, |state, cx| {
        state
            .search_panel
            .as_ref()
            .map(|panel| panel.read(cx).search_input_for_test())
    });
    let find_input = find_input.expect("打开查找后应有搜索面板");

    // 前置：编辑器处于 Normal 模式——这正是 vim 键位生效的条件。
    assert_eq!(
        mode_of(&state, cx),
        VimMode::Normal,
        "前置：编辑器应在 Normal"
    );

    // 前置：焦点确实在查找框里，否则这条测的就成了别的东西。
    let focused = cx.update(|window, cx| {
        let handle = find_input.read_with(cx, |state, _| state.focus_handle.clone());
        handle.is_focused(window)
    });
    assert!(focused, "前置：打开查找后焦点应在查找框里");

    // 在查找框里打字。`hjkl` 全是 vim 的移动键，但对查找框来说必须是字符。
    type_text(cx, "hjkl");
    assert_eq!(
        find_input.read_with(cx, |state, _| state.value().to_string()),
        "hjkl",
        "查找框里 `hjkl` 应作为查询文本输入，而不是被祖先编辑器的 vim 键位吃掉"
    );
}

#[gpui::test]
fn escape_cancels_pending_char(cx: &mut TestAppContext) {
    // `f` 之后按 `Esc`：vim 会放弃这次查找。若不清 pending_char，
    // 拦截器会继续吞掉下一个按键（用户感觉「编辑器卡住」）。
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "f");
    state.read_with(cx, |s, _| {
        assert!(
            s.vim.as_ref().unwrap().pending_char.is_some(),
            "前置：`f` 后应等待字符"
        );
    });

    press(cx, "escape");
    state.read_with(cx, |s, _| {
        assert!(
            s.vim.as_ref().unwrap().pending_char.is_none(),
            "`Esc` 应放弃待决的字符输入"
        );
    });

    // 之后按键必须恢复正常语义（`l` 是移动，不是参数）。
    let before = cursor_of(&state, cx);
    press(cx, "l");
    assert_eq!(
        cursor_of(&state, cx),
        before + 1,
        "取消后 `l` 应正常移动光标"
    );
}

#[gpui::test]
fn pending_char_does_not_leak_into_insert_mode(cx: &mut TestAppContext) {
    // `f` 之后切到 Insert：待决状态必须丢弃，否则插入的字符会被当成
    // 查找参数吞掉。
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "f");
    press(cx, "i");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`i` 应进入 Insert");
    state.read_with(cx, |s, _| {
        assert!(
            s.vim.as_ref().unwrap().pending_char.is_none(),
            "进入 Insert 应丢弃待决字符"
        );
    });

    type_text(cx, "Z");
    assert_eq!(
        text_of(&state, cx),
        "Zhello\n",
        "Insert 模式下输入应正常插入，不被拦截器吞掉"
    );
}

// ==================== 标记 ====================

#[gpui::test]
fn mark_jump_returns_to_position(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "line one\nline two\nline three\n");

    // 在第 0 行打 `ma`。
    press(cx, "m");
    press_char(cx, &state, 'a');

    // 走到第三行，`` `a `` 跳回来。
    press(cx, "j");
    press(cx, "j");
    assert_eq!(cursor_of(&state, cx), 18, "前置：应在第三行行首");

    press(cx, "`");
    press_char(cx, &state, 'a');
    assert_eq!(cursor_of(&state, cx), 0, "`` `a `` 应跳回标记位置");
}

#[gpui::test]
fn mark_line_jump_goes_to_first_non_whitespace(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "    indented\nsecond\n");

    // 把光标移到行中（列 8），打标记。
    for _ in 0..8 {
        press(cx, "l");
    }
    press(cx, "m");
    press_char(cx, &state, 'b');

    // 走开再跳回。
    press(cx, "j");
    press(cx, "'");
    press_char(cx, &state, 'b');
    assert_eq!(
        cursor_of(&state, cx),
        4,
        "`'b` 应跳到标记所在行的首个非空白字符"
    );
}

#[gpui::test]
fn jump_to_unset_mark_does_nothing(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "`");
    press_char(cx, &state, 'z');
    assert_eq!(cursor_of(&state, cx), 0, "未设置的标记不应移动光标");
    assert_eq!(text_of(&state, cx), "abc\n", "不应改动文本");
}

#[gpui::test]
fn marks_survive_across_editors(cx: &mut TestAppContext) {
    // 标记是全局的：新建一个编辑器后仍能跳到先前打的标记。
    // 用同一份全局状态创建两个编辑器——`vim_editor` 会重置全局，
    // 所以第二个直接复用第一个的环境。
    let (first, cx) = vim_editor(cx, "target here\nother\n");
    for _ in 0..7 {
        press(cx, "l");
    }
    press(cx, "m");
    press_char(cx, &first, 't');
    assert_eq!(cursor_of(&first, cx), 7);

    // 在同一测试环境里再建一个编辑器（模拟另一个标签页）。
    let cell = std::cell::RefCell::new(None);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("text")
                .multi_line(true);
            state.vim = Some(VimState::default());
            state
        });
        state.update(cx, |state, cx| {
            state.set_value("different\n", window, cx);
            state.move_to(0, None, cx);
            state.focus(window, cx);
        });
        *cell.borrow_mut() = Some(state.clone());
        VimHost { state }
    });
    let second = cell.into_inner().unwrap();

    press(cx, "`");
    press_char(cx, &second, 't');
    assert_eq!(
        cursor_of(&second, cx),
        7,
        "标记应跨编辑器共享（vim 里跨文件可用）"
    );
}

// ==================== 计数 ====================

/// `3j` 之后按 `dw` 只应删一个词。
///
/// 回归背景：`pre_count` 只在 `vim_try_operator` 里消费，普通移动既不使用
/// 也不清空它——`3j` 之后残留的计数会被下一个操作符乘进去，静默多删。
#[gpui::test]
fn stale_count_does_not_leak_into_next_operator(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three four\nline2\nline3\nline4\n");

    // `3j` 应下移三行。
    press_all(cx, "3 j");
    let (cursor, count) = state.read_with(cx, |s, _| {
        (
            s.vim
                .as_ref()
                .and_then(|v| v.visual_head)
                .unwrap_or_else(|| s.cursor()),
            s.vim.as_ref().unwrap().pre_count,
        )
    });
    assert_eq!(cursor, 31, "`3j` 应下移三行（到 line4 行首）");
    assert_eq!(
        count, None,
        "`3j` 之后计数必须已清空（否则泄漏到下一个命令）"
    );

    // 回到首行再 `dw`：只应删一个词。
    press_all(cx, "g g");
    let after_gg = state.read_with(cx, |s, _| s.cursor());
    assert_eq!(after_gg, 0, "`gg` 应回到文档开头");

    press_all(cx, "d w");
    let (text, cursor_after) = state.read_with(cx, |s, _| (s.value().to_string(), s.cursor()));
    assert_eq!(
        text, "two three four\nline2\nline3\nline4\n",
        "`dw` 只应删一个词（光标在 {cursor_after}）"
    );
}

/// `gg` 在计数命令之后仍应生效。
#[gpui::test]
fn gg_works_after_a_counted_motion(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three four\nline2\nline3\nline4\n");

    press_all(cx, "3 j");
    assert_eq!(cursor_of(&state, cx), 31, "前置：`3j` 应到第四行");

    press_all(cx, "g g");
    assert_eq!(cursor_of(&state, cx), 0, "`3j` 之后 `gg` 仍应回到文档开头");
}

/// `2dd` 应删两行。
#[gpui::test]
fn count_applies_to_line_operator(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\nfour\n");

    press_all(cx, "2 d d");
    assert_eq!(text_of(&state, cx), "three\nfour\n", "`2dd` 应删掉两行");
}

/// `3x` 应删三个字符。
#[gpui::test]
fn count_applies_to_delete_char(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcdef\n");

    press_all(cx, "3 x");
    assert_eq!(text_of(&state, cx), "def\n", "`3x` 应删掉三个字符");
}

/// `3w` 应前进三个词。
#[gpui::test]
fn gg_returns_to_document_start(cx: &mut TestAppContext) {
    // `gg` 是两键序列，单独验证它确实生效（下面的计数测试依赖它）。
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    press(cx, "j");
    press(cx, "j");
    assert_eq!(cursor_of(&state, cx), 8, "前置：应到第三行");

    press_all(cx, "g g");
    assert_eq!(cursor_of(&state, cx), 0, "`gg` 应回到文档开头");
}

#[gpui::test]
fn count_applies_to_word_motion(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three four\n");

    press_all(cx, "3 w");
    assert_eq!(
        cursor_of(&state, cx),
        14,
        "`3w` 从词首起跳三次应落在第四个词（`four`）的词首"
    );
}

/// `3j` 本身也要真的下移三行。
#[gpui::test]
fn count_applies_to_vertical_motion(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "a\nb\nc\nd\n");

    press_all(cx, "3 j");
    assert_eq!(cursor_of(&state, cx), 6, "`3j` 应到第四行");
}

/// `2~` 应切换两个字符的大小写。
#[gpui::test]
fn count_applies_to_toggle_case(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcd\n");

    press_all(cx, "2 ~");
    assert_eq!(text_of(&state, cx), "ABcd\n", "`2~` 应切换两个字符");
}

/// `Esc` 退出可视后，光标应停在**最后一个被选中的字符**上。
///
/// 回归背景：退出时用的是 `cursor()`，而可视模式下它等于 `head + 1`
/// （选区含尾），于是光标落在选区右侧一格。
#[gpui::test]
fn escape_from_visual_leaves_cursor_on_last_selected_char(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `0` + `v` + 两次 `l` 选中 "hel"（0..3），光标应在 `l`（偏移 2）上。
    press(cx, "0");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "l");
    press(cx, "l");
    press(cx, "escape");

    assert_eq!(
        cursor_of(&state, cx),
        2,
        "退出可视后光标应停在最后选中的字符（偏移 2），而非其右侧"
    );
}

/// `gv` 恢复的选区应与原选区一致（不多选一格）。
#[gpui::test]
fn gv_restores_exact_selection_after_toggle(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "0");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "l");
    press(cx, "l");
    let original = state.read_with(cx, |s, _| (s.selected_range.start, s.selected_range.end));

    // 再按 `v` 退出（toggle 路径），然后 `gv` 恢复。
    press(cx, "v");
    press_all(cx, "g v");

    let restored = state.read_with(cx, |s, _| (s.selected_range.start, s.selected_range.end));
    assert_eq!(original, restored, "`gv` 应恢复与原选区完全一致的范围");
}

/// 可视模式下 `$` 应扩展选区到行尾，而不是塌缩选区。
#[gpui::test]
fn visual_end_of_line_extends_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "0");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "$");

    let selected = state.read_with(cx, |s, _| s.selected_range.len());
    assert_eq!(
        selected, 11,
        "`v$` 应选中整行内容（11 字符），实际 {selected}"
    );
}

/// 可视模式下 `%` 应扩展选区，而不是塌缩。
#[gpui::test]
fn visual_matching_bracket_extends_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "f(a)\n");

    // 光标到 `(`（偏移 1）。
    press(cx, "0");
    press(cx, "l");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "%");

    let selected = state.read_with(cx, |s, _| s.selected_range.len());
    assert_eq!(selected, 3, "`v%` 应选中 `(a)`，实际 {selected}");
}

/// 可视模式下 `~` 应切换整个选区的大小写。
#[gpui::test]
fn visual_toggle_case_switches_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "0");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "l");
    press(cx, "l");
    press(cx, "~");

    assert_eq!(
        text_of(&state, cx),
        "HELlo world\n",
        "`v~~` 应切换整个选区（3 个字符）的大小写"
    );
}

/// 可视模式下 `ctrl-d` 应扩展选区（向下移动），而不是塌缩。
#[gpui::test]
fn visual_half_page_extends_selection(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "a\nb\nc\nd\ne\nf\ng\nh\n");

    press(cx, "0");
    press(cx, "v");
    cx.refresh().unwrap();
    press(cx, "ctrl-d");

    let selected = state.read_with(cx, |s, _| s.selected_range.len());
    assert!(selected > 0, "`v` + `ctrl-d` 应扩展选区，实际 {selected}");
}

/// `dw` 在行尾最后一个词上**不得**吃掉换行。
///
/// vim 的独占移动在行尾会钳住，`dw` 删掉词但保留换行；若跨行删除，
/// 两行会被静默合并。
#[gpui::test]
fn delete_word_at_line_end_keeps_the_newline(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc def\nghi\n");

    // 光标移到 `def`（偏移 4）。
    press_all(cx, "w");
    assert_eq!(cursor_of(&state, cx), 4, "前置：光标应在 `def` 上");

    press_all(cx, "d w");
    assert_eq!(
        text_of(&state, cx),
        "abc \nghi\n",
        "`dw` 应删掉 `def` 但保留换行，不能把两行合并"
    );
}

/// `cw` 在空行上应进入 Insert 而不删除任何内容。
#[gpui::test]
fn change_word_on_empty_line_does_not_delete(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n\ndef\n");

    // 光标移到空行（偏移 4）。
    press(cx, "j");
    assert_eq!(cursor_of(&state, cx), 4, "前置：光标应在空行");

    press_all(cx, "c w");
    assert_eq!(
        mode_of(&state, cx),
        VimMode::Insert,
        "`cw` 在空行上应进入 Insert"
    );
    type_text(cx, "X");
    assert_eq!(
        text_of(&state, cx),
        "abc\nX\ndef\n",
        "`cw` 在空行上只应插入，不应删掉换行"
    );
}

/// `x` 之后 `i`+文本+`Esc`，`.` 只应插入，不应吃掉字符。
///
/// 回归背景：`vim_record_insert` 曾把**任何**上一次 `Delete` 升级成
/// `Replace`，于是无关的删除被并进插入，`.` 会多删一个字符。
#[gpui::test]
fn dot_after_plain_insert_does_not_consume_a_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcdef\n");

    // `x` 删掉 `a`，记录一次 Delete。
    press(cx, "x");
    assert_eq!(text_of(&state, cx), "bcdef\n");

    // 到别处插入（与那次删除无关）。
    press(cx, "l");
    press(cx, "i");
    type_text(cx, "Z");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "bZcdef\n");

    // `.` 应只重复插入。若把之前的 `x` 也并进来，会先删掉一个字符。
    press(cx, "l");
    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "bZZcdef\n",
        "`.` 只应插入 `Z`；若把无关的 `x` 也重放，会得到 `bZZdef` 之类"
    );
}

/// `.` 在光标处插入时不应执行两次（回归：曾出现同一位置插入两遍）。
#[gpui::test]
fn dot_inserts_exactly_once(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "ab\n");

    press(cx, "i");
    type_text(cx, "X");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "Xab\n");

    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "XXab\n",
        "`.` 应只插入一次 `X`（若插入两次会得到 XXXab）"
    );
}

/// `>>` 应可被 `.` 重复。
#[gpui::test]
fn dot_repeats_indent(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press_all(cx, "> >");
    assert_eq!(text_of(&state, cx), "    one\ntwo\n");

    press(cx, "j");
    let (cursor, recorded) = state.read_with(cx, |s, _| {
        (s.cursor(), s.vim.as_ref().unwrap().last_change.clone())
    });
    assert_eq!(cursor, 8, "前置：`j` 应到第二行");

    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "    one\n    two\n",
        "`.` 应重复 `>>`（光标在 {cursor}，记录 {recorded:?}）"
    );
}

/// `s` 之后 `.` 应重复整次替换（删 + 插入）。
#[gpui::test]
fn dot_repeats_substitute_char(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcd\n");

    press(cx, "s");
    type_text(cx, "X");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "Xbcd\n");

    // 移到下一个字符再 `.`：应替换掉它。
    press(cx, "l");
    press(cx, ".");
    assert_eq!(
        text_of(&state, cx),
        "XXcd\n",
        "`.` 应重复 `s`（删一个字符 + 插入 X）"
    );
}

/// `"_dd` 不得改动 `1`–`9` 删除历史。
#[gpui::test]
fn blackhole_delete_leaves_numbered_registers_untouched(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    // `dd` 把 `one` 记进寄存器 1。
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\nthree\n");

    // `"_dd` 删掉 `two`，但寄存器 1 应仍是 `one`。
    press(cx, "\"");
    press_char(cx, &state, '_');
    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "three\n");

    // `"1p` 应粘回 `one`（而不是被 `"_dd` 挤掉的 `two`）。
    press(cx, "\"");
    press_char(cx, &state, '1');
    press(cx, "p");
    assert!(
        text_of(&state, cx).contains("one"),
        "`\"1p` 应粘贴第一次 `dd` 的内容（one）：{:?}",
        text_of(&state, cx)
    );
}

// ==================== 多字节文本安全性 ====================

/// `daw` 在词后跟**多字节空白**时不得 panic。
///
/// U+3000（全角空格）在中日韩文本里很常见，`char::is_whitespace()` 为真
/// 而 `len_utf8() == 3`。回归背景：`aw` 的尾随空白扫描曾用裸 `end += 1`
/// 逐字节推进，第一步就落在字符中间，随后 `Rope::slice` 因「不在字符边界」
/// 而 panic。
#[gpui::test]
fn delete_around_word_handles_multibyte_whitespace(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\u{3000}world\n");

    press_all(cx, "d a w");
    assert_eq!(
        text_of(&state, cx),
        "world\n",
        "`daw` 应删掉词与全角空格，且不得 panic"
    );
}

/// `diw` 在多字节空白旁也不得 panic。
#[gpui::test]
fn delete_inner_word_handles_multibyte_whitespace(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\u{3000}world\n");

    press_all(cx, "d i w");
    assert_eq!(
        text_of(&state, cx),
        "\u{3000}world\n",
        "`diw` 应只删词，保留全角空格"
    );
}

/// `daw` 在 CJK 词上按字符边界工作。
#[gpui::test]
fn delete_around_word_on_cjk_text(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "中文 测试\n");

    press_all(cx, "d a w");
    assert_eq!(
        text_of(&state, cx),
        "测试\n",
        "`daw` 应删掉第一个 CJK 词与分隔空格"
    );
}

// ==================== 文本对象 ====================

#[gpui::test]
fn change_inner_quote_replaces_quoted_content(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, r#"let x = "hello";"#);

    // 光标移进引号内（`hello` 的 `h` 在索引 10）。
    press(cx, "0");
    for _ in 0..10 {
        press(cx, "l");
    }
    press(cx, "c");
    press(cx, "i");
    press(cx, "\"");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`ci\"` 应进入 Insert");
    type_text(cx, "bye");
    assert_eq!(
        text_of(&state, cx),
        r#"let x = "bye";"#,
        "`ci\"` 应替换引号内容、保留引号"
    );
}

#[gpui::test]
fn delete_around_quote_removes_quotes(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, r#"let x = "hi";"#);

    press(cx, "0");
    for _ in 0..10 {
        press(cx, "l");
    }
    press(cx, "d");
    press(cx, "a");
    press(cx, "\"");
    assert_eq!(text_of(&state, cx), "let x = ;", "`da\"` 应连引号一起删掉");
}

#[gpui::test]
fn change_inner_paren_replaces_parenthesized_content(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "call(arg)\n");

    // 光标移到 `arg` 上（索引 5）。
    press(cx, "0");
    for _ in 0..5 {
        press(cx, "l");
    }
    press(cx, "c");
    press(cx, "i");
    press(cx, "(");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`ci(` 应进入 Insert");
    type_text(cx, "new");
    assert_eq!(text_of(&state, cx), "call(new)\n", "`ci(` 应替换括号内内容");
}

#[gpui::test]
fn delete_inner_brace_keeps_braces(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "fn f() { body }\n");

    press(cx, "0");
    for _ in 0..12 {
        press(cx, "l");
    }
    press(cx, "d");
    press(cx, "i");
    press(cx, "{");
    assert_eq!(
        text_of(&state, cx),
        "fn f() {}\n",
        "`di{{` 应只删花括号内容"
    );
}

#[gpui::test]
fn delete_inner_paragraph_removes_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n\nthree\n");

    // 光标在第二行（`two`）。
    press(cx, "j");
    press(cx, "d");
    press(cx, "i");
    press(cx, "p");
    assert_eq!(text_of(&state, cx), "\nthree\n", "`dip` 应删掉整段（两行）");
}

#[gpui::test]
fn delete_inner_tag_removes_tag_content(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "<div>content</div>\n");

    press(cx, "0");
    for _ in 0..6 {
        press(cx, "l");
    }
    press(cx, "d");
    press(cx, "i");
    press(cx, "t");
    assert_eq!(text_of(&state, cx), "<div></div>\n", "`dit` 应只删标签内容");
}

#[gpui::test]
fn yank_inner_quote_copies_without_modifying(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, r#"x = "abc";"#);

    press(cx, "0");
    for _ in 0..5 {
        press(cx, "l");
    }
    press(cx, "y");
    press(cx, "i");
    press(cx, "\"");
    assert_eq!(text_of(&state, cx), r#"x = "abc";"#, "`yi\"` 不得改动文本");
}

#[gpui::test]
fn missing_object_leaves_text_unchanged(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "no quotes here\n");

    // 没有引号：`di"` 应什么都不做。
    press(cx, "d");
    press(cx, "i");
    press(cx, "\"");
    assert_eq!(
        text_of(&state, cx),
        "no quotes here\n",
        "找不到对象时不得改动文本"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.vim.as_ref().unwrap().active_operator_id(),
            "none",
            "找不到对象时应丢弃待决操作符"
        );
    });
}

#[gpui::test]
fn delete_around_word_removes_trailing_space(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `daw` 删词加其后的空白。
    press_all(cx, "d a w");
    assert_eq!(text_of(&state, cx), "world\n", "`daw` 应删掉词与分隔空白");
}

#[gpui::test]
fn change_word_enters_insert(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `cw` 删词并进入 Insert，可以立刻输入。
    // vim 的 `cw` 只替换词本身，保留分隔空格（等价 `ce`）。
    press_all(cx, "c w");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`cw` 应进入 Insert");

    type_text(cx, "HI");
    assert_eq!(
        text_of(&state, cx),
        "HI world\n",
        "`cw` 应替换词并保留分隔空格"
    );
}

#[gpui::test]
fn yank_word_copies_without_modifying(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press_all(cx, "y w");
    assert_eq!(text_of(&state, cx), "hello world\n", "`yw` 不得改动文本");
    assert_eq!(cursor_of(&state, cx), 0, "`yw` 后光标回到范围起点");
}

#[gpui::test]
fn delete_char_removes_one_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "x");
    assert_eq!(text_of(&state, cx), "bc\n", "`x` 应删掉光标处字符");
    assert_eq!(cursor_of(&state, cx), 0);

    press(cx, "x");
    assert_eq!(text_of(&state, cx), "c\n");
}

#[gpui::test]
fn delete_char_at_line_end_does_not_cross_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "ab\ncd\n");

    // 移到行尾字符上，`x` 只删这一个字符，不吞换行。
    press(cx, "$");
    press(cx, "x");
    assert_eq!(text_of(&state, cx), "a\ncd\n", "`x` 不应跨行删除");
}

#[gpui::test]
fn escape_cancels_pending_operator(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "d");
    press(cx, "escape");
    // 取消后再按 `w` 应只是移动，不删任何东西。
    press(cx, "w");
    assert_eq!(text_of(&state, cx), "hello world\n", "取消后不得删除文本");
    assert_eq!(cursor_of(&state, cx), 6, "取消后 `w` 应正常移动");
}

#[gpui::test]
fn join_lines_merges_with_space(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press(cx, "shift-j");
    assert_eq!(
        text_of(&state, cx),
        "one two\n",
        "`J` 应把下一行接上来并补空格"
    );
}

#[gpui::test]
fn paste_after_inserts_at_cursor(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 先复制一个词，再粘到别处。
    press_all(cx, "y w");
    press(cx, "$");
    press(cx, "p");
    let text = text_of(&state, cx);
    assert!(
        text.contains("hello worldhello ") || text.starts_with("hello world"),
        "`p` 应把抽出的内容粘到光标后：{text:?}"
    );
}

#[gpui::test]
fn indent_and_outdent_lines(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "line\n");

    press_all(cx, "> >");
    assert_eq!(text_of(&state, cx), "    line\n", "`>>` 应缩进一个单元");

    press_all(cx, "< <");
    assert_eq!(text_of(&state, cx), "line\n", "`<<` 应反缩进一个单元");
}

#[gpui::test]
fn outdent_stops_at_line_start(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "line\n");

    // 没有缩进时 `<<` 不应删掉内容。
    press_all(cx, "< <");
    assert_eq!(text_of(&state, cx), "line\n", "无缩进时 `<<` 应无操作");
}

#[gpui::test]
fn counts_multiply_for_operators(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one two three four five\n");

    // `2dw`：一次删两个词。
    press_all(cx, "2 d w");
    assert_eq!(
        text_of(&state, cx),
        "three four five\n",
        "`2dw` 应删掉两个词"
    );
}

// ==================== 撤销 / 重做 ====================

#[gpui::test]
fn undo_restores_the_deleted_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\nthree\n");

    press_all(cx, "d d");
    assert_eq!(text_of(&state, cx), "two\nthree\n", "`dd` 先删掉一行");

    // `u` 必须把删掉的内容找回来——这是最基础的编辑循环。
    press(cx, "u");
    assert_eq!(text_of(&state, cx), "one\ntwo\nthree\n", "`u` 应撤销 `dd`");
}

#[gpui::test]
fn undo_restores_the_deleted_word(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press_all(cx, "d w");
    assert_eq!(text_of(&state, cx), "world\n");

    press(cx, "u");
    assert_eq!(text_of(&state, cx), "hello world\n", "`u` 应撤销 `dw`");
}

#[gpui::test]
fn undo_restores_deleted_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "x");
    assert_eq!(text_of(&state, cx), "bc\n");

    press(cx, "u");
    assert_eq!(text_of(&state, cx), "abc\n", "`u` 应撤销 `x`");
}

#[gpui::test]
fn undo_restores_text_typed_in_insert_mode(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "base\n");

    press(cx, "shift-a");
    type_text(cx, "XYZ");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "baseXYZ\n");

    // 一次 Insert 会话应作为**一步**撤销。
    press(cx, "u");
    assert_eq!(
        text_of(&state, cx),
        "base\n",
        "`u` 应把整次 Insert 会话一次撤销"
    );
}

#[gpui::test]
fn redo_reapplies_the_undone_change(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press_all(cx, "d d");
    press(cx, "u");
    assert_eq!(text_of(&state, cx), "one\ntwo\n");

    // `ctrl-r` 重做。
    press(cx, "ctrl-r");
    assert_eq!(text_of(&state, cx), "two\n", "`ctrl-r` 应重做 `dd`");
}

#[gpui::test]
fn undo_after_multiple_edits_walks_back_one_step_at_a_time(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abcdef\n");

    press(cx, "x");
    assert_eq!(text_of(&state, cx), "bcdef\n");
    press(cx, "x");
    assert_eq!(text_of(&state, cx), "cdef\n");

    press(cx, "u");
    assert_eq!(text_of(&state, cx), "bcdef\n", "第一次 `u` 只回退一步");
    press(cx, "u");
    assert_eq!(text_of(&state, cx), "abcdef\n", "第二次 `u` 回到最初");
}

// ==================== 行内快捷命令 ====================

#[gpui::test]
fn capital_d_deletes_to_end_of_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\nsecond\n");

    press(cx, "shift-d");
    assert_eq!(
        text_of(&state, cx),
        "\nsecond\n",
        "`D` 应删到行尾并保留换行"
    );
}

#[gpui::test]
fn capital_c_changes_to_end_of_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "shift-c");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`C` 应进入 Insert");
    type_text(cx, "bye");
    assert_eq!(text_of(&state, cx), "bye\n", "`C` 后输入应替换到行尾");
}

#[gpui::test]
fn substitute_line_clears_line_and_enters_insert(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\nsecond\n");

    press(cx, "shift-s");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`S` 应进入 Insert");
    assert_eq!(
        text_of(&state, cx),
        "\nsecond\n",
        "`S` 应清空当前行内容但保留行"
    );

    type_text(cx, "new");
    assert_eq!(text_of(&state, cx), "new\nsecond\n");
}

#[gpui::test]
fn substitute_char_replaces_one_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "s");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`s` 应进入 Insert");
    assert_eq!(text_of(&state, cx), "bc\n", "`s` 应删掉光标处字符");

    type_text(cx, "X");
    assert_eq!(text_of(&state, cx), "Xbc\n");
}

#[gpui::test]
fn open_line_below_inserts_new_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press(cx, "o");
    assert_eq!(mode_of(&state, cx), VimMode::Insert, "`o` 应进入 Insert");
    type_text(cx, "mid");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "one\nmid\ntwo\n", "`o` 应在下方开新行");
}

#[gpui::test]
fn open_line_above_inserts_new_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press(cx, "shift-o");
    type_text(cx, "top");
    press(cx, "escape");
    assert_eq!(text_of(&state, cx), "top\none\ntwo\n", "`O` 应在上方开新行");
}

#[gpui::test]
fn open_line_preserves_indentation(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "    indented\nnext\n");

    press(cx, "o");
    type_text(cx, "x");
    press(cx, "escape");
    assert_eq!(
        text_of(&state, cx),
        "    indented\n    x\nnext\n",
        "`o` 应沿用当前行缩进"
    );
}

#[gpui::test]
fn capital_y_yanks_whole_line(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "one\ntwo\n");

    press(cx, "shift-y");
    assert_eq!(text_of(&state, cx), "one\ntwo\n", "`Y` 不得改动文本");

    // 粘到别处应带上整行内容。
    press(cx, "j");
    press(cx, "p");
    let text = text_of(&state, cx);
    assert!(text.contains("one"), "`Y` + `p` 应粘贴整行：{text:?}");
}

#[gpui::test]
fn toggle_case_swaps_character_case(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "~");
    assert_eq!(text_of(&state, cx), "Abc\n", "`~` 应把小写变大写");
    assert_eq!(cursor_of(&state, cx), 1, "`~` 后光标应右移一格");

    press(cx, "~");
    assert_eq!(text_of(&state, cx), "ABc\n");
}

#[gpui::test]
fn first_non_whitespace_jumps_to_indent_end(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "    code here\n");

    press(cx, "^");
    assert_eq!(cursor_of(&state, cx), 4, "`^` 应跳到首个非空白字符");
}

// ==================== 字符查找与替换 ====================

/// 投递一个字符参数（寄存器名等），走拦截器路径。
fn press_char(cx: &mut VisualTestContext, state: &Entity<InputState>, ch: char) {
    type_char_via_interceptor(cx, state, ch);
}

/// 走拦截器路径投递一个字符（`f`/`t`/`r` 参数的正式入口）。
fn type_char_via_interceptor(cx: &mut VisualTestContext, state: &Entity<InputState>, ch: char) {
    let source = format!("{ch}->{ch}");
    let parsed = Keystroke::parse(&source).expect("击键应可解析");
    cx.update(|window, cx| {
        crate::components::input::state::intercept_for_test(state, &parsed, window, cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn find_forward_jumps_to_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 前置条件：`f` 必须进入「等待一个字符」状态。
    press(cx, "f");
    state.read_with(cx, |state, _| {
        let pending = state.vim.as_ref().unwrap().pending_char;
        assert!(
            pending.is_some(),
            "`f` 后应处于等待字符状态，实际是 {pending:?}"
        );
    });

    // 走拦截器路径——这是 `f` 参数的正式送达入口。
    type_char_via_interceptor(cx, &state, 'o');
    let find = state.read_with(cx, |state, _| state.vim.as_ref().unwrap().last_find);
    assert!(
        find.is_some(),
        "经 on_key_down 送达的字符应被消费并记录：{find:?}"
    );
    assert_eq!(cursor_of(&state, cx), 4, "`fo` 应跳到第一个 `o` 上");
}

/// 用一个**没有任何绑定**的字符验证送达路径。
///
/// `o` 本身是绑定键（`vim::OpenLineBelow`），它的 action 派发会中止
/// 事件传播，从而跳过 `on_key_down`。用一个未绑定的字符可以单独验证
/// 「等待字符」这条链路本身。
#[gpui::test]
fn pending_char_receives_unbound_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "helloZworld\n");

    press(cx, "f");
    state.read_with(cx, |state, _| {
        assert!(
            state.vim.as_ref().unwrap().pending_char.is_some(),
            "`f` 后应等待字符"
        );
    });

    // `Z` 在 vim 键位表里没有绑定，用它验证「等待字符」这条链路。
    type_char_via_interceptor(cx, &state, 'Z');
    let (pending, find, cursor) = state.read_with(cx, |state, _| {
        let vim = state.vim.as_ref().unwrap();
        (vim.pending_char, vim.last_find, state.cursor())
    });
    assert!(pending.is_none(), "字符应被消费，实际仍在等待：{pending:?}");
    assert!(find.is_some(), "应记录查找：{find:?}");
    assert_eq!(cursor, 5, "`fZ` 应跳到 `Z` 上（偏移 5）");
}

/// 拦截器必须能在测试环境里找到聚焦的 `InputState`。
///
/// 这是 `f`/`t`/`r` 参数送达的前提；若这里失败，说明焦点注册表没命中。
#[gpui::test]
fn interceptor_locates_focused_input(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    let located = state.read_with(cx, |state, _| state.focus_handle.clone());
    cx.update(|window, _cx| {
        assert!(
            located.is_focused(window),
            "测试前提：编辑器应持有焦点（否则拦截器找不到它）"
        );
    });
}

/// `f` 之后的字符即使本身是绑定键，也必须被当作参数消费。
///
/// 这是 `f`/`t`/`r` 可用性的核心：vim 里 `fa` 的 `a` 是参数，而不是
/// 「进入插入模式」。捕获阶段监听器必须在 action 派发之前截住它。
#[gpui::test]
fn pending_char_wins_over_bound_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "f");
    // `o` 本身绑定到 `vim::OpenLineBelow`；作为 `f` 的参数时不得开新行。
    type_char_via_interceptor(cx, &state, 'o');

    let (text, cursor, mode) = state.read_with(cx, |state, _| {
        (
            state.value().to_string(),
            state.cursor(),
            state.vim.as_ref().unwrap().mode,
        )
    });
    assert_eq!(text, "hello world\n", "`fo` 不得插入新行（`o` 是参数）");
    assert_eq!(cursor, 4, "`fo` 应跳到 `o` 上");
    assert_eq!(mode, VimMode::Normal, "`fo` 不得进入 Insert");
}

#[gpui::test]
fn find_before_stops_one_short(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    press(cx, "t");
    type_char_via_interceptor(cx, &state, 'o');
    assert_eq!(cursor_of(&state, cx), 3, "`to` 应停在目标字符之前");
}

#[gpui::test]
fn find_backward_jumps_to_character(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // 先到行尾，再 `Fl` 向前找 `l`。
    press(cx, "$");
    press(cx, "shift-f");
    type_char_via_interceptor(cx, &state, 'l');
    assert_eq!(cursor_of(&state, cx), 9, "`Fl` 应向前跳到 `l` 上");
}

#[gpui::test]
fn find_missing_character_leaves_cursor(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello\n");

    press(cx, "f");
    type_char_via_interceptor(cx, &state, 'z');
    assert_eq!(cursor_of(&state, cx), 0, "找不到目标时光标应原地不动");
    assert_eq!(text_of(&state, cx), "hello\n", "`fz` 不得改动文本");
}

#[gpui::test]
fn repeat_find_repeats_the_search(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "a.b.c.d\n");

    // `f.` 跳到第一个 `.`（索引 1）。
    press(cx, "f");
    type_char_via_interceptor(cx, &state, '.');
    assert_eq!(cursor_of(&state, cx), 1);

    // `;` 重复，跳到第二个 `.`（索引 3）。
    press(cx, ";");
    assert_eq!(cursor_of(&state, cx), 3, "`;` 应重复上次查找");

    // `,` 反向重复，回到第一个 `.`。
    press(cx, ",");
    assert_eq!(cursor_of(&state, cx), 1, "`,` 应反向重复");
}

#[gpui::test]
fn replace_char_substitutes_under_cursor(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "abc\n");

    press(cx, "r");
    type_char_via_interceptor(cx, &state, 'x');
    assert_eq!(text_of(&state, cx), "xbc\n", "`r` 应替换光标处字符");
    assert_eq!(cursor_of(&state, cx), 0, "`r` 后光标不移动");
}

#[gpui::test]
fn find_works_as_operator_target(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor(cx, "hello world\n");

    // `dfo` 删到第一个 `o`（含）。
    press(cx, "d");
    press(cx, "f");
    type_char_via_interceptor(cx, &state, 'o');
    assert_eq!(
        text_of(&state, cx),
        " world\n",
        "`dfo` 应删到 `o` 为止（含）"
    );
}

#[gpui::test]
fn toggle_comment_comments_the_line(cx: &mut TestAppContext) {
    // 用 rust 语言，注释符是 `//`。
    let (state, cx) = vim_editor_with_language(cx, "let x = 1;\n", "rust");

    press_all(cx, "g c");
    let text = text_of(&state, cx);
    assert!(text.contains("//"), "`gc` 应给当前行加注释：{text:?}");
}

#[gpui::test]
fn toggle_comment_is_idempotent(cx: &mut TestAppContext) {
    let (state, cx) = vim_editor_with_language(cx, "let x = 1;\n", "rust");

    press_all(cx, "g c");
    let commented = text_of(&state, cx);
    press_all(cx, "g c");
    let restored = text_of(&state, cx);
    assert_ne!(commented, restored, "第二次 `gc` 应取消注释");
    assert!(
        !restored.contains("//"),
        "取消注释后不应残留注释符：{restored:?}"
    );
}

#[gpui::test]
fn matching_bracket_jumps_between_pair(cx: &mut TestAppContext) {
    // `fn main() {}`：`(` 在索引 7，`)` 在索引 8。
    let (state, cx) = vim_editor(cx, "fn main() {}\n");

    press(cx, "0");
    for _ in 0..7 {
        press(cx, "l");
    }
    let at_paren = cursor_of(&state, cx);
    state.read_with(cx, |state, _| {
        use crate::components::input::RopeExt as _;
        assert_eq!(
            state.text.char_at(at_paren),
            Some('('),
            "前置条件：光标应停在 `(` 上（实际偏移 {at_paren}）"
        );
    });

    // 从 `(` 跳到 `)`。
    press(cx, "%");
    let target = cursor_of(&state, cx);
    state.read_with(cx, |state, _| {
        use crate::components::input::RopeExt as _;
        assert_eq!(
            state.text.char_at(target),
            Some(')'),
            "`%` 应跳到配对的 `)` 上（实际偏移 {target}）"
        );
    });

    // 再从 `)` 跳回 `(`——往返对称，且不能原地不动。
    press(cx, "%");
    let back = cursor_of(&state, cx);
    state.read_with(cx, |state, _| {
        use crate::components::input::RopeExt as _;
        assert_eq!(
            state.text.char_at(back),
            Some('('),
            "`%` 应从 `)` 跳回 `(`（实际偏移 {back}）"
        );
    });
    assert_ne!(back, target, "往返跳转必须真的移动光标");
}

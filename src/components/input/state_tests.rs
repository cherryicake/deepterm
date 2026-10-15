//! InputState 的单元测试（从 state.rs 拆出以控制单文件体积）。
//! 通过 `#[path]` 挂载为 state.rs 的子模块，可访问其私有项。

use super::edit_primitives::{find_word_occurrences, find_word_occurrences_naive, word_range_at};
use super::*;
use crate::components::input::{InputState, Rope};

/// 行首缩进的每级字节边界（legacy 测试辅助，不再被产品代码使用：
/// 缩进参考线现在改用 indent columns）。
fn indent_level_bounds(line_text: &str) -> Vec<usize> {
    let mut bounds = Vec::new();
    let mut spaces = 0;
    for (i, b) in line_text.bytes().enumerate() {
        match b {
            b'\t' => {
                spaces = 0;
                bounds.push(i + 1);
            }
            b' ' => {
                spaces += 1;
                if spaces == 4 {
                    spaces = 0;
                    bounds.push(i + 1);
                }
            }
            _ => break,
        }
    }
    bounds
}

/// 反缩进的长度语义由 `TabSize` 承载（审计 3.14），这里验证默认配置
/// （4 空格软缩进，行首 Tab 也按一级删除）下的行为；`indent.rs` 的
/// 测试覆盖其它宽度与硬 Tab 配置。
#[test]
fn outdent_unit_len_prefers_tab() {
    let tab = TabSize::default();
    assert_eq!(tab.outdent_len("\tfoo"), 1);
    assert_eq!(tab.outdent_len("\t\tfoo"), 1);
    assert_eq!(tab.outdent_len("foo"), 0);
    assert_eq!(tab.outdent_len("    foo"), 4); // 空格缩进
    assert_eq!(tab.outdent_len("  foo"), 2);
    assert_eq!(tab.outdent_len("      foo"), 4); // 最多 4 个
    assert_eq!(tab.outdent_len("  \tfoo"), 2); // 空格在 tab 前
    assert_eq!(tab.outdent_len(""), 0);
}

#[test]
fn shifted_outdent_offset_basic() {
    // 两行均删除 2 空格："  foo\n  bar" -> "foo\nbar"
    let edits = [(0, 2), (6, 2)];
    // 选区起点 0 不变
    assert_eq!(shifted_outdent_offset(0, &edits), 0);
    // 选区终点 11（全文结尾）回退 4
    assert_eq!(shifted_outdent_offset(11, &edits), 7);
    // 光标在第二行行首 6：落在删除区间内 → 钳制到新行首 4
    assert_eq!(shifted_outdent_offset(6, &edits), 4);
    // 光标在第二行内容 (8)：回退 2（第一行 2 + 第二行 2）
    assert_eq!(shifted_outdent_offset(8, &edits), 4);
}

#[test]
fn shifted_outdent_offset_single_line_selection() {
    // 光标在行中间 3（"  foo" 内），行首删除 2 → 新位置 1
    let edits = [(0, 2)];
    assert_eq!(shifted_outdent_offset(3, &edits), 1);
    // 光标在行首：钳制到 0
    assert_eq!(shifted_outdent_offset(0, &edits), 0);
    // 光标在删除区中间 (1)：钳制到 0
    assert_eq!(shifted_outdent_offset(1, &edits), 0);
}

#[test]
fn line_text_for_clipboard_middle_line_includes_newline() {
    let rope = Rope::from("first\nsecond\nthird");
    // 光标在第二行行中
    let (text, range) = line_text_for_clipboard(&rope, "first\nsec".len() + 1);
    assert_eq!(text, "second\n");
    assert_eq!(range, "first\n".len().."first\nsecond\n".len());
}

#[test]
fn line_text_for_clipboard_last_line_appends_newline() {
    let rope = Rope::from("first\nsecond\nthird");
    let (text, range) = line_text_for_clipboard(&rope, rope.len());
    assert_eq!(text, "third\n");
    assert_eq!(range, "first\nsecond\n".len()..rope.len());
}

#[test]
fn line_text_for_clipboard_single_line_document() {
    let rope = Rope::from("only line");
    let (text, range) = line_text_for_clipboard(&rope, 0);
    assert_eq!(text, "only line\n");
    assert_eq!(range, 0..9);
}

#[test]
fn line_text_for_clipboard_crlf_preserved() {
    let rope = Rope::from("a\r\nb\r\nc");
    let (text, range) = line_text_for_clipboard(&rope, 3);
    assert_eq!(text, "b\r\n");
    assert_eq!(range, "a\r\n".len().."a\r\nb\r\n".len());
}

#[test]
fn line_text_for_clipboard_blank_line() {
    let rope = Rope::from("a\n\nb"); // 中间空行
    let (text, range) = line_text_for_clipboard(&rope, 2);
    assert_eq!(text, "\n");
    assert_eq!(range, 2..3);
}

#[test]
fn comment_tokens_by_language() {
    assert_eq!(InputState::line_comment_token(Some("rust")), Some("//"));
    assert_eq!(InputState::line_comment_token(Some("python")), Some("#"));
    assert_eq!(InputState::line_comment_token(Some("lua")), Some("--"));
    // 无行注释语义/暂不支持的语言
    assert_eq!(InputState::line_comment_token(Some("json")), None);
    assert_eq!(InputState::line_comment_token(Some("css")), None);
    assert_eq!(InputState::line_comment_token(None), None);
}

#[test]
fn add_and_strip_line_comment() {
    let (out, pt) = InputState::add_line_comment("    let x = 1;", "//");
    assert_eq!(out, "    // let x = 1;");
    assert_eq!(pt, 4);
    let (out, pt) = InputState::strip_line_comment(&out, "//");
    assert_eq!(out, "    let x = 1;");
    assert_eq!(pt, 4);
    // 无空格分隔的注释也能取消
    let (out, _) = InputState::strip_line_comment("  #comment", "#");
    assert_eq!(out, "  comment");
    // 无注释 token 时原样返回
    let (out, _) = InputState::strip_line_comment("let x = 1;", "//");
    assert_eq!(out, "let x = 1;");
}

#[test]
fn newline_indent_decisions() {
    use NewlineIndent::*;
    // 行尾开括号 → 深入
    assert_eq!(InputState::newline_indent_kind("fn main() {", None), Deeper);
    // 开括号后紧跟配对闭括号 → 空行
    assert_eq!(
        InputState::newline_indent_kind("fn main() {", Some('}')),
        BlankBetween
    );
    // 不配对的闭括号不触发空行
    assert_eq!(InputState::newline_indent_kind("foo(", Some('}')), Deeper);
    // 冒号（Python/YAML 风格，行内无花括号）→ 深入
    assert_eq!(InputState::newline_indent_kind("def foo():", None), Deeper);
    // 行尾冒号但行内含花括号（JS 对象字面量）→ 不深入
    assert_eq!(
        InputState::newline_indent_kind("  foo({ key:", None),
        Inherit
    );
    assert_eq!(InputState::newline_indent_kind("let x = 1", None), Inherit);
    assert_eq!(InputState::newline_indent_kind("", None), Inherit);
}

#[test]
fn word_occurrence_scanning() {
    let rope = Rope::from_str("let foo = foo + foo2 + foo;");
    // 光标在词内
    assert_eq!(word_range_at(&rope, 4), Some(4..7));
    // 光标紧贴词尾（边界处仍算在词内）
    assert_eq!(word_range_at(&rope, 7), Some(4..7));
    // 光标在空格处（两侧都非词字符）
    assert_eq!(word_range_at(&rope, 8), None);
    // 词边界匹配：foo 命中 3 处，foo2 不命中
    let occ = find_word_occurrences(&rope, &(4..7), 100).unwrap();
    assert_eq!(occ.len(), 3);
    assert_eq!(occ[0], 4..7);
    assert_eq!(occ[1], 10..13);
    assert_eq!(occ[2], 23..26);

    // 下划线词
    let rope = Rope::from_str("my_var my_var2");
    assert_eq!(word_range_at(&rope, 2), Some(0..6));
    let occ = find_word_occurrences(&rope, &(0..6), 100).unwrap();
    assert_eq!(occ.len(), 1);

    // CJK：孤立单字不参与（噪音），多字词参与高亮。
    let rope = Rope::from_str("中 foo");
    assert_eq!(word_range_at(&rope, 0), None);
    let rope = Rope::from_str("中文 foo 中文");
    assert_eq!(word_range_at(&rope, 0), Some(0..6));
    let occ = find_word_occurrences(&rope, &(0..6), 100).unwrap();
    assert_eq!(occ, vec![0..6, 11..17]);

    // 超出次数上限 → None
    let many = "ab ".repeat(50);
    let rope = Rope::from_str(&many);
    assert!(find_word_occurrences(&rope, &(0..2), 10).is_none());
}

/// `find_word_occurrences` 改成定长滑动窗口的流式扫描后，用朴素全量实现
/// 做差分对照：窗口边界、文档首尾、多字节字符都不能改变结果。
/// ASCII 词对照 ASCII 朴素实现；Unicode 词走独立断言（边界按字符判定，
/// 与 ASCII 字节判定天然不一致，不直接对照）。
#[test]
fn word_occurrences_streaming_matches_naive() {
    // 覆盖：任意词长 × 各种文档长度，含文档首尾命中与相邻重复。
    let alphabet = ['a', 'b', ' ', '_'];
    let mut seed = 0x2545_F491_4F6D_D1A9u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for doc_len in [0usize, 1, 2, 3, 7, 8, 9, 15, 16, 33, 64, 129] {
        for _ in 0..12 {
            let text: String = (0..doc_len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let rope = Rope::from_str(&text);
            // 词范围必须落在字符边界上（生产路径由 `word_range_at` 保证）。
            let bounds: Vec<usize> = text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
                .collect();

            for (bi, &start) in bounds.iter().enumerate() {
                for &end in bounds.iter().skip(bi + 1) {
                    // 词长封顶：再长就是同一套窗口逻辑的重复验证。
                    if end - start > 8 {
                        break;
                    }
                    let got = find_word_occurrences(&rope, &(start..end), 1000);
                    let want = find_word_occurrences_naive(&rope, &(start..end), 1000);
                    assert_eq!(got, want, "doc={text:?} word={start}..{end}");
                }
            }
        }
    }

    // 长文档（远超窗口容量）上的跨边界命中。
    let long = "foo ".repeat(5000);
    let rope = Rope::from_str(long.trim_end());
    let got = find_word_occurrences(&rope, &(0..3), 100_000).unwrap();
    assert_eq!(got.len(), 5000);
    assert_eq!(got.last(), Some(&(19996..19999)));
}

#[test]
fn word_occurrences_unicode_boundaries() {
    // Unicode 词按字符判定边界：`变量` 在标点/空格间命中，黏连时不命中。
    let rope = Rope::from_str("变量,变量;变量x x变量 变量");
    let word = word_range_at(&rope, 0).unwrap();
    assert_eq!(&rope.slice(word.clone()).to_string(), "变量");
    let occ = find_word_occurrences(&rope, &word, 100).unwrap();
    // `变量x` / `x变量` 因边界黏连被排除，仅 3 处独立出现。
    assert_eq!(occ.len(), 3);
}

#[test]
fn subword_motion_splits_snake_and_camel() {
    // 蛇形：末段起点
    assert_eq!(InputState::last_word_start("foo_bar"), Some(4));
    assert_eq!(InputState::first_word_end("foo_bar"), Some(3));
    // 驼峰：Bar 起点 / foo 终点
    assert_eq!(InputState::last_word_start("fooBar"), Some(3));
    assert_eq!(InputState::first_word_end("fooBar"), Some(3));
    // 首字母缩写：HTML|Parser
    assert_eq!(InputState::last_word_start("HTMLParser"), Some(4));
    assert_eq!(InputState::first_word_end("HTMLParser"), Some(4));
    // 尾随标点跳过：hello.| 取 hello 首
    assert_eq!(InputState::last_word_start("hello."), Some(0));
}

#[test]
fn indent_level_bounds_scan() {
    // Tab：每 1 个 = 1 级
    assert_eq!(indent_level_bounds("\t\tfoo"), vec![1, 2]);
    // 空格：每 4 个 = 1 级
    assert_eq!(indent_level_bounds("        bar"), vec![4, 8]);
    // 混合：tab + 4 空格 = 2 级
    assert_eq!(indent_level_bounds("\t    baz"), vec![1, 5]);
    // 不足 4 个空格不算一级
    assert_eq!(indent_level_bounds("   x"), Vec::<usize>::new());
    // 空行 / 无缩进
    assert_eq!(indent_level_bounds(""), Vec::<usize>::new());
    assert_eq!(indent_level_bounds("no indent"), Vec::<usize>::new());
    // 只有空白
    assert_eq!(indent_level_bounds("  \t "), vec![3]);
}

#[test]
fn rainbow_pair_basic() {
    let rope = Rope::from_str("(a[b]{c})");
    let spans = scan_bracket_spans_text(&rope)
        .into_iter()
        .map(|(r, d, is_open, m)| (r.start, d, is_open, m))
        .collect::<Vec<_>>();
    // ( 层级0，[ 层级1，] 层级1，{ 层级1，} 层级1，) 层级0，全部配对
    assert_eq!(
        spans,
        vec![
            (0, 0, true, true),
            (2, 1, true, true),
            (4, 1, false, true),
            (5, 1, true, true),
            (7, 1, false, true),
            (8, 0, false, true),
        ]
    );
}

#[test]
fn rainbow_pair_unmatched() {
    // 游离闭括号 → 未配对，深度 0
    let rope = Rope::from_str(")(");
    let spans = scan_bracket_spans_text(&rope);
    assert_eq!(spans.len(), 2);
    assert_eq!(
        (spans[0].0.start, spans[0].1, spans[0].2, spans[0].3),
        (0, 0, false, false)
    );
    assert_eq!(
        (spans[1].0.start, spans[1].1, spans[1].2, spans[1].3),
        (1, 0, true, false)
    );

    // 未闭合的开括号 → 未配对（但保留层级，渲染不标红）
    let rope = Rope::from_str("(((");
    let spans = scan_bracket_spans_text(&rope);
    assert!(spans.iter().all(|(_, _, _, m)| !m));
    assert_eq!(spans[2].1, 2); // 第三个 ( 在第 2 层
}

#[test]
fn rainbow_pair_interleaved() {
    // 交错括号 ([)]：配对算法保证 ( 与 ) 同色（层级 0），[ 与 ] 被跨越 → 未配对
    let rope = Rope::from_str("([)]");
    let spans = scan_bracket_spans_text(&rope)
        .into_iter()
        .map(|(r, d, is_open, m)| (r.start, d, is_open, m))
        .collect::<Vec<_>>();
    assert_eq!(
        spans,
        vec![
            (0, 0, true, true),
            (1, 1, true, false),
            (2, 0, false, true),
            (3, 0, false, false)
        ]
    );
}

#[test]
fn rainbow_tree_scan_excludes_strings_and_comments() {
    // rust 语法树：字符串/注释里的括号不应参与 rainbow
    let code = r#"fn main() {
    let s = "a)b(c";
    // ) comment
    let v = (1 + 2);
}
"#;
    let mut h = crate::editor::syntax::SimpleHighlighter::new("rust");
    let rope = Rope::from_str(code);
    assert!(h.update(None, &rope, None));
    let tree = h.tree().expect("rust tree");
    let spans = scan_bracket_spans_tree(tree);
    let starts: Vec<usize> = spans.iter().map(|(r, _, _, _)| r.start).collect();
    // 只应有 main 的 ()、(1+2) 的 () 和函数体的 {}
    assert_eq!(starts, vec![7, 8, 10, 62, 68, 71]);
    assert!(spans.iter().all(|(_, _, _, m)| *m));

    // 纯文本 fallback 同样跳过字符串/注释里的括号（C 风格），与 tree 一致。
    assert_eq!(scan_bracket_spans_text(&rope).len(), 6);
}

#[test]
fn bracket_matching_offsets() {
    let rope = Rope::from_str("fn main() {\n    let a = (1 + 2);\n}\n");
    // 光标在 `{` 后 → 匹配 `{`..`}`
    assert_eq!(InputState::matching_bracket(&rope, 11), Some((10, 33)));
    // 光标在 `(` 后 → 前向匹配
    assert_eq!(InputState::matching_bracket(&rope, 25), Some((24, 30)));
    // 光标在 `)` 前（同一个闭括号，后向匹配）
    assert_eq!(InputState::matching_bracket(&rope, 31), Some((24, 30)));
    // 光标在 `}` 后 → 后向匹配到 `{`
    assert_eq!(InputState::matching_bracket(&rope, 34), Some((10, 33)));
    // 非括号处 → None
    assert_eq!(InputState::matching_bracket(&rope, 0), None);

    // 嵌套：光标在第一个 `(` 后，应跳过内层括号
    let nested = Rope::from_str("a (b (c)) d");
    assert_eq!(InputState::matching_bracket(&nested, 3), Some((2, 8)));
    // 内层
    assert_eq!(InputState::matching_bracket(&nested, 6), Some((5, 7)));

    // 未闭合括号 → None
    let unclosed = Rope::from_str("(abc");
    assert_eq!(InputState::matching_bracket(&unclosed, 1), None);
    let unopened = Rope::from_str("abc)");
    assert_eq!(InputState::matching_bracket(&unopened, 4), None);

    // 多字节字符中的括号不会被误判（中文字节 ≥ 0x80，不会与 ASCII 括号混淆）
    // 布局：中文(0-5) （(6-8) 注(9-11) ）(12-14) ((15) )(16) “(17-19) (20) ”(21-23)
    let cjk = Rope::from_str("中文（注）()“ ”");
    assert_eq!(InputState::matching_bracket(&cjk, 16), Some((15, 16)));
    assert_eq!(InputState::matching_bracket(&cjk, 17), Some((15, 16)));
    // 中文全角括号（多字节）不参与 ASCII 括号匹配
    assert_eq!(InputState::matching_bracket(&cjk, 9), None);

    // 空文本/边界
    let empty = Rope::from_str("");
    assert_eq!(InputState::matching_bracket(&empty, 0), None);
}

#[test]
fn rainbow_pair_depth_stable_for_nested_mixed() {
    let cases = [
        (
            "((()))",
            vec![(0, 0), (1, 1), (2, 2), (3, 2), (4, 1), (5, 0)],
        ),
        (
            "({[]})",
            vec![(0, 0), (1, 1), (2, 2), (3, 2), (4, 1), (5, 0)],
        ),
        (
            "a(b[c]{d}e)f",
            vec![(1, 0), (3, 1), (5, 1), (6, 1), (8, 1), (10, 0)],
        ),
    ];
    for (text, expected) in cases {
        let rope = Rope::from_str(text);
        let spans = scan_bracket_spans_text(&rope);
        let depths: Vec<(usize, usize)> = spans.iter().map(|(r, d, _, _)| (r.start, *d)).collect();
        assert_eq!(depths, expected, "text={text}");
    }
}

#[test]
fn rainbow_pair_all_brackets_paired_same_color_depth() {
    let rope = Rope::from_str("((a){b}[c])");
    let spans = scan_bracket_spans_text(&rope);
    let got: Vec<(usize, usize, bool, bool)> = spans
        .into_iter()
        .map(|(r, d, o, m)| (r.start, d, o, m))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, 0, true, true),
            (1, 1, true, true),
            (3, 1, false, true),
            (4, 1, true, true),
            (6, 1, false, true),
            (7, 1, true, true),
            (9, 1, false, true),
            (10, 0, false, true),
        ]
    );
}

/// H3 回归：程序化插入（undo/redo 回放等）不走括号自动配对，
/// 否则 redo 回放单字符 "(" 会再次配对成 "()"，与原始键入结果不一致；
/// 而真实键入路径（EntityInputHandler）仍应配对。
#[gpui::test]
fn programmatic_insert_bypasses_auto_pair(cx: &mut gpui::TestAppContext) {
    fn build_code_input(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<InputState>, &mut gpui::VisualTestContext) {
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        let cell = std::cell::RefCell::new(None);
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
            *cell.borrow_mut() = Some(input.clone());
            Host(input)
        });
        (cell.into_inner().unwrap(), cx)
    }

    // 程序化插入：不配对
    let (input, cx) = build_code_input(cx);
    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            state.replace_text_in_range_silent(None, "(", window, cx);
            assert_eq!(state.value().as_ref(), "(");
        })
        .unwrap();

    // 键入路径（EntityInputHandler）：仍应自动配对成 "()"
    let (input, cx) = build_code_input(cx);
    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            gpui::EntityInputHandler::replace_text_in_range(state, None, "(", window, cx);
            assert_eq!(state.value().as_ref(), "()");
            // 光标应停在配对括号中间
            assert_eq!(state.cursor(), 1);
        })
        .unwrap();
}

// ==================== Code folding (zed semantics) ====================

const FOLD_TEST_TEXT: &str = "fn main() {\n    let a = 1;\n    if true {\n        a();\n    }\n}\n\nfn other() {\n    let b = 2;\n}\n";

fn build_fold_input(
    cx: &mut gpui::TestAppContext,
) -> (gpui::Entity<InputState>, &mut gpui::VisualTestContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true);
            state.set_value(FOLD_TEST_TEXT, window, cx);
            state
        });
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    (cell.into_inner().unwrap(), cx)
}

#[gpui::test]
fn fold_at_hides_rows_and_keeps_closing_bracket(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input.update(cx, |state, cx| {
        // Rows: 0 `fn main() {`, 1 `let a`, 2 `if true {`, 3 `a();`, 4 `}` (inner),
        // 5 `}` (outer), 6 blank, 7 `fn other() {`, 8 `let b`, 9 `}`
        state.fold_at(0, cx);
        assert!(state.display_map.is_line_folded(0));
        assert!(!state.display_map.is_line_folded(7));
        // Rows 1-5 hidden (row 5's `}` tail is painted on row 0).
        for row in 1..=5 {
            assert!(state.display_map.is_buffer_line_hidden(row), "row {row}");
        }
        assert!(!state.display_map.is_buffer_line_hidden(6));
        assert!(!state.display_map.is_buffer_line_hidden(7));

        // Unfolding at the header row restores everything.
        state.unfold_at(0, cx);
        assert!(!state.display_map.is_line_folded(0));
        assert!(!state.display_map.is_buffer_line_hidden(1));
    });
}

#[gpui::test]
fn nested_folds_survive_folding_the_outer(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input.update(cx, |state, cx| {
        // Fold the inner `if true {` block first.
        state.fold_at(2, cx);
        assert!(state.display_map.is_line_folded(2));
        assert!(!state.display_map.is_line_folded(0));

        // Fold the outer function; the inner fold stays in storage.
        state.fold_at(0, cx);
        assert!(state.display_map.is_line_folded(0));
        assert!(state.display_map.is_line_folded(2));

        // Unfolding the outer reveals the still-folded inner
        // (zed's unfold_at only removes folds intersecting the row).
        state.unfold_at(0, cx);
        assert!(!state.display_map.is_line_folded(0));
        assert!(state.display_map.is_line_folded(2));
        assert!(state.display_map.is_buffer_line_hidden(3));
        assert!(state.display_map.is_buffer_line_hidden(4));
        assert!(!state.display_map.is_buffer_line_hidden(5));
    });
}

#[gpui::test]
fn cursor_is_clamped_out_of_folds(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input.update(cx, |state, cx| {
        state.fold_at(0, cx);

        // Put the cursor inside the fold, then move by boundaries — the
        // clamps keep the cursor out of the hidden range.
        state.selected_range = (30..30).into(); // row 3 (`a();`) inside the fold
        let backward = state.previous_boundary(30);
        assert_eq!(
            backward, 11,
            "should clamp back to the fold start (end of row 0)"
        );
        let forward = state.next_boundary(30);
        assert_eq!(
            forward, 60,
            "should clamp forward to the fold end (row 5 closing brace start)"
        );
    });
}

/// 整体换文档（文件加载 / 标签切换）后不能把旧折叠按 offset 重映射到新文档上。
#[gpui::test]
fn wholesale_replacement_clears_folds(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true);
            state.set_value(FOLD_TEST_TEXT, window, cx);
            state
        });

        input.update(cx, |state, cx| {
            state.fold_at(0, cx);
            state.fold_at(7, cx);
            assert!(state.display_map.is_line_folded(0));
            assert!(state.display_map.is_line_folded(7));
        });

        // `set_value` 走整体 replace（0..len），与折叠边界重叠 → 折叠全部丢弃。
        input.update(cx, |state, cx| {
            state.set_value("fn fresh() {\n    x();\n}\n", window, cx);
            assert!(
                state.display_map.folds().is_empty(),
                "set_value 后不应残留旧文档的折叠"
            );
        });

        // `set_text`（后台文件加载路径，无 Window）同样必须清折叠。
        input.update(cx, |state, cx| {
            state.fold_at(0, cx);
            assert!(state.display_map.is_line_folded(0));
            state.set_text("fn other() {\n    y();\n}\n", cx);
            assert!(
                state.display_map.folds().is_empty(),
                "set_text 后不应残留旧文档的折叠"
            );
        });

        Host(input)
    });
}

/// `text_version` 是「文档内容已变化」的唯一信号，三条改写 `self.text` 的
/// 路径都必须推进它 —— 少一处，脏检查闸门 / 同词高亮缓存 / 后台解析的
/// stale 判定就会把这次编辑当成「文本没变」。
///
/// IME 组字路径最容易漏（它不经过 `replace_text_in_range_inner`）：中文输入
/// 时文件不会显示“已修改”，直到用户敲一个非组字键。
#[gpui::test]
fn text_version_advances_on_every_mutation(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true)
        });

        // 1) 常规键入（EntityInputHandler）
        input.update(cx, |state, cx| {
            let before = state.text_version;
            gpui::EntityInputHandler::replace_text_in_range(state, None, "let a = 1;", window, cx);
            assert!(
                state.text_version > before,
                "常规键入未推进 text_version（{} -> {}）",
                before,
                state.text_version
            );
        });

        // 2) 整体替换（后台文件加载 / 切标签页 / 重载）
        input.update(cx, |state, cx| {
            let before = state.text_version;
            state.set_text("fn reloaded() {}\n", cx);
            assert!(
                state.text_version > before,
                "set_text 未推进 text_version（{} -> {}）",
                before,
                state.text_version
            );
        });

        // 3) IME 组字：每一步都算一次编辑，都要推进版本号。
        input.update(cx, |state, cx| {
            state.set_text("", cx);
            for ch in ["n", "ni", "你", "你好"] {
                let before = state.text_version;
                gpui::EntityInputHandler::replace_and_mark_text_in_range(
                    state, None, ch, None, window, cx,
                );
                assert!(
                    state.text_version > before,
                    "IME 组字 {ch:?} 未推进 text_version（{before} -> {}）",
                    state.text_version
                );
                assert_eq!(state.value().as_ref(), ch);
            }
        });

        // 4) 校验失败回滚时不应推进版本号（文本最终没变）。
        let validated = cx.new(|cx| {
            // 默认即单行模式；`pattern` 只接受数字。
            InputState::new(window, cx).pattern(regex::Regex::new(r"^\d*$").unwrap())
        });
        validated.update(cx, |state, cx| {
            state.set_value("123", window, cx);
            let before = state.text_version;
            gpui::EntityInputHandler::replace_text_in_range(state, None, "abc", window, cx);
            assert_eq!(state.value().as_ref(), "123", "非法输入应被拒绝");
            assert_eq!(
                state.text_version, before,
                "被拒绝的输入不应推进 text_version"
            );
        });

        Host(input)
    });
}

#[gpui::test]
fn fold_all_and_unfold_all(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input.update(cx, |state, cx| {
        state.display_map.clear_folds();
        state.fold_at(0, cx);
        state.fold_at(2, cx);
        state.fold_at(7, cx);
        assert!(state.display_map.is_line_folded(0));
        assert!(state.display_map.is_line_folded(2));
        assert!(state.display_map.is_line_folded(7));

        // Unfold everything via the whole-buffer range (zed's unfold_all).
        state
            .display_map
            .unfold_ranges(std::slice::from_ref(&(0..state.text.len())), true);
        assert!(state.display_map.folds().is_empty());
        assert!(!state.display_map.is_buffer_line_hidden(1));
    });
}

// ==================== 语法诊断（tree-sitter ERROR/MISSING） ====================

/// 端到端：语法错误的文本经同步解析后，诊断集合应被填充，
/// 且 `for_offset` / `max_severity_for_range` 在编辑器路径上可用。
///
/// 回归背景：`DiagnosticSet` 曾是空壳（`for_offset` 恒 `None`），
/// 渲染管线虽已接通但数据源恒空 —— 用户在编辑器里看不到任何错误。
#[gpui::test]
fn syntax_diagnostics_populate_after_edit(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            // 先切到 json：语法错误的判定依赖具体语法。
            state.set_language("json", window, cx);
            state.set_value(r#"{"a": }"#, window, cx);

            let diagnostics = state.diagnostics().expect("code editor has diagnostics");
            assert!(!diagnostics.is_empty(), "未闭合的 json 值应产出语法诊断");

            let items = diagnostics.items();
            let first = &items[0];
            assert!(
                first.range.end <= state.text.len(),
                "诊断区间必须在文本范围内：{:?}",
                first.range
            );
            assert!(
                state.text.is_char_boundary(first.range.start)
                    && state.text.is_char_boundary(first.range.end),
                "诊断区间必须落在字符边界上：{:?}",
                first.range
            );

            // 行区间查询（gutter 标记的路径）应能取到级别。
            let line_range = state.text.line_start_offset(0)..state.text.line_content_end_offset(0);
            assert!(
                diagnostics.max_severity_for_range(&line_range).is_some(),
                "第一行应报告诊断级别"
            );
        })
        .unwrap();
}

/// 无语法错误的文本不应产出诊断（避免满屏假阳性下划线）。
#[gpui::test]
fn syntax_diagnostics_empty_for_valid_source(cx: &mut gpui::TestAppContext) {
    let (input, cx) = build_fold_input(cx);
    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            state.set_language("json", window, cx);
            state.set_value(r#"{"a": 1}"#, window, cx);
            let diagnostics = state.diagnostics().expect("code editor has diagnostics");
            assert!(
                diagnostics.is_empty(),
                "合法 json 不应产出诊断，实际 {:?}",
                diagnostics.items()
            );
        })
        .unwrap();
}

// ==================== 软换行默认值（zed parity） ====================

/// 代码编辑器默认关软换行（Zed `"soft_wrap": "none"`、VSCode
/// `editor.wordWrap: off`）：长行保持原始行结构，靠水平滚动 + 底部横向
/// 滚动条查看；其它多行输入（提交框、说明框…）仍是默认的软换行。
#[gpui::test]
fn soft_wrap_defaults_match_zed(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let code = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        let plain = cx.new(|cx| InputState::new(window, cx).multi_line(true));
        *cell.borrow_mut() = Some((code.clone(), plain.clone()));
        Host(code)
    });
    let (code, plain) = cell.into_inner().unwrap();

    let code_wraps = code.read_with(cx, |s, _| s.soft_wrap);
    let plain_wraps = plain.read_with(cx, |s, _| s.soft_wrap);
    assert!(!code_wraps, "代码编辑器默认应关闭软换行");
    assert!(plain_wraps, "普通多行输入（提交框等）默认仍应软换行");
}

// ==================== LSP 补全应用 ====================

/// 补全候选必须能被 `render` 驱动的那次调用取走，且取出是一次性的。
///
/// 回归：`Editor::drain_lsp_results`（把 pending 槽里的结果搬进浮层状态的
/// 唯一入口）只在 `render` 里调用。后台任务拿到结果后若不 notify，就不会
/// 有新的一帧，`render` 不跑，候选永远搬不出来 —— 症状是「服务器明明返回
/// 了候选，界面上却没有弹窗」，且日志里看不到任何错误。
///
/// 这里钉住流水线的后半段：写入 pending 槽 → 取走 → 再取为空。
#[gpui::test]
fn pending_completion_is_drained_exactly_once(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, cx| {
        let item = lsp_types::CompletionItem {
            label: "foo_bar".into(),
            ..Default::default()
        };
        state.set_pending_completions(vec![item], lsp_types::Position::new(0, 0), state.cursor());
        // 契约：写入后必须 notify，否则不会产生新的一帧，`render` 不跑，
        // `drain_lsp_results` 也就永远不会把候选搬进浮层状态。
        cx.notify();
    });
    cx.run_until_parked();

    let first = input.update(cx, |state, _cx| state.take_pending_completions());
    assert!(
        first.is_some_and(|pending| !pending.items.is_empty()),
        "第一次取应拿到候选"
    );
    let second = input.update(cx, |state, _cx| state.take_pending_completions());
    assert!(
        second.is_none(),
        "第二次取应为空（取出即清空，否则同一菜单会被反复重画）"
    );
}

/// 光标移开之后，那份候选必须被丢弃 —— 不能拿去按新位置展示。
///
/// 回归：后台任务结束与下一帧 `drain_lsp_results` 之间用户往往又敲了字符。
/// 此前 `take_pending` 把随结果一起带来的 `Position` 写成 `_position` 丢掉
/// 了，于是既无法判断候选是否还对得上光标，又用**当前**光标去算替换区间
/// 与菜单锚点 —— 拿 A 位置的候选按 B 位置替换。症状就是用户报的「补全出现了，
/// 但内容不匹配」。
#[gpui::test]
fn pending_completion_is_dropped_once_cursor_moved(cx: &mut gpui::TestAppContext) {
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, cx| {
        state.set_text("let ab = 1;", cx);
    });

    // 伪造「请求时的光标在别处」—— 这正是「后台任务结束与下一帧之间用户又
    // 敲了字符」留下的状态。`VisualTestContext::update` 拿不到 `Window`，
    // 没法真的敲键；而被测的判定只看两个偏移是否相等，直接构造即可。
    let (requested_at, cursor_now) =
        input.update(cx, |state, _| (state.cursor() + 1, state.cursor()));
    assert_ne!(
        requested_at, cursor_now,
        "前置：请求偏移必须与当前光标不同，否则测不到东西"
    );
    input.update(cx, |state, _| {
        let item = lsp_types::CompletionItem {
            label: "at_request_position".into(),
            ..Default::default()
        };
        state.set_pending_completions(vec![item], lsp_types::Position::new(0, 3), requested_at);
    });

    let taken = input.update(cx, |state, _cx| state.take_pending_completions());
    assert!(
        taken.is_none(),
        "光标已从 {requested_at} 移开，过期候选必须丢弃（否则菜单内容与光标处不匹配）"
    );
}

/// 光标没动时，候选要能正常取走，且带出**请求时**的偏移。
#[gpui::test]
fn pending_completion_reports_the_offset_it_was_requested_at(cx: &mut gpui::TestAppContext) {
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, cx| {
        state.set_text("let ab = 1;", cx);
    });
    let requested_at = input.update(cx, |state, _| state.cursor());
    input.update(cx, |state, _| {
        state.set_pending_completions(
            vec![lsp_types::CompletionItem {
                label: "alpha".into(),
                ..Default::default()
            }],
            lsp_types::Position::new(0, 7),
            requested_at,
        );
    });

    let taken = input
        .update(cx, |state, _cx| state.take_pending_completions())
        .expect("光标未动，候选应可用");
    assert_eq!(
        taken.cursor_offset, requested_at,
        "必须能拿回请求时的偏移——菜单锚点与替换区间都要以它为准"
    );
    assert_eq!(taken.items.len(), 1);
}

/// 延迟隐藏的三条取消路径都必须有效。
///
/// 回归：「鼠标移进卡片、卡片大概率直接关闭」。根因是「空白响应立刻清浮层」：
/// 从符号把指针挪进卡片的路上必然经过其它字符，其中一次「无内容」响应先到，
/// 卡片就在指针到达之前被清掉。现在无内容只安排 deadline，到期才收。
#[gpui::test]
fn hover_hide_deadline_cancel_paths(cx: &mut gpui::TestAppContext) {
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    // 前置：没有浮层时安排了也判不出到期。
    input.update(cx, |state, _| {
        state.schedule_hover_hide(std::time::Duration::from_millis(0));
    });
    input.read_with(cx, |state, _| {
        assert!(
            !state.hover_hide_expired(),
            "没有浮层时不能判到期，否则会误清别的东西"
        );
    });

    // 挂一个浮层 + 已过期的 deadline。
    input.update(cx, |state, cx| {
        state.set_hover_popover(
            0,
            crate::editor::hover::HoverInfo::from_plain_text("fn foo()", cx),
        );
        state.schedule_hover_hide(std::time::Duration::from_millis(0));
    });
    std::thread::sleep(std::time::Duration::from_millis(5));
    input.read_with(cx, |state, _| {
        assert!(
            state.hover_hide_expired(),
            "deadline 已过且无守卫，应判到期"
        );
    });

    // 取消路径 1：指针进入卡片。
    input.update(cx, |state, _| state.set_hover_pointer_inside(true));
    input.read_with(cx, |state, _| {
        assert!(
            !state.hover_hide_expired(),
            "指针在卡片上时不能到期，否则进卡片就被收走"
        );
    });
    input.update(cx, |state, _| state.set_hover_pointer_inside(false));

    // 取消路径 2：键盘接管。
    input.update(cx, |state, _| state.set_hover_keyboard_takeover(true));
    input.read_with(cx, |state, _| {
        assert!(
            !state.hover_hide_expired(),
            "键盘接管中不能到期，否则 K 进去就被收走"
        );
    });

    // 取消路径 3：直接取消（新内容/新请求）。
    input.update(cx, |state, _| {
        state.set_hover_keyboard_takeover(false);
        state.cancel_hover_hide();
    });
    input.read_with(cx, |state, _| {
        assert!(
            !state.hover_hide_expired() && state.hover_hide_deadline().is_none(),
            "cancel 后 deadline 必须清空"
        );
    });
}

/// 滚轮落在 hover 浮层矩形内时，必须判定为「命中浮层」。
///
/// 回归：GPUI 为每个 `overflow: Scroll` 容器自动注册的滚动 listener **不会**
/// `stop_propagation`，所以浮层滚得动的同时事件也冒泡到
/// `InputState::on_scroll_wheel`，把背后的代码一起滚走。修法是在编辑器侧用
/// 几何判断放行 —— 那就要求这里能判对。
#[gpui::test]
fn scroll_over_hover_popover_is_detected_by_bounds(cx: &mut gpui::TestAppContext) {
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, cx| {
        state.set_hover_popover(
            0,
            crate::editor::hover::HoverInfo::from_plain_text("fn foo()", cx),
        );
        state.record_hover_popover_bounds(gpui::Bounds {
            origin: gpui::point(gpui::px(100.), gpui::px(200.)),
            size: gpui::size(gpui::px(400.), gpui::px(160.)),
        });
    });

    input.read_with(cx, |state, _| {
        // 卡片内部。
        assert!(
            state.hover_popover_contains(gpui::point(gpui::px(300.), gpui::px(280.))),
            "卡片内部应判为命中"
        );
        // 卡片外的代码区。
        assert!(
            !state.hover_popover_contains(gpui::point(gpui::px(300.), gpui::px(500.))),
            "卡片下方应判为未命中，否则编辑器会滚不动"
        );
        assert!(
            !state.hover_popover_contains(gpui::point(gpui::px(50.), gpui::px(280.))),
            "卡片左侧应判为未命中"
        );
    });

    // 浮层消失后，矩形必须一并作废：否则上一张卡片的位置会永久吞掉滚动。
    input.update(cx, |state, _| state.clear_hover_popover());
    input.read_with(cx, |state, _| {
        assert!(
            state.hover_popover_bounds().is_none(),
            "clear_hover_popover 必须同时清掉矩形，否则该区域滚不动"
        );
        assert!(
            !state.hover_popover_contains(gpui::point(gpui::px(300.), gpui::px(280.))),
            "浮层消失后不该再有任何命中"
        );
    });
}

/// `hover_popover_covers`（Zed `same_info_hover` 的对应物）：
/// 决定「鼠标在卡片覆盖的区间内移动时，要不要重新起防抖」。
///
/// 判错的两个方向各有代价：
/// - 判**过窄** → 每动一下都把任务往后推，卡片可能永远不出现，且反复发请求；
/// - 判**过宽** → 真正换了符号却不重新查询，卡片内容停在旧符号上。
#[gpui::test]
fn hover_popover_covers_drives_debounce_restart(cx: &mut gpui::TestAppContext) {
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, _| {
        state.set_hover_popover(
            4,
            crate::editor::hover::HoverInfo {
                contents: "fn foo(x: u8) -> u8".into(),
                range: Some(4..8),
                markdown: None,
                scroll_handle: gpui::ScrollHandle::new(),
            },
        );
    });

    input.read_with(cx, |state, _| {
        // 锚点本身：服务器没给命中区间时唯一的判据。
        assert!(state.hover_popover_covers(4), "锚点偏移应算覆盖");
        // 区间内部。
        assert!(state.hover_popover_covers(5), "命中区间内部应算覆盖");
        assert!(state.hover_popover_covers(7), "命中区间末字符应算覆盖");
        // **闭区间**：`range.end` 是「下一个偏移」，但鼠标正好停在符号最后一个
        // 字符上时取到的就是它 —— Zed 那边同样是 `start..=end`。
        assert!(state.hover_popover_covers(8), "区间右端点应算覆盖");
        // 区间之外：必须重新起防抖并重新查询。
        assert!(!state.hover_popover_covers(3), "区间左侧外不应覆盖");
        assert!(!state.hover_popover_covers(9), "区间右侧外不应覆盖");
    });

    // 只有锚点、没有命中区间（`range: None`，签名/纯文本 hover 常见）
    // → 只有锚点偏移本身算覆盖。
    input.update(cx, |state, cx| {
        state.set_hover_popover(
            4,
            crate::editor::hover::HoverInfo::from_plain_text("fn foo()", cx),
        );
    });
    input.read_with(cx, |state, _| {
        assert!(state.hover_popover_covers(4), "range 缺失时锚点仍应覆盖");
        assert!(
            !state.hover_popover_covers(5),
            "range 缺失时不能把相邻字符也算进来，否则换了符号也不重新查询"
        );
    });

    // 卡片没了：任何偏移都不该算覆盖（否则新位置会拿到「已覆盖」而永远不发请求）。
    input.update(cx, |state, _| state.clear_hover_popover());
    input.read_with(cx, |state, _| {
        assert!(!state.hover_popover_covers(4), "浮层消失后必须重新起防抖");
    });
}

#[gpui::test]
fn apply_completion_replaces_range_and_moves_cursor(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            state.replace_text_in_range_silent(None, "let x = fo", window, cx);
            // 把 `fo` 补全成 `foo_bar`：区间覆盖用户已输入的前缀。
            let range = 8..10;
            let ok = state.apply_completion(range, "foo_bar", cx);
            assert!(ok, "apply_completion should succeed on a valid range");
            assert_eq!(state.value().as_ref(), "let x = foo_bar");
            // 光标落在插入文本末尾。
            assert_eq!(state.cursor(), 15);
        })
        .unwrap();
}

#[gpui::test]
fn apply_completion_rejects_bad_ranges(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            state.replace_text_in_range_silent(None, "abc", window, cx);
            // 越界、反向区间都必须被拒绝，且**不能改动文本**。
            assert!(!state.apply_completion(0..99, "x", cx));
            // 用变量构造反向区间：字面量 `2..1` 会撞上 clippy 的
            // `reversed_empty_ranges`（deny 级，整个 lib test target 编译不过），
            // 而 clippy 建议的 `(1..2).rev()` 传不进来——`apply_completion` 收的
            // 是 `Range<usize>`。`2..1` 正是这里要测的输入，别把它换成合法区间。
            let (reversed_start, reversed_end) = (2usize, 1usize);
            assert!(!state.apply_completion(reversed_start..reversed_end, "x", cx));
            assert_eq!(state.value().as_ref(), "abc");
        })
        .unwrap();
}

#[gpui::test]
fn apply_completion_rejects_non_char_boundary(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("rust"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input
        .downgrade()
        .update_in(cx, |state, window, cx| {
            // "中" 占 3 字节：偏移 1 落在字符中间，必须拒绝（否则 Rope 会 panic）。
            state.replace_text_in_range_silent(None, "中x", window, cx);
            assert!(!state.apply_completion(1..2, "y", cx));
            assert_eq!(state.value().as_ref(), "中x");
        })
        .unwrap();
}

/// 诊断计数没变时既不改状态、也不触发重绘。
///
/// 服务器每次改动都会**重发同一份**诊断（实测 basedpyright 保存一次就重发
/// 全部 59 条），而 `cx.notify()` 的代价是整棵视图树重建。返回「是否变化」
/// 让监听侧能把它跳过。
#[gpui::test]
fn identical_diagnostic_counts_are_a_no_op(cx: &mut gpui::TestAppContext) {
    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("python"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, _| {
        assert!(
            state.set_lsp_diagnostic_counts(59, 0),
            "首次计数必须落进状态"
        );
        assert!(
            !state.set_lsp_diagnostic_counts(59, 0),
            "同样的计数不该再算一次变化"
        );
        assert!(
            state.set_lsp_diagnostic_counts(58, 1),
            "计数变了就要重绘——灯色跟着计数走"
        );
    });
}

/// 「什么都没带」的 `$/progress` 不能把 chip 钉死在转圈态。
///
/// 转圈没有灯：chip 上那个「有没有进程活着」的圆点会消失。实测的疑问
/// 「Python 前面为什么没有点」有一半是这个（另一半见 `lsp_chip_shows_lamp`）。
#[gpui::test]
fn a_content_less_progress_report_changes_nothing(cx: &mut gpui::TestAppContext) {
    use crate::lsp::status::LspStatus;

    struct Host(#[allow(dead_code)] gpui::Entity<InputState>);
    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
    let cell = std::cell::RefCell::new(None);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("python"));
        *cell.borrow_mut() = Some(input.clone());
        Host(input)
    });
    let input = cell.into_inner().unwrap();

    input.update(cx, |state, _| {
        assert!(state.set_lsp_diagnostic_counts(0, 0));
        let token = lsp_types::ProgressToken::String("orphan".into());
        let empty = lsp_types::WorkDoneProgress::Report(lsp_types::WorkDoneProgressReport {
            cancellable: None,
            message: None,
            percentage: None,
        });
        assert!(
            !state.apply_lsp_progress(&token, &empty),
            "空 report 不该算变化"
        );
        assert!(
            matches!(state.lsp_status(), Some(LspStatus::Ready { .. })),
            "chip 必须还停在就绪（有灯），而不是转圈"
        );
    });
}

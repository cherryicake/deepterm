//! InputState 的单元测试（从 state.rs 拆出以控制单文件体积）。
//! 通过 `#[path]` 挂载为 state.rs 的子模块，可访问其私有项。

use super::*;
use super::edit_primitives::{
    find_word_occurrences, find_word_occurrences_naive, word_range_at,
};
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


    /// 反缩进的长度语义现在由 `TabSize` 承载（审计 3.14），这里验证默认配置
    /// （硬 Tab，列宽 4）下的行为不变；`indent.rs` 的测试覆盖空格配置。
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
            let depths: Vec<(usize, usize)> =
                spans.iter().map(|(r, d, _, _)| (r.start, *d)).collect();
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
                fn render(
                    &mut self,
                    _: &mut Window,
                    _: &mut Context<Self>,
                ) -> impl gpui::IntoElement {
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
                gpui::EntityInputHandler::replace_text_in_range(
                    state,
                    None,
                    "let a = 1;",
                    window,
                    cx,
                );
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
                assert!(
                    !diagnostics.is_empty(),
                    "未闭合的 json 值应产出语法诊断"
                );

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
                let line_range = state.text.line_start_offset(0)
                    ..state.text.line_content_end_offset(0);
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

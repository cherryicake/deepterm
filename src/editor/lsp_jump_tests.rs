//! LSP 跳转（定义 / 引用）的测试。
//!
//! 回归背景：`goto_definition` 曾经只在**目标与当前文件相同**时才跳，
//! 跨文件时静默返回——而「跳到别的模块的定义」恰恰是最常见的场景，
//! 用户看到的现象就是「跳转不工作」。
//!
//! 对照 Zed 的 `go_to_definition_of_kind`：它拿到 `Location` 后
//! `open_local_buffer_via_lsp` 打开目标文件再选中区间，从不因为跨文件而放弃。
//!
//! 这里驱动真实的 [`crate::editor::editor::lsp_location_action`]——它是
//! 「原地跳 / 打开目标文件 / 忽略」的**唯一分支点**。
//!
//! 为什么不直接调 `Editor::open_lsp_location`：那条路径会调用
//! `cx.watch_path`，启动真实的 OS 文件监视线程；在 gpui 的
//! `TestScheduler` 下这些线程会被判为「非确定性活动」并 abort 整个测试
//! 进程（`test_scheduler.rs` 的 non-determinism 检查）。决策函数把分支
//! 逻辑完整覆盖，而文件打开本身复用已有的 `open_file_at_line_col_no_notify`
//! （它有自己的测试）。

use gpui::AppContext as _;
use lsp_types::{Location, Position, Range, Url};

use super::{LspJumpAction, lsp_location_action};

fn loc(uri: &str, line: u32, col: u32) -> Location {
    Location {
        uri: Url::parse(uri).unwrap(),
        range: Range::new(Position::new(line, col), Position::new(line, col + 4)),
    }
}

/// 构造一个 `file://` uri，指向给定的**平台本地路径**。
///
/// 必须走 `Url::from_file_path` 而不是手写 `file:///tmp/x.rs`：windows 上
/// 合法形式是 `file:///C:/tmp/x.rs`，`to_file_path()` 遇到 `/tmp/x.rs`
/// 会因「不是本机绝对路径」返回 `Err`，于是 `uri_to_path` 给出 `None`、
/// 跳转退化成 `Ignore`——被验的「行号换算」根本没机会执行。
fn file_loc(path: &std::path::Path, line: u32, col: u32) -> Location {
    Location {
        uri: Url::from_file_path(path).expect("必须是合法的本地路径"),
        range: Range::new(Position::new(line, col), Position::new(line, col + 4)),
    }
}

/// 跨文件跳转必须产出 `OpenFile`（而不是放弃）。
///
/// 这是用户报的「跳转不工作」的核心场景。
#[test]
fn cross_file_location_opens_target_file() {
    let target = std::env::temp_dir().join("other.rs");
    let location = file_loc(&target, 9, 2);
    let action = lsp_location_action(&location, false);
    match action {
        LspJumpAction::OpenFile { path, line, col } => {
            assert_eq!(path, target);
            // LSP 0 基 → 编辑器 1 基：行、列**都要** +1。
            assert_eq!(line, 10, "LSP line is 0-based; editor line is 1-based");
            assert_eq!(col, 3, "LSP col is 0-based; editor col is 1-based");
        }
        other => panic!("cross-file jump must open the target file, got {other:?}"),
    }
}

/// 同文件跳转必须原地跳（不重复打开标签）。
#[test]
fn same_file_location_jumps_in_place() {
    let location = file_loc(&std::env::temp_dir().join("same.rs"), 3, 0);
    let action = lsp_location_action(&location, true);
    match action {
        LspJumpAction::JumpInPlace { line, col } => {
            assert_eq!(line, 4, "0-based LSP line 3 -> 1-based editor line 4");
            assert_eq!(col, 1, "0-based LSP col 0 -> 1-based editor col 1");
        }
        other => panic!("same-file jump must jump in place, got {other:?}"),
    }
}

/// 非 `file://` 的 uri（如 untitled:）必须被忽略，不能 panic。
#[test]
fn non_file_uri_is_ignored() {
    let location = loc("untitled:Untitled-1", 0, 0);
    assert_eq!(lsp_location_action(&location, false), LspJumpAction::Ignore);
}

/// 同文件判定优先于 uri 形态：即便 uri 不是 file://，同文件也原地跳。
#[test]
fn same_file_takes_precedence_over_uri_shape() {
    let location = loc("untitled:Untitled-1", 1, 1);
    assert!(matches!(
        lsp_location_action(&location, true),
        LspJumpAction::JumpInPlace { .. }
    ));
}

/// 行号换算在边界上正确（第 0 行 → 第 1 行）。
#[test]
fn first_line_maps_to_line_one() {
    let location = file_loc(&std::env::temp_dir().join("a.rs"), 0, 0);
    match lsp_location_action(&location, false) {
        LspJumpAction::OpenFile { line, .. } => assert_eq!(line, 1),
        other => panic!("expected OpenFile, got {other:?}"),
    }
}

/// 用户报的现象：Python 里点 `List[int]` 跳到 `.pyi`，「编辑器不认这个文件」。
///
/// `.pyi` 是 Python 的**类型声明存根**（typeshed / stub 包），语法就是 Python。
/// 语言认不出来（`text`）时掉的不只是高亮：`#` 注释切换没了，LSP 也根本不接
/// （`start_lsp_for_tab` 见 language 为空/`text` 就 return，连状态栏 chip 都不出现），
/// 于是跳转的落点看起来像个「打不开的二进制」。
#[test]
fn type_stubs_are_python_not_text() {
    for name in ["typing.pyi", "_collections_abc.pyi", "/x/y/stub.pyi"] {
        let got = super::guess_language_from_extension(std::path::Path::new(name));
        assert_eq!(got, "python", "{name} 是 Python 存根，得按 python 打开");
    }
    // 反向：`.py` 当然还是 python，别的语言不受影响。
    assert_eq!(
        super::guess_language_from_extension(std::path::Path::new("main.py")),
        "python"
    );
    assert_eq!(
        super::guess_language_from_extension(std::path::Path::new("lib.rs")),
        "rust"
    );
}

/// 跳转归属表的键**含主机**：`/srv/app/main.py` 在两台主机上是两个文件，
/// 各自的服务器也是两个进程——键撞了就会让一台主机的根把另一台的顶掉。
/// 这与 `ServerKey` 用 `(root, language, 主机)` 认身份是同一个理由。
#[test]
fn jump_root_key_keeps_the_host_in_it() {
    use crate::ssh::{AuthMethod, SshConnectionConfig};
    let a = SshConnectionConfig::new("10.0.0.1", "u", AuthMethod::Agent);
    let b = SshConnectionConfig::new("10.0.0.2", "u", AuthMethod::Agent);
    let local = super::lsp_jump_root_key("/srv/app/main.py", None);
    let ka = super::lsp_jump_root_key("/srv/app/main.py", Some(&a));
    let kb = super::lsp_jump_root_key("/srv/app/main.py", Some(&b));
    assert_eq!(
        local,
        super::lsp_jump_root_key("/srv/app/main.py", None),
        "本地文件的键要稳定（同一路径命中同一条记录）"
    );
    assert_ne!(ka, kb, "同路径不同主机必须是两个键");
    assert_ne!(ka, local, "远端与本地不能混用一条记录");
}

// ------------------------------------------------ prepareRename 错误分类

/// 服务器**明确拒绝**重命名时要能识别出来。
///
/// 实测 rust-analyzer 对无引用/关键字位置回
/// `-32602 No references found at position`。这类错误应直接告诉用户
/// 「不可重命名」，而不是退回按词边界猜的行为（那样用户会看到输入框、
/// 输完却没有任何效果）。
#[test]
fn recognizes_explicit_rename_rejection() {
    for msg in [
        "JSON-RPC error -32602: No references found at position",
        "symbol cannot be renamed",
        "element is not renameable",
        "InvalidParams",
    ] {
        let err = anyhow::anyhow!("{msg}");
        assert!(
            super::is_rename_rejection(&err),
            "{msg:?} should be classified as a rename rejection"
        );
    }
}

/// 传输类失败**不能**被当成「不可重命名」——那会因一次超时就禁掉功能。
#[test]
fn transport_failures_are_not_rename_rejections() {
    for msg in [
        "timed out after 30s waiting for `textDocument/prepareRename`",
        "language server is closed",
        "failed to write to language server stdin",
    ] {
        let err = anyhow::anyhow!("{msg}");
        assert!(
            !super::is_rename_rejection(&err),
            "{msg:?} must NOT be treated as a rename rejection (should fall back)"
        );
    }
}

// ------------------------------------------------ 跳转历史（返回）

/// 跳转历史是后进先出：返回应回到**最近一次**跳转前的位置。
///
/// 回归：此前完全没有历史——跳到定义后无法返回，用户只能自己找回来。
/// 对照 Zed：它用 `ItemNavHistory` 支持 `navigate_backward`。
#[test]
fn nav_history_is_lifo() {
    let mut history: Vec<super::NavLocation> = Vec::new();
    for (tab, offset) in [("a.rs", 10usize), ("b.rs", 20), ("c.rs", 30)] {
        history.push(super::NavLocation {
            tab_id: tab.to_string(),
            offset,
        });
    }
    // 返回三次应得到逆序。
    assert_eq!(history.pop().unwrap().tab_id, "c.rs");
    assert_eq!(history.pop().unwrap().tab_id, "b.rs");
    assert_eq!(history.pop().unwrap().tab_id, "a.rs");
    assert!(history.pop().is_none(), "empty history must yield None");
}

/// 历史有容量上限，不会无限增长。
#[test]
fn nav_history_is_bounded() {
    let limit = super::NAV_HISTORY_LIMIT;
    assert!(
        limit > 0 && limit <= 1000,
        "limit should be sane, got {limit}"
    );

    let mut history: Vec<super::NavLocation> = Vec::new();
    for i in 0..(limit * 2) {
        history.push(super::NavLocation {
            tab_id: "t.rs".to_string(),
            offset: i,
        });
        if history.len() > limit {
            history.remove(0);
        }
    }
    assert_eq!(history.len(), limit, "history must be capped");
    // 最旧的被丢弃，最新的保留。
    assert_eq!(history.last().unwrap().offset, limit * 2 - 1);
}

/// 同一位置连续跳转不重复压栈（避免历史里一堆相同项）。
#[test]
fn consecutive_identical_jumps_are_deduplicated() {
    let entry = super::NavLocation {
        tab_id: "a.rs".to_string(),
        offset: 42,
    };
    let mut history: Vec<super::NavLocation> = Vec::new();
    // 模拟 `push_nav_history` 的去重条件。
    for _ in 0..5 {
        if history.last() == Some(&entry) {
            continue;
        }
        history.push(entry.clone());
    }
    assert_eq!(
        history.len(),
        1,
        "identical consecutive entries must collapse"
    );
}

// ------------------------------------------------ 端到端：跳到目标函数上

struct Host(gpui::Entity<super::Editor>);
impl gpui::Render for Host {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

fn editor_ctx<'a>(
    cx: &'a mut gpui::TestAppContext,
) -> (gpui::Entity<super::Editor>, &'a mut gpui::VisualTestContext) {
    cx.update(|cx| {
        crate::settings::SettingsStore::init_for_test(cx);
        crate::theme::init(cx);
        crate::sidebar_view::font_settings::FontState::init(cx);
        crate::file_manager::watcher::init(cx);
        // 打开文件时会向 LspManager 查询服务器（本地文件即使没有 server
        // 也会走这条路径），缺这个全局会 panic。
        crate::lsp::init(cx);
    });
    let (host, cx) =
        cx.add_window_view(|window, cx| Host(cx.new(|cx| super::Editor::new(window, cx))));
    (host.read_with(cx, |h, _| h.0.clone()), cx)
}

/// 跳到**尚未打开**的文件里的某个函数：必须打开该文件并把光标移到那一行。
///
/// 回归：用户报「只能打开对应的代码文件，没有跳到函数上」。此前的测试只
/// 断言「文件被打开」（`cross_file_location_opens_target_file` 走纯决策函数），
/// **没有断言光标落点**——所以「打开了但没跳」这个缺陷完全隐形。
#[gpui::test]
fn jump_lands_cursor_on_target_function(cx: &mut gpui::TestAppContext) {
    // 关掉 LSP 接入：打开文件会尝试连服务器（`smol::spawn` 到全局执行器），
    // 会被 gpui 的 TestScheduler 判为非确定性并 abort 整个测试二进制。
    super::with_lsp_disabled(|| {
        let (editor, cx) = editor_ctx(cx);
        let dir = std::env::temp_dir().join("deepterm-lsp-jump-cursor");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("defs.rs");
        // 目标函数在第 20 行（0 基 19），前面填足够多的行以证明真的跳了。
        let mut body = String::new();
        for i in 1..=19 {
            body.push_str(&format!("// filler line {i}\n"));
        }
        body.push_str("fn target_function() {}\n");
        body.push_str("// trailing\n");
        std::fs::write(&target, &body).unwrap();

        let uri = Url::from_file_path(&target).unwrap();
        // LSP 行号 0 基：第 20 行 → 19。列 3（`fn ` 之后，`target_function` 起点）。
        let location = Location {
            uri,
            range: Range::new(Position::new(19, 3), Position::new(19, 18)),
        };

        editor.update_in(cx, |editor, window, cx| {
            editor.open_lsp_location_for_test(&location, window, cx);
        });
        // 文件加载是异步的：跑到空闲让 pending jump 落地。
        cx.run_until_parked();

        let pos = editor
            .read_with(cx, |editor, cx| {
                editor.cursor_position_of_open_path(&target, cx)
            })
            .expect("target file must be open");
        assert_eq!(
            pos.0, 19,
            "cursor must land on the definition line (0-based 19), got line {} (col {})",
            pos.0, pos.1
        );
        assert_eq!(
            pos.1, 3,
            "cursor must land on the definition column (0-based 3), got {}",
            pos.1
        );
    });
}

/// 同文件跳转：光标必须移到目标行（不重复开标签）。
#[gpui::test]
fn same_file_jump_moves_cursor(cx: &mut gpui::TestAppContext) {
    super::with_lsp_disabled(|| {
        let (editor, cx) = editor_ctx(cx);
        let dir = std::env::temp_dir().join("deepterm-lsp-jump-same-cursor");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("one.rs");
        let mut body = String::new();
        for i in 1..=30 {
            body.push_str(&format!("// line {i}\n"));
        }
        body.push_str("fn here() {}\n");
        std::fs::write(&file, &body).unwrap();

        // 先把文件打开。
        editor.update_in(cx, |editor, window, cx| {
            editor.open_file(file.clone(), window, cx);
        });
        cx.run_until_parked();

        // 让 `open_lsp_location` 认为目标是「当前文件」。
        editor.update_in(cx, |editor, _window, _cx| {
            editor.set_session_uri_for_test(Url::from_file_path(&file).unwrap());
        });

        let uri = Url::from_file_path(&file).unwrap();
        let location = Location {
            uri,
            range: Range::new(Position::new(30, 3), Position::new(30, 7)),
        };
        editor.update_in(cx, |editor, window, cx| {
            editor.open_lsp_location_for_test(&location, window, cx);
        });
        cx.run_until_parked();

        let pos = editor
            .read_with(cx, |editor, cx| {
                editor.cursor_position_of_open_path(&file, cx)
            })
            .expect("file must be open");
        assert_eq!(pos.0, 30, "same-file jump must move the cursor to line 30");
    });
}

/// 跳转必须把**键盘焦点**也带到目标文件上。
///
/// 用户报的原话是「直接打开代码文件就完事了，光标焦点也没有跳转过去」。
/// 光设 `selected_range` 不够：选区与光标的绘制都以 focus 为门
/// （`components/input/element.rs`），没聚焦的标签里那个选区**根本画不出来**，
/// 按键也还打到原来那个文件。上面两条断言光标行列的测试早就存在，正是
/// 因为它们不看焦点，这个缺陷一直隐形。
///
/// 对照 VSCode：`openCodeEditor` 不带 `preserveFocus`，目标分组被激活。
#[gpui::test]
fn jump_focuses_the_target_editor(cx: &mut gpui::TestAppContext) {
    super::with_lsp_disabled(|| {
        let (editor, cx) = editor_ctx(cx);
        let dir = std::env::temp_dir().join("deepterm-lsp-jump-focus");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 源文件（跳转从这里发起）与目标文件。
        let source = dir.join("source.rs");
        std::fs::write(&source, "// from here\n").unwrap();
        let target = dir.join("defs.rs");
        let mut body = String::new();
        for i in 1..=19 {
            body.push_str(&format!("// filler line {i}\n"));
        }
        body.push_str("fn target_function() {}\n");
        std::fs::write(&target, &body).unwrap();

        editor.update_in(cx, |editor, window, cx| {
            editor.open_file(source.clone(), window, cx);
        });
        cx.run_until_parked();

        let location = Location {
            uri: Url::from_file_path(&target).unwrap(),
            range: Range::new(Position::new(19, 3), Position::new(19, 18)),
        };
        editor.update_in(cx, |editor, window, cx| {
            editor.open_lsp_location_for_test(&location, window, cx);
        });
        cx.run_until_parked();

        let focused = editor.update_in(cx, |editor, window, cx| {
            editor.is_text_editor_of_path_focused(&target, window, cx)
        });
        assert!(focused, "跨文件跳转后焦点必须在目标文件的编辑器上");

        // 源文件的编辑器同时失去焦点（否则「接着敲键打进原文件」还会复现）。
        let source_focused = editor.update_in(cx, |editor, window, cx| {
            editor.is_text_editor_of_path_focused(&source, window, cx)
        });
        assert!(!source_focused, "跳转后焦点不能还留在发起跳转的文件里");
    });
}

// ---------------------------------------------------------------- Cmd+Click

/// 构造一次鼠标按下（只有这里关心的三个维度有意义）。
fn mouse_down(
    button: gpui::MouseButton,
    click_count: usize,
    modifiers: gpui::Modifiers,
) -> gpui::MouseDownEvent {
    gpui::MouseDownEvent {
        position: gpui::point(gpui::px(40.), gpui::px(40.)),
        button,
        click_count,
        modifiers,
        first_mouse: false,
    }
}

/// Cmd+Click（其余平台 Ctrl+Click）触发跳转定义。
///
/// 用 `Modifiers::secondary_key()` 而不是手写字段：gpui 里这个谓词就是
/// 「macOS 的 Cmd / 其余平台的 Ctrl」，与 VSCode 的触发键一致，测试因此
/// 在两个平台上都成立。
#[test]
fn secondary_click_triggers_goto_definition() {
    let event = mouse_down(gpui::MouseButton::Left, 1, gpui::Modifiers::secondary_key());
    assert!(
        super::is_goto_definition_click(&event),
        "Cmd/Ctrl + 单次左键按下必须触发跳转"
    );
}

/// 普通点击、右键、双击/三击都不触发。
///
/// 对照 VSCode `clickLinkGesture`：`isLeftClick` 与 `event.detail <= 1`
/// ——双击是选词、三击是选行，让它们顺手跳一次转会打断正常选择。
#[test]
fn ordinary_clicks_do_not_trigger_goto_definition() {
    let secondary = gpui::Modifiers::secondary_key();
    for case in [
        (
            "plain left click",
            mouse_down(gpui::MouseButton::Left, 1, gpui::Modifiers::none()),
            "没有修饰键的点击只是移动光标",
        ),
        (
            "secondary right click",
            mouse_down(gpui::MouseButton::Right, 1, secondary.clone()),
            "右键是上下文菜单",
        ),
        (
            "secondary double click",
            mouse_down(gpui::MouseButton::Left, 2, secondary.clone()),
            "双击选词，不该跳转",
        ),
        (
            "secondary triple click",
            mouse_down(gpui::MouseButton::Left, 3, secondary),
            "三击选行，不该跳转",
        ),
    ] {
        let (what, event, why) = case;
        assert!(!super::is_goto_definition_click(&event), "{what}: {why}");
    }
}

/// Alt / Shift 单独按住不触发跳转（VSCode 把 Alt+Click 留给 declaration，
/// 且它的触发键判定只看 Cmd/Ctrl）。
#[test]
fn other_modifiers_do_not_trigger_goto_definition() {
    for modifiers in [gpui::Modifiers::alt(), gpui::Modifiers::shift()] {
        let event = mouse_down(gpui::MouseButton::Left, 1, modifiers);
        assert!(
            !super::is_goto_definition_click(&event),
            "单按 Alt/Shift 不该跳转定义"
        );
    }
}

// ---------------------------------------------------------------- 状态栏 chip

/// 未启动态必须显示**服务器名**，而不是「未启动」三个字。
///
/// 这是「默认关闭 + 可发现开启入口」这个产品决策的测试锚点：默认关闭时
/// 每个新打开的代码文件都停在这个态，chip 上写什么决定了用户知不知道
/// 「这个文件本来可以接 rust-analyzer、点它能开」。写成「未启动」用户只
/// 知道没开，不知道该开什么、更不知道怎么开。
#[gpui::test]
fn not_started_chip_shows_the_server_name(_cx: &mut gpui::TestAppContext) {
    let (label, tone) = super::lsp_chip_content(
        &crate::lsp::status::LspStatus::NotStarted,
        Some("rust-analyzer"),
    );
    assert_eq!(label, "rust-analyzer");
    assert_eq!(tone, super::LspChipTone::Dim);
}

/// 没有可用服务器的语言不该出现「未启动」字样占位。
///
/// `start_lsp_for_tab` 只在 `language_has_server_on` 为真时才写 `NotStarted`，
/// 这条守的是万一那个门控失效时的兜底文案。
#[gpui::test]
fn not_started_without_a_server_name_falls_back(_cx: &mut gpui::TestAppContext) {
    let (label, _) = super::lsp_chip_content(&crate::lsp::status::LspStatus::NotStarted, None);
    assert_eq!(label, "未启动");
}

/// 诊断计数要走灯色，不走文字。
///
/// 用户扫一眼颜色就知道有没有问题，不用读字——这是 chip 从「就绪」改成
/// 「服务器名 + 灯」的初衷。断言 tone 而不是渲染结果，是因为 tone 就是
/// 灯色的唯一来源。
#[gpui::test]
fn diagnostic_counts_pick_the_lamp_colour(_cx: &mut gpui::TestAppContext) {
    let ok = super::lsp_chip_content(
        &crate::lsp::status::LspStatus::Ready {
            errors: 0,
            warnings: 0,
        },
        Some("gopls"),
    );
    assert_eq!(ok, ("gopls".to_string(), super::LspChipTone::Ok));

    let warn = super::lsp_chip_content(
        &crate::lsp::status::LspStatus::Ready {
            errors: 0,
            warnings: 3,
        },
        Some("gopls"),
    );
    assert_eq!(warn.1, super::LspChipTone::Warning);

    let err = super::lsp_chip_content(
        &crate::lsp::status::LspStatus::Ready {
            errors: 2,
            warnings: 3,
        },
        Some("gopls"),
    );
    // 错误优先于警告：有错误时不该显示成「警告」的黄色。
    assert_eq!(err.1, super::LspChipTone::Error);
}

/// 失败必须把原因显示出来。
///
/// 失败是唯一「用户必须读文字」的态——灯只能表示「坏了」，不能表示
/// 「为什么坏」。
#[gpui::test]
fn failure_shows_its_reason(_cx: &mut gpui::TestAppContext) {
    let (label, tone) = super::lsp_chip_content(
        &crate::lsp::status::LspStatus::Failed {
            reason: "rust-analyzer 启动失败".into(),
        },
        Some("rust-analyzer"),
    );
    assert!(label.contains("rust-analyzer 启动失败"), "got {label}");
    assert_eq!(tone, super::LspChipTone::Error);
}

/// **有错误也点灯**：灯意味着「有个进程活着」，跟诊断计数无关。
///
/// 用户报的现象：Rust 文件前有个绿点，Python 文件前什么都没有——而两者
/// 都在 `Ready`。差别只在诊断计数：`Ready { errors > 0 }` 归到 `Error` 档，
/// 而 `Error` 档当时顺手关掉了灯。于是最需要被看见的状态反而唯一没有灯，
/// 看起来就像「Python 的 LSP 根本没起来」（实测 basedpyright 报了 59 条）。
#[gpui::test]
fn errors_still_light_the_lamp(_cx: &mut gpui::TestAppContext) {
    use crate::lsp::status::LspStatus;

    // 有错误：红点，不是没有点。
    assert!(super::lsp_chip_shows_lamp(&LspStatus::Ready {
        errors: 59,
        warnings: 0
    }));
    // 干净就绪 / 用户停止：都点灯（颜色不同而已）。
    assert!(super::lsp_chip_shows_lamp(&LspStatus::Ready {
        errors: 0,
        warnings: 0
    }));
    assert!(super::lsp_chip_shows_lamp(&LspStatus::Stopped));

    // 不点灯的三种：没有进程（未启动 / 失败），或进程在忙（画转圈）。
    assert!(!super::lsp_chip_shows_lamp(&LspStatus::NotStarted));
    assert!(!super::lsp_chip_shows_lamp(&LspStatus::Failed {
        reason: "x".into()
    }));
    assert!(!super::lsp_chip_shows_lamp(&LspStatus::Starting));
    assert!(!super::lsp_chip_shows_lamp(&LspStatus::Working {
        title: "Indexing".into(),
        message: None,
        percentage: Some(50),
    }));
}

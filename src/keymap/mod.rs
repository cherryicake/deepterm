//! 键位表：从 JSON 文件加载键位绑定。
//!
//! 分层与优先级由 gpui 的 keymap 决定：**同一上下文深度下，后加入的绑定胜出**
//! （`gpui/src/keymap.rs:188-190` 的排序是 `depth_b.cmp(depth_a).then(ix_b.cmp(ix_a))`）。
//! 因此加载顺序即优先级，从低到高：
//!
//! 1. `assets/keymaps/default.json` —— 内置默认键位
//! 2. `assets/keymaps/vim.json` —— vim 键位（仅当 `settings.vim.enabled`）
//! 3. `keymap.json` —— 用户覆盖
//!
//! 上下文谓词（`&&` `||` `!` `==` `>`）由 gpui 的
//! `KeyBindingContextPredicate` 解析，键位表里可以直接写
//! `"Input && vim_mode == normal"` 这类条件。

pub mod file;
pub mod registry;

use gpui::{App, KeyBinding};

/// 内置默认键位表。
const DEFAULT_KEYMAP: &str = include_str!("../../assets/keymaps/default.json");
/// 平台专属默认键位表。
#[cfg(target_os = "macos")]
const PLATFORM_KEYMAP: &str = include_str!("../../assets/keymaps/default-macos.json");
#[cfg(not(target_os = "macos"))]
const PLATFORM_KEYMAP: &str = include_str!("../../assets/keymaps/default-linux.json");
/// vim 键位表。
const VIM_KEYMAP: &str = include_str!("../../assets/keymaps/vim.json");

/// 初始化：加载内置键位表，并启动用户键位表的热重载。
pub fn init(cx: &mut App) {
    let (bindings, errors) = load_builtin(cx);
    log_errors(&errors);
    cx.bind_keys(bindings);

    registry::watch_user_keymap(cx);
}

/// 重建整个键位表：其它模块的绑定 + 内置 + vim（按开关）+ 用户覆盖。
///
/// `clear_key_bindings` 会清掉**所有**绑定，包括工作区 / 终端 / 弹窗 / 模态 /
/// 编辑器面板 / markdown 这些模块在各自 `init` 里注册的（共 8 处），而它们
/// 不会重新注册——结果是用户一改设置，那些快捷键就永久失效。
///
/// 因此所有模块都必须经 [`bind`] 注册：它把绑定记进 [`RECORDED`]，
/// `reload` 清空后按记录重放，谁都不会丢。
pub fn reload(cx: &mut App) {
    let (bindings, errors) = load_builtin(cx);
    log_errors(&errors);

    cx.clear_key_bindings();

    // 先重放其它模块的绑定，再压上本模块管理的三层。
    // 同深度下后加入者胜，所以内置 / vim / 用户的绑定优先级更高。
    let recorded = RECORDED.with(|r| r.borrow().clone());
    cx.bind_keys(recorded);

    cx.bind_keys(bindings);

    match registry::load_user_keymap(cx) {
        Ok((user_bindings, user_errors)) => {
            for error in &user_errors {
                log::warn!("keymap.json: {error}");
            }
            cx.bind_keys(user_bindings);
        }
        Err(err) => log::warn!("keymap.json 读取失败：{err}"),
    }
}

/// 注册键位并记录，使 [`reload`] 能重放。
///
/// 除本模块外的所有 `cx.bind_keys` 调用都应改走这里；直接调 `cx.bind_keys`
/// 的绑定会在下一次 `reload` 时丢失。
pub fn bind(cx: &mut App, bindings: impl IntoIterator<Item = KeyBinding>) {
    let bindings: Vec<KeyBinding> = bindings.into_iter().collect();
    RECORDED.with(|r| r.borrow_mut().extend(bindings.iter().cloned()));
    cx.bind_keys(bindings);
}

thread_local! {
    /// 各模块注册的键位，供 [`reload`] 重放。
    static RECORDED: std::cell::RefCell<Vec<KeyBinding>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 加载内置键位表（默认 + 平台专属 + 可选的 vim）。
fn load_builtin(cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut errors = Vec::new();

    for (label, source) in builtin_sources(cx) {
        let (loaded, mut errs) = file::parse(source, cx);
        bindings.extend(loaded);
        for error in &errs {
            errors.push(format!("{label}: {error}"));
        }
        errs.clear();
    }

    (bindings, errors)
}

/// 需要加载的内置键位表。vim 关闭时不加载 vim 表——
/// 否则 `vim_mode` 上下文永不出现，绑定也永不匹配，白白占用内存。
///
/// 设置 store 缺席时按「vim 关闭」处理：测试与独立渲染组件会只初始化
/// 键位表而不装 store。
fn builtin_sources(cx: &App) -> Vec<(&'static str, &'static str)> {
    let vim_enabled = cx
        .try_global::<crate::settings::SettingsStore>()
        .and_then(|store| store.get::<crate::settings::VimContent>())
        .is_some_and(|vim| vim.enabled);

    let mut sources = vec![
        ("default.json", DEFAULT_KEYMAP),
        ("default-platform.json", PLATFORM_KEYMAP),
    ];
    if vim_enabled {
        sources.push(("vim.json", VIM_KEYMAP));
    }
    sources
}

fn log_errors(errors: &[String]) {
    for error in errors {
        log::warn!("键位表：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 平台无关 + 平台专属 + vim 三份文档。
    fn builtin_documents() -> [(&'static str, &'static str); 3] {
        [
            ("default.json", DEFAULT_KEYMAP),
            ("default-platform.json", PLATFORM_KEYMAP),
            ("vim.json", VIM_KEYMAP),
        ]
    }

    fn binding_count(source: &str) -> usize {
        file::sections_of(source)
            .unwrap_or_else(|err| panic!("键位表解析失败：{err}"))
            .iter()
            .map(|section| section.bindings.iter().count())
            .sum()
    }

    /// 所有内置键位表必须是合法 JSON。
    #[test]
    fn builtin_keymaps_are_well_formed() {
        for (label, source) in builtin_documents() {
            file::sections_of(source).unwrap_or_else(|err| panic!("{label} 解析失败：{err}"));
        }
        assert!(binding_count(DEFAULT_KEYMAP) > 0, "default.json 不应为空");
        assert!(
            binding_count(PLATFORM_KEYMAP) > 0,
            "平台键位表不应为空"
        );
    }

    /// 迁移后键位数必须与迁移前 `state.rs` 的有效键位一致。
    ///
    /// 迁移前有 106 条 `KeyBinding::new`：30 条平台无关、46 条 macOS、
    /// 30 条非 macOS。同一平台只生效一半（`#[cfg]` 互斥），且 `shift-enter`
    /// 被后写的 `SearchPrevious` 覆盖，故：
    /// - macOS：29 + 46 = 75
    /// - 其它：29 + 30 = 59
    #[test]
    fn default_keymaps_cover_all_migrated_bindings() {
        let total = binding_count(DEFAULT_KEYMAP) + binding_count(PLATFORM_KEYMAP);
        let expected = if cfg!(target_os = "macos") { 75 } else { 59 };
        assert_eq!(total, expected, "迁移后键位数应与原 state.rs 的有效键位一致");
    }

    /// 每个 action 名都必须能在 gpui 的注册表里查到——否则该绑定是死键。
    #[test]
    fn every_action_name_is_registered() {
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            for (label, source) in builtin_documents() {
                let sections = file::sections_of(source).unwrap();
                for section in sections {
                    for (keystroke, action) in section.bindings.iter() {
                        let (name, params) =
                            file::split_action(action).unwrap_or_else(|err| {
                                panic!("{label}: 键位 `{keystroke}` 的 action 非法：{err}")
                            });
                        assert!(
                            cx.build_action(&name, params).is_ok(),
                            "{label}: 键位 `{keystroke}` 引用了未注册的 action `{name}`"
                        );
                    }
                }
            }
        });
    }

    /// 键位表能真正被 keymap 解析成绑定（不只是 JSON 合法）。
    ///
    /// 这条覆盖 `parse` 的完整路径：按名构造 action + 解析键序列 + 解析上下文谓词。
    #[gpui::test]
    fn builtin_keymaps_build_real_bindings(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut total = 0;
            for (label, source) in builtin_documents() {
                let (bindings, errors) = file::parse(source, cx);
                assert!(
                    errors.is_empty(),
                    "{label} 有绑定未能构造：{errors:?}"
                );
                assert_eq!(
                    bindings.len(),
                    binding_count(source),
                    "{label} 应把每条键位都构造成绑定"
                );
                total += bindings.len();
            }
            assert!(total > 0);
        });
    }

    /// 加载进 keymap 后，编辑器键位必须真正可被击键匹配到。
    ///
    /// 这是「迁移到 JSON 后键位没有静默失效」的端到端判据：构造与渲染时
    /// 相同的上下文栈（`Input`），断言 `left` / `cmd-backspace` 等键位
    /// 解析出的 action 名与迁移前一致。
    #[gpui::test]
    fn migrated_bindings_resolve_under_input_context(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            init(cx);

            // 与 `EditorInput` 渲染时一致的上下文。
            let context = gpui::KeyContext::parse("Input").unwrap();
            let stack = [context];

            let resolve = |cx: &mut gpui::App, keystroke: &str| -> Option<String> {
                let keys: Vec<gpui::Keystroke> = keystroke
                    .split_whitespace()
                    .map(|key| gpui::Keystroke::parse(key).unwrap())
                    .collect();
                let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
                bindings.first().map(|binding| binding.action().name().to_string())
            };

            // 迁移前由 `state.rs` 的 `cx.bind_keys` 提供；迁移后由 JSON 提供。
            assert_eq!(resolve(cx, "left").as_deref(), Some("editor::MoveLeft"));
            assert_eq!(resolve(cx, "right").as_deref(), Some("editor::MoveRight"));
            assert_eq!(resolve(cx, "escape").as_deref(), Some("editor::Escape"));
            assert_eq!(resolve(cx, "enter").as_deref(), Some("editor::Enter"));
            assert_eq!(
                resolve(cx, "shift-enter").as_deref(),
                Some("editor::SearchPrevious"),
                "迁移前的后写覆盖语义必须保持"
            );
            assert_eq!(resolve(cx, "alt-z").as_deref(), Some("editor::ToggleSoftWrap"));
            assert_eq!(
                resolve(cx, "alt-shift-up").as_deref(),
                Some("editor::DuplicateLine")
            );

            #[cfg(target_os = "macos")]
            {
                assert_eq!(
                    resolve(cx, "cmd-backspace").as_deref(),
                    Some("editor::DeleteToBeginningOfLine"),
                    "平台专属键位（macOS）必须生效"
                );
                assert_eq!(resolve(cx, "cmd-a").as_deref(), Some("editor::SelectAll"));
                assert_eq!(
                    resolve(cx, "alt-cmd-1").as_deref(),
                    Some("editor::FoldLevel")
                );
            }
            #[cfg(not(target_os = "macos"))]
            {
                assert_eq!(
                    resolve(cx, "ctrl-backspace").as_deref(),
                    Some("editor::DeleteToPreviousWordStart"),
                    "平台专属键位（非 macOS）必须生效"
                );
                assert_eq!(resolve(cx, "ctrl-a").as_deref(), Some("editor::SelectAll"));
            }
        });
    }

    /// vim 键位表只在开关打开时加载，且加载后 `vim::` 绑定必须真正可解析。
    ///
    /// 这条覆盖「设置 → 键位表」这条链路：关着的时候 `h` 不该是 vim 命令，
    /// 打开并重建键位表之后必须变成 `vim::Left`。
    #[gpui::test]
    fn vim_keymap_loads_only_when_enabled(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::settings::SettingsStore::init_for_test(cx);

            let resolve = |cx: &mut gpui::App| -> Option<String> {
                // 与 `EditorInput` 在 Normal 模式下产出的上下文一致。
                let mut context = gpui::KeyContext::new_with_defaults();
                context.add("Input");
                context.add("VimControl");
                context.set("vim_mode", "normal");
                context.set("vim_operator", "none");
                let stack = [context];
                let keys = vec![gpui::Keystroke::parse("h").unwrap()];
                let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
                bindings.first().map(|binding| binding.action().name().to_string())
            };

            // 默认关闭：`h` 不应解析到 vim 命令。
            reload(cx);
            assert_ne!(
                resolve(cx).as_deref(),
                Some("vim::Left"),
                "vim 关闭时不应加载 vim 键位表"
            );

            // 打开开关并重建：`h` 必须变成 vim 的移动。
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            reload(cx);
            assert_eq!(
                resolve(cx).as_deref(),
                Some("vim::Left"),
                "vim 打开后 `h` 应是 vim::Left"
            );

            // 关掉再重建：应退回未加载状态。
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = false;
            });
            reload(cx);
            assert_ne!(
                resolve(cx).as_deref(),
                Some("vim::Left"),
                "关闭 vim 后应卸载 vim 键位表"
            );
        });
    }

    /// 操作符待决时，第二个 `d` 必须仍解析到 `vim::PushDelete`。
    ///
    /// 这是 `dd` 的关键：第一次 `d` 在 `vim_operator == none` 段命中，
    /// 第二次 `d` 必须能在 `vim_operator == d` 段命中同一条 action，
    /// 由 handler 依据操作符栈判定「重复 = 整行」。两段里缺任何一条，
    /// `dd` 都会变成「`d` + 无操作」。
    ///
    /// 上下文栈按真实渲染层级构造：`AppWindow` > `EditorPanel` > `Input`。
    /// 谓词只在**最深**的上下文上求值，所以 `Input` 上的 vim 键位必须
    /// 在有多层父上下文时依然命中——这是测试环境与真实编辑器唯一的
    /// 结构差异，必须覆盖。
    #[gpui::test]
    fn operator_context_still_resolves_operator_keys(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            reload(cx);

            let resolve = |cx: &mut gpui::App, operator: &str, key: &str| -> Option<String> {
                let mut app_window = gpui::KeyContext::new_with_defaults();
                app_window.add("AppWindow");
                let mut panel = gpui::KeyContext::new_with_defaults();
                panel.add("EditorPanel");
                let mut input = gpui::KeyContext::new_with_defaults();
                input.add("Input");
                input.add("VimControl");
                input.set("vim_mode", "normal");
                input.set("vim_operator", operator);
                // 由外到内，与 `AppWindow` → `EditorPanel` → `EditorInput` 一致。
                let stack = [app_window, panel, input];
                let keys = vec![gpui::Keystroke::parse(key).unwrap()];
                let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
                bindings.first().map(|binding| binding.action().name().to_string())
            };

            // 无操作符时 `d` 开启操作符。
            assert_eq!(
                resolve(cx, "none", "d").as_deref(),
                Some("vim::PushDelete"),
                "`vim_operator == none` 段必须绑定 `d`"
            );
            // 操作符待决时 `d` 仍是 PushDelete（handler 据此判定 `dd`）。
            assert_eq!(
                resolve(cx, "d", "d").as_deref(),
                Some("vim::PushDelete"),
                "`vim_operator == d` 段必须绑定 `d`，否则 `dd` 失效"
            );
            // 同理适用于 `y` / `c`。
            assert_eq!(
                resolve(cx, "y", "y").as_deref(),
                Some("vim::PushYank"),
                "`yy` 依赖 `y` 在操作符段可解析"
            );
            assert_eq!(
                resolve(cx, "c", "c").as_deref(),
                Some("vim::PushChange"),
                "`cc` 依赖 `c` 在操作符段可解析"
            );
            // 对象前缀段：`diw` 的 `w` 是对象名而非移动。
            assert_eq!(
                resolve(cx, "i", "w").as_deref(),
                Some("vim::ObjectKey"),
                "对象前缀段必须把 `w` 解析为对象名"
            );
            // 操作符段的移动仍然可用。
            assert_eq!(
                resolve(cx, "d", "w").as_deref(),
                Some("vim::NextWordStart"),
                "对象前缀段之外，操作符段的 `w` 仍是移动"
            );
        });
    }

    /// `reload` 不得丢掉其它模块注册的绑定。
    ///
    /// 回归背景：`reload` 起初用 `clear_key_bindings()` 清空整个键位表，
    /// 而工作区 / 终端 / 弹窗 / 模态 / 编辑器面板 / markdown 这些模块的绑定
    /// 只在各自 `init` 里注册一次，不会重来——用户一改设置，那些快捷键就
    /// 永久失效。修法是让所有模块经 [`bind`] 注册，`reload` 重放记录。
    #[gpui::test]
    fn reload_preserves_bindings_from_other_modules(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::settings::SettingsStore::init_for_test(cx);

            // 模拟另一个模块（模态弹窗）注册一条键位。
            bind(
                cx,
                [gpui::KeyBinding::new(
                    "escape",
                    crate::components::modal::Cancel,
                    Some("ModalTest"),
                )],
            );

            let resolve = |cx: &mut gpui::App, context: &str| -> Option<String> {
                let ctx = gpui::KeyContext::parse(context).unwrap();
                let stack = [ctx];
                let keys = vec![gpui::Keystroke::parse("escape").unwrap()];
                let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
                bindings.first().map(|b| b.action().name().to_string())
            };

            let context = "ModalTest";
            assert!(
                resolve(cx, context).is_some(),
                "前置：其它模块的绑定应已生效"
            );

            // 触发一次 reload（模拟用户改设置）。
            reload(cx);

            assert!(
                resolve(cx, context).is_some(),
                "`reload` 后其它模块的绑定必须仍在（否则改设置会让快捷键永久失效）"
            );
        });
    }

    /// 用户 `keymap.json` 的绑定必须能覆盖内置键位。
    ///
    /// 覆盖依赖 gpui 的「同深度下后加入者胜」，所以 `reload` 里用户表
    /// 必须最后加载。
    #[gpui::test]
    fn user_keymap_overrides_builtin(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            init(cx);

            let context = gpui::KeyContext::parse("Input").unwrap();
            let stack = [context];
            let resolve = |cx: &mut gpui::App| -> Option<String> {
                let keys = vec![gpui::Keystroke::parse("left").unwrap()];
                let (bindings, _) = cx.key_bindings().borrow().bindings_for_input(&keys, &stack);
                bindings.first().map(|binding| binding.action().name().to_string())
            };

            assert_eq!(resolve(cx).as_deref(), Some("editor::MoveLeft"));

            // 模拟用户把 `left` 改绑到另一个 action。
            let user = r#"[{ "context": "Input", "bindings": { "left": "editor::MoveRight" } }]"#;
            let (bindings, errors) = file::parse(user, cx);
            assert!(errors.is_empty(), "{errors:?}");
            cx.bind_keys(bindings);

            assert_eq!(
                resolve(cx).as_deref(),
                Some("editor::MoveRight"),
                "用户绑定应覆盖内置绑定"
            );
        });
    }

    /// vim 的 `g` 系列 LSP 跳转绑定必须解析到真实动作。
    ///
    /// 回归：`g d`（跳转定义）此前**完全没有绑定**——键位表里只有
    /// `g g` / `g v` / `g c`，用户按 `gd` 毫无反应。这是 vim 用户最常用的
    /// 跳转手势，缺失等于「LSP 跳转不工作」。
    ///
    /// 断言的是**解析出的动作名**（而非「表里有这条记录」）：动作名或命名
    /// 空间写错会被 `build_action` 拒绝，键位会静默失效。
    #[gpui::test]
    fn vim_g_series_lsp_bindings_resolve(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            reload(cx);

            // 与 `EditorInput` 在 Normal 模式下产出的上下文一致。
            let mut context = gpui::KeyContext::new_with_defaults();
            context.add("Input");
            context.add("VimControl");
            context.set("vim_mode", "normal");
            context.set("vim_operator", "none");
            let stack = [context];

            // 多键序列（`g d`）必须把**所有**键一起喂进去才会「matched」；
            // 只喂 `g` 时它是 pending。`bindings_for_input` 返回
            // `(matched, pending)`，这里取 matched。
            let resolve = |cx: &mut gpui::App, keys: &str| -> Option<String> {
                let strokes: Vec<gpui::Keystroke> = keys
                    .split_whitespace()
                    .map(|k| gpui::Keystroke::parse(k).unwrap())
                    .collect();
                let (matched, _pending) =
                    cx.key_bindings().borrow().bindings_for_input(&strokes, &stack);
                matched
                    .first()
                    .map(|binding| binding.action().name().to_string())
            };

            // VSCodeVim 的 g 系列约定。
            assert_eq!(
                resolve(cx, "g d").as_deref(),
                Some("editor_panel::GotoDefinition"),
                "gd 必须绑定到跳转定义"
            );
            assert_eq!(
                resolve(cx, "g D").as_deref(),
                Some("editor_panel::GotoDeclaration"),
                "gD 必须绑定到跳转声明"
            );
            assert_eq!(
                resolve(cx, "g y").as_deref(),
                Some("editor_panel::GotoTypeDefinition"),
                "gy 必须绑定到跳转类型定义"
            );
            assert_eq!(
                resolve(cx, "g i").as_deref(),
                Some("editor_panel::GotoImplementation"),
                "gi 必须绑定到跳转实现"
            );
            assert_eq!(
                resolve(cx, "g r").as_deref(),
                Some("editor_panel::FindReferences"),
                "gr 必须绑定到查找引用"
            );
        });
    }

    /// 键位表里的所有动作都必须能构造（名字与命名空间正确）。
    ///
    /// 通用护栏：写错动作名只会被记进 errors 而**不会让加载失败**，
    /// 表现为「这个键静默失效」——`g d` 缺失时同类的隐形故障。
    #[gpui::test]
    fn all_keymap_actions_resolve(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::settings::SettingsStore::init_for_test(cx);
            // 打开 vim，让三份表都被解析。
            crate::settings::SettingsStore::update_user(cx, |content| {
                content.vim.enabled = true;
            });
            let (_, errors) = super::load_builtin(cx);
            assert!(
                errors.is_empty(),
                "键位表存在无法解析的动作/上下文：{errors:#?}"
            );
        });
    }
}

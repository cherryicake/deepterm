//! Vim 模式。
//!
//! 结构对齐 Zed 的 `crates/vim`：模式与待决状态是**两件事**。
//! `VimMode` 只有编辑语义上的几种模式，而「等待后续输入」的状态
//! （操作符栈、计数、寄存器选择）单独存放，不塞进 `Mode` 的变体里。
//! 这样 `d`、`2`、`"a` 这类前缀可以组合，而不需要为每种组合加一个模式。
//!
//! 键位分发不在这里做按键解析：模式写入 [`KeyContext`]（`vim_mode` /
//! `vim_operator` / `VimControl`），由 `keymap.json` 按上下文匹配。
//! 这是 Zed 的做法，也是唯一能让键位可配置的分发方式。
//!
//! [`KeyContext`]: gpui::KeyContext

pub mod actions;
pub mod input;
pub mod marks;
pub mod motion;
pub mod object;
pub mod operator;
pub mod registers;
pub mod state;

#[cfg(test)]
#[path = "vim_tests.rs"]
mod vim_tests;

pub use marks::Marks;
pub use registers::Registers;
pub use state::{MotionKind, Operator, VimMode, VimState};

use gpui::{App, KeyContext};

/// 初始化 vim。
///
/// action 的注册走编译期的 `inventory`（`actions!` 宏内部），无需运行时注册；
/// 键位表由 `crate::keymap` 在 `settings.vim.enabled` 时加载。
/// 这里只做一次设置一致性检查，便于定位「开了开关但没生效」的问题。
pub fn init(cx: &mut App) {
    // 寄存器跨标签共享（vim 里 `"ayy` 之后切文件也能 `"ap`），所以是全局的。
    Registers::init(cx);
    Marks::init(cx);
    if enabled(cx) {
        log::info!("vim 模式已启用");
    }
}

impl VimState {
    /// 把模式写入键位上下文。
    ///
    /// 产出的谓词与 `assets/keymaps/vim.json` 里的 `context` 字段一一对应：
    ///
    /// - `VimControl` —— Normal / Visual / 操作符待决时存在，
    ///   用于「只在 vim 控制下生效」的通用键位。
    /// - `vim_mode == normal|insert|replace|visual` —— 模式专属键位。
    /// - `vim_operator == d|y|c|i|a|none` —— 操作符待决时的后续键位。
    pub fn extend_key_context(&self, context: &mut KeyContext) {
        let mode = match self.mode {
            VimMode::Normal => "normal",
            VimMode::Insert => "insert",
            VimMode::Replace => "replace",
            VimMode::Visual | VimMode::VisualLine => "visual",
        };

        let operator = self.active_operator_id();

        // Normal 与 Visual 下 vim 接管普通按键；Insert 下放行给文本输入。
        if matches!(
            self.mode,
            VimMode::Normal | VimMode::Visual | VimMode::VisualLine
        ) {
            context.add("VimControl");
        }

        context.set("vim_mode", mode);
        context.set("vim_operator", operator);
    }

    /// 当前操作符的键位标识；无操作符时为 `none`。
    ///
    /// 返回 `&'static str` 而非 `String`：这些标识同时用于键位表与上下文，
    /// 是编译期常量，没有理由每次渲染都分配。
    pub fn active_operator_id(&self) -> &'static str {
        match self.operator_stack.last() {
            Some(Operator::Delete) => "d",
            Some(Operator::Yank) => "y",
            Some(Operator::Change) => "c",
            Some(Operator::Object { around: false }) => "i",
            Some(Operator::Object { around: true }) => "a",
            None => "none",
        }
    }

    /// 是否处于「不接受文本输入」的模式。
    ///
    /// Normal / Visual 下按键是命令而非文本，平台输入处理器必须让路，
    /// 否则 `h` 会被当作字符插入。
    pub fn blocks_text_input(&self) -> bool {
        matches!(
            self.mode,
            VimMode::Normal | VimMode::Visual | VimMode::VisualLine
        )
    }
}

/// 用户是否开启了 vim 模式。
pub fn enabled(cx: &App) -> bool {
    cx.try_global::<crate::settings::SettingsStore>()
        .and_then(|store| store.get::<crate::settings::VimContent>())
        .is_some_and(|vim| vim.enabled)
}

/// 普通输入框的键位上下文标记：vim 键位对这个输入框整体不适用。
///
/// # 为什么需要它
///
/// gpui 解析绑定时按**最深的能匹配上的层级**取用（`KeyContext::depth_of`
/// 从最深往上找），于是祖先的上下文可以替后代命中谓词。编辑器的查找框
/// 正是这种结构：它是普通输入框，却渲染在编辑器 `key_context` 的子树里。
/// 编辑器处于 Normal 模式时，查找框的上下文栈是
/// `… > Input(VimControl, vim_mode=normal) > SearchPanel > Input`——
/// 查找框自己不带 `VimControl`，但 gpui 退到祖先那一层就匹配上了，
/// 于是 `h` 被派发成 `vim::Left`：字符进不了查找框，编辑器光标反而在动。
///
/// 只在祖先上加标记解决不了（祖先满足谓词就够），必须让**最深的那层**
/// 带一个谓词能排除的标记。`vim.json` 的每段因此都带上 `!NoVim`——
/// gpui 的 `Not` 会在**所有**层级上求值，只要栈里任何一层带了这个标记，
/// 整条 vim 绑定就不再匹配，嵌套的输入框因此彻底退出 vim 的管辖。
///
/// # 注意：这个标记会向下继承
///
/// 因为 `Not` 扫的是整条栈，「祖先带 `NoVim`」对它的**所有后代**都成立：
/// 若将来某个代码编辑器被渲染进一个带 `NoVim` 的子树（普通输入框里内嵌
/// md 预览之类），它的 vim 会静默失效，而且现场极难定位——键位表明明加载了、
/// `vim_capable()` 也是真的，只是谓词再也匹配不上。
///
/// 现状是安全的：编辑器只会把**查找框 / 替换框**挂进自己的 `key_context`
/// （方向相反，查找框是后代），没有任何代码编辑器被嵌在别的输入框里。新增
/// 这种嵌套前先想清楚方向。
pub const NO_VIM_CONTEXT: &str = "NoVim";

impl crate::components::input::InputState {
    /// 本输入框是否接受 vim 状态。
    ///
    /// 只有代码编辑器（含以「编辑」模式打开的 md 文件）接受：普通输入框里
    /// `h`/`j`/`k`/`l` 是要输入的字符而不是移动，一旦装上 vim，Normal 模式
    /// 会吞掉它们，搜索框、表单、提交框会变成「打不了字」。
    pub(crate) fn vim_capable(&self) -> bool {
        self.mode.is_code_editor()
    }

    /// 按设置同步 vim 状态：能装且开关打开时装上，否则清掉。
    ///
    /// 返回状态是否发生变化，调用方据此决定要不要 `notify`。
    /// 唯一入口——创建与设置变更两条路径共用，避免两处判断漂移。
    pub(crate) fn sync_vim_state(&mut self, cx: &App) -> bool {
        if self.vim_capable() && enabled(cx) {
            if self.vim.is_none() {
                self.vim = initial_state(cx);
                return true;
            }
        } else if self.vim.is_some() {
            self.vim = None;
            self.vim_insert_buffer.clear();
            self.vim_insert_start = None;
            return true;
        }
        false
    }
}

/// 按设置同步所有编辑器的 vim 状态。
///
/// 开启时给尚未启用 vim 的编辑器装上状态；关闭时清掉——清掉是必需的，
/// 否则残留的 Normal 模式会继续吞按键，而 vim 键位表已随开关卸载，
/// 编辑器在重启前无法输入。
///
/// 只装 [`InputState::vim_capable`] 的输入框：vim 是给代码/文本编辑器的，
/// 普通输入框（搜索、表单、提交框）装上它连打字都会失效。
pub fn disable_all(cx: &mut App) {
    let inputs = crate::components::input::state::all_input_states();

    for input in inputs {
        input.update(cx, |state, cx| {
            if state.sync_vim_state(cx) {
                cx.notify();
            }
        });
    }
}

/// 从设置构造初始 vim 状态。
pub fn initial_state(cx: &App) -> Option<VimState> {
    if !enabled(cx) {
        return None;
    }
    let settings = cx
        .try_global::<crate::settings::SettingsStore>()
        .and_then(|store| store.get::<crate::settings::VimContent>())?;
    Some(VimState::from_settings(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_of(state: &VimState) -> KeyContext {
        let mut context = KeyContext::new_with_defaults();
        context.add("Input");
        state.extend_key_context(&mut context);
        context
    }

    #[test]
    fn normal_mode_blocks_text_input_and_sets_context() {
        let state = VimState::default();
        assert!(state.blocks_text_input(), "Normal 不接受文本输入");

        let context = context_of(&state);
        assert!(context.contains("VimControl"));
        assert_eq!(context.get("vim_mode").map(|v| v.as_ref()), Some("normal"));
        assert_eq!(
            context.get("vim_operator").map(|v| v.as_ref()),
            Some("none")
        );
    }

    #[test]
    fn insert_mode_allows_text_input_and_omits_vim_control() {
        let state = VimState {
            mode: VimMode::Insert,
            ..VimState::default()
        };
        assert!(!state.blocks_text_input(), "Insert 必须接受文本输入");

        let context = context_of(&state);
        assert_eq!(context.get("vim_mode").map(|v| v.as_ref()), Some("insert"));
        assert!(
            !context.contains("VimControl"),
            "Insert 下不应暴露 vim 通用键位"
        );
    }

    #[test]
    fn visual_modes_map_to_visual_context() {
        for mode in [VimMode::Visual, VimMode::VisualLine] {
            let state = VimState {
                mode,
                ..VimState::default()
            };
            let context = context_of(&state);
            assert_eq!(
                context.get("vim_mode").map(|v| v.as_ref()),
                Some("visual"),
                "{mode:?}"
            );
            assert!(context.contains("VimControl"), "{mode:?}");
        }
    }

    #[test]
    fn operator_stack_drives_operator_context() {
        let mut state = VimState::default();
        assert_eq!(state.active_operator_id(), "none");

        state.operator_stack.push(Operator::Delete);
        assert_eq!(state.active_operator_id(), "d");
        assert_eq!(
            context_of(&state).get("vim_operator").map(|v| v.as_ref()),
            Some("d")
        );

        state
            .operator_stack
            .push(Operator::Object { around: false });
        assert_eq!(state.active_operator_id(), "i", "内层对象前缀优先");

        state.operator_stack.push(Operator::Object { around: true });
        assert_eq!(state.active_operator_id(), "a");
    }

    #[test]
    fn settings_drive_default_mode_and_cursor() {
        let mut settings = crate::settings::VimContent {
            enabled: true,
            ..Default::default()
        };
        settings.default_mode = "insert".to_string();
        let state = VimState::from_settings(&settings);
        assert_eq!(state.mode, VimMode::Insert);

        settings.default_mode = "normal".to_string();
        settings.normal_cursor_shape = "hollow".to_string();
        let state = VimState::from_settings(&settings);
        assert_eq!(state.mode, VimMode::Normal);
        assert_eq!(
            state.normal_cursor_shape,
            crate::components::input::CursorShape::Hollow
        );
    }

    #[test]
    fn unknown_cursor_shape_falls_back_to_block() {
        let settings = crate::settings::VimContent {
            enabled: true,
            normal_cursor_shape: "nonsense".to_string(),
            ..Default::default()
        };
        let state = VimState::from_settings(&settings);
        assert_eq!(
            state.normal_cursor_shape,
            crate::components::input::CursorShape::Block
        );
    }
}

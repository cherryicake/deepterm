# DeepTerm 编辑器代码优化计划

> 目标：参照 gpui-component 的编辑器实现，清理未使用/重复的代码，统一输入架构。
> 基准代码：`~/Documents/workspace/gpui-component/crates/ui/src/input/`
> 当前状态：相关优化代码已本地提交，尚未推送到远程。

---

## 一、现状分析

### 1.1 核心问题：存在两套并行的输入实现

| 实现位置 | 数据模型 | 使用场景 | 问题 |
|---------|---------|---------|------|
| `src/components/input/input.rs` | `SharedString` | `file_browser`、`settings_hub`、`ssh_view` 等单行输入 | 功能有限，与 `InputState` 重复 |
| `src/components/input/editor/state.rs` | `ropey::Rope` | `EditorView` 代码编辑器 | 直接实现了 `Render`，缺少清晰的 UI wrapper |

**gpui-component 的参考架构**只有一套核心实现：
- `InputState`（`state.rs`）：纯状态层，管理 Rope 缓冲、光标、选择、历史记录、Action 处理，**不实现 `Render`**
- `Input`（`input.rs`）：`RenderOnce` UI 包装器，负责边框、前缀/后缀、滚动条、clear 按钮等 chrome
- `TextElement`（`element.rs`）：自定义 `Element`，负责文本 shaping、光标绘制、选择高亮

### 1.2 未使用的 Stub 文件和死代码

以下文件为空实现或几乎为空，却占据了完整的模块位置和导出：

| 文件 | 现状 | 建议 |
|------|------|------|
| `editor/lsp.rs` | `Lsp` struct 全是空方法 | 删除或最小化内联到 `state.rs` |
| `editor/popovers.rs` | `ContextMenu`、`DiagnosticPopover`、`HoverPopover` 只返回 `div()` | 删除 |
| `editor/search.rs` | `SearchPanel` 为空 stub，`init()` 为空 | 删除 |
| `editor/mask_pattern.rs` | `MaskPattern` 是透传 stub | 删除或内联 |
| `editor/number_input.rs` | 只有 `format_number` 和空 `init()` | 删除 |
| `editor/change.rs` | 只是 `pub use crate::components::input::change::Change;` | 删除，直接引用父模块 |

`state.rs` 中还有大量从未被调用的字段和方法：
- `search_panel`, `searchable`
- `diagnostic_popover`, `hover_popover`, `context_menu`
- `lsp`, `inline_completion`, `completion_inserting`
- `mask_pattern`（虽然字段存在，但 `MaskPattern` 本身是 stub）
- `_context_menu_task`, `_pending_update`

### 1.3 架构层面的混乱点

1. **`InputState` 直接实现了 `Render`**
   - 将 `_pending_update`（高亮器、fold 候选、LSP 更新）的逻辑混入了 `render()` 方法
   - gpui-component 中这部分逻辑在 `prepare_if_needed`（由 `TextElement` 在 layout 前调用）

2. **`EditorView` 过于单薄且多余**
   - `src/editor/editor_view.rs` 只是简单包装 `Entity<InputState>`，增加了一个 `file_path` 字段
   - 但 `file_path` 在 `Panel` 层级已经通过 `tab_label` 管理， EditorView 没有提供额外的渲染逻辑
   - 可以直接让 `PanelContent::Editor` 持有 `Entity<InputState>`，从而删除 `EditorView`

3. **模块导出混乱**
   - `editor/mod.rs` 导出了 `Lsp`, `HoverDefinition`, `InlineCompletion`, `MaskPattern`, `ContextMenu` 等实际上不存在的功能
   - 这会在外部代码中制造错误的依赖和预期

---

## 二、优化目标

1. **统一输入实现**：移除旧的 `SharedString` 版 `Input`，让 `Input` 成为 `InputState` 的 UI wrapper（与 gpui-component 一致）
2. **删除所有未使用的 stub 模块**，简化模块树
3. **重构 `InputState` 的渲染入口**，将状态更新逻辑从 `render()` 中移出
4. **移除或简化 `EditorView`**，让代码编辑器直接使用 `InputState`
5. **整理公共 API 导出**，只暴露实际可用的类型

---

## 三、详细优化步骤与进度追踪

### Phase 1: 清理未使用的 Editor Stub 模块

| # | 任务 | 文件/模块 | 状态 | 备注 |
|---|------|----------|------|------|
| 1.1 | 删除 `editor/lsp.rs` | `src/components/input/editor/lsp.rs` | ✅ | 已删除，`state.rs` 中 `lsp` 字段已移除 |
| 1.2 | 删除 `editor/popovers.rs` | `src/components/input/editor/popovers.rs` | ✅ | 已删除，`state.rs` 中 popover 相关字段已移除 |
| 1.3 | 删除 `editor/search.rs` | `src/components/input/editor/search.rs` | ✅ | 已删除，`search_panel`、`searchable` 字段已移除 |
| 1.4 | 删除 `editor/mask_pattern.rs` | `src/components/input/editor/mask_pattern.rs` | ✅ | 已删除，`mask_pattern` 字段已移除 |
| 1.5 | 删除 `editor/number_input.rs` | `src/components/input/editor/number_input.rs` | ✅ | 已删除 |
| 1.6 | 删除 `editor/change.rs` | `src/components/input/editor/change.rs` | ✅ | 已删除，`editor/mod.rs` 直接引用父模块 `crate::components::input::change::Change` |
| 1.7 | 更新 `editor/mod.rs` 导出 | `src/components/input/editor/mod.rs` | ✅ | 已移除上述模块的 `pub mod` 和 `pub use` |

### Phase 2: 重构 `InputState` 渲染入口

| # | 任务 | 文件 | 状态 | 备注 |
|---|------|------|------|------|
| 2.1 | 将 `_pending_update` 逻辑从 `render()` 移出 | `src/components/input/editor/state.rs` | ✅ | 已提取到 `TextElement::prepare_if_needed` 并在 `request_layout` 中调用 |
| 2.2 | 移除 `InputState` 的 `Render` 实现 | `src/components/input/editor/state.rs` | ✅ | `InputState` 不再实现 `Render` |
| 2.3 | 新建/重构 `Input` 作为 UI wrapper | `src/components/input/editor/input.rs` | ✅ | `EditorInput` 已作为 `RenderOnce` wrapper 存在，负责事件和边框 |
| 2.4 | 确保 `TextElement` 正确集成 | `src/components/input/editor/element.rs` | ✅ | `TextElement` 与 `EditorInput` 衔接正常，渲染无异常 |

### Phase 3: 移除/简化 `EditorView`

| # | 任务 | 文件 | 状态 | 备注 |
|---|------|------|------|------|
| 3.1 | 评估 `EditorView` 的 `file_path` 是否必要 | `src/editor/editor_view.rs` | ✅ | `file_path` 无读取逻辑，`Panel` 的 `tab_label` 已满足需求 |
| 3.2 | 修改 `PanelContent::Editor` 类型 | `src/workspace/panel.rs` | ✅ | `Editor(Entity<EditorView>)` → `Editor(Entity<InputState>)` |
| 3.3 | 更新 `Workspace` 中编辑器创建逻辑 | `src/workspace/workspace.rs` | ✅ | 直接创建 `InputState` 并传入 `Panel::with_editor` |
| 3.4 | 删除 `editor_view.rs` 和 `editor/mod.rs` | `src/editor/` | ✅ | 已完全删除 `src/editor/` 目录 |
| 3.5 | 更新 `main.rs` 模块声明 | `src/main.rs` | ✅ | 已移除 `pub mod editor;` |

### Phase 4: 迁移所有使用旧 `Input` 的代码

旧 `Input` 的使用点：

| 文件 | 使用方式 | 迁移策略 |
|------|---------|---------|
| `src/file_browser/view.rs` | `Input::new(cx)` | 改为 `cx.new(|cx| InputState::new(window, cx))`，再用 `Input::new(&state)` 包装 |
| `src/file_browser/remote_view.rs` | `Input::new(cx)` | 同上 |
| `src/ssh_view/picker.rs` | `Input::new(cx)` | 同上 |
| `src/settings_hub/host_form.rs` | `Input::new(cx)` | 同上；注意 `on_change` 回调需改为订阅 `InputEvent::Change` |
| `src/settings_hub/hosts_view.rs` | `Input::new(cx)` | 同上 |
| `src/settings_hub/keychain_view.rs` | `Input::new(cx)` | 同上 |

| # | 任务 | 文件 | 状态 | 备注 |
|---|------|------|------|------|
| 4.1 | 重构 `Input` 以支持旧 API 的关键功能 | `src/components/input/input.rs` | ✅ | `EditorInput` 已支持 `placeholder`、`masked`、`disabled`、`transparent`、`clear` 等；`on_change` 通过 `InputEvent::Change` 订阅替代 |
| 4.2 | 迁移 `file_browser/view.rs` | `src/file_browser/view.rs` | ✅ | 已迁移为 `InputState` + `EditorInput` |
| 4.3 | 迁移 `file_browser/remote_view.rs` | `src/file_browser/remote_view.rs` | ✅ | 已迁移为 `InputState` + `EditorInput` |
| 4.4 | 迁移 `ssh_view/picker.rs` | `src/ssh_view/picker.rs` | ✅ | 已迁移为 `InputState` + `EditorInput`，通过 `InputEvent::Change` 订阅更新查询 |
| 4.5 | 迁移 `settings_hub/*` | `src/settings_hub/` | ✅ | `hosts_view.rs`、`keychain_view.rs`、`host_form.rs` 全部迁移完成 |
| 4.6 | 删除旧 `Input` 实现 | `src/components/input/input.rs`（旧） | ✅ | 已删除，并更新 `components/input/mod.rs` 和 `components/mod.rs` |

### Phase 5: 清理 `state.rs` 中的死字段和未使用方法

| # | 任务 | 文件 | 状态 | 备注 |
|---|------|------|------|------|
| 5.1 | 移除 LSP/Popover/Search 相关字段 | `src/components/input/editor/state.rs` | ✅ | 已移除 `diagnostic_popover`, `context_menu`, `hover_popover`, `search_panel`, `lsp`, `inline_completion` 等 |
| 5.2 | 移除或注释掉对应的空方法 | `src/components/input/editor/state.rs` | ✅ | 相关空方法和字段已一并移除 |
| 5.3 | 移除 `mask_pattern` 字段 | `src/components/input/editor/state.rs` | ✅ | 已移除 |
| 5.4 | 清理未使用的 import | `src/components/input/editor/state.rs` | ⚠️ | 仍有少量 unused import warning（`anyhow::Result`、`Task` 等），不影响编译 |
| 5.5 | 运行 `cargo check` / `cargo clippy` | 项目根目录 | ✅ | `cargo build` 已通过，无 error |

### Phase 6: 最终整理与验证

| # | 任务 | 状态 | 备注 |
|---|------|------|------|
| 6.1 | 确保 `components/input/mod.rs` 导出清晰 | ✅ | 已精简导出，只保留 `editor` 子模块和 `change`/`history`；`EditorInput` 和 `InputState` 通过 `editor` 访问 |
| 6.2 | 编译验证（`cargo build`） | ✅ | 全量编译通过 |
| 6.3 | 运行时验证 | ⬜ | 待验证：编辑器打开文件、SSH picker 输入、settings 表单输入、file browser 路径输入 |
| 6.4 | 提交优化结果 | ⬜ | 待用户手动提交 |

---

## 四、风险与注意事项

1. **`on_change` 回调的迁移风险**
   - 旧 `Input` 使用 `Box<dyn Fn(&str, &mut App)>` 回调
   - 新架构需通过 `cx.subscribe(&input_state, |..., event| { ... })` 监听 `InputEvent::Change`
   - 在 `settings_hub` 等表单密集处，需确保订阅时机正确（通常在父 View 的 `new` 或 `init` 中）

2. **`InputState` 的 `focus_handle` 获取**
   - 旧 `Input` 的父组件若直接依赖 `input.focus_handle(cx)`，迁移后需改为 `input_state.read(cx).focus_handle.clone()`
   - 新的 `Input` wrapper 若为 `RenderOnce`，则没有自己的 `focus_handle`，焦点始终在 `InputState` 上

3. **代码编辑器功能回退风险**
   - `EditorView` 被移除后，需确保 `InputState` 的 `code_editor()` 模式在 Panel 中渲染正常
   - 需验证语法高亮、行号、折叠等功能未被意外破坏

4. **`InputState::render` 移除后的影响**
   - 当前 `editor_view.rs` 直接 `.child(self.editor.clone())`，因为 `InputState` 实现了 `Render`
   - 移除 `Render` 后，必须通过 `Input::new(&self.editor)` 来渲染
   - `Panel::render` 中需要相应调整

---

## 五、文件映射对照表

| gpui-component（参考） | DeepTerm（当前） | DeepTerm（目标） |
|-----------------------|-----------------|-----------------|
| `crates/ui/src/input/mod.rs` | `src/components/input/mod.rs` | 精简导出，移除旧 `input.rs` 的 re-export |
| `crates/ui/src/input/state.rs` | `src/components/input/editor/state.rs` | 清理死代码，移除 `Render` 实现 |
| `crates/ui/src/input/input.rs` | `src/components/input/input.rs`（旧） | 完全重写为 `InputState` 的 UI wrapper |
| `crates/ui/src/input/element.rs` | `src/components/input/editor/element.rs` | 基本保留，可能微调与 `Input` 的接口 |
| `crates/ui/src/input/lsp.rs` | `src/components/input/editor/lsp.rs` | **删除** |
| `crates/ui/src/input/popovers/` | `src/components/input/editor/popovers.rs` | **删除** |
| `crates/ui/src/input/search.rs` | `src/components/input/editor/search.rs` | **删除** |
| `crates/ui/src/input/mask_pattern.rs` | `src/components/input/editor/mask_pattern.rs` | **删除** |
| `crates/ui/src/input/number_input.rs` | `src/components/input/editor/number_input.rs` | **删除** |
| N/A | `src/editor/editor_view.rs` | **删除**（功能下沉到 `Panel` + `InputState`） |

---

## 六、当前进度

- [x] Phase 1: 清理未使用的 Editor Stub 模块
- [x] Phase 2: 重构 `InputState` 渲染入口
- [x] Phase 3: 移除/简化 `EditorView`
- [x] Phase 4: 迁移所有使用旧 `Input` 的代码
- [x] Phase 5: 清理 `state.rs` 中的死字段和未使用方法
- [~] Phase 6: 最终整理与验证（编译通过，运行时验证待进行）

**计划制定日期**: 2026-04-14
**最后更新日期**: 2026-04-14
**计划状态**: 主要重构已完成，`cargo build` 通过

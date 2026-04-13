# Editor Migration Plan

> 将 gpui-component 的 code editor 核心实现裁剪迁移到 DeepTerm，支持基本文件编辑和语法高亮，并通过 `/open-file` 内建命令打开文件。

## 1. 分析结论

### 1.1 gpui-component 架构

- **`input/state.rs`** (2678 行): 核心状态机 `InputState`，基于 `ropey::Rope` 管理文本。
- **`input/element.rs`** (2105 行): 自定义 GPUI `TextElement`，负责文本排版、选择高亮、光标绘制、语法高亮 `TextRun` 应用。
- **`input/display_map/`** (~1934 行): 坐标映射系统（自动换行、代码折叠）。
- **`highlighter/`** (~2038 行): 基于 `tree-sitter` 的增量语法解析和样式生成。

### 1.2 DeepTerm 现状

- 已有单行 `Input` 组件 (`src/components/input/input.rs`)，使用 `SharedString` 作为缓冲，具备自定义 Element 渲染能力。
- 工作区通过 `Panel` / `PanelContent` 管理内容，`Workspace` 管理 Tab 和分屏。
- `/open-file` 命令已注册但此前映射为 `LogOnly`（未实现）。

## 2. 实际实施策略（与计划有所调整）

由于对 gpui-component 完整 `state.rs` + `element.rs` (~4700 行) 的直接迁移会产生大量依赖和编译问题，实际采用**更轻量的自定义实现**：

- **未直接迁移**：`display_map/`、`state.rs`、`element.rs`（过于复杂且与 gpui-component UI kit 强耦合）
- **实际实现**：基于 DeepTerm 现有 `Input` 组件的 `Element` 渲染模式，全新编写了多行 `CodeEditor`（`src/components/input/code_editor.rs`），使用 `Rope` 作为缓冲。
- **语法高亮**：未直接移植 gpui-component 复杂的 `LanguageRegistry` + `registry.rs` 体系，而是编写了轻量的 `SimpleHighlighter`（`src/highlighter/simple.rs`），内置常见语言的简化 tree-sitter query，直接映射为 `HighlightStyle`。

## 3. 已完成工作

### Phase 0: 基础设施与依赖 ✅
- [x] `Cargo.toml` 新增依赖：`ropey`、`tree-sitter`、`tree-sitter-rust/js/ts/py/json/go/html/css/c/cpp/bash/md`、`streaming-iterator`、`sum-tree`、`smallvec`、`lsp-types`、`serde_repr`、`schemars`、`tracing`、`enum-iterator`
- [x] 注册 `tree-sitter-languages` feature（用于兼容 gpui-component 条件编译）

### Phase 1: 多行编辑器组件 ✅
- [x] 新建 `src/components/input/code_editor.rs`
  - `Rope` 缓冲管理
  - 多行光标移动（上下左右、Home/End、选区扩展）
  - Enter 插入换行、Backspace/Delete
  - Copy/Cut/Paste
  - Undo/Redo（复用 `History<Change>`）
  - 垂直滚动 + 鼠标选取
  - IME 支持（`EntityInputHandler`）
  - 自定义 `Element` 渲染，逐行 `ShapedLine` 排版
- [x] 绑定 `Editor` context 快捷键（`cmd-s` Save）
- [x] 在 `components::init()` 中注册 `code_editor::init(cx)`

### Phase 2: 轻量语法高亮 ✅
- [x] 新建 `src/highlighter/simple.rs`
  - 内置 Rust / JavaScript / TypeScript / Python / JSON / Go / HTML / CSS / C / C++ / Bash / Markdown 的简化 tree-sitter query
  - `styles(&mut self, text, cx)` 返回 `Vec<(Range<usize>, HighlightStyle)>`
  - 通过 `UiTheme` 和 dark/light 模式选择合适的高亮颜色
- [x] 简化 `src/highlighter/mod.rs`（stub `DiagnosticSet`、`LanguageRegistry` 等，避免编译 gpui-component 的复杂高亮模块）
- [x] 在 `code_editor.rs` 的 `prepaint` 中集成 `SimpleHighlighter`，将 `HighlightStyle` 映射为 `TextRun` 切片

### Phase 3: DeepTerm 集成 ✅
- [x] 创建 `src/editor/editor_view.rs`，包装 `Entity<Editor>`
- [x] `PanelContent` 新增 `Editor(Entity<EditorView>)` 分支
- [x] `Panel::render_content()` / `content_focus_handle()` 处理 `Editor`
- [x] `Panel::with_editor(...)` 构造函数
- [x] `builtin_commands/mod.rs`: `/open-file` 映射为 `BuiltinCommandHandler::OpenFile`
- [x] `terminal_view.rs`: `execute_builtin_command()` 解析文件路径并发出 `Event::OpenFile { path }`
- [x] `panel.rs`: 订阅 `TerminalViewEvent::OpenFile` 并转发为 `PanelEvent::OpenFile`
- [x] `workspace.rs`: `handle_panel_event()` 响应 `PanelEvent::OpenFile`，调用 `show_editor_panel()`
- [x] `show_editor_panel()`: 读取文件内容、猜测语言扩展名、创建 `EditorView` 和对应 Tab
- [x] `src/main.rs`: 注册 `pub mod editor;` 和 `pub mod highlighter;`

## 4. 编译状态

- `cargo check` **通过**，无新增 errors（仅有项目原有 warnings）。

## 5. 使用方式

在任意终端面板中输入：

```
/open-file <文件路径>
```

即可在当前 Workspace 中新建一个 Editor Tab，打开并编辑该文件，同时根据扩展名自动启用对应语言的语法高亮。

## 6. 进度记录

- **2025-04-13**: 完成架构分析，制定迁移计划，完成开发文档。
- **2025-04-13**: 完成所有实现：多行编辑器 `code_editor.rs`、轻量语法高亮 `SimpleHighlighter`、`/open-file` 工作区集成，项目编译通过。

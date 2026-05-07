# DeepTerm 内建命令架构优化计划

## 目标

参考 Warp 终端的 slash command 设计，重构 deepterm 的 `/open` 等内建命令系统，实现：
- **职责分离**：命令定义、状态管理、执行逻辑、视图渲染解耦
- **可测试性**：核心逻辑可独立单元测试
- **可扩展性**：新增命令无需修改 TerminalView
- **事件驱动**：通过 GPUI 事件系统传递状态变更

---

## 现状问题

1. **静态注册**：命令硬编码在 `BUILTIN_COMMAND_SPECS` 数组中
2. **视图耦合**：`BuiltinCommandManager` 直接内嵌在 `TerminalView` 中
3. **职责过重**：`TerminalView` 背负命令检测、建议计算、执行逻辑
4. **参数简陋**：仅支持字符串分割，无结构化参数校验
5. **无优先级链**：Enter 键处理分散，容易冲突

---

## 优化方案（分6个阶段）

### 阶段1：提取 SuggestionEngine（低风险，立即可见改善）

**目标**：将建议匹配算法从 `BuiltinCommandManager` 中独立出来。

**文件变更**：
- 新建 `src/builtin_commands/suggestion_engine.rs`
- 修改 `src/builtin_commands/mod.rs`（移除建议逻辑）

**设计要点**：
```rust
pub struct SuggestionEngine {
    registry: &'static CommandRegistry,
}

impl SuggestionEngine {
    pub fn suggest(&self, filter: &str, cx: &App) -> Vec<ScoredCommand>;
    fn score_match(&self, command: &BuiltinCommand, filter: &str) -> i32;
}
```

**验收标准**：
- [ ] `BuiltinCommandManager::refresh_suggestions()` 内部逻辑迁移到 `SuggestionEngine`
- [ ] 建议排序算法保持现有行为（精确匹配 > 前缀匹配 > 子串匹配 > 描述匹配）
- [ ] 不影响现有 UI 渲染

---

### 阶段2：引入 BuiltinCommandModel（GPUI Model）

**目标**：创建独立的 GPUI Model 管理命令输入状态机。

**文件变更**：
- 新建 `src/builtin_commands/command_model.rs`
- 修改 `src/terminal_view/terminal_view.rs`（集成 Model）

**设计要点**：
```rust
pub struct BuiltinCommandModel {
    state: CommandEntryState,
    registry: &'static CommandRegistry,
}

pub enum CommandEntryState {
    None,
    Composing { filter: String },
    Detected(DetectedCommand),
    DisabledUntilEmpty,
}

pub struct DetectedCommand {
    pub command: BuiltinCommand,
    pub argument: Option<String>,
}

pub struct UpdatedCommandModel {
    pub old_state: CommandEntryState,
}
```

**验收标准**：
- [ ] `BuiltinCommandModel` 可被独立实例化（不依赖 TerminalView）
- [ ] 状态变更通过 `cx.emit(UpdatedCommandModel)` 传播
- [ ] TerminalView 订阅 Model 事件并刷新 UI
- [ ] `detect_command()` 为纯函数，可被外部调用

---

### 阶段3：创建 CommandRegistry（动态注册表）

**目标**：替换静态数组，支持命令动态注册和可用性控制。

**文件变更**：
- 新建 `src/builtin_commands/registry.rs`
- 修改 `src/builtin_commands/mod.rs`（导出 Registry）
- 修改 `src/main.rs`（初始化注册表）

**设计要点**：
```rust
pub struct CommandRegistry {
    commands: HashMap<&'static str, BuiltinCommand>,
}

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub icon: Icon,
    pub availability: Availability,
    pub argument: Option<ArgumentType>,
    pub handler_id: CommandHandlerId,
}

pub enum Availability {
    Always,
    LocalOnly,
    RemoteOnly,
}
```

**验收标准**：
- [ ] 现有所有命令从 `BUILTIN_COMMAND_SPECS` 迁移到 `CommandRegistry`
- [ ] 支持 `register()` 动态注册（为未来插件预留）
- [ ] `active_commands()` 根据环境（本地/远程）过滤可用命令
- [ ] 查找性能不低于现有 O(n) 扫描（使用 HashMap 达到 O(1)）

---

### 阶段4：实现 CommandHandler Trait

**目标**：将命令处理逻辑从 TerminalView 中抽离，每个命令独立实现 Handler。

**文件变更**：
- 新建 `src/builtin_commands/handlers/mod.rs`
- 新建 `src/builtin_commands/handlers/open_file_browser.rs`
- 新建 `src/builtin_commands/handlers/open_file.rs`
- 修改 `src/terminal_view/terminal_view.rs`（移除处理逻辑）

**设计要点**：
```rust
pub struct CommandContext<'a> {
    pub terminal_view: Entity<TerminalView>,
    pub working_directory: Option<PathBuf>,
    pub is_remote: bool,
    pub ssh_config: Option<SshConnectionConfig>,
    pub cx: &'a mut AppContext,
}

pub trait CommandHandler: Send + Sync {
    fn execute(&self, argument: Option<&str>, ctx: CommandContext<'_>);
}
```

**验收标准**：
- [ ] `/open` 逻辑迁移到 `OpenFileBrowserHandler`
- [ ] `/open-file` 逻辑迁移到 `OpenFileHandler`
- [ ] TerminalView 中无命令特定处理逻辑（只剩通用框架代码）
- [ ] Handler 可独立单元测试（使用 mock CommandContext）

---

### 阶段5：引入 CommandExecutor（执行引擎）

**目标**：统一命令分发执行，支持错误处理和异步操作。

**文件变更**：
- 新建 `src/builtin_commands/executor.rs`
- 修改 `src/terminal_view/terminal_view.rs`（调用 Executor）

**设计要点**：
```rust
pub struct CommandExecutor {
    handlers: HashMap<CommandHandlerId, Box<dyn CommandHandler>>,
}

impl CommandExecutor {
    pub fn register(&mut self, id: CommandHandlerId, handler: Box<dyn CommandHandler>);
    pub fn execute(&self, command: &BuiltinCommand, argument: Option<&str>, ctx: CommandContext<'_>);
}
```

**验收标准**：
- [ ] TerminalView 通过 `CommandExecutor` 执行命令
- [ ] 未知命令返回 `false`，回退到 shell 执行
- [ ] 命令执行错误通过 toast 或事件反馈给用户

---

### 阶段6：添加 Argument 结构化参数解析

**目标**：为命令提供参数校验、提示和结构化解析。

**文件变更**：
- 新建 `src/builtin_commands/argument.rs`
- 修改 `src/builtin_commands/registry.rs`（使用 ArgumentType）

**设计要点**：
```rust
pub enum ArgumentType {
    None,
    Optional { hint: Option<&'static str> },
    Required { hint: &'static str },
}

pub struct ParsedArgument {
    pub raw: String,
    pub tokens: Vec<String>,
}

impl BuiltinCommand {
    pub fn parse_argument(&self, input: &str) -> Result<ParsedArgument, ArgumentError>;
}
```

**验收标准**：
- [ ] `/open-file` 无参数时提示 "Please provide a file path"
- [ ] 参数提示在建议浮层中显示
- [ ] 支持带引号的路径（如 `"/path/with spaces"`）

---

## 文件重构地图

```
src/
├── builtin_commands/
│   ├── mod.rs                  # 公开接口聚合，保留向后兼容
│   ├── registry.rs             # CommandRegistry（阶段3）
│   ├── command_model.rs        # BuiltinCommandModel（阶段2）
│   ├── executor.rs             # CommandExecutor（阶段5）
│   ├── argument.rs             # 参数解析（阶段6）
│   ├── suggestion_engine.rs    # SuggestionEngine（阶段1）
│   ├── handlers/               # 命令处理器（阶段4）
│   │   ├── mod.rs
│   │   ├── open_file_browser.rs
│   │   ├── open_file.rs
│   │   └── ...
│   └── ui/                     # 建议浮层渲染（可选，后续阶段）
│       └── suggestion_overlay.rs
└── terminal_view/
    └── terminal_view.rs        # 移除命令逻辑，仅保留集成
```

---

## 迁移策略

**原则**：每个阶段独立可编译、可运行、可测试。

1. **阶段1-2**：先提取独立模块，TerminalView 内部调用方式不变（通过适配层）。
2. **阶段3**：创建 Registry，现有静态数组作为初始化数据源。
3. **阶段4-5**：逐个命令迁移 Handler，每迁移一个命令进行一次端到端测试。
4. **阶段6**：最后添加参数校验，不影响已有命令。

---

## 测试策略

- **单元测试**：`SuggestionEngine`、`BuiltinCommandModel`、`CommandExecutor` 均可独立测试
- **集成测试**：每个阶段完成后，手动验证 `/open`、`/open-file` 等命令在本地和远程终端中正常工作
- **回归测试**：确保未知 `/` 命令仍回退到 shell 执行

---

## 参考

- Warp: `warp/app/src/search/slash_command_menu/static_commands/commands.rs`
- Warp: `warp/app/src/terminal/input/slash_command_model.rs`
- Warp: `warp/app/src/terminal/input/slash_commands/mod.rs`

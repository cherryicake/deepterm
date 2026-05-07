# DeepTerm 内建命令架构优化 - 开发进度

## 最新状态：所有阶段已完成 ✅

**日期**：2025-05-07
**总测试数**：42 passed, 0 failed

---

## 完成记录

### 2025-05-07: 全部阶段完成

| 阶段 | 状态 | 测试数 | 关键产出 |
|------|------|--------|----------|
| 1. SuggestionEngine | ✅ | 5 | 独立建议匹配算法 |
| 2. BuiltinCommandModel | ✅ | 7 | 状态机驱动的命令检测 |
| 3. CommandRegistry | ✅ | 5 | 动态命令注册表 |
| 4-5. CommandHandler + Executor | ✅ | 5 | Trait-based 执行引擎 |
| 6. Argument 解析 | ✅ | 8 | 结构化参数解析 |
| **迁移到 TerminalView** | ✅ | 12 | 完整集成 |
| **总计** | **✅** | **42** | **全部通过** |

---

## 阶段1: 提取 SuggestionEngine ✅

**目标**：将建议匹配算法从 `BuiltinCommandManager` 中独立出来。

- **新建** `src/builtin_commands/suggestion_engine.rs`
- 无状态工具结构，保持原有评分算法
- 5个单元测试

## 阶段2: 引入 BuiltinCommandModel ✅

**目标**：创建独立的命令输入状态机。

- **新建** `src/builtin_commands/command_model.rs`
- 状态机：`None`, `Composing`, `Detected`, `DisabledUntilEmpty`
- 纯函数 `detect_command()`，不依赖 GPUI
- 7个单元测试

## 阶段3: 创建 CommandRegistry ✅

**目标**：替换静态数组，支持动态注册。

- **新建** `src/builtin_commands/registry.rs`
- `HashMap` 动态注册表，支持 `Availability` 环境过滤
- `from_specs()` 向后兼容迁移
- 全局单例 `global_registry()`
- 5个单元测试

## 阶段4-5: 实现 CommandHandler + CommandExecutor ✅

**目标**：将命令处理逻辑从 TerminalView 中抽离。

- **新建** `src/builtin_commands/executor.rs`
- `CommandResult` 返回模式（解决 `AppContext` dyn-incompatible 问题）
- `CommandHandler` trait（dyn-compatible）
- `CommandExecutor` 引擎 + 全局单例
- 实现 `OpenFileBrowserHandler`、`OpenFileHandler`、`LogOnlyHandler`
- 5个单元测试

## 阶段6: 添加 Argument 结构化参数解析 ✅

**目标**：为命令提供参数校验和结构化解析。

- **新建** `src/builtin_commands/argument.rs`
- 支持双引号路径、必填/可选参数校验
- 8个单元测试

## 迁移阶段：整合到 TerminalView ✅

**目标**：将新框架与 TerminalView 集成。

- **新建** `src/builtin_commands/path_resolver.rs`
- 提取本地/远程路径解析逻辑（纯函数）
- 进程工作目录查询（Linux/macOS）
- 5个单元测试

- **修改** `src/terminal_view/terminal_view.rs`
- 重写 `execute_builtin_command` 使用新框架
- 使用 `global_registry().find()` + `global_executor().execute()`
- 根据 `CommandResult` 发射事件
- 添加 `working_directory()` / `ssh_config()` 公共 getter

- **修改** `src/main.rs`
- 初始化 `init_global_executor()`

---

## 新增文件

```
src/builtin_commands/
├── argument.rs              # 参数解析
├── command_model.rs         # 命令状态机
├── executor.rs              # 执行引擎 + Handler
├── handlers/
│   └── mod.rs
├── path_resolver.rs         # 路径解析工具
├── registry.rs              # 动态注册表
└── suggestion_engine.rs     # 建议引擎
```

## 修改文件

```
src/builtin_commands/mod.rs         # 导出所有新模块
src/main.rs                         # 初始化全局 executor
src/terminal_view/terminal_view.rs  # 迁移 execute_builtin_command
```

---

## 关键设计决策

### 1. CommandResult 返回模式
由于 `AppContext` 不是 dyn-compatible，`CommandHandler` trait 不能直接接收 `cx`。采用"返回结果 + 调用方处理"模式：
- Handler 返回 `CommandResult`（纯数据）
- `TerminalView` 根据结果发射事件
- trait 保持 dyn-compatible，可使用 `Box<dyn CommandHandler>`

### 2. 全局单例
`CommandExecutor` 使用 `OnceLock<Mutex<CommandExecutor>>` 全局单例：
- TerminalView 不需要持有 executor 引用
- 应用启动时通过 `init_global_executor()` 显式初始化

### 3. 路径解析提取
将 TerminalView 中的路径解析逻辑提取到 `PathResolver`：
- 本地路径：处理 `~`、相对路径、绝对路径
- 远程路径：处理 `~/`、SFTP 路径拼接
- 进程工作目录：Linux/macOS 原生支持

---

## 架构对比

### 重构前
```
用户输入 "/open-file src/main.rs"
       ↓
[TerminalView] execute_builtin_command()
       ↓
手动 strip_prefix + trim
       ↓
直接 cx.emit(Event::OpenFile)
```

### 重构后
```
用户输入 "/open-file src/main.rs"
       ↓
[TerminalView] execute_builtin_command()
       ↓
[ArgumentParser] 提取参数 "src/main.rs"
       ↓
[CommandRegistry] find("/open-file") → BuiltinCommand
       ↓
[CommandExecutor] execute() → CommandResult::OpenFile { path }
       ↓
[TerminalView] cx.emit(Event::OpenFile { path })
       ↓
[Workspace] 打开编辑器
```

---

## 待办（可选后续优化）

- [ ] 将所有命令从 `BuiltinCommandSpec` 迁移到 `BuiltinCommand`
- [ ] `SuggestionEngine` 支持 `CommandRegistry` 作为数据源
- [ ] 实现更多命令处理器（OpenRepoHandler, OpenSkillHandler 等）
- [ ] 添加命令参数提示到建议浮层
- [ ] 实现 toast 错误提示
- [ ] 命令执行异步支持
- [ ] 插件化：支持外部 crate 注册自定义 Handler

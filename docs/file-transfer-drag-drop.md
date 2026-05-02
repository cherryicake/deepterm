# File Transfer Drag & Drop Design

本文档记录 DeepTerm 文件浏览器拖拽传输能力的设计、实现计划、开发进度与后续细节。

## 目标

在本地文件浏览器、远程文件浏览器，以及不同服务器的远程文件浏览器之间，通过拖拽文件或文件夹完成传输。

需要覆盖：

- 本地 → 本地
- 本地 → 远程
- 远程 → 本地
- 远程 A → 远程 A 同服务器
- 远程 A → 远程 B 不同服务器
- 文件、文件夹、多选条目的复制
- 传输进度、取消、失败提示、冲突处理

## 当前代码基础

相关现有模块：

- `src/file_browser/target.rs`：已有 `FileBrowserTarget`，区分本地和远程浏览器目标。
- `src/file_browser/view.rs`：本地文件浏览器，使用 `std::fs` 读取目录与元数据。
- `src/file_browser/remote_view.rs`：远程文件浏览器，基于 SFTP 加载目录、元数据和预览。
- `src/ssh/sftp_fs.rs`：远程 SFTP 文件系统能力，目前支持 `canonicalize`、`metadata`、`read_dir`、`read_text_preview`。
- `src/file_browser/mod.rs`：目前 `FileBrowserEvent` 只有 `Close`。
- `src/workspace/panel.rs`：负责按 `FileBrowserTarget` 创建本地或远程文件浏览器 panel/overlay。

重要现状判断：

- 本地和远程浏览器 UI 分开实现，但交互结构相似。
- 远程浏览器已经使用 `cx.background_executor()` 执行耗时 SFTP 操作，可作为传输任务异步执行的参考。
- 当前 SFTP session cache 使用 `Arc<Mutex<BlockingSftpSession>>`，适合浏览和预览，但不适合长时间文件传输，否则会阻塞目录刷新和预览。
- 当前还没有统一的文件端点、拖拽载荷、传输请求、传输队列与进度模型。

## 总体架构

拖拽传输不应直接写进 `FileBrowserView` 或 `RemoteFileBrowserView` 的内部逻辑中。推荐抽象为三层：

1. 文件端点层：统一描述本地路径与远程路径。
2. 拖拽载荷层：描述拖拽源文件/文件夹列表。
3. 传输调度层：根据源和目标选择最高效、安全的传输策略。

### FileEndpoint

用于统一表示本地或远程文件系统位置。

```rust
enum FileEndpoint {
    Local {
        path: PathBuf,
    },
    Remote {
        ssh_config: SshConnectionConfig,
        path: String,
    },
}
```

### FileDragPayload

拖拽开始后，UI 层携带 `FileDragPayload`，而不是只传路径字符串。

```rust
struct FileDragPayload {
    sources: Vec<FileEndpoint>,
    operation_hint: TransferOperation,
}

enum TransferOperation {
    Copy,
    Move,
}
```

内部文件浏览器拖拽默认使用 `Move`，对齐 macOS Finder 的文件浏览器内拖拽语义。`Copy` 仍保留为底层操作类型，后续可用于 Option/修饰键语义。

### TransferRequest

drop 到目标目录后，由文件浏览器生成传输请求并向上层 emit。

```rust
struct TransferRequest {
    sources: Vec<FileEndpoint>,
    destination: FileEndpoint,
    operation: TransferOperation,
    conflict_policy: ConflictPolicy,
}

enum ConflictPolicy {
    AutoRename,
    Overwrite,
    Skip,
    Ask,
}
```

第一版建议默认使用 `AutoRename`，避免危险覆盖。

### TransferManager

`TransferManager` 负责：

- 接收 `TransferRequest`
- 创建传输任务
- 扫描目录并构建 manifest
- 根据源/目标组合选择传输策略
- 管理队列、并发、进度、取消、错误
- 将任务状态暴露给 Workspace 或传输面板

传输状态应放在 Workspace 级别，而不是某个文件浏览器内部。这样文件浏览器关闭后，后台传输仍可继续。

## 建议模块结构

新增模块：

```text
src/file_transfer/
├── mod.rs
├── endpoint.rs       # FileEndpoint, EndpointId
├── request.rs        # TransferRequest, TransferOperation, ConflictPolicy
├── manager.rs        # TransferManager, queue, progress, cancellation
├── local.rs          # local filesystem transfer helpers
├── sftp.rs           # SFTP transfer helpers / transfer sessions
└── planner.rs        # transfer strategy selection
```

第一版不建议过度抽象 stream trait。更直接、易维护的做法是在 `planner` 中按组合分发：

```rust
match (&source, &destination) {
    (FileEndpoint::Local { .. }, FileEndpoint::Local { .. }) => copy_local_to_local(...),
    (FileEndpoint::Local { .. }, FileEndpoint::Remote { .. }) => upload_local_to_remote(...),
    (FileEndpoint::Remote { .. }, FileEndpoint::Local { .. }) => download_remote_to_local(...),
    (FileEndpoint::Remote { .. }, FileEndpoint::Remote { .. }) => copy_remote_to_remote(...),
}
```

## 传输策略矩阵

### Local → Local

- 同文件系统 move 可使用 `rename`。
- 跨设备 move fallback 到 copy + delete。
- copy 使用本地递归复制。
- 第一版仅实现 copy。

### Local → Remote

- 本地流式读。
- SFTP 流式写。
- 文件夹先扫描本地目录，再在远程创建目录，最后并发上传文件。

### Remote → Local

- SFTP 流式读。
- 本地流式写。
- 文件夹先远程递归 `read_dir`，再本地创建目录，最后并发下载文件。

### Remote A → Remote A 同服务器

- move 优先使用远程 `rename`。
- copy 第一版可走同一服务器 SFTP read/write 流式复制。
- 后续可考虑远程 shell `cp -a` 优化，但不作为第一版依赖。

### Remote A → Remote B 不同服务器

第一版使用本机中转流式传输：

```text
Remote A read stream
        ↓
bounded buffer / memory channel
        ↓
Remote B write stream
```

优点：

- 不要求源服务器访问目标服务器。
- 不需要远程安装 `rsync` 或 `scp`。
- 不把目标服务器凭据暴露给源服务器。
- 行为与上传/下载统一。

缺点：

- 数据经过本机。
- 速度受本机网络和两端服务器链路影响。

后续高级优化可增加用户确认的 server-to-server 模式，例如 `rsync`，但需要额外处理认证、安全与 shell quoting 风险。

## 性能设计

### 流式传输

- 不要把整个文件读入内存。
- 使用固定 buffer，例如 `256 KiB` 或 `1 MiB`。
- 每完成一个 chunk 后更新传输进度。
- 目标文件先写入临时文件，再 rename 到最终路径。

临时文件命名建议：

```text
.<filename>.deepterm-part
```

### 并发限制

建议默认值：

- 单个远程 host：2 个并发传输。
- 本地目录扫描：可高于远程传输并发。
- 跨远程传输：同时受源 host 和目标 host 限制。

原因：

- SFTP 并发过高容易导致服务器限流或断开。
- 当前浏览用 SFTP session 由 `Mutex` 串行保护，传输应使用独立 session pool，避免阻塞 UI 浏览。

### 进度模型

```rust
struct TransferProgress {
    total_bytes: Option<u64>,
    transferred_bytes: u64,
    current_item: String,
    files_done: usize,
    files_total: Option<usize>,
}
```

目录传输开始时可能还不知道总大小，应先显示 `scanning...`，扫描过程中逐步更新总文件数和总字节数。

## SFTP 层需要补齐的能力

现有 `src/ssh/sftp_fs.rs` 需要新增面向传输的 API：

- `create_dir`
- `create_dir_all`
- `exists`
- `open_read` 或 chunked read helper
- `open_write` 或 chunked write helper
- `remove_file`
- `remove_dir`
- `rename`
- 可选：`set_metadata`，用于保留 mtime/权限
- 可选：`lstat`，用于更好地区分 symlink

注意：浏览/预览 session 和传输 session 应分离。

推荐：

- `with_cached_session` 继续服务目录浏览、元数据和预览。
- 新增 `SftpTransferPool` 或专用传输连接管理，用于长时间传输。

## 拖拽交互设计

文件浏览器 UI 层职责：

- 识别拖拽源文件/文件夹。
- 构建 `FileDragPayload`。
- 显示拖拽 ghost、文件数量和源类型。
- 标记 drop target。
- drop 后构建 `TransferRequest` 并 emit 给上层。

文件浏览器 UI 层不负责：

- 实际读写文件。
- 传输并发调度。
- 冲突策略执行。
- 长任务状态管理。

建议扩展事件：

```rust
pub enum FileBrowserEvent {
    Close,
    TransferRequested(TransferRequest),
}
```

## 安全与边界条件

### 路径处理

- 本地路径使用 `PathBuf::join`。
- 远程路径使用 `sftp_fs::join_remote_path`。
- drop 到文件时，目标应转换为该文件的 parent 目录。
- drop 到目录时，目标为目录本身。

### 符号链接

第一版建议默认不跟随 symlink，避免循环和意外复制大量数据。

后续可增加设置：

- 复制链接本身
- 跟随链接复制目标
- 遇到链接时询问

### 权限与元数据

第一版目标：

- 保留文件内容。
- 尽量保留 mtime。
- 权限保留失败不应导致整个传输失败。
- owner/group 不作为第一版目标。

### 大目录

- 目录扫描必须可取消。
- 不把完整 manifest 放进 UI state。
- UI 只订阅任务进度摘要。

## 第一版 MVP 范围

第一版建议实现：

- 本地 ↔ 远程单文件拖拽复制。
- 本地 ↔ 远程文件夹递归复制。
- 远程 ↔ 远程本机中转复制。
- 默认操作为 copy。
- 默认冲突策略为 `AutoRename`。
- 写入 `.deepterm-part` 临时文件，完成后 rename。
- 传输队列显示当前任务、进度、错误和取消按钮。
- 单个远程 host 默认最多 2 个并发传输。

暂不实现：
- server-to-server `rsync` 加速。
- 完整权限/owner/group 保留。
- 断点续传。
- checksum 强校验。

## 分阶段计划

### Phase 1: 类型和事件

- [x] 新增 `src/file_transfer/` 模块。
- [x] 定义 `FileEndpoint`。
- [x] 定义 `TransferRequest`。
- [x] 定义 `TransferOperation`。
- [x] 定义 `ConflictPolicy`。
- [x] 扩展 `FileBrowserEvent::TransferRequested`。
- [x] 本地/远程浏览器可以生成拖拽源端点和目标端点。

### Phase 2: 单文件上传/下载

- [x] 为 SFTP 层增加 chunked read/write 能力。
- [x] 实现 Local → Remote 单文件上传。
- [x] 实现 Remote → Local 单文件下载。
- [x] 增加最小传输任务执行入口。
- [ ] 在 UI 显示单任务进度。
- [ ] 支持取消任务。

### Phase 3: 文件夹递归

- [x] 实现本地目录扫描 manifest。
- [x] 实现远程目录扫描 manifest。
- [x] 支持目标目录递归创建。
- [x] 支持文件夹上传。
- [x] 支持文件夹下载。
- [x] 支持冲突自动重命名。

### Phase 4: 远程到远程

- [ ] 同服务器 move 使用 `rename`。
- [x] 同服务器 copy 使用 SFTP read/write。
- [x] 不同服务器 copy 使用本机中转传输。
- [ ] 增加 host 级并发限制。

### Phase 5: 体验完善

- [ ] 多选拖拽。
- [ ] drop target 高亮。
- [x] 传输队列面板。
- [ ] 传输完成通知。
- [ ] 失败重试。
- [ ] 可配置冲突策略。
- [ ] 可配置 symlink 策略。

## 开发进度记录

### 2026-05-02

- 完成需求分析。
- 确认现有文件浏览器与 SFTP 模块边界。
- 确定新增 `file_transfer` 模块的总体方向。
- 初始设计以 copy 为主；后续根据 Finder 语义调整为内部文件浏览器拖拽默认 move。
- 确定远程 A → 远程 B 第一版采用本机中转流式传输。
- 创建本文档作为后续开发进度和实现细节记录入口。
- 完成 Phase 1 类型和事件骨架：新增 `FileEndpoint`、`FileDragPayload`、`TransferRequest`、`TransferOperation`、`ConflictPolicy`。
- 扩展 `FileBrowserEvent::TransferRequested`，并通过 `PanelEvent::TransferRequested` 转发到 Workspace。
- 为本地和远程文件浏览器条目接入 GPUI drag/drop，drop 后可生成传输请求。
- 暂时在 Workspace 记录传输请求日志，实际 `TransferManager` 和文件读写将在 Phase 2 开始实现。
- 已运行 `cargo check`，编译通过，仅存在项目既有 warning。
- 完成 Phase 2 的最小可执行传输链路：新增 `TransferManager::execute_blocking`，Workspace 收到传输请求后会在后台任务中执行。
- 为 `BlockingSftpSession` 补充 `exists`、`create_dir_all`、`rename`、`remove_file`、`upload_file_from_local`、`download_file_to_local`。
- 支持单文件 Local → Local、Local → Remote、Remote → Local、同服务器 Remote → Remote copy。
- 写入目标时使用 `.deepterm-part` 临时文件，完成后 rename 到最终路径。
- 默认冲突策略 `AutoRename` 已在执行器中落地；`Ask` 暂未实现。
- 当前仍只支持单文件传输，文件夹递归、多文件进度 UI、取消能力留到后续阶段。
- 已再次运行 `cargo fmt` 和 `cargo check`，编译通过，仅存在项目既有 warning。
- 完成 Phase 3 递归目录传输：本地目录复制、本地目录上传、远程目录下载均已接入 `TransferManager`。
- 同服务器 Remote → Remote copy 已支持文件和文件夹；当前实现逐文件通过本地临时文件中转，后续可优化为远程 shell/server-side copy。
- 增加防护：本地目录和同服务器远程目录不能复制到自身内部，避免递归无限复制。
- 递归传输会跳过非普通文件/目录条目；symlink 策略仍待后续明确。
- 目录传输仍复用当前 cached SFTP session，会在传输期间占用该远程会话；专用传输连接池仍留到后续优化。
- 已运行 `cargo fmt` 和 `cargo check`，编译通过，仅存在项目既有 warning。
- 完成不同服务器 Remote → Remote 文件和文件夹传输：当前以本地临时文件作为中转，逐文件下载后上传。
- 为传输任务加入 Workspace 级状态列表，支持 Running、Completed、Failed、Cancelled。
- Workspace 右下角新增轻量 Transfers 浮层，显示最近 5 个任务状态和 Cancel 按钮。
- 取消能力已接入 `AtomicBool`，递归目录传输会在文件/目录边界检查取消；单个大文件传输会在当前文件完成后响应取消。
- `with_cached_session` 支持 `FnMut`，以便跨服务器传输时同时操作源和目标 SFTP 会话。
- 已运行 `cargo fmt` 和 `cargo check`，编译通过，仅存在项目既有 warning。
- 优化取消响应：本地 copy、SFTP upload/download、同服务器远程 copy、跨服务器远程 copy 均改为 chunk 级检查取消标志。
- 优化失败清理：本地 `.deepterm-part`、远程 `.deepterm-part`、跨服务器中转临时文件会在失败或取消后尽量清理。
- 优化传输浮层完成文案：完成状态使用 `format_size` 展示可读文件大小。
- 已运行 `cargo fmt` 和 `cargo check`，编译通过，仅存在项目既有 warning。
- 修复独立文件浏览器 panel 之间拖拽无效：之前只订阅了 overlay 文件浏览器事件，独立 `PanelContent::FileBrowser` 未转发 `TransferRequested` 到 Workspace。
- 为本地/远程文件浏览器内容空白区域增加 drop target，拖到目标浏览器空白区域会传输到当前 root 目录。
- 传输完成后刷新所有本地/远程文件浏览器内容，避免文件已复制但 UI 缓存未更新。
- 修复拖拽语义：内部文件浏览器拖拽现在默认是 `Move`，对齐 macOS Finder。
- 本地 → 本地 move 优先使用 `rename`；失败时 fallback 为 copy 后删除源。
- 同服务器远程 → 远程 move 优先使用 SFTP `rename`；失败时 fallback 为 copy 后递归删除源。
- 跨端 move 在 copy 成功后删除源：Local → Remote 删除本地源，Remote → Local / Remote → Remote 删除远程源。
- `Copy` 仍保留在底层类型中，后续可用于 Option/修饰键语义。

## 待决策问题

- 传输队列 UI 放在底部状态栏、独立 panel，还是文件浏览器内浮层？
- 是否需要支持拖拽到系统 Finder 或从 Finder 拖入远程浏览器？
- 默认冲突策略是否使用 `AutoRename`，还是首次冲突时弹窗询问？
- symlink 第一版是跳过、复制链接本身，还是跟随链接？
- 传输任务是否需要在应用重启后恢复？

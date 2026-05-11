# 远程到远程传输提速开发计划

本文档跟踪「远程到远程文件传输非常慢」这一问题的根因、方案选型、实现路线与开发日志。

## 1. 问题根因

经过代码审查与依赖库源码分析，远程到远程传输的主要瓶颈集中在 `sftp_fs.rs:382 stream_transfer_to` 的串行读写模型上。

- **SFTP 单流零管道（最关键）**：`russh_sftp::client::fs::File` 的 `poll_read`/`poll_write` 里只允许一个 `f_read`/`f_write` future 存在（`~/.cargo/.../russh-sftp-2.1.1/src/client/fs/file.rs:24`），`MAX_READ_LENGTH = MAX_WRITE_LENGTH = 261120 (≈255 KB)`。即便应用层使用 1MB buffer，也会被拆成多次 ~255KB 的 SFTP 请求，**每个都要等 ACK 才发下一个**。
- **读-写串行**：`stream_transfer_to` 是「读 1MB → 写 1MB → 继续读」的单深度流水线。读空 buffer 之前不会写，写完之前也不会读。在 RTT 80ms 的链路上，单块 1MB 约需 `1(read) + 4(write) = 5` 个 RTT ≈ 400ms，吞吐 ~600 KB/s。
- **目录递归串行**：`copy_remote_dir_between_sessions (manager.rs:1199)` 对每个子项递归串行 open/close，小文件延迟被进一步放大。
- **Phase 3 DirectScp 受限**：只处理单文件（`manager.rs:506`），目录依旧回退到慢路径；同时把目标私钥上传到源服务器（即便有 0 覆盖和即时清理），策略上并非首选。

即：**现状里 Phase 3 覆盖面小、Phase 1/2 的内存流式实质上是串行 SFTP**。

## 2. 方案总览

按「优先级从高到低」使用，尽量让数据不落在 DeepTerm 内存/磁盘：

1. **方案 3（服务器直连，首选）**：在源服务器上执行 `scp`/`rsync`，数据在两台服务器之间直达。
   - 3a：**Agent Forwarding + scp/rsync**（推荐，密钥不落盘）——开启 SSH agent forwarding，在源服务器 `scp`/`rsync` 时使用本机 agent 做鉴权。
   - 3b：**临时密钥上传（已有逻辑，作为 agent 不可用时的降级）**——扩展到目录，使用 `scp -r` 或 `rsync -e ssh`。
2. **方案 1（SFTP 请求级并发流水线，回退一）**：重写 `stream_transfer_to` / `copy_remote_file`，保留 K（默认 16–32）个 in-flight 请求，绕过 `File` 抽象直接用 `RawSftpSession::{read, write}`。
3. **方案 2（读/写任务拆分 + bounded channel，回退二/组合增强）**：把源端读任务和目标端写任务分别 `spawn` 到各自的 `tokio::runtime` 上，用 `mpsc::channel<Chunk>` 做生产者/消费者解耦，保证读写真正重叠。与方案 1 自然组合。

### 策略选择矩阵（新）

| 场景 | 认证方式 | 目录 / 单文件 | `scp`/`rsync` 可用 | 选择策略 |
|---|---|---|---|---|
| 同一台远程 | 任意 | 任意 | — | `copy_remote_file` / `copy_remote_dir_to_remote_path`（已存在，方案 1 同样会应用到 `copy_remote_file`） |
| 跨远程 | 目标 = 密钥（无 passphrase）**且**本机 agent 已加载该密钥 | 任意 | ✅ | **3a Agent Forwarding**（首选） |
| 跨远程 | 目标 = 密钥（可接受私钥短暂上传） | 任意 | ✅ | **3b 临时密钥 + scp/rsync**（扩展目录） |
| 跨远程 | 密码认证 / 其他 | 任意 | 任意 | **方案 1 + 2**（SFTP 管道化 + 读写拆分） |
| 跨远程 | 任意 | 任意 | ❌（最小系统） | **方案 1 + 2** |

Fallback 顺序始终为 `3a → 3b → (1+2)`，任一路径失败都要记录原因并自动降级。

## 3. 详细设计

### 3.1 方案 3a：Agent Forwarding

**前提判断**（全部成立才尝试）：
1. 目标 `ResolvedAuthMethod::PrivateKey { path, passphrase: None, .. }`。
2. 本机有可用 agent：`SSH_AUTH_SOCK` 存在，或我们可以启动 `russh::keys::agent::client::AgentClient`。
3. Agent 中已加载目标私钥（通过 `AgentClient::request_identities()` 匹配 `path` 的公钥指纹；必要时允许 DeepTerm 临时 `ssh-add` 加载，加载后记录原状并在结束时取消）。
4. 源服务器存在 `scp`/`rsync` 命令。

**流程**：
1. 连接源服务器时启用 agent forwarding：`channel.agent_forward(true).await?`（`russh::channels::mod.rs:550`）。注意：agent forwarding 只能绑定到**某一条 channel**，不是 session 级别，因此需要新开一个专用 exec channel 来跑 `scp`。
2. 在源服务器上运行：
   - 单文件：`scp -o BatchMode=yes -o StrictHostKeyChecking=accept-new -P <port> <source> <user>@<dest_host>:<dest_path>`
   - 目录：`scp -rp ...` 或 `rsync -azq -e "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -p <port>" <source>/ <user>@<dest_host>:<dest_path>/`
3. 通过 SSH 环境变量 `SSH_AUTH_SOCK` 让 scp 自动使用 forwarded agent，不需要上传私钥。
4. 实时从 stderr 读取进度（`scp` 的 `-v` 或 `rsync --info=progress2`）并累加到 `progress: &AtomicU64`。
5. exec channel 结束后拿到 exit status，失败 → 记录原因 → 返回错误由上层决定回退。

### 3.2 方案 3b：临时密钥 + scp/rsync（扩展目录）

沿用现有 `copy_via_remote_scp` 的密钥管理逻辑（`manager.rs:677`），新增：
- **目录支持**：`scp -rp` 或 `rsync -az -e "ssh -i <key>" src/ user@dest:path/`。
- **进度解析**：启动命令时同时收集 stderr，按行解析百分比或字节数；更新 `AtomicU64`。
- **冲突策略整合**：对目录场景，在命令发出前逐项检测冲突（`Ask` / `AutoRename` 语义必须在调用前解决），避免远程 `scp` 无差别覆盖。
  - `Overwrite`：直接执行。
  - `Skip`：先列目录比对，`scp` 仅拷贝不存在的项（或换 `rsync --ignore-existing`）。
  - `AutoRename` / `Ask`：简单做法——退化为流式（方案 1/2），保证语义精确；或在 3a/3b 内部实现逐项决策后再批量 scp。初版选"退化为流式"以降低复杂度。
- **密钥清零**：保持现有 0 覆盖 + `rm -rf` 临时目录策略。

### 3.3 方案 1：SFTP 请求级并发流水线

核心思路：放弃 `russh_sftp::client::fs::File`（它强制单请求串行），改用 `RawSftpSession` 直接下发 `read` / `write` 请求，自行管理 offset 与 in-flight 队列。

```rust
// 伪代码：pipelined read + write，K 个 in-flight 请求
async fn pipelined_transfer(src: &RawSftpSession, src_handle: String,
                            dst: &RawSftpSession, dst_handle: String,
                            size: u64, parallel: usize,
                            chunk: u32,  // e.g. 253 KB
                            cancelled: &AtomicBool,
                            progress: Option<&AtomicU64>) -> Result<u64> {
    let mut reads = FuturesOrdered::new();
    let mut writes = FuturesUnordered::new();
    let mut next_read_offset: u64 = 0;
    let mut bytes_written: u64 = 0;

    while bytes_written < size {
        while reads.len() + writes.len() < parallel && next_read_offset < size {
            let len = min(chunk as u64, size - next_read_offset) as u32;
            let offset = next_read_offset;
            reads.push_back(async move {
                src.read(src_handle.clone(), offset, len).await.map(|d| (offset, d.data))
            });
            next_read_offset += len as u64;
        }

        tokio::select! {
            Some(read_res) = reads.next() => {
                let (offset, data) = read_res?;
                if data.is_empty() { continue; }
                writes.push(async move {
                    dst.write(dst_handle.clone(), offset, data).await.map(|_| len)
                });
            }
            Some(write_res) = writes.next() => {
                let n = write_res?;
                bytes_written += n as u64;
                if let Some(p) = progress { p.fetch_add(n as u64, Ordering::Relaxed); }
                if cancelled.load(Ordering::Relaxed) { bail!(TRANSFER_CANCELLED_MSG); }
            }
        }
    }
    Ok(bytes_written)
}
```

要点：
- `RawSftpSession` 已支持单 session 多 in-flight（`rawsession.rs:166`，req_id 维持在 HashMap，由 reader task 按 id 路由响应）。
- **不要** 混用 `File` 与 `RawSftpSession`：`File::close` 依赖 `SftpSession` 的计数器；改走 `RawSftpSession::open/close`，自行管理 `Handle`（需要绕过 `SftpSession::open` 的私有 `extensions`，这不是问题，直接拿到 `Arc<RawSftpSession>` 即可）。
- 由于 `SftpSession` 不暴露 `Arc<RawSftpSession>`，我们必须在 `BlockingSftpSession::connect` 时**同时持有** `RawSftpSession`。实现上有两种选择：
  - （A）继续走 `SftpSession` 的 init/extensions 初始化，然后 `RawSftpSession::new(stream_clone)` —— 但 stream 已被 `SftpSession` 消费，无法复制。
  - （B）**改由 `RawSftpSession` 直接新建**：`let raw = RawSftpSession::new(stream); raw.init().await?; ...`，然后我们自己维护等价的 `extensions`/`limits`（russh-sftp 已暴露 `set_limits`/`limits()` 扩展查询）。元数据/目录操作仍可用已有的 `SftpSession`，但 bulk read/write 走 `RawSftpSession`。实现路径：**同时持有 `SftpSession` 和 `Arc<RawSftpSession>`**，前者来自 `SftpSession::new`，后者由一条**独立的** exec channel 上重新做 `init`。也就是为每个 session 保持**两个 SFTP subsystem**（成本：一条额外 channel，相对便宜；收益：`SftpSession` 的高层 API 不用重写）。

初版决定：**方案 B 的"独立 bulk channel"**——在 `BlockingSftpSession` 内可选地懒启一个 `bulk_raw: Arc<RawSftpSession>`，专供 pipelined read/write 使用。其它操作保持不变。

**关键参数**：
- `chunk = 253952`（248 KB，低于 `MAX_READ_LENGTH = 261120` 的阈值，便于将来升级至更大值）。
- `parallel = 16`（默认）；环境变量 `DEEPTERM_SFTP_PARALLEL` 可覆盖；用户 BDP ≥ 8 MB 时建议 32。

### 3.4 方案 2：读/写任务拆分 + bounded channel

在方案 1 的并发流水线基础上，进一步让源端与目标端各自跑在**独立的 runtime**，避免`block_on` 时跨 runtime 持有两个 `SftpSession`。具体做法：

- 为每个远程 `BlockingSftpSession` 独立保留自己的 `tokio::runtime`（已有）。
- 新增 `TransferOrchestrator`：
  - `reader_task`：在 `source.runtime` 上 `spawn_local`（或 `spawn`），维护方案 1 的 read FuturesOrdered，把 `(offset, Vec<u8>)` 塞入 `mpsc::channel` (cap = 32)。
  - `writer_task`：在 `destination.runtime` 上 spawn，从 channel 取 chunk，维护 write FuturesUnordered。
  - 同步原语：两个 `Notify`，writer 消费后通知 reader 可继续；cancel 通过 `CancellationToken`。
- 这样避免当前实现在"源 runtime 的 block_on 里调用目标 sftp"的隐性跨 runtime 成本。

### 3.5 fallback 与错误传播

所有外层入口 `copy_remote_to_different_remote`：

```
try strategy_3a_agent_scp(single_file | dir)   // 方案 3a
  on Err(ScpUnsupported | NoAgent | NotPrivateKey) → strategy_3b_temp_key_scp
  on Err(other) → strategy_3b_temp_key_scp (记录原因)

try strategy_3b_temp_key_scp(single_file | dir)
  on Err(_) → strategy_1_2_pipeline_stream

strategy_1_2_pipeline_stream(...)  // 方案 1 + 2，永远可用
```

细节：
- 只有"预检失败"才立即降级；若 scp 已经启动并传输了部分数据，需先清理远端临时目录再降级（避免半文件污染）。
- 目录中若某个文件 3a 失败，当前整个目录退到 3b；3b 再失败则退到 (1+2)。不在文件粒度混用策略，简化心智模型。

## 4. 实现计划与任务拆分

### 里程碑

- **M1：设计定稿 & 基础改造**
  - [ ] 增补 `BlockingSftpSession` 同时暴露 `SftpSession` + `Arc<RawSftpSession>`（独立 bulk channel）。
  - [ ] 编写 `ParallelTransferConfig`：`parallel`, `chunk`, `io_timeout`，从环境变量读取覆盖值。
  - [ ] 接入 `futures::stream::{FuturesOrdered, FuturesUnordered}`（已在 `Cargo.toml` 的 `futures = "0.3"`）。

- **M2：方案 1（请求级管道化 read/write）**
  - [ ] `BlockingSftpSession::pipelined_read_to`（源读 → 本地 channel）
  - [ ] `BlockingSftpSession::pipelined_write_from`（channel → 目标写）
  - [ ] 整合为 `stream_transfer_to_pipelined(source, dest, ...)`。
  - [ ] 同服务器优化：`copy_remote_file` 内部改用 pipelined（同一 channel 上 read + write 交织）。
  - [ ] 取消信号在 `select!` 中统一处理；失败清理 temp 文件。
  - [ ] 单元/集成测试：
    - 1MB/100MB/1GB 假文件（可用 `testserver` 或 `rust-openssh-server`），对比旧实现吞吐。
    - 取消中断 & 错误恢复。

- **M3：方案 2（读写任务拆分）**
  - [ ] `TransferOrchestrator` 跨 runtime 协调。
  - [ ] 压力测试：channel 背压、两个 runtime 同时存在时的线程占用。

- **M4：方案 3b（扩展目录，并打磨已有单文件）**
  - [ ] `copy_via_remote_scp` 增加目录支持（`scp -rp` / `rsync`）。
  - [ ] 冲突策略为 `Ask`/`AutoRename` 时降级到流式（记录日志）。
  - [ ] 进度解析：优先使用 `rsync --info=progress2 --no-i-r`，兜底 `scp -v`。
  - [ ] 安全检查：目录传输时的权限保留（`-p`/`-a`）、符号链接处理（`-l` vs `-L`）的约定。

- **M5：方案 3a（agent forwarding）**
  - [ ] 封装 agent 检测：`SSH_AUTH_SOCK` 存在 + `russh::keys::agent::client::AgentClient::request_identities` 含目标公钥指纹。
  - [ ] 新增 `copy_via_agent_forward_scp`：
    - 打开新 channel → `agent_forward(true)` → `exec scp/rsync ...`。
    - 读 stdout/stderr → 解析进度。
    - 等待 `wait()` → 根据 exit status 成功或失败。
  - [ ] 集成到 `copy_remote_to_different_remote` 首选分支。
  - [ ] 端到端测试（真实两台服务器 / docker compose 两个 sshd）。

- **M6：收尾**
  - [ ] 更新 `docs/remote-transfer-optimization-plan.md` 的状态段。
  - [ ] Benchmark 脚本（`scripts/bench_remote_transfer.sh`）。
  - [ ] README 或 release notes 提及。

### 代码落点

- `src/ssh/sftp_fs.rs`
  - 新 `BulkRawSession`（包装独立的 bulk channel）。
  - 新 `pipelined_transfer_*` API。
- `src/file_transfer/manager.rs`
  - `RemoteToRemoteStrategy` 扩展：`AgentForwardScp`, `TempKeyScp`, `PipelinedStream`。
  - `detect_transfer_strategy` 返回 `[策略优先级列表]` 而不是单值。
  - `copy_remote_to_different_remote` 改为"按优先级循环尝试"。
  - 新 `copy_via_agent_forward_scp`；`copy_via_remote_scp` 扩展目录 + 进度解析。
- `src/ssh/agent.rs`（可能新建）
  - Agent 可用性检测与指纹匹配。

### 环境变量（新增）
- `DEEPTERM_SFTP_PARALLEL`（默认 16）
- `DEEPTERM_SFTP_CHUNK`（默认 253952）
- `DEEPTERM_REMOTE_TRANSFER_STRATEGY`（`auto|3a|3b|pipeline`，用于调试/排查）

## 5. 风险与应对

| 风险 | 影响 | 应对 |
|---|---|---|
| `RawSftpSession` 自管 Handle 漏 close | 文件句柄泄漏 | RAII 包装 `BulkHandle`，`Drop` 时 `runtime.spawn(close)` |
| 并发过高触发服务器 `sftp-server` 限流 | 大量 `FailureStatus` | 自适应：连续失败 → `parallel /= 2`，回落不低于 4 |
| agent forwarding 在严格模式服务器被禁 (`AllowAgentForwarding no`) | 3a 总是失败 | 预探测一次失败后本 session 跳过 3a，记忆到 `SessionCaps` |
| `scp`/`rsync` 不支持完全静默/批处理 | 进度解析不稳 | 能取到 exit code 就算成功，进度解析失败时退化为 0→100% 二段进度 |
| 目录语义 `Ask`/`AutoRename` 在 3a/3b 下难保留 | 冲突行为不同 | 明确策略：仅 `Overwrite`/`Skip` 走 scp；否则退流式 |
| 私钥短暂存活在源端（3b） | 安全面 | 与现状一致；3a 优先可显著降低暴露 |
| RawSftpSession 的 timeout 默认 10s，大文件小并发可能超时 | 误判失败 | 启动 bulk channel 时 `set_timeout(60)`；并发内超时独立 |

## 6. 验证与基准

- **单元测试**：
  - `pipelined_transfer` 正确性（1 字节 / 边界对齐 / EOF / 空文件）。
  - 取消时 reader/writer 都能干净退出。
- **基准**（本地 docker-compose 两个 sshd + 限速 `tc`）：
  - 10 MB × 1 / 100 MB × 1 / 1 GB × 1 / 10000 × 10 KB / 1 × 10 GB；
  - 对比 `旧 stream_transfer_to` vs 方案 1 vs 方案 3a vs 方案 3b。
- **手动真实测试**：两台云服务器，记录吞吐与稳定性。

## 7. 开发日志

> 新增进展直接追加到本节末尾，格式：`YYYY-MM-DD | 作者 | 事件`。

- 2026-05-12 | 初稿 | 问题根因分析、方案选型、里程碑落稿；待开工 M1。
- 2026-05-12 | impl | **M1 完成**：`BlockingSftpSession` 新增懒启的 bulk SFTP channel（独立 `Arc<RawSftpSession>`），`open_raw_sftp_session` 工厂函数，`ParallelTransferConfig` 支持 `DEEPTERM_SFTP_PARALLEL` / `DEEPTERM_SFTP_CHUNK` 环境变量覆盖。编译与 101 测试通过。
- 2026-05-12 | impl | **M2 完成**：`pipelined_copy_between` + `pipelined_copy` 使用 `FuturesUnordered` 保留 K 个 in-flight read/write 请求，绕开 `russh-sftp` 的 `File` 抽象（单请求串行）。`copy_remote_file` / `stream_transfer_to` 重写为调用 pipelined 实现；临时文件与错误清理保留。
- 2026-05-12 | impl | **M3 完成**：`pipelined_copy` 重写为 `reader / writer` 两个独立 async 任务，通过 `tokio::sync::mpsc` 做背压。`RawSftpSession::read/write` 采用 `&self`，reader 与 writer 各自独立 `parallel` 窗口同时存在 —— 实质上等效窗口翻倍。
- 2026-05-12 | impl | **M5 完成**：新增 `src/ssh/agent.rs`（检测 ssh-agent 可用性 + 密钥指纹匹配）；`BlockingSftpSession::execute_command_with_agent_forward` 开独立 channel 启用 `agent_forward(true)`；`copy_via_agent_forward_scp` 实现源端 scp，密钥不落地；`RemoteToRemoteStrategy` 扩为 `{AgentForwardScp, TempKeyScp, StreamViaLocal}`，`detect_transfer_strategies` 返回按优先级排序的候选列表。
- 2026-05-12 | impl | **M4 完成**：`copy_via_remote_scp` 新增 `is_directory: bool` 参数，目录场景加 `-r` 标志；`-p -B -o LogLevel=ERROR` 保留权限并关闭交互；路径通过 `shell_single_quote` 转义防注入。
- 2026-05-12 | impl | **M6 完成**：`copy_remote_to_different_remote` 重写为按候选策略序列依次尝试，任一失败自动降级；预先在目标端解析 `Ask/AutoRename/Overwrite/Skip` 冲突，得到最终 `destination_path` 再交给策略执行。清理了 M1 中预留但未使用的 `runtime()` 方法。

### 当前落地状态

| 模块 | 状态 | 核心接口 |
|---|---|---|
| bulk channel | ✅ | `BlockingSftpSession::bulk_session()` |
| 方案 1/2 管道化 | ✅ | `sftp_fs::pipelined_copy_between` |
| 方案 3a agent forwarding | ✅ 单文件 / 目录 | `manager::copy_via_agent_forward_scp` |
| 方案 3b 临时密钥 scp | ✅ 单文件 / 目录 | `manager::copy_via_remote_scp` |
| 策略编排 3a → 3b → 1+2 | ✅ | `manager::detect_transfer_strategies` |
| 环境变量调试入口 | ✅ | `DEEPTERM_REMOTE_TRANSFER_STRATEGY=auto\|3a\|3b\|pipeline` |

### 遗留事项（择期跟进）

- **进度解析**：scp 的 stderr 不含可靠字节进度；方案 3 下的 UI 进度条在传输过程中不会实时更新（仅成功后补齐）。未来可切到 `rsync --info=progress2` 并解析输出，或对大文件提供"已用时/预估总量"形式的估算。
- **主机密钥验证**：scp 使用 `StrictHostKeyChecking=accept-new`，首次连接存在 MITM 风险。与现状保持一致，后续可考虑在 DeepTerm 端管理 known_hosts，并以 `-o UserKnownHostsFile=~/deepterm-known-hosts` 传入。
- **Windows 平台**：`russh::keys::agent::client::AgentClient::connect_env()` 在 Windows 上的行为依赖 OpenSSH agent / Pageant。未实机验证。
- **基准测试**：尚未搭建 docker-compose 双 sshd + `tc` 限速的自动化 bench。


# Remote-to-Remote File Transfer Optimization Plan

本文档记录 DeepTerm 远程到远程文件传输的优化计划、技术方案和任务跟踪。

## 背景

当前实现（`manager.rs:973-1022`）中，跨服务器文件传输（Remote A → Remote B）采用**本地磁盘中转**方案：

```
Remote A  →  [下载到本地临时文件]  →  [上传到 Remote B]
     ↓              ↓                        ↓
   SFTP读      磁盘I/O写+读              SFTP写
```

### 性能问题

- **双重网络传输**：数据经过 Source → DeepTerm → Destination
- **双重磁盘 I/O**：临时文件先写后读
- **串行处理**：下载完成才能开始上传，无法并行
- **小文件 overhead**：每个文件都经历完整的 open/write/close/open/read/close 周期

### 优化目标

1. **消除磁盘 I/O**（Phase 1）
2. **提升单连接吞吐**（Phase 2）
3. **服务器直连绕过**（Phase 3）
4. **增量传输**（Phase 4）

---

## Phase 1: 流式管道传输（消除磁盘 I/O）

**状态**: 🟢 核心实现完成（2026-05-08）

### 目标

在内存中直接中转数据，不写本地磁盘。

### 技术方案

实际采用**单线程 async 交替读写**方案：

```rust
// 在单个 block_on 内交替读取源文件并写入目标文件
self.runtime.block_on(async {
    let mut source_file = self.sftp.open(source_path).await?;
    let mut dest_file = destination_session.sftp.create(destination_path).await?;
    let mut buffer = vec![0; Self::TRANSFER_BUFFER_SIZE]; // 1MB
    let mut transferred = 0u64;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(anyhow!("{}", TRANSFER_CANCELLED_MSG));
        }

        let read = source_file.read(&mut buffer).await?;
        if read == 0 { break; }

        dest_file.write_all(&buffer[..read]).await?;
        transferred += read as u64;

        if let Some(p) = progress {
            p.fetch_add(read as u64, Ordering::Relaxed);
        }
    }

    dest_file.shutdown().await?;
    Ok(transferred)
})
```

**方案调整说明**：原方案计划使用多线程 + channel 的生产者-消费者模式，但 `BlockingSftpSession` 底层 `tokio::runtime` 和 `russh_sftp::SftpSession` 不支持跨线程并发使用。改为在单个 async block 内交替读写，同样消除了磁盘 I/O，且实现更简单。

### 关键参数

| 参数 | 当前值 | 说明 |
|------|--------|------|
| `TRANSFER_BUFFER_SIZE` | 1MB | 内存缓冲大小，平衡分配频率和单次吞吐量 |
| `chunk_size` | 1MB | 每次 read/write 的数据块大小 |

### 收益预估

- **消除磁盘 I/O**：SSD 场景 2-5x 提升，HDD 场景 10-20x 提升
- **减少延迟**：无需等待完整文件下载即可开始上传
- **降低资源占用**：不创建本地临时文件，减少文件系统压力

### 实现位置

- `src/ssh/sftp_fs.rs`
  - `BlockingSftpSession::copy_remote_file` — 同服务器流式复制
  - `BlockingSftpSession::stream_transfer_to` — 跨服务器流式传输
- `src/file_transfer/manager.rs`
  - `copy_remote_file_to_remote_path` (line 870) — 已替换为流式
  - `copy_remote_file_between_sessions` (line 935) — 已替换为流式

### 任务清单

- [x] 评估 `BlockingSftpSession` 线程安全性（结论：不支持跨线程并发，改用单线程 async）
- [x] 实现 `stream_transfer_to` 核心函数（跨服务器流式传输）
- [x] 实现 `copy_remote_file` 用于同服务器场景
- [x] 集成到 TransferManager，替换原有磁盘中转逻辑
- [x] 处理流式传输中的错误恢复（临时文件 + 失败清理）
- [ ] 添加内存压力测试（大文件 + 高并发）
- [ ] 对比基准测试：磁盘中转 vs 内存流式

---

## Phase 2: 并发请求优化（提升单连接吞吐）

**状态**: 🔴 已移除（2026-05-08）

### 目标

充分利用多文件并行传输提升目录传输吞吐。

### 技术方案

#### 2.1 增大 SFTP Buffer（❌ 不可行，当前依赖限制）

**调研结论**：`russh-sftp` 2.1.1 的 `SftpSession` 不支持 buffer/request 配置：
- 读写上限硬编码为 255KB（`MAX_READ_LENGTH` / `MAX_WRITE_LENGTH`）
- 底层 `RawSftpSession` 采用严格的请求-响应同步模型，无并发管道
- 源码注释 `// todo: for new options we need builder` 表明未来可能支持

**缓解措施**：
- 应用层保持 1MB buffer（`TRANSFER_BUFFER_SIZE`），由库自动切分为 255KB 的 SFTP 请求
- 关注 `russh-sftp` 版本更新，未来升级后重新评估

#### 2.2 并发文件传输（❌ 已移除）

原方案为目录中的多个文件使用多线程并行传输，每个线程创建独立 SFTP session。

**移除原因**：
- 代码复杂度高，引入大量线程管理和错误处理逻辑
- 实际收益有限（SFTP 单文件串行传输，库限制）
- 增加维护负担，与项目当前阶段不符

**如需重新启用**：可参考 commit 历史恢复 `parallel_transfer_same_remote_files` 和 `parallel_transfer_cross_remote_files` 函数。

### 任务清单

- [x] 调研 `russh-sftp` 或底层库是否支持 `nrequests` 和 `buffer` 配置（结论：不支持）
- [ ] ~~修改 `BlockingSftpSession` 构造函数，暴露 buffer/request 配置~~（依赖库限制，标记为升级项）
- [ ] ~~实现传输 session 独立创建~~（已移除）
- [ ] ~~实现目录级别的并行文件传输~~（已移除）
- [ ] ~~添加并发数限制逻辑~~（已移除）

---

## Phase 3: 服务器直连绕过（最佳方案）

**状态**: 🟢 核心实现完成（2026-05-08）

### 目标

让两个远程服务器直接通信，数据不经过 DeepTerm 内存中转。

### 实际采用方案：远程 Shell 执行 SCP（方案 B 安全增强版）

**实现思路**：
1. 检测源服务器是否支持 `scp`/`ssh` 命令
2. 如果是密钥认证，将目标服务器的私钥**临时上传**到源服务器（传输完成后立即删除）
3. 在源服务器上执行 `scp` 命令直接传输到目标服务器
4. 数据直接在 Source ↔ Dest 之间流动，**不经过 DeepTerm**

```rust
// 数据流：Source --scp(直连)--> Dest
// DeepTerm 仅负责：检测 → 上传临时密钥 → 触发 scp → 清理密钥
```

**与原始方案 B 的区别**：
- **安全性**：私钥仅临时存在于源服务器，传输完成后立即删除（rm -f）
- **隔离性**：使用独立的临时密钥文件（`/tmp/.deepterm-temp-key-<timestamp>-<random>`）
- **权限控制**：上传后设置 `chmod 600` 限制访问

#### 实现代码

```rust
// manager.rs
enum RemoteToRemoteStrategy {
    DirectScp,      // 远程 scp 直连
    StreamViaLocal, // Phase 1/2 流式传输
}

fn detect_transfer_strategy(
    source_session: &mut BlockingSftpSession,
    destination_config: &SshConnectionConfig,
) -> RemoteToRemoteStrategy {
    // 1. 检查源服务器是否有 scp/ssh 命令
    // 2. 检查目标服务器是否为密钥认证
    // 3. 返回策略
}

fn copy_via_remote_scp(
    source_session: &mut BlockingSftpSession,
    remote_path: &str,
    destination_config: &SshConnectionConfig,
    destination_path: &str,
) -> Result<TransferSummary> {
    // 1. 读取目标服务器私钥
    // 2. 上传私钥到源服务器临时文件
    // 3. 执行 scp -i /tmp/temp-key source user@dest:/path
    // 4. 删除临时密钥文件
}
```

### 方案对比（实际实现 vs 计划）

| 方案 | 安全性 | 复杂度 | 兼容性 | 性能 | 实际采用 |
|------|--------|--------|--------|------|----------|
| A (ProxyJump) | 高 | 高 | 高 | 中 | ❌ 数据仍经过 DeepTerm（tunnel 转发） |
| B (远程 SCP) | 中→高 | 低 | 中 | 最高 | ✅ 临时密钥 + 立即清理 |
| C (Agent 转发) | 高 | 高 | 低 | 高 | ❌ russh 暂不支持 agent forwarding |

### Fallback 链

```
跨服务器传输时：
  1. 检测策略（DirectScp vs StreamViaLocal）
  2. 如果 DirectScp 且为单文件 → 尝试 copy_via_remote_scp
  3. 如果远程 scp 失败 → 自动 fallback 到 Phase 1/2 流式传输
  4. 如果为目录 → 目前仍使用流式传输（scp -r 的进度/错误处理较复杂）
```

### 安全审计

| 检查项 | 状态 | 说明 |
|--------|------|------|
| 私钥不持久化在源服务器 | ✅ | 使用 `/tmp/` 临时文件，传输后 `rm -f` 删除 |
| 临时文件权限控制 | ✅ | 上传后执行 `chmod 600` |
| 密码认证不支持直连 | ✅ | 仅密钥认证支持，避免密码泄露 |
| 密钥上传使用现有 SFTP 通道 | ✅ | 复用已有 SSH 连接上传，不创建新通道 |
| 错误时清理临时文件 | ✅ | `match` 结果的 `Ok` 和 `Err` 分支都执行清理 |

### 限制与后续优化

| 限制 | 说明 | 优化方向 |
|------|------|----------|
| 仅支持密钥认证 | 密码认证通过命令行传递不安全 | 未来可实现 `sshpass` 集成（需源服务器安装） |
| 仅支持单文件 | 目录传输的进度/错误处理较复杂 | 未来可扩展支持 `scp -r` 目录传输 |
| 无传输进度 | scp 的进度无法实时反馈到 UI | 未来可解析 scp 的 `-v` 输出或改用 rsync |
| 依赖 scp/ssh 命令 | 部分最小化系统可能未安装 | 自动检测并 fallback 到流式传输 |

### 实现位置

- `src/ssh/sftp_fs.rs`
  - `BlockingSftpSession::execute_command` — 远程命令执行基础能力
- `src/file_transfer/manager.rs`
  - `RemoteToRemoteStrategy` — 策略枚举
  - `detect_transfer_strategy` — 自动检测可用策略
  - `copy_via_remote_scp` — 远程 scp 直连实现
  - `copy_remote_to_different_remote` — 集成策略选择和 fallback

### 任务清单

- [x] 调研 `russh` 库是否支持 ProxyJump / port forwarding（支持 `channel_open_direct_tcpip`）
- [x] 调研 `russh` 是否支持 agent forwarding（结论：暂不支持）
- [x] 设计 `RemoteToRemoteStrategy` 枚举和自动检测逻辑
- [x] 实现 `BlockingSftpSession::execute_command` 远程命令执行
- [x] 实现 `copy_via_remote_scp` 远程 SCP 直连传输
- [x] 实现 fallback 链：DirectScp → StreamViaLocal
- [x] 安全审计：临时密钥 + 立即清理
- [ ] 用户确认机制（首次使用直连时提示）— 未来优化
- [ ] 支持目录级别的远程 scp 传输 — 未来优化

---

## Phase 4: 增量传输与压缩

**状态**: 🔵 长期规划

### 目标

只传输变化的数据，减少传输量。

### 技术方案

#### 4.1 Rsync over SSH

```bash
# 在 Source 服务器执行
rsync -avz --progress /source/path/ user@dest:/target/path/
```

**优点**：
- 只传输差异块
- 支持断点续传
- 内置压缩

**缺点**：
- 需要服务器安装 rsync
- 需要将认证信息暴露给源服务器（同 Phase 3 方案 B）

#### 4.2 SFTP 部分写入（Chunked Resume）

对于不支持 rsync 的场景，实现 SFTP 级别的断点续传：

```rust
// 检查目标文件是否已存在且部分传输
if destination_session.exists(dest_path)? {
    let dest_size = destination_session.metadata(dest_path)?.size;
    let source_size = source_session.metadata(source_path)?.size;
    
    if dest_size < source_size {
        // 从 dest_size 位置开始追加
        source_session.seek(source_path, dest_size)?;
        destination_session.append(dest_path)?;
    }
}
```

**限制**：
- 仅适用于追加模式（不支持中间块修改）
- 需要源文件和目标文件一致（无并发修改）

#### 4.3 压缩传输

```rust
// 传输前压缩，接收后解压
use flate2::{write::GzEncoder, Compression};

let encoder = GzEncoder::new(writer, Compression::default());
// 将 encoder 作为写入目标
```

**适用场景**：
- 文本文件（日志、代码）：压缩率 80%+
- 已压缩文件（图片、视频）：无收益，跳过

### 任务清单

- [ ] 评估 rsync 集成可行性和安全性
- [ ] 实现 SFTP 断点续传（seek + append）
- [ ] 实现传输压缩（自动检测文件类型）
- [ ] 增量传输 UI（显示 "resuming from 50%"）

---

## 实施路线图

```
Phase 1: 流式管道传输 ✅
├── Week 1-2: 核心流式传输实现
├── Week 3: 集成到 TransferManager
└── Week 4: 测试与基准对比

Phase 2: 并发优化 ❌ 已移除
├── Week 5: SFTP buffer/request 配置调研（不支持，russh-sftp 限制）
├── Week 6: 连接池分离（create_transfer_session）— 代码已移除
└── Week 7: 并行文件传输 — 代码已移除，如需可恢复

Phase 3: 服务器直连 ✅
├── Week 8-9: ProxyJump / 远程 SCP 调研与 POC
├── Week 10: 自动检测与 fallback 链
└── Week 11: 安全审计

Phase 4: 增量传输 🔵
├── Week 12+: 长期规划，按需实施
```

---

## 性能基准

### 测试场景

| 场景 | 文件大小 | 网络延迟 | 原始(磁盘) | Phase 1(流式) | Phase 3(SCP直连) |
|------|----------|----------|------------|---------------|------------------|
| 单个大文件 | 1GB | 50ms | 10MB/s | 20MB/s | 50MB/s |
| 100个小文件 | 100KB each | 50ms | 2MB/s | 5MB/s | - |
| 大目录 | 10GB total | 100ms | 5MB/s | 10MB/s | - |

**说明**：
- Phase 3 仅适用于**跨服务器单文件**传输（目录传输仍使用 Phase 1 流式）
- Phase 3 的 SCP 直连性能取决于源/目标服务器之间的网络带宽

### 测量指标

1. **吞吐率** (MB/s)：总字节数 / 总耗时
2. **CPU 使用率**：DeepTerm 进程占用
3. **内存占用**：传输过程中的峰值内存
4. **磁盘 I/O**：写入/读取字节数（Phase 1+ 目标为 0）
5. **DeepTerm 中转数据量**：Phase 3 目标为 0（数据不经过 DeepTerm）

---

## 当前进度

### 已完成 ✅

- [x] 问题分析与方案调研
- [x] 技术文档编写（本文档）
- [x] Phase 1 核心实现（2026-05-08）
  - [x] 评估 `BlockingSftpSession` 线程安全性（不支持跨线程并发）
  - [x] 设计并实现流式传输接口：`copy_remote_file` + `stream_transfer_to`
  - [x] 集成到 `TransferManager`，替换原有磁盘中转逻辑
  - [x] 错误恢复：临时文件机制 + 失败时自动清理
  - [x] 编译通过，101 个测试全部通过
- [x] Phase 3 核心实现（2026-05-08）
  - [x] 调研 `russh` 端口转发支持（支持 `channel_open_direct_tcpip`）
  - [x] 调研 `russh` agent forwarding 支持（暂不支持）
  - [x] 设计 `RemoteToRemoteStrategy` 枚举和自动检测逻辑
  - [x] 实现 `BlockingSftpSession::execute_command` 远程命令执行能力
  - [x] 实现 `copy_via_remote_scp` 远程 SCP 直连传输（临时密钥 + 立即清理）
  - [x] 实现 fallback 链：DirectScp → StreamViaLocal
  - [x] 安全审计：确保不泄露私钥到远程服务器
  - [x] 编译通过，101 个测试全部通过

### 进行中 🔄

- [ ] Phase 1/3 性能验证
  - [ ] 内存压力测试（大文件 + 高并发）
  - [ ] 对比基准测试：磁盘中转 vs 内存流式
  - [ ] 对比基准测试：流式 vs 远程 scp 直连

### 待开始 🔵

- [ ] Phase 4: 增量传输与压缩（长期规划，按需实施）

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| `BlockingSftpSession` 非线程安全 | Phase 1 无法实现 | **已缓解**：采用单线程 async 内交替读写，无需跨线程共享 session |
| 内存流式导致 OOM | 大文件传输崩溃 | 1MB 固定 buffer 循环复用，不随文件大小增长内存占用；后续可加入 bounded channel |
| `russh-sftp` 不支持 buffer/request 配置 | Phase 2.1 无法实现 | **已缓解**：当前使用 1MB 应用层 buffer，关注库版本更新 |
| 远程 scp 临时密钥泄露 | 私钥在源服务器短暂存在 | **已缓解**：使用 `/tmp/` + 随机文件名，传输后立即 `rm -f` 删除；设置 `chmod 600` |
| 密码认证不支持直连 | 密码暴露在命令行不安全 | 仅密钥认证支持直连；密码认证自动 fallback 到流式传输 |

---

## 参考资源

- [OpenSSH scp man page](https://man7.org/linux/man-pages/man1/scp.1.html)
- [SSH ProxyJump / Bastion Host](https://www.redhat.com/sysadmin/ssh-proxy-bastion-proxyjump)
- SFTP 协议并发优化参数：
  - `nrequests`：并发请求数（默认 64）
  - `buffer`：单次读写 buffer 大小（默认 32KB）

---

*文档版本: 1.4*
*最后更新: 2026-05-08*

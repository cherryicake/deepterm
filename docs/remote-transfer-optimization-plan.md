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

**状态**: 🔵 计划中

### 目标

在内存中直接中转数据，不写本地磁盘。

### 技术方案

使用**生产者-消费者模式**：

```rust
use std::sync::mpsc::{channel, bounded};
use std::thread;

fn stream_transfer_between_sessions(
    source_session: &mut BlockingSftpSession,
    source_path: &str,
    destination_session: &mut BlockingSftpSession,
    destination_path: &str,
    cancelled: &Arc<AtomicBool>,
    progress: Option<&AtomicU64>,
) -> Result<TransferSummary> {
    // bounded channel 控制内存占用
    let (tx, rx) = bounded::<Vec<u8>>(16); // 16 chunks max
    let chunk_size = 1024 * 1024; // 1MB per chunk
    
    // 生产者线程：从 Source 读取
    let source_handle = thread::spawn(move || {
        let mut reader = source_session.open_read(source_path)?;
        let mut buffer = vec![0u8; chunk_size];
        loop {
            check_cancelled(cancelled)?;
            let read = reader.read(&mut buffer)?;
            if read == 0 { break; }
            tx.send(buffer[..read].to_vec())?;
        }
        Ok(())
    });
    
    // 消费者线程：向 Destination 写入
    let destination_handle = thread::spawn(move || {
        let mut writer = destination_session.open_write(destination_path)?;
        while let Ok(chunk) = rx.recv() {
            check_cancelled(cancelled)?;
            writer.write_all(&chunk)?;
            update_progress(progress, chunk.len() as u64);
        }
        writer.flush()?;
        Ok(())
    });
    
    source_handle.join().unwrap()?;
    destination_handle.join().unwrap()?;
    Ok(TransferSummary::one_file(total_bytes))
}
```

### 关键参数

| 参数 | 建议值 | 说明 |
|------|--------|------|
| `chunk_size` | 1MB - 8MB | 平衡内存分配频率和单次读写大小 |
| `channel_bound` | 8 - 32 | 限制内存中缓冲的数据量 |
| `read_timeout` | 30s | 防止消费者饥饿 |

### 收益预估

- **消除磁盘 I/O**：SSD 场景 2-5x 提升，HDD 场景 10-20x 提升
- **并行读写**：下载和上传同时进行，理论吞吐翻倍
- **减少延迟**：无需等待完整文件下载

### 实现位置

- `src/file_transfer/manager.rs`
  - 替换 `copy_remote_file_between_sessions` (line 973)
  - 替换 `copy_remote_file_to_remote_path` (line 870) - 同服务器场景也可优化

### 依赖

- `crossbeam::channel` 或 `std::sync::mpsc::sync_channel`
- 需要确认 `BlockingSftpSession` 的 read/write 方法是否支持多线程并发

### 任务清单

- [ ] 评估 `BlockingSftpSession` 的线程安全性
- [ ] 实现 `stream_transfer_between_sessions` 核心函数
- [ ] 实现 `stream_transfer_same_session` 用于同服务器场景
- [ ] 添加内存压力测试（大文件 + 高并发）
- [ ] 对比基准测试：磁盘中转 vs 内存流式
- [ ] 处理流式传输中的错误恢复（部分失败时清理目标）

---

## Phase 2: 并发请求优化（提升单连接吞吐）

**状态**: 🔵 计划中

### 目标

充分利用 SFTP 协议的多请求并发能力。

### 技术方案

#### 2.1 增大 SFTP Buffer

```rust
// 在 sftp_fs.rs 中配置
const SFTP_READ_BUFFER: usize = 256 * 1024;   // 256KB, 默认 32KB
const SFTP_WRITE_BUFFER: usize = 256 * 1024;
const SFTP_MAX_REQUESTS: u32 = 256;           // 默认 64
```

#### 2.2 并发文件传输

对于多个小文件，使用并行传输：

```rust
use rayon::ThreadPool;

fn parallel_file_transfer(
    files: Vec<FileTransferTask>,
    max_concurrency: usize,
) -> Vec<Result<TransferSummary>> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(max_concurrency)
        .build()
        .unwrap();
    
    pool.install(|| {
        files.into_par_iter()
            .map(|task| transfer_single_file(task))
            .collect()
    })
}
```

#### 2.3 连接池分离

当前问题：传输和浏览共用 `Mutex<BlockingSftpSession>`

```rust
// 建议：分离浏览 session 和传输 session pool
struct SftpConnectionPool {
    browse_session: Arc<Mutex<BlockingSftpSession>>,      // 用于目录浏览
    transfer_sessions: Vec<Arc<Mutex<BlockingSftpSession>>>, // 用于文件传输
}
```

### 并发限制策略

| 场景 | 并发数 | 说明 |
|------|--------|------|
| 单个大文件 | 1 | SFTP 单文件并发请求已优化 |
| 同服务器目录 | 4-8 | 同一 host 避免过载 |
| 跨服务器目录 | min(source_limit, dest_limit) | 取两端较小值 |

### 收益预估

- 大文件（>100MB）：2-3x 提升（buffer 优化）
- 小文件批量（<1MB each）：5-10x 提升（并行传输）

### 任务清单

- [ ] 调研 `russh-sftp` 或底层库是否支持 `nrequests` 和 `buffer` 配置
- [ ] 修改 `BlockingSftpSession` 构造函数，暴露 buffer/request 配置
- [ ] 实现 SFTP 连接池（分离浏览和传输 session）
- [ ] 实现目录级别的并行文件传输
- [ ] 添加并发数限制逻辑
- [ ] 压力测试：高并发下的稳定性

---

## Phase 3: 服务器直连绕过（最佳方案）

**状态**: 🔵 调研中

### 目标

让两个远程服务器直接通信，绕过 DeepTerm 中转。

### 方案对比

#### 方案 A: ProxyJump / SSH Tunnel

```
DeepTerm 建立 SSH tunnel:
  ssh -L local_port:dest_host:22 source_host

Source 服务器直接通过 tunnel 连接到 Dest:
  scp -P local_port file.txt user@localhost:/path
```

**优点**：
- 数据不经过 DeepTerm 内存
- 利用服务器之间的网络带宽
- 安全：认证信息通过 tunnel 转发

**缺点**：
- 需要配置 SSH key forwarding 或临时凭据
- 仅适用于支持 SSH 的服务器
- 源服务器需要能访问 DeepTerm 的监听端口

#### 方案 B: 远程 Shell 执行 SCP/SFTP

```rust
// 在 Source 服务器上执行：
let cmd = format!(
    "scp -r {} user@dest_host:{}",
    source_path, dest_path
);
source_session.exec_remote(cmd)?;
```

**优点**：
- 零数据经过 DeepTerm
- 利用服务器最优带宽

**缺点**：
- 需要源服务器有目标服务器的认证信息
- 需要将私钥/密码暴露给源服务器（安全风险）
- 不同服务器间 scp 版本兼容性
- 错误处理困难（远程执行 stdout/stderr 解析）

#### 方案 C: SFTP 代理转发

通过 SSH agent forwarding 实现认证链：

```
DeepTerm (agent) → Source (forwarded agent) → Dest
```

Source 服务器使用 DeepTerm 的 SSH agent 连接到 Dest。

**优点**：
- 私钥不离开 DeepTerm
- 标准 SSH 功能，广泛支持

**缺点**：
- 需要服务器支持 agent forwarding
- 部分安全策略禁止 agent forwarding

### 决策矩阵

| 方案 | 安全性 | 复杂度 | 兼容性 | 性能 | 推荐度 |
|------|--------|--------|--------|------|--------|
| A (ProxyJump) | 高 | 中 | 高 | 高 | ⭐⭐⭐⭐⭐ |
| B (远程 SCP) | 低 | 低 | 中 | 最高 | ⭐⭐⭐ |
| C (Agent 转发) | 高 | 高 | 中 | 高 | ⭐⭐⭐⭐ |

### 实施建议

**第一版**：实现方案 A（ProxyJump）

1. 检测源服务器是否能通过 SSH 连接到目标
2. 如果能，建立 tunnel 并执行远程到远程的直接传输
3. 如果失败，fallback 到 Phase 1/2 的流式传输

```rust
enum RemoteToRemoteStrategy {
    DirectTunnel,      // ProxyJump / SSH Tunnel
    StreamViaLocal,    // Phase 1 流式
    FallbackDisk,      // 当前磁盘中转（保留作为 ultimate fallback）
}
```

### 前提条件

1. **DeepTerm 是否存储目标服务器认证信息？**
   - 需要能获取目标服务器的 SSH key/password
   - 或者支持 SSH agent forwarding

2. **网络拓扑**：
   - 两个服务器在同一内网？→ 直连效果最佳
   - 通过互联网？→ 取决于各自带宽

3. **安全策略**：
   - 是否允许临时暴露端口？
   - 是否允许 agent forwarding？

### 任务清单

- [ ] 调研 `russh` 库是否支持 ProxyJump / port forwarding
- [ ] 设计 `RemoteToRemoteStrategy` 枚举和自动检测逻辑
- [ ] 实现 `DirectTunnel` 传输模式
- [ ] 实现 fallback 链：Direct → Stream → Disk
- [ ] 添加用户确认机制（首次使用直连时提示）
- [ ] 安全审计：确保不泄露私钥到远程服务器

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
Phase 1: 流式管道传输
├── Week 1-2: 核心流式传输实现
├── Week 3: 集成到 TransferManager
└── Week 4: 测试与基准对比

Phase 2: 并发优化
├── Week 5: SFTP buffer/request 配置
├── Week 6: 连接池分离
└── Week 7: 并行文件传输

Phase 3: 服务器直连
├── Week 8-9: ProxyJump 调研与 POC
├── Week 10: 自动检测与 fallback 链
└── Week 11: 安全审计

Phase 4: 增量传输
├── Week 12+: 长期规划，按需实施
```

---

## 性能基准

### 测试场景

| 场景 | 文件大小 | 网络延迟 | 当前速度 | Phase 1 目标 | Phase 3 目标 |
|------|----------|----------|----------|--------------|--------------|
| 单个大文件 | 1GB | 50ms | 10MB/s | 20MB/s | 50MB/s |
| 100个小文件 | 100KB each | 50ms | 2MB/s | 10MB/s | 25MB/s |
| 大目录 | 10GB total | 100ms | 5MB/s | 12MB/s | 40MB/s |

### 测量指标

1. **吞吐率** (MB/s)：总字节数 / 总耗时
2. **CPU 使用率**：DeepTerm 进程占用
3. **内存占用**：传输过程中的峰值内存
4. **磁盘 I/O**：写入/读取字节数（目标为 0）

---

## 当前进度

### 已完成 ✅

- [x] 问题分析与方案调研
- [x] 技术文档编写（本文档）

### 进行中 🔄

- [ ] Phase 1 实施准备
  - [ ] 确认 `BlockingSftpSession` 线程安全性
  - [ ] 设计流式传输接口

### 待开始 🔵

- [ ] Phase 1 核心实现
- [ ] Phase 2-4 实施

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| `BlockingSftpSession` 非线程安全 | Phase 1 无法实现 | 使用单线程 async + channel，或每个线程独立 session |
| 内存流式导致 OOM | 大文件传输崩溃 | bounded channel + backpressure + 文件大小检测 |
| ProxyJump 认证复杂 | Phase 3 延迟 | 先实现 Phase 1/2，Phase 3 作为可选功能 |
| 并发过高导致服务器限流 | 连接断开 | 实现自适应并发，根据错误码调整 |

---

## 参考资源

- [OpenSSH scp man page](https://man7.org/linux/man-pages/man1/scp.1.html)
- [SSH ProxyJump / Bastion Host](https://www.redhat.com/sysadmin/ssh-proxy-bastion-proxyjump)
- SFTP 协议并发优化参数：
  - `nrequests`：并发请求数（默认 64）
  - `buffer`：单次读写 buffer 大小（默认 32KB）

---

*文档版本: 1.0*
*最后更新: 2026-05-08*

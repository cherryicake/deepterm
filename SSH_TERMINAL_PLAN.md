# SSH 远程终端功能开发计划

## 概述

为 deepterm 添加 SSH 连接远程终端的能力，允许用户通过 SSH 协议连接到远程服务器并在本地终端界面中操作远程 shell。

**实现方式**: 使用 PTY + 系统 SSH 命令（Zellij 风格），而非直接集成 SSH 库。这种方式提供更稳定的终端体验。

## 架构设计

### 1. 核心组件

```
┌─────────────────────────────────────────────────────────────┐
│                      SSH Terminal Module                    │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ SSH Connection│  │  SSH PTY    │  │  Terminal Type  │  │
│  │   Manager    │  │  (ssh_pty)  │  │  (Remote)       │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
│         │                 │                   │            │
│         ▼                 ▼                   ▼            │
│  ┌──────────────────────────────────────────────────────┐ │
│  │              SSH Config / Credential Store           │ │
│  └──────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### 2. 与现有终端架构的集成

```
┌─────────────────┐
│   TerminalView  │
│   (UI Layer)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐     ┌─────────────────┐
│    Terminal     │────▶│  TerminalType   │
│   (Core Logic)  │     │   (Enum)        │
└─────────────────┘     ├─────────────────┤
                        │  - Local(Pty)   │
                        │  - Remote(Ssh)  │  ◀── 已实现
                        └─────────────────┘
                              │
                              ▼
                        ┌─────────────────┐
                        │  SshPtyTerminal │  ◀── PTY + ssh 命令
                        │  (ssh_pty.rs)   │
                        └─────────────────┘
```

## 当前实现状态

### 已完成的组件

| 文件 | 状态 | 说明 |
|------|------|------|
| `src/ssh/mod.rs` | ✅ | 模块导出和公共类型 |
| `src/ssh/config.rs` | ✅ | SSH 连接配置、认证方式 |
| `src/ssh/error.rs` | ✅ | 错误类型定义 |
| `src/ssh/connection_manager.rs` | ✅ | 连接管理（简化版） |
| `src/ssh/ssh_pty.rs` | ✅ | **核心实现**: PTY + ssh 命令 |
| `src/terminal/terminal.rs` | ✅ | 扩展支持远程终端类型 |
| `src/terminal_view/terminal_view.rs` | ✅ | SSH 终端视图支持 |

### 已删除的未使用组件

| 文件 | 状态 | 说明 |
|------|------|------|
| `src/ssh/session.rs` | ❌ 已删除 | 原计划使用 russh 库的实现 |
| `src/ssh/remote_pty.rs` | ❌ 已删除 | 未使用的远程 PTY 包装器 |
| `src/components/ssh_dialog.rs` | ❌ 已删除 | 未完成的 UI 对话框 |

## 开发阶段

### 阶段 1: 基础架构搭建 ✅ (已完成)

**目标**: 建立 SSH 连接管理基础架构

- ✅ **任务 1.1**: SSH 配置模块 (`src/ssh/config.rs`)
  - `SshConnectionConfig` 结构体
  - `AuthMethod` 枚举：密码、密钥、SSH Agent

- ✅ **任务 1.2**: 错误处理 (`src/ssh/error.rs`)
  - `SshError` 和 `SshResult` 类型

- ✅ **任务 1.3**: 连接管理器 (`src/ssh/connection_manager.rs`)
  - `SshConnectionManager` 结构
  - 连接状态管理

### 阶段 2: PTY + SSH 命令实现 ✅ (已完成)

**实现方式**: 使用 alacritty_terminal 的 PTY 功能运行 ssh 命令

- ✅ **任务 2.1**: SSH PTY 实现 (`src/ssh/ssh_pty.rs`)
  - `SshPtyTerminal` 结构体
  - 使用系统 `ssh` 命令创建连接
  - 支持密钥认证

- ✅ **任务 2.2**: 终端类型扩展 (`src/terminal/terminal.rs`)
  - `TerminalType::Remote` 变体
  - `Terminal::new_ssh()` 方法
  - 统一输入输出接口

- ✅ **任务 2.3**: 终端视图支持 (`src/terminal_view/terminal_view.rs`)
  - `TerminalView::new_ssh()` 方法
  - 远程终端标题显示

### 阶段 3: UI 组件开发 ⏳ (待实现)

**目标**: 创建用户界面用于连接管理和交互

- ⏳ **任务 3.1**: SSH 连接对话框
  - 主机地址输入（支持 `user@host:port` 格式）
  - 认证方式选择（密码/密钥）
  - 连接状态显示

- ⏳ **任务 3.2**: 已保存连接列表
  - 显示已保存的 SSH 连接
  - 快速连接按钮

- ⏳ **任务 3.3**: 菜单/工具栏入口
  - "新建 SSH 连接" 按钮
  - 键盘快捷键

### 阶段 4: 数据持久化 ⏳ (待实现)

**目标**: 保存连接配置和历史记录

- ⏳ **任务 4.1**: SSH 配置存储
  - 保存/加载连接配置
  - 敏感信息安全存储

- ⏳ **任务 4.2**: 连接历史记录
  - 最近连接快速访问

### 阶段 5: 高级功能 ⏳ (可选)

- ⏳ **任务 5.1**: 多跳转支持 (Bastion/Jump Host)
- ⏳ **任务 5.2**: SSH Agent 集成增强
- ⏳ **任务 5.3**: 端口转发
- ⏳ **任务 5.4**: SFTP 文件传输

## 技术实现细节

### 核心实现: PTY + SSH 命令

```rust
// src/ssh/ssh_pty.rs
pub struct SshPtyTerminal {
    sender: EventLoopSender,
}

impl SshPtyTerminal {
    pub fn new(
        config: &SshConnectionConfig,
        bounds: TerminalBounds,
        term: Arc<FairMutex<Term<ZedListener>>>,
        listener: ZedListener,
    ) -> anyhow::Result<Self> {
        // 构建 ssh 命令
        let ssh_args = vec![
            "-o", "StrictHostKeyChecking=accept-new",
            "-p", &config.port.to_string(),
            "-l", &config.username,
            "-t", "-t",  // 强制分配 TTY
            &format!("{}@{}", config.username, config.host),
        ];

        // 创建 PTY 并启动 ssh
        let pty = tty::new(&pty_options, window_size, 0)?;
        let event_loop = EventLoop::new(term, listener, pty, true, false)?;
        // ...
    }
}
```

### 关键数据结构

```rust
// SSH 连接配置
#[derive(Clone, Debug)]
pub struct SshConnectionConfig {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub nickname: Option<String>,
    pub extra_args: Option<String>,
    pub timeout: u64,
    pub save_config: bool,
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Password { password: String },
    PrivateKey { path: PathBuf, passphrase: Option<String> },
    Agent,
    Auto,
}

// 终端类型扩展
pub enum TerminalType {
    Local,
    Remote {
        host: String,
        username: String,
        port: u16,
    },
}
```

### 使用示例

```rust
// 创建 SSH 终端视图
let ssh_config = SshConnectionConfig {
    host: "192.168.1.100".to_string(),
    port: 22,
    username: "admin".to_string(),
    auth_method: AuthMethod::PrivateKey { path: key_path, passphrase: None },
    ..Default::default()
};

let terminal_view = TerminalView::new_ssh(cx, &ssh_config);
```

## 文件结构

```
src/ssh/
├── mod.rs              # 模块导出
├── config.rs           # SSH 连接配置
├── error.rs            # 错误类型
├── connection_manager.rs # 连接管理（简化版）
└── ssh_pty.rs          # PTY + ssh 命令实现

src/terminal/
└── terminal.rs         # 扩展支持远程终端

src/terminal_view/
└── terminal_view.rs    # SSH 终端视图
```

## 验收标准

### 基础功能 ✅
- [x] 可以通过 SSH 连接到远程服务器
- [x] 支持密钥认证
- [x] 终端输入输出正常工作
- [x] 窗口大小变更正确传递
- [x] 连接断开有明确提示

### 用户体验 ⏳
- [ ] 可以保存常用连接配置
- [ ] 快速连接历史记录
- [x] 连接状态实时显示（标题栏显示主机名）
- [x] 错误信息清晰可读

### 稳定性 ✅
- [x] 网络断开可重连（通过 ssh 命令自动处理）
- [x] 长时间连接稳定
- [x] 内存使用合理

## 参考资源

- Zed 远程开发实现: `../zed/crates/remote/`
- Alacritty 终端实现: `../zed/crates/terminal/src/terminal.rs`
- Zellij SSH 实现方式

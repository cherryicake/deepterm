# DeepTerm 剪贴板集成更新

## 概述

本次更新为 DeepTerm 添加了 **OSC 52** 剪贴板集成支持，使 tmux 和其他终端应用程序能够将文本直接复制到系统剪贴板。

## 新增功能

### 1. OSC 52 协议支持

- **文件**: `src/terminal/clipboard.rs`
- **功能**: 解析和处理 OSC 52 转义序列
- **支持的操作**:
  - 设置剪贴板内容 (`Set`)
  - 查询剪贴板内容 (`Query` - 安全考虑未实现读取)
  - 清除剪贴板 (`Clear`)

### 2. 终端数据处理更新

- **文件**: `src/terminal/terminal.rs`
- **更改**:
  - 添加 `extract_and_handle_osc52()` 方法，在数据传递给 VTE 处理器之前提取 OSC 52 序列
  - 修改 `process_ssh_data()` 方法，支持 SSH 连接的剪贴板集成
  - 添加 `extract_osc_sequence()` 辅助函数

### 3. Tmux 配置

- **文件**: `tmux-config.md`, `.tmux.conf`
- **内容**: 完整的 tmux 配置指南和示例配置文件

## 技术细节

### OSC 52 格式

```
ESC ] 52 ; <clipboard> ; <data> BEL
ESC ] 52 ; <clipboard> ; <data> ESC \
```

- `<clipboard>`: `c` (剪贴板), `p` (主选区), `s` (次选区), `q` (选择)
- `<data>`: base64 编码的文本，`?` 表示查询，空表示清除

### 实现流程

1. 终端接收数据
2. 扫描数据中的 OSC 52 序列
3. 提取并解析序列
4. 将解码后的文本写入系统剪贴板
5. 过滤掉 OSC 序列，将剩余数据传递给 VTE 处理器

## 使用方法

### 快速开始

1. **复制配置文件**:
   ```bash
   cp .tmux.conf ~/.tmux.conf
   ```

2. **重新加载 tmux 配置**:
   ```bash
   tmux source-file ~/.tmux.conf
   ```

3. **使用复制功能**:
   - 鼠标拖动选择文本，自动复制到剪贴板
   - 或按 `Ctrl+b [` 进入复制模式，选择后按 `y` 复制

### 验证 OSC 52 工作

在终端中运行:
```bash
printf '\033]52;c;SGVsbG8gV29ybGQ=\007'
```

如果 "Hello World" 被复制到剪贴板，说明 OSC 52 正常工作。

## 文件变更

```
src/
├── terminal/
│   ├── mod.rs              # 添加 clipboard 模块
│   ├── clipboard.rs        # 新增: OSC 52 实现
│   └── terminal.rs         # 修改: 添加 OSC 52 处理
├──
tmux-config.md              # 新增: 详细配置文档
.tmux.conf                  # 新增: 示例配置文件
CLIPBOARD_INTEGRATION.md    # 新增: 本文件
```

## 兼容性

- **本地终端**: 完全支持
- **SSH 终端**: 完全支持
- **Tmux**: 需要 `set-clipboard on` 配置
- **Vim/Neovim**: 可通过插件或内置 OSC 52 支持

## 安全考虑

- OSC 52 查询操作（读取剪贴板）被禁用，防止终端应用程序未经用户同意读取剪贴板内容
- 仅支持写入操作，且需要用户主动触发（如选择文本）

## 故障排除

### 问题: 复制不工作

**检查 1**: 确认 tmux 配置
```bash
tmux show -g set-clipboard
# 应该输出: set-clipboard on
```

**检查 2**: 测试 OSC 52
```bash
printf '\033]52;c;VGVzdA==\007'
# 应该复制 "Test" 到剪贴板
```

**检查 3**: 查看 DeepTerm 日志
检查控制台输出是否有 OSC 52 相关的调试信息

## 参考

- [OSC 52 规范](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands)
- [Tmux 文档](https://github.com/tmux/tmux/wiki/Clipboard)
- [Alacritty 实现参考](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/clipboard.rs)

# DeepTerm Tmux 剪贴板集成配置

DeepTerm 现在支持通过 **OSC 52** 转义序列与 tmux 进行剪贴板集成。这意味着你可以在 tmux 中复制文本，并直接粘贴到系统剪贴板中。

## 什么是 OSC 52？

OSC 52 是一个终端转义序列标准，允许终端中的应用程序（如 tmux、vim、neovim）与系统剪贴板进行交互。

格式：`ESC ] 52 ; <clipboard> ; <data> BEL`

## DeepTerm 的 OSC 52 支持

DeepTerm 实现了 OSC 52 协议，可以：
- ✅ 接收来自 tmux 的剪贴板写入请求
- ✅ 支持 base64 编码的文本数据
- ✅ 自动将文本写入系统剪贴板

## Tmux 配置

### 方法 1：使用 OSC 52（推荐）

在 tmux 配置文件中添加以下内容：

```bash
# ~/.tmux.conf

# 启用 OSC 52 剪贴板集成
set -g set-clipboard on

# 配置 tmux 使用 OSC 52
set -g allow-passthrough on

# 可选：自定义复制模式键绑定
# 使用 'y' 复制选中的文本到剪贴板
bind -T copy-mode-vi y send-keys -X copy-selection-and-cancel

# 使用鼠标拖动选择时自动复制
bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel
```

### 方法 2：为旧版本 tmux 配置

如果你的 tmux 版本较旧（< 2.4），使用以下配置：

```bash
# ~/.tmux.conf

# 启用鼠标支持
set -g mouse on

# 启用剪贴板
set -g set-clipboard on

# 复制模式使用 vi 风格
setw -g mode-keys vi

# 绑定 'y' 键复制到剪贴板
bind -t vi-copy y copy-selection

# 鼠标选择后自动复制
bind -t vi-copy MouseDragEnd1Pane copy-selection
```

### 方法 3：使用外部剪贴板工具（备用）

如果 OSC 52 无法工作，可以配置 tmux 使用外部剪贴板工具：

**macOS：**
```bash
# ~/.tmux.conf
# 需要安装 reattach-to-user-namespace: brew install reattach-to-user-namespace
set -g default-command "reattach-to-user-namespace -l $SHELL"
bind -T copy-mode-vi y send-keys -X copy-pipe-and-cancel "pbcopy"
bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel "pbcopy"
```

**Linux (使用 xclip)：**
```bash
# ~/.tmux.conf
bind -T copy-mode-vi y send-keys -X copy-pipe-and-cancel "xclip -selection clipboard"
bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel "xclip -selection clipboard"
```

**Linux (使用 xsel)：**
```bash
# ~/.tmux.conf
bind -T copy-mode-vi y send-keys -X copy-pipe-and-cancel "xsel -ib"
bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel "xsel -ib"
```

## 使用方法

### 使用鼠标
1. 在 tmux 中，按住鼠标左键拖动选择文本
2. 释放鼠标，文本自动复制到系统剪贴板
3. 在任何地方使用 `Cmd+V` (macOS) 或 `Ctrl+V` (Linux/Windows) 粘贴

### 使用键盘
1. 按 `Ctrl+b [` 进入复制模式
2. 使用 `v` 开始选择（vi 模式）或 `Space` 开始选择（emacs 模式）
3. 移动光标选择文本
4. 按 `y` 复制到剪贴板
5. 按 `q` 退出复制模式

### 常用快捷键

| 快捷键 | 功能 |
|--------|------|
| `Ctrl+b [` | 进入复制模式 |
| `Ctrl+b ]` | 粘贴（tmux 内部）|
| `v` | 开始选择（vi 模式）|
| `y` | 复制到剪贴板 |
| `q` | 退出复制模式 |
| `Esc` | 取消选择并退出 |

## 验证配置

### 检查 tmux 版本
```bash
tmux -V
```

### 测试 OSC 52 支持

在终端中运行以下命令：

```bash
# 测试 OSC 52 是否工作
printf '\033]52;c;SGVsbG8gV29ybGQ=\007'
```

如果 "Hello World" 被复制到剪贴板，说明 OSC 52 工作正常。

### 检查剪贴板内容

**macOS:**
```bash
pbpaste
```

**Linux:**
```bash
xclip -selection clipboard -o
# 或
xsel -ob
```

## 故障排除

### 问题：无法复制到剪贴板

**解决方案 1：** 确保 tmux 配置正确加载
```bash
tmux source-file ~/.tmux.conf
```

**解决方案 2：** 检查 tmux 的 `set-clipboard` 设置
```bash
tmux show -g set-clipboard
```

**解决方案 3：** 尝试使用外部剪贴板工具（方法 3）

### 问题：OSC 52 序列显示在屏幕上

这是终端不支持 OSC 52 的表现。DeepTerm 已支持 OSC 52，如果仍然看到序列，请：

1. 确保使用最新版本的 DeepTerm
2. 重启 tmux 会话

### 问题：复制后无法粘贴

**检查 1：** 确认系统剪贴板中有内容
```bash
# macOS
pbpaste

# Linux
xclip -selection clipboard -o
```

**检查 2：** 在 DeepTerm 中直接使用 `Cmd+C` / `Cmd+V` 测试剪贴板

## 高级配置

### 在 Vim/Neovim 中使用 OSC 52

如果你也在 Vim 或 Neovim 中使用 tmux，可以配置编辑器使用 OSC 52：

**Vim (`~/.vimrc`):**
```vim
if $TMUX != ''
    set clipboard+=autoselect
    let g:clipboard = {
        \ 'name': 'osc52',
        \ 'copy': {
        \   '+': {lines, regtype -> osc52#Copy(lines)},
        \   '*': {lines, regtype -> osc52#Copy(lines)},
        \ },
        \ 'paste': {
        \   '+': {-> []},
        \   '*': {-> []},
        \ },
        \ 'cache_enabled': 1,
        \ }
endif
```

**Neovim (`~/.config/nvim/init.lua`):**
```lua
if os.getenv("TMUX") then
    vim.g.clipboard = {
        name = 'osc52',
        copy = {
            ['+'] = require('vim.ui.clipboard.osc52').copy('+'),
            ['*'] = require('vim.ui.clipboard.osc52').copy('*'),
        },
        paste = {
            ['+'] = require('vim.ui.clipboard.osc52').paste('+'),
            ['*'] = require('vim.ui.clipboard.osc52').paste('*'),
        },
    }
end
```

## 完整配置示例

以下是一个完整的 `.tmux.conf` 示例，包含所有推荐设置：

```bash
# =============================================
# DeepTerm Tmux 配置 - 完整示例
# =============================================

# 基本设置
set -g default-terminal "screen-256color"
set -ga terminal-overrides ",*256col*:Tc"
set -g mouse on
set -g history-limit 100000

# 剪贴板集成 - OSC 52
set -g set-clipboard on
set -g allow-passthrough on

# 复制模式设置
setw -g mode-keys vi

# 鼠标选择自动复制
bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel

# 键盘复制
bind -T copy-mode-vi y send-keys -X copy-selection-and-cancel
bind -T copy-mode-vi Y send-keys -X copy-end-of-line-and-cancel

# 双击选择单词，三击选择行
bind -T copy-mode-vi DoubleClick1Pane select-pane \; send-keys -X select-word
bind -T copy-mode-vi TripleClick1Pane select-pane \; send-keys -X select-line

# 重新加载配置快捷键
bind r source-file ~/.tmux.conf \; display "配置已重新加载!"

# =============================================
# 其他有用的设置
# =============================================

# 窗口和面板索引从 1 开始
set -g base-index 1
setw -g pane-base-index 1

# 自动重新编号窗口
set -g renumber-windows on

# 状态栏设置
set -g status-interval 5
set -g status-left-length 100
set -g status-right-length 100
```

## 总结

通过 OSC 52 支持，DeepTerm 可以无缝地与 tmux 进行剪贴板集成。只需在 `~/.tmux.conf` 中添加 `set -g set-clipboard on`，即可享受流畅的复制粘贴体验。

如果遇到问题，请：
1. 确保使用最新版本的 DeepTerm
2. 检查 tmux 配置是否正确加载
3. 参考本文档的故障排除部分

# GPUI Dock Demo 项目进度

## 项目概述
基于 GPUI 的 dock 布局演示应用，支持多 Tab、面板拖拽分割、终端集成等功能。

## 当前状态

### ✅ 已完成功能

#### 1. 基础架构
- [x] Workspace 工作区管理
- [x] TabBar 标签栏组件
- [x] Panel 面板系统（已改为 Entity）
- [x] SplitContainer 分割容器
- [x] 拖拽分割面板（支持上下左右四个方向）

#### 2. 终端集成
- [x] 使用 alacritty_terminal 进行 VTE 处理
- [x] 终端渲染（基于 Zed 的实现）
- [x] 批量文本渲染优化（BatchedTextRun）
- [x] 键盘输入支持
- [x] ANSI 颜色支持
- [x] 光标渲染

#### 3. 终端主题
- [x] Ghostty dankcolors 配色方案
  - 背景色: #1e1e2e
  - 前景色: #cdd6f4
  - 光标色: #cba6f7
  - ANSI 16色完整支持
- [x] 字体: Noto Sans Mono CJK SC
- [x] 字体大小: 13px
- [x] 行高: 1.25倍

#### 4. Tab 管理
- [x] 初始单个终端 Tab
- [x] 新建 Tab 默认创建终端
- [x] Tab 命名规则:
  - Local Terminal
  - Local Terminal[1]
  - Local Terminal[2]
  - ...（编号只增不减）
- [x] 新建 Tab 按钮放在最后一个 Tab 后面

#### 5. Panel 标题栏
- [x] WorkSpace 模式下显示标题栏
- [x] 标题显示对应 Tab 标签名
- [x] 字体大小: xs（超小）
- [x] 关闭按钮

#### 6. 视觉优化
- [x] 终端 Panel 使用统一背景色
- [x] 选中/未选中状态边框颜色区分
- [x] 圆角边框

### 🚧 进行中/待优化

#### 1. 功能增强
- [ ] Tab 拖拽重新排序
- [ ] Tab 合并到 WorkSpace
- [ ] 从 WorkSpace 分离 Tab
- [ ] 终端内容滚动
- [ ] 终端选择/复制

#### 2. 性能优化
- [ ] 终端内容虚拟化（只渲染可见区域）
- [ ] 减少不必要的重绘

#### 3. 用户体验
- [ ] 拖拽时的视觉反馈优化
- [ ] 分割预览动画
- [ ] 空状态提示

### 📝 技术债务

1. **TerminalElement.focus 字段未使用**
   - 位置: `src/terminal_view/terminal_element.rs:84`
   - 建议: 如果不需要焦点管理功能，可以移除

2. **代码重复**
   - `Panel::new` 和 `Panel::with_terminal` 有少量重复逻辑
   - 当前可接受，如添加更多 Panel 类型建议提取公共构造函数

### 🔧 项目结构

```
src/
├── main.rs                      # 应用入口
├── components/                  # 可复用 UI 组件
│   ├── animation.rs            # 动画组件
│   ├── split_view.rs           # 分割视图
│   └── tab_bar.rs              # 标签栏
├── panels/                      # 面板类型
│   └── terminal_panel.rs       # 终端面板（旧）
├── terminal/                    # 终端核心
│   ├── ansi.rs                 # ANSI 处理
│   ├── event.rs                # 终端事件
│   ├── mappings/               # 键鼠映射
│   ├── pty.rs                  # PTY 抽象
│   ├── pty/unix.rs            # Unix PTY 实现
│   ├── terminal.rs            # 终端核心
│   └── mod.rs
├── terminal_view/              # 终端视图
│   ├── mod.rs
│   ├── terminal_element.rs    # 终端元素渲染
│   └── terminal_view.rs       # 终端视图组件
└── workspace/                  # 工作区
    ├── mod.rs
    ├── panel.rs               # 面板组件
    ├── split_manager.rs       # 分割管理
    └── workspace.rs           # 工作区主组件
```

### 📊 代码统计

- **总文件数**: 19 个新增/修改
- **新增代码**: ~2378 行
- **删除代码**: ~129 行
- **主要提交**: `145fe36` - 终端功能和 Tab 管理改进

### 🎯 下一步计划

1. **短期（1-2 周）**
   - 实现 Tab 拖拽重新排序
   - 添加终端滚动支持
   - 优化拖拽视觉反馈

2. **中期（1 个月）**
   - Tab 合并/分离功能
   - 终端选择复制
   - 性能优化

3. **长期（可选）**
   - 支持更多面板类型（编辑器、浏览器等）
   - 会话保存/恢复
   - 插件系统

### 🐛 已知问题

1. 终端内容较多时可能有性能问题（需要虚拟化）
2. 拖拽预览在某些情况下可能不流畅

### 💡 设计决策记录

1. **使用 alacritty_terminal**: 成熟的 VTE 实现，支持完整的 ANSI 序列
2. **参考 Zed 终端实现**: 学习 GPUI 文本渲染的最佳实践
3. **累加计数器命名**: 避免 Tab 关闭后编号重复，保持用户预期
4. **Panel 作为 Entity**: 独立处理事件，支持更灵活的交互

### 📚 参考资源

- [GPUI Documentation](https://gpui.rs)
- [Zed Editor Terminal Implementation](https://github.com/zed-industries/zed)
- [Alacritty Terminal](https://github.com/alacritty/alacritty)
- [Ghostty Terminal](https://ghostty.org)

---

**最后更新**: 2026-03-07
**版本**: v0.2.0
**状态**: 功能基本完整，进入优化阶段

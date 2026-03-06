# 拖拽分割面板实现问题记录

## 当前架构问题

### 1. Panel 不是 Entity
- **问题**: 当前 `Panel` 只是一个数据结构，无法独立处理事件
- **影响**: 无法在面板级别检测拖拽，只能在工作区层面统一处理
- **解决方案**: 将 `Panel` 改为独立的 Entity，实现 `Render` trait

### 2. 无法精确检测鼠标所在面板
- **问题**: 拖拽检测在 Workspace 层面，不知道鼠标具体在哪个面板上
- **影响**: 无法准确分割鼠标指向的面板
- **解决方案**: 每个 Panel Entity 独立处理 `on_drag_move` 事件

### 3. 生命周期问题
- **问题**: `App` vs `Context` 混淆，Entity 创建时机不对
- **影响**: 编译错误，无法正确创建 Panel Entity
- **解决方案**: 在 `Context<Self>` 中创建 Entity，不是在 `App` 中

### 4. 借用冲突
- **问题**: 遍历面板时同时需要读写 Context
- **影响**: Rust 借用检查器错误
- **解决方案**: 先收集面板，再统一处理；或使用克隆

### 5. 类型不匹配
- **问题**: `match` 分支返回不同类型，`impl IntoElement` 不统一
- **影响**: 编译错误
- **解决方案**: 统一返回 `gpui::Div` 类型

## 重构计划

### 阶段 1: 创建 Panel Entity
- [x] 创建 `panel.rs`，实现 `Panel` struct
- [x] 实现 `Render` trait
- [x] 实现 `Focusable` trait
- [x] 实现 `EventEmitter<PanelEvent>`
- [x] 在 Panel 级别处理拖拽事件

### 阶段 2: 重写 SplitManager
- [ ] 修改 `SplitContainer` 使用 `Entity<Panel>`
- [ ] 修复生命周期问题
- [ ] 修复借用冲突

### 阶段 3: 重写 Workspace
- [ ] 订阅所有 Panel 的事件
- [ ] 处理分割逻辑
- [ ] 渲染 SplitContainer

### 阶段 4: 测试和调试
- [ ] 测试拖拽检测
- [ ] 测试分割预览
- [ ] 测试实际分割

## 关键设计决策

### 1. 事件传递机制
使用 GPUI 的 `EventEmitter`：
```rust
impl EventEmitter<PanelEvent> for Panel {}

// 在 Workspace 中订阅
cx.subscribe(&panel, |this, panel, event: &PanelEvent, cx| {
    this.handle_panel_event(event, cx);
});
```

### 2. 拖拽检测阈值
使用 gpui-component 的 35%/65% 阈值：
```rust
if position.x < bounds.left() + bounds.size.width * 0.35 {
    Some(PanelPlacement::Left)
} else if position.x > bounds.left() + bounds.size.width * 0.65 {
    Some(PanelPlacement::Right)
}
// ...
```

### 3. 分割预览显示
在 Panel 的 render 中直接显示预览层：
```rust
.when(self.drag_preview.is_some(), |this| {
    this.child(render_preview_layer())
})
```

## 遇到的编译错误

1. **E0425**: `cx` not found in scope
   - 原因: 在错误的地方使用 `cx`
   - 解决: 确保在正确的上下文中使用

2. **E0308**: mismatched types
   - 原因: match 分支返回不同类型
   - 解决: 统一返回 `gpui::Div`

3. **E0382**: borrow of moved value
   - 原因: Entity 被 move 后继续使用
   - 解决: 使用 `.clone()`

4. **E0500**: closure requires unique access
   - 原因: 同时借用 `cx` 多次
   - 解决: 先收集数据，再处理

5. **E0599**: no method named `new` found
   - 原因: 在 `App` 上使用 `cx.new`
   - 解决: 在 `Context` 上使用

## 参考实现

### gpui-component 的关键代码

**TabPanel 拖拽检测** (`tab_panel.rs:897-920`):
```rust
fn on_panel_drag_move(&mut self, drag: &DragMoveEvent<DragPanel>, ...) {
    let bounds = drag.bounds;
    let position = drag.event.position;

    if position.x < bounds.left() + bounds.size.width * 0.35 {
        self.will_split_placement = Some(Placement::Left);
    } else if position.x > bounds.left() + bounds.size.width * 0.65 {
        self.will_split_placement = Some(Placement::Right);
    }
    // ...
}
```

**视觉预览** (`tab_panel.rs:864-892`):
```rust
.on_drag_move(cx.listener(Self::on_panel_drag_move))
.child(
    div()
        .invisible()
        .absolute()
        .bg(cx.theme().drop_target)
        .map(|this| match self.will_split_placement {
            Some(placement) => { /* 显示预览 */ }
            None => this
        })
)
```

## 下一步行动

1. 完成 Panel Entity 实现
2. 重写 SplitManager 使用 Entity<Panel>
3. 重写 Workspace 订阅和处理事件
4. 测试完整流程

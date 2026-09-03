# Safari Liquid Glass Tab 复刻 PRD（macOS 26 Tahoe → DeepTerm 终端 Tab）

基于 WWDC25 219 Meet Liquid Glass、Apple Newsroom、9to5Mac/MacRumors 追踪，目标将 macOS 26 Safari 顶部 Tab 的 **悬浮胶囊 + 玻璃材质 + 精细动效** 完整复刻到 DeepTerm 的 `workspace/tab_bar.rs`。

## 1. 设计目标
- 终端 Tab 仅做 UI 升级，不改业务逻辑（点击/关闭/拖拽/溢出/右键菜单保持现有回调）
- 做到与 Safari 一致的 **“悬浮于内容之上、圆角胶囊、半透明可折射、顶边高光、柔和阴影”** 心智

## 2. 材料定义（Liquid Glass）
| 层级 | 实现 | 取值 |
|---|---|---|
| 外环高光 | `p(1px) + bg(white 14%) + rounded 12 + shadow_sm` | `gpui::white().opacity(0.14)` |
| 内层玻璃 | `bg(elevated_background 0.72) + border_1(white 10% dark / 60% light)` | `theme.elevated_background.opacity(0.72)` / `theme.tab_active_border` |
| 阴影 | `shadow_sm + shadow 10px rounding 10` | `CLIENT_SIDE_DECORATION_SHADOW` 已有 |
| 模糊 | GPUI 暂无 backdrop-blur，用 `opacity 0.72` 叠加模拟 |

> Apple 原话：`translucent material that reflects and refracts surroundings`，工具栏/侧边栏/Dock 均用此材质，MenuBar 完全透明以突出内容。

## 3. 几何规格
- **外容器**：`h 44`（Editor 固定），`px 8 py 4` 透明留空，使内胶囊浮起
- **内胶囊**：`h 36, rounded 12, px 6`，`flex row 居中`
- **单 Tab**：`h 30, px 12, gap 6, r 8, maxW 180, gap_1(4)`，`NEW_TAB 32, rounded 999`
- **操作胶囊**：`ml3 mr2(整体右移1) / p1 外环 + px4 py2 内 / gap2 / 按钮 26 / 图标 18 / r999`
- **间距**：Tab↔新建 `3px`，胶囊↔分割线 `2px`（右移1后），胶囊↔左侧 Tab 组 `3px`

## 4. 颜色映射（深/浅共用）
- `tab_bar_background → transparent`（外层）
- `tab_active_background / elevated_background 0.72`（活跃/胶囊基底）
- `tab_inactive_background transparent`
- `tab_hover_background`（hover 圆形/矩形覆盖）
- `tab_active_border white 10% dark / 60% light`（活跃描边）
- `border 55~58% + border_strong 55%`（胶囊/预览描边）
- `text / text_muted / icon_muted` 保持不变，活跃 `semibold`

## 5. 状态
- **默认**：非活跃透明，仅文字+图标
- **Hover（非活跃）**：`bg(tab_hover_background)` 圆角 8 覆盖；关闭按钮 `hover` 显示 `xmark.circle.fill 16`
- **Active**：`bg(tab_active) + border_1(tab_active_border)`，文字 `text semibold`
- **操作胶囊按钮**：常态 `icon_muted`，`hover: bg(tab_hover) rounded_full 26`
- **Drag**：被拖 `opacity 0`，其余 `translateX = width+4` 让位，预览 `opacity 0.75` 虚影

## 6. 动效
- 重排：`155ms easeOutQuint` 位移+透明度补间，`epoch` 驱动 `with_animation`
- Loading：`800ms easeInOut rotate repeat`，`progress.indicator`
- 拖拽预览：`12px paddingX` 测宽，居中跟随鼠标

## 7. 交互
- 左键切换、左键拖拽重排（中点判定）、中键关闭、右键菜单（Copy/Duplicate/Reconnect）、溢出横向滚动自动居中激活项、Panel 拖入插入预览

## 8. 复刻实施清单
- [x] 操作胶囊占位（3 图标、圆形 hover、玻璃环、3px/2px 间距、图标18）
- [ ] 外容器透明化 + 内胶囊悬浮化
- [ ] 单 Tab 高光/阴影微调至 Safari r10/h32 规格（可选）
- [ ] 深浅主题对比度校验

## 9. 验证
- `cargo check / cargo test tab_bar` 通过
- 深/浅主题下文字 4.5:1 对比，玻璃环在深色 white 14% 清晰、浅色下仍可见
- 44px 高度内无截断，溢出滚动正常

## 10. 参考
- Apple Newsroom - Tahoe 26 / Liquid Glass / WWDC25 219 / 9to5Mac 26.4 Compact 回归

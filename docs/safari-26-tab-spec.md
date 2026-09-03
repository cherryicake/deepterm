# Safari 26 (macOS Tahoe) Tab 标签栏 UI 交互设计规范 — DeepTerm 复刻依据

> 本文是全网检索的**研究结论 + 可直接落地的设计规格**，与 `docs/safari-liquid-glass-pills-prd.md`（实施 PRD）互补。
> 数据来源：Apple Newsroom / WWDC25 Session 219 & 323 / 9to5Mac / MacRumors / Apple Support / Apple 社区，
> 以及最关键的一手资料 —— **iTerm2 `PSMTahoeTabStyle.swift`**（gnachman/iTerm2，对 Tahoe 原生标签栏的像素级复刻，已验证 macOS 26.5）。

---

## 0. 一句话总结

Safari 26 标签栏 = **「玻璃导航层上的一排胶囊药丸」**：
标签行悬浮于内容之上（Liquid Glass），**激活 tab 是一枚亮色圆角胶囊**（带描边+微投影），
非激活 tab **完全透明**、只靠文字与细分隔线区分；hover 时浮现淡底与左侧淡色 xmark；
一切状态变化用 **spring 补间**在同一玻璃平面上 morph，绝不生硬跳变。

---

## 1. 研究来源

| 来源 | 关键结论 |
|---|---|
| Apple Newsroom (2025.06) | "Safari featuring a **rounded tab design that floats in the toolbar**"（圆角标签悬浮于工具栏）|
| WWDC25 #219 Meet Liquid Glass | 玻璃=独立导航层悬浮内容之上；小元素随背景明暗翻转；交互时 scale/bounce/shimmer；元素 morph 不瞬移；Regular/Clear 两种变体；禁止玻璃叠玻璃 |
| WWDC25 #323 Build a SwiftUI app | 工具栏自动分组浮于玻璃面；scroll edge effect 保证悬浮栏下文字可读 |
| 9to5Mac 2025.07.08 (Beta 3) | Beta 1-2 曾给非激活 tab 底部加黑色横条 → 因辨识度差被**移除**，并提高激活/非激活对比度。**不要复刻黑条** |
| MacRumors / 9to5Mac 2026.02-03 | Compact 布局（地址栏合并进激活 tab）在 26.0 被移除、**26.4 回归**；Separate（默认）= 地址栏胶囊一行 + 标签行一行 |
| Apple 社区（多帖）| 关闭按钮：hover 时出现在 tab **左侧**（替换 favicon 位）、较淡（"The x is quite faint, but it is there"）；26.0.1/26.2 有过消失 bug，属回归非设计 |
| Apple Support「Arrange tabs」| 拖拽重排/拖出成新窗/合并窗口；多选 tab（Cmd/Shift-click）后可批量拖动；溢出时双指横滑滚动 |
| How-to Geek / andesco/safari-color-tinting | "Show color in tab bar"：工具栏随当前页面 `<body>` 背景色实时取色，按 **luma** 决定文字黑/白翻转 |
| lapcatsoftware / Zed discussion #38233 | Tahoe 工具栏类窗口圆角 **26pt**；有 sidebar 等因素会改变窗口圆角 |
| **iTerm2 PSMTahoeTabStyle.swift** | 全部精确尺寸/颜色/阴影/描边数值（见下表），是对系统标签栏最完整的第三方逆向 |

---

## 2. Safari 26 顶部区域解剖（Separate 布局，默认）

```
╭──────────────────────────────────────────────────────────╮ ← 窗口圆角 26pt
│  (玻璃导航层：工具栏胶囊)                                   │
│  ◧  ◀ ▶   ┌────────────────────────┐   ⛨ ↗ ⚭ ＋ ▤     │ ← 行1：地址栏胶囊（浮起、两侧 inset）
│            │  搜索或输入网站名称      │                    │
│            └────────────────────────┘                    │
│  (玻璃延续：标签行，无独立胶囊背景)                          │
│  ▣ 维基百科  │  ▣ GitHub  │  ▣ Apple  │  ▣ 新闻 …    ＋   │ ← 行2：标签行
├──────────────────────────────────────────────────────────┤
│                     网页内容 (内容层，永不放玻璃)              │
└──────────────────────────────────────────────────────────┘
```

要点：
- **两层严格分离**（HIG 原文 "Don't use Liquid Glass in the content layer"）：玻璃只给导航/控制层；内容层永远普通材质。
- 标签行与工具栏共享同一片连续玻璃背景；激活 tab 是亮胶囊，非激活 tab 透明。
- **＋按钮固定在标签行最右端**（不随标签滚动），新建/溢出按钮为玻璃圆钮。
- 内容在玻璃下方滚动时，接触面出现 **scroll edge effect**（渐进模糊/淡出），保证玻璃上文字可读。

> DeepTerm 映射：标题栏（title_bar.rs）≈ 行1 胶囊；tab_bar.rs ≈ 行2 标签行。
> 现有 PRD 采用「整条内嵌胶囊 h36 r12」方案（Terminal.app/iTerm2 风格）——
> 若要 100% Safari 观感，应改为「标签直接坐在玻璃条上、无第二层胶囊」；两者取其一即可，不要叠加。

---

## 3. 精确视觉规格

### 3.1 几何（pt/px，来自 iTerm2 Tahoe style + Safari 实测）

| 项 | 值 | 说明 |
|---|---|---|
| 标签条总高 | **36** | `horizontalTabBarHeight = 36` |
| 标签条左右 inset | **8** | `containerSideInset = 8` |
| Tab 胶囊高度 | **25–26** | 在 28px 可视带内 y+2、h−3（视觉居中，上留 2px 呼吸）|
| Tab 胶囊圆角 | **10–11.5** | `barRadius(14) − 2.5`；接近胶囊但保留约 2px 平边 |
| Tab 内边距 | leading 10 / trailing 12（≈12/12 亦可）| iTerm: 6 与 6+8；Safari 观感对称 10–12 |
| Tab 间距 | **1–2** | `intercellSpacing = 1`；相邻非激活 tab 之间画 1px 分隔线 |
| Tab 最大宽度 | ≈ 220 | 激活 tab 始终全宽显示标题 |
| Tab 最小宽度 | ≈ 84 → 40 | 先压缩出标题截断，再 favicon-only，最后横滑滚动 |
| 关闭按钮 | 热区 **15×15**，xmark 字形 **8pt Medium** | hover 圆底 15px `white 10%`，按下 `white 27%` |
| 新建按钮 | **28×28** 圆形玻璃钮（bezel glass + circle）| plus 字形居中 |
| 溢出按钮 | 28×28 圆形玻璃钮 | ellipsis 字形 |
| 字号 | 13px（iTerm 用 11pt，系统 Terminal ≈13pt）| 系统字体 |
| 窗口圆角 | 26pt（工具栏类窗口）| DeepTerm CSD 现为 10px，可提至 12–16 折中 |

### 3.2 颜色 — 暗色（iTerm2 Tahoe Dark 精确值）

| 元素 | 值 |
|---|---|
| 标签条玻璃底 | `#2B2E30`（displayP3 0.17/0.18/0.19），窗口底 `#2D3032` |
| **激活 tab（窗口聚焦）** | `#626466`（98/100/102）|
| **激活 tab（窗口非聚焦）** | `#535557`（83/85/87）|
| 非激活 tab | **transparent**（透出玻璃底）|
| hover（非激活）| `#363638`（0.21/0.21/0.22）|
| 激活文字（聚焦）| `#EFEFEF` |
| 非激活文字（聚焦）| `#7E8081`（126/128/129）|
| 激活文字（非聚焦窗）| `#7E7F80`；非激活 `#777777` |
| 分隔线 | `#454A4C`（0.271/0.292/0.301）|
| 激活描边 | 白 ≈8–12%（暗色用左/右端帽图 + 中段描边模拟玻璃受光边）|

### 3.3 颜色 — 亮色

| 元素 | 值 |
|---|---|
| 标签条玻璃底 | `#E6E6E6`（white 0.9），窗口底 `#E1E1E1`（225）|
| **激活 tab** | `#F7F7F7`（white 0.97）+ **白色描边** |
| 非激活 tab | transparent |
| hover | `#DBDBDB`（0.86）|
| 文字（窗口聚焦）| `#464646`（激活/非激活同色，靠胶囊区分；Safari 实际非激活更灰 ≈`#85858D`）|
| 文字（非聚焦窗）| 激活 `#B1B1B1`，非激活 `#A1A1A1` |
| 分隔线 | `#D1D1D1`（0.82）|

### 3.4 材质分层（Liquid Glass 落地配方）

```
激活胶囊（自下而上）：
  1. 底色      #626466 (dark) / #F7F7F7 (light)
  2. 顶边高光   1px inner-top white 10–14%（玻璃上缘受光）
  3. 描边      1px white 10% (dark) / white 60% (light)
  4. 环境投影   顶/底各 2px black 2.7% / 3.5%，左右端帽渐变（16px、峰值 7%）
非激活：
  无底色、无描边；hover 才出现淡底（120ms crossfade）
```

- GPUI 无 backdrop-blur → 用 `opacity 0.72–0.9` 半透明叠加模拟（现 PRD 方案可用）。
- Reduce Transparency（无障碍）降级：不透明灰底（`#2B2E30`/`#E6E6E6` 原样）。
- "Show color in tab bar" 等价物：标签条可随**激活终端会话的背景色**取色，按 luma 翻转文字黑白（`tabColorBrightness < 0.5 → 白字`）。

### 3.5 分隔线规则（细节成败点）

- 仅当 **tab 总数 ≥ 3** 时绘制；
- 只画在**两个非激活 tab 之间**（任一侧是激活 tab 或 hover tab 则跳过）；
- 1px 宽、垂直上下各 inset 5px；
- 激活 tab **最后绘制**（z 序置顶），保证投影不被邻 tab 盖住。

---

## 4. 状态矩阵

| 状态 | 背景 | 文字 | 左槽（icon 槽）| 描边 |
|---|---|---|---|---|
| 非激活 | transparent | `#7E8081` regular | favicon 16px | 无 |
| 非激活+hover | `#363638` | 同上微亮 | **淡 xmark 替换 favicon**（crossfade）| 无 |
| **激活** | `#626466` | `#EFEFEF` regular（**不加粗**，靠颜色区分）| favicon；加载中换 spinner | white 10% + 顶边高光 + 微投影 |
| 激活（窗口非聚焦）| `#535557` | `#7E7F80` | favicon | 同上减弱 |
| 拖拽中（源）| 原位隐藏/半透明 | — | — | 拖影：胶囊+阴影+0.96 透明度跟随光标 |
| 拖入预览 | — | — | — | 1px `border_strong 55%` 虚位胶囊 |

> Safari 激活 tab 文字**不做 semibold**（与现实现不同）；权重统一 regular，对比全靠颜色+胶囊。
> 修改态（modified 圆点）是 DeepTerm 自有扩展，保留：置于标题前 6px 圆点，非激活 70% 透明。

---

## 5. 交互动效规格

所有动效原则（WWDC25）：**同一玻璃平面上的 morph**——元素变形滑移而非消失重现；spring 优先；交互瞬间元素"从内部点亮"（scale 1.02–1.05 + 高光增强）。

| 交互 | 规格 |
|---|---|
| 切换激活 | 胶囊高亮 **原地 crossfade 150–200ms**（不做滑块位移）；文字色同步过渡 |
| Hover 进/出 | 淡底 120ms crossfade；xmark↔favicon 图标交叉淡化 100–150ms |
| 新建 tab | 新胶囊自 ＋ 按钮处 **宽度 0→目标 spring 展开 ~250ms**（轻微过冲），邻 tab 左移让位；内容 50% 进度后淡入 |
| 关闭 tab | 胶囊 **宽度收敛+fade ~200ms**，邻 tab spring 补位；关闭的是激活 tab 时先切焦点：**优先右侧，无则左侧** |
| 拖拽重排 | 源 tab 抬起（投影加深、scale≈1.03）跟手；其余 tab `easeOutQuint ~155–200ms` 让位（现实现已达标）；松手 settle 回弹 |
| 拖出标签条 | 垂直阈值越界 → tear-off（Safari 拖出成新窗口；终端可映射为独立窗口/分屏，二期）|
| 溢出 | 依次压缩：标题截断→84px；favicon-only→40px；仍放不下进入横滑滚动（现实现只有滚动，**缺压缩分级**）；＋按钮恒固定 |
| Loading | spinner（800ms 旋转）替换 favicon，完成后 fade 回 favicon |
| 激活 tab 滚入视野 | 溢出时最小滚动策略（现实现已达标）|

### 键盘 / 指针

| 输入 | 动作 |
|---|---|
| 左键 | 激活（并阻止冒泡到标题栏拖窗，现实现已达标）|
| 中键 | 关闭（现实现已达标）|
| 右键 | 菜单：新建 / 关闭 / 关闭其他 / 关闭右侧 / 复制地址 / 复制标签页 / 重连（对齐 Safari 菜单措辞）|
| Cmd+1..9 / Cmd+Shift+[ ] / Cmd+W / Cmd+T | 终端侧对应键位映射 |

---

## 6. 与当前 `tab_bar.rs` 的 Diff 对照

| 项 | 当前值 | 目标值（Safari 26）|
|---|---|---|
| `TAB_HEIGHT_PX` | 30 | **26**（外条 36，pt_2/pb_2 呼吸）|
| `TAB_RADIUS_PX` | 8 | **10** |
| `TAB_PADDING_X_PX` | 12 | 12（leading 10 / trailing 12 亦可）|
| tab 间 `gap_1` | 4px | **2px** + 条件分隔线 1px |
| `TAB_MAX_WIDTH_PX` | 180（仅非激活限宽）| ≈220，激活/非激活一致；新增**压缩分级** 84/40px |
| 激活背景 | `#2C2C2E`（macOS26 Dark 主题）| `#626466` / 非聚焦 `#535557` |
| 非激活背景 | `#1C1C1E` | **transparent** |
| hover 背景 | `#252527` | `#363638`（暗）/`#DBDBDB`（亮）|
| 激活描边 | `#FFFFFF1A` border_1 | 保留 + **顶边 1px 高光线** + 底部 2px 投影（`shadow_sm`）|
| 激活文字 | `text` + **SEMIBOLD** | `#EFEFEF` + **NORMAL**（去粗体）|
| 非激活文字 | `text_muted` | `#7E8081`（暗）/≈`#85858D`（亮）|
| 关闭按钮 | `XmarkCircleFill` 16px | **裸 `xmark`** 字形 8pt、热区 15px、hover 圆底 white10%、按下 27% |
| 新建按钮 | 32px 圆 | **28px** 玻璃圆钮（`shadow_sm`+白 10% 描边模拟 glass bezel）|
| 分隔线 | 无 | **新增**（§3.5 规则）|
| 窗口焦点响应 | 无 | 激活胶囊随 window focus 变暗（`#626466→#535557`）|
| 新建/关闭动效 | 无（瞬变）| 新增 width+fade spring（§5）|
| 重排动效 | 155ms easeOutQuint ✅ | 保持（可延至 200ms）|
| 中键关闭 / tooltip / 拖拽预览 / panel drop ✅ | 已达标 | 保持 |

主题 JSON（`assets/themes/builtin.json` 两个 macOS 26 主题）同步建议：

```jsonc
// macOS 26 Dark
"tab_bar_background":    "#2B2E30",   // 或保持 #1C1C1E 作玻璃底，胶囊亮色对比更接近终端审美
"tab_active_background": "#626466",
"tab_inactive_background": "#00000000",  // 完全透明
"tab_hover_background":  "#363638",
"tab_active_border":     "#FFFFFF1A"
// macOS 26 Light
"tab_bar_background":    "#E6E6E6",
"tab_active_background": "#F7F7F7",
"tab_inactive_background": "#00000000",
"tab_hover_background":  "#DBDBDB",
"tab_active_border":     "#FFFFFF99"
```

> 注：`tab_inactive_background` 置透明后，玻璃底即 `tab_bar_background`；若嫌 `#626466` 胶囊在深色终端里过亮，
> 可回落 `#3A3A3E` 一档——Safari 原值偏亮是因为真实玻璃有页面内容透色参与调和。

---

## 7. 实施分期

**P0（视觉核心）— ✅ 已实施**
1. ✅ 几何常量、裸 xmark 关闭钮、激活胶囊高光/投影、非激活透明、条件分隔线、主题字段（`tab_active_pill`/`tab_divider`）

**P0.5（Safari 标签布局模型）— ✅ 已实施**（重新分析 Safari 实际行为：选中标签始终全尺寸、
非选中向边缘渐小 "tabs get smaller towards the edge"、Safari 不横向滚动）
1. ✅ **外层胶囊轨道（Safari 式饱满胶囊）**：高 36pt（44pt 标题栏内上下各留 4pt），
   紧贴包裹全部标签、随数量伸缩；＋按钮紧随其后；
   仅一层柔和底色井（无描边/高光边，Safari 同源）：默认 dark white 8% / light black 6%
   （字段 `tab_track_background`）
2. ✅ **非激活标签无胶囊形状（Safari）**：纯透明，仅靠分隔线隔开（窄于 favicon-only
   阈值 48pt 的标签间不画线）；hover 用自适应淡色（text 5%）；仅激活标签显示胶囊；
   favicon-only（sliver）标签不显示关闭钮（点击激活后再关）
3. ✅ **内容自适应 hug**：自然宽 = 文本实测 + chrome（非激活上限 220 / 激活 400）
4. ✅ **阶梯扇形（Safari 核心行为）**：空间不足时激活标签保宽
   （min(自然宽, 可用宽-其余×sliver)，尽可能显示完整标题），非激活按离激活的
   距离线性递减权重（k, k-1, …, 1）向左右两侧阶梯堆叠：靠近的宽、远端窄，
   下限 sliver 14（favicon-only 极限，超出侧空间时 sliver 之上等比收缩）
5. ✅ **触控板横滑展开**：阶梯模式下左滑展开（0..1 向等宽布局插值，堆叠的边缘标签
   变宽方便选中）、右滑收起；指针离开标签栏后动画收起；hug 模式下无效
6. ✅ **不滚动**：Safari macOS 标签栏不横向滚动（溢出直接堆叠压缩）
7. ✅ **统一动效**：槽位 x/宽度/透明度 180ms easeOutQuint（一动全动）；新标签 0 宽长出
8. ✅ 拖拽重排/panel 拖入预览基于槽位；文本宽度 shape_line 实测
9. ✅ **关闭钮交互（Safari 经典）**：hover 时图标槽直接换成裸 xmark
   （同宽 16pt 互换、标题不跳动，优先于 loading）

> 配色（macOS 26 主题，与窗口背景和谐的层递关系 窗口<轨道<胶囊）：
> Dark 窗口 #1C1C1E → 轨道 #2E2E30（white ~8% 玻璃井）、激活胶囊 #44444A、
> 分隔线 #3C3C42；Light 窗口 #F5F5F7 → 轨道 #E9E9EB、胶囊 #FFFFFF、分隔线 #D6D6DA。
> 未声明 `tab_active_pill` 的主题回退到自身 `tab_active_background`（尊重主题设计）。

> 量化参考（原生/ghostty）：轨道 28pt、胶囊内缩 2pt、最小槽宽 100pt、sliver 14pt、
> 每侧最多 4 层、动效 snappy 180ms；分割发丝线 1×14pt。
>
> **踩坑记录（宽度坍缩）**：标签轨道内全是绝对定位子元素（无流内内容宽度），
> 而标题栏中承载 TabBar 的容器原本是内容自适应宽度（`flex_shrink(1)` 无 grow）——
> 旧实现靠标签流内内容撑开，新模型下整条宽度链坍缩只剩＋按钮。
> 修复：标题栏 tab 区 `flex_grow(1)` 主动生长，拖拽区在有标签栏时限宽 `max_w(200)`
> 让胶囊轨道占据主导（Safari 式近全宽）；轨道宽度由 paint 阶段 canvas 回填。
>
> **踩坑记录（文字溢出胶囊）**：`render_tab_label` 沿用了旧 flex 布局的
> `flex_none + max_w(220)` 固定宽度——等宽槽位下槽宽可能远小于 220（多标签压缩时），
> 文字不收缩直接溢出胶囊。修复：标签容器 `flex_1 + min_w_0` 随胶囊剩余宽度自适应收缩，
> 胶囊本体加 `overflow_hidden` 兕底裁剪；槽位宽度本身就是上限，不再依赖固定 max。

**P1（动效）**
5. 激活切换胶囊高光瞬切（原生无 crossfade，仅位置/宽度动画——槽位模型已实现位置动画）
6. hover 图标交叉淡化

**P2（结构/高阶）**
7. 溢出时轨道两侧渐变提醒（原生边缘 fade 指示）
8. 窗口 focus 变暗联动
9. 激活会话背景取色 luma 翻转（"Show color in tab bar" 等价物）
10. 拖出 tear-off（映射为拆分独立窗口）
11. Reduce Transparency 无障碍降级

## 8. 验收清单

- [ ] 激活/非激活一眼可辨（beta 1-2 黑条教训：对比度必须拉满）
- [ ] 激活 tab 投影不被邻 tab 覆盖（z 序最后绘制）
- [ ] 分隔线仅在 ≥3 tab 且两侧均非激活/hover 时出现
- [ ] 深浅两套主题下文字对比 ≥4.5:1；胶囊亮色 `#626466` 上 `#EFEFEF` 文字达标
- [ ] 重排/新建/关闭三条动效无跳变、无布局抖动
- [ ] 中键关闭、tooltip、panel drop、右键菜单回归通过
- [ ] `cargo check` / `cargo test tab_bar` 通过

## 9. 参考

- Apple Newsroom: macOS Tahoe 26 发布稿（Safari "rounded tab design that floats in the toolbar"）
- WWDC25 Session 219 *Meet Liquid Glass* / Session 323 *Build a SwiftUI app with the new design*
- 9to5Mac: *macOS Tahoe 26 beta 3 makes tabs less confusing*（黑条移除始末）
- MacRumors: *Compact Safari Tab Bar Returns in macOS Tahoe 26.4*（Separate/Compact 布局现状）
- gnachman/iTerm2 `ThirdParty/PSMTabBarControl/source/PSMTahoeTabStyle.swift`（全部精确数值的出处）
- uncSoft/Tabberwocky `docs/custom-document-group-tab-styling.md`（NSTabBar/NSGlassEffectView 私有层级，佐证 tab=玻璃+tint 结构）
- andesco/safari-color-tinting（tab bar 取色与 luma 翻转机制）

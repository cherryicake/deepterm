# Markdown 全语法渲染展示 — DeepTerm Typora/Notion 风格 · v2

> [!TIP]
> 本文档验证 DeepTerm Markdown 预览的 **Tier 1 + Tier 2** 全部美化效果。直接用 DeepTerm 打开 `markdown-showcase.md`（非 `#edit` 模式）即可预览。亮/暗主题均已适配。

---

## 1. 标题层级 · 含悬停锚点

> 标题采用 Notion 节奏：`H1 1.875rem / H2 1.5rem / H3 1.25rem`，全部 `BOLD` 且行高收紧。鼠标悬停标题时右侧出现 `#` 锚点（`group-hover`），H1/H2 保留淡分割线。

# H1 — 一级标题：文档主标题，带底部分割线
## H2 — 二级标题：章节标题，同样带分割线
### H3 — 三级标题：小节标题
#### H4 — 四级标题
##### H5 — 五级标题（字号收缩，字重 600）
###### H6 — 六级标题（弱化为 muted）

---

## 2. 段落与行内样式

这是一段普通段落，用于验证 **15px / 1.75 行高** 的阅读体验。DeepTerm 会将超长段落自动折行，并在段间保留 `my_3` 的呼吸感。中文与 English 混排时基线对齐，无 `items_center` 偏移。

这是第二段，演示 *斜体 Emphasis*、**加粗 Strong**、***粗斜体***、~~删除线 Strikethrough~~，以及 `行内代码 inline code` 的 Notion 胶囊效果（`text 6%/12% 淡底 + 13px 等宽`）。混合示例：**加粗内含 `代码` 与 *斜体***。

- 行内代码相邻：`cargo check`、`cargo test`、`pulldown-cmark`
- 智能标点：-- 破折号、... 省略号、"引号"、'单引号'

软换行与硬换行：  
第一行结尾两个空格硬换行  
第二行紧接显示。软换行则由空格合并为单个空格。

---

## 3. 引用块与 Callout

> 这是一条普通引用。Notion 风格：左侧 3px 重边框 + 淡底 `text 3%` + 圆角 6px + 斜体 `0.78` 透明度。支持多段落，第二段验证 `my_3 / py_3` 的卡片内边距。

> 嵌套引用：
> > 二级引用验证 `blockquote_depth` 叠加。边框保持 3px，背景淡底叠加。

### 3.1 GitHub Alert Callout（已完成 Tier 2）

> [!NOTE]
> 蓝色 **NOTE** 卡片：`accent` 左边框 + `accent 7%` 淡底 + 顶部 `● NOTE` 12px 700 标题。用于一般提示。

> [!TIP]
> 绿色 **TIP** 卡片：`success` 强调，适合技巧与最佳实践。

> [!IMPORTANT]
> 紫色 **IMPORTANT** 卡片：`#8250DF` 强调，关键信息。

> [!WARNING]
> 橙色 **WARNING** 卡片：`warning` 强调，潜在风险。

> [!CAUTION]
> 红色 **CAUTION** 卡片：`error` 强调，危险操作。支持同行标题写法：`> [!CAUTION] 行内标题后紧接正文`，解析时自动分离 marker 与余下文本。

> [!NOTE] 行内标题写法：同一行内 `> [!NOTE] 这是标题余下正文` 也能正确识别，余下文本会保留在卡片内。

---

## 4. 列表

### 4.1 无序列表

- 一级无序项 `•`，缩进 `pl_6`，间距 `gap_2 / mb_2`
  - 嵌套二级列表验证递增缩进
    - 三级嵌套
  - 返回二级
- 另一一级项，验证 `line_height 1.75` 与项目符号 `text_muted` 弱化

### 4.2 有序列表

1. 第一项，有序数字 `1.` 自动递增，`22px` 固定宽度
2. 第二项
3. 第三项
   1. 嵌套有序 1
   2. 嵌套有序 2
4. 第四项

### 4.3 任务列表 Task List（已完成 Tier 2）

- [x] 已完成任务 — 自定义 16px 圆角 4 勾选框（`success` 实心 + 白色 `✓`），文字 `muted + 删除线 0.6 透明度`
- [ ] 未完成任务 — 空心方框 `text_muted 35%` 边框
- [x] 另一个已完成示例
- [ ] 包含 **加粗** 与 `代码` 的任务（勾选后整行进入删除线样式）
- [ ] 嵌套任务：
  - [x] 子任务已完成
  - [ ] 子任务未完成

---

## 5. 代码块 Code Blocks

> 卡片化：`rounded 8 / border 0.2 / bg editor_background / padding 14-16 / 13.5px 1.6 行高`。顶部左侧为语言徽标 `pill 10px 600`，右上角为 `Wrap / NoWrap` 与 `复制` 按钮（`group-hover` 可见，`shadow_sm`）。

### 5.1 带语言的围栏代码块

```rust
// Rust — 验证 tree-sitter 高亮 + Wrap 切换
use gpui::{div, px, rems};

fn main() {
    let style = MarkdownStyle::themed(window, cx);
    println!("Hello, DeepTerm! 1.75 行高 + 13.5px 等宽");
    // 点击右上角 Wrap 可在 横向滚动 ↔ 自动换行 间切换
}
```

```python
# Python — 语言徽标应显示 PYTHON
def highlight_code_runs(content: str, language: str):
    highlighter = SyntaxHighlighter(language)
    return highlighter.styles(content)
```

```javascript
// JavaScript — 复制按钮 hover 可见，2s 内显示 ✓ success
async function copyCode(id) {
  await navigator.clipboard.writeText(document.getElementById(id).innerText);
  console.log("copied!");
}
```

```bash
# Bash — 横向滚动验证
cargo check --message-format=short | head -n 100
cargo test -- --nocapture
```

```json
{
  "name": "deepterm",
  "markdown": {
    "style": "notion",
    "max_width": 780,
    "line_height": 1.75
  }
}
```

### 5.2 不带语言的普通代码块

```
plain text code block
no language specified
should still have card style but no language pill
```

### 5.3 超长行代码块（验证 Wrap 按钮）

```rust
let very_long_line = "This is a very long line that should trigger horizontal scroll when Wrap is OFF (overflow_x_scroll), and should softly wrap to the next line when you click Wrap → NoWrap, without breaking the card layout — DeepTerm code-block max_w 780 centered narrow column.".to_string();
```

---

## 6. 表格 Table（已美化：横向滚动 + 斑马纹）

> 外层 `rounded 8 / border 0.22 / bg panel 0.5 / overflow_x_scroll` + 内层 `grid`，表头 `elevated_bg`，偶数行 `panel_bg`，单元格 `px_3 py_2 14px`，支持左/中/右对齐。

| 功能 | 样式 | 状态 |
| :--- | :---: | ---: |
| 标题层级 | 加粗 + 1.875rem + 锚点 # | ✅ |
| 代码块 | 卡片 + 复制/Wrap | ✅ |
| 引用/Callout | 左 3px + 淡底 + 彩色 | ✅ |
| 表格 | 圆角 8 + 滚动 + 斑马纹 | ✅ |
| 图片 | 圆角卡片 + 真图/占位 | ✅ |
| 任务列表 | 自定义勾选 + 删除线 | ✅ |
| 链接 | accent + ↗ 外链图标 | ✅ |

### 6.1 对齐验证

| 左对齐 | 居中 | 右对齐 |
| :--- | :---: | ---: |
| left | center | right |
| `code` | **bold** | *italic* |
| 123 | 456 | 789 |

### 6.2 超宽表格（验证横向滚动）

| 列 A 很长很长 | 列 B | 列 C | 列 D | 列 E | 列 F | 列 G |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| 当表格总宽度超过 780px 时，外层容器应出现横向滚动条而非挤压换行 | 2 | 3 | 4 | 5 | 6 | 7 |
| 第二行同理 | b | c | d | e | f | g |

---

## 7. 分割线与链接

分割线（`---`）应为 1px 柔和实线，上下 `my_6`，`rounded_full`：

---

### 7.1 链接 Links（已美化：外链 ↗）

- 普通链接：[DeepTerm GitHub](https://github.com) — 应显示 `↗` 11px 60% 透明度
- 带标题链接：[GPUI 官方](https://www.gpui.dev "hover title")
- 裸 URL：https://example.com
- 站内锚点占位：[跳到标题](#1-标题层级-headings)

---

## 8. 图片 Images（已完成 Tier 2）

> 本地路径（如 `assets/icon.png`）会尝试 `fs::read` 并用 `image_viewer::load_image_from_bytes` 渲染真实图片（`max_w 640` 等比）；远程 `http(s)` 链接显示占位卡片，点击卡片任意位置 `open::that` 外部打开。

![DeepTerm Logo](assets/icon.png)
*↑ 本地图片 `assets/icon.png` — 若文件存在则渲染真实图片，否则回退为 🖼 占位卡片*

![远程图片示例](https://via.placeholder.com/640x360.png?text=Remote+Image)
*↑ 远程图片占位卡片：`elevated bg + 🖼 + alt + url`，点击外部打开*

![带 alt 的图片](https://example.com/not-found.png "Title text")
*↑ 失效图片同样为占位卡片，下方另有 Notion 风格 caption 行*

---

## 9. 行内混合与转义

验证 **加粗 *嵌套斜体* 与 `代码` 混合**、~~删除线内 **加粗**~~、以及转义 `\*不是斜体\*`、\`不是代码\`。

`行内代码` 与 **加粗 `代码` 嵌套**、*斜体 `代码`* 均应保持胶囊淡底而不断行。

---

## 10. 脚注 Footnote

这是一句带脚注的正文[^1]，用于验证 `ENABLE_FOOTNOTES`。脚注定义在文档末尾。

[^1]: 这是脚注内容，当前 parser 已开启但尚未独立渲染样式，未来将置底为小字 muted 块。

---

## 11. HTML 透传占位

<div style="color: red">这是内联 HTML 块，当前 `Html` 事件被忽略，未来可选择性透传或转义。</div>

---

## 13. 公式 Math（Typora 风格）

> 行内 `$...$` 斜体显示、分隔符隐藏；独占行 `$$...$$` 居中并放大 `1.12` 倍；多行 `$$` 块每行居中。光标点进公式行时整块切回源码态（`$$` 分隔符现身），点别处恢复渲染。KaTeX 级真排版（如 `\frac` 分数展开）暂为近似渲染：内容斜体居中，不做 TeX 展开。

行内公式：质能方程 $E = mc^2$ 就该长这样，勾股定理 $a^2 + b^2 = c^2$ 也是。货币 $5 和 $10 不会被误判为公式，`$x$` 写在代码里保持原样，\$转义后同样是普通字符。

单行块公式：

$$E = mc^2$$

多行块公式（`$$` 独占一行开合）：

$$
\sum_{i=1}^n i = \frac{n(n+1)}{2}
\int_0^\infty e^{-x} dx = 1
\sqrt{a^2 + b^2} = c
$$

---

## 15. 大纲与字数（实时编辑器功能测试点）

> 非渲染语法：用 `#live` 实时模式打开本文档测试。底部状态条显示 `行/列 + 共 N 字 · M 行`（选中文字追加 `已选 N 字`）；右下角 `大纲` 按钮或 `Cmd+Shift+O` 开关右侧大纲栏，点击标题跳转对应章节并保持焦点，当前章节高亮为主题色。

### 15.1 大纲层级样本

#### 15.1.1 四级标题样本

##### 五级标题样本

###### 六级标题样本（最深）

## 15.2 回到二级标题（验证高亮切换）

> 测试步骤：打开大纲 → 点击 `15.1` 跳转 → 光标行章节高亮跟随；选中一段文字看 `已选` 计数；`Cmd+Shift+O` 收起大纲栏。

---

## 16. 表格编辑（实时编辑器功能测试点）

> 用 `#live` 实时模式测试（按 Typora 语义）：**点击任一单元格即可在格内直接打字**（表格保持网格渲染；光标所在的那个行内元素才展开它的标记，例如 `**粗体**` 里的 `**`，其余内容与其他格保持渲染）；`Tab` / `Shift+Tab` 在单元格间跳转，末尾单元格 `Tab` 自动追行；`Enter` 移到下方同列格（末行则退出表格），`Shift+Enter` 插入 `<br />`，`Cmd/Ctrl+Enter` 在当前行下方插入空行；`↑`/`↓` 按**列索引**换行（越界退出表格），`←`/`→` 在格边界跳到相邻格；`Cmd/Ctrl+A` 只选当前格内容；`Esc` 不退出表格（只关弹窗/清选区）。

| 姓名 | 年龄 | 城市 |
| :--- | ---: | :---: |
| Alice | 30 | 北京 |
| Bob | 25 | 上海 |

> 测试步骤：
> 1. 点进 `Alice` 单元格 → 直接打字，确认**表格没有变成源码行**，只有该格内容变化；
> 2. 点进含行内标记的格（如 `**粗体**`、`` `code` ``）→ 只有该元素展开标记，移出后恢复渲染；
> 3. `Tab` 跨格、末格 `Tab` 追行；`Enter` 到下方同列格；`↑`/`↓` 保持列位置；
> 4. `Cmd+A` 只选中当前格内容（不是整行含管道）；
> 5. 含转义管道的格（如 `a\|b`）仍然只算一格；
> 6. 编辑过的表格**不会**被自动重排格式（与 Typora 一致：只有显式"prettify"才重排）。

---

## 17. 查找替换（实时编辑器功能测试点）

> 用 `#live` 实时模式测试：`Cmd+F` 打开与代码编辑器同款的查找面板（自动预填选中词，同一套匹配引擎与 UI），`Enter` / `Shift+Enter` 在命中之间跳转，`Cmd+G` / `Cmd+Shift+G` 全局可用，`Esc` 关闭；`Aa` 区分大小写、`ab` 全字匹配、`.*` 正则（非法正则红色提示 `正则无效`）；展开 `替换` 行可逐个 `替换` 或 `全部`（单步撤销，另有 `Aa` 保留大小写）。只读模式仅可查找。

银河漫游指南是一本奇书，银河系的旅行者人手一册银河漫游指南。
第二遍出现银河漫游指南，第三遍银河漫游指南藏在段尾银河漫游指南。

> 测试步骤：选中 `银河漫游指南` → `Cmd+F`（查询框已预填）→ `Enter` 逐个跳转（当前命中主题色高亮）→ 点 `.*` 输入正则 `银河.{2}指南` 验证匹配 → 展开替换行把 `银河` 换成 `星系` 点 `全部` → `Cmd+Z` 一次撤销。

---

## 18. 图片粘贴拖拽（实时编辑器功能测试点）

> 用 `#live` 实时模式测试：截图后 `Cmd+V` 直接落盘为 `<本文档>.assets/image.png` 并插入 `![](...)`（独占行渲染为图片卡片）；从 Finder 拖入或拷贝图片文件同理，非图片文件不受影响；第二次粘贴自动顺延 `image-1.png` 不覆盖。

![占位示例](assets/icon.png)

> 测试步骤：任意截图 → 回到 live 文档 `Cmd+V` → 出现图片卡片 → 同目录出现 `markdown-showcase.assets/image.png` → 再粘一次得 `image-1.png`。

---

## 19. 链接增强（实时编辑器与预览测试点）

> 自动链接与裸链在两端都可点击：尖括号自动链接、裸 `https` 链接、`www.` 链接自动补 `https://`；引用式链接文档级解析（定义行保留为正文）。

尖括号自动链接 <https://www.rust-lang.org> 与裸链 https://github.com 结尾标点自动剥离。

www.example.com 自动补全协议。

[引用式全写][ref-demo]、[折叠空标][ref-demo][]、[快捷引用][ref-demo] 三种写法同一定位。

[ref-demo]: https://typora.io

> 测试步骤：live 模式点进链接行切源码（标记显现），光标移出恢复隐藏；预览点击外链出现 `↗`；未定义的 `[nope]` 保持原文。

---

## 20. 行内标记（实时编辑器与预览测试点）

> 上标 `^2^` 与下标 `~2~` 主题色区分（分隔符隐藏，gpui 单行单字号无基线升降）；`:tada:` 类短码预览出字形、live 主题色标出；`[^note]` 脚注引用主题色芯片。

质能方程 E=mc^2^ 与水 H~2~O 与 X^a b^ 多字上标。

庆祝 :tada: 与 :rocket: 与未知 :nope: 保持原文，行内代码 `:tada:` 不展开。

脚注引用见 [^note]，`~~删除~~` 双波浪仍是删除线，`^` 行尾单个不成对保持原文 ^ 。

[^note]: 这是脚注定义行，两端都按普通段落渲染。

> 测试步骤：live 点进行内标记切源码看分隔符显现；预览检查 `:tada:` 已是 🎉 且代码段内短码原文。

---

## 21. 块语法（实时编辑器与预览测试点）

> `[TOC]` 独占段落在预览展开为标题大纲（live 用右侧大纲栏交互）；HTML 块钝化展示源码；4 空格缩进代码两端一致；`冒号` 定义列表术语加粗、释义缩进。

[TOC]

<div class="note">
这段 HTML 只读展示不执行
</div>

    缩进代码块在 live 显示为等宽行

Term A
: Definition of A goes here

Term B
: First definition of B
: Second definition of B

> 测试步骤：预览检查目录含本节标题且释义缩进；live 中 `: Definition` 行隐藏冒号、`    缩进代码` 为等宽、`[TOC]` 行保持原文（用大纲栏导航）。

---

## §22 编辑辅助：源码模式 / Slash / 自动配对（live 模式）

> 测试步骤：live 模式按 `Cmd+/`（或点状态条"源码"芯片）全文档切源码标记；
> 空行行首输入 `/表` 用上下键选择、回车插入；输入 `(` `[` `"` 自动配对，
> 退格一次删一对；mermaid 块渲染为真实图形，卡片左上角「预览 | 源码」按钮切换。

### Slash 演示：空行输入 `/链` 插入链接

常用查询：`/标题` `/列表` `/表格` `/代码块` `/引用` `/分隔`，`Esc` 忽略后继续输入恢复。

### 自动配对演示

括号（示例）、方框[示例]、引号"示例"、代码 `示例`：输入左半自动补右半并选中包裹，退格一次删一对。

### Mermaid 演示（真实图形 + 预览/源码按钮）

```mermaid
flowchart TD
    A["改 my_policy"] --> B{模型可用?}
    B -->|"否"| B1["换端点 / 等配额"] --> B
    B -->|"是"| D["离线自检"] --> E{通过?}
    E -->|否| A
    E -->|是| F["构建 → 部署 → 评测"] --> G[保留或回环]
```

> 测试步骤（live 模式，逐项核对）：
> 1. 上式渲染为**真实图形**（节点按 accent/success/warning/git 蓝轮换配色，随当前主题自动换色）；
> 2. 卡片左上角「预览 | 源码」两个按钮：点**源码**才可编辑源码行（光标/点击无法进入预览模式的块内部），点**预览**回到渲染；
> 3. 源码改动约 0.2s 后后台重渲染，期间旧图呼吸提示、不闪回源码；
> 4. 切换亮/暗主题，图形配色随之更新（缓存键含主题指纹）；
> 5. 语法写坏 → 卡片回退源码并在 footer 给出失败原因，编辑修好即恢复；
> 6. sequenceDiagram / classDiagram / gantt / pie / xychart 等全类型同理（无白名单，检测不出即回退源码）。

---

## 14. 尾声 · 验证清单

> 若平滑滚动至此且所有色块、间距、圆角在亮/暗主题下保持一致，说明 Typora/Notion 风格的 Tier 1 + Tier 2 美化已生效。

**请逐项验证：**

1. 标题悬停出现 `#`，H1/H2 底线 `border 0.22` 淡
2. 普通引用 vs 5 种 Callout 彩色卡片（NOTE/TIP/IMPORTANT/WARNING/CAUTION）区分明显
3. 任务列表：空心/实心勾选框 + 已完成删除线
4. 代码块：语言 pill、`Wrap ↔ NoWrap` 切换、`复制 → ✓` 2s 反馈
5. 表格：横向滚动、斑马纹、单元格 `14px`
6. 链接：外链 `↗` 仅对 https/http 出现
7. 图片：本地 `icon.png` 真图、远程占位、点击外部打开
8. 公式：行内斜体、`$$` 居中块、货币 `$5` 不误判、点进公式切源码
9. 大纲字数（live 模式）：底部字数行数、选区计数、大纲开关跳转高亮
10. 表格编辑（live 模式，Typora 语义）：格内直接编辑（点格打字不退回源码）、行内元素级标记展开、Tab 跨格/追行、Enter 下方同列、Shift+Enter 换行、Cmd+Enter 插行、↑↓ 按列索引、Cmd+A 只选当前格
11. 查找替换（live 模式）：Cmd+F 预填跳转、Cmd+G 全局跳转、正则无效提示、替换单步撤销
12. 图片粘贴拖拽（live 模式）：截图粘贴落盘、拖拽导入、重名顺延、图片卡片渲染
13. 链接增强：尖括号自动链接、裸链补协议、三式引用链接、标点剥离
14. 行内标记：上下标主题色、emoji 短码、脚注引用芯片
15. 块语法：TOC 大纲、HTML 钝化、缩进代码、定义列表
16. 编辑辅助（live 模式）：Cmd+/ 源码切换、Slash 菜单、自动配对、mermaid 图渲染（预览/源码按钮、主题联动、失败回退）
11. 切换亮/暗主题（`macos26-light / macos26-dark`）对比底色，窗口缩窄观察 780px 居中

*—— End of Showcase v2 —*

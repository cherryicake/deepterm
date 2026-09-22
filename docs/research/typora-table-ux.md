# Typora table editing UX — 1:1 parity reference

Research date: 2026-09-23.

**Primary evidence**: Typora **1.14.10** macOS app bundle, read directly.
`Typora.app/Contents/Resources/TypeMark/appsrc/frame.js` (minified but un-obfuscated editor core,
~1.7 MB) — every claim marked *(src)* below is a direct code read, not inference.
Supporting: `support.typora.io/Table-Editing/`, `Shortcut-Keys`, `Markdown-Reference`, release notes,
`typora/typora-issues`.

> Note: typora.io and forum.typora.io are unreachable from this network; the docs were read from the
> `typora/support.typora.io` `gh-pages` source repo and the reachable mirror `support.typoraio.cn`.
> The app bundle came from `downloads.typoraio.cn/mac/Typora.dmg` (1.14.10).

---

## 1. Findings table

| # | Question | Answer | Conf. | Evidence |
|---|---|---|---|---|
| 1 | Click into a rendered cell → blinking caret? | **Yes.** Native caret in the cell. Table = `<figure class="md-table-fig" contenteditable="false"><table class="md-table">`; each cell is `<span class="td-span" contenteditable="true">`. CSS `table{cursor:text}`, `.td-span{display:inline-block;min-width:1ch;width:100%;min-height:10px}`. | high | src: `case v.table_cell: return "<span contenteditable='true' class='td-span' …"`; `base-control.css` |
| 2 | Active cell: raw syntax or rendered? | **Rendered by default; markers appear only when the caret touches them.** `.md-meta{display:none}` / `.md-content{display:none}`; `.md-expand .md-content, .md-expand .md-meta{display:inline}`. `md-expand` is added by `brush.expand()` only when the caret is at the *boundary* of the inline node (`endOffset == node length` or `startOffset == 0`). Docs: "Markdown tags for inline styles, such as `**` will be hidden or displayed smartly." So caret mid-word in `**bold**` → shows **bold**; caret at start/end → shows `**bold**`. | high | src: `function g(e,t){ if(e.endOffset==e.endContainer.length …) return[n,!0]; if(0==e.startOffset) … }`; `base-control.css`; Quick Start |
| 3 | Drag-select inside a cell | **Yes**, standard `::selection` highlight renders inside the cell (`--select-text-bg-color:#B5D6FC`, `.in-text-selection,::selection{background:#B5D6FC}`). | high | src: `base.css` |
| 4 | Double-click | **Selects the word inside the cell** (native word selection). `fixDblClick` only special-cases images/`md-meta`; cells fall through to the browser's word selection. Docs: `Cmd/Ctrl+D` = Select Word. | high | src: `.on("dblclick", … m.selection.fixDblClick(e) …)`; `fixDblClick` body |
| 5 | Triple-click | **Selects the whole cell content.** `fixTripleClick: if(mdtype==table_cell) selectAllElem(cell) else selectLineWrap()`. Outside tables, triple-click = line/paragraph wrap. | high | src: `fixTripleClick:function(e){ e=…; e.attr("mdtype")==R.TYPE.table_cell ? this.selectAllElem(e) : this.selectLineWrap() }` |
| 6 | Selection across cell boundaries | **Possible visually, but it is a cell-range, not a text-range.** A drag can produce a range whose endpoints are in different cells; Typora then treats it structurally: delete/type-over **merges the two endpoint cells and blanks everything between them**, and paste **distributes content across the cells** (split on `\|` / newlines). Whole-row selection is first-class (`Cmd/Ctrl+L`); whole-column selection is not (feature request #6214). | med-high | src: `function C(e,t,n){ … d!=u … for(h=…; h&&h!=u;) h.set("text",""), … }`; `function m(e,t,n,i){ … for each cell … }`; `_selectLine` (`tr[mdtype='table_row']` → `n.selectNode(tr)`) |
| 7 | Cmd/Ctrl+A in a table | **Selects the current cell's content.** Not the table, not the document. No second-press escalation in the source — every Cmd+A in a table re-selects the cell. | high | src: `selectAll(){ … t.closest('[mdtype="table"]').length ? (this.selectAllElem(t.closest("[mdtype='table_cell']")), e&&e.preventDefault()) : … }` |
| 8 | Arrow keys | **Left at cell start / Right at cell end wrap to the same-column cell above/below** (Typora treats a row like a line, so line-wrap maps to cell-wrap). **Up/Down jump to the cell above/below at the same column index** — the *column index* is the goal column, not a pixel x. Wraps: first body row ↑ → last header cell; header ↓ → first body row. **↑ from the header row and ↓ from the last body row exit the table** into the neighbouring block. Alt/Ctrl/Cmd+arrows bypass the table logic (Alt+↑/↓ = move row). | med-high | src: `moveLeftOrRight`: `if(n&&0===s.start) return …this.moveUpOrDown(!0,t,!0)` / `if(!n&&s.end>=d) … moveUpOrDown(!1,…)`; `moveUpOrDown` `.td-span` branch with helper `k()` using `prevAll().length+1` (column index) |
| 9 | Tab / Shift+Tab | **Tab** = next cell (left→right, then wrap to first cell of next row). **Tab at the last cell appends a new empty row** and puts the caret in its first cell (`moveToNextCell(true, false, undefined, true)` → `addRow()`). **Shift+Tab** = previous cell; at the first cell of a row it wraps to the **last cell of the previous row** (from the first body row → last **header** cell). At the very first header cell the source also falls through to `addRow()` (quirk — likely a real edge case, untested). Tab never inserts a literal `\t`. | high (main) / med (Shift+Tab-at-first-cell quirk) | src: `tableEdit.keydownHandler` Tab branch; `moveToNextCell(e,t,n,i)`; `P.addRow`; docs: "In newer versions, you can add rows just by pressing Tab." |
| 10 | Enter / Shift+Enter / Cmd+Enter | **Enter** = move to the cell **below in the same column**, caret at its **start**; if there is no row below, it **exits the table downward**. Never inserts a newline in the cell (issue #2402). **Shift+Enter** = insert `<br />` (hard break) into the cell. **Cmd/Ctrl+Enter** = insert an empty row **below** the current row (caret into its first cell). | high | src: `moveToBelowCell(getNextCellInColumn(…))`; `e.shiftKey ? pasteHandler("<br />") : (mac? metaKey:ctrlKey) ? addRow(false, …) : moveToBelowCell(…)`; issues #2281/#4760/#6015; docs |
| 11 | Esc | **Dismisses autocomplete / inline preview only; it does not exit the cell or the table.** Esc in the table-insert dialog closes the dialog. | med | src: `if("Esc"==keyNames[i]){ if($("#table-insert-dialog:visible").modal("hide").length) … ; h.brush.triggerAutoComplete(!0), e.preventDefault() }` |
| 12 | Backspace at cell start / Delete at cell end | **No merging.** Backspace on an **empty** cell jumps the caret to the previous cell; if the table is empty it deletes the table. On a non-empty cell it hits the zero-width sentinel and does nothing. Delete at the end likewise does nothing. `Cmd/Ctrl+Shift+Backspace` deletes the whole row. Cells can never be merged (no colspan in GFM; issue #809). | med-high | src: `function b(e,t){ if(R.isType(t,O.table_cell)) … (t=I.getSibling(t,"before")) && (jumpIntoElemEnd(prev), …) : e.tableEdit.deleteTable(…) }`; `if(…closest("[mdtype='table_row']").length && shift && (meta\|ctrl)) return … i.tableEdit.deleteRow()` |
| 13 | Auto-reformat on edit? | **Yes, but only for tables you touched.** The model keeps the original source in `userText` and re-emits it verbatim while untouched. Any text change or structural op calls `tryResetTable()`, which drops `userText` → the table is **re-serialized with column-width padding** (width = widest cell, min 4, capped at 60 display columns) and re-aligned delimiters (`-`.repeat(width), `:` for left/right/center). Opening + saving an untouched table does **not** reformat it (that was the #1189/#989 fix). `Prettify Source Code` in the table menu forces a full re-format. | high | src: `case Q.table: if(this.get("userText")) … else { … N[_]=Math.max(X.wcswidth(…)); N[_]=Math.min(N[_],60) … }`; `w.tryResetTable=function(e){ … e.set("userText",void 0) }`; `reformatTable()`; `alignText` helpers |
| 14 | Add/delete rows+cols, alignment | **Menu bar** `Paragraph → Table` (added 0.9.73): Add Row Above/Below, Add Column Before/After, Move Row Up/Down, Move Column Left/Right, Delete Row, Delete Column, Copy Table, Prettify Source Code, Delete Table. **Right-click** → same `#table-menu`. **Floating toolbar above the header** while the caret is in the table: resize/menu button (leftmost), left/center/right align (per column, multi-column selectable since 1.4), "More Actions"/delete-table. **Resize popover** sets row/column *counts* (up to 99 rows / 40 cols). **Hover the row/column edge → drag tracker** (`#typora-table-row-tracker` / `#typora-table-col-tracker`) with an insert marker to move/reorder. **Shortcuts**: `Cmd/Ctrl+Enter` add row below; `Tab` append row; `Cmd/Ctrl+Shift+Backspace` delete row; `Cmd/Ctrl+Shift+Alt+L` delete line/row; `Alt+↑/↓` move row; `Alt+←/→` move column; `Cmd/Ctrl+L` select row; `Cmd/Ctrl+E` select cell. Column *widths* are not user-resizable (open request #1088). | high | src: `#table-menu` markup in `index.html`; `U=function(){ $("#table-menu").on("click",'[data-key="insert_row_before"]' …)`; `showTableEdit`; `typora-table-row/col-tracker` handlers; docs |
| 15 | Copy inside a cell | **Both formats are on the clipboard at once.** Default `Copy` = HTML/rich; the **plain-text flavor is the rendered text** ("A boy"). With Preferences → Editor → "Copy Markdown source as plain text" enabled, the plain-text flavor is the **Markdown source** ("A \*\*boy\*\*"). `Cmd/Ctrl+Shift+C` (Copy as Markdown) always yields source. With a collapsed caret, `lineWiseCopyCut` (default on) copies the whole **row** inside a table. | high | src: `copyHandler`; `File.option.copyMarkdownByDefault ? g(s) : m(s)`; `selectLineWrap()` → `_selectLine` row path; docs "Copy and Paste" |

---

## 2. Structural notes worth copying

1. **Cell = inline-only container.** A cell is a `td`/`th` wrapping a single `contenteditable` `span.td-span`.
   Block content (paragraphs, fences, lists) cannot live inside a cell; that is why pasting multi-line
   text distributes across cells, and why `Enter` can never create a paragraph inside a cell.
2. **Row like a line.** The model maps horizontal wrap to vertical cell wrap; this is the single most
   surprising behaviour to replicate.
3. **Header is a real row.** `th` cells are part of the same column-index arithmetic as `td` cells, so
   ↑/↓ and Shift+Tab flow through the header.
4. **Empty-cell sentinel.** Cells are padded with `U+200B`; backspace/delete at the boundary hits it
   instead of a neighbour. This is what makes "backspace at cell start" a no-op.
5. **Serialization is width-based, not delimiter-based.** The delimiter row's dashes are as wide as the
   column, and pipes are aligned, once the table has been edited.
6. **`npTable` (pass-through) mode.** Tables parsed with `pass_format` (the GFM-compat path) keep
   per-cell `marginLeft`/`marginRight` and are emitted without re-padding.

## 3. Known gaps / lower-confidence items

- Shift+Tab at the very first header cell: source falls through to `addRow()`; behaviour not verified
  against a live app. Treat as "unclear, verify before copying".
- Whether Up/Down first traverse multiple lines *inside* a multi-line (`<br>`) cell: source routes the
  cell branch before generic movement, which suggests it jumps cells immediately; unverified.
- Cross-cell drag selection visuals: confirmed structurally (row selection + range handling exist), but
  the exact highlight shape during a drag across cells was not verified on a live app.
- Esc: source-backed but no official doc statement; "unclear" whether it also blurs the cell.

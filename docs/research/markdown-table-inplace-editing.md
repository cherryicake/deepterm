# In-place Markdown table editing in a live/WYSIWYG preview — comparative research

Scope: how real editors let a user click a *rendered* table cell and type, without the table
flipping back to raw Markdown source. Includes a recommendation for deepterm's
`src/editor/markdown_live` line-based editor.

Research date: 2026-09. All source references were read from cloned repos or upstream docs.

---

## 0. Summary matrix

| Project | Click cell → type in rendered cell? | Mapping model | Tab / Enter | Structural ops | Cursor in cell |
|---|---|---|---|---|---|
| **Typora** | **Y** — rendered table is the only editing surface | DOM table ↔ hidden block source; per-cell buffer, serialized on commit | Tab = next cell (appends row past last); Enter = new row below; Cmd/Ctrl+Enter = insert row | Context menu / toolbar / drag handles: row+col add/delete, align, resize, move | Native caret in cell; cell shows raw inline syntax while focused |
| **Obsidian ≥1.5** | **Y** — brand-new table editor, cell-by-cell | CM6 block `Decoration.replace` + widget; DOM table ↔ doc ranges | `Tab` = next cell (documented) | Context menu, command palette, hotkeys, drag rows/cols, alignment, sort, copy/paste, side "+" affordances | Caret lives in the widget (own DOM selection), not in CM's text layer |
| **Zed** | **N** — preview is read-only | n/a | n/a | n/a | n/a |
| **VS Code** | **N** (classic preview). **New**: experimental "hybrid Markdown editor" (1.131, Jul 2026) allows in-place rendered editing — table specifics not documented | n/a / webview | n/a | n/a | n/a |
| **Logseq** | **N** built-in (click → enters block edit, whole block becomes source). **Y via plugin** `logseq-plugin-markdown-table` (a modal editor, not in-place) | block string ↔ modal grid | Tab/Shift+Tab next/prev cell, Shift+Enter line break | Modal toolbar | Native caret in modal |
| **Notion** | **Y** (reference UX, not Markdown) | block tree; each cell is a text node | Tab = right, Enter = down, Esc = exit to cell-select, Shift+Enter = line break inside cell | Handles/`:::` menu, drag rows+cols, resize, fill right/down | Native caret; selection model is block-level then cell-level |
| **CM6 `codemirror-markdown-tables`** | **Y** — closest full reference implementation | `Decoration.replace({block:true})` over table range; cell edits dispatched as **whole-table replacement** | Tab/Shift+Tab cell nav (+row create), Enter = cell below, Shift+Enter `<br>`, arrows at boundaries, Esc closes | Floating toolbar + row/col handles + drag: insert/delete/move/duplicate/clear/sort/align, cell-range select + copy/paste | **Nested CM6 `EditorView` per active cell** (a real editor, so selection/caret come free) |
| **Joplin Rich Tables** | **Y** — same architecture, most battle-tested | Same as above + dedicated `editorBridge` with offset maps | Tab/Shift+Tab, Enter, arrows with `exitTableAtBoundary`, Shift+arrows cell-range select | Toolbar + commands, source-mode toggle, boundary blank-line protection | Nested CM6 view + root-DOM selection mirroring + caret suppression |
| **Zettlr 4.x** | **Y** — "active cell shows Markdown, other cells rendered" | Block widget; **subview holds the entire document** with replace-decorations hiding everything outside the cell | Tab/Shift+Tab/arrows via table-editor keymap | Commands + context menu + row/col controls in widget DOM | Subview caret; transaction filter clamps edits to the cell |
| **ProseMirror / Tiptap** | **Y** (reference model) | Schema nodes (`table`/`table_row`/`table_cell`) + positions; `TableMap` for geometry | `goToNextCell` / `goToPreviousCell`; Enter/arrow keymap | `addRowBefore/After`, `deleteRow/Column`, `mergeCells`, `splitCell`, `toggleHeader`, `columnResizing` | Native PM selection; `CellSelection` for ranges |

---

## 1. Typora

Sources: <https://support.typora.io/Table-Editing/>, <https://support.typora.io/Markdown-Reference/>,
<https://support.typora.io/Shortcut-Keys/>, <https://support.typora.io/What's-New-0.11>

**Answer: Y.** Typora has no preview pane at all — the rendered table *is* the editing surface.
Clicking a cell puts a native caret in it and you type directly; there is no "flip to source"
step for the table as a whole.

**Mapping.** Typora is a DOM-based editor (Electron/WebKit) with an internal document model, not a
text-buffer editor. The visible table is a real `<table>`; the Markdown source is *generated*.
The docs are explicit: "The full syntax for tables is described below, but it is not necessary to
know the full syntax in detail as the Markdown source code for tables is generated automatically
by Typora." Typora rewrites/normalizes the source table on commit, so the mapping is
"DOM cell text → regenerate row → rewrite the block", not "cell ↔ byte range".

**Keyboard.**
- `Tab` — next cell; in newer versions, tabbing past the last cell appends a row.
- `Enter` — inserts a new row (historically `Cmd/Ctrl+Enter`; docs say "In newer versions, you can
  add rows just by pressing `Tab`"). The key detail for us: **Enter inside a cell does not insert a
  newline into the cell** — it is a row-level operation.
- `Ctrl/Cmd+L` selects the row; `Ctrl/Cmd+E` selects the cell; `Ctrl/Cmd+Shift+D` deletes the word;
  `Ctrl/Cmd+Shift+Backspace` deletes the row.
- Arrow keys move the caret within the cell's text; there is no documented "arrow past the cell
  boundary jumps to the neighbouring cell" rule.

**Structural ops.** A floating toolbar appears when the table has focus ("resize, align, or delete
the table"); the context menu does add/delete row and column; columns can be dragged/resized;
alignment is a per-column toolbar control. Cell selection is a first-class concept
(`Select Cell`, `Select Row` shortcuts).

**Cursor/selection.** Native caret in the cell. Crucially, **the active cell shows its raw inline
Markdown** — `**bold**` reads as `**bold**` while the caret is in that cell — while *inactive* cells
stay rendered. This is the "hybrid" reveal discipline: reveal is scoped to the focused cell, not the
whole row/table. It is what makes in-place editing honest: you always see the characters you type.

**Pitfalls documented.** The block-level reveal rule is documented for inline elements: "Moving the
cursor to the middle of a span element will expand that element into the Markdown source." The
table analogue (whole table flips to source) is the behaviour Typora *avoids*, which is precisely
what makes it the reference UX. A long-standing complaint
(<https://github.com/typora/typora-issues/issues/1189>) is that Typora's automatic table
*prettification* is not optional and can rewrite a hand-formatted table on save — a warning about
regenerating source without asking.

---

## 2. Obsidian Live Preview

Sources: <https://obsidian.md/changelog/2023-12-26-desktop-v1.5.3>,
<https://forum.obsidian.md/t/live-preview-support-editing-a-table-cell-by-cell/34110>,
<https://forum.obsidian.md/t/add-toggle-for-plaintext-table-editing-source-mode-in-live-preview/73866>

**Answer: Y — but only since v1.5 (Dec 2023).** Before that, Obsidian's Live Preview behaved the way
the question describes as bad: "As of Obsidian 0.14.1, Obsidian supports tables rendering in Live
Preview. **Editing the table transforms the whole table to its source markdown format.**"
v1.5 replaced that with a real table editor: "Brand new editor for Markdown tables. Table rows and
columns are now easier to create, edit, sort, reorder, select, copy, and paste… Tables are still
saved to plain text Markdown."

**Mapping.** Obsidian is CodeMirror 6. The rendered table is a block-level
`Decoration.replace` widget over the table's source range, and the *whole table is rendered as a
widget at all times* in Live Preview (it does not revert to source when the cursor enters, unlike
Obsidian's own code blocks/embeds). Cell ↔ source mapping is therefore widget-internal: the widget
knows the doc range of the table, parses it into a grid, and writes back by editing that range.
Community code and theme CSS confirm the widget class is `cm-table-widget` (e.g. CSS snippets
targeting `.cm-table-widget`, and the forum thread
<https://forum.obsidian.md/t/help-modifying-rushikesh-css-snippets-to-run-in-live-preview-and-source-modes/110960>).
This is the same architecture as `codemirror-markdown-tables` §7 — almost certainly with a
per-cell nested editor, which is the only way CM6 supports this (see §7 for why).

**Keyboard.** Release notes: "Press `tab` to navigate between cells." Context menu / command
palette / hotkeys cover the structural operations; `Ctrl/Cmd-A` inside a cell selects the cell
content (changelog mentions fixing `Ctrl/Cmd-A` inside embedded inputs, which is the same
routing problem §7 solves with `globalKeyBindings`).

**Structural ops.** Add rows/columns by clicking the sides of the table; context menu, command
palette, and hotkeys; drag to reorder rows and columns; per-column alignment; sort; select/copy/paste
ranges. New CSS variables `--table-*` for selection and dragging were added in 1.5.0 — i.e. the
selection chrome is themeable, so the widget owns its own selection rendering.

**Cursor/selection.** Owned by the widget (not CM's text-layer caret) — that is forced by the
architecture, and it is why the Vim keybindings broke: "doing `5k` no longer goes up 5 lines, the
cursor just stays there", and `dd`/`o` stopped working inside tables
(<https://forum.obsidian.md/t/vim-support-more-commands-in-live-preview-tables/73992>).

**Pitfalls they document.**
1. **The reveal toggle was lost.** Because tables are *always* widgets, users who wanted plain-text
   table editing must now switch to Source mode — a widely-upvoted regression
   (<https://forum.obsidian.md/t/add-toggle-for-plaintext-table-editing-source-mode-in-live-preview/73866>).
   The requested fix is exactly the mermaid-style "源码/预览" toggle deepterm already has.
2. **Vim/editor keybindings break** inside the widget (see above).
3. **Row/column reorder leaked memory** — a changelog bug fix: "Live Preview: Fixed memory leak
   caused by reordering rows or columns in a Markdown table." Widget churn is a real cost.
4. **Alignment/padding drift** — pre-1.5, Live Preview could not pad cells, so wide tables
   misaligned (<https://forum.obsidian.md/t/better-table-handling-in-new-live-preview-mode/29079>).

---

## 3. Zed

Sources: <https://zed.dev/docs/languages/markdown>, <https://zed.dev/languages/markdown>,
<https://zed.dev/docs/all-actions>, <https://github.com/zed-industries/zed/discussions/23951>

**Answer: N.** Zed has two separate surfaces:
- The **Markdown editor** (the buffer) — tree-sitter highlighting, list continuation
  (`extend_list_on_newline`), list indent on Tab (`indent_list_on_tab`), Prettier formatting.
  Tables are plain text; no widget, no grid. There is an open request "Format Shorthand Markdown
  Tables #5382" in the tracking issue.
- The **Markdown preview** (`markdown::*` actions, `project panel: open markdown preview`) — a
  read-only rendered pane with scroll actions (`markdown::ScrollUp`, `ScrollUpByItem`). It is a
  preview, not an editing surface.

Zed's own tracking issue for Markdown improvements lists preview defects (table alignment
"#11024 Markdown Tables are Misaligned in Preview", column widths "hard to read tables in the
preview") and an explicit feature request for "Inline markdown editing (Typora/Obsidian-style
preview)" (<https://github.com/zed-industries/zed/discussions/55710>) — i.e. **Zed does not have
this feature and it is not implemented**. A separate discussion (#59598) proposes extension-provided
read-only previews and lists "editable previews" only as future work.

Relevant to us: Zed's preview *is* GPUI, and it renders tables as real layout — so the rendering
half is proven in GPUI, but there is no upstream implementation of the editing half to copy.

---

## 4. VS Code built-in Markdown preview

Sources: <https://code.visualstudio.com/docs/languages/markdown>,
<https://code.visualstudio.com/updates/v1_131>,
<https://visualstudiomagazine.com/articles/2026/07/29/vs-code-1-131-adds-built-in-dictation-hybrid-markdown-editing-and-subagent-status.aspx>

**Answer: N for the classic preview; new experimental surface is Y-in-principle.**

The built-in preview (`Ctrl+Shift+V`, `Ctrl+K V`) is a **webview** with script execution disabled and
a content-security model (`Markdown: Change preview security settings`, `Markdown: Toggle Preview
Locking`). It is scroll-synced to the text editor and highlights changed lines, but it is read-only:
the documented workflow is "toggle the visualization of the editor between the code and the
preview". No cell editing, no mapping back to the buffer. Formatters (e.g. Markdown Table
Prettifier) operate on the *text*, not the preview.

**New in 1.131 (July 2026):** an experimental **"hybrid Markdown editor"**
(`workbench.editor.markdownDefaultEditorInAgentsWindow`) that "lets you view Markdown files, edit
them in-place, and add comments that an agent can act on", switchable via *Reopen Editor With*.
Microsoft's own framing is that it is "experimental" and "you're going to run into some bugs".
Table-cell specifics are not documented; treat it as a webview/rich-renderer with in-place editing
rather than a cell-level table editor. Not a source we can mine for implementation detail.

---

## 5. Logseq

Sources: <https://discuss.logseq.com/t/tables-in-logseq/775>,
<https://discuss.logseq.com/t/implement-a-live-table-editing-without-going-into-editing-mode/19217>,
<https://github.com/haydenull/logseq-plugin-markdown-table>,
<https://xyhp915.github.io/logseq-marketplace-table>,
<https://discuss.logseq.com/t/tables-in-logseq/775>

**Answer: N built-in; Y via plugin (but as a modal, not in-place).**

Built-in: a table is the content of a single block. Rendering shows the block rendered; **clicking
enters block edit mode, which shows the whole block as raw Markdown text**. Users explicitly
requested the thing we are designing: "Implement a 'live table editing' without going into editing
mode" — the proposal is literally "the table cells could be rendered as text fields by Logseq …
just like checkboxes" (checkboxes are the one element Logseq *does* let you toggle without entering
edit mode, via `{{renderer}}`/block property machinery).

Plugin path — `haydenull/logseq-plugin-markdown-table`: a `/` command opens a **modal grid editor**;
documented shortcuts are `Tab` next cell, `Shift+Tab` previous cell, `Shift+Enter` line break in the
current cell. `VictorVow/logseq-plugin-advanced-markdown-table` adds auto-formatting and
"TAB and ENTER to move between cells and row, and a few buttons for common table manipulation like
deleting columns". `xyhp915`'s marketplace table plugin renders tables as "a live, editable table
with toolbar, context menu, drag-to…". So Logseq's ecosystem converged on the same UX as everyone
else, but implemented outside the editor as a modal — which is exactly the "not in-place" outcome
to avoid.

**Pitfalls.** The modal approach loses the document context and breaks inline Markdown that the
modal cannot render; Logseq also has no `<br>` in cells and no multi-line cells, so cell content is
single-line by construction.

---

## 6. Notion (UX reference, not Markdown)

Sources: <https://www.notion.com/help/keyboard-shortcuts>,
<https://www.notion.com/help/guides/simple-tables-vs-databases>,
<https://www.notion.vip/insights/the-finer-points-of-simple-tables>,
<https://spellapp.com/resources/how-to-enter-in-notion-table>

**Answer: Y** (block-level model, the gold standard for the interaction vocabulary).

**Mapping.** Everything is a block; a simple table is a `table` block containing
`table_row` blocks containing `table_cell` blocks, each cell holding inline rich-text. There is no
byte mapping — the cell *is* the addressable node. Serialization is a separate concern.

**Keyboard model — the vocabulary to copy:**
- `Tab` moves right (and to the first cell of the next row at the end).
- `Enter` moves down (and appends a row at the bottom).
- `Esc` **deactivates the typing caret and selects the current cell** — a two-state model
  (cell-selected vs cell-editing).
- `Shift+Enter` inserts a line break *inside* the cell.
- Arrows navigate; `Shift+arrows` extend a rectangular **cell selection**; `Cmd/Ctrl+D` fill down,
  `Cmd/Ctrl+R` fill right on a multi-cell selection.
- Backspace/Delete clears the selected cells; `Delete` on a row handle deletes the row.

**Structural ops.** Hover handles on rows/columns, a `:::` per-row/column menu (duplicate, delete,
clear, add adjacent), `+` bars on the last row/column and at the bottom-right corner, drag to
reorder, drag borders to resize, header-row formatting, column alignment.

**Cursor/selection.** Native caret while editing; a distinct block-level/cell-level selection state
when not editing. The separation is what makes Tab/arrow navigation unambiguous.

**Pitfalls.** Notion's model only supports *simple* tables for text; multi-block content inside a
cell is impossible by design ("you're unable to add other blocks—including bullet, number and to-do
lists"), and cells are text-only. That maps well onto GFM's actual constraints (no newlines in
cells), so it is a defensible scope cut for a Markdown editor.

---

## 7. CodeMirror 6 table-editing extensions

This is the most directly transferable body of work. Three implementations, in increasing order of
sophistication.

### 7a. `codemirror-markdown-tables` (ckant) — the cleanest full reference

Repo: <https://github.com/ckant/codemirror-markdown-tables> (cloned; ~12.7k LOC TS/Svelte).
Docs: <https://ckant.com/codemirror-markdown-tables>

**Core mechanism.** The whole table is one block replace decoration
(`src/lib/codemirror/decoration/tableDecorations.ts`):

```ts
return { widget: TableWidget.of(tableDescription, state), block: true, inclusive: true }
```

**Cell editing: a nested CM6 `EditorView` per active cell**
(`src/lib/componentModels/table/cell/cellEditor/cellEditor.ts`). The nested view gets its own
extensions: `drawSelection()`, `lineWrapping`, a *markdown* language instance with a
`tableCellLike` config (so inline syntax highlights inside the cell but block syntax does not),
`keymap.of(globalKeyBindings)` rebound to run against the *root* editor, and
`syntaxHighlighting`. It is constructed with `dispatchTransactions` overridden so every local
transaction is forwarded to `onChange`:

```ts
dispatchTransactions: (transactions, view) => {
  view.update(transactions)
  transactions.forEach(onChange)
}
```

**Why nested editors are mandatory in CM6** — from the CM6 author, on the "WYSIWYG table" thread
(<https://discuss.codemirror.net/t/wysiwyg-table/9763>): *"Not without displaying the table as a
block widget and nesting multiple inner editors into the cells. CodeMirror itself requires all its
content lines to be arranged in a single column."* This is the single most important architectural
fact for anyone planning a table editor on a linear text buffer.

**Sync model (the interesting part).** `TableDescription`
(`src/lib/codemirror/state/tableDescription.svelte.ts`) holds:
- `span` — the table's doc range, remapped with `transaction.changes.mapPos(from, 1)` / `mapPos(to)`;
- a `Table` model (`src/lib/core/models/table.svelte.ts`) that keeps `Text` cells + cached
  `colSizes` / `contentSizes` and does *incremental* minimal edits (never "reformat the whole
  string": see its header comment, which explicitly rejects the naive approach as generating "a
  continuous stream of discarded strings");
- `selection.value` = `CellSelection | "all" | "hidden" | "none"`.

`computeSelection` translates a root-editor selection into cell space by *arithmetic on offsets*
relative to `this.from`, and clamps to the cell span. Then the widget effect
(`tableWidget.svelte.ts`) writes back:

```ts
const newChanges = textChanged
  ? { from: tableDescription.from, to: tableDescription.to, insert: tableDescription.table.text }
  : undefined
tableDescription.markSynchronized()
view.dispatch({ annotations: TableAnnotation.of("table.edit"), changes: newChanges, selection: newSelection })
```

So: **cell edits are committed as a single whole-table `changes` replacement**, annotated, and the
widget is marked synchronized to avoid a feedback loop. Undo/redo is the root editor's.

**The "hidden" cursor trick.** A cursor placed immediately after the table's first `|` is mapped to
a *hidden* selection state — the table is still a widget (no flip to source), and a click at that
position is interpreted as "select all / no cell". This is how they support clicking a table without
forcing a cell activation.

**Keyboard** (`src/lib/componentActions/navigate/navigateActions.ts`) — very concrete:
`Tab`/`Shift+Tab` → `moveRight/moveLeft({createRow:true, position:"end"})`;
`Enter` → `moveDown({createRow:true, position:"end"})`;
arrow keys only fire **at the cell boundary** (`if (!this.position.left) return`), which is how
they avoid stealing in-cell caret movement;
`Shift+arrows` build a rectangular `outlinedSection`;
`moveTo` sets `selectionValue = {cell, section:{anchor:cursor, head:cursor}}` with `cursor = 0` for
"start" and `cell.length` for "end". `Esc` closes the cell editor. `Shift+Enter` inserts `<br>`.
Past the last cell, `appendEmptyRows(1)` is called — **structural ops are model mutations, then a
single commit**.

**Formatting.** `tableParser.ts` + `tableFormatter.ts` normalize every row to
`| cell | cell |` with one space of padding and column widths = max content width; alignment
hyphens are padded to the same width. `tableSpanParser.ts` computes `ChangeSpec[]` and only
*auto-formats* tables that were not already parsed in a previous pass (incremental). Note the
policy: formatting is applied automatically while typing, not on demand.

**Pitfalls they encode.**
- Nested editors do **not** inherit root extensions; the config exposes `extensions`,
  `globalKeyBindings`, `markdownConfig`, `selectionType`, `lineWrapping` separately, with docs
  explaining which keybindings belong to the cell (`defaultKeymap`) vs the document
  (`historyKeymap`, `searchKeymap`). Getting this wrong breaks Ctrl+Z and Ctrl+F inside a cell.
- Selection rendering: they ship a `drawSelection`-based mode and a `native` mode, plus
  `--tbl-theme-select-all-focus-overlay` / `-blur-overlay` variables because CM's selection
  background paints *behind* text while the table is opaque — the widget must fake it with an
  alpha overlay above the table.
- `WidgetType.coordsAt` must be implemented (they map `pos → closestCellAtPosition(pos)` and return
  that cell's rect) or CM's cursor/scroll logic misbehaves.
- `estimatedHeight` + `ResizeObserver` are needed or the editor scroll-jumps.
- Collaboration: internal sync transactions are annotated (`syncAnnotation`) and must be filtered
  by the collab integration.

### 7b. Joplin `Rich Tables` (bwat47) — the production-hardened variant

Repo: <https://github.com/bwat47/joplin-rich-tables> (cloned; README notes it was "created entirely
with AI tools"). The `@markwhen/codemirror-tables` npm package is "originally extracted from
Joplin's rich markdown editor".

**Same core**: `Decoration.replace({widget: new TableWidget(context), block: true})`
(`src/contentScript/tableWidget/tableDecorationField.ts`), with the comment: *"Tables are always
rendered as widgets - editing happens via nested cell editors."* It also preserves the active
table's decoration across rebuilds (`PreservedActiveDecoration`) so the open cell editor survives a
document change, and tracks `activeHostInvalidated`.

**Better separation of concerns than 7a.** There is a dedicated `editorBridge/`:
- `cellTextCodec.ts` + `shared/cellTextNormalization.ts` — the most valuable file to read. Cell text
  is stored *sanitized* and displayed *unsanitized*:
  - `\n` ⇄ `<br>` (`convertNewlinesToBr`, `normalizeBrTags`)
  - bare `|` → `\|` (`escapeUnescapedPipesWithContext`, backslash-run aware)
  - `sanitizeLocalText` / `unsanitizeRootText` are **exact inverses**, and each is generated by the
    same single `scanLocalRuns` walk as its offset map (`localToRootOffsets` / `rootToLocalOffsets`,
    `Int32Array`s) — "so the two cannot disagree about where a character went". Offsets inside a
    multi-char stored spelling (e.g. inside `<br>`) all map to the start, so a caret can never land
    halfway through it.
  - `sanitizeCellChanges` rejects any change touching outside the cell bounds and rewrites the
    insert; this is the guard against a paste blowing up the table.
- `rootDomSelection.ts`, `syncAnnotation.ts`, `nestedEditorFocusGuard.ts`.
- `tableWidget/mainCaretSuppression.ts` — suppresses the *root* editor's caret (both the native
  `caret-color` and CM's `.cm-cursorLayer`) while a cell selection is active or a cell is opening,
  because otherwise a stray caret paints "a stripe on the cell divider, right where the click
  landed, a frame before the real caret appears in the cell".

**Keyboard** (`src/contentScript/nestedEditor/domHandlers.ts`) uses a dedicated keymap *scope*
(`table.nestedEditor.routing`) and inverts CM's `Command` contract: the keydown handler stops
propagation for every chord the scope does not claim, so a command returning `true` means "this
chord belongs to the host, let it bubble". `Mod-b/i/u/`/e/k` first mirror the cell selection to the
root and then bubble, so Joplin's own bold/italic commands operate on the cell text.

**Pitfalls they document** (README + forum):
- **Lists cannot be rendered in cells.** GFM has no newlines in cells, so markdown-it cannot render
  a multi-item list; the fix was to *not render lists at all* rather than render them wrong.
- Footnotes break because "table cells are rendered in isolation, which breaks
  markdown-it-footnote's footnote numbering".
- Sorting is by raw Markdown, so `**bold**` sorts before `bold`.
- **Caret placement on click was the hardest bug**: "placing the cursor accurately when clicking on
  a table cell … is tricky to implement since the plugin shows rendered HTML in table cells not
  being edited, and then switches to a nested codemirror editor on click". They added a dedicated
  `cellCaretHit.ts`.
- Blank-line boundaries around a table must be protected from incidental deletion, and line breaks
  auto-inserted around pasted tables.
- Search: "Table markdown is revealed while joplin search panel is active. When closing the search
  panel … closing search will activate that table cell with the current search result selected."

### 7c. Zettlr 4.x — the "active cell shows source, others rendered" variant

Repo path: `source/common/modules/markdown-editor/table-editor/`
(`widget.ts`, `subview.ts`, `commands/{rows,columns,tables}.ts`, `context-menu.ts`).
Docs: <https://docs.zettlr.com/en/editor/tables.html>, <https://zettlr.com/post/zettlr-400-released>

**Answer: Y.** Docs: *"To edit tables with the table editor, click into any of the cells of the
table and begin writing. Regular Markdown syntax is supported. Whenever a cell is currently active,
you will see the Markdown source of the cell. Every cell that is not active will be rendered as
HTML to make the display less cluttered."*

This is the **hybrid reveal** model, and it is the closest to what deepterm already does for
inline markup — except scoped per *cell* rather than per line.

**Mapping.** `TableWidget.toDOM` parses the table AST (`parseTableNode`) and builds a real
`<table>`. The trick in `subview.ts`: the subview **holds the entire document**
(`doc: mainView.state.sliceDoc()`, with an apologetic comment about 100 MB documents) and uses
`hiddenSpanField` to install four `Decoration.replace` ranges hiding everything outside the cell:

1. block before the cell's line, 2. the cell's line up to `cellRange.from`,
3. the cell's line after `cellRange.to`, 4. block after the line.

Because the subview is a real editor over the real document at real offsets, sync is trivially
correct: `dispatchFromSubview` re-dispatches the subview's transaction to the main view with a
`syncAnnotation`, and `maybeDispatchToSubview` does the reverse. No offset arithmetic at all.

**Safety filter.** `ensureBoundariesFilter` (`EditorState.transactionFilter`) maps the cell range
through the change set with `mapPos(cellFrom, -1, TrackBefore)` / `mapPos(cellTo, 1, TrackAfter)`,
**rejects the transaction if the range was deleted**, clamps any selection range to the cell, and
replaces newlines in inserts with spaces:

```ts
if (fromA >= cellFrom && toA <= cellTo) {
  const ins = inserted.toString()
  safeChanges.push({ from: fromA, to: toA, insert: ins.replace(/\n+/g, ' ') })
}
```

The code comment documents a subtle bug: CM's parser excludes surrounding whitespace from
`TableCell`, so using the AST span as the boundary means "the user could not enter any text
afterwards" once they type a space. Their fix is to derive boundaries from the **hide decorations**
instead, which map through changes and therefore "add any manually added space to the cell's
content". **This is a directly transferable pitfall.**

**Pitfalls they document.** Zettlr's own 4.0 post is a post-mortem: the *first* table editor
"was always plagued by bugs. Most crucially, many users had experienced data loss issues where
their changes to a table's contents were not properly saved." They rebuilt it on CM6 and added
tab-between-cells + row/column commands that work **even when the visual editor is off**. Also
documented: the table editor supports **pipe tables only** — grid tables are explicitly out of
scope.

---

## 8. ProseMirror / Tiptap (reference model)

Sources: <https://github.com/ProseMirror/prosemirror-tables>,
<https://tiptap.dev/docs/editor/extensions/nodes/table>,
<https://discuss.prosemirror.net/t/prosemirror-markdown-tables/2334>

**Answer: Y**, trivially — this is a rich-text document model, so a table cell is a real node with
its own editable text.

**Mapping.** `tableNodes()` builds a schema extension: `table`, `table_row`, `table_cell`,
`table_header`. Positions are structural (node offsets), so there is no "rendered vs source"
distinction to maintain — the doc *is* the rendered model. `TableMap.get(tableNode)` computes the
geometry (row/col → position) and is the analogue of a cell↔offset index. `CellSelection` is a
first-class selection class for rectangular ranges; `tableEditing()` is a plugin that enables
cell selection, cell-based copy/paste, and enforces the invariants "each row has the same width,
and cells don't overlap".

**Keyboard.** `goToNextCell(direction)` / `goToPreviousCell`; the table plugin binds Tab/Shift-Tab
to these by default. `Esc`/`Enter` behaviour comes from the surrounding keymap.

**Structural ops** (all commands): `addRowBefore/After`, `addColumnBefore/After`, `deleteRow`,
`deleteColumn`, `deleteTable`, `mergeCells`, `splitCell`, `toggleHeader`, `setCellAttr`,
`columnResizing` (three modes: `neighbor` — Google-Docs-style constant width, `independent`,
`redistribute`). Tiptap exposes these as `editor.commands.*` and ships `fixTables()` for
invariant repair.

**Pitfalls documented.**
- **Markdown round-tripping is the hard part, not editing.** From the PM forum thread on
  prosemirror-markdown + tables: you must write the serializer/deserializer yourself, and
  "markdown-it's table plugin produces a format which is hard to map with prosemirror-table".
  Cells with `rowspan`/`colspan` have no GFM representation at all, so they must be disabled.
- Firefox injects native table controls that don't work with PM; you must call
  `document.execCommand("enableObjectResizing", false, "false")` and
  `("enableInlineTableEditing", false, "false")`.
- `tableEditing` requires tables to stay well-formed; `fixTables()` exists because they drift.

**Takeaway for a line-based editor.** The PM model tells us the *interaction vocabulary* is settled
(Tab = right, Enter = down, Esc = exit, Shift+Enter = line break, cell-range selection) and that
structural commands should be model-level operations with an invariant repair pass — not ad-hoc
string surgery.

---

## 9. Rust / GPUI projects doing markdown table live editing

- **Zed** (§3) — read-only preview; no table editing. Confirmed from docs, actions list, and the
  tracking issue.
- **Velotype** (<https://github.com/manyougz/velotype>, <https://users.rust-lang.org/t/140046>) —
  "a block-based Markdown editor built with Rust and GPUI … supports both WYSIWYG-style rendered
  editing and Markdown source-text editing." A `rustfmt`-adjacent project whose README compares
  itself to Typora/MarkText/Obsidian. It is early; table-specific implementation details are not
  published.
- **Aster** (<https://github.com/kumarUjjawal/aster>) — "Markdown editor built in Rust on top of
  GPUI … focuses on a fast inline-editing experience." Early stage.
- **Ferrite** (<https://news.ycombinator.com/item?id=46571980>) — Rust, GFM tables + code blocks;
  its author describes three modes (Rendered / Split / Raw) and says explicitly: *"It's not pure
  'type and it formats inline' like Typora … The Rendered mode lets you click elements to edit
  them, but it's not seamless WYSIWYG yet."* Notably, **"Rendered mode — Click-to-edit rendered
  Markdown"** is exactly the "click a rendered element to edit it" model, and the author treats it
  as a *separate mode* rather than inline editing.
- **gpui-component / gpui-kit** — GPUI has virtualized Table and List components and an Editor
  component with tree-sitter highlighting, but neither ships Markdown table *editing*.

**Conclusion for §9: there is no existing Rust/GPUI implementation of in-place Markdown table
editing to copy.** The JS ecosystem (CM6 nested-editor widget) is the only mature reference. This
means the design must be derived, not ported — which makes the choice of the least-invasive
approach more important, not less.

---

## 10. Synthesis: the four architectural approaches

| # | Approach | Who uses it | Mapping | Cost | Failure mode |
|---|---|---|---|---|---|
| A | **Source reveal** — cursor entering the block reveals raw Markdown | Obsidian **pre-1.5**, Logseq built-in, deepterm's current table behaviour | 1:1 lines, trivial | ~0 | The table "flips back to source" — the exact thing to avoid |
| B | **Widget overlay, whole-table replace** — rendered widget, edits committed as a table-range replacement | Obsidian ≥1.5, CM6 `codemirror-markdown-tables`, Joplin Rich Tables | Widget owns the grid; commit = replace table range | High | Needs nested editors; breaks Vim/search/undo routing; scroll-jump; reveal toggle lost |
| C | **Per-cell buffer over the shared document** — subview holds the doc, hides everything outside the cell | Zettlr 4.x | Real offsets into the real doc; sync is transaction forwarding | Medium | Only one cell editable at a time; boundary filter needed; whitespace boundary bug |
| D | **Cell-level source reveal inside the grid** — grid stays rendered; the *active cell* shows its own source text, other cells stay rendered | Typora (inline-level), Zettlr (cell-level, described in its docs) | Cell → byte range in its source line; no whole-table replacement | Low | You must keep a per-cell byte mapping and re-derive it every frame |

Approaches **B** and **C** are both "a nested editor inside a block widget" — B hides it in the
widget, C hides it in a document-sized subview. Both are forced by CM6's single-column constraint
and neither maps onto a line-based `Vec<String>` buffer, because there is no need to nest anything
when a row is already a line and a cell is already a byte range inside it.

Approach **D** is the one that fits deepterm, and it is the approach Typora and Zettlr's *cell*
reveal already validate. deepterm additionally has a working precedent for the "render an image
instead of the source lines, and make the hidden source un-clickable/un-typable" discipline: the
mermaid image mode (`view.rs` `mermaid_image_ranges` / `clamp_cursor_out_of_image_blocks`).

---

## 11. Recommendation for deepterm

### 11.1 What the codebase already gives us

Read these before designing anything:

- `src/editor/markdown_live/layout.rs:78` — `RowPlan { kind, visible, runs, segs, links, revealed }`.
  `segs: Vec<Seg>` is a **source-range → visible-range correspondence** covering the source line
  exactly, and `visible` is always a concatenation of verbatim source slices ("never transformed
  text, so a `Seg` maps byte offsets between the two by plain arithmetic").
- `src/editor/markdown_live/layout.rs:393-431` — `vis_for_src` / `src_for_vis` / `byte_for_col` /
  `col_for_byte`. **The inverse mapping already exists and is already used for click→cursor**
  (`view.rs:1255-1272` in `on_line_mouse_down`).
- `src/editor/markdown_live/interactions.rs:320-384` — `table_cell_bounds` (code-span and `\|`
  aware byte ranges) and `table_cell_contents` (trimmed content char-cols).
- `src/editor/markdown_live/interactions.rs:390-456` — `table_nav` (Tab/Shift+Tab across the whole
  grid, appends a row past the last cell) and `table_insert_row_below` (Enter; keeps the delimiter
  row glued to the header). Already wired to keys at `view.rs:1554` and `view.rs:1576-1595`.
- `src/editor/markdown_live/element.rs:237-351` — `inline_cell_runs` / `inline_cell_runs_with_refs`:
  a table cell's raw source → (rendered string, runs). **This is a second inline renderer**, separate
  from `layout::Builder::inline`, and it returns no `segs`.
- `src/editor/markdown_live/view.rs:3806-3941` — `Group::Table`: builds the grid via
  `split_table_row`, `align_of`, `inline_cell_runs_with_refs`, one `div()` per cell, and jumps the
  cursor to the block start on click. Note it pushes `row_targets.push((first_line, last_line))`,
  which is what makes the whole-grid click land somewhere sane.
- `src/editor/markdown_live/view.rs:999-1095` — `line_in_mermaid_image` /
  `mermaid_image_ranges` / `clamp_cursor_out_of_image_blocks` / `set_mermaid_showing_code` /
  `toggle_show_source`. This is the existing "rendered image replaces source lines, and the source
  is a no-go zone" pattern, including the frame-synchronous cursor clamp.

The important structural fact: **the grid already draws every cell from the exact source line, and
the cursor's source column is already a cell-local byte offset.** The missing pieces are
(a) a per-cell byte↔visible mapping for the rendered cell text, and (b) letting the caret *live*
inside the grid instead of being ejected to the block start.

### 11.2 The recommendation: extend `Group::Table` with an "active cell" — Approach D, no nested editor

Keep the current `Group::Table` grid, the current `RowPlan` per source line, and the current
`table_nav` / `table_insert_row_below`. Add exactly three things:

**(1) Per-cell byte mapping.** Replace the per-cell call to `inline_cell_runs_with_refs` with a
planner that produces `(rendered, runs, segs)` for the cell's *content byte range* — i.e. reuse
`layout::Builder` logic rather than the parallel implementation in `element.rs`. The `segs` must be
offset by the cell's content start (`table_cell_bounds(line)[col]` plus the leading-whitespace
trim already computed by `table_cell_contents`) so they are absolute-in-line, exactly like a
`RowPlan`'s segs. Then:

- click in cell `(row, col)` → `src_for_vis(cell_segs, hit_byte)` → absolute line byte →
  `col_for_byte` → `model.move_to(line, col, false)`;
- caret paint → `vis_for_src(cell_segs, byte_for_col(src_line, cursor.col))` → byte offset inside
  the rendered cell string → `TextLayout::position_for_index`.

This is the *only* new mapping machinery, and it is the same machinery `LiveLine` already uses for
normal rows. `inline_cell_runs` can then be deleted, removing a duplicated renderer.

**(2) Reveal the active cell only.** `reveal.rs:220-243` currently expands any revealed line to the
whole contiguous `Table` run, which is exactly why the table flips to source. Change the rule for
tables to: **do not expand the run; reveal only the cell under the cursor.** Concretely, keep
`plan.revealed == false` for all table rows so the grid still draws, and let the *grid* decide, per
cell, whether to render that cell's text with `reveal = true` (markers dimmed, source visible) or
`false` (markers hidden). This is Zettlr's documented rule ("you see the Markdown source of the
cell … every cell that is not active will be rendered") and it satisfies the "no hidden editing"
constraint perfectly: the cell you are typing in shows you the characters you are typing, and every
other cell stays pretty.

Keep the existing escape hatch intact: the `revealed` / `!has_delimiter` branch at
`view.rs:3821-3837` already renders mono source rows when the block is not a real GFM table. That
branch is also the fallback if the per-cell mapping ever fails, and it is where the whole-block
source view lives. Wire it to the existing `show_source` toggle (the "源码" chip at `view.rs:4106`)
so users can still get the old plaintext-table behaviour — this is precisely the toggle Obsidian
users are asking for and never got (see §2 pitfall 1).

**(3) Make the grid focusable for the caret.** The caret currently paints only in `LiveLine`s. Two
options, in order of preference:

- **Preferred: give the active cell a `LiveLine`-shaped element.** Draw the active cell's text with
  the existing `LiveLine` element (it already handles runs, caret, block caret, hitbox, click and
  drag) using the cell's `segs`-derived `caret_vis`, and wrap it in the `<td>` chrome. Inactive
  cells stay `StyledText`. This reuses every bit of caret/selection/hit-test code and keeps
  `record_row_layout` / `line_for_click` semantics; you just need the row's `row_targets` entry to
  remain the table range (it already is) and `line_for_click` to resolve to the right line.
- **Fallback: compute the caret rect directly from the cell's `TextLayout`.** More code, duplicates
  `LiveLine::paint`.

Note the important interaction with `LiveLine`'s mouse handler (`element.rs:674-713`): it is a
*window-level* listener that claims the whole horizontal band of the row and maps x→byte via the
row layout. Inside a table the row layout is the *cell's* layout, so the handler's
`bounds.left()/right()` are the cell's — which is what you want, but it means the per-cell
`LiveLine` must be the element that owns the click for that cell, and the grid's own
`on_mouse_down` (`view.rs:3870-3876`) must stop claiming clicks once an active cell exists.

**What you do *not* need:** nested editors, a whole-table replacement commit, `Decoration`-like
range mapping, or a second document. Every edit is already a normal `model.insert` / `model.delete`
at a `Pos` inside one line, so undo, find, selection, and `after_edit`'s `LiveViewEvent::Change`
all keep working unchanged. This is the decisive advantage of Approach D over B/C here.

### 11.3 Concrete task list

1. `layout.rs`: add `pub fn plan_cell(cell_src: &str, base_offset: usize, reveal: bool, refs) -> (String, Vec<RunAttrs>, Vec<Seg>)`,
   factoring the `Builder` body so it can be driven over a sub-slice of the line. Keep `Seg.src`
   **absolute in the line** so it composes with `vis_for_src` / `src_for_vis` unchanged.
2. `element.rs`: delete `inline_cell_runs` / `inline_cell_runs_with_refs`; have callers use
   `plan_cell` + `runs_for_row`-style run refinement (extract the style-refinement match arm into a
   shared helper so cell styling and row styling cannot drift).
3. `reveal.rs`: for `Block::Table`, stop expanding the revealed run (keep `Math`/`FrontMatter`
   expansion as is). Add a unit test asserting a cursor in cell 2 of a table leaves every
   `plan.revealed == false`.
4. `view.rs` `Group::Table`: track `active_cell: Option<(usize /*line*/, usize /*col*/)>` derived
   from `buffer.cursor()` + `table_cell_bounds`. Render the active cell with `reveal = true` and a
   `LiveLine`; render the rest as now. Keep `caret_el_ix` pointing at the table element so
   `scroll_to_item` still works.
5. `view.rs`: on cell click, map to a byte via the cell's `segs` and `move_to(line, col, false)`
   instead of `move_to(start_line, 0, false)`. Guard the grid's blanket
   `on_mouse_down` so it only applies when there is no active cell.
6. `interactions.rs`: keep `table_nav` and `table_insert_row_below` as the Tab/Enter behaviour;
   they are already Typora-parity. Add `Esc` → move the cursor out of the table (to the line after
   the last row), matching Notion/`codemirror-markdown-tables`.
7. Optional, later: a row/column context menu reusing the existing slash-menu / context-menu
   infrastructure for insert/delete row+column and alignment (write `:---` / `:---:` / `---:` into
   the delimiter row, which is a one-line edit).

### 11.4 Pitfalls to handle explicitly

These are the ones that bite in *every* implementation surveyed, plus two concrete bugs I found in
deepterm's current table code.

**Universal pitfalls**
1. **Never let the caret sit in hidden source.** This is the invariant deepterm already enforces for
   mermaid (`clamp_cursor_out_of_image_blocks`, and the frame-synchronous clamp in
   `set_mermaid_showing_code`). Tables need the same discipline the moment any cell text is hidden:
   if the cursor's column falls inside a hidden `Seg` (e.g. a `**` marker), `vis_for_src` collapses
   it to the marker's start — which is correct — but the *edit* must still land on the right byte.
   Decide explicitly: either the active cell is always `reveal = true` (recommended — no hidden
   markers in the cell you are editing, so `vis_for_src`/`src_for_vis` are bijective), or you
   accept collapsed carets and must define typing behaviour at a collapsed position.
2. **Enter must not insert a newline.** GFM has no in-cell line break. Typora, Obsidian, Notion, and
   `codemirror-markdown-tables` all bind Enter to "cell below / new row". deepterm already does this
   (`table_insert_row_below`) — keep it, and keep the "delimiter row stays glued to the header"
   rule (test `table_insert_row_below_keeps_delimiter_glued`). If a user wants a line break, the
   ecosystem answer is a literal `<br>` (`Shift+Enter` in CM6's extension), which then needs the
   inverse mapping to keep the caret out of the middle of the tag (Joplin's
   `localToRootOffsets` solves exactly this).
3. **Whitespace vs the cell boundary.** Zettlr's documented bug: if the boundary is taken from the
   *trimmed* cell content, typing a space at the end puts the caret "outside the cell" and the next
   character is refused. Take the boundary from the untrimmed pipe-delimited span and trim only for
   caret *placement*.
4. **Pipes inside cells.** A literal `|` must be escaped `\|`, and *even inside a code span* GFM
   still splits on the pipe — "the backslash is eaten by the table parser, not by the code span"
   (<https://talk.commonmark.org/t/three-things-i-decided-not-to-parse-building-a-live-preview-editor/9126>).
   Joplin therefore escapes on insert (`escapeUnescapedPipesWithContext`, backslash-run aware).
   Whatever the renderer does, the *editor* must be consistent with it.
5. **Click precision into a rendered cell.** Joplin calls this out as their hardest bug ("placing
   the cursor accurately when clicking on a table cell … is tricky"); they added a dedicated
   `cellCaretHit.ts` and `WidgetType.coordsAt`. deepterm's advantage: `TextLayout::index_for_position`
   on the cell's own layout gives this for free, exactly as `LiveLine` already does for normal rows.
6. **Ragged rows.** GFM ignores extra cells and treats missing ones as empty; CM6's reference
   implementation deliberately tightens this ("This is stricter than the GFM spec … this doesn't
   work well for a table editor"). deepterm's grid currently takes `col_count = max(len)` over rows
   and pads with `""`, while the delimiter check requires `delimiter_cols == header_cols`. Pick one
   policy and make the grid, the nav, and the delimiter check agree.
7. **Auto-formatting is a data-loss risk.** Typora's prettifier rewriting hand-formatted tables is a
   long-standing complaint; Zettlr's first table editor shipped data loss. If you add column
   alignment/width normalization, make it explicit and reversible, never silent-on-every-keystroke.
8. **Vim / non-caret keybindings.** Obsidian's widget broke `5k`, `dd`, `o`. deepterm's Approach D
   keeps the caret in the same buffer, so this class of bug largely does not arise — a strong
   argument for D over B/C.

**Two concrete inconsistencies in deepterm's current table code** (worth fixing as part of this
work, because the new mapping will make them visible):

1. **Two different cell splitters.** The grid uses `split_table_row`
   (`block.rs:308`) — a naive `t.split('|')` with no code-span or `\|` awareness — while keyboard
   navigation uses `table_cell_bounds` (`interactions.rs:320`) — code-span and escape aware.
   So for `| \`a|b\` | c |` the *grid renders 3 columns* while *Tab navigates 2 cells*, and
   `table_delimiter_cols` (which also uses `split_table_row`) can fail the header/delimiter count
   check at `view.rs:3818` and dump the whole table to source rows. Unify on
   `table_cell_bounds` (or make `split_table_row` escape/code-span aware) before adding per-cell
   editing, or click mapping and Tab will disagree with the pixels.
2. **The rendered cell string is not derivable back to source.** `inline_cell_runs_with_refs`
   drops marker spans (`if span.marker { continue }`) and *substitutes* text for emoji
   (`push(glyph, …)`) and footnotes (`push(&format!("[{label}]"), …)`) without emitting any `Seg`.
   Emoji substitution changes byte length, so no arithmetic mapping is possible from the current
   output — which is why step (1) of the task list is to have the cell planner emit `segs` the way
   `layout::Builder::emit` does.

   The row-level path does **not** have this problem, and the difference is deliberate. `emit`
   copies `self.line[src.clone()]` verbatim, so every visible byte traces to a source byte; the
   emoji case is handled by *styling the token rather than swapping the glyph* — see the comment at
   `inline.rs:668-669`: "Typora swaps the glyph; the live editor styles the token since its mapping
   is byte-exact." `inline.rs:1090` even has a test pinning this (`emoji_tokens_flagged_not_replaced`).
   So the grid's substitution is a second, mapping-hostile renderer, and unifying the two is not
   just deduplication — it is what makes cell-level mapping possible at all. The same applies to the
   footnote `[label]` substitution: either emit the source bytes verbatim (styling the token) or
   emit a `Seg` that maps the whole construct to a single visible position, and decide explicitly
   where a caret inside it goes.

### 11.5 Why not the alternatives here

- **Whole-table widget + nested editor (B, = Obsidian/CM6/Joplin):** the nested editor exists only
  because CM6 forces "all content lines into a single column". A `Vec<String>` buffer has no such
  constraint — a table row *is* a line and a cell *is* a byte range. Adding a nested editor would
  mean a second text buffer, a second caret, a second undo stack, and a commit that replaces N
  source lines on every keystroke, for zero benefit. It would also break the `RowPlan` invariant
  that every visible byte traces to a source byte, and it would fight the existing
  `row_layouts` / `line_for_click` / `scroll_to_item` machinery.
- **Per-cell document subview (C, = Zettlr):** the subview exists to get correct offsets for free
  by reusing the real document. deepterm already has the offsets — `segs` and
  `table_cell_bounds` are exactly that — so the subview is pure overhead (Zettlr's own comment
  admits it becomes a problem on very large documents).
- **Source reveal (A, = current behaviour):** this is the thing being fixed.

### 11.6 Bottom line

Implement **Approach D**: keep the grid, add a `segs`-carrying per-cell planner, reveal the active
cell's own source inside the still-rendered grid, and route the caret through the existing
`vis_for_src` / `src_for_vis` pair. Reuse `table_nav` / `table_insert_row_below` for Tab/Enter,
reuse `show_source` for the escape hatch, and reuse the mermaid no-go-zone discipline for the
"caret never in hidden source" invariant. The only genuinely new code is the cell-level byte
mapping and the caret element for the active cell — everything else already exists in
`markdown_live`.

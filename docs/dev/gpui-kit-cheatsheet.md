
# gpui-kit v0.9.1 cheat-sheet for a SQL client (checked against the source)

**How to read this**
- Paths: `REF` = `~/.cargo/registry/src/*/gpui-component-0.9.1 (and gpui-base-0.9.1)`. `GPUI` = `C:\Users\joel.beckham\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\gpui-pre-0.3.12\src`.
- `file:line` points at the declaration line.
- Most widgets live in `gpui-base`. `gpui_kit::component::*` re-exports them.
- Imports used below:
  - `use gpui_kit::*;` gives GPUI: `Window`, `Context`, `App`, `Entity`, `div`, `px`, `KeyBinding`, `actions!`, `Task`, `Result`, and so on.
  - `use gpui_kit::component::{...}` gives the widgets.
  - Do **not** use `use gpui_kit::*` in test modules (see section 12).

---

## 0. App bootstrap (required by everything else)

```rust
// REF/kit/src/lib.rs:181
pub fn open_window<V: Render>(options: WindowOptions, cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> Result<(AnyWindowHandle, Entity<V>)>
// REF/kit/src/lib.rs:207
pub fn init(cx: &mut App)   // = gpui_component::init: theme, input keymap, table keymap, menus, ...
```

Minimal `main`, adapted from examples/dialog_overlay/src/main.rs:105-117 and examples/editor/src/main.rs:1193:

```rust
fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)   // see section 9: `Assets` = 106 icons only
        .run(|cx| {
            gpui_kit::init(cx);                     // MUST come before bind_keys/open_window
            cx.bind_keys([KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor > Input"))]);
            gpui_kit::open_window(TitleBar::window_options(), cx, |window, cx| cx.new(|cx| App_::new(window, cx)))
                .expect("open window");
        });
}
```

**Gotchas**
- `open_window` wraps your view in `base::Root`. The dialog, sheet, notification and tooltip layers mount automatically (REF/component/src/root.rs:711-779). You do not render the layers yourself.
- With no `.with_assets(...)`, icons render blank. hello_world sets no assets.

---

## 1. Code editor: `EditorState` + `Editor`

### Types and features
- `EditorState` is `pub type EditorState = InputBaseState<EditorMode>;` (REF/base/src/input/editor/mod.rs:11).
- It is re-exported as `gpui_kit::component::input::{Editor, EditorState, InputEvent, ...}` (REF/component/src/input/mod.rs:30-55).
- **Feature:** `gpui-kit = { version = "0.9.1", features = ["tree-sitter-sql"] }` (REF/kit/Cargo.toml). It pulls in `tree-sitter` and `tree-sitter-sequel`.
- Without the feature, the language name `"sql"` does not resolve. The text shows unhighlighted and no error is raised.

### Construction and config (REF/base/src/input/base/state.rs)

```rust
pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self                    // :12894
// Defaults: line numbers on, tab size 2 soft, indent guides on, searchable. "handles ... up to about 50K lines"
pub fn language(mut self, language: impl Into<SharedString>) -> Self              // :12904  e.g. "sql"
pub fn set_highlighter(&mut self, new_language: impl Into<SharedString>, cx: &mut Context<Self>) // :1095 change later
pub fn line_number(mut self, line_number: bool) -> Self                           // :12986 (#[doc(hidden)] but pub)
pub fn set_line_number(&mut self, line_number: bool, _: &mut Window, cx: &mut Context<Self>) // :12994
pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self        // :1089
pub fn default_value(mut self, value: impl Into<SharedString>) -> Self            // :1633
pub fn soft_wrap(mut self, wrap: bool) -> Self                                    // :12772
pub fn set_readonly(&mut self, readonly: bool, cx: &mut Context<Self>)            // :1457
pub fn tab_size(mut self, tab: TabSize) -> Self  // REF/base/src/input/editor/indent.rs:501; TabSize{tab_size: usize, hard_tabs: bool}
pub fn indent_guides(mut self, indent_guides: bool) -> Self                       // indent.rs:174
pub fn submit_on_enter(mut self, submit: bool) -> Self                            // :1496 (plain Enter = submit, Shift+Enter = newline)
```

### Get and set text

```rust
pub fn text(&self) -> &Rope                       // :1679  borrowed, no copy
pub fn value(&self) -> SharedString               // :1649  owned String copy
pub fn set_value(&mut self, value: impl Into<InputContent>, window: &mut Window, cx: &mut Context<Self>) // :1281
    // InputContent: From<&str, String, &String, ...> (REF/base/src/input/base/inline_tokens.rs:148)
pub fn replace_all(&mut self, text: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) // :1314 (undoable)
pub fn insert(&mut self, text: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>)      // :1342 at cursor
pub fn replace(&mut self, text: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>)     // :1361 replaces selection
```

**Gotcha:** `set_value` clears the undo history. It also sets `emit_events = false` (state.rs:1290-1295), so it does **not** emit `InputEvent::Change`.

### Cursor and selection (UTF-8 byte offsets)

```rust
pub fn cursor(&self) -> usize                                // :3496 byte offset of caret
pub fn cursor_position(&self) -> Position                    // :1709 lsp_types::Position (0-based line/character)
pub fn set_cursor_position(&mut self, position: impl Into<Position>, window: &mut Window, cx: &mut Context<Self>) // :1717 (also focuses)
pub fn selected_range(&self) -> std::ops::Range<usize>       // :3564 empty => start==end==cursor()
pub fn selected_text(&self) -> RopeSlice<'_>                 // :4242
pub fn selected_value(&self) -> SharedString                 // :1658 owned
pub fn set_selected_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) // :3588
pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>)                  // :1732
```

### Events (state.rs:138; `EventEmitter<InputEvent>` and `EventEmitter<InputEditsEvent>` at :747-748)

```rust
pub enum InputEvent { Change, PressEnter { secondary: bool, shift: bool }, Focus, Blur }
// InputEditsEvent::edits(&self) -> &[(InputEdit, Rope)]  (:157), emitted before Change
```

### Font
- `Editor` implements `Styled` (REF/component/src/input/editor.rs:142).
- By default it renders with `cx.theme().mono_font_family` / `mono_font_size` and line-height 1.5×font (editor.rs:155-157).
- Set `.font_family(..)` / `.text_size(..)` on the element to override (editor.rs:150-157). To change the font globally, edit the `Theme` fields `mono_font_family` / `mono_font_size` (theme/mod.rs:185-187).

### `Editor` element (REF/component/src/input/editor.rs)

```rust
pub fn new(state: &Entity<EditorState>) -> Self      // :40
pub fn h(mut self, height: impl Into<DefiniteLength>) -> Self   // :58 (inherent; example uses .h(relative(1.)))
pub fn bordered(mut self, bordered: bool) -> Self    // :68
pub fn readonly(mut self, readonly: bool) -> Self    // :83
pub fn context_menu(mut self, f: impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static) -> Self // :109
```

### Snippet

Adapted from examples/editor/src/main.rs:695-733 and :1167-1174.

```rust
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
let editor = cx.new(|cx| EditorState::new(window, cx)
    .language("sql").line_number(true).placeholder("SELECT ...").default_value("SELECT 1;"));
let sub = cx.subscribe(&editor, |this, editor, _: &InputEvent, cx| {   // Context::subscribe GPUI/app/context.rs:93
    let st = editor.read(cx);
    this.cursor = st.cursor();                  // byte offset
    this.sel = st.selected_range();             // Range<usize>
    this.sel_text = st.selected_value();        // SharedString
});
// render:
Editor::new(&self.editor).bordered(false).h(relative(1.)).font_family("JetBrains Mono").text_size(px(13.))
// later:
self.editor.update(cx, |s, cx| s.set_value("SELECT * FROM t", window, cx));
```

### Completion provider

The trait is in REF/base/src/input/editor/lsp/completions.rs:146:

```rust
pub trait CompletionProvider {
    fn completions(&self, text: &Rope, offset: usize, trigger: CompletionContext,
                   window: &mut Window, cx: &mut App) -> Task<Result<CompletionResponse>>;   // required
    fn inline_completion(&self, _rope: &Rope, _offset: usize, _trigger: InlineCompletionContext,
                   _window: &mut Window, _cx: &mut App) -> Task<Result<InlineCompletionResponse>> { /* empty */ }
    fn inline_completion_debounce(&self) -> Duration { 300ms }
    fn resolve_completions(&self, _: Vec<usize>, _: Rc<RefCell<Box<[Completion]>>>, _: &mut App) -> Task<Result<bool>> { Ok(false) }
    fn is_completion_trigger(&self, offset: usize, new_text: &str, cx: &mut App) -> bool;    // required, main thread
}
```

**Registration:** set `state.lsp_mut().completion_provider = Some(Rc::new(p))`.
- `lsp_mut` is at REF/base/src/input/editor/mod.rs:216.
- The field is `pub completion_provider: Option<Rc<dyn CompletionProvider>>` (lsp/mod.rs:41).
- Other fields on the same struct: `hover_provider`, `code_action_providers`, `definition_provider`, `completion_menu: CompletionMenuOptions { max_width }`.

**Items:** return `lsp_types::CompletionResponse::Array(Vec<CompletionItem>)` (or `List`). Types come from `lsp-types` **0.97** (workspace Cargo.toml:94, features `["proposed"]`).
- gpui-kit re-exports only `Position`, so add `lsp-types = "0.97"` yourself.
- The version must match, or the trait types will not line up.

**Behaviour:**
- After each typed edit, `is_completion_trigger(start, new_text)` is called. If it returns `true`, `completions` is called with `trigger.trigger_character = Some(query)`, where `query` is the word typed since the menu opened (completions.rs:283-300).
- **The menu does not filter.** It shows exactly what you return (completions.rs:396-405), so filter by the query yourself.
- What gets inserted (lsp/overlay.rs:166-191):
  - With no `text_edit`/`insert_text`, the item's `label` replaces the typed prefix.
  - `insert_text` is **inserted at the cursor without replacing** the prefix.
  - `text_edit` uses its own range.
- To open the menu manually, dispatch `ShowCompletions`. It has no default key. Example: `KeyBinding::new("ctrl-space", ShowCompletions, Some("Input && mode == editor"))` (doc at completions.rs:314-318), or call `state.show_completions(window, cx)`.

Adapted from examples/editor/src/main.rs:160-261 and :709-717:

```rust
struct SqlCompleter { words: Arc<Vec<String>> }
impl CompletionProvider for SqlCompleter {
    fn completions(&self, _t: &Rope, _off: usize, trigger: lsp_types::CompletionContext,
                   _w: &mut Window, cx: &mut App) -> Task<Result<lsp_types::CompletionResponse>> {
        let q = trigger.trigger_character.unwrap_or_default().to_uppercase();
        let words = self.words.clone();
        cx.background_spawn(async move {
            let items = words.iter().filter(|w| w.starts_with(&q)).take(50)
                .map(|w| lsp_types::CompletionItem { label: w.clone(), ..Default::default() }).collect();
            Ok(lsp_types::CompletionResponse::Array(items))
        })
    }
    fn is_completion_trigger(&self, _o: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
editor.update(cx, |s, cx| { s.lsp_mut().completion_provider = Some(Rc::new(SqlCompleter{..})); cx.notify(); });
```

### Ctrl-Enter and keybindings while the editor has focus
- The editor element's key context is `"Input"` (state.rs:162, :5156). The editor overrides it to `"Input mode=editor"` (editor/mod.rs:199).
- Built-in bindings in `"Input"` (state.rs:164-339):
  - `enter` → `Enter{secondary:false, shift:false}`
  - `shift-enter` → `Enter{secondary:false, shift:true}`
  - **`ctrl-enter` (cmd-enter on mac) → `Enter{secondary:true, shift:false}`**
  - `escape`, `tab`/`shift-tab`, `ctrl-a/c/x/v/z/y`, `ctrl-f` (Search), `ctrl-h` (Replace), `ctrl-.` (code actions), `ctrl-]`/`ctrl-[`, `ctrl-home`/`ctrl-end`, and more.
- **So yes, the editor consumes ctrl-enter.** In multi-line mode (with `submit_on_enter` false) it **inserts a newline** and emits `PressEnter{secondary:true,..}`. It does not propagate (state.rs:2373-2487).
- GPUI precedence: a deeper context depth wins. At equal depth, the **later-added** binding wins (GPUI/keymap.rs:154-159, 216-217).

Working options:
1. **Recommended:** after `gpui_kit::init(cx)`, call `cx.bind_keys([KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor > Input"))])`. Put `.key_context("SqlEditor").on_action(cx.listener(Self::run_statement))` on an ancestor `div` of the `Editor`.
   - `A > B` evaluates at B's depth (GPUI/keymap/context.rs:258-305). Because yours is bound later, it beats the built-in binding.
   - `RunStatement` is unhandled by the input, so it bubbles to your `on_action`.
2. Alternative: `.capture_action(cx.listener(|this, a: &gpui_kit::component::input::Enter, w, cx| if a.secondary { cx.stop_propagation(); this.run(w, cx) }))` on an ancestor div. `capture_action` is at GPUI/elements/div.rs:1139. `Enter` is re-exported from `component::input`, and its fields are pub (state.rs:59).

---

## 2. Large virtual grid: `DataTable` / `TableState<D>` / `TableDelegate`

Module: `gpui_kit::component::table::{Column, ColumnFixed, ColumnSort, DataTable, TableDelegate, TableEvent, TableSelection, TableState}`.

### Trait (REF/component/src/table/delegate.rs:16)

Required:
```rust
fn columns_count(&self, cx: &App) -> usize;                                   // :18
fn rows_count(&self, cx: &App) -> usize;                                      // :21
fn column(&self, col_ix: usize, cx: &App) -> Column;                          // :26  "only call on Table prepare or refresh"
fn render_td(&mut self, row_ix: usize, col_ix: usize, window: &mut Window,
             cx: &mut Context<TableState<Self>>) -> impl IntoElement;          // :135
```

Optional, with defaults:
```rust
fn render_th(&mut self, col_ix, window, cx) -> impl IntoElement               // :102
fn render_tr(&mut self, row_ix, window, cx) -> Stateful<Div>                  // :114
fn context_menu(&mut self, row_ix: usize, menu: PopupMenu, window, cx) -> PopupMenu // :124
fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window, cx)       // :29
fn fit_column(&mut self, col_ix, window, cx) -> Option<Pixels>                // :54 (double-click on resize handle)
fn move_column(&mut self, col_ix, to_ix, window, cx)                          // :176
fn render_empty / loading(&self, cx)->bool / render_loading(..)               // :186/:200/:207
fn has_more(&self, cx: &App) -> bool { false }                                // :219
fn load_more_threshold(&self) -> usize { 20 }                                 // :228
fn load_more(&mut self, window: &mut Window, cx: &mut Context<TableState<Self>>) {} // :238
fn visible_rows_changed(&mut self, visible_range: Range<usize>, window, cx)   // :258
fn visible_columns_changed(&mut self, visible_range: Range<usize>, window, cx) // :272
fn cell_text(&self, row_ix, col_ix, cx: &App) -> String                       // :284 (used by dump/dump_range)
fn cell_label / row_label (a11y)                                              // :149/:159
fn group_headers(&self, cx) -> Option<Vec<Vec<ColumnGroup>>>                  // :75
```

### `TableState` (REF/component/src/table/state.rs)

```rust
pub fn new(delegate: D, _: &mut Window, cx: &mut Context<Self>) -> Self   // :313
pub fn delegate(&self) -> &D / delegate_mut(&mut self) -> &mut D          // :355/:360
// builders: loop_selection :365, col_movable :371, col_resizable :377, sortable :383, row_selectable :389,
//           col_selectable :402, cell_selectable(bool) :424 (default false), row_header(bool) :439
pub fn refresh(&mut self, cx: &mut Context<Self>)          // :445 re-reads columns (call after column set changes)
pub fn scroll_to_row(&mut self, row_ix: usize, cx: &mut Context<Self>)   // :450
pub fn selection(&self) -> TableSelection                  // :523  None|Row(r)|Column(c)|Cell(r,c)
pub fn selected_row / selected_cell / set_selected_cell(row, col, cx) / clear_selection // :539/:618/:637/:665
pub fn visible_range(&self) -> &TableVisibleRange          // :677  .rows() / .cols()
pub fn dump_range(&self, range: Range<usize>, cx: &App) -> (Vec<String>, Vec<Vec<String>>) // :713
pub fn col_width(&self, col_ix: usize) -> Option<Pixels>   // :1312
```

`TableEvent` (state.rs:78): `SelectRow(usize), DoubleClickedRow(usize), SelectColumn(usize), SelectCell(r,c), DoubleClickedCell(r,c), ColumnWidthsChanged(Vec<Pixels>), MoveColumn(from,to), RightClickedRow(Option<usize>), RightClickedCell(r,c), ClearSelection`.

### `DataTable` element (data_table.rs)
- `new(&Entity<TableState<D>>)` :110
- `.stripe(bool)` :126
- `.bordered(bool)` :140
- `.scrollbar_visible(v,h)` :168
- `.header_row_height(px)` :151
- `.fixed_cols_border(bool)` :162
- `Sizable` (row height comes from `Size::table_row_height`)
- Key context `"DataTable"` (:16). It binds up/down/left/right/home/end/pageup/pagedown/escape.

### `Column` (column.rs)
- `Column::new(key, name)` :91. Defaults: width 100px, resizable, movable, min 20, max 1200 (:70-87).
- `.width(px)` :159, `.fixed_left()` :171 / `.fixed_right()` :179 / `.fixed(ColumnFixed)` :165, `.resizable(bool)` :185, `.movable(bool)` :191, `.sortable()` :110, `.text_right()` :141, `.min_width` / `.max_width`.

### Snippet

Adapted from crates/story/src/stories/data_table_story.rs:489-905 and :1068-1070.

```rust
struct ResultGrid { cols: Vec<String>, rows: Vec<Vec<SharedString>>, loading_more: bool, eof: bool }
impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize { self.cols.len() }
    fn rows_count(&self, _: &App) -> usize { self.rows.len() }
    fn column(&self, ix: usize, _: &App) -> Column { Column::new(ix.to_string(), self.cols[ix].clone()).width(px(140.)) }
    fn render_td(&mut self, r: usize, c: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        self.rows[r][c].clone()
    }
    fn cell_text(&self, r: usize, c: usize, _: &App) -> String { self.rows[r][c].to_string() }
    fn has_more(&self, _: &App) -> bool { !self.loading_more && !self.eof }
    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.loading_more = true; /* spawn fetch; on completion: delegate_mut().rows.extend(..); loading_more=false; cx.notify() */
    }
    fn visible_rows_changed(&mut self, range: Range<usize>, _: &mut Window, _: &mut Context<TableState<Self>>) { /* page-in */ }
}
let table = cx.new(|cx| TableState::new(ResultGrid{..}, window, cx).cell_selectable(true).row_header(false));
let _s = cx.subscribe_in(&table, window, |this, _t, ev: &TableEvent, window, cx| { /* match ev */ });
// render: DataTable::new(&self.table).stripe(true)
// rows appended:
table.update(cx, |t, cx| { t.delegate_mut().rows.extend(batch); cx.notify(); });
// columns replaced:
table.update(cx, |t, cx| { *t.delegate_mut() = new_grid; t.refresh(cx); cx.notify(); });
```

### Answers to your specific questions
- **Row-count changes:** no special API. Mutate the delegate and call `cx.notify()` on the `TableState`. `rows_count` is re-read on every render (state.rs:2786).
- **Column changes:** call `refresh(cx)`. This rebuilds the column groups from `column()`, so persist user widths from `ColumnWidthsChanged` into your `Column::width`, or they may be lost.
- **Visible range:** use `visible_rows_changed` / `visible_columns_changed`, or poll `visible_range()`.
  - These hooks run inside the table's render processor (state.rs:1514-1547, 2856-2890). Keep them fast.
  - Do not `update` the `TableState` entity from inside them, because you are already inside its update.
- **Load-more:** it fires when `visible_end >= rows_count - threshold && has_more()` (state.rs:1492-1511). It is re-scheduled on every frame while that holds, so guard with a loading flag as the story does.
- **Copy:** **there is no built-in copy action** in `TableState`/`DataTable`. Bind your own action in the `"DataTable"` context, for example `KeyBinding::new("ctrl-c", CopyCell, Some("DataTable"))`. Handle it with `on_action` on an ancestor div: read `table.read(cx).selection()` and call `cx.write_to_clipboard(ClipboardItem::new_string(s))`.
- **Selection and right-click:** `cell_selectable(true)` emits the `SelectCell`/`DoubleClickedCell`/`RightClickedCell` events. A row context menu comes from `TableDelegate::context_menu`.
- **Column resize:** on by default. Disable per table with `col_resizable(false)` or per column with `.resizable(false)`.
- **Fixed columns:** `.fixed_left()` / `.fixed_right()` on a `Column`.
- **1,000,000+ rows:**
  - The body is a GPUI `uniform_list` (state.rs:2856), so only visible rows call `render_td`. There is no per-row allocation in the table. With `stripe(true)` it adds only filler rows to fill the viewport.
  - Memory is all yours, in the delegate.
  - `dump()` materializes everything. Use `dump_range` instead.
  - Row height is fixed (`Size::table_row_height`).
  - Caveat to verify, not established: total height = rows × row_height in f32 pixels. At 1M rows × ~30px that is past 2^24, so scroll offsets may lose sub-pixel precision.

---

## 3. Tree: `Tree` / `TreeState` / `TreeItem` / `TreeEntry`

Module: `gpui_kit::component::tree::{tree, Tree, TreeEntry, TreeEvent, TreeItem, TreeState}`. These are re-exports of the gpui-base types (REF/component/src/tree.rs:15).

```rust
// REF/base/src/tree.rs
pub struct TreeItem { pub id: SharedString, pub label: SharedString, pub children: Vec<TreeItem>, /* Rc<RefCell<state>> */ } // :41
pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self  // :99
pub fn child(mut self, child: TreeItem) -> Self                                   // :111
pub fn children(mut self, children: impl IntoIterator<Item = TreeItem>) -> Self   // :116
pub fn expanded(self, expanded: bool) -> Self / disabled(self, bool)              // :121/:126
pub fn is_folder(&self) -> bool { !self.children.is_empty() }                     // :132
// TreeEntry: item() :61, depth() :66, is_folder() :76, is_expanded() :81
pub enum TreeEvent { Expanded(SharedString), Collapsed(SharedString) }            // :93
// TreeState
pub fn new(cx: &mut App) -> Self                                                  // :204
pub fn items(mut self, items: impl Into<Vec<TreeItem>>) -> Self                   // :216
pub fn set_items(&mut self, items: impl Into<Vec<TreeItem>>, cx: &mut Context<Self>) // :221 (RESETS selection)
pub fn selected_index(&self) -> Option<usize> / set_selected_index(Option<usize>, cx) // :228/:232
pub fn set_selected_item(&mut self, item: Option<&TreeItem>, cx)                  // :237
pub fn selected_item(&self) -> Option<&TreeItem> / selected_entry() / entry(ix)   // :250/:255/:259
pub fn reveal_item(&mut self, id: &SharedString, strategy: ScrollStrategy, cx)    // :275
pub fn focus(&mut self, window: &mut Window, cx: &mut App)                        // :287
// REF/component/src/tree.rs
pub fn tree<R>(state: &Entity<TreeState>, render_item: R) -> Tree                 // :18
   where R: Fn(usize, &TreeEntry, bool /*selected*/, &mut Window, &mut App) -> ListItem + 'static
pub fn context_menu<F>(mut self, f: F) -> Self                                    // :57
   where F: Fn(usize, &TreeEntry, PopupMenu, &mut Window, &mut Context<TreeState>) -> PopupMenu + 'static
```

Snippet, adapted from crates/story/src/stories/tree_story.rs:231-273 and examples/editor/src/main.rs:922-970:

```rust
let view = cx.entity();
tree(&self.tree_state, move |ix, entry, _selected, _w, cx| {
    view.update(cx, |_, cx| {
        let icon = if !entry.is_folder() { IconName::File } else if entry.is_expanded() { IconName::FolderOpen } else { IconName::Folder };
        let id = entry.item().id.clone();
        ListItem::new(ix).w_full().px_2().pl(px(16.) * entry.depth() + px(8.))
            .child(h_flex().gap_2().child(icon).child(entry.item().label.clone()))
            .on_click(cx.listener(move |this, ev: &ClickEvent, w, cx| {
                if ev.click_count() == 2 { this.open_object(&id, w, cx) }   // ClickEvent::click_count GPUI/interactive.rs:424
            }))
    })
})
.context_menu(|_ix, entry, menu, _w, _cx| menu.menu("Select top 100", Box::new(SelectTop)).separator().menu("Refresh", Box::new(Refresh)))
```

**Lazy loading**
- An item is a folder only if it has children. Give unloaded nodes a placeholder child, e.g. `TreeItem::new(format!("{id}/…"), "Loading…")`.
- Subscribe with `cx.subscribe(&tree_state, |this, _, ev: &TreeEvent, cx| if let TreeEvent::Expanded(id) = ev { ... })`.
- When the data arrives, rebuild your roots with the real children (`.expanded(true)`) and call `set_items`.
- `set_items` clears selection (:221-226), so restore it with `set_selected_item` / `set_selected_index`.
- Expanded state lives in an `Rc<RefCell>` shared by clones of a `TreeItem`. Reusing the same items keeps their expansion.

**Gotchas**
- A single click on a row selects it **and toggles a folder** (`on_entry_click` :441).
- A double-click therefore toggles twice. Prefer double-click only for leaves, or handle expansion yourself.
- No selection event is emitted. Read `selected_index()` in a `ListItem::on_click`, or use `cx.observe(&tree_state, ..)`.
- Keys: the `"Tree"` context binds only up/down/left/right (:20-27). **Enter is not bound.** The internal `Confirm` handler (:378) needs a binding: `KeyBinding::new("enter", gpui_kit::base::actions::Confirm{secondary:false}, Some("Tree"))`. Or bind your own action with `Some("Tree")` and read `selected_item()`.
- Icons per item: chosen in your `render_item` closure. There is no icon field on `TreeItem`.
- The flattened list is virtualized (`uniform_list`, :449), but entries deep-clone visible items, children included (`add_entry` :335).

---

## 4. Tabs: `TabBar` / `Tab`

Module: `gpui_kit::component::tab::{Tab, TabBar, TabVariant}`.

```rust
// REF/component/src/tab/tab.rs
pub fn new() -> Self                                 // :624
pub fn label(mut self, label: impl Into<SharedString>) -> Self   // :629
pub fn icon(mut self, icon: impl Into<Icon>) -> Self             // :645
pub fn prefix(mut self, prefix: impl IntoElement) -> Self        // :687
pub fn suffix(mut self, suffix: impl IntoElement) -> Self        // :693
pub fn disabled(mut self, disabled: bool) -> Self                // :699
pub fn on_click(mut self, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self // :705
// also Selectable::selected(bool), Styled, ParentElement; From<&'static str|String|SharedString|Icon|IconName>
// REF/component/src/tab/tab_bar.rs
pub fn new(id: impl Into<ElementId>) -> Self                     // :68
pub fn selected_index(mut self, index: usize) -> Self            // :173
pub fn on_click<F>(mut self, on_click: F) -> Self where F: Fn(&usize, &mut Window, &mut App) + 'static // :187 (overrides tabs' on_click)
pub fn child / children / prefix / suffix / menu(bool) / max_width(px) / track_scroll(&ScrollHandle) // :167/:161/:149/:155/:125/:133/:143
// variants: .underline() .pill() .outline() .segmented() .folder()
```

- **There is no `closable` API.** Put a close `Button` in `.suffix(...)`, as the stories do.
- `Button::on_click` does **not** stop propagation (button.rs:875-884). Call `cx.stop_propagation()` in the close handler, or the tab click fires too.

Snippet, adapted from crates/story/src/stories/tabs_story.rs:372-395:

```rust
TabBar::new("sql-tabs").selected_index(self.active)
    .on_click(cx.listener(|this, ix: &usize, _, cx| { this.active = *ix; cx.notify(); }))
    .children(self.tabs.iter().enumerate().map(|(ix, t)| {
        Tab::new()
            .prefix(div().size_2().rounded_full().bg(t.color))                     // colored indicator
            .label(t.title.clone())
            .suffix(Button::new(("close", ix)).ghost().xsmall().icon(IconName::Close)
                .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.close_tab(ix, cx); })))
    }))
    .suffix(Button::new("new-tab").ghost().xsmall().icon(IconName::Plus))
```

---

## 5. Layout: resizable splits

Module: `gpui_kit::component::resizable::{h_resizable, v_resizable, resizable_panel, ResizableState, ResizablePanelEvent}`.

```rust
pub fn h_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup   // REF/component/src/resizable.rs:29
pub fn v_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup   // :34
pub fn resizable_panel() -> ResizablePanel                             // REF/base/src/resizable/mod.rs:29
// ResizablePanelGroup (REF/base/src/resizable/panel.rs)
pub fn with_state(mut self, state: &Entity<ResizableState>) -> Self    // :64 (else internal state)
pub fn child(mut self, panel: impl Into<ResizablePanel>) -> Self       // :80  (From<T: Into<AnyElement>>, From<ResizablePanelGroup>)
pub fn on_resize(mut self, on_resize: impl Fn(&Entity<ResizableState>, &mut Window, &mut App) + 'static) -> Self // :108
// ResizablePanel
pub fn size(mut self, size: impl Into<Pixels>) -> Self                 // :282 initial
pub fn size_range(mut self, range: impl Into<Range<Pixels>>) -> Self   // :290
pub fn fixed(mut self) -> Self                                         // :302 keep size when container resizes
pub fn visible(mut self, visible: bool) -> Self                        // :276
// ResizableState: sizes() -> &Vec<Pixels> (mod.rs:58), resize_panel(ix, size, window, cx) (:71); ResizableState::default()
```

Snippet, adapted from crates/story/src/stories/resizable_story.rs:83-108 and examples/editor/src/main.rs:1160-1175:

```rust
h_resizable("main")
    .child(resizable_panel().size(px(260.)).size_range(px(160.)..px(600.)).fixed().child(self.render_tree(window, cx)))
    .child(
        v_resizable("work")
            .child(resizable_panel().size(px(300.)).child(Editor::new(&self.editor).h(relative(1.)).into_any_element()))
            .child(resizable_panel().child(DataTable::new(&self.table)))
    )
```

**Gotchas**
- Pass `RenderOnce` components with `.into_any_element()` if inference fails.
- A panel without `.size()` gets an average share.
- Avoid `.overflow_hidden()` on panels. It clips the handle (doc at panel.rs:~240).
- The group must sit in a container with definite size (e.g. `size_full()`).

---

## 6. Actions and keybindings

```rust
// gpui_kit::actions!  (REF/kit/src/lib.rs:53) — derives Clone, PartialEq, Default, Debug, Action
actions!(sql_client, [RunStatement, RunAll, CancelQuery]);
// KeyBinding::new<A: Action>(keystrokes: &'static str, action: A, context: Option<&str>) -> Self   GPUI/keymap/binding.rs:35
// App::bind_keys(&mut self, bindings: impl IntoIterator<Item = KeyBinding>)                        GPUI/app.rs:2732
// InteractiveElement::key_context<C: TryInto<KeyContext>>(self, C) -> Self                           GPUI/elements/div.rs:854
// InteractiveElement::on_action<A: Action>(self, listener: impl Fn(&A, &mut Window, &mut App) + 'static) GPUI/elements/div.rs:1153
// Context::listener(&self, impl Fn(&mut T, &E, &mut Window, &mut Context<T>)) -> impl Fn(&E,&mut Window,&mut App)  GPUI/app/context.rs:241
// App::on_action<A: Action>(&mut self, listener: impl Fn(&A, &mut App)) -> &mut Self   (global)  GPUI/app.rs:2751
```

```rust
cx.bind_keys([KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor > Input")),
              KeyBinding::new("f5", RunAll, Some("SqlApp"))]);
// render:
div().id("root").key_context("SqlApp").track_focus(&self.focus_handle).size_full()
    .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_all(window, cx)))
    .child(div().key_context("SqlEditor").on_action(cx.listener(Self::run_statement)).child(Editor::new(&self.editor)))
fn run_statement(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) { ... }
```

**Gotchas**
- Bind after `gpui_kit::init` so your bindings win ties.
- Actions dispatch only along the focused element's ancestor chain. With nothing focused, nothing fires, so focus something at startup (the editor example uses `window.defer`, main.rs:721-724).
- Built-in context names: `"Input"` (attribute `mode=editor` on Editor), `"DataTable"`, `"Tree"`.
- Built-in keys that may collide (Input context): `ctrl-a`, `ctrl-f`, `ctrl-h`, `ctrl-.`, `ctrl-]`, `ctrl-[`, `ctrl-z`, `ctrl-y`, `ctrl-enter`.

---

## 7. Async: channel receiver, update the entity, 100ms ticker

```rust
// GPUI/app/context.rs:225
pub fn spawn<AsyncFn, R>(&self, f: AsyncFn) -> Task<R>
    where AsyncFn: AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R + 'static, R: 'static
// GPUI/app/context.rs:649 (when you need Window)
pub fn spawn_in<AsyncFn, R>(&self, window: &Window, f: AsyncFn) -> Task<R>
    where AsyncFn: AsyncFnOnce(WeakEntity<T>, &mut AsyncWindowContext) -> R + 'static
// WeakEntity::update<C: AppContext, R>(&self, cx: &mut C, f: impl FnOnce(&mut T, &mut Context<T>) -> R) -> Result<R>  GPUI/app/entity_map.rs:883
// WeakEntity::update_in(&self, cx, f: impl FnOnce(&mut T, &mut Window, &mut Context<T>) -> R) -> Result<R>             :899
// AsyncApp::background_executor(&self) -> &BackgroundExecutor   GPUI/app/async_context.rs:159
// BackgroundExecutor::timer(&self, duration: Duration) -> Task<()>  GPUI/executor.rs:200
// Context::background_spawn (used in examples/editor/src/main.rs:817)
```

Pattern; the timer loop mirrors crates/story/src/stories/data_table_story.rs:1072-1080:

```rust
fn start(&mut self, mut rx: tokio::sync::mpsc::UnboundedReceiver<QueryEvent>, cx: &mut Context<Self>) {
    self._rx_task = cx.spawn(async move |this, cx| {
        while let Some(ev) = rx.recv().await {          // tokio mpsc recv is executor-agnostic
            if this.update(cx, |view, cx| { view.apply(ev); cx.notify(); }).is_err() { break; } // entity dropped
        }
    });
    self._tick_task = cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(Duration::from_millis(100)).await;
        let running = this.update(cx, |v, cx| { cx.notify(); v.running }).unwrap_or(false);
        if !running { break; }
    });
}
```

**Gotchas**
- Store the `Task` in a field (dropping it cancels the task) or call `.detach()`.
- `this.update(..)` returns `Result`. Do **not** `.await` it; the skill doc gpui/test-reference.md:157 wrongly does.
- Mutating entity state does not redraw by itself. Call `cx.notify()`.
- For elapsed time, `cx.background_executor().now()` (executor.rs:~193) is faked in tests, whereas `Instant::now()` is not.

---

## 8. Theme

```rust
pub trait ActiveTheme { fn theme(&self) -> &Theme; }   // REF/component/src/theme/mod.rs:44, impl for App (:48)
pub fn change(mode: impl Into<ThemeMode>, _window: Option<&mut Window>, cx: &mut App)   // :398
pub enum ThemeMode { #[default] Light, Dark }          // :817
pub fn update<R>(cx: &mut App, edit: impl FnOnce(&mut Theme) -> R) -> R                 // :297 (preferred edit)
pub fn is_dark(&self) -> bool                          // :349
// Theme derefs to ThemeColor (:229), plus fields: font_family, font_size, mono_font_family, mono_font_size, radius, radius_lg, mode, tokens
```

Usage:
```rust
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
Theme::change(ThemeMode::Dark, Some(window), cx);
div().bg(cx.theme().background).text_color(cx.theme().muted_foreground).border_color(cx.theme().border)
```

`ThemeColor` fields (all `Hsla`, REF/component/src/theme/theme_color.rs):
- **Base:** `background`, `foreground`, `border`, `muted`, `muted_foreground`, `accent`, `accent_foreground`, `primary`, `primary_foreground`, `primary_hover`, `primary_active`, `secondary`, `secondary_foreground` (+`_hover`/`_active`).
- **Status:** `danger`, `danger_foreground`, `danger_hover`, `danger_active`, `success`, `success_foreground`, `success_hover`, `success_active`, `warning`, `warning_foreground`, `warning_hover`, `warning_active`, `info`, `info_foreground`, `info_hover`, `info_active`.
- **Surfaces:** `ring`, `selection`, `caret`, `input`, `link`, `popover`, `popover_foreground`, `overlay`, `window_border`.
- **Sidebar:** `sidebar`, `sidebar_foreground`, `sidebar_border`, `sidebar_accent`, `sidebar_accent_foreground`, `sidebar_primary`, `sidebar_primary_foreground`.
- **Tabs:** `tab`, `tab_active`, `tab_active_foreground`, `tab_bar`, `tab_bar_segmented`, `tab_foreground`.
- **Table:** `table`, `table_active`, `table_active_border`, `table_even`, `table_head`, `table_head_foreground`, `table_hover`, `table_row_border`, `table_foot`, `table_foot_foreground`.
- **List:** `list`, `list_active`, `list_active_border`, `list_even`, `list_head`, `list_hover`.
- **Chrome:** `title_bar`, `title_bar_border`, `status_bar`, `status_bar_border`, `scrollbar`, `scrollbar_thumb`, `scrollbar_thumb_hover`.
- **Palette:** `red`, `red_light`, `green`, `green_light`, `blue`, `blue_light`, `yellow`, `yellow_light`, `magenta`, `magenta_light`, `cyan`, `cyan_light`.
- **Charts and others:** `chart_1`..`chart_5`, `skeleton`, `progress_bar`, `switch`, `slider_bar`, and others.
- `button_*` variants exist too.

**Gotcha:** there is no `destructive` or `error` field. Use `danger`.

---

## 9. Icons

There are **two enums**:
- `gpui_kit::component::IconName`: the legacy 106-icon subset listed in REF/assets/default-icons.txt. These are the only icons embedded by `gpui_kit::assets::Assets`.
- `gpui_kit::assets::IconName`: the full ~1830 Lucide catalog, generated from REF/assets/assets/icons/*.svg (REF/assets/build.rs:26-56). Variant = PascalCase of the file name, e.g. `table-2` → `Table2`. Using it requires `.with_assets(gpui_kit::assets::AllAssets)`, or `icon_assets!(...)` for a hand-picked set (REF/assets/src/lib.rs).

Both work in `Icon::new(..)`, `Button::icon(..)`, `Tab::icon(..)` via `impl<T: IconNamed> From<T> for Icon` (REF/component/src/icon.rs:59). Suggested alias: `use gpui_kit::assets::IconName as Lucide;`.

Variants you asked for:

| Need | Component `IconName` (default `Assets`) | Full catalog `assets::IconName` (`AllAssets`) |
|---|---|---|
| database | — | `Database`, `DatabaseZap` |
| table | — | `Table`, `Table2`, `Sheet` |
| folder / open | `Folder`, `FolderOpen`, `FolderClosed` | same |
| file | `File`, `FileText` | `File`, `FileText`, `FileCode` |
| file-plus | — | `FilePlus` (no `FilePlus2`) |
| play | `Play` | `Play` |
| square/stop | `Square` | `Square`, `SquareStop`, `CircleStop`, `OctagonX` |
| chevrons | `ChevronDown/Up/Left/Right`, `ChevronsUpDown` | same |
| circle | `CircleCheck`, `CircleX`, `CircleAlert`, `CircleUser` (no plain `Circle`) | `Circle`, `CircleDot` |
| check | `Check` | `Check` |
| x | `Close` (no `X`) | `X`, `Close` |
| loader | `Loader`, `LoaderCircle` (no `Loader2`) | same |
| plus | `Plus` | `Plus` |
| settings | `Settings`, `Settings2` | same |
| search | `Search` | `Search` |
| refresh | `RefreshCw`, `RotateCw` | `RefreshCw`, `RefreshCcw`, `RotateCw` |
| eye | `Eye`, `EyeOff` | same |
| columns | — | `Columns2`, `Columns3` (no `Columns`) |
| key | — | `Key`, `KeyRound` |
| list | `Menu` | `List`, `ListTree` |
| zap | — | `Zap` |
| download | — | `Download` |
| save | — | `Save` |
| git-commit | — | `GitCommitHorizontal`, `GitCommitVertical` (no `GitCommit`) |
| undo / rotate-ccw | `Undo`, `Undo2`, `Redo`, `Redo2` | + `RotateCcw` |
| alert | `TriangleAlert`, `CircleAlert` | same (no `AlertTriangle`) |
| lock | — | `Lock`, `LockKeyhole` |
| plug / link | — | `Plug`, `PlugZap`, `Unplug`, `Link`, `Link2` |
| terminal | `SquareTerminal` | same |

`Icon`:
- `Icon::new(icon: impl Into<Icon>)` (icon.rs:136). `Styled` + `Sizable`, e.g. `.size_4()`, `.small()`, `.text_color(c)`. `.rotate(radians)` (:201).
- Spinner: `gpui_kit::component::spinner::Spinner::new()` (spinner.rs:20).

---

## 10. Overlays

`WindowExt` lives at REF/component/src/window_ext.rs; import `use gpui_kit::component::WindowExt as _;`.

```rust
fn open_dialog<F>(&mut self, cx: &mut App, build: F) where F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static;  // :75
fn open_alert_dialog<F>(&mut self, cx: &mut App, build: F) where F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static; // :101
fn close_dialog(&mut self, cx: &mut App);                                       // :112
fn push_notification(&mut self, note: impl Into<Notification>, cx: &mut App);   // :118  (From<&str|String|SharedString>)
// Dialog (REF/component/src/dialog/dialog.rs): title(impl IntoElement) :402, footer(impl IntoElement) :421,
//   on_ok(Fn(&ClickEvent,&mut Window,&mut App)->bool) :462, on_cancel(..->bool) :473, on_close :451, close_button(bool) :482, w(px) :505; ParentElement
// Footer pieces (dialog/footer.rs): DialogFooter::new() :29, DialogClose::new() :76 (→ cancel), DialogAction::new() :109 (→ Confirm → on_ok)
// Notification (notification.rs): Notification::info/success/warning/error(msg) :203-224, .title() :250, .autohide(bool) :321
```

**Gotchas**
- The builder is a re-run render closure. Create the `InputState`s **before** `open_dialog` and capture them (doc at window_ext.rs:40-73).
- A plain `Dialog` renders **no buttons** from `on_ok`/`on_cancel` (dialog.rs:412-420). Compose them with `DialogFooter`/`DialogClose`/`DialogAction`, or use `open_alert_dialog`, which gives a default footer.
- Return `false` from `on_ok` to keep the dialog open.

Snippet, adapted from crates/story/src/stories/dialog_story.rs:347-391 and examples/editor/src/main.rs:769-809:

```rust
let host = cx.new(|cx| InputState::new(window, cx).placeholder("host"));   // InputState::new state.rs:12597; Input::new input.rs:234
let user = cx.new(|cx| InputState::new(window, cx).placeholder("user"));
let first = std::cell::Cell::new(true);
window.open_dialog(cx, move |dialog, window, cx| {
    if first.replace(false) { host.focus_handle(cx).focus(window, cx); }
    dialog.title("New connection")
        .child(v_flex().gap_2().child(Input::new(&host)).child(Input::new(&user)))
        .footer(DialogFooter::new()
            .child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
            .child(DialogAction::new().child(Button::new("ok").label("Connect").primary())))
        .on_ok({ let host = host.clone(); move |_, window, cx| {
            let h = host.read(cx).value(); if h.is_empty() { return false; }
            window.push_notification(Notification::success(format!("Connecting to {h}")), cx); true }})
});
```

**Context menu on right-click**
- Trait: `ContextMenuExt` (REF/component/src/menu/context_menu.rs:17), implemented for any `InteractiveElement + ParentElement + Styled`:
  ```rust
  fn context_menu(self, f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static) -> ContextMenu<Self>
  ```
- `PopupMenu` (menu/popup_menu.rs):
  - `.menu(label, Box<dyn Action>)` :483
  - `.menu_with_icon(label, icon, action)` :561
  - `.separator()` :700
  - `.submenu(..)` :714
  - `.item(PopupMenuItem::new(label).on_click(..))` :748 / :202
  - `.action_context(FocusHandle)` :387
- Example from examples/dialog_overlay/src/main.rs:90-98: `div().id("x").child(..).context_menu(|m, _, _| m.menu("Open", Box::new(Open)).separator().menu("Delete", Box::new(Delete)))`.
- **Gotcha:** menu actions dispatch to `action_context`, or else to the element that had focus. Your `on_action` must sit on an ancestor of that element, or be registered globally with `cx.on_action`.
- `Editor` has its own right-click menu, built from a `NativeMenu` (`Editor::context_menu`); it is not a `PopupMenu`.

---

## 11. Command palette

Module: `gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState}`.

```rust
CommandState::new(window: &mut Window, cx: &mut Context<Self>) -> Self   // REF/component/src/command/state.rs:146
Command::new(state: &Entity<CommandState>) -> Self                       // command.rs:74
  .items(impl IntoIterator<Item = CommandItem>) :95  .group(CommandGroup) :102  .placeholder(..) :179
  .on_confirm<F: Fn(IndexPath, &mut Window, &mut App)>(f) :159   .on_cancel(Fn(&mut Window,&mut App)) :170  .on_query(Fn(&str,..)) :132
CommandItem::new() :37 .label(..) :50 .icon(..) :56 .action(Box<dyn Action>) :64 .keywords([..]) :80
// IndexPath { section, row, column } (REF/base/src/index_path.rs:9)
```

Snippet, adapted from crates/story/src/stories/command_story.rs:636-668:

```rust
let palette = self.palette.clone(); let owner = cx.weak_entity();
window.open_dialog(cx, move |dialog, _, _| {
    let (palette, owner) = (palette.clone(), owner.clone());
    dialog.close_button(false).p_0().content(move |content, window, cx| {
        content.child(Command::new(&palette).placeholder("Type a command…")
            .items([CommandItem::new().label("Run statement").icon(IconName::Play).action(Box::new(RunStatement))])
            .on_confirm(move |ix, window, cx| { _ = owner.update(cx, |v, cx| v.on_palette(ix, window, cx)); }))
    })
});
```

**Gotcha:** a `CommandItem::action` is dispatched with `window.dispatch_action` from the palette's focus (state.rs:630). That focus is inside the Root dialog layer, **not** inside your view. View-level `on_action` will not see it. Use `on_confirm`, or register the action globally with `cx.on_action::<RunStatement>(..)` (GPUI/app.rs:2751).

---

## 12. Testing

**Setup**
- Dev-dependency: `gpui-kit = { version = "0.9.1", features = ["test-support"] }` (REF/kit/Cargo.toml).
- Attribute: `#[gpui_kit::test]` on `fn` or `async fn` taking `cx: &mut TestAppContext` (REF/kit/tests/integration/test_macro.rs).
- **Do not `use gpui_kit::*` in test modules.** With test-support on, the glob imports GPUI's `test` macro and shadows `#[test]`. Import explicitly.
- Call `cx.update(gpui_kit::init)` first.

`TestAppContext` (GPUI/app/test_context.rs):
- `add_window_view(F) -> (Entity<V>, &mut VisualTestContext)` :352 (no Root, so dialogs and notifications won't work)
- `update` :271, `read` :277, `run_until_parked()` :600, `dispatch_action(window, action)` :605, `simulate_keystrokes(window, "ctrl-enter")` :622, `simulate_input` :639, `executor()` :261
- `cx.executor().advance_clock(Duration)` (GPUI/executor.rs:215) fires timers; `run_until_parked` only runs ready tasks.
- `allow_parking()` (:239) is needed when a task waits on something outside GPUI (e.g. a real tokio channel fed by another thread).

`VisualTestContext`:
- `from_window(AnyWindowHandle, &TestAppContext)` :887
- `simulate_keystrokes(&str)` :924, `simulate_input` :930, `run_until_parked` :895, `update(|window, cx| ..)` :878, `dispatch_action` :900, `simulate_click` :980, `debug_bounds(sel)` :1031

Kit helpers in `gpui_kit::test::*` (REF/kit/src/test.rs):
- `TestWindowExt` for `Window` (:33): `render_frame`, `find(id) -> ElementSnapshot` (`.value()`, `.label()`, `.focused()`, `.bounds()`), `click`, `right_click`, `double_click`, `press("ctrl-enter")`, `input("text")`, `scroll`, `drag_to`.
- `TestAppContextExt::wait_for(window, timeout, pred)` (:572).
- Elements need an `.id(..)` and, for custom divs, `.test_support()`.

Snippet with a Root (adapted from REF/kit/tests/common/mod.rs:8-31 and tests/ui.rs:58-97):

```rust
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext, WindowOptions};
#[gpui_kit::test]
fn ctrl_enter_runs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| cx.bind_keys([gpui_kit::KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor > Input"))]));
    let (win, view) = cx.update(|cx| gpui_kit::open_window(WindowOptions::default(), cx, |w, cx| cx.new(|cx| SqlView::new(w, cx)))).unwrap();
    let mut vcx = VisualTestContext::from_window(win, cx);
    vcx.update(|w, cx| { let fh = view.read(cx).editor.focus_handle(cx); fh.focus(w, cx); });
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    assert_eq!(view.read_with(&vcx, |v, _| v.runs), 1);
}
```

**Screenshots**
- `HeadlessAppContext::with_platform(text_system, assets, gpui_kit::platform::current_headless_renderer)` plus `capture_screenshot(window) -> Result<RgbaImage>` (GPUI/app/headless_app_context.rs:65, :168; example REF/kit/tests/rendering.rs).
- **macOS (Metal) and Linux (WGPU) only.** On Windows `current_headless_renderer` returns no renderer, and the kit's own rendering test prints "skipped" (rendering.rs:3-8).
- `VisualTestAppContext::capture_screenshot` is macOS-only.
- So on your Windows box: no pixel capture. Use `ElementSnapshot` and state assertions instead.

---

## 13. StatusBar and TitleBar

```rust
// gpui_kit::component::status_bar::StatusBar   (REF/component/src/status_bar.rs)
pub fn new() -> Self  :41    pub fn left(mut self, child: impl IntoElement) -> Self  :51
pub fn right(mut self, child: impl IntoElement) -> Self  :57    // .child(..) = center region; Styled
// gpui_kit::component::TitleBar   (REF/component/src/title_bar.rs)
pub fn new() -> Self  :51     ParentElement + Styled
pub fn window_options() -> WindowOptions  :82   // use as base WindowOptions so the bar owns drag/double-click
pub fn on_close_window(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self  :96 (Linux only)
```

Usage, from examples/editor/src/main.rs:1177-1187 and examples/dialog_overlay/src/main.rs:42, 112:

```rust
v_flex().size_full()
    .child(TitleBar::new().child("Not Bad Query"))
    .child(/* splits */)
    .child(StatusBar::new().left(format!("Ln {}, Col {}", pos.line + 1, pos.character + 1)).right(format!("{:.1}s", elapsed)))
```

Pass `TitleBar::window_options()` (or `WindowOptions { .., ..TitleBar::window_options() }`) to `gpui_kit::open_window`. Windows draws its own min/max/close controls in the bar.

---

## Things you asked for that don't exist as named

- **Closable tabs:** no `closable` API. Use `Tab::suffix(Button)` and call `cx.stop_propagation()` in its handler.
- **Table copy:** there is no copy/clipboard action. Bind your own in the `"DataTable"` context and use `TableState::selection()` plus `cx.write_to_clipboard`.
- **Tree selection event and double-click:**
  - `TreeEvent` has only `Expanded`/`Collapsed`.
  - Use `ListItem::on_click` with `ClickEvent::click_count()`, or `cx.observe(&tree_state, ..)`.
  - Enter is unbound in `"Tree"`.
- **Editor ctrl-enter:** the editor consumes it (`Enter{secondary:true}` inserts a newline). Override it with a later binding in `"SqlEditor > Input"`, or `capture_action::<Enter>`.
- **`InputState` "code-editor mode":** a separate type, `EditorState` (`InputBaseState<EditorMode>`).
- **Icon names not in the catalog:** `IconName::Database`, `Table`, `Lock`, `Plug`, `Zap`, `Save`, `Download`, `Key`, `List` exist only in `gpui_kit::assets::IconName` (needs `AllAssets`). `Columns`, `GitCommit`, `Loader2`, `FilePlus2`, `AlertTriangle` do not exist; use `Columns2`, `GitCommitHorizontal`, `LoaderCircle`, `FilePlus`, `TriangleAlert`.
- **Windows screenshots:** not available (headless renderer is macOS/Linux only).

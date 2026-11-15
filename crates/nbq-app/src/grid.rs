//! Result grid: a `TableDelegate` that reads rows lazily from the spool.
//!
//! Only a window of rows around what's visible is decoded and kept as
//! display strings. Scrolling near the end of what has been fetched asks the
//! engine for more rows.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_driver::{ColumnKind, display::cell_text, meta};
use nbq_engine::{Engine, QueryId, Spool, TabId};

/// Longest text shown in a cell; the full value is still in the spool.
const MAX_CELL_CHARS: usize = 300;
// Small, aligned tiles let neighboring pages and columns coexist. Keep both
// text and entry counts bounded, including results full of empty/null cells.
const PREVIEW_ROWS: usize = 32;
const PREVIEW_COLS: usize = 16;
const PREVIEW_BYTES: usize = 4 * 1024 * 1024;
const PREVIEW_TILES: usize = 64;
const PREFETCH_THRESHOLD: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewRange {
    rows: Range<usize>,
    cols: Range<usize>,
}

struct PreviewTile {
    range: PreviewRange,
    cells: Vec<Vec<Option<SharedString>>>,
}

impl PreviewTile {
    fn cell(&self, row: usize, col: usize) -> Option<&Option<SharedString>> {
        self.cells.get(row.checked_sub(self.range.rows.start)?)?.get(col.checked_sub(self.range.cols.start)?)
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.cells.capacity() * std::mem::size_of::<Vec<Option<SharedString>>>()
            + self.cells.iter().map(|row| {
                row.capacity() * std::mem::size_of::<Option<SharedString>>()
                    + row.iter().flatten().map(|text| text.len()).sum::<usize>()
            }).sum::<usize>()
    }
}

#[derive(Default)]
struct PreviewCache {
    tiles: VecDeque<PreviewTile>,
    bytes: usize,
}

impl PreviewCache {
    fn cell(&self, row: usize, col: usize) -> Option<Option<SharedString>> {
        self.tiles.iter().rev().find_map(|tile| tile.cell(row, col)).cloned()
    }

    fn insert(&mut self, tile: PreviewTile, visible: &PreviewRange) {
        self.tiles.retain(|old| old.range.rows.start != tile.range.rows.start || old.range.cols.start != tile.range.cols.start);
        // Preserve current viewport tiles ahead of older/off-screen pages.
        let mut on_screen = VecDeque::new();
        let mut off_screen = VecDeque::new();
        for old in self.tiles.drain(..) {
            if old.range.rows.start < visible.rows.end && old.range.rows.end > visible.rows.start
                && old.range.cols.start < visible.cols.end && old.range.cols.end > visible.cols.start {
                on_screen.push_back(old);
            } else {
                off_screen.push_back(old);
            }
        }
        off_screen.append(&mut on_screen);
        self.tiles = off_screen;
        self.tiles.push_back(tile);
        self.bytes = self.tiles.iter().map(PreviewTile::bytes).sum();
        while self.bytes > PREVIEW_BYTES || self.tiles.len() > PREVIEW_TILES {
            self.bytes -= self.tiles.pop_front().unwrap().bytes();
        }
    }
}

#[derive(Clone)]
struct Col {
    name: SharedString,
    db_type: SharedString,
    kind: ColumnKind,
}

pub struct ResultGrid {
    engine: Arc<Engine>,
    tab: TabId,
    query: Option<QueryId>,
    spool: Option<Arc<Spool>>,
    cols: Vec<Col>,
    /// Rows available in the spool (refreshed on fetch events).
    rows: usize,
    initial_rows: u64,
    preview: PreviewCache,
    visible: PreviewRange,
    /// Rows the grid has asked the engine for.
    requested: u64,
    generation: u64,
    loading: bool,
    load_error: Option<String>,
}

impl ResultGrid {
    pub fn new(engine: Arc<Engine>, tab: TabId, initial_rows: u64) -> Self {
        ResultGrid {
            engine,
            tab,
            query: None,
            spool: None,
            cols: Vec::new(),
            rows: 0,
            initial_rows,
            preview: PreviewCache::default(),
            visible: PreviewRange { rows: 0..0, cols: 0..0 },
            requested: 0,
            generation: 0,
            loading: false,
            load_error: None,
        }
    }

    pub fn set_result(&mut self, query: QueryId, spool: Arc<Spool>) {
        self.cols = spool
            .schema()
            .fields()
            .iter()
            .map(|f| Col {
                name: f.name().clone().into(),
                db_type: f.metadata().get(meta::DB_TYPE).cloned().unwrap_or_default().into(),
                kind: ColumnKind::parse(f.metadata().get(meta::KIND).map(String::as_str).unwrap_or("")),
            })
            .collect();
        self.rows = spool.rows() as usize;
        self.spool = Some(spool);
        self.query = Some(query);
        self.preview = PreviewCache::default();
        self.visible = PreviewRange { rows: 0..0, cols: 0..self.cols.len().min(PREVIEW_COLS) };
        self.requested = 0;
        self.generation = self.generation.wrapping_add(1);
        self.loading = false;
        self.load_error = None;
    }

    /// Called when the spool grew. Returns true if the row count changed.
    pub fn refresh_rows(&mut self) -> bool {
        let n = self.spool.as_ref().map(|s| s.rows() as usize).unwrap_or(0);
        let old = std::mem::replace(&mut self.rows, n);
        // Spool rows are immutable. Keep populated cells while a partial tile
        // at the old end is extended in the background.
        n != old
    }

    pub fn spool_for_export(&self) -> Option<Arc<Spool>> {
        self.spool.clone()
    }

    pub fn has_result(&self) -> bool {
        self.spool.is_some()
    }

    fn prefetch(&mut self, visible: Range<usize>) {
        let (Some(query), Some(spool)) = (self.query, &self.spool) else { return };
        if visible.is_empty() || spool.progress().status != nbq_engine::SpoolStatus::Open { return; }
        // Slow queries publish partial batches before the initial fetch ends.
        // Let that existing demand finish instead of extending it on first paint.
        if (self.rows as u64) < self.initial_rows { return; }
        // Keep the proximity trigger separate from the requested lookahead.
        // Small custom initial batches use a proportionally smaller threshold.
        let threshold = (self.rows / 5).clamp(1, PREFETCH_THRESHOLD);
        if self.rows.saturating_sub(visible.end) > threshold { return; }
        let want = (visible.end as u64).saturating_add(500);
        if want > self.requested {
            self.requested = want;
            self.engine.ensure_rows(self.tab, query, want);
        }
    }

    /// Seed/recheck previews after a result replacement or append, even if the
    /// table's visible-range callbacks do not fire (the range can be unchanged).
    pub fn prepare_preview(&mut self, rows: Range<usize>, cols: Range<usize>, cx: &mut Context<TableState<Self>>) {
        self.visible = PreviewRange {
            rows: if rows.is_empty() { 0..self.rows.min(PREVIEW_ROWS) } else { rows },
            cols: if cols.is_empty() { 0..self.cols.len().min(PREVIEW_COLS) } else { cols },
        };
        self.load_visible(cx);
    }

    fn next_preview(&self) -> Option<PreviewRange> {
        let rows = self.visible.rows.start..self.visible.rows.end.min(self.rows);
        let cols = self.visible.cols.start..self.visible.cols.end.min(self.cols.len());
        if rows.is_empty() || cols.is_empty() { return None; }
        let row_start = rows.start / PREVIEW_ROWS * PREVIEW_ROWS;
        let col_start = cols.start / PREVIEW_COLS * PREVIEW_COLS;
        for row in (row_start..rows.end).step_by(PREVIEW_ROWS) {
            for col in (col_start..cols.end).step_by(PREVIEW_COLS) {
                let end_row = (row + PREVIEW_ROWS).min(rows.end);
                let end_col = (col + PREVIEW_COLS).min(cols.end);
                if self.cell(row.max(rows.start), col.max(cols.start)).is_none()
                    || self.cell(end_row - 1, end_col - 1).is_none() {
                    return Some(PreviewRange {
                        rows: row..(row + PREVIEW_ROWS).min(self.rows),
                        cols: col..(col + PREVIEW_COLS).min(self.cols.len()),
                    });
                }
            }
        }
        None
    }

    fn load_visible(&mut self, cx: &mut Context<TableState<Self>>) {
        if self.loading || self.load_error.is_some() { return; }
        let Some(spool) = self.spool.clone() else { return };
        let Some(range) = self.next_preview() else { return };
        self.loading = true;
        let generation = self.generation;
        let task = cx.background_executor().spawn(async move {
            read_window(&spool, range.rows.start, range.rows.end, range.cols.clone())
                .map(|cells| PreviewTile { range, cells })
        });
        cx.spawn(async move |table, cx| {
            let result = task.await;
            let _ = table.update(cx, |table, cx| {
                let grid = table.delegate_mut();
                if grid.generation != generation { return; }
                grid.loading = false;
                match result {
                    Ok(tile) => grid.preview.insert(tile, &grid.visible),
                    Err(e) => { grid.load_error = Some(e); }
                }
                grid.load_visible(cx);
                cx.notify();
            });
        }).detach();
    }

    fn cell(&self, row: usize, col: usize) -> Option<Option<SharedString>> {
        // Rendering also happens for off-screen layout measurements (row 0).
        // It must never schedule IO or replace the actual viewport's previews.
        self.preview.cell(row, col)
    }

    /// Cached preview for synchronous table helpers. Full-value copy is async.
    pub fn full_text(&self, row: usize, col: usize) -> Option<String> {
        self.cell(row, col)?.as_ref().map(|s| s.to_string())
    }
}

fn read_window(spool: &Spool, start: usize, end: usize, columns: Range<usize>) -> Result<Vec<Vec<Option<SharedString>>>, String> {
    let mut win = Vec::with_capacity(end - start);
    let mut text = String::new();
    while start + win.len() < end {
        let batches = spool.read_bounded((start + win.len()) as u64..end as u64, 4 * 1024 * 1024).map_err(|e| e.to_string())?;
        if batches.is_empty() { break; }
        for batch in &batches {
            for row in 0..batch.num_rows() {
                win.push(batch.columns()[columns.clone()].iter().map(|column| {
                    text.clear();
                    cell_text(column, row, &mut text).then(|| display(&text))
                }).collect());
            }
        }
    }
    Ok(win)
}

/// Single-line, length-capped display form of a value.
fn display(s: &str) -> SharedString {
    let mut out = String::with_capacity(s.len().min(MAX_CELL_CHARS + 4));
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push('↵'),
            '\r' => {}
            '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out.into()
}

fn width_for(c: &Col) -> f32 {
    let by_kind: f32 = match c.kind {
        ColumnKind::Number => 110.,
        ColumnKind::Bool => 70.,
        ColumnKind::Temporal => 255.,
        ColumnKind::Uuid => 290.,
        ColumnKind::Json => 260.,
        ColumnKind::Binary => 160.,
        ColumnKind::Text | ColumnKind::Other => 180.,
    };
    let by_name = (c.name.chars().count() as f32 * 8.) + 40.;
    by_kind.max(by_name).min(420.)
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let c = &self.cols[ix];
        let col = Column::new(SharedString::from(format!("c{ix}")), c.name.clone()).width(px(width_for(c)));
        if c.kind == ColumnKind::Number { col.text_right() } else { col }
    }

    fn render_th(&mut self, ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let c = &self.cols[ix];
        h_flex()
            .gap_1p5()
            .overflow_hidden()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(c.name.clone()))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(c.db_type.clone()))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let kind = self.cols.get(col).map(|c| c.kind).unwrap_or(ColumnKind::Other);
        let theme = cx.theme().clone();
        let d = div().w_full().overflow_hidden().whitespace_nowrap().text_ellipsis();
        let d = if kind == ColumnKind::Number { d.text_right() } else { d };
        match self.cell(row, col) {
            Some(Some(v)) => {
                let color = match kind {
                    ColumnKind::Number => Some(theme.blue),
                    ColumnKind::Bool => Some(theme.magenta),
                    ColumnKind::Temporal => Some(theme.green),
                    _ => None,
                };
                d.when_some(color, |d, c| d.text_color(c)).child(v)
            }
            Some(None) => d.text_color(theme.muted_foreground.opacity(0.6)).italic().child("NULL"),
            None => d.text_color(theme.muted_foreground).child(if self.load_error.is_some() { "Read error" } else { "…" }),
        }
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.full_text(row, col).unwrap_or_default()
    }

    fn visible_rows_changed(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.visible.rows = range.clone();
        self.load_visible(cx);
        self.prefetch(range);
    }

    fn visible_columns_changed(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.visible.cols = range;
        self.load_visible(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    fn fixture(rows: usize, cols: usize) -> (tempfile::TempDir, ResultGrid, Arc<Spool>) {
        let dir = tempfile::tempdir().unwrap();
        let schema = Arc::new(Schema::new((0..cols).map(|col| Field::new(format!("c{col}"), DataType::Int64, false)).collect::<Vec<_>>()));
        let spool = Spool::create(dir.path(), schema).unwrap();
        append_rows(&spool, 0..rows);
        let (engine, _) = Engine::new(nbq_engine::EngineOptions::new(dir.path().join("engine")));
        let mut grid = ResultGrid::new(engine, TabId(1), rows.min(500) as u64);
        grid.set_result(QueryId(1), spool.clone());
        (dir, grid, spool)
    }

    fn append_rows(spool: &Spool, rows: Range<usize>) {
        let batch = RecordBatch::try_new(spool.schema().clone(), (0..spool.schema().fields().len()).map(|col| {
            Arc::new(Int64Array::from_iter_values(rows.clone().map(|row| (col * 100_000 + row) as i64))) as _
        }).collect()).unwrap();
        spool.append(&batch).unwrap();
    }

    fn load_previews(grid: &mut ResultGrid, spool: &Spool) {
        for _ in 0..PREVIEW_TILES {
            let Some(range) = grid.next_preview() else { return };
            let cells = read_window(spool, range.rows.start, range.rows.end, range.cols.clone()).unwrap();
            grid.preview.insert(PreviewTile { range, cells }, &grid.visible);
        }
        panic!("preview requests did not settle");
    }

    #[::core::prelude::v1::test]
    fn prefetch_waits_until_scrolling_near_the_fetched_end() {
        let (_dir, mut grid, spool) = fixture(500, 2);
        for viewport in [0..20, 100..120, 379..399] {
            grid.prefetch(viewport);
            assert_eq!(grid.requested, 0, "initial/early scrolling fetched another batch");
        }
        grid.prefetch(380..400);
        assert_eq!(grid.requested, 900);
        grid.prefetch(380..400);
        assert_eq!(grid.requested, 900);
        append_rows(&spool, 500..2900);
        grid.refresh_rows();
        grid.prefetch(400..420);
        assert_eq!(grid.requested, 900, "appending rows immediately requested another batch");
        grid.prefetch(2780..2800);
        assert_eq!(grid.requested, 3300);
    }

    #[::core::prelude::v1::test]
    fn prefetch_respects_small_initial_batches_and_closed_results() {
        let (_dir, mut grid, spool) = fixture(50, 2);
        grid.prefetch(0..20);
        assert_eq!(grid.requested, 0);
        grid.prefetch(0..0);
        assert_eq!(grid.requested, 0);
        grid.prefetch(20..40);
        assert_eq!(grid.requested, 540);
        for status in [nbq_engine::SpoolStatus::Complete, nbq_engine::SpoolStatus::Stopped("closed".into())] {
            spool.finish(status);
            grid.requested = 0;
            grid.prefetch(30..50);
            assert_eq!(grid.requested, 0, "closed result requested more rows");
        }
    }

    #[::core::prelude::v1::test]
    fn partial_batches_do_not_extend_the_initial_fetch() {
        let (_dir, mut grid, spool) = fixture(5, 2);
        grid.initial_rows = 500;
        grid.prefetch(0..5); // Slow query's first partial batch fills the viewport.
        assert_eq!(grid.requested, 0);
        append_rows(&spool, 5..80);
        grid.refresh_rows();
        grid.prefetch(60..80);
        assert_eq!(grid.requested, 0);
        append_rows(&spool, 80..500);
        grid.refresh_rows();
        grid.prefetch(380..400);
        assert_eq!(grid.requested, 900);
    }

    #[::core::prelude::v1::test]
    fn offscreen_measurement_cannot_evict_visible_rows_or_restart_loading() {
        let (_dir, mut grid, spool) = fixture(3000, 2);
        grid.visible = PreviewRange { rows: 0..15, cols: 0..2 };
        load_previews(&mut grid, &spool);
        grid.visible.rows = 950..970; // Beyond both the initial fetch and old preview window.
        load_previews(&mut grid, &spool);
        for _ in 0..20 {
            let _ = grid.cell(0, 0); // GPUI's off-screen row-height measurement.
            assert_eq!(grid.full_text(950, 0).as_deref(), Some("950"));
            assert_eq!(grid.full_text(969, 1).as_deref(), Some("100969"));
            assert!(grid.next_preview().is_none());
        }
    }

    #[::core::prelude::v1::test]
    fn appending_rows_preserves_populated_cells_while_extending_a_partial_tile() {
        let (_dir, mut grid, spool) = fixture(500, 2);
        grid.visible = PreviewRange { rows: 490..500, cols: 0..2 };
        load_previews(&mut grid, &spool);
        append_rows(&spool, 500..2500);
        assert!(grid.refresh_rows());
        assert_eq!(grid.full_text(499, 0).as_deref(), Some("499"));
        grid.visible.rows = 495..515;
        assert!(grid.next_preview().is_some());
        assert_eq!(grid.full_text(499, 0).as_deref(), Some("499"));
        load_previews(&mut grid, &spool);
        assert_eq!(grid.full_text(514, 0).as_deref(), Some("514"));
    }

    #[::core::prelude::v1::test]
    fn horizontal_and_vertical_tiles_coexist_and_follow_the_latest_viewport() {
        let (_dir, mut grid, spool) = fixture(3000, 70);
        grid.visible = PreviewRange { rows: 990..1020, cols: 12..51 };
        // A request completing after a fast scroll must be followed by the latest viewport.
        let old_request = grid.next_preview().unwrap();
        grid.visible.rows = 1980..2015;
        let cells = read_window(&spool, old_request.rows.start, old_request.rows.end, old_request.cols.clone()).unwrap();
        grid.preview.insert(PreviewTile { range: old_request, cells }, &grid.visible);
        load_previews(&mut grid, &spool);
        for row in grid.visible.rows.clone() {
            for col in grid.visible.cols.clone() {
                assert_eq!(grid.full_text(row, col), Some((col * 100_000 + row).to_string()));
            }
        }
        assert!(grid.next_preview().is_none());
    }

    #[::core::prelude::v1::test]
    fn preview_cache_is_bounded_and_keeps_current_viewport_tiles() {
        let mut cache = PreviewCache::default();
        let visible = PreviewRange { rows: 0..32, cols: 0..16 };
        for page in 0..100 {
            let range = PreviewRange { rows: page * 32..(page + 1) * 32, cols: 0..16 };
            cache.insert(PreviewTile { range, cells: vec![vec![Some(SharedString::from("🙂".repeat(MAX_CELL_CHARS))); 16]; 32] }, &visible);
            assert!(cache.bytes <= PREVIEW_BYTES);
            assert!(cache.tiles.len() <= PREVIEW_TILES);
            assert!(cache.cell(0, 0).is_some(), "visible tile was evicted");
        }
        // Tiny cells still have a strict tile count bound.
        for page in 100..200 {
            let range = PreviewRange { rows: page * 32..(page + 1) * 32, cols: 0..1 };
            cache.insert(PreviewTile { range, cells: vec![vec![None]; 32] }, &visible);
        }
        assert!(cache.tiles.len() <= PREVIEW_TILES);
        assert!(cache.cell(0, 0).is_some());
    }

    #[::core::prelude::v1::test]
    fn preview_reads_bounded_batches_and_projects_columns() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false), Field::new("id", DataType::Int64, false)]));
        let spool = Spool::create(dir.path(), schema.clone()).unwrap();
        let text = "🙂".repeat(16384);
        for first in [0, 50] {
            let batch = RecordBatch::try_new(schema.clone(), vec![
                Arc::new(StringArray::from(vec![text.as_str(); 50])),
                Arc::new(Int64Array::from_iter_values(first..first + 50)),
            ]).unwrap();
            spool.append(&batch).unwrap();
        }
        let rows = read_window(&spool, 5, 95, 1..2).unwrap();
        assert_eq!(rows.len(), 90);
        assert_eq!(rows[0][0].as_ref().unwrap().as_ref(), "5");
        assert_eq!(rows[89][0].as_ref().unwrap().as_ref(), "94");
        assert!(rows.iter().all(|row| row.len() == 1));
        let rows = read_window(&spool, 0, 1, 0..1).unwrap();
        assert_eq!(rows[0][0].as_ref().unwrap().chars().count(), MAX_CELL_CHARS + 1);
    }
}

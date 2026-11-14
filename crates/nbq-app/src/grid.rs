//! Result grid: a `TableDelegate` that reads rows lazily from the spool.
//!
//! Only a window of rows around what's visible is decoded and kept as
//! display strings. Scrolling near the end of what has been fetched asks the
//! engine for more rows.

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
/// Rows decoded around the visible range.
const WINDOW_MARGIN: usize = 150;

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
    win_start: usize,
    col_start: usize,
    win: Vec<Vec<Option<SharedString>>>,
    /// Rows the grid has asked the engine for.
    requested: u64,
    generation: u64,
    loading: bool,
    wanted: (usize, usize),
    load_error: Option<String>,
}

impl ResultGrid {
    pub fn new(engine: Arc<Engine>, tab: TabId) -> Self {
        ResultGrid {
            engine,
            tab,
            query: None,
            spool: None,
            cols: Vec::new(),
            rows: 0,
            win_start: 0,
            col_start: 0,
            win: Vec::new(),
            requested: 0,
            generation: 0,
            loading: false,
            wanted: (0, 0),
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
        self.win.clear();
        self.win_start = 0;
        self.requested = 0;
        self.generation = self.generation.wrapping_add(1);
        self.loading = false;
        self.wanted = (0, 0);
        self.load_error = None;
    }

    /// Called when the spool grew. Returns true if the row count changed.
    pub fn refresh_rows(&mut self) -> bool {
        let n = self.spool.as_ref().map(|s| s.rows() as usize).unwrap_or(0);
        let old = std::mem::replace(&mut self.rows, n);
        // A window that reached the old end was cut short; reload it.
        if n != old && self.win_start + self.win.len() >= old {
            self.win.clear();
        }
        n != old
    }

    pub fn spool_for_export(&self) -> Option<Arc<Spool>> {
        self.spool.clone()
    }

    pub fn has_result(&self) -> bool {
        self.spool.is_some()
    }

    fn load_window(&mut self, row: usize, col: usize, cx: &mut Context<TableState<Self>>) {
        self.wanted = (row, col);
        if self.loading || self.load_error.is_some() { return; }
        let Some(spool) = self.spool.clone() else { return };
        let col_end = (col + 32).min(self.cols.len());
        let count = (4 * 1024 * 1024 / (col_end - col).max(1).saturating_mul(1280)).clamp(1, WINDOW_MARGIN * 3);
        let end = (row + count).min(self.rows);
        if row >= end { return; }
        self.loading = true;
        let generation = self.generation;
        let task = cx.background_executor().spawn(async move { read_window(&spool, row, end, col..col_end) });
        cx.spawn(async move |table, cx| {
            let result = task.await;
            let _ = table.update(cx, |table, cx| {
                let grid = table.delegate_mut();
                if grid.generation != generation { return; }
                grid.loading = false;
                match result {
                    Ok(win) => { grid.win_start = row; grid.col_start = col; grid.win = win; }
                    Err(e) => { grid.load_error = Some(e); }
                }
                let (wanted_row, wanted_col) = grid.wanted;
                if !grid.contains(wanted_row, wanted_col) {
                    grid.load_window(wanted_row, wanted_col, cx);
                }
                cx.notify();
            });
        }).detach();
    }

    fn cell(&mut self, row: usize, col: usize, cx: &mut Context<TableState<Self>>) -> Option<Option<SharedString>> {
        if !self.contains(row, col) {
            self.load_window(row, col, cx);
        }
        self.win.get(row.checked_sub(self.win_start)?).and_then(|r| r.get(col.checked_sub(self.col_start)?)).cloned()
    }

    fn contains(&self, row: usize, col: usize) -> bool {
        self.win.get(row.wrapping_sub(self.win_start)).is_some_and(|r| col >= self.col_start && col - self.col_start < r.len())
    }

    /// Cached preview for synchronous table helpers. Full-value copy is async.
    pub fn full_text(&self, row: usize, col: usize) -> Option<String> {
        self.win.get(row.checked_sub(self.win_start)?)?.get(col.checked_sub(self.col_start)?)?.as_ref().map(|s| s.to_string())
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
        match self.cell(row, col, cx) {
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

    fn visible_rows_changed(&mut self, range: Range<usize>, _: &mut Window, _: &mut Context<TableState<Self>>) {
        let (Some(q), Some(spool)) = (self.query, &self.spool) else { return };
        if spool.progress().status != nbq_engine::SpoolStatus::Open {
            return;
        }
        // Ask for more once the viewport is within a page of the end.
        let want = range.end as u64 + 500;
        if want > self.rows as u64 && want > self.requested {
            self.requested = want;
            self.engine.ensure_rows(self.tab, q, want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

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

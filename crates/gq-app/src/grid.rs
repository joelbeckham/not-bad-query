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
use gq_driver::{ColumnKind, display::cell_text, meta};
use gq_engine::{Engine, QueryId, Spool, TabId};

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
    win: Vec<Vec<Option<SharedString>>>,
    /// Rows the grid has asked the engine for.
    requested: u64,
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
            win: Vec::new(),
            requested: 0,
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
    }

    pub fn clear(&mut self) {
        self.spool = None;
        self.query = None;
        self.cols.clear();
        self.rows = 0;
        self.win.clear();
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

    pub fn column_names(&self) -> Vec<SharedString> {
        self.cols.iter().map(|c| c.name.clone()).collect()
    }

    fn load_window(&mut self, row: usize) {
        let Some(spool) = &self.spool else { return };
        let start = row.saturating_sub(WINDOW_MARGIN);
        let end = (row + WINDOW_MARGIN * 2).min(self.rows);
        let Ok(batches) = spool.read(start as u64..end as u64) else { return };
        let mut win = Vec::with_capacity(end - start);
        let mut s = String::new();
        for b in &batches {
            for r in 0..b.num_rows() {
                let row: Vec<Option<SharedString>> = b
                    .columns()
                    .iter()
                    .map(|c| {
                        s.clear();
                        cell_text(c, r, &mut s).then(|| display(&s))
                    })
                    .collect();
                win.push(row);
            }
        }
        self.win_start = start;
        self.win = win;
    }

    fn cell(&mut self, row: usize, col: usize) -> Option<Option<SharedString>> {
        if row < self.win_start || row >= self.win_start + self.win.len() {
            self.load_window(row);
        }
        self.win.get(row.checked_sub(self.win_start)?).and_then(|r| r.get(col)).cloned()
    }

    /// Text of a cell for copying (full value, not truncated).
    pub fn full_text(&self, row: usize, col: usize) -> Option<String> {
        let spool = self.spool.as_ref()?;
        let b = spool.read(row as u64..row as u64 + 1).ok()?;
        let b = b.first()?;
        let mut s = String::new();
        cell_text(b.column(col), 0, &mut s).then_some(s)
    }
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
        let theme = cx.theme();
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
            None => d.text_color(theme.muted_foreground).child("…"),
        }
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.full_text(row, col).unwrap_or_default()
    }

    fn visible_rows_changed(&mut self, range: Range<usize>, _: &mut Window, _: &mut Context<TableState<Self>>) {
        let (Some(q), Some(spool)) = (self.query, &self.spool) else { return };
        if spool.progress().status != gq_engine::SpoolStatus::Open {
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

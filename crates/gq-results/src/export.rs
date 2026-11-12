//! CSV export straight from the spool. If the result is still being fetched,
//! the export follows the producer until the end, so the file contains the
//! complete result without re-running the query.

use std::path::Path;

use gq_driver::display::cell_text;

use crate::{Result, Spool, Status};

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub header: bool,
    /// Written for SQL NULL. Empty by default.
    pub null: String,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions { delimiter: b',', header: true, null: String::new() }
    }
}

/// Export the whole result to `path`. `on_progress(rows_written)` is called
/// after every batch; returning `false` cancels the export (the partial file
/// is removed). Returns the final spool status and rows written, or `None`
/// if cancelled.
pub fn export_csv(
    spool: &Spool,
    path: &Path,
    opts: &CsvOptions,
    mut on_progress: impl FnMut(u64) -> bool,
) -> Result<Option<(Status, u64)>> {
    let file = std::fs::File::create(path)?;
    let mut w = csv::WriterBuilder::new()
        .delimiter(opts.delimiter)
        .from_writer(std::io::BufWriter::with_capacity(1 << 20, file));
    let csv_err = |e: csv::Error| std::io::Error::other(e);
    if opts.header {
        w.write_record(spool.schema().fields().iter().map(|f| f.name().as_str())).map_err(csv_err)?;
    }
    let mut written = 0u64;
    let mut cell = String::new();
    let mut err = None;
    let status = spool.for_each_batch(|b| {
        for r in 0..b.num_rows() {
            for c in b.columns() {
                cell.clear();
                let field = if cell_text(c, r, &mut cell) { cell.as_str() } else { opts.null.as_str() };
                if let Err(e) = w.write_field(field) {
                    err = Some(e);
                    return false;
                }
            }
            if let Err(e) = w.write_record(None::<&[u8]>) {
                err = Some(e);
                return false;
            }
        }
        written += b.num_rows() as u64;
        on_progress(written)
    })?;
    if let Some(e) = err {
        return Err(csv_err(e).into());
    }
    match status {
        Some(s) => {
            w.flush()?;
            Ok(Some((s, written)))
        }
        None => {
            drop(w);
            let _ = std::fs::remove_file(path);
            Ok(None)
        }
    }
}

//! The result spool.
//!
//! Rows arrive from a driver as Arrow record batches. Each batch is encoded as
//! a small LZ4-compressed Arrow IPC stream and appended to a temp file; only a
//! tiny index (offset, length, first row) stays in memory. Readers ask for a
//! row window and get zero-copy slices of the batches that cover it, decoded
//! on demand through a small LRU cache. Memory use is therefore bounded by
//! the cache, not by the result size.
//!
//! One writer, many readers: the writer is the engine's fetch task, readers
//! are the grid and exporters. All methods are blocking and cheap except
//! reads that miss the cache (one positioned file read + decode).

mod export;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_ipc::CompressionType;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::SchemaRef;
use parking_lot::{Condvar, Mutex, RwLock};

pub use export::{CsvOptions, export_csv};

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("spool io: {0}")]
    Io(#[from] std::io::Error),
    #[error("spool encoding: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("spool is closed")]
    Closed,
}

pub type Result<T, E = SpoolError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy)]
struct BatchMeta {
    offset: u64,
    len: u64,
    first_row: u64,
    rows: u32,
}

/// Where the producer is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// More rows may arrive.
    Open,
    /// All rows of the result are in the spool.
    Complete,
    /// The producer stopped early (cancelled, closed, or failed) with this message.
    Stopped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub rows: u64,
    pub bytes_on_disk: u64,
    pub status: Status,
}

struct State {
    index: Vec<BatchMeta>,
    rows: u64,
    bytes: u64,
    status: Status,
}

struct Lru {
    cap: usize,
    /// Most recently used last.
    entries: Vec<(usize, RecordBatch)>,
}

impl Lru {
    fn get(&mut self, k: usize) -> Option<RecordBatch> {
        let i = self.entries.iter().position(|(key, _)| *key == k)?;
        let e = self.entries.remove(i);
        let b = e.1.clone();
        self.entries.push(e);
        Some(b)
    }
    fn put(&mut self, k: usize, b: RecordBatch) {
        if self.entries.len() >= self.cap {
            self.entries.remove(0);
        }
        self.entries.push((k, b));
    }
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool").field("progress", &self.progress()).finish()
    }
}

pub struct Spool {
    schema: SchemaRef,
    state: RwLock<State>,
    changed: Condvar,
    changed_lock: Mutex<()>,
    writer: Mutex<Option<File>>,
    reader: Mutex<File>,
    cache: Mutex<Lru>,
    write_opts: IpcWriteOptions,
    // Deleted when the spool is dropped.
    _file: tempfile::NamedTempFile,
}

impl Spool {
    /// Create a spool file in `dir`.
    pub fn create(dir: &Path, schema: SchemaRef) -> Result<Arc<Spool>> {
        std::fs::create_dir_all(dir)?;
        let file = tempfile::Builder::new().prefix("gq-spool-").suffix(".arrows").tempfile_in(dir)?;
        let writer = file.reopen()?;
        let reader = file.reopen()?;
        let write_opts = IpcWriteOptions::default().try_with_compression(Some(CompressionType::LZ4_FRAME))?;
        Ok(Arc::new(Spool {
            schema,
            state: RwLock::new(State { index: Vec::new(), rows: 0, bytes: 0, status: Status::Open }),
            changed: Condvar::new(),
            changed_lock: Mutex::new(()),
            writer: Mutex::new(Some(writer)),
            reader: Mutex::new(reader),
            cache: Mutex::new(Lru { cap: 16, entries: Vec::new() }),
            write_opts,
            _file: file,
        }))
    }

    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    pub fn progress(&self) -> Progress {
        let s = self.state.read();
        Progress { rows: s.rows, bytes_on_disk: s.bytes, status: s.status.clone() }
    }

    pub fn rows(&self) -> u64 {
        self.state.read().rows
    }

    /// Append a batch (writer only).
    pub fn append(&self, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(batch.get_array_memory_size() / 2);
        {
            let mut w = StreamWriter::try_new_with_options(&mut buf, &self.schema, self.write_opts.clone())?;
            w.write(batch)?;
            w.finish()?;
        }
        let mut wguard = self.writer.lock();
        let w = wguard.as_mut().ok_or(SpoolError::Closed)?;
        let offset = {
            let s = self.state.read();
            s.bytes
        };
        w.write_all(&buf)?;
        {
            let mut s = self.state.write();
            let first_row = s.rows;
            s.index.push(BatchMeta { offset, len: buf.len() as u64, first_row, rows: batch.num_rows() as u32 });
            s.rows += batch.num_rows() as u64;
            s.bytes += buf.len() as u64;
        }
        drop(wguard);
        self.notify();
        Ok(())
    }

    /// Mark the end of the result (writer only).
    pub fn finish(&self, status: Status) {
        debug_assert!(status != Status::Open);
        if let Some(mut w) = self.writer.lock().take() {
            let _ = w.flush();
        }
        self.state.write().status = status;
        self.notify();
    }

    fn notify(&self) {
        let _g = self.changed_lock.lock();
        self.changed.notify_all();
    }

    /// Block until more than `rows` rows are available, the spool is no
    /// longer open, or `timeout` passes. Returns the current progress.
    pub fn wait_beyond(&self, rows: u64, timeout: Duration) -> Progress {
        let mut g = self.changed_lock.lock();
        loop {
            let p = self.progress();
            if p.rows > rows || p.status != Status::Open {
                return p;
            }
            if self.changed.wait_for(&mut g, timeout).timed_out() {
                return self.progress();
            }
        }
    }

    fn batch(&self, i: usize, meta: BatchMeta) -> Result<RecordBatch> {
        if let Some(b) = self.cache.lock().get(i) {
            return Ok(b);
        }
        let b = self.read_uncached(meta)?;
        self.cache.lock().put(i, b.clone());
        Ok(b)
    }

    /// Rows in `range` (clamped to what is available) as zero-copy slices,
    /// in order. Their row counts sum to the clamped range length.
    pub fn read(&self, range: Range<u64>) -> Result<Vec<RecordBatch>> {
        let metas: Vec<(usize, BatchMeta)> = {
            let s = self.state.read();
            let end = range.end.min(s.rows);
            if range.start >= end {
                return Ok(Vec::new());
            }
            // First batch whose end is beyond range.start.
            let first = s.index.partition_point(|m| m.first_row + m.rows as u64 <= range.start);
            s.index[first..]
                .iter()
                .enumerate()
                .take_while(|(_, m)| m.first_row < end)
                .map(|(j, m)| (first + j, *m))
                .collect()
        };
        let end = range.end.min(self.rows());
        let mut out = Vec::with_capacity(metas.len());
        for (i, m) in metas {
            let b = self.batch(i, m)?;
            let lo = range.start.max(m.first_row) - m.first_row;
            let hi = end.min(m.first_row + m.rows as u64) - m.first_row;
            out.push(b.slice(lo as usize, (hi - lo) as usize));
        }
        Ok(out)
    }

    /// Visit every batch in order, waiting for the producer as needed, until
    /// the spool is complete/stopped and all its batches have been visited.
    /// Returns the final status. `f` returning `false` aborts with `Ok(None)`.
    pub fn for_each_batch(&self, mut f: impl FnMut(&RecordBatch) -> bool) -> Result<Option<Status>> {
        let mut next = 0usize;
        loop {
            let (pending, status, rows_seen) = {
                let s = self.state.read();
                let pending: Vec<(usize, BatchMeta)> =
                    s.index[next..].iter().enumerate().map(|(j, m)| (next + j, *m)).collect();
                (pending, s.status.clone(), s.rows)
            };
            for (i, m) in &pending {
                // Bypass the LRU so a full export doesn't evict the grid's batches.
                let b = self.read_uncached(*m)?;
                if !f(&b) {
                    return Ok(None);
                }
                next = i + 1;
            }
            if pending.is_empty() {
                if status != Status::Open {
                    return Ok(Some(status));
                }
                self.wait_beyond(rows_seen, Duration::from_millis(200));
            }
        }
    }

    fn read_uncached(&self, meta: BatchMeta) -> Result<RecordBatch> {
        let mut buf = vec![0u8; meta.len as usize];
        {
            let mut r = self.reader.lock();
            r.seek(SeekFrom::Start(meta.offset))?;
            r.read_exact(&mut buf)?;
        }
        let mut reader = StreamReader::try_new(std::io::Cursor::new(buf), None)?;
        Ok(reader
            .next()
            .ok_or_else(|| SpoolError::Arrow(arrow_schema::ArrowError::IpcError("empty spool batch".into())))??)
    }
}

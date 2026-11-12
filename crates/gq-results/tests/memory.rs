//! Memory stays bounded regardless of result size. Runs alone in its own
//! binary so the counting allocator only sees this test.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{batch, ids, schema};
use gq_results::{CsvOptions, Spool, Status, export_csv};

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let c = CUR.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(c, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

const MB: usize = 1 << 20;

#[test]
fn large_result_has_bounded_memory() {
    let d = tempfile::tempdir().unwrap();
    let s = Spool::create(d.path(), schema()).unwrap();
    let base = CUR.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);

    // ~2M rows x ~140 bytes: well over 200MB of row data flows through.
    let per = 8192;
    let batches = 250;
    let mut raw_bytes = 0;
    for i in 0..batches {
        let b = batch(i * per as i64, per);
        raw_bytes += b.get_array_memory_size();
        s.append(&b).unwrap();
    }
    s.finish(Status::Complete);
    let total = (batches as usize * per) as u64;
    assert_eq!(s.rows(), total);

    // Scroll around: random windows, like a user dragging the scrollbar.
    let mut x: u64 = 12345;
    for _ in 0..500 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let start = (x >> 11) % (total - 300);
        let got = ids(&s.read(start..start + 300).unwrap());
        assert_eq!(got.first().copied(), Some(start as i64));
        assert_eq!(got.len(), 300);
    }

    // Full export.
    let out = d.path().join("all.csv");
    let (_, n) = export_csv(&s, &out, &CsvOptions::default(), |_| true).unwrap().unwrap();
    assert_eq!(n, total);

    let peak = PEAK.load(Ordering::Relaxed) - base;
    eprintln!(
        "raw row data {} MB, spool on disk {} MB, peak heap {} MB",
        raw_bytes / MB,
        s.progress().bytes_on_disk as usize / MB,
        peak / MB
    );
    assert!(raw_bytes > 200 * MB);
    // 16 cached batches of ~1.2MB + one batch being encoded + CSV buffer.
    assert!(peak < 48 * MB, "peak heap {} MB", peak / MB);
}

mod common;
use common::{batch, schema};
use nbq_results::{ByteBudget, Spool, SpoolLimits};

#[test]
fn shared_disk_budget_follows_the_last_result_owner() {
    let dir = tempfile::tempdir().unwrap();
    let disk = ByteBudget::new(4096);
    let limits = SpoolLimits { disk: disk.clone(), ..Default::default() };
    let a = Spool::create_with_limits(dir.path(), schema(), limits.clone()).unwrap();
    let b = Spool::create_with_limits(dir.path(), schema(), limits).unwrap();
    a.append(&batch(0, 10)).unwrap();
    let reserved = disk.used();
    assert!(reserved > 0);
    while b.append(&batch(0, 10)).is_ok() {}
    assert!(disk.used() <= 4096);
    let rows = b.rows();
    assert!(b.append(&batch(0, 10)).is_err());
    assert_eq!(b.rows(), rows);
    let retained = a.clone();
    drop(a);
    drop(b);
    assert_eq!(disk.used(), reserved);
    drop(retained);
    assert_eq!(disk.used(), 0);
}

#[test]
fn caches_obey_byte_budgets_and_reclaim_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ByteBudget::new(30 * 1024);
    let limits = SpoolLimits { cache_bytes: 20 * 1024, cache: cache.clone(), ..Default::default() };
    let a = Spool::create_with_limits(dir.path(), schema(), limits.clone()).unwrap();
    let b = Spool::create_with_limits(dir.path(), schema(), limits).unwrap();
    for spool in [&a, &b] {
        spool.append(&batch(0, 100)).unwrap();
        spool.append(&batch(100, 100)).unwrap();
        spool.read(0..1).unwrap();
        spool.read(100..101).unwrap();
    }
    assert!(cache.used() > 0 && cache.used() <= 30 * 1024);
    drop(a);
    drop(b);
    assert_eq!(cache.used(), 0);
}

#[test]
fn rejects_oversized_batches_and_full_index_without_publishing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let small = SpoolLimits { max_batch_bytes: 1024, ..Default::default() };
    let spool = Spool::create_with_limits(dir.path(), schema(), small).unwrap();
    assert!(spool.append(&batch(0, 100)).is_err());
    assert_eq!(spool.rows(), 0);
    assert_eq!(spool.progress().bytes_on_disk, 0);
    let limited = SpoolLimits { max_batches: 1, ..Default::default() };
    let spool = Spool::create_with_limits(dir.path(), schema(), limited).unwrap();
    spool.append(&batch(0, 100)).unwrap();
    assert!(spool.append(&batch(100, 100)).is_err());
    assert_eq!(spool.rows(), 100);
}

#[test]
fn bounded_reads_return_a_nonempty_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let spool = Spool::create(dir.path(), schema()).unwrap();
    spool.append(&batch(0, 100)).unwrap();
    spool.append(&batch(100, 100)).unwrap();
    let batches = spool.read_bounded(0..200, 1).unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 100);
}

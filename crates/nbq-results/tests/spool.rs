mod common;

use std::sync::Arc;
use std::time::Duration;

use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use common::{batch, ids, schema};
use nbq_results::{CsvOptions, Spool, Status, export_csv};

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn windows_across_batch_boundaries() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    // Uneven batch sizes, like a driver's first small batch then big ones.
    let mut start = 0;
    for n in [200, 4096, 1, 4096, 333] {
        s.append(&batch(start, n)).unwrap();
        start += n as i64;
    }
    s.finish(Status::Complete);
    assert_eq!(s.rows(), start as u64);
    for (a, b) in [(0, 10), (190, 210), (4290, 4300), (4296, 4298), (0, start as u64), (8700, 9000), (8790, 8800)] {
        let got = ids(&s.read(a..b).unwrap());
        let want: Vec<i64> = (a as i64..(b as i64).min(start)).collect();
        assert_eq!(got, want, "window {a}..{b}");
    }
    assert!(s.read(start as u64..start as u64 + 10).unwrap().is_empty());
    assert!(s.read(5..5).unwrap().is_empty());
}

#[test]
fn file_removed_on_drop() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    s.append(&batch(0, 10)).unwrap();
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    drop(s);
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
}

#[test]
fn compression_keeps_disk_small() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    for i in 0..10 {
        s.append(&batch(i * 4096, 4096)).unwrap();
    }
    let raw: usize = (0..10).map(|i| batch(i * 4096, 4096).get_array_memory_size()).sum();
    let on_disk = s.progress().bytes_on_disk as usize;
    assert!(on_disk < raw / 3, "on disk {on_disk} vs raw {raw}");
}

/// Export follows a producer that is still writing, and ends with everything.
#[test]
fn export_follows_live_producer() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    s.append(&batch(0, 100)).unwrap();
    let producer = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || {
            for i in 1..20 {
                std::thread::sleep(Duration::from_millis(10));
                s.append(&batch(i * 100, 100)).unwrap();
            }
            s.finish(Status::Complete);
        })
    };
    let path = d.path().join("out.csv");
    let (status, n) = export_csv(&s, &path, &CsvOptions::default(), |_| true).unwrap().unwrap();
    producer.join().unwrap();
    assert_eq!((status, n), (Status::Complete, 2000));
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2001);
    assert_eq!(lines[0], "id,name,score");
    assert!(lines[1].starts_with("0,row 0 abcdefghij"), "{}", lines[1]);
    assert!(lines[1].ends_with(','), "NULL score should be empty: {}", lines[1]);
    assert!(lines[2].ends_with(",0.5"), "{}", lines[2]);
}

#[test]
fn export_reports_stopped_producer_and_cancel() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    s.append(&batch(0, 50)).unwrap();
    s.finish(Status::Stopped("cancelled".into()));
    let path = d.path().join("a.csv");
    let (status, n) = export_csv(&s, &path, &CsvOptions::default(), |_| true).unwrap().unwrap();
    assert_eq!((status, n), (Status::Stopped("cancelled".into()), 50));
    assert!(!path.exists());

    // Cancelling the export removes the partial file.
    let p2 = d.path().join("b.csv");
    assert!(export_csv(&s, &p2, &CsvOptions::default(), |_| false).unwrap().is_none());
    assert!(!p2.exists());
}

#[test]
fn export_never_overwrites_an_existing_destination() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    s.append(&batch(0, 50)).unwrap();
    s.finish(Status::Complete);
    let path = d.path().join("existing.csv");
    std::fs::write(&path, "keep me").unwrap();
    assert!(export_csv(&s, &path, &CsvOptions::default(), |_| true).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    assert!(export_csv(&s, &path, &CsvOptions::default(), |_| false).unwrap().is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[test]
fn csv_quoting() {
    let d = dir();
    let schema = Arc::new(Schema::new(vec![Field::new("t", DataType::Utf8, true)]));
    let s = Spool::create(d.path(), schema.clone()).unwrap();
    let vals = StringArray::from(vec![Some("a,b"), Some("say \"hi\""), Some("line1\nline2"), None, Some("")]);
    s.append(&RecordBatch::try_new(schema, vec![Arc::new(vals)]).unwrap()).unwrap();
    s.finish(Status::Complete);
    let path = d.path().join("q.csv");
    let opts = CsvOptions { null: "NULL".into(), ..Default::default() };
    export_csv(&s, &path, &opts, |_| true).unwrap().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "t\n\"a,b\"\n\"say \"\"hi\"\"\"\n\"line1\nline2\"\nNULL\n\"\"\n");
}

#[test]
fn wait_beyond_wakes_on_append_and_finish() {
    let d = dir();
    let s = Spool::create(d.path(), schema()).unwrap();
    let w = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.wait_beyond(0, Duration::from_secs(10)))
    };
    std::thread::sleep(Duration::from_millis(50));
    s.append(&batch(0, 5)).unwrap();
    assert_eq!(w.join().unwrap().rows, 5);
    let w = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.wait_beyond(5, Duration::from_secs(10)))
    };
    std::thread::sleep(Duration::from_millis(50));
    s.finish(Status::Complete);
    assert_eq!(w.join().unwrap().status, Status::Complete);
}

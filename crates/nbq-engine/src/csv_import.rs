use nbq_driver::{
    ConnectParams, Execution, Session,
    csv_import::{Recipe, literal, quote, reader},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone, Default)]
pub struct ImportProgress {
    pub cancelled: Arc<AtomicBool>,
    pub rows: Arc<AtomicU64>,
}
async fn execute(db: &mut dyn Session, sql: &str) -> Result<(), String> {
    if let Execution::Rows(mut rows) = db.execute(sql).await.map_err(|e| e.to_string())? {
        while rows
            .next_batch(1000, Duration::from_millis(20))
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {}
    }
    Ok(())
}
pub async fn run(mut recipe: Recipe, create: bool, reload: bool, progress: ImportProgress) -> Result<Recipe, String> {
    if progress.cancelled.load(Ordering::Acquire) {
        return Err("Import cancelled".into());
    }
    if !recipe.engine.is_file_based() || recipe.columns.is_empty() || recipe.table.trim().is_empty() {
        return Err("Choose a database, table, and columns".into());
    }
    if recipe
        .columns
        .iter()
        .any(|c| !["TEXT", "BIGINT", "DOUBLE"].contains(&c.kind.as_str()) || c.name.trim().is_empty())
    {
        return Err("Invalid column name or type".into());
    }
    let source = recipe.source.clone();
    let metadata = tokio::task::spawn_blocking(move || std::fs::metadata(source))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if create {
        let (kind, path) = (recipe.engine, recipe.database.clone());
        tokio::task::spawn_blocking(move || nbq_driver::create_database(kind, &path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    }
    let mut db = match nbq_driver::connect(&ConnectParams::file(recipe.engine, &recipe.database)).await {
        Ok(db) => db,
        Err(e) => {
            if create { let path = recipe.database.clone(); let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await; }
            return Err(e.to_string());
        }
    };
    let result = async {
        execute(&mut *db, "BEGIN").await?;
        let table = quote(&recipe.table);
        if reload {
            execute(&mut *db, &format!("DELETE FROM {table}")).await?;
        } else {
            let columns = recipe
                .columns
                .iter()
                .map(|c| format!("{} {}", quote(&c.name), c.kind))
                .collect::<Vec<_>>()
                .join(", ");
            execute(&mut *db, &format!("CREATE TABLE {table} ({columns})")).await?;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let input = recipe.clone();
        let cancel = progress.cancelled.clone();
        let expected_size = metadata.len();
        let expected_modified = metadata.modified().ok();
        let producer = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut csv = reader(&input.source, input.delimiter, input.headers)?;
            if input.headers
                && !input.source_columns.is_empty()
                && csv
                    .headers()
                    .map_err(|e| e.to_string())?
                    .iter()
                    .ne(input.source_columns.iter().map(String::as_str))
            {
                return Err("CSV headers changed. Use Import CSV… to review the new columns.".into());
            }
            let mut batch = Vec::new();
            let mut bytes = 0;
            for (i, row) in csv.records().enumerate() {
                if cancel.load(Ordering::Acquire) {
                    return Err("Import cancelled".into());
                }
                let row = row.map_err(|e| e.to_string())?;
                if row.len() != input.columns.len() {
                    return Err(format!("Row {}: column count changed", i + 1));
                }
                if row.as_slice().len() > nbq_driver::MAX_ROW_BYTES {
                    return Err(format!("Row {} exceeds 16 MiB", i + 1));
                }
                let values = row
                    .iter()
                    .zip(&input.columns)
                    .map(|(v, c)| literal(v, &c.kind))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Row {}: {e}. Adjust the import column types.", i + 1))?;
                let value = format!("({})", values.join(","));
                bytes += value.len();
                batch.push(value);
                if batch.len() >= 250 || bytes >= 256 * 1024 {
                    tx.blocking_send(std::mem::take(&mut batch))
                        .map_err(|_| "Import stopped")?;
                    bytes = 0;
                }
            }
            if !batch.is_empty() {
                tx.blocking_send(batch).map_err(|_| "Import stopped")?;
            }
            let after = std::fs::metadata(&input.source).map_err(|e| e.to_string())?;
            if after.len() != expected_size || after.modified().ok() != expected_modified {
                return Err("The CSV changed during import. Retry with a stable source file.".into());
            }
            Ok(())
        });
        let columns = recipe
            .columns
            .iter()
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(",");
        while let Some(batch) = rx.recv().await {
            if progress.cancelled.load(Ordering::Acquire) {
                return Err("Import cancelled".into());
            }
            execute(
                &mut *db,
                &format!("INSERT INTO {table} ({columns}) VALUES {}", batch.join(",")),
            )
            .await?;
            progress.rows.fetch_add(batch.len() as u64, Ordering::Release);
        }
        producer.await.map_err(|e| e.to_string())??;
        if progress.cancelled.load(Ordering::Acquire) {
            return Err("Import cancelled".into());
        }
        execute(&mut *db, "COMMIT").await
    }
    .await;
    if let Err(e) = result {
        let _ = execute(&mut *db, "ROLLBACK").await;
        drop(db);
        if create {
            let path = recipe.database.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
        }
        return Err(e);
    }
    recipe.source_size = metadata.len();
    recipe.source_modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    recipe.imported_at = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(recipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nbq_driver::{DbKind, csv_import::preview};

    #[tokio::test]
    async fn snapshots_reload_atomically_in_both_engines() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/csv-tests");
        std::fs::create_dir_all(&root).unwrap();
        for engine in [DbKind::Sqlite, DbKind::DuckDb] {
            let dir = tempfile::tempdir_in(&root).unwrap();
            let source = dir.path().join("source.csv");
            std::fs::write(&source, "id,label\n1,\"a,b\"\n2,\"O'Brien\"\n").unwrap();
            let p = preview(&source, None, true).unwrap();
            let recipe = Recipe {
                source: source.clone(),
                database: dir.path().join(engine.default_file_name()),
                engine,
                table: "sales data".into(),
                delimiter: p.delimiter,
                headers: true,
                columns: p.columns,
                source_columns: p.source_columns,
                imported_at: 0,
                source_size: 0,
                source_modified: 0,
            };
            let recipe = run(recipe, true, false, ImportProgress::default()).await.unwrap();
            let mut db = nbq_driver::connect(&ConnectParams::file(engine, &recipe.database))
                .await
                .unwrap();
            execute(&mut *db, "CREATE TABLE unrelated (id INTEGER)").await.unwrap();
            std::fs::write(&source, "id,label\n3,new\ninvalid,bad\n").unwrap();
            assert!(
                run(recipe.clone(), false, true, ImportProgress::default())
                    .await
                    .is_err()
            );
            let Execution::Rows(mut rows) = db.execute("SELECT id FROM \"sales data\" ORDER BY id").await.unwrap()
            else {
                panic!()
            };
            assert_eq!(
                rows.next_batch(100, Duration::from_secs(1))
                    .await
                    .unwrap()
                    .unwrap()
                    .num_rows(),
                2
            );
            drop(rows);
            std::fs::write(&source, "label,id\nnew,3\n").unwrap();
            assert!(
                run(recipe.clone(), false, true, ImportProgress::default())
                    .await
                    .unwrap_err()
                    .contains("headers changed")
            );
            let cancelled = ImportProgress::default();
            cancelled.cancelled.store(true, Ordering::Release);
            assert!(run(recipe.clone(), false, true, cancelled).await.is_err());
            std::fs::write(&source, "id,label\n3,new\n").unwrap();
            run(recipe.clone(), false, true, ImportProgress::default())
                .await
                .unwrap();
            let Execution::Rows(mut rows) = db.execute("SELECT * FROM \"sales data\"").await.unwrap() else {
                panic!()
            };
            assert_eq!(
                rows.next_batch(100, Duration::from_secs(1))
                    .await
                    .unwrap()
                    .unwrap()
                    .num_rows(),
                1
            );
            drop(rows);
            execute(&mut *db, "SELECT * FROM unrelated").await.unwrap();
            std::fs::write(&source, format!("id,label\n{}", "4,next\n".repeat(100_000))).unwrap();
            let progress = ImportProgress::default();
            let worker = tokio::spawn(run(recipe.clone(), false, true, progress.clone()));
            tokio::time::timeout(Duration::from_secs(10), async {
                while progress.rows.load(Ordering::Acquire) == 0 { tokio::time::sleep(Duration::from_millis(1)).await; }
            }).await.unwrap();
            progress.cancelled.store(true, Ordering::Release);
            assert!(worker.await.unwrap().is_err());
            let Execution::Rows(mut rows) = db.execute("SELECT * FROM \"sales data\"").await.unwrap() else { panic!() };
            assert_eq!(rows.next_batch(100, Duration::from_secs(1)).await.unwrap().unwrap().num_rows(), 1);
            drop(rows);
            assert!(run(recipe, true, false, ImportProgress::default()).await.is_err());
        }
    }
}

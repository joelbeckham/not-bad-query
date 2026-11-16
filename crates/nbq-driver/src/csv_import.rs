//! CSV recipes and bounded previews shared by the engine and project state.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub kind: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    pub source: PathBuf,
    pub database: PathBuf,
    pub engine: crate::DbKind,
    pub table: String,
    pub delimiter: u8,
    pub headers: bool,
    pub columns: Vec<Column>,
    #[serde(default)]
    pub source_columns: Vec<String>,
    #[serde(default)]
    pub imported_at: u64,
    #[serde(default)]
    pub source_size: u64,
    #[serde(default)]
    pub source_modified: u64,
}
#[derive(Debug, Clone)]
pub struct Preview {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
    pub delimiter: u8,
    pub source_columns: Vec<String>,
}
pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
pub fn reader(path: &Path, delimiter: u8, headers: bool) -> Result<csv::Reader<std::fs::File>, String> {
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(headers)
        .from_path(path)
        .map_err(|e| e.to_string())
}
pub fn preview(path: &Path, delimiter: Option<u8>, headers: bool) -> Result<Preview, String> {
    let delimiter = delimiter.unwrap_or_else(|| {
        [b',', b'\t', b';', b'|']
            .into_iter()
            .max_by_key(|d| {
                reader(path, *d, false)
                    .ok()
                    .and_then(|mut r| r.records().next())
                    .and_then(Result::ok)
                    .map_or(0, |r| r.len())
            })
            .unwrap_or(b',')
    });
    let mut reader = reader(path, delimiter, headers)?;
    let names = reader.headers().map_err(|e| e.to_string())?.clone();
    if names.is_empty() || names.len() > 1024 {
        return Err("CSV must contain between 1 and 1024 columns".into());
    }
    let mut used = std::collections::HashSet::new();
    let mut columns: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let base = if headers && !n.trim().is_empty() {
                n.trim_start_matches('\u{feff}').trim().to_string()
            } else {
                format!("column_{}", i + 1)
            };
            let mut name = base.clone();
            let mut suffix = 2;
            while !used.insert(name.to_lowercase()) {
                name = format!("{base}_{suffix}");
                suffix += 1;
            }
            Column {
                name,
                kind: "BIGINT".into(),
            }
        })
        .collect();
    let mut rows = Vec::new();
    for result in reader.records().take(100) {
        let record = result.map_err(|e| e.to_string())?;
        if record.as_slice().len() > crate::MAX_ROW_BYTES {
            return Err("CSV row exceeds 16 MiB".into());
        }
        for (column, value) in columns.iter_mut().zip(record.iter()) {
            if value.is_empty() {
                continue;
            }
            let leading_zero = value.len() > 1 && value.starts_with('0') && !value.starts_with("0.");
            if leading_zero || value.trim() != value {
                column.kind = "TEXT".into();
            } else if column.kind == "BIGINT" && value.parse::<i64>().is_err() {
                column.kind = "DOUBLE".into();
            }
            if column.kind == "DOUBLE" && !value.parse::<f64>().is_ok_and(|v| v.is_finite()) {
                column.kind = "TEXT".into();
            }
        }
        if rows.len() < 5 {
            rows.push(record.iter().map(|v| v.chars().take(120).collect()).collect());
        }
    }
    Ok(Preview {
        columns,
        rows,
        delimiter,
        source_columns: if headers {
            names.iter().map(str::to_string).collect()
        } else {
            Vec::new()
        },
    })
}

pub fn literal(value: &str, kind: &str) -> Result<String, String> {
    if value.is_empty() {
        return Ok("NULL".into());
    }
    match kind {
        "TEXT" => Ok(format!("'{}'", value.replace('\'', "''"))),
        "BIGINT" => value
            .parse::<i64>()
            .map(|v| v.to_string())
            .map_err(|_| format!("{value:?} is not an integer")),
        "DOUBLE" => value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v.to_string())
            .ok_or_else(|| format!("{value:?} is not a number")),
        _ => Err("Unsupported column type".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_delimiter_preserves_identifiers_and_quotes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/csv-preview-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("input.csv");
        std::fs::write(&path, "id;id;amount\n001;2;2.50\n002;3;4.25\n").unwrap();
        let p = preview(&path, None, true).unwrap();
        assert_eq!(p.delimiter, b';');
        assert_eq!(p.columns[0].kind, "TEXT");
        assert_eq!(p.columns[1].name, "id_2");
        assert_eq!(p.columns[2].kind, "DOUBLE");
        assert_eq!(literal("O'Brien", "TEXT").unwrap(), "'O''Brien'");
    }
}

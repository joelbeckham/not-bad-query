//! Blocking script IO. Call from a background executor, never a render callback.
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

pub const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;

pub fn read_script(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?.take(MAX_SCRIPT_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(io::Error::other("Scripts larger than 8 MiB are not supported"));
    }
    Ok(text)
}

/// Replace only the revision the editor opened, or create a new file without
/// clobbering an existing destination. Same-directory temp files avoid torn saves.
pub fn save_script(path: &Path, text: &str, expected: Option<&str>) -> io::Result<()> {
    if text.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(io::Error::other("Scripts larger than 8 MiB are not supported"));
    }
    write_atomic(path, text, expected, false)
}

pub(crate) fn write_atomic(path: &Path, text: &str, expected: Option<&str>, replace: bool) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(text.as_bytes())?;
    temp.as_file().sync_all()?;
    for attempt in 0..=20 {
        if let Some(expected) = expected {
            if read_script(path)? != expected {
                return Err(io::Error::other("This script changed on disk. Reopen it or use Save As."));
            }
        }
        let result = if replace || expected.is_some() { temp.persist(path) } else { temp.persist_noclobber(path) };
        match result {
            Ok(_) => return Ok(()),
            Err(error) if cfg!(windows) && attempt < 20 && matches!(error.error.raw_os_error(), Some(5 | 32 | 33)) => {
                temp = error.file;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.error),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_preserves_external_edits_and_existing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("query.sql");
        save_script(&path, "select 1", None).unwrap();
        assert!(save_script(&path, "overwrite", None).is_err());
        save_script(&path, "select 2", Some("select 1")).unwrap();
        std::fs::write(&path, "external edit").unwrap();
        assert!(save_script(&path, "editor edit", Some("select 2")).is_err());
        assert_eq!(read_script(&path).unwrap(), "external edit");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        std::fs::remove_file(&path).unwrap();
        assert!(save_script(&path, "editor edit", Some("select 2")).is_err());
        assert!(!path.exists());
    }
}

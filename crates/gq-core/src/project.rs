//! A project is a folder (often inside a git repo) holding connections and
//! scripts:
//!
//! ```text
//! <root>/
//!   good-query.toml              project marker: id + name (shared)
//!   connections/<folder>/<Name>.toml   one file per connection (shared)
//!   scripts/...                  SQL files (shared)
//!   exports/                     CSV exports (git-ignored)
//!   .good-query/local.toml       per-user state (git-ignored)
//!   .gitignore
//! ```
//!
//! A connection's name and folder come from its file path, so renaming or
//! moving a connection renames or moves its file. Usernames and passwords
//! never go into the project; they live in the OS credential store (see
//! [`crate::secrets`]).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, Connection, Connections, Result, read_toml, write_atomic};

pub const MARKER: &str = "good-query.toml";
pub const CONNECTIONS_DIR: &str = "connections";
pub const SCRIPTS_DIR: &str = "scripts";
pub const EXPORTS_DIR: &str = "exports";
pub const LOCAL_DIR: &str = ".good-query";
const LOCAL_FILE: &str = "local.toml";
const GITIGNORE: &str = "# good-query: per-user state and exports stay out of git\n.good-query/\nexports/\n";

/// Unique per process, so two projects (even copies with the same id) never
/// share engine caches or tree ids.
static NEXT_KEY: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub id: String,
    pub name: String,
}

/// Per-user, per-project state (git-ignored).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalState {
    /// Script path (relative to `scripts/`, `/`-separated) → connection id.
    pub script_connections: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Project {
    /// Runtime key, unique within this process.
    pub key: u32,
    pub root: PathBuf,
    pub meta: ProjectMeta,
    pub connections: Connections,
    pub local: LocalState,
    /// Connection files that could not be read (path: error).
    pub warnings: Vec<String>,
}

/// Characters Windows (and sanity) forbid in file names.
fn file_name_for(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c })
        .collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() { "connection".into() } else { s }
}

/// Folder path (`Prod/US`) with each segment made file-name safe.
fn clean_folder(folder: &str) -> Option<String> {
    let parts: Vec<String> =
        folder.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty() && *s != "." && *s != "..").map(file_name_for).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Io { path: path.to_path_buf(), source }
}

impl Project {
    pub fn is_project(dir: &Path) -> bool {
        dir.join(MARKER).is_file()
    }

    /// Make `root` a project (it may already contain other files, e.g. a
    /// repo subfolder). Fails if it already is one.
    pub fn create(root: &Path, name: &str) -> Result<Project> {
        if Self::is_project(root) {
            return Err(ConfigError::Project(format!("{} is already a good-query project", root.display())));
        }
        for d in [CONNECTIONS_DIR, SCRIPTS_DIR, EXPORTS_DIR, LOCAL_DIR] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(io_err(&p))?;
        }
        let gi = root.join(".gitignore");
        if !gi.exists() {
            std::fs::write(&gi, GITIGNORE).map_err(io_err(&gi))?;
        }
        let name = if name.trim().is_empty() {
            root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".into())
        } else {
            name.trim().to_string()
        };
        let meta = ProjectMeta { id: uuid::Uuid::new_v4().to_string(), name };
        write_atomic(&root.join(MARKER), &toml::to_string_pretty(&meta)?)?;
        Self::open(root)
    }

    pub fn open(root: &Path) -> Result<Project> {
        let marker = root.join(MARKER);
        if !marker.is_file() {
            return Err(ConfigError::Project(format!(
                "{} is not a good-query project (no {MARKER})",
                root.display()
            )));
        }
        let meta: ProjectMeta = read_toml(&marker)?.ok_or_else(|| ConfigError::Project(format!("{} disappeared", marker.display())))?;
        let local = read_toml(&root.join(LOCAL_DIR).join(LOCAL_FILE))?.unwrap_or_default();
        let mut p = Project {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            root: root.to_path_buf(),
            meta,
            connections: Connections::default(),
            local,
            warnings: Vec::new(),
        };
        p.reload()?;
        Ok(p)
    }

    pub fn name(&self) -> &str {
        &self.meta.name
    }

    pub fn connections_dir(&self) -> PathBuf {
        self.root.join(CONNECTIONS_DIR)
    }

    pub fn scripts_dir(&self) -> PathBuf {
        self.root.join(SCRIPTS_DIR)
    }

    pub fn exports_dir(&self) -> PathBuf {
        self.root.join(EXPORTS_DIR)
    }

    /// Re-read connections from disk (after an external change). Files
    /// without an id, or with an id another file already uses, get a fresh
    /// one written back so ids stay unique and stable.
    pub fn reload(&mut self) -> Result<()> {
        let dir = self.connections_dir();
        let mut found = Vec::new();
        let mut folders = Vec::new();
        scan_connections(&dir, None, &mut found, &mut folders, 0);
        let mut seen = HashSet::new();
        let mut warnings = Vec::new();
        let mut conns = Vec::new();
        for (path, folder) in found {
            let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let mut c: Connection = match toml::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            if c.id.trim().is_empty() || !seen.insert(c.id.clone()) {
                c.id = uuid::Uuid::new_v4().to_string();
                seen.insert(c.id.clone());
                if let Err(e) = write_atomic(&path, &toml::to_string_pretty(&c)?) {
                    warnings.push(e.to_string());
                }
            }
            c.name = name;
            c.folder = folder;
            self.stamp(&mut c);
            conns.push(c);
        }
        self.connections = Connections { folders, connections: conns };
        self.warnings = warnings;
        Ok(())
    }

    fn stamp(&self, c: &mut Connection) {
        c.project_key = self.key;
        c.project_id = self.meta.id.clone();
    }

    fn conn_path(&self, folder: Option<&str>, name: &str) -> PathBuf {
        let mut p = self.connections_dir();
        if let Some(f) = folder {
            p.extend(f.split('/'));
        }
        p.join(format!("{}.toml", file_name_for(name)))
    }

    /// Path of an existing connection's file.
    pub fn connection_file(&self, c: &Connection) -> PathBuf {
        self.conn_path(c.folder.as_deref(), &c.name)
    }

    /// Add or update a connection. Renaming or moving it renames or moves
    /// its file. Returns the stored connection (name/folder normalized).
    pub fn save_connection(&mut self, mut c: Connection) -> Result<Connection> {
        c.folder = c.folder.as_deref().and_then(clean_folder);
        c.name = file_name_for(&c.name);
        let new_path = self.conn_path(c.folder.as_deref(), &c.name);
        let old_path = self.connections.get(&c.id).map(|old| self.connection_file(old));
        if old_path.as_deref() != Some(new_path.as_path()) && new_path.exists() {
            return Err(ConfigError::Project(format!(
                "a connection named \"{}\" already exists in that folder",
                c.name
            )));
        }
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        write_atomic(&new_path, &toml::to_string_pretty(&c)?)?;
        if let Some(old) = old_path.filter(|o| *o != new_path) {
            let _ = std::fs::remove_file(old);
        }
        self.stamp(&mut c);
        self.connections.upsert(c.clone());
        Ok(c)
    }

    pub fn delete_connection(&mut self, id: &str) -> Result<()> {
        if let Some(c) = self.connections.get(id) {
            let path = self.connection_file(c);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ConfigError::Io { path, source: e }),
            }
        }
        self.connections.remove(id);
        Ok(())
    }

    /// The script's path relative to `scripts/` (`/`-separated), if it is
    /// inside this project's scripts folder.
    pub fn script_rel(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(self.scripts_dir()).ok()?;
        Some(rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect::<Vec<_>>().join("/"))
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// The connection last used with this script, if it still exists.
    pub fn script_connection(&self, script: &Path) -> Option<&Connection> {
        let rel = self.script_rel(script)?;
        self.connections.get(self.local.script_connections.get(&rel)?)
    }

    /// Remember the connection for a script. Returns whether anything changed.
    pub fn set_script_connection(&mut self, script: &Path, conn_id: Option<&str>) -> Result<bool> {
        let Some(rel) = self.script_rel(script) else { return Ok(false) };
        let changed = match conn_id {
            Some(id) => self.local.script_connections.insert(rel, id.to_string()).as_deref() != Some(id),
            None => self.local.script_connections.remove(&rel).is_some(),
        };
        if changed {
            let dir = self.root.join(LOCAL_DIR);
            std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
            write_atomic(&dir.join(LOCAL_FILE), &toml::to_string_pretty(&self.local)?)?;
        }
        Ok(changed)
    }

    /// Whether a changed path should trigger a reload (exports and local
    /// state don't).
    pub fn is_relevant_change(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else { return false };
        match rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()) {
            Some(first) => first == CONNECTIONS_DIR || first == SCRIPTS_DIR || first == MARKER,
            None => false,
        }
    }
}

/// Connection files under `dir`, with their folder path.
fn scan_connections(
    dir: &Path,
    folder: Option<&str>,
    out: &mut Vec<(PathBuf, Option<String>)>,
    folders: &mut Vec<String>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            let sub = match folder {
                Some(f) => format!("{f}/{name}"),
                None => name,
            };
            folders.push(sub.clone());
            scan_connections(&p, Some(&sub), out, folders, depth + 1);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("toml")) {
            out.push((p, folder.map(str::to_string)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gq_driver::DbKind;

    fn conn(name: &str, folder: Option<&str>) -> Connection {
        let mut c = Connection::new(name, DbKind::Postgres);
        c.folder = folder.map(str::to_string);
        c
    }

    #[test]
    fn create_lays_out_folders_and_refuses_twice() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("queries");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("README.md"), "existing repo content").unwrap();
        let p = Project::create(&root, "").unwrap();
        assert_eq!(p.name(), "queries");
        for sub in [MARKER, CONNECTIONS_DIR, SCRIPTS_DIR, EXPORTS_DIR, ".gitignore"] {
            assert!(root.join(sub).exists(), "{sub}");
        }
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gi.contains("exports/") && gi.contains(".good-query/"), "{gi}");
        assert!(Project::create(&root, "again").is_err());
        assert!(Project::open(d.path()).is_err(), "parent isn't a project");
    }

    #[test]
    fn connections_are_files_in_folders() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Project::create(d.path(), "A").unwrap();
        let db1 = p.save_connection(conn("DB 1", Some("Prod"))).unwrap();
        p.save_connection(conn("DB 3", Some("Dev"))).unwrap();
        p.save_connection(conn("loose", None)).unwrap();
        assert!(d.path().join("connections/Prod/DB 1.toml").is_file());
        assert!(d.path().join("connections/loose.toml").is_file());

        let text = std::fs::read_to_string(d.path().join("connections/Prod/DB 1.toml")).unwrap();
        for absent in ["user", "password", "name", "folder", "project"] {
            assert!(!text.lines().any(|l| l.starts_with(absent)), "{absent} in:\n{text}");
        }

        // Rename + move: the file follows.
        let mut moved = db1.clone();
        moved.name = "Orders".into();
        moved.folder = Some("Prod/US".into());
        p.save_connection(moved).unwrap();
        assert!(!d.path().join("connections/Prod/DB 1.toml").exists());
        assert!(d.path().join("connections/Prod/US/Orders.toml").is_file());

        // A fresh open sees the same thing, with names/folders from paths.
        let q = Project::open(d.path()).unwrap();
        let mut got: Vec<_> =
            q.connections.connections.iter().map(|c| (c.folder.clone(), c.name.clone())).collect();
        got.sort();
        assert_eq!(
            got,
            [(None, "loose".into()), (Some("Dev".into()), "DB 3".into()), (Some("Prod/US".into()), "Orders".into())]
        );
        assert!(q.connections.folders.contains(&"Prod".to_string()), "empty folder kept");
        assert_eq!(q.connections.get(&db1.id).unwrap().name, "Orders", "id is stable");

        // Name clash in the same folder is refused.
        assert!(p.save_connection(conn("DB 3", Some("Dev"))).is_err());

        p.delete_connection(&db1.id).unwrap();
        assert!(!d.path().join("connections/Prod/US/Orders.toml").exists());
    }

    #[test]
    fn hand_written_and_copied_files_get_unique_ids() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Project::create(d.path(), "A").unwrap();
        let a = p.save_connection(conn("a", None)).unwrap();
        let dir = d.path().join("connections");
        std::fs::copy(dir.join("a.toml"), dir.join("b.toml")).unwrap();
        std::fs::write(dir.join("c.toml"), "kind = \"postgres\"\nhost = \"h\"\nport = 5432\ndatabase = \"d\"\n").unwrap();
        std::fs::write(dir.join("bad.toml"), "port = 'x'").unwrap();
        let q = Project::open(d.path()).unwrap();
        let ids: HashSet<_> = q.connections.connections.iter().map(|c| c.id.clone()).collect();
        assert_eq!((q.connections.connections.len(), ids.len()), (3, 3));
        assert!(ids.contains(&a.id));
        assert_eq!(q.warnings.len(), 1, "{:?}", q.warnings);
        assert!(q.warnings[0].contains("bad.toml"));
        // Ids were written back, so they're stable on the next open.
        let r = Project::open(d.path()).unwrap();
        let ids2: HashSet<_> = r.connections.connections.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, ids2);
    }

    #[test]
    fn script_connections_are_local() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Project::create(d.path(), "A").unwrap();
        let c = p.save_connection(conn("db", None)).unwrap();
        let script = p.scripts_dir().join("reports").join("daily.sql");
        assert!(p.set_script_connection(&script, Some(&c.id)).unwrap());
        assert!(!p.set_script_connection(&script, Some(&c.id)).unwrap(), "unchanged");
        assert!(!p.set_script_connection(Path::new("C:/elsewhere.sql"), Some(&c.id)).unwrap());
        let q = Project::open(d.path()).unwrap();
        assert_eq!(q.script_connection(&script).map(|c| c.name.as_str()), Some("db"));
        let local = std::fs::read_to_string(d.path().join(".good-query/local.toml")).unwrap();
        assert!(local.contains("reports/daily.sql"), "{local}");
    }

    #[test]
    fn relevant_changes() {
        let d = tempfile::tempdir().unwrap();
        let p = Project::create(d.path(), "A").unwrap();
        assert!(p.is_relevant_change(&d.path().join("connections/x.toml")));
        assert!(p.is_relevant_change(&d.path().join("scripts/a/b.sql")));
        assert!(!p.is_relevant_change(&d.path().join("exports/big.csv")));
        assert!(!p.is_relevant_change(&d.path().join(".good-query/local.toml")));
    }

    #[test]
    fn unsafe_names() {
        assert_eq!(file_name_for(" a/b:c? "), "a_b_c_");
        assert_eq!(file_name_for("..."), "connection");
        assert_eq!(clean_folder("/Prod//US/../x/"), Some("Prod/US/x".into()));
        assert_eq!(clean_folder(" / "), None);
    }
}

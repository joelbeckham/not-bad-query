//! Configuration.
//!
//! * Projects ([`project`]): folders holding connections and scripts, often
//!   inside a git repo.
//! * App config in the user's config dir: `settings.toml` (preferences) and
//!   `workspace.toml` (which projects are open).
//!
//! Usernames and passwords never touch any file; they live in the OS
//! credential store ([`secrets`]).

mod color;
pub mod files;
pub mod project;
mod tree;
pub mod watch;

use std::path::{Path, PathBuf};
use std::time::Duration;

use nbq_driver::{ConnectParams, DbKind, SslMode};
use serde::{Deserialize, Serialize};

pub use color::{ConnColor, PALETTE};
pub use project::{LocalState, Project, ProjectMeta};
pub use secrets::Credentials;
pub use tree::{FolderNode, build_tree};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("credential store: {0}")]
    Secret(String),
    #[error("{0}")]
    Project(String),
    #[error("file watcher: {0}")]
    Watch(String),
}

pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

/// A connection as stored in `connections/<folder>/<name>.toml`. The name
/// and folder come from the file path; the credentials from the OS store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    /// Stable id (survives renames); keys the stored credentials.
    #[serde(default)]
    pub id: String,
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub folder: Option<String>,
    pub kind: DbKind,
    /// File-backed database path, absolute or relative to the project root.
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub file_path: PathBuf,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    #[serde(default)]
    pub ssl: SslMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssl_root_cert: String,
    /// Palette name (`"red"`) or `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Default commit mode for tabs using this connection.
    #[serde(default = "yes")]
    pub autocommit: bool,
    /// The owning project's runtime key (see [`Project::key`]).
    #[serde(skip)]
    pub project_key: u32,
    /// The owning project's stable id.
    #[serde(skip)]
    pub project_id: String,
    #[serde(skip)]
    pub project_root: PathBuf,
}

fn path_is_empty(path: &Path) -> bool { path.as_os_str().is_empty() }
fn is_zero(port: &u16) -> bool { *port == 0 }

fn yes() -> bool {
    true
}

impl Connection {
    pub fn new(name: impl Into<String>, kind: DbKind) -> Self {
        Connection {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            folder: None,
            kind,
            file_path: PathBuf::new(),
            host: if kind.is_file_based() { String::new() } else { "localhost".into() },
            port: match kind {
                DbKind::Postgres => 5432,
                DbKind::Redshift => 5439,
                DbKind::Sqlite | DbKind::DuckDb => 0,
            },
            database: match kind {
                DbKind::Postgres => "postgres".into(),
                DbKind::Redshift => "dev".into(),
                DbKind::Sqlite | DbKind::DuckDb => String::new(),
            },
            ssl: SslMode::VerifyFull,
            ssl_root_cert: String::new(),
            color: None,
            autocommit: true,
            project_key: 0,
            project_id: String::new(),
            project_root: PathBuf::new(),
        }
    }

    /// Unique within this process (engine caches, tree ids, catalogs).
    pub fn key(&self) -> String {
        format!("{}:{}", self.project_key, self.id)
    }

    /// Credential store key: stable across runs.
    pub fn secret_key(&self) -> String {
        format!("{}/{}", self.project_id, self.id)
    }

    pub fn color(&self) -> Option<ConnColor> {
        self.color.as_deref().and_then(ConnColor::parse)
    }

    pub fn params(&self, creds: Option<Credentials>) -> ConnectParams {
        let creds = creds.unwrap_or_default();
        ConnectParams {
            kind: self.kind,
            file_path: if self.kind.is_file_based() && self.file_path.is_relative() { self.project_root.join(&self.file_path) } else { self.file_path.clone() },
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            user: creds.user,
            password: creds.password,
            ssl: self.ssl,
            ssl_root_cert: self.ssl_root_cert.clone(),
            connect_timeout: Duration::from_secs(10),
            application_name: "not-bad-query".into(),
        }
    }
}

/// A project's connections plus its (possibly empty) folders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Connections {
    /// Every folder path, including empty ones. Parents are implied.
    pub folders: Vec<String>,
    pub connections: Vec<Connection>,
}

impl Connections {
    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn upsert(&mut self, c: Connection) {
        if let Some(f) = &c.folder {
            if !self.folders.contains(f) {
                self.folders.push(f.clone());
            }
        }
        match self.connections.iter_mut().find(|x| x.id == c.id) {
            Some(x) => *x = c,
            None => self.connections.push(c),
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.connections.retain(|c| c.id != id);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    pub editor_font_size: f32,
    pub initial_rows: u64,
    /// Roll back abandoned transactions after this many seconds without DB activity.
    pub transaction_idle_timeout_secs: u64,
    pub csv_delimiter: char,
    pub csv_null: String,
    /// Blank lines separate statements for "run statement".
    pub blank_line_delimits: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dark: true,
            editor_font_size: 14.0,
            initial_rows: 500,
            transaction_idle_timeout_secs: 15 * 60,
            csv_delimiter: ',',
            csv_null: String::new(),
            blank_line_delimits: true,
        }
    }
}

/// Which projects are open (restored on startup) and recently used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub open: Vec<PathBuf>,
    pub recent: Vec<PathBuf>,
}

impl Workspace {
    const MAX_RECENT: usize = 10;

    pub fn opened(&mut self, root: &Path) {
        if !self.open.iter().any(|p| p == root) {
            self.open.push(root.to_path_buf());
        }
        self.recent.retain(|p| p != root);
        self.recent.insert(0, root.to_path_buf());
        self.recent.truncate(Self::MAX_RECENT);
    }

    pub fn closed(&mut self, root: &Path) {
        self.open.retain(|p| p != root);
    }
}

/// Read a TOML file; `None` if it doesn't exist.
pub(crate) fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map(Some).map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
    }
}

/// Write via a temp file + rename so a crash never leaves a torn file.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io { path: dir.into(), source })?;
    files::write_atomic(path, text, None, true).map_err(|source| ConfigError::Io { path: path.into(), source })
}

/// Where app config lives. `Store::default_location()` is the user config
/// dir; tests point it at a temp dir.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Store { dir: dir.into() }
    }

    pub fn default_location() -> Self {
        let dirs = directories::ProjectDirs::from("", "", "not-bad-query").expect("home directory");
        Store::new(dirs.config_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Default spool location: the local (non-roaming) cache dir.
    pub fn default_spool_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "not-bad-query")
            .map(|d| d.cache_dir().join("spool"))
            .unwrap_or_else(|| std::env::temp_dir().join("not-bad-query-spool"))
    }

    fn load<T: Default + for<'de> Deserialize<'de>>(&self, name: &str) -> Result<T> {
        Ok(read_toml(&self.dir.join(name))?.unwrap_or_default())
    }

    fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        write_atomic(&self.dir.join(name), &toml::to_string_pretty(value)?)
    }

    pub fn load_settings(&self) -> Result<Settings> {
        self.load("settings.toml")
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        self.save("settings.toml", s)
    }

    pub fn load_workspace(&self) -> Result<Workspace> {
        self.load("workspace.toml")
    }

    pub fn save_workspace(&self, w: &Workspace) -> Result<()> {
        self.save("workspace.toml", w)
    }
}

/// Credentials in the OS credential store (Windows Credential Manager),
/// keyed by [`Connection::secret_key`].
pub mod secrets {
    use serde::{Deserialize, Serialize};

    use super::{ConfigError, Result};

    const SERVICE: &str = "not-bad-query";

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct Credentials {
        pub user: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub password: Option<String>,
    }

    fn entry(key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, key).map_err(|e| ConfigError::Secret(e.to_string()))
    }

    pub fn get(key: &str) -> Result<Option<Credentials>> {
        match entry(key)?.get_password() {
            Ok(s) => toml::from_str(&s).map(Some).map_err(|e| ConfigError::Secret(e.to_string())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(ConfigError::Secret(e.to_string())),
        }
    }

    pub fn set(key: &str, creds: &Credentials) -> Result<()> {
        let s = toml::to_string(creds)?;
        entry(key)?.set_password(&s).map_err(|e| ConfigError::Secret(e.to_string()))
    }

    pub fn delete(key: &str) -> Result<()> {
        match entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(ConfigError::Secret(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_workspace_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path());
        assert_eq!(store.load_settings().unwrap(), Settings::default());
        assert_eq!(store.load_workspace().unwrap(), Workspace::default());

        let mut w = Workspace::default();
        w.opened(Path::new("C:/a"));
        w.opened(Path::new("C:/b"));
        w.opened(Path::new("C:/a"));
        w.closed(Path::new("C:/b"));
        store.save_workspace(&w).unwrap();
        let w2 = store.load_workspace().unwrap();
        assert_eq!(w2.open, [PathBuf::from("C:/a")]);
        assert_eq!(w2.recent, [PathBuf::from("C:/a"), PathBuf::from("C:/b")]);
    }

    #[test]
    fn hand_written_connection_with_missing_fields() {
        let c: Connection =
            toml::from_str("kind = \"postgres\"\nhost = \"localhost\"\nport = 5432\ndatabase = \"postgres\"\n").unwrap();
        assert!(c.autocommit && c.id.is_empty());
        assert_eq!(c.ssl, SslMode::Prefer);
    }

    #[test]
    fn parse_error_names_the_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("settings.toml"), "dark = 'nope'").unwrap();
        let e = Store::new(d.path()).load_settings().unwrap_err().to_string();
        assert!(e.contains("settings.toml"), "{e}");
    }
}

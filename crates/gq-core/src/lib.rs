//! Configuration persisted as hand-editable TOML in the user's config dir:
//!
//! * `connections.toml`: connections and the folders that organize them.
//!   Folders are `/`-separated paths, e.g. `"Prod/US"`.
//! * `settings.toml`: app settings (scripts folder, fetch sizes, ...).
//!
//! Passwords never touch these files; they live in the OS credential store.

mod color;
mod tree;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gq_driver::{ConnectParams, DbKind, SslMode};
use serde::{Deserialize, Serialize};

pub use color::{ConnColor, PALETTE};
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
}

pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl: SslMode,
    /// Palette name (`"red"`) or `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Default commit mode for tabs using this connection.
    #[serde(default = "yes")]
    pub autocommit: bool,
    /// Store the password in the OS credential store.
    #[serde(default = "yes")]
    pub save_password: bool,
}

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
            host: "localhost".into(),
            port: match kind {
                DbKind::Postgres => 5432,
                DbKind::Redshift => 5439,
            },
            database: match kind {
                DbKind::Postgres => "postgres".into(),
                DbKind::Redshift => "dev".into(),
            },
            user: String::new(),
            ssl: SslMode::Prefer,
            color: None,
            autocommit: true,
            save_password: true,
        }
    }

    pub fn color(&self) -> Option<ConnColor> {
        self.color.as_deref().and_then(ConnColor::parse)
    }

    pub fn params(&self, password: Option<String>) -> ConnectParams {
        ConnectParams {
            kind: self.kind,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            user: self.user.clone(),
            password,
            ssl: self.ssl,
            connect_timeout: Duration::from_secs(10),
            application_name: "good-query".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Connections {
    /// Every folder path, including empty ones. Parents are implied.
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default, rename = "connection")]
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
    /// Root of the SQL script tree.
    pub scripts_dir: Option<PathBuf>,
    pub dark: bool,
    pub editor_font_size: f32,
    pub initial_rows: u64,
    pub csv_delimiter: char,
    pub csv_null: String,
    /// Blank lines separate statements for "run statement".
    pub blank_line_delimits: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            scripts_dir: None,
            dark: true,
            editor_font_size: 14.0,
            initial_rows: 500,
            csv_delimiter: ',',
            csv_null: String::new(),
            blank_line_delimits: true,
        }
    }
}

/// Where config files live. `Store::default_location()` is the user config
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
        let dirs = directories::ProjectDirs::from("", "", "good-query").expect("home directory");
        Store::new(dirs.config_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Default spool location: the local (non-roaming) cache dir.
    pub fn default_spool_dir() -> PathBuf {
        directories::ProjectDirs::from("", "", "good-query")
            .map(|d| d.cache_dir().join("spool"))
            .unwrap_or_else(|| std::env::temp_dir().join("good-query-spool"))
    }

    fn load<T: Default + for<'de> Deserialize<'de>>(&self, name: &str) -> Result<T> {
        let path = self.dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|source| ConfigError::Parse { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(source) => Err(ConfigError::Io { path, source }),
        }
    }

    /// Write via a temp file + rename so a crash never leaves a torn file.
    fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| ConfigError::Io { path, source }
        };
        std::fs::create_dir_all(&self.dir).map_err(io(&self.dir))?;
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        std::fs::write(&tmp, toml::to_string_pretty(value)?).map_err(io(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io(&path))
    }

    pub fn load_connections(&self) -> Result<Connections> {
        self.load("connections.toml")
    }

    pub fn save_connections(&self, c: &Connections) -> Result<()> {
        self.save("connections.toml", c)
    }

    pub fn load_settings(&self) -> Result<Settings> {
        self.load("settings.toml")
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        self.save("settings.toml", s)
    }
}

/// Passwords in the OS credential store (Windows Credential Manager).
pub mod secrets {
    use super::{ConfigError, Result};

    const SERVICE: &str = "good-query";

    fn entry(conn_id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, conn_id).map_err(|e| ConfigError::Secret(e.to_string()))
    }

    pub fn get(conn_id: &str) -> Result<Option<String>> {
        match entry(conn_id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(ConfigError::Secret(e.to_string())),
        }
    }

    pub fn set(conn_id: &str, password: &str) -> Result<()> {
        entry(conn_id)?.set_password(password).map_err(|e| ConfigError::Secret(e.to_string()))
    }

    pub fn delete(conn_id: &str) -> Result<()> {
        match entry(conn_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(ConfigError::Secret(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path());
        assert_eq!(store.load_connections().unwrap(), Connections::default());
        assert_eq!(store.load_settings().unwrap(), Settings::default());

        let mut cs = Connections::default();
        let mut c = Connection::new("orders prod", DbKind::Postgres);
        c.folder = Some("Prod/US".into());
        c.color = Some("red".into());
        c.autocommit = false;
        cs.upsert(c.clone());
        cs.folders.push("Empty".into());
        store.save_connections(&cs).unwrap();
        assert_eq!(store.load_connections().unwrap(), cs);
        assert_eq!(cs.folders, ["Prod/US", "Empty"]);

        let text = std::fs::read_to_string(d.path().join("connections.toml")).unwrap();
        assert!(text.contains("[[connection]]"), "{text}");
        assert!(!text.lines().any(|l| l.trim_start().starts_with("password")), "{text}");
    }

    #[test]
    fn hand_written_file_with_missing_fields() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("connections.toml"),
            r#"
[[connection]]
id = "a"
name = "local"
kind = "postgres"
host = "localhost"
port = 5432
database = "postgres"
user = "me"
"#,
        )
        .unwrap();
        let cs = Store::new(d.path()).load_connections().unwrap();
        let c = &cs.connections[0];
        assert!(c.autocommit && c.save_password);
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

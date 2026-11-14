//! Small shared UI helpers, and the open projects shared by all views.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
use gq_core::{Connection, Credentials, Project};
use gq_driver::ConnectParams;

/// Open projects, shared by the workbench (owner), sidebar and tabs.
pub type Projects = Rc<RefCell<Vec<Project>>>;

pub fn project<'a>(ps: &'a [Project], key: u32) -> Option<&'a Project> {
    ps.iter().find(|p| p.key == key)
}

/// A connection by its runtime key (`Connection::key`).
pub fn connection(ps: &[Project], key: &str) -> Option<Connection> {
    let (pk, id) = key.split_once(':')?;
    project(ps, pk.parse().ok()?)?.connections.get(id).cloned()
}

/// The project a file belongs to (the innermost, if projects nest).
pub fn project_for_path(ps: &[Project], path: &Path) -> Option<u32> {
    ps.iter().filter(|p| p.contains(path)).max_by_key(|p| p.root.as_os_str().len()).map(|p| p.key)
}

/// The connection's accent color (falls back to a neutral).
pub fn conn_color(c: &Connection, cx: &App) -> Hsla {
    match c.color() {
        Some(col) => rgb(col.rgb).into(),
        None => cx.theme().muted_foreground,
    }
}

pub fn rgb_hsla(v: u32) -> Hsla {
    rgb(v).into()
}

/// Stored credentials for a connection.
pub fn credentials(c: &Connection) -> Option<Credentials> {
    let stored = gq_core::secrets::get(&c.secret_key()).ok().flatten();
    // Debug builds: dev credentials from the environment (with GQ_CONFIG_DIR,
    // for local testing without touching the OS credential store).
    if cfg!(debug_assertions) && stored.is_none() {
        if let Ok(password) = std::env::var("GQ_DEV_PASSWORD") {
            let user = std::env::var("GQ_DEV_USER").unwrap_or_else(|_| "postgres".into());
            return Some(Credentials { user, password: Some(password) });
        }
    }
    stored
}

/// Connect params with the credentials from the OS credential store.
pub fn params_for(c: &Connection) -> ConnectParams {
    c.params(credentials(c))
}

/// `1234567` → `1,234,567`.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `0.042s`, `3.2s`, `1:05.3`.
pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{}ms", d.as_millis())
    } else if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}:{:04.1}", (s / 60.0) as u64, s % 60.0)
    }
}

/// Default export file name: `<title>_2026-11-12_1530.csv`.
pub fn export_file_name(title: &str, now: chrono::NaiveDateTime) -> String {
    let base = title.trim_end_matches(".sql");
    let base: String =
        base.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    format!("{base}_{}.csv", now.format("%Y-%m-%d_%H%M"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn counts_and_durations() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_elapsed(Duration::from_millis(42)), "42ms");
        assert_eq!(fmt_elapsed(Duration::from_millis(3240)), "3.2s");
        assert_eq!(fmt_elapsed(Duration::from_millis(65_300)), "1:05.3");
    }

    #[::core::prelude::v1::test]
    fn export_names() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 12).unwrap().and_hms_opt(15, 30, 9).unwrap();
        assert_eq!(export_file_name("daily.sql", t), "daily_2026-11-12_1530.csv");
        assert_eq!(export_file_name("untitled-2", t), "untitled-2_2026-11-12_1530.csv");
        assert_eq!(export_file_name("a b/c", t), "a_b_c_2026-11-12_1530.csv");
    }

    #[::core::prelude::v1::test]
    fn finds_projects_and_connections() {
        let d = tempfile::tempdir().unwrap();
        let outer = d.path().join("repo");
        let inner = outer.join("queries");
        std::fs::create_dir_all(&inner).unwrap();
        let mut a = Project::create(&outer, "outer").unwrap();
        let b = Project::create(&inner, "inner").unwrap();
        let c = a.save_connection(Connection::new("db", gq_driver::DbKind::Postgres)).unwrap();
        let ps = vec![a, b];
        assert_eq!(project_for_path(&ps, &inner.join("scripts/x.sql")), Some(ps[1].key));
        assert_eq!(project_for_path(&ps, &outer.join("scripts/x.sql")), Some(ps[0].key));
        assert_eq!(project_for_path(&ps, d.path()), None);
        assert_eq!(connection(&ps, &c.key()).map(|c| c.name), Some("db".into()));
        assert_eq!(connection(&ps, "999:nope"), None);
    }
}

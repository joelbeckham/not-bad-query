//! Watch open project folders for outside changes (a `git pull`, an editor)
//! so the sidebar can refresh. The UI polls [`ProjectWatcher::take_changed`];
//! the OS callback only records which paths changed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::{ConfigError, Result};

pub struct ProjectWatcher {
    inner: RecommendedWatcher,
    changed: Arc<Mutex<HashSet<PathBuf>>>,
}

impl ProjectWatcher {
    pub fn new() -> Result<Self> {
        let changed: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        let sink = changed.clone();
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            if let Ok(mut set) = sink.lock() {
                set.extend(ev.paths);
            }
        })
        .map_err(|e| ConfigError::Watch(e.to_string()))?;
        Ok(ProjectWatcher { inner, changed })
    }

    pub fn watch(&mut self, root: &Path) -> Result<()> {
        self.inner.watch(root, RecursiveMode::Recursive).map_err(|e| ConfigError::Watch(e.to_string()))
    }

    pub fn unwatch(&mut self, root: &Path) {
        let _ = self.inner.unwatch(root);
    }

    /// Paths changed since the last call.
    pub fn take_changed(&self) -> Vec<PathBuf> {
        self.changed.lock().map(|mut s| s.drain().collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reports_changes_under_watched_root() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let mut w = ProjectWatcher::new().unwrap();
        w.watch(&root).unwrap();
        std::fs::create_dir(root.join("connections")).unwrap();
        std::fs::write(root.join("connections").join("x.toml"), "a").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            seen.extend(w.take_changed());
            if seen.iter().any(|p| p.ends_with("x.toml")) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no event for x.toml; saw {seen:?}");
    }
}

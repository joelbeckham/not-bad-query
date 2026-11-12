//! Build the sidebar's folder tree from flat folder paths.

use crate::{Connection, Connections};

#[derive(Debug, Clone, PartialEq)]
pub struct FolderNode {
    pub name: String,
    /// Full `/`-separated path; empty for the root.
    pub path: String,
    pub folders: Vec<FolderNode>,
    pub connections: Vec<Connection>,
}

impl FolderNode {
    fn new(name: &str, path: String) -> Self {
        FolderNode { name: name.to_string(), path, folders: Vec::new(), connections: Vec::new() }
    }

    fn child(&mut self, name: &str) -> &mut FolderNode {
        let i = match self.folders.iter().position(|f| f.name == name) {
            Some(i) => i,
            None => {
                let path = if self.path.is_empty() { name.to_string() } else { format!("{}/{name}", self.path) };
                self.folders.push(FolderNode::new(name, path));
                self.folders.len() - 1
            }
        };
        &mut self.folders[i]
    }

    fn at(&mut self, path: &str) -> &mut FolderNode {
        path.split('/').map(str::trim).filter(|s| !s.is_empty()).fold(self, |node, part| node.child(part))
    }

    fn sort(&mut self) {
        let key = |s: &str| s.to_lowercase();
        self.folders.sort_by_key(|f| key(&f.name));
        self.connections.sort_by_key(|c| key(&c.name));
        self.folders.iter_mut().for_each(FolderNode::sort);
    }
}

/// Root node: folders first (alphabetical), then unfiled connections.
pub fn build_tree(c: &Connections) -> FolderNode {
    let mut root = FolderNode::new("", String::new());
    for f in &c.folders {
        root.at(f);
    }
    for conn in &c.connections {
        let node = match conn.folder.as_deref() {
            Some(f) => root.at(f),
            None => &mut root,
        };
        node.connections.push(conn.clone());
    }
    root.sort();
    root
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
    fn tree() {
        let cs = Connections {
            folders: vec!["Prod".into(), "Dev/Local".into(), "Empty".into()],
            connections: vec![
                conn("zeta", None),
                conn("orders", Some("Prod/US")),
                conn("alpha", None),
                conn("pg", Some("Dev/Local")),
            ],
        };
        let t = build_tree(&cs);
        let names: Vec<_> = t.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Dev", "Empty", "Prod"]);
        let unfiled: Vec<_> = t.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(unfiled, ["alpha", "zeta"]);
        let us = &t.folders[2].folders[0];
        assert_eq!((us.name.as_str(), us.path.as_str()), ("US", "Prod/US"));
        assert_eq!(us.connections[0].name, "orders");
        assert_eq!(t.folders[0].folders[0].connections[0].name, "pg");
    }
}

//! Left sidebar: one tree of open projects. Each project has its
//! connections (with lazily-loaded database objects) and its scripts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_core::{FolderNode, Project, build_tree};
use nbq_engine::{ConnState, Engine};
use nbq_engine::meta::{ObjItem, ObjKind, ObjPath};

use crate::ui::{Projects, connection, params_for, rgb_hsla};

pub enum SidebarEvent {
    /// Open a tab on this connection (runtime key).
    OpenConnection(String),
    Connect(String),
    Disconnect(String),
    OpenScript(PathBuf),
    ReloadCsv(PathBuf),
    ShowCsvTable(PathBuf),
    ImportCsvFile(PathBuf),
    ImportFiles { paths: Vec<PathBuf>, connection: String },
    NewConnection { project: u32, folder: Option<String> },
    EditConnection(String),
    DeleteConnection(String),
    /// Open a tab with `select * from schema.name`.
    SelectFrom { conn: String, schema: String, name: String },
    /// An untitled tab that saves into `dir`.
    NewScript { project: u32, dir: PathBuf },
    NewFolder(PathBuf),
    Move { from: PathBuf, to: PathBuf },
    NewProject,
    OpenProject(Option<PathBuf>),
    CloseProject(u32),
}

#[derive(Clone)]
enum Node {
    Project(u32),
    Section { project: u32, scripts: bool },
    Folder { project: u32, path: String },
    Conn { key: String, color: Option<u32>, state: ConnState },
    Obj { conn: String, path: Option<ObjPath>, item: ObjItem },
    Note { error: bool },
    ScriptDir { project: u32, dir: PathBuf },
    Script(PathBuf),
}

#[derive(Clone)]
struct TreeDrag { path: PathBuf, root: PathBuf, label: SharedString }
impl Render for TreeDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().ml(px(20.)).mt(px(24.)).px_3().py_1().bg(cx.theme().popover).text_color(cx.theme().foreground).rounded_md().child(self.label.clone())
    }
}

/// (connection key, object path)
type Key = (String, ObjPath);

/// Surface failures even when another session for the same profile is healthy.
fn combined_state<'a>(states: impl Iterator<Item = &'a ConnState>) -> ConnState {
    let mut result = ConnState::Disconnected;
    for state in states {
        let rank = |s: &ConnState| match s {
            ConnState::Disconnected => 0,
            ConnState::Connected => 1,
            ConnState::Failed(_) => 2,
            ConnState::Connecting => 3,
        };
        if rank(state) > rank(&result) { result = state.clone(); }
    }
    result
}

fn status_text(state: &ConnState) -> &'static str {
    match state {
        ConnState::Disconnected => "Disconnected",
        ConnState::Connecting => "Connecting…",
        ConnState::Connected => "Connected",
        ConnState::Failed(_) => "Connection error",
    }
}

/// A scanned scripts folder.
struct ScriptEntry {
    name: String,
    path: PathBuf,
    children: Option<Vec<ScriptEntry>>,
}

pub struct Sidebar {
    engine: Arc<Engine>,
    projects: Projects,
    recent: Vec<PathBuf>,
    tree: Entity<TreeState>,
    /// Scanned scripts per project (rescanned on `refresh`).
    scripts: HashMap<u32, Vec<ScriptEntry>>,
    /// Display info per tree item id, rebuilt with the items.
    nodes: Rc<HashMap<SharedString, Node>>,
    loaded: HashMap<Key, Result<Vec<ObjItem>, String>>,
    loading: HashSet<Key>,
    metadata_states: HashMap<String, ConnState>,
    tab_states: Vec<(String, ConnState)>,
    disconnected: HashSet<String>,
    expanded: HashSet<SharedString>,
    /// Projects already shown once (new ones start expanded).
    seen: HashSet<u32>,
    reveal_table: Option<(String, String)>,
    csv_changes: HashMap<u32, HashSet<PathBuf>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

fn obj_id(conn: &str, path: &ObjPath) -> SharedString {
    format!("o:{conn}:{path:?}").into()
}

fn project_id(p: u32) -> SharedString {
    format!("p:{p}").into()
}

fn section_id(p: u32, scripts: bool) -> SharedString {
    format!("{}:{p}", if scripts { "ps" } else { "pc" }).into()
}

/// Open a folder in the OS file manager.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

impl Sidebar {
    pub fn new(engine: Arc<Engine>, projects: Projects, recent: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let subs = vec![cx.subscribe(&tree, |this, _, ev: &TreeEvent, cx| this.on_tree(ev, cx))];
        let mut s = Sidebar {
            engine,
            projects,
            recent,
            tree,
            scripts: HashMap::new(),
            nodes: Rc::new(HashMap::new()),
            loaded: HashMap::new(),
            loading: HashSet::new(),
            metadata_states: HashMap::new(),
            tab_states: Vec::new(),
            disconnected: HashSet::new(),
            expanded: HashSet::new(),
            seen: HashSet::new(),
            reveal_table: None,
            csv_changes: Default::default(),
            _subs: subs,
        };
        s.refresh(cx);
        s
    }

    /// Projects or their files changed: rescan scripts and rebuild.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let ps = self.projects.borrow();
        self.scripts = ps.iter().map(|p| (p.key, scan_scripts(&p.scripts_dir(), 0))).collect();
        // Forget cached objects of connections that are gone.
        let keys: HashSet<String> =
            ps.iter().flat_map(|p| p.connections.connections.iter().map(|c| c.key())).collect();
        drop(ps);
        self.loaded.retain(|(conn, _), _| keys.contains(conn));
        self.metadata_states.retain(|key, _| keys.contains(key));
        self.disconnected.retain(|key| keys.contains(key));
        self.rebuild(cx);
    }

    pub fn set_recent(&mut self, recent: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.recent = recent;
        cx.notify();
    }

    /// Drop cached objects so they reload (after a connection was edited).
    pub fn forget_connection(&mut self, key: &str, cx: &mut Context<Self>) {
        self.loaded.retain(|(c, _), _| c != key);
        self.loading.retain(|(c, _)| c != key);
        self.rebuild(cx);
        let expanded: Vec<SharedString> = self.expanded.iter().cloned().collect();
        for id in expanded {
            self.ensure_loaded(&id);
        }
    }

    pub fn refresh_connection(&mut self, key: &str, cx: &mut Context<Self>) {
        self.allow_connection(key);
        self.engine.forget_connection(key);
        self.forget_connection(key, cx);
    }

    pub fn allow_connection(&mut self, key: &str) {
        self.disconnected.remove(key);
    }

    pub fn set_csv_changes(&mut self, project: u32, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.csv_changes.insert(project, paths.into_iter().collect()); cx.notify();
    }

    pub fn show_table(&mut self, conn: &nbq_core::Connection, table: &str, cx: &mut Context<Self>) {
        self.reveal_table = Some((conn.key(), table.into()));
        self.expanded.insert(project_id(conn.project_key));
        self.expanded.insert(section_id(conn.project_key, false));
        if let Some(folder) = &conn.folder {
            let mut path = String::new();
            for part in folder.split('/') { if !path.is_empty() { path.push('/'); } path.push_str(part); self.expanded.insert(format!("f:{}:{path}", conn.project_key).into()); }
        }
        self.expand_reveal(cx);
    }

    fn expand_reveal(&mut self, cx: &mut Context<Self>) {
        let Some((conn, table)) = self.reveal_table.clone() else { return; };
        let ids = [format!("c:{conn}").into(), obj_id(&conn, &ObjPath::Schema("main".into())), obj_id(&conn, &ObjPath::Folder { schema: "main".into(), kind: ObjKind::Table })];
        for id in &ids { self.expanded.insert(id.clone()); }
        self.rebuild(cx);
        for id in &ids { self.ensure_loaded(id); }
        let target = obj_id(&conn, &ObjPath::Object { schema: "main".into(), name: table, kind: ObjKind::Table });
        let found = self.tree.update(cx, |t, cx| { if let Some(item) = find(t, &target) { t.set_selected_item(Some(&item), cx); true } else { false } });
        if found { self.reveal_table = None; }
    }

    pub fn is_disconnected(&self, key: &str) -> bool {
        self.disconnected.contains(key)
    }

    pub fn disconnect(&mut self, key: &str, cx: &mut Context<Self>) {
        self.disconnected.insert(key.into());
        self.metadata_states.remove(key);
        self.loaded.retain(|(c, _), _| c != key);
        self.loading.retain(|(c, _)| c != key);
        self.expanded.retain(|id| id.as_ref() != format!("c:{key}") && !id.starts_with(&format!("o:{key}:")));
        self.rebuild(cx);
    }

    pub fn on_metadata_state(&mut self, key: String, state: ConnState, cx: &mut Context<Self>) {
        if self.is_disconnected(&key) { return; }
        self.metadata_states.insert(key, state);
        self.rebuild(cx);
    }

    pub fn set_tab_states(&mut self, states: Vec<(String, ConnState)>, cx: &mut Context<Self>) {
        if self.tab_states != states {
            self.tab_states = states;
            self.rebuild(cx);
        }
    }

    /// Shared profile status for the tree and workbench status bar.
    pub fn connection_state(&self, key: &str) -> ConnState {
        if self.is_disconnected(key) { return ConnState::Disconnected; }
        combined_state(self.metadata_states.get(key).into_iter().chain(
            self.tab_states.iter().filter(|(k, _)| k == key).map(|(_, state)| state)
        ))
    }

    pub fn on_objects(&mut self, conn: String, path: ObjPath, items: Result<Vec<ObjItem>, String>, cx: &mut Context<Self>) {
        let key = (conn, path);
        if !self.loading.remove(&key) {
            return; // forgotten meanwhile
        }
        self.loaded.insert(key, items);
        self.rebuild(cx);
        self.expand_reveal(cx);
    }

    fn on_tree(&mut self, ev: &TreeEvent, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Expanded(id) => {
                self.expanded.insert(id.clone());
                self.ensure_loaded(id);
            }
            TreeEvent::Collapsed(id) => {
                self.expanded.remove(id);
            }
        }
        let _ = cx;
    }

    fn ensure_loaded(&mut self, id: &SharedString) {
        let target = match self.nodes.get(id) {
            Some(Node::Conn { key, .. }) => Some((key.clone(), ObjPath::Root)),
            Some(Node::Obj { conn, path: Some(p), .. }) => Some((conn.clone(), p.clone())),
            _ => None,
        };
        let Some(key) = target else { return };
        self.allow_connection(&key.0);
        if self.loaded.contains_key(&key) || self.loading.contains(&key) {
            return;
        }
        let Some(c) = connection(&self.projects.borrow(), &key.0) else { return };
        self.engine.load_objects(&key.0, &params_for(&c), key.1.clone());
        self.loading.insert(key);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut nodes = HashMap::new();
        let ps = self.projects.clone();
        let ps = ps.borrow();
        for p in ps.iter() {
            if self.seen.insert(p.key) {
                self.expanded.insert(project_id(p.key));
                self.expanded.insert(section_id(p.key, false));
                self.expanded.insert(section_id(p.key, true));
            }
        }
        let items: Vec<TreeItem> = ps.iter().map(|p| self.project_item(p, &mut nodes)).collect();
        drop(ps);
        fn expand(items: Vec<TreeItem>, ex: &HashSet<SharedString>) -> Vec<TreeItem> {
            items
                .into_iter()
                .map(|mut it| {
                    let children = std::mem::take(&mut it.children);
                    let open = ex.contains(&it.id);
                    it.children(expand(children, ex)).expanded(open)
                })
                .collect()
        }
        let items = expand(items, &self.expanded);
        self.nodes = Rc::new(nodes);
        self.tree.update(cx, |t, cx| {
            let selected = t.selected_item().map(|i| i.id.clone());
            t.set_items(items, cx);
            if let Some(id) = selected {
                if let Some(item) = find(t, &id) {
                    t.set_selected_item(Some(&item), cx);
                }
            }
        });
        cx.notify();
    }

    fn project_item(&self, p: &Project, nodes: &mut HashMap<SharedString, Node>) -> TreeItem {
        let id = project_id(p.key);
        nodes.insert(id.clone(), Node::Project(p.key));
        let conns_id = section_id(p.key, false);
        nodes.insert(conns_id.clone(), Node::Section { project: p.key, scripts: false });
        let scripts_id = section_id(p.key, true);
        nodes.insert(scripts_id.clone(), Node::Section { project: p.key, scripts: true });
        let root = build_tree(&p.connections);
        let scripts = self.scripts.get(&p.key).map(|s| script_items(p.key, s, nodes)).unwrap_or_default();
        TreeItem::new(id, p.name().to_string()).children(vec![
            TreeItem::new(conns_id, "Connections").children(self.folder_items(p.key, &root, nodes)),
            TreeItem::new(scripts_id, "Scripts").children(scripts),
        ])
    }

    fn folder_items(&self, project: u32, f: &FolderNode, nodes: &mut HashMap<SharedString, Node>) -> Vec<TreeItem> {
        let mut out = Vec::new();
        for sub in &f.folders {
            let id: SharedString = format!("f:{project}:{}", sub.path).into();
            nodes.insert(id.clone(), Node::Folder { project, path: sub.path.clone() });
            out.push(TreeItem::new(id, sub.name.clone()).children(self.folder_items(project, sub, nodes)));
        }
        for c in &f.connections {
            let key = c.key();
            let id: SharedString = format!("c:{key}").into();
            nodes.insert(id.clone(), Node::Conn {
                key: key.clone(), color: c.color().map(|x| x.rgb), state: self.connection_state(&key),
            });
            out.push(TreeItem::new(id, c.name.clone()).children(self.obj_children(&key, &ObjPath::Root, nodes)));
        }
        out
    }

    fn obj_children(&self, conn: &str, path: &ObjPath, nodes: &mut HashMap<SharedString, Node>) -> Vec<TreeItem> {
        let parent = obj_id(conn, path);
        match self.loaded.get(&(conn.to_string(), path.clone())) {
            None => {
                let id: SharedString = format!("{parent}/loading").into();
                nodes.insert(id.clone(), Node::Note { error: false });
                vec![TreeItem::new(id, "Loading…")]
            }
            Some(Err(e)) => {
                let id: SharedString = format!("{parent}/error").into();
                nodes.insert(id.clone(), Node::Note { error: true });
                vec![TreeItem::new(id, e.clone())]
            }
            Some(Ok(items)) => items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let (id, children) = match (&it.path, it.has_children) {
                        (Some(p), true) => (obj_id(conn, p), self.obj_children(conn, p, nodes)),
                        _ => (SharedString::from(format!("{parent}/{i}:{}", it.name)), Vec::new()),
                    };
                    nodes.insert(id.clone(), Node::Obj { conn: conn.to_string(), path: it.path.clone(), item: it.clone() });
                    TreeItem::new(id, it.name.clone()).children(children)
                })
                .collect(),
        }
    }

    fn activate(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        match self.nodes.get(id).cloned() {
            Some(Node::Conn { key, .. }) => cx.emit(SidebarEvent::OpenConnection(key)),
            Some(Node::Script(p)) => cx.emit(SidebarEvent::OpenScript(p)),
            Some(Node::Obj { conn, path: Some(ObjPath::Object { schema, name, .. }), .. }) => {
                cx.emit(SidebarEvent::SelectFrom { conn, schema, name })
            }
            _ => {}
        }
    }

    fn render_item(
        nodes: &HashMap<SharedString, Node>,
        entry_id: &SharedString,
        label: SharedString,
        folder: Option<bool>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let node = nodes.get(entry_id);
        let (icon, detail, color, strong): (Icon, Option<String>, Option<Hsla>, bool) = match node {
            Some(Node::Project(_)) => (Icon::new(Lucide::Briefcase), None, Some(theme.primary), true),
            Some(Node::Section { scripts: false, .. }) => (Icon::new(Lucide::Cable), None, Some(muted), false),
            Some(Node::Section { scripts: true, .. }) => (Icon::new(Lucide::FileCode), None, Some(muted), false),
            Some(Node::Folder { .. }) | Some(Node::ScriptDir { .. }) => {
                (Icon::new(if folder == Some(true) { IconName::FolderOpen } else { IconName::Folder }), None, Some(muted), false)
            }
            Some(Node::Conn { color, .. }) => {
                (Icon::new(Lucide::Database), None, Some(color.map(rgb_hsla).unwrap_or(muted)), true)
            }
            Some(Node::Obj { item, .. }) => {
                let (icon, c) = match item.kind {
                    ObjKind::Schema => (Icon::new(Lucide::Layers), theme.muted_foreground),
                    ObjKind::Table => (Icon::new(Lucide::Table2), theme.blue),
                    ObjKind::View => (Icon::new(Lucide::Eye), theme.cyan),
                    ObjKind::MatView => (Icon::new(Lucide::Layers2), theme.cyan),
                    ObjKind::Foreign => (Icon::new(Lucide::ExternalLink), theme.muted_foreground),
                    ObjKind::Function => (Icon::new(Lucide::SquareFunction), theme.magenta),
                    ObjKind::Sequence => (Icon::new(Lucide::ListOrdered), theme.muted_foreground),
                    ObjKind::Column if item.key => (Icon::new(Lucide::KeyRound), theme.yellow),
                    ObjKind::Column => (Icon::new(Lucide::Columns2), theme.muted_foreground),
                    ObjKind::Index => (Icon::new(Lucide::ListTree), theme.muted_foreground),
                    ObjKind::Trigger => (Icon::new(Lucide::Zap), theme.yellow),
                };
                // Category folders (Tables, Views…) inside a schema stay quiet.
                let (icon, c) = if matches!(item.path, Some(ObjPath::Folder { .. })) {
                    (Icon::new(IconName::Folder), theme.muted_foreground)
                } else {
                    (icon, c)
                };
                let detail = match item.kind {
                    ObjKind::Index | ObjKind::Trigger => None,
                    _ => item.detail.clone(),
                };
                (icon, detail, Some(c), item.key)
            }
            Some(Node::Note { error: true }) => (Icon::new(IconName::TriangleAlert), None, Some(theme.danger), false),
            Some(Node::Note { error: false }) => (Icon::new(IconName::LoaderCircle), None, Some(muted), false),
            Some(Node::Script(_)) => (Icon::new(Lucide::FileCode), None, Some(theme.green), false),
            None => (Icon::new(IconName::File), None, None, false),
        };
        let section = matches!(node, Some(Node::Section { .. }));
        // Keep indentation and icons rigid when the panel narrows. Only the
        // text may yield; shrinking these slots offsets every row differently.
        let chevron = div().w(px(12.)).h(px(12.)).flex_shrink_0().when_some(folder, |el, open| {
            el.child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall().text_color(muted))
        });
        h_flex()
            .w_full()
            .min_w_0()
            .gap_1p5()
            .items_center()
            .overflow_hidden()
            .child(chevron)
            .child(if matches!(node, Some(Node::Note { error: false })) {
                Spinner::new().icon(IconName::LoaderCircle).small().color(muted)
                    .ease(|t| t).into_any_element()
            } else {
                icon.small().flex_shrink_0().when_some(color, |i, c| i.text_color(c)).into_any_element()
            })
            .when_some(match node { Some(Node::Conn { state, .. }) => Some(state), _ => None }, |el, state| {
                let (icon, color) = match state {
                    ConnState::Disconnected => (Lucide::Circle, muted),
                    ConnState::Connecting => (Lucide::LoaderCircle, theme.warning),
                    ConnState::Connected => (Lucide::CircleCheck, theme.success),
                    ConnState::Failed(_) => (Lucide::CircleAlert, theme.danger),
                };
                let tip = match state {
                    ConnState::Failed(e) => format!("Connection error: {}", e.message),
                    _ => status_text(state).to_string(),
                };
                el.child(div().id(SharedString::from(format!("status:{entry_id}")))
                    .flex_shrink_0()
                    .child(if matches!(state, ConnState::Connecting) {
                        Spinner::new().icon(IconName::LoaderCircle).xsmall().color(color)
                            .ease(|t| t).into_any_element()
                    } else {
                        Icon::new(icon).xsmall().text_color(color).into_any_element()
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(
                        div()
                            .max_w_full()
                            .flex_shrink_0()
                            .text_sm()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .when(strong, |d| d.font_weight(FontWeight::MEDIUM))
                            .when(section, |d| d.text_color(muted))
                            .child(label),
                    )
                    .when_some(detail, |el, d| {
                        el.child(div().flex_1().min_w_0().text_xs().whitespace_nowrap().text_color(muted).overflow_hidden().text_ellipsis().child(d))
                    }),
            )
            .into_any_element()
    }

    fn location(&self, node: &Node) -> Option<(PathBuf, PathBuf, bool)> {
        let ps = self.projects.borrow();
        match node {
            Node::Section { project, scripts } => {
                let p = ps.iter().find(|p| p.key == *project)?;
                let root = if *scripts { p.scripts_dir() } else { p.connections_dir() };
                Some((root.clone(), root, true))
            }
            Node::Folder { project, path } => {
                let root = ps.iter().find(|p| p.key == *project)?.connections_dir();
                Some((root.join(path), root, true))
            }
            Node::ScriptDir { project, dir } => Some((dir.clone(), ps.iter().find(|p| p.key == *project)?.scripts_dir(), true)),
            Node::Script(path) => ps.iter().find(|p| path.starts_with(p.scripts_dir())).map(|p| (path.clone(), p.scripts_dir(), false)),
            Node::Conn { key, .. } => {
                let c = connection(&ps, key)?;
                let p = ps.iter().find(|p| p.key == c.project_key)?;
                let path = if p.local.database_ids.values().any(|id| id == &c.id) { p.root.join(&c.file_path) } else { p.connection_file(&c) };
                Some((path, p.connections_dir(), false))
            }
            _ => None,
        }
    }

    fn context_menu(&self, node: Option<Node>, menu: PopupMenu, view: &Entity<Self>, window: &mut Window) -> PopupMenu {
        let emit = |ev: Rc<dyn Fn() -> SidebarEvent>| {
            window.listener_for(view, move |_, _: &ClickEvent, _, cx| cx.emit(ev()))
        };
        let projects = self.projects.borrow();
        let root_of = |p: u32| crate::ui::project(&projects, p).map(|p| p.root.clone());
        let item = |label: &'static str, icon: IconName| PopupMenuItem::new(label).icon(icon);
        let reveal_item = |path: PathBuf| {
            PopupMenuItem::new("Show in file manager").icon(IconName::FolderOpen).on_click(move |_, _, _| reveal(&path))
        };
        let folder_item = |dir: PathBuf| item("New folder...", IconName::Folder).on_click(emit(Rc::new(move || SidebarEvent::NewFolder(dir.clone()))));
        match node {
            Some(Node::Project(p)) => {
                let root = root_of(p).unwrap_or_default();
                let scripts = root.join(nbq_core::project::SCRIPTS_DIR);
                menu.item(item("New connection…", IconName::Plus).on_click(emit(Rc::new(move || {
                    SidebarEvent::NewConnection { project: p, folder: None }
                }))))
                .item(item("New script", IconName::File).on_click(emit(Rc::new(move || SidebarEvent::NewScript {
                    project: p,
                    dir: scripts.clone(),
                }))))
                .separator()
                .item(reveal_item(root))
                .item(item("Close project", IconName::Close).on_click(emit(Rc::new(move || SidebarEvent::CloseProject(p)))))
            }
            Some(Node::Section { project, scripts: false }) => {
                let root = root_of(project).unwrap_or_default().join(nbq_core::project::CONNECTIONS_DIR);
                menu.item(item("New connection…", IconName::Plus).on_click(emit(Rc::new(move || {
                    SidebarEvent::NewConnection { project, folder: None }
                }))))
                .item(folder_item(root.clone()))
                .separator()
                .item(reveal_item(root))
            }
            Some(Node::Folder { project, path }) => {
                let dir = root_of(project).unwrap_or_default().join(nbq_core::project::CONNECTIONS_DIR).join(&path);
                menu.item(item("New connection here…", IconName::Plus).on_click(emit(Rc::new(move || {
                    SidebarEvent::NewConnection { project, folder: Some(path.clone()) }
                }))))
                .item(folder_item(dir.clone()))
                .separator()
                .item(reveal_item(dir))
            }
            Some(Node::Section { project, scripts: true }) => {
                let dir = root_of(project).unwrap_or_default().join(nbq_core::project::SCRIPTS_DIR);
                let d2 = dir.clone();
                menu.item(item("New script", IconName::File).on_click(emit(Rc::new(move || SidebarEvent::NewScript {
                    project,
                    dir: d2.clone(),
                }))))
                .item(folder_item(dir.clone()))
                .separator()
                .item(reveal_item(dir))
            }
            Some(Node::ScriptDir { project, dir }) => {
                let d2 = dir.clone();
                menu.item(item("New script here", IconName::File).on_click(emit(Rc::new(move || {
                    SidebarEvent::NewScript { project, dir: d2.clone() }
                }))))
                .item(folder_item(dir.clone()))
                .separator()
                .item(reveal_item(dir))
            }
            Some(Node::Script(path)) => {
                let p2 = path.clone();
                let reload = path.clone();
                let show = path.clone();
                let import = path.clone();
                let linked = self.projects.borrow().iter().any(|p| p.local.csv_imports.iter().any(|r| p.root.join(&r.source) == path));
                let menu = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")) {
                    menu.item(item("Import CSV…", IconName::Plus).on_click(emit(Rc::new(move || SidebarEvent::ImportCsvFile(import.clone())))))
                        .item(item("Reload from CSV", IconName::RefreshCw).disabled(!linked).on_click(emit(Rc::new(move || SidebarEvent::ReloadCsv(reload.clone())))))
                        .item(item("Show imported table", IconName::ArrowRight).disabled(!linked).on_click(emit(Rc::new(move || SidebarEvent::ShowCsvTable(show.clone())))))
                } else { menu };
                menu.item(item("Open", IconName::File).on_click(emit(Rc::new(move || SidebarEvent::OpenScript(p2.clone())))))
                    .item(reveal_item(path.parent().map(Path::to_path_buf).unwrap_or_default()))
            }
            Some(Node::Conn { key, state, .. }) => {
                let (a, c, d, connect, disconnect) = (key.clone(), key.clone(), key.clone(), key.clone(), key.clone());
                let active = !matches!(state, ConnState::Disconnected | ConnState::Failed(_));
                let b = key;
                menu.item(PopupMenuItem::new(status_text(&state)).disabled(true))
                .separator()
                .item(PopupMenuItem::new("Connect").icon(Lucide::Plug).disabled(active).on_click(emit(Rc::new(move || {
                    SidebarEvent::Connect(connect.clone())
                }))))
                .item(PopupMenuItem::new("Disconnect").icon(Lucide::Unplug).disabled(matches!(state, ConnState::Disconnected)).on_click(emit(Rc::new(move || {
                    SidebarEvent::Disconnect(disconnect.clone())
                }))))
                .separator()
                .item(item("New query tab", IconName::Plus).on_click(emit(Rc::new(move || {
                    SidebarEvent::OpenConnection(a.clone())
                }))))
                .item(
                    item("Refresh", IconName::RefreshCw)
                        .on_click(window.listener_for(view, move |s, _: &ClickEvent, _, cx| s.refresh_connection(&b, cx))),
                )
                .separator()
                .item(item("Edit connection…", IconName::Settings).on_click(emit(Rc::new(move || {
                    SidebarEvent::EditConnection(c.clone())
                }))))
                .item(item("Delete connection", IconName::Delete).on_click(emit(Rc::new(move || {
                    SidebarEvent::DeleteConnection(d.clone())
                }))))
            }
            Some(Node::Obj { conn, path: Some(ObjPath::Object { schema, name, .. }), .. }) => menu.item(
                item("Select rows (new tab)", IconName::Play).on_click(emit(Rc::new(move || SidebarEvent::SelectFrom {
                    conn: conn.clone(),
                    schema: schema.clone(),
                    name: name.clone(),
                }))),
            ),
            _ => menu,
        }
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let open: HashSet<PathBuf> = self.projects.borrow().iter().map(|p| p.root.clone()).collect();
        let recent: Vec<PathBuf> = self.recent.iter().filter(|p| !open.contains(*p)).cloned().collect();
        v_flex()
            .p_3()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("A project is a folder with your connections and scripts. It can live inside a git repo.")
            .child(
                Button::new("new-project")
                    .small()
                    .primary()
                    .icon(IconName::Plus)
                    .label("New project…")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewProject))),
            )
            .child(
                Button::new("open-project")
                    .small()
                    .outline()
                    .icon(IconName::FolderOpen)
                    .label("Open project…")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenProject(None)))),
            )
            .when(!recent.is_empty(), |el| {
                el.child(div().pt_2().text_xs().font_weight(FontWeight::SEMIBOLD).child("RECENT")).children(
                    recent.into_iter().enumerate().map(|(i, p)| {
                        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let tip = p.display().to_string();
                        Button::new(("recent", i))
                            .small()
                            .ghost()
                            .icon(Icon::new(Lucide::Briefcase))
                            .label(name)
                            .tooltip(tip)
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::OpenProject(Some(p.clone())))))
                    }),
                )
            })
            .into_any_element()
    }
}

fn find(t: &TreeState, id: &SharedString) -> Option<TreeItem> {
    let mut ix = 0;
    while let Some(e) = t.entry(ix) {
        if &e.item().id == id {
            return Some(e.item().clone());
        }
        ix += 1;
    }
    None
}

/// A scripts folder, folders first. Depth-limited for safety.
fn scan_scripts(dir: &Path, depth: usize) -> Vec<ScriptEntry> {
    if depth > 12 {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            dirs.push((name, p));
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sql") || x.eq_ignore_ascii_case("csv")) {
            files.push((name, p));
        }
    }
    dirs.sort_by_key(|d| d.0.to_lowercase());
    files.sort_by_key(|f| f.0.to_lowercase());
    let mut out: Vec<ScriptEntry> = dirs
        .into_iter()
        .map(|(name, p)| ScriptEntry { children: Some(scan_scripts(&p, depth + 1)), name, path: p })
        .collect();
    out.extend(files.into_iter().map(|(name, path)| ScriptEntry { name, path, children: None }));
    out
}

fn script_items(project: u32, entries: &[ScriptEntry], nodes: &mut HashMap<SharedString, Node>) -> Vec<TreeItem> {
    entries
        .iter()
        .map(|e| match &e.children {
            Some(children) => {
                let id: SharedString = format!("sd:{}", e.path.display()).into();
                nodes.insert(id.clone(), Node::ScriptDir { project, dir: e.path.clone() });
                TreeItem::new(id, e.name.clone()).children(script_items(project, children, nodes))
            }
            None => {
                let id: SharedString = format!("sf:{}", e.path.display()).into();
                nodes.insert(id.clone(), Node::Script(e.path.clone()));
                TreeItem::new(id, e.name.clone())
            }
        })
        .collect()
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let nodes = self.nodes.clone();
        let empty = self.projects.borrow().is_empty();
        let header = h_flex()
            .h(px(30.))
            .px_3()
            .items_center()
            .justify_between()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child("PROJECTS")
            .child(
                h_flex()
                    .child(
                        Button::new("hdr-new-project")
                            .xsmall()
                            .ghost()
                            .icon(Icon::new(Lucide::FolderPlus))
                            .tooltip("New project…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewProject))),
                    )
                    .child(
                        Button::new("hdr-open-project")
                            .xsmall()
                            .ghost()
                            .icon(IconName::FolderOpen)
                            .tooltip("Open project…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenProject(None)))),
                    ),
            );
        let body = if empty {
            self.render_empty(cx)
        } else {
            let (v, n) = (view.clone(), nodes.clone());
            let linked: HashMap<PathBuf, String> = self.projects.borrow().iter().flat_map(|p| p.local.csv_imports.iter().map(|r| (p.root.join(&r.source), format!("{} · {}", r.database.display(), r.table)))).collect();
            let changed: HashSet<PathBuf> = self.csv_changes.values().flatten().cloned().collect();
            tree(&self.tree, move |ix, entry, selected, _, cx| {
                let id = entry.item().id.clone();
                let el = Sidebar::render_item(&n, &id, entry.item().label.clone(), entry.is_folder().then(|| entry.is_expanded()), cx);
                let badge = match n.get(&id) {
                    Some(Node::Script(path)) => linked.get(path).map(|tip| {
                        let path = path.clone(); let is_changed = changed.contains(&path); let tip = if is_changed { format!("{tip} — Source changed since import") } else { tip.clone() }; let view = v.clone();
                        Button::new(("csv-link", ix)).ghost().xsmall().icon(if is_changed { Lucide::RefreshCw } else { Lucide::Link }).tooltip(tip)
                            .on_click(move |_, _, cx| { cx.stop_propagation(); view.update(cx, |_, cx| cx.emit(SidebarEvent::ShowCsvTable(path.clone()))); })
                    }),
                    _ => None,
                };
                let view = v.clone();
                let drop_view = v.clone();
                let drop_conn = match n.get(&id) { Some(Node::Conn { key, .. }) => Some(key.clone()), _ => None };
                let location = n.get(&id).and_then(|node| v.read(cx).location(node));
                let source = location.as_ref().filter(|(path, root, _)| path != root).map(|(path, root, _)| TreeDrag { path: path.clone(), root: root.clone(), label: entry.item().label.clone() });
                let target = location.filter(|(_, _, folder)| *folder);
                let move_view = v.clone();
                let content = div().id(("tree-row", ix)).w_full()
                    .when_some(source, |row, drag| row.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone())))
                    .when_some(target, |row, (dir, root, _)| {
                        let highlight_dir = dir.clone();
                        row.drag_over::<TreeDrag>(move |style, drag, _, cx| {
                            if drag.root == root && !highlight_dir.starts_with(&drag.path) { style.bg(cx.theme().primary.opacity(0.14)) } else { style }
                        }).on_drop(move |drag: &TreeDrag, _, cx| {
                            cx.stop_propagation();
                            if dir.starts_with(&drag.root) && !dir.starts_with(&drag.path) {
                                if let Some(name) = drag.path.file_name() {
                                    let to = dir.join(name);
                                    if to != drag.path { move_view.update(cx, |_, cx| cx.emit(SidebarEvent::Move { from: drag.path.clone(), to })); }
                                }
                            }
                        })
                    });
                ListItem::new(ix)
                    .selected(selected)
                    .py_0p5()
                    .pl(px(14.) * entry.depth() + px(8.))
                    .child(content.child(h_flex().w_full().child(div().flex_1().min_w_0().child(el)).when_some(badge, |row, badge| row.child(badge))))
                    .when_some(drop_conn, |row, key| row.on_drop(move |paths: &ExternalPaths, _, cx| {
                        cx.stop_propagation();
                        drop_view.update(cx, |_, cx| cx.emit(SidebarEvent::ImportFiles { paths: paths.paths().to_vec(), connection: key.clone() }));
                    }))
                    .on_click(move |ev: &ClickEvent, _, cx| {
                        if ev.click_count() == 2 {
                            view.update(cx, |s, cx| s.activate(&id, cx));
                        }
                    })
            })
            .context_menu({
                let (view, nodes) = (view.clone(), nodes.clone());
                move |_ix, entry, menu, window, cx| {
                    let node = nodes.get(&entry.item().id).cloned();
                    view.read(cx).context_menu(node, menu, &view, window)
                }
            })
            .into_any_element()
        };
        v_flex().size_full().child(header).child(div().flex_1().overflow_hidden().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::combined_state;
    use nbq_engine::ConnState;

    #[test]
    fn profile_state_includes_all_sessions_and_preserves_failures() {
        let failed = ConnState::Failed(nbq_driver::DriverError::connection("connection lost"));
        assert_eq!(combined_state([].iter()), ConnState::Disconnected);
        assert_eq!(combined_state([ConnState::Disconnected, ConnState::Connected].iter()), ConnState::Connected);
        assert_eq!(combined_state([ConnState::Connected, failed.clone()].iter()), failed);
        assert_eq!(combined_state([failed, ConnState::Connecting].iter()), ConnState::Connecting);
    }
}

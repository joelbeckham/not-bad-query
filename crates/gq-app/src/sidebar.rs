//! Left sidebar: connections (with lazily-loaded database objects) and the
//! SQL scripts folder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gq_core::{Connections, FolderNode, build_tree};
use gq_engine::Engine;
use gq_engine::meta::{ObjItem, ObjKind, ObjPath};

use crate::ui::{params_for, rgb_hsla};

pub enum SidebarEvent {
    /// Open a new tab on this connection.
    OpenConnection(String),
    OpenScript(PathBuf),
    NewConnection,
    EditConnection(String),
    DeleteConnection(String),
    /// Open a tab with `select * from schema.name`.
    SelectFrom { conn: String, schema: String, name: String },
    ChooseScriptsDir,
}

#[derive(Clone)]
enum Node {
    Folder,
    Conn { id: String, color: Option<u32> },
    Obj { conn: String, path: Option<ObjPath>, item: ObjItem },
    Note { error: bool },
    ScriptDir,
    Script(PathBuf),
}

type Key = (String, ObjPath);

pub struct Sidebar {
    engine: Arc<Engine>,
    connections: Connections,
    conn_tree: Entity<TreeState>,
    script_tree: Entity<TreeState>,
    scripts_dir: Option<PathBuf>,
    /// Display info per tree item id, rebuilt with the items.
    nodes: Rc<HashMap<SharedString, Node>>,
    loaded: HashMap<Key, Result<Vec<ObjItem>, String>>,
    loading: HashSet<Key>,
    expanded: HashSet<SharedString>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

fn obj_id(conn: &str, path: &ObjPath) -> SharedString {
    format!("o:{conn}:{path:?}").into()
}

impl Sidebar {
    pub fn new(
        engine: Arc<Engine>,
        connections: Connections,
        scripts_dir: Option<PathBuf>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let conn_tree = cx.new(|cx| TreeState::new(cx));
        let script_tree = cx.new(|cx| TreeState::new(cx));
        let subs = vec![
            cx.subscribe(&conn_tree, |this, _, ev: &TreeEvent, cx| this.on_conn_tree(ev, cx)),
            cx.subscribe(&script_tree, |this, _, ev: &TreeEvent, cx| match ev {
                TreeEvent::Expanded(id) => {
                    this.expanded.insert(id.clone());
                }
                TreeEvent::Collapsed(id) => {
                    this.expanded.remove(id);
                    let _ = cx;
                }
            }),
        ];
        let mut s = Sidebar {
            engine,
            connections,
            conn_tree,
            script_tree,
            scripts_dir,
            nodes: Rc::new(HashMap::new()),
            loaded: HashMap::new(),
            loading: HashSet::new(),
            expanded: HashSet::new(),
            _subs: subs,
        };
        s.rebuild(cx);
        s
    }

    pub fn set_connections(&mut self, c: Connections, cx: &mut Context<Self>) {
        self.connections = c;
        // Forget cached objects of connections that changed or were removed.
        let ids: HashSet<String> = self.connections.connections.iter().map(|c| c.id.clone()).collect();
        self.loaded.retain(|(conn, _), _| ids.contains(conn));
        self.rebuild(cx);
    }

    pub fn refresh_connection(&mut self, conn: &str, cx: &mut Context<Self>) {
        self.loaded.retain(|(c, _), _| c != conn);
        self.engine.forget_connection(conn);
        let expanded: Vec<SharedString> = self.expanded.iter().cloned().collect();
        self.rebuild(cx);
        for id in expanded {
            self.ensure_loaded(&id, cx);
        }
    }

    pub fn set_scripts_dir(&mut self, dir: Option<PathBuf>, cx: &mut Context<Self>) {
        self.scripts_dir = dir;
        self.rebuild(cx);
    }

    pub fn on_objects(&mut self, conn: String, path: ObjPath, items: Result<Vec<ObjItem>, String>, cx: &mut Context<Self>) {
        let key = (conn, path);
        self.loading.remove(&key);
        self.loaded.insert(key, items);
        self.rebuild(cx);
    }

    fn on_conn_tree(&mut self, ev: &TreeEvent, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Expanded(id) => {
                self.expanded.insert(id.clone());
                self.ensure_loaded(id, cx);
            }
            TreeEvent::Collapsed(id) => {
                self.expanded.remove(id);
            }
        }
    }

    fn ensure_loaded(&mut self, id: &SharedString, _cx: &mut Context<Self>) {
        let target = match self.nodes.get(id) {
            Some(Node::Conn { id, .. }) => Some((id.clone(), ObjPath::Root)),
            Some(Node::Obj { conn, path: Some(p), .. }) => Some((conn.clone(), p.clone())),
            _ => None,
        };
        let Some(key) = target else { return };
        if self.loaded.contains_key(&key) || self.loading.contains(&key) {
            return;
        }
        let Some(c) = self.connections.get(&key.0) else { return };
        self.engine.load_objects(&key.0, &params_for(c), key.1.clone());
        self.loading.insert(key);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut nodes = HashMap::new();
        let root = build_tree(&self.connections);
        let conn_items = self.folder_items(&root, &mut nodes);
        let script_items = match &self.scripts_dir {
            Some(d) => script_items(d, &mut nodes, 0),
            None => Vec::new(),
        };
        // Apply expansion state.
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
        let conn_items = expand(conn_items, &self.expanded);
        let script_items = expand(script_items, &self.expanded);
        self.nodes = Rc::new(nodes);
        for (state, items) in [(&self.conn_tree, conn_items), (&self.script_tree, script_items)] {
            state.update(cx, |t, cx| {
                let selected = t.selected_item().map(|i| i.id.clone());
                t.set_items(items, cx);
                if let Some(id) = selected {
                    if let Some(item) = find(t, &id) {
                        t.set_selected_item(Some(&item), cx);
                    }
                }
            });
        }
        cx.notify();
    }

    fn folder_items(&self, f: &FolderNode, nodes: &mut HashMap<SharedString, Node>) -> Vec<TreeItem> {
        let mut out = Vec::new();
        for sub in &f.folders {
            let id: SharedString = format!("f:{}", sub.path).into();
            nodes.insert(id.clone(), Node::Folder);
            out.push(TreeItem::new(id, sub.name.clone()).children(self.folder_items(sub, nodes)));
        }
        for c in &f.connections {
            let id: SharedString = format!("c:{}", c.id).into();
            nodes.insert(id.clone(), Node::Conn { id: c.id.clone(), color: c.color().map(|x| x.rgb) });
            out.push(TreeItem::new(id, c.name.clone()).children(self.obj_children(&c.id, &ObjPath::Root, nodes)));
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
            Some(Node::Conn { id, .. }) => cx.emit(SidebarEvent::OpenConnection(id)),
            Some(Node::Script(p)) => cx.emit(SidebarEvent::OpenScript(p)),
            Some(Node::Obj { conn, path: Some(ObjPath::Object { schema, name, .. }), .. }) => {
                cx.emit(SidebarEvent::SelectFrom { conn, schema, name })
            }
            _ => {}
        }
    }

    fn render_conn_item(
        nodes: &HashMap<SharedString, Node>,
        entry_id: &SharedString,
        label: SharedString,
        folder: Option<bool>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (icon, detail, color, key): (Icon, Option<String>, Option<Hsla>, bool) = match nodes.get(entry_id) {
            Some(Node::Folder) => (Icon::new(IconName::Folder), None, Some(muted), false),
            Some(Node::Conn { color, .. }) => (
                Icon::new(Lucide::Database),
                None,
                Some(color.map(rgb_hsla).unwrap_or(muted)),
                false,
            ),
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
                // Category folders (Tables, Views…) inside a schema.
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
            Some(Node::ScriptDir) => (Icon::new(IconName::Folder), None, Some(muted), false),
            Some(Node::Script(_)) => (Icon::new(Lucide::FileCode), None, Some(theme.green), false),
            None => (Icon::new(IconName::File), None, None, false),
        };
        let is_conn = matches!(nodes.get(entry_id), Some(Node::Conn { .. }));
        let chevron = match folder {
            Some(open) => Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                .xsmall()
                .text_color(muted)
                .into_any_element(),
            None => div().w(px(12.)).into_any_element(),
        };
        h_flex()
            .gap_1p5()
            .items_center()
            .overflow_hidden()
            .child(chevron)
            .child(icon.small().when_some(color, |i, c| i.text_color(c)))
            .child(
                div()
                    .text_sm()
                    .whitespace_nowrap()
                    .when(is_conn, |d| d.font_weight(FontWeight::MEDIUM))
                    .when(key, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label),
            )
            .when_some(detail, |el, d| {
                el.child(div().text_xs().whitespace_nowrap().text_color(muted).overflow_hidden().text_ellipsis().child(d))
            })
            .into_any_element()
    }

    fn section_header(title: &'static str, cx: &App) -> Div {
        h_flex()
            .h(px(30.))
            .px_3()
            .items_center()
            .justify_between()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(title)
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

/// Scripts folder as tree items (folders first). Depth-limited for safety.
fn script_items(dir: &Path, nodes: &mut HashMap<SharedString, Node>, depth: usize) -> Vec<TreeItem> {
    if depth > 8 {
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
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sql")) {
            files.push((name, p));
        }
    }
    dirs.sort_by_key(|d| d.0.to_lowercase());
    files.sort_by_key(|f| f.0.to_lowercase());
    let mut out = Vec::new();
    for (name, p) in dirs {
        let id: SharedString = format!("sd:{}", p.display()).into();
        nodes.insert(id.clone(), Node::ScriptDir);
        let children = script_items(&p, nodes, depth + 1);
        out.push(TreeItem::new(id, name).children(children));
    }
    for (name, p) in files {
        let id: SharedString = format!("sf:{}", p.display()).into();
        nodes.insert(id.clone(), Node::Script(p));
        out.push(TreeItem::new(id, name));
    }
    out
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity();
        let nodes = self.nodes.clone();
        let conn_tree = {
            let (v, n) = (view.clone(), nodes.clone());
            tree(&self.conn_tree, move |ix, entry, selected, _, cx| {
                let id = entry.item().id.clone();
                let el = Sidebar::render_conn_item(&n, &id, entry.item().label.clone(), entry.is_folder().then(|| entry.is_expanded()), cx);
                let view = v.clone();
                ListItem::new(ix)
                    .selected(selected)
                    .py_0p5()
                    .pl(px(14.) * entry.depth() + px(8.))
                    .child(el)
                    .on_click(move |ev: &ClickEvent, _, cx| {
                        if ev.click_count() == 2 {
                            view.update(cx, |s, cx| s.activate(&id, cx));
                        }
                    })
            })
            .context_menu({
                let (view, nodes) = (view.clone(), nodes.clone());
                move |_ix, entry, menu, window, _cx| {
                    let id = entry.item().id.clone();
                    match nodes.get(&id).cloned() {
                        Some(Node::Conn { id: cid, .. }) => {
                            let (a, b, c, d) = (cid.clone(), cid.clone(), cid.clone(), cid);
                            menu.item(PopupMenuItem::new("New query tab").icon(IconName::Plus).on_click(
                                window.listener_for(&view, move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(SidebarEvent::OpenConnection(a.clone()))
                                }),
                            ))
                            .item(PopupMenuItem::new("Refresh").icon(IconName::RefreshCw).on_click(
                                window.listener_for(&view, move |s, _: &ClickEvent, _, cx| s.refresh_connection(&b, cx)),
                            ))
                            .separator()
                            .item(PopupMenuItem::new("Edit connection…").icon(IconName::Settings).on_click(
                                window.listener_for(&view, move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(SidebarEvent::EditConnection(c.clone()))
                                }),
                            ))
                            .item(PopupMenuItem::new("Delete connection").icon(IconName::Delete).on_click(
                                window.listener_for(&view, move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(SidebarEvent::DeleteConnection(d.clone()))
                                }),
                            ))
                        }
                        Some(Node::Obj { conn, path: Some(ObjPath::Object { schema, name, .. }), .. }) => menu.item(
                            PopupMenuItem::new("Select rows (new tab)").icon(IconName::Play).on_click(
                                window.listener_for(&view, move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(SidebarEvent::SelectFrom {
                                        conn: conn.clone(),
                                        schema: schema.clone(),
                                        name: name.clone(),
                                    })
                                }),
                            ),
                        ),
                        _ => menu.item(PopupMenuItem::new("New connection…").icon(IconName::Plus).on_click(
                            window.listener_for(&view, |_, _: &ClickEvent, _, cx| cx.emit(SidebarEvent::NewConnection)),
                        )),
                    }
                }
            })
        };
        let script_tree = {
            let (view, nodes) = (view.clone(), nodes.clone());
            tree(&self.script_tree, move |ix, entry, selected, _, cx| {
                let id = entry.item().id.clone();
                let el = Sidebar::render_conn_item(&nodes, &id, entry.item().label.clone(), entry.is_folder().then(|| entry.is_expanded()), cx);
                let view = view.clone();
                ListItem::new(ix)
                    .selected(selected)
                    .py_0p5()
                    .pl(px(14.) * entry.depth() + px(8.))
                    .child(el)
                    .on_click(move |ev: &ClickEvent, _, cx| {
                        if ev.click_count() == 2 {
                            view.update(cx, |s, cx| s.activate(&id, cx));
                        }
                    })
            })
        };
        let scripts_body = if self.scripts_dir.is_some() {
            div().flex_1().child(script_tree).into_any_element()
        } else {
            v_flex()
                .p_3()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No scripts folder yet.")
                .child(
                    Button::new("pick-scripts")
                        .small()
                        .outline()
                        .icon(IconName::FolderOpen)
                        .label("Choose folder…")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ChooseScriptsDir))),
                )
                .into_any_element()
        };
        let empty = self.connections.connections.is_empty();
        v_resizable("sidebar-split")
            .child(
                resizable_panel()
                    .child(
                        v_flex()
                            .size_full()
                            .child(Sidebar::section_header("CONNECTIONS", cx).child(
                                Button::new("new-conn")
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::Plus)
                                    .tooltip("New connection")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewConnection))),
                            ))
                            .child(if empty {
                                v_flex()
                                    .p_3()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("No connections yet.")
                                    .child(
                                        Button::new("first-conn")
                                            .small()
                                            .primary()
                                            .icon(IconName::Plus)
                                            .label("Add connection")
                                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewConnection))),
                                    )
                                    .into_any_element()
                            } else {
                                div().flex_1().child(conn_tree).into_any_element()
                            }),
                    )
            )
            .child(
                resizable_panel()
                    .size(px(240.))
                    .child(
                        v_flex()
                            .size_full()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(Sidebar::section_header("SCRIPTS", cx).child(
                                Button::new("scripts-dir")
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::FolderOpen)
                                    .tooltip("Choose scripts folder")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ChooseScriptsDir))),
                            ))
                            .child(scripts_body),
                    )
            )
    }
}

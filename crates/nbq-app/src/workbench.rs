//! The window's root view: sidebar, tab strip, active tab, status bar.
//! Owns the engine, the open projects and app config, and routes events.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_core::watch::ProjectWatcher;
use nbq_core::{Connection, Credentials, Project, Settings, Store, Workspace};
use nbq_engine::{ConnState, Engine, EngineOptions, Event, TabId};

use crate::actions::*;
use crate::complete::Catalogs;
use crate::query_tab::QueryTab;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::ui::{Projects, conn_color, connection, project_for_path};

pub struct Workbench {
    engine: Arc<Engine>,
    store: Store,
    settings: Settings,
    workspace: Workspace,
    projects: Projects,
    watcher: Option<ProjectWatcher>,
    sidebar: Entity<Sidebar>,
    tabs: Vec<Entity<QueryTab>>,
    active: usize,
    catalogs: Catalogs,
    next_untitled: usize,
    /// A close that needs confirming (second Ctrl+W).
    close_armed: Option<EntityId>,
    quit_dialog_open: bool,
    closing: bool,
    csv_queue: std::collections::VecDeque<PathBuf>,
    csv_target: Option<PathBuf>,
    palette: Entity<CommandState>,
    focus: FocusHandle,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

fn event_tab(ev: &Event) -> Option<TabId> {
    Some(match ev {
        Event::Conn { tab, .. }
        | Event::Txn { tab, .. }
        | Event::TransactionExpired { tab, .. }
        | Event::StatementStarted { tab, .. }
        | Event::ResultOpened { tab, .. }
        | Event::StatementFinished { tab, .. }
        | Event::Fetch { tab, .. }
        | Event::QueryDone { tab, .. }
        | Event::Export { tab, .. } => *tab,
        Event::Objects { .. } | Event::Catalog { .. } | Event::MetadataConn { .. } => return None,
    })
}

fn error(window: &mut Window, cx: &mut App, msg: impl Into<SharedString>) {
    window.push_notification(Notification::error(msg.into()), cx);
}

impl Workbench {
    fn check_csv_sources(&self, window: &mut Window, cx: &mut Context<Self>) {
        for project in self.projects.borrow().iter() {
            let key = project.key;
            let result = self.engine.csv_source_changes(project.root.clone(), project.local.csv_imports.clone());
            cx.spawn_in(window, async move |this, cx| {
                if let Ok(paths) = result.await { let _ = this.update_in(cx, |wb, _, cx| wb.sidebar.update(cx, |s, cx| s.set_csv_changes(key, paths, cx))); }
            }).detach();
        }
    }
    fn pick_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            if let Some(files) = rfd::AsyncFileDialog::new().add_filter("CSV", &["csv"]).pick_files().await {
                let _ = this.update_in(cx, |wb, window, cx| wb.csv_batch(files.iter().map(|f| f.path().to_path_buf()).collect(), None, window, cx));
            }
        }).detach();
    }

    fn csv_batch(&mut self, paths: Vec<PathBuf>, target: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        self.csv_queue = paths.into_iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv"))).collect();
        self.csv_target = target;
        if let Some(path) = self.csv_queue.pop_front() { self.open_csv_options(path, false, true, window, cx); }
    }

    fn open_csv(&mut self, path: PathBuf, reload: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open_csv_options(path, reload, false, window, cx);
    }

    fn open_csv_options(&mut self, path: PathBuf, reload: bool, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let projects = self.projects.borrow();
        let project = self.csv_target.as_ref().and_then(|target| projects.iter().find(|p| p.connections.connections.iter().any(|c| c.params(None).file_path == *target)))
            .or_else(|| projects.iter().find(|p| p.contains(&path))).or_else(|| self.active_tab().and_then(|t| t.read(cx).project).and_then(|key| projects.iter().find(|p| p.key == key))).or_else(|| projects.first());
        let Some(project) = project else { error(window, cx, "Open a project before importing CSV"); return; };
        let (key, root) = (project.key, project.root.clone());
        let destinations = project.connections.connections.iter().filter(|c| c.kind.is_file_based()).cloned().collect::<Vec<_>>();
        let linked = project.local.csv_imports.iter().rev().find(|r| !force && project.root.join(&r.source) == path).cloned();
        drop(projects);
        if let Some(mut recipe) = linked {
            recipe.source = root.join(&recipe.source); recipe.database = root.join(&recipe.database);
            if !reload { self.show_csv(key, recipe, window, cx); return; }
            let owner = cx.weak_entity();
            crate::csv_dialog::reload(recipe, self.engine.clone(), window, cx, move |recipe, window, cx| { let _ = owner.update(cx, |wb, cx| wb.csv_done(key, recipe, window, cx)); });
            return;
        }
        let preview = self.engine.preview_csv(path.clone(), None, true);
        let engine = self.engine.clone();
        let target = self.csv_target.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = preview.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(preview) => { let owner = cx.weak_entity(); crate::csv_dialog::open(path, root, preview, destinations, target, engine, window, cx, move |recipe, window, cx| { let _ = owner.update(cx, |wb, cx| match recipe { Some(recipe) => wb.csv_done(key, recipe, window, cx), None => { wb.csv_queue.clear(); wb.csv_target = None; } }); }); }
                Err(e) => error(window, cx, e),
            });
        }).detach();
    }

    fn csv_done(&mut self, key: u32, recipe: nbq_driver::csv_import::Recipe, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.projects.borrow().iter().find(|p| p.key == key).cloned();
        let Some(project) = project else { error(window, cx, "Import succeeded; the project was closed before its link could be saved"); return; };
        let result = self.engine.record_csv_import(project, recipe.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = result.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |wb, window, cx| {
                match result {
                    Ok(saved) => {
                        if let Some(project) = wb.projects.borrow_mut().iter_mut().find(|p| p.key == key) {
                            project.connections = saved.connections;
                            project.local.csv_imports = saved.local.csv_imports;
                            project.local.database_ids = saved.local.database_ids;
                        }
                        wb.sidebar.update(cx, |s, cx| s.refresh(cx));
                        wb.check_csv_sources(window, cx);
                        wb.csv_target = Some(recipe.database.clone());
                        wb.show_csv(key, recipe, window, cx);
                        if let Some(path) = wb.csv_queue.pop_front() { wb.open_csv_options(path, false, true, window, cx); } else { wb.csv_target = None; }
                    }
                    Err(e) => error(window, cx, format!("Import succeeded, but saving its link failed: {e}")),
                }
            });
        }).detach();
    }
    fn show_csv(&mut self, key: u32, recipe: nbq_driver::csv_import::Recipe, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.projects.borrow().iter().find(|p| p.key == key).and_then(|p| p.connections.connections.iter().find(|c| c.params(None).file_path == recipe.database).cloned());
        let Some(conn) = conn else { error(window, cx, "The imported database is missing"); return; };
        self.sidebar.update(cx, |s, cx| { s.refresh_connection(&conn.key(), cx); s.show_table(&conn, &recipe.table, cx); });
        let sql = format!("SELECT * FROM {} LIMIT 100;", nbq_driver::csv_import::quote(&recipe.table));
        let tab = self.new_tab(Some(conn), Some(key), window, cx);
        tab.update(cx, |t, cx| { t.editor.update(cx, |e, cx| e.set_value(sql, window, cx)); t.run_statement(cx); });
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = match std::env::var_os("NBQ_CONFIG_DIR") {
            Some(d) => Store::new(PathBuf::from(d)),
            None => Store::default_location(),
        };
        let mut problems = Vec::new();
        let settings = store.load_settings().unwrap_or_else(|e| {
            problems.push(e.to_string());
            Settings::default()
        });
        let mut workspace = store.load_workspace().unwrap_or_else(|e| {
            problems.push(e.to_string());
            Workspace::default()
        });
        Theme::change(if settings.dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);

        let mut watcher = ProjectWatcher::new().map_err(|e| problems.push(e.to_string())).ok();
        let mut projects = Vec::new();
        for root in workspace.open.clone() {
            match Project::open(&root) {
                Ok(p) => {
                    problems.extend(p.warnings.iter().cloned());
                    if let Some(w) = watcher.as_mut() {
                        if let Err(e) = w.watch(&p.root) {
                            problems.push(e.to_string());
                        }
                    }
                    projects.push(p);
                }
                Err(e) => {
                    problems.push(format!("Could not open project: {e}"));
                    workspace.closed(&root);
                }
            }
        }
        let projects: Projects = Rc::new(RefCell::new(projects));

        let mut opts = EngineOptions::new(Store::default_spool_dir());
        opts.initial_rows = settings.initial_rows;
        opts.transaction_idle_timeout = Duration::from_secs(settings.transaction_idle_timeout_secs.max(1));
        let (engine, mut rx) = Engine::new(opts);

        let sidebar = cx.new(|cx| Sidebar::new(engine.clone(), projects.clone(), workspace.recent.clone(), cx));
        let subs = vec![cx.subscribe_in(&sidebar, window, |this, _, ev: &SidebarEvent, window, cx| {
            this.on_sidebar(ev, window, cx)
        })];
        let events = cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if this.update_in(cx, |wb, window, cx| wb.on_event(ev, window, cx)).is_err() {
                    break;
                }
            }
        });
        // Outside changes to project folders (git pull, an editor): poll the
        // watcher's change set; cheap when nothing changed.
        let watch = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                if this.update_in(cx, |wb, window, cx| wb.poll_watcher(window, cx)).is_err() {
                    break;
                }
            }
        });
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let mut wb = Workbench {
            engine,
            store,
            settings,
            workspace,
            projects,
            watcher,
            sidebar,
            tabs: Vec::new(),
            active: 0,
            catalogs: Rc::new(RefCell::new(HashMap::new())),
            next_untitled: 1,
            close_armed: None,
            quit_dialog_open: false,
            closing: false,
            csv_queue: Default::default(),
            csv_target: None,
            palette,
            focus: cx.focus_handle(),
            _tasks: vec![events, watch],
            _subs: subs,
        };
        let first = wb.projects.borrow().first().and_then(|p| p.connections.connections.first().cloned());
        wb.new_tab(first, None, window, cx);
        wb.check_csv_sources(window, cx);
        // The component Root is attached by open_window only after this builder
        // returns. Notifications need its WindowState, even though init ran.
        window.defer(cx, move |window, cx| {
            for problem in problems {
                error(window, cx, problem);
            }
        });
        let owner = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            owner.update(cx, |wb, cx| wb.request_close(window, cx)).unwrap_or(true)
        });
        wb
    }

    fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active)
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing { return false; }
        if self.tabs.iter().any(|tab| tab.read(cx).saving) {
            error(window, cx, "Wait for the script save to finish before closing.");
            return false;
        }
        if !self.tabs.iter().any(|tab| { let (dirty, pending) = self.tab_risk(tab, cx); dirty || pending }) {
            self.begin_shutdown(window, cx);
            return false;
        }
        if self.quit_dialog_open { return false; }
        self.quit_dialog_open = true;
        let owner = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ok, cancel) = (owner.clone(), owner.clone());
            dialog.title("Quit not-bad-query?")
                .child("Unsaved scripts will be discarded. Running queries will stop and open transactions will roll back.")
                .footer(DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("keep-working").outline().label("Keep working")))
                    .child(DialogAction::new().child(Button::new("quit").primary().label("Quit"))))
                .on_cancel(move |_, _, cx| { let _ = cancel.update(cx, |wb, _| wb.quit_dialog_open = false); true })
                .on_ok(move |_, window, cx| {
                    let _ = ok.update(cx, |wb, cx| {
                        // A save may have started while the dialog was open.
                        if wb.tabs.iter().any(|tab| tab.read(cx).saving) {
                            wb.quit_dialog_open = false;
                            error(window, cx, "Wait for the script save to finish before closing.");
                            return;
                        }
                        wb.begin_shutdown(window, cx);
                    });
                    true
                })
        });
        false
    }

    fn begin_shutdown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing { return; }
        self.closing = true;
        let done = self.engine.shutdown();
        cx.spawn_in(window, async move |this, cx| {
            let _ = done.await;
            let _ = this.update_in(cx, |_, window, _| window.remove_window());
        }).detach();
    }

    fn with_active(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut QueryTab, &mut Context<QueryTab>)) {
        if let Some(t) = self.active_tab().cloned() {
            t.update(cx, f);
        }
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab() {
            let ed = t.read(cx).editor.clone();
            window.defer(cx, move |window, cx| ed.update(cx, |e, cx| e.focus(window, cx)));
        }
    }

    /// The project new things go into: the active tab's, else the only/first one.
    fn current_project(&self, cx: &App) -> Option<u32> {
        self.active_tab()
            .and_then(|t| t.read(cx).project)
            .or_else(|| self.projects.borrow().first().map(|p| p.key))
    }

    pub fn new_tab(
        &mut self,
        conn: Option<Connection>,
        project: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<QueryTab> {
        let n = self.next_untitled;
        self.next_untitled += 1;
        let (engine, catalogs, settings, projects) =
            (self.engine.clone(), self.catalogs.clone(), self.settings.clone(), self.projects.clone());
        let tab = cx.new(|cx| {
            let mut t = QueryTab::new(engine, catalogs, projects, n, &settings, window, cx);
            t.project = project;
            if conn.is_some() {
                t.set_connection(conn, cx);
            }
            t
        });
        self.tabs.push(tab.clone());
        self.active = self.tabs.len() - 1;
        self.focus_editor(window, cx);
        cx.notify();
        tab
    }

    /// Whether closing would lose something: (unsaved edits, uncommitted changes).
    fn tab_risk(&self, tab: &Entity<QueryTab>, cx: &App) -> (bool, bool) {
        let t = tab.read(cx);
        (t.dirty, t.txn.in_txn || t.is_running() || t.saving)
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else { return };
        if tab.read(cx).saving {
            error(window, cx, "Wait for the script save to finish before closing.");
            return;
        }
        let (dirty, pending) = self.tab_risk(&tab, cx);
        let id = tab.entity_id();
        if (dirty || pending) && self.close_armed != Some(id) {
            self.close_armed = Some(id);
            let what = match (dirty, pending) {
                (true, true) => "unsaved edits and uncommitted changes",
                (true, false) => "unsaved edits",
                _ => "uncommitted changes (they will be rolled back)",
            };
            window.push_notification(
                Notification::warning(format!("This tab has {what}. Press Ctrl+W again to close it.")),
                cx,
            );
            return;
        }
        self.close_armed = None;
        self.engine.close_tab(tab.read(cx).engine_tab);
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            let (conn, project) = {
                let t = tab.read(cx);
                (t.conn.clone(), t.project)
            };
            self.new_tab(conn, project, window, cx);
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
            if ix < self.active {
                self.active -= 1;
            }
        }
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn on_event(&mut self, ev: Event, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing { return; }
        match ev {
            Event::MetadataConn { conn, state } => {
                self.sidebar.update(cx, |s, cx| s.on_metadata_state(conn, state, cx));
            }
            Event::Objects { conn, path, items } => {
                self.sidebar.update(cx, |s, cx| s.on_objects(conn, path, items, cx));
            }
            Event::Catalog { conn, tables } => match tables {
                Ok(t) => {
                    if !self.sidebar.read(cx).is_disconnected(&conn) {
                        self.catalogs.borrow_mut().insert(conn, t);
                    }
                }
                Err(e) => tracing::warn!("catalog for {conn}: {e}"),
            },
            ev => {
                let Some(tab_id) = event_tab(&ev) else { return };
                let Some(tab) = self.tabs.iter().find(|t| t.read(cx).engine_tab == tab_id).cloned() else { return };
                if let Event::Conn { state: ConnState::Connected, .. } = &ev {
                    if let Some(c) = tab.read(cx).conn.clone().filter(|_| tab.read(cx).session_enabled) {
                        self.sidebar.update(cx, |s, _| s.allow_connection(&c.key()));
                        if !self.catalogs.borrow().contains_key(&c.key()) {
                            self.engine.load_catalog(&c.key(), &crate::ui::params_for(&c));
                        }
                    }
                }
                tab.update(cx, |t, cx| t.on_event(ev, window, cx));
                cx.notify();
            }
        }
    }

    fn reuse_empty_tab(&self, cx: &App) -> Option<Entity<QueryTab>> {
        self.active_tab().cloned().filter(|t| {
            let t = t.read(cx);
            t.path.is_none() && t.text(cx).trim().is_empty() && !t.is_running() && !t.txn.in_txn && !t.saving
        })
    }

    fn on_sidebar(&mut self, ev: &SidebarEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing { return; }
        match ev {
            SidebarEvent::Connect(key) => {
                let Some(c) = connection(&self.projects.borrow(), key) else { return };
                self.sidebar.update(cx, |s, _| s.allow_connection(key));
                self.engine.connect_profile(key, &crate::ui::params_for(&c));
                for tab in &self.tabs {
                    let t = tab.read(cx);
                    if t.conn.as_ref().is_some_and(|c| c.key() == *key)
                        && matches!(t.conn_state, ConnState::Disconnected | ConnState::Failed(_))
                        && !t.txn.in_txn && !t.is_running()
                    {
                        tab.update(cx, |t, cx| t.reconnect(cx));
                    }
                }
            }
            SidebarEvent::Disconnect(key) => {
                let pending = self.tabs.iter().any(|tab| {
                    let t = tab.read(cx);
                    t.conn.as_ref().is_some_and(|c| c.key() == *key) && (t.txn.in_txn || t.is_running())
                });
                if pending {
                    let title = connection(&self.projects.borrow(), key).map(|c| c.name).unwrap_or_default();
                    let (key, owner) = (key.clone(), cx.weak_entity());
                    window.open_dialog(cx, move |dialog, _, _| {
                        let (key, owner) = (key.clone(), owner.clone());
                        dialog.title(format!("Disconnect {title}?"))
                            .child("Running queries will stop and all open transactions for this connection will roll back. Scripts and fetched rows will stay open.")
                            .footer(DialogFooter::new()
                                .child(DialogClose::new().child(Button::new("keep-connected").outline().label("Keep connected")))
                                .child(DialogAction::new().child(Button::new("disconnect-rollback").danger().label("Disconnect and roll back"))))
                            .on_ok(move |_, _, cx| {
                                let _ = owner.update(cx, |wb, cx| wb.disconnect_connection(&key, cx));
                                true
                            })
                    });
                } else {
                    self.disconnect_connection(key, cx);
                }
            }
            SidebarEvent::OpenConnection(key) => {
                let c = connection(&self.projects.borrow(), key);
                let project = c.as_ref().map(|c| c.project_key);
                if let Some(t) = self.reuse_empty_tab(cx) {
                    t.update(cx, |t, cx| {
                        t.project = project;
                        t.set_connection(c, cx)
                    });
                    self.focus_editor(window, cx);
                    return;
                }
                self.new_tab(c, project, window, cx);
            }
            SidebarEvent::ReloadCsv(p) => self.open_csv(p.clone(), true, window, cx),
            SidebarEvent::ImportCsvFile(p) => self.open_csv_options(p.clone(), false, true, window, cx),
            SidebarEvent::ImportFiles { paths, connection: key } => {
                let conn = connection(&self.projects.borrow(), key);
                if let Some(conn) = conn.filter(|c| c.kind.is_file_based()) { self.csv_batch(paths.clone(), Some(conn.params(None).file_path), window, cx); }
                else { error(window, cx, "CSV imports require SQLite or DuckDB"); }
            }
            SidebarEvent::ShowCsvTable(path) => {
                let projects = self.projects.borrow();
                if let Some((conn, table)) = projects.iter().find_map(|p| p.local.csv_imports.iter().rev().find(|r| p.root.join(&r.source) == *path).and_then(|r| p.connections.connections.iter().find(|c| c.params(None).file_path == p.root.join(&r.database)).map(|c| (c.clone(), r.table.clone())))) {
                    drop(projects);
                    self.sidebar.update(cx, |s, cx| s.show_table(&conn, &table, cx));
                }
            }
            SidebarEvent::OpenScript(p) => self.open_path(p.clone(), window, cx),
            SidebarEvent::NewConnection { project, folder } => {
                self.edit_connection(*project, None, folder.clone(), window, cx)
            }
            SidebarEvent::EditConnection(key) => {
                let c = connection(&self.projects.borrow(), key);
                if let Some(c) = c {
                    self.edit_connection(c.project_key, Some(c), None, window, cx)
                }
            }
            SidebarEvent::DeleteConnection(key) => self.delete_connection(key, window, cx),
            SidebarEvent::SelectFrom { conn, schema, name } => {
                let c = connection(&self.projects.borrow(), conn);
                let project = c.as_ref().map(|c| c.project_key);
                let q = |s: &str| {
                    if s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                        s.to_string()
                    } else {
                        format!("\"{}\"", s.replace('"', "\"\""))
                    }
                };
                let sql = format!("select *\nfrom {}.{}", q(schema), q(name));
                let tab = self.new_tab(c, project, window, cx);
                tab.update(cx, |t, cx| {
                    t.editor.update(cx, |e, cx| e.set_value(sql.as_str(), window, cx));
                    t.run_statement(cx);
                });
            }
            SidebarEvent::NewScript { project, dir } => {
                let conn = self
                    .active_tab()
                    .and_then(|t| t.read(cx).conn.clone())
                    .filter(|c| c.project_key == *project);
                let tab = match self.reuse_empty_tab(cx) {
                    Some(t) => {
                        t.update(cx, |t, cx| {
                            t.project = Some(*project);
                            if t.conn.as_ref().is_some_and(|c| c.project_key != *project) {
                                t.set_connection(None, cx);
                            }
                        });
                        t
                    }
                    None => self.new_tab(conn, Some(*project), window, cx),
                };
                let dir = dir.clone();
                tab.update(cx, |t, _| t.save_dir = Some(dir));
                self.focus_editor(window, cx);
            }
            SidebarEvent::NewProject => self.new_project(window, cx),
            SidebarEvent::OpenProject(None) => self.pick_project(window, cx),
            SidebarEvent::OpenProject(Some(root)) => self.open_project(root.clone(), window, cx),
            SidebarEvent::CloseProject(key) => self.close_project(*key, window, cx),
        }
    }

    // ---- projects ----

    fn disconnect_connection(&mut self, key: &str, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if tab.read(cx).conn.as_ref().is_some_and(|c| c.key() == key) {
                tab.update(cx, |t, cx| t.disconnect(cx));
            }
        }
        self.engine.forget_connection(key);
        self.catalogs.borrow_mut().remove(key);
        self.sidebar.update(cx, |s, cx| s.disconnect(key, cx));
        cx.notify();
    }

    fn save_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.store.save_workspace(&self.workspace) {
            error(window, cx, format!("Could not save workspace: {e}"));
        }
        let recent = self.workspace.recent.clone();
        self.sidebar.update(cx, |s, cx| {
            s.set_recent(recent, cx);
            s.refresh(cx);
        });
        cx.notify();
    }

    fn add_project(&mut self, p: Project, window: &mut Window, cx: &mut Context<Self>) {
        for w in &p.warnings {
            error(window, cx, w.clone());
        }
        if let Some(w) = self.watcher.as_mut() {
            if let Err(e) = w.watch(&p.root) {
                error(window, cx, e.to_string());
            }
        }
        self.workspace.opened(&p.root);
        let key = p.key;
        self.projects.borrow_mut().push(p);
        self.save_workspace(window, cx);
        // An empty, project-less tab joins the new project.
        if let Some(t) = self.reuse_empty_tab(cx) {
            t.update(cx, |t, _| {
                if t.project.is_none() {
                    t.project = Some(key);
                }
            });
        }
    }

    fn open_project(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.projects.borrow().iter().any(|p| p.root == root) {
            return;
        }
        match Project::open(&root) {
            Ok(p) => self.add_project(p, window, cx),
            Err(e) => {
                error(window, cx, e.to_string());
                if !root.exists() {
                    self.workspace.recent.retain(|r| r != &root);
                    self.save_workspace(window, cx);
                }
            }
        }
    }

    fn pick_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let Some(dir) = rfd::AsyncFileDialog::new().set_title("Open not-bad-query project").pick_folder().await else {
                return;
            };
            let root = dir.path().to_path_buf();
            let _ = this.update_in(cx, |wb, window, cx| {
                if Project::is_project(&root) {
                    wb.open_project(root, window, cx);
                } else {
                    error(
                        window,
                        cx,
                        format!(
                            "{} is not a not-bad-query project. Use \"New project…\" to make it one.",
                            root.display()
                        ),
                    );
                }
            });
        })
        .detach();
    }

    /// Pick a folder (new or existing, e.g. `repo/queries`) and make it a project.
    fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let Some(dir) =
                rfd::AsyncFileDialog::new().set_title("Choose or create a folder for the project").pick_folder().await
            else {
                return;
            };
            let root = dir.path().to_path_buf();
            let _ = this.update_in(cx, |wb, window, cx| {
                if Project::is_project(&root) {
                    // Already one: just open it.
                    wb.open_project(root, window, cx);
                    return;
                }
                match Project::create(&root, "") {
                    Ok(p) => {
                        let key = p.key;
                        wb.add_project(p, window, cx);
                        wb.edit_connection(key, None, None, window, cx);
                    }
                    Err(e) => error(window, cx, e.to_string()),
                }
            });
        })
        .detach();
    }

    fn close_project(&mut self, key: u32, window: &mut Window, cx: &mut Context<Self>) {
        let tabs: Vec<_> = self.tabs.iter().filter(|t| t.read(cx).project == Some(key)).cloned().collect();
        if tabs.iter().any(|t| {
            let (dirty, pending) = self.tab_risk(t, cx);
            dirty || pending
        }) {
            window.push_notification(
                Notification::warning("Save or close this project's tabs with unsaved edits or uncommitted changes first."),
                cx,
            );
            return;
        }
        for t in &tabs {
            self.engine.close_tab(t.read(cx).engine_tab);
        }
        self.tabs.retain(|t| !tabs.contains(t));
        let removed = {
            let mut ps = self.projects.borrow_mut();
            let ix = ps.iter().position(|p| p.key == key);
            ix.map(|ix| ps.remove(ix))
        };
        if let Some(p) = removed {
            for c in &p.connections.connections {
                self.engine.forget_connection(&c.key());
                self.catalogs.borrow_mut().remove(&c.key());
            }
            if let Some(w) = self.watcher.as_mut() {
                w.unwatch(&p.root);
            }
            self.workspace.closed(&p.root);
        }
        if self.tabs.is_empty() {
            self.new_tab(None, None, window, cx);
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.save_workspace(window, cx);
    }

    /// Reload projects whose files changed outside the app.
    fn poll_watcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = &self.watcher else { return };
        let changed = w.take_changed();
        if changed.is_empty() {
            return;
        }
        if changed.iter().any(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv"))) { self.check_csv_sources(window, cx); }
        let mut reloaded = Vec::new();
        for p in self.projects.borrow_mut().iter_mut() {
            if changed.iter().any(|c| p.is_relevant_change(c)) {
                let before = p.connections.connections.clone();
                if let Err(e) = p.reload() {
                    error(window, cx, e.to_string());
                }
                reloaded.push((before, p.connections.connections.clone()));
            }
        }
        if reloaded.is_empty() {
            return;
        }
        for (before, after) in reloaded {
            for old in &before {
                if !after.iter().any(|n| n.key() == old.key() && n == old) {
                    self.connection_changed(old, window, cx);
                }
            }
        }
        self.sidebar.update(cx, |s, cx| s.refresh(cx));
        cx.notify();
    }

    // ---- connections ----

    /// A connection's settings changed: drop caches and reconnect its tabs.
    fn connection_changed(&mut self, old: &Connection, _window: &mut Window, cx: &mut Context<Self>) {
        let key = old.key();
        self.engine.forget_connection(&key);
        self.catalogs.borrow_mut().remove(&key);
        self.sidebar.update(cx, |s, cx| s.forget_connection(&key, cx));
        let new = connection(&self.projects.borrow(), &key);
        for t in self.tabs.clone() {
            if t.read(cx).conn.as_ref().is_some_and(|x| x.key() == key) {
                t.update(cx, |t, cx| t.set_connection(new.clone(), cx));
            }
        }
    }

    fn edit_connection(
        &mut self,
        project: u32,
        existing: Option<Connection>,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let user = existing.as_ref().and_then(crate::ui::credentials).map(|c| c.user).unwrap_or_default();
        let this = cx.weak_entity();
        let root = crate::ui::project(&self.projects.borrow(), project).map(|p| p.root.clone()).unwrap_or_default();
        crate::conn_dialog::open(existing.clone(), folder, user, self.engine.clone(), root, window, cx, move |c, user, password, window, cx| {
            let _ = this.update(cx, |wb, cx| wb.save_connection(project, existing.clone(), c, user, password, window, cx));
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn save_connection(
        &mut self,
        project: u32,
        old: Option<Connection>,
        c: Connection,
        user: String,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = {
            let mut ps = self.projects.borrow_mut();
            let Some(p) = ps.iter_mut().find(|p| p.key == project) else { return };
            p.save_connection(c)
        };
        let c = match saved {
            Ok(c) => c,
            Err(e) => return error(window, cx, format!("Could not save connection: {e}")),
        };
        // Keep the stored password when the field was left empty.
        if !c.kind.is_file_based() {
            let password = password.or_else(|| crate::ui::credentials(&c).and_then(|x| x.password));
            if let Err(e) = nbq_core::secrets::set(&c.secret_key(), &Credentials { user, password }) {
                error(window, cx, format!("Could not store credentials: {e}"));
            }
        }
        match old {
            Some(old) => self.connection_changed(&old, window, cx),
            None => {
                // A brand-new connection: put it on an empty tab of this project.
                if let Some(t) = self.active_tab().cloned() {
                    let fits = {
                        let t = t.read(cx);
                        t.conn.is_none() && t.project.is_none_or(|k| k == project)
                    };
                    if fits {
                        t.update(cx, |t, cx| t.set_connection(Some(c.clone()), cx));
                    }
                }
            }
        }
        self.sidebar.update(cx, |s, cx| s.refresh(cx));
        cx.notify();
    }

    fn delete_connection(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = connection(&self.projects.borrow(), key) else { return };
        let res = {
            let mut ps = self.projects.borrow_mut();
            match ps.iter_mut().find(|p| p.key == c.project_key) {
                Some(p) => p.delete_connection(&c.id),
                None => return,
            }
        };
        if let Err(e) = res {
            return error(window, cx, format!("Could not delete connection: {e}"));
        }
        if !c.kind.is_file_based() { let _ = nbq_core::secrets::delete(&c.secret_key()); }
        self.engine.forget_connection(key);
        self.catalogs.borrow_mut().remove(key);
        self.sidebar.update(cx, |s, cx| s.refresh(cx));
        cx.notify();
    }

    fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.current_project(cx) {
            Some(p) => self.edit_connection(p, None, None, window, cx),
            None => window.push_notification(
                Notification::warning("Create or open a project first: connections live in a project."),
                cx,
            ),
        }
    }

    // ---- files ----

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")) { self.open_csv(path, false, window, cx); return; }
        if let Some(ix) = self.tabs.iter().position(|t| t.read(cx).path.as_ref() == Some(&path)) {
            self.active = ix;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let source = path.clone();
            let result = cx.background_executor().spawn(async move { nbq_core::files::read_script(&source) }).await;
            let _ = this.update_in(cx, |wb, window, cx| match result {
                Ok(text) => wb.show_file(path, text, window, cx),
                Err(e) => error(window, cx, format!("{}: {e}", path.display())),
            });
        }).detach();
    }

    fn show_file(&mut self, path: PathBuf, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| t.read(cx).path.as_ref() == Some(&path)) {
            self.active = ix;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        // The script's remembered connection; else keep the current one if
        // it's from the same project.
        let (project, conn) = {
            let ps = self.projects.borrow();
            let project = project_for_path(&ps, &path);
            let remembered = project
                .and_then(|k| crate::ui::project(&ps, k))
                .and_then(|p| p.script_connection(&path).cloned());
            let current = self.active_tab().and_then(|t| t.read(cx).conn.clone());
            let conn = remembered.or(current.filter(|c| Some(c.project_key) == project));
            (project, conn)
        };
        let tab = match self.reuse_empty_tab(cx) {
            Some(t) => t,
            None => self.new_tab(None, project, window, cx),
        };
        tab.update(cx, |t, cx| t.load_file(path, &text, conn, window, cx));
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn project_scripts_dir(&self, key: Option<u32>) -> Option<PathBuf> {
        let ps = self.projects.borrow();
        key.and_then(|k| crate::ui::project(&ps, k)).or(ps.first()).map(Project::scripts_dir)
    }

    fn save(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        let (path, text, title, project, save_dir, expected) = {
            let t = tab.read(cx);
            if t.saving { return; }
            (t.path.clone(), t.text(cx), t.title(), t.project, t.save_dir.clone(), t.saved_text.clone())
        };
        if let (Some(p), false) = (path.clone(), save_as) {
            self.save_script(tab, p, text, expected, window, cx);
            return;
        }
        let dir = path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or(save_dir)
            .or_else(|| self.project_scripts_dir(project));
        let name = if title.ends_with(".sql") { title.to_string() } else { format!("{title}.sql") };
        cx.spawn_in(window, async move |this, cx| {
            let mut d = rfd::AsyncFileDialog::new().set_file_name(&name).add_filter("SQL", &["sql"]);
            if let Some(dir) = dir {
                let _ = std::fs::create_dir_all(&dir);
                d = d.set_directory(dir);
            }
            let Some(f) = d.save_file().await else { return };
            let p = f.path().to_path_buf();
            let expected = if path.as_ref() == Some(&p) { expected } else { None };
            let _ = this.update_in(cx, |wb, window, cx| {
                wb.save_script(tab, p, text, expected, window, cx);
            });
        })
        .detach();
    }

    fn save_script(&mut self, tab: Entity<QueryTab>, path: PathBuf, text: String, expected: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if tab.read(cx).saving { return; }
        let t = tab.read(cx);
        let project = project_for_path(&self.projects.borrow(), &path);
        if project != t.project && (t.txn.in_txn || t.is_running()) {
            error(window, cx, "Finish the query and commit or roll back before saving into another project.");
            return;
        }
        tab.update(cx, |t, _| t.saving = true);
        cx.spawn_in(window, async move |this, cx| {
            let (target, contents) = (path.clone(), text.clone());
            let result = cx.background_executor().spawn(async move {
                nbq_core::files::save_script(&target, &contents, expected.as_deref())
            }).await;
            let _ = this.update_in(cx, |wb, window, cx| {
                tab.update(cx, |t, _| t.saving = false);
                match result {
                    Ok(()) => {
                        tab.update(cx, |t, cx| t.mark_saved(path, text, cx));
                        wb.sidebar.update(cx, |s, cx| s.refresh(cx));
                    }
                    Err(e) => error(window, cx, format!("Save failed: {e}")),
                }
            });
        }).detach();
    }

    fn open_dialog_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.project_scripts_dir(self.current_project(cx));
        cx.spawn_in(window, async move |this, cx| {
            let mut d = rfd::AsyncFileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]);
            if let Some(dir) = dir {
                d = d.set_directory(dir);
            }
            let Some(f) = d.pick_file().await else { return };
            let p = f.path().to_path_buf();
            let _ = this.update_in(cx, |wb, window, cx| wb.open_path(p, window, cx));
        })
        .detach();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.dark = !self.settings.dark;
        Theme::change(if self.settings.dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);
        let _ = self.store.save_settings(&self.settings);
        cx.notify();
    }

    /// Run a command by action (used by key bindings and the palette).
    fn perform(&mut self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        let a = action.as_any();
        if a.is::<RunStatement>() {
            self.with_active(cx, |t, cx| t.run_statement(cx));
        } else if a.is::<RunScript>() {
            self.with_active(cx, |t, cx| t.run_script(cx));
        } else if a.is::<CancelQuery>() {
            self.with_active(cx, |t, _| t.cancel());
        } else if a.is::<Commit>() {
            self.with_active(cx, |t, _| t.commit());
        } else if a.is::<Rollback>() {
            self.with_active(cx, |t, _| t.rollback());
        } else if a.is::<ToggleAutocommit>() {
            self.with_active(cx, |t, _| t.toggle_autocommit());
        } else if a.is::<NewTab>() {
            let (conn, project) =
                self.active_tab().map(|t| (t.read(cx).conn.clone(), t.read(cx).project)).unwrap_or_default();
            self.new_tab(conn, project, window, cx);
        } else if a.is::<CloseTab>() {
            self.close_tab(self.active, window, cx);
        } else if a.is::<NextTab>() {
            if !self.tabs.is_empty() {
                self.active = (self.active + 1) % self.tabs.len();
                self.focus_editor(window, cx);
            }
        } else if a.is::<PrevTab>() {
            if !self.tabs.is_empty() {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
                self.focus_editor(window, cx);
            }
        } else if a.is::<SaveFile>() {
            self.save(false, window, cx);
        } else if a.is::<SaveFileAs>() {
            self.save(true, window, cx);
        } else if a.is::<ImportCsv>() {
            self.pick_csv(window, cx);
        } else if a.is::<OpenFile>() {
            self.open_dialog_file(window, cx);
        } else if a.is::<ExportCsv>() {
            if let Some(t) = self.active_tab().cloned() {
                t.update(cx, |t, cx| t.export_csv(window, cx));
            }
        } else if a.is::<CopyCell>() {
            self.with_active(cx, |t, cx| t.copy_cell(cx));
        } else if a.is::<NewConnection>() {
            self.new_connection(window, cx);
        } else if a.is::<NewProject>() {
            self.new_project(window, cx);
        } else if a.is::<OpenProject>() {
            self.pick_project(window, cx);
        } else if a.is::<ToggleTheme>() {
            self.toggle_theme(window, cx);
        } else if a.is::<FocusEditor>() {
            self.focus_editor(window, cx);
        } else if a.is::<CommandPalette>() {
            self.open_palette(window, cx);
        }
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palette = self.palette.clone();
        let owner = cx.weak_entity();
        let focus_on_mount = Rc::new(std::cell::Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let (palette, owner, focus_on_mount) = (palette.clone(), owner.clone(), focus_on_mount.clone());
            dialog.close_button(false).p_0().w(px(520.)).content(move |content, window, cx| {
                if focus_on_mount.replace(false) {
                    let palette = palette.clone();
                    window.defer(cx, move |window, cx| palette.read(cx).focus_handle(cx).focus(window, cx));
                }
                let owner = owner.clone();
                let items = palette_items();
                content.child(
                    Command::new(&palette)
                        .placeholder("Type a command…")
                        .items(items)
                        .on_confirm(move |ix, window, cx| {
                            window.close_dialog(cx);
                            let cmds = crate::actions::palette();
                            if let Some((_, _, action)) = cmds.get(ix.row) {
                                let action = action.boxed_clone();
                                let owner = owner.clone();
                                // After the palette has fully closed, so a command that
                                // opens its own dialog isn't closed with it.
                                window.defer(cx, move |window, cx| {
                                    let _ = owner.update(cx, |wb, cx| wb.perform(&*action, window, cx));
                                });
                            }
                        }),
                )
            })
        });
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        TabBar::new("query-tabs")
            .selected_index(self.active)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.active = *ix;
                this.focus_editor(window, cx);
                cx.notify();
            }))
            .children(self.tabs.iter().enumerate().map(|(ix, t)| {
                let t = t.read(cx);
                let color = t.conn.as_ref().map(|c| conn_color(c, cx)).unwrap_or(theme.muted_foreground.opacity(0.5));
                let indicator = if t.is_running() {
                    Icon::new(IconName::LoaderCircle).xsmall().text_color(theme.warning).into_any_element()
                } else {
                    div().size(px(8.)).rounded_full().bg(color).into_any_element()
                };
                let pending = t.txn.in_txn && t.txn.pending_writes > 0;
                Tab::new()
                    .prefix(h_flex().pl_2().child(indicator))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(t.title())
                            .when(t.dirty, |el| el.child(div().text_color(theme.muted_foreground).child("●")))
                            .when(pending, |el| {
                                el.child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .bg(theme.warning.opacity(0.15))
                                        .text_color(theme.warning)
                                        .child(t.txn.pending_writes.to_string()),
                                )
                            }),
                    )
                    .suffix(
                        Button::new(("close-tab", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_armed = None;
                                this.close_tab(ix, window, cx);
                            })),
                    )
            }))
            .suffix(
                Button::new("new-tab")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip("New tab (Ctrl+T)")
                    .on_click(cx.listener(|this, _, window, cx| this.perform(&NewTab, window, cx))),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let left = self.active_tab().map(|t| {
            let t = t.read(cx);
            match &t.conn {
                Some(c) => {
                    let color = conn_color(c, cx);
                    let state = match &t.conn_state {
                        ConnState::Connected => ("connected", theme.success),
                        ConnState::Connecting => ("connecting…", theme.warning),
                        ConnState::Failed(_) => ("failed", theme.danger),
                        ConnState::Disconnected => ("disconnected", theme.muted_foreground),
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size(px(8.)).rounded_full().bg(color))
                        .child(div().text_color(theme.foreground).child(c.name.clone()))
                        .child(if c.kind.is_file_based() { c.file_path.display().to_string() } else { format!("{}:{}/{}", c.host, c.port, c.database) })
                        .child(div().text_color(state.1).child(state.0))
                        .into_any_element()
                }
                None => div().child("No connection").into_any_element(),
            }
        });
        StatusBar::new()
            .text_xs()
            .text_color(theme.muted_foreground)
            .when_some(left, |s, l| s.left(l))
            .right(
                h_flex()
                    .gap_3()
                    .child("Ctrl+K commands")
                    .child(
                        Button::new("theme")
                            .xsmall()
                            .ghost()
                            .icon(if self.settings.dark { Icon::new(Lucide::Sun) } else { Icon::new(Lucide::Moon) })
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
                    ),
            )
    }
}

fn palette_items() -> Vec<CommandItem> {
    crate::actions::palette()
        .into_iter()
        .map(|(label, keys, _)| {
            let item = CommandItem::new().label(label);
            if keys.is_empty() { item } else { item.keywords([keys]) }
        })
        .collect()
}

macro_rules! bind {
    ($el:expr, $cx:expr, $($a:ty),*) => {
        $el$(.on_action($cx.listener(|this, a: &$a, window, cx| this.perform(a, window, cx))))*
    };
}

impl Render for Workbench {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let states = self.tabs.iter().filter_map(|tab| {
            let tab = tab.read(cx);
            tab.conn.as_ref().map(|c| (c.key(), tab.conn_state.clone()))
        }).collect();
        self.sidebar.update(cx, |s, cx| s.set_tab_states(states, cx));
        let theme = cx.theme().clone();
        let active = self.active_tab().cloned();
        let title_conn = active.as_ref().and_then(|t| t.read(cx).conn.clone());
        let title_project = active
            .as_ref()
            .and_then(|t| t.read(cx).project)
            .and_then(|k| crate::ui::project(&self.projects.borrow(), k).map(|p| p.name().to_string()));
        let root = v_flex()
            .id("workbench")
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.csv_batch(paths.paths().to_vec(), None, window, cx);
            }))
            .key_context("Workbench")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground);
        let root = bind!(
            root, cx, RunStatement, RunScript, CancelQuery, Commit, Rollback, ToggleAutocommit, NewTab, CloseTab,
            NextTab, PrevTab, SaveFile, SaveFileAs, OpenFile, ImportCsv, ExportCsv, CopyCell, NewConnection, NewProject, OpenProject,
            CommandPalette, FocusEditor,
            ToggleTheme
        );
        root.child(
            TitleBar::new().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(img(crate::app_icon::image()).size(px(20.)).flex_shrink_0())
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("not-bad-query"))
                    .when_some(title_project, |el, p| {
                        el.child(div().text_sm().text_color(theme.muted_foreground).child("—"))
                            .child(div().text_sm().text_color(theme.muted_foreground).child(p))
                    })
                    .when_some(title_conn, |el, c| {
                        let color = conn_color(&c, cx);
                        el.child(div().text_sm().text_color(theme.muted_foreground).child("/"))
                            .child(div().text_sm().text_color(color).font_weight(FontWeight::MEDIUM).child(c.name))
                    }),
            ),
        )
        .child(
            div().flex_1().overflow_hidden().child(
                h_resizable("main")
                    .child(
                        resizable_panel()
                            .size(px(270.))
                            .size_range(px(160.)..px(600.))
                            .child(
                                div()
                                    .size_full()
                                    .bg(theme.sidebar)
                                    .border_r_1()
                                    .border_color(theme.border)
                                    .child(self.sidebar.clone()),
                            )
                    )
                    .child(
                        resizable_panel()
                            .child(
                                v_flex()
                                    .size_full()
                                    .child(self.render_tabs(cx))
                                    .child(div().flex_1().when_some(active, |d, t| d.child(t))),
                            ),
                    ),
            ),
        )
        .child(self.render_status(cx))
    }
}

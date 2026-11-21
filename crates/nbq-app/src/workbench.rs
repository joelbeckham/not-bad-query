//! The window's root view: sidebar, tab strip, active tab, status bar.
//! Owns the engine, the open projects and app config, and routes events.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogEntrance, DialogFooter};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_core::watch::ProjectWatcher;
use nbq_core::{ConfigError, Connection, Credentials, Project, Settings, Store, Workspace};
use nbq_driver::vpn::Status as VpnStatus;
use nbq_engine::{ConnState, Engine, EngineOptions, Event, TabId};

use crate::actions::*;
use crate::complete::Catalogs;
use crate::query_tab::QueryTab;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::ui::{Projects, conn_color, connection, project_for_path};

pub struct Workbench {
    assistant_open: bool,
    engine: Arc<Engine>,
    store: Store,
    settings: Settings,
    workspace: Workspace,
    project_recovery: VecDeque<UnavailableProject>,
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
    vpn_status: Option<VpnStatus>,
    vpn_prompt_open: bool,
    closing: bool,
    csv_queue: VecDeque<PathBuf>,
    csv_target: Option<PathBuf>,
    palette: Entity<CommandState>,
    focus: FocusHandle,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

struct UnavailableProject {
    root: PathBuf,
    detail: Option<String>,
    locating: bool,
}

impl UnavailableProject {
    fn new(root: PathBuf, error: ConfigError) -> Self {
        // Project::open reports a missing marker as a project error. The
        // standard dialog text already covers that case.
        let detail = (!matches!(error, ConfigError::Project(_))).then(|| error.to_string());
        Self { root, detail, locating: false }
    }
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
        let workspace = store.load_workspace().unwrap_or_else(|e| {
            problems.push(e.to_string());
            Workspace::default()
        });
        Theme::change(if settings.dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);

        let mut watcher = ProjectWatcher::new().map_err(|e| problems.push(e.to_string())).ok();
        let mut projects = Vec::new();
        let mut project_recovery = VecDeque::new();
        for root in &workspace.open {
            match Project::open(root) {
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
                    project_recovery.push_back(UnavailableProject::new(root.clone(), e));
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
        let mut tasks = vec![events, watch];
        if cfg!(windows) {
            tasks.push(cx.spawn_in(window, async move |this, cx| {
                loop {
                    let status = cx.background_executor().spawn(async { nbq_driver::vpn::status() }).await;
                    if this.update_in(cx, |wb, window, cx| {
                        if wb.vpn_status != Some(status) {
                            wb.vpn_status = Some(status);
                            window.refresh();
                            cx.notify();
                        }
                    }).is_err() {
                        break;
                    }
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                }
            }));
        }
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let mut wb = Workbench {
            engine,
            store,
            settings,
            workspace,
            project_recovery,
            projects,
            watcher,
            sidebar,
            tabs: Vec::new(),
            active: 0,
            assistant_open: false,
            catalogs: Rc::new(RefCell::new(HashMap::new())),
            next_untitled: 1,
            close_armed: None,
            quit_dialog_open: false,
            vpn_status: None,
            vpn_prompt_open: false,
            closing: false,
            csv_queue: Default::default(),
            csv_target: None,
            palette,
            focus: cx.focus_handle(),
            _tasks: tasks,
            _subs: subs,
        };
        wb.new_tab(None, None, window, cx);
        wb.check_csv_sources(window, cx);
        // The component Root is attached by open_window only after this builder
        // returns. Notifications and dialogs need its WindowState.
        let owner = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            for problem in problems {
                error(window, cx, problem);
            }
            let _ = owner.update(cx, |wb, cx| {
                if !wb.project_recovery.is_empty() {
                    wb.show_project_recovery(window, cx);
                }
            });
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
                if crate::vpn::blocked(&state) && !self.sidebar.read(cx).is_disconnected(&conn) {
                    let profile = connection(&self.projects.borrow(), &conn).filter(|c| c.vpn_required);
                    if let Some(profile) = profile {
                        self.prompt_vpn(Some(profile), window, cx);
                    }
                }
                self.sidebar.update(cx, |s, cx| s.on_metadata_state(conn, state, cx));
                cx.notify();
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
                if matches!(&ev, Event::Conn { state, .. } if crate::vpn::blocked(state)) {
                    let profile = tab.read(cx).conn.clone().filter(|c| c.vpn_required);
                    if let Some(profile) = profile {
                        self.prompt_vpn(Some(profile), window, cx);
                    }
                }
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

    fn prompt_vpn(&mut self, profile: Option<Connection>, window: &mut Window, cx: &mut Context<Self>) {
        if self.vpn_prompt_open || self.closing {
            return;
        }
        self.vpn_prompt_open = true;
        let key = profile.as_ref().map(Connection::key);
        let profile = profile.map(|c| c.name);
        let (status, retry, owner) = (cx.weak_entity(), cx.weak_entity(), cx.weak_entity());
        crate::vpn::prompt(
            profile, window, cx,
            move |cx| status.upgrade().and_then(|wb| wb.read(cx).vpn_status),
            move |window, cx| {
                if let Some(key) = &key {
                    let _ = retry.update(cx, |wb, cx| {
                        wb.on_sidebar(&SidebarEvent::Connect(key.clone()), window, cx);
                    });
                }
            },
            move |cx| { let _ = owner.update(cx, |wb, _| wb.vpn_prompt_open = false); },
        );
    }

    fn reuse_empty_tab(&self, cx: &App) -> Option<Entity<QueryTab>> {
        self.active_tab().cloned().filter(|t| {
            let t = t.read(cx);
            t.path.is_none() && t.text(cx).trim().is_empty() && !t.is_running() && !t.txn.in_txn && !t.saving
                && !t.assistant.read(cx).in_use(cx)
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
            SidebarEvent::NewFolder(dir) => {
                use gpui_kit::component::input::{Input, InputState};
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
                let owner = cx.entity();
                let dir = dir.clone();
                let destination_label = self.projects.borrow().iter().find(|p| dir.starts_with(&p.root)).map(|p| format!("{} / {}", p.name(), dir.strip_prefix(&p.root).unwrap_or(&dir).display())).unwrap_or_else(|| dir.display().to_string());
                let first = std::cell::Cell::new(true);
                window.open_dialog(cx, move |dialog, window, cx| {
                    if first.replace(false) { input.read(cx).focus_handle(cx).focus(window, cx); }
                    let (input_ok, owner, dir) = (input.clone(), owner.clone(), dir.clone());
                    dialog.title("New folder")
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!("Create in {}", destination_label)))
                        .child(Input::new(&input))
                        .footer(DialogFooter::new()
                            .child(div().w(px(88.)).child(DialogClose::new().trigger(|button| button.outline().w_full().label("Cancel"))))
                            .child(div().w(px(88.)).child(DialogAction::new().child(Button::new("create-folder").primary().w_full().label("Create").disabled(input.read(cx).value().trim().is_empty())))))
                        .on_ok(move |_, window, cx| {
                            let name = input_ok.read(cx).value().trim().to_string();
                            if name.is_empty() || name == "." || name == ".." || name.ends_with(['.', ' ']) || name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
                                error(window, cx, "Enter a valid folder name."); return false;
                            }
                            let path = dir.join(name);
                            owner.update(cx, |wb, cx| wb.tree_operation(None, path, window, cx));
                            true
                        })
                });
            }
            SidebarEvent::Move { from, to } => self.tree_operation(Some(from.clone()), to.clone(), window, cx),
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
        self.refresh_projects(cx);
    }

    fn refresh_projects(&mut self, cx: &mut Context<Self>) {
        let recent = self.workspace.recent.clone();
        self.sidebar.update(cx, |s, cx| {
            s.set_recent(recent, cx);
            s.refresh(cx);
        });
        cx.notify();
    }

    fn add_project(&mut self, p: Project, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.opened(&p.root);
        self.attach_project(p, window, cx);
        self.save_workspace(window, cx);
    }

    fn attach_project(&mut self, p: Project, window: &mut Window, cx: &mut Context<Self>) {
        for w in &p.warnings {
            error(window, cx, w.clone());
        }
        if let Some(w) = self.watcher.as_mut() {
            if let Err(e) = w.watch(&p.root) {
                error(window, cx, e.to_string());
            }
        }
        let key = p.key;
        self.projects.borrow_mut().push(p);
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
            Err(e) if self.workspace.recent.contains(&root) || self.workspace.open.contains(&root) => {
                let show_dialog = self.project_recovery.is_empty();
                self.project_recovery.push_back(UnavailableProject::new(root, e));
                if show_dialog {
                    self.show_project_recovery(window, cx);
                }
            }
            Err(e) => error(window, cx, e.to_string()),
        }
    }

    fn show_project_recovery(&self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(workbench) = owner.upgrade() else {
                return dialog;
            };
            let Some(project) = workbench.read(cx).project_recovery.front() else {
                return dialog;
            };
            let theme = cx.theme();
            let locating = project.locating;
            let path = project.root.display().to_string();
            let title = project.root.file_name().map_or_else(
                || "Couldn’t open project".to_string(),
                |name| format!("Couldn’t open “{}”", name.to_string_lossy()),
            );
            let message = if project.detail.is_none() {
                "The saved folder or its project file couldn’t be found."
            } else {
                "Choose the project’s folder to try opening it again."
            };
            let (remove, locate, dismiss) = (owner.clone(), owner.clone(), owner.clone());
            dialog.title(div().pr_6().truncate().child(title))
                .entrance(DialogEntrance::None)
                .close_button(!locating)
                .overlay_closable(false)
                .child(
                    v_flex()
                        .gap_3()
                        .text_sm()
                        .child(div().text_color(theme.muted_foreground).child(message))
                        .child(
                            v_flex()
                                .gap_1()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .px_3()
                                .py_2()
                                .child(div().text_xs().text_color(theme.muted_foreground).child("Saved folder"))
                                .child(
                                    div().id("saved-project-folder")
                                        .font_family(theme.mono_font_family.clone())
                                        .text_xs()
                                        .truncate()
                                        .child(path.clone())
                                        .tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx)),
                                ),
                        )
                        .when_some(project.detail.clone(), |el, detail| {
                            el.child(div().text_color(theme.danger).child(detail))
                        })
                        .child(div().text_xs().text_color(theme.muted_foreground)
                            .child("Removing the workspace entry only forgets the saved path.")),
                )
                .footer(
                    DialogFooter::new()
                        .justify_between()
                        .child(
                            Button::new("remove-unavailable-project")
                                .ghost()
                                .text_color(theme.muted_foreground)
                                .label("Remove from workspace")
                                .disabled(locating)
                                .on_click(move |_, window, cx| {
                                    let _ = remove.update(cx, |wb, cx| wb.resolve_project(None, window, cx));
                                }),
                        )
                        .child(
                            h_flex().gap_2()
                                .child(div().w(px(80.)).child(DialogClose::new().trigger(move |button| {
                                    button.outline().w_full().label("Dismiss").disabled(locating).tooltip("Keep the saved path")
                                })))
                                .child(DialogAction::new().child(
                                    Button::new("locate-project")
                                        .primary()
                                        .label("Locate folder…")
                                        .loading(locating),
                                )),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let _ = locate.update(cx, |wb, cx| wb.locate_project(window, cx));
                    false
                })
                .on_cancel(move |_, window, cx| {
                    dismiss.update(cx, |wb, _| {
                        if wb.project_recovery.front().is_some_and(|project| project.locating) {
                            return false;
                        }
                        wb.project_recovery.pop_front();
                        window.refresh();
                        wb.project_recovery.is_empty()
                    }).unwrap_or(true)
                })
        });
    }

    fn locate_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project_recovery.front_mut() else {
            return;
        };
        if project.locating {
            return;
        }
        project.locating = true;
        window.refresh();
        let mut picker = rfd::AsyncFileDialog::new()
            .set_parent(window)
            .set_title("Locate not-bad-query project");
        if let Some(parent) = project.root.parent().filter(|path| !path.as_os_str().is_empty()) {
            picker = picker.set_directory(parent);
        }
        cx.spawn_in(window, async move |this, cx| {
            let opened = if let Some(dir) = picker.pick_folder().await {
                let path = dir.path().to_path_buf();
                Some(cx.background_executor().spawn(async move { Project::open(&path) }).await)
            } else {
                None
            };
            let _ = this.update_in(cx, |wb, window, cx| {
                let Some(project) = wb.project_recovery.front_mut() else {
                    return;
                };
                project.locating = false;
                match opened {
                    Some(Ok(p)) => wb.resolve_project(Some(p), window, cx),
                    Some(Err(e)) => project.detail = Some(e.to_string()),
                    None => {}
                }
                window.refresh();
            });
        }).detach();
    }

    fn resolve_project(
        &mut self,
        replacement: Option<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project_recovery.front_mut() else {
            return;
        };
        if project.locating || self.closing {
            return;
        }
        let mut workspace = self.workspace.clone();
        workspace.forget(&project.root);
        if let Some(p) = &replacement {
            workspace.opened(&p.root);
        }
        if let Err(e) = self.store.save_workspace(&workspace) {
            project.detail = Some(format!("Could not save workspace: {e}"));
            window.refresh();
            return;
        }
        self.workspace = workspace;
        let previous_root = project.root.clone();
        self.project_recovery.retain(|pending| {
            pending.root != previous_root && replacement.as_ref().is_none_or(|p| pending.root != p.root)
        });
        if let Some(p) = replacement {
            let already_open = self.projects.borrow().iter().any(|open| open.root == p.root);
            if !already_open {
                self.attach_project(p, window, cx);
                self.check_csv_sources(window, cx);
            }
        }
        self.refresh_projects(cx);
        if self.project_recovery.is_empty() {
            window.close_dialog(cx);
        }
        window.refresh();
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
        if a.is::<ToggleAssistant>() {
            self.assistant_open = !self.assistant_open;
            cx.notify();
        } else if a.is::<RunStatement>() {
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

    fn tree_operation(&mut self, from: Option<PathBuf>, to: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut project) = self.projects.borrow().iter().find(|p| to.starts_with(&p.root)).cloned() else { return; };
        if self.tabs.iter().any(|tab| tab.read(cx).saving) { error(window, cx, "Wait for the script save to finish before moving files."); return; }
        let original = from.clone();
        let destination = to.clone();
        let task = cx.background_executor().spawn(async move {
            let result = if let Some(from) = from { project.move_tree_entry(&from, &to).map_err(|e| e.to_string()) }
                else { std::fs::create_dir(&to).map_err(|e| e.to_string()).and_then(|_| project.reload().map_err(|e| e.to_string())) };
            (project, result)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (project, result) = task.await;
            let _ = this.update_in(cx, |wb, window, cx| {
                match result {
                    Err(e) => error(window, cx, &e),
                    Ok(()) => {
                        if let Some(p) = wb.projects.borrow_mut().iter_mut().find(|p| p.key == project.key) { *p = project; }
                        if let Some(from) = original {
                            for tab in &wb.tabs {
                                tab.update(cx, |t, cx| {
                                    if let Some(path) = t.path.as_ref().and_then(|p| p.strip_prefix(&from).ok()).map(|tail| destination.join(tail)) { t.set_path(path, cx); }
                                    if let Some(path) = t.save_dir.as_ref().and_then(|p| p.strip_prefix(&from).ok()).map(|tail| destination.join(tail)) { t.save_dir = Some(path); }
                                });
                            }
                        }
                        wb.sidebar.update(cx, |s, cx| s.refresh(cx));
                        cx.notify();
                    }
                }
            });
        }).detach();
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

    fn rename_tab(&mut self, tab: Entity<QueryTab>, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::input::{Input, InputState};
        if tab.read(cx).saving { return; }
        let saved = tab.read(cx).path.is_some();
        let directory = tab.read(cx).path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| tab.read(cx).save_dir.clone()).or_else(|| self.project_scripts_dir(tab.read(cx).project));
        if directory.is_none() { error(window, cx, "Open a project before renaming an unsaved tab."); return; }
        let description = format!("{} {}", if saved { "Rename in" } else { "Save SQL script in" }, directory.unwrap().display());
        let input = cx.new(|cx| InputState::new(window, cx).default_value(tab.read(cx).title()));
        let owner = cx.entity();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) { input.read(cx).focus_handle(cx).focus(window, cx); }
            let (input_ok, owner, tab) = (input.clone(), owner.clone(), tab.clone());
            dialog.title(if saved { "Rename script" } else { "Save script" })
                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(description.clone()))
                .child(Input::new(&input))
                .footer(DialogFooter::new()
                    .child(div().w(px(88.)).child(DialogClose::new().trigger(|b| b.outline().w_full().label("Cancel"))))
                    .child(div().w(px(88.)).child(DialogAction::new().child(Button::new("rename-tab").primary().w_full().label(if saved { "Rename" } else { "Save" }).disabled(input.read(cx).value().trim().is_empty())))))
                .on_ok(move |_, window, cx| {
                    let name = input_ok.read(cx).value().trim().to_string();
                    if name.is_empty() || name == "." || name == ".." || name.ends_with('.') || name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
                        error(window, cx, "Enter a valid file name."); return false;
                    }
                    owner.update(cx, |wb, cx| {
                        let t = tab.read(cx);
                        if t.saving { return; }
                        if let Some(from) = t.path.clone() {
                            let mut to = from.with_file_name(&name);
                            if to.extension().is_none() { if let Some(ext) = from.extension() { to.set_extension(ext); } }
                            if from != to {
                                if project_for_path(&wb.projects.borrow(), &from).is_some() {
                                    wb.tree_operation(Some(from), to, window, cx);
                                } else {
                                    tab.update(cx, |t, _| t.saving = true);
                                    let tab = tab.clone();
                                    cx.spawn_in(window, async move |this, cx| {
                                        let destination = to.clone();
                                        let result = cx.background_executor().spawn(async move {
                                            // Windows rename refuses to replace an existing file.
                                            std::fs::rename(&from, &destination)
                                        }).await;
                                        let _ = this.update_in(cx, |_, window, cx| {
                                            tab.update(cx, |t, cx| {
                                                t.saving = false;
                                                match result {
                                                    Ok(()) => t.set_path(to, cx),
                                                    Err(e) => error(window, cx, format!("Rename failed: {e}")),
                                                }
                                            });
                                            cx.notify();
                                        });
                                    }).detach();
                                }
                            }
                        } else {
                            let dir = t.save_dir.clone().or_else(|| wb.project_scripts_dir(t.project));
                            if let Some(dir) = dir {
                                let name = if Path::new(&name).extension().is_none() { format!("{name}.sql") } else { name };
                                let text = t.text(cx);
                                wb.save_script(tab.clone(), dir.join(name), text, None, window, cx);
                            } else { error(window, cx, "Open a project before renaming an unsaved tab."); }
                        }
                    });
                    true
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
                let tab = t.clone();
                let t = t.read(cx);
                let color = t.conn.as_ref().map(|c| conn_color(c, cx)).unwrap_or(theme.muted_foreground.opacity(0.5));
                let indicator = if t.is_running() {
                    div().id(("running-query-tab", t.engine_tab.0)).child(
                        Spinner::new().icon(IconName::LoaderCircle).xsmall().color(theme.warning).ease(|t| t),
                    ).into_any_element()
                } else {
                    div().size(px(8.)).rounded_full().bg(color).into_any_element()
                };
                let pending = t.txn.in_txn && t.txn.pending_writes > 0;
                Tab::new()
                    .prefix(h_flex().pl_2().child(h_flex().w_3().justify_center().child(indicator)))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().id(("tab-name", ix)).child(t.title()).on_click(cx.listener({
                                let tab = tab.clone();
                                move |wb, ev: &ClickEvent, window, cx| {
                                    if ev.click_count() == 2 { cx.stop_propagation(); wb.rename_tab(tab.clone(), window, cx); }
                                }
                            }))
                    .context_menu({
                        let owner = cx.entity();
                        move |menu, _, _| {
                            let (owner, tab) = (owner.clone(), tab.clone());
                            menu.item(gpui_kit::component::menu::PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                                owner.update(cx, |wb, cx| wb.rename_tab(tab.clone(), window, cx));
                            }))
                        }
                    }))
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
        let show_vpn = cfg!(windows) && (matches!(self.vpn_status, Some(VpnStatus::Connected | VpnStatus::Disconnected))
            || self.projects.borrow().iter().any(|p| p.connections.connections.iter().any(|c| c.vpn_required)));
        let vpn_needed = self.active_tab().and_then(|t| t.read(cx).conn.as_ref()).is_some_and(|c| c.vpn_required);
        let left = self.active_tab().map(|t| {
            let t = t.read(cx);
            match &t.conn {
                Some(c) => {
                    let profile_state = self.sidebar.read(cx).connection_state(&c.key());
                    let state = match &profile_state {
                        ConnState::Connected => ("connected", theme.success),
                        ConnState::Connecting => ("connecting…", theme.warning),
                        state if crate::vpn::blocked(state) => ("VPN required", theme.warning),
                        ConnState::Failed(_) => ("failed", theme.danger),
                        ConnState::Disconnected => ("disconnected", theme.muted_foreground),
                    };
                    let detail = match &t.conn_state {
                        ConnState::Disconnected if matches!(profile_state, ConnState::Connected) =>
                            "Connected. This tab's query session is idle; running a query opens it.".to_string(),
                        ConnState::Disconnected => "Run a query to connect.".to_string(),
                        ConnState::Connecting => "Query session connecting…".to_string(),
                        ConnState::Connected => "Query session connected.".to_string(),
                        ConnState::Failed(e) => format!("Query session failed: {e}"),
                    };
                    let detail = match &profile_state {
                        ConnState::Failed(e) => format!("Connection error: {e}\n{detail}"),
                        _ => detail,
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size(px(8.)).rounded_full().bg(state.1))
                        .child(div().text_color(theme.foreground).child(c.name.clone()))
                        .child(if c.kind.is_file_based() { c.file_path.display().to_string() } else { format!("{}:{}/{}", c.host, c.port, c.database) })
                        .child(div().id("profile-status").text_color(state.1).child(state.0)
                            .tooltip(move |window, cx| Tooltip::new(detail.clone()).text_xs().mb(px(18.)).build(window, cx)))
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
                    .when(show_vpn, |row| {
                        let offline = if vpn_needed { theme.warning } else { theme.muted_foreground };
                        let (icon, label, color) = match self.vpn_status {
                            Some(VpnStatus::Connected) => (Lucide::ShieldCheck, "VPN connected", theme.success),
                            Some(VpnStatus::Disconnected) => (Lucide::ShieldOff, "VPN disconnected", offline),
                            Some(VpnStatus::Unavailable) => (Lucide::ShieldAlert, "VPN unavailable", offline),
                            None => (Lucide::Shield, "Checking VPN…", theme.muted_foreground),
                        };
                        let can_open = matches!(self.vpn_status, Some(VpnStatus::Disconnected | VpnStatus::Unavailable));
                        row.child(
                            Button::new("aws-vpn-status")
                                .xsmall()
                                .ghost()
                                .icon(Icon::new(icon))
                                .label(label)
                                .text_color(color)
                                .tooltip(if can_open { "Open VPN status" } else { "AWS Client VPN" })
                                .tab_stop(can_open)
                                .when(can_open, |button| button.on_click(cx.listener(|this, _, window, cx| {
                                    this.prompt_vpn(None, window, cx);
                                }))),
                        )
                    })
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
            ToggleTheme, ToggleAssistant
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
                                    .child(div().flex_1().when_some(active.clone(), |d, t| d.child(t))),
                            ),
                    ).when_some(active.filter(|_| self.assistant_open), |row, t| row.child(
                        resizable_panel().size(px(350.)).size_range(px(260.)..px(650.))
                            .child(t.read(cx).assistant.clone()))),
            ),
        )
        .child(self.render_status(cx))
    }
}

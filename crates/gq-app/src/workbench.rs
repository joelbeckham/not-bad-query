//! The window's root view: sidebar, tab strip, active tab, status bar.
//! Owns the engine and the config, and routes engine events.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gq_core::{Connection, Connections, Settings, Store};
use gq_engine::{ConnState, Engine, EngineOptions, Event, TabId};

use crate::actions::*;
use crate::complete::Catalogs;
use crate::query_tab::QueryTab;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::ui::conn_color;

pub struct Workbench {
    engine: Arc<Engine>,
    store: Store,
    connections: Connections,
    /// Shared with tabs for their connection pickers.
    conn_list: Rc<RefCell<Vec<Connection>>>,
    settings: Settings,
    sidebar: Entity<Sidebar>,
    tabs: Vec<Entity<QueryTab>>,
    active: usize,
    catalogs: Catalogs,
    next_untitled: usize,
    /// A close that needs confirming (second Ctrl+W).
    close_armed: Option<EntityId>,
    palette: Entity<CommandState>,
    focus: FocusHandle,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

fn event_tab(ev: &Event) -> Option<TabId> {
    Some(match ev {
        Event::Conn { tab, .. }
        | Event::Txn { tab, .. }
        | Event::StatementStarted { tab, .. }
        | Event::ResultOpened { tab, .. }
        | Event::StatementFinished { tab, .. }
        | Event::Fetch { tab, .. }
        | Event::QueryDone { tab, .. }
        | Event::Export { tab, .. } => *tab,
        Event::Objects { .. } | Event::Catalog { .. } => return None,
    })
}

impl Workbench {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = match std::env::var_os("GQ_CONFIG_DIR") {
            Some(d) => Store::new(PathBuf::from(d)),
            None => Store::default_location(),
        };
        let mut load_errors = Vec::new();
        let settings = store.load_settings().unwrap_or_else(|e| {
            load_errors.push(e.to_string());
            Settings::default()
        });
        let connections = store.load_connections().unwrap_or_else(|e| {
            load_errors.push(e.to_string());
            Connections::default()
        });
        Theme::change(if settings.dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);

        let mut opts = EngineOptions::new(Store::default_spool_dir());
        opts.initial_rows = settings.initial_rows;
        let (engine, mut rx) = Engine::new(opts);

        let sidebar =
            cx.new(|cx| Sidebar::new(engine.clone(), connections.clone(), settings.scripts_dir.clone(), window, cx));
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
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let mut wb = Workbench {
            engine,
            store,
            conn_list: Rc::new(RefCell::new(connections.connections.clone())),
            connections,
            settings,
            sidebar,
            tabs: Vec::new(),
            active: 0,
            catalogs: Rc::new(RefCell::new(HashMap::new())),
            next_untitled: 1,
            close_armed: None,
            palette,
            focus: cx.focus_handle(),
            _tasks: vec![events],
            _subs: subs,
        };
        let first = wb.connections.connections.first().cloned();
        wb.new_tab(first, window, cx);
        for e in load_errors {
            window.push_notification(Notification::error(format!("Config error: {e}")), cx);
        }
        wb
    }

    fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active)
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

    pub fn new_tab(&mut self, conn: Option<Connection>, window: &mut Window, cx: &mut Context<Self>) -> Entity<QueryTab> {
        let n = self.next_untitled;
        self.next_untitled += 1;
        let (engine, catalogs, settings, list) =
            (self.engine.clone(), self.catalogs.clone(), self.settings.clone(), self.conn_list.clone());
        let tab = cx.new(|cx| {
            let mut t = QueryTab::new(engine, catalogs, list, n, &settings, window, cx);
            if conn.is_some() {
                t.set_connection(conn, cx);
            }
            t
        });
        // Tabs ask the workbench to load the catalog when they connect.
        self.tabs.push(tab.clone());
        self.active = self.tabs.len() - 1;
        self.focus_editor(window, cx);
        cx.notify();
        tab
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else { return };
        let (dirty, pending, id) = {
            let t = tab.read(cx);
            (t.dirty && !t.text(cx).trim().is_empty(), t.txn.in_txn && t.txn.pending_writes > 0, tab.entity_id())
        };
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
            let conn = tab.read(cx).conn.clone();
            self.new_tab(conn, window, cx);
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
        match ev {
            Event::Objects { conn, path, items } => {
                self.sidebar.update(cx, |s, cx| s.on_objects(conn, path, items, cx));
            }
            Event::Catalog { conn, tables } => match tables {
                Ok(t) => {
                    self.catalogs.borrow_mut().insert(conn, t);
                }
                Err(e) => tracing::warn!("catalog for {conn}: {e}"),
            },
            ev => {
                let Some(tab_id) = event_tab(&ev) else { return };
                let Some(tab) = self.tabs.iter().find(|t| t.read(cx).engine_tab == tab_id).cloned() else { return };
                if let Event::Conn { state: ConnState::Connected, .. } = &ev {
                    if let Some(c) = tab.read(cx).conn.clone() {
                        if !self.catalogs.borrow().contains_key(&c.id) {
                            self.engine.load_catalog(&c.id, &crate::ui::params_for(&c));
                        }
                    }
                }
                tab.update(cx, |t, cx| t.on_event(ev, window, cx));
                cx.notify();
            }
        }
    }

    fn on_sidebar(&mut self, ev: &SidebarEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            SidebarEvent::OpenConnection(id) => {
                let c = self.connections.get(id).cloned();
                // Reuse an empty untitled tab without a connection.
                if let Some(t) = self.active_tab().cloned() {
                    let reuse = {
                        let t = t.read(cx);
                        t.path.is_none() && t.text(cx).trim().is_empty() && !t.is_running()
                    };
                    if reuse {
                        t.update(cx, |t, cx| t.set_connection(c, cx));
                        self.focus_editor(window, cx);
                        return;
                    }
                }
                self.new_tab(c, window, cx);
            }
            SidebarEvent::OpenScript(p) => self.open_path(p.clone(), window, cx),
            SidebarEvent::NewConnection => self.edit_connection(None, window, cx),
            SidebarEvent::EditConnection(id) => {
                let c = self.connections.get(id).cloned();
                self.edit_connection(c, window, cx)
            }
            SidebarEvent::DeleteConnection(id) => {
                self.connections.remove(id);
                let _ = gq_core::secrets::delete(id);
                self.save_connections(window, cx);
            }
            SidebarEvent::SelectFrom { conn, schema, name } => {
                let c = self.connections.get(conn).cloned();
                let q = |s: &str| {
                    if s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                        s.to_string()
                    } else {
                        format!("\"{}\"", s.replace('"', "\"\""))
                    }
                };
                let sql = format!("select *\nfrom {}.{}", q(schema), q(name));
                let tab = self.new_tab(c, window, cx);
                tab.update(cx, |t, cx| {
                    t.editor.update(cx, |e, cx| e.set_value(sql.as_str(), window, cx));
                    t.run_statement(cx);
                });
            }
            SidebarEvent::ChooseScriptsDir => {
                cx.spawn_in(window, async move |this, cx| {
                    let Some(dir) = rfd::AsyncFileDialog::new().pick_folder().await else { return };
                    let dir = dir.path().to_path_buf();
                    let _ = this.update_in(cx, |wb, window, cx| {
                        wb.settings.scripts_dir = Some(dir.clone());
                        if let Err(e) = wb.store.save_settings(&wb.settings) {
                            window.push_notification(Notification::error(e.to_string()), cx);
                        }
                        wb.sidebar.update(cx, |s, cx| s.set_scripts_dir(Some(dir), cx));
                    });
                })
                .detach();
            }
        }
    }

    fn save_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.store.save_connections(&self.connections) {
            window.push_notification(Notification::error(format!("Could not save connections: {e}")), cx);
        }
        *self.conn_list.borrow_mut() = self.connections.connections.clone();
        let c = self.connections.clone();
        self.sidebar.update(cx, |s, cx| s.set_connections(c, cx));
        cx.notify();
    }

    fn edit_connection(&mut self, existing: Option<Connection>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        crate::conn_dialog::open(existing, window, cx, move |c, password, window, cx| {
            let _ = this.update(cx, |wb, cx| {
                if let Some(pw) = password.filter(|_| c.save_password) {
                    if let Err(e) = gq_core::secrets::set(&c.id, &pw) {
                        window.push_notification(Notification::error(format!("Could not store password: {e}")), cx);
                    }
                }
                if !c.save_password {
                    let _ = gq_core::secrets::delete(&c.id);
                }
                wb.engine.forget_connection(&c.id);
                wb.catalogs.borrow_mut().remove(&c.id);
                wb.connections.upsert(c.clone());
                wb.save_connections(window, cx);
                // Tabs on this connection reconnect with the new settings.
                for t in wb.tabs.clone() {
                    if t.read(cx).conn.as_ref().is_some_and(|x| x.id == c.id) {
                        t.update(cx, |t, cx| t.set_connection(Some(c.clone()), cx));
                    }
                }
                // A brand-new connection: put it on the empty tab.
                if let Some(t) = wb.active_tab().cloned() {
                    if t.read(cx).conn.is_none() {
                        t.update(cx, |t, cx| t.set_connection(Some(c.clone()), cx));
                    }
                }
            });
        });
    }

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| t.read(cx).path.as_ref() == Some(&path)) {
            self.active = ix;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                window.push_notification(Notification::error(format!("{}: {e}", path.display())), cx);
                return;
            }
        };
        let conn = self.active_tab().and_then(|t| t.read(cx).conn.clone());
        // Replace an empty untitled tab rather than piling up tabs.
        let reuse = self.active_tab().cloned().filter(|t| {
            let t = t.read(cx);
            t.path.is_none() && t.text(cx).trim().is_empty() && !t.is_running()
        });
        let tab = match reuse {
            Some(t) => t,
            None => self.new_tab(conn, window, cx),
        };
        tab.update(cx, |t, cx| t.load_file(path, &text, window, cx));
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn save(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        let (path, text, title) = {
            let t = tab.read(cx);
            (t.path.clone(), t.text(cx), t.title())
        };
        if let (Some(p), false) = (path.clone(), save_as) {
            match std::fs::write(&p, text) {
                Ok(()) => tab.update(cx, |t, cx| t.mark_saved(p, cx)),
                Err(e) => window.push_notification(Notification::error(format!("Save failed: {e}")), cx),
            }
            return;
        }
        let dir = path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf())).or(self.settings.scripts_dir.clone());
        let name = if title.ends_with(".sql") { title.to_string() } else { format!("{title}.sql") };
        cx.spawn_in(window, async move |this, cx| {
            let mut d = rfd::AsyncFileDialog::new().set_file_name(&name).add_filter("SQL", &["sql"]);
            if let Some(dir) = dir {
                d = d.set_directory(dir);
            }
            let Some(f) = d.save_file().await else { return };
            let p = f.path().to_path_buf();
            let _ = this.update_in(cx, |wb, window, cx| match std::fs::write(&p, &text) {
                Ok(()) => {
                    tab.update(cx, |t, cx| t.mark_saved(p, cx));
                    let dir = wb.settings.scripts_dir.clone();
                    wb.sidebar.update(cx, |s, cx| s.set_scripts_dir(dir, cx));
                }
                Err(e) => window.push_notification(Notification::error(format!("Save failed: {e}")), cx),
            });
        })
        .detach();
    }

    fn open_dialog_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.settings.scripts_dir.clone();
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
            let conn = self.active_tab().and_then(|t| t.read(cx).conn.clone());
            self.new_tab(conn, window, cx);
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
        } else if a.is::<OpenFile>() {
            self.open_dialog_file(window, cx);
        } else if a.is::<ExportCsv>() {
            if let Some(t) = self.active_tab().cloned() {
                t.update(cx, |t, cx| t.export_csv(window, cx));
            }
        } else if a.is::<NewConnection>() {
            self.edit_connection(None, window, cx);
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
                        ConnState::Disconnected => ("idle", theme.muted_foreground),
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size(px(8.)).rounded_full().bg(color))
                        .child(div().text_color(theme.foreground).child(c.name.clone()))
                        .child(format!("{}@{}:{}/{}", c.user, c.host, c.port, c.database))
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
        let theme = cx.theme().clone();
        let active = self.active_tab().cloned();
        let title_conn = active.as_ref().and_then(|t| t.read(cx).conn.clone());
        let root = v_flex()
            .id("workbench")
            .key_context("Workbench")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground);
        let root = bind!(
            root, cx, RunStatement, RunScript, CancelQuery, Commit, Rollback, ToggleAutocommit, NewTab, CloseTab,
            NextTab, PrevTab, SaveFile, SaveFileAs, OpenFile, ExportCsv, NewConnection, CommandPalette, FocusEditor,
            ToggleTheme
        );
        root.child(
            TitleBar::new().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(Lucide::Database).small().text_color(theme.primary))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("good-query"))
                    .when_some(title_conn, |el, c| {
                        let color = conn_color(&c, cx);
                        el.child(div().text_sm().text_color(theme.muted_foreground).child("—"))
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

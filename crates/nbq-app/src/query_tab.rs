//! One query tab: editor + results, bound to an engine tab (its own session).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::*;
use nbq_core::{Connection, Settings};
use nbq_driver::DriverError;
use nbq_engine::{ConnState, CsvOptions, Engine, Event, Outcome, Progress, QueryId, SpoolStatus, TabId, TxnState};
use nbq_sql::{SplitOptions, split, statement_at};

use crate::complete::{Catalogs, SqlCompleter};
use crate::grid::ResultGrid;
use crate::ui::{Projects, conn_color, fmt_count, fmt_elapsed};

pub enum Status {
    Idle,
    Running,
    Rows { elapsed: Duration },
    Done { message: String, elapsed: Duration },
    Failed { error: DriverError, elapsed: Duration },
    Cancelled { elapsed: Duration },
}

pub struct QueryTab {
    pub engine_tab: TabId,
    engine: Arc<Engine>,
    pub conn: Option<Connection>,
    projects: Projects,
    /// The project this tab belongs to (its script and connection both come from it).
    pub project: Option<u32>,
    /// Where Save first suggests putting an untitled script.
    pub save_dir: Option<PathBuf>,
    completer_conn: Rc<RefCell<Option<String>>>,
    pub editor: Entity<EditorState>,
    pub grid: Entity<TableState<ResultGrid>>,
    pub path: Option<PathBuf>,
    untitled: usize,
    pub dirty: bool,
    pub saved_text: Option<String>,
    pub saving: bool,
    pub conn_state: ConnState,
    pub session_enabled: bool,
    pub txn: TxnState,
    /// (query, started, statements in the batch)
    running: Option<(QueryId, Instant)>,
    current: Option<QueryId>,
    status: Status,
    fetch: Option<(Progress, bool)>,
    export: Option<(PathBuf, u64, Option<Result<(), String>>)>,
    blank_line_delimits: bool,
    csv: CsvOptions,
    _ticker: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl QueryTab {
    pub fn new(
        engine: Arc<Engine>,
        catalogs: Catalogs,
        projects: Projects,
        untitled: usize,
        settings: &Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let engine_tab = engine.open_tab();
        let completer_conn = Rc::new(RefCell::new(None));
        let editor = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).language("sql").line_number(true).placeholder("select …");
            s.lsp_mut().completion_provider =
                Some(Rc::new(SqlCompleter { catalogs, conn: completer_conn.clone() }));
            s
        });
        let grid = cx.new(|cx| TableState::new(ResultGrid::new(engine.clone(), engine_tab), window, cx).cell_selectable(true));
        let subs = vec![cx.subscribe(&editor, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) && !this.dirty {
                this.dirty = true;
                cx.notify();
            }
        })];
        QueryTab {
            engine_tab,
            engine,
            conn: None,
            projects,
            project: None,
            save_dir: None,
            completer_conn,
            editor,
            grid,
            path: None,
            untitled,
            dirty: false,
            saved_text: None,
            saving: false,
            conn_state: ConnState::Disconnected,
            session_enabled: false,
            txn: TxnState { autocommit: true, in_txn: false, pending_writes: 0, since: None },
            running: None,
            current: None,
            status: Status::Idle,
            fetch: None,
            export: None,
            blank_line_delimits: settings.blank_line_delimits,
            csv: CsvOptions {
                delimiter: settings.csv_delimiter as u8,
                header: true,
                null: settings.csv_null.clone(),
            },
            _ticker: None,
            _subs: subs,
        }
    }

    pub fn title(&self) -> SharedString {
        match &self.path {
            Some(p) => p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default().into(),
            None => format!("untitled-{}", self.untitled).into(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn set_connection(&mut self, conn: Option<Connection>, cx: &mut Context<Self>) {
        if self.conn.is_some() && (self.txn.in_txn || self.is_running()) {
            self.status = Status::Failed {
                error: DriverError::connection("Finish the query and commit or roll back before changing connections."),
                elapsed: Duration::ZERO,
            };
            cx.notify();
            return;
        }
        let params = conn.as_ref().map(crate::ui::params_for);
        let autocommit = conn.as_ref().map(|c| c.autocommit).unwrap_or(true);
        *self.completer_conn.borrow_mut() = conn.as_ref().map(|c| c.key());
        self.engine.set_connection(self.engine_tab, params, autocommit);
        if let Some(c) = &conn {
            self.project = Some(c.project_key);
        }
        self.conn = conn;
        self.session_enabled = self.conn.is_some();
        self.conn_state = if self.session_enabled { ConnState::Connecting } else { ConnState::Disconnected };
        self.remember_connection();
        cx.notify();
    }

    /// Keep the script and its selected profile, but release its live session.
    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if self.txn.in_txn || self.is_running() { return; }
        self.session_enabled = false;
        self.conn_state = ConnState::Disconnected;
        self.engine.set_connection(self.engine_tab, None, self.txn.autocommit);
        cx.notify();
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = &self.conn {
            self.session_enabled = true;
            self.conn_state = ConnState::Connecting;
            self.engine.set_connection(self.engine_tab, Some(crate::ui::params_for(c)), self.txn.autocommit);
            cx.notify();
        }
    }

    /// Record this script's connection in the project's local state, so
    /// reopening the script reconnects to the same database.
    fn remember_connection(&self) {
        let (Some(path), Some(c)) = (&self.path, &self.conn) else { return };
        let mut ps = self.projects.borrow_mut();
        if let Some(p) = ps.iter_mut().find(|p| p.key == c.project_key) {
            if let Err(e) = p.set_script_connection(path, Some(&c.id)) {
                tracing::warn!("remember script connection: {e}");
            }
        }
    }

    /// The tab's file moved to `path` (Save As, or opened into this tab):
    /// adopt that file's project, dropping a connection from another one.
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let project = crate::ui::project_for_path(&self.projects.borrow(), &path);
        self.path = Some(path);
        if project.is_some() || self.conn.is_none() {
            self.project = project;
        }
        if self.conn.as_ref().is_some_and(|c| Some(c.project_key) != self.project) {
            self.set_connection(None, cx);
        }
        self.remember_connection();
        cx.notify();
    }

    fn split_opts(&self) -> SplitOptions {
        let dialect = self.conn.as_ref().map(|c| c.kind.dialect()).unwrap_or(nbq_sql::Dialect::Postgres);
        SplitOptions { dialect, blank_line_delimits: self.blank_line_delimits }
    }

    fn start(&mut self, statements: Vec<String>, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.dismiss_lsp_overlays(cx));
        if statements.is_empty() {
            return;
        }
        if self.conn.is_none() {
            self.status = Status::Failed {
                error: DriverError::connection("Pick a connection for this tab first (the chip at top left)."),
                elapsed: Duration::ZERO,
            };
            cx.notify();
            return;
        }
        if !self.session_enabled { self.reconnect(cx); }
        let q = self.engine.execute(self.engine_tab, statements);
        self.running = Some((q, Instant::now()));
        self.current = Some(q);
        self.export = None;
        self.status = Status::Running;
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let running = this.update(cx, |t, cx| {
                    cx.notify();
                    t.running.is_some()
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Run the selection, or the statement under the cursor.
    pub fn run_statement(&mut self, cx: &mut Context<Self>) {
        let (text, sel, cursor) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.selected_range(), e.cursor())
        };
        let opts = self.split_opts();
        let stmts: Vec<String> = if sel.start != sel.end {
            let s = &text[sel.clone()];
            split(s, opts).iter().map(|st| st.text(s).to_string()).collect()
        } else {
            statement_at(&text, cursor, opts).map(|st| vec![st.text(&text).to_string()]).unwrap_or_default()
        };
        self.start(stmts, cx);
    }

    pub fn run_script(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let stmts = split(&text, self.split_opts()).iter().map(|s| s.text(&text).to_string()).collect();
        self.start(stmts, cx);
    }

    pub fn cancel(&mut self) {
        self.engine.cancel(self.engine_tab);
    }

    pub fn commit(&mut self) {
        self.engine.commit(self.engine_tab);
    }

    pub fn rollback(&mut self) {
        self.engine.rollback(self.engine_tab);
    }

    pub fn toggle_autocommit(&mut self) {
        if self.txn.in_txn || self.is_running() {
            return;
        }
        self.engine.set_autocommit(self.engine_tab, !self.txn.autocommit);
    }

    pub fn copy_cell(&self, cx: &mut Context<Self>) {
        let grid = self.grid.read(cx);
        let (Some((row, col)), Some(spool)) = (grid.selected_cell(), grid.delegate().spool_for_export()) else { return };
        let task = cx.background_executor().spawn(async move {
            let batches = spool.read(row as u64..row as u64 + 1).map_err(|e| e.to_string())?;
            let mut text = String::new();
            if let Some(batch) = batches.first() {
                nbq_driver::display::cell_text(batch.column(col), 0, &mut text);
            }
            Ok::<_, String>(text)
        });
        cx.spawn(async move |_, cx| {
            if let Ok(text) = task.await {
                let _ = cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
            }
        }).detach();
    }

    pub fn export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(q), Some(spool)) = (self.current, self.grid.read(cx).delegate().spool_for_export()) else {
            window.push_notification("No result to export", cx);
            return;
        };
        let opts = self.csv.clone();
        let engine = self.engine.clone();
        let tab = self.engine_tab;
        let name = crate::ui::export_file_name(&self.title(), chrono::Local::now().naive_local());
        // Default to the project's exports/ folder (git-ignored).
        let dir = self.project.and_then(|k| crate::ui::project(&self.projects.borrow(), k).map(|p| p.exports_dir()));
        if let Some(d) = &dir {
            let _ = std::fs::create_dir_all(d);
        }
        cx.spawn(async move |this, cx| {
            let mut d = rfd::AsyncFileDialog::new().set_file_name(&name).add_filter("CSV", &["csv"]);
            if let Some(dir) = dir {
                d = d.set_directory(dir);
            }
            let Some(file) = d.save_file().await else { return };
            let path = file.path().to_path_buf();
            let _ = this.update(cx, |t, cx| {
                t.export = Some((path.clone(), 0, None));
                cx.notify();
            });
            engine.export_csv(tab, q, spool, path, opts);
        })
        .detach();
    }

    /// Show a file in this tab, on `conn` (which the caller picked from the
    /// file's project).
    pub fn load_file(
        &mut self,
        path: PathBuf,
        text: &str,
        conn: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |e, cx| e.set_value(text, window, cx));
        self.project = crate::ui::project_for_path(&self.projects.borrow(), &path);
        self.path = Some(path);
        self.saved_text = Some(text.to_string());
        self.dirty = false;
        if self.conn.as_ref().map(|c| c.key()) != conn.as_ref().map(|c| c.key()) {
            self.set_connection(conn, cx);
        }
        cx.notify();
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    pub fn mark_saved(&mut self, path: PathBuf, text: String, cx: &mut Context<Self>) {
        self.dirty = self.text(cx) != text;
        self.saved_text = Some(text);
        if self.path.as_ref() != Some(&path) {
            self.set_path(path, cx);
        }
        cx.notify();
    }

    pub fn on_event(&mut self, ev: Event, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            Event::Conn { state, .. } => {
                if !self.session_enabled && !matches!(state, ConnState::Disconnected) { return; }
                if let ConnState::Failed(e) = &state {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(format!("Connection failed: {e}")),
                        cx,
                    );
                }
                self.conn_state = state;
            }
            Event::Txn { state, .. } => self.txn = state,
            Event::StatementStarted { .. } => {}
            Event::ResultOpened { query, spool, .. } => {
                self.fetch = None;
                self.grid.update(cx, |g, cx| {
                    let visible_rows = g.visible_range().rows().len().max(1);
                    let visible_cols = g.visible_range().cols().clone();
                    g.delegate_mut().set_result(query, spool);
                    g.refresh(cx);
                    g.scroll_to_row(0, cx);
                    g.delegate_mut().prepare_preview(0..visible_rows, visible_cols, cx);
                    cx.notify();
                });
            }
            Event::StatementFinished { query, elapsed, outcome, .. } => {
                if Some(query) != self.current {
                    return;
                }
                self.status = match outcome {
                    Outcome::Rows => Status::Rows { elapsed },
                    Outcome::Done { rows_affected, command } => {
                        let message = match rows_affected {
                            Some(n) if !command.starts_with("CREATE") && !command.starts_with("DROP")
                                && !command.starts_with("ALTER") && !command.starts_with("SET")
                                && !command.starts_with("BEGIN") && !command.starts_with("COMMIT")
                                && !command.starts_with("ROLLBACK") => {
                                format!("{command} · {} row{} affected", fmt_count(n), if n == 1 { "" } else { "s" })
                            }
                            _ => format!("{command} · ok"),
                        };
                        Status::Done { message, elapsed }
                    }
                    Outcome::Failed(error) => Status::Failed { error, elapsed },
                    Outcome::Cancelled => Status::Cancelled { elapsed },
                };
            }
            Event::Fetch { query, progress, paused, .. } => {
                if Some(query) == self.current {
                    self.fetch = Some((progress, paused));
                    self.grid.update(cx, |g, cx| {
                        if g.delegate_mut().refresh_rows() {
                            let rows = g.visible_range().rows().clone();
                            let cols = g.visible_range().cols().clone();
                            g.delegate_mut().prepare_preview(rows, cols, cx);
                            cx.notify();
                        }
                    });
                }
            }
            Event::QueryDone { query, .. } => {
                if self.running.is_some_and(|(q, _)| q == query) {
                    self.running = None;
                }
            }
            Event::Export { rows, done, path, .. } => {
                if let Some(Err(e)) = &done {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(format!("Export failed: {e}")),
                        cx,
                    );
                } else if done.is_some() {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::success(format!(
                            "Exported {} rows to {}",
                            fmt_count(rows),
                            path.display()
                        )),
                        cx,
                    );
                }
                self.export = Some((path, rows, done));
            }
            Event::Objects { .. } | Event::Catalog { .. } | Event::MetadataConn { .. } => {}
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let color = self.conn.as_ref().map(|c| conn_color(c, cx)).unwrap_or(theme.muted_foreground);
        let conn_label: SharedString =
            self.conn.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| "No connection".into()).into();
        let conn_icon = match &self.conn_state {
            ConnState::Connected => Icon::new(Lucide::Plug).text_color(color),
            ConnState::Connecting => Icon::new(IconName::LoaderCircle).text_color(theme.muted_foreground),
            ConnState::Failed(_) => Icon::new(IconName::TriangleAlert).text_color(theme.danger),
            ConnState::Disconnected => Icon::new(Lucide::Unplug).text_color(theme.muted_foreground),
        };
        let manual = !self.txn.autocommit;
        let txn_chip = (manual || self.txn.in_txn).then(|| {
            let pending = self.txn.pending_writes;
            let (fg, label) = if self.txn.in_txn && pending > 0 {
                (theme.warning, format!("{pending} pending"))
            } else if self.txn.in_txn {
                (theme.warning, "txn open".to_string())
            } else {
                (theme.muted_foreground, "no txn".to_string())
            };
            h_flex()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(fg.opacity(0.12))
                .text_color(fg)
                .text_xs()
                .child(Icon::new(Lucide::GitCommitHorizontal).xsmall())
                .child(label)
        });
        let running = self.running.map(|(_, t)| t.elapsed());
        h_flex()
            .id("tab-toolbar")
            .w_full()
            .h(px(38.))
            .px_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child({
                let projects = self.projects.clone();
                let only = self.project;
                let me = cx.entity();
                let current = self.conn.as_ref().map(|c| c.key());
                Button::new("conn-chip")
                    .small()
                    .outline()
                    .icon(conn_icon)
                    .label(conn_label)
                    .tooltip("Connection for this tab")
                    .dropdown_menu(move |mut menu, window, _| {
                        // A tab's script and connection come from one project:
                        // once the tab has a project, only its connections are offered.
                        let ps = projects.borrow();
                        let shown: Vec<_> = ps.iter().filter(|p| only.is_none_or(|k| k == p.key)).collect();
                        if shown.iter().all(|p| p.connections.connections.is_empty()) {
                            return menu.label("No connections in this project yet");
                        }
                        for (i, p) in shown.iter().enumerate() {
                            if shown.len() > 1 {
                                if i > 0 {
                                    menu = menu.separator();
                                }
                                menu = menu.label(p.name().to_string());
                            }
                            let mut conns = p.connections.connections.clone();
                            conns.sort_by_key(|c| (c.folder.clone().unwrap_or_default().to_lowercase(), c.name.to_lowercase()));
                            for c in conns {
                                let label = match &c.folder {
                                    Some(f) => format!("{f} / {}", c.name),
                                    None => c.name.clone(),
                                };
                                let checked = current.as_deref() == Some(c.key().as_str());
                                menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(window.listener_for(
                                    &me,
                                    move |t: &mut QueryTab, _: &ClickEvent, _, cx| t.set_connection(Some(c.clone()), cx),
                                )));
                            }
                        }
                        menu
                    })
            })
            .child(
                Button::new("autocommit")
                    .xsmall()
                    .ghost()
                    .label(if manual { "Manual commit" } else { "Auto-commit" })
                    .icon(if manual { Icon::new(Lucide::Lock) } else { Icon::new(Lucide::Zap) })
                    .tooltip("Toggle auto-commit (Alt+A)")
                    .disabled(self.txn.in_txn || self.is_running())
                    .on_click(cx.listener(|this, _, _, _| this.toggle_autocommit())),
            )
            .children(txn_chip)
            .when(self.txn.in_txn, |el| {
                el.child(
                    Button::new("commit")
                        .xsmall()
                        .outline()
                        .label("Commit")
                        .icon(IconName::Check)
                        .tooltip("Commit (Alt+C)")
                        .on_click(cx.listener(|this, _, _, _| this.commit())),
                )
                .child(
                    Button::new("rollback")
                        .xsmall()
                        .ghost()
                        .label("Rollback")
                        .icon(IconName::Undo2)
                        .tooltip("Rollback (Alt+R)")
                        .on_click(cx.listener(|this, _, _, _| this.rollback())),
                )
            })
            .child(div().flex_1())
            .when_some(running, |el, d| {
                el.child(
                    h_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child(fmt_elapsed(d)),
                )
            })
            .child(if self.running.is_some() {
                Button::new("cancel")
                    .small()
                    .danger()
                    .icon(IconName::Square)
                    .label("Cancel")
                    .tooltip("Cancel on the server (Ctrl+.)")
                    .on_click(cx.listener(|this, _, _, _| this.cancel()))
            } else {
                Button::new("run")
                    .small()
                    .success()
                    .icon(IconName::Play)
                    .label("Run")
                    .tooltip("Run statement or selection (Ctrl+Enter) · script (Alt+X)")
                    .on_click(cx.listener(|this, _, _, cx| this.run_statement(cx)))
            })
    }

    fn render_result_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut left = h_flex().gap_2().items_center().text_sm();
        match &self.status {
            Status::Idle => left = left.text_color(theme.muted_foreground).child("Ctrl+Enter runs the statement under the cursor"),
            Status::Running => {
                left = left
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::LoaderCircle).small())
                    .child("Running…");
            }
            Status::Rows { elapsed } => {
                let (rows, more, label, color) = match &self.fetch {
                    Some((p, paused)) => match &p.status {
                        SpoolStatus::Complete => (p.rows, false, None, None),
                        SpoolStatus::Open => (p.rows, true, paused.then_some("paused on server"), Some(theme.info)),
                        SpoolStatus::Stopped(m) => (p.rows, false, Some(m.as_str()), Some(theme.warning)),
                    },
                    None => (0, true, None, None),
                };
                left = left
                    .child(Icon::new(Lucide::Table2).small().text_color(theme.success))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!("{}{} rows", fmt_count(rows), if more { "+" } else { "" })),
                    )
                    .child(div().text_color(theme.muted_foreground).child(fmt_elapsed(*elapsed)))
                    .when_some(label.zip(color), |el, (l, c)| {
                        el.child(div().px_1p5().rounded_sm().bg(c.opacity(0.12)).text_color(c).text_xs().child(l.to_string()))
                    });
            }
            Status::Done { message, elapsed } => {
                left = left
                    .child(Icon::new(IconName::CircleCheck).small().text_color(theme.success))
                    .child(message.clone())
                    .child(div().text_color(theme.muted_foreground).child(fmt_elapsed(*elapsed)));
            }
            Status::Failed { error, elapsed, .. } => {
                left = left
                    .child(Icon::new(IconName::CircleX).small().text_color(theme.danger))
                    .child(div().text_color(theme.danger).child(
                        error.code.as_ref().map(|c| format!("Error {c}")).unwrap_or_else(|| "Error".into()),
                    ))
                    .child(div().text_color(theme.muted_foreground).child(fmt_elapsed(*elapsed)));
            }
            Status::Cancelled { elapsed } => {
                left = left
                    .child(Icon::new(IconName::CircleAlert).small().text_color(theme.warning))
                    .child("Cancelled")
                    .child(div().text_color(theme.muted_foreground).child(fmt_elapsed(*elapsed)));
            }
        }
        let export_label = match &self.export {
            Some((_, rows, None)) => Some(format!("Exporting… {}", fmt_count(*rows))),
            _ => None,
        };
        h_flex()
            .w_full()
            .h(px(32.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .child(left)
            .child(div().flex_1())
            .when_some(export_label, |el, l| el.child(div().text_xs().text_color(theme.info).child(l)))
            .when(self.grid.read(cx).delegate().has_result(), |el| {
                el.child(
                    Button::new("export")
                        .xsmall()
                        .ghost()
                        .icon(Lucide::Download)
                        .label("CSV")
                        .tooltip("Export the full result to CSV (Ctrl+Shift+E)")
                        .on_click(cx.listener(|this, _, window, cx| this.export_csv(window, cx))),
                )
            })
    }

    fn render_error(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let Status::Failed { error, .. } = &self.status else { return None };
        let theme = cx.theme();
        Some(
            v_flex()
                .p_4()
                .gap_2()
                .child(div().text_color(theme.danger).font_weight(FontWeight::MEDIUM).child(error.message.clone()))
                .when_some(error.detail.clone(), |el, d| el.child(div().text_sm().child(format!("Detail: {d}"))))
                .when_some(error.hint.clone(), |el, h| {
                    el.child(div().text_sm().text_color(theme.info).child(format!("Hint: {h}")))
                })
                .when_some(error.position, |el, p| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).child(format!("at character {p}")))
                }),
        )
    }
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let color = self.conn.as_ref().map(|c| conn_color(c, cx)).unwrap_or(theme.border);
        let error = self.render_error(cx).map(|e| e.into_any_element());
        let has_rows = {
            let g = self.grid.read(cx).delegate();
            g.has_result()
                && error.is_none()
                && matches!(self.status, Status::Rows { .. } | Status::Running)
        };
        let results = v_flex()
            .size_full()
            .child(self.render_result_bar(cx))
            .child(match (error, has_rows) {
                (Some(e), _) => div().flex_1().child(e).into_any_element(),
                (None, true) => div().flex_1().child(DataTable::new(&self.grid).stripe(true)).into_any_element(),
                (None, false) => div().flex_1().into_any_element(),
            });
        v_flex()
            .size_full()
            // A stripe in the connection's color, so the environment is obvious.
            .border_t_2()
            .border_color(color)
            .child(self.render_toolbar(cx))
            .child(
                div().flex_1().child(
                    v_resizable("editor-results")
                        .child(
                            resizable_panel()
                                .size(px(320.))
                                .child(
                                    div()
                                        .key_context("SqlEditor")
                                        .size_full()
                                        .child(Editor::new(&self.editor).bordered(false).h(relative(1.))),
                                )
                        )
                        .child(resizable_panel().child(results.into_any_element())),
                ),
            )
    }
}

//! Add/edit connection dialog.

use std::cell::RefCell;
use std::rc::Rc;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_core::{Connection, PALETTE};
use nbq_driver::{DbKind, SslMode};

struct Choices {
    kind: DbKind,
    server_kind: DbKind,
    ssl: SslMode,
    color: Option<String>,
    autocommit: bool,
    ca: String,
    create: bool,
    busy: bool,
}

/// Open the dialog. `on_save(connection, user, new_password)`; `new_password` is
/// `None` when the field was left empty while editing (keep the stored one).
pub fn open(
    existing: Option<Connection>,
    folder: Option<String>,
    user: String,
    engine: Arc<nbq_engine::Engine>,
    project_root: PathBuf,
    window: &mut Window,
    cx: &mut App,
    on_save: impl Fn(Connection, String, Option<String>, &mut Window, &mut App) + 'static,
) {
    let editing = existing.is_some();
    let base = existing.unwrap_or_else(|| {
        let mut c = Connection::new("", DbKind::Postgres);
        c.folder = folder;
        c
    });
    let input = |v: String, ph: &'static str, window: &mut Window, cx: &mut App| {
        cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
    };
    let name = input(base.name.clone(), "Connection name", window, cx);
    let folder = input(base.folder.clone().unwrap_or_default(), "Prod/US", window, cx);
    let host = input(base.host.clone(), "localhost", window, cx);
    let server_kind = if base.kind.is_file_based() { DbKind::Postgres } else { base.kind };
    let port = input(if base.port == 0 { server_kind.default_port() } else { base.port }.to_string(), "", window, cx);
    let database = input(base.database.clone(), "Database name", window, cx);
    let file_path = input(base.file_path.to_string_lossy().into_owned(), "Choose a database file…", window, cx);
    let user = input(if editing { user } else { "postgres".into() }, "Username", window, cx);
    let password = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(if editing { "Unchanged" } else { "Password" })
    });
    let choices = Rc::new(RefCell::new(Choices {
        kind: base.kind,
        server_kind,
        ssl: base.ssl,
        color: base.color.clone(),
        autocommit: base.autocommit,
        ca: base.ssl_root_cert.clone(),
        create: false,
        busy: false,
    }));
    let on_save = Rc::new(on_save);
    let first = std::cell::Cell::new(true);

    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let theme = cx.theme().clone();
        let ch = choices.borrow();
        let file_based = ch.kind.is_file_based();
        let field = |label: &'static str, el: AnyElement| {
            v_flex()
                .gap_1()
                .flex_1()
                .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                .child(el)
        };
        let seg = |id: &'static str, label: &'static str, on: bool, f: Rc<dyn Fn(&mut Choices)>| {
            let choices = choices.clone();
            Button::new(id).xsmall().px_2().label(label).when(on, |b| b.primary()).when(!on, |b| b.ghost()).on_click(
                move |_, window, _| {
                    f(&mut choices.borrow_mut());
                    window.refresh();
                },
            )
        };
        let engine_button = |id: &'static str, label: &'static str, kind: DbKind| {
            let (choices, port, database, user) = (choices.clone(), port.clone(), database.clone(), user.clone());
            Button::new(id).xsmall().px_2().label(label)
                .when(ch.kind == kind, |button| button.primary())
                .when(ch.kind != kind, |button| button.ghost())
                .on_click(move |_, window, cx| {
                    let previous = choices.borrow().server_kind;
                    if !kind.is_file_based() && previous != kind {
                        port.update(cx, |input, cx| {
                            if input.value().trim() == previous.default_port().to_string() {
                                input.set_value(kind.default_port().to_string(), window, cx);
                            }
                        });
                        database.update(cx, |input, cx| {
                            if input.value().trim() == previous.default_database() {
                                input.set_value(kind.default_database(), window, cx);
                            }
                        });
                        if !editing {
                            user.update(cx, |input, cx| {
                                let old_default = if previous == DbKind::MySql { "" } else { "postgres" };
                                if input.value().trim() == old_default {
                                    let new_default = if kind == DbKind::MySql { "" } else { "postgres" };
                                    input.set_value(new_default, window, cx);
                                }
                            });
                        }
                        choices.borrow_mut().server_kind = kind;
                    }
                    choices.borrow_mut().kind = kind;
                    window.refresh();
                })
        };
        let swatches = h_flex().gap_1p5().children(
            std::iter::once((None, 0x000000u32)).chain(PALETTE.iter().map(|(n, c)| (Some(*n), *c))).map(|(n, rgbv)| {
                let selected = ch.color.as_deref() == n;
                let choices = choices.clone();
                let id = SharedString::from(format!("swatch-{}", n.unwrap_or("none")));
                div()
                    .id(id)
                    .size(px(18.))
                    .rounded_full()
                    .cursor_pointer()
                    .border_2()
                    .border_color(if selected { theme.foreground } else { theme.border })
                    .when_some(n, |d, _| d.bg(rgb(rgbv)))
                    .on_click(move |_, window, _| {
                        choices.borrow_mut().color = n.map(str::to_string);
                        window.refresh();
                    })
            }),
        );
        let body = v_flex()
            .gap_3()
            .child(h_flex().gap_3().child(field("Name", Input::new(&name).into_any_element())).child(field(
                "Folder (optional)",
                Input::new(&folder).into_any_element(),
            )))
            .child(field("Connection type",
                h_flex()
                    .gap_1()
                    .self_start()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .p_1()
                    .child(engine_button("k-pg", "PostgreSQL", DbKind::Postgres))
                    .child(engine_button("k-rs", "Redshift", DbKind::Redshift))
                    .child(engine_button("k-mysql", "MySQL", DbKind::MySql))
                    .child(div().mx_1().w(px(1.)).h_4().bg(theme.border))
                    .child(engine_button("k-sqlite", "SQLite", DbKind::Sqlite))
                    .child(engine_button("k-duckdb", "DuckDB", DbKind::DuckDb)).into_any_element(),
            ))
            .when(file_based, |body| body.child(
                v_flex().gap_3()
                    .child(h_flex().gap_2()
                        .child(seg("file-open", "Open existing", !ch.create, Rc::new(|c| c.create = false)))
                        .child(seg("file-create", "Create new", ch.create, Rc::new(|c| c.create = true))))
                    .child(field("Database file", h_flex().gap_2()
                        .child(div().flex_1().min_w_0().child(Input::new(&file_path)))
                        .child({
                            let (choices, file_path, project_root) = (choices.clone(), file_path.clone(), project_root.clone());
                            Button::new("browse-db").outline().label("Browse…").on_click(move |_, window, cx| {
                                let (create, kind) = { let ch = choices.borrow(); (ch.create, ch.kind) };
                                let file_path = file_path.clone();
                                let project_root = project_root.clone();
                                window.spawn(cx, async move |cx| {
                                    let picker = rfd::AsyncFileDialog::new().set_directory(project_root)
                                        .add_filter("Database files", kind.file_extensions()).add_filter("All files", &["*"]);
                                    let picked = if create { picker.set_file_name(kind.default_file_name()).save_file().await } else { picker.pick_file().await };
                                    if let Some(file) = picked {
                                        let path = file.path().to_string_lossy().into_owned();
                                        let _ = cx.update(|window, cx| file_path.update(cx, |s, cx| s.set_value(path, window, cx)));
                                    }
                                }).detach();
                            })
                        }).into_any_element()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(if ch.create {
                        "Choose a new filename. The database is created when you save; existing files are never replaced."
                    } else {
                        "Open a local database file. Paths may be relative to this project."
                    }))
            ))
            .when(!file_based, |body| body
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Host", Input::new(&host).into_any_element())))
                    .child(div().w(px(90.)).child(field("Port", Input::new(&port).into_any_element()))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(field(if ch.kind == DbKind::MySql { "Database (optional)" } else { "Database" }, Input::new(&database).into_any_element()))
                    .child(field("User", Input::new(&user).into_any_element()))
                    .child(field("Password", Input::new(&password).into_any_element())),
            )
            .child(
                h_flex()
                    .mt_1()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).w(px(40.)).child("SSL"))
                    .child(h_flex().gap_1().rounded_md().border_1().border_color(theme.border).p_1()
                        .child(seg("ssl-off", "Disable", ch.ssl == SslMode::Disable, Rc::new(|c| c.ssl = SslMode::Disable)))
                        .child(seg("ssl-pref", "Prefer", ch.ssl == SslMode::Prefer, Rc::new(|c| c.ssl = SslMode::Prefer)))
                        .child(seg("ssl-req", "Require", ch.ssl == SslMode::Require, Rc::new(|c| c.ssl = SslMode::Require)))
                        .child(seg("ssl-verify", "Verify full", ch.ssl == SslMode::VerifyFull, Rc::new(|c| c.ssl = SslMode::VerifyFull))
                            .tooltip("Verifies the certificate chain and server hostname"))),
            )
            .when(ch.ssl == SslMode::Prefer || ch.ssl == SslMode::Require, |body| body.child(
                div().text_xs().text_color(theme.warning).child("Server identity is not verified. Use Verify full to validate its certificate.")
            ))
            .when(ch.ssl == SslMode::VerifyFull, |body| body.child(
                h_flex().gap_2().items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).w(px(40.)).child("CA"))
                    .child({
                        let choices = choices.clone();
                        Button::new("import-ca").small().outline().label("Import CA…").on_click(move |_, window, cx| {
                            let choices = choices.clone();
                            window.spawn(cx, async move |cx| {
                                let Some(file) = rfd::AsyncFileDialog::new().add_filter("Public CA", &["pem", "crt"]).pick_file().await else { return };
                                let result = cx.background_executor().spawn(async move {
                                    use std::io::Read;
                                    let mut pem = String::new();
                                    std::fs::File::open(file.path())?.take(512 * 1024 + 1).read_to_string(&mut pem)?;
                                    nbq_driver::pg::validate_ca_bundle(&pem).map_err(std::io::Error::other)?;
                                    Ok::<_, std::io::Error>(pem)
                                }).await;
                                let _ = cx.update(|window, cx| match result {
                                    Ok(pem) => { choices.borrow_mut().ca = pem; window.refresh(); }
                                    Err(e) => window.push_notification(format!("CA import failed: {e}"), cx),
                                });
                            }).detach();
                        })
                    })
                    .child(div().text_xs().text_color(theme.muted_foreground).child(if !ch.ca.is_empty() { "Custom CA loaded" } else if ch.kind == DbKind::MySql { "Using system and public roots" } else { "Using system and AWS roots" }))
                    .when(!ch.ca.is_empty(), |row| {
                        let choices = choices.clone();
                        row.child(Button::new("clear-ca").xsmall().ghost().label("Clear").on_click(move |_, window, _| {
                            choices.borrow_mut().ca.clear(); window.refresh();
                        }))
                    })
            )))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).w(px(40.)).child("Color"))
                    .child(swatches),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child({
                        let choices = choices.clone();
                        Switch::new("autocommit").checked(ch.autocommit).label("Auto-commit by default").on_click(
                            move |v: &bool, window, _| {
                                choices.borrow_mut().autocommit = *v;
                                window.refresh();
                            },
                        )
                    }),
            )
            .when(!file_based, |body| body.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("User and password are kept in your OS credential store, never in the project folder."),
            ));
        let busy = ch.busy;
        drop(ch);
        let (name, folder, host, port, database, user, password, choices, base, on_save, file_path, engine, project_root) = (
            name.clone(),
            folder.clone(),
            host.clone(),
            port.clone(),
            database.clone(),
            user.clone(),
            password.clone(),
            choices.clone(),
            base.clone(),
            on_save.clone(),
            file_path.clone(),
            engine.clone(),
            project_root.clone(),
        );
        dialog
            .close_button(!busy)
            .overlay_closable(!busy)
            .keyboard(!busy)
            .title(if editing { "Edit connection" } else { "New connection" })
            .w(px(620.))
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("cancel").label("Cancel").disabled(busy).outline()))
                    .child(DialogAction::new().child(Button::new("save").label(if busy { "Saving…" } else { "Save" }).disabled(busy).primary())),
            )
            .on_ok(move |_, window, cx| {
                let v = |s: &Entity<InputState>| s.read(cx).value().trim().to_string();
                let ch = choices.borrow();
                if ch.busy { return false; }
                let mut c = base.clone();
                c.name = v(&name);
                if c.name.is_empty() {
                    window.push_notification("Give the connection a name", cx);
                    return false;
                }
                let p = if ch.kind.is_file_based() { 0 } else if let Ok(p) = v(&port).parse::<u16>() { p } else {
                    window.push_notification("Port must be a number", cx);
                    return false;
                };
                c.port = p;
                c.folder = Some(v(&folder)).filter(|f| !f.is_empty());
                c.host = v(&host);
                c.database = v(&database);
                let user = v(&user);
                c.kind = ch.kind;
                c.ssl = ch.ssl;
                c.ssl_root_cert = ch.ca.clone();
                c.color = ch.color.clone();
                c.autocommit = ch.autocommit;
                if ch.kind.is_file_based() {
                    let path = v(&file_path);
                    if path.is_empty() {
                        window.push_notification("Choose a database file", cx);
                        return false;
                    }
                    c.file_path = PathBuf::from(path);
                    c.host.clear();
                    c.database.clear();
                    c.ssl = SslMode::Disable;
                    c.ssl_root_cert.clear();
                    let path = if c.file_path.is_relative() { project_root.join(&c.file_path) } else { c.file_path.clone() };
                    let result = engine.prepare_database_file(c.kind, path, ch.create);
                    drop(ch);
                    choices.borrow_mut().busy = true;
                    let (choices, on_save) = (choices.clone(), on_save.clone());
                    window.spawn(cx, async move |cx| {
                        let result = result.await.unwrap_or_else(|e| Err(nbq_driver::DriverError::connection(e.to_string())));
                        let _ = cx.update(|window, cx| {
                            choices.borrow_mut().busy = false;
                            match result {
                                Ok(()) => {
                                    window.close_dialog(cx);
                                    on_save(c, String::new(), None, window, cx);
                                }
                                Err(e) => { window.push_notification(format!("Database file: {e}"), cx); window.refresh(); }
                            }
                        });
                    }).detach();
                    return false;
                }
                c.file_path.clear();
                let pw = password.read(cx).value().to_string();
                let pw = (!pw.is_empty()).then_some(pw);
                on_save(c, user, pw, window, cx);
                true
            })
    });
}

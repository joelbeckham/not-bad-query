//! Add/edit connection dialog.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gq_core::{Connection, PALETTE};
use gq_driver::{DbKind, SslMode};

struct Choices {
    kind: DbKind,
    ssl: SslMode,
    color: Option<String>,
    autocommit: bool,
}

/// Open the dialog. `on_save(connection, user, new_password)`; `new_password` is
/// `None` when the field was left empty while editing (keep the stored one).
pub fn open(
    existing: Option<Connection>,
    folder: Option<String>,
    user: String,
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
    let name = input(base.name.clone(), "orders-prod", window, cx);
    let folder = input(base.folder.clone().unwrap_or_default(), "Prod/US (optional)", window, cx);
    let host = input(base.host.clone(), "localhost", window, cx);
    let port = input(base.port.to_string(), "5432", window, cx);
    let database = input(base.database.clone(), "postgres", window, cx);
    let user = input(if editing { user } else { "postgres".into() }, "postgres", window, cx);
    let password = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(if editing { "unchanged" } else { "password" })
    });
    let choices = Rc::new(RefCell::new(Choices {
        kind: base.kind,
        ssl: base.ssl,
        color: base.color.clone(),
        autocommit: base.autocommit,
    }));
    let on_save = Rc::new(on_save);
    let first = std::cell::Cell::new(true);

    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let theme = cx.theme().clone();
        let ch = choices.borrow();
        let field = |label: &'static str, el: AnyElement| {
            v_flex()
                .gap_1()
                .flex_1()
                .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                .child(el)
        };
        let seg = |id: &'static str, label: &'static str, on: bool, f: Rc<dyn Fn(&mut Choices)>| {
            let choices = choices.clone();
            Button::new(id).xsmall().label(label).when(on, |b| b.primary()).when(!on, |b| b.ghost()).on_click(
                move |_, window, _| {
                    f(&mut choices.borrow_mut());
                    window.refresh();
                },
            )
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
                "Folder",
                Input::new(&folder).into_any_element(),
            )))
            .child(
                h_flex()
                    .gap_2()
                    .child(seg("k-pg", "PostgreSQL", ch.kind == DbKind::Postgres, Rc::new(|c| c.kind = DbKind::Postgres)))
                    .child(seg("k-rs", "Redshift", ch.kind == DbKind::Redshift, Rc::new(|c| c.kind = DbKind::Redshift))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Host", Input::new(&host).into_any_element())))
                    .child(div().w(px(90.)).child(field("Port", Input::new(&port).into_any_element()))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(field("Database", Input::new(&database).into_any_element()))
                    .child(field("User", Input::new(&user).into_any_element()))
                    .child(field("Password", Input::new(&password).into_any_element())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).w(px(40.)).child("SSL"))
                    .child(seg("ssl-off", "Disable", ch.ssl == SslMode::Disable, Rc::new(|c| c.ssl = SslMode::Disable)))
                    .child(seg("ssl-pref", "Prefer", ch.ssl == SslMode::Prefer, Rc::new(|c| c.ssl = SslMode::Prefer)))
                    .child(seg("ssl-req", "Require", ch.ssl == SslMode::Require, Rc::new(|c| c.ssl = SslMode::Require))),
            )
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
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("User and password are kept in your OS credential store, never in the project folder."),
            );
        drop(ch);
        let (name, folder, host, port, database, user, password, choices, base, on_save) = (
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
        );
        dialog
            .title(if editing { "Edit connection" } else { "New connection" })
            .w(px(620.))
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
                    .child(DialogAction::new().child(Button::new("save").label("Save").primary())),
            )
            .on_ok(move |_, window, cx| {
                let v = |s: &Entity<InputState>| s.read(cx).value().trim().to_string();
                let ch = choices.borrow();
                let mut c = base.clone();
                c.name = v(&name);
                if c.name.is_empty() {
                    window.push_notification("Give the connection a name", cx);
                    return false;
                }
                let Ok(p) = v(&port).parse::<u16>() else {
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
                c.color = ch.color.clone();
                c.autocommit = ch.autocommit;
                let pw = password.read(cx).value().to_string();
                let pw = (!pw.is_empty()).then_some(pw);
                on_save(c, user, pw, window, cx);
                true
            })
    });
}

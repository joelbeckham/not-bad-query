use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_driver::{
    DbKind,
    csv_import::{Column, Preview, Recipe},
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

pub fn open(
    source: PathBuf,
    root: PathBuf,
    preview: Preview,
    destinations: Vec<nbq_core::Connection>,
    target: Option<PathBuf>,
    engine: Arc<nbq_engine::Engine>,
    window: &mut Window,
    cx: &mut App,
    on_done: impl Fn(Option<Recipe>, &mut Window, &mut App) + 'static,
) {
    open_settings(
        source,
        root,
        preview,
        true,
        destinations,
        target,
        None,
        engine,
        window,
        cx,
        Rc::new(on_done),
    );
}

type Done = Rc<dyn Fn(Option<Recipe>, &mut Window, &mut App)>;

fn engine_kind(index: usize) -> DbKind {
    if index == 1 { DbKind::Sqlite } else { DbKind::DuckDb }
}

fn matching_destinations(destinations: &[nbq_core::Connection], index: usize) -> Vec<nbq_core::Connection> {
    destinations
        .iter()
        .filter(|c| c.kind == engine_kind(index))
        .cloned()
        .collect()
}

fn destination_labels(destinations: &[nbq_core::Connection]) -> Vec<SharedString> {
    std::iter::once("New database".into())
        .chain(destinations.iter().map(|c| SharedString::from(c.name.clone())))
        .collect()
}

fn open_settings(
    source: PathBuf,
    root: PathBuf,
    preview: Preview,
    headers: bool,
    destinations: Vec<nbq_core::Connection>,
    target: Option<PathBuf>,
    draft: Option<(String, String, usize)>,
    engine: Arc<nbq_engine::Engine>,
    window: &mut Window,
    cx: &mut App,
    on_done: Done,
) {
    let stem = source.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let (database_name, table_name, engine_index) = draft.unwrap_or_else(|| {
        let sqlite = target
            .as_ref()
            .and_then(|p| destinations.iter().find(|c| c.params(None).file_path == *p))
            .is_some_and(|c| c.kind == DbKind::Sqlite);
        (
            format!("{stem}.{}", if sqlite { "sqlite" } else { "duckdb" }),
            stem,
            usize::from(sqlite),
        )
    });
    let database = cx.new(|cx| InputState::new(window, cx).default_value(database_name));
    let table = cx.new(|cx| InputState::new(window, cx).default_value(table_name));
    let kind = cx.new(|cx| SelectState::new(vec!["DuckDB", "SQLite"], Some(IndexPath::new(engine_index)), window, cx));
    let initial_destinations = matching_destinations(&destinations, engine_index);
    let labels = destination_labels(&initial_destinations);
    let target_index = target
        .as_ref()
        .and_then(|p| initial_destinations.iter().position(|c| c.params(None).file_path == *p))
        .map_or(0, |i| i + 1);
    let destination = cx.new(|cx| SelectState::new(labels, Some(IndexPath::new(target_index)), window, cx));
    let header = cx.new(|cx| {
        SelectState::new(
            vec!["First row is headers", "No headers"],
            Some(IndexPath::new(if headers { 0 } else { 1 })),
            window,
            cx,
        )
    });
    let delimiter = cx.new(|cx| {
        SelectState::new(
            vec!["Comma", "Tab", "Semicolon", "Pipe"],
            Some(IndexPath::new(
                [b',', b'\t', b';', b'|']
                    .iter()
                    .position(|d| *d == preview.delimiter)
                    .unwrap_or(0),
            )),
            window,
            cx,
        )
    });
    let columns: Vec<_> = preview
        .columns
        .iter()
        .map(|c| {
            let name = cx.new(|cx| InputState::new(window, cx).default_value(c.name.clone()));
            let ix = ["TEXT", "BIGINT", "DOUBLE"]
                .iter()
                .position(|k| *k == c.kind)
                .unwrap_or(0);
            let kind =
                cx.new(|cx| SelectState::new(vec!["TEXT", "BIGINT", "DOUBLE"], Some(IndexPath::new(ix)), window, cx));
            (name, kind)
        })
        .collect();
    let busy = Rc::new(RefCell::new(None::<nbq_engine::csv_import::ImportProgress>));
    let loading = Rc::new(std::cell::Cell::new(false));
    let last_engine = std::cell::Cell::new(engine_index);
    window.open_dialog(cx, move |dialog, window, cx| {
        let selected_engine = kind.read(cx).selected_index(cx).map_or(0, |i| i.row);
        let available = matching_destinations(&destinations, selected_engine);
        if last_engine.replace(selected_engine) != selected_engine {
            destination.update(cx, |s, cx| {
                s.set_items(destination_labels(&available), window, cx);
                s.set_selected_index(Some(IndexPath::new(0)), window, cx);
            });
            let mut path = PathBuf::from(database.read(cx).value().as_ref());
            if path.extension().is_some_and(|e| {
                ["duckdb", "db", "sqlite", "sqlite3"]
                    .iter()
                    .any(|x| e.eq_ignore_ascii_case(x))
            }) {
                path.set_extension(if selected_engine == 0 { "duckdb" } else { "sqlite" });
                database.update(cx, |s, cx| s.set_value(path.to_string_lossy().to_string(), window, cx));
            }
        }
        let working = busy.borrow().is_some() || loading.get();
        let existing = destination
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| ix.row.checked_sub(1))
            .and_then(|i| available.get(i))
            .cloned();
        let field = |label: &'static str, el: AnyElement| {
            v_flex()
                .gap_1()
                .flex_1()
                .flex_basis(px(0.))
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .w_full()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(el)
        };
        let body = v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(source.file_name().unwrap_or_default().to_string_lossy().to_string()),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(field("Engine", Select::new(&kind).disabled(working).into_any_element()))
                    .child(field(
                        "Destination",
                        Select::new(&destination).disabled(working).into_any_element(),
                    )),
            )
            .child(
                h_flex()
                    .gap_3()
                    .when(existing.is_none(), |row| {
                        row.child(field(
                            "Database in connections/",
                            Input::new(&database).disabled(working).into_any_element(),
                        ))
                    })
                    .child(field("Table", Input::new(&table).disabled(working).into_any_element())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Snapshot · Empty fields become NULL"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Select::new(&header).disabled(working))
                    .child(Select::new(&delimiter).disabled(working))
                    .child({
                        let (source, root, header, delimiter, destinations, engine, on_done) = (
                            source.clone(),
                            root.clone(),
                            header.clone(),
                            delimiter.clone(),
                            destinations.clone(),
                            engine.clone(),
                            on_done.clone(),
                        );
                        let (database, table, kind, destination, loading) = (
                            database.clone(),
                            table.clone(),
                            kind.clone(),
                            destination.clone(),
                            loading.clone(),
                        );
                        Button::new("csv-preview")
                            .outline()
                            .disabled(working)
                            .label("Update preview")
                            .on_click(move |_, window, cx| {
                                let headers = header.read(cx).selected_value() == Some(&"First row is headers");
                                let delimiter = [b',', b'\t', b';', b'|']
                                    [delimiter.read(cx).selected_index(cx).map_or(0, |i| i.row)];
                                let result = engine.preview_csv(source.clone(), Some(delimiter), headers);
                                let draft = (
                                    database.read(cx).value().to_string(),
                                    table.read(cx).value().to_string(),
                                    kind.read(cx).selected_index(cx).map_or(0, |i| i.row),
                                );
                                let target = destination
                                    .read(cx)
                                    .selected_index(cx)
                                    .and_then(|i| i.row.checked_sub(1))
                                    .and_then(|i| destinations.iter().filter(|c| c.kind == engine_kind(draft.2)).nth(i))
                                    .map(|c| c.params(None).file_path);
                                let (source, root, destinations, engine, on_done, loading) = (
                                    source.clone(),
                                    root.clone(),
                                    destinations.clone(),
                                    engine.clone(),
                                    on_done.clone(),
                                    loading.clone(),
                                );
                                loading.set(true);
                                window.refresh();
                                window
                                    .spawn(cx, async move |cx| {
                                        let result = result.await.unwrap_or_else(|e| Err(e.to_string()));
                                        let _ = cx.update(|window, cx| match result {
                                            Ok(preview) => {
                                                loading.set(false);
                                                window.close_dialog(cx);
                                                open_settings(
                                                    source,
                                                    root,
                                                    preview,
                                                    headers,
                                                    destinations,
                                                    target,
                                                    Some(draft),
                                                    engine,
                                                    window,
                                                    cx,
                                                    on_done,
                                                );
                                            }
                                            Err(e) => {
                                                loading.set(false);
                                                window.push_notification(e, cx);
                                                window.refresh();
                                            }
                                        });
                                    })
                                    .detach();
                            })
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().w(px(180.)).child("Column"))
                    .child(div().w(px(120.)).child("Type"))
                    .child(div().flex_1().child("Sample values")),
            )
            .child(
                div()
                    .id("csv-columns")
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .gap_2()
                            .children(columns.iter().enumerate().map(|(i, (name, kind))| {
                                h_flex()
                                    .gap_2()
                                    .child(div().w(px(180.)).child(Input::new(name).disabled(working)))
                                    .child(div().w(px(120.)).child(Select::new(kind).disabled(working)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .overflow_hidden()
                                            .child(
                                                preview
                                                    .rows
                                                    .iter()
                                                    .filter_map(|r| r.get(i))
                                                    .take(3)
                                                    .cloned()
                                                    .collect::<Vec<_>>()
                                                    .join(" · "),
                                            ),
                                    )
                            })),
                    ),
            )
            .child({
                let cancel_busy = busy.clone();
                let cancel_done = on_done.clone();
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(working, |row| {
                        row.child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{} rows",
                                    busy.borrow()
                                        .as_ref()
                                        .map_or(0, |p| p.rows.load(std::sync::atomic::Ordering::Acquire))
                                )),
                        )
                    })
                    .child(
                        Button::new("csv-cancel")
                            .outline()
                            .disabled(loading.get())
                            .label(if working { "Cancel import" } else { "Cancel" })
                            .on_click(move |_, window, cx| {
                                if let Some(p) = cancel_busy.borrow().as_ref() {
                                    p.cancelled.store(true, std::sync::atomic::Ordering::Release);
                                } else {
                                    window.close_dialog(cx);
                                    cancel_done(None, window, cx);
                                }
                            }),
                    )
                    .child({
                        let source_columns = preview.source_columns.clone();
                        let (source, root, database, table, kind, columns, engine, busy, on_done, header, delimiter) = (
                            source.clone(),
                            root.clone(),
                            database.clone(),
                            table.clone(),
                            kind.clone(),
                            columns.clone(),
                            engine.clone(),
                            busy.clone(),
                            on_done.clone(),
                            header.clone(),
                            delimiter.clone(),
                        );
                        Button::new("csv-import")
                            .primary()
                            .disabled(working)
                            .label(if working { "Importing…" } else { "Import" })
                            .on_click(move |_, window, cx| {
                                let engine_kind = engine_kind(kind.read(cx).selected_index(cx).map_or(0, |i| i.row));
                                if existing.as_ref().is_some_and(|c| c.kind != engine_kind) {
                                    window.push_notification("Choose a destination for the selected engine", cx);
                                    return;
                                }
                                if (header.read(cx).selected_value() == Some(&"First row is headers")) != headers
                                    || [b',', b'\t', b';', b'|']
                                        [delimiter.read(cx).selected_index(cx).map_or(0, |i| i.row)]
                                        != preview.delimiter
                                {
                                    window.push_notification("Update the preview before importing", cx);
                                    return;
                                }
                                let mut filename = PathBuf::from(database.read(cx).value().trim().to_string());
                                if filename.components().count() != 1 || filename.file_name().is_none() {
                                    window.push_notification("Enter a database filename", cx);
                                    return;
                                }
                                if engine_kind == DbKind::Sqlite && filename.extension().is_some_and(|e| e == "duckdb")
                                {
                                    filename.set_extension("sqlite");
                                }
                                if !filename.extension().is_some_and(|e| {
                                    engine_kind.file_extensions().iter().any(|x| e.eq_ignore_ascii_case(x))
                                }) {
                                    filename = PathBuf::from(format!(
                                        "{}.{}",
                                        filename.display(),
                                        if engine_kind == DbKind::DuckDb {
                                            "duckdb"
                                        } else {
                                            "sqlite"
                                        }
                                    ));
                                }
                                let recipe = Recipe {
                                    source: source.clone(),
                                    database: existing
                                        .as_ref()
                                        .map(|c| c.params(None).file_path)
                                        .unwrap_or_else(|| root.join("connections").join(filename)),
                                    engine: engine_kind,
                                    table: table.read(cx).value().trim().to_string(),
                                    delimiter: preview.delimiter,
                                    headers,
                                    source_columns: source_columns.clone(),
                                    columns: columns
                                        .iter()
                                        .map(|(n, k)| Column {
                                            name: n.read(cx).value().trim().to_string(),
                                            kind: k.read(cx).selected_value().unwrap_or(&"TEXT").to_string(),
                                        })
                                        .collect(),
                                    imported_at: 0,
                                    source_size: 0,
                                    source_modified: 0,
                                };
                                let (progress, result) = engine.import_csv(recipe, existing.is_none(), false);
                                *busy.borrow_mut() = Some(progress);
                                refresh_progress(busy.clone(), window, cx);
                                let (busy, on_done) = (busy.clone(), on_done.clone());
                                window
                                    .spawn(cx, async move |cx| {
                                        let result = result.await.unwrap_or_else(|e| Err(e.to_string()));
                                        let _ = cx.update(|window, cx| {
                                            *busy.borrow_mut() = None;
                                            match result {
                                                Ok(recipe) => {
                                                    window.close_dialog(cx);
                                                    on_done(Some(recipe), window, cx);
                                                }
                                                Err(e) => {
                                                    window.push_notification(e, cx);
                                                    window.refresh();
                                                }
                                            }
                                        });
                                    })
                                    .detach();
                                window.refresh();
                            })
                    })
            });
        let cancel_done = on_done.clone();
        dialog
            .title("Import CSV")
            .w(px(720.))
            .close_button(!working)
            .overlay_closable(!working)
            .keyboard(!working)
            .on_cancel(move |_, window, cx| {
                cancel_done(None, window, cx);
                true
            })
            .child(body)
    });
    // Select triggers measure themselves on mount; settle the surrounding labels too.
    window.on_next_frame(|window, _| window.refresh());
}

fn refresh_progress(
    busy: Rc<RefCell<Option<nbq_engine::csv_import::ImportProgress>>>,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                if !cx
                    .update(|window, _| {
                        window.refresh();
                        busy.borrow().is_some()
                    })
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
        .detach();
}

pub fn reload(
    recipe: Recipe,
    engine: Arc<nbq_engine::Engine>,
    window: &mut Window,
    cx: &mut App,
    on_done: impl Fn(Recipe, &mut Window, &mut App) + 'static,
) {
    let (progress, result) = engine.import_csv(recipe.clone(), false, true);
    let busy = Rc::new(RefCell::new(Some(progress.clone())));
    refresh_progress(busy.clone(), window, cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let cancel = progress.cancelled.clone();
        dialog
            .title("Reload CSV")
            .w(px(400.))
            .close_button(false)
            .overlay_closable(false)
            .keyboard(false)
            .child(
                v_flex()
                    .gap_3()
                    .child(recipe.table.clone())
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!(
                        "{} rows",
                        progress.rows.load(std::sync::atomic::Ordering::Acquire)
                    )))
                    .child(
                        Button::new("cancel-reload")
                            .outline()
                            .label("Cancel")
                            .on_click(move |_, _, _| {
                                cancel.store(true, std::sync::atomic::Ordering::Release);
                            }),
                    ),
            )
    });
    window
        .spawn(cx, async move |cx| {
            let result = result.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = cx.update(|window, cx| {
                *busy.borrow_mut() = None;
                window.close_dialog(cx);
                match result {
                    Ok(recipe) => on_done(recipe, window, cx),
                    Err(e) => window.push_notification(e, cx),
                }
            });
        })
        .detach();
}

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod assistant;
mod app_icon;
mod complete;
mod conn_dialog;
mod csv_dialog;
mod grid;
mod query_tab;
mod sidebar;
mod ui;
mod vpn;
mod workbench;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

fn main() {
    tracing_subscriber_init();
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        cx.set_app_identity("not-bad-query", "not-bad-query");
        // After init, so these win over the components' own bindings.
        cx.bind_keys(actions::bindings());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("not-bad-query");
            cx.new(|cx| workbench::Workbench::new(window, cx))
        })
            .expect("open window");
        cx.activate(true);
    });
}

fn tracing_subscriber_init() {
    // Logging hook; kept minimal (stderr in debug builds via RUST_LOG later).
}

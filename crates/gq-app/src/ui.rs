//! Small shared UI helpers.

use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
use gq_core::Connection;
use gq_driver::ConnectParams;

/// The connection's accent color (falls back to a neutral).
pub fn conn_color(c: &Connection, cx: &App) -> Hsla {
    match c.color() {
        Some(col) => rgb(col.rgb).into(),
        None => cx.theme().muted_foreground,
    }
}

pub fn rgb_hsla(v: u32) -> Hsla {
    rgb(v).into()
}

/// Params with the password from the OS credential store.
pub fn params_for(c: &Connection) -> ConnectParams {
    let mut password = if c.save_password { gq_core::secrets::get(&c.id).ok().flatten() } else { None };
    // Debug builds: a dev password from the environment (used with GQ_CONFIG_DIR
    // for local testing without touching the OS credential store).
    if cfg!(debug_assertions) && password.is_none() {
        password = std::env::var("GQ_DEV_PASSWORD").ok();
    }
    c.params(password)
}

/// `1234567` → `1,234,567`.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `0.042s`, `3.2s`, `1:05.3`.
pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{}ms", d.as_millis())
    } else if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}:{:04.1}", (s / 60.0) as u64, s % 60.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn counts_and_durations() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_elapsed(Duration::from_millis(42)), "42ms");
        assert_eq!(fmt_elapsed(Duration::from_millis(3240)), "3.2s");
        assert_eq!(fmt_elapsed(Duration::from_millis(65_300)), "1:05.3");
    }
}

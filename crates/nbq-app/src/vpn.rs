//! The local VPN prompt. Authentication stays in the AWS client.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nbq_driver::{ErrorKind, vpn::Status};
use nbq_engine::ConnState;

pub fn blocked(state: &ConnState) -> bool {
    matches!(state, ConnState::Failed(error) if error.kind == ErrorKind::VpnRequired)
}

pub fn prompt(
    profile: Option<String>,
    window: &mut Window,
    cx: &mut App,
    read_status: impl Fn(&App) -> Option<Status> + 'static,
    on_retry: impl Fn(&mut Window, &mut App) + 'static,
    on_close: impl Fn(&mut App) + 'static,
) {
    let on_retry = Rc::new(on_retry);
    let on_close = Rc::new(on_close);
    window.open_dialog(cx, move |dialog, _, cx| {
        let on_retry = on_retry.clone();
        let on_close = on_close.clone();
        let theme = cx.theme();
        let status = read_status(cx);
        let connected = status == Some(Status::Connected);
        let has_profile = profile.is_some();
        let title = if has_profile {
            "VPN required"
        } else {
            "VPN status"
        };
        let (label, color) = match status {
            Some(Status::Connected) => ("Connected", theme.success),
            Some(Status::Disconnected) => ("Disconnected", theme.warning),
            Some(Status::Unavailable) => ("Unavailable", theme.muted_foreground),
            None => ("Checking…", theme.muted_foreground),
        };
        let instructions = match (cfg!(windows), has_profile, connected) {
            (false, _, _) => "VPN detection for this profile requires AWS VPN Client on Windows.",
            (true, true, true) => "Retry to open the database connection.",
            (true, true, false) => "Connect your profile in AWS VPN Client, then choose Retry.",
            (true, false, true) => "Your VPN is ready.",
            (true, false, false) => "Open AWS VPN Client to connect to your VPN profile.",
        };
        dialog
            .title(title)
            .child(
                v_flex()
                    .gap_3()
                    .text_sm()
                    .when_some(profile.as_ref(), |body, profile| {
                        body.child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("“{profile}” requires a VPN connection.")),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Tunnel"),
                            )
                            .child(div().text_color(color).child(label)),
                    )
                    .child(div().text_color(theme.muted_foreground).child(instructions)),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        div().w(px(88.)).child(
                            DialogClose::new()
                                .trigger(|button| button.outline().w_full().label("Dismiss")),
                        ),
                    )
                    .when(cfg!(windows) && has_profile, |footer| {
                        let retry = Button::new("retry-vpn-connection").label("Retry");
                        if connected {
                            footer.child(
                                div()
                                    .w(px(88.))
                                    .child(DialogAction::new().child(retry.primary().w_full())),
                            )
                        } else {
                            footer.child(retry.outline().disabled(true))
                        }
                    })
                    .when(cfg!(windows) && !connected, |footer| {
                        footer.child(
                            div().w(px(160.)).child(
                                DialogAction::new().child(
                                    Button::new("launch-aws-vpn")
                                        .w_full()
                                        .primary()
                                        .label("Open AWS VPN Client"),
                                ),
                            ),
                        )
                    }),
            )
            .on_ok(move |_, window, cx| {
                if connected && has_profile {
                    on_retry(window, cx);
                } else if cfg!(windows) && !connected {
                    window
                        .spawn(cx, async move |cx| {
                            let result = cx.background_executor().spawn(async { launch() }).await;
                            if let Err(error) = result {
                                let _ = cx.update(|window, cx| window.push_notification(error, cx));
                            }
                        })
                        .detach();
                    return false;
                }
                true
            })
            .on_close(move |_, _, cx| on_close(cx))
    });
}

fn launch() -> Result<(), String> {
    let path = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|base| {
            std::path::PathBuf::from(base)
                .join("Amazon")
                .join("AWS VPN Client")
        })
        .flat_map(|dir| ["AWS VPN Client.exe", "AWSVPNClient.exe"].map(|name| dir.join(name)))
        .find(|path| path.is_file())
        .ok_or("Could not locate AWS VPN Client. Open it from the Windows Start menu.")?;
    std::process::Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open AWS VPN Client: {error}"))
}

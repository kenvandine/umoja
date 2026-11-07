//! Session bring-up and actions that leave the compositor process.

use crate::tiling::{Action, Rect};
use std::process::Command;
use std::sync::LazyLock;
use umoja_common::dbus;

/// Runtime for D-Bus work, kept off Mir's threads.
pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("umoja-mir-dbus")
        .build()
        .expect("tokio runtime")
});

/// Called once Mir is accepting Wayland clients. Exports the environment to
/// systemd and D-Bus activation, then starts `umoja-session.target`, which
/// pulls in the panel, dock, overlay, portal and embedded Lemonade services.
pub fn started(wayland_display: &str) {
    tracing::info!(wayland_display, "compositor ready");
    if !wayland_display.is_empty() {
        // SAFETY: called from Mir's start callback before any of our own
        // threads read the environment.
        unsafe { std::env::set_var("WAYLAND_DISPLAY", wayland_display) };
    }
    std::thread::spawn(|| {
        let vars = [
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_TYPE",
        ];
        run(
            "dbus-update-activation-environment",
            &[&["--systemd"][..], &vars[..]].concat(),
        );
        run(
            "systemctl",
            &[&["--user", "import-environment"][..], &vars[..]].concat(),
        );
        if std::env::var_os("UMOJA_NO_SESSION_TARGET").is_none() {
            run(
                "systemctl",
                &["--user", "start", "--no-block", "umoja-session.target"],
            );
        }
    });
}

/// Actions handled by other processes over D-Bus.
pub fn dispatch(action: Action) {
    let method = match action {
        Action::ToggleOverlay => "Toggle",
        Action::ToggleDictation => "ToggleDictation",
        Action::ScreenshotUi => "ShowScreenshotUI",
        Action::Screenshot => "ScreenshotScreen",
        _ => return,
    };
    call_overlay(method, ());
}

pub fn screenshot_area(r: Rect) {
    call_overlay("ScreenshotArea", (r.x, r.y, r.w, r.h));
}

fn call_overlay<B>(method: &'static str, body: B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    // Never block the compositor thread on D-Bus.
    RUNTIME.spawn(async move {
        let result = async {
            let conn = zbus::Connection::session().await?;
            conn.call_method(
                Some(dbus::OVERLAY_BUS_NAME),
                dbus::OVERLAY_OBJECT_PATH,
                Some(dbus::OVERLAY_INTERFACE),
                method,
                &body,
            )
            .await?;
            Ok::<_, zbus::Error>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(method, error = %e, "overlay call failed");
        }
    });
}

fn run(cmd: &str, args: &[&str]) {
    match Command::new(cmd).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(cmd, ?args, status = %s, "command failed"),
        Err(e) => tracing::warn!(cmd, error = %e, "could not run"),
    }
}

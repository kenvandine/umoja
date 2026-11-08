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
        Action::ToggleOverview => "ToggleOverview",
        Action::ToggleDictation => "ToggleDictation",
        Action::ScreenshotUi => "ShowScreenshotUI",
        Action::Screenshot => "ScreenshotScreen",
        _ => return,
    };
    overlay_call(method, ());
}

pub fn screenshot_area(r: Rect) {
    overlay_call("ScreenshotArea", (r.x, r.y, r.w, r.h));
}

type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = zbus::Result<()>> + Send>>;
type Body = Box<dyn FnOnce(zbus::Connection) -> BoxFuture + Send>;

struct Call {
    method: &'static str,
    send: Body,
}

/// Calls to the overlay go through one queue so they arrive in order (a
/// "hide preview" must never overtake the "show preview" before it). Bursts
/// of state updates such as the tile preview during a drag are coalesced to
/// the latest one, so a slow overlay never builds up a backlog.
static QUEUE: LazyLock<tokio::sync::mpsc::UnboundedSender<Call>> = LazyLock::new(|| {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Call>();
    RUNTIME.spawn(async move {
        let mut conn: Option<zbus::Connection> = None;
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            // Keep only the last of each coalescable method, in order.
            let mut keep = Vec::with_capacity(batch.len());
            for (i, call) in batch.iter().enumerate() {
                let later = COALESCE.contains(&call.method)
                    && batch[i + 1..].iter().any(|c| c.method == call.method);
                keep.push(!later);
            }
            for (call, keep) in batch.into_iter().zip(keep) {
                if !keep {
                    continue;
                }
                if conn.is_none() {
                    conn = zbus::Connection::session().await.ok();
                }
                let Some(c) = conn.clone() else { continue };
                let method = call.method;
                if let Err(e) = (call.send)(c).await {
                    tracing::debug!(method, error = %e, "overlay call failed");
                }
            }
        }
    });
    tx
});

/// Methods that only carry the latest state.
const COALESCE: &[&str] = &["TilePreview", "LayoutPicker", "SwitcherSelect"];

pub fn overlay_call<B>(method: &'static str, body: B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    let send: Body = Box::new(move |conn: zbus::Connection| {
        Box::pin(async move {
            // Fire and forget: never wait on the overlay's reply.
            let msg = zbus::Message::method_call(dbus::OVERLAY_OBJECT_PATH, method)?
                .destination(dbus::OVERLAY_BUS_NAME)?
                .interface(dbus::OVERLAY_INTERFACE)?
                .with_flags(zbus::message::Flags::NoReplyExpected)?
                .build(&body)?;
            conn.send(&msg).await
        })
    });
    let _ = QUEUE.send(Call { method, send });
}

fn run(cmd: &str, args: &[&str]) {
    match Command::new(cmd).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(cmd, ?args, status = %s, "command failed"),
        Err(e) => tracing::warn!(cmd, error = %e, "could not run"),
    }
}

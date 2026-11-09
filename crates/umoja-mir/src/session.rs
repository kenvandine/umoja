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
    if crate::greeter::enabled() {
        crate::greeter::launch(wayland_display);
        return;
    }
    // Test and nested compositors (UMOJA_NO_SESSION_TARGET) must not touch
    // the user's systemd/D-Bus environment: exporting their WAYLAND_DISPLAY
    // sends every activated app of the real session to the test display.
    if std::env::var_os("UMOJA_NO_SESSION_TARGET").is_some() {
        return;
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
        run(
            "systemctl",
            &["--user", "start", "--no-block", "umoja-session.target"],
        );
    });
}

/// Actions handled by other processes over D-Bus.
pub fn dispatch(action: Action) {
    let method = match action {
        Action::ToggleOverlay => "Toggle",
        Action::ToggleOverview => "ToggleOverview",
        Action::ToggleDictation => "ToggleDictation",
        Action::AskAboutScreen => "AskAboutScreen",
        Action::ClipboardHistory => "ShowClipboard",
        Action::ShiftOverviewUp => {
            overlay_call("ShiftOverview", (true,));
            return;
        }
        Action::ShiftOverviewDown => {
            overlay_call("ShiftOverview", (false,));
            return;
        }
        Action::ToggleMessageTray => {
            panel_call("ToggleMenu", ("calendar",));
            return;
        }
        Action::ScreenshotUi => "ShowScreenshotUI",
        Action::Screenshot => "ScreenshotScreen",
        Action::ScreenRecordingUi => "ShowScreenRecordingUI",
        Action::ShowShortcuts => "ToggleShortcuts",
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
    send_call(
        (
            dbus::OVERLAY_BUS_NAME,
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
        ),
        method,
        body,
    )
}

/// The top bar (umoja-panel): open its calendar or quick settings menu.
pub fn panel_call<B>(method: &'static str, body: B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    send_call(
        (
            dbus::PANEL_BUS_NAME,
            dbus::PANEL_OBJECT_PATH,
            dbus::PANEL_INTERFACE,
        ),
        method,
        body,
    )
}

/// A media or hardware key press for umoja-panel (see media_keys.rs).
pub fn media_key(name: &'static str) {
    send_call(
        (
            dbus::MEDIA_KEYS_BUS_NAME,
            dbus::MEDIA_KEYS_OBJECT_PATH,
            dbus::MEDIA_KEYS_INTERFACE,
        ),
        "Press",
        (name,),
    )
}

/// The dock (umoja-dock): keyboard focus for Ctrl+Alt+Tab.
pub fn dock_call<B>(method: &'static str, body: B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    send_call(
        (
            dbus::DOCK_BUS_NAME,
            dbus::DOCK_OBJECT_PATH,
            dbus::DOCK_INTERFACE,
        ),
        method,
        body,
    )
}

fn send_call<B>(to: (&'static str, &'static str, &'static str), method: &'static str, body: B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    let (name, path, iface) = to;
    let send: Body = Box::new(move |conn: zbus::Connection| {
        Box::pin(async move {
            // Fire and forget: never wait on the reply.
            let msg = zbus::Message::method_call(path, method)?
                .destination(name)?
                .interface(iface)?
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

/// Take `name` back if the bus ever takes it away from `conn`.
///
/// On the test laptop both of our well-known names were dropped at once
/// while the connections stayed up (cause unknown), leaving the overview and
/// the assistant without the compositor. Re-request with a backoff.
pub fn keep_name(conn: zbus::Connection, name: &'static str) {
    use futures_util::StreamExt;
    RUNTIME.spawn(async move {
        let Ok(proxy) = zbus::fdo::DBusProxy::new(&conn).await else {
            return;
        };
        let Ok(mut lost) = proxy.receive_name_lost().await else {
            return;
        };
        while let Some(sig) = lost.next().await {
            if sig.args().is_ok_and(|a| a.name.as_str() != name) {
                continue;
            }
            tracing::warn!(name, "lost bus name; requesting it again");
            let mut delay = std::time::Duration::from_millis(500);
            loop {
                tokio::time::sleep(delay).await;
                match conn.request_name(name).await {
                    Ok(()) => {
                        tracing::info!(name, "bus name reacquired");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(name, error = %e, "could not reacquire bus name");
                        delay = (delay * 2).min(std::time::Duration::from_secs(30));
                    }
                }
            }
        }
    });
}

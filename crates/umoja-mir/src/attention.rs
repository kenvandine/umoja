//! Windows that want attention and apps that stop responding.
//!
//! Umoja keeps GNOME's focus rules relaxed (apps with a token or a recent
//! input event raise themselves straight away). What is left over is
//! GNOME's fallback for the few requests that are still refused: a window
//! that asked to be activated without the user asking (an invalid
//! xdg-activation token), or that raised itself on another workspace, is
//! marked as wanting attention (the dock shows it, as Ubuntu Dock does) and
//! gets windowAttentionHandler.js's notification, "“Title” is ready",
//! which takes you to it when clicked.
//!
//! And mutter's "not responding" check: when the user clicks or closes a
//! window, its client is pinged (xdg_wm_base.ping, through Mir patch 3025);
//! no pong within 5 s (mutter's check-alive-timeout) and the overlay asks
//! "App is not responding" with Force Quit and Wait.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;

/// The Mir patch's ping function: pings every xdg_wm_base of a process.
type PingFn = extern "C" fn(pid: libc::pid_t, serial: u32);
static PING: OnceLock<PingFn> = OnceLock::new();

/// org.gnome.mutter check-alive-timeout's default.
const ALIVE_TIMEOUT: Duration = Duration::from_millis(5000);
/// Don't ping one process more often than this.
const PING_SPACING: Duration = Duration::from_secs(2);

struct Pending {
    serial: u32,
    window: u64,
    app_id: String,
    title: String,
    notified: bool,
}

#[derive(Default)]
struct Pings {
    next_serial: u32,
    pending: HashMap<libc::pid_t, Pending>,
    last: HashMap<libc::pid_t, Instant>,
}

static PINGS: LazyLock<Mutex<Pings>> = LazyLock::new(Default::default);

/// Called by Mir (patch 3025) when xdg-shell comes up.
#[unsafe(no_mangle)]
pub extern "C" fn mir_xdg_ping_setup_hook(ping: PingFn) {
    let _ = PING.set(ping);
}

/// Called by Mir on the Wayland thread for every xdg_wm_base.pong.
#[unsafe(no_mangle)]
pub extern "C" fn mir_xdg_pong_hook(pid: libc::pid_t, serial: u32) {
    let _ = std::panic::catch_unwind(|| {
        let mut p = PINGS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = p.pending.get(&pid) else { return };
        if pending.serial != serial {
            return;
        }
        let pending = p.pending.remove(&pid).expect("checked");
        if pending.notified {
            tracing::info!(pid, app = pending.app_id, "app is responding again");
            crate::session::overlay_call("AppResponding", (pending.window,));
        }
    });
}

/// The user interacted with `window` (a click, a close request): check its
/// client is still alive.
pub fn check_alive(window: u64, pid: libc::pid_t, app_id: String, title: String) {
    let Some(ping) = PING.get().copied() else { return };
    if pid <= 0 || crate::greeter::enabled() {
        return;
    }
    let serial = {
        let mut p = PINGS.lock().unwrap_or_else(|e| e.into_inner());
        if p.pending.contains_key(&pid) {
            return;
        }
        let now = Instant::now();
        if p.last.get(&pid).is_some_and(|t| now.duration_since(*t) < PING_SPACING) {
            return;
        }
        p.last.insert(pid, now);
        if p.last.len() > 256 {
            p.last.retain(|_, t| now.duration_since(*t) < PING_SPACING);
        }
        p.next_serial = p.next_serial.wrapping_add(1).max(1);
        let serial = p.next_serial;
        p.pending.insert(
            pid,
            Pending {
                serial,
                window,
                app_id,
                title,
                notified: false,
            },
        );
        serial
    };
    ping(pid, serial);
    std::thread::spawn(move || {
        std::thread::sleep(ALIVE_TIMEOUT);
        let mut p = PINGS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = p.pending.get_mut(&pid) else { return };
        if pending.serial != serial || pending.notified {
            return;
        }
        pending.notified = true;
        tracing::warn!(pid, app = pending.app_id, "app is not responding");
        crate::session::overlay_call(
            "AppNotResponding",
            (pending.window, pending.app_id.clone(), pending.title.clone()),
        );
    });
}

/// The window closed: forget its ping.
pub fn window_closed(window: u64) {
    let mut p = PINGS.lock().unwrap_or_else(|e| e.into_inner());
    let gone: Vec<libc::pid_t> = p
        .pending
        .iter()
        .filter(|(_, v)| v.window == window)
        .map(|(k, _)| *k)
        .collect();
    for pid in gone {
        if let Some(pending) = p.pending.remove(&pid)
            && pending.notified
        {
            crate::session::overlay_call("AppResponding", (window,));
        }
    }
}

// ---- "is ready" ---------------------------------------------------------

/// notification id -> window, for the notifications we posted.
static NOTIFIED: LazyLock<Mutex<HashMap<u32, u64>>> = LazyLock::new(Default::default);

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// `window` asked for attention: post "“Title” is ready".
pub fn wants_attention(window: u64, app_id: String, title: String) {
    if crate::greeter::enabled() {
        return;
    }
    crate::session::RUNTIME.spawn(async move {
        // Resolved before any await: gio objects aren't Send.
        let (name, icon, desktop) = {
            use gio::prelude::{AppInfoExt, IconExt};
            let app = umoja_common::apps::app_for_window(&app_id);
            let name = app
                .as_ref()
                .map(|i| i.display_name().to_string())
                .unwrap_or_else(|| app_id.clone());
            // The app's own icon (a snap's is a file path) and desktop id; the
            // window's app id is often neither (Fluxer's showed a broken image).
            let icon = app
                .as_ref()
                .and_then(|i| i.icon())
                .and_then(|i| i.to_string())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let desktop = app
                .as_ref()
                .and_then(|i| i.id())
                .map(|s| s.to_string())
                .unwrap_or_else(|| app_id.clone());
            let desktop = desktop.trim_end_matches(".desktop").to_string();
            (name, icon, desktop)
        };
        let Some(conn) = connection().await else { return };
        // windowAttentionHandler.js: "“%s” is ready", the app as source.
        let summary = if title.is_empty() {
            format!("{name} is ready")
        } else {
            format!("“{title}” is ready")
        };
        let mut hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", zbus::zvariant::Value::from(desktop));
        let reply = conn
            .call_method(
                Some(NOTIFICATIONS),
                NOTIFICATIONS_PATH,
                Some(NOTIFICATIONS),
                "Notify",
                &(
                    name.as_str(),
                    0u32,
                    icon.as_str(),
                    summary.as_str(),
                    "",
                    vec!["default", "Show"],
                    hints,
                    -1i32,
                ),
            )
            .await;
        match reply.and_then(|r| r.body().deserialize::<u32>()) {
            Ok(id) => {
                NOTIFIED.lock().unwrap_or_else(|e| e.into_inner()).insert(id, window);
            }
            Err(e) => tracing::debug!(error = %e, "attention notification not sent"),
        }
    });
}

/// The window got focus or closed: its "is ready" notification goes.
pub fn attention_cleared(window: u64) {
    let ids: Vec<u32> = {
        let mut n = NOTIFIED.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<u32> = n.iter().filter(|(_, w)| **w == window).map(|(id, _)| *id).collect();
        for id in &ids {
            n.remove(id);
        }
        ids
    };
    if ids.is_empty() {
        return;
    }
    crate::session::RUNTIME.spawn(async move {
        let Some(conn) = connection().await else { return };
        for id in ids {
            let _ = conn
                .call_method(
                    Some(NOTIFICATIONS),
                    NOTIFICATIONS_PATH,
                    Some(NOTIFICATIONS),
                    "CloseNotification",
                    &(id,),
                )
                .await;
        }
    });
}

static CONN: tokio::sync::OnceCell<Option<zbus::Connection>> = tokio::sync::OnceCell::const_new();

/// Our own session bus connection, listening for clicks on our
/// notifications.
async fn connection() -> Option<zbus::Connection> {
    CONN.get_or_init(|| async {
        let conn = zbus::Connection::session().await.ok()?;
        let listen = conn.clone();
        crate::session::RUNTIME.spawn(async move {
            let Ok(rule) = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(NOTIFICATIONS)
                .and_then(|r| r.member("ActionInvoked"))
                .map(|r| r.build())
            else {
                return;
            };
            let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &listen, None).await else {
                return;
            };
            while let Some(Ok(msg)) = stream.next().await {
                let Ok((id, _action)) = msg.body().deserialize::<(u32, String)>() else { continue };
                let window = NOTIFIED.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                if let Some(window) = window {
                    tokio::task::spawn_blocking(move || crate::shell::activate(window));
                }
            }
        });
        Some(conn)
    })
    .await
    .clone()
}

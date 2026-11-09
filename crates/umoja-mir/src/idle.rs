//! Screen blanking and idle inhibitors, as GNOME does them.
//!
//! Mir dims the screen, turns the outputs off and wakes them on input; the
//! shim (`shim/idle.cpp`) hands it the timeouts worked out here from:
//!
//! * `org.gnome.desktop.session idle-delay`: seconds of inactivity before
//!   the screen blanks (Settings > Power > Screen Blank; 0 = never). Mir
//!   dims the screen for the last 10 seconds, much as gnome-shell fades it
//!   out before blanking.
//! * `org.gnome.desktop.screensaver lock-enabled` / `lock-delay`: lock the
//!   session that long after the screen blanks (Settings > Privacy >
//!   Screen Lock), unless `org.gnome.desktop.lockdown disable-lock-screen`.
//!   The lock goes through logind (`loginctl lock-session`), which
//!   umoja-panel's lock screen listens for.
//! * On the lock screen the outputs go off after 30 seconds, like
//!   gnome-settings-daemon's screensaver blank.
//!
//! Inhibitors keep the screen on while they are held:
//!
//! * Wayland's `zwp_idle_inhibit_manager_v1` (mpv, Firefox, Chromium...)
//!   goes straight to Mir.
//! * `org.freedesktop.ScreenSaver.Inhibit` (browsers, VLC, Electron) and
//!   `org.gnome.SessionManager.Inhibit` with the idle flag (GTK apps, and
//!   the Inhibit portal through xdg-desktop-portal-gnome/-gtk) are served
//!   here, since the Umoja session runs neither gnome-session nor
//!   gnome-settings-daemon. Inhibitors die with their D-Bus connection.

use crate::session::RUNTIME;
use gio::prelude::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use umoja_common::gsettings as gs;
use zbus::interface;
use zbus::message::Header;

const SESSION: &str = "org.gnome.desktop.session";
const SCREENSAVER: &str = "org.gnome.desktop.screensaver";
const LOCKDOWN: &str = "org.gnome.desktop.lockdown";

/// gnome-settings-daemon's SCREENSAVER_TIMEOUT_BLANK.
pub const LOCKED_BLANK_SECONDS: u32 = 30;

/// gnome-session's GsmInhibitorFlag for "inhibit the session being idle".
pub const INHIBIT_IDLE: u32 = 8;

const SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";
const SESSION_MANAGER_NAME: &str = "org.gnome.SessionManager";

unsafe extern "C" {
    pub(crate) fn umoja_idle_set_timeouts(off_seconds: i32, locked_off_seconds: i32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub idle_delay: u32,
    pub lock_enabled: bool,
    pub lock_delay: u32,
    pub lock_disabled: bool,
}

impl Default for Settings {
    /// GNOME's schema defaults.
    fn default() -> Self {
        Self {
            idle_delay: 300,
            lock_enabled: true,
            lock_delay: 0,
            lock_disabled: false,
        }
    }
}

impl Settings {
    fn read() -> Self {
        let d = Self::default();
        let uint = |schema: &str, key: &str, default: u32| match gs::settings(schema) {
            Some(s) if gs::has_key(&s, key) => s.uint(key),
            _ => default,
        };
        Self {
            idle_delay: uint(SESSION, "idle-delay", d.idle_delay),
            lock_enabled: gs::get_bool(SCREENSAVER, "lock-enabled", d.lock_enabled),
            lock_delay: uint(SCREENSAVER, "lock-delay", d.lock_delay),
            lock_disabled: gs::get_bool(LOCKDOWN, "disable-lock-screen", d.lock_disabled),
        }
    }
}

/// Seconds before the outputs turn off (unlocked, locked); 0 = never.
///
/// While the screen is off after idling unlocked, the locked timeout matches the
/// unlocked one. Mir restarts its idle timers when the session locks only if
/// the two differ, and doing so turns the outputs back on for a moment
/// (then straight off, the idle time being past 30s): a flash of the lock
/// screen just after blanking. Once the screen wakes, the lock screen's own
/// 30 seconds apply again.
pub fn timeouts(s: &Settings, inhibited: bool, blanked: bool) -> (u32, u32) {
    if inhibited {
        return (0, 0);
    }
    if blanked && s.idle_delay > 0 {
        return (s.idle_delay, s.idle_delay);
    }
    (s.idle_delay, LOCKED_BLANK_SECONDS)
}

/// How long after the screen blanks the session locks, if it does.
pub fn lock_after_blank(s: &Settings) -> Option<Duration> {
    (s.lock_enabled && !s.lock_disabled).then(|| Duration::from_secs(s.lock_delay.into()))
}

#[derive(Debug, Clone)]
struct Inhibitor {
    owner: String,
    app: String,
    flags: u32,
}

#[derive(Default)]
struct State {
    settings: Settings,
    inhibitors: HashMap<u32, Inhibitor>,
    last_cookie: u32,
    /// Mir counts idle time from the last input and can't be poked from
    /// outside, so when the last inhibitor goes (or an app reports user
    /// activity) the timeouts stay off for one more idle-delay. Otherwise a
    /// film that ends after an hour without input would blank at once.
    hold_until: Option<Instant>,
    blanked: bool,
    /// The session lock (ext-session-lock) is engaged.
    locked: bool,
    /// The screen went off while unlocked and hasn't woken since; see
    /// [`timeouts`].
    blanked_unlocked: bool,
    /// Bumped on every blank and wake, so a pending lock-after-blank can
    /// tell the screen came back in the meantime.
    generation: u64,
    applied: Option<(u32, u32)>,
}

impl State {
    fn idle_inhibited(&self) -> bool {
        self.inhibitors
            .values()
            .any(|i| i.flags & INHIBIT_IDLE != 0)
    }

    fn held(&self) -> bool {
        self.hold_until.is_some_and(|t| Instant::now() < t)
    }

    fn hold(&mut self) {
        if self.settings.idle_delay > 0 {
            let until = Instant::now() + Duration::from_secs(self.settings.idle_delay.into());
            self.hold_until = Some(until);
            RUNTIME.spawn(async move {
                tokio::time::sleep_until(until.into()).await;
                apply(&mut state());
            });
        }
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn apply(st: &mut State) {
    if !st.held() {
        st.hold_until = None;
    }
    let t = timeouts(
        &st.settings,
        st.idle_inhibited() || st.held(),
        st.blanked_unlocked,
    );
    if st.applied == Some(t) {
        return;
    }
    st.applied = Some(t);
    tracing::info!(
        off = t.0,
        locked_off = t.1,
        inhibitors = st.inhibitors.len(),
        held = st.hold_until.is_some(),
        "screen blank timeouts"
    );
    let clamp = |v: u32| v.min(i32::MAX as u32) as i32;
    unsafe { umoja_idle_set_timeouts(clamp(t.0), clamp(t.1)) };
}

/// Read the settings, follow changes, and serve the inhibit interfaces.
pub fn start() {
    {
        let mut st = state();
        st.settings = Settings::read();
        apply(&mut st);
    }
    std::thread::Builder::new()
        .name("umoja-idle-settings".into())
        .spawn(|| {
            let ctx = glib::MainContext::new();
            let _guard = ctx.acquire();
            let _ = ctx.with_thread_default(|| {
                let keep: Vec<gio::Settings> = [SESSION, SCREENSAVER, LOCKDOWN]
                    .into_iter()
                    .filter_map(gs::settings)
                    .inspect(|s| {
                        s.connect_changed(None, |_, _| {
                            let mut st = state();
                            st.settings = Settings::read();
                            apply(&mut st);
                        });
                    })
                    .collect();
                glib::MainLoop::new(Some(&ctx), false).run();
                drop(keep);
            });
        })
        .expect("spawn idle settings thread");
    serve_inhibitors();
}

/// Mir's idle state changed (`UMOJA_IDLE_*`); called on a Mir thread,
/// sometimes from inside `umoja_idle_set_timeouts` while [`apply`] holds the
/// state lock, so the work is moved off that thread.
pub fn mir_state(code: u32) {
    RUNTIME.spawn(async move { on_mir_state(code) });
}

fn on_mir_state(code: u32) {
    let mut st = state();
    match code {
        1 => tracing::info!("screen dimmed"),
        2 => {
            tracing::info!("screen off");
            st.blanked = true;
            st.blanked_unlocked = !st.locked;
            st.generation += 1;
            apply(&mut st);
            if let Some(delay) = lock_after_blank(&st.settings) {
                let generation = st.generation;
                RUNTIME.spawn(async move {
                    tokio::time::sleep(delay).await;
                    let still_blank = {
                        let st = state();
                        st.blanked && st.generation == generation
                    };
                    if still_blank {
                        lock_session().await;
                    }
                });
            }
        }
        3 => {
            tracing::info!("screen on");
            st.blanked = false;
            st.blanked_unlocked = false;
            st.generation += 1;
            apply(&mut st);
        }
        4 => st.locked = true,
        5 => st.locked = false,
        _ => {}
    }
}

async fn lock_session() {
    tracing::info!("locking the session after the screen blanked");
    let session = std::env::var("XDG_SESSION_ID").unwrap_or_default();
    let mut cmd = tokio::process::Command::new("loginctl");
    cmd.arg("lock-session");
    if !session.is_empty() {
        cmd.arg(&session);
    }
    match cmd.status().await {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(status = %s, "loginctl lock-session failed"),
        Err(e) => tracing::warn!(error = %e, "could not run loginctl"),
    }
}

// ---- inhibitors -------------------------------------------------------------

fn add_inhibitor(owner: String, app: String, reason: String, flags: u32) -> u32 {
    let mut st = state();
    let mut cookie = st.last_cookie;
    loop {
        cookie = cookie.wrapping_add(1).max(1);
        if !st.inhibitors.contains_key(&cookie) {
            break;
        }
    }
    st.last_cookie = cookie;
    tracing::info!(cookie, %owner, %app, %reason, flags, "inhibitor added");
    st.inhibitors
        .insert(cookie, Inhibitor { owner, app, flags });
    apply(&mut st);
    cookie
}

fn remove_where(f: impl Fn(u32, &Inhibitor) -> bool) {
    let mut st = state();
    let was = st.idle_inhibited();
    let before = st.inhibitors.len();
    st.inhibitors.retain(|&c, i| {
        let drop = f(c, i);
        if drop {
            tracing::info!(cookie = c, owner = %i.owner, app = %i.app, "inhibitor removed");
        }
        !drop
    });
    if st.inhibitors.len() == before {
        return;
    }
    if was && !st.idle_inhibited() {
        st.hold();
    }
    apply(&mut st);
}

fn sender(hdr: &Header<'_>) -> String {
    hdr.sender().map(|s| s.to_string()).unwrap_or_default()
}

/// `org.freedesktop.ScreenSaver`, as KDE and the freedesktop spec define it.
struct ScreenSaver;

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    fn inhibit(
        &self,
        application_name: String,
        reason_for_inhibit: String,
        #[zbus(header)] hdr: Header<'_>,
    ) -> u32 {
        add_inhibitor(
            sender(&hdr),
            application_name,
            reason_for_inhibit,
            INHIBIT_IDLE,
        )
    }

    fn un_inhibit(&self, cookie: u32, #[zbus(header)] hdr: Header<'_>) {
        let who = sender(&hdr);
        remove_where(|c, i| c == cookie && i.owner == who);
    }

    fn get_active(&self) -> bool {
        state().blanked
    }

    fn set_active(&self, active: bool) -> bool {
        if active {
            RUNTIME.spawn(lock_session());
        }
        false
    }

    fn get_active_time(&self) -> u32 {
        0
    }

    fn get_session_idle_time(&self) -> u32 {
        0
    }

    fn lock(&self) {
        RUNTIME.spawn(lock_session());
    }

    /// Restart the blank countdown (screen stays on for another idle-delay).
    fn simulate_user_activity(&self) {
        let mut st = state();
        st.hold();
        apply(&mut st);
    }
}

/// The inhibitor part of gnome-session's `org.gnome.SessionManager`. The
/// rest of it (logout, client registration) isn't offered: logind handles
/// that in Umoja.
struct SessionManager;

#[interface(name = "org.gnome.SessionManager")]
impl SessionManager {
    fn inhibit(
        &self,
        app_id: String,
        _toplevel_xid: u32,
        reason: String,
        flags: u32,
        #[zbus(header)] hdr: Header<'_>,
    ) -> u32 {
        add_inhibitor(sender(&hdr), app_id, reason, flags)
    }

    fn uninhibit(&self, inhibit_cookie: u32) {
        remove_where(|c, _| c == inhibit_cookie);
    }

    fn is_inhibited(&self, flags: u32) -> bool {
        state().inhibitors.values().any(|i| i.flags & flags != 0)
    }

    #[zbus(property)]
    fn inhibited_actions(&self) -> u32 {
        state().inhibitors.values().fold(0, |a, i| a | i.flags)
    }

    #[zbus(property)]
    fn session_is_active(&self) -> bool {
        true
    }
}

/// Test compositors (UMOJA_NO_SESSION_TARGET) usually share the real
/// session's bus and must not take these names from it; the headless test
/// runs its own bus and asks for them with UMOJA_IDLE_BUS=1.
fn own_bus_names() -> bool {
    std::env::var_os("UMOJA_NO_SESSION_TARGET").is_none()
        || std::env::var("UMOJA_IDLE_BUS").is_ok_and(|v| v == "1")
}

fn serve_inhibitors() {
    if !own_bus_names() {
        return;
    }
    RUNTIME.spawn(async move {
        let built = match zbus::connection::Builder::session()
            .and_then(|b| b.serve_at("/org/freedesktop/ScreenSaver", ScreenSaver))
            .and_then(|b| b.serve_at("/ScreenSaver", ScreenSaver))
            .and_then(|b| b.serve_at("/org/gnome/SessionManager", SessionManager))
        {
            Ok(b) => b.build().await,
            Err(e) => Err(e),
        };
        let conn = match built {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "idle inhibitor service unavailable");
                return;
            }
        };
        for name in [SCREENSAVER_NAME, SESSION_MANAGER_NAME] {
            let flags = zbus::fdo::RequestNameFlags::DoNotQueue.into();
            match conn.request_name_with_flags(name, flags).await {
                Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => tracing::info!(name, "serving"),
                Ok(reply) => tracing::warn!(name, ?reply, "bus name taken; not serving it"),
                Err(e) => tracing::warn!(name, error = %e, "could not own bus name"),
            }
        }
        watch_owners(&conn).await;
        std::mem::forget(conn);
    });
}

/// Drop an app's inhibitors when it leaves the bus (exits or crashes).
async fn watch_owners(conn: &zbus::Connection) {
    use futures_util::StreamExt;
    let Ok(proxy) = zbus::fdo::DBusProxy::new(conn).await else {
        return;
    };
    let Ok(mut changes) = proxy.receive_name_owner_changed().await else {
        return;
    };
    RUNTIME.spawn(async move {
        while let Some(sig) = changes.next().await {
            let Ok(args) = sig.args() else { continue };
            if args.new_owner.is_some() || !args.name.starts_with(':') {
                continue;
            }
            let gone = args.name.to_string();
            remove_where(|_, i| i.owner == gone);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_after_idle_delay() {
        let s = Settings {
            idle_delay: 300,
            ..Default::default()
        };
        assert_eq!(timeouts(&s, false, false), (300, LOCKED_BLANK_SECONDS));
    }

    #[test]
    fn never_blanks_when_set_to_never() {
        let s = Settings {
            idle_delay: 0,
            ..Default::default()
        };
        assert_eq!(timeouts(&s, false, false).0, 0);
        // The lock screen still turns the screen off, as in GNOME.
        assert_eq!(timeouts(&s, false, false).1, LOCKED_BLANK_SECONDS);
    }

    #[test]
    fn locking_after_blank_keeps_screen_off() {
        let s = Settings::default();
        assert_eq!(timeouts(&s, false, true), (300, 300));
        let never = Settings {
            idle_delay: 0,
            ..Default::default()
        };
        assert_eq!(timeouts(&never, false, true), (0, LOCKED_BLANK_SECONDS));
    }

    #[test]
    fn inhibitor_keeps_screen_on() {
        assert_eq!(timeouts(&Settings::default(), true, false), (0, 0));
    }

    #[test]
    fn lock_follows_screensaver_settings() {
        let mut s = Settings::default();
        assert_eq!(lock_after_blank(&s), Some(Duration::ZERO));
        s.lock_delay = 30;
        assert_eq!(lock_after_blank(&s), Some(Duration::from_secs(30)));
        s.lock_enabled = false;
        assert_eq!(lock_after_blank(&s), None);
        s.lock_enabled = true;
        s.lock_disabled = true;
        assert_eq!(lock_after_blank(&s), None);
    }
}

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
//! * `org.umoja.shell screensaver-enabled` / `screensaver-duration`: at
//!   idle-delay the Umoja screensaver (umoja-screensaver) starts instead,
//!   and the screen blanks that many seconds later. Input dismisses it; if
//!   the lock is due by then (lock-delay after the screensaver started),
//!   the session locks before the screensaver goes, so the desktop is
//!   never shown unlocked. Mir only reports idling as "dim" 10 seconds
//!   before "off", so the off timeout is idle-delay + 10 while waiting and
//!   the dim marks idle-delay.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use umoja_common::gsettings as gs;
use zbus::interface;
use zbus::message::Header;

const SESSION: &str = "org.gnome.desktop.session";
const SCREENSAVER: &str = "org.gnome.desktop.screensaver";
const LOCKDOWN: &str = "org.gnome.desktop.lockdown";

/// Mir dims this long before turning the outputs off.
pub const DIM_LEAD_SECONDS: u32 = 10;

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
    pub saver_enabled: bool,
    pub saver_duration: u32,
}

impl Default for Settings {
    /// GNOME's schema defaults.
    fn default() -> Self {
        Self {
            idle_delay: 300,
            lock_enabled: true,
            lock_delay: 0,
            lock_disabled: false,
            saver_enabled: true,
            saver_duration: 300,
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
            saver_enabled: gs::get_bool(gs::UMOJA_SHELL, "screensaver-enabled", d.saver_enabled),
            saver_duration: uint(gs::UMOJA_SHELL, "screensaver-duration", d.saver_duration),
        }
    }
}

/// Where the screensaver is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Stage {
    /// Not showing; starts at the next idle-delay if enabled.
    #[default]
    Armed,
    /// Showing since `since`; `manual` from `umoja-screensaver --activate`.
    Showing { manual: bool, since: Instant },
}

/// Seconds before the outputs turn off (unlocked, locked); 0 = never.
/// `saver` says whether the screensaver is on and installed.
///
/// While the screen is off after idling unlocked, the locked timeout matches the
/// unlocked one. Mir restarts its idle timers when the session locks only if
/// the two differ, and doing so turns the outputs back on for a moment
/// (then straight off, the idle time being past 30s): a flash of the lock
/// screen just after blanking. Once the screen wakes, the lock screen's own
/// 30 seconds apply again.
pub fn timeouts(
    s: &Settings,
    inhibited: bool,
    blanked: bool,
    stage: Stage,
    saver: bool,
) -> (u32, u32) {
    let off = match stage {
        Stage::Showing { manual: true, .. } => s.saver_duration,
        _ if inhibited => return (0, 0),
        Stage::Showing { manual: false, .. } if s.saver_duration == 0 => 0,
        Stage::Showing { manual: false, .. } => s.idle_delay + s.saver_duration,
        Stage::Armed if saver && s.idle_delay > 0 => s.idle_delay + DIM_LEAD_SECONDS,
        Stage::Armed => s.idle_delay,
    };
    if blanked && off > 0 {
        return (off, off);
    }
    (off, LOCKED_BLANK_SECONDS)
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
    stage: Stage,
    /// Input before this is ignored (the key that ran --activate).
    grace_until: Option<Instant>,
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
/// The screensaver is up (or the screen blanked after it): the next input
/// ends it. Read on every input event, so kept outside the lock.
static EPISODE: AtomicBool = AtomicBool::new(false);
static DISMISSING: AtomicBool = AtomicBool::new(false);
/// The connection serving org.freedesktop.ScreenSaver, for ActiveChanged.
static SCREENSAVER_CONN: std::sync::OnceLock<zbus::Connection> = std::sync::OnceLock::new();

/// Tell clients the screen blanked (`true`) or came back (`false`). The
/// panel and dock wait for `false` before mapping their surfaces
/// (umoja_common::screen).
fn emit_active_changed(active: bool) {
    let Some(conn) = SCREENSAVER_CONN.get().cloned() else {
        return;
    };
    RUNTIME.spawn(async move {
        for path in ["/org/freedesktop/ScreenSaver", "/ScreenSaver"] {
            if let Err(e) = conn
                .emit_signal(
                    None::<&str>,
                    path,
                    SCREENSAVER_NAME,
                    "ActiveChanged",
                    &(active,),
                )
                .await
            {
                tracing::debug!(error = %e, "ActiveChanged not sent");
            }
        }
    });
}

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn apply(st: &mut State) {
    if !st.held() {
        st.hold_until = None;
    }
    let inhibited = st.idle_inhibited() || st.held();
    let saver = saver_on(&st.settings);
    // An inhibitor (or the screensaver being turned off) ends an automatic
    // screensaver, as it would have kept the session from idling at all.
    if let Stage::Showing { manual, .. } = st.stage
        && !st.blanked
        && ((inhibited && !manual) || (!saver && !manual))
    {
        end_screensaver(st);
    }
    let t = timeouts(
        &st.settings,
        inhibited,
        st.blanked_unlocked,
        st.stage,
        saver,
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
                let keep: Vec<gio::Settings> = [SESSION, SCREENSAVER, LOCKDOWN, gs::UMOJA_SHELL]
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
        1 => {
            tracing::info!("screen dimmed");
            if st.stage == Stage::Armed
                && saver_on(&st.settings)
                && !st.locked
                && !st.blanked
                && !(st.idle_inhibited() || st.held())
            {
                start_screensaver(&mut st, false);
            }
        }
        2 => {
            tracing::info!("screen off");
            st.blanked = true;
            st.blanked_unlocked = !st.locked;
            st.generation += 1;
            // Nothing to animate for a dark screen. The stage stays, so the
            // timeouts (and with them the outputs) don't change until input.
            crate::screensaver::stop();
            apply(&mut st);
            emit_active_changed(true);
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
            emit_active_changed(false);
        }
        4 => {
            st.locked = true;
            if matches!(st.stage, Stage::Showing { .. }) {
                end_screensaver(&mut st);
                apply(&mut st);
            }
        }
        5 => st.locked = false,
        _ => {}
    }
}

fn saver_on(s: &Settings) -> bool {
    s.saver_enabled && crate::screensaver::available()
}

fn start_screensaver(st: &mut State, manual: bool) -> bool {
    let now = Instant::now();
    st.stage = Stage::Showing { manual, since: now };
    st.grace_until = Some(now + Duration::from_millis(if manual { 1500 } else { 300 }));
    EPISODE.store(true, Ordering::SeqCst);
    let started = crate::screensaver::start(on_screensaver_exit);
    if !started {
        st.stage = Stage::Armed;
        EPISODE.store(false, Ordering::SeqCst);
    }
    apply(st);
    started
}

/// Back to waiting for the next idle (the caller applies the timeouts).
fn end_screensaver(st: &mut State) {
    crate::screensaver::stop();
    st.stage = Stage::Armed;
    st.grace_until = None;
    EPISODE.store(false, Ordering::SeqCst);
}

fn on_screensaver_exit(expected: bool) {
    if !expected {
        // It crashed or was killed: whatever it covered is visible now, so
        // treat it as the user coming back (and lock if that's due).
        RUNTIME.spawn(dismiss(true));
    }
}

/// `umoja-screensaver --activate`: start it now, as idling would. The
/// screen blanks screensaver-duration seconds after the last input.
pub fn activate_screensaver() -> bool {
    let mut st = state();
    if st.locked || !crate::screensaver::available() {
        return false;
    }
    start_screensaver(&mut st, true)
}

/// Any input event (from the shim's event filter, on Mir's input thread).
pub fn input_activity() {
    if !EPISODE.load(Ordering::Relaxed) || DISMISSING.swap(true, Ordering::SeqCst) {
        return;
    }
    RUNTIME.spawn(dismiss(false));
}

/// The user is back: lock first if the lock is due, then drop the
/// screensaver and re-arm.
async fn dismiss(crashed: bool) {
    let lock_first = {
        let st = state();
        if !crashed && st.grace_until.is_some_and(|g| Instant::now() < g) {
            DISMISSING.store(false, Ordering::SeqCst);
            return;
        }
        let Stage::Showing { since, .. } = st.stage else {
            DISMISSING.store(false, Ordering::SeqCst);
            return;
        };
        EPISODE.store(false, Ordering::SeqCst);
        // After a blank the lock was handled there (see on_mir_state).
        !st.locked
            && (crashed || crate::screensaver::running())
            && lock_after_blank(&st.settings).is_some_and(|d| since.elapsed() >= d)
    };
    if lock_first {
        lock_session().await;
        // Keep the screensaver up until the lock screen covers the desktop.
        for _ in 0..60 {
            if state().locked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    {
        let mut st = state();
        if matches!(st.stage, Stage::Showing { .. }) {
            end_screensaver(&mut st);
            apply(&mut st);
        }
    }
    tracing::info!(locked = lock_first, "screensaver dismissed");
    DISMISSING.store(false, Ordering::SeqCst);
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
        let _ = SCREENSAVER_CONN.set(conn.clone());
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
        assert_eq!(
            timeouts(&s, false, false, Stage::Armed, false),
            (300, LOCKED_BLANK_SECONDS)
        );
    }

    #[test]
    fn never_blanks_when_set_to_never() {
        let s = Settings {
            idle_delay: 0,
            ..Default::default()
        };
        assert_eq!(timeouts(&s, false, false, Stage::Armed, false).0, 0);
        // The lock screen still turns the screen off, as in GNOME.
        assert_eq!(
            timeouts(&s, false, false, Stage::Armed, false).1,
            LOCKED_BLANK_SECONDS
        );
    }

    #[test]
    fn locking_after_blank_keeps_screen_off() {
        let s = Settings::default();
        assert_eq!(timeouts(&s, false, true, Stage::Armed, false), (300, 300));
        let never = Settings {
            idle_delay: 0,
            ..Default::default()
        };
        assert_eq!(
            timeouts(&never, false, true, Stage::Armed, false),
            (0, LOCKED_BLANK_SECONDS)
        );
    }

    #[test]
    fn screensaver_runs_before_the_blank() {
        let s = Settings::default();
        // Waiting: Mir dims (our cue) at idle-delay, 10s before its off.
        assert_eq!(timeouts(&s, false, false, Stage::Armed, true).0, 310);
        let since = Instant::now();
        let auto = Stage::Showing {
            manual: false,
            since,
        };
        assert_eq!(timeouts(&s, false, false, auto, true).0, 600);
        // --activate counts from the last input.
        let manual = Stage::Showing {
            manual: true,
            since,
        };
        assert_eq!(timeouts(&s, false, false, manual, true).0, 300);
        assert_eq!(timeouts(&s, true, false, manual, true).0, 300);
        let forever = Settings {
            saver_duration: 0,
            ..s
        };
        assert_eq!(timeouts(&forever, false, false, auto, true).0, 0);
        let never = Settings { idle_delay: 0, ..s };
        assert_eq!(timeouts(&never, false, false, Stage::Armed, true).0, 0);
        assert_eq!(timeouts(&s, true, false, Stage::Armed, true), (0, 0));
    }

    #[test]
    fn inhibitor_keeps_screen_on() {
        assert_eq!(
            timeouts(&Settings::default(), true, false, Stage::Armed, false),
            (0, 0)
        );
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

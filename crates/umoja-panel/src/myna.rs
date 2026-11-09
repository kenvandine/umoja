//! Myna dictation, integrated the way Ubuntu 26.10's GNOME session does it.
//!
//! In Ubuntu the pieces are:
//!
//! * the `myna` snap's daemon (`myna-desktop`), which owns
//!   `com.canonical.Myna.Dictation` on the session bus and publishes the
//!   dictation `State` (`idle`, `loading`, `recording`, `transcribing`,
//!   `finalizing`, `notice`, `error`);
//! * Myna Settings (`myna-config`), which binds the shortcut (Super+J by
//!   default) as a GNOME custom keybinding whose command calls `Toggle`.
//!   umoja-mir runs GNOME custom keybindings itself (see its `input.rs`);
//! * the `myna-shell@canonical.com` GNOME Shell extension, which runs the
//!   `myna.hud` renderer (the bottom-centre pill with the level wave) for as
//!   long as the daemon is on the bus, and calls `RegisterClient` first so
//!   the daemon shows the pill rather than a notification.
//!
//! This module is that extension for Umoja: it hosts the same renderer, as
//! a layer-shell surface (`myna.hud --host layer-shell`, the HUD's own
//! placement for wlr-layer-shell compositors), with the extension's
//! registration and respawn policy. It also shows a microphone in the top
//! bar while Myna records, as GNOME Shell's privacy indicator does while the
//! snap captures audio; clicking it stops dictation.

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use umoja_common::{dbus, gsettings as gs, i18n};

/// The extension's override for a locally built renderer.
const HUD_OVERRIDE_ENV: &str = "MYNA_HUD_BINARY";
/// The renderer exits with this when the session cannot host it.
const EX_CONFIG: i32 = 78;
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 30_000;
/// Consecutive quick failures before the host gives up until the daemon
/// next appears (respawn.js's RESTART_BUDGET).
const RESTART_BUDGET: u32 = 5;
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
const KILL_GRACE: Duration = Duration::from_secs(3);
const MYNA_SHORTCUT_PATH: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/myna/";

/// What the top bar shows for a `State` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indication {
    Hidden,
    Recording,
    Working,
}

pub fn indication(state: &str) -> Indication {
    match state {
        "recording" | "active" => Indication::Recording,
        "loading" | "transcribing" | "finalizing" => Indication::Working,
        _ => Indication::Hidden,
    }
}

/// The renderer's argv, resolved as the extension's `resolve.js` does.
fn hud_argv() -> Option<Vec<String>> {
    let host = ["--host".to_string(), "layer-shell".to_string()];
    if let Some(path) = std::env::var_os(HUD_OVERRIDE_ENV).filter(|p| !p.is_empty()) {
        let path = path.to_string_lossy().into_owned();
        return std::path::Path::new(&path)
            .is_file()
            .then(|| [vec![path], host.to_vec()].concat());
    }
    glib::find_program_in_path("snap").map(|snap| {
        [
            vec![
                snap.to_string_lossy().into_owned(),
                "run".into(),
                "myna.hud".into(),
            ],
            host.to_vec(),
        ]
        .concat()
    })
}

/// The next respawn delay after `failures` consecutive quick failures, or
/// `None` once the budget is spent.
pub fn backoff(failures: u32) -> Option<Duration> {
    if failures == 0 || failures > RESTART_BUDGET {
        return None;
    }
    let ms = BASE_BACKOFF_MS.saturating_mul(1 << (failures - 1).min(16));
    Some(Duration::from_millis(ms.min(MAX_BACKOFF_MS)))
}

/// "<Super>j" → "Super+J", for the tooltip.
pub fn pretty_accelerator(accel: &str) -> String {
    let mut parts = Vec::new();
    let mut rest = accel.trim();
    while let Some(stripped) = rest.strip_prefix('<') {
        let Some(end) = stripped.find('>') else { break };
        let m = &stripped[..end];
        parts.push(match m.to_ascii_lowercase().as_str() {
            "primary" | "control" | "ctrl" => "Ctrl".to_string(),
            "mod4" | "meta" => "Super".to_string(),
            "mod1" => "Alt".to_string(),
            _ => {
                let mut c = m.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            }
        });
        rest = &stripped[end + 1..];
    }
    if !rest.is_empty() {
        parts.push(if rest.chars().count() == 1 {
            rest.to_uppercase()
        } else {
            rest.to_string()
        });
    }
    parts.join("+")
}

struct Hud {
    /// The daemon is on the bus and we mean to show the pill.
    wanted: Cell<bool>,
    child: RefCell<Option<gio::Subprocess>>,
    started: Cell<Option<Instant>>,
    failures: Cell<u32>,
    /// Bumped on every start/stop, so a stale exit or timer does nothing.
    generation: Cell<u64>,
    registered: Cell<bool>,
}

pub struct Myna {
    button: gtk::Button,
    proxy: RefCell<Option<gio::DBusProxy>>,
    hud: Hud,
}

impl Myna {
    pub fn start() -> Rc<Self> {
        let button = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .css_classes(["flat", "myna-indicator"])
            .visible(false)
            .build();
        let this = Rc::new(Self {
            button,
            proxy: RefCell::new(None),
            hud: Hud {
                wanted: Cell::new(false),
                child: RefCell::new(None),
                started: Cell::new(None),
                failures: Cell::new(0),
                generation: Cell::new(0),
                registered: Cell::new(false),
            },
        });
        let weak = Rc::downgrade(&this);
        this.button.connect_clicked(move |_| {
            if let Some(m) = weak.upgrade() {
                m.call("Toggle");
            }
        });
        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            let proxy = gio::DBusProxy::for_bus_future(
                gio::BusType::Session,
                gio::DBusProxyFlags::DO_NOT_AUTO_START
                    | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                None,
                dbus::MYNA_BUS_NAME,
                dbus::MYNA_OBJECT_PATH,
                dbus::MYNA_INTERFACE,
            )
            .await;
            let Some(this) = weak.upgrade() else { return };
            let proxy = match proxy {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "myna: no session bus proxy");
                    return;
                }
            };
            let w = Rc::downgrade(&this);
            proxy.connect_g_properties_changed(move |_, _, _| {
                if let Some(m) = w.upgrade() {
                    m.update();
                }
            });
            let w = Rc::downgrade(&this);
            proxy.connect_notify_local(Some("g-name-owner"), move |_, _| {
                if let Some(m) = w.upgrade() {
                    m.owner_changed();
                }
            });
            *this.proxy.borrow_mut() = Some(proxy);
            this.owner_changed();
        });
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.button.clone().upcast()
    }

    fn proxy(&self) -> Option<gio::DBusProxy> {
        self.proxy.borrow().clone()
    }

    fn owner_changed(self: &Rc<Self>) {
        let owned = self.proxy().and_then(|p| p.name_owner()).is_some();
        tracing::info!(owned, "myna: dictation daemon");
        if owned {
            self.hud.failures.set(0);
            self.hud.registered.set(false);
            self.hud.wanted.set(true);
            self.spawn_hud();
        } else {
            // The daemon's registry went with it: nothing to unregister.
            self.hud.registered.set(false);
            self.stop_hud();
        }
        self.update();
    }

    fn state(&self) -> String {
        self.proxy()
            .and_then(|p| p.cached_property("State"))
            .and_then(|v| v.str().map(String::from))
            .unwrap_or_default()
    }

    fn update(&self) {
        let shown = indication(&self.state());
        let b = &self.button;
        b.set_visible(shown != Indication::Hidden);
        let key = gs::settings_at(gs::CUSTOM_KEYBINDING, MYNA_SHORTCUT_PATH)
            .map(|s| s.string("binding").to_string())
            .filter(|k| !k.is_empty())
            .map(|k| pretty_accelerator(&k));
        let tip = match (shown, key) {
            (Indication::Recording, Some(k)) => {
                format!(
                    "{} ({k})",
                    i18n::umoja("Dictating. Click or press the shortcut to stop")
                )
            }
            (Indication::Recording, None) => i18n::umoja("Dictating. Click to stop"),
            _ => i18n::umoja("Transcribing"),
        };
        b.set_tooltip_text(Some(&tip));
        b.update_property(&[gtk::accessible::Property::Label(&tip)]);
        if shown == Indication::Recording {
            b.add_css_class("recording");
        } else {
            b.remove_css_class("recording");
        }
    }

    fn call(&self, method: &str) {
        let Some(proxy) = self.proxy() else { return };
        if proxy.name_owner().is_none() {
            return;
        }
        let method = method.to_string();
        proxy.call(
            &method.clone(),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r {
                    tracing::warn!(method, error = %e, "myna: call failed");
                }
            },
        );
    }

    fn register(&self) {
        if !self.hud.registered.replace(true) {
            self.call("RegisterClient");
        }
    }

    fn unregister(&self) {
        if self.hud.registered.replace(false) {
            self.call("UnregisterClient");
        }
    }

    fn spawn_hud(self: &Rc<Self>) {
        if !self.hud.wanted.get() || self.hud.child.borrow().is_some() {
            return;
        }
        let Some(argv) = hud_argv() else {
            tracing::info!("myna: no HUD renderer (snap run myna.hud) to host");
            return;
        };
        // Tell the daemon a pill is coming before the renderer takes its
        // seconds to start, so the first dictation isn't a notification.
        self.register();
        let args: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        let child = match gio::Subprocess::newv(&args, gio::SubprocessFlags::NONE) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, ?argv, "myna: could not start the HUD");
                self.hud_exited(None);
                return;
            }
        };
        tracing::info!(?argv, "myna: HUD started");
        let generation = self.hud.generation.get() + 1;
        self.hud.generation.set(generation);
        self.hud.started.set(Some(Instant::now()));
        *self.hud.child.borrow_mut() = Some(child.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let _ = child.wait_future().await;
            let Some(this) = weak.upgrade() else { return };
            if this.hud.generation.get() != generation {
                return;
            }
            this.hud.child.borrow_mut().take();
            let code = child.has_exited().then(|| child.exit_status());
            this.hud_exited(code);
        });
    }

    fn hud_exited(self: &Rc<Self>, code: Option<i32>) {
        if !self.hud.wanted.get() {
            return;
        }
        if code == Some(EX_CONFIG) {
            tracing::warn!("myna: the HUD cannot run in this session; not hosting it");
            self.hud.wanted.set(false);
            self.unregister();
            return;
        }
        let healthy = self
            .hud
            .started
            .get()
            .is_some_and(|t| t.elapsed() >= HEALTHY_UPTIME);
        let failures = if healthy {
            1
        } else {
            self.hud.failures.get() + 1
        };
        self.hud.failures.set(failures);
        let Some(delay) = backoff(failures) else {
            tracing::warn!(
                ?code,
                "myna: the HUD keeps exiting; giving up until Myna restarts"
            );
            self.hud.wanted.set(false);
            self.unregister();
            return;
        };
        tracing::info!(?code, ?delay, "myna: HUD exited; restarting");
        let generation = self.hud.generation.get();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(this) = weak.upgrade()
                && this.hud.generation.get() == generation
            {
                this.spawn_hud();
            }
        });
    }

    fn stop_hud(&self) {
        self.hud.wanted.set(false);
        self.hud.generation.set(self.hud.generation.get() + 1);
        if let Some(child) = self.hud.child.borrow_mut().take() {
            child.send_signal(libc::SIGTERM);
            glib::timeout_add_local_once(KILL_GRACE, move || {
                // Still running: it has not been reaped yet.
                if child.identifier().is_some() {
                    child.force_exit();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indication_follows_state() {
        assert_eq!(indication("recording"), Indication::Recording);
        assert_eq!(indication("loading"), Indication::Working);
        assert_eq!(indication("finalizing"), Indication::Working);
        assert_eq!(indication("idle"), Indication::Hidden);
        assert_eq!(indication("error"), Indication::Hidden);
        assert_eq!(indication(""), Indication::Hidden);
    }

    #[test]
    fn backoff_doubles_then_gives_up() {
        assert_eq!(backoff(1), Some(Duration::from_millis(500)));
        assert_eq!(backoff(2), Some(Duration::from_millis(1000)));
        assert_eq!(backoff(5), Some(Duration::from_millis(8000)));
        assert_eq!(backoff(6), None);
        assert_eq!(backoff(0), None);
    }

    #[test]
    fn accelerators_read_like_gnome_settings() {
        assert_eq!(pretty_accelerator("<Super>j"), "Super+J");
        assert_eq!(pretty_accelerator("<Primary><Alt>space"), "Ctrl+Alt+space");
        assert_eq!(pretty_accelerator("<Shift><Super>F5"), "Shift+Super+F5");
    }
}

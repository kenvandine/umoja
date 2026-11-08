//! Output volume via WirePlumber's `wpctl`, polled so that media keys and
//! other apps changing the volume show up in the panel icon and slider.
//! TODO: talk to PipeWire directly instead of polling.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const POLL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VolumeState {
    pub available: bool,
    pub volume: f64,
    pub muted: bool,
}

impl VolumeState {
    /// gnome-shell's volume icon thresholds.
    pub fn icon(&self) -> &'static str {
        if self.muted || self.volume <= 0.0 {
            "audio-volume-muted-symbolic"
        } else if self.volume < 1.0 / 3.0 {
            "audio-volume-low-symbolic"
        } else if self.volume < 2.0 / 3.0 {
            "audio-volume-medium-symbolic"
        } else if self.volume <= 1.0 {
            "audio-volume-high-symbolic"
        } else {
            "audio-volume-overamplified-symbolic"
        }
    }
}

/// Parses "Volume: 0.45" or "Volume: 0.45 [MUTED]".
pub fn parse_wpctl(out: &str) -> Option<VolumeState> {
    let mut words = out.split_whitespace();
    if words.next()? != "Volume:" {
        return None;
    }
    let volume = words.next()?.parse().ok()?;
    Some(VolumeState {
        available: true,
        volume,
        muted: out.contains("[MUTED]"),
    })
}

type Listener = Box<dyn Fn(&VolumeState)>;

pub struct Volume {
    state: Cell<VolumeState>,
    listeners: RefCell<Vec<Listener>>,
}

thread_local! {
    static VOLUME: Rc<Volume> = Volume::start();
}

impl Volume {
    pub fn get() -> Rc<Self> {
        VOLUME.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let v = Rc::new(Self {
            state: Cell::default(),
            listeners: RefCell::default(),
        });
        v.poll();
        let weak = Rc::downgrade(&v);
        glib::timeout_add_local(POLL, move || match weak.upgrade() {
            Some(v) => {
                v.poll();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        v
    }

    pub fn subscribe(&self, f: impl Fn(&VolumeState) + 'static) {
        f(&self.state.get());
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn update(&self, s: VolumeState) {
        if self.state.replace(s) != s {
            for l in self.listeners.borrow().iter() {
                l(&s);
            }
        }
    }

    fn poll(self: &Rc<Self>) {
        let Ok(proc) = gio::Subprocess::newv(
            &[
                "wpctl".as_ref(),
                "get-volume".as_ref(),
                "@DEFAULT_AUDIO_SINK@".as_ref(),
            ],
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
        ) else {
            self.update(VolumeState::default());
            return;
        };
        let weak = Rc::downgrade(self);
        proc.communicate_utf8_async(None, gio::Cancellable::NONE, move |res| {
            let Some(v) = weak.upgrade() else { return };
            let state = res
                .ok()
                .and_then(|(out, _)| out)
                .and_then(|out| parse_wpctl(&out))
                .unwrap_or_default();
            v.update(state);
        });
    }

    pub fn set_volume(&self, volume: f64) {
        let mut s = self.state.get();
        s.volume = volume;
        s.muted = false;
        self.update(s);
        let vol = format!("{volume:.2}");
        spawn(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", "0"]);
        spawn(&["wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", &vol]);
    }

    pub fn toggle_mute(&self) {
        let mut s = self.state.get();
        s.muted = !s.muted;
        self.update(s);
        spawn(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]);
    }
}

/// GSubprocess reaps the child for us, unlike a dropped std::process::Child.
fn spawn(argv: &[&str]) {
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE) {
        tracing::warn!(error = %e, "could not run wpctl");
    }
}

/// The top bar speaker icon.
pub fn indicator() -> gtk::Image {
    let img = gtk::Image::new();
    Volume::get().subscribe({
        let img = img.clone();
        move |s| {
            img.set_visible(s.available);
            img.set_icon_name(Some(s.icon()));
        }
    });
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wpctl() {
        let s = parse_wpctl("Volume: 0.45\n").unwrap();
        assert!(!s.muted && (s.volume - 0.45).abs() < 1e-9);
        assert!(parse_wpctl("Volume: 0.45 [MUTED]").unwrap().muted);
        assert_eq!(parse_wpctl("nope"), None);
    }

    #[test]
    fn icons() {
        let s = |volume, muted| VolumeState {
            available: true,
            volume,
            muted,
        };
        assert_eq!(s(0.5, true).icon(), "audio-volume-muted-symbolic");
        assert_eq!(s(0.2, false).icon(), "audio-volume-low-symbolic");
        assert_eq!(s(0.5, false).icon(), "audio-volume-medium-symbolic");
        assert_eq!(s(0.9, false).icon(), "audio-volume-high-symbolic");
    }
}

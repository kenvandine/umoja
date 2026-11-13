//! Output and input volume, through the PulseAudio client API (see
//! `sound.rs`, which follows the server): levels arrive as events and
//! changes go back as introspection calls, as gnome-shell's libgvc does.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

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

type Listener = Box<dyn Fn(&VolumeState)>;

pub struct Volume {
    /// The default source (microphone) rather than the default sink.
    input: bool,
    state: Cell<VolumeState>,
    listeners: RefCell<Vec<Listener>>,
    /// When a media key last set the volume. While keys are coming in we
    /// trust our own value, since the server may not have applied the last
    /// one yet.
    last_key: Cell<Option<std::time::Instant>>,
    /// The OSD label for the current device.
    label: RefCell<Option<String>>,
}

thread_local! {
    static VOLUME: Rc<Volume> = Volume::start(false);
    static MIC: Rc<Volume> = Volume::start(true);
}

impl Volume {
    /// The default output.
    pub fn get() -> Rc<Self> {
        VOLUME.with(Rc::clone)
    }

    /// The default input, for the Quick Settings microphone slider.
    pub fn mic() -> Rc<Self> {
        MIC.with(Rc::clone)
    }

    fn start(input: bool) -> Rc<Self> {
        let v = Rc::new(Self {
            input,
            state: Cell::default(),
            listeners: RefCell::default(),
            last_key: Cell::default(),
            label: RefCell::default(),
        });
        // Start following the sound server. Deferred: Sound::start reaches
        // back here once it has levels.
        glib::idle_add_local_once(|| {
            let _ = crate::sound::Sound::get();
        });
        v
    }

    /// The level and mute the sound server reports for this device, and
    /// its OSD label.
    pub fn from_server(&self, s: VolumeState, label: Option<String>) {
        self.label.replace(label);
        // While keys are coming in our own value wins (see `key`).
        if self.last_key.get().is_some_and(|t| t.elapsed() < KEY_BURST) {
            return;
        }
        self.update(s);
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

    pub fn state(&self) -> VolumeState {
        self.state.get()
    }

    pub fn set_volume(&self, volume: f64) {
        let mut s = self.state.get();
        s.volume = volume;
        s.muted = false;
        self.update(s);
        let sound = crate::sound::Sound::get();
        sound.set_mute(self.input, false);
        sound.set_volume(self.input, volume);
    }

    pub fn toggle_mute(&self) {
        let mut s = self.state.get();
        s.muted = !s.muted;
        self.update(s);
        crate::sound::Sound::get().set_mute(self.input, s.muted);
    }
}

/// gnome-settings-daemon's VOLUME_STEP_PRECISE (Shift+volume keys).
const STEP_PRECISE: f64 = 0.02;
/// How long after a key press our own idea of the volume wins over the
/// server's.
const KEY_BURST: std::time::Duration = std::time::Duration::from_millis(800);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Mute,
}

/// What a volume key does, as in gnome-settings-daemon's do_sound_action.
/// Returns the new state and whether anything changed.
pub fn apply_key(old: VolumeState, key: Key, step: f64, max: f64) -> (VolumeState, bool) {
    let mut new = old;
    match key {
        Key::Mute => new.muted = !old.muted,
        Key::Down => {
            if old.volume <= step + 1e-6 {
                new.volume = 0.0;
                new.muted = true;
            } else {
                new.volume = old.volume - step;
            }
        }
        Key::Up => {
            new.muted = false;
            // Coming out of mute only raises the volume if it was 0.
            if !old.muted || old.volume <= 0.0 {
                new.volume = (old.volume + step).min(max);
            }
        }
    }
    // Keep to whole percent so repeated steps don't drift.
    new.volume = (new.volume * 100.0).round() / 100.0;
    let changed = new.muted != old.muted || (new.volume - old.volume).abs() > 1e-6;
    (new, changed)
}

/// 100%, or ~150% with "Over-Amplification" on, like gnome-shell's slider.
pub fn max_volume() -> f64 {
    if umoja_common::gsettings::get_bool(
        "org.gnome.desktop.sound",
        "allow-volume-above-100-percent",
        false,
    ) {
        1.5
    } else {
        1.0
    }
}

impl Volume {
    /// A volume key: change the default output and show the OSD.
    pub fn key(&self, key: Key, quiet: bool, precise: bool) {
        let old = self.state.get();
        if !old.available {
            return;
        }
        self.last_key.set(Some(std::time::Instant::now()));
        let step = if precise {
            STEP_PRECISE
        } else {
            umoja_common::gsettings::get_int(umoja_common::gsettings::MEDIA_KEYS, "volume-step", 6)
                .clamp(1, 100) as f64
                / 100.0
        };
        let max = max_volume();
        let (new, changed) = apply_key(old, key, step, max);
        let sound = crate::sound::Sound::get();
        if new.muted != old.muted {
            sound.set_mute(false, new.muted);
        }
        if (new.volume - old.volume).abs() > 1e-6 {
            sound.set_volume(false, new.volume);
        }
        self.update(new);
        let level = if new.muted { 0.0 } else { new.volume };
        crate::osd::show(
            crate::osd::Request::icon(crate::osd::volume_icon(false, new.muted, level))
                .label(self.label.borrow().clone())
                .level(level, max),
        );
        if !quiet && changed && !new.muted {
            self.play_feedback();
        }
    }

    /// The microphone mute key.
    pub fn toggle_mic_mute(&self) {
        let mic = Volume::mic();
        let old = mic.state();
        if !old.available {
            return;
        }
        let muted = !old.muted;
        crate::sound::Sound::get().set_mute(true, muted);
        mic.update(VolumeState { muted, ..old });
        let level = if muted { 0.0 } else { old.volume };
        crate::osd::show(
            crate::osd::Request::icon(crate::osd::volume_icon(true, muted, level))
                .label(mic.label.borrow().clone())
                .level(level, max_volume()),
        );
    }

    /// The "audio-volume-change" event sound, as gnome-settings-daemon
    /// plays it: not when sounds are off, and not over something playing.
    fn play_feedback(&self) {
        use umoja_common::gsettings as gs;
        if !gs::get_bool("org.gnome.desktop.sound", "event-sounds", true) {
            return;
        }
        // A held key mustn't stack clicks.
        if crate::event_sound::playing(FEEDBACK_ID) {
            return;
        }
        let theme = gs::get_string("org.gnome.desktop.sound", "theme-name", "freedesktop");
        crate::mpris::any_playing(move |playing| {
            if !playing {
                crate::event_sound::play(
                    FEEDBACK_ID,
                    "audio-volume-change",
                    "volume changed through key press",
                    &theme,
                );
            }
        });
    }
}

/// libcanberra id for the volume-change click.
const FEEDBACK_ID: u32 = 1;

/// The microphone icon: gnome-shell's microphone-sensitivity levels.
pub fn mic_icon(s: &VolumeState) -> &'static str {
    crate::osd::volume_icon(true, s.muted || s.volume <= 0.0, s.volume)
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

/// The top bar microphone icon, shown while an app is recording, as
/// gnome-shell's volume indicator shows its input icon.
pub fn mic_indicator() -> gtk::Image {
    let img = gtk::Image::new();
    img.set_visible(false);
    let sync = {
        let img = img.clone();
        move || {
            let mic = Volume::mic().state();
            img.set_visible(crate::sound::Sound::get().state().recording && mic.available);
            img.set_icon_name(Some(mic_icon(&mic)));
        }
    };
    let sync = Rc::new(sync);
    crate::sound::Sound::get().subscribe({
        let sync = sync.clone();
        move |_| sync()
    });
    Volume::mic().subscribe(move |_| sync());
    img
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn keys_follow_gsd() {
        let s = |volume, muted| VolumeState {
            available: true,
            volume,
            muted,
        };
        assert_eq!(
            apply_key(s(0.5, false), Key::Up, 0.06, 1.0).0,
            s(0.56, false)
        );
        assert_eq!(
            apply_key(s(0.98, false), Key::Up, 0.06, 1.0).0,
            s(1.0, false)
        );
        assert_eq!(apply_key(s(1.0, false), Key::Up, 0.06, 1.0).1, false);
        assert_eq!(
            apply_key(s(1.0, false), Key::Up, 0.06, 1.5).0,
            s(1.06, false)
        );
        // Unmuting with Up keeps the old volume unless it was 0.
        assert_eq!(apply_key(s(0.5, true), Key::Up, 0.06, 1.0).0, s(0.5, false));
        assert_eq!(
            apply_key(s(0.0, true), Key::Up, 0.06, 1.0).0,
            s(0.06, false)
        );
        // Down to the bottom mutes.
        assert_eq!(
            apply_key(s(0.04, false), Key::Down, 0.06, 1.0).0,
            s(0.0, true)
        );
        assert_eq!(
            apply_key(s(0.06, false), Key::Down, 0.06, 1.0).0,
            s(0.0, true)
        );
        assert_eq!(
            apply_key(s(0.5, false), Key::Down, 0.06, 1.0).0,
            s(0.44, false)
        );
        assert_eq!(
            apply_key(s(0.5, false), Key::Mute, 0.06, 1.0).0,
            s(0.5, true)
        );
    }
}

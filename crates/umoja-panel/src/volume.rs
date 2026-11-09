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
    /// When a media key last set the volume. While keys are coming in we
    /// trust our own value, since wpctl may not have applied the last one.
    last_key: Cell<Option<std::time::Instant>>,
    /// The OSD label for the current sink, looked up once per burst.
    label: RefCell<Option<String>>,
    /// The volume-change click, so a held key doesn't stack them.
    feedback: RefCell<Option<gio::Subprocess>>,
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
            last_key: Cell::default(),
            label: RefCell::default(),
            feedback: RefCell::default(),
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

pub const SINK: &str = "@DEFAULT_AUDIO_SINK@";
pub const SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";
/// gnome-settings-daemon's VOLUME_STEP_PRECISE (Shift+volume keys).
const STEP_PRECISE: f64 = 0.02;
/// How long after a key press our own idea of the volume wins over wpctl.
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

/// `wpctl get-volume`, synchronously: a key press needs the current value.
fn read_now(target: &str) -> Option<VolumeState> {
    let out = std::process::Command::new("wpctl")
        .args(["get-volume", target])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_wpctl(&String::from_utf8_lossy(&out.stdout))
}

/// gnome-settings-daemon labels the OSD with the device name, except for
/// built-in speakers. From `wpctl inspect`'s node properties.
pub fn device_label(inspect: &str) -> Option<String> {
    let prop = |key: &str| {
        inspect.lines().find_map(|l| {
            let l = l.trim_start().trim_start_matches('*').trim();
            let rest = l.strip_prefix(key)?.trim_start().strip_prefix('=')?;
            Some(rest.trim().trim_matches('"').to_string())
        })
    };
    let description = prop("node.description")?;
    let name = prop("node.name").unwrap_or_default();
    let form = prop("device.form-factor").unwrap_or_default();
    let internal = form == "internal"
        || (form.is_empty()
            && (name.starts_with("alsa_output.pci-") || name.starts_with("alsa_input.pci-"))
            && !name.contains("hdmi"));
    (!internal).then_some(description)
}

fn read_label(target: &str) -> Option<String> {
    let out = std::process::Command::new("wpctl")
        .args(["inspect", target])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    device_label(&String::from_utf8_lossy(&out.stdout))
}

impl Volume {
    /// A volume key: change the default output and show the OSD.
    pub fn key(&self, key: Key, quiet: bool, precise: bool) {
        let in_burst = self.last_key.get().is_some_and(|t| t.elapsed() < KEY_BURST);
        let old = if in_burst {
            self.state.get()
        } else {
            self.label.replace(read_label(SINK));
            read_now(SINK).unwrap_or_else(|| self.state.get())
        };
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
        if new.muted != old.muted {
            spawn(&["wpctl", "set-mute", SINK, if new.muted { "1" } else { "0" }]);
        }
        if (new.volume - old.volume).abs() > 1e-6 {
            spawn(&["wpctl", "set-volume", SINK, &format!("{:.2}", new.volume)]);
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
        let Some(old) = read_now(SOURCE) else { return };
        let muted = !old.muted;
        spawn(&["wpctl", "set-mute", SOURCE, if muted { "1" } else { "0" }]);
        let level = if muted { 0.0 } else { old.volume };
        crate::osd::show(
            crate::osd::Request::icon(crate::osd::volume_icon(true, muted, level))
                .label(read_label(SOURCE))
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
        if self
            .feedback
            .borrow()
            .as_ref()
            .is_some_and(|p| p.identifier().is_some())
        {
            return;
        }
        let theme = gs::get_string("org.gnome.desktop.sound", "theme-name", "freedesktop");
        let Some(file) = [theme.as_str(), "Yaru", "freedesktop"]
            .iter()
            .find_map(|t| {
                ["oga", "ogg", "wav"].iter().find_map(|ext| {
                    let p = format!("/usr/share/sounds/{t}/stereo/audio-volume-change.{ext}");
                    std::path::Path::new(&p).exists().then_some(p)
                })
            })
        else {
            return;
        };
        crate::mpris::any_playing(move |playing| {
            if !playing {
                Volume::get().feedback.replace(play_file(&file));
            }
        });
    }
}

fn play_file(file: &str) -> Option<gio::Subprocess> {
    let argv: [&std::ffi::OsStr; 4] = [
        "pw-play".as_ref(),
        "--media-role=Event".as_ref(),
        "--volume=1.0".as_ref(),
        file.as_ref(),
    ];
    gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE).ok()
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

    #[test]
    fn labels_external_devices_only() {
        let speaker = r#"id 52, type PipeWire:Interface:Node
    node.description = "Ryzen HD Audio Controller Speaker"
  * node.name = "alsa_output.pci-0000_c4_00.6.analog-stereo""#;
        assert_eq!(device_label(speaker), None);
        let bt = r#"  * node.description = "WH-1000XM4"
  * node.name = "bluez_output.AA_BB.1"
    device.form-factor = "headphone""#;
        assert_eq!(device_label(bt).as_deref(), Some("WH-1000XM4"));
        let usb = r#"  * node.description = "USB Audio"
  * node.name = "alsa_output.usb-Foo-00.analog-stereo""#;
        assert_eq!(device_label(usb).as_deref(), Some("USB Audio"));
    }
}

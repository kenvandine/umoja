//! Sound devices, levels and recording streams through the PulseAudio
//! client API (served by pipewire-pulse), as gnome-shell's libgvc follows
//! them: the output and input device lists in Quick Settings, the default
//! of each with its volume and mute, and whether any app is recording
//! (which is when gnome-shell shows the microphone slider and top bar
//! icon). Changes arrive as subscription events on the panel's GLib main
//! loop; nothing is spawned or polled.

use gtk::glib;
use gtk4 as gtk;
use libpulse_binding as pulse;
use libpulse_glib_binding::Mainloop;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SourceInfo, SourceOutputInfo};
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::{Context, FlagSet, State};
use pulse::proplist::Proplist;
use pulse::volume::{ChannelVolumes, Volume as PaVolume};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::volume::VolumeState;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Device {
    /// PulseAudio sink / source index.
    pub id: u32,
    /// Sink / source name, what the server's default refers to.
    pub name: String,
    pub description: String,
    /// What tells this device apart: the port ("Headphones", "HDMI /
    /// DisplayPort 1 Output") or the monitor's name for HDMI.
    pub port: String,
    /// The sound card ("800 Series Chipset Family Audio..."), empty when
    /// the port already is the whole name (a Bluetooth headset).
    pub card: String,
    pub icon: &'static str,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SoundState {
    pub outputs: Vec<Device>,
    pub inputs: Vec<Device>,
    /// Name of the default sink / source.
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    /// An app has a capture stream open, as gnome-shell's
    /// InputStreamSlider._maybeShowInput checks.
    pub recording: bool,
    /// Level and mute of the default sink / source.
    pub output_volume: Option<VolumeState>,
    pub input_volume: Option<VolumeState>,
    /// The OSD labels for the default sink / source (see `osd_label`).
    pub output_label: Option<String>,
    pub input_label: Option<String>,
}

impl SoundState {
    pub fn devices(&self, input: bool) -> &[Device] {
        if input { &self.inputs } else { &self.outputs }
    }

    pub fn default_name(&self, input: bool) -> Option<&str> {
        if input {
            self.default_input.as_deref()
        } else {
            self.default_output.as_deref()
        }
    }
}

/// gnome-shell skips these: they only "record" to show input levels.
const SKIPPED_APPS: [&str; 2] = ["org.gnome.VolumeControl", "org.PulseAudio.pavucontrol"];

/// A sink or source as the server reported it.
#[derive(Debug, Clone, Default)]
struct Node {
    index: u32,
    name: String,
    description: String,
    card: Option<u32>,
    form_factor: String,
    profile: Option<String>,
    volume: ChannelVolumes,
    muted: bool,
}

impl Node {
    fn new(
        index: u32,
        name: Option<&str>,
        description: Option<&str>,
        card: Option<u32>,
        props: &Proplist,
        volume: ChannelVolumes,
        muted: bool,
    ) -> Self {
        let name = name.unwrap_or_default().to_string();
        Self {
            index,
            description: description
                .map(str::to_string)
                .or_else(|| props.get_str("device.nick"))
                .unwrap_or_else(|| name.clone()),
            name,
            card,
            form_factor: props.get_str("device.form-factor").unwrap_or_default(),
            profile: props.get_str("device.profile.description"),
            volume,
            muted,
        }
    }

    fn from_sink(i: &SinkInfo) -> Self {
        Self::new(
            i.index,
            i.name.as_deref(),
            i.description.as_deref(),
            i.card,
            &i.proplist,
            i.volume,
            i.mute,
        )
    }

    fn from_source(i: &SourceInfo) -> Self {
        Self::new(
            i.index,
            i.name.as_deref(),
            i.description.as_deref(),
            i.card,
            &i.proplist,
            i.volume,
            i.mute,
        )
    }

    /// gnome-shell's slider value: the loudest channel, as libgvc reads
    /// it, relative to 100%.
    fn level(&self) -> VolumeState {
        let v = if self.volume.len() > 0 {
            self.volume.max().0
        } else {
            0
        };
        VolumeState {
            available: true,
            volume: (v as f64 / PaVolume::NORMAL.0 as f64 * 100.0).round() / 100.0,
            muted: self.muted,
        }
    }
}

/// Everything one refresh reads from the server.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    default_sink: Option<String>,
    default_source: Option<String>,
    sinks: Vec<Node>,
    /// Not monitors of sinks: those aren't microphones.
    sources: Vec<Node>,
    /// Card index → its description.
    cards: HashMap<u32, String>,
    recording: bool,
}

impl Snapshot {
    fn default_node(&self, input: bool) -> Option<&Node> {
        let (list, name) = if input {
            (&self.sources, &self.default_source)
        } else {
            (&self.sinks, &self.default_sink)
        };
        list.iter().find(|n| Some(&n.name) == name.as_ref())
    }

    fn state(&self) -> SoundState {
        let device = |n: &Node, input: bool| {
            let card = n
                .card
                .and_then(|c| self.cards.get(&c))
                .map(String::as_str)
                .unwrap_or("");
            let (port, card) = split_name(&n.description, card, n.profile.as_deref());
            Device {
                id: n.index,
                name: n.name.clone(),
                description: n.description.clone(),
                port,
                card,
                icon: icon_for(input, &n.name, &n.form_factor),
            }
        };
        let mut outputs: Vec<Device> = self.sinks.iter().map(|n| device(n, false)).collect();
        let mut inputs: Vec<Device> = self.sources.iter().map(|n| device(n, true)).collect();
        for list in [&mut outputs, &mut inputs] {
            // Group a card's ports together.
            let key = |d: &Device| {
                let group = if d.card.is_empty() { &d.port } else { &d.card };
                (group.clone(), d.port.clone(), d.id)
            };
            list.sort_by_key(key);
        }
        let out = self.default_node(false);
        let inp = self.default_node(true);
        SoundState {
            outputs,
            inputs,
            default_output: self.default_sink.clone(),
            default_input: self.default_source.clone(),
            recording: self.recording,
            output_volume: out.map(Node::level),
            input_volume: inp.map(Node::level),
            output_label: out.and_then(osd_label),
            input_label: inp.and_then(osd_label),
        }
    }
}

/// The icon gnome-shell would show for the device's port.
fn icon_for(input: bool, name: &str, form_factor: &str) -> &'static str {
    match form_factor {
        "headphone" => return "audio-headphones-symbolic",
        "headset" | "hands-free" => return "audio-headset-symbolic",
        "webcam" => return "camera-web-symbolic",
        "tv" | "monitor" => return "video-display-symbolic",
        _ => {}
    }
    if input {
        "audio-input-microphone-symbolic"
    } else if name.contains("hdmi") || name.contains("HDMI") || name.contains("DisplayPort") {
        "video-display-symbolic"
    } else {
        "audio-speakers-symbolic"
    }
}

/// gnome-settings-daemon labels the OSD with the device name, except for
/// built-in speakers and microphones.
fn osd_label(n: &Node) -> Option<String> {
    let internal = n.form_factor == "internal"
        || (n.form_factor.is_empty()
            && (n.name.starts_with("alsa_output.pci-") || n.name.starts_with("alsa_input.pci-"))
            && !n.name.contains("hdmi"));
    (!internal).then(|| n.description.clone())
}

/// Whether a capture stream is an app recording, not a level meter (libgvc
/// drops "peaks" streams; gnome-shell skips the volume-control apps) or the
/// internal half of a loopback / echo-cancel.
fn is_recording(prop: impl Fn(&str) -> Option<String>, resample_method: Option<&str>) -> bool {
    if resample_method == Some("peaks") {
        return false;
    }
    let truthy = |k: &str| prop(k).is_some_and(|v| v == "true" || v == "1");
    let app = prop("application.id")
        .or_else(|| prop("pipewire.access.portal.app_id"))
        .unwrap_or_default();
    !(truthy("stream.monitor")
        || prop("node.link-group").is_some()
        || SKIPPED_APPS.contains(&app.as_str()))
}

fn recording_stream(o: &SourceOutputInfo) -> bool {
    is_recording(|k| o.proplist.get_str(k), o.resample_method.as_deref())
}

/// Split PipeWire's "<card> <port>" node description the way gnome-shell
/// labels devices: the port (what differs between them) first, the card
/// apart. An HDMI output's monitor name ("... [CR340HD]") is the port.
pub fn split_name(description: &str, card: &str, profile: Option<&str>) -> (String, String) {
    let mut desc = description.trim().to_string();
    let mut card = card.trim().to_string();
    let mut monitor = None;
    for s in [&mut desc, &mut card] {
        if let Some(open) = s.rfind(" [")
            && s.ends_with(']')
        {
            let name = s[open + 2..s.len() - 1].trim().to_string();
            s.truncate(open);
            if !name.is_empty() {
                monitor = Some(name);
            }
        }
    }
    let (mut port, mut card) = match desc.strip_prefix(card.as_str()) {
        Some(rest) if !card.is_empty() => (rest.trim().to_string(), card),
        _ => match profile.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) if desc != p => {
                let c = desc
                    .strip_suffix(p)
                    .map(str::trim)
                    .unwrap_or(&desc)
                    .to_string();
                (p.to_string(), if c == desc { String::new() } else { c })
            }
            _ => (desc.clone(), String::new()),
        },
    };
    if let Some(m) = monitor {
        port = m;
    }
    if port.is_empty() {
        port = std::mem::take(&mut card);
    }
    if port == card {
        card.clear();
    }
    (port, card)
}

type Listener = Box<dyn Fn(&SoundState)>;

pub struct Sound {
    state: RefCell<SoundState>,
    listeners: RefCell<Vec<Listener>>,
    /// The connection; replaced when the server goes away and comes back.
    mainloop: RefCell<Option<Mainloop>>,
    context: RefCell<Option<Context>>,
    /// The last snapshot, for each default device's channel volumes (a
    /// new level scales them, keeping the balance, as libgvc does).
    snapshot: RefCell<Snapshot>,
    pending: Cell<bool>,
    in_flight: Cell<bool>,
    again: Cell<bool>,
}

thread_local! {
    static SOUND: Rc<Sound> = Sound::start();
}

/// How long to wait before reconnecting after the server went away.
const RECONNECT: std::time::Duration = std::time::Duration::from_secs(2);

impl Sound {
    pub fn get() -> Rc<Self> {
        SOUND.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let sound = Rc::new(Self {
            state: RefCell::default(),
            listeners: RefCell::default(),
            mainloop: RefCell::default(),
            context: RefCell::default(),
            snapshot: RefCell::default(),
            pending: Cell::new(false),
            in_flight: Cell::new(false),
            again: Cell::new(false),
        });
        sound.connect();
        sound
    }

    fn connect(self: &Rc<Self>) {
        let Some(mainloop) = Mainloop::new(None) else {
            tracing::warn!("no PulseAudio GLib main loop; no sound controls");
            return;
        };
        let mut props = Proplist::new().expect("proplist");
        let _ = props.set_str(pulse::proplist::properties::APPLICATION_NAME, "Umoja Panel");
        let _ = props.set_str(
            pulse::proplist::properties::APPLICATION_ID,
            "org.umoja.Panel",
        );
        let Some(mut ctx) = Context::new_with_proplist(&mainloop, "Umoja Panel", &props) else {
            tracing::warn!("could not create a PulseAudio context");
            return;
        };
        // Callbacks can run inside our own calls into the context (the
        // state changes during connect()), so they only schedule work.
        let weak = Rc::downgrade(self);
        ctx.set_state_callback(Some(Box::new(move || {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(s) = weak.upgrade() {
                    s.state_changed();
                }
            });
        })));
        let weak = Rc::downgrade(self);
        ctx.set_subscribe_callback(Some(Box::new(move |_, _, _| {
            if let Some(s) = weak.upgrade() {
                s.schedule_refresh();
            }
        })));
        // NOFAIL: wait for pipewire-pulse to appear instead of failing.
        if let Err(e) = ctx.connect(None, FlagSet::NOFAIL | FlagSet::NOAUTOSPAWN, None) {
            tracing::warn!(error = %e, "could not connect to the sound server");
        }
        self.mainloop.replace(Some(mainloop));
        self.context.replace(Some(ctx));
    }

    fn state_changed(self: &Rc<Self>) {
        let state = self.context.borrow().as_ref().map(Context::get_state);
        match state {
            Some(State::Ready) => {
                if let Some(ctx) = self.context.borrow_mut().as_mut() {
                    ctx.subscribe(
                        InterestMaskSet::SINK
                            | InterestMaskSet::SOURCE
                            | InterestMaskSet::SOURCE_OUTPUT
                            | InterestMaskSet::CARD
                            | InterestMaskSet::SERVER,
                        |_| {},
                    );
                }
                self.refresh();
            }
            Some(State::Failed | State::Terminated) => {
                tracing::info!("sound server went away; reconnecting");
                // Drop the old connection outside its own callback.
                let old = (self.context.take(), self.mainloop.take());
                glib::idle_add_local_once(move || drop(old));
                self.in_flight.set(false);
                self.publish(Snapshot::default());
                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(RECONNECT, move || {
                    if let Some(s) = weak.upgrade() {
                        s.connect();
                    }
                });
            }
            _ => {}
        }
    }

    /// Events come in bursts (one per changed object); coalesce them.
    fn schedule_refresh(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(30), move || {
            if let Some(s) = weak.upgrade() {
                s.pending.set(false);
                s.refresh();
            }
        });
    }

    /// Read the server info, sinks, sources, cards and capture streams,
    /// and publish once all have arrived.
    fn refresh(self: &Rc<Self>) {
        if self.in_flight.replace(true) {
            self.again.set(true);
            return;
        }
        let ctx = self.context.borrow();
        let Some(ctx) = ctx.as_ref().filter(|c| c.get_state() == State::Ready) else {
            self.in_flight.set(false);
            return;
        };
        let snap = Rc::new(RefCell::new(Snapshot::default()));
        let left = Rc::new(Cell::new(5u8));
        let done = {
            let (weak, snap, left) = (Rc::downgrade(self), snap.clone(), left.clone());
            Rc::new(move || {
                left.set(left.get() - 1);
                if left.get() > 0 {
                    return;
                }
                let Some(s) = weak.upgrade() else { return };
                s.in_flight.set(false);
                s.publish(snap.take());
                if s.again.replace(false) {
                    s.refresh();
                }
            })
        };
        let intro = ctx.introspect();
        {
            let (snap, done) = (snap.clone(), done.clone());
            intro.get_server_info(move |i| {
                let mut s = snap.borrow_mut();
                s.default_sink = i.default_sink_name.as_ref().map(|n| n.to_string());
                s.default_source = i.default_source_name.as_ref().map(|n| n.to_string());
                drop(s);
                done();
            });
        }
        {
            let (snap, done) = (snap.clone(), done.clone());
            intro.get_sink_info_list(move |r| match r {
                ListResult::Item(i) => snap.borrow_mut().sinks.push(Node::from_sink(i)),
                _ => done(),
            });
        }
        {
            let (snap, done) = (snap.clone(), done.clone());
            intro.get_source_info_list(move |r| match r {
                ListResult::Item(i) if i.monitor_of_sink.is_none() => {
                    snap.borrow_mut().sources.push(Node::from_source(i))
                }
                ListResult::Item(_) => {}
                _ => done(),
            });
        }
        {
            let (snap, done) = (snap.clone(), done.clone());
            intro.get_card_info_list(move |r| match r {
                ListResult::Item(i) => {
                    let name = i
                        .proplist
                        .get_str("device.description")
                        .or_else(|| i.proplist.get_str("device.nick"))
                        .unwrap_or_default();
                    snap.borrow_mut().cards.insert(i.index, name);
                }
                _ => done(),
            });
        }
        {
            let (snap, done) = (snap.clone(), done.clone());
            intro.get_source_output_info_list(move |r| match r {
                ListResult::Item(o) => {
                    if recording_stream(o) {
                        snap.borrow_mut().recording = true;
                    }
                }
                _ => done(),
            });
        }
    }

    fn publish(&self, snap: Snapshot) {
        let state = snap.state();
        self.snapshot.replace(snap);
        self.update(state);
    }

    pub fn state(&self) -> SoundState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self, f: impl Fn(&SoundState) + 'static) {
        f(&self.state.borrow());
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn update(&self, s: SoundState) {
        let old = self.state.replace(s.clone());
        if old == s {
            return;
        }
        crate::volume::Volume::get()
            .from_server(s.output_volume.unwrap_or_default(), s.output_label.clone());
        crate::volume::Volume::mic()
            .from_server(s.input_volume.unwrap_or_default(), s.input_label.clone());
        for l in self.listeners.borrow().iter() {
            l(&s);
        }
    }

    /// Make a device the default, as picking it in gnome-shell's menu does.
    pub fn set_default(&self, input: bool, dev: &Device) {
        {
            let mut s = self.state.borrow_mut();
            let name = Some(dev.name.clone());
            if input {
                s.default_input = name;
            } else {
                s.default_output = name;
            }
        }
        let s = self.state();
        for l in self.listeners.borrow().iter() {
            l(&s);
        }
        if let Some(ctx) = self.context.borrow_mut().as_mut() {
            if input {
                ctx.set_default_source(&dev.name, |_| {});
            } else {
                ctx.set_default_sink(&dev.name, |_| {});
            }
        }
    }

    /// Set the default device's level (1.0 = 100%), scaling its channels
    /// together so the balance stays.
    pub fn set_volume(&self, input: bool, level: f64) {
        let target = (level.max(0.0) * PaVolume::NORMAL.0 as f64).round() as u32;
        let mut cv = self
            .snapshot
            .borrow()
            .default_node(input)
            .map(|n| n.volume)
            .filter(|v| v.len() > 0)
            .unwrap_or_else(|| {
                let mut v = ChannelVolumes::default();
                v.set(2, PaVolume::NORMAL);
                v
            });
        if cv.max().0 == 0 {
            let n = cv.len();
            cv.set(n, PaVolume(target));
        } else {
            cv.scale(PaVolume(target));
        }
        // Remember it, so quick successive steps scale the newest value.
        if let Some(n) = self.snapshot.borrow_mut().default_node_mut(input) {
            n.volume = cv;
        }
        let ctx = self.context.borrow();
        let Some(ctx) = ctx.as_ref() else { return };
        let mut intro = ctx.introspect();
        if input {
            intro.set_source_volume_by_name(DEFAULT_SOURCE, &cv, None);
        } else {
            intro.set_sink_volume_by_name(DEFAULT_SINK, &cv, None);
        }
    }

    pub fn set_mute(&self, input: bool, muted: bool) {
        let ctx = self.context.borrow();
        let Some(ctx) = ctx.as_ref() else { return };
        let mut intro = ctx.introspect();
        if input {
            intro.set_source_mute_by_name(DEFAULT_SOURCE, muted, None);
        } else {
            intro.set_sink_mute_by_name(DEFAULT_SINK, muted, None);
        }
    }
}

/// The server's names for whatever is the default right now.
const DEFAULT_SINK: &str = "@DEFAULT_SINK@";
const DEFAULT_SOURCE: &str = "@DEFAULT_SOURCE@";

impl Snapshot {
    fn default_node_mut(&mut self, input: bool) -> Option<&mut Node> {
        let (list, name) = if input {
            (&mut self.sources, &self.default_source)
        } else {
            (&mut self.sinks, &self.default_sink)
        };
        list.iter_mut().find(|n| Some(&n.name) == name.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: u32, name: &str, description: &str, card: Option<u32>, form: &str) -> Node {
        let mut volume = ChannelVolumes::default();
        volume.set(2, PaVolume::NORMAL);
        Node {
            index,
            name: name.into(),
            description: description.into(),
            card,
            form_factor: form.into(),
            profile: None,
            volume,
            muted: false,
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            default_sink: Some("bluez_output.AA_BB.1".into()),
            default_source: Some("alsa_input.pci-0000_c4_00.6.analog-stereo".into()),
            sinks: vec![
                node(
                    52,
                    "alsa_output.pci-0000_c4_00.6.analog-stereo",
                    "Ryzen HD Audio Controller Speaker",
                    Some(40),
                    "",
                ),
                node(60, "bluez_output.AA_BB.1", "WH-1000XM4", None, "headphone"),
            ],
            sources: vec![node(
                53,
                "alsa_input.pci-0000_c4_00.6.analog-stereo",
                "Ryzen HD Audio Controller Digital Microphone",
                Some(40),
                "",
            )],
            cards: HashMap::from([(40, "Ryzen HD Audio Controller".to_string())]),
            recording: false,
        }
    }

    #[test]
    fn lists_devices_and_defaults() {
        let s = sample().state();
        assert_eq!(s.outputs.len(), 2);
        assert_eq!(s.outputs[0].port, "Speaker");
        assert_eq!(s.outputs[0].card, "Ryzen HD Audio Controller");
        assert_eq!(s.outputs[0].icon, "audio-speakers-symbolic");
        assert_eq!(s.outputs[1].icon, "audio-headphones-symbolic");
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.inputs[0].icon, "audio-input-microphone-symbolic");
        assert_eq!(s.default_output.as_deref(), Some("bluez_output.AA_BB.1"));
        // The headphones are named on the OSD; the built-in mic isn't.
        assert_eq!(s.output_label.as_deref(), Some("WH-1000XM4"));
        assert_eq!(s.input_label, None);
    }

    #[test]
    fn level_is_loudest_channel_of_normal() {
        let mut snap = sample();
        let mut cv = ChannelVolumes::default();
        cv.set(2, PaVolume(PaVolume::NORMAL.0 / 4));
        cv.get_mut()[1] = PaVolume(PaVolume::NORMAL.0 / 2);
        snap.sinks[1].volume = cv;
        snap.sinks[1].muted = true;
        let v = snap.state().output_volume.unwrap();
        assert!((v.volume - 0.5).abs() < 1e-9);
        assert!(v.muted && v.available);
        // No default device: no level.
        snap.default_sink = Some("gone".into());
        assert!(snap.state().output_volume.is_none());
    }

    #[test]
    fn splits_port_from_card() {
        let ace = "800 Series Chipset Family Audio Context Engine (ACE)";
        let sp = |d: &str, c: &str, p: Option<&str>| {
            let (a, b) = split_name(d, c, p);
            (a, b)
        };
        assert_eq!(
            sp(&format!("{ace} HDMI / DisplayPort 1 Output"), ace, None),
            ("HDMI / DisplayPort 1 Output".into(), ace.into())
        );
        assert_eq!(
            sp(&format!("{ace} Headphones"), ace, None),
            ("Headphones".into(), ace.into())
        );
        assert_eq!(
            sp(
                "Creative Pebble Pro Analog Stereo",
                "Creative Pebble Pro",
                None
            ),
            ("Analog Stereo".into(), "Creative Pebble Pro".into())
        );
        assert_eq!(
            sp(
                "GB205 High Definition Audio Controller Digital Stereo (HDMI) [CR340HD]",
                "GB205 High Definition Audio Controller",
                None
            ),
            (
                "CR340HD".into(),
                "GB205 High Definition Audio Controller".into()
            )
        );
        // No device object: fall back to the profile description.
        assert_eq!(
            sp("USB Audio Analog Stereo", "", Some("Analog Stereo")),
            ("Analog Stereo".into(), "USB Audio".into())
        );
        // One-profile devices are just their name.
        assert_eq!(
            sp("WH-1000XM4", "WH-1000XM4", None),
            ("WH-1000XM4".into(), String::new())
        );
        assert_eq!(
            sp("WH-1000XM4", "", None),
            ("WH-1000XM4".into(), String::new())
        );
    }

    #[test]
    fn recording_ignores_meters_and_loopbacks() {
        let props = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!is_recording(
            props(&[("application.id", "org.PulseAudio.pavucontrol")]),
            None
        ));
        assert!(!is_recording(props(&[("stream.monitor", "true")]), None));
        assert!(!is_recording(
            props(&[("node.link-group", "echo-cancel-1")]),
            None
        ));
        assert!(!is_recording(props(&[]), Some("peaks")));
        assert!(is_recording(
            props(&[("application.name", "Firefox")]),
            Some("speex-float-1")
        ));
    }
}

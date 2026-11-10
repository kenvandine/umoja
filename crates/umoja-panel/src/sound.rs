//! Sound devices and recording streams, followed live through
//! `pw-dump --monitor`: the output and input device lists in Quick
//! Settings, the default of each, and whether any app is recording (which
//! is when gnome-shell shows the microphone slider and top bar icon).

use gtk::glib;
use gtk4 as gtk;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Device {
    /// PipeWire node id, for `wpctl set-default`.
    pub id: u32,
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
    /// node.name of the default sink / source.
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    /// An app has a capture stream open, as gnome-shell's
    /// InputStreamSlider._maybeShowInput checks.
    pub recording: bool,
    /// Level and mute of the default sink / source, as `wpctl get-volume`
    /// reports them, from the node's Props (None until pw-dump sent them).
    pub output_volume: Option<crate::volume::VolumeState>,
    pub input_volume: Option<crate::volume::VolumeState>,
    /// The OSD label for the default sink (see `volume::device_label`).
    pub output_label: Option<String>,
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

fn props(obj: &Value) -> Option<&serde_json::Map<String, Value>> {
    obj.pointer("/info/props").and_then(Value::as_object)
}

fn prop<'a>(p: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// PipeWire props hold booleans as either JSON bools or strings.
fn prop_true(p: &serde_json::Map<String, Value>, key: &str) -> bool {
    match p.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "1",
        _ => false,
    }
}

/// The icon gnome-shell would show for the device's port.
fn icon_for(
    input: bool,
    node: &serde_json::Map<String, Value>,
    device: Option<&serde_json::Map<String, Value>>,
) -> &'static str {
    let get = |k: &str| {
        prop(node, k)
            .or_else(|| device.and_then(|d| prop(d, k)))
            .unwrap_or("")
    };
    let name = get("node.name");
    match get("device.form-factor") {
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

/// The node.name in a default-metadata value: `{"name": "..."}`, either
/// already parsed or still as a JSON string.
fn meta_name(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => o.get("name").and_then(Value::as_str).map(str::to_string),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| meta_name(&v)),
        _ => None,
    }
}

/// The PipeWire graph as pw-dump reports it, by object id.
#[derive(Default)]
pub struct Graph {
    objects: BTreeMap<u64, Value>,
    /// The "default" metadata, key → value, merged across updates.
    defaults: BTreeMap<String, Value>,
}

impl Graph {
    /// One batch from pw-dump: an array of objects. An object without a
    /// "type" (`{"id": N, "info": null}`) is a removal.
    pub fn apply(&mut self, batch: Value) {
        let Value::Array(items) = batch else { return };
        for obj in items {
            let Some(id) = obj.get("id").and_then(Value::as_u64) else {
                continue;
            };
            if obj.get("type").is_none() {
                if self
                    .objects
                    .remove(&id)
                    .is_some_and(|o| is_default_meta(&o))
                {
                    self.defaults.clear();
                }
                continue;
            }
            if is_default_meta(&obj)
                && let Some(entries) = obj.get("metadata").and_then(Value::as_array)
            {
                for e in entries {
                    if e.get("subject").and_then(Value::as_u64).unwrap_or(0) != 0 {
                        continue;
                    }
                    let Some(key) = e.get("key").and_then(Value::as_str) else {
                        continue;
                    };
                    match e.get("value") {
                        Some(Value::Null) | None => {
                            self.defaults.remove(key);
                        }
                        Some(v) => {
                            self.defaults.insert(key.to_string(), v.clone());
                        }
                    }
                }
            }
            self.objects.insert(id, obj);
        }
    }

    pub fn state(&self) -> SoundState {
        let mut s = SoundState {
            default_output: self.defaults.get("default.audio.sink").and_then(meta_name),
            default_input: self
                .defaults
                .get("default.audio.source")
                .and_then(meta_name),
            ..Default::default()
        };
        for (id, obj) in &self.objects {
            if obj.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
                continue;
            }
            let Some(p) = props(obj) else { continue };
            let class = prop(p, "media.class").unwrap_or("");
            match class {
                "Audio/Sink" | "Audio/Source" | "Audio/Duplex" => {
                    let input = class == "Audio/Source";
                    let device = p
                        .get("device.id")
                        .and_then(|d| d.as_u64().or_else(|| d.as_str()?.parse().ok()))
                        .and_then(|d| self.objects.get(&d))
                        .and_then(props);
                    let name = prop(p, "node.name").unwrap_or("").to_string();
                    let description = prop(p, "node.description")
                        .or_else(|| prop(p, "node.nick"))
                        .unwrap_or(&name)
                        .to_string();
                    let card_name = device
                        .and_then(|d| prop(d, "device.description").or_else(|| prop(d, "device.nick")))
                        .unwrap_or("");
                    let (port, card) =
                        split_name(&description, card_name, prop(p, "device.profile.description"));
                    if !input && s.default_output.as_deref() == Some(name.as_str()) {
                        s.output_volume = node_volume(obj);
                        s.output_label = node_label(p);
                    }
                    if input && s.default_input.as_deref() == Some(name.as_str()) {
                        s.input_volume = node_volume(obj);
                    }
                    let dev = Device {
                        id: *id as u32,
                        icon: icon_for(input, p, device),
                        name,
                        description,
                        port,
                        card,
                    };
                    if input {
                        s.inputs.push(dev);
                    } else {
                        s.outputs.push(dev);
                    }
                }
                "Stream/Input/Audio" => {
                    let app = prop(p, "application.id")
                        .or_else(|| prop(p, "pipewire.access.portal.app_id"))
                        .unwrap_or("");
                    // Peak meters (pavucontrol, Settings) and the internal
                    // half of loopbacks / echo-cancel aren't apps recording.
                    let internal = prop_true(p, "stream.monitor")
                        || p.contains_key("node.link-group")
                        || SKIPPED_APPS.contains(&app);
                    if !internal {
                        s.recording = true;
                    }
                }
                _ => {}
            }
        }
        for list in [&mut s.outputs, &mut s.inputs] {
            // Group a card's ports together.
            let key = |d: &Device| {
                let group = if d.card.is_empty() { &d.port } else { &d.card };
                (group.clone(), d.port.clone(), d.id)
            };
            list.sort_by_key(key);
        }
        s
    }
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
                let c = desc.strip_suffix(p).map(str::trim).unwrap_or(&desc).to_string();
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

/// `wpctl get-volume` for a node: the mean of its Props channelVolumes on
/// WirePlumber's cubic scale, rounded to whole percent as wpctl prints it.
fn node_volume(obj: &Value) -> Option<crate::volume::VolumeState> {
    let props = obj.pointer("/info/params/Props")?.as_array()?;
    let main = props.iter().find(|p| p.get("channelVolumes").is_some())?;
    let channels: Vec<f64> = main
        .get("channelVolumes")?
        .as_array()?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    if channels.is_empty() {
        return None;
    }
    let linear = channels.iter().sum::<f64>() / channels.len() as f64;
    let volume = (linear.max(0.0).cbrt() * 100.0).round() / 100.0;
    Some(crate::volume::VolumeState {
        available: true,
        volume,
        muted: main.get("mute").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// `volume::device_label` from a node's own properties.
fn node_label(p: &serde_json::Map<String, Value>) -> Option<String> {
    let description = prop(p, "node.description")?;
    let name = prop(p, "node.name").unwrap_or("");
    let form = prop(p, "device.form-factor").unwrap_or("");
    let internal = form == "internal"
        || (form.is_empty()
            && (name.starts_with("alsa_output.pci-") || name.starts_with("alsa_input.pci-"))
            && !name.contains("hdmi"));
    (!internal).then(|| description.to_string())
}

fn is_default_meta(obj: &Value) -> bool {
    obj.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Metadata")
        && obj.pointer("/props/metadata.name").and_then(Value::as_str) == Some("default")
}

type Listener = Box<dyn Fn(&SoundState)>;

pub struct Sound {
    state: RefCell<SoundState>,
    listeners: RefCell<Vec<Listener>>,
}

thread_local! {
    static SOUND: Rc<Sound> = Sound::start();
}

enum Msg {
    Reset,
    Batch(Value),
}

impl Sound {
    pub fn get() -> Rc<Self> {
        SOUND.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let sound = Rc::new(Self {
            state: RefCell::default(),
            listeners: RefCell::default(),
        });
        let (tx, rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("pw-dump".into())
            .spawn(move || monitor(tx))
            .ok();
        let weak = Rc::downgrade(&sound);
        glib::spawn_future_local(async move {
            let mut graph = Graph::default();
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Msg::Reset => graph = Graph::default(),
                    Msg::Batch(b) => graph.apply(b),
                }
                // Coalesce a burst (the initial dump, a device appearing).
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        Msg::Reset => graph = Graph::default(),
                        Msg::Batch(b) => graph.apply(b),
                    }
                }
                let Some(sound) = weak.upgrade() else { break };
                sound.update(graph.state());
            }
        });
        sound
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
        // Volume levels come from the same stream; without them (a
        // PipeWire that sends no Props) ask wpctl once instead.
        match s.output_volume {
            Some(v) => crate::volume::Volume::get().from_graph(v, s.output_label.clone()),
            None if old.default_output != s.default_output => crate::volume::Volume::get().poll(),
            None => {}
        }
        match s.input_volume {
            Some(v) => crate::volume::Volume::mic().from_graph(v, None),
            None if old.default_input != s.default_input || old.recording != s.recording => {
                crate::volume::Volume::mic().poll()
            }
            None => {}
        }
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
        let id = dev.id.to_string();
        let argv: [&std::ffi::OsStr; 3] = ["wpctl".as_ref(), "set-default".as_ref(), id.as_ref()];
        match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE) {
            Ok(p) => p.wait_async(gio::Cancellable::NONE, move |_| {
                let v = if input {
                    crate::volume::Volume::mic()
                } else {
                    crate::volume::Volume::get()
                };
                v.poll();
            }),
            Err(e) => tracing::warn!(error = %e, "could not run wpctl set-default"),
        }
    }
}

/// Runs `pw-dump --monitor` (restarting it with PipeWire) and forwards
/// each JSON batch it prints.
fn monitor(tx: async_channel::Sender<Msg>) {
    use std::os::unix::process::CommandExt;
    let mut warned = false;
    loop {
        let mut cmd = std::process::Command::new("pw-dump");
        cmd.args(["--monitor", "--no-colors"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // Don't outlive the panel.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(mut child) => {
                warned = false;
                if let Some(out) = child.stdout.take() {
                    let reader = std::io::BufReader::new(out);
                    for v in serde_json::Deserializer::from_reader(reader).into_iter::<Value>() {
                        let Ok(v) = v else { break };
                        if tx.send_blocking(Msg::Batch(v)).is_err() {
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                if tx.send_blocking(Msg::Reset).is_err() {
                    return;
                }
            }
            Err(e) => {
                if !warned {
                    tracing::warn!(error = %e, "could not run pw-dump; no sound device list");
                    warned = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!([
            {"id": 40, "type": "PipeWire:Interface:Device", "info": {"props": {
                "device.description": "Ryzen HD Audio Controller"}}},
            {"id": 52, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "device.id": 40,
                "node.name": "alsa_output.pci-0000_c4_00.6.analog-stereo",
                "node.description": "Ryzen HD Audio Controller Speaker"}}},
            {"id": 53, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Source", "device.id": 40,
                "node.name": "alsa_input.pci-0000_c4_00.6.analog-stereo",
                "node.description": "Ryzen HD Audio Controller Digital Microphone"}}},
            {"id": 60, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink",
                "node.name": "bluez_output.AA_BB.1",
                "node.description": "WH-1000XM4", "device.form-factor": "headphone"}}},
            {"id": 30, "type": "PipeWire:Interface:Metadata",
             "props": {"metadata.name": "default"},
             "metadata": [
                {"subject": 0, "key": "default.audio.sink", "type": "Spa:String:JSON",
                 "value": {"name": "bluez_output.AA_BB.1"}},
                {"subject": 0, "key": "default.audio.source", "type": "Spa:String:JSON",
                 "value": "{\"name\":\"alsa_input.pci-0000_c4_00.6.analog-stereo\"}"}
             ]}
        ])
    }

    #[test]
    fn default_sink_volume_from_props() {
        let mut g = Graph::default();
        g.apply(json!([
            {"id": 52, "type": "PipeWire:Interface:Node",
             "info": {"props": {"media.class": "Audio/Sink",
                "node.name": "alsa_output.usb-headset", "node.description": "USB Headset",
                "device.form-factor": "headset"},
              "params": {"Props": [{"mute": true, "channelVolumes": [0.125, 0.125]}]}}},
            {"id": 30, "type": "PipeWire:Interface:Metadata",
             "props": {"metadata.name": "default"},
             "metadata": [{"subject": 0, "key": "default.audio.sink",
                "value": {"name": "alsa_output.usb-headset"}}]}
        ]));
        let s = g.state();
        let v = s.output_volume.unwrap();
        assert!((v.volume - 0.5).abs() < 1e-9, "cube root of 0.125");
        assert!(v.muted && v.available);
        assert_eq!(s.output_label.as_deref(), Some("USB Headset"));
        assert!(s.input_volume.is_none());
    }

    #[test]
    fn lists_devices_and_defaults() {
        let mut g = Graph::default();
        g.apply(sample());
        let s = g.state();
        assert_eq!(s.outputs.len(), 2);
        assert_eq!(
            s.outputs[0].description,
            "Ryzen HD Audio Controller Speaker"
        );
        assert_eq!(s.outputs[0].icon, "audio-speakers-symbolic");
        assert_eq!(s.outputs[1].icon, "audio-headphones-symbolic");
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.inputs[0].icon, "audio-input-microphone-symbolic");
        assert_eq!(s.default_output.as_deref(), Some("bluez_output.AA_BB.1"));
        assert_eq!(
            s.default_input.as_deref(),
            Some("alsa_input.pci-0000_c4_00.6.analog-stereo")
        );
        assert!(!s.recording);

        // Unplugging the headphones moves the default back.
        g.apply(json!([
            {"id": 60, "info": null},
            {"id": 30, "type": "PipeWire:Interface:Metadata",
             "props": {"metadata.name": "default"},
             "metadata": [{"subject": 0, "key": "default.audio.sink",
                "value": {"name": "alsa_output.pci-0000_c4_00.6.analog-stereo"}}]}
        ]));
        let s = g.state();
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(
            s.default_output.as_deref(),
            Some("alsa_output.pci-0000_c4_00.6.analog-stereo")
        );
        assert!(s.default_input.is_some(), "metadata updates merge");
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
        assert_eq!(sp(&format!("{ace} Headphones"), ace, None), ("Headphones".into(), ace.into()));
        assert_eq!(
            sp("Creative Pebble Pro Analog Stereo", "Creative Pebble Pro", None),
            ("Analog Stereo".into(), "Creative Pebble Pro".into())
        );
        assert_eq!(
            sp(
                "GB205 High Definition Audio Controller Digital Stereo (HDMI) [CR340HD]",
                "GB205 High Definition Audio Controller",
                None
            ),
            ("CR340HD".into(), "GB205 High Definition Audio Controller".into())
        );
        // No device object: fall back to the profile description.
        assert_eq!(
            sp("USB Audio Analog Stereo", "", Some("Analog Stereo")),
            ("Analog Stereo".into(), "USB Audio".into())
        );
        // One-profile devices are just their name.
        assert_eq!(sp("WH-1000XM4", "WH-1000XM4", None), ("WH-1000XM4".into(), String::new()));
        assert_eq!(sp("WH-1000XM4", "", None), ("WH-1000XM4".into(), String::new()));
    }

    #[test]
    fn recording_ignores_meters_and_loopbacks() {
        let stream = |id: u64, extra: Value| {
            let mut props = json!({"media.class": "Stream/Input/Audio"});
            props
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!([{"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}}])
        };
        let mut g = Graph::default();
        g.apply(sample());
        g.apply(stream(
            70,
            json!({"application.id": "org.PulseAudio.pavucontrol"}),
        ));
        g.apply(stream(71, json!({"stream.monitor": "true"})));
        g.apply(stream(72, json!({"node.link-group": "echo-cancel-1"})));
        assert!(!g.state().recording);
        g.apply(stream(73, json!({"application.name": "Firefox"})));
        assert!(g.state().recording);
        g.apply(json!([{"id": 73, "info": null}]));
        assert!(!g.state().recording);
    }
}

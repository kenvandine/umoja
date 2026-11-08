//! Read-only system diagnostics for the assistant.
//!
//! Lets Umoja's local assistant answer "why is my fan loud?" or "why won't
//! Wi-Fi connect?" from facts instead of guesses. Every collector here only
//! *reads*: `/proc`, `/sys`, `journalctl`, `systemctl status`, `nmcli`
//! queries and friends. Nothing is started, stopped, written or rescanned,
//! no shell is involved (arguments are passed as argv), every command has a
//! timeout, and every report is truncated so it fits a small model's context.
//!
//! Two ways in:
//! * [`report_for`] picks the collectors that match a free-text question
//!   and returns one compact report. The overlay uses it to ground the
//!   prompt before the model answers, which works even with small models
//!   that are unreliable at tool calling.
//! * [`tools`] exposes each collector as a tool spec (name, description,
//!   JSON-schema parameters, blocking `run` function) for the assistant's
//!   tool-calling framework.
//!
//! Collectors block (they spawn processes), so call them off the GTK thread.

use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on the text a single collector returns.
const MAX_SECTION: usize = 2500;
/// Cap on a whole [`report_for`] report.
const MAX_REPORT: usize = 7000;
const CMD_TIMEOUT: Duration = Duration::from_secs(4);

/// A read-only diagnostic tool the assistant can call.
#[derive(Clone, Copy)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema of the arguments object, OpenAI `tools` style.
    pub parameters: fn() -> Value,
    /// Short progress line for the user, e.g. "Check the network".
    pub summary: fn(&Value) -> String,
    /// Always true here; lets a framework skip confirmation prompts.
    pub read_only: bool,
    /// Blocking. Takes the arguments object, returns plain text.
    pub run: fn(&Value) -> String,
}

impl ToolSpec {
    /// The OpenAI-compatible `{"type":"function",...}` declaration.
    pub fn openai_declaration(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": (self.parameters)(),
            }
        })
    }
}

fn no_params() -> Value {
    json!({"type": "object", "properties": {}})
}

fn journal_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "unit": {"type": "string", "description": "Optional systemd unit, e.g. NetworkManager.service"},
            "user": {"type": "boolean", "description": "Unit is a user (session) unit"},
            "minutes": {"type": "integer", "description": "How far back to look, 1-1440 (default 60)"},
            "priority": {"type": "string", "enum": ["err", "warning", "notice", "info"], "description": "Lowest priority shown (default warning)"},
            "grep": {"type": "string", "description": "Optional case-insensitive text to match"}
        }
    })
}

fn unit_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "unit": {"type": "string", "description": "systemd unit name, e.g. bluetooth.service"},
            "user": {"type": "boolean", "description": "Unit is a user (session) unit"}
        },
        "required": ["unit"]
    })
}

/// Every diagnostic tool, in a stable order.
pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "system_resources",
            description: "CPU load, memory, swap and the processes using the most CPU and memory. \
                Use for slowness, a loud fan, heat or high battery drain.",
            parameters: no_params,
            summary: |_| "Check CPU and memory use".into(),
            read_only: true,
            run: |_| resources(),
        },
        ToolSpec {
            name: "thermal_status",
            description: "Temperatures, fan speeds, CPU frequency and the power profile.",
            parameters: no_params,
            summary: |_| "Check temperatures and fans".into(),
            read_only: true,
            run: |_| thermal(),
        },
        ToolSpec {
            name: "power_status",
            description: "Battery charge, health, charge rate and AC adapter state.",
            parameters: no_params,
            summary: |_| "Check the battery".into(),
            read_only: true,
            run: |_| power(),
        },
        ToolSpec {
            name: "network_status",
            description: "NetworkManager state, devices, active connections, nearby Wi-Fi, radio \
                kill switches, DNS and recent NetworkManager/wpa_supplicant errors.",
            parameters: no_params,
            summary: |_| "Check the network".into(),
            read_only: true,
            run: |_| network(),
        },
        ToolSpec {
            name: "bluetooth_status",
            description: "Bluetooth adapter power, paired devices and recent bluetoothd errors.",
            parameters: no_params,
            summary: |_| "Check Bluetooth".into(),
            read_only: true,
            run: |_| bluetooth(),
        },
        ToolSpec {
            name: "audio_status",
            description: "PipeWire/WirePlumber sinks, sources and default devices.",
            parameters: no_params,
            summary: |_| "Check sound devices".into(),
            read_only: true,
            run: |_| audio(),
        },
        ToolSpec {
            name: "disk_usage",
            description: "Free space on mounted filesystems.",
            parameters: no_params,
            summary: |_| "Check disk space".into(),
            read_only: true,
            run: |_| disks(),
        },
        ToolSpec {
            name: "failed_units",
            description: "systemd system and user units that have failed.",
            parameters: no_params,
            summary: |_| "Look for failed services".into(),
            read_only: true,
            run: |_| failed_units(),
        },
        ToolSpec {
            name: "unit_status",
            description: "systemctl status of one unit, with its latest log lines.",
            parameters: unit_params,
            summary: |a| {
                format!(
                    "Check {}",
                    a.get("unit").and_then(Value::as_str).unwrap_or("a service")
                )
            },
            read_only: true,
            run: unit_status_tool,
        },
        ToolSpec {
            name: "journal",
            description: "Recent system journal entries, optionally for one unit or matching text.",
            parameters: journal_params,
            summary: |a| match a.get("unit").and_then(Value::as_str) {
                Some(u) => format!("Read the log for {u}"),
                None => "Read the system log".into(),
            },
            read_only: true,
            run: journal_tool,
        },
    ]
}

/// Look a tool up by name and run it. `None` for unknown names.
pub fn run_tool(name: &str, args: &Value) -> Option<String> {
    tools()
        .into_iter()
        .find(|t| t.name == name)
        .map(|t| (t.run)(args))
}

// ---------------------------------------------------------------- topics

/// A troubleshooting area a question can be about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    Performance,
    Thermal,
    Power,
    Network,
    Bluetooth,
    Audio,
    Disk,
    Services,
}

impl Topic {
    fn collect(self) -> (&'static str, String) {
        match self {
            Topic::Performance => ("Resources", resources()),
            Topic::Thermal => ("Thermals", thermal()),
            Topic::Power => ("Power", power()),
            Topic::Network => ("Network", network()),
            Topic::Bluetooth => ("Bluetooth", bluetooth()),
            Topic::Audio => ("Audio", audio()),
            Topic::Disk => ("Disks", disks()),
            Topic::Services => ("Failed services and recent errors", services()),
        }
    }
}

const TROUBLE_WORDS: &[&str] = &[
    "why",
    "won't",
    "wont",
    "can't",
    "cant",
    "cannot",
    "doesn't",
    "doesnt",
    "isn't",
    "isnt",
    "not working",
    "broken",
    "stopped",
    "keeps",
    "fail",
    "error",
    "problem",
    "issue",
    "wrong",
    "slow",
    "loud",
    "hot",
    "laggy",
    "lag",
    "freez",
    "crash",
    "drop",
    "disconnect",
    "troubleshoot",
    "diagnos",
    "check my",
    "what's using",
    "whats using",
    "what is using",
    "draining",
    "noisy",
    "no sound",
    "no audio",
    "no internet",
    "no wifi",
    "no wi-fi",
    "no signal",
    "not hearing",
    "can't hear",
    "cant hear",
    "silent",
    "muted",
    "not charging",
    "not detected",
    "not showing",
    "missing",
    "stuck",
    "unstable",
];

/// Which topics a question is about, or empty when it doesn't look like a
/// troubleshooting question at all (so ordinary questions stay fast).
pub fn topics_for(query: &str) -> Vec<Topic> {
    let q = query.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| q.contains(w));
    let troubled = has(TROUBLE_WORDS);

    let mut topics = Vec::new();
    // Topic words must start a word, so "disconnecting" isn't "connect"
    // and "dynamic" isn't "mic".
    let mut add = |t: Topic, words: &[&str]| {
        if words.iter().any(|w| starts_word(&q, w)) && !topics.contains(&t) {
            topics.push(t);
        }
    };
    add(
        Topic::Thermal,
        &[
            "fan", "hot", "heat", "temperat", "thermal", "overheat", "noisy", "loud",
        ],
    );
    add(
        Topic::Performance,
        &[
            "fan",
            "loud",
            "noisy",
            "hot",
            "slow",
            "lag",
            "freez",
            "cpu",
            "memory",
            "ram",
            "swap",
            "process",
            "using",
            "hog",
            "sluggish",
            "unresponsive",
        ],
    );
    add(
        Topic::Power,
        &[
            "battery", "charg", "power", "drain", "suspend", "sleep", "plugged",
        ],
    );
    add(
        Topic::Network,
        &[
            "wi-fi", "wifi", "wireless", "network", "internet", "ethernet", "connect", "dns",
            "vpn", "online", "offline", "router", "hotspot",
        ],
    );
    add(
        Topic::Bluetooth,
        &["bluetooth", "headphone", "earbud", "airpod", "pair"],
    );
    add(
        Topic::Audio,
        &["sound", "audio", "speaker", "mic", "volume", "headphone"],
    );
    add(Topic::Disk, &["disk", "storage", "space", "full", "drive"]);
    add(
        Topic::Services,
        &[
            "service", "crash", "fail", "error", "boot", "startup", "systemd", "journal", "log",
        ],
    );

    // "Why is my laptop slow" and similar: name a symptom but no subsystem.
    if topics.is_empty()
        && troubled
        && has(&["computer", "laptop", "system", "pc", "machine", "desktop"])
    {
        topics.extend([Topic::Performance, Topic::Services]);
    }
    // A subsystem word without a sign of trouble ("connect to my VPN how?")
    // is a how-to, not a diagnosis, unless it asks about state.
    if !troubled
        && !has(&[
            "status", "check", "what's", "whats", "which", "how much", "how hot", "is my",
        ])
    {
        topics.clear();
    }
    topics
}

/// Whether `needle` occurs in `hay` at the start of a word.
fn starts_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

/// A compact report for the topics in `query`, or `None` when the question
/// is not about troubleshooting. Blocking.
pub fn report_for(query: &str) -> Option<String> {
    let topics = topics_for(query);
    if topics.is_empty() {
        return None;
    }
    let mut out = String::new();
    for t in topics {
        let (title, body) = t.collect();
        out.push_str(&format!("## {title}\n{}\n\n", body.trim_end()));
    }
    Some(truncate(&out, MAX_REPORT))
}

// ------------------------------------------------------------ collectors

pub fn resources() -> String {
    let mut out = String::new();
    if let Some(load) = read("/proc/loadavg") {
        let f: Vec<&str> = load.split_whitespace().collect();
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        if f.len() >= 3 {
            out.push_str(&format!(
                "Load average: {} {} {} ({cpus} CPUs)\n",
                f[0], f[1], f[2]
            ));
        }
    }
    if let Some(up) =
        read("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
    {
        out.push_str(&format!("Uptime: {}\n", human_duration(up as u64)));
    }
    if let Some(mem) = read("/proc/meminfo") {
        let kb = |k: &str| meminfo(&mem, k);
        if let (Some(total), Some(avail)) = (kb("MemTotal"), kb("MemAvailable")) {
            out.push_str(&format!(
                "Memory: {} used of {} ({} available)\n",
                human_kb(total.saturating_sub(avail)),
                human_kb(total),
                human_kb(avail)
            ));
        }
        if let (Some(total), Some(free)) = (kb("SwapTotal"), kb("SwapFree"))
            && total > 0
        {
            out.push_str(&format!(
                "Swap: {} used of {}\n",
                human_kb(total - free),
                human_kb(total)
            ));
        }
    }
    if let Some(psi) = read("/proc/pressure/cpu").and_then(|p| p.lines().next().map(str::to_string))
    {
        out.push_str(&format!("CPU pressure: {psi}\n"));
    }
    // ps measures %CPU over each process's lifetime, so sample for real.
    out.push_str("Top CPU (sampled over 1s, % of one core):\n");
    for line in top_cpu(Duration::from_millis(1000), 8) {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("Top memory:\n");
    for line in top_memory(6) {
        out.push_str(&format!("  {line}\n"));
    }
    truncate(&out, MAX_SECTION)
}

pub fn thermal() -> String {
    let mut out = String::new();
    // (chip, readings, fan RPMs) per hwmon device.
    let mut chips: Vec<(String, Vec<String>, Vec<(String, u64)>)> = Vec::new();
    for dir in list_dir("/sys/class/hwmon") {
        let name = read(&format!("{dir}/name"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = |kind: &str, idx: &str| {
            read(&format!("{dir}/{kind}{idx}_label"))
                .map(|l| l.trim().to_string())
                .unwrap_or_else(|| format!("{kind}{idx}"))
        };
        let (mut temps, mut fans) = (Vec::new(), Vec::new());
        for f in list_dir(&dir) {
            let file = f.rsplit('/').next().unwrap_or_default();
            let Some(v) = read_num(&f) else { continue };
            if let Some(idx) = file
                .strip_prefix("temp")
                .and_then(|s| s.strip_suffix("_input"))
            {
                // 0°C (or below) is an absent or dead sensor, not a reading.
                if v > 0 {
                    temps.push(format!("{} {:.0}°C", label("temp", idx), v as f64 / 1000.0));
                }
            } else if let Some(idx) = file
                .strip_prefix("fan")
                .and_then(|s| s.strip_suffix("_input"))
            {
                fans.push((label("fan", idx), v));
            }
        }
        chips.push((name, temps, fans));
    }
    // Several drivers often report the same fan; a 0 RPM entry next to a
    // spinning one is the stale ACPI copy.
    let any_spinning = chips.iter().flat_map(|c| &c.2).any(|(_, rpm)| *rpm > 0);
    let mut fan_count = 0;
    for (name, mut readings, fans) in chips {
        for (label, rpm) in fans {
            if rpm > 0 || !any_spinning {
                fan_count += 1;
                readings.push(if rpm > 0 {
                    format!("{label} {rpm} RPM")
                } else {
                    format!("{label} stopped")
                });
            }
        }
        if !readings.is_empty() {
            readings.sort();
            out.push_str(&format!("{name}: {}\n", readings.join(", ")));
        }
    }
    if fan_count == 0 {
        // ThinkPads report the fan through thinkpad_acpi's proc file.
        if let Some(fan) = read("/proc/acpi/ibm/fan") {
            for l in fan
                .lines()
                .filter(|l| l.starts_with("speed:") || l.starts_with("level:"))
            {
                out.push_str(&format!(
                    "fan {}\n",
                    l.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
            }
        }
    }
    let freqs: Vec<u64> = list_dir("/sys/devices/system/cpu/cpufreq")
        .iter()
        .filter_map(|p| read_num(&format!("{p}/scaling_cur_freq")))
        .collect();
    if !freqs.is_empty() {
        let max = freqs.iter().max().copied().unwrap_or(0);
        let avg = freqs.iter().sum::<u64>() / freqs.len() as u64;
        out.push_str(&format!(
            "CPU frequency: average {} MHz, highest {} MHz\n",
            avg / 1000,
            max / 1000
        ));
    }
    if let Some(gov) = read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") {
        out.push_str(&format!("CPU governor: {}\n", gov.trim()));
    }
    if let Some(p) = run("powerprofilesctl", &["get"]) {
        out.push_str(&format!("Power profile: {}\n", p.trim()));
    }
    if out.is_empty() {
        out.push_str("No temperature or fan sensors are exposed.\n");
    }
    truncate(&out, MAX_SECTION)
}

pub fn power() -> String {
    let mut out = String::new();
    for dir in list_dir("/sys/class/power_supply") {
        let name = dir.rsplit('/').next().unwrap_or_default().to_string();
        let get = |k: &str| read(&format!("{dir}/{k}")).map(|v| v.trim().to_string());
        match get("type").as_deref() {
            Some("Battery") => {
                let mut parts = vec![format!("{name}:")];
                if let Some(c) = get("capacity") {
                    parts.push(format!("{c}%"));
                }
                if let Some(s) = get("status") {
                    parts.push(s.to_lowercase());
                }
                let full = get("energy_full")
                    .or_else(|| get("charge_full"))
                    .and_then(|v| v.parse::<f64>().ok());
                let design = get("energy_full_design")
                    .or_else(|| get("charge_full_design"))
                    .and_then(|v| v.parse::<f64>().ok());
                if let (Some(f), Some(d)) = (full, design)
                    && d > 0.0
                {
                    parts.push(format!("health {:.0}% of design", f / d * 100.0));
                }
                if let Some(p) = get("power_now").and_then(|v| v.parse::<f64>().ok())
                    && p > 0.0
                {
                    parts.push(format!("{:.1} W", p / 1e6));
                }
                if let Some(c) = get("cycle_count").filter(|c| c != "0") {
                    parts.push(format!("{c} cycles"));
                }
                if let Some(t) = get("charge_control_end_threshold") {
                    parts.push(format!("charge limit {t}%"));
                }
                out.push_str(&parts.join(" "));
                out.push('\n');
            }
            Some("Mains") | Some("USB") => {
                let online = get("online").as_deref() == Some("1");
                out.push_str(&format!(
                    "{name}: {}\n",
                    if online { "plugged in" } else { "unplugged" }
                ));
            }
            _ => {}
        }
    }
    if let Some(p) = run("powerprofilesctl", &["get"]) {
        out.push_str(&format!("Power profile: {}\n", p.trim()));
    }
    if out.is_empty() {
        out.push_str("No battery or power adapter is reported.\n");
    }
    truncate(&out, MAX_SECTION)
}

pub fn network() -> String {
    let mut out = String::new();
    match run(
        "nmcli",
        &[
            "-t",
            "-f",
            "STATE,CONNECTIVITY,WIFI-HW,WIFI,NETWORKING",
            "general",
        ],
    ) {
        Some(g) => out.push_str(&format!(
            "NetworkManager (state:connectivity:wifi-hw:wifi:networking): {}\n",
            g.trim()
        )),
        None => out.push_str("NetworkManager is not running or nmcli is missing.\n"),
    }
    if let Some(d) = run(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"],
    ) {
        out.push_str("Devices (device:type:state:connection):\n");
        for l in d.lines().filter(|l| !l.contains(":loopback:")).take(10) {
            out.push_str(&format!("  {l}\n"));
        }
    }
    // --rescan no: only report what NetworkManager already knows.
    if let Some(w) = run(
        "nmcli",
        &[
            "-t",
            "-f",
            "IN-USE,SSID,SIGNAL,SECURITY",
            "device",
            "wifi",
            "list",
            "--rescan",
            "no",
        ],
    ) {
        let active = active_wifi();
        let nets = wifi_networks(&w, &active);
        if !nets.is_empty() {
            out.push_str("Wi-Fi networks (strongest access point each):\n");
            for n in nets.iter().take(8) {
                out.push_str(&format!("  {n}\n"));
            }
        }
    }
    let blocked: Vec<String> = list_dir("/sys/class/rfkill")
        .iter()
        .filter_map(|d| {
            let kind = read(&format!("{d}/type"))?.trim().to_string();
            let soft = read(&format!("{d}/soft")).is_some_and(|v| v.trim() == "1");
            let hard = read(&format!("{d}/hard")).is_some_and(|v| v.trim() == "1");
            (soft || hard).then(|| {
                format!(
                    "{kind} {}",
                    if hard {
                        "hard-blocked (hardware switch)"
                    } else {
                        "soft-blocked (airplane mode or toggle)"
                    }
                )
            })
        })
        .collect();
    if !blocked.is_empty() {
        out.push_str(&format!("Radio blocks: {}\n", blocked.join(", ")));
    }
    if let Some(dns) = run("resolvectl", &["dns"]) {
        let servers: Vec<&str> = dns
            .lines()
            .filter(|l| l.split(':').nth(1).is_some_and(|s| !s.trim().is_empty()))
            .take(4)
            .collect();
        if !servers.is_empty() {
            out.push_str(&format!("DNS: {}\n", servers.join("; ")));
        }
    }
    let log = journal_lines(&JournalQuery {
        units: &[
            "NetworkManager.service",
            "wpa_supplicant.service",
            "iwd.service",
        ],
        minutes: 60,
        priority: "warning",
        ..Default::default()
    });
    out.push_str(&format!(
        "Recent network log warnings:\n{}",
        indent(&log, 12)
    ));
    truncate(&out, MAX_SECTION)
}

/// SSIDs NetworkManager has an active Wi-Fi connection on.
fn active_wifi() -> Vec<String> {
    run("nmcli", &["-t", "-f", "TYPE,DEVICE,CONNECTION", "device"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f = split_terse(l);
            (f.first().map(String::as_str) == Some("wifi") && f.len() >= 3 && !f[2].is_empty())
                .then(|| f[2].clone())
        })
        .collect()
}

/// Collapse `nmcli -t` Wi-Fi rows (one per access point) to one line per
/// SSID with its best signal, marking the one in use.
fn wifi_networks(terse: &str, active: &[String]) -> Vec<String> {
    let mut nets: Vec<(String, u32, String, bool)> = Vec::new();
    for l in terse.lines() {
        let f = split_terse(l);
        if f.len() < 4 || f[1].is_empty() {
            continue;
        }
        let (in_use, ssid, sec) = (f[0] == "*", f[1].clone(), f[3].clone());
        let signal = f[2].parse().unwrap_or(0);
        let in_use = in_use || active.contains(&ssid);
        match nets.iter_mut().find(|n| n.0 == ssid) {
            Some(n) => {
                n.1 = n.1.max(signal);
                n.3 |= in_use;
            }
            None => nets.push((ssid, signal, sec, in_use)),
        }
    }
    nets.sort_by(|a, b| b.3.cmp(&a.3).then(b.1.cmp(&a.1)));
    nets.into_iter()
        .map(|(ssid, signal, sec, in_use)| {
            let sec = if sec.is_empty() {
                "open".to_string()
            } else {
                sec
            };
            format!(
                "{ssid}{} signal {signal}% {sec}",
                if in_use { " (connected)" } else { "" }
            )
        })
        .collect()
}

/// Split an `nmcli -t` line on unescaped colons.
fn split_terse(line: &str) -> Vec<String> {
    let (mut fields, mut cur, mut esc) = (Vec::new(), String::new(), false);
    for c in line.chars() {
        match (esc, c) {
            (true, _) => {
                cur.push(c);
                esc = false;
            }
            (false, '\\') => esc = true,
            (false, ':') => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

pub fn bluetooth() -> String {
    let mut out = String::new();
    match run("bluetoothctl", &["show"]) {
        Some(s) => {
            for l in s.lines().map(str::trim).filter(|l| {
                [
                    "Controller",
                    "Powered:",
                    "Discoverable:",
                    "Pairable:",
                    "PowerState:",
                ]
                .iter()
                .any(|k| l.starts_with(k))
            }) {
                out.push_str(&format!("{l}\n"));
            }
        }
        None => out.push_str("No Bluetooth adapter, or bluetoothd is not running.\n"),
    }
    let devices = bluez_devices();
    if devices.is_empty() {
        out.push_str("Known devices: none paired or connected\n");
    } else {
        out.push_str("Known devices:\n");
        for d in devices.iter().take(10) {
            out.push_str(&format!("  {d}\n"));
        }
    }
    let log = journal_lines(&JournalQuery {
        units: &["bluetooth.service"],
        minutes: 120,
        priority: "warning",
        ..Default::default()
    });
    out.push_str(&format!("Recent bluetoothd warnings:\n{}", indent(&log, 8)));
    truncate(&out, MAX_SECTION)
}

/// Paired and connected devices from BlueZ over D-Bus (bluetoothctl's
/// device listing needs an interactive agent on some versions).
fn bluez_devices() -> Vec<String> {
    let Some(json) = run(
        "busctl",
        &[
            "--system",
            "--json=short",
            "call",
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
        ],
    ) else {
        return Vec::new();
    };
    bluez_devices_from(&json)
}

fn bluez_devices_from(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(objects) = v["data"][0].as_object() else {
        return Vec::new();
    };
    // busctl wraps each variant as {"type": .., "data": ..}.
    let prop = |ifaces: &Value, iface: &str, key: &str| ifaces[iface][key]["data"].clone();
    let mut rows = Vec::new();
    for ifaces in objects.values() {
        if ifaces.get("org.bluez.Device1").is_none() {
            continue;
        }
        let dev = |k| prop(ifaces, "org.bluez.Device1", k);
        let paired = dev("Paired").as_bool().unwrap_or(false);
        let connected = dev("Connected").as_bool().unwrap_or(false);
        if !paired && !connected {
            continue; // just something seen in a scan
        }
        let name = dev("Alias")
            .as_str()
            .or(dev("Name").as_str())
            .unwrap_or("unknown")
            .to_string();
        let mut row = name;
        if let Some(icon) = dev("Icon").as_str() {
            row.push_str(&format!(" [{icon}]"));
        }
        row.push_str(if connected {
            " connected"
        } else {
            " paired, not connected"
        });
        if dev("Trusted").as_bool() == Some(false) {
            row.push_str(", not trusted");
        }
        if let Some(b) = prop(ifaces, "org.bluez.Battery1", "Percentage").as_u64() {
            row.push_str(&format!(", battery {b}%"));
        }
        rows.push((connected, row));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    rows.into_iter().map(|r| r.1).collect()
}

pub fn audio() -> String {
    let mut out = String::new();
    match run("wpctl", &["status"]) {
        Some(s) => {
            // Keep the Audio section (sinks/sources/defaults), drop Video etc.
            let audio = s
                .lines()
                .skip_while(|l| !l.starts_with("Audio"))
                .take_while(|l| !l.starts_with("Video"));
            for l in audio.filter(|l| !l.trim_matches(['│', '├', '└', '─', ' ']).is_empty())
            {
                out.push_str(l.trim_end());
                out.push('\n');
            }
        }
        None => out.push_str("PipeWire is not running (wpctl status failed).\n"),
    }
    for unit in [
        "pipewire.service",
        "wireplumber.service",
        "pipewire-pulse.service",
    ] {
        if let Some(state) = run("systemctl", &["--user", "is-active", unit]) {
            out.push_str(&format!("{unit}: {}\n", state.trim()));
        }
    }
    truncate(&out, MAX_SECTION)
}

pub fn disks() -> String {
    let out = run(
        "df",
        &[
            "-h",
            "--output=target,fstype,size,used,avail,pcent",
            "-x",
            "tmpfs",
            "-x",
            "devtmpfs",
            "-x",
            "squashfs",
            "-x",
            "overlay",
            "-x",
            "efivarfs",
        ],
    )
    .unwrap_or_else(|| "df failed.\n".into());
    truncate(&out, MAX_SECTION)
}

pub fn failed_units() -> String {
    let mut out = String::new();
    for (label, user) in [("System", false), ("User", true)] {
        let mut args = vec!["--failed", "--no-legend", "--plain", "--no-pager"];
        if user {
            args.insert(0, "--user");
        }
        let list = run("systemctl", &args).unwrap_or_default();
        let units: Vec<&str> = list
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        out.push_str(&format!(
            "{label} failed units: {}\n",
            if units.is_empty() {
                "none".to_string()
            } else {
                units.join(", ")
            }
        ));
    }
    out
}

fn services() -> String {
    let mut out = failed_units();
    let log = journal_lines(&JournalQuery {
        minutes: 30,
        priority: "err",
        ..Default::default()
    });
    out.push_str(&format!(
        "Errors in the last 30 minutes:\n{}",
        indent(&log, 15)
    ));
    truncate(&out, MAX_SECTION)
}

/// `systemctl status` for one unit. Rejects anything that isn't a unit name.
pub fn unit_status(unit: &str, user: bool) -> String {
    let Some(unit) = valid_unit(unit) else {
        return format!("\"{unit}\" is not a valid unit name.");
    };
    let mut args = vec!["status", "--no-pager", "--lines=15", "--full"];
    if user {
        args.insert(0, "--user");
    }
    args.push("--");
    args.push(&unit);
    // `systemctl status` exits non-zero for inactive units; keep the text.
    let out = run_any("systemctl", &args).unwrap_or_else(|| "systemctl is not available.".into());
    truncate(&out, MAX_SECTION)
}

fn unit_status_tool(args: &Value) -> String {
    let unit = args.get("unit").and_then(Value::as_str).unwrap_or_default();
    let user = args.get("user").and_then(Value::as_bool).unwrap_or(false);
    unit_status(unit, user)
}

fn journal_tool(args: &Value) -> String {
    let unit = args
        .get("unit")
        .and_then(Value::as_str)
        .and_then(valid_unit);
    let units: Vec<&str> = unit.iter().map(String::as_str).collect();
    let priority = match args.get("priority").and_then(Value::as_str) {
        Some(p @ ("err" | "warning" | "notice" | "info")) => p,
        _ => "warning",
    };
    let grep = args
        .get("grep")
        .and_then(Value::as_str)
        .map(|g| g.chars().take(80).collect::<String>());
    let lines = journal_lines(&JournalQuery {
        units: &units,
        user: args.get("user").and_then(Value::as_bool).unwrap_or(false),
        minutes: args
            .get("minutes")
            .and_then(Value::as_u64)
            .unwrap_or(60)
            .clamp(1, 1440) as u32,
        priority,
        grep: grep.as_deref(),
    });
    truncate(&indent(&lines, 40), MAX_SECTION)
}

// --------------------------------------------------------------- journal

#[derive(Default)]
struct JournalQuery<'a> {
    units: &'a [&'a str],
    user: bool,
    minutes: u32,
    priority: &'a str,
    grep: Option<&'a str>,
}

impl JournalQuery<'_> {
    fn args(&self) -> Vec<String> {
        let mut a = vec![
            "--no-pager".to_string(),
            "--quiet".into(),
            "-o".into(),
            "short".into(),
            "-n".into(),
            "60".into(),
            "-b".into(),
            format!("--since=-{}min", self.minutes.max(1)),
            format!(
                "--priority={}",
                if self.priority.is_empty() {
                    "warning"
                } else {
                    self.priority
                }
            ),
        ];
        if self.user {
            a.push("--user".into());
        }
        for u in self.units {
            a.push(if self.user {
                "--user-unit".into()
            } else {
                "-u".into()
            });
            a.push((*u).into());
        }
        if let Some(g) = self.grep.filter(|g| !g.is_empty()) {
            a.push("--case-sensitive=false".into());
            a.push(format!("--grep={}", regex_escape(g)));
        }
        a
    }
}

/// Journal lines, de-duplicated and with the hostname column dropped.
fn journal_lines(q: &JournalQuery) -> String {
    let args = q.args();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(text) = run_any("journalctl", &argv) else {
        return "journalctl is not available.\n".into();
    };
    let host = hostname();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = strip_host(line, &host);
        // Repeated messages differ only by timestamp; keep the latest few.
        let key = line.splitn(4, ' ').nth(3).unwrap_or(&line).to_string();
        if seen.insert(key) {
            out.push(line);
        }
    }
    if out.is_empty() || text.contains("-- No entries --") && out.len() <= 1 {
        return "(none)\n".into();
    }
    // Most recent last, and the most recent matter most.
    let skip = out.len().saturating_sub(40);
    out[skip..].join("\n") + "\n"
}

fn strip_host(line: &str, host: &str) -> String {
    // "Nov 08 03:12:01 umoja NetworkManager[812]: ..." -> drop "umoja ".
    let mut parts = line.splitn(5, ' ');
    let (Some(m), Some(d), Some(t), Some(h), Some(rest)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return line.to_string();
    };
    if !host.is_empty() && h == host {
        format!("{m} {d} {t} {rest}")
    } else {
        line.to_string()
    }
}

fn hostname() -> String {
    read("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Accepts names like `NetworkManager.service`, `user@1000.service`,
/// `sys-subsystem-net.device`; rejects options and paths.
fn valid_unit(unit: &str) -> Option<String> {
    let unit = unit.trim();
    let ok = !unit.is_empty()
        && unit.len() <= 128
        && !unit.starts_with('-')
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._:-\\".contains(c));
    ok.then(|| unit.to_string())
}

// --------------------------------------------------------------- helpers

/// A readable process name. Some programs (Claude Code installs, for one)
/// run from a binary named after a version, so `comm` is "2.1.319"; then
/// use argv[0] instead.
fn process_name(pid: u32, comm: &str) -> String {
    if comm.chars().any(char::is_alphabetic) {
        return comm.to_string();
    }
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|c| {
            let argv0 = String::from_utf8_lossy(c.split(|b| *b == 0).next()?).into_owned();
            name_from_path(&argv0)
        })
        .unwrap_or_else(|| comm.to_string())
}

/// The last path component that reads like a name: for
/// `~/.local/share/claude/versions/2.1.319` that is "claude".
fn name_from_path(path: &str) -> Option<String> {
    path.rsplit('/')
        .map(str::trim)
        .find(|p| {
            p.chars().any(char::is_alphabetic) && !matches!(*p, "versions" | "bin" | "current")
        })
        .map(str::to_string)
}

/// Processes with the largest resident memory, from /proc.
fn top_memory(n: usize) -> Vec<String> {
    let me = std::process::id();
    let mut rows: Vec<(u64, u32, String)> = list_dir("/proc")
        .iter()
        .filter_map(|p| {
            let pid: u32 = p.rsplit('/').next()?.parse().ok()?;
            let status = read(&format!("{p}/status"))?;
            let rss = meminfo(&status, "VmRSS")?;
            let comm = status
                .lines()
                .next()?
                .strip_prefix("Name:")?
                .trim()
                .to_string();
            (pid != me).then(|| (rss, pid, process_name(pid, &comm)))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter()
        .take(n)
        .map(|(rss, pid, name)| format!("{name} (pid {pid}) {}", human_kb(rss)))
        .collect()
}

/// Per-process CPU use measured between two /proc snapshots.
fn top_cpu(window: Duration, n: usize) -> Vec<String> {
    fn snapshot() -> Vec<(u32, String, u64)> {
        list_dir("/proc")
            .iter()
            .filter_map(|p| {
                let pid: u32 = p.rsplit('/').next()?.parse().ok()?;
                let stat = read(&format!("{p}/stat"))?;
                let (l, r) = (stat.find('(')?, stat.rfind(')')?);
                let comm = process_name(pid, &stat[l + 1..r]);
                let f: Vec<&str> = stat[r + 2..].split_whitespace().collect();
                let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
                Some((pid, comm, ticks))
            })
            .collect()
    }
    let before = snapshot();
    let start = Instant::now();
    std::thread::sleep(window);
    let secs = start.elapsed().as_secs_f64().max(0.01);
    let after = snapshot();
    let before: std::collections::HashMap<u32, u64> =
        before.into_iter().map(|(p, _, t)| (p, t)).collect();
    // USER_HZ is 100 on every Linux architecture Ubuntu ships.
    let mut rows: Vec<(f64, u32, String)> = after
        .into_iter()
        .filter_map(|(pid, comm, t)| {
            let d = t.saturating_sub(*before.get(&pid)?);
            let pct = d as f64 / 100.0 / secs * 100.0;
            (pct >= 0.5 && pid != std::process::id()).then_some((pct, pid, comm))
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    if rows.is_empty() {
        return vec!["nothing above 0.5%".into()];
    }
    rows.into_iter()
        .take(n)
        .map(|(pct, pid, comm)| format!("{comm} (pid {pid}) {pct:.0}%"))
        .collect()
}

fn meminfo(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with(key) && l[key.len()..].starts_with(':'))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn human_kb(kb: u64) -> String {
    let gb = kb as f64 / 1024.0 / 1024.0;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", kb / 1024)
    }
}

fn human_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_num(path: &str) -> Option<u64> {
    read(path)?.trim().parse().ok()
}

fn list_dir(path: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(Path::new(path)) else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .filter_map(|e| Some(e.ok()?.path().to_str()?.to_string()))
        .collect();
    v.sort();
    v
}

fn indent(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].iter().map(|l| format!("  {l}\n")).collect()
}

/// Cut at a char boundary and say so.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[…truncated]\n", &s[..end])
}

/// Run a command, returning stdout only on success.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    run_inner(cmd, args).and_then(|(ok, out)| ok.then_some(out))
}

/// Run a command, returning stdout whatever the exit status.
fn run_any(cmd: &str, args: &[&str]) -> Option<String> {
    run_inner(cmd, args).map(|(_, out)| out)
}

fn run_inner(cmd: &str, args: &[&str]) -> Option<(bool, String)> {
    let mut child = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C.UTF-8")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a thread so a chatty command can't fill the pipe and stall.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(256 * 1024).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < CMD_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = String::from_utf8_lossy(&reader.join().ok()?).into_owned();
    Some((status.is_some_and(|s| s.success()), out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_from_questions() {
        let t = topics_for("Why is my fan so loud?");
        assert!(t.contains(&Topic::Thermal) && t.contains(&Topic::Performance));
        assert_eq!(topics_for("Why won't Wi-Fi connect?"), vec![Topic::Network]);
        assert!(
            topics_for("my bluetooth headphones keep disconnecting").contains(&Topic::Bluetooth)
        );
        assert!(topics_for("why is my laptop so slow").contains(&Topic::Performance));
        assert!(topics_for("battery draining fast").contains(&Topic::Power));
        assert!(topics_for("how hot is my cpu").contains(&Topic::Thermal));
    }

    #[test]
    fn symptom_phrasings() {
        assert_eq!(topics_for("no sound from speakers"), vec![Topic::Audio]);
        let bt = topics_for("my bluetooth headphones keep disconnecting");
        assert!(!bt.contains(&Topic::Network), "{bt:?}");
        assert!(topics_for("wifi keeps disconnecting").contains(&Topic::Network));
        assert!(
            topics_for("dynamic wallpaper is broken")
                .iter()
                .all(|t| *t != Topic::Audio)
        );
    }

    #[test]
    fn wifi_rows_collapse_per_ssid() {
        let rows = " :posey:85:WPA2\n*:posey:95:WPA2\n :Kramden Guest:94:WPA2\n :a\\:b:40:\n";
        let nets = wifi_networks(rows, &[]);
        assert_eq!(
            nets,
            vec![
                "posey (connected) signal 95% WPA2",
                "Kramden Guest signal 94% WPA2",
                "a:b signal 40% open",
            ]
        );
        let nets = wifi_networks(" :posey:85:WPA2\n :other:90:WPA2", &["posey".into()]);
        assert_eq!(nets[0], "posey (connected) signal 85% WPA2");
    }

    #[test]
    fn version_named_binaries() {
        assert_eq!(
            name_from_path("/home/ken/.local/share/claude/versions/2.1.319").as_deref(),
            Some("claude")
        );
        assert_eq!(
            name_from_path("/usr/bin/firefox").as_deref(),
            Some("firefox")
        );
        assert_eq!(name_from_path("1.2.3"), None);
    }

    #[test]
    fn bluez_objects() {
        let json = r#"{"type":"a{oa{sa{sv}}}","data":[{
            "/org/bluez/hci0":{"org.bluez.Adapter1":{"Powered":{"type":"b","data":true}}},
            "/org/bluez/hci0/dev_1":{"org.bluez.Device1":{"Alias":{"type":"s","data":"WH-1000XM5"},"Icon":{"type":"s","data":"audio-headset"},"Paired":{"type":"b","data":true},"Connected":{"type":"b","data":true},"Trusted":{"type":"b","data":true}},"org.bluez.Battery1":{"Percentage":{"type":"y","data":60}}},
            "/org/bluez/hci0/dev_2":{"org.bluez.Device1":{"Alias":{"type":"s","data":"Mouse"},"Paired":{"type":"b","data":true},"Connected":{"type":"b","data":false}}},
            "/org/bluez/hci0/dev_3":{"org.bluez.Device1":{"Alias":{"type":"s","data":"Stranger TV"},"Paired":{"type":"b","data":false},"Connected":{"type":"b","data":false}}}
        }]}"#;
        assert_eq!(
            bluez_devices_from(json),
            vec![
                "WH-1000XM5 [audio-headset] connected, battery 60%",
                "Mouse paired, not connected",
            ]
        );
    }

    #[test]
    fn how_to_questions_skip_diagnostics() {
        assert!(topics_for("How do I set up a WireGuard VPN?").is_empty());
        assert!(topics_for("convert sample.png to webp").is_empty());
        assert!(topics_for("Scale my secondary 4K display to 150%").is_empty());
        assert!(report_for("open firefox").is_none());
    }

    #[test]
    fn unit_names_are_validated() {
        assert_eq!(
            valid_unit("NetworkManager.service").as_deref(),
            Some("NetworkManager.service")
        );
        assert!(valid_unit("user@1000.service").is_some());
        assert!(valid_unit("--root=/").is_none());
        assert!(valid_unit("a; rm -rf /").is_none());
        assert!(valid_unit("../etc").is_none());
        assert!(valid_unit("").is_none());
    }

    #[test]
    fn journal_args_are_bounded() {
        let a = JournalQuery {
            units: &["bluetooth.service"],
            minutes: 10,
            priority: "err",
            grep: Some("a.b("),
            ..Default::default()
        }
        .args();
        assert!(a.contains(&"--since=-10min".to_string()));
        assert!(a.contains(&"-u".to_string()));
        assert!(a.contains(&"--grep=a\\.b\\(".to_string()));
        assert!(a.contains(&"-n".to_string()));
    }

    #[test]
    fn hostname_column_dropped() {
        assert_eq!(
            strip_host(
                "Nov 08 03:12:01 umoja NetworkManager[812]: <warn> x",
                "umoja"
            ),
            "Nov 08 03:12:01 NetworkManager[812]: <warn> x"
        );
        assert_eq!(strip_host("short line", "umoja"), "short line");
    }

    #[test]
    fn truncation_respects_utf8() {
        let s = "°".repeat(100);
        let t = truncate(&s, 51);
        assert!(t.ends_with("[…truncated]\n"));
    }

    #[test]
    fn tools_are_read_only_and_declared() {
        let tools = tools();
        assert!(tools.iter().all(|t| t.read_only));
        let mut names: Vec<_> = tools.iter().map(|t| t.name).collect();
        names.dedup();
        assert_eq!(names.len(), tools.len());
        for t in &tools {
            let d = t.openai_declaration();
            assert_eq!(d["function"]["parameters"]["type"], "object");
        }
        assert!(run_tool("nope", &json!({})).is_none());
    }

    #[test]
    fn collectors_run_without_panicking() {
        // Exercise the real collectors on whatever machine runs the tests.
        for t in tools() {
            let out = (t.run)(&json!({"unit": "dbus.service"}));
            assert!(out.len() <= MAX_SECTION + 32, "{} too long", t.name);
        }
        assert!(meminfo("MemTotal:  1024 kB\nMemTotalX: 5 kB", "MemTotal") == Some(1024));
    }
}

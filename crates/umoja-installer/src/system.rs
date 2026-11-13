//! What the installer needs to know about the machine, and the checks on
//! what the user types. No GTK here, so it can be unit-tested.

use serde::Deserialize;
use std::process::Command;

/// Smallest disk worth offering: the image's root is 16 GiB, plus /boot
/// and the EFI partition and some room to live in.
pub const MIN_DISK_BYTES: u64 = 24 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    pub path: String,
    pub size: u64,
    pub model: String,
    pub transport: String,
    pub removable: bool,
}

impl Disk {
    /// "Samsung SSD 990 PRO · 1 TB · NVMe"
    pub fn description(&self) -> String {
        let mut parts = vec![];
        if !self.model.is_empty() {
            parts.push(self.model.clone());
        }
        parts.push(human_size(self.size));
        let kind = match self.transport.as_str() {
            "nvme" => "NVMe".to_string(),
            "usb" => "USB".to_string(),
            "sata" | "ata" => "SATA".to_string(),
            "virtio" | "" => String::new(),
            t => t.to_uppercase(),
        };
        if !kind.is_empty() {
            parts.push(kind);
        }
        parts.join(" · ")
    }
}

/// Decimal units, as disks are sold.
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e12 {
        format!("{:.1} TB", b / 1e12)
    } else {
        format!("{:.0} GB", b / 1e9)
    }
}

#[derive(Deserialize)]
struct Lsblk {
    blockdevices: Vec<LsblkDev>,
}

#[derive(Deserialize)]
struct LsblkDev {
    path: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    size: Option<serde_json::Value>,
    model: Option<String>,
    tran: Option<String>,
    rm: Option<serde_json::Value>,
    ro: Option<serde_json::Value>,
}

fn as_u64(v: &Option<serde_json::Value>) -> u64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_bool(v: &Option<serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "1" || s == "true",
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// Disks Umoja can go on, from `lsblk -J -d -b -o PATH,TYPE,SIZE,MODEL,TRAN,RM,RO`
/// output: whole writable disks big enough, minus `exclude` (the live
/// medium).
pub fn parse_disks(json: &str, exclude: &[String]) -> Vec<Disk> {
    let Ok(l) = serde_json::from_str::<Lsblk>(json) else {
        return vec![];
    };
    l.blockdevices
        .into_iter()
        .filter_map(|d| {
            let path = d.path?;
            if d.kind.as_deref() != Some("disk") || as_bool(&d.ro) || exclude.contains(&path) {
                return None;
            }
            // zram swap and friends show up as "disk" too.
            if path.starts_with("/dev/zram") || path.starts_with("/dev/loop") {
                return None;
            }
            let size = as_u64(&d.size);
            if size < MIN_DISK_BYTES {
                return None;
            }
            Some(Disk {
                path,
                size,
                model: d.model.unwrap_or_default().trim().to_string(),
                transport: d.tran.unwrap_or_default(),
                removable: as_bool(&d.rm),
            })
        })
        .collect()
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where casper mounts the live medium; the installer copies this file.
const SQUASHFS: [&str; 2] = [
    "/cdrom/casper/umoja.squashfs",
    "/run/live/medium/casper/umoja.squashfs",
];

/// Read the system image into the page cache in the background while the
/// user answers the questions, so the copy reads RAM instead of a slow USB
/// stick. Only a read: if memory gets tight the kernel just drops it, and
/// the copy reads the stick as before. Idle I/O priority, so the live
/// session stays responsive.
pub fn prefetch_squashfs() {
    let Some(path) = SQUASHFS.into_iter().find(|p| std::path::Path::new(p).is_file()) else {
        return;
    };
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX);
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| mem_available(&m))
        .unwrap_or(0);
    if !worth_prefetching(size, available) {
        tracing::info!(size, available, "not prefetching the system image: not enough memory");
        return;
    }
    tracing::info!(path, size, "prefetching the system image");
    let _ = Command::new("ionice")
        .args(["-c3", "nice", "-n19", "cat", path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// MemAvailable from /proc/meminfo, in bytes.
fn mem_available(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|l| {
        let kb = l.strip_prefix("MemAvailable:")?.trim().strip_suffix("kB")?.trim();
        kb.parse::<u64>().ok().map(|k| k * 1024)
    })
}

/// Keep at least 2 GiB free for the live session after caching the image.
fn worth_prefetching(size: u64, available: u64) -> bool {
    size.saturating_add(2 << 30) <= available
}

/// The disk the live session was started from, so it isn't offered.
fn live_medium() -> Vec<String> {
    let mut out = vec![];
    for mount in ["/cdrom", "/run/live/medium"] {
        if let Some(src) = output("findmnt", &["-no", "SOURCE", mount]) {
            if let Some(parent) = output("lsblk", &["-npdo", "PKNAME", &src]) {
                if !parent.is_empty() {
                    out.push(parent);
                }
            }
            out.push(src);
        }
    }
    out
}

pub fn disks() -> Vec<Disk> {
    let json = output(
        "lsblk",
        &["-J", "-d", "-b", "-o", "PATH,TYPE,SIZE,MODEL,TRAN,RM,RO"],
    )
    .unwrap_or_default();
    let mut list = parse_disks(&json, &live_medium());
    // Internal disks first, then by size.
    list.sort_by_key(|d| (d.removable, d.transport == "usb", std::cmp::Reverse(d.size)));
    list
}

pub fn timezones() -> Vec<String> {
    let mut zones: Vec<String> = output("timedatectl", &["list-timezones"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    if zones.is_empty() {
        // zone1970.tab: "CC<TAB>coords<TAB>Zone/Name..."
        if let Ok(tab) = std::fs::read_to_string("/usr/share/zoneinfo/zone1970.tab") {
            zones = tab
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| l.split('\t').nth(2).map(str::to_string))
                .collect();
        }
    }
    if !zones.iter().any(|z| z == "UTC") {
        zones.push("UTC".into());
    }
    zones.sort();
    zones.dedup();
    zones
}

/// "America/New_York" -> "New York (America/New_York)"; "UTC" stays.
pub fn timezone_label(zone: &str) -> String {
    match zone.rsplit_once('/') {
        Some((_, city)) => format!("{} ({zone})", city.replace('_', " ")),
        None => zone.to_string(),
    }
}

pub fn current_timezone() -> String {
    output("timedatectl", &["show", "-p", "Timezone", "--value"])
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/timezone").ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "UTC".into())
}

/// True when the live session was started with `umoja.autoinstall`: no
/// questions, first disk, test account, power off when done. For CI.
pub fn autoinstall() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|c| c.split_whitespace().any(|w| w == "umoja.autoinstall"))
        .unwrap_or(false)
}

/// "Ken Vandine" -> "ken"
pub fn username_from(full_name: &str) -> String {
    let first = full_name.split_whitespace().next().unwrap_or("");
    let mut out: String = first
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    while out.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
        out.remove(0);
    }
    out.truncate(32);
    out
}

/// Names useradd and the system already use.
const RESERVED: &[&str] = &[
    "root", "daemon", "bin", "sys", "sync", "games", "man", "lp", "mail", "news", "uucp",
    "proxy", "www-data", "backup", "list", "irc", "nobody", "systemd-network", "messagebus",
    "syslog", "lightdm", "admin", "sudo", "adm", "users",
];

pub fn check_username(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Choose a user name");
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or(' ');
    if !(first.is_ascii_lowercase() || first == '_')
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err("Use lowercase letters, digits, - and _, starting with a letter");
    }
    if name.len() > 32 {
        return Err("At most 32 characters");
    }
    if RESERVED.contains(&name) {
        return Err("That name is taken by the system");
    }
    Ok(())
}

pub fn check_hostname(name: &str) -> Result<(), &'static str> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err("Letters, digits and -, up to 63 characters")
    }
}

pub fn check_password(password: &str, confirm: &str) -> Result<(), &'static str> {
    if password.chars().count() < 8 {
        return Err("At least 8 characters: it also protects the disk");
    }
    if password != confirm {
        return Err("The passwords don't match");
    }
    Ok(())
}

/// One line of `umoja-install` output.
#[derive(Debug, PartialEq)]
pub enum Event {
    Progress(u32, String),
    Error(String),
    Log,
}

pub fn parse_line(line: &str) -> Event {
    if let Some(rest) = line.strip_prefix("PROGRESS ") {
        let (pct, what) = rest.split_once(' ').unwrap_or((rest, ""));
        if let Ok(p) = pct.parse::<u32>() {
            return Event::Progress(p.min(100), what.to_string());
        }
    }
    if let Some(rest) = line.strip_prefix("ERROR ") {
        return Event::Error(rest.to_string());
    }
    Event::Log
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = r#"{"blockdevices": [
        {"path":"/dev/sda","type":"disk","size":31457280000,"model":"QEMU HARDDISK  ","tran":"sata","rm":false,"ro":false},
        {"path":"/dev/sr0","type":"rom","size":4000000000,"model":"QEMU DVD-ROM","tran":"sata","rm":true,"ro":false},
        {"path":"/dev/nvme0n1","type":"disk","size":"1000204886016","model":"Samsung SSD 990 PRO 1TB","tran":"nvme","rm":"0","ro":"0"},
        {"path":"/dev/sdb","type":"disk","size":64000000000,"model":"Flash","tran":"usb","rm":true,"ro":false},
        {"path":"/dev/sdc","type":"disk","size":8000000000,"model":"Tiny","tran":"usb","rm":true,"ro":false},
        {"path":"/dev/zram0","type":"disk","size":64000000000,"model":null,"tran":null,"rm":false,"ro":false}
    ]}"#;

    #[test]
    fn disks_filtered() {
        let d = parse_disks(LSBLK, &["/dev/sdb".into()]);
        let paths: Vec<_> = d.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/dev/sda", "/dev/nvme0n1"]);
        assert_eq!(d[0].model, "QEMU HARDDISK");
        assert_eq!(d[1].description(), "Samsung SSD 990 PRO 1TB · 1.0 TB · NVMe");
    }

    #[test]
    fn bad_json_is_no_disks() {
        assert!(parse_disks("nope", &[]).is_empty());
    }

    #[test]
    fn usernames() {
        assert_eq!(username_from("Ken Vandine"), "ken");
        assert_eq!(username_from("  Zoë "), "zo");
        assert_eq!(username_from("42 Douglas"), "");
        assert!(check_username("ken").is_ok());
        assert!(check_username("Ken").is_err());
        assert!(check_username("root").is_err());
        assert!(check_username("1abc").is_err());
        assert!(check_username("").is_err());
    }

    #[test]
    fn hostnames() {
        assert!(check_hostname("ken-laptop").is_ok());
        assert!(check_hostname("-x").is_err());
        assert!(check_hostname("has space").is_err());
    }

    #[test]
    fn passwords() {
        assert!(check_password("short", "short").is_err());
        assert!(check_password("longenough", "longenougH").is_err());
        assert!(check_password("longenough", "longenough").is_ok());
    }

    #[test]
    fn timezone_labels() {
        assert_eq!(timezone_label("America/New_York"), "New York (America/New_York)");
        assert_eq!(timezone_label("America/Argentina/Buenos_Aires"), "Buenos Aires (America/Argentina/Buenos_Aires)");
        assert_eq!(timezone_label("UTC"), "UTC");
    }

    #[test]
    fn lines() {
        assert_eq!(parse_line("PROGRESS 42 Copying Umoja"), Event::Progress(42, "Copying Umoja".into()));
        assert_eq!(parse_line("PROGRESS 140 x"), Event::Progress(100, "x".into()));
        assert_eq!(parse_line("ERROR no disk"), Event::Error("no disk".into()));
        assert_eq!(parse_line("mke2fs 1.47"), Event::Log);
    }

    #[test]
    fn prefetch_needs_room() {
        let m = "MemTotal:       16000000 kB\nMemAvailable:    8388608 kB\n";
        assert_eq!(mem_available(m), Some(8 << 30));
        assert!(worth_prefetching(4 << 30, 8 << 30));
        assert!(!worth_prefetching(4 << 30, 5 << 30));
        assert!(!worth_prefetching(u64::MAX, 8 << 30));
    }
}

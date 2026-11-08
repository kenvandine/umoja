//! When indexing is allowed to run.
//!
//! Embedding is the expensive part (it wakes the GPU), so it only runs on
//! mains power. On battery the indexer keeps the file list current at most
//! once an hour from cached metadata, which costs a directory walk and no
//! content reads. The embedding model is downloaded only on an unmetered
//! connection, like GNOME's other background downloads.

use std::path::Path;

/// What the power supply allows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    /// On AC, or a machine without a battery.
    Mains,
    /// On battery with this much charge left (percent).
    Battery(u8),
}

impl Power {
    pub fn on_mains(self) -> bool {
        matches!(self, Power::Mains)
    }
}

pub fn current() -> Power {
    read(Path::new("/sys/class/power_supply"))
}

/// Read the state from a sysfs `power_supply` directory.
pub fn read(root: &Path) -> Power {
    let Ok(dir) = std::fs::read_dir(root) else {
        return Power::Mains;
    };
    let mut mains_online = false;
    let mut batteries: Vec<u8> = Vec::new();
    for e in dir.flatten() {
        let p = e.path();
        let get = |f: &str| {
            std::fs::read_to_string(p.join(f))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        match get("type").as_str() {
            "Mains" | "USB" => mains_online |= get("online") == "1",
            // Peripheral batteries (mice, headsets) report scope=Device.
            "Battery" if get("scope") != "Device" => {
                if let Ok(c) = get("capacity").parse::<u8>() {
                    batteries.push(c.min(100));
                }
            }
            _ => {}
        }
    }
    if mains_online || batteries.is_empty() {
        Power::Mains
    } else {
        Power::Battery(batteries.into_iter().min().unwrap_or(0))
    }
}

/// True when NetworkManager says the connection is metered (or guesses so).
/// Without NetworkManager we assume unmetered, like GNetworkMonitor does.
pub async fn network_metered(conn: &zbus::Connection) -> bool {
    let Ok(proxy) = zbus::Proxy::new(
        conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await
    else {
        return false;
    };
    // NMMetered: 1 = yes, 3 = guess yes.
    matches!(proxy.get_property::<u32>("Metered").await, Ok(1 | 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (f, v) in files {
            std::fs::write(d.join(f), format!("{v}\n")).unwrap();
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("umoja-files-power-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn laptop_on_ac_and_battery() {
        let d = tmp("laptop");
        supply(&d, "AC", &[("type", "Mains"), ("online", "1")]);
        supply(&d, "BAT0", &[("type", "Battery"), ("capacity", "57")]);
        assert_eq!(read(&d), Power::Mains);
        supply(&d, "AC", &[("type", "Mains"), ("online", "0")]);
        assert_eq!(read(&d), Power::Battery(57));
    }

    #[test]
    fn desktop_and_mouse_battery() {
        let d = tmp("desktop");
        supply(
            &d,
            "hidpp_battery_0",
            &[("type", "Battery"), ("scope", "Device"), ("capacity", "5")],
        );
        assert_eq!(read(&d), Power::Mains);
        assert_eq!(read(&d.join("missing")), Power::Mains);
    }
}

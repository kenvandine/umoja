//! GPU choice on hybrid (integrated + NVIDIA) laptops.
//!
//! Mir's gbm-kms platform already refuses to *display* on a card driven by
//! the proprietary `nvidia` driver, but its rendering probe still opens the
//! NVIDIA render node through NVIDIA's GBM/EGL. When that succeeds Mir can
//! end up compositing on the discrete GPU and copying every frame across to
//! the integrated one that drives the panel, which keeps the dGPU awake and,
//! with some driver versions, fails outright at start-up (the login screen
//! never appears). With nouveau, Mir also lights up the dGPU's outputs.
//!
//! Umoja's shell should always run on the integrated GPU, as GNOME does on
//! these machines (apps can still be offloaded with `prime-run` /
//! `__NV_PRIME_RENDER_OFFLOAD`). So when a machine has an NVIDIA card *and*
//! another usable GPU, umoja-mir passes Mir
//! `--driver-quirks skip:driver:nvidia` (and `nouveau`).
//!
//! Left alone when:
//! * the user already passes `--driver-quirks` or sets
//!   `MIR_SERVER_DRIVER_QUIRKS`,
//! * `UMOJA_ALL_GPUS=1` (e.g. to use monitors wired to a nouveau dGPU),
//! * the NVIDIA card is the only GPU (skipping it would leave nothing).

use std::path::Path;

/// Drivers of dGPUs we keep the compositor off when something else can do it.
const DISCRETE: &[&str] = &["nvidia", "nouveau"];

/// Drivers that are not a real GPU for the desktop: firmware framebuffers
/// that disappear once the real driver loads, and virtual/USB devices.
const NOT_A_GPU: &[&str] = &[
    "simple-framebuffer",
    "simpledrm",
    "efi-framebuffer",
    "vesa-framebuffer",
    "vgem",
    "vkms",
    "evdi",
    "udl",
];

/// The kernel driver of every `/sys/class/drm/cardN` under `sysfs_drm`.
pub fn card_drivers(sysfs_drm: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sysfs_drm) else {
        return Vec::new();
    };
    let mut cards: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            // card0, card1, ... but not the connectors (card0-eDP-1).
            let n = name.strip_prefix("card")?;
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let driver = std::fs::read_link(e.path().join("device/driver")).ok()?;
            let driver = driver.file_name()?.to_str()?.to_owned();
            Some((name, driver))
        })
        .collect();
    cards.sort();
    cards.into_iter().map(|(_, d)| d).collect()
}

/// The `--driver-quirks` values to hand Mir for these card drivers.
pub fn quirks_for(drivers: &[String]) -> Vec<String> {
    let other_gpu = drivers
        .iter()
        .any(|d| !DISCRETE.contains(&d.as_str()) && !NOT_A_GPU.contains(&d.as_str()));
    if !other_gpu {
        return Vec::new();
    }
    let mut quirks = Vec::new();
    for d in DISCRETE {
        if drivers.iter().any(|x| x == d) {
            quirks.push(format!("skip:driver:{d}"));
        }
    }
    quirks
}

/// Add the quirks to Mir's command line unless the user chose for themselves.
pub fn add_quirks(args: &mut Vec<std::ffi::OsString>) {
    let user_set = std::env::var_os("MIR_SERVER_DRIVER_QUIRKS").is_some()
        || args.iter().any(|a| {
            a.to_str()
                .is_some_and(|a| a == "--driver-quirks" || a.starts_with("--driver-quirks="))
        });
    let all_gpus = std::env::var("UMOJA_ALL_GPUS").is_ok_and(|v| !v.is_empty() && v != "0");
    let drivers = card_drivers(Path::new("/sys/class/drm"));
    tracing::info!(?drivers, "DRM cards");
    if user_set || all_gpus {
        return;
    }
    let quirks = quirks_for(&drivers);
    if quirks.is_empty() {
        return;
    }
    tracing::info!(
        ?quirks,
        "hybrid graphics: keeping the compositor on the integrated GPU (UMOJA_ALL_GPUS=1 to use every GPU)"
    );
    let at = 1.min(args.len());
    for q in quirks.into_iter().rev() {
        args.insert(at, q.into());
        args.insert(at, "--driver-quirks".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn intel_plus_nvidia_skips_nvidia() {
        assert_eq!(quirks_for(&v(&["i915", "nvidia"])), v(&["skip:driver:nvidia"]));
        assert_eq!(quirks_for(&v(&["nvidia", "xe"])), v(&["skip:driver:nvidia"]));
        assert_eq!(quirks_for(&v(&["amdgpu", "nouveau"])), v(&["skip:driver:nouveau"]));
    }

    #[test]
    fn single_gpu_untouched() {
        assert!(quirks_for(&v(&["i915"])).is_empty());
        assert!(quirks_for(&v(&["nvidia"])).is_empty());
        assert!(quirks_for(&v(&["simple-framebuffer", "nvidia"])).is_empty());
        assert!(quirks_for(&v(&[])).is_empty());
    }

    #[test]
    fn reads_sysfs() {
        let dir = std::env::temp_dir().join(format!("umoja-gpu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (card, driver) in [("card1", "i915"), ("card2", "nvidia")] {
            let dev = dir.join(card).join("device");
            std::fs::create_dir_all(&dev).unwrap();
            std::os::unix::fs::symlink(format!("../../../bus/pci/drivers/{driver}"), dev.join("driver"))
                .unwrap();
        }
        std::fs::create_dir_all(dir.join("card1-eDP-1")).unwrap();
        std::fs::create_dir_all(dir.join("renderD128")).unwrap();
        assert_eq!(card_drivers(&dir), v(&["i915", "nvidia"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inserts_after_argv0() {
        // Uses the real environment; only check the shape when it applies.
        let mut args: Vec<std::ffi::OsString> = vec!["umoja-mir".into(), "--foo".into()];
        add_quirks(&mut args);
        assert_eq!(args[0], "umoja-mir");
        assert_eq!(args.last().unwrap(), "--foo");
    }
}

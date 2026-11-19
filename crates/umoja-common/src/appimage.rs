//! Desktop entries of running AppImages that were never integrated.
//!
//! An AppImage started straight from a download installs no `.desktop`
//! file, so its windows match nothing (GitHub Copilot's Tauri window says
//! app id `github`). Its entry and icon are still there, inside the
//! squashfs the runtime mounted at `$APPDIR` while it runs. The AppImage
//! runtime exports `APPDIR` and `APPIMAGE` to the app, so the running
//! AppImages are found by reading `/proc/*/environ`.

use std::path::{Path, PathBuf};

/// Key the stand-in entry carries with the mount it came from, so a cached
/// copy can be dropped once that AppImage exits (its icon goes with it).
pub const APPDIR_KEY: &str = "X-Umoja-AppDir";

/// A stand-in desktop entry for the running AppImage whose window has
/// `app_id`: the AppImage's own entry, with `Exec` running the AppImage
/// file and `Icon` the icon file inside its mount. It has no desktop id,
/// so it can't be pinned.
pub fn entry_for_window(app_id: &str) -> Option<gio::DesktopAppInfo> {
    if app_id.is_empty() {
        return None;
    }
    running()
        .into_iter()
        .find_map(|(appdir, appimage)| entry_in(&appdir, &appimage, app_id))
}

/// Whether the mount a stand-in entry came from is still there.
pub fn still_running(info: &gio::DesktopAppInfo) -> bool {
    info.string(APPDIR_KEY)
        .is_none_or(|dir| Path::new(dir.as_str()).is_dir())
}

/// `(APPDIR, APPIMAGE)` of every running AppImage of this user, once each.
fn running() -> Vec<(PathBuf, PathBuf)> {
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return found;
    };
    for p in procs.flatten() {
        let name = p.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // Other users' processes refuse the read.
        let Ok(env) = std::fs::read(p.path().join("environ")) else {
            continue;
        };
        if let Some(pair) = appimage_env(&env)
            && !found.iter().any(|(d, _)| *d == pair.0)
        {
            found.push(pair);
        }
    }
    found
}

/// `APPDIR` and `APPIMAGE` from a NUL-separated environment block.
fn appimage_env(env: &[u8]) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;
    let (mut dir, mut image) = (None, None);
    for var in env.split(|b| *b == 0) {
        if let Some(v) = var.strip_prefix(b"APPDIR=") {
            dir = Some(PathBuf::from(std::ffi::OsStr::from_bytes(v)));
        } else if let Some(v) = var.strip_prefix(b"APPIMAGE=") {
            image = Some(PathBuf::from(std::ffi::OsStr::from_bytes(v)));
        }
    }
    let (dir, image) = (dir?, image?);
    (!dir.as_os_str().is_empty() && !image.as_os_str().is_empty()).then_some((dir, image))
}

/// The AppImage's entry (a `.desktop` at the mount's root), if it is the
/// one `app_id` names.
fn entry_in(appdir: &Path, appimage: &Path, app_id: &str) -> Option<gio::DesktopAppInfo> {
    let files = std::fs::read_dir(appdir).ok()?;
    for f in files.flatten() {
        let path = f.path();
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        let kf = glib::KeyFile::new();
        if kf.load_from_file(&path, glib::KeyFileFlags::KEEP_TRANSLATIONS).is_err() {
            continue;
        }
        if !names_window(&kf, &path, app_id) {
            continue;
        }
        return stand_in(&kf, appdir, appimage);
    }
    None
}

const GROUP: &str = "Desktop Entry";

/// Whether the entry is the window's: its `StartupWMClass`, file name,
/// program or icon name is the app id (Tauri and Electron apps use the
/// program name as their Wayland app id).
fn names_window(kf: &glib::KeyFile, path: &Path, app_id: &str) -> bool {
    let is = |s: &str| !s.is_empty() && s.eq_ignore_ascii_case(app_id);
    let get = |key: &str| kf.string(GROUP, key).map(|s| s.to_string()).unwrap_or_default();
    let program = get("Exec")
        .split_whitespace()
        .next()
        .map(|p| p.trim_matches('"').rsplit('/').next().unwrap_or("").to_string())
        .unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    is(&get("StartupWMClass")) || is(&stem) || is(&program) || is(&get("Icon"))
}

/// The entry with `Exec` and `Icon` pointing at the AppImage and its mount.
fn stand_in(kf: &glib::KeyFile, appdir: &Path, appimage: &Path) -> Option<gio::DesktopAppInfo> {
    let icon = kf.string(GROUP, "Icon").map(|s| s.to_string()).unwrap_or_default();
    match icon_file(appdir, &icon) {
        Some(file) => kf.set_string(GROUP, "Icon", &file.to_string_lossy()),
        None => kf.set_string(GROUP, "Icon", "application-x-executable"),
    }
    let args = kf
        .string(GROUP, "Exec")
        .ok()
        .and_then(|e| e.split_once(char::is_whitespace).map(|(_, a)| a.to_string()))
        .unwrap_or_default();
    let exec = format!("{} {args}", quote(&appimage.to_string_lossy()));
    kf.set_string(GROUP, "Exec", exec.trim_end());
    // Desktop actions run the mount's program, which isn't on PATH.
    let _ = kf.remove_key(GROUP, "Actions");
    let _ = kf.remove_key(GROUP, "TryExec");
    let _ = kf.remove_key(GROUP, "NoDisplay");
    kf.set_string(GROUP, APPDIR_KEY, &appdir.to_string_lossy());
    gio::DesktopAppInfo::from_keyfile(kf)
}

/// A desktop-entry `Exec` argument for `s`, quoted when it needs to be.
fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The icon file for `icon` inside the mount: a scalable or the largest
/// hicolor icon, else the one at the mount's root (`.DirIcon` last).
fn icon_file(appdir: &Path, icon: &str) -> Option<PathBuf> {
    if icon.starts_with('/') {
        return Path::new(icon).is_file().then(|| PathBuf::from(icon));
    }
    let mut best: Option<(u32, PathBuf)> = None;
    if !icon.is_empty()
        && let Ok(sizes) = std::fs::read_dir(appdir.join("usr/share/icons/hicolor"))
    {
        for size in sizes.flatten() {
            let name = size.file_name().to_string_lossy().to_string();
            let rank = if name == "scalable" {
                u32::MAX
            } else {
                // "128x128", "256x256@2"
                let (px, scale) = name.split_once('@').unwrap_or((&name, "1"));
                let px: u32 = px.split('x').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                px * scale.parse::<u32>().unwrap_or(1)
            };
            for ext in ["svg", "png"] {
                let file = size.path().join("apps").join(format!("{icon}.{ext}"));
                if file.is_file() && best.as_ref().is_none_or(|(r, _)| rank > *r) {
                    best = Some((rank, file));
                }
            }
        }
    }
    if let Some((_, file)) = best {
        return Some(file);
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if !icon.is_empty() {
        roots.extend(["svg", "png"].map(|ext| appdir.join(format!("{icon}.{ext}"))));
    }
    roots.push(appdir.join(".DirIcon"));
    roots.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::*;

    #[test]
    fn reads_appdir_and_appimage_from_environ() {
        let env = b"HOME=/home/k\0APPDIR=/tmp/.mount_GitHubmf\0APPIMAGE=/home/k/Downloads/C.AppImage\0";
        assert_eq!(
            appimage_env(env),
            Some((
                PathBuf::from("/tmp/.mount_GitHubmf"),
                PathBuf::from("/home/k/Downloads/C.AppImage")
            ))
        );
        assert_eq!(appimage_env(b"APPDIR=/tmp/x\0"), None);
    }

    #[test]
    fn quotes_exec_paths() {
        assert_eq!(quote("/home/k/C.AppImage"), "/home/k/C.AppImage");
        assert_eq!(quote("/home/k/My App$.AppImage"), "\"/home/k/My App\\$.AppImage\"");
    }

    /// GitHub Copilot's AppImage: Tauri, Wayland app id `github`, the
    /// entry `GitHub Copilot.desktop` and only 128x128 and 256x256@2 icons.
    #[test]
    fn copilot_appimage_entry() {
        let dir = std::env::temp_dir().join(format!("umoja-appimage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for size in ["128x128", "256x256@2", "16x16"] {
            std::fs::create_dir_all(dir.join("usr/share/icons/hicolor").join(size).join("apps")).unwrap();
        }
        for size in ["128x128", "256x256@2"] {
            std::fs::write(dir.join(format!("usr/share/icons/hicolor/{size}/apps/github.png")), b"png").unwrap();
        }
        std::fs::write(
            dir.join("GitHub Copilot.desktop"),
            "[Desktop Entry]\nExec=github %u\nIcon=github\nName=GitHub Copilot\n\
             Terminal=false\nType=Application\nStartupWMClass=github\n",
        )
        .unwrap();
        // gio drops an entry whose Exec program doesn't exist.
        let image = dir.join("GitHub-Copilot-linux-x64.AppImage");
        std::fs::write(&image, b"").unwrap();
        std::fs::set_permissions(&image, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let image = image.as_path();

        assert!(entry_in(&dir, image, "other").is_none());
        let info = entry_in(&dir, image, "github").expect("matches by class");
        assert_eq!(info.display_name(), "GitHub Copilot");
        assert_eq!(
            info.string("Exec").unwrap(),
            format!("{} %u", image.display())
        );
        assert_eq!(
            PathBuf::from(info.string("Icon").unwrap().as_str()),
            dir.join("usr/share/icons/hicolor/256x256@2/apps/github.png")
        );
        assert!(info.id().is_none());
        assert!(still_running(&info));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!still_running(&info));
    }
}

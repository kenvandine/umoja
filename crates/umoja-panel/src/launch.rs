//! Starting apps from the panel.
//!
//! Like gnome-shell, each app goes into its own `app-*.scope` instead of
//! staying in umoja-panel.service's cgroup, so restarting the panel never
//! takes Settings (or anything else it opened) down with it.

use gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;

pub fn spawn_app(app_id: &str, argv: &[String], activation_token: Option<&str>) {
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
    if let Some(token) = activation_token {
        launcher.setenv("XDG_ACTIVATION_TOKEN", token, true);
        launcher.setenv("DESKTOP_STARTUP_ID", token, true);
    }
    let unit = format!(
        "app-umoja-{}-{}-{}.scope",
        systemd_escape(app_id),
        std::process::id(),
        glib_counter()
    );
    let mut full: Vec<String> = vec![
        "systemd-run".into(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        "--slice=app.slice".into(),
        format!("--unit={unit}"),
        "--".into(),
    ];
    full.extend(argv.iter().cloned());
    let os: Vec<&std::ffi::OsStr> = full.iter().map(|a| a.as_ref()).collect();
    match launcher.spawn(&os) {
        Ok(_) => tracing::info!(app = app_id, unit, "launched"),
        Err(e) => {
            // No systemd --user? Run it directly instead.
            tracing::warn!(error = %e, "systemd-run failed; launching directly");
            let os: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
            if let Err(e) = launcher.spawn(&os) {
                tracing::warn!(app = app_id, error = %e, "could not launch");
            }
        }
    }
}

/// Start an app in its own scope, like the rest of the panel's launches.
pub fn launch_info(info: gio::AppInfo, arg: Option<String>) {
    let Some(cmd) = info.commandline() else { return };
    let Ok(words) = glib::shell_parse_argv(cmd.as_os_str()) else { return };
    let mut argv: Vec<String> = words
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .filter(|w| !(w.len() == 2 && w.starts_with('%')))
        .collect();
    if let Some(arg) = arg {
        argv.push(arg);
    }
    let id = info.id().map(|s| s.trim_end_matches(".desktop").to_string()).unwrap_or_default();
    // An activation token, so the compositor focuses the new window.
    let token = gtk4::gdk::Display::default()
        .and_then(|d| d.app_launch_context().startup_notify_id(Some(&info), &[]))
        .map(|t| t.to_string());
    spawn_app(&id, &argv, token.as_deref());
}

fn glib_counter() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Unit names allow [A-Za-z0-9:_.\-]; escape the rest like systemd-escape.
fn systemd_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.') {
                c.to_string()
            } else {
                format!("\\x{:02x}", c as u32)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_unit_names() {
        assert_eq!(
            super::systemd_escape("org.gnome.Settings"),
            "org.gnome.Settings"
        );
        assert_eq!(super::systemd_escape("a-b c"), "a\\x2db\\x20c");
    }
}

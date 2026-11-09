//! Give every app the shell launches its own `app-*.scope`.
//!
//! A child process inherits its parent's cgroup. Without this, an app opened
//! from the dock lives inside umoja-dock.service: systemd charges its memory
//! to the dock, and restarting the dock (KillMode=control-group) kills the
//! app too. gnome-shell avoids that by moving each spawned PID into a
//! transient scope under app.slice, and this does the same.
//!
//! Use [`launch_context`] in place of a bare launch context and launch with
//! it as usual. D-Bus activated apps don't go through it (no PID is spawned);
//! the bus already starts those in their own units.

use gio::prelude::*;

/// Hook `ctx` so every process launched through it moves into its own scope.
/// Returns the same context for convenience.
pub fn scoped<C: IsA<gio::AppLaunchContext>>(ctx: C) -> C {
    ctx.upcast_ref::<gio::AppLaunchContext>()
        .connect_launched(|_, info, platform_data| {
            let Some(pid) = launched_pid(platform_data) else {
                return;
            };
            let id = info.id().map(|s| s.to_string()).unwrap_or_default();
            move_to_scope(id.trim_end_matches(".desktop"), pid);
        });
    ctx
}

/// A plain launch context, already [`scoped`], for callers with no display.
pub fn launch_context() -> gio::AppLaunchContext {
    scoped(gio::AppLaunchContext::new())
}

/// GLib reports the child's PID as `{"pid": <i32>}` in the platform data.
fn launched_pid(platform_data: &glib::Variant) -> Option<u32> {
    let dict = glib::VariantDict::new(Some(platform_data));
    let pid: i32 = dict.lookup("pid").ok().flatten()?;
    u32::try_from(pid).ok().filter(|p| *p > 0)
}

/// Ask systemd --user to move `pid` into `app-umoja-<app>-<pid>.scope`.
/// Best effort: if there's no user manager the app just stays where it is.
pub fn move_to_scope(app_id: &str, pid: u32) {
    let unit = unit_name(app_id, pid);
    let bus = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = %e, "no session bus; app keeps the launcher's cgroup");
            return;
        }
    };
    let props = glib::Variant::array_from_iter_with_type(
        &glib::VariantTy::new("(sv)").unwrap(),
        [
            ("PIDs", vec![pid].to_variant()).to_variant(),
            ("Slice", "app.slice".to_variant()).to_variant(),
            ("CollectMode", "inactive-or-failed".to_variant()).to_variant(),
            (
                "Description",
                format!("Application launched by Umoja: {app_id}").to_variant(),
            )
                .to_variant(),
        ],
    );
    let aux = glib::Variant::array_from_iter_with_type(
        &glib::VariantTy::new("(sa(sv))").unwrap(),
        std::iter::empty::<glib::Variant>(),
    );
    let args = glib::Variant::tuple_from_iter([unit.to_variant(), "fail".to_variant(), props, aux]);
    // Synchronous on purpose: systemd answers in about a millisecond, and the
    // one-shot `umoja-mcp call` exits before an async reply would arrive.
    match bus.call_sync(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
    ) {
        Ok(_) => tracing::info!(app = app_id, pid, unit, "moved app to its own scope"),
        Err(e) => {
            tracing::warn!(app = app_id, pid, error = %e, "could not move app to its own scope")
        }
    }
}

fn unit_name(app_id: &str, pid: u32) -> String {
    let app = if app_id.is_empty() { "unknown" } else { app_id };
    format!("app-umoja-{}-{pid}.scope", systemd_escape(app))
}

/// Unit names allow [A-Za-z0-9:_.\-]; `-` is the slice separator, so escape
/// it and everything else outside that set like `systemd-escape` does.
pub fn systemd_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.') {
                c.to_string()
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("\\x{b:02x}"))
                    .collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names() {
        assert_eq!(
            unit_name("org.gnome.Settings", 42),
            "app-umoja-org.gnome.Settings-42.scope"
        );
        assert_eq!(
            unit_name("claude-desktop", 7),
            "app-umoja-claude\\x2ddesktop-7.scope"
        );
        assert_eq!(unit_name("", 7), "app-umoja-unknown-7.scope");
        assert_eq!(systemd_escape("a b"), "a\\x20b");
    }

    #[test]
    fn reads_pid_from_platform_data() {
        let d = glib::VariantDict::new(None);
        d.insert("pid", 1234i32);
        assert_eq!(launched_pid(&d.end()), Some(1234));
        assert_eq!(launched_pid(&glib::VariantDict::new(None).end()), None);
    }

    /// Needs a systemd --user session: `cargo test -p umoja-common -- --ignored`.
    #[test]
    #[ignore]
    fn launched_app_lands_in_its_own_scope() {
        let dir = std::env::temp_dir().join(format!("umoja-scope-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("cgroup");
        let desktop = dir.join("umoja-scope-test.desktop");
        std::fs::write(
            &desktop,
            format!(
                "[Desktop Entry]\nType=Application\nName=Scope test\nExec=sh -c 'sleep 0.3; cat /proc/self/cgroup > {}'\n",
                out.display()
            ),
        )
        .unwrap();
        let app = gio::DesktopAppInfo::from_filename(&desktop).unwrap();
        app.launch(&[], Some(&launch_context())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let cgroup = loop {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if !s.is_empty() {
                    break s;
                }
            }
            assert!(std::time::Instant::now() < deadline, "app never ran");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        std::fs::remove_dir_all(&dir).ok();
        assert!(cgroup.contains("/app.slice/app-umoja-"), "{cgroup}");
    }
}

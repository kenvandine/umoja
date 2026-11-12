//! "App crashed" notifications with an **Ask Why** button.
//!
//! Watches Ubuntu's crash reports: Apport's `/var/crash` (a new or
//! rewritten `.crash` file owned by this user) and, when systemd-coredump
//! is installed, its journal entries (which also catch repeat crashes,
//! which Apport only counts). A new crash shows one notification per
//! program per minute; **Ask Why** opens the overlay and asks the local
//! assistant, which reads the report (see `umoja_common::crashes`) and
//! explains it. **Don't Show for This App** adds the program to
//! `org.umoja.shell crash-notifications-muted`; `crash-notifications`
//! turns the whole thing off.
//!
//! The panel can crash too, and restarts within a moment: on start it
//! announces reports written in the last two minutes that it hasn't
//! announced yet (remembered in `$XDG_RUNTIME_DIR`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk4::glib;
use umoja_common::crashes::{self, Crash};
use umoja_common::{dbus, gsettings as gs, i18n};

use crate::notifications::Center;

const ENABLED_KEY: &str = "crash-notifications";
const MUTED_KEY: &str = "crash-notifications-muted";
/// systemd-coredump's journal message id (systemd.journal-fields(7)).
const COREDUMP_MESSAGE_ID: &str = "fc2e22bc6ee647b6b90729ab34a250b1";
/// A crash loop is announced once a minute at most.
const DEDUPE: Duration = Duration::from_secs(60);
/// Reports this old when the panel starts are left alone.
const STARTUP_WINDOW: u64 = 120;

struct Watch {
    center: Rc<Center>,
    settings: Option<gio::Settings>,
    /// Program → when it was last announced.
    last: RefCell<HashMap<String, Instant>>,
    /// "path mtime" of reports already announced (also on disk).
    seen: RefCell<HashSet<String>>,
    /// Debounce timers for report files still being written.
    pending: RefCell<HashMap<PathBuf, glib::SourceId>>,
    _monitor: RefCell<Option<gio::FileMonitor>>,
}

pub fn start(center: Rc<Center>) {
    let w = Rc::new(Watch {
        center,
        settings: gs::settings(gs::UMOJA_SHELL),
        last: RefCell::default(),
        seen: RefCell::new(load_seen()),
        pending: RefCell::default(),
        _monitor: RefCell::default(),
    });
    w.watch_apport();
    w.watch_coredumps();
    w.catch_up();
}

impl Watch {
    fn enabled(&self) -> bool {
        self.settings
            .as_ref()
            .is_none_or(|s| !gs::has_key(s, ENABLED_KEY) || s.boolean(ENABLED_KEY))
    }

    fn muted(&self, program: &str) -> bool {
        self.settings.as_ref().is_some_and(|s| {
            gs::has_key(s, MUTED_KEY) && s.strv(MUTED_KEY).iter().any(|m| m == program)
        })
    }

    fn mute(&self, program: &str) {
        let Some(s) = self.settings.as_ref().filter(|s| gs::has_key(s, MUTED_KEY)) else {
            return;
        };
        let mut list: Vec<String> = s.strv(MUTED_KEY).iter().map(|m| m.to_string()).collect();
        if !list.iter().any(|m| m == program) {
            list.push(program.to_string());
            let refs: Vec<&str> = list.iter().map(String::as_str).collect();
            let _ = s.set_strv(MUTED_KEY, refs.as_slice());
        }
    }

    fn watch_apport(self: &Rc<Self>) {
        let dir = gio::File::for_path(crashes::APPORT_DIR);
        let monitor =
            match dir.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
                Ok(m) => m,
                Err(e) => {
                    tracing::info!(error = %e, "not watching /var/crash");
                    return;
                }
            };
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, _, event| {
            use gio::FileMonitorEvent as E;
            if !matches!(event, E::Created | E::Changed | E::ChangesDoneHint) {
                return;
            }
            let Some(path) = file.path() else { return };
            if path.extension().is_none_or(|e| e != "crash") {
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.settle(path);
            }
        });
        *self._monitor.borrow_mut() = Some(monitor);
    }

    /// Apport writes the core dump into the file for a while; read the
    /// header once it has been quiet for two seconds.
    fn settle(self: &Rc<Self>, path: PathBuf) {
        if let Some(old) = self.pending.borrow_mut().remove(&path) {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let p = path.clone();
        let id = glib::timeout_add_local_once(Duration::from_secs(2), move || {
            let Some(w) = weak.upgrade() else { return };
            w.pending.borrow_mut().remove(&p);
            w.report_file(p);
        });
        self.pending.borrow_mut().insert(path, id);
    }

    fn report_file(self: &Rc<Self>, path: PathBuf) {
        let uid = crashes::current_uid();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let crash = gio::spawn_blocking(move || {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::metadata(&path).ok()?;
                if uid.is_some_and(|u| meta.uid() != u) {
                    return None;
                }
                crashes::read_apport(&path)
            })
            .await
            .ok()
            .flatten();
            if let (Some(w), Some(c)) = (weak.upgrade(), crash) {
                w.announce(c);
            }
        });
    }

    /// Follow systemd-coredump's journal entries, when it is installed.
    fn watch_coredumps(self: &Rc<Self>) {
        if !crashes::have_coredumpctl() {
            return;
        }
        let uid = crashes::current_uid();
        let mut args = vec![
            "journalctl".to_string(),
            "-f".into(),
            "-n".into(),
            "0".into(),
            "-o".into(),
            "json".into(),
            format!("MESSAGE_ID={COREDUMP_MESSAGE_ID}"),
        ];
        if let Some(u) = uid {
            args.push(format!("COREDUMP_UID={u}"));
        }
        let argv: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let proc = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "could not follow coredumps");
                return;
            }
        };
        let Some(out) = proc.stdout_pipe() else {
            return;
        };
        let lines = gio::DataInputStream::new(&out);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let _keep = proc;
            while let Ok(Some(line)) = lines.read_line_utf8_future(glib::Priority::LOW).await {
                let Some(w) = weak.upgrade() else { break };
                if let Some(c) = coredump_entry(&line) {
                    w.announce(c);
                }
            }
        });
    }

    /// Reports written just before the panel (re)started.
    fn catch_up(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let recent = gio::spawn_blocking(|| {
                let now = now();
                crashes::apport_reports(Path::new(crashes::APPORT_DIR))
                    .into_iter()
                    .filter(|c| now.saturating_sub(c.time) < STARTUP_WINDOW)
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let Some(w) = weak.upgrade() else { return };
            for c in recent {
                w.announce(c);
            }
        });
    }

    fn announce(self: &Rc<Self>, c: Crash) {
        let key = match &c.source {
            crashes::Source::Apport(p) => format!("{} {}", p.display(), c.time),
            crashes::Source::Coredump(pid) => format!("coredump {pid} {}", c.time),
        };
        if !self.seen.borrow_mut().insert(key.clone()) {
            return;
        }
        save_seen(&key);
        let program = c.program().to_string();
        if !self.enabled() || self.muted(&program) {
            return;
        }
        // Apport's coredump hook and the journal both report one crash.
        let now = Instant::now();
        if self
            .last
            .borrow()
            .get(&program)
            .is_some_and(|t| now.duration_since(*t) < DEDUPE)
        {
            return;
        }
        self.last.borrow_mut().insert(program.clone(), now);
        tracing::info!(program, signal = ?c.signal, "announcing crash");

        let (name, icon) = display_name(&c);
        let summary = i18n::umoja("%s quit unexpectedly").replace("%s", &name);
        let body = match c.signal_text() {
            Some(s) => {
                i18n::umoja("It stopped with a %s. The assistant can look at what happened.")
                    .replace("%s", &s)
            }
            None => i18n::umoja("The assistant can look at what happened."),
        };
        let ask = i18n::umoja("Ask Why");
        let mute = i18n::umoja("Don't Show for This App");
        let weak = Rc::downgrade(self);
        let time = crashes::when(c.time);
        self.center.notify_with_actions(
            &summary,
            &body,
            &icon,
            &[("default", ""), ("ask", &ask), ("mute", &mute)],
            move |action| {
                let Some(w) = weak.upgrade() else { return };
                match action {
                    "mute" => w.mute(&program),
                    _ => ask_assistant(&format!("Why did {program} crash {time}?")),
                }
            },
        );
    }
}

/// A friendly name and icon: the app's own for apps, plain words for the
/// shell's parts.
fn display_name(c: &Crash) -> (String, String) {
    let program = c.program();
    let shell = match program {
        "umoja-panel" => Some(i18n::umoja("The top bar")),
        "umoja-dock" => Some(i18n::umoja("The dock")),
        "umoja-overlay" => Some(i18n::umoja("The app overview")),
        "umoja-mir" => Some(i18n::umoja("The desktop")),
        "umoja-files" => Some(i18n::umoja("File search")),
        _ => None,
    };
    if let Some(name) = shell {
        return (name, "computer-fail-symbolic".into());
    }
    let app = gio::AppInfo::all().into_iter().find(|a| {
        umoja_common::apps::executable(a)
            .as_deref()
            .and_then(|e| e.file_name())
            .is_some_and(|n| n.to_string_lossy() == program)
            || a.id().is_some_and(|id| {
                id.trim_end_matches(".desktop")
                    .rsplit('.')
                    .next()
                    .is_some_and(|last| last.eq_ignore_ascii_case(program))
            })
    });
    match app {
        Some(a) => {
            let icon = a
                .icon()
                .and_then(|i| i.to_string())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "application-x-executable".into());
            (a.display_name().to_string(), icon)
        }
        None => (program.to_string(), "application-x-executable".into()),
    }
}

/// Open the overlay and ask the assistant.
fn ask_assistant(question: &str) {
    let question = question.to_string();
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            "Ask",
            Some(&(question.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
            |r| {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "could not ask the assistant");
                }
            },
        );
    });
}

/// One `journalctl -o json` line from systemd-coredump.
fn coredump_entry(line: &str) -> Option<Crash> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str());
    let time = field("COREDUMP_TIMESTAMP")
        .or_else(|| field("__REALTIME_TIMESTAMP"))
        .and_then(|t| t.parse::<u64>().ok())
        .map(|us| us / 1_000_000)
        .unwrap_or_else(now);
    Some(Crash {
        executable: field("COREDUMP_EXE")?.to_string(),
        time,
        signal: field("COREDUMP_SIGNAL").and_then(|s| s.parse().ok()),
        package: None,
        cmdline: field("COREDUMP_CMDLINE").map(str::to_string),
        stack: None,
        count: 1,
        source: crashes::Source::Coredump(field("COREDUMP_PID")?.parse().ok()?),
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn seen_file() -> PathBuf {
    glib::user_runtime_dir().join("umoja-crashes-seen")
}

fn load_seen() -> HashSet<String> {
    std::fs::read_to_string(seen_file())
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_seen(key: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(seen_file())
    {
        let _ = writeln!(f, "{key}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_coredump_journal_entries() {
        let line = r#"{"MESSAGE_ID":"fc2e22bc6ee647b6b90729ab34a250b1","COREDUMP_EXE":"/usr/bin/ptyxis","COREDUMP_PID":"4242","COREDUMP_SIGNAL":"11","COREDUMP_TIMESTAMP":"1762696931000000","COREDUMP_CMDLINE":"ptyxis --new-window"}"#;
        let c = coredump_entry(line).unwrap();
        assert_eq!(c.program(), "ptyxis");
        assert_eq!(c.signal, Some(11));
        assert_eq!(c.time, 1762696931);
        assert_eq!(c.source, crashes::Source::Coredump(4242));
        assert!(coredump_entry("{}").is_none());
    }
}

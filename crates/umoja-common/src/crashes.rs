//! Crashed programs on this computer, read from the crash reports Ubuntu
//! keeps: Apport's `/var/crash/*.crash` files and, when `systemd-coredump`
//! is installed, `coredumpctl`.
//!
//! Like the rest of [`crate::diagnostics`] this only reads. A `.crash`
//! file can be hundreds of megabytes because it carries the core dump, so
//! only its text header is read: Apport writes the binary fields
//! (`CoreDump: base64`) last, and reading stops there.
//!
//! The panel uses [`Crash`] and [`read_apport`] to announce a new crash;
//! the assistant uses [`report`] and [`details`] to explain one.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cap on a crash report, the same as the other diagnostics sections.
const MAX_TEXT: usize = 2400;

/// Where Apport writes its reports.
pub const APPORT_DIR: &str = "/var/crash";

/// Fields kept from a report header; everything else (memory maps,
/// environment, the core dump) is skipped.
const KEEP: &[&str] = &[
    "ProblemType",
    "Date",
    "ExecutablePath",
    "Package",
    "ProcCmdline",
    "Signal",
    "SignalName",
    "StacktraceTop",
    "Title",
    "CrashCounter",
    "UnreportableReason",
    "_MarkForUpload",
];

/// Where a crash was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// An Apport report file.
    Apport(PathBuf),
    /// A systemd-coredump entry, by the crashed process id.
    Coredump(u32),
}

/// One crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crash {
    /// Full path of the program that crashed.
    pub executable: String,
    /// Unix time in seconds.
    pub time: u64,
    pub signal: Option<i32>,
    /// "name version" of the package it belongs to, when known.
    pub package: Option<String>,
    pub cmdline: Option<String>,
    /// The top stack frames, when the report has them.
    pub stack: Option<String>,
    /// Times Apport counted this program crashing (repeats of an unseen
    /// report bump it instead of writing a new file).
    pub count: u32,
    pub source: Source,
}

impl Crash {
    /// The program's file name, e.g. `nautilus`.
    pub fn program(&self) -> &str {
        self.executable
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.executable)
    }

    /// "segmentation fault (SIGSEGV)" and friends.
    pub fn signal_text(&self) -> Option<String> {
        self.signal.map(signal_text)
    }

    /// A one-line description for lists.
    pub fn line(&self) -> String {
        let mut s = format!("{} crashed {}", self.program(), when(self.time));
        if let Some(sig) = self.signal_text() {
            s.push_str(&format!(", {sig}"));
        }
        if self.count > 1 {
            s.push_str(&format!(", {} times so far", self.count));
        }
        s
    }
}

/// What a signal number means, in plain words.
pub fn signal_text(sig: i32) -> String {
    let (name, what) = match sig {
        4 => ("SIGILL", "illegal instruction"),
        5 => ("SIGTRAP", "trap or failed assertion"),
        6 => ("SIGABRT", "aborted itself"),
        7 => ("SIGBUS", "bus error"),
        8 => ("SIGFPE", "arithmetic error"),
        11 => ("SIGSEGV", "segmentation fault"),
        24 => ("SIGXCPU", "CPU time limit"),
        25 => ("SIGXFSZ", "file size limit"),
        31 => ("SIGSYS", "bad system call"),
        _ => return format!("signal {sig}"),
    };
    format!("{what} ({name})")
}

/// The header fields of an Apport report, in file order. Stops at the
/// first binary field. Continuation lines (starting with a space) join
/// their key's value with newlines.
pub fn parse_apport(reader: impl BufRead) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut keep_current = false;
    for line in reader.split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(' ') {
            if keep_current && let Some((_, v)) = out.last_mut() {
                if !v.is_empty() {
                    v.push('\n');
                }
                // Cap long values (a stack trace is about five lines).
                if v.len() < 4000 {
                    v.push_str(rest);
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            keep_current = false;
            continue;
        };
        let value = value.trim_start();
        if value == "base64" {
            break;
        }
        keep_current = KEEP.contains(&key);
        if keep_current {
            out.push((key.to_string(), value.to_string()));
        }
    }
    out
}

/// Read the header of one Apport report into a [`Crash`]. `None` for a
/// file that isn't a crash report (Apport also writes hangs and package
/// failures) or can't be read.
pub fn read_apport(path: &Path) -> Option<Crash> {
    let file = std::fs::File::open(path).ok()?;
    let mtime = file
        .metadata()
        .ok()?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let fields = parse_apport(BufReader::with_capacity(64 * 1024, file));
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    if get("ProblemType").is_some_and(|t| t != "Crash") {
        return None;
    }
    let executable = get("ExecutablePath").or_else(|| {
        // _usr_bin_foo.1000.crash
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".crash")?;
        let exe = stem.rsplit_once('.').map(|(e, _)| e).unwrap_or(stem);
        Some(exe.replace('_', "/"))
    })?;
    Some(Crash {
        executable,
        time: mtime,
        signal: get("Signal").and_then(|s| s.trim().parse().ok()),
        package: get("Package"),
        cmdline: get("ProcCmdline"),
        stack: get("StacktraceTop").filter(|s| !s.trim().is_empty()),
        count: get("CrashCounter")
            .and_then(|c| c.trim().parse().ok())
            .unwrap_or(1),
        source: Source::Apport(path.to_path_buf()),
    })
}

/// This user's crash reports in `dir`, newest first.
pub fn apport_reports(dir: &Path) -> Vec<Crash> {
    let uid = current_uid();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<Crash> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "crash"))
        .filter(|p| uid.is_none_or(|u| owner(p) == Some(u)))
        .filter_map(|p| read_apport(&p))
        .collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.time));
    v
}

/// Whether `coredumpctl` (systemd-coredump) is installed.
pub fn have_coredumpctl() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("coredumpctl").is_file()))
}

/// systemd-coredump's record of this user's crashes in the last `days`.
pub fn coredumps(days: u32) -> Vec<Crash> {
    if !have_coredumpctl() {
        return Vec::new();
    }
    let since = format!("--since=-{days}d");
    let Some(out) = crate::diagnostics::run_cmd(
        "coredumpctl",
        &["list", "--json=short", "--no-pager", &since],
    ) else {
        return Vec::new();
    };
    parse_coredumps(&out, current_uid())
}

/// `coredumpctl list --json=short` output to crashes, newest first.
pub fn parse_coredumps(json: &str, uid: Option<u32>) -> Vec<Crash> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut v: Vec<Crash> = items
        .iter()
        .filter(|i| uid.is_none_or(|u| i["uid"].as_u64() == Some(u as u64)))
        .filter_map(|i| {
            Some(Crash {
                executable: i["exe"].as_str()?.to_string(),
                time: i["time"].as_u64()? / 1_000_000,
                signal: i["sig"].as_i64().map(|s| s as i32),
                package: None,
                cmdline: None,
                stack: None,
                count: 1,
                source: Source::Coredump(i["pid"].as_u64()? as u32),
            })
        })
        .collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.time));
    v
}

/// Crashes from both sources in the last `days`, newest first. A crash
/// recorded by both (Apport's systemd-coredump hook) is listed once.
pub fn recent(days: u32) -> Vec<Crash> {
    let cutoff = now().saturating_sub(days as u64 * 86_400);
    let mut all: Vec<Crash> = apport_reports(Path::new(APPORT_DIR))
        .into_iter()
        .filter(|c| c.time >= cutoff)
        .collect();
    for c in coredumps(days) {
        let dup = all
            .iter()
            .any(|a| a.executable == c.executable && a.time.abs_diff(c.time) < 120);
        if !dup {
            all.push(c);
        }
    }
    all.sort_by_key(|c| std::cmp::Reverse(c.time));
    all
}

/// The crash list for the troubleshooting report.
pub fn report() -> String {
    report_about("")
}

/// [`report`], with the crash `question` names in detail ("why did
/// nautilus crash?"), else the newest one.
pub fn report_about(question: &str) -> String {
    let crashes = recent(7);
    if crashes.is_empty() {
        let mut s = String::from("No programs crashed in the last 7 days.\n");
        if !have_coredumpctl() {
            s.push_str("(Read from Apport's reports in /var/crash.)\n");
        }
        return s;
    }
    let mut out = String::from("Crashes in the last 7 days, newest first:\n");
    for c in crashes.iter().take(8) {
        out.push_str(&format!("- {}\n", c.line()));
    }
    for group in same_minute(&crashes) {
        out.push_str(&format!(
            "Note: {} crashed within the same minute ({}), which points to a shared cause such as the display or session going away.\n",
            group.join(", "),
            when(crashes.iter().find(|c| c.program() == group[0]).map(|c| c.time).unwrap_or_default()),
        ));
    }
    // One crash in detail, so a "why did it crash" question can be
    // answered without another tool call.
    let q = question.to_lowercase();
    let named = crashes.iter().find(|c| {
        let p = c.program().to_lowercase();
        p.len() > 2 && q.contains(&p)
    });
    out.push('\n');
    out.push_str(&details_of(named.unwrap_or(&crashes[0])));
    crate::diagnostics::truncate(&out, MAX_TEXT)
}

/// Programs that crashed within a minute of each other (2 or more).
fn same_minute(crashes: &[Crash]) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut i = 0;
    while i < crashes.len() {
        let mut j = i + 1;
        while j < crashes.len() && crashes[i].time.abs_diff(crashes[j].time) <= 60 {
            j += 1;
        }
        if j - i >= 2 {
            let mut names: Vec<String> = crashes[i..j]
                .iter()
                .map(|c| c.program().to_string())
                .collect();
            names.dedup();
            if names.len() >= 2 {
                groups.push(names);
            }
        }
        i = j;
    }
    groups
}

/// Details of the newest crash of `program` (a name like "nautilus" or a
/// path), or of the newest crash at all when `program` is empty.
pub fn details(program: &str) -> String {
    let crashes = recent(30);
    let want = program.trim().to_lowercase();
    let pick = crashes.iter().find(|c| {
        want.is_empty()
            || c.program().to_lowercase() == want
            || c.executable.to_lowercase() == want
            || c.program().to_lowercase().contains(&want)
    });
    match pick {
        Some(c) => details_of(c),
        None if crashes.is_empty() => "No crash reports in the last 30 days.".into(),
        None => format!(
            "No crash of \"{program}\" in the last 30 days. Recent crashes: {}.",
            crashes
                .iter()
                .take(6)
                .map(|c| c.program().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Everything known about one crash: the report's own facts, then what
/// happened around it (memory pressure, the journal, package updates).
pub fn details_of(c: &Crash) -> String {
    let mut out = format!("Crash of {} ({})\n", c.program(), c.executable);
    out.push_str(&format!("When: {}\n", when(c.time)));
    if let Some(s) = c.signal_text() {
        out.push_str(&format!("Signal: {s}\n"));
    }
    if c.count > 1 {
        out.push_str(&format!(
            "It has crashed {} times since the report was written.\n",
            c.count
        ));
    }
    if let Some(p) = &c.package {
        out.push_str(&format!("Package: {p}\n"));
    } else {
        out.push_str("Package: unknown (may not come from an Ubuntu package)\n");
    }
    if let Some(cmd) = &c.cmdline {
        out.push_str(&format!("Command line: {}\n", clip(cmd, 300)));
    }
    let stack = c.stack.clone().or_else(|| match &c.source {
        Source::Coredump(pid) => coredump_stack(*pid),
        Source::Apport(_) => None,
    });
    match stack {
        Some(s) => out.push_str(&format!("Top of the stack:\n{}\n", indent(&s, 12))),
        None => out.push_str(
            "No stack trace in the report (Apport keeps the core dump unprocessed; \
             installing systemd-coredump records one).\n",
        ),
    }
    match &c.source {
        Source::Apport(p) => out.push_str(&format!("Report: {}\n", p.display())),
        Source::Coredump(pid) => out.push_str(&format!("Core dump: coredumpctl info {pid}\n")),
    }
    let around = around(c.time);
    if !around.trim().is_empty() {
        out.push_str(&around);
    }
    if let Some(u) = updates_before(c.time) {
        out.push_str(&u);
    }
    crate::diagnostics::truncate(&out, MAX_TEXT)
}

/// The stack of the crashing thread from `coredumpctl info`.
fn coredump_stack(pid: u32) -> Option<String> {
    let out =
        crate::diagnostics::run_cmd("coredumpctl", &["info", "--no-pager", &pid.to_string()])?;
    let mut lines = out
        .lines()
        .skip_while(|l| !l.contains("Stack trace of thread"));
    lines.next()?;
    let frames: Vec<&str> = lines
        .take_while(|l| !l.trim().is_empty())
        .take(12)
        .map(str::trim)
        .collect();
    (!frames.is_empty()).then(|| frames.join("\n"))
}

/// Warnings from the minute around the crash, and any sign the system
/// was out of memory.
fn around(time: u64) -> String {
    let since = format!("@{}", time.saturating_sub(60));
    let until = format!("@{}", time + 15);
    let mut out = String::new();
    if let Some(log) = crate::diagnostics::run_cmd(
        "journalctl",
        &[
            "--no-pager",
            "-q",
            "-o",
            "short",
            "-p",
            "warning",
            "--since",
            &since,
            "--until",
            &until,
        ],
    ) {
        let oom = log.lines().any(|l| {
            let l = l.to_lowercase();
            l.contains("out of memory") || l.contains("oom-kill") || l.contains("oom_reaper")
        });
        out.push_str(if oom {
            "Memory: the system was OUT OF MEMORY at the time (see the log below).\n"
        } else {
            "Memory: no out-of-memory messages around the crash.\n"
        });
        let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
        if !lines.is_empty() {
            let skip = lines.len().saturating_sub(10);
            out.push_str("Log warnings around the crash:\n");
            for l in &lines[skip..] {
                out.push_str(&format!("  {}\n", clip(l, 200)));
            }
        }
    }
    out
}

/// Packages upgraded in the three days before the crash (dpkg.log).
fn updates_before(time: u64) -> Option<String> {
    let log = std::fs::read_to_string("/var/log/dpkg.log").ok()?;
    let from = date_string(time.saturating_sub(3 * 86_400))?;
    let to = date_string(time + 60)?;
    let mut pkgs: Vec<String> = log
        .lines()
        .filter(|l| l.len() > 19 && &l[..19] >= from.as_str() && &l[..19] <= to.as_str())
        .filter_map(|l| {
            let mut f = l.split_whitespace().skip(2);
            (f.next()? == "upgrade").then(|| {
                let pkg = f.next()?.split(':').next()?.to_string();
                let _old = f.next()?;
                let new = f.next()?;
                Some(format!("{pkg} {new}"))
            })?
        })
        .collect();
    // Reinstalling the same version logs it again.
    let mut seen = std::collections::HashSet::new();
    pkgs.retain(|p| seen.insert(p.clone()));
    if pkgs.is_empty() {
        return Some("Package updates in the 3 days before: none\n".into());
    }
    let n = pkgs.len();
    pkgs.truncate(12);
    Some(format!(
        "Package updates in the 3 days before ({n}): {}{}\n",
        pkgs.join(", "),
        if n > 12 { ", …" } else { "" }
    ))
}

/// "2026-11-09 14:02:11" in local time, as dpkg.log writes it.
fn date_string(time: u64) -> Option<String> {
    glib::DateTime::from_unix_local(time as i64)
        .ok()?
        .format("%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|s| s.to_string())
}

/// "today at 14:02", "yesterday at 09:10" or "Mon 2 Nov at 18:00".
pub fn when(time: u64) -> String {
    let Ok(t) = glib::DateTime::from_unix_local(time as i64) else {
        return format!("at {time}");
    };
    let clock = t.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
    let today = glib::DateTime::now_local().ok();
    let days = today
        .as_ref()
        .map(|n| (n.ymd(), t.ymd()))
        .map(|(a, b)| {
            let d = |(y, m, d): (i32, i32, i32)| {
                glib::DateTime::from_local(y, m, d, 0, 0, 0.0)
                    .map(|x| x.to_unix())
                    .unwrap_or_default()
            };
            (d(a) - d(b)) / 86_400
        })
        .unwrap_or(99);
    match days {
        0 => format!("today at {clock}"),
        1 => format!("yesterday at {clock}"),
        _ => format!(
            "{} at {clock}",
            t.format("%a %e %b")
                .map(|s| s.to_string().replace("  ", " "))
                .unwrap_or_default()
        ),
    }
}

/// One line cut to `max` bytes with an ellipsis.
fn clip(line: &str, max: usize) -> String {
    let line = line.trim_end();
    if line.len() <= max {
        return line.to_string();
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

fn indent(text: &str, max_lines: usize) -> String {
    text.lines()
        .take(max_lines)
        .map(|l| format!("  {}", l.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn owner(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

/// This process's real user id, from /proc (no libc needed).
pub fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = "ProblemType: Crash\n\
Architecture: amd64\n\
CrashCounter: 2\n\
Date: Mon Nov  9 14:02:11 2026\n\
ExecutablePath: /usr/bin/umoja-panel\n\
Package: umoja-desktop 2026.46.3\n\
ProcCmdline: /usr/bin/umoja-panel --flag\n\
ProcEnviron:\n LANG=en_US.UTF-8\n SHELL=/bin/bash\n\
Signal: 11\n\
StacktraceTop:\n gtk_widget_show () from libgtk\n main ()\n\
CoreDump: base64\n H4sICAAAAAAC/0NvcmVEdW1wAA==\n\
Title: never read\n";

    #[test]
    fn parses_header_and_stops_at_core_dump() {
        let f = parse_apport(REPORT.as_bytes());
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ProblemType",
                "CrashCounter",
                "Date",
                "ExecutablePath",
                "Package",
                "ProcCmdline",
                "Signal",
                "StacktraceTop"
            ]
        );
        let stack = &f.iter().find(|(k, _)| k == "StacktraceTop").unwrap().1;
        assert_eq!(stack, "gtk_widget_show () from libgtk\nmain ()");
    }

    #[test]
    fn reads_a_report_file() {
        let dir = std::env::temp_dir().join(format!("umoja-crash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("_usr_bin_umoja-panel.1000.crash");
        std::fs::write(&path, REPORT).unwrap();
        let c = read_apport(&path).unwrap();
        assert_eq!(c.program(), "umoja-panel");
        assert_eq!(c.signal, Some(11));
        assert_eq!(c.count, 2);
        assert_eq!(c.package.as_deref(), Some("umoja-desktop 2026.46.3"));
        assert!(c.signal_text().unwrap().contains("SIGSEGV"));
        let list = apport_reports(&dir);
        assert_eq!(list.len(), 1);
        // Not a crash: a hang report.
        std::fs::write(
            dir.join("_usr_bin_x.1000.crash"),
            "ProblemType: Hang\nExecutablePath: /usr/bin/x\n",
        )
        .unwrap();
        assert_eq!(apport_reports(&dir).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn executable_from_file_name() {
        let dir = std::env::temp_dir().join(format!("umoja-crash-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("_usr_bin_nautilus.1000.crash");
        std::fs::write(&path, "ProblemType: Crash\nSignal: 6\n").unwrap();
        let c = read_apport(&path).unwrap();
        assert_eq!(c.executable, "/usr/bin/nautilus");
        assert!(c.line().contains("aborted itself"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn groups_crashes_in_one_minute() {
        let c = |exe: &str, time: u64| Crash {
            executable: exe.into(),
            time,
            signal: Some(8),
            package: None,
            cmdline: None,
            stack: None,
            count: 1,
            source: Source::Coredump(1),
        };
        let list = [
            c("/a/mir", 5000),
            c("/a/panel", 1030),
            c("/a/files", 1010),
            c("/a/term", 1000),
        ];
        assert_eq!(same_minute(&list), vec![vec!["panel", "files", "term"]]);
        assert_eq!(clip("abcdef", 3), "abc…");
    }

    #[test]
    fn coredumpctl_json() {
        let json = r#"[{"time":1762696931000000,"pid":4242,"uid":1000,"gid":1000,"sig":11,"corefile":"present","exe":"/usr/bin/ptyxis","size":123},
                       {"time":1762696000000000,"pid":7,"uid":0,"gid":0,"sig":6,"corefile":"none","exe":"/usr/sbin/x","size":0}]"#;
        let v = parse_coredumps(json, Some(1000));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].program(), "ptyxis");
        assert_eq!(v[0].source, Source::Coredump(4242));
        assert_eq!(v[0].time, 1762696931);
        assert_eq!(parse_coredumps(json, None).len(), 2);
    }
}

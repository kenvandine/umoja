//! Activity recall: an opt-in, text-only timeline of what the user did on
//! this computer (which window was in front, files opened, notifications
//! received), so "what was that page I had open yesterday afternoon?" has
//! an answer. No screenshots, no keystrokes, nothing leaves the computer:
//! one JSON line per entry in `~/.local/share/umoja/recall/<YYYY-MM-DD>.jsonl`,
//! days older than `activity-recall-days` are deleted.
//!
//! Off by default (`org.umoja.shell activity-recall`). Writers call
//! [`enabled`] each time, so turning it off stops recording at once.

use crate::gsettings as gs;
use gio::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A window that stayed in front for a while.
    Window,
    /// A file opened (from the recently-used list).
    File,
    Notification,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds.
    pub at: i64,
    pub kind: Kind,
    /// App name or desktop id ("Firefox", "org.gnome.Nautilus").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// Window title, file path, or notification summary and body.
    pub text: String,
    /// Seconds a window stayed in front.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<i64>,
}

pub const DEFAULT_DAYS: i64 = 14;

pub fn enabled() -> bool {
    gs::settings(gs::UMOJA_SHELL)
        .filter(|s| gs::has_key(s, "activity-recall"))
        .is_some_and(|s| s.boolean("activity-recall"))
}

pub fn keep_days() -> i64 {
    gs::settings(gs::UMOJA_SHELL)
        .filter(|s| gs::has_key(s, "activity-recall-days"))
        .map(|s| s.uint("activity-recall-days") as i64)
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_DAYS)
}

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("UMOJA_RECALL_DIR") {
        return PathBuf::from(d);
    }
    glib::user_data_dir().join("umoja").join("recall")
}

fn day_name(at: i64) -> Option<String> {
    glib::DateTime::from_unix_local(at).ok()?.format("%F").ok().map(|s| s.to_string())
}

/// Window titles and notification text can hold anything; keep entries
/// one short line.
pub fn tidy(text: &str) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 300 { t.chars().take(300).collect::<String>() + "…" } else { t }
}

/// Append `e` (when recall is on).
pub fn add(e: Entry) {
    if !enabled() {
        return;
    }
    add_to(&dir(), e);
}

pub fn add_to(dir: &std::path::Path, mut e: Entry) {
    e.text = tidy(&e.text);
    if e.text.is_empty() {
        return;
    }
    let Some(day) = day_name(e.at) else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Private: only the user can read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{day}.jsonl"));
    let Ok(line) = serde_json::to_string(&e) else { return };
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Delete day files older than `days`.
pub fn prune(dir: &std::path::Path, days: i64, now: i64) {
    let Some(oldest) = day_name(now - days * 86400) else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for f in rd.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        if let Some(day) = name.strip_suffix(".jsonl") {
            if day < oldest.as_str() {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

/// Delete everything, or the entries in `[start, end)`.
pub fn forget(dir: &std::path::Path, range: Option<(i64, i64)>) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for f in rd.flatten() {
        let path = f.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some((start, end)) = range else {
            n += read_file(&path).len();
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let all = read_file(&path);
        let keep: Vec<&Entry> = all.iter().filter(|e| e.at < start || e.at >= end).collect();
        if keep.len() == all.len() {
            continue;
        }
        n += all.len() - keep.len();
        if keep.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let body: String = keep.iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
            let _ = std::fs::write(&path, body);
        }
    }
    n
}

fn read_file(path: &std::path::Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Entries in `[start, end)` whose app or text holds every word of
/// `query` (any order, ignoring case), oldest first.
pub fn search(dir: &std::path::Path, start: i64, end: i64, query: &str, kind: Option<Kind>) -> Vec<Entry> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut days = Vec::new();
    let mut t = start;
    while t < end + 86400 {
        if let Some(d) = day_name(t) {
            if days.last() != Some(&d) {
                days.push(d);
            }
        }
        t += 86400;
    }
    let mut out = Vec::new();
    for d in days {
        for e in read_file(&dir.join(format!("{d}.jsonl"))) {
            if e.at < start || e.at >= end || kind.is_some_and(|k| k != e.kind) {
                continue;
            }
            let hay = format!("{} {}", e.app, e.text).to_lowercase();
            if words.iter().all(|w| hay.contains(w)) {
                out.push(e);
            }
        }
    }
    out.sort_by_key(|e| e.at);
    out.dedup_by(|a, b| a.kind == b.kind && a.text == b.text && a.app == b.app && a.at - b.at < 60);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("umoja-recall-test-{}-{}", std::process::id(), rand_suffix()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    fn e(at: i64, kind: Kind, app: &str, text: &str) -> Entry {
        Entry { at, kind, app: app.into(), text: text.into(), secs: None }
    }

    #[test]
    fn add_search_forget() {
        let d = tmp();
        let now = glib::DateTime::now_local().unwrap().to_unix();
        add_to(&d, e(now - 7200, Kind::Window, "Firefox", "Rust   async book — Mozilla Firefox"));
        add_to(&d, e(now - 3600, Kind::File, "", "/home/k/Documents/budget 2026.ods"));
        add_to(&d, e(now - 60, Kind::Notification, "Slack", "Dana: lunch?"));
        add_to(&d, e(now - 30, Kind::Window, "Firefox", "   "));
        let all = search(&d, now - 86400, now + 1, "", None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text, "Rust async book — Mozilla Firefox");
        assert_eq!(search(&d, now - 86400, now + 1, "BUDGET ods", None).len(), 1);
        assert_eq!(search(&d, now - 86400, now + 1, "firefox", Some(Kind::Window)).len(), 1);
        assert_eq!(search(&d, now - 1800, now + 1, "", None).len(), 1);
        assert_eq!(forget(&d, Some((now - 4000, now - 3000))), 1);
        assert_eq!(search(&d, now - 86400, now + 1, "", None).len(), 2);
        assert_eq!(forget(&d, None), 2);
        assert!(search(&d, now - 86400, now + 1, "", None).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prunes_old_days() {
        let d = tmp();
        let now = glib::DateTime::now_local().unwrap().to_unix();
        add_to(&d, e(now - 20 * 86400, Kind::Window, "a", "old"));
        add_to(&d, e(now, Kind::Window, "a", "new"));
        prune(&d, 14, now);
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn long_text_is_cut() {
        assert_eq!(tidy(&"x".repeat(400)).chars().count(), 301);
    }
}

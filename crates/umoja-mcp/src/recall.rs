//! Activity recall (see `umoja_common::recall`): the recorder the
//! automations service runs, and the assistant's tools to search and
//! forget the timeline. The panel adds notifications itself.

use crate::tool::{Outcome, Tool, arg_str, arg_u64};
use gio::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::dbus;
use umoja_common::recall::{self, Entry, Kind};

/// A window must stay in front this long to count.
const MIN_SECS: i64 = 5;
/// A window in front for longer is written in pieces this long, so the
/// timeline isn't missing what's in front right now.
const PIECE_SECS: i64 = 300;

fn now() -> i64 {
    glib::DateTime::now_local().map(|d| d.to_unix()).unwrap_or(0)
}

fn app_name(app_id: &str) -> String {
    if app_id.is_empty() {
        return String::new();
    }
    gio::DesktopAppInfo::new(&format!("{app_id}.desktop"))
        .map(|a| a.name().to_string())
        .unwrap_or_else(|| app_id.to_string())
}

struct Front {
    app: String,
    title: String,
    since: i64,
}

fn write_front(f: &Front, until: i64) {
    let secs = until - f.since;
    if secs >= MIN_SECS && !f.title.trim().is_empty() {
        recall::add(Entry { at: f.since, kind: Kind::Window, app: app_name(&f.app), text: f.title.clone(), secs: Some(secs) });
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let i = tag.find(&key)? + key.len();
    let rest = &tag[i..];
    Some(&rest[..rest.find('"')?])
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// (time, uri) of each bookmark in a recently-used.xbel, by the later of
/// its modified and visited times.
pub fn parse_xbel(xml: &str) -> Vec<(i64, String)> {
    xml.split("<bookmark ")
        .skip(1)
        .filter_map(|b| {
            let tag = &b[..b.find('>')?];
            let tag = format!(" {tag}");
            let uri = unescape(attr(&tag, "href")?);
            let t = ["modified", "visited", "added"]
                .iter()
                .filter_map(|k| attr(&tag, k))
                .filter_map(|v| glib::DateTime::from_iso8601(v, None).ok())
                .map(|d| d.to_unix())
                .max()?;
            Some((t, uri))
        })
        .collect()
}

/// Files from the recently-used list changed after `after`.
fn recent_files(after: i64) -> (Vec<(i64, String)>, i64) {
    let path = glib::user_data_dir().join("recently-used.xbel");
    let Ok(xml) = std::fs::read_to_string(&path) else { return (Vec::new(), after) };
    let mut newest = after;
    let mut out = Vec::new();
    for (t, uri) in parse_xbel(&xml) {
        if t <= after {
            continue;
        }
        newest = newest.max(t);
        if let Some(p) = gio::File::for_uri(&uri).path() {
            out.push((t, p.display().to_string()));
        }
    }
    out.sort();
    (out, newest)
}

/// Record the front window and opened files while recall is on.
pub async fn record(session: gio::DBusConnection) {
    recall::prune(&recall::dir(), recall::keep_days(), now());
    let front: Rc<RefCell<Option<Front>>> = Rc::default();
    let busy = Rc::new(Cell::new(false));
    let check = {
        let (front, busy, session) = (front.clone(), busy.clone(), session.clone());
        move || {
            if busy.replace(true) {
                return;
            }
            let (front, busy, _session) = (front.clone(), busy.clone(), session.clone());
            glib::spawn_future_local(async move {
                let on = recall::enabled();
                let wins = if on { crate::desktop::windows().await.unwrap_or_default() } else { Vec::new() };
                let focused = wins.iter().find(|w| w.flags & crate::desktop::FOCUSED != 0);
                let t = now();
                let mut f = front.borrow_mut();
                let same = match (f.as_ref(), focused) {
                    (Some(a), Some(b)) => a.app == b.app_id && a.title == b.title,
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    if let Some(old) = f.take() {
                        write_front(&old, t);
                    }
                    *f = focused.map(|w| Front { app: w.app_id.clone(), title: w.title.clone(), since: t });
                }
                busy.set(false);
            });
        }
    };
    let check = Rc::new(check);
    {
        let check = check.clone();
        let id = session.signal_subscribe(
            Some(dbus::COMPOSITOR_BUS_NAME),
            Some(dbus::COMPOSITOR_INTERFACE),
            Some("WindowsChanged"),
            Some(dbus::COMPOSITOR_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| check(),
        );
        std::mem::forget(id);
    }
    // Long stretches, and recall being turned off.
    {
        let (front, check) = (front.clone(), check.clone());
        glib::timeout_add_seconds_local(30, move || {
            let t = now();
            if !recall::enabled() {
                front.borrow_mut().take();
            } else if let Some(f) = front.borrow_mut().as_mut() {
                if t - f.since >= PIECE_SECS {
                    write_front(f, t);
                    f.since = t;
                }
            }
            check();
            glib::ControlFlow::Continue
        });
    }
    // Once a day, drop old days.
    glib::timeout_add_seconds_local(3600, || {
        recall::prune(&recall::dir(), recall::keep_days(), now());
        glib::ControlFlow::Continue
    });
    // Files.
    let seen = Rc::new(Cell::new(now()));
    let file = gio::File::for_path(glib::user_data_dir().join("recently-used.xbel"));
    if let Ok(m) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        m.connect_changed(move |_, _, _, ev| {
            if !matches!(ev, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created) {
                return;
            }
            let (files, newest) = recent_files(seen.get());
            seen.set(newest);
            for (at, path) in files {
                recall::add(Entry { at, kind: Kind::File, app: String::new(), text: path, secs: None });
            }
        });
        std::mem::forget(m);
    }
    check();
}

fn kind_arg(a: &Value) -> Option<Kind> {
    match arg_str(a, "kind")? {
        "window" | "windows" => Some(Kind::Window),
        "file" | "files" => Some(Kind::File),
        "notification" | "notifications" => Some(Kind::Notification),
        _ => None,
    }
}

fn range(a: &Value) -> Result<(i64, i64), String> {
    let Ok(t) = glib::DateTime::now_local() else { return Err("no clock".into()) };
    let first = crate::calendar::start_day(arg_str(a, "start").or(Some("today")), &t)
        .ok_or("start must be today, yesterday or YYYY-MM-DD")?;
    let days = arg_u64(a, "days").unwrap_or(1).clamp(1, 31) as i64;
    Ok((first.to_unix(), first.to_unix() + days * 86400))
}

fn clock(at: i64) -> String {
    let fmt = if umoja_common::clock::twelve_hour() { "%a %-d %b %-l:%M %p" } else { "%a %-d %b %H:%M" };
    glib::DateTime::from_unix_local(at).ok().and_then(|d| d.format(fmt).ok()).map(|s| s.to_string()).unwrap_or_default()
}

pub fn render(list: &[Entry], limit: usize) -> String {
    let skip = list.len().saturating_sub(limit);
    let mut lines: Vec<String> = list
        .iter()
        .skip(skip)
        .map(|e| {
            let what = match e.kind {
                Kind::Window => {
                    let mins = e.secs.map(|s| (s + 30) / 60).filter(|m| *m > 0).map(|m| format!(" ({m} min)")).unwrap_or_default();
                    format!("window {}: {}{mins}", e.app, e.text)
                }
                Kind::File => format!("opened file {}", e.text),
                Kind::Notification => format!("notification from {}: {}", e.app, e.text),
            };
            format!("- {} {what}", clock(e.at))
        })
        .collect();
    if skip > 0 {
        lines.insert(0, format!("({skip} earlier entries left out; narrow the search)"));
    }
    lines.join("\n")
}

fn search(a: &Value) -> Outcome {
    if !recall::enabled() {
        return Outcome::ok(
            "Activity recall is off, so there is no timeline. It can be turned on with the activity_recall setting (it keeps a private, text-only list of windows, files and notifications on this computer).",
        );
    }
    let (start, end) = match range(a) {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    let list = recall::search(&recall::dir(), start, end, arg_str(a, "query").unwrap_or(""), kind_arg(a));
    if list.is_empty() {
        return Outcome::ok("Nothing in the timeline matches.");
    }
    Outcome::ok(render(&list, 120))
}

fn forget(a: &Value) -> Outcome {
    let all = arg_str(a, "start").is_none_or(|s| s == "all");
    let r = if all {
        None
    } else {
        match range(a) {
            Ok(r) => Some(r),
            Err(e) => return Outcome::err(e),
        }
    };
    let n = recall::forget(&recall::dir(), r);
    Outcome::ok(format!("Forgot {n} entries"))
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::read_only(
            "recall_activity",
            "Search the user's activity timeline (only when they turned on activity recall): which windows were in front (app and title), files opened and notifications, with times. Use for \"what was that page I had open yesterday\", \"which document was I editing this morning\", \"what did Dana message me about\". query: words to look for (empty for everything); start: today, yesterday or YYYY-MM-DD; days: how many days (default 1); kind: window, file or notification.",
            json!({"type": "object", "properties": {
                "query": {"type": "string"},
                "start": {"type": "string"},
                "days": {"type": "integer", "minimum": 1, "maximum": 31},
                "kind": {"type": "string", "enum": ["window", "file", "notification"]}
            }}),
            |a| Box::pin(async move { search(&a) }),
        )
        .with_summary(|_| "Look through the activity timeline".into()),
        Tool::new(
            "forget_activity",
            "Delete entries from the activity timeline: start \"all\" (or left out) for everything, or a day (today, yesterday, YYYY-MM-DD) and days.",
            json!({"type": "object", "properties": {
                "start": {"type": "string"},
                "days": {"type": "integer", "minimum": 1, "maximum": 31}
            }}),
            |a| Box::pin(async move { forget(&a) }),
        )
        .confirm()
        .with_summary(|a| match arg_str(a, "start") {
            None | Some("all") => "Forget the whole activity timeline".into(),
            Some(s) => format!("Forget the activity timeline from {s}"),
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_recently_used() {
        let xml = r#"<?xml version="1.0"?><xbel version="1.0">
  <bookmark href="file:///home/k/a%20b.txt" added="2026-11-10T10:00:00.000000Z" modified="2026-11-11T09:00:00.000000Z" visited="2026-11-10T10:00:00.000000Z">
    <info/></bookmark>
  <bookmark href="file:///home/k/x&amp;y.pdf" added="2026-11-12T08:00:00Z" modified="2026-11-12T08:00:00Z" visited="2026-11-12T08:30:00Z"></bookmark>
</xbel>"#;
        let v = parse_xbel(xml);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].1, "file:///home/k/a%20b.txt");
        assert_eq!(v[0].0, glib::DateTime::from_iso8601("2026-11-11T09:00:00Z", None).unwrap().to_unix());
        assert_eq!(v[1].1, "file:///home/k/x&y.pdf");
        assert_eq!(gio::File::for_uri(&v[0].1).path().unwrap().display().to_string(), "/home/k/a b.txt");
    }

    #[test]
    fn renders_entries() {
        let at = 1_000_000;
        let list = vec![
            Entry { at, kind: Kind::Window, app: "Firefox".into(), text: "Docs".into(), secs: Some(600) },
            Entry { at, kind: Kind::File, app: String::new(), text: "/tmp/a.txt".into(), secs: None },
        ];
        let s = render(&list, 1);
        assert!(s.starts_with("(1 earlier"));
        assert!(s.contains("opened file /tmp/a.txt"));
        assert!(render(&list, 5).contains("window Firefox: Docs (10 min)"));
    }
}

//! Calendar events, from the same place gnome-shell's date menu gets them:
//! `org.gnome.Shell.CalendarServer` (gnome-shell-calendar-server, D-Bus
//! activated), which reads Evolution Data Server, so every calendar added
//! in Settings › Online Accounts or GNOME Calendar shows up.
//!
//! [`fetch`] asks for a time range and collects the events the server
//! sends back. The server keeps one range for all its clients, so callers
//! ask again whenever they need events rather than holding a subscription.

use gio::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

pub const BUS_NAME: &str = "org.gnome.Shell.CalendarServer";
pub const OBJECT_PATH: &str = "/org/gnome/Shell/CalendarServer";
pub const INTERFACE: &str = "org.gnome.Shell.CalendarServer";

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: String,
    pub summary: String,
    /// Unix seconds.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
}

/// Whether `[start, end)` covers whole local days (midnight to midnight).
pub fn whole_days(start: i64, end: i64) -> bool {
    let at_midnight = |t: i64| {
        glib::DateTime::from_unix_local(t)
            .map(|d| d.hour() == 0 && d.minute() == 0 && d.second() == 0)
            .unwrap_or(false)
    };
    end > start && (end - start) % 3600 == 0 && (end - start) >= 23 * 3600 && at_midnight(start) && at_midnight(end)
}

/// `EventsAddedOrUpdated`'s `a(ssxxa{sv})`.
pub fn parse_events(v: &glib::Variant) -> Vec<Event> {
    let list = if v.type_().as_str().starts_with('(') { v.child_value(0) } else { v.clone() };
    list.iter()
        .filter_map(|e| {
            let id = e.child_value(0).str()?.to_string();
            let summary = e.child_value(1).str().unwrap_or("").to_string();
            let start = e.child_value(2).get::<i64>()?;
            let end = e.child_value(3).get::<i64>().unwrap_or(start);
            let raw = e.child_value(4);
            let raw = if raw.type_().as_str() == "v" { raw.as_variant()? } else { raw };
            let extras = glib::VariantDict::new(Some(&raw).filter(|r| r.type_().as_str() == "a{sv}"));
            let all_day = extras
                .lookup::<bool>("is-all-day")
                .ok()
                .flatten()
                .unwrap_or_else(|| whole_days(start, end));
            Some(Event { id, summary, start, end, all_day })
        })
        .collect()
}

/// Events overlapping `[start, end)`, sorted (all-day first, then by
/// start). `None` when no calendar server runs (gnome-shell isn't
/// installed) or no calendars are set up.
pub async fn fetch(conn: &gio::DBusConnection, start: i64, end: i64) -> Option<Vec<Event>> {
    let events: Rc<RefCell<Vec<Event>>> = Rc::default();
    let (tx, rx) = async_channel::unbounded::<()>();
    let sink = events.clone();
    let id = conn.signal_subscribe(
        Some(BUS_NAME),
        Some(INTERFACE),
        Some("EventsAddedOrUpdated"),
        Some(OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            let mut list = sink.borrow_mut();
            for e in parse_events(params) {
                list.retain(|x| x.id != e.id);
                list.push(e);
            }
            let _ = tx.try_send(());
        },
    );
    let has = conn
        .call_future(
            Some(BUS_NAME),
            OBJECT_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(INTERFACE, "HasCalendars").to_variant()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await
        .ok()
        .and_then(|v| v.child_value(0).as_variant())
        .and_then(|v| v.get::<bool>());
    let result = if has != Some(true) {
        None
    } else {
        let set = conn
            .call_future(
                Some(BUS_NAME),
                OBJECT_PATH,
                INTERFACE,
                "SetTimeRange",
                Some(&(start, end, true).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await;
        if set.is_err() {
            None
        } else {
            // Events arrive in batches after the call; stop once they've
            // been quiet for a moment (or after a few seconds in all).
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let mut quiet = Duration::from_millis(1500);
            while std::time::Instant::now() < deadline {
                match glib::future_with_timeout(quiet, rx.recv()).await {
                    Ok(Ok(())) => quiet = Duration::from_millis(400),
                    _ => break,
                }
            }
            let mut list: Vec<Event> =
                events.borrow().iter().filter(|e| e.end > start && e.start < end).cloned().collect();
            sort(&mut list);
            Some(list)
        }
    };
    conn.signal_unsubscribe(id);
    result
}

pub fn sort(list: &mut [Event]) {
    list.sort_by(|a, b| (!a.all_day, a.start, &a.summary).cmp(&(!b.all_day, b.start, &b.summary)));
}

/// Events on the local day starting at `day_start`.
pub fn on_day(events: &[Event], day_start: i64) -> Vec<Event> {
    let day_end = day_start + 24 * 3600;
    let mut v: Vec<Event> = events.iter().filter(|e| e.end > day_start && e.start < day_end || e.start == day_start).cloned().collect();
    sort(&mut v);
    v
}

/// "10:00 – 10:30" (or 12-hour), "All Day", for a list row.
pub fn time_label(e: &Event, twelve_hour: bool) -> String {
    if e.all_day {
        return crate::i18n::shell("All Day");
    }
    let fmt = if twelve_hour { "%-l:%M %p" } else { "%H:%M" };
    let t = |s: i64| {
        glib::DateTime::from_unix_local(s)
            .ok()
            .and_then(|d| d.format(fmt).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    if e.end > e.start {
        format!("{} – {}", t(e.start), t(e.end))
    } else {
        t(e.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, start: i64, end: i64, all_day: bool) -> Event {
        Event { id: id.into(), summary: id.into(), start, end, all_day }
    }

    #[test]
    fn parses_the_server_signal() {
        let mut extras = std::collections::HashMap::<String, glib::Variant>::new();
        extras.insert("is-all-day".into(), false.to_variant());
        let row = ("id1".to_string(), "Standup".to_string(), 1000i64, 2800i64, extras);
        let v = (vec![row],).to_variant();
        let list = parse_events(&v);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].summary, "Standup");
        assert_eq!((list[0].start, list[0].end, list[0].all_day), (1000, 2800, false));
    }

    #[test]
    fn day_filter_and_order() {
        let day = 1_000_000;
        let list = vec![
            ev("late", day + 7200, day + 9000, false),
            ev("allday", day, day + 86400, true),
            ev("early", day + 3600, day + 5400, false),
            ev("tomorrow", day + 90000, day + 93600, false),
        ];
        let ids: Vec<String> = on_day(&list, day).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["allday", "early", "late"]);
    }
}

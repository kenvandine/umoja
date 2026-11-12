//! The user's calendar events (read only), from the calendar server
//! gnome-shell's date menu uses (see `umoja_common::calendar`).

use crate::tool::{Outcome, Tool, arg_str, arg_u64};
use serde_json::{Value, json};
use umoja_common::calendar;

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::read_only(
            "calendar_events",
            "List the user's calendar events (from GNOME Online Accounts and GNOME Calendar). Use for \"what's on today\", \"when is my next meeting\", \"am I free on Friday\". start is \"today\", \"tomorrow\" or a date (YYYY-MM-DD); days is how many days to cover (default 1, at most 31; use 7 for the next meeting).",
            json!({"type": "object", "properties": {
                "start": {"type": "string"},
                "days": {"type": "integer", "minimum": 1, "maximum": 31}
            }}),
            |a| Box::pin(async move { events(a).await }),
        )
        .with_summary(|_| "Look at the calendar".into()),
    ]
}

/// Local midnight of the day `start` names.
pub fn start_day(start: Option<&str>, now: &glib::DateTime) -> Option<glib::DateTime> {
    let today = glib::DateTime::from_local(now.year(), now.month(), now.day_of_month(), 0, 0, 0.0).ok()?;
    match start.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") | Some("today") | Some("now") => Some(today),
        Some("tomorrow") => today.add_days(1).ok(),
        Some("yesterday") => today.add_days(-1).ok(),
        Some(s) => {
            let mut it = s.splitn(3, '-').map(|p| p.parse::<i32>().ok());
            let (y, m, d) = (it.next()??, it.next()??, it.next()??);
            glib::DateTime::from_local(y, m, d, 0, 0, 0.0).ok()
        }
    }
}

async fn events(a: Value) -> Outcome {
    let Ok(now) = glib::DateTime::now_local() else { return Outcome::err("no clock") };
    let Some(first) = start_day(arg_str(&a, "start"), &now) else {
        return Outcome::err("start must be today, tomorrow or YYYY-MM-DD");
    };
    let days = arg_u64(&a, "days").unwrap_or(1).clamp(1, 31) as i64;
    let (start, end) = (first.to_unix(), first.to_unix() + days * 86400);
    let Ok(conn) = crate::desktop::session_bus().await else { return Outcome::err("no session bus") };
    let Some(list) = calendar::fetch(&conn, start, end).await else {
        return Outcome::ok("No calendars are set up (add one in Settings › Online Accounts or GNOME Calendar).");
    };
    if list.is_empty() {
        return Outcome::ok(format!("No events in those {days} day(s)."));
    }
    let twelve = umoja_common::clock::twelve_hour();
    let lines: Vec<String> = list
        .iter()
        .map(|e| {
            let day = glib::DateTime::from_unix_local(e.start)
                .ok()
                .and_then(|d| d.format("%a %-d %b").ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let past = if e.end <= now.to_unix() { " (over)" } else { "" };
            format!("- {day} {}: {}{past}", calendar::time_label(e, twelve), e.summary)
        })
        .collect();
    let today = now.format("%A %-d %B %Y %H:%M").map(|s| s.to_string()).unwrap_or_default();
    Outcome::ok(format!("Now: {today}\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_days() {
        let now = glib::DateTime::from_local(2026, 11, 12, 15, 30, 0.0).unwrap();
        let d = |s| start_day(Some(s), &now).map(|d| (d.month(), d.day_of_month(), d.hour()));
        assert_eq!(d("today"), Some((11, 12, 0)));
        assert_eq!(d("tomorrow"), Some((11, 13, 0)));
        assert_eq!(d("2026-12-01"), Some((12, 1, 0)));
        assert_eq!(d("friday"), None);
    }
}

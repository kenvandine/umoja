//! What the user picks in Ask Umoja, so the rows they use most come first.
//!
//! Kept in ~/.local/share/umoja/ask-history.json as row id → uses and the
//! last time. Nothing is kept while Settings › Privacy › "Remember app
//! usage" (org.gnome.desktop.privacy remember-app-usage) is off, and only
//! row ids are stored: never what was typed or asked.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Rows remembered at most; the least used go first.
const MAX_ENTRIES: usize = 200;
/// A use counts half as much after this many days.
const HALF_LIFE_DAYS: f64 = 14.0;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Use {
    pub count: u32,
    /// Seconds since the Unix epoch.
    pub last: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    uses: HashMap<String, Use>,
}

fn path() -> PathBuf {
    gtk4::glib::user_data_dir()
        .join("umoja")
        .join("ask-history.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether usage may be recorded (GNOME's privacy switch).
pub fn enabled() -> bool {
    umoja_common::gsettings::settings("org.gnome.desktop.privacy")
        .filter(|s| umoja_common::gsettings::has_key(s, "remember-app-usage"))
        .map(|s| gio::prelude::SettingsExt::boolean(&s, "remember-app-usage"))
        .unwrap_or(true)
}

impl History {
    pub fn load() -> Self {
        if !enabled() {
            return Self::default();
        }
        std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Note that `id` was picked, and save.
    pub fn record(&mut self, id: &str) {
        if !enabled() {
            return;
        }
        self.add(id, now());
        if let Ok(json) = serde_json::to_string(self) {
            let p = path();
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }

    fn add(&mut self, id: &str, at: u64) {
        let u = self.uses.entry(id.to_string()).or_default();
        u.count = u.count.saturating_add(1);
        u.last = at;
        if self.uses.len() > MAX_ENTRIES {
            let mut all: Vec<(String, f64)> = self
                .uses
                .iter()
                .map(|(k, u)| (k.clone(), weight(*u, at)))
                .collect();
            all.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (k, _) in all.into_iter().take(self.uses.len() - MAX_ENTRIES) {
                self.uses.remove(&k);
            }
        }
    }

    /// A small bonus for rows used often and lately, added to the match
    /// score: at most about a third of a strong match, so typing still
    /// decides.
    pub fn bonus(&self, id: &str) -> u32 {
        self.uses
            .get(id)
            .map(|u| (weight(*u, now()) * 3.0).min(12.0) as u32)
            .unwrap_or(0)
    }

    /// The most used row ids, best first, for the empty search.
    pub fn top(&self, n: usize) -> Vec<String> {
        let t = now();
        let mut all: Vec<(&String, f64)> =
            self.uses.iter().map(|(k, u)| (k, weight(*u, t))).collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        all.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }
}

/// Uses, halved for every HALF_LIFE_DAYS since the last one.
fn weight(u: Use, now: u64) -> f64 {
    let days = now.saturating_sub(u.last) as f64 / 86_400.0;
    u.count as f64 * 0.5f64.powf(days / HALF_LIFE_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_uses_weigh_more() {
        let day = 86_400;
        let fresh = weight(
            Use {
                count: 2,
                last: 100 * day,
            },
            100 * day,
        );
        let old = weight(
            Use {
                count: 2,
                last: 86 * day,
            },
            100 * day,
        );
        assert!((fresh - 2.0).abs() < 1e-9);
        assert!((old - 1.0).abs() < 1e-9);
    }

    #[test]
    fn least_used_are_dropped() {
        let mut h = History::default();
        for i in 0..MAX_ENTRIES {
            h.add(&format!("app:{i}"), 1000);
            h.add(&format!("app:{i}"), 1000);
        }
        h.add("app:new", 1000);
        assert_eq!(h.uses.len(), MAX_ENTRIES);
        assert!(!h.uses.contains_key("app:new"));
    }
}

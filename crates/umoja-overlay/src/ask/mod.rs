//! Ask Umoja's type-ahead: what the typed words match, and in what order.
//!
//! Everything here is plain data and arithmetic, so it is tested without a
//! display. The card itself (rows, keys, the assistant) lives in view.rs.

pub mod calc;
pub mod catalog;
pub mod history;

/// Words that say how, not what: "turn on wifi" is about Wi-Fi.
const FILLER: &[&str] = &[
    "turn", "switch", "toggle", "set", "the", "a", "an", "my", "please", "on", "off", "enable",
    "disable", "start", "stop", "open", "launch", "run", "show", "go", "to", "mode",
];

/// The words that name the thing being looked for, lowercased. Filler is
/// dropped unless nothing else is left ("open" alone still finds "OpenSCAD").
pub fn search_words(query: &str) -> Vec<String> {
    let all: Vec<String> = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let kept: Vec<String> = all
        .iter()
        .filter(|w| !FILLER.contains(&w.as_str()))
        .cloned()
        .collect();
    if kept.is_empty() { all } else { kept }
}

/// "turn on wifi" → Some(true), "wifi off" → Some(false), "wifi" → None.
pub fn switch_intent(query: &str) -> Option<bool> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(&w.as_str()));
    match (
        has(&["on", "enable", "start"]),
        has(&["off", "disable", "stop"]),
    ) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// How well `words` match an item called `title` that also answers to
/// `keywords`. Every word must match something; 0 means no match. A name
/// that starts with the word scores best, then a word in the name, then a
/// keyword, then (for words of three letters or more) any part of one.
pub fn score(title: &str, keywords: &[&str], words: &[String]) -> u32 {
    if words.is_empty() {
        return 0;
    }
    let name = title.to_lowercase();
    let parts: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect();
    let keywords: Vec<String> = keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut total = 0;
    for w in words {
        let best = if name.starts_with(w.as_str()) {
            30
        } else if parts.iter().any(|p| p.starts_with(w.as_str())) {
            25
        } else if keywords
            .iter()
            .any(|k| k.starts_with(w.as_str()) || k.split(' ').any(|p| p.starts_with(w.as_str())))
        {
            20
        } else if w.chars().count() > 2
            && (name.contains(w.as_str()) || keywords.iter().any(|k| k.contains(w.as_str())))
        {
            10
        } else {
            return 0;
        };
        total += best;
    }
    total
}

/// A score this high is a confident match for a short query: "dark" for
/// Dark Style. It then outranks the Ask row even if the text reads like a
/// question.
pub const STRONG: u32 = 20;

/// Whether the Ask row goes first: the text reads like a question and no
/// row is a confident match for a short query.
pub fn ask_first(looks_like_question: bool, best: Option<u32>, query: &str) -> bool {
    if !looks_like_question {
        return false;
    }
    let words = query.split_whitespace().count();
    match best {
        Some(s) if words < 3 => s < STRONG * words.max(1) as u32,
        _ => true,
    }
}

/// desktop_status's "Wi-Fi: on" lines as name → on. Lines that aren't
/// plainly on or off ("Power Mode: Balanced") are left out.
pub fn parse_status(text: &str) -> std::collections::HashMap<String, bool> {
    text.lines()
        .filter_map(|l| {
            let (name, value) = l.split_once(':')?;
            let on = match value.trim() {
                "on" => true,
                "off" => false,
                _ => return None,
            };
            Some((name.trim().to_string(), on))
        })
        .collect()
}

/// The search URL for "Search the web for …", with the query escaped.
pub fn web_search_url(query: &str) -> String {
    let mut q = String::new();
    for b in query.trim().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                q.push(b as char)
            }
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://duckduckgo.com/?q={q}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(q: &str) -> Vec<String> {
        search_words(q)
    }

    #[test]
    fn filler_words_are_dropped() {
        assert_eq!(w("turn on the Wi-Fi"), vec!["wi-fi"]);
        assert_eq!(w("dark mode off"), vec!["dark"]);
        assert_eq!(w("open"), vec!["open"]);
        assert_eq!(w("Firefox"), vec!["firefox"]);
    }

    #[test]
    fn on_off_intent() {
        assert_eq!(switch_intent("turn on wifi"), Some(true));
        assert_eq!(switch_intent("bluetooth off"), Some(false));
        assert_eq!(switch_intent("wifi"), None);
        assert_eq!(switch_intent("on off"), None);
    }

    #[test]
    fn scoring() {
        let dark = ["dark", "theme", "night mode"];
        assert_eq!(score("Dark Style", &dark, &w("dark")), 30);
        assert_eq!(score("Dark Style", &dark, &w("style")), 25);
        assert_eq!(score("Dark Style", &dark, &w("theme")), 20);
        assert_eq!(score("Dark Style", &dark, &w("night")), 20);
        assert_eq!(score("Dark Style", &dark, &w("tyl")), 10);
        assert_eq!(score("Dark Style", &dark, &w("dark banana")), 0);
        assert_eq!(score("Dark Style", &dark, &[]), 0);
        // Every word has to match.
        assert_eq!(score("Wi-Fi", &["wireless"], &w("turn on wifi")), 0);
        assert_eq!(
            score("Wi-Fi", &["wifi", "wireless"], &w("turn on wifi")),
            20
        );
        assert_eq!(score("Wi-Fi", &["wifi"], &w("wi")), 30);
    }

    #[test]
    fn ask_row_placement() {
        // "dark" is a confident match: the switch goes first.
        assert!(!ask_first(true, Some(30), "dark?"));
        // A real question goes to the assistant first.
        assert!(ask_first(true, Some(30), "why is my fan so loud"));
        assert!(ask_first(true, None, "fan loud"));
        // Not a question: the matches go first.
        assert!(!ask_first(false, None, "firefox"));
        // A weak match for a short question.
        assert!(ask_first(true, Some(10), "fan loud"));
    }

    #[test]
    fn status_lines() {
        let s =
            parse_status("Dark Style: on\nPower Mode: Balanced\nWi-Fi: off\nBluetooth: no adapter");
        assert_eq!(s.get("Dark Style"), Some(&true));
        assert_eq!(s.get("Wi-Fi"), Some(&false));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn web_urls() {
        assert_eq!(
            web_search_url(" rust & gtk "),
            "https://duckduckgo.com/?q=rust+%26+gtk"
        );
        assert_eq!(web_search_url("é"), "https://duckduckgo.com/?q=%C3%A9");
    }
}

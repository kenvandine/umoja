//! Emoji in Ask Umoja: "party" finds 🎉, ":tada:" or "emoji heart" too.
//!
//! Names and shortcodes come from the `emojis` crate (Unicode's names plus
//! GitHub's gemoji shortcodes and tags). Skin tone variants are left out:
//! the default yellow one is listed.

/// Rows shown at most.
pub const MAX: usize = 4;

/// Words that only say "I want an emoji".
const MARKERS: &[&str] = &["emoji", "emojis", "emoticon", "smiley"];

/// How a query asks for emoji.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// "emoji party", ":tada": match parts of words too.
    Explicit,
    /// "party": only whole words, so "fire" doesn't bury Firefox.
    Casual,
}

/// The words to look up and how, or `None` when the text isn't for emoji.
pub fn parse(query: &str) -> Option<(Vec<String>, Mode)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let (q, explicit) = match q.strip_prefix(':') {
        Some(rest) => (rest.trim_end_matches(':').to_string(), true),
        None => (q, false),
    };
    let all: Vec<String> = q
        .split(|c: char| c.is_whitespace() || c == '_' || c == ',')
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    let marked = all.iter().any(|w| MARKERS.contains(&w.as_str()));
    let words: Vec<String> = all.into_iter().filter(|w| !MARKERS.contains(&w.as_str())).collect();
    if words.is_empty() || words.len() > 3 {
        return None;
    }
    let mode = if explicit || marked { Mode::Explicit } else { Mode::Casual };
    if mode == Mode::Casual && (words.len() > 1 || words[0].chars().count() < 3) {
        return None;
    }
    Some((words, mode))
}

/// Score `name` and `codes` (shortcodes) against `words`; 0 = no match.
pub fn score(name: &str, codes: &[&str], words: &[String], mode: Mode) -> u32 {
    let name = name.to_lowercase();
    let name_words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let code_words: Vec<&str> = codes.iter().flat_map(|c| c.split('_')).collect();
    let mut total = 0;
    for w in words {
        let w = w.as_str();
        let s = if codes.contains(&w) {
            40
        } else if name_words.contains(&w) {
            30
        } else if code_words.contains(&w) {
            25
        } else if mode == Mode::Explicit
            && w.chars().count() >= 2
            && (name_words.iter().any(|n| n.starts_with(w)) || code_words.iter().any(|c| c.starts_with(w)))
        {
            15
        } else {
            return 0;
        };
        total += s;
    }
    // Shorter names are the plainer match ("fire" over "fire engine").
    total * 10 + 10u32.saturating_sub(name_words.len() as u32)
}

/// (emoji, name) for `query`, best first.
pub fn search(query: &str) -> Vec<(&'static str, &'static str)> {
    let Some((words, mode)) = parse(query) else {
        return Vec::new();
    };
    let mut hits: Vec<(u32, &'static emojis::Emoji)> = emojis::iter()
        .filter(|e| e.skin_tone().is_none_or(|t| t == emojis::SkinTone::Default))
        .filter_map(|e| {
            let codes: Vec<&str> = e.shortcodes().collect();
            let s = score(e.name(), &codes, &words, mode);
            (s > 0).then_some((s, e))
        })
        .collect();
    hits.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    hits.into_iter().take(MAX).map(|(_, e)| (e.as_str(), e.name())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn party_finds_tada() {
        let hits = search("party");
        assert!(hits.iter().any(|(e, _)| *e == "🎉"), "{hits:?}");
    }

    #[test]
    fn shortcodes_and_markers() {
        assert_eq!(search(":tada:").first().map(|h| h.0), Some("🎉"));
        assert!(!search("emoji hea").is_empty());
        assert!(search("heart emoji").iter().any(|(e, _)| e.starts_with('❤')));
    }

    #[test]
    fn casual_needs_whole_words() {
        assert!(search("firef").is_empty());
        assert!(search("open files").is_empty());
        assert!(search("ab").is_empty());
        assert!(parse("").is_none());
    }

    #[test]
    fn no_skin_tone_duplicates() {
        let hits = search("emoji thumbs up");
        let thumbs = hits.iter().filter(|(_, n)| n.contains("thumbs up")).count();
        assert!(thumbs <= 1, "{hits:?}");
    }
}

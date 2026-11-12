//! Rewriting selected text in place.
//!
//! Select text in any app and press Super+Shift+A: besides "Explain",
//! Ask Umoja offers rewrites (fix it, shorter, friendlier, more formal,
//! translate, or whatever you type). The result shows in the card with a
//! **Replace** button that types it over the selection in that app.

/// (id, row title, instruction for the model).
pub const PRESETS: &[(&str, &str, &str)] = &[
    (
        "fix",
        "Fix Spelling and Grammar",
        "Fix spelling, grammar and punctuation. Change nothing else.",
    ),
    ("shorter", "Make It Shorter", "Make it shorter and more direct, keeping every point."),
    ("friendlier", "Make It Friendlier", "Make it warmer and friendlier."),
    ("formal", "Make It More Formal", "Make it more formal and professional."),
    ("translate", "Translate", "translate"),
];

/// The system prompt for `instruction` (a preset's or the user's words).
/// `language` is the user's own, for "translate".
pub fn system_prompt(instruction: &str, language: &str) -> String {
    let task = if instruction == "translate" {
        format!(
            "Translate it into {language}; if it is already in {language}, translate it into \
             English instead."
        )
    } else {
        instruction.trim().to_string()
    };
    format!(
        "You rewrite text the user selected in another app; your reply replaces it there. \
         Task: {task} Keep the original language (unless asked to translate), its meaning, \
         names, numbers, links and line breaks. Reply with only the rewritten text: no \
         preamble, no quotes, no code fences, no notes."
    )
}

/// Whether typed words read as a rewrite instruction ("make it shorter",
/// "more casual", "rewrite as a haiku") rather than a question.
pub fn looks_like_instruction(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    const STARTS: &[&str] = &[
        "make ", "rewrite", "rephrase", "reword", "shorten", "fix", "translate", "turn it",
        "turn this", "more ", "less ", "simplify", "expand", "polish", "improve", "correct",
        "summarize", "as a ", "in ", "change ", "convert",
    ];
    !t.ends_with('?') && STARTS.iter().any(|s| t.starts_with(s))
}

/// The model's reply ready to type: fences and wrapping quotes gone.
pub fn clean_reply(reply: &str) -> String {
    let t = reply.trim();
    let t = t
        .strip_prefix("```")
        .map(|r| r.split_once('\n').map(|(_, b)| b).unwrap_or(r))
        .map(|r| r.trim_end().trim_end_matches("```"))
        .unwrap_or(t)
        .trim();
    let quoted = t.chars().count() > 1
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('“') && t.ends_with('”')));
    if quoted {
        let mut c = t.chars();
        c.next();
        c.next_back();
        c.as_str().to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_vs_questions() {
        assert!(looks_like_instruction("make it shorter"));
        assert!(looks_like_instruction("More casual"));
        assert!(!looks_like_instruction("what does this mean?"));
        assert!(!looks_like_instruction("who wrote this"));
    }

    #[test]
    fn replies_are_cleaned() {
        assert_eq!(clean_reply("```\nHello\n```"), "Hello");
        assert_eq!(clean_reply("\"Hi there\""), "Hi there");
        assert_eq!(clean_reply("  plain \n"), "plain");
    }

    #[test]
    fn prompts() {
        assert!(system_prompt("translate", "German").contains("into German"));
        assert!(system_prompt("Make it shorter.", "German").contains("Make it shorter."));
    }
}

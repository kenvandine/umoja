//! Screenshots named after what they show.
//!
//! After a screenshot is saved from the screenshot UI, its text is read
//! with Tesseract and the local model suggests a few words for it; the file
//! is then renamed from "Screenshot From 2026-11-12 10-04-33.png" to
//! "Screenshot From 2026-11-12 10-04-33 – Invoice from Acme.png", keeping
//! the date so the folder still sorts by time. Always the local model, in
//! the background; if anything is missing (no text, no OCR, no model) the
//! name stays as it was. `org.umoja.shell screenshot-names` turns it off.
//!
//! A whole-screen shot is named after the window in front: its text and
//! title, not whatever text-heavy windows happen to sit behind it.

use crate::context;
use gio::prelude::*;
use crate::lemonade::{ChatEvent, LemonadeClient, RUNTIME};
use std::path::{Path, PathBuf};
use std::time::Duration;
use umoja_common::gsettings as gs;

/// OCR text sent to the model.
const MAX_TEXT: usize = 1500;
/// Words and characters kept from the model's suggestion.
const MAX_WORDS: usize = 6;
const MAX_CHARS: usize = 60;

const PROMPT: &str = "You name screenshot files. You get the text read from a screenshot \
(it may be messy). Reply with a short name of two to six words saying what it shows, like \
\"Invoice from Acme\" or \"Firefox download error\". Use the language of the text. No quotes, \
no punctuation at the end, no file extension, nothing else.";

fn enabled() -> bool {
    gs::settings(gs::UMOJA_SHELL)
        .filter(|s| gs::has_key(s, "screenshot-names"))
        .is_none_or(|s| s.boolean("screenshot-names"))
}

/// The model's reply as a safe file-name part, or `None` if it's no use.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .trim_start_matches(|c: char| c == '-' || c == '*' || c.is_whitespace());
    let line = line.split_once(':').map(|(k, v)| if k.to_lowercase().contains("name") { v } else { line }).unwrap_or(line);
    let cleaned: String = line
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '&' | '\'' | ',' | '+') { c } else { ' ' })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().take(MAX_WORDS).collect();
    let mut name = words.join(" ");
    if name.chars().count() > MAX_CHARS {
        name = name.chars().take(MAX_CHARS).collect::<String>().trim_end().to_string();
    }
    let lower = name.to_lowercase();
    let useless = name.is_empty() || ["screenshot", "image", "screen", "unknown", "none"].contains(&lower.as_str());
    (!useless).then_some(name)
}

/// `path` with ` – <label>` before the extension.
pub fn renamed(path: &Path, label: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
    path.with_file_name(format!("{stem} – {label}.{ext}"))
}

/// Name the screenshot at `path` in the background. `whole_screen` says
/// the shot is of the entire screen, so it is named after the window in
/// front rather than everything on it.
pub fn start(path: PathBuf, whole_screen: bool) {
    if !enabled() {
        return;
    }
    glib::spawn_future_local(async move {
        // Ask straight away, while the window in front is still the one
        // that was shot.
        let front = if whole_screen { context::focused_window().await } else { None };
        if let Err(e) = name(&path, front).await {
            tracing::debug!(path = %path.display(), reason = %e, "screenshot keeps its name");
        }
    });
}

/// What the model is told about the shot: the window in front, if known,
/// then the text read from it.
pub fn describe(front: Option<&context::FocusedWindow>, text: &str) -> String {
    let text = context::truncate_chars(text, MAX_TEXT);
    match front {
        Some(w) => format!(
            "The screenshot is of the whole screen; name it after the window in front, app \"{}\", titled \"{}\". Text read from that window:\n{text}",
            w.app_id, w.title
        ),
        None => text,
    }
}

async fn name(path: &Path, front: Option<context::FocusedWindow>) -> Result<(), String> {
    let p = path.to_path_buf();
    let rect = front.as_ref().map(|w| w.rect);
    let text = context::blocking(move || {
        if !context::ocr_available() {
            return Err("no OCR".to_string());
        }
        // The window in front, captured again right away (its text is what
        // the shot is about); the whole file if that gives nothing.
        let squash = |t: String| t.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(rect) = rect {
            if let Ok(img) = context::capture(Some(rect)) {
                if let Ok(t) = context::ocr_file(&img.0) {
                    let t = squash(t);
                    if t.chars().count() >= 8 {
                        return Ok((t, true));
                    }
                }
            }
        }
        context::ocr_file(&p).map(|t| (squash(t), false)).map_err(|e| e.message())
    })
    .await?;
    let (text, from_front) = text;
    if text.chars().count() < 8 {
        return Err("too little text".into());
    }
    let front = front.filter(|_| from_front);
    let client = LemonadeClient::for_endpoint(umoja_common::assistant_backend::Endpoint::local(), None);
    let messages = vec![
        serde_json::json!({"role": "system", "content": PROMPT}),
        serde_json::json!({"role": "user", "content": describe(front.as_ref(), &text)}),
    ];
    let (tx, rx) = async_channel::unbounded();
    RUNTIME.spawn(async move { client.stream_chat(messages, Vec::new(), tx).await });
    let collect = async {
        let mut reply = String::new();
        while let Ok(ev) = rx.recv().await {
            match ev {
                ChatEvent::Text(d) => reply.push_str(&d),
                ChatEvent::Done => break,
                ChatEvent::Error(e) => return Err(e),
                ChatEvent::ToolCalls(_) => {}
            }
        }
        Ok(reply)
    };
    let reply = glib::future_with_timeout(Duration::from_secs(90), collect)
        .await
        .map_err(|_| "the model took too long".to_string())??;
    let label = clean(&context::strip_fences(&reply)).ok_or("no usable name")?;
    let target = renamed(path, &label);
    if !path.exists() || target.exists() {
        return Err("file moved or name taken".into());
    }
    std::fs::rename(path, &target).map_err(|e| e.to_string())?;
    tracing::info!(from = %path.display(), to = %target.display(), "screenshot named");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_become_names() {
        assert_eq!(clean("Invoice from Acme\n").as_deref(), Some("Invoice from Acme"));
        assert_eq!(clean("\"Firefox: download error.\"").as_deref(), Some("Firefox download error"));
        assert_eq!(clean("Name: Terminal build log").as_deref(), Some("Terminal build log"));
        assert_eq!(clean("a/b\\c").as_deref(), Some("a b c"));
        assert_eq!(clean("one two three four five six seven eight").as_deref(), Some("one two three four five six"));
        assert_eq!(clean("Screenshot"), None);
        assert_eq!(clean("  \n "), None);
    }

    #[test]
    fn whole_screen_names_the_front_window() {
        let w = context::FocusedWindow {
            app_id: "org.gnome.TextEditor".into(),
            title: "invoice.txt".into(),
            rect: (0, 0, 10, 10),
        };
        let d = describe(Some(&w), "Invoice 42 from Acme");
        assert!(d.contains("titled \"invoice.txt\"") && d.ends_with("Invoice 42 from Acme"));
        assert_eq!(describe(None, "just text"), "just text");
    }

    #[test]
    fn keeps_the_date() {
        let p = Path::new("/p/Screenshot From 2026-11-12 10-04-33.png");
        assert_eq!(
            renamed(p, "Invoice"),
            PathBuf::from("/p/Screenshot From 2026-11-12 10-04-33 – Invoice.png")
        );
    }
}

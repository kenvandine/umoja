//! Tools that let the assistant look at what the user is looking at: the
//! selected text, the clipboard and the current window (read with OCR).

use super::tools::{LocalFuture, Run, ToolSpec, arg_u64};
use crate::clipboard;
use crate::context;
use serde_json::{Value, json};

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "read_selected_text",
            "Read the text the user currently has selected in any app. Use when they say \
             \"this\", \"the selection\" or \"what I highlighted\" and attached nothing.",
            json!({"type": "object", "properties": {}}),
            Run::Main(selected_text),
        )
        .with_summary(|_| umoja_common::i18n::umoja("Read the selected text")),
        ToolSpec::new(
            "read_clipboard",
            "Read what the user recently copied, newest first. Use for \"what I copied\" or \
             \"the clipboard\".",
            json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "description": "How many recent items, 1 to 5 (default 1)"}
                }
            }),
            Run::Main(clipboard_items),
        )
        .with_summary(|_| umoja_common::i18n::umoja("Read the clipboard")),
        ToolSpec::new(
            "read_focused_window",
            "Read the window the user was working in: its app, title and the text visible in \
             it (via OCR, so it may contain recognition errors). Use when they ask about \"this \
             window\", \"this page\", \"this error\" or what is on their screen.",
            json!({"type": "object", "properties": {}}),
            Run::Main(focused_window),
        )
        .with_summary(|_| umoja_common::i18n::umoja("Read the current window")),
    ]
}

fn selected_text(_: Value) -> LocalFuture {
    Box::pin(async {
        match clipboard::history().selection() {
            Some((text, _)) => context::truncate_chars(&text, context::MAX_CONTEXT_CHARS),
            None => "Nothing is selected.".into(),
        }
    })
}

fn clipboard_items(args: Value) -> LocalFuture {
    let count = arg_u64(&args, "count").unwrap_or(1).clamp(1, 5) as usize;
    Box::pin(async move {
        let items = clipboard::history().items();
        if items.is_empty() {
            return "The clipboard history is empty.".into();
        }
        let budget = context::MAX_CONTEXT_CHARS / count;
        items
            .iter()
            .take(count)
            .enumerate()
            .map(|(i, t)| format!("[{}] {}", i + 1, context::truncate_chars(t, budget)))
            .collect::<Vec<_>>()
            .join("\n\n")
    })
}

fn focused_window(_: Value) -> LocalFuture {
    Box::pin(async {
        let Some(win) = context::focused_window().await else {
            return "There is no window open on this workspace.".into();
        };
        let header = format!("App: {}\nTitle: {}", win.app_id, win.title);
        match context::read_window(win.rect).await {
            Ok(text) => format!(
                "{header}\nVisible text (OCR):\n{}",
                context::truncate_chars(&text, context::MAX_CONTEXT_CHARS)
            ),
            Err(e) => format!("{header}\nThe window's text couldn't be read: {}", e.message()),
        }
    })
}

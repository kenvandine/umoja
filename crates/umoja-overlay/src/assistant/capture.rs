//! Screenshots, screen recording and the clipboard as assistant tools.
//!
//! These need the overlay itself: it steps out of the way before a capture
//! (unless the user asked for a picture of the overlay), knows which window
//! the user was in before it opened, and owns the screenshot UI and the
//! screen recorder. The overlay installs a [`Capturer`] with
//! [`set_capturer`]; the tools here only describe and dispatch.

use super::tools::{LocalFuture, Run, ToolSpec, arg_str, arg_u64};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use umoja_common::types::Risk;

/// What to capture.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// The window the user was in before the overlay opened, or the one
    /// named (app name or part of its title).
    Window(Option<String>),
    Screen,
    /// Let the user drag out an area in the screenshot UI.
    Area,
    /// The overlay itself, as it is on screen now.
    Overlay,
}

impl Target {
    pub fn from_args(a: &Value) -> Self {
        let named = arg_str(a, "window").map(String::from);
        match arg_str(a, "target").unwrap_or("window").to_lowercase().as_str() {
            "screen" | "desktop" | "full" | "fullscreen" | "everything" | "monitor" => Target::Screen,
            "area" | "region" | "selection" | "part" => Target::Area,
            "overlay" | "assistant" | "this" => Target::Overlay,
            _ => Target::Window(named),
        }
    }
}

type Reply = Pin<Box<dyn Future<Output = Result<String, String>>>>;

pub trait Capturer {
    /// Take a screenshot after `delay` seconds; resolves to a sentence for
    /// the model ("Saved … and copied it to the clipboard").
    fn screenshot(&self, target: Target, delay: u64) -> Reply;
    fn record(&self, start: bool) -> Reply;
    fn copy(&self, text: &str);
}

thread_local! {
    static CAPTURER: RefCell<Option<Rc<dyn Capturer>>> = const { RefCell::new(None) };
    /// "Firefox" / "Terminal": the window the user was in when the overlay
    /// opened, for the confirmation.
    static PREVIOUS_WINDOW: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn set_capturer(c: Rc<dyn Capturer>) {
    CAPTURER.with(|s| *s.borrow_mut() = Some(c));
}

pub fn set_previous_window(name: Option<String>) {
    PREVIOUS_WINDOW.with(|s| *s.borrow_mut() = name);
}

fn previous_window() -> Option<String> {
    PREVIOUS_WINDOW.with(|s| s.borrow().clone())
}

fn capturer() -> Option<Rc<dyn Capturer>> {
    CAPTURER.with(|s| s.borrow().clone())
}

pub fn screenshot_summary(a: &Value) -> String {
    let what = match Target::from_args(a) {
        Target::Window(Some(w)) => format!("a screenshot of {w}"),
        Target::Window(None) => match previous_window() {
            Some(w) => format!("a screenshot of the {w} window"),
            None => "a screenshot of the current window".into(),
        },
        Target::Screen => "a screenshot of the whole screen".into(),
        Target::Area => "a screenshot of an area you pick".into(),
        Target::Overlay => "a screenshot of the assistant".into(),
    };
    match arg_u64(a, "delay").filter(|d| *d > 0) {
        Some(d) => format!("Take {what} in {} second{}", d.min(10), if d == 1 { "" } else { "s" }),
        None => format!("Take {what}"),
    }
}

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "take_screenshot",
            "Take a screenshot and save it to Pictures/Screenshots (also copied to the clipboard). \
             target: window (default: the window the user was working in; pass window to pick \
             another app), screen (everything), area (the user drags out a part of the screen), or \
             overlay (only when the user asks for a picture of this assistant). The assistant \
             gets out of the way first. Never use run_command for screenshots.",
            json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["window", "screen", "area", "overlay"]},
                    "window": {"type": "string", "description": "Another window's app name or title, only if the user named one"},
                    "delay": {"type": "integer", "description": "Seconds to wait first, 0 to 10"}
                }
            }),
            Run::Main(screenshot),
        )
        .with_risk(|_| Risk::Medium)
        .with_summary(screenshot_summary),
        ToolSpec::new(
            "screen_recording",
            "Start or stop recording a video of the screen (saved to Videos/Screencasts).",
            json!({
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["start", "stop"]}},
                "required": ["action"]
            }),
            Run::Main(record),
        )
        .with_risk(|a| if arg_str(a, "action") == Some("stop") { Risk::Low } else { Risk::Medium })
        .with_summary(|a| {
            if arg_str(a, "action") == Some("stop") {
                "Stop the screen recording".into()
            } else {
                "Record the screen".into()
            }
        }),
        ToolSpec::new(
            "copy_to_clipboard",
            "Put text on the clipboard so the user can paste it (an address, a command, an \
             answer, a draft).",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
            Run::Main(copy),
        )
        .with_summary(|_| "Copy to the clipboard".into()),
    ]
}

fn screenshot(a: Value) -> LocalFuture {
    Box::pin(async move {
        let Some(c) = capturer() else {
            return "error: screenshots aren't available here".into();
        };
        let delay = arg_u64(&a, "delay").unwrap_or(0).min(10);
        match c.screenshot(Target::from_args(&a), delay).await {
            Ok(t) => t,
            Err(e) => format!("error: {e}"),
        }
    })
}

fn record(a: Value) -> LocalFuture {
    Box::pin(async move {
        let Some(c) = capturer() else {
            return "error: screen recording isn't available here".into();
        };
        let start = arg_str(&a, "action") != Some("stop");
        match c.record(start).await {
            Ok(t) => {
                if start {
                    super::hide_when_done();
                }
                t
            }
            Err(e) => format!("error: {e}"),
        }
    })
}

fn copy(a: Value) -> LocalFuture {
    Box::pin(async move {
        let Some(text) = a.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
            return "error: text is required".into();
        };
        match capturer() {
            Some(c) => {
                c.copy(text);
                "Copied to the clipboard".into()
            }
            None => "error: the clipboard isn't available here".into(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(Target::from_args(&json!({})), Target::Window(None));
        assert_eq!(
            Target::from_args(&json!({"target": "window", "window": "firefox"})),
            Target::Window(Some("firefox".into()))
        );
        assert_eq!(Target::from_args(&json!({"target": "screen"})), Target::Screen);
        assert_eq!(Target::from_args(&json!({"target": "area"})), Target::Area);
        assert_eq!(Target::from_args(&json!({"target": "overlay"})), Target::Overlay);
    }

    #[test]
    fn plain_confirmations() {
        set_previous_window(None);
        assert_eq!(screenshot_summary(&json!({})), "Take a screenshot of the current window");
        set_previous_window(Some("Firefox".into()));
        assert_eq!(screenshot_summary(&json!({"target": "window"})), "Take a screenshot of the Firefox window");
        assert_eq!(
            screenshot_summary(&json!({"target": "screen", "delay": 3})),
            "Take a screenshot of the whole screen in 3 seconds"
        );
        set_previous_window(None);
    }
}

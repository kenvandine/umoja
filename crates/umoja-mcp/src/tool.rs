//! The tool type every module contributes, and helpers for arguments.
//!
//! A [`Tool`] is a name, a description the model reads, a JSON Schema for
//! its arguments, a risk level, a one-line summary for the user and an
//! async function that does the work. Everything runs on the GLib main
//! loop; blocking work goes through [`blocking`].
//!
//! Risk is decided here, in the server, for every client: `Low` runs
//! straight away, `Medium` and `High` are confirmed with the user first
//! (see [`crate::server`]).

use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

pub use umoja_common::types::Risk;

pub type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// What a tool call produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Plain text for the model.
    pub text: String,
    pub is_error: bool,
    /// Machine-readable result (MCP `structuredContent`), e.g. file hits.
    pub data: Option<Value>,
    /// The tool put something in front of the user (opened an app, moved
    /// a window): an overlay-style client should get out of the way once
    /// the answer is finished.
    pub hide: bool,
}

impl Outcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    /// A text result; one starting with "error" counts as a failure.
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            is_error: text.starts_with("error"),
            text,
            ..Default::default()
        }
    }

    pub fn err(msg: impl std::fmt::Display) -> Self {
        Self {
            text: format!("error: {msg}"),
            is_error: true,
            ..Default::default()
        }
    }

    /// Mark that the client should hide once the answer is done.
    pub fn hide(mut self) -> Self {
        self.hide = true;
        self
    }

    /// [`Self::hide`] unless the call failed.
    pub fn hide_if_ok(mut self) -> Self {
        self.hide = !self.is_error;
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl From<Result<String, String>> for Outcome {
    fn from(r: Result<String, String>) -> Self {
        match r {
            Ok(s) => Outcome::ok(s),
            Err(e) => Outcome::err(e),
        }
    }
}

pub type RunFn = std::rc::Rc<dyn Fn(Value) -> LocalFuture<Outcome>>;
/// Extra detail for the confirmation ("This also removes libfoo"), worked
/// out before asking. An `Err` stops the call without asking.
pub type PreflightFn = fn(Value) -> LocalFuture<Result<Option<String>, String>>;

#[derive(Clone)]
pub struct Tool {
    pub name: &'static str,
    /// What the tool does and when to use it, for the model.
    pub description: String,
    /// JSON Schema (`{"type": "object", "properties": ...}`).
    pub parameters: Value,
    pub risk: fn(&Value) -> Risk,
    /// A short sentence for the user ("Turn on Dark Style"), shown as a
    /// progress line and in the confirmation. Empty falls back to the name.
    pub summary: fn(&Value) -> String,
    /// Only reads state (MCP `readOnlyHint`).
    pub read_only: bool,
    pub preflight: Option<PreflightFn>,
    pub run: RunFn,
}

pub fn low(_: &Value) -> Risk {
    Risk::Low
}

pub fn medium(_: &Value) -> Risk {
    Risk::Medium
}

pub fn high(_: &Value) -> Risk {
    Risk::High
}

fn no_summary(_: &Value) -> String {
    String::new()
}

impl Tool {
    /// A low-risk tool that changes something (dark style, a window).
    pub fn new(
        name: &'static str,
        description: impl Into<String>,
        parameters: Value,
        run: impl Fn(Value) -> LocalFuture<Outcome> + 'static,
    ) -> Self {
        Self {
            name,
            description: description.into(),
            parameters,
            risk: low,
            summary: no_summary,
            read_only: false,
            preflight: None,
            run: std::rc::Rc::new(run),
        }
    }

    /// A tool that only reads state.
    pub fn read_only(
        name: &'static str,
        description: impl Into<String>,
        parameters: Value,
        run: impl Fn(Value) -> LocalFuture<Outcome> + 'static,
    ) -> Self {
        Self {
            read_only: true,
            ..Self::new(name, description, parameters, run)
        }
    }

    /// Always ask the user before running.
    pub fn confirm(mut self) -> Self {
        self.risk = medium;
        self
    }

    pub fn with_risk(mut self, risk: fn(&Value) -> Risk) -> Self {
        self.risk = risk;
        self
    }

    pub fn with_summary(mut self, summary: fn(&Value) -> String) -> Self {
        self.summary = summary;
        self
    }

    pub fn with_preflight(mut self, f: PreflightFn) -> Self {
        self.preflight = Some(f);
        self
    }

    /// The user-facing line for a call with `args`.
    pub fn describe(&self, args: &Value) -> String {
        let s = (self.summary)(args);
        if s.is_empty() {
            let n = self.name.replace('_', " ");
            let mut c = n.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        } else {
            s
        }
    }

    /// The MCP `tools/list` entry.
    pub fn mcp_entry(&self) -> Value {
        let risk = (self.risk)(&json!({}));
        json!({
            "name": self.name,
            "title": self.describe(&json!({})),
            "description": self.description,
            "inputSchema": self.parameters,
            "annotations": {
                "readOnlyHint": self.read_only,
                "destructiveHint": risk == Risk::High,
                "openWorldHint": false,
            },
        })
    }
}

/// Run blocking work on GLib's thread pool.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    gio::spawn_blocking(f).await.map_err(|_| "the tool crashed".to_string())
}

/// Helpers for reading the loose arguments models produce.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Some(true),
            "false" | "off" | "no" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => s.trim().trim_start_matches('#').parse().ok(),
        _ => None,
    }
}

/// Show a JSON value the way a person would write it.
pub fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `~/…` → absolute path.
pub fn expand(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => glib::home_dir().join(rest),
        None if p == "~" => glib::home_dir(),
        None => std::path::PathBuf::from(p),
    }
}

/// Absolute path → `~/…` when under home.
pub fn tilde(p: &str) -> String {
    let home = glib::home_dir().to_string_lossy().into_owned();
    match p.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_helpers() {
        let a = json!({"on": "yes", "id": "42", "n": 3, "s": "  ", "h": "#7"});
        assert_eq!(arg_bool(&a, "on"), Some(true));
        assert_eq!(arg_u64(&a, "id"), Some(42));
        assert_eq!(arg_u64(&a, "n"), Some(3));
        assert_eq!(arg_u64(&a, "h"), Some(7));
        assert_eq!(arg_str(&a, "s"), None);
    }

    #[test]
    fn describe_falls_back_to_the_name() {
        let t = Tool::read_only("list_windows", "x", json!({"type": "object"}), |_| {
            Box::pin(async { Outcome::ok("") })
        });
        assert_eq!(t.describe(&json!({})), "List windows");
    }
}

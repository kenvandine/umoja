//! The assistant's tool registry.
//!
//! Every capability the Lemonade assistant can use is a [`ToolSpec`]: a
//! name, a description the model reads, a JSON Schema for its arguments, a
//! risk level and a function that does the work and returns a short text
//! result for the model. Tools are sent to Lemonade in the OpenAI `tools`
//! field and run by [`super::agent`] when the model calls them.
//!
//! ## Adding tools
//!
//! Put them in their own module with a `pub fn tools() -> Vec<ToolSpec>`
//! and add that function to [`PROVIDERS`] below (one line). Use
//! [`ToolSpec::read_only`] for blocking, side-effect-free tools (they run on
//! a worker thread), or [`ToolSpec::new`] with [`Run::Main`] for tools that
//! need the GTK main loop (GSettings, gio D-Bus, launching apps). Anything
//! that changes data or the system in a way that's hard to undo should use
//! [`ToolSpec::confirm`] (or a [`ToolSpec::with_risk`] function) so the user
//! is asked first.

use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

pub use umoja_common::types::Risk;

/// A tool future running on the GTK main loop.
pub type LocalFuture = Pin<Box<dyn Future<Output = String>>>;

// Blocking/read_only are for diagnostics and other contributed tools.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Run {
    /// Blocking work, run on a worker thread. Must not touch GTK.
    Blocking(fn(&Value) -> String),
    /// Async work on the GTK main loop.
    Main(fn(Value) -> LocalFuture),
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    /// What the tool does and when to use it, for the model.
    pub description: &'static str,
    /// JSON Schema (`{"type": "object", "properties": ...}`).
    pub parameters: Value,
    /// `Low` runs straight away; `Medium` and `High` ask the user first.
    pub risk: fn(&Value) -> Risk,
    /// A short sentence for the user ("Turn on Dark Style"), shown as a
    /// progress line and in the confirmation dialog. Empty falls back to
    /// the tool name.
    pub summary: fn(&Value) -> String,
    pub run: Run,
}

pub fn low(_: &Value) -> Risk {
    Risk::Low
}

pub fn medium(_: &Value) -> Risk {
    Risk::Medium
}

fn no_summary(_: &Value) -> String {
    String::new()
}

impl ToolSpec {
    /// A low-risk tool.
    pub fn new(name: &'static str, description: &'static str, parameters: Value, run: Run) -> Self {
        Self {
            name,
            description,
            parameters,
            risk: low,
            summary: no_summary,
            run,
        }
    }

    #[allow(dead_code)]
    /// A blocking tool that only reads state (never asks for confirmation).
    pub fn read_only(
        name: &'static str,
        description: &'static str,
        parameters: Value,
        run: fn(&Value) -> String,
    ) -> Self {
        Self::new(name, description, parameters, Run::Blocking(run))
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

    /// The user-facing line for a call with `args`.
    pub fn describe(&self, args: &Value) -> String {
        let s = (self.summary)(args);
        if s.is_empty() {
            self.name.replace('_', " ")
        } else {
            s
        }
    }

    /// The entry for the OpenAI `tools` array.
    pub fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    pub async fn run(&self, args: Value) -> String {
        match self.run {
            Run::Main(f) => f(args).await,
            Run::Blocking(f) => crate::lemonade::RUNTIME
                .spawn_blocking(move || f(&args))
                .await
                .unwrap_or_else(|e| format!("error: {e}")),
        }
    }
}

/// Every module that contributes tools. Add yours here.
const PROVIDERS: &[fn() -> Vec<ToolSpec>] = &[
    super::desktop::tools,
    super::settings::tools,
    super::notifications::tools,
    super::screen::tools,
    super::diagnostics::tools,
];

/// All registered tools, in registration order.
pub fn registry() -> Vec<ToolSpec> {
    PROVIDERS.iter().flat_map(|p| p()).collect()
}

/// Helpers for reading tool arguments.
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
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_schemas_are_objects() {
        let tools = registry();
        assert!(!tools.is_empty());
        let mut names: Vec<_> = tools.iter().map(|t| t.name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate tool names");
        for t in &tools {
            assert_eq!(t.parameters["type"], "object", "{}", t.name);
            assert!(
                t.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}",
                t.name
            );
            assert_eq!(t.schema()["function"]["name"], t.name);
        }
    }

    #[test]
    fn argument_helpers() {
        let a = json!({"on": "yes", "id": "42", "n": 3, "s": "  "});
        assert_eq!(arg_bool(&a, "on"), Some(true));
        assert_eq!(arg_u64(&a, "id"), Some(42));
        assert_eq!(arg_u64(&a, "n"), Some(3));
        assert_eq!(arg_str(&a, "s"), None);
    }
}

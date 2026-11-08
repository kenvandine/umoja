//! The assistant's tool-calling loop.
//!
//! A question goes to Lemonade with every registered tool. When the model
//! answers with tool calls, each one is shown to the user as a progress
//! line, confirmed first if it's risky, run, and its result sent back; the
//! loop repeats until the model answers in plain text (or gives up after
//! [`MAX_ROUNDS`]). Runs on the GTK main loop; network I/O stays on the
//! Tokio runtime.

use super::tools::{Risk, ToolSpec, registry};
use crate::lemonade::{ChatEvent, LemonadeClient, RUNTIME, ToolCall};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

/// Model round trips per question before we stop calling tools.
const MAX_ROUNDS: usize = 6;

pub const SYSTEM_PROMPT: &str = "You are Umoja OS Assistant, built into the user's Ubuntu \
desktop. You can act on the desktop with the provided tools: open apps, change settings, \
switch Wi-Fi, Bluetooth, power mode and dark style, and arrange windows and workspaces. When \
the user asks you to do something, do it with a tool call instead of explaining how. For \
settings, call list_settings to find the right one and its current value, then change_setting \
in the same answer; don't stop to ask. \"Show X\" or \"turn on X\" means set it to true. For \
relative changes like \"bigger\" or \"slower to dim\", compute the new value from the current \
one rather than switching the feature off. For windows, pass the app name as window (e.g. \
arrange_window with window \"terminal\" and position \"left\"). Never invent tool results. If you called a tool, \
reply in one short sentence saying what it did. For questions, answer in at most four short \
sentences of practical guidance. Write plain text: no Markdown, no backticks, no bold.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
    Declined,
}

/// What the agent needs from the window showing it.
pub trait Ui {
    /// The answer text so far (replaces what was shown).
    fn text(&self, text: &str);
    /// Add or update progress line `id` for a tool call.
    fn step(&self, id: usize, label: &str, state: StepState);
    /// Ask the user whether to go ahead; resolves to their answer.
    fn confirm(&self, label: &str, risk: Risk) -> Pin<Box<dyn Future<Output = bool>>>;
    /// True once the question was replaced or the window closed.
    fn cancelled(&self) -> bool;
}

/// A question plus optional context (selected text, window contents...)
/// that goes to the model as part of the user's message.
#[derive(Debug, Clone, Default)]
pub struct Question {
    pub text: String,
    pub context: Option<String>,
}

impl From<&str> for Question {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            context: None,
        }
    }
}

pub fn initial_messages(q: &Question) -> Vec<Value> {
    let user = match q.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(ctx) => format!("{ctx}\n\n{}", q.text),
        None => q.text.clone(),
    };
    vec![
        json!({"role": "system", "content": SYSTEM_PROMPT}),
        json!({"role": "user", "content": user}),
    ]
}

/// Answer `question`, calling tools as the model asks. Returns the final
/// answer text (empty when cancelled or failed; errors are shown via `ui`).
pub async fn run(client: LemonadeClient, question: Question, ui: Rc<dyn Ui>) -> String {
    // Troubleshooting questions start from a read-only system report.
    let question = super::diagnostics::ground(question, ui.as_ref()).await;
    if ui.cancelled() {
        return String::new();
    }
    run_with(client, question, ui, registry()).await
}

/// [`run`] with a given tool set (tests swap in stubs).
pub async fn run_with(
    client: LemonadeClient,
    question: Question,
    ui: Rc<dyn Ui>,
    tools: Vec<ToolSpec>,
) -> String {
    let schemas: Vec<Value> = tools.iter().map(ToolSpec::schema).collect();
    let mut messages = initial_messages(&question);
    let mut shown = String::new();
    let mut step_id = 0usize;

    for round in 0..MAX_ROUNDS {
        // On the last round, take the tools away so the model has to answer.
        let offered = if round + 1 == MAX_ROUNDS {
            Vec::new()
        } else {
            schemas.clone()
        };
        let (tx, rx) = async_channel::unbounded();
        let c = client.clone();
        let msgs = messages.clone();
        RUNTIME.spawn(async move { c.stream_chat(msgs, offered, tx).await });

        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        while let Ok(ev) = rx.recv().await {
            if ui.cancelled() {
                return String::new();
            }
            match ev {
                ChatEvent::Text(d) => {
                    text.push_str(&d);
                    ui.text(&visible_text(&text, &tools));
                }
                ChatEvent::ToolCalls(c) => calls = c,
                ChatEvent::Done => break,
                ChatEvent::Error(e) => {
                    ui.text(&e);
                    return String::new();
                }
            }
        }
        if ui.cancelled() {
            return String::new();
        }
        // Some chat templates leave tool calls in the text instead.
        if calls.is_empty() {
            calls = inline_tool_calls(&text, &tools);
        }
        let prose = visible_text(&text, &tools);
        if calls.is_empty() {
            // Only the final round's words are the answer; "I'll do that
            // now" from rounds that called tools is stale by now.
            let answer = if prose.is_empty() { shown } else { prose };
            ui.text(&answer);
            return answer;
        }
        if !prose.is_empty() {
            shown = prose.clone();
        }

        messages.push(json!({
            "role": "assistant",
            "content": prose,
            "tool_calls": calls.iter().map(|c| json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.arguments},
            })).collect::<Vec<_>>(),
        }));
        for call in &calls {
            step_id += 1;
            let result = run_call(&tools, call, step_id, ui.as_ref()).await;
            if ui.cancelled() {
                return String::new();
            }
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": result,
            }));
        }
    }
    shown
}

async fn run_call(tools: &[ToolSpec], call: &ToolCall, id: usize, ui: &dyn Ui) -> String {
    let Some(tool) = tools.iter().find(|t| t.name == call.name) else {
        return format!("error: there is no tool named {}", call.name);
    };
    let args = parse_args(&call.arguments);
    let label = tool.describe(&args);
    let risk = (tool.risk)(&args);
    if risk != Risk::Low {
        ui.step(id, &label, StepState::Running);
        if !ui.confirm(&label, risk).await {
            ui.step(id, &label, StepState::Declined);
            return "The user declined; do not try again.".into();
        }
    }
    ui.step(id, &label, StepState::Running);
    tracing::info!(tool = tool.name, args = %args, "assistant tool call");
    let result = tool.run(args).await;
    let failed = result.starts_with("error");
    ui.step(
        id,
        &label,
        if failed {
            StepState::Failed
        } else {
            StepState::Done
        },
    );
    result
}

fn parse_args(raw: &str) -> Value {
    let raw = raw.trim();
    if raw.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        // Some models double-encode the arguments.
        Ok(Value::String(s)) => serde_json::from_str(&s).unwrap_or(json!({})),
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

/// JSON objects in `text` that look like `{"name": <a tool>, "arguments": {...}}`,
/// with their byte ranges. Qwen-style templates wrap them in `<tool_call>`.
fn inline_spans(text: &str, tools: &[ToolSpec]) -> Vec<(usize, usize, ToolCall)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find('{') {
        let start = i + off;
        let mut it = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) => {
                let end = start + it.byte_offset();
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                if tools.iter().any(|t| t.name == name) {
                    let args = v
                        .get("arguments")
                        .or_else(|| v.get("parameters"))
                        .cloned()
                        .unwrap_or(json!({}));
                    let arguments = match args {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    out.push((
                        start,
                        end,
                        ToolCall {
                            id: format!("inline_{}", out.len()),
                            name: name.to_string(),
                            arguments,
                        },
                    ));
                }
                i = end;
            }
            _ => i = start + 1,
        }
    }
    out
}

fn inline_tool_calls(text: &str, tools: &[ToolSpec]) -> Vec<ToolCall> {
    inline_spans(text, tools).into_iter().map(|(_, _, c)| c).collect()
}

/// The text without reasoning, inline tool calls or their wrappers.
fn visible_text(text: &str, tools: &[ToolSpec]) -> String {
    let mut t = text.to_string();
    // Drop <think>…</think>, including an unfinished one while streaming.
    while let Some(s) = t.find("<think>") {
        match t[s..].find("</think>") {
            Some(e) => t.replace_range(s..s + e + "</think>".len(), ""),
            None => t.truncate(s),
        }
    }
    for (s, e, _) in inline_spans(&t.clone(), tools).into_iter().rev() {
        t.replace_range(s..e, "");
    }
    // Hide a tool call that is still streaming in.
    if let Some(s) = t.find("<tool_call>")
        && !t[s..].contains("</tool_call>")
    {
        t.truncate(s);
    }
    let t = t
        .replace("<tool_call>", "")
        .replace("</tool_call>", "")
        .replace("```json", "")
        .replace("```", "")
        // The answer is a plain label: drop Markdown emphasis and code marks.
        .replace("**", "")
        .replace('`', "");
    // Removing a call can leave its trailing "." or "," on a line of its own.
    t.lines()
        .filter(|l| {
            let l = l.trim();
            l.is_empty() || l.chars().any(char::is_alphanumeric)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::glib;

    #[test]
    fn inline_calls_are_found_and_hidden() {
        let tools = registry();
        let text = "Sure.\n<tool_call>\n{\"name\": \"set_dark_style\", \"arguments\": {\"enabled\": true}}\n</tool_call>";
        let calls = inline_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "set_dark_style");
        assert_eq!(parse_args(&calls[0].arguments), json!({"enabled": true}));
        assert_eq!(visible_text(text, &tools), "Sure.");
        // Unknown names and plain JSON are left alone.
        assert!(inline_tool_calls(r#"{"name": "rm", "arguments": {}}"#, &tools).is_empty());
    }

    #[test]
    fn streaming_text_hides_partial_calls_and_thinking() {
        let tools = registry();
        assert_eq!(visible_text("<think>hmm", &tools), "");
        assert_eq!(visible_text("<think>a</think>Done.", &tools), "Done.");
        assert_eq!(visible_text("Ok <tool_call>{\"name\": \"set_", &tools), "Ok");
        assert_eq!(visible_text("It is running.\n.", &tools), "It is running.");
        assert_eq!(visible_text("Open **`a.png`**", &tools), "Open a.png");
    }

    #[test]
    fn arguments_tolerate_double_encoding() {
        assert_eq!(parse_args(r#""{\"a\":1}""#), json!({"a": 1}));
        assert_eq!(parse_args(""), json!({}));
        assert_eq!(parse_args("not json"), json!({}));
    }

    /// Asks the live Lemonade on 127.0.0.1:13345 a set of requests and
    /// prints which tools the model called. State-changing tools are
    /// stubbed, so nothing on the desktop changes. Run with
    /// `cargo test -p umoja-overlay live_tool_choice -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_tool_choice() {
        use std::cell::RefCell;
        struct Rec(RefCell<Vec<String>>, RefCell<String>);
        impl Ui for Rec {
            fn text(&self, t: &str) {
                *self.1.borrow_mut() = t.to_string();
            }
            fn step(&self, _: usize, label: &str, state: StepState) {
                if state != StepState::Running {
                    self.0.borrow_mut().push(format!("{label} [{state:?}]"));
                }
            }
            fn confirm(&self, label: &str, _: Risk) -> Pin<Box<dyn Future<Output = bool>>> {
                self.0.borrow_mut().push(format!("ASKED: {label}"));
                Box::pin(async { false })
            }
            fn cancelled(&self) -> bool {
                false
            }
        }
        const REAL: &[&str] = &["list_settings", "desktop_status"];
        let tools: Vec<ToolSpec> = registry()
            .into_iter()
            .map(|mut t| {
                if t.name == "list_windows" {
                    t.run = super::super::tools::Run::Blocking(|_| {
                        "1. org.gnome.Ptyxis — ken@umoja: ~ (focused)\n2. firefox — Ubuntu — Mozilla Firefox\nYou are on workspace 1 of 3.".into()
                    });
                } else if !REAL.contains(&t.name) {
                    t.run = super::super::tools::Run::Blocking(|_| "done".into());
                }
                t
            })
            .collect();
        let prompts = [
            "turn on dark mode",
            "open firefox",
            "switch to power saver",
            "turn off bluetooth",
            "turn wifi off",
            "make text bigger",
            "stop the screen dimming so fast",
            "put the terminal on the left half of the screen",
            "move firefox to workspace 2",
            "show battery percentage",
            "delete everything in my Downloads folder",
            "how do I set up a VPN?",
        ];
        let ctx = glib::MainContext::default();
        for p in prompts {
            let rec = Rc::new(Rec(RefCell::new(Vec::new()), RefCell::new(String::new())));
            let t0 = std::time::Instant::now();
            let answer = ctx.block_on(run_with(
                LemonadeClient::new(),
                Question::from(p),
                rec.clone(),
                tools.clone(),
            ));
            println!("\n> {p}  ({:.1}s)", t0.elapsed().as_secs_f32());
            for s in rec.0.borrow().iter() {
                println!("  - {s}");
            }
            println!("  = {}", answer.replace('\n', " "));
            assert!(
                !answer.is_empty(),
                "no answer for {p:?}; last shown: {:?}",
                rec.1.borrow()
            );
        }
    }

    #[test]
    fn context_goes_before_the_question() {
        let m = initial_messages(&Question {
            text: "explain this".into(),
            context: Some("Selected text: E0382".into()),
        });
        assert_eq!(m[1]["content"], "Selected text: E0382\n\nexplain this");
    }
}

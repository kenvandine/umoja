//! The assistant's tool-calling loop.
//!
//! A question goes to the model (the local Lemonade or the cloud backend)
//! with every tool: the desktop tools served by umoja-mcp (see [`super::mcp`])
//! plus the overlay's own (selected text, clipboard, current window). When the model
//! answers with tool calls, each one is shown to the user as a progress
//! line, confirmed first if it's risky (umoja-mcp decides and asks through
//! the overlay's dialog), run, and its result sent back; the
//! loop repeats until the model answers in plain text (or gives up after
//! [`MAX_ROUNDS`]). Runs on the GTK main loop; network I/O stays on the
//! Tokio runtime.

use super::mcp::{Confirm, McpClient};
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
switch Wi-Fi, Bluetooth, power mode and dark style, arrange windows and workspaces, and find \
or remove installed apps. When the user asks you to do something, do it with a tool call \
instead of explaining how, and never ask the user to run a command to find something out: \
look it up with a tool. To uninstall an app, call remove_app with its name; it works out how \
the app was installed and asks the user itself; to install one, call install_app with its name. \
Everyday actions each have their own tool, so use it and never run_command for them: \
take_screenshot (target window by default, screen, area, or overlay only if the user asks for a \
picture of you), screen_recording, session (log out, lock, suspend, restart, power off), \
set_volume, set_audio_device, set_brightness, media_control, set_do_not_disturb, \
set_night_light, set_airplane_mode, connect_wifi, bluetooth_device, open_url (web pages and \
web searches), open_in_terminal (command-line programs, in the user's own terminal; never \
name a terminal app), open_folder, set_wallpaper, empty_trash, clear_folder, eject_drive, force_quit_app, \
check_updates, install_updates, copy_to_clipboard, calendar_events (the user's meetings and events), and date_time or system_info for facts \
about now and this computer. Those tools ask the user themselves when needed. For \
settings, call list_settings to find the right one and its current value, then change_setting \
in the same answer; don't stop to ask. \"Show X\" or \"turn on X\" means set it to true. For \
relative changes like \"bigger\" or \"slower to dim\", compute the new value from the current \
one rather than switching the feature off. When the user replies to a question you asked (yes, \
no, sure, the other one), do what you offered in the earlier turn. For windows, pass the app name as window (e.g. \
arrange_window with window \"terminal\" and position \"left\"). Never invent tool results. If you called a tool, \
reply in one short sentence saying what it did. For questions, answer in at most four short \
sentences of practical guidance. For \"when X happens, do Y\" or \"every day at...\" requests, call create_automation. To save or bring back a set of windows (\"save this as Coding\", \"set up for writing\"), use save_setup, define_setup or open_setup. When the user says undo or put it back, call undo. For \"what was that page/file/message from earlier\" questions, call recall_activity. When the user wants help learning the desktop, the manual or the guide, call open_user_guide. For coding jobs (writing a script or program, changing code in \
a project, fixing a build), don't write the code yourself: call hand_off_to_agent with the whole \
task, and if no agent is installed, say so in one sentence. Write plain text: no Markdown, no \
backticks, no bold.";

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
    /// Step `id` changed something that can be put back: offer Undo for
    /// journal entry `undo_id` (see umoja-mcp's undo module).
    fn undoable(&self, _id: usize, _undo_id: u64) {}
}

/// A question plus optional context (selected text, window contents...)
/// that goes to the model as part of the user's message, and the earlier
/// turns of the conversation when it's a follow-up.
#[derive(Debug, Clone, Default)]
pub struct Question {
    pub text: String,
    pub context: Option<String>,
    /// Earlier user and assistant messages, oldest first, as chat
    /// messages (`{"role": "user"|"assistant", "content": ...}`).
    pub history: Vec<Value>,
}

impl From<&str> for Question {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            ..Default::default()
        }
    }
}

/// The user's message for `q`: its context, then the question.
pub fn user_content(q: &Question) -> String {
    match q
        .context
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        Some(ctx) => format!("{ctx}\n\n{}", q.text),
        None => q.text.clone(),
    }
}

pub fn initial_messages(q: &Question) -> Vec<Value> {
    let mut m = vec![json!({"role": "system", "content": SYSTEM_PROMPT})];
    m.extend(q.history.iter().cloned());
    m.push(json!({"role": "user", "content": user_content(q)}));
    m
}

/// How the user can answer an assistant's reply that ends in a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// "Do you want me to open it?": offer Yes and No.
    YesNo,
    /// "Which one should I remove?": the user types an answer.
    Open,
}

/// Whether `answer` ends by asking the user something, and what kind of
/// question it is.
pub fn expects_reply(answer: &str) -> Option<Reply> {
    let t = answer.trim_end();
    if !t.ends_with('?') {
        return None;
    }
    let body = &t[..t.len() - 1];
    // The last sentence: after the previous ".", "!", "?" or line break.
    let last = body
        .rfind(['.', '!', '?', '\n'])
        .map_or(body, |i| &body[i + 1..])
        .trim();
    let first = last
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
        .to_lowercase();
    const YES_NO: &[&str] = &[
        "do",
        "does",
        "did",
        "should",
        "shall",
        "would",
        "will",
        "can",
        "could",
        "may",
        "is",
        "are",
        "want",
        "ok",
        "okay",
        "shouldn't",
        "don't",
        "have",
        "has",
    ];
    // "Should I remove the snap or the deb?" offers a choice, not yes/no.
    let choice = last.contains(" or ") && !last.trim_end().ends_with("or not");
    if YES_NO.contains(&first.as_str()) && !choice {
        Some(Reply::YesNo)
    } else {
        Some(Reply::Open)
    }
}

/// The tools offered to the model: the overlay's own plus umoja-mcp's.
#[derive(Clone)]
pub struct Toolbox {
    pub local: Vec<ToolSpec>,
    pub mcp: Option<Rc<McpClient>>,
}

impl Toolbox {
    /// The overlay's tools and the shared umoja-mcp connection.
    pub async fn load() -> Self {
        Self {
            local: registry(),
            mcp: super::mcp::shared().await,
        }
    }

    fn local(&self, name: &str) -> Option<&ToolSpec> {
        self.local.iter().find(|t| t.name == name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.local(name).is_some() || self.mcp.as_ref().is_some_and(|m| m.has(name))
    }

    pub fn schemas(&self) -> Vec<Value> {
        let mut s: Vec<Value> = self.local.iter().map(ToolSpec::schema).collect();
        if let Some(m) = &self.mcp {
            s.extend(m.openai_schemas().into_iter().filter(|t| {
                self.local(t["function"]["name"].as_str().unwrap_or(""))
                    .is_none()
            }));
        }
        s
    }
}

/// Answer `question`, calling tools as the model asks. Returns the final
/// answer text (empty when cancelled or failed; errors are shown via `ui`).
pub async fn run(client: LemonadeClient, question: Question, ui: Rc<dyn Ui>) -> String {
    // Troubleshooting questions start from a read-only system report.
    let question = super::diagnostics::ground(question, ui.as_ref()).await;
    if ui.cancelled() {
        return String::new();
    }
    let tools = Toolbox::load().await;
    if ui.cancelled() {
        return String::new();
    }
    run_with(client, question, ui, tools).await
}

/// [`run`] with a given tool set (tests swap in stubs).
pub async fn run_with(
    client: LemonadeClient,
    question: Question,
    ui: Rc<dyn Ui>,
    tools: Toolbox,
) -> String {
    let schemas: Vec<Value> = tools.schemas();
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
            let result = run_call(&tools, call, step_id, &ui).await;
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

async fn run_call(tools: &Toolbox, call: &ToolCall, id: usize, ui: &Rc<dyn Ui>) -> String {
    let args = parse_args(&call.arguments);
    if let Some(tool) = tools.local(&call.name) {
        return run_local(tool, args, id, ui.as_ref()).await;
    }
    match &tools.mcp {
        Some(m) if m.has(&call.name) => run_mcp(m, &call.name, args, id, ui).await,
        _ => format!("error: there is no tool named {}", call.name),
    }
}

async fn run_local(tool: &ToolSpec, args: Value, id: usize, ui: &dyn Ui) -> String {
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

/// A umoja-mcp tool. The server decides whether to ask and asks through
/// `ui`'s dialog.
async fn run_mcp(mcp: &McpClient, name: &str, args: Value, id: usize, ui: &Rc<dyn Ui>) -> String {
    let (label, _) = mcp.describe(name, &args).await;
    ui.step(id, &label, StepState::Running);
    tracing::info!(tool = name, args = %args, "assistant tool call (umoja-mcp)");
    let asker = ui.clone();
    let confirm: Confirm = Rc::new(move |text, risk| asker.confirm(&text, risk));
    let r = mcp.call(name, args, confirm).await;
    if r.declined {
        ui.step(id, &label, StepState::Declined);
        return r.text;
    }
    if r.hide {
        super::hide_when_done();
    }
    if name == "search_files"
        && let Some(hits) = r.data.as_ref().and_then(|d| d["hits"].as_array())
    {
        let hits: Vec<crate::files::Hit> = hits
            .iter()
            .map(|h| crate::files::Hit {
                path: h["path"].as_str().unwrap_or("").into(),
                name: h["name"].as_str().unwrap_or("").into(),
                detail: h["detail"].as_str().unwrap_or("").into(),
                score: h["score"].as_f64().unwrap_or(0.0),
            })
            .collect();
        crate::files::show_tool_hits(&hits);
    }
    ui.step(
        id,
        &label,
        if r.is_error {
            StepState::Failed
        } else {
            StepState::Done
        },
    );
    if let Some(u) = r.undo.filter(|_| !r.is_error) {
        ui.undoable(id, u);
    }
    r.text
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
fn inline_spans(text: &str, tools: &Toolbox) -> Vec<(usize, usize, ToolCall)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find('{') {
        let start = i + off;
        let mut it = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) => {
                let end = start + it.byte_offset();
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                if tools.has(name) {
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

fn inline_tool_calls(text: &str, tools: &Toolbox) -> Vec<ToolCall> {
    inline_spans(text, tools)
        .into_iter()
        .map(|(_, _, c)| c)
        .collect()
}

/// The text without reasoning, inline tool calls or their wrappers.
fn visible_text(text: &str, tools: &Toolbox) -> String {
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

    /// A toolbox with a stand-in for a umoja-mcp tool.
    fn toolbox() -> Toolbox {
        let mut local = registry();
        local.push(ToolSpec::read_only(
            "set_dark_style",
            "stand-in",
            json!({"type": "object"}),
            |_| String::new(),
        ));
        Toolbox { local, mcp: None }
    }

    #[test]
    fn inline_calls_are_found_and_hidden() {
        let tools = toolbox();
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
        let tools = toolbox();
        assert_eq!(visible_text("<think>hmm", &tools), "");
        assert_eq!(visible_text("<think>a</think>Done.", &tools), "Done.");
        assert_eq!(
            visible_text("Ok <tool_call>{\"name\": \"set_", &tools),
            "Ok"
        );
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
    /// prints which tools the model called. umoja-mcp runs with --dry-run
    /// and every confirmation is declined, so nothing on the desktop
    /// changes. Build umoja-mcp first, then run
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
        // umoja-mcp in dry-run mode: read-only tools run for real, the
        // rest confirm as usual and then report "done" without acting.
        let ctx = glib::MainContext::default();
        let mcp = ctx
            .block_on(McpClient::start(&["--dry-run"]))
            .expect("build umoja-mcp first: cargo build -p umoja-mcp");
        let tools = Toolbox {
            local: registry(),
            mcp: Some(mcp),
        };
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
            "remove discord",
            "how is firefox installed?",
            "make the mouse pointer bigger",
            "take a screenshot of this window",
            "screenshot the whole screen",
            "shut down the computer",
            "restart",
            "log me out",
            "turn the volume down",
            "mute my microphone",
            "make the screen brighter",
            "pause the music",
            "turn on do not disturb",
            "install vlc",
            "are there any updates?",
            "empty the trash",
            "what time is it?",
            "what version of ubuntu am I running?",
            "search the web for gtk4 layer shell",
            "open my downloads folder",
            "firefox is frozen, kill it",
            "did anything crash today?",
            "why did nautilus crash?",
            "send the nautilus crash to ubuntu",
            "free up the memory the AI model is using",
            "write me a script that renames my photos by the date they were taken",
        ];
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
    fn questions_at_the_end_expect_a_reply() {
        assert_eq!(
            expects_reply(
                "btop is already installed as a snap, so I didn't install it again. Do you want me to open it?"
            ),
            Some(Reply::YesNo)
        );
        assert_eq!(
            expects_reply("Want me to turn it off? "),
            Some(Reply::YesNo)
        );
        assert_eq!(
            expects_reply("I found two. Which one should I remove?"),
            Some(Reply::Open)
        );
        assert_eq!(
            expects_reply("Should I remove the snap or the deb?"),
            Some(Reply::Open)
        );
        assert_eq!(expects_reply("Turned on Dark Style."), None);
        assert_eq!(expects_reply("Is it on? It is now."), None);
    }

    #[test]
    fn follow_ups_carry_the_earlier_turns() {
        let m = initial_messages(&Question {
            text: "yes".into(),
            context: None,
            history: vec![
                json!({"role": "user", "content": "Install btop"}),
                json!({"role": "assistant", "content": "It's installed. Open it?"}),
            ],
        });
        assert_eq!(m.len(), 4);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["content"], "Install btop");
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[3]["content"], "yes");
    }

    #[test]
    fn context_goes_before_the_question() {
        let m = initial_messages(&Question {
            text: "explain this".into(),
            context: Some("Selected text: E0382".into()),
            ..Default::default()
        });
        assert_eq!(m[1]["content"], "Selected text: E0382\n\nexplain this");
    }
}

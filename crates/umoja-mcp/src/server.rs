//! The MCP protocol: JSON-RPC 2.0 messages, one per line, over any
//! transport (stdio in `umoja-mcp`, see [`serve_stdio`]).
//!
//! Confirmation is enforced here for every client. Before a `Medium` or
//! `High` risk tool runs, the server asks the user:
//!
//! 1. through the client with an MCP `elicitation/create` request, when
//!    the client supports elicitation (the Umoja overlay does, and so does
//!    Claude Code);
//! 2. otherwise with a desktop dialog (zenity), when there is a display;
//! 3. otherwise the call is refused.
//!
//! Besides the standard methods the server answers `umoja/describe`
//! (`{name, arguments}` → `{label, risk}`), so a client can show a progress
//! line before a call. Tool results carry `_meta` with `umoja/hide` (the
//! tool put something in front of the user), `umoja/declined`, and
//! `umoja/undo` (`{id, label}`) for a change that can be put back with
//! `umoja/undo` (`{id}`), see [`crate::undo`].

use crate::tool::{Outcome, Risk, Tool};
use gio::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub const DECLINED: &str = "The user declined; do not try again.";

const INSTRUCTIONS: &str = "Tools for the Umoja desktop on this Ubuntu computer: apps (find how \
an app is installed, open it, remove it), desktop settings in plain language (list_settings, \
change_setting), Wi-Fi, Bluetooth, power mode, windows and workspaces, notifications, files and \
read-only troubleshooting reports. Prefer these tools over shell commands: to remove an app call \
remove_app with its name; it works out whether it is a snap, a deb, a Flatpak or an AppImage and \
asks the user itself. Use the dedicated tools for everyday actions (session for log out, lock, \
suspend, restart and power off; set_volume, set_brightness, media_control, connect_wifi, \
install_app, install_updates, empty_trash, force_quit_app, open_url, open_folder, date_time, \
system_info...) instead of run_command. Risky tools ask the user before they run; if a result says the user \
declined, don't try again.";

pub fn risk_name(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

/// Where outgoing messages go.
pub type Sink = Rc<dyn Fn(Value)>;

pub struct Server {
    tools: Vec<Tool>,
    /// Confirm as usual, then report "done" instead of changing anything.
    dry_run: bool,
    out: Sink,
    pending: RefCell<HashMap<i64, async_channel::Sender<Result<Value, String>>>>,
    next_id: Cell<i64>,
    client_caps: RefCell<Value>,
    /// Requests still being handled.
    inflight: Cell<usize>,
}

impl Server {
    pub fn new(tools: Vec<Tool>, dry_run: bool, out: Sink) -> Rc<Self> {
        Rc::new(Self {
            tools,
            dry_run,
            out,
            pending: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            client_caps: RefCell::new(json!({})),
            inflight: Cell::new(0),
        })
    }

    /// Handle one incoming message (request, notification or a response to
    /// one of our requests). Requests run concurrently on the main loop.
    pub fn receive(self: &Rc<Self>, msg: Value) {
        if let Value::Array(batch) = msg {
            for m in batch {
                self.receive(m);
            }
            return;
        }
        if msg.get("method").is_none() {
            // A response to elicitation/create.
            let Some(id) = msg.get("id").and_then(Value::as_i64) else { return };
            if let Some(tx) = self.pending.borrow_mut().remove(&id) {
                let r = match msg.get("error") {
                    Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.try_send(r);
            }
            return;
        }
        let this = self.clone();
        self.inflight.set(self.inflight.get() + 1);
        glib::spawn_future_local(async move {
            let id = msg.get("id").cloned();
            let reply = this.handle(&msg).await;
            this.inflight.set(this.inflight.get() - 1);
            if let Some(id) = id.filter(|i| !i.is_null()) {
                (this.out)(match reply {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, message)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                    }
                });
            }
        });
    }

    async fn handle(self: &Rc<Self>, msg: &Value) -> Result<Value, (i64, String)> {
        let method = msg["method"].as_str().unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        match method {
            "initialize" => {
                *self.client_caps.borrow_mut() = params.get("capabilities").cloned().unwrap_or(json!({}));
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "umoja-mcp", "title": "Umoja Desktop", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools.iter().map(Tool::mcp_entry).collect::<Vec<_>>()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = normalize_args(params.get("arguments"));
                let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
                    return Err((-32602, format!("unknown tool: {name}")));
                };
                Ok(result_json(self.call(tool, args).await))
            }
            "umoja/describe" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = normalize_args(params.get("arguments"));
                let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
                    return Err((-32602, format!("unknown tool: {name}")));
                };
                let (label, risk) = match self.redirect(tool, args.clone()) {
                    Ok((t, a)) => (t.describe(&a), (t.risk)(&a)),
                    // Nothing runs: say what the assistant is really doing.
                    Err(_) => ("Pick the right tool".to_string(), Risk::Low),
                };
                Ok(json!({"label": label, "risk": risk_name(risk)}))
            }
            // Undo a change a tool result offered (`_meta["umoja/undo"]`).
            "umoja/undo" => {
                let id = params.get("id").and_then(Value::as_u64);
                let o = crate::undo::apply(id).await;
                Ok(json!({"ok": !o.is_error, "text": o.text}))
            }
            m if m.starts_with("notifications/") => Ok(Value::Null),
            "resources/list" => Ok(json!({"resources": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }

    /// A `run_command` call that one of our tools does better becomes a
    /// call to that tool (or a hint for the model), so the user is asked
    /// in plain words ("Power off the computer") or not at all.
    fn redirect<'a>(&'a self, tool: &'a Tool, args: Value) -> Result<(&'a Tool, Value), String> {
        if tool.name != "run_command" {
            return Ok((tool, args));
        }
        let cmd = crate::tool::arg_str(&args, "command").unwrap_or("");
        match crate::actions::redirect(cmd) {
            Some(crate::actions::Redirect::Call(name, a)) => match self.tools.iter().find(|t| t.name == name) {
                Some(t) => {
                    tracing::info!(command = cmd, tool = name, "run_command redirected");
                    Ok((t, a))
                }
                None => Ok((tool, args)),
            },
            Some(crate::actions::Redirect::Hint(h)) => Err(h.to_string()),
            None => Ok((tool, args)),
        }
    }

    /// Confirm if needed, then run.
    pub async fn call(self: &Rc<Self>, tool: &Tool, args: Value) -> CallResult {
        let (tool, args) = match self.redirect(tool, args) {
            Ok(v) => v,
            Err(hint) => return CallResult::from(Outcome::err(hint)),
        };
        let risk = (tool.risk)(&args);
        if risk != Risk::Low {
            let detail = match tool.preflight {
                Some(f) => match f(args.clone()).await {
                    Ok(d) => d,
                    Err(e) => return CallResult::from(Outcome::err(e)),
                },
                None => None,
            };
            let label = tool.describe(&args);
            match self.confirm(tool.name, &label, detail.as_deref(), risk).await {
                Ok(true) => {}
                Ok(false) => return CallResult { outcome: Outcome::ok(DECLINED), declined: true, undo: None },
                Err(e) => return CallResult::from(Outcome::err(e)),
            }
        }
        tracing::info!(tool = tool.name, args = %args, dry_run = self.dry_run, "tool call");
        if self.dry_run && !tool.read_only {
            return CallResult::from(Outcome::ok("done"));
        }
        // Note how to put it back before it changes anything.
        let inverse = if tool.read_only || tool.name == "undo" {
            None
        } else {
            crate::undo::capture(tool.name, &args).await
        };
        let label = tool.describe(&args);
        let outcome = (tool.run)(args).await;
        let undo = match inverse {
            Some(inv) if !outcome.is_error => Some((crate::undo::record(&label, inv), label)),
            _ => None,
        };
        CallResult { outcome, declined: false, undo }
    }

    async fn confirm(&self, tool: &str, label: &str, detail: Option<&str>, risk: Risk) -> Result<bool, String> {
        let message = match detail {
            Some(d) if !d.trim().is_empty() => format!("{label}\n\n{}", d.trim()),
            _ => label.to_string(),
        };
        if self.client_caps.borrow().get("elicitation").is_some() {
            let r = self
                .request(
                    "elicitation/create",
                    json!({
                        "message": format!("Allow the assistant to do this?\n\n{message}"),
                        "requestedSchema": {"type": "object", "properties": {}},
                        "_meta": {"umoja/tool": tool, "umoja/label": label, "umoja/detail": detail, "umoja/risk": risk_name(risk)},
                    }),
                )
                .await?;
            return Ok(r["action"] == "accept");
        }
        zenity_confirm(&message, risk).await
    }

    /// Send a request to the client and wait for its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = async_channel::bounded(1);
        self.pending.borrow_mut().insert(id, tx);
        (self.out)(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.recv().await.map_err(|_| "the client went away".to_string())?
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Wait until every request received so far has been answered.
    pub async fn drain(&self) {
        while self.inflight.get() > 0 {
            glib::timeout_future(std::time::Duration::from_millis(20)).await;
        }
    }
}

pub struct CallResult {
    pub outcome: Outcome,
    pub declined: bool,
    /// (journal id, what was done) when the change can be undone.
    pub undo: Option<(u64, String)>,
}

impl From<Outcome> for CallResult {
    fn from(outcome: Outcome) -> Self {
        Self { outcome, declined: false, undo: None }
    }
}

fn result_json(r: CallResult) -> Value {
    let mut v = json!({
        "content": [{"type": "text", "text": r.outcome.text}],
        "isError": r.outcome.is_error,
        "_meta": {"umoja/hide": r.outcome.hide, "umoja/declined": r.declined},
    });
    if let Some((id, label)) = r.undo {
        v["_meta"]["umoja/undo"] = json!({"id": id, "label": label});
    }
    if let Some(d @ Value::Object(_)) = r.outcome.data {
        v["structuredContent"] = d;
    }
    v
}

/// Arguments as an object; some models double-encode them as a string.
pub fn normalize_args(v: Option<&Value>) -> Value {
    match v {
        Some(v @ Value::Object(_)) => v.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({}),
        },
        _ => json!({}),
    }
}

/// Ask with a desktop dialog when the client can't.
async fn zenity_confirm(message: &str, risk: Risk) -> Result<bool, String> {
    let has_display = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if !has_display || glib::find_program_in_path("zenity").is_none() {
        return Err("this needs the user's OK, but this client can't ask (no MCP elicitation) and no dialog can be shown".into());
    }
    let icon = if risk == Risk::High { "dialog-warning" } else { "dialog-question" };
    let text = format!("Allow the assistant to do this?\n\n{message}");
    let words = [
        "zenity", "--question", "--title=Umoja Assistant", "--ok-label=Allow", "--cancel-label=Cancel",
        "--no-markup", "--icon", icon, "--text", text.as_str(),
    ];
    let argv: Vec<&std::ffi::OsStr> = words.iter().map(|s| s.as_ref()).collect();
    let p = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE).map_err(|e| e.to_string())?;
    p.wait_future().await.map_err(|e| e.to_string())?;
    Ok(p.is_successful())
}

/// Serve over stdin/stdout until stdin closes. Logs go to stderr.
pub async fn serve_stdio(tools: Vec<Tool>, dry_run: bool) {
    use std::io::Write;
    let out: Sink = Rc::new(|v: Value| {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    });
    let server = Server::new(tools, dry_run, out.clone());
    // SAFETY: fd 0 stays open for the life of the process.
    let stdin = unsafe { gio::UnixInputStream::with_fd(0) };
    let reader = gio::DataInputStream::new(&stdin);
    loop {
        match reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(msg) => server.receive(msg),
                    Err(e) => out(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "stdin read failed");
                break;
            }
        }
    }
    // Answer what's still running before exiting.
    server.drain().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn tools() -> Vec<Tool> {
        vec![
            Tool::read_only("echo", "Echo", json!({"type": "object"}), |a| {
                Box::pin(async move { Outcome::ok(a.to_string()) })
            }),
            Tool::new("zap", "Zap", json!({"type": "object"}), |_| Box::pin(async { Outcome::ok("zapped").hide() }))
                .confirm()
                .with_summary(|_| "Zap it".into()),
        ]
    }

    /// A client that records what the server sends and answers
    /// elicitation with `accept`.
    fn harness(accept: bool) -> (Rc<Server>, Rc<RefCell<Vec<Value>>>) {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let server_cell: Rc<RefCell<Option<Rc<Server>>>> = Rc::new(RefCell::new(None));
        let (s2, sc) = (sent.clone(), server_cell.clone());
        let out: Sink = Rc::new(move |v: Value| {
            if v["method"] == "elicitation/create" {
                let reply = json!({"jsonrpc": "2.0", "id": v["id"], "result": {"action": if accept {"accept"} else {"decline"}}});
                let server = sc.borrow().clone().unwrap();
                glib::MainContext::ref_thread_default().spawn_local(async move { server.receive(reply) });
            }
            s2.borrow_mut().push(v);
        });
        let server = Server::new(tools(), false, out);
        *server_cell.borrow_mut() = Some(server.clone());
        (server, sent)
    }

    fn roundtrip(server: &Rc<Server>, sent: &Rc<RefCell<Vec<Value>>>, msg: Value) -> Value {
        let id = msg["id"].clone();
        server.receive(msg);
        let ctx = glib::MainContext::ref_thread_default();
        for _ in 0..1000 {
            ctx.iteration(false);
            if let Some(v) = sent.borrow().iter().find(|v| v["id"] == id && v.get("method").is_none()) {
                return v.clone();
            }
        }
        panic!("no reply");
    }

    #[test]
    fn initialize_list_and_call() {
        glib::MainContext::new().with_thread_default(|| initialize_list_and_call_inner()).unwrap();
    }

    fn initialize_list_and_call_inner() {
        let (s, sent) = harness(true);
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": "a", "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {"elicitation": {}}}}));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        assert_eq!(r["result"]["tools"][1]["annotations"]["readOnlyHint"], false);
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "echo", "arguments": "{\"x\":1}"}}));
        assert_eq!(r["result"]["content"][0]["text"], "{\"x\":1}");
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 4, "method": "umoja/describe", "params": {"name": "zap"}}));
        assert_eq!(r["result"], json!({"label": "Zap it", "risk": "medium"}));
    }

    #[test]
    fn risky_calls_are_confirmed_through_the_client() {
        glib::MainContext::new().with_thread_default(|| risky_calls_are_confirmed_through_the_client_inner()).unwrap();
    }

    fn risky_calls_are_confirmed_through_the_client_inner() {
        let (s, sent) = harness(true);
        roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {"elicitation": {}}}}));
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "zap"}}));
        assert_eq!(r["result"]["content"][0]["text"], "zapped");
        assert_eq!(r["result"]["_meta"]["umoja/hide"], true);
        let asked = sent.borrow().iter().find(|v| v["method"] == "elicitation/create").cloned().unwrap();
        assert_eq!(asked["params"]["_meta"]["umoja/risk"], "medium");

        let (s, sent) = harness(false);
        roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {"elicitation": {}}}}));
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "zap"}}));
        assert_eq!(r["result"]["content"][0]["text"], DECLINED);
        assert_eq!(r["result"]["_meta"]["umoja/declined"], true);
    }

    #[test]
    fn unknown_tools_are_protocol_errors() {
        glib::MainContext::new().with_thread_default(|| unknown_tools_are_protocol_errors_inner()).unwrap();
    }

    fn unknown_tools_are_protocol_errors_inner() {
        let (s, sent) = harness(true);
        let r = roundtrip(&s, &sent, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "rm_rf"}}));
        assert_eq!(r["error"]["code"], -32602);
    }
}

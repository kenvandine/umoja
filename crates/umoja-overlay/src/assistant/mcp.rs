//! The overlay's client for `umoja-mcp`, the Umoja MCP server that holds
//! the desktop tools (apps, settings, windows, files, notifications,
//! troubleshooting) and their confirm rules.
//!
//! The server runs as a child process speaking MCP (JSON-RPC, one message
//! per line) on its stdin/stdout. It is started on the first question and
//! kept running; if it exits it is started again on the next one. When
//! the server wants the user's OK for a risky tool it sends an
//! `elicitation/create` request, which [`McpClient::call`] answers with
//! the overlay's own "Allow the assistant to do this?" dialog.

use super::tools::Risk;
use gio::prelude::*;
use gtk4::glib;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;

/// Asks the user; resolves to their answer.
pub type Confirm = Rc<dyn Fn(String, Risk) -> Pin<Box<dyn Future<Output = bool>>>>;

/// What a tool call produced.
#[derive(Debug, Clone, Default)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
    pub declined: bool,
    /// The tool put something in front of the user: hide when done.
    pub hide: bool,
    pub data: Option<Value>,
}

pub struct McpClient {
    proc: gio::Subprocess,
    writer: async_channel::Sender<String>,
    pending: RefCell<HashMap<i64, async_channel::Sender<Result<Value, String>>>>,
    next_id: Cell<i64>,
    /// MCP `tools/list` entries.
    tools: RefCell<Vec<Value>>,
    /// Answers the server's confirmation requests for the current call.
    confirm: RefCell<Option<Confirm>>,
    alive: Cell<bool>,
}

/// Where the server binary is: `$UMOJA_MCP`, next to this executable (or
/// one level up, for test binaries in target/*/deps), else on PATH.
fn binary() -> PathBuf {
    if let Some(p) = std::env::var_os("UMOJA_MCP") {
        return p.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1).take(2) {
            let p = dir.join("umoja-mcp");
            if p.is_file() {
                return p;
            }
        }
    }
    "umoja-mcp".into()
}

fn risk_from(s: &str) -> Risk {
    match s {
        "low" => Risk::Low,
        "high" => Risk::High,
        _ => Risk::Medium,
    }
}

impl McpClient {
    /// Start `umoja-mcp` with `args` and complete the MCP handshake.
    pub async fn start(args: &[&str]) -> Result<Rc<Self>, String> {
        let bin = binary();
        let mut argv: Vec<&std::ffi::OsStr> = vec![bin.as_os_str()];
        argv.extend(args.iter().map(|a| std::ffi::OsStr::new(*a)));
        let proc = gio::Subprocess::newv(
            &argv,
            gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE,
        )
        .map_err(|e| format!("couldn't start {}: {e}", bin.display()))?;
        let stdin = proc.stdin_pipe().ok_or("no stdin")?;
        let stdout = proc.stdout_pipe().ok_or("no stdout")?;

        let (writer, lines) = async_channel::unbounded::<String>();
        let client = Rc::new(Self {
            proc,
            writer,
            pending: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            tools: RefCell::new(Vec::new()),
            confirm: RefCell::new(None),
            alive: Cell::new(true),
        });

        // One writer, so writes never overlap.
        glib::spawn_future_local(async move {
            while let Ok(mut line) = lines.recv().await {
                line.push('\n');
                if stdin
                    .write_all_future(line.into_bytes(), glib::Priority::DEFAULT)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        // Reader: responses resolve pending requests; requests from the
        // server (confirmation) are answered.
        let weak = Rc::downgrade(&client);
        let reader = gio::DataInputStream::new(&stdout);
        glib::spawn_future_local(async move {
            loop {
                let line = reader.read_line_utf8_future(glib::Priority::DEFAULT).await;
                let Some(c) = weak.upgrade() else { return };
                match line {
                    Ok(Some(l)) => {
                        if let Ok(msg) = serde_json::from_str::<Value>(l.trim()) {
                            c.receive(msg);
                        }
                    }
                    _ => {
                        c.died();
                        return;
                    }
                }
            }
        });

        let handshake = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"elicitation": {}},
                "clientInfo": {"name": "umoja-overlay", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
        let init = glib::future_with_timeout(std::time::Duration::from_secs(10), handshake)
            .await
            .map_err(|_| "umoja-mcp didn't answer".to_string())??;
        tracing::info!(server = %init["serverInfo"]["name"], version = %init["protocolVersion"], "connected to umoja-mcp");
        client.notify("notifications/initialized", json!({}));
        let list = client.request("tools/list", json!({})).await?;
        *client.tools.borrow_mut() = list["tools"].as_array().cloned().unwrap_or_default();
        Ok(client)
    }

    fn died(&self) {
        if self.alive.replace(false) {
            tracing::warn!("umoja-mcp exited");
        }
        for (_, tx) in self.pending.borrow_mut().drain() {
            let _ = tx.try_send(Err("the Umoja tool server stopped".into()));
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.get()
    }

    fn send(&self, v: Value) {
        let _ = self.writer.try_send(v.to_string());
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if !self.alive.get() {
            return Err("the Umoja tool server stopped".into());
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = async_channel::bounded(1);
        self.pending.borrow_mut().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.recv()
            .await
            .map_err(|_| "the Umoja tool server stopped".to_string())?
    }

    fn receive(self: &Rc<Self>, msg: Value) {
        match msg.get("method").and_then(Value::as_str) {
            None => {
                let Some(id) = msg.get("id").and_then(Value::as_i64) else { return };
                if let Some(tx) = self.pending.borrow_mut().remove(&id) {
                    let r = match msg.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.try_send(r);
                }
            }
            Some("elicitation/create") => {
                let id = msg["id"].clone();
                let p = &msg["params"];
                let meta = &p["_meta"];
                // Prefer the plain label and detail over the full message.
                let text = match (meta["umoja/label"].as_str(), meta["umoja/detail"].as_str()) {
                    (Some(l), Some(d)) if !d.trim().is_empty() => format!("{l}\n\n{}", d.trim()),
                    (Some(l), _) => l.to_string(),
                    _ => p["message"].as_str().unwrap_or("").to_string(),
                };
                let risk = risk_from(meta["umoja/risk"].as_str().unwrap_or("medium"));
                let confirm = self.confirm.borrow().clone();
                let this = self.clone();
                glib::spawn_future_local(async move {
                    let ok = match confirm {
                        Some(f) => f(text, risk).await,
                        None => false,
                    };
                    let action = if ok { "accept" } else { "decline" };
                    this.send(json!({"jsonrpc": "2.0", "id": id, "result": {"action": action, "content": {}}}));
                });
            }
            Some("ping") => self.send(json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}})),
            Some(_) if msg.get("id").is_some() => self.send(json!({
                "jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "method not found"},
            })),
            Some(_) => {}
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.borrow().iter().any(|t| t["name"] == name)
    }

    /// The tools as OpenAI `tools` entries.
    pub fn openai_schemas(&self) -> Vec<Value> {
        self.tools
            .borrow()
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t["name"],
                    "description": t["description"],
                    "parameters": t.get("inputSchema").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                }})
            })
            .collect()
    }

    /// The progress line and risk for a call.
    pub async fn describe(&self, name: &str, args: &Value) -> (String, Risk) {
        match self
            .request("umoja/describe", json!({"name": name, "arguments": args}))
            .await
        {
            Ok(v) => (
                v["label"].as_str().unwrap_or(name).to_string(),
                risk_from(v["risk"].as_str().unwrap_or("low")),
            ),
            Err(_) => (name.replace('_', " "), Risk::Low),
        }
    }

    pub async fn call(&self, name: &str, args: Value, confirm: Confirm) -> CallResult {
        *self.confirm.borrow_mut() = Some(confirm);
        let r = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await;
        *self.confirm.borrow_mut() = None;
        match r {
            Ok(v) => CallResult {
                text: v["content"]
                    .as_array()
                    .map(|c| {
                        c.iter()
                            .filter_map(|i| i["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
                is_error: v["isError"].as_bool().unwrap_or(false),
                declined: v["_meta"]["umoja/declined"].as_bool().unwrap_or(false),
                hide: v["_meta"]["umoja/hide"].as_bool().unwrap_or(false),
                data: v.get("structuredContent").cloned(),
            },
            Err(e) => CallResult {
                text: format!("error: {e}"),
                is_error: true,
                ..Default::default()
            },
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.proc.force_exit();
    }
}

thread_local! {
    static SHARED: RefCell<Option<Rc<McpClient>>> = const { RefCell::new(None) };
}

/// The overlay's server connection, started (or restarted) as needed.
/// `None` when umoja-mcp can't be started; the assistant then only has
/// the overlay's own tools.
pub async fn shared() -> Option<Rc<McpClient>> {
    if let Some(c) = SHARED.with(|s| s.borrow().clone()).filter(|c| c.is_alive()) {
        stop_when_idle();
        return Some(c);
    }
    match McpClient::start(&[]).await {
        Ok(c) => {
            SHARED.with(|s| *s.borrow_mut() = Some(c.clone()));
            stop_when_idle();
            Some(c)
        }
        Err(e) => {
            tracing::warn!(error = %e, "umoja-mcp isn't available; desktop tools are off");
            None
        }
    }
}

/// The server is stopped after this long without a question (it starts
/// again with the next one, in well under a second).
const IDLE_STOP: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// (Re)arm the idle stop: once nothing has asked for the server for
/// [`IDLE_STOP`] and no answer still holds it, let it go.
fn stop_when_idle() {
    thread_local! {
        static TIMER: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
    }
    TIMER.with(|t| {
        if let Some(old) = t.borrow_mut().take() {
            old.remove();
        }
        let id = glib::timeout_add_local_once(IDLE_STOP, || {
            TIMER.with(|t| t.borrow_mut().take());
            let idle = SHARED.with(|s| {
                s.borrow()
                    .as_ref()
                    .is_some_and(|c| Rc::strong_count(c) == 1 && c.pending.borrow().is_empty())
            });
            if idle {
                tracing::info!("stopping the idle umoja-mcp");
                // Dropping the last reference stops the process.
                SHARED.with(|s| s.borrow_mut().take());
            } else {
                stop_when_idle();
            }
        });
        *t.borrow_mut() = Some(id);
    });
}

//! Talking to the agent inside its sandbox: the Umoja agent bridge
//! protocol (v1). Every agent image ships `umoja-agent-bridge`; this file
//! is all umoja-agentd knows about how an agent works, so a new agent
//! (OpenClaw, PicoClaw, ...) is an image that implements the verbs below,
//! not new daemon code. See agents/README.md.
//!
//! The workload container has no network card, so nothing on the host can
//! open a connection to the agent. Every call runs the bridge in the
//! sandbox through the OpenShell gateway's exec API (under the sandbox's
//! policy, like any agent command); the bridge prints each answer as one
//! `@@{json}` line. A run's event stream is one exec that keeps printing.
//!
//!   health                 @@{"ok": bool}
//!   restart                restart the agent so it reads agent.env again
//!   submit                 $UMOJA_BODY {"input","instructions","session"}
//!                          -> @@{"run": id, "session": id?}
//!   status RUN             -> @@{"status": running|waiting|completed|failed|
//!                          cancelled|gone, "output", "error", "session",
//!                          "pending": [{"kind": approval|question, "id",
//!                          "generation", "text", "detail"}]}
//!   events RUN             @@{"event": name, "data": {...}} until the run
//!                          ends: tool.started {tool, preview},
//!                          tool.completed {tool, preview, error},
//!                          message.delta, run.completed/failed/cancelled
//!   approve                $UMOJA_BODY {"run","id","generation","allow"}
//!   answer                 $UMOJA_BODY {"run","id","generation","text"}
//!   stop RUN
//!   write                  $UMOJA_BODY {"path","hex","mode"}
//!
//! Settings reach the agent as /sandbox/.config/umoja/agent.env (see
//! Agent::write_config); the agent's start script reads it.

use anyhow::{Result, anyhow, bail};
use openshell_gateway_client::Gateway;
use openshell_gateway_client::{ExecOptions, ExecOutput};
use serde_json::{Value, json};

const BRIDGE: &str = "/usr/local/bin/umoja-agent-bridge";
pub const ENV_FILE: &str = "/sandbox/.config/umoja/agent.env";

/// What the agent is told about where it runs, with every run.
pub fn instructions(name: &str) -> String {
    format!(
        "You are {name}, running for the user inside a sandbox on their computer, set up by the \
Umoja desktop. Save every file you make in /sandbox/workspace (the user sees it as ~/Agents/{name}) \
unless they name another shared folder under /sandbox/shared. You cannot see or control the \
user's desktop, apps or other files. Network access is limited: when a site is blocked the user \
is asked, so wait and retry once rather than giving up. Keep your final answer short and say \
which files you wrote."
    )
}

/// Runs the bridge with `args` and feeds every `@@` line to `on_line`.
/// Returns the bridge's exit status.
pub async fn bridge(
    gw: &Gateway,
    args: &[&str],
    body: Option<&Value>,
    mut on_line: impl FnMut(Value),
) -> Result<i32> {
    let mut gw = gw.clone();
    let mut command = vec![BRIDGE.to_owned()];
    command.extend(args.iter().map(|a| (*a).to_owned()));
    let mut environment = vec![("PYTHONUNBUFFERED".to_owned(), "1".to_owned())];
    if let Some(body) = body {
        environment.push(("UMOJA_BODY".into(), body.to_string()));
    }
    let (_input, mut output) = gw
        .exec_interactive(
            &crate::paths::sandbox(),
            ExecOptions {
                command,
                workdir: String::new(),
                environment,
                cols: 1000,
                rows: 40,
            },
        )
        .await
        .map_err(|e| anyhow!("sandbox exec: {e}"))?;
    let mut buffer = Vec::<u8>::new();
    loop {
        match output.next().await.map_err(|e| anyhow!("sandbox exec: {e}"))? {
            Some(ExecOutput::Data(data)) => {
                buffer.extend_from_slice(&data);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    if let Some(v) = parse_line(&line) {
                        on_line(v);
                    }
                }
            }
            Some(ExecOutput::Exit(code)) => {
                if let Some(v) = parse_line(&buffer) {
                    on_line(v);
                }
                return Ok(code);
            }
            None => {
                if let Some(v) = parse_line(&buffer) {
                    on_line(v);
                }
                bail!("the sandbox closed the command without an exit status");
            }
        }
    }
}

/// `@@{...}` → the JSON; anything else (TTY noise, Python warnings) → None.
pub fn parse_line(line: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_matches(|c: char| c == '\r' || c == '\n' || c.is_whitespace());
    let json = text.find("@@").map(|i| &text[i + 2..])?;
    serde_json::from_str(json).ok()
}

/// One request/response call: the bridge's single `@@` line.
pub async fn call(gw: &Gateway, args: &[&str], body: Option<&Value>) -> Result<Value> {
    let mut answer = Value::Null;
    let code = bridge(gw, args, body, |v| answer = v).await?;
    if let Some(err) = answer.get("error").and_then(Value::as_str) {
        bail!("{err}");
    }
    if code != 0 && answer.is_null() {
        bail!("the agent bridge exited with status {code}");
    }
    Ok(answer)
}

pub async fn health(gw: &Gateway) -> bool {
    call(gw, &["health"], None)
        .await
        .map(|v| v["ok"] == true)
        .unwrap_or(false)
}

/// Starts a run, in `session` (the agent's conversation) when given.
/// Returns (run id, session id).
pub async fn submit(gw: &Gateway, input: &str, instructions: &str, session: Option<&str>) -> Result<(String, String)> {
    let v = call(
        gw,
        &["submit"],
        Some(&json!({ "input": input, "instructions": instructions, "session": session.unwrap_or_default() })),
    )
    .await?;
    let run = v["run"]
        .as_str()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("the agent didn't start the run: {v}"))?;
    Ok((run.to_owned(), v["session"].as_str().unwrap_or_default().to_owned()))
}

/// A run, as the bridge reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunStatus {
    /// running, waiting, completed, failed, cancelled or gone.
    pub status: String,
    pub output: String,
    pub error: String,
    pub session: String,
    pub pending: Vec<Pending>,
}

impl RunStatus {
    pub fn parse(v: &Value) -> Self {
        let s = |k: &str| v[k].as_str().unwrap_or_default().trim().to_owned();
        Self {
            status: s("status"),
            output: s("output"),
            error: s("error"),
            session: s("session"),
            pending: v["pending"]
                .as_array()
                .map(|a| a.iter().filter_map(Pending::parse).collect())
                .unwrap_or_default(),
        }
    }
}

pub async fn status(gw: &Gateway, run: &str) -> Result<RunStatus> {
    Ok(RunStatus::parse(&call(gw, &["status", run], None).await?))
}

pub async fn stop(gw: &Gateway, run: &str) -> Result<()> {
    call(gw, &["stop", run], None).await?;
    Ok(())
}

pub async fn approve(gw: &Gateway, run: &str, id: &str, generation: &str, allow: bool) -> Result<()> {
    call(gw, &["approve"], Some(&json!({ "run": run, "id": id, "generation": generation, "allow": allow }))).await?;
    Ok(())
}

pub async fn answer(gw: &Gateway, run: &str, id: &str, generation: &str, text: &str) -> Result<()> {
    call(gw, &["answer"], Some(&json!({ "run": run, "id": id, "generation": generation, "text": text }))).await?;
    Ok(())
}

/// Writes `content` to `path` inside the sandbox (as the agent's user, so
/// only where the policy lets it write).
pub async fn write_file(gw: &Gateway, path: &str, content: &str, mode: u32) -> Result<()> {
    use std::fmt::Write;
    let mut hex = String::with_capacity(content.len() * 2);
    for b in content.as_bytes() {
        let _ = write!(hex, "{b:02x}");
    }
    call(gw, &["write"], Some(&json!({ "path": path, "hex": hex, "mode": mode }))).await?;
    Ok(())
}

/// Restarts the agent (its start script runs it in a loop), so new
/// settings take effect.
pub async fn restart(gw: &Gateway) -> Result<()> {
    call(gw, &["restart"], None).await?;
    Ok(())
}

/// Something a run waits on: a command to approve or a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub kind: PendingKind,
    pub request_id: String,
    pub generation: String,
    /// The command, or the question.
    pub text: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Approval,
    Clarify,
}

impl Pending {
    fn parse(v: &Value) -> Option<Self> {
        let s = |k: &str| match &v[k] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        let id = s("id");
        if id.is_empty() {
            return None;
        }
        Some(Self {
            kind: if s("kind") == "question" { PendingKind::Clarify } else { PendingKind::Approval },
            request_id: id,
            generation: s("generation"),
            text: s("text"),
            detail: s("detail"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_lines() {
        assert_eq!(parse_line(b"@@{\"ok\":true}\r\n"), Some(json!({"ok": true})));
        assert_eq!(parse_line(b"warning: something\r\n"), None);
        assert_eq!(parse_line(b"\x1b[0m@@{\"a\":1}"), Some(json!({"a": 1})));
    }

    #[test]
    fn run_status() {
        let r = RunStatus::parse(&json!({
            "status": "waiting", "session": "s1", "output": " partial ",
            "pending": [
                {"kind": "approval", "id": "p1", "generation": 3, "text": "rm -rf build", "detail": "recursive delete"},
                {"kind": "question", "id": "q", "text": "Which account?"},
                {"kind": "approval"}
            ]
        }));
        assert_eq!((r.status.as_str(), r.session.as_str(), r.output.as_str()), ("waiting", "s1", "partial"));
        assert_eq!(r.pending.len(), 2);
        assert_eq!(r.pending[0].kind, PendingKind::Approval);
        assert_eq!((r.pending[0].request_id.as_str(), r.pending[0].generation.as_str()), ("p1", "3"));
        assert_eq!(r.pending[1].kind, PendingKind::Clarify);
        assert_eq!(RunStatus::parse(&json!({"status": "gone"})).status, "gone");
    }
}

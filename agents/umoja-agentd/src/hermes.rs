//! Talking to Hermes inside the sandbox.
//!
//! The workload container has no network card, so nothing on the host can
//! open a connection to Hermes's API server (127.0.0.1:8642 inside). Instead
//! every call runs `umoja-hermes-bridge` in the sandbox through the
//! OpenShell gateway's exec API (under the sandbox's policy, like any agent
//! command); the bridge makes the HTTP request on the sandbox's loopback and
//! prints the answer as `@@{json}` lines. Long-lived calls (a run's event
//! stream) are one exec that keeps printing.

use anyhow::{Result, anyhow, bail};
use openshell_gateway_client::Gateway;
use openshell_gateway_client::{ExecOptions, ExecOutput};
use serde_json::{Value, json};

use crate::paths::SANDBOX;

const BRIDGE: &str = "/usr/local/bin/umoja-hermes-bridge";

/// What Hermes is told about where it runs, with every run.
pub const INSTRUCTIONS: &str = "You are Hermes, running for the user inside Umoja's OpenShell \
sandbox on their computer. Save every file you make in /sandbox/workspace (the user sees it as \
~/Agents/Hermes) unless they name another shared folder under /sandbox/shared. You cannot see \
or control the user's desktop, apps or other files. Network access is limited: when a site is \
blocked the user is asked, so wait and retry once rather than giving up. Keep your final answer \
short and say which files you wrote.";

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
            SANDBOX,
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
        bail!("the Hermes bridge exited with status {code}");
    }
    Ok(answer)
}

pub async fn health(gw: &Gateway) -> bool {
    call(gw, &["health"], None)
        .await
        .map(|v| v["ok"] == true)
        .unwrap_or(false)
}

pub async fn submit(gw: &Gateway, input: &str) -> Result<String> {
    let v = call(
        gw,
        &["post", "/v1/runs"],
        Some(&json!({ "input": input, "instructions": INSTRUCTIONS })),
    )
    .await?;
    let body = &v["body"];
    body["run_id"]
        .as_str()
        .or_else(|| body["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Hermes didn't start the run: {body}"))
}

pub async fn run(gw: &Gateway, run_id: &str) -> Result<Value> {
    Ok(call(gw, &["get", &format!("/v1/runs/{run_id}")], None).await?["body"].clone())
}

pub async fn stop(gw: &Gateway, run_id: &str) -> Result<()> {
    call(gw, &["post", &format!("/v1/runs/{run_id}/stop")], Some(&json!({}))).await?;
    Ok(())
}

pub async fn approve(gw: &Gateway, run_id: &str, request_id: &str, generation: &str, allow: bool) -> Result<()> {
    let generation: Value = generation
        .parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(generation));
    let v = call(
        gw,
        &["post", &format!("/v1/runs/{run_id}/approval")],
        Some(&json!({
            "request_id": request_id,
            "execution_generation": generation,
            "choice": if allow { "once" } else { "deny" },
        })),
    )
    .await?;
    check_status(&v)
}

pub async fn clarify(gw: &Gateway, run_id: &str, request_id: &str, generation: &str, answer: &str) -> Result<()> {
    let generation: Value = generation
        .parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(generation));
    let v = call(
        gw,
        &["post", &format!("/v1/runs/{run_id}/clarify")],
        Some(&json!({
            "request_id": request_id,
            "execution_generation": generation,
            "answer": answer,
        })),
    )
    .await?;
    check_status(&v)
}

fn check_status(v: &Value) -> Result<()> {
    match v["status"].as_u64() {
        Some(s) if (200..300).contains(&s) => Ok(()),
        Some(s) => bail!("Hermes answered {s}: {}", v["body"]),
        None => Ok(()),
    }
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

/// Restarts `hermes gateway` (umoja-hermes-start runs it in a loop), so a
/// new config takes effect.
pub async fn restart(gw: &Gateway) -> Result<()> {
    call(gw, &["restart"], None).await?;
    Ok(())
}

/// A Hermes run's pending control (approval or clarify) from GET /v1/runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub kind: PendingKind,
    pub request_id: String,
    pub generation: String,
    /// The command or question, as Hermes describes it.
    pub text: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Approval,
    Clarify,
}

/// Reads `pending_controls` (a list or a map) from a run. Field names are
/// matched loosely: Hermes's docs name prompt_id/request_id and
/// execution_generation, and describe the command or question.
pub fn pending_controls(run: &Value) -> Vec<Pending> {
    let controls = &run["pending_controls"];
    let items: Vec<&Value> = match controls {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().flat_map(|v| match v {
            Value::Array(a) => a.iter().collect::<Vec<_>>(),
            other => vec![other],
        }).collect(),
        _ => Vec::new(),
    };
    let generation_top = run["execution_generation"].clone();
    items
        .into_iter()
        .filter_map(|c| {
            let request_id = str_of(c, &["prompt_id", "request_id", "id"])?;
            let kind_text = str_of(c, &["kind", "type", "control"]).unwrap_or_default();
            let kind = if kind_text.contains("clarif") || c.get("question").is_some() {
                PendingKind::Clarify
            } else {
                PendingKind::Approval
            };
            let generation = c
                .get("execution_generation")
                .or_else(|| c.get("generation"))
                .unwrap_or(&generation_top);
            let generation = match generation {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            };
            let text = str_of(c, &["command", "question", "prompt", "title", "summary", "tool"])
                .or_else(|| c.get("arguments").map(|a| a.to_string()))
                .unwrap_or_default();
            let detail = str_of(c, &["description", "reason", "explanation", "risk", "message"])
                .unwrap_or_default();
            Some(Pending { kind, request_id, generation, text, detail })
        })
        .collect()
}

fn str_of(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The text of a finished run's answer.
pub fn output_text(run: &Value) -> String {
    match &run["output"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| {
                i["text"].as_str().map(str::to_owned).or_else(|| {
                    i["content"].as_array().map(|c| {
                        c.iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(o) => o
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
    .trim()
    .to_owned()
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
    fn controls_list_or_map() {
        let run = json!({
            "status": "waiting_for_approval",
            "pending_controls": [{
                "kind": "approval", "prompt_id": "p1", "execution_generation": 3,
                "command": "rm -rf build", "description": "recursive delete"
            }]
        });
        let p = pending_controls(&run);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, PendingKind::Approval);
        assert_eq!((p[0].request_id.as_str(), p[0].generation.as_str()), ("p1", "3"));
        assert_eq!(p[0].text, "rm -rf build");
        let run = json!({
            "execution_generation": 7,
            "pending_controls": {"clarify": {"request_id": "q", "question": "Which account?"}}
        });
        let p = pending_controls(&run);
        assert_eq!(p[0].kind, PendingKind::Clarify);
        assert_eq!(p[0].generation, "7");
        assert_eq!(p[0].text, "Which account?");
    }

    #[test]
    fn outputs() {
        assert_eq!(output_text(&json!({"output": " hi "})), "hi");
        assert_eq!(
            output_text(&json!({"output": [{"content": [{"text": "a"}, {"text": "b"}]}]})),
            "ab"
        );
    }
}

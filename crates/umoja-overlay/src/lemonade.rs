//! Client for Umoja's embedded Lemonade server.
//!
//! Talks to the `umoja-lemonade` user service, which runs the `lemond`
//! binary shipped inside the umoja-desktop package on a private port
//! (13345 by default, see `umoja_common::lemonade`). It never touches a
//! system `lemonade-server` on the upstream default port 13305.

use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;
use umoja_common::assistant_backend::{self as backend, Endpoint};
use umoja_common::lemonade;

/// Shared Tokio runtime for network I/O; GTK stays on its own main loop.
pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("umoja-overlay-io")
        .build()
        .expect("tokio runtime")
});

pub const DEFAULT_MODEL: &str = "Qwen3-4B-Instruct-2507-GGUF";

const SYSTEM_PROMPT: &str = "You are Umoja OS Assistant, built into an Ubuntu desktop. \
Answer in at most four short sentences of practical guidance. When the user's request maps \
to a desktop action, also return one or more JSON objects, each on its own line, using only \
these shapes:\n\
{\"action\": \"open_settings\", \"panel\": \"<gnome-control-center panel>\", \"subpage\": \"<optional>\"}\n\
{\"action\": \"run_command\", \"cmd\": \"<shell command>\", \"risk\": \"low|medium|high\"}\n\
{\"action\": \"launch_app\", \"app_id\": \"<desktop file id>\"}\n\
{\"action\": \"set_setting\", \"schema\": \"org.gnome...\", \"key\": \"<key>\", \"value\": <json>}\n\
Mark anything that deletes data, needs sudo or changes the system as medium or high risk.";

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct CompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
}

// Non-streaming response types, used by `query_guidance`.
#[allow(dead_code)]
#[derive(Deserialize)]
pub struct ChatChoice {
    pub message: ResponseMessage,
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub struct ResponseMessage {
    pub content: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub struct CompletionResponse {
    pub choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

/// One tool call requested by the model (OpenAI `tool_calls` format).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The raw JSON arguments string, as the model wrote it.
    pub arguments: String,
}

/// Events from [`LemonadeClient::stream_chat`].
#[derive(Debug)]
pub enum ChatEvent {
    Text(String),
    /// Sent once, just before `Done`, when the model asked for tools.
    ToolCalls(Vec<ToolCall>),
    Done,
    Error(String),
}

#[derive(Deserialize, Default)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    Done,
    Error(String),
}

/// Chat client for the assistant. Despite the name it talks to whichever
/// backend is selected (see `umoja_common::assistant_backend`): the embedded
/// Lemonade by default, or the user's OpenAI-compatible cloud endpoint.
#[derive(Clone)]
pub struct LemonadeClient {
    client: Client,
    endpoint: String,
    #[allow(dead_code)]
    health: String,
    model: String,
    backend: Endpoint,
    /// Key to use instead of the keyring's (the settings "Test" button).
    token: Option<String>,
}

impl LemonadeClient {
    /// A client for the backend selected right now. Reads GSettings, so
    /// create it on the main thread, once per question.
    pub fn new() -> Self {
        Self::for_endpoint(backend::current(), None)
    }

    pub fn for_endpoint(backend: Endpoint, token: Option<String>) -> Self {
        let connect = if backend.cloud { 10 } else { 2 };
        Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(connect))
                .build()
                .unwrap_or_default(),
            endpoint: backend.chat_url.clone(),
            health: lemonade::health_url(),
            model: backend.model.clone(),
            backend,
            token,
        }
    }

    /// True when requests go to a cloud endpoint.
    #[allow(dead_code)]
    pub fn is_cloud(&self) -> bool {
        self.backend.cloud
    }

    /// POST `body` as JSON, with the cloud key when there is one. A strict
    /// provider's 400 about a parameter gets one retry without it. Errors
    /// come back as a sentence for the user.
    async fn post(&self, mut body: serde_json::Value) -> Result<reqwest::Response, String> {
        let token = if !self.backend.cloud {
            None
        } else if let Some(t) = &self.token {
            Some(t.clone())
        } else {
            tokio::task::spawn_blocking(backend::token::lookup)
                .await
                .ok()
                .flatten()
        };
        let token = token.map(|t| backend::clean_key(&t));
        if !self.backend.cloud {
            lemonade::idle::note_chat_use(&self.model);
        }
        if self.backend.cloud {
            let shape = token.as_deref().map(backend::key_shape);
            tracing::info!(url = %self.endpoint, key = ?shape, "cloud chat request");
        }
        let mut retried = false;
        loop {
            let mut req = self.client.post(&self.endpoint).json(&body);
            if let Some(t) = token.as_deref().filter(|t| !t.is_empty()) {
                req = req.bearer_auth(t);
            }
            if self.backend.cloud {
                // OpenRouter's app attribution; other servers ignore it.
                req = req.header("X-Title", "Umoja Desktop");
            }
            let resp = req.send().await.map_err(|e| self.describe(&e))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            let text = resp.text().await.unwrap_or_default();
            let err = backend::provider_error(&text).unwrap_or_default();
            if !retried && status.as_u16() == 400 && backend::adapt_request(&mut body, &err) {
                tracing::info!(%err, "retrying the chat request without the rejected parameter");
                retried = true;
                continue;
            }
            tracing::warn!(status = status.as_u16(), body = %text, "chat request failed");
            return Err(if self.backend.cloud {
                backend::describe_status(&self.backend, status.as_u16(), &text)
            } else if status.as_u16() == 404 {
                "The assistant's model isn't downloaded yet.".into()
            } else {
                format!("The local assistant failed ({status}). {err}")
                    .trim()
                    .to_string()
            });
        }
    }

    fn describe(&self, e: &reqwest::Error) -> String {
        if !self.backend.cloud {
            return describe(e);
        }
        describe_cloud(&self.backend.host(), e)
    }

    fn request(&self, user_query: &str, stream: bool) -> CompletionRequest {
        CompletionRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: SYSTEM_PROMPT.into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: user_query.into(),
                },
            ],
            temperature: 0.2,
            stream,
        }
    }

    #[allow(dead_code)]
    pub async fn is_available(&self) -> bool {
        matches!(self.client.get(&self.health).send().await, Ok(r) if r.status().is_success())
    }

    /// One-shot completion (the blueprint's original API).
    #[allow(dead_code)]
    pub async fn query_guidance(&self, user_query: &str) -> Result<String, reqwest::Error> {
        let res = self
            .client
            .post(&self.endpoint)
            .json(&self.request(user_query, false))
            .send()
            .await?
            .error_for_status()?
            .json::<CompletionResponse>()
            .await?;
        Ok(res
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default())
    }

    /// Stream tokens to `tx` as they arrive (OpenAI SSE format), without
    /// tools. The overlay now goes through `assistant::agent`.
    #[allow(dead_code)]
    pub async fn stream_guidance(&self, user_query: &str, tx: async_channel::Sender<StreamEvent>) {
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&self.request(user_query, true))
            .send()
            .await;
        let resp = match resp.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(describe(&e))).await;
                return;
            }
        };
        let mut body = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            let Ok(chunk) = chunk else {
                let _ = tx.send(StreamEvent::Error("connection lost".into())).await;
                return;
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                match parse_sse_line(&line) {
                    Some(SseLine::Delta(d)) => {
                        if tx.send(StreamEvent::Delta(d)).await.is_err() {
                            return; // UI went away (query changed)
                        }
                    }
                    Some(SseLine::Done) => {
                        let _ = tx.send(StreamEvent::Done).await;
                        return;
                    }
                    None => {}
                }
            }
        }
        let _ = tx.send(StreamEvent::Done).await;
    }
}

impl LemonadeClient {
    /// Model name requests are sent with.
    #[allow(dead_code)]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Stream a chat with OpenAI-style tool calling. `messages` and `tools`
    /// are sent as-is (see `assistant::agent`). Text arrives as
    /// [`ChatEvent::Text`]; tool calls are assembled from their streamed
    /// fragments and sent as one [`ChatEvent::ToolCalls`] before `Done`.
    pub async fn stream_chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Vec<serde_json::Value>,
        tx: async_channel::Sender<ChatEvent>,
    ) {
        let mut messages = messages;
        if self.backend.cloud {
            backend::strip_local_only_fields(&mut messages);
        }
        // The idle-unload countdown starts when the answer ends, however it ends.
        let _used = (!self.backend.cloud).then(|| UseMark(self.model.clone()));
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.2,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        let resp = match self.post(body).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(ChatEvent::Error(e)).await;
                return;
            }
        };
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut calls = CallAssembler::default();
        let mut finished = false;
        let finish = |calls: CallAssembler| {
            let tx = tx.clone();
            async move {
                let calls = calls.finish();
                if !calls.is_empty() {
                    let _ = tx.send(ChatEvent::ToolCalls(calls)).await;
                }
                let _ = tx.send(ChatEvent::Done).await;
            }
        };
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                let _ = tx.send(ChatEvent::Error("connection lost".into())).await;
                return;
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data == "[DONE]" {
                    finish(calls).await;
                    return;
                }
                let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                    continue;
                };
                for choice in chunk.choices {
                    finished |= choice.finish_reason.is_some();
                    calls.add(choice.delta.tool_calls);
                    if let Some(text) = choice.delta.content.filter(|t| !t.is_empty())
                        && tx.send(ChatEvent::Text(text)).await.is_err()
                    {
                        return; // UI went away
                    }
                }
            }
        }
        // A body that ends without [DONE] or a finish_reason was cut off
        // (e.g. the service restarted): don't pass a fragment off as the answer.
        if finished {
            finish(calls).await;
        } else {
            let _ = tx
                .send(ChatEvent::Error(
                    "The assistant stopped before finishing. Try again.".into(),
                ))
                .await;
        }
    }
}

/// Marks the local chat model as used when dropped (see `lemonade::idle`).
struct UseMark(String);

impl Drop for UseMark {
    fn drop(&mut self) {
        lemonade::idle::note_chat_use(&self.0);
    }
}

/// Joins streamed `tool_calls` fragments (keyed by `index`) into calls.
#[derive(Default)]
struct CallAssembler {
    calls: Vec<ToolCall>,
}

impl CallAssembler {
    fn add(&mut self, deltas: Vec<ToolCallDelta>) {
        for d in deltas {
            let i = d.index.unwrap_or(self.calls.len().saturating_sub(1));
            while self.calls.len() <= i {
                self.calls.push(ToolCall::default());
            }
            let c = &mut self.calls[i];
            if let Some(id) = d.id.filter(|s| !s.is_empty()) {
                c.id = id;
            }
            if let Some(f) = d.function {
                if let Some(n) = f.name {
                    c.name.push_str(&n);
                }
                if let Some(a) = f.arguments {
                    c.arguments.push_str(&a);
                }
            }
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, mut c)| {
                if c.id.is_empty() {
                    c.id = format!("call_{i}");
                }
                c
            })
            .collect()
    }
}

/// A sentence about a failed cloud request, with the underlying cause
/// (DNS, TLS, refused…) so a wrong URL and a network problem look different.
pub fn describe_cloud(host: &str, e: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    let cause = cause.to_string();
    let detail = if cause.is_empty() || cause == e.to_string() {
        String::new()
    } else {
        format!(" ({cause})")
    };
    if e.is_timeout() {
        format!("{host} didn't respond in time{detail}. Check your internet connection.")
    } else if e.is_connect() {
        format!("Couldn't reach {host}{detail}. Check the server URL and your internet connection.")
    } else {
        format!("The request to {host} failed: {e}{detail}")
    }
}

fn describe(e: &reqwest::Error) -> String {
    if e.is_connect() {
        format!(
            "The local assistant isn't running. Start it with: systemctl --user start {}",
            lemonade::SYSTEMD_UNIT
        )
    } else {
        e.to_string()
    }
}

#[derive(Debug, PartialEq)]
enum SseLine {
    Delta(String),
    Done,
}

fn parse_sse_line(line: &str) -> Option<SseLine> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(SseLine::Done);
    }
    let chunk: StreamChunk = serde_json::from_str(data).ok()?;
    let text: String = chunk
        .choices
        .into_iter()
        .filter_map(|c| c.delta.content)
        .collect();
    (!text.is_empty()).then_some(SseLine::Delta(text))
}

/// Heuristic for "this is a question for the assistant, not an app search".
pub fn looks_like_question(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    const STARTERS: &[&str] = &[
        "how", "what", "why", "where", "when", "which", "who", "can", "could", "should", "is",
        "are", "do", "does", "set up", "setup", "make", "turn", "change", "convert", "scale",
        "enable", "disable", "connect", "install", "show me", "help",
    ];
    q.ends_with('?')
        || q.split_whitespace().count() >= 4
        || STARTERS.iter().any(|s| q.starts_with(&format!("{s} ")))
        // Short symptom reports: "fan loud", "wifi broken".
        || !umoja_common::diagnostics::topics_for(&q).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parsing() {
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#),
            Some(SseLine::Delta("Hi".into()))
        );
        assert_eq!(parse_sse_line("data: [DONE]"), Some(SseLine::Done));
        assert_eq!(parse_sse_line(": keep-alive"), None);
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{}}]}"#), None);
    }

    #[test]
    fn tool_call_fragments_are_joined() {
        let mut a = CallAssembler::default();
        for line in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"set_dark_style","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"enabled\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":" true}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"launch_app","arguments":"{}"}}]}}]}"#,
        ] {
            let c: StreamChunk = serde_json::from_str(line).unwrap();
            for ch in c.choices {
                a.add(ch.delta.tool_calls);
            }
        }
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].name, "set_dark_style");
        assert_eq!(calls[0].arguments, r#"{"enabled": true}"#);
        assert_eq!(calls[1].id, "call_1");
    }

    #[test]
    fn question_heuristic() {
        assert!(looks_like_question("How do I set up a WireGuard VPN?"));
        assert!(looks_like_question("wifi broken"));
        assert!(looks_like_question("fan loud"));
        assert!(!looks_like_question("bluetooth"));
        assert!(looks_like_question("Scale my secondary 4K display to 150%"));
        assert!(looks_like_question("convert sample.png to webp"));
        assert!(!looks_like_question("firefox"));
        assert!(!looks_like_question("text editor"));
    }

    #[test]
    fn endpoint_avoids_upstream_port() {
        let c = LemonadeClient::new();
        assert!(!c.endpoint.contains(":13305"));
        assert!(c.endpoint.ends_with("/api/v1/chat/completions"));
    }
}

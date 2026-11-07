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
use umoja_common::{gsettings as gs, lemonade};

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
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    Done,
    Error(String),
}

#[derive(Clone)]
pub struct LemonadeClient {
    client: Client,
    endpoint: String,
    #[allow(dead_code)]
    health: String,
    model: String,
}

impl LemonadeClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
            endpoint: lemonade::chat_completions_url(),
            health: lemonade::health_url(),
            model: gs::get_string(gs::UMOJA_SHELL, "lemonade-model", DEFAULT_MODEL),
        }
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

    /// Stream tokens to `tx` as they arrive (OpenAI SSE format).
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
    fn question_heuristic() {
        assert!(looks_like_question("How do I set up a WireGuard VPN?"));
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

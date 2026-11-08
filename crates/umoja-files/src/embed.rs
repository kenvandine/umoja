//! Embeddings from the embedded Lemonade server.

use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use umoja_common::lemonade;

/// 78 MB, 768 dimensions, fast on CPU and GPU. Lemonade also offers
/// nomic-embed-text-v2-moe and Qwen3-Embedding models for more accuracy.
pub const DEFAULT_MODEL: &str = "nomic-embed-text-v1-GGUF";

#[derive(Clone)]
pub struct Embedder {
    client: Client,
    pub model: String,
}

#[derive(Debug)]
pub enum EmbedError {
    /// The server is down or the model isn't downloaded yet.
    Unavailable(String),
    Other(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Unavailable(s) | EmbedError::Other(s) => f.write_str(s),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

impl Embedder {
    pub fn new(model: String) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
            model,
        }
    }

    /// Task prefixes some embedding models are trained with.
    fn prefix(&self, query: bool) -> &'static str {
        let m = self.model.to_lowercase();
        if m.contains("nomic") {
            if query {
                "search_query: "
            } else {
                "search_document: "
            }
        } else if m.contains("qwen3-embedding") && query {
            "Instruct: Given a description of a file, find the file it describes\nQuery: "
        } else {
            ""
        }
    }

    pub async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let p = self.prefix(false);
        let input: Vec<String> = texts.iter().map(|t| format!("{p}{t}")).collect();
        self.embed(input, Duration::from_secs(120)).await
    }

    /// Embed a search query. Short timeout: the overlay is waiting, and
    /// word matching is a fine answer when the model is busy.
    pub async fn query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let input = vec![format!("{}{text}", self.prefix(true))];
        let mut v = self.embed(input, Duration::from_millis(2500)).await?;
        v.pop()
            .ok_or_else(|| EmbedError::Other("empty embedding".into()))
    }

    async fn embed(
        &self,
        input: Vec<String>,
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let n = input.len();
        let resp = self
            .client
            .post(lemonade::embeddings_url())
            .timeout(timeout)
            .json(&json!({ "model": self.model, "input": input }))
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    EmbedError::Unavailable("Lemonade isn't running".into())
                } else {
                    EmbedError::Other(e.to_string())
                }
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(EmbedError::Unavailable(format!(
                "{} isn't downloaded",
                self.model
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if body.contains("not_found")
                || body.contains("not downloaded")
                || body.contains("not installed")
            {
                return Err(EmbedError::Unavailable(format!(
                    "{} isn't downloaded",
                    self.model
                )));
            }
            return Err(EmbedError::Other(format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        let mut r: Response = resp
            .json()
            .await
            .map_err(|e| EmbedError::Other(e.to_string()))?;
        if r.data.len() != n {
            return Err(EmbedError::Other(format!(
                "asked for {n} embeddings, got {}",
                r.data.len()
            )));
        }
        r.data.sort_by_key(|i| i.index);
        Ok(r.data.into_iter().map(|i| i.embedding).collect())
    }

    /// Download the model through Lemonade. Blocks until it's done.
    pub async fn pull(&self) -> Result<(), String> {
        let resp = self
            .client
            .post(lemonade::pull_url())
            .timeout(Duration::from_secs(1800))
            .json(&json!({ "model_name": self.model, "stream": false }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() && !body.contains("\"error\"") {
            Ok(())
        } else {
            Err(format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            ))
        }
    }
}

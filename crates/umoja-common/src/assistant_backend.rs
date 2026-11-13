//! Which chat model the assistant talks to.
//!
//! By default that is the embedded Lemonade server (see [`crate::lemonade`]).
//! The user can instead point the assistant at any OpenAI-compatible cloud
//! endpoint: `org.umoja.shell assistant-backend` is `'cloud'`, with
//! `cloud-base-url` and `cloud-model` set. The API key is never written to
//! GSettings; it lives in the user's keyring (Secret Service, through
//! `secret-tool`), see [`token`].
//!
//! Embeddings for file search always stay on the local server.

use crate::{gsettings as gs, lemonade};

pub const BACKEND_KEY: &str = "assistant-backend";
pub const CLOUD_URL_KEY: &str = "cloud-base-url";
pub const CLOUD_MODEL_KEY: &str = "cloud-model";
pub const LOCAL: &str = "local";
pub const CLOUD: &str = "cloud";

/// Where chat requests go, and with which model.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// Full `…/chat/completions` URL.
    pub chat_url: String,
    pub model: String,
    /// True for a user-configured cloud endpoint (needs the keyring token).
    pub cloud: bool,
}

impl Endpoint {
    /// The embedded Lemonade with the model picked in the overlay.
    pub fn local() -> Self {
        Self {
            chat_url: lemonade::chat_completions_url(),
            model: lemonade::chat_model(),
            cloud: false,
        }
    }

    pub fn cloud(base_url: &str, model: &str) -> Self {
        Self {
            chat_url: chat_url(base_url),
            model: model.trim().to_string(),
            cloud: true,
        }
    }

    /// Host name for messages ("api.openai.com"), or "the local assistant".
    pub fn host(&self) -> String {
        if !self.cloud {
            return "the local assistant".into();
        }
        host_of(&self.chat_url).to_string()
    }
}

/// True when the user picked the cloud backend.
pub fn cloud_selected() -> bool {
    gs::get_string(gs::UMOJA_SHELL, BACKEND_KEY, LOCAL) == CLOUD
}

/// Saved cloud settings: (base URL, model). Empty strings when unset.
pub fn cloud_settings() -> (String, String) {
    (
        gs::get_string(gs::UMOJA_SHELL, CLOUD_URL_KEY, ""),
        gs::get_string(gs::UMOJA_SHELL, CLOUD_MODEL_KEY, ""),
    )
}

/// The endpoint the assistant should use right now. Falls back to the local
/// server when the cloud backend is selected but not fully configured.
/// Reads GSettings, so call it on the thread that owns the main context.
pub fn current() -> Endpoint {
    if cloud_selected() {
        let (url, model) = cloud_settings();
        if valid_base_url(&url) && !model.trim().is_empty() {
            return Endpoint::cloud(&url, &model);
        }
    }
    Endpoint::local()
}

/// `https://api.openai.com/v1` → `https://api.openai.com/v1/chat/completions`.
/// A full chat completions URL is accepted as is.
pub fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", api_root(base_url))
}

/// `…/models` for the same API root, used to suggest model names.
pub fn models_url(base_url: &str) -> String {
    format!("{}/models", api_root(base_url))
}

fn api_root(base_url: &str) -> &str {
    let b = base_url.trim().trim_end_matches('/');
    b.strip_suffix("/chat/completions").unwrap_or(b)
}

/// An http(s) URL with a host.
pub fn valid_base_url(url: &str) -> bool {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"));
    rest.is_some_and(|r| !host_of(u).is_empty() && !r.starts_with('/'))
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host)
}

/// OpenAI-style providers are stricter than Lemonade. Given a rejected
/// request body and the provider's error, drop or rename the parameter it
/// complained about. Returns true when the body changed and is worth
/// retrying (newer OpenAI models refuse a custom `temperature` and want
/// `max_completion_tokens` instead of `max_tokens`).
pub fn adapt_request(body: &mut serde_json::Value, error: &str) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if error.contains("max_tokens")
        && let Some(v) = obj.remove("max_tokens")
    {
        obj.insert("max_completion_tokens".into(), v);
        changed = true;
    }
    if error.contains("temperature") && obj.remove("temperature").is_some() {
        changed = true;
    }
    changed
}

/// Drop fields that only Lemonade's chat templates use (`name` on tool
/// results), so strict OpenAI-compatible servers don't reject the request.
pub fn strip_local_only_fields(messages: &mut [serde_json::Value]) {
    for m in messages {
        if m.get("role").and_then(|r| r.as_str()) == Some("tool")
            && let Some(o) = m.as_object_mut()
        {
            o.remove("name");
        }
    }
}

/// The provider's own error text from an error response body
/// (`{"error": {"message": …}}`, `{"error": "…"}` or `{"message": …}`).
pub fn provider_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.get("error").filter(|e| e.is_string()))
        .or_else(|| v.get("message"))
        .or_else(|| v.get("detail"))?;
    let s = match msg {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Tidy a pasted API key: drop whitespace and invisible characters
/// (zero-width spaces, BOMs, which `trim` keeps), surrounding quotes and a
/// leading "Bearer ". API keys never contain any of these.
pub fn clean_key(raw: &str) -> String {
    let mut k: String = raw
        .chars()
        .filter(|c| {
            !c.is_whitespace()
                && !c.is_control()
                && !matches!(
                    c,
                    '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'
                )
        })
        .collect();
    loop {
        let before = k.len();
        k = k
            .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
            .to_string();
        if k.len() >= 6 && k[..6].eq_ignore_ascii_case("bearer") {
            k = k[6..].trim_start_matches(':').to_string();
        }
        if k.len() == before {
            return k;
        }
    }
}

/// What a key looks like, without revealing it: for logs.
pub fn key_shape(key: &str) -> String {
    let prefix: String = key.chars().take(6).collect();
    format!(
        "{} chars, starts {prefix:?}{}",
        key.chars().count(),
        if key.is_ascii() {
            ""
        } else {
            ", has non-ASCII"
        }
    )
}

/// A sentence for the user about a failed cloud request.
pub fn describe_status(ep: &Endpoint, status: u16, body: &str) -> String {
    let detail = provider_error(body)
        .map(|m| format!(" ({})", truncate(&m, 200)))
        .unwrap_or_default();
    let host = ep.host();
    match status {
        401 if body.contains("Missing Authentication header") => format!(
            "{host} doesn't recognise that as an API key{detail}. OpenRouter keys start with “sk-or-v1-”."
        ),
        401 | 403 => format!("{host} rejected the API key{detail}."),
        404 => format!(
            "{host} doesn't have the model “{}”, or the server URL is wrong{detail}.",
            ep.model
        ),
        429 => format!("{host} is rate limiting requests or the account is out of credit{detail}."),
        500..=599 => format!("{host} had a server error ({status}){detail}. Try again later."),
        _ => format!("{host} refused the request ({status}){detail}."),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// The cloud API key, kept in the user's keyring (gnome-keyring) through
/// the Secret Service. Blocking: run these off the main loop.
pub mod token {
    use std::io::Write;
    use std::process::{Command, Stdio};

    const TOOL: &str = "secret-tool";
    const ATTRS: [&str; 4] = ["umoja", "assistant-cloud", "kind", "api-key"];
    const LABEL: &str = "Umoja assistant cloud API key";

    /// The stored key, if any.
    pub fn lookup() -> Option<String> {
        let out = Command::new(TOOL)
            .arg("lookup")
            .args(ATTRS)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!t.is_empty()).then_some(t)
    }

    /// Save `key`, replacing any stored one. The key goes through stdin, so
    /// it never shows up in a process listing.
    pub fn store(key: &str) -> Result<(), String> {
        let mut child = Command::new(TOOL)
            .args(["store", "--label", LABEL])
            .args(ATTRS)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't run {TOOL}: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(super::clean_key(key).as_bytes())
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() {
                "the keyring refused it".into()
            } else {
                err
            })
        }
    }

    /// Forget the stored key.
    pub fn clear() {
        let _ = Command::new(TOOL)
            .arg("clear")
            .args(ATTRS)
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_url_normalises() {
        assert_eq!(
            chat_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_url(" https://openrouter.ai/api/v1/chat/completions "),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(models_url("https://x.ai/v1"), "https://x.ai/v1/models");
    }

    #[test]
    fn base_url_validation() {
        assert!(valid_base_url("https://api.openai.com/v1"));
        assert!(valid_base_url("http://192.168.1.5:11434/v1"));
        assert!(!valid_base_url("api.openai.com/v1"));
        assert!(!valid_base_url("https://"));
        assert!(!valid_base_url(""));
        assert_eq!(
            Endpoint::cloud("https://u:p@h.example:8443/v1", "m").host(),
            "h.example:8443"
        );
    }

    #[test]
    fn adapts_to_strict_providers() {
        let mut b = json!({"model": "m", "temperature": 0.2, "max_tokens": 10});
        assert!(adapt_request(
            &mut b,
            "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."
        ));
        assert_eq!(b["max_completion_tokens"], 10);
        assert!(b.get("max_tokens").is_none());
        assert!(adapt_request(
            &mut b,
            "Unsupported value: 'temperature' does not support 0.2"
        ));
        assert!(b.get("temperature").is_none());
        assert!(!adapt_request(&mut b, "temperature"));
        assert!(!adapt_request(&mut b, "something else"));
    }

    #[test]
    fn tool_messages_lose_name() {
        let mut m = vec![
            json!({"role": "user", "content": "hi", "name": "ken"}),
            json!({"role": "tool", "tool_call_id": "c1", "name": "launch_app", "content": "ok"}),
        ];
        strip_local_only_fields(&mut m);
        assert_eq!(m[0]["name"], "ken");
        assert!(m[1].get("name").is_none());
    }

    #[test]
    fn keys_are_cleaned() {
        let k = "sk-or-v1-abc123";
        assert_eq!(clean_key(k), k);
        assert_eq!(clean_key(" \u{FEFF}sk-or-v1-abc123\u{200B}\n"), k);
        assert_eq!(clean_key("Bearer sk-or-v1-abc123"), k);
        assert_eq!(clean_key("\"sk-or-v1-abc123\""), k);
        assert_eq!(clean_key("“Bearer sk-or-v1-abc123”"), k);
        assert_eq!(clean_key("sk-or-v1-abc\n123"), k);
        assert_eq!(clean_key(""), "");
        assert_eq!(key_shape(k), "15 chars, starts \"sk-or-\"");
    }

    #[test]
    fn provider_errors() {
        assert_eq!(
            provider_error(r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#).as_deref(),
            Some("Incorrect API key provided")
        );
        assert_eq!(
            provider_error(r#"{"error":"bad model"}"#).as_deref(),
            Some("bad model")
        );
        assert_eq!(provider_error("<html>"), None);
        let ep = Endpoint::cloud("https://api.openai.com/v1", "gpt-x");
        assert_eq!(
            describe_status(&ep, 401, r#"{"error":{"message":"Incorrect API key"}}"#),
            "api.openai.com rejected the API key (Incorrect API key)."
        );
        assert!(describe_status(&ep, 404, "").contains("“gpt-x”"));
    }
}

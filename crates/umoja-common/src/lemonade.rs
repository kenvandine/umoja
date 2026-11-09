//! Where to find Umoja's embedded Lemonade server.
//!
//! Umoja ships its own `lemond` binary (installed to
//! `/usr/lib/umoja/lemonade/lemond`) and runs it as the `umoja-lemonade`
//! systemd user service. It deliberately listens on [`DEFAULT_PORT`] instead
//! of Lemonade's upstream default of 13305, and keeps its config and cache
//! under `~/.config/umoja/lemonade` and `~/.cache/umoja/lemonade`, so it can
//! run side by side with a system-wide `lemonade-server` package without
//! either one touching the other.
//!
//! The port can be changed by writing `UMOJA_LEMONADE_PORT=<port>` to
//! `~/.config/umoja/lemonade.env`; the service and the overlay both read it.

/// Port used by Umoja's embedded Lemonade. Upstream Lemonade defaults to 13305.
pub const DEFAULT_PORT: u16 = 13345;
/// Upstream lemonade-server's default port, which Umoja must never use.
pub const UPSTREAM_DEFAULT_PORT: u16 = 13305;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const PORT_ENV: &str = "UMOJA_LEMONADE_PORT";
pub const URL_ENV: &str = "UMOJA_LEMONADE_URL";
pub const BINARY_PATH: &str = "/usr/lib/umoja/lemonade/lemond";
pub const SYSTEMD_UNIT: &str = "umoja-lemonade.service";
/// Model used when `org.umoja.shell lemonade-model` is unset.
pub const DEFAULT_MODEL: &str = "Qwen3-4B-Instruct-2507-GGUF";

/// Base URL of the embedded server, e.g. `http://127.0.0.1:13345`.
///
/// Resolution order: `UMOJA_LEMONADE_URL`, then `UMOJA_LEMONADE_PORT`, then
/// [`DEFAULT_PORT`].
pub fn base_url() -> String {
    base_url_from(
        std::env::var(URL_ENV).ok().as_deref(),
        std::env::var(PORT_ENV).ok().as_deref(),
    )
}

fn base_url_from(url: Option<&str>, port: Option<&str>) -> String {
    if let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) {
        return url.trim_end_matches('/').to_string();
    }
    let port = port
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT);
    format!("http://{DEFAULT_HOST}:{port}")
}

/// OpenAI-compatible chat completions endpoint.
pub fn chat_completions_url() -> String {
    format!("{}/api/v1/chat/completions", base_url())
}

/// OpenAI-compatible embeddings endpoint, used by umoja-files.
pub fn embeddings_url() -> String {
    format!("{}/api/v1/embeddings", base_url())
}

/// Model download endpoint (`POST`, SSE progress with `"stream": true`).
pub fn pull_url() -> String {
    format!("{}/api/v1/pull", base_url())
}

/// Unload endpoint (`POST`): drops loaded models from memory; the next
/// request loads its model again.
pub fn unload_url() -> String {
    format!("{}/api/v1/unload", base_url())
}

/// Free the memory the local models use, by asking the server to unload
/// them. Blocking (a plain HTTP request to the local server, so headless
/// processes need no HTTP client). `Ok(false)` when the server isn't
/// running, which leaves nothing loaded either.
pub fn unload() -> Result<bool, String> {
    use std::io::{Read, Write};
    use std::time::Duration;
    let url = unload_url();
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported server URL {url}"))?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(a, p)| (a, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    use std::net::ToSocketAddrs;
    let addr = authority
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address for the server")?;
    let mut stream = match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    let body = "{}";
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut head = [0u8; 64];
    let n = stream.read(&mut head).map_err(|e| e.to_string())?;
    let status = String::from_utf8_lossy(&head[..n]);
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code.starts_with('2') {
        Ok(true)
    } else {
        Err(format!("the AI server answered {code}"))
    }
}

/// Health endpoint, used to check the service is up before querying.
pub fn health_url() -> String {
    format!("{}/api/v1/health", base_url())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_is_not_upstream() {
        assert_ne!(DEFAULT_PORT, UPSTREAM_DEFAULT_PORT);
        assert_eq!(base_url_from(None, None), "http://127.0.0.1:13345");
    }

    #[test]
    fn port_override() {
        assert_eq!(base_url_from(None, Some("14000")), "http://127.0.0.1:14000");
        assert_eq!(base_url_from(None, Some("junk")), "http://127.0.0.1:13345");
    }

    #[test]
    fn url_override_wins() {
        assert_eq!(
            base_url_from(Some("http://h:1/"), Some("14000")),
            "http://h:1"
        );
    }
}

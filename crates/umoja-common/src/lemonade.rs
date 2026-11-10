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
    unload_model(None)
}

/// Like [`unload`], for one model only (`None`: all of them), so dropping
/// the idle chat model leaves the embedding model alone and the other way
/// round.
pub fn unload_model(model: Option<&str>) -> Result<bool, String> {
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
    let body = match model {
        Some(m) => serde_json::json!({ "model_name": m }).to_string(),
        None => "{}".to_string(),
    };
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

/// Unloading the chat model when nobody has asked it anything for a while
/// (`org.umoja.shell assistant-idle-unload` minutes). Loaded, Qwen3-4B and
/// its context take about 5.5 GB of memory, which an APU shares with
/// everything else.
///
/// Every process that talks to the local chat model calls [`note_chat_use`]
/// when a request starts and when it ends. That touches a file in
/// `$XDG_RUNTIME_DIR`, so the overlay and the panel see each other's use,
/// and wakes one watcher thread in this process. The watcher unloads the
/// model once the file is older than the idle time, but only when this
/// process used it since the last unload, so idle processes never wake.
pub mod idle {
    use std::path::PathBuf;
    use std::sync::{Condvar, LazyLock, Mutex};
    use std::time::{Duration, SystemTime};

    const DEFAULT_MINUTES: i32 = 10;

    /// The chat model this process last used, waiting to be unloaded.
    static PENDING: LazyLock<(Mutex<Option<String>>, Condvar)> =
        LazyLock::new(|| (Mutex::new(None), Condvar::new()));

    fn stamp() -> PathBuf {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join("umoja-lemonade-chat-used")
    }

    fn idle_time() -> Option<Duration> {
        let minutes =
            crate::gsettings::get_int(crate::gsettings::UMOJA_SHELL, "assistant-idle-unload", DEFAULT_MINUTES);
        (minutes > 0).then(|| Duration::from_secs(minutes as u64 * 60))
    }

    /// When any process last used the chat model.
    fn last_use() -> Option<SystemTime> {
        std::fs::metadata(stamp()).and_then(|m| m.modified()).ok()
    }

    /// A request to the local chat `model` starts or ends. Cheap; any thread.
    pub fn note_chat_use(model: &str) {
        let path = stamp();
        let touched = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        if let Err(e) = touched {
            tracing::debug!(error = %e, path = %path.display(), "could not mark model use");
        }
        let (lock, cv) = &*PENDING;
        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
        let start = pending.is_none();
        *pending = Some(model.to_string());
        cv.notify_all();
        drop(pending);
        if start {
            start_watcher();
        }
    }

    fn start_watcher() {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let spawned = std::thread::Builder::new()
                .name("umoja-model-idle".into())
                .spawn(watch);
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no idle model unload");
            }
        });
    }

    fn watch() {
        let (lock, cv) = &*PENDING;
        loop {
            // Sleep until this process has used the model.
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            while pending.is_none() {
                pending = cv.wait(pending).unwrap_or_else(|e| e.into_inner());
            }
            drop(pending);
            let Some(idle) = idle_time() else {
                // Kept loaded: look again in a while (the setting may change).
                std::thread::sleep(Duration::from_secs(300));
                continue;
            };
            let since = last_use()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .unwrap_or(idle);
            if since < idle {
                // Wake early when used again, so a new question re-arms it.
                let pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                let _ = cv.wait_timeout(pending, idle - since);
                continue;
            }
            let model = lock.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(model) = model else { continue };
            match super::unload_model(Some(&model)) {
                Ok(true) => tracing::info!(model, "unloaded the idle chat model"),
                Ok(false) => {}
                Err(e) => tracing::info!(model, error = %e, "could not unload the idle chat model"),
            }
        }
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

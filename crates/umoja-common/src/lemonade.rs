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
/// Model used when `org.umoja.shell lemonade-model` is unset, on a
/// computer with enough memory for it (see [`auto_model`]).
pub const DEFAULT_MODEL: &str = "Qwen3-4B-Instruct-2507-GGUF";
/// Default below [`SMALL_RAM_GB`]: Qwen3-4B (2.5 GB plus its context) got
/// OOM-killed on an 8 GB laptop (6.7 GB usable) with a browser open.
pub const SMALL_MODEL: &str = "Qwen3.5-2B-GGUF";
/// Last resort after the small model got OOM-killed too.
pub const TINY_MODEL: &str = "Qwen3.5-0.8B-GGUF";
/// Below this much RAM (MemTotal, GiB) the small model is the default. A
/// "16 GB" laptop reports ~15, an "8 GB" one 6.5 to 7.8.
pub const SMALL_RAM_GB: f64 = 12.0;
/// The runtime file `umoja-lemonade.service` writes its `$SERVICE_RESULT`
/// to whenever lemond stops (ExecStopPost): "oom-kill" after an OOM kill.
const RESULT_FILE: &str = "umoja-lemonade.result";
/// Kept when an OOM kill made Umoja step down to [`TINY_MODEL`].
const STEPPED_DOWN_FILE: &str = "lemonade-tiny-model";

/// Total RAM in GiB (`MemTotal`), 16 when unknown.
pub fn mem_total_gb() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| mem_total_from(&m))
        .unwrap_or(16.0)
}

fn mem_total_from(meminfo: &str) -> Option<f64> {
    meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<f64>()
        .ok()
        .map(|kb| kb / 1024.0 / 1024.0)
}

/// The chat model to use when the user hasn't picked one: by RAM, and the
/// tiny one once the small one ran out of memory.
pub fn auto_model() -> &'static str {
    auto_model_for(mem_total_gb(), stepped_down_path().exists())
}

fn auto_model_for(mem_gb: f64, stepped_down: bool) -> &'static str {
    if mem_gb >= SMALL_RAM_GB {
        DEFAULT_MODEL
    } else if stepped_down {
        TINY_MODEL
    } else {
        SMALL_MODEL
    }
}

/// The local chat model: `org.umoja.shell lemonade-model`, or
/// [`auto_model`] when that is empty (the default).
pub fn chat_model() -> String {
    let picked = crate::gsettings::get_string(crate::gsettings::UMOJA_SHELL, "lemonade-model", "");
    let picked = picked.trim();
    if picked.is_empty() {
        auto_model().to_string()
    } else {
        picked.to_string()
    }
}

fn state_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("umoja")
}

/// The file whose presence makes [`TINY_MODEL`] the automatic choice.
pub fn stepped_down_path() -> std::path::PathBuf {
    state_dir().join(STEPPED_DOWN_FILE)
}

fn result_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(RESULT_FILE)
}

/// Why the local server just went away, from what the service wrote
/// when lemond last stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The kernel OOM killer took lemond (the model didn't fit).
    OutOfMemory,
    /// lemond stopped some other way and systemd is bringing it back.
    Restarting,
}

/// How recent a stop has to be to explain a failed request.
const TROUBLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(180);

/// Whether the local server stopped in the last few minutes, and why.
/// Call it after a request to the local server failed.
pub fn recent_trouble() -> Option<Trouble> {
    let path = result_path();
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())?;
    if age > TROUBLE_WINDOW {
        return None;
    }
    let result = std::fs::read_to_string(&path).ok()?;
    trouble_from(&result)
}

fn trouble_from(result: &str) -> Option<Trouble> {
    match result.trim() {
        "oom-kill" => Some(Trouble::OutOfMemory),
        // A clean stop (logout, `systemctl stop`) isn't trouble.
        "" | "success" => None,
        _ => Some(Trouble::Restarting),
    }
}

/// A failed local request, said for the user. After an OOM kill of the
/// automatic small model this also steps the automatic choice down to
/// [`TINY_MODEL`], once; `None` when the server didn't stop recently.
pub fn trouble_message() -> Option<String> {
    let trouble = recent_trouble()?;
    Some(match trouble {
        Trouble::Restarting => {
            "The local AI server stopped and is restarting. Ask again in a moment.".into()
        }
        Trouble::OutOfMemory => {
            let model = chat_model();
            if model == SMALL_MODEL && step_down() {
                format!(
                    "The local model ran out of memory, so Umoja switched to a smaller one ({TINY_MODEL}). \
                     Ask again once it has downloaded."
                )
            } else {
                "The local model ran out of memory. Close some apps, or pick a smaller model \
                 or a cloud model in Ask Umoja's settings, then ask again."
                    .into()
            }
        }
    })
}

/// [`trouble_message`], waiting up to `wait` for the service to record
/// why lemond stopped (ExecStopPost runs just after it dies). Blocking.
pub fn trouble_message_wait(wait: std::time::Duration) -> Option<String> {
    let until = std::time::Instant::now() + wait;
    loop {
        if let Some(msg) = trouble_message() {
            return Some(msg);
        }
        if std::time::Instant::now() >= until {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Make [`TINY_MODEL`] the automatic choice from now on. True when this
/// call changed it.
fn step_down() -> bool {
    let picked = crate::gsettings::get_string(crate::gsettings::UMOJA_SHELL, "lemonade-model", "");
    let path = stepped_down_path();
    if !picked.trim().is_empty() || path.exists() || mem_total_gb() >= SMALL_RAM_GB {
        return false;
    }
    let made =
        std::fs::create_dir_all(state_dir()).and_then(|()| std::fs::write(&path, TINY_MODEL));
    match made {
        Ok(()) => {
            tracing::warn!(
                from = SMALL_MODEL,
                to = TINY_MODEL,
                "local model ran out of memory; using a smaller one"
            );
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not switch to the smaller model");
            false
        }
    }
}

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
    fn auto_model_by_memory() {
        assert_eq!(auto_model_for(31.0, false), DEFAULT_MODEL);
        assert_eq!(auto_model_for(15.2, true), DEFAULT_MODEL);
        assert_eq!(auto_model_for(6.7, false), SMALL_MODEL);
        assert_eq!(auto_model_for(6.7, true), TINY_MODEL);
    }

    #[test]
    fn meminfo_parse() {
        let gb = mem_total_from("MemTotal:        7025344 kB\nMemFree: 1 kB\n").unwrap();
        assert!((gb - 6.7).abs() < 0.05, "{gb}");
        assert_eq!(mem_total_from("nothing"), None);
    }

    #[test]
    fn service_results() {
        assert_eq!(trouble_from("oom-kill\n"), Some(Trouble::OutOfMemory));
        assert_eq!(trouble_from("signal"), Some(Trouble::Restarting));
        assert_eq!(trouble_from("success"), None);
        assert_eq!(trouble_from(""), None);
    }

    #[test]
    fn url_override_wins() {
        assert_eq!(
            base_url_from(Some("http://h:1/"), Some("14000")),
            "http://h:1"
        );
    }
}

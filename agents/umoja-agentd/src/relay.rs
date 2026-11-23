//! Lets the sandbox reach the embedded Lemonade.
//!
//! Lemonade listens on 127.0.0.1 only, and a sandbox's supervisor (the only
//! part with a network card) is on the LXD bridge. This forwards
//! bridge-address:relay-port to 127.0.0.1:lemonade-port, and only for
//! connections from this account's sandbox supervisors, so other containers
//! on the bridge can't use the user's model.
//!
//! Only model requests pass (chat, completions, embeddings, the model list
//! and health): Lemonade's management API (installing backends, pulling
//! models, changing a provider's URL, which would send the cloud key
//! elsewhere) is refused. Each connection carries one request
//! (`Connection: close`), so every request is checked.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

pub type Allowed = Arc<RwLock<Vec<Ipv4Addr>>>;

pub async fn serve(bind: Ipv4Addr, port: u16, target_port: u16, allowed: Allowed) -> anyhow::Result<()> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(bind), port)).await?;
    tracing::info!(%bind, port, target_port, "model relay listening");
    loop {
        let (mut inbound, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "model relay accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let ok = match peer.ip() {
            IpAddr::V4(ip) => allowed.read().await.contains(&ip),
            IpAddr::V6(_) => false,
        };
        if !ok {
            tracing::warn!(%peer, "model relay refused a connection that isn't from an Umoja sandbox");
            continue;
        }
        tokio::spawn(async move {
            let head = match read_head(&mut inbound).await {
                Some(h) => h,
                None => return,
            };
            let Some(forward) = filter(&head) else {
                let line = String::from_utf8_lossy(head.split(|b| *b == b'\n').next().unwrap_or_default()).trim().to_owned();
                tracing::warn!(request = %line, "model relay refused a request that isn't for the model");
                let _ = inbound
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            };
            match TcpStream::connect(("127.0.0.1", target_port)).await {
                Ok(mut outbound) => {
                    if outbound.write_all(&forward).await.is_ok() {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "model relay: Lemonade isn't answering"),
            }
        });
    }
}

/// Most a request's head (request line and headers) may take.
const MAX_HEAD: usize = 64 * 1024;

/// Reads a request's head and whatever of its body came with it.
async fn read_head(inbound: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let read = async {
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(());
            }
            if buf.len() > MAX_HEAD {
                return None;
            }
            match inbound.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), read).await.ok()??;
    Some(buf)
}

/// Whether `method path` is a model request.
fn allowed(method: &str, path: &str) -> bool {
    let path = path.split('?').next().unwrap_or_default();
    if path.contains("..") || path.contains("//") || path.contains('%') {
        return false;
    }
    let rest = path.strip_prefix("/api").unwrap_or(path);
    let Some(rest) = rest.strip_prefix("/v1/") else { return false };
    match method {
        "POST" => matches!(rest, "chat/completions" | "completions" | "embeddings" | "responses"),
        "GET" => rest == "models" || rest == "health" || rest.strip_prefix("models/").is_some_and(|m| !m.contains('/')),
        _ => false,
    }
}

/// The head to send on for an allowed request, with `Connection: close`
/// (one request per connection, so the next one is checked too). None
/// refuses it.
fn filter(head: &[u8]) -> Option<Vec<u8>> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = std::str::from_utf8(&head[..end]).ok()?;
    let mut lines = text.split("\r\n");
    let request = lines.next()?;
    let mut parts = request.split(' ');
    let (method, path, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !version.starts_with("HTTP/1.") || !allowed(method, path) {
        return None;
    }
    let mut out = format!("{request}\r\n");
    for line in lines {
        let name = line.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        if matches!(name.as_str(), "connection" | "keep-alive" | "proxy-connection" | "upgrade") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&head[end + 4..]);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_model_requests_pass() {
        assert!(allowed("POST", "/api/v1/chat/completions"));
        assert!(allowed("GET", "/api/v1/models"));
        assert!(allowed("GET", "/api/v1/models/user.Umoja-Agent-Auto"));
        assert!(allowed("GET", "/v1/health"));
        assert!(!allowed("POST", "/api/v1/install"));
        assert!(!allowed("POST", "/api/v1/pull"));
        assert!(!allowed("POST", "/api/v1/load"));
        assert!(!allowed("GET", "/api/v1/models/../install"));
        assert!(!allowed("POST", "/api/v1/chat/completions/../../install"));
        assert!(!allowed("DELETE", "/api/v1/models"));
        assert!(!allowed("POST", "/api/v1/%69nstall"));
    }

    #[test]
    fn requests_go_on_one_per_connection() {
        let head = b"POST /api/v1/chat/completions HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\n{}";
        let out = String::from_utf8(filter(head).unwrap()).unwrap();
        assert!(out.contains("Connection: close\r\n\r\n{}"));
        assert!(!out.contains("keep-alive"));
        assert!(filter(b"POST /api/v1/install HTTP/1.1\r\nHost: x\r\n\r\n").is_none());
    }
}

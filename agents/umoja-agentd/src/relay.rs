//! Lets the sandbox reach the embedded Lemonade.
//!
//! Lemonade listens on 127.0.0.1 only, and a sandbox's supervisor (the only
//! part with a network card) is on the LXD bridge. This forwards
//! bridge-address:relay-port to 127.0.0.1:lemonade-port, and only for
//! connections from this account's sandbox supervisors, so other containers
//! on the bridge can't use the user's model.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

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
            match TcpStream::connect(("127.0.0.1", target_port)).await {
                Ok(mut outbound) => {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                Err(e) => tracing::warn!(error = %e, "model relay: Lemonade isn't answering"),
            }
        });
    }
}

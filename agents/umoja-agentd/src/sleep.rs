//! Suspend and resume (logind): laptops sleep with the lid shut while
//! Hermes works. Before sleep the tasks in flight are marked interrupted
//! (a delay inhibitor gives us the moment to save that); after wake the
//! watchers reconnect, the sandbox and model are checked, and a task whose
//! run died or stalled is carried on in the same Hermes conversation
//! (Agent::after_wake, Agent::follow).

use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;
use zbus::zvariant::OwnedFd;

use crate::agent::Agent;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

async fn delay_lock(m: &ManagerProxy<'_>) -> Option<OwnedFd> {
    m.inhibit("sleep", "Hermes", "Saving what Hermes is working on", "delay")
        .await
        .map_err(|e| tracing::debug!(error = %e, "no sleep delay lock"))
        .ok()
}

pub async fn watch(agent: Arc<Agent>) -> Result<()> {
    let system = zbus::Connection::system().await?;
    let manager = ManagerProxy::new(&system).await?;
    let mut signals = manager.receive_prepare_for_sleep().await?;
    let mut lock = delay_lock(&manager).await;
    while let Some(sig) = signals.next().await {
        let Ok(args) = sig.args() else { continue };
        if args.start {
            agent.before_sleep().await;
            // Dropping the fd lets the computer go to sleep.
            lock.take();
        } else {
            tracing::info!("resumed from sleep");
            lock = delay_lock(&manager).await;
            agent.after_wake().await;
        }
    }
    Ok(())
}

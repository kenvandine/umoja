//! umoja-agentd: runs Hermes in a per-user OpenShell sandbox (LXD driver)
//! and puts its consent prompts on the Umoja desktop.

mod agent;
mod hermes;
mod lxd;
mod notify;
mod paths;
mod relay;
mod runtime;
mod service;
mod state;

use anyhow::Result;
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "umoja_agentd=info".into()),
        )
        .without_time()
        .init();

    let conn = zbus::Connection::session().await?;
    let agent = agent::Agent::new(conn.clone());
    conn.object_server()
        .at(agent::PATH, service::Agents(agent.clone()))
        .await?;
    conn.request_name("org.umoja.Agents").await?;
    tracing::info!("org.umoja.Agents ready");

    let notes = notify::NotificationsProxy::new(&conn).await?;
    let mut actions = notes.receive_action_invoked().await?;
    let a = agent.clone();
    tokio::spawn(async move {
        while let Some(sig) = actions.next().await {
            if let Ok(args) = sig.args() {
                a.on_action(args.id, &args.action_key).await;
            }
        }
    });

    agent.resume().await;

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    agent.shutdown().await;
    Ok(())
}

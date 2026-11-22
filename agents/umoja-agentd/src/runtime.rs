//! The OpenShell side, run per user: Umoja's gateway and the LXD compute
//! driver as child processes of umoja-agentd (restarted if they die), and
//! the gateway client to talk to them.

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use openshell_gateway_client::{Config, Gateway};
use tokio::process::{Child, Command};

use crate::paths;

pub struct Runtime {
    pub bridge_ip: Ipv4Addr,
    pub gateway_port: u16,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Runtime {
    /// Gateway TLS material, made once (idempotent).
    async fn certs() -> Result<()> {
        let tls = paths::tls_dir();
        std::fs::create_dir_all(&tls)?;
        let status = Command::new(paths::libexec("umoja-openshell-gateway"))
            .arg("generate-certs")
            .arg("--output-dir")
            .arg(&tls)
            .args(["--server-san", "host.openshell.internal"])
            .stdout(Stdio::null())
            .status()
            .await
            .context("couldn't run umoja-openshell-gateway")?;
        if !status.success() {
            bail!("the OpenShell gateway couldn't make its certificates ({status})");
        }
        Ok(())
    }

    /// Starts the driver and gateway and keeps them running until dropped.
    pub async fn start(bridge_ip: Ipv4Addr) -> Result<Self> {
        Self::certs().await?;
        let (gateway_port, _) = paths::ports();
        std::fs::create_dir_all(paths::runtime_dir())?;
        std::fs::create_dir_all(paths::staging_dir())?;
        let (stop, stopped) = tokio::sync::watch::channel(false);

        let driver = {
            let socket = paths::driver_socket();
            let tls_ca = paths::tls_dir().join("ca.crt");
            move || {
                let mut c = Command::new(paths::libexec("umoja-openshell-lxd"));
                c.arg("--socket")
                    .arg(&socket)
                    .arg("--staging-dir")
                    .arg(paths::staging_dir())
                    .args(["--project", &paths::lxd_project()])
                    .args(["--supervisor-network", &paths::lxd_network()])
                    .args(["--grpc-endpoint", &format!("https://host.openshell.internal:{gateway_port}")])
                    .arg("--tls-ca")
                    .arg(&tls_ca)
                    .env("RUST_LOG", child_log());
                c
            }
        };
        let gateway = move || {
            let mut c = Command::new(paths::libexec("umoja-openshell-gateway"));
            // Supervisors dial it over the bridge; the bridge address is only
            // reachable from this computer and its containers, and every
            // client needs the mTLS certificate anyway.
            c.args(["--bind-address", &bridge_ip.to_string()])
                .args(["--port", &gateway_port.to_string()])
                .arg("--db-url")
                .arg(format!("sqlite:{}?mode=rwc", paths::state_dir().join("gateway.db").display()))
                .args(["--compute-driver", "lxd"])
                .arg("--compute-driver-socket")
                .arg(paths::driver_socket())
                .env("OPENSHELL_LOCAL_TLS_DIR", paths::tls_dir())
                .env("RUST_LOG", child_log());
            c
        };
        tokio::spawn(keep_running("openshell-lxd-driver", driver, stopped.clone()));
        // The gateway connects to the driver at start; give the socket a moment.
        for _ in 0..50 {
            if paths::driver_socket().exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::spawn(keep_running("openshell-gateway", gateway, stopped));
        Ok(Self { bridge_ip, gateway_port, stop })
    }

    pub fn config(&self) -> Config {
        Config {
            endpoint: format!("https://{}:{}", self.bridge_ip, self.gateway_port),
            tls_dir: paths::tls_dir(),
        }
    }

    /// A gateway client, waiting up to `timeout` for the gateway to answer.
    pub async fn connect(&self, timeout: Duration) -> Result<Gateway> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = String::new();
        while tokio::time::Instant::now() < deadline {
            match Gateway::connect(&self.config()).await {
                Ok(mut gw) => match gw.list().await {
                    Ok(_) => return Ok(gw),
                    Err(e) => last = e.to_string(),
                },
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(anyhow!("the OpenShell gateway didn't start: {last}"))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn keep_running(
    name: &'static str,
    make: impl Fn() -> Command,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        if *stopped.borrow() {
            return;
        }
        let mut cmd = make();
        cmd.kill_on_drop(true).stdin(Stdio::null());
        // stdout/stderr go to our journal.
        let child: Result<Child, _> = cmd.spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(%name, error = %e, "couldn't start");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let started = tokio::time::Instant::now();
        tokio::select! {
            status = child.wait() => {
                tracing::warn!(%name, ?status, "exited; restarting");
            }
            _ = stopped.changed() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return;
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_millis(500);
        } else {
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
        tokio::time::sleep(backoff).await;
    }
}

/// The gateway logs every RPC at info (agentd polls every 1.5 s), so the
/// journal gets only warnings unless UMOJA_AGENTS_OPENSHELL_LOG says more.
fn child_log() -> String {
    std::env::var("UMOJA_AGENTS_OPENSHELL_LOG").unwrap_or_else(|_| "warn".into())
}

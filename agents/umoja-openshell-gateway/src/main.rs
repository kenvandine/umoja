//! OpenShell's gateway, built without telemetry or in-tree compute drivers.
//! Umoja runs it per user (umoja-openshell-gateway.service) with
//! `--compute-driver lxd`, pointing at umoja-openshell-lxd's socket.
//!
//! Same composition as OpenShell's own `openshell-gateway` binary.

#[tokio::main]
async fn main() -> miette::Result<()> {
    openshell_server::cli::run_cli_with_compute_drivers(
        openshell_gateway::install_default_compute_drivers(),
    )
    .await
}

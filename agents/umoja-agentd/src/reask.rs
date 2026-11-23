//! Asking again after a Deny.
//!
//! The guardrails keep a turned-down request as "rejected", and a later try
//! at the same site by the same program only counts another hit on that
//! record: they never ask again. The agent is just refused ("permission
//! restrictions"), with nothing on screen, for good. In Umoja, Deny means
//! "not this time", and requests Umoja turns down by itself (a job that
//! ended, the agent's own background fetches) mean even less. So a rejected
//! request that has been tried again since it was turned down is shown as a
//! new card, and Allow approves it (the guardrails let a rejected request be
//! approved later).
//!
//! The gateway client lists pending requests only, so this keeps its own
//! connection for the rejected ones.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use openshell_gateway_client::proto::datamodel::v1::{WorkspaceSelector, workspace_selector::Selection};
use openshell_gateway_client::proto::open_shell_client::OpenShellClient;
use openshell_gateway_client::proto::{GetDraftPolicyRequest, PolicyChunk};
use openshell_gateway_client::{Config, Proposal, WORKSPACE};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// A try this soon after the turn-down is the same attempt still retrying.
const GRACE_MS: i64 = 3_000;

#[derive(Clone)]
pub struct Rejected {
    client: OpenShellClient<Channel>,
}

impl Rejected {
    pub async fn connect(config: &Config) -> Result<Self> {
        let read = |p: &Path| std::fs::read(p).with_context(|| format!("reading {}", p.display()));
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(read(&config.tls_dir.join("ca.crt"))?))
            .identity(Identity::from_pem(
                read(&config.tls_dir.join("client/tls.crt"))?,
                read(&config.tls_dir.join("client/tls.key"))?,
            ))
            .domain_name("localhost");
        let channel = Endpoint::from_shared(config.endpoint.clone())?
            .tls_config(tls)?
            .connect_timeout(Duration::from_secs(3))
            .connect()
            .await?;
        Ok(Self { client: OpenShellClient::new(channel) })
    }

    /// Rejected requests of `sandbox` tried again since they were turned
    /// down: by the guardrails, or by Umoja (`turned_down`, chunk id → ms).
    pub async fn tried_again(&self, sandbox: &str, turned_down: &HashMap<String, i64>) -> Result<Vec<Proposal>> {
        let chunks = self
            .client
            .clone()
            .get_draft_policy(GetDraftPolicyRequest {
                workspace_scope: Some(WorkspaceSelector { selection: Some(Selection::Workspace(WORKSPACE.into())) }),
                status_filter: "rejected".into(),
                sandbox: sandbox.into(),
            })
            .await?
            .into_inner()
            .chunks;
        Ok(chunks
            .iter()
            .filter(|c| tried_since(c, turned_down.get(&c.id).copied()))
            .filter_map(Proposal::from_chunk)
            .collect())
    }
}

fn tried_since(chunk: &PolicyChunk, ours: Option<i64>) -> bool {
    let Some(last) = chunk.last_seen_time.as_ref().map(|t| t.seconds * 1000 + i64::from(t.nanos) / 1_000_000) else {
        return false;
    };
    let decided = chunk
        .decided_time
        .as_ref()
        .map(|t| t.seconds * 1000 + i64::from(t.nanos) / 1_000_000)
        .unwrap_or(0)
        .max(ours.unwrap_or(0));
    last > decided + GRACE_MS
}

//! The agents Umoja can run. Hermes comes first, but nothing outside this
//! file and the agent's image assumes it: each agent is a catalog entry
//! (agents/catalog/<id>.json, installed to /usr/share/umoja/agents/catalog)
//! plus a sandbox image that ships `umoja-agent-start` (its main process)
//! and `umoja-agent-bridge` (the protocol umoja-agentd speaks, see
//! bridge.rs and agents/README.md). Adding OpenClaw, PicoClaw and so on is
//! a new image and a new JSON file, not new daemon code.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::state::Preset;

const BUILTIN: &str = include_str!("../../catalog/hermes.json");
const DIR: &str = "/usr/share/umoja/agents/catalog";
pub const DEFAULT: &str = "hermes";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentKind {
    /// Lowercase id: the sandbox's name and the `@id` hand-off prefix.
    pub id: String,
    /// What the user sees: "Hermes".
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The sandbox image (an OCI image the LXD driver can pull).
    pub image: String,
    /// The sandbox's main process.
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// The smallest model context the agent accepts (tokens); the local
    /// model is loaded with at least this much.
    #[serde(default)]
    pub min_context: u32,
    /// The agent's own approval mode per Umoja preset (passed to the
    /// bridge as UMOJA_APPROVALS); agents without one ignore it.
    #[serde(default)]
    pub approval_modes: BTreeMap<String, String>,
    /// Chat platforms its bridge can connect ("telegram").
    #[serde(default)]
    pub messaging: Vec<String>,
    /// Sites the agent fetches for itself (update checks, plugin catalogs)
    /// that stay blocked without a consent card.
    #[serde(default)]
    pub quiet_hosts: Vec<String>,
}

fn default_command() -> Vec<String> {
    vec!["/usr/local/bin/umoja-agent-start".into()]
}

impl AgentKind {
    pub fn approval_mode(&self, preset: Preset) -> String {
        let key = match preset {
            Preset::Careful => "careful",
            Preset::Balanced => "balanced",
            Preset::HandsOff => "hands-off",
        };
        self.approval_modes
            .get(key)
            .cloned()
            .unwrap_or_else(|| if preset == Preset::Careful { "manual" } else { "smart" }.into())
    }

    fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 32
            && self.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.is_empty()
            && !self.image.is_empty()
    }
}

/// Every known agent, by id: the built-in Hermes entry, then the installed
/// catalog (which may replace it).
pub fn all() -> BTreeMap<String, AgentKind> {
    let mut out = BTreeMap::new();
    if let Ok(k) = serde_json::from_str::<AgentKind>(BUILTIN) {
        out.insert(k.id.clone(), k);
    }
    let dir = std::env::var("UMOJA_AGENTS_CATALOG").unwrap_or_else(|_| DIR.into());
    if let Ok(entries) = std::fs::read_dir(Path::new(&dir)) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "json") {
                match std::fs::read_to_string(&path).map(|t| serde_json::from_str::<AgentKind>(&t)) {
                    Ok(Ok(k)) if k.valid() => {
                        out.insert(k.id.clone(), k);
                    }
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                        tracing::warn!(path = %path.display(), "ignoring a bad agent catalog entry")
                    }
                }
            }
        }
    }
    out
}

pub fn get(id: &str) -> Option<AgentKind> {
    all().remove(id)
}

static ACTIVE: RwLock<Option<Arc<AgentKind>>> = RwLock::new(None);

/// The agent this account runs (chosen at setup; Hermes until then).
pub fn active() -> Arc<AgentKind> {
    if let Some(k) = ACTIVE.read().ok().and_then(|a| a.clone()) {
        return k;
    }
    let k = Arc::new(get(DEFAULT).expect("the built-in Hermes entry parses"));
    set_active_kind(k.clone());
    k
}

/// Makes `id` the active agent; false (and no change) if it isn't known.
pub fn set_active(id: &str) -> bool {
    match get(id) {
        Some(k) => {
            set_active_kind(Arc::new(k));
            true
        }
        None => false,
    }
}

fn set_active_kind(k: Arc<AgentKind>) {
    if let Ok(mut a) = ACTIVE.write() {
        *a = Some(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_hermes() {
        let k = get("hermes").unwrap();
        assert_eq!(k.name, "Hermes");
        assert_eq!(k.min_context, 65536);
        assert_eq!(k.approval_mode(Preset::Careful), "manual");
        assert_eq!(k.approval_mode(Preset::HandsOff), "smart");
        assert!(k.valid());
    }

    #[test]
    fn other_agents_need_no_code() {
        let k: AgentKind = serde_json::from_str(
            r#"{"id":"picoclaw","name":"PicoClaw","image":"ghcr.io/example/umoja-picoclaw:latest"}"#,
        )
        .unwrap();
        assert!(k.valid());
        assert_eq!(k.command, vec!["/usr/local/bin/umoja-agent-start"]);
        assert_eq!(k.approval_mode(Preset::Balanced), "smart");
        assert!(!AgentKind { id: "Bad Id".into(), ..k }.valid());
    }
}

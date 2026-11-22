//! org.umoja.Agents on the session bus. Every method returns quickly; long
//! work (setup, runs) happens in tasks and reports through StateChanged.
//!
//! State is one JSON document (see state.rs) so the panel, overlay and the
//! test script all read the same thing.

use std::sync::Arc;

use zbus::{fdo, interface, object_server::SignalEmitter};

use crate::agent::Agent;
use crate::state::Preset;

pub struct Agents(pub Arc<Agent>);

fn err(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

fn preset(s: &str) -> fdo::Result<Preset> {
    Preset::parse(s).ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown preset {s:?}")))
}

#[interface(name = "org.umoja.Agents")]
impl Agents {
    /// The whole state as JSON.
    async fn get_state(&self) -> String {
        self.0.state.lock().await.json()
    }

    /// Choose the agent to set up (a catalog id from the state's `agents`,
    /// "hermes" by default). Only while none is set up.
    async fn set_agent(&self, agent: &str) -> fdo::Result<()> {
        self.0.set_agent(agent).await.map_err(err)
    }

    /// Install/prepare everything for the agent with a trust preset
    /// ("careful", "balanced", "hands-off"). Progress via StateChanged.
    async fn setup(&self, preset_name: &str) -> fdo::Result<()> {
        let p = preset(preset_name)?;
        self.0.setup(p).await;
        Ok(())
    }

    /// Hand a task to the agent; returns the task id.
    async fn submit(&self, prompt: &str) -> fdo::Result<String> {
        self.0.submit(prompt).await.map_err(err)
    }

    async fn stop(&self, task: &str) -> fdo::Result<()> {
        self.0.stop_task(task).await.map_err(err)
    }

    /// choice: "allow", "always" (network: keep the rule), or "deny".
    async fn answer(&self, approval: &str, choice: &str) -> fdo::Result<()> {
        self.0.answer(approval, choice).await.map_err(err)
    }

    /// Text answer to a Hermes question.
    async fn reply(&self, approval: &str, text: &str) -> fdo::Result<()> {
        self.0.reply(approval, text).await.map_err(err)
    }

    async fn set_paused(&self, paused: bool) -> fdo::Result<()> {
        self.0.set_paused(paused).await.map_err(err)
    }

    async fn set_preset(&self, preset_name: &str) -> fdo::Result<()> {
        self.0.set_preset(preset(preset_name)?).await.map_err(err)
    }

    /// "local" or "cloud".
    async fn set_model(&self, model: &str) -> fdo::Result<()> {
        self.0.set_model(model).await.map_err(err)
    }

    /// Share a host folder; returns its path inside the sandbox.
    async fn share_folder(&self, path: &str, writable: bool) -> fdo::Result<String> {
        self.0.share(path, writable).await.map_err(err)
    }

    async fn unshare(&self, path: &str) -> fdo::Result<()> {
        self.0.unshare(path).await.map_err(err)
    }

    /// platform "telegram" (token stored in the keyring) or "" to turn off.
    async fn set_messaging(&self, platform: &str, token: &str, allowed: &str) -> fdo::Result<()> {
        self.0.set_messaging(platform, token, allowed).await.map_err(err)
    }

    /// Delete the sandbox (the workspace folder stays).
    async fn remove(&self) -> fdo::Result<()> {
        self.0.remove().await.map_err(err)
    }

    /// Open a task's result (its files or workspace).
    async fn open(&self, task: &str) -> fdo::Result<()> {
        self.0.open(task).await.map_err(err)
    }

    /// Add the user to the lxd group (pkexec); needs a new login.
    async fn fix_lxd(&self) -> fdo::Result<()> {
        self.0.fix_lxd().await.map_err(err)
    }

    /// Test hook (only with UMOJA_AGENTS_TEST=1): what logind's
    /// PrepareForSleep(start) does, for tools/test/umoja-agents-test.
    async fn test_sleep(&self, start: bool) -> fdo::Result<()> {
        if std::env::var("UMOJA_AGENTS_TEST").as_deref() != Ok("1") {
            return Err(fdo::Error::AccessDenied("test hooks are off".into()));
        }
        if start {
            self.0.before_sleep().await;
        } else {
            self.0.after_wake().await;
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn state_changed(emitter: &SignalEmitter<'_>, state: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn approval_requested(emitter: &SignalEmitter<'_>, approval: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn task_finished(emitter: &SignalEmitter<'_>, task: &str) -> zbus::Result<()>;
}

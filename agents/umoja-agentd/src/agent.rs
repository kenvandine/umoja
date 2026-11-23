//! The agent service's logic: setting Hermes up in its sandbox, keeping
//! the sandbox and its guardrails in place, running tasks and gathering
//! every approval (OpenShell's and Hermes's) into one list for the shell.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use openshell_gateway_client::{CreateOptions, Gateway, Proposal, State as Phase};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::bridge::{self, PendingKind};
use crate::catalog;
use crate::lxd::{Access, Lxd};
use crate::paths::{self, SANDBOX_SHARED, SANDBOX_WORKSPACE};
use crate::relay;
use crate::runtime::Runtime;
use crate::state::{Approval, ApprovalKind, Messaging, Preset, Setup, Shared, State, Task, TaskStatus, now, title_for};
use crate::notify;

pub const PATH: &str = "/org/umoja/Agents";

pub struct Agent {
    pub state: Mutex<State>,
    conn: zbus::Connection,
    runtime: Mutex<Option<Runtime>>,
    gw: RwLock<Option<Gateway>>,
    relay_allowed: relay::Allowed,
    relay_running: Mutex<bool>,
    /// Open notifications: id → what its actions do.
    notes: Mutex<HashMap<u32, Note>>,
    /// Proposals as the gateway sent them, by approval id (to approve).
    proposals: Mutex<HashMap<String, Proposal>>,
    /// Consent prompts also sent to Telegram (a Telegram job's): approval
    /// id -> its buttons' key and the messages, to settle them.
    chat_prompts: Mutex<HashMap<String, ChatPrompt>>,
    /// Tasks with a watcher running.
    watching: Mutex<std::collections::HashSet<String>>,
    /// Serialises setup / pause / remove.
    lifecycle: Mutex<()>,
    /// Bumped on every wake from sleep: watchers reconnect.
    wake: std::sync::atomic::AtomicU64,
    /// The task Ask Umoja shows (Viewing), and when it last said so: its
    /// "finished" notification would only cover it.
    viewing: Mutex<(String, Option<std::time::Instant>)>,
    /// Rejected requests, to ask again when tried again (reask.rs), and
    /// when Umoja turned each down (chunk id → ms).
    rejected: Mutex<Option<crate::reask::Rejected>>,
    turned_down: Mutex<HashMap<String, i64>>,
}

#[derive(Debug, Clone, Default)]
struct ChatPrompt {
    key: String,
    messages: Value,
    host: String,
}

#[derive(Debug, Clone)]
enum Note {
    Approval(String),
    /// A finished task: the body opens its answer in Ask Umoja (to read it
    /// all and reply), the button its file or folder.
    Result(String, PathBuf),
}

type Snapshot = BTreeMap<PathBuf, SystemTime>;

impl Agent {
    pub fn new(conn: zbus::Connection) -> Arc<Self> {
        let mut state = State::load();
        if state.agent.is_empty() || !catalog::set_active(&state.agent) {
            state.agent = catalog::DEFAULT.into();
            catalog::set_active(catalog::DEFAULT);
        }
        fill_agent_info(&mut state);
        Arc::new(Self {
            state: Mutex::new(state),
            conn,
            runtime: Mutex::new(None),
            gw: RwLock::new(None),
            relay_allowed: Default::default(),
            relay_running: Mutex::new(false),
            notes: Mutex::new(HashMap::new()),
            proposals: Mutex::new(HashMap::new()),
            chat_prompts: Mutex::new(HashMap::new()),
            watching: Mutex::new(Default::default()),
            lifecycle: Mutex::new(()),
            wake: Default::default(),
            viewing: Mutex::new((String::new(), None)),
            rejected: Mutex::new(None),
            turned_down: Mutex::new(HashMap::new()),
        })
    }

    /// Save and tell the shell.
    pub async fn changed(&self) {
        let json = {
            let s = self.state.lock().await;
            s.save();
            s.json()
        };
        if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&self.conn, PATH) {
            let _ = crate::service::Agents::state_changed(&emitter, &json).await;
        }
    }

    /// Picks which agent to set up (a catalog id). Only before setup: a
    /// set-up agent is removed first (Remove), so its sandbox and tasks
    /// never mix with another's.
    pub async fn set_agent(&self, id: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        {
            let s = self.state.lock().await;
            if s.agent == id {
                return Ok(());
            }
            if matches!(s.setup, Setup::Ready | Setup::Working) {
                bail!("Remove {} first, then set up another agent.", catalog::active().name);
            }
        }
        if !catalog::set_active(id) {
            bail!("Umoja doesn't know an agent called {id}.");
        }
        {
            let mut s = self.state.lock().await;
            s.agent = id.into();
            s.tasks.clear();
            s.shared.clear();
            s.messaging = None;
            fill_agent_info(&mut s);
        }
        self.changed().await;
        Ok(())
    }

    async fn set_step(&self, step: &str) {
        tracing::info!(%step, "setup");
        self.state.lock().await.step = step.into();
        self.changed().await;
    }

    async fn gateway(&self) -> Result<Gateway> {
        self.gw
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("{n} isn't running. Set it up in Ask Umoja's settings first.", n = catalog::active().name))
    }

    /// Session end: stop the gateway and driver (the sandbox keeps its
    /// state in LXD and comes back at the next login).
    pub async fn shutdown(&self) {
        self.gw.write().await.take();
        if let Some(rt) = self.runtime.lock().await.take() {
            drop(rt);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    /// At login: bring a set-up Hermes back.
    pub async fn resume(self: &Arc<Self>) {
        let ready = self.state.lock().await.setup == Setup::Ready;
        if !ready {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = this.lifecycle.lock().await;
            if let Err(e) = this.bring_up(false).await {
                tracing::error!(error = %format!("{e:#}"), "couldn't start the agent");
                let mut s = this.state.lock().await;
                s.sandbox = "error".into();
                s.step.clear();
                s.error = for_people(&e);
                drop(s);
                this.changed().await;
            }
        });
    }

    pub async fn setup(self: &Arc<Self>, preset: Preset) {
        {
            let mut s = self.state.lock().await;
            if s.setup == Setup::Working {
                return;
            }
            s.setup = Setup::Working;
            s.preset = preset;
            s.error.clear();
            s.needs.clear();
            if s.private.api_key.is_empty() {
                s.private.api_key = random_key();
            }
        }
        self.changed().await;
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = this.lifecycle.lock().await;
            let mut result = this.bring_up(true).await;
            // This account can't use the sandbox yet: pressing Set Up is the
            // ask, so put up the password prompt now instead of failing.
            let needs = this.state.lock().await.needs.clone();
            if result.is_err() && matches!(needs.as_str(), "lxd-install" | "lxd-group") {
                this.set_step("Waiting for your permission").await;
                match this.grant_lxd().await {
                    Ok(true) => {
                        this.state.lock().await.needs.clear();
                        result = this.bring_up(true).await;
                    }
                    Ok(false) => {
                        this.state.lock().await.needs = "relogin".into();
                        result = Err(anyhow::anyhow!(
                            "Almost there. Log out and back in, then set up {n} again.",
                            n = catalog::active().name
                        ));
                    }
                    // Dismissed or failed: keep the first error and its
                    // Allow button.
                    Err(e) => tracing::info!(error = %format!("{e:#}"), "sandbox permission not given"),
                }
            }
            let mut s = this.state.lock().await;
            match result {
                Ok(()) => {
                    s.setup = Setup::Ready;
                    s.step.clear();
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "setup failed");
                    s.setup = Setup::Error;
                    s.error = for_people(&e);
                }
            }
            drop(s);
            this.changed().await;
        });
    }

    /// Everything from LXD to a healthy Hermes. `configure` (re)applies
    /// guardrails and Hermes's config (setup, preset or model changes);
    /// otherwise only what is missing is done.
    async fn bring_up(self: &Arc<Self>, configure: bool) -> Result<()> {
        let project = paths::lxd_project();
        let lxd = Lxd::new(&project);

        // An earlier start's error (saved with the state) isn't this one's.
        self.state.lock().await.error.clear();
        self.set_step("Checking this computer").await;
        match tokio::task::spawn_blocking({
            let project = project.clone();
            move || Lxd::new(&project).access()
        })
        .await?
        {
            Access::Ok => tracing::debug!("LXD access ok"),
            Access::Missing => {
                self.state.lock().await.needs = "lxd-install".into();
                bail!("{n} needs one more part installed before it can run safely.", n = catalog::active().name);
            }
            Access::Denied => {
                self.state.lock().await.needs = "lxd-group".into();
                bail!("{n} needs your permission to set up its sandbox.", n = catalog::active().name);
            }
        }
        let bridge = tokio::task::spawn_blocking(move || -> Result<_> {
            lxd.ensure_project()?;
            tracing::debug!("LXD project ok");
            lxd.bridge_address(&paths::lxd_network())
        })
        .await??;
        tracing::debug!(?bridge, "LXD bridge");

        self.set_step("Starting the guardrails").await;
        if self.runtime.lock().await.is_none() {
            // The guardrails run as this daemon's child, so a sandbox still
            // running now outlived an earlier daemon. Started again by the
            // new guardrails it would be refused ("already running
            // generation …"), so stop it first: they start it afresh, and
            // tasks it was running are carried on.
            let project = project.clone();
            match tokio::task::spawn_blocking(move || Lxd::new(&project).stop_sandbox(&paths::sandbox())).await? {
                Ok(true) => tracing::info!("stopped the sandbox left running by an earlier start"),
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "couldn't stop the old sandbox"),
            }
            let rt = Runtime::start(bridge.0).await?;
            *self.runtime.lock().await = Some(rt);
        }
        let connected = {
            let rt = self.runtime.lock().await;
            rt.as_ref().unwrap().connect(Duration::from_secs(60)).await
        };
        // Connected isn't answering: guardrails stuck on a half-made sandbox
        // answer a list and then hang on the next calls. Their first calls
        // run under one deadline; past it, they're recovered.
        let settle = |gw: Gateway| async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                let _ = gw.clone().ensure_profiles().await;
                gw.clone().list().await
            })
            .await
            .map_err(|_| anyhow!("the guardrails took calls and didn't answer"))?
            .map_err(anyhow::Error::from)
            .map(|list| (gw, list))
        };
        let (gw, list) = match connected {
            Ok(gw) => match settle(gw).await {
                Ok(ok) => ok,
                Err(e) => settle(self.recover_guardrails(bridge.0, e).await?).await?,
            },
            Err(e) => settle(self.recover_guardrails(bridge.0, e).await?).await?,
        };
        *self.gw.write().await = Some(gw.clone());

        self.ensure_relay(bridge.0).await;
        self.spawn_poller();

        let paused = self.state.lock().await.paused;
        let exists = list.into_iter().find(|s| s.name == paths::sandbox());
        match exists {
            None => {
                self.end_lost_tasks().await;
                self.set_step(&format!("Downloading {} (first time only)", catalog::active().name)).await;
                self.create_sandbox(&gw).await?;
            }
            // A new image (an Umoja update): the sandbox is made again from
            // it. The agent's memory is kept on the host (state_dir), and
            // its folder and shares are mounted again.
            Some(info) if !paused && !info.image.is_empty() && !same_image(&info.image, &paths::image()) => {
                tracing::info!(from = %info.image, to = %paths::image(), "new agent image: making the sandbox again");
                self.set_step(&format!("Updating {}", catalog::active().name)).await;
                self.end_lost_tasks().await;
                gw.clone().delete(&paths::sandbox()).await?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
                while gw.clone().list().await?.iter().any(|s| s.name == paths::sandbox()) {
                    if tokio::time::Instant::now() > deadline {
                        bail!("{n}'s old sandbox didn't go away. Restart this computer, then try again.", n = catalog::active().name);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                self.create_sandbox(&gw).await?;
            }
            Some(info) if info.state == Phase::Stopped && !paused => {
                self.set_step(&format!("Starting {n}'s sandbox", n = catalog::active().name)).await;
                gw.clone().start(&paths::sandbox()).await?;
            }
            // A failed start (an earlier try, or the guardrails refusing
            // a sandbox that was still running): stop it and start again.
            Some(info) if info.state == Phase::Failed && !paused => {
                tracing::info!(detail = %info.detail, "sandbox failed earlier, starting it again");
                self.set_step(&format!("Starting {n}'s sandbox", n = catalog::active().name)).await;
                // Stopped through the guardrails (they won't start it again
                // from a failed start otherwise), then started afresh.
                gw.clone().stop(&paths::sandbox()).await?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                loop {
                    let phase = gw.clone().list().await?.into_iter().find(|s| s.name == paths::sandbox()).map(|s| s.state);
                    if phase == Some(Phase::Stopped) {
                        break;
                    }
                    if tokio::time::Instant::now() > deadline {
                        bail!("{n}'s sandbox didn't stop. Restart this computer, then try again.", n = catalog::active().name);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                gw.clone().start(&paths::sandbox()).await?;
            }
            Some(_) => {}
        }
        if paused {
            return Ok(());
        }
        self.wait_running(&gw, Duration::from_secs(20 * 60)).await?;

        // Every start: a sandbox made again (an update) starts with no
        // rules, and adding a rule that's there already changes nothing.
        self.set_step("Applying guardrails").await;
        self.apply_policy(&gw).await?;
        self.set_step(&format!("Sharing ~/Agents/{n}", n = catalog::active().name)).await;
        let home_added = self.mount_folders().await?;

        self.set_step(&format!("Connecting {n} to the model", n = catalog::active().name)).await;
        if configure || home_added || !bridge::health(&gw).await {
            self.write_config(&gw).await?;
            bridge::restart(&gw).await.ok();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while !bridge::health(&gw).await {
            if tokio::time::Instant::now() > deadline {
                bail!("{n} didn't start. Try again in a moment.", n = catalog::active().name);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // Tasks that were running when we stopped: watch them again.
        let running: Vec<String> = self
            .state
            .lock()
            .await
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) && !t.run.is_empty())
            .map(|t| t.id.clone())
            .collect();
        for id in running {
            self.watch_task(id, Snapshot::new());
        }
        {
            let mut s = self.state.lock().await;
            s.error.clear();
            s.step.clear();
        }
        self.changed().await;
        Ok(())
    }

    /// The guardrails won't answer. Seen when they were stopped (a log out)
    /// while making the sandbox: on every start after, they wait for that
    /// half-made sandbox for ever. With no sandbox on this computer there
    /// is nothing of value in their state (the rules are applied again on
    /// every start), so it's set aside and they start afresh; the sandbox is
    /// then made again. With a sandbox present, the error stands.
    async fn recover_guardrails(&self, bridge: std::net::Ipv4Addr, error: anyhow::Error) -> Result<Gateway> {
        let project = paths::lxd_project();
        let (workload, supervisor) =
            tokio::task::spawn_blocking(move || Lxd::new(&project).sandbox(&paths::sandbox())).await??;
        if workload.is_some() || supervisor.is_some() {
            return Err(error);
        }
        tracing::warn!(error = %format!("{error:#}"), "the guardrails don't answer and there's no sandbox: starting them afresh");
        *self.runtime.lock().await = None; // stops them
        tokio::time::sleep(Duration::from_secs(2)).await;
        let db = paths::state_dir().join("gateway.db");
        let aside = paths::state_dir().join(format!("gateway.db.stuck-{}", now()));
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", db.display()));
            if from.exists() {
                std::fs::rename(&from, format!("{}{suffix}", aside.display()))?;
            }
        }
        let rt = Runtime::start(bridge).await?;
        let gw = rt.connect(Duration::from_secs(60)).await?;
        *self.runtime.lock().await = Some(rt);
        Ok(gw)
    }

    async fn wait_running(&self, gw: &Gateway, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let info = gw.clone().list().await?.into_iter().find(|s| s.name == paths::sandbox());
            match info {
                Some(i) if i.state == Phase::Running => return Ok(()),
                Some(i) if i.state == Phase::Failed => {
                    tracing::error!(detail = %i.detail, phase = %i.phase, "sandbox failed");
                    bail!("{n}'s sandbox couldn't start. Try again, or restart this computer if it keeps happening.", n = catalog::active().name)
                }
                Some(i) => {
                    let mut s = self.state.lock().await;
                    s.sandbox = i.phase.to_lowercase();
                }
                None => bail!("{n}'s sandbox disappeared", n = catalog::active().name),
            }
            if tokio::time::Instant::now() > deadline {
                bail!("{n}'s sandbox took too long to start", n = catalog::active().name);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Network rules from the preset, the model relay, messaging, and the
    /// gateway's auto-approval.
    async fn apply_policy(&self, gw: &Gateway) -> Result<()> {
        let (preset, messaging) = {
            let s = self.state.lock().await;
            (s.preset, s.messaging.clone())
        };
        let mut gw = gw.clone();
        for (host, port) in self.model_endpoints().await {
            gw.add_rule(&paths::sandbox(), &host, port, Some(ANY_PROGRAM)).await.context("model rule")?;
        }
        for (host, port) in preset.open_sites() {
            gw.add_rule(&paths::sandbox(), host, *port, Some(ANY_PROGRAM)).await?;
        }
        if messaging.as_ref().is_some_and(|m| m.platform == "telegram") {
            gw.add_rule(&paths::sandbox(), "api.telegram.org", 443, Some(ANY_PROGRAM)).await?;
        }
        if let Err(e) = gw.set_auto_approve(&paths::sandbox(), preset.auto_approve()).await {
            tracing::warn!(error = %e, "couldn't set OpenShell auto-approval");
        }
        Ok(())
    }

    /// Where Hermes's model is: the host's relay to Lemonade, or the cloud.
    async fn model_endpoints(&self) -> Vec<(String, u16)> {
        let cloud = self.state.lock().await.model == "cloud";
        if cloud {
            if let Some((url, _, _)) = cloud_model() {
                if let Some((host, port)) = host_port(&url) {
                    return vec![(host, port)];
                }
            }
        }
        vec![("host.openshell.internal".into(), paths::ports().1)]
    }

    async fn write_config(&self, gw: &Gateway) -> Result<()> {
        let s = self.state.lock().await.clone();
        let (url, model, key) = if s.model == "cloud" {
            cloud_model().ok_or_else(|| anyhow!("No cloud model is set up in Ask Umoja's settings."))?
        } else {
            (
                format!("http://host.openshell.internal:{}/api/v1", paths::ports().1),
                routed_model().unwrap_or_else(local_model),
                "lemonade".to_owned(),
            )
        };
        let local = s.model != "cloud";
        let mut env = vec![
            ("UMOJA_MODEL_URL", url),
            ("UMOJA_MODEL", model),
            ("UMOJA_MODEL_KEY", key),
            ("UMOJA_APPROVALS", catalog::active().approval_mode(s.preset)),
            ("UMOJA_AGENT_KEY", s.private.api_key.clone()),
        ];
        // Always say how big the context is: otherwise Hermes looks the model
        // up on OpenRouter, a site the sandbox would have to ask for.
        if local {
            env.push(("UMOJA_MODEL_CONTEXT", context_for_agent().to_string()));
        }
        let token;
        if let Some(m) = &s.messaging {
            if m.platform == "telegram" {
                token = read_secret("telegram").unwrap_or_default();
                env.push(("UMOJA_TELEGRAM_TOKEN", token));
                env.push(("UMOJA_TELEGRAM_ALLOWED", m.allowed.clone()));
            }
        }
        let text: String = env
            .iter()
            .map(|(k, v)| format!("{k}={}\n", shell_quote(v)))
            .collect();
        bridge::write_file(gw, bridge::ENV_FILE, &text, 0o600).await
    }

    async fn ensure_relay(&self, bind: std::net::Ipv4Addr) {
        let mut running = self.relay_running.lock().await;
        if *running {
            return;
        }
        *running = true;
        let allowed = self.relay_allowed.clone();
        let (_, port) = paths::ports();
        let target = paths::lemonade_port();
        tokio::spawn(async move {
            if let Err(e) = relay::serve(bind, port, target, allowed).await {
                tracing::error!(error = %e, "model relay stopped");
            }
        });
    }

    async fn create_sandbox(&self, gw: &Gateway) -> Result<()> {
        let mem = paths::memory_cap_mb();
        gw.clone()
            .create(
                &paths::sandbox(),
                &CreateOptions {
                    image: paths::image(),
                    memory_mb: mem,
                    command: catalog::active().command.clone(),
                    labels: vec![("umoja.agent".into(), catalog::active().id.clone())],
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("Couldn't create {n}'s sandbox", n = catalog::active().name))?;
        self.state.lock().await.memory_mb = mem;
        Ok(())
    }

    /// Returns true when the agent's home was just mounted (the agent must
    /// restart to use it).
    async fn mount_folders(&self) -> Result<bool> {
        let workspace = paths::workspace();
        std::fs::create_dir_all(&workspace)?;
        let state_dir = catalog::active().state_dir.clone();
        let home = paths::agent_home();
        if !state_dir.is_empty() {
            std::fs::create_dir_all(&home)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
        }
        // A shared folder that was deleted or moved since is let go, so it
        // can't stop the agent from starting.
        let shared = {
            let mut s = self.state.lock().await;
            let before = s.shared.len();
            s.shared.retain(|x| {
                let there = Path::new(&x.path).is_dir();
                if !there {
                    tracing::info!(path = %x.path, "shared folder is gone, no longer sharing it");
                }
                there
            });
            if s.shared.len() != before {
                s.save();
            }
            s.shared.clone()
        };
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let lxd = Lxd::new(&paths::lxd_project());
            let (workload, _) = lxd.sandbox(&paths::sandbox())?;
            let workload = workload.ok_or_else(|| anyhow!("can't find {n}'s container in LXD", n = catalog::active().name))?;
            let mut wanted = vec![(
                "umoja-workspace".to_owned(),
                workspace.display().to_string(),
                SANDBOX_WORKSPACE.to_owned(),
                false,
            )];
            if !state_dir.is_empty() {
                wanted.push((HOME_DEVICE.to_owned(), home.display().to_string(), state_dir.clone(), false));
            }
            for s in &shared {
                wanted.push((device_for(&s.inside), s.path.clone(), s.inside.clone(), !s.writable));
            }
            // Shares that were let go (or forgotten while LXD was out of
            // reach) leave the container too.
            for device in workload.devices.keys() {
                if device.starts_with("umoja-share") && !wanted.iter().any(|w| &w.0 == device) {
                    if let Err(e) = lxd.remove_device(&workload.name, device) {
                        tracing::warn!(device, error = %format!("{e:#}"), "couldn't remove an old share");
                    }
                }
            }
            let mut home_added = false;
            for (device, source, path, readonly) in wanted {
                let have = workload.devices.get(&device);
                if have.is_some_and(|d| d.get("source") == Some(&source)) {
                    continue;
                }
                lxd.add_disk(&workload.name, &device, &source, &path, readonly)
                    .with_context(|| format!("Couldn't share {source} with {n}", n = catalog::active().name))?;
                home_added |= device == HOME_DEVICE;
            }
            Ok(home_added)
        })
        .await?
    }

    /// Every 1.5 s: the sandbox's phase, OpenShell's pending proposals,
    /// which supervisors may use the model relay, and memory use.
    fn spawn_poller(self: &Arc<Self>) {
        let this = self.clone();
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let chat = self.clone();
        tokio::spawn(async move {
            let mut tick: u64 = 0;
            loop {
                tick += 1;
                if let Err(e) = this.poll(tick).await {
                    tracing::debug!(error = %e, "poll");
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        });
        tokio::spawn(async move {
            loop {
                if let Err(e) = chat.poll_chat().await {
                    tracing::debug!(error = %e, "chat poll");
                }
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });
    }

    /// Messages from Telegram go straight to the agent's own chat gateway,
    /// not through Submit, so this follows along: while the gateway is busy
    /// with a chat turn there's a task for it (the top bar, Quick Settings
    /// and the local model's idle unload see the agent working), and when
    /// the turn ends the task gets its answer.
    async fn poll_chat(self: &Arc<Self>) -> Result<()> {
        let (on, current) = {
            let s = self.state.lock().await;
            let on = s.setup == Setup::Ready
                && !s.paused
                && s.switching.is_empty()
                && s.sandbox == "running"
                && s.messaging.as_ref().is_some_and(|m| m.platform == "telegram");
            let current = s
                .tasks
                .iter()
                .find(|t| t.source == "telegram" && matches!(t.status, TaskStatus::Running | TaskStatus::Waiting))
                .map(|t| (t.id.clone(), t.session.clone()));
            (on, current)
        };
        if !on && current.is_none() && self.chat_prompts.lock().await.is_empty() {
            return Ok(());
        }
        let gw = self.gateway().await?;
        if on && let Err(e) = self.chat_answers(&gw).await {
            tracing::debug!(error = %e, "chat answers");
        }
        let busy = on && {
            let v = bridge::call(&gw, &["call", "GET", "/health/detailed"], None).await?;
            v["body"]["active_agents"].as_u64().unwrap_or(0) > 0
        };
        match (busy, current) {
            (true, None) => {
                let session = bridge::call(&gw, &["call", "GET", "/api/sessions?source=telegram&limit=1"], None)
                    .await
                    .ok()
                    .and_then(|v| v["body"]["data"][0]["id"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let asked = if session.is_empty() {
                    String::new()
                } else {
                    last_message(&gw, &session, "user").await
                };
                let prompt = if asked.trim().is_empty() { "A message from Telegram".to_owned() } else { asked };
                let shared = self.state.lock().await.shared.clone();
                let task = Task {
                    id: format!("t{}", now_ms()),
                    title: title_for(&host_words(&prompt, &shared)),
                    prompt,
                    source: "telegram".into(),
                    session,
                    status: TaskStatus::Running,
                    step: "Answering you in Telegram…".into(),
                    started: now(),
                    ..Default::default()
                };
                {
                    let mut s = self.state.lock().await;
                    s.tasks.push(task);
                    s.trim_tasks();
                }
                self.changed().await;
            }
            (false, Some((id, session))) => {
                let answer = if session.is_empty() || !on {
                    String::new()
                } else {
                    last_message(&gw, &session, "assistant").await
                };
                {
                    let mut s = self.state.lock().await;
                    let shared = s.shared.clone();
                    let stopped = !on;
                    if let Some(t) = s.task_mut(&id) {
                        t.status = if stopped { TaskStatus::Cancelled } else { TaskStatus::Done };
                        t.step = if stopped { "Stopped".into() } else { "Answered in Telegram".into() };
                        t.result = host_words(&answer, &shared).chars().take(4000).collect();
                        t.finished = now();
                    }
                }
                self.changed().await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn poll(self: &Arc<Self>, tick: u64) -> Result<()> {
        let Ok(gw) = self.gateway().await else { return Ok(()) };
        let snapshot = gw.clone().snapshot().await?;
        let mine = snapshot.into_iter().find(|s| s.info.name == paths::sandbox());
        let mut changed = false;

        let phase = mine
            .as_ref()
            .map(|s| match s.info.state {
                Phase::Running => "running".to_owned(),
                Phase::Stopped => "stopped".to_owned(),
                Phase::Failed => "error".to_owned(),
                Phase::Transitioning => s.info.phase.to_lowercase(),
            })
            .unwrap_or_default();
        let mut proposals = mine.map(|s| s.proposals).unwrap_or_default();
        // Turned down earlier and tried again since: asked again (the
        // guardrails themselves never would).
        // Every poll, like the pending ones: one missing from a poll would
        // count as answered and its card would go.
        if phase == "running" {
            for p in self.tried_again().await {
                if !proposals.iter().any(|x| x.chunk_id == p.chunk_id) {
                    proposals.push(p);
                }
            }
        }
        // Sites the agent reaches for on its own (update checks, catalogs)
        // stay blocked without asking the user.
        let quiet = catalog::active().quiet_hosts.clone();
        let telegram = self.state.lock().await.messaging.as_ref().is_some_and(|m| m.platform == "telegram");
        let is_quiet = |host: &str| quiet.iter().any(|h| h.eq_ignore_ascii_case(host)) || (telegram && telegram_host(host));
        for p in proposals.iter().filter(|p| is_quiet(&p.host)) {
            tracing::debug!(host = %p.host, "turning down the agent's own background fetch");
            let _ = self.reject(&gw, &p.chunk_id).await;
        }
        proposals.retain(|p| !is_quiet(&p.host));

        // Supervisors allowed on the model relay, and memory, every ~6 s.
        let mut memory = None;
        if tick % 4 == 1 {
            let found = tokio::task::spawn_blocking(|| -> Result<_> {
                let lxd = Lxd::new(&paths::lxd_project());
                let instances = lxd.instances()?;
                let sups: Vec<_> = instances
                    .iter()
                    .filter(|i| i.name.starts_with("openshell-sup-"))
                    .filter_map(|i| i.ipv4())
                    .collect();
                let (workload, _) = lxd.sandbox(&paths::sandbox())?;
                let mem = workload.and_then(|w| lxd.memory_usage(&w.name));
                Ok((sups, mem))
            })
            .await?;
            if let Ok((sups, mem)) = found {
                *self.relay_allowed.write().await = sups;
                memory = mem;
            }
        }

        let mut s = self.state.lock().await;
        if s.sandbox != phase {
            s.sandbox = phase;
            changed = true;
        }
        if let Some(bytes) = memory {
            let mb = (bytes / (1024 * 1024)) as u32;
            if mb.abs_diff(s.memory_used_mb) > 8 {
                s.memory_used_mb = mb;
                changed = true;
            }
        }
        let task = s
            .tasks
            .iter()
            .rev()
            .find(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Waiting))
            .map(|t| (t.id.clone(), t.title.clone()));
        // New proposals → approvals; answered ones go away.
        let ids: Vec<String> = proposals.iter().map(|p| format!("net-{}", p.chunk_id)).collect();
        let mut fresh = Vec::new();
        {
            let mut known = self.proposals.lock().await;
            for p in &proposals {
                let id = format!("net-{}", p.chunk_id);
                if known.contains_key(&id) {
                    // The gateway re-reviews a pending proposal now and
                    // then, which gives it a new review token: keep the
                    // latest, or answering it is refused.
                    known.insert(id, p.clone());
                    continue;
                }
                known.insert(id.clone(), p.clone());
                let what = task.as_ref().map(|(_, t)| format!(" for “{t}”")).unwrap_or_default();
                let approval = Approval {
                    id: id.clone(),
                    kind: ApprovalKind::Network,
                    task: task.as_ref().map(|(i, _)| i.clone()).unwrap_or_default(),
                    title: format!("{n} wants to reach {}", p.host, n = catalog::active().name),
                    detail: format!(
                        "{n} tried to reach {}{what}. Umoja held it back because it isn't one of the sites {n} may use.",
                        p.host,
                        n = catalog::active().name,
                    ),
                    host: p.host.clone(),
                    port: p.port,
                    binary: p.binary.clone(),
                    flagged: p.rationale.to_lowercase().contains("security"),
                    created: now(),
                    answer_key: (p.chunk_id.clone(), p.review_token.clone()),
                    ..Default::default()
                };
                fresh.push(approval.clone());
                s.approvals.push(approval);
                if let Some(t) = task.as_ref().and_then(|(i, _)| s.tasks.iter_mut().find(|t| &t.id == i)) {
                    t.status = TaskStatus::Waiting;
                    let site = format!("{}:{}", p.host, p.port);
                    if !t.sites.contains(&site) {
                        t.sites.push(site);
                    }
                }
                changed = true;
            }
            known.retain(|id, _| ids.contains(id));
        }
        let before = s.approvals.len();
        let vanished: Vec<String> = s
            .approvals
            .iter()
            .filter(|a| a.kind == ApprovalKind::Network && !ids.contains(&a.id))
            .map(|a| a.id.clone())
            .collect();
        s.approvals
            .retain(|a| a.kind != ApprovalKind::Network || ids.contains(&a.id));
        if s.approvals.len() != before {
            changed = true;
            refresh_waiting(&mut s);
        }
        // A Telegram job's (or, with no job of Umoja's running, one the
        // chat turn raised before poll_chat noticed it).
        let chatting = s.messaging.as_ref().is_some_and(|m| m.platform == "telegram");
        let from_chat: Vec<bool> = fresh
            .iter()
            .map(|a| {
                chatting
                    && (a.task.is_empty() || s.tasks.iter().any(|t| t.id == a.task && t.source == "telegram"))
            })
            .collect();
        drop(s);
        for id in vanished {
            self.settle_chat(&id, "No longer needed.");
            self.close_note_for(&id).await;
        }
        for (a, chat) in fresh.into_iter().zip(from_chat) {
            self.announce(&a).await;
            if chat {
                self.ask_in_chat(&a);
            }
        }
        if changed {
            self.changed().await;
        }
        Ok(())
    }

    /// A Telegram job's consent prompt, in Telegram too, with buttons;
    /// Hermes's Telegram adapter (umoja-patch-hermes) records the tap and
    /// poll_chat answers it.
    fn ask_in_chat(self: &Arc<Self>, a: &Approval) {
        let this = self.clone();
        let a = a.clone();
        tokio::spawn(async move {
            let Ok(gw) = this.gateway().await else { return };
            let key = chat_key(&a.id);
            let n = catalog::active().name.clone();
            let text = format!(
                "{n} wants to reach {host}. It can't until you say so.",
                host = a.host
            );
            let mut buttons = vec![("Allow once", "allow")];
            if !a.flagged {
                buttons.push(("Always allow", "always"));
            }
            buttons.push(("Deny", "deny"));
            let body = serde_json::json!({ "key": key, "text": text, "buttons": buttons });
            match bridge::call(&gw, &["chat-ask"], Some(&body)).await {
                Ok(v) => {
                    // Answered on the computer meanwhile: nothing to keep.
                    if this.state.lock().await.approvals.iter().any(|x| x.id == a.id) {
                        this.chat_prompts.lock().await.insert(
                            a.id.clone(),
                            ChatPrompt { key, messages: v["messages"].clone(), host: a.host.clone() },
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "couldn't send the prompt to Telegram"),
            }
        });
    }

    /// The Telegram copy of approval `id` loses its buttons (answered on
    /// the computer, or gone).
    fn settle_chat(self: &Arc<Self>, id: &str, text: &str) {
        let this = self.clone();
        let id = id.to_owned();
        let text = text.to_owned();
        tokio::spawn(async move {
            let Some(p) = this.chat_prompts.lock().await.remove(&id) else { return };
            let Ok(gw) = this.gateway().await else { return };
            let n = catalog::active().name.clone();
            let body = serde_json::json!({
                "messages": p.messages,
                "text": format!("{n} wanted to reach {}. {text}", p.host),
            });
            let _ = bridge::call(&gw, &["chat-settle"], Some(&body)).await;
        });
    }

    /// Button taps in Telegram: answer those prompts as the desktop would.
    async fn chat_answers(self: &Arc<Self>, gw: &Gateway) -> Result<()> {
        if self.chat_prompts.lock().await.is_empty() {
            return Ok(());
        }
        let v = bridge::call(gw, &["chat-answers"], None).await?;
        for ans in v["answers"].as_array().cloned().unwrap_or_default() {
            let key = ans["key"].as_str().unwrap_or_default();
            let choice = ans["choice"].as_str().unwrap_or_default().to_owned();
            let found = {
                let mut prompts = self.chat_prompts.lock().await;
                let id = prompts.iter().find(|(_, p)| p.key == key).map(|(id, _)| id.clone());
                // Settled in Telegram already (the tap edited the message).
                id.and_then(|id| prompts.remove(&id).map(|p| (id, p)))
            };
            let Some((id, prompt)) = found else { continue };
            let n = catalog::active().name.clone();
            let say = match self.answer(&id, &choice).await {
                Ok(()) if choice == "deny" => None,
                Ok(()) => {
                    let chatting = self
                        .state
                        .lock()
                        .await
                        .tasks
                        .iter()
                        .any(|t| t.source == "telegram" && matches!(t.status, TaskStatus::Running | TaskStatus::Waiting));
                    // Mid-answer it may try again by itself; after, it gave up.
                    (!chatting).then(|| format!("{n} can reach {} now. Ask again and it will use it.", prompt.host))
                }
                Err(e) => Some(e.to_string()),
            };
            if let Some(text) = say {
                let _ = bridge::call(gw, &["chat-say"], Some(&serde_json::json!({ "text": text }))).await;
            }
        }
        Ok(())
    }

    /// ApprovalRequested + a notification with Allow/Deny.
    async fn announce(&self, a: &Approval) {
        if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&self.conn, PATH) {
            let _ = crate::service::Agents::approval_requested(
                &emitter,
                &serde_json::to_string(a).unwrap_or_default(),
            )
            .await;
        }
        let actions: &[(&str, &str)] = match a.kind {
            ApprovalKind::Question => &[("default", "Answer")],
            _ => &[("allow", "Allow"), ("deny", "Deny")],
        };
        let body = if a.command.is_empty() {
            a.detail.clone()
        } else {
            format!("{}\n\n{}", a.command, a.detail)
        };
        let id = notify::send(&self.conn, &a.title, &body, actions, true).await;
        if id != 0 {
            self.notes.lock().await.insert(id, Note::Approval(a.id.clone()));
            self.state.lock().await.private.approval_notes.push(id);
        }
    }

    /// Approval notifications an earlier run left on screen: their
    /// actions reach nobody now, and what's still waiting is asked again.
    pub async fn close_stale_notes(&self) {
        let ids = std::mem::take(&mut self.state.lock().await.private.approval_notes);
        for id in ids {
            notify::close(&self.conn, id).await;
        }
    }

    async fn close_note_for(&self, approval: &str) {
        let ids: Vec<u32> = self
            .notes
            .lock()
            .await
            .iter()
            .filter(|(_, n)| matches!(n, Note::Approval(a) if a == approval))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.notes.lock().await.remove(&id);
            self.state.lock().await.private.approval_notes.retain(|n| *n != id);
            notify::close(&self.conn, id).await;
        }
    }

    pub async fn on_action(self: &Arc<Self>, id: u32, key: &str) {
        let note = self.notes.lock().await.get(&id).cloned();
        match note {
            Some(Note::Approval(a)) if key == "allow" || key == "deny" => {
                if let Err(e) = self.answer(&a, key).await {
                    tracing::warn!(error = %e, "answer from notification");
                }
            }
            Some(Note::Approval(_)) => {
                // "Answer": open the panel's Agents card.
                let _ = self
                    .conn
                    .call_method(
                        Some("org.umoja.Panel"),
                        "/org/umoja/Panel",
                        Some("org.umoja.Panel"),
                        "OpenMenu",
                        &("agents",),
                    )
                    .await;
            }
            Some(Note::Result(task, path)) => {
                self.notes.lock().await.remove(&id);
                // Both open the task in Ask Umoja (answer, Open file, reply);
                // the file itself only when Ask Umoja isn't there.
                let _ = key;
                if !show_in_ask(&self.conn, &task).await {
                    let _ = tokio::process::Command::new("gio").arg("open").arg(&path).spawn();
                }
            }
            None => {}
        }
    }

    // ---- tasks ----

    pub async fn submit(self: &Arc<Self>, prompt: &str) -> Result<String> {
        self.start_task(prompt, None).await
    }

    /// Carries on from task `parent` in the same conversation: "make it
    /// cheaper", "add a day in the desert". A new task, so it has its own
    /// progress, answer and files.
    pub async fn follow_up(self: &Arc<Self>, parent: &str, text: &str) -> Result<String> {
        let session = {
            let s = self.state.lock().await;
            let t = s.tasks.iter().find(|t| t.id == parent).ok_or_else(|| anyhow!("That job isn't there any more."))?;
            t.session.clone()
        };
        self.start_task(text, Some((parent.to_owned(), session))).await
    }

    async fn start_task(self: &Arc<Self>, prompt: &str, parent: Option<(String, String)>) -> Result<String> {
        let prompt = prompt.trim().trim_start_matches("@hermes").trim().to_owned();
        if prompt.is_empty() {
            bail!("Tell {n} what to do.", n = catalog::active().name);
        }
        let (paused, setup) = {
            let s = self.state.lock().await;
            (s.paused, s.setup.clone())
        };
        if setup != Setup::Ready {
            bail!("{n} isn't set up yet. Set it up in Ask Umoja's settings.", n = catalog::active().name);
        }
        if paused {
            bail!("Agents are paused. Resume them from the panel first.");
        }
        let gw = self.gateway().await?;
        let before = snapshot(&paths::workspace());
        let id = format!("t{}", now_ms());
        let shared = self.state.lock().await.shared.clone();
        let task = Task {
            id: id.clone(),
            title: title_for(&host_words(&prompt, &shared)),
            prompt: prompt.clone(),
            parent: parent.as_ref().map(|(p, _)| p.clone()).unwrap_or_default(),
            session: parent.as_ref().map(|(_, s)| s.clone()).unwrap_or_default(),
            status: TaskStatus::Running,
            step: "Starting…".into(),
            started: now(),
            ..Default::default()
        };
        {
            let mut s = self.state.lock().await;
            s.tasks.push(task);
            s.trim_tasks();
        }
        self.changed().await;
        if self.state.lock().await.model != "cloud" {
            preload_local_model().await;
        }
        // Just after a restart the sandbox is still coming up: wait for it
        // (within the caller's D-Bus timeout) instead of failing the task.
        if !bridge::health(&gw).await {
            if let Some(t) = self.state.lock().await.task_mut(&id) {
                t.step = format!("Waiting for {n} to start…", n = catalog::active().name);
            }
            self.changed().await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
            while !bridge::health(&gw).await && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        let instructions = {
            let shared = self.state.lock().await.shared.clone();
            bridge::instructions(&catalog::active().name, &shared)
        };
        let session = parent.as_ref().map(|(_, s)| s.as_str()).filter(|s| !s.is_empty());
        match bridge::submit(&gw, &prompt, &instructions, session).await {
            Ok((run, session)) => {
                if let Some(t) = self.state.lock().await.task_mut(&id) {
                    t.run = run;
                    t.session = session;
                    t.step = "Thinking…".into();
                }
                self.changed().await;
                self.watch_task(id.clone(), before);
                Ok(id)
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't start the task");
                self.finish(&id, TaskStatus::Failed, &for_people(&e), Vec::new()).await;
                Err(e)
            }
        }
    }

    /// Follows a task's run: its event stream for live steps, and a status
    /// poll for approvals, questions and the end.
    fn watch_task(self: &Arc<Self>, id: String, before: Snapshot) {
        let this = self.clone();
        tokio::spawn(async move {
            if !this.watching.lock().await.insert(id.clone()) {
                return;
            }
            this.follow(&id, &before).await;
            this.watching.lock().await.remove(&id);
        });
    }

    /// Live steps from the run's event stream; `activity` notes when the
    /// last one came and whether a tool is running (for stall checks).
    fn stream_events(
        self: &Arc<Self>,
        gw: Gateway,
        id: &str,
        run: &str,
        activity: Arc<std::sync::Mutex<Activity>>,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        let (id, run) = (id.to_owned(), run.to_owned());
        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let stream = tokio::spawn(async move {
                let _ = bridge::bridge(&gw, &["events", &run], None, |v| {
                    let _ = tx.send(v);
                })
                .await;
            });
            while let Some(ev) = rx.recv().await {
                if let Ok(mut a) = activity.lock() {
                    a.last = tokio::time::Instant::now();
                    match ev["event"].as_str().unwrap_or_default() {
                        "tool.started" => a.in_tool = true,
                        "tool.completed" => a.in_tool = false,
                        _ => {}
                    }
                }
                this.on_event(&id, &ev).await;
            }
            let _ = stream.await;
        })
    }

    async fn follow(self: &Arc<Self>, id: &str, before: &Snapshot) {
        use std::sync::atomic::Ordering;
        let (mut run, interrupted) = {
            let s = self.state.lock().await;
            match s.tasks.iter().find(|t| t.id == id) {
                Some(t) => (t.run.clone(), t.interrupted),
                None => return,
            }
        };
        let activity = Arc::new(std::sync::Mutex::new(Activity { last: tokio::time::Instant::now(), in_tool: false }));
        let mut events: Option<tokio::task::JoinHandle<()>> = None;
        let mut wake_seen = self.wake.load(Ordering::SeqCst);
        let mut restream = true;
        // When it last came back from an interruption (sleep, restart).
        let mut woke = interrupted.then(tokio::time::Instant::now);
        // We stopped a stalled run to carry it on.
        let mut restarting = false;
        let mut seen_pending: std::collections::HashSet<String> = Default::default();
        let mut failures = 0;
        loop {
            let wake = self.wake.load(Ordering::SeqCst);
            if wake != wake_seen {
                wake_seen = wake;
                woke = Some(tokio::time::Instant::now());
                failures = 0;
                restream = true;
            }
            if restream {
                if let Some(e) = events.take() {
                    e.abort();
                }
                if let Ok(mut a) = activity.lock() {
                    a.last = tokio::time::Instant::now();
                }
                if let Ok(gw) = self.gateway().await {
                    events = Some(self.stream_events(gw, id, &run, activity.clone()));
                    restream = false;
                }
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            // Ended elsewhere (Pause, Remove): nothing left to follow.
            let ended = self
                .state
                .lock()
                .await
                .tasks
                .iter()
                .find(|t| t.id == id)
                .is_none_or(|t| !matches!(t.status, TaskStatus::Running | TaskStatus::Waiting));
            if ended {
                break;
            }
            // Running fine a while after waking up: no longer interrupted.
            if woke.is_some_and(|w| w.elapsed() > CARRY_ON_WINDOW) {
                woke = None;
                if let Some(t) = self.state.lock().await.task_mut(id) {
                    t.interrupted = false;
                }
            }
            let r = match self.gateway().await {
                Ok(gw) => bridge::status(&gw, &run).await,
                Err(e) => Err(e),
            };
            let r = match r {
                Ok(r) => {
                    failures = 0;
                    r
                }
                Err(e) => {
                    failures += 1;
                    if failures > 40 {
                        if let Some(new) = self.carry_on(id).await {
                            (run, restream, failures, restarting) = (new, true, 0, false);
                            seen_pending.clear();
                            continue;
                        }
                        self.finish(id, TaskStatus::Failed, &format!("Lost track of {}: {e:#}", catalog::active().name), Vec::new())
                            .await;
                        break;
                    }
                    continue;
                }
            };
            let status = r.status.clone();
            if let Some(session) = Some(r.session.as_str()).filter(|s| !s.is_empty()) {
                let mut s = self.state.lock().await;
                if let Some(t) = s.task_mut(id) {
                    if t.session != session {
                        t.session = session.to_owned();
                    }
                }
            }
            for p in r.pending.clone() {
                let aid = format!("{}-{}", match p.kind { PendingKind::Approval => "cmd", PendingKind::Clarify => "ask" }, p.request_id);
                if !seen_pending.insert(aid.clone()) {
                    continue;
                }
                let title = self.state.lock().await.task_mut(id).map(|t| t.title.clone()).unwrap_or_default();
                let approval = match p.kind {
                    PendingKind::Approval => Approval {
                        id: aid.clone(),
                        kind: ApprovalKind::Command,
                        task: id.to_owned(),
                        title: format!("{n} wants to run a command", n = catalog::active().name),
                        detail: if p.detail.is_empty() {
                            format!("For “{title}”. Its safety rules want you to OK this first.")
                        } else {
                            format!("For “{title}”: {}", p.detail)
                        },
                        command: p.text.clone(),
                        created: now(),
                        answer_key: (p.request_id.clone(), p.generation.clone()),
                        ..Default::default()
                    },
                    PendingKind::Clarify => Approval {
                        id: aid.clone(),
                        kind: ApprovalKind::Question,
                        task: id.to_owned(),
                        title: format!("{n} asks: {}", p.text, n = catalog::active().name),
                        detail: format!("For “{title}”."),
                        created: now(),
                        answer_key: (p.request_id.clone(), p.generation.clone()),
                        ..Default::default()
                    },
                };
                {
                    let mut s = self.state.lock().await;
                    s.approvals.push(approval.clone());
                    if let Some(t) = s.task_mut(id) {
                        t.status = TaskStatus::Waiting;
                    }
                }
                self.announce(&approval).await;
                self.changed().await;
            }
            let terminal = match status.as_str() {
                "completed" => Some(TaskStatus::Done),
                "failed" | "interrupted" | "gone" => Some(TaskStatus::Failed),
                "cancelled" => Some(TaskStatus::Cancelled),
                _ => None,
            };
            if let Some(end) = terminal {
                // Died coming back from sleep (a dropped model connection,
                // Hermes restarted): carry on in the same conversation.
                if end == TaskStatus::Failed || (end == TaskStatus::Cancelled && restarting) {
                    if let Some(new) = self.carry_on(id).await {
                        (run, restream, failures, restarting) = (new, true, 0, false);
                        seen_pending.clear();
                        continue;
                    }
                }
                let name = catalog::active().name.clone();
                let mut text = r.output.clone();
                if text.is_empty() && end == TaskStatus::Failed {
                    text = if status == "gone" {
                        format!("{name} restarted and lost this task. Ask again to start it over.")
                    } else if !r.error.is_empty() {
                        r.error.clone()
                    } else {
                        format!("{name} stopped with an error.")
                    };
                }
                let files = changed_files(before, &snapshot(&paths::workspace()));
                self.finish(id, end, &text, files).await;
                break;
            }
            if status == "running" {
                let waiting = {
                    let mut s = self.state.lock().await;
                    let waiting = s.approvals.iter().any(|a| a.task == id);
                    if let Some(t) = s.task_mut(id) {
                        if t.status == TaskStatus::Waiting && !waiting {
                            t.status = TaskStatus::Running;
                            drop(s);
                            self.changed().await;
                        }
                    }
                    waiting
                };
                // After a wake, a run that went silent outside any tool is
                // most likely stuck on a model connection that died while
                // the computer slept: stop it and carry on.
                let stall_after = self.stall_after();
                let stalled = activity.lock().map(|a| !a.in_tool && a.last.elapsed() > stall_after).unwrap_or(false);
                if !restarting && !waiting && woke.is_some() && stalled {
                    tracing::info!(task = id, "run stalled after wake; carrying it on");
                    if let Ok(gw) = self.gateway().await {
                        restarting = bridge::stop(&gw, &run).await.is_ok();
                    }
                }
            }
        }
        if let Some(e) = events {
            e.abort();
        }
    }

    /// Starts a new run that carries on an interrupted task in the same
    /// Hermes conversation. None when the task wasn't interrupted, or was
    /// carried on enough already.
    async fn carry_on(self: &Arc<Self>, id: &str) -> Option<String> {
        let (prompt, session, log) = {
            let s = self.state.lock().await;
            let t = s.tasks.iter().find(|t| t.id == id)?;
            if s.paused || !t.interrupted || t.resumes >= MAX_CARRY_ON || t.status == TaskStatus::Cancelled {
                return None;
            }
            (t.prompt.clone(), t.session.clone(), t.log.clone())
        };
        let gw = self.gateway().await.ok()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        while !bridge::health(&gw).await {
            if tokio::time::Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if self.state.lock().await.model != "cloud" {
            preload_local_model().await;
        }
        let text = carry_on_text(&prompt, &log, !session.is_empty());
        let instructions = {
            let shared = self.state.lock().await.shared.clone();
            bridge::instructions(&catalog::active().name, &shared)
        };
        match bridge::submit(&gw, &text, &instructions, Some(&session)).await {
            Ok((run, new_session)) => {
                tracing::info!(task = id, run = %run, "carrying on after an interruption");
                if let Some(t) = self.state.lock().await.task_mut(id) {
                    t.run = run.clone();
                    if !new_session.is_empty() {
                        t.session = new_session;
                    }
                    t.resumes += 1;
                    t.interrupted = false;
                    t.status = TaskStatus::Running;
                    t.step = "Picking up where it left off…".into();
                }
                self.changed().await;
                Some(run)
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't carry on");
                None
            }
        }
    }

    /// How long a run may stay silent after a wake before it counts as
    /// stuck: local models can take a while for the first token.
    fn stall_after(&self) -> Duration {
        let cloud = self.state.try_lock().map(|s| s.model == "cloud").unwrap_or(false);
        if cloud { Duration::from_secs(90) } else { Duration::from_secs(240) }
    }

    /// The computer is about to sleep: note what's in flight.
    pub async fn before_sleep(&self) {
        let mut any = false;
        for t in self.state.lock().await.tasks.iter_mut() {
            if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) && t.source.is_empty() {
                t.interrupted = true;
                any = true;
            }
        }
        if any {
            tracing::info!("going to sleep with tasks in flight");
            self.changed().await;
        }
    }

    /// Back from sleep: reconnect the watchers, make sure the sandbox and
    /// Hermes answer, and warm the local model again.
    pub async fn after_wake(self: &Arc<Self>) {
        self.wake.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (ready, paused, busy, local) = {
            let s = self.state.lock().await;
            (
                s.setup == Setup::Ready,
                s.paused,
                s.tasks.iter().any(|t| t.interrupted),
                s.model != "cloud",
            )
        };
        if !ready || paused || !busy {
            return;
        }
        tracing::info!("woke up with tasks in flight");
        let this = self.clone();
        tokio::spawn(async move {
            if local {
                preload_local_model().await;
            }
            let healthy = match this.gateway().await {
                Ok(gw) => {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                    loop {
                        if bridge::health(&gw).await {
                            break true;
                        }
                        if tokio::time::Instant::now() > deadline {
                            break false;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                Err(_) => false,
            };
            if !healthy {
                tracing::warn!("Hermes didn't answer after wake; bringing it back up");
                let _guard = this.lifecycle.lock().await;
                if let Err(e) = this.bring_up(false).await {
                    tracing::error!(error = %format!("{e:#}"), "couldn't bring Hermes back after wake");
                }
            }
        });
    }

    async fn on_event(&self, id: &str, ev: &Value) {
        let name = ev["event"].as_str().unwrap_or_default();
        let data = &ev["data"];
        let mut s = self.state.lock().await;
        let Some(t) = s.task_mut(id) else { return };
        match name {
            "tool.started" => {
                let tool = data["tool"].as_str().unwrap_or("a tool").to_owned();
                let preview = data["preview"].as_str().unwrap_or_default();
                t.step = step_text(&tool, preview);
                t.steps += 1;
                t.log.push(t.step.clone());
                if t.log.len() > 12 {
                    t.log.remove(0);
                }
                t.tools.push(tool);
                for host in hosts_in(preview) {
                    if !t.sites.contains(&host) {
                        t.sites.push(host);
                    }
                }
            }
            "tool.completed" => {
                if data["preview"].as_str().unwrap_or_default().starts_with("BLOCKED") {
                    t.step = "A command was blocked".into();
                }
            }
            "message.delta" | "message.interim" => {
                if !t.step.starts_with("Writing") {
                    t.step = "Writing the answer…".into();
                }
            }
            "subagent.start" => t.step = "Started a helper…".into(),
            _ => return,
        }
        drop(s);
        self.changed().await;
    }

    async fn finish(&self, id: &str, status: TaskStatus, result: &str, files: Vec<PathBuf>) {
        // An answer given only in words (no file written) is saved too, so
        // "Open" always has something to show and it's kept with the rest.
        let mut files = files;
        // Hermes's own "File-mutation verifier" footer (a write it refused,
        // usually because the file was already there) becomes a plain note,
        // and Open goes to that file instead of saving this answer as one
        // more.
        let (cleaned, refused) = {
            let s = self.state.lock().await;
            refused_writes(&host_words(result, &s.shared), &catalog::active().name)
        };
        let result = cleaned.as_str();
        if files.is_empty() {
            files.extend(refused.into_iter().filter(|p| p.exists()));
        }
        // A job done in a shared folder (tidying Downloads): Open goes
        // there, and its answer isn't saved as a file of its own (it
        // only cluttered ~/Agents/<Name>).
        if status == TaskStatus::Done && files.is_empty() {
            let s = self.state.lock().await;
            // Named by its path, or as people say it ("your Downloads
            // folder").
            let worked_in = s.shared.iter().find(|sh| {
                let name = Path::new(&sh.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                sh.writable
                    && (result.contains(&sh.inside)
                        || host_words(result, &s.shared).contains(&tilde(&sh.path))
                        || (!name.is_empty() && result.contains(&name)))
            });
            if let Some(sh) = worked_in {
                files.push(PathBuf::from(&sh.path));
            }
        }
        if status == TaskStatus::Done && files.is_empty() && !result.trim().is_empty() {
            let (title, text) = {
                let s = self.state.lock().await;
                let title = s.tasks.iter().find(|t| t.id == id).map(|t| t.title.clone()).unwrap_or_default();
                (title, host_words(result, &s.shared))
            };
            match save_answer(&paths::workspace(), &title, &text) {
                Ok(p) => files.push(p),
                Err(e) => tracing::warn!(error = %e, "couldn't save the answer"),
            }
        }
        let stale: Vec<Approval>;
        let (title, ok) = {
            let mut s = self.state.lock().await;
            // Its questions and sites wait on nothing now: the task is over.
            let removed: Vec<Approval> = s.approvals.iter().filter(|a| a.task == id).cloned().collect();
            s.approvals.retain(|a| a.task != id);
            stale = removed;
            let result = host_words(result, &s.shared);
            let Some(t) = s.task_mut(id) else { return };
            t.status = status.clone();
            t.finished = now();
            t.result = result.chars().take(4000).collect();
            t.files = files.iter().map(|p| p.display().to_string()).collect();
            t.step = match status {
                TaskStatus::Done => "Done".into(),
                TaskStatus::Cancelled => "Stopped".into(),
                _ => "Failed".into(),
            };
            (t.title.clone(), status == TaskStatus::Done)
        };
        for a in &stale {
            if a.kind == ApprovalKind::Network {
                let p = self.proposals.lock().await.remove(&a.id);
                if let (Some(p), Ok(gw)) = (p, self.gateway().await) {
                    let _ = self.reject(&gw, &p.chunk_id).await;
                }
            }
            self.close_note_for(&a.id).await;
        }
        self.changed().await;
        let task_json = {
            let s = self.state.lock().await;
            s.tasks.iter().find(|t| t.id == id).map(|t| serde_json::to_string(t).unwrap_or_default())
        };
        if let (Some(json), Ok(emitter)) = (task_json, zbus::object_server::SignalEmitter::new(&self.conn, PATH)) {
            let _ = crate::service::Agents::task_finished(&emitter, &json).await;
        }
        if status == TaskStatus::Cancelled {
            return;
        }
        let summary = if ok {
            format!("{n} finished “{title}”", n = catalog::active().name)
        } else {
            format!("{n} couldn't finish “{title}”", n = catalog::active().name)
        };
        if self.is_viewing(id).await {
            return;
        }
        let shown = {
            let s = self.state.lock().await;
            plain_text(&host_words(result, &s.shared))
        };
        let mut body: String = shown.chars().take(220).collect();
        if shown.chars().count() > 220 {
            body.push('…');
        }
        let target = files.first().cloned().unwrap_or_else(paths::workspace);
        let nid = notify::send(&self.conn, &summary, &body, &[("default", "Open"), ("open", "Open")], false).await;
        if nid != 0 {
            self.notes.lock().await.insert(nid, Note::Result(id.to_owned(), target));
        }
    }

    /// Takes one finished job off the list (its files stay); a running
    /// one is stopped first.
    pub async fn dismiss(self: &Arc<Self>, id: &str) -> Result<()> {
        let active = {
            let s = self.state.lock().await;
            let t = s.tasks.iter().find(|t| t.id == id).ok_or_else(|| anyhow!("That job isn't there any more."))?;
            matches!(t.status, TaskStatus::Running | TaskStatus::Waiting)
        };
        if active {
            self.stop_task(id).await?;
        }
        {
            let mut s = self.state.lock().await;
            s.tasks.retain(|t| t.id != id);
            s.approvals.retain(|a| a.task != id);
        }
        let notes: Vec<u32> = self
            .notes
            .lock()
            .await
            .iter()
            .filter(|(_, n)| matches!(n, Note::Result(t, _) if t == id))
            .map(|(nid, _)| *nid)
            .collect();
        for nid in notes {
            self.notes.lock().await.remove(&nid);
            notify::close(&self.conn, nid).await;
        }
        self.changed().await;
        Ok(())
    }

    /// Forgets finished jobs (their files stay) and turns down requests
    /// no running job is waiting on: a clean Agents menu.
    pub async fn clear_finished(self: &Arc<Self>) {
        let stray: Vec<Approval> = {
            let mut s = self.state.lock().await;
            s.tasks.retain(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Waiting));
            let running: Vec<String> = s.tasks.iter().map(|t| t.id.clone()).collect();
            s.approvals.iter().filter(|a| !running.contains(&a.task)).cloned().collect()
        };
        for a in stray {
            if a.kind == ApprovalKind::Question {
                self.settle(&a).await;
            } else if let Err(e) = self.answer(&a.id, "deny").await {
                tracing::info!(error = %e, "couldn't turn down a stray request; dropping it");
                self.settle(&a).await;
            }
        }
        let results: Vec<u32> = self
            .notes
            .lock()
            .await
            .iter()
            .filter(|(_, n)| matches!(n, Note::Result(..)))
            .map(|(id, _)| *id)
            .collect();
        for nid in results {
            self.notes.lock().await.remove(&nid);
            notify::close(&self.conn, nid).await;
        }
        self.changed().await;
    }

    /// Turns a request down at the guardrails, noting when: a later try
    /// asks again (reask.rs).
    async fn reject(&self, gw: &Gateway, chunk_id: &str) -> Result<()> {
        self.turned_down.lock().await.insert(chunk_id.to_owned(), now_ms() as i64);
        gw.clone().reject(&paths::sandbox(), chunk_id).await?;
        Ok(())
    }

    /// Rejected requests tried again since they were turned down.
    async fn tried_again(&self) -> Vec<Proposal> {
        let mut rejected = self.rejected.lock().await;
        if rejected.is_none() {
            let config = self.runtime.lock().await.as_ref().map(|r| r.config());
            if let Some(config) = config {
                match crate::reask::Rejected::connect(&config).await {
                    Ok(r) => *rejected = Some(r),
                    Err(e) => tracing::debug!(error = %format!("{e:#}"), "no connection for rejected requests yet"),
                }
            }
        }
        let Some(r) = rejected.clone() else { return Vec::new() };
        drop(rejected);
        let turned_down = self.turned_down.lock().await.clone();
        match r.tried_again(&paths::sandbox(), &turned_down).await {
            Ok(found) => found,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "couldn't list rejected requests");
                // A gateway restarted since: connect again next time.
                *self.rejected.lock().await = None;
                Vec::new()
            }
        }
    }

    /// Ask Umoja shows task `id` (empty: nothing). Its "finished"
    /// notification is closed, and none comes while it's on show.
    pub async fn set_viewing(&self, id: &str) {
        *self.viewing.lock().await = (id.to_owned(), Some(std::time::Instant::now()));
        if id.is_empty() {
            return;
        }
        let ids: Vec<u32> = self
            .notes
            .lock()
            .await
            .iter()
            .filter(|(_, n)| matches!(n, Note::Result(t, _) if t == id))
            .map(|(nid, _)| *nid)
            .collect();
        for nid in ids {
            self.notes.lock().await.remove(&nid);
            notify::close(&self.conn, nid).await;
        }
    }

    /// Task `id` (or the one it follows up) is on show in Ask Umoja. Ask
    /// Umoja says so every couple of seconds while it does, so a view that
    /// went away without saying so stops counting.
    async fn is_viewing(&self, id: &str) -> bool {
        let (viewing, at) = self.viewing.lock().await.clone();
        if viewing.is_empty() || !at.is_some_and(|t| t.elapsed() < Duration::from_secs(8)) {
            return false;
        }
        viewing == id
            || self.state.lock().await.tasks.iter().any(|t| t.id == id && t.parent == viewing)
    }

    pub async fn stop_task(&self, id: &str) -> Result<()> {
        let (run, source) = self
            .state
            .lock()
            .await
            .task_mut(id)
            .map(|t| (t.run.clone(), t.source.clone()))
            .ok_or_else(|| anyhow!("no such task"))?;
        if source == "telegram" {
            bail!("Send /stop in Telegram to stop it.");
        }
        // Nothing in the sandbox to stop (it never got a run, or the run is
        // gone): Stop still ends the job here, so it can't hang on forever.
        let stopped = match self.gateway().await {
            Ok(gw) if !run.is_empty() => bridge::stop(&gw, &run).await,
            Ok(_) => Err(anyhow!("no run")),
            Err(e) => Err(e),
        };
        if let Err(e) = stopped {
            tracing::info!(task = id, error = %format!("{e:#}"), "no run to stop; ending the job here");
            {
                let mut s = self.state.lock().await;
                if let Some(t) = s.task_mut(id) {
                    if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) {
                        t.status = TaskStatus::Cancelled;
                        t.step = "Stopped".into();
                        t.interrupted = false;
                        t.finished = now();
                    }
                }
                s.approvals.retain(|a| a.task != id);
            }
            self.changed().await;
        }
        Ok(())
    }

    /// The sandbox is being made again: runs in the old one are gone, so
    /// their jobs end now instead of "Reconnecting…" forever.
    async fn end_lost_tasks(&self) {
        let n = catalog::active().name.clone();
        let ended = {
            let mut s = self.state.lock().await;
            let mut ended = false;
            for t in s.tasks.iter_mut() {
                if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) && t.source.is_empty() {
                    t.status = TaskStatus::Failed;
                    t.step.clear();
                    t.result = format!("Interrupted when {n} restarted. Ask again to carry on.");
                    t.interrupted = false;
                    t.finished = now();
                    ended = true;
                }
            }
            ended
        };
        if ended {
            self.changed().await;
        }
    }

    pub async fn answer(self: &Arc<Self>, approval: &str, choice: &str) -> Result<()> {
        let allow = matches!(choice, "allow" | "once" | "always" | "yes");
        let a = self
            .state
            .lock()
            .await
            .approvals
            .iter()
            .find(|a| a.id == approval)
            .cloned()
            .ok_or_else(|| anyhow!("That request was already answered."))?;
        let gw = self.gateway().await?;
        match a.kind {
            ApprovalKind::Network => {
                let p = self
                    .proposals
                    .lock()
                    .await
                    .get(&a.id)
                    .cloned()
                    .ok_or_else(|| anyhow!("That request has expired."))?;
                if allow {
                    if let Err(e) = gw.clone().approve(&paths::sandbox(), &p).await {
                        // A stale review token: fetch the proposal again and
                        // answer that; gone means nothing is waiting any more.
                        tracing::info!(error = %e, "approving again with a fresh proposal");
                        let fresh = gw
                            .clone()
                            .snapshot()
                            .await?
                            .into_iter()
                            .find(|s| s.info.name == paths::sandbox())
                            .and_then(|s| s.proposals.into_iter().find(|x| x.chunk_id == p.chunk_id));
                        match fresh {
                            Some(fresh) => gw.clone().approve(&paths::sandbox(), &fresh).await?,
                            None => {
                                self.settle(&a).await;
                                bail!("That request isn't waiting any more.");
                            }
                        }
                    }
                } else if let Err(e) = self.reject(&gw, &p.chunk_id).await {
                    // Turned down either way: it's gone, or it will lapse.
                    tracing::info!(error = %e, "couldn't reject; dropping the card");
                }
            }
            ApprovalKind::Command => {
                let run = self.state.lock().await.task_mut(&a.task).map(|t| t.run.clone()).unwrap_or_default();
                bridge::approve(&gw, &run, &a.answer_key.0, &a.answer_key.1, allow).await?;
            }
            ApprovalKind::Question => {
                bail!("Answer {n}'s question with Reply.", n = catalog::active().name);
            }
        }
        self.settle(&a).await;
        Ok(())
    }

    pub async fn reply(self: &Arc<Self>, approval: &str, text: &str) -> Result<()> {
        let a = self
            .state
            .lock()
            .await
            .approvals
            .iter()
            .find(|a| a.id == approval)
            .cloned()
            .ok_or_else(|| anyhow!("That question was already answered."))?;
        let gw = self.gateway().await?;
        let run = self.state.lock().await.task_mut(&a.task).map(|t| t.run.clone()).unwrap_or_default();
        bridge::answer(&gw, &run, &a.answer_key.0, &a.answer_key.1, text).await?;
        self.settle(&a).await;
        Ok(())
    }

    async fn settle(self: &Arc<Self>, a: &Approval) {
        self.settle_chat(&a.id, "Answered on the computer.");
        {
            let mut s = self.state.lock().await;
            s.approvals.retain(|x| x.id != a.id);
            refresh_waiting(&mut s);
        }
        self.proposals.lock().await.remove(&a.id);
        self.close_note_for(&a.id).await;
        self.changed().await;
    }

    // ---- lifecycle ----

    /// Returns at once: stopping or starting the sandbox can take a minute,
    /// longer than a D-Bus call waits. The shell follows `switching`.
    /// Presses while one is under way do nothing.
    pub async fn set_paused(self: &Arc<Self>, paused: bool) -> Result<()> {
        {
            let mut s = self.state.lock().await;
            if !s.switching.is_empty() || s.paused == paused {
                return Ok(());
            }
            s.switching = if paused { "pausing" } else { "resuming" }.into();
        }
        self.changed().await;
        let this = self.clone();
        tokio::spawn(async move {
            let res = this.switch_paused(paused).await;
            let mut s = this.state.lock().await;
            s.switching.clear();
            if let Err(e) = res {
                tracing::error!(error = %format!("{e:#}"), paused, "couldn't pause or resume");
                if !paused {
                    // Back to paused, so Resume can be pressed again.
                    s.paused = true;
                    s.sandbox = "error".into();
                    s.error = for_people(&e);
                }
            }
            drop(s);
            this.changed().await;
        });
        Ok(())
    }

    async fn switch_paused(self: &Arc<Self>, paused: bool) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let gw = self.gateway().await?;
        if paused {
            let runs: Vec<String> = self
                .state
                .lock()
                .await
                .tasks
                .iter()
                .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Waiting))
                .map(|t| t.run.clone())
                .collect();
            for run in runs {
                let _ = bridge::stop(&gw, &run).await;
            }
            // Tasks end now, before the sandbox stops (that can take a
            // minute): their watchers see them finished and let go.
            {
                let mut s = self.state.lock().await;
                s.paused = true;
                for t in s.tasks.iter_mut() {
                    if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) {
                        t.status = TaskStatus::Cancelled;
                        t.step = "Stopped (agents paused)".into();
                        t.finished = now();
                    }
                }
                s.approvals.clear();
            }
            self.changed().await;
            gw.clone().stop(&paths::sandbox()).await?;
        } else {
            {
                let mut s = self.state.lock().await;
                s.paused = false;
                s.error.clear();
            }
            self.changed().await;
            gw.clone().start(&paths::sandbox()).await?;
            self.wait_running(&gw, Duration::from_secs(300)).await?;
            self.mount_folders().await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while !bridge::health(&gw).await && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        self.changed().await;
        Ok(())
    }

    pub async fn set_preset(self: &Arc<Self>, preset: Preset) -> Result<()> {
        self.state.lock().await.preset = preset;
        self.changed().await;
        let gw = self.gateway().await?;
        self.apply_policy(&gw).await?;
        self.write_config(&gw).await?;
        bridge::restart(&gw).await
    }

    pub async fn set_model(self: &Arc<Self>, model: &str) -> Result<()> {
        if model != "local" && model != "cloud" {
            bail!("model must be local or cloud");
        }
        if model == "cloud" && cloud_model().is_none() {
            bail!("Set up a cloud model in Ask Umoja's settings first.");
        }
        self.state.lock().await.model = model.into();
        self.changed().await;
        let gw = self.gateway().await?;
        self.apply_policy(&gw).await?;
        self.write_config(&gw).await?;
        bridge::restart(&gw).await
    }

    pub async fn share(self: &Arc<Self>, path: &str, writable: bool) -> Result<String> {
        let path = PathBuf::from(path);
        if !path.is_dir() {
            bail!("{} isn't a folder", path.display());
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "folder".into());
        let inside = format!("{SANDBOX_SHARED}/{}", safe_name(&name));
        {
            let mut s = self.state.lock().await;
            s.shared.retain(|x| x.path != path.display().to_string());
            s.shared.push(Shared { path: path.display().to_string(), writable, inside: inside.clone() });
        }
        if let Err(e) = self.mount_folders().await {
            // Not shared after all: don't keep it to trip the next start.
            self.state.lock().await.shared.retain(|x| x.path != path.display().to_string());
            self.changed().await;
            return Err(e);
        }
        self.changed().await;
        Ok(inside)
    }

    pub async fn unshare(self: &Arc<Self>, path: &str) -> Result<()> {
        let gone = {
            let mut s = self.state.lock().await;
            let gone = s.shared.iter().find(|x| x.path == path).cloned();
            s.shared.retain(|x| x.path != path);
            gone
        };
        // Forgotten first, so it never comes back even when the sandbox
        // can't be reached now (it is left out on the next start anyway).
        self.changed().await;
        if let Some(g) = gone {
            tokio::task::spawn_blocking(move || -> Result<()> {
                let lxd = Lxd::new(&paths::lxd_project());
                if let (Some(w), _) = lxd.sandbox(&paths::sandbox())? {
                    lxd.remove_device(&w.name, &device_for(&g.inside))?;
                }
                Ok(())
            })
            .await??;
        }
        Ok(())
    }

    pub async fn set_messaging(self: &Arc<Self>, platform: &str, token: &str, allowed: &str) -> Result<()> {
        if platform.is_empty() {
            self.state.lock().await.messaging = None;
            clear_secret("telegram");
        } else if platform == "telegram" {
            if !token.is_empty() {
                store_secret("telegram", token)?;
            } else if read_secret("telegram").is_none() {
                bail!("Paste the bot token from @BotFather.");
            }
            if allowed.trim().is_empty() {
                bail!("Add your Telegram user id, so only you can talk to {n}.", n = catalog::active().name);
            }
            self.state.lock().await.messaging = Some(Messaging { platform: platform.into(), allowed: allowed.trim().into() });
        } else {
            bail!("Only Telegram is supported for now.");
        }
        self.changed().await;
        let gw = self.gateway().await?;
        self.apply_policy(&gw).await?;
        self.write_config(&gw).await?;
        bridge::restart(&gw).await
    }

    /// Deletes the sandbox and stops OpenShell (tests, or "Remove Hermes").
    pub async fn remove(self: &Arc<Self>) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        if let Ok(gw) = self.gateway().await {
            gw.clone().delete(&paths::sandbox()).await.ok();
            for _ in 0..60 {
                if !gw.clone().list().await.map(|l| l.iter().any(|s| s.name == paths::sandbox())).unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        *self.gw.write().await = None;
        *self.runtime.lock().await = None;
        {
            let mut s = self.state.lock().await;
            s.setup = Setup::None;
            s.sandbox.clear();
            s.approvals.clear();
            s.paused = false;
            s.step.clear();
            s.error.clear();
        }
        self.changed().await;
        Ok(())
    }

    pub async fn open(&self, id: &str) -> Result<()> {
        let target = {
            let mut s = self.state.lock().await;
            let shared = s.shared.clone();
            s.task_mut(id).and_then(|t| {
                // Finished before answers were saved: save it now.
                if t.files.is_empty() && t.status == TaskStatus::Done && !t.result.trim().is_empty() {
                    let text = host_words(&t.result, &shared);
                    if let Ok(p) = save_answer(&paths::workspace(), &t.title, &text) {
                        t.files.push(p.display().to_string());
                    }
                }
                t.files.first().cloned()
            })
        }
        .map(PathBuf::from)
        .unwrap_or_else(paths::workspace);
        self.changed().await;
        tokio::process::Command::new("gio").arg("open").arg(target).spawn()?;
        Ok(())
    }

    /// Gives this account LXD access (joins the lxd group through polkit).
    pub async fn fix_lxd(self: &Arc<Self>) -> Result<()> {
        let ready = self.grant_lxd().await?;
        let mut s = self.state.lock().await;
        if ready {
            // Allowed: carry on with the setup the user started.
            s.needs.clear();
            s.error.clear();
            let preset = s.preset;
            drop(s);
            self.setup(preset).await;
            return Ok(());
        }
        s.needs = "relogin".into();
        s.error = format!("Almost there. Log out and back in, then set up {n} again.", n = catalog::active().name);
        drop(s);
        self.changed().await;
        Ok(())
    }
}

impl Agent {
    /// Runs lxd-setup through polkit (org.umoja.agents.lxd-setup): installs
    /// and initialises LXD only where needed, adds the account to the lxd
    /// group and lets it reach the socket until its next login. Whether this
    /// session can use LXD now.
    async fn grant_lxd(&self) -> Result<bool> {
        let status = tokio::process::Command::new("pkexec")
            .arg(paths::libexec("lxd-setup"))
            .arg(paths::user_name())
            .status()
            .await?;
        if !status.success() {
            bail!("That didn't work ({status}).");
        }
        let project = paths::lxd_project();
        let access = tokio::task::spawn_blocking(move || Lxd::new(&project).access()).await?;
        Ok(matches!(access, crate::lxd::Access::Ok))
    }
}

/// Markdown as a notification shows it: no heading marks, emphasis, code
/// ticks or table rules; list items as bullets, table rows as "a · b".
pub fn plain_text(md: &str) -> String {
    let mut lines = Vec::new();
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("```") {
            continue;
        }
        // A table's |---|---| rule.
        if t.starts_with('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            continue;
        }
        let t = t.trim_start_matches('#').trim_start();
        let t = if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            format!("• {rest}")
        } else if t.starts_with('|') {
            t.trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>().join(" · ")
        } else {
            t.to_owned()
        };
        let t = t.replace("**", "").replace("__", "").replace('`', "");
        lines.push(t);
    }
    lines.join("\n")
}

/// Paths inside the sandbox as the user knows them: "/sandbox/workspace/
/// plan.md" is "~/Agents/Hermes/plan.md", a shared folder its own place.
/// "/home/ken/Downloads" as "~/Downloads".
/// Splits Hermes's "File-mutation verifier" footer off an answer: the
/// answer with a plain note in its place, and the files it named (host
/// paths). The footer lists each failed edit as "• `path` — [tool] why".
fn refused_writes(text: &str, name: &str) -> (String, Vec<PathBuf>) {
    let Some(at) = text.find("File-mutation verifier") else {
        return (text.to_owned(), Vec::new());
    };
    // From the start of its line (it opens with a warning sign).
    let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let footer = &text[start..];
    let mut files: Vec<PathBuf> = Vec::new();
    for part in footer.split('`').skip(1).step_by(2) {
        let part = part.trim();
        let path = if let Some(rest) = part.strip_prefix("~/") {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest))
        } else if part.starts_with('/') {
            Some(PathBuf::from(part))
        } else {
            None
        };
        if let Some(p) = path {
            if !files.contains(&p) {
                files.push(p);
            }
        }
    }
    let mut out = text[..start].trim_end().to_owned();
    let names: Vec<String> = files
        .iter()
        .map(|p| format!("“{}”", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
        .collect();
    let note = if names.is_empty() {
        format!("{name} couldn't save a file it meant to change.")
    } else {
        format!(
            "{name} couldn't save {}: a file with that name is already there, and {name} doesn't write over files.",
            names.join(", ")
        )
    };
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!("*{note}*"));
    (out, files)
}

fn tilde(p: impl AsRef<Path>) -> String {
    let p = p.as_ref();
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if p.starts_with(&home) => format!("~/{}", p.strip_prefix(&home).unwrap_or(p).display()),
        _ => p.display().to_string(),
    }
}

pub fn host_words(text: &str, shared: &[crate::state::Shared]) -> String {
    let mut out = text.replace(paths::SANDBOX_WORKSPACE, &tilde(&paths::workspace()));
    // Longest first, so /sandbox/shared/Downloads2 isn't cut at Downloads.
    let mut shared: Vec<_> = shared.iter().collect();
    shared.sort_by_key(|s| std::cmp::Reverse(s.inside.len()));
    for s in shared {
        out = out.replace(&s.inside, &tilde(Path::new(&s.path)));
    }
    out
}

/// An error as the user sees it: what went wrong and what to do, never a
/// registry URL or an RPC status (the whole chain goes to the journal).
pub fn for_people(e: &anyhow::Error) -> String {
    let n = &catalog::active().name;
    let chain = format!("{e:#}").to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| chain.contains(w));
    if has(&["manifest unknown", "name unknown", "repository not found", "unauthorized", "denied: "]) {
        return format!("{n} isn't available to download right now. Try again later.");
    }
    if has(&["dns", "lookup", "connection refused", "network is unreachable", "timed out", "tls", "certificate"])
        && has(&["registry", "pull", "download", "ghcr", "image"])
    {
        return format!("Couldn't download {n}. Check that you're online, then try again.");
    }
    if has(&["no space left", "disk quota"]) {
        return format!("There isn't enough free disk space for {n}. Free some up, then try again.");
    }
    // Our own messages are the outermost one (a bail or a context); a
    // library's raw error is replaced by a plain one.
    let top = e.to_string();
    let technical = top.contains("://")
        || ["error", "status", "rpc", "errno", "os error", "{"].iter().any(|w| top.to_lowercase().contains(w));
    if technical {
        format!("{n} couldn't start. Try again in a moment.")
    } else if top.ends_with('.') {
        top
    } else {
        format!("{top}.")
    }
}

/// Tasks waiting on nothing any more go back to running.
fn refresh_waiting(s: &mut State) {
    let waiting: Vec<String> = s.approvals.iter().map(|a| a.task.clone()).collect();
    for t in s.tasks.iter_mut() {
        if t.status == TaskStatus::Waiting && !waiting.contains(&t.id) {
            t.status = TaskStatus::Running;
        }
    }
}

/// The active agent's name, folder and the agents on offer, for the shell.
fn fill_agent_info(s: &mut State) {
    let kind = catalog::active();
    s.agent_name = kind.name.clone();
    s.workspace = paths::workspace().display().to_string();
    s.agents = catalog::all()
        .into_values()
        .map(|k| crate::state::AgentChoice { id: k.id, name: k.name, description: k.description })
        .collect();
}

/// A task interrupted by sleep is carried on at most this often.
const MAX_CARRY_ON: u32 = 2;

/// The program a network rule applies to: any in the sandbox. OpenShell
/// matches a rule only for the programs it lists, so a rule with none
/// allows nothing (only a proposal the user allows names one program).
const ANY_PROGRAM: &str = "/**";
/// Running fine this long after waking: it no longer counts as interrupted.
const CARRY_ON_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
struct Activity {
    last: tokio::time::Instant,
    in_tool: bool,
}

/// What Hermes is told when a task is carried on after the computer slept.
fn carry_on_text(prompt: &str, log: &[String], same_session: bool) -> String {
    let mut text = String::from(
        "The computer went to sleep while you were working on this task, and your last step may \
not have finished. Don't start over: check what is already done (the files in /sandbox/workspace",
    );
    text.push_str(if same_session { " and this conversation" } else { "" });
    text.push_str("), redo only the step that was cut off, and carry on.\n\nThe task: ");
    text.push_str(prompt);
    if !log.is_empty() {
        text.push_str("\n\nYour steps so far:\n");
        for step in log {
            text.push_str("- ");
            text.push_str(step);
            text.push('\n');
        }
    }
    text
}

fn step_text(tool: &str, preview: &str) -> String {
    let short: String = preview.chars().take(60).collect();
    let short = short.replace('\n', " ");
    match tool {
        "terminal" | "shell" | "bash" | "execute_command" => format!("Running {}", first_word(&short)),
        t if t.contains("write") || t.contains("file") => "Writing a file".into(),
        t if t.contains("search") => "Searching the web".into(),
        t if t.contains("fetch") || t.contains("web") || t.contains("browser") => "Reading a page".into(),
        t if t.contains("memory") => "Remembering something".into(),
        t if t.contains("skill") => "Using a skill".into(),
        other => format!("Using {other}"),
    }
}

fn first_word(s: &str) -> String {
    let s = s.trim_start_matches(|c: char| c == '{' || c == '"' || c.is_whitespace());
    let s = s.strip_prefix("command\":").unwrap_or(s).trim_start_matches(|c: char| c == '"' || c.is_whitespace());
    s.split_whitespace().next().unwrap_or("a command").trim_matches('"').to_owned()
}

/// host:port for every http(s) URL in a tool preview.
fn hosts_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for scheme in ["https://", "http://"] {
        for part in text.split(scheme).skip(1) {
            let host: String = part
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == ':')
                .collect();
            if host.is_empty() {
                continue;
            }
            let with_port = if host.contains(':') {
                host
            } else {
                format!("{host}:{}", if scheme == "https://" { 443 } else { 80 })
            };
            if !out.contains(&with_port) {
                out.push(with_port);
            }
        }
    }
    out
}

fn snapshot(dir: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if out.len() < 20000 && !e.file_name().to_string_lossy().starts_with('.') {
                    stack.push(p);
                }
            } else if let Ok(m) = meta.modified() {
                out.insert(p, m);
            }
        }
    }
    out
}

fn changed_files(before: &Snapshot, after: &Snapshot) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = after
        .iter()
        .filter(|(p, m)| before.get(*p) != Some(m))
        .map(|(p, m)| (*m, p.clone()))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().map(|(_, p)| p).take(20).collect()
}

/// Opens a task's answer in Ask Umoja, where it can be read whole and
/// replied to. False when Ask Umoja isn't there.
async fn show_in_ask(conn: &zbus::Connection, task: &str) -> bool {
    conn.call_method(Some("org.umoja.Overlay"), "/org/umoja/Overlay", Some("org.umoja.Overlay"), "ShowAgentTask", &(task,))
        .await
        .is_ok()
}

/// Writes an answer as "<title>.md" in `dir` (" 2", " 3"… if taken).
fn save_answer(dir: &Path, title: &str, text: &str) -> std::io::Result<PathBuf> {
    let mut stem: String = title
        .trim_end_matches('…')
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '-' } else { c })
        .take(60)
        .collect::<String>()
        .trim()
        .trim_end_matches(['.', ',', ' '])
        .to_owned();
    if stem.is_empty() {
        stem = "Answer".into();
    }
    std::fs::create_dir_all(dir)?;
    let mut path = dir.join(format!("{stem}.md"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} {n}.md"));
        n += 1;
    }
    std::fs::write(&path, format!("# {stem}\n\n{}\n", text.trim()))?;
    Ok(path)
}

fn safe_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    if out.is_empty() { "folder".into() } else { out }
}

fn device_for(inside: &str) -> String {
    let mut name = String::from("umoja-share");
    for c in inside.trim_start_matches(SANDBOX_SHARED).chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    name.trim_end_matches('-').to_owned()
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

fn random_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

/// The agent's home inside the sandbox, as an LXD disk device.
const HOME_DEVICE: &str = "umoja-agent-home";

/// Whether two image references name the same image ("name:tag", ignoring
/// how the registry is spelled).
fn same_image(a: &str, b: &str) -> bool {
    let last = |s: &str| s.rsplit('/').next().unwrap_or(s).to_owned();
    last(a) == last(b)
}

/// Telegram's own addresses: allowed by the messaging rule (the name),
/// or asked for only while that rule wasn't in place yet (Hermes's
/// fallback addresses). Never a question for the user.
fn telegram_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("api.telegram.org") || host.starts_with("149.154.") || host.starts_with("91.108.")
}

/// A short key for approval `id`, for Telegram's 64-byte button data.
fn chat_key(id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    now_ms().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The text of the last `role` message in the agent's session `session`.
async fn last_message(gw: &Gateway, session: &str, role: &str) -> String {
    let path = format!("/api/sessions/{session}/messages");
    let Ok(v) = bridge::call(gw, &["call", "GET", &path], None).await else { return String::new() };
    v["body"]["data"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == role).map(|m| message_text(&m["content"])))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_default()
}

/// A message's text: a plain string, or the text parts of a list.
fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_owned(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str().or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn gsetting(key: &str) -> String {
    std::process::Command::new("gsettings")
        .args(["get", "org.umoja.shell", key])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_owned())
        .unwrap_or_default()
}

/// The context window to load the local model with for the agent: its
/// catalog floor (Hermes refuses less than 64K), at least 32K.
fn context_for_agent() -> u32 {
    catalog::active().min_context.max(32768)
}

/// The Lemonade model the assistant uses, picked the same way.

fn local_model() -> String {
    if let Ok(m) = std::env::var("UMOJA_AGENTS_MODEL") {
        if !m.is_empty() {
            return m;
        }
    }
    let set = gsetting("lemonade-model");
    if !set.is_empty() {
        return set;
    }
    if paths::total_ram_mb() >= 12 * 1024 {
        "Qwen3-4B-Instruct-2507-GGUF".into()
    } else {
        "Qwen3.5-2B-GGUF".into()
    }
}

/// Cloud offload (opt-in, `org.umoja.shell cloud-offload`): the Lemonade
/// routing model umoja-lemonade-routing registered for the agent. It keeps
/// requests on the local model and sends long agent tasks and complex
/// requests to the user's cloud model. None while offload is off.
fn routed_model() -> Option<String> {
    if std::env::var_os("UMOJA_AGENTS_MODEL").is_some() {
        return None;
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let raw = std::fs::read(std::path::Path::new(&dir).join("umoja-routing.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    if v["ok"] != true {
        return None;
    }
    v["agent"].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Asks Umoja's Lemonade to have the local model loaded with Hermes's
/// context window (the assistant's default is smaller). Best effort: if
/// it fails, Hermes reports the context problem itself.
async fn preload_local_model() {
    if std::env::var_os("UMOJA_AGENTS_MODEL_PORT").is_some() {
        return;
    }
    let body = serde_json::json!({ "model_name": local_model(), "ctx_size": context_for_agent() }).to_string();
    let port = paths::lemonade_port();
    let result = tokio::time::timeout(Duration::from_secs(180), async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let req = format!(
            "POST /api/v1/load HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await?;
        let mut answer = Vec::new();
        sock.read_to_end(&mut answer).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&answer).lines().next().unwrap_or("").to_owned())
    })
    .await;
    match result {
        Ok(Ok(status)) => tracing::info!(%status, "local model preload"),
        Ok(Err(e)) => tracing::warn!(error = %e, "local model preload"),
        Err(_) => tracing::warn!("local model preload timed out"),
    }
}

/// (base URL, model, key) of the assistant's cloud model, if set up.
fn cloud_model() -> Option<(String, String, String)> {
    let url = gsetting("cloud-base-url");
    let model = gsetting("cloud-model");
    if url.is_empty() || model.is_empty() {
        return None;
    }
    let key = std::process::Command::new("secret-tool")
        .args(["lookup", "umoja", "assistant-cloud", "kind", "api-key"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())?;
    Some((url.trim_end_matches('/').to_owned(), model, key))
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let hostport = rest.split('/').next()?;
    let default = if scheme == "https" { 443 } else { 80 };
    match hostport.rsplit_once(':') {
        Some((h, p)) => Some((h.to_owned(), p.parse().ok()?)),
        None => Some((hostport.to_owned(), default)),
    }
}

const SECRET_ATTRS: [&str; 2] = ["umoja", "agents"];

fn read_secret(kind: &str) -> Option<String> {
    let out = std::process::Command::new("secret-tool")
        .args(["lookup", SECRET_ATTRS[0], SECRET_ATTRS[1], "kind", kind])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !t.is_empty()).then_some(t)
}

fn store_secret(kind: &str, value: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("secret-tool")
        .args(["store", "--label", "Umoja agents: Hermes messaging", SECRET_ATTRS[0], SECRET_ATTRS[1], "kind", kind])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("secret-tool")?;
    child.stdin.take().unwrap().write_all(value.trim().as_bytes())?;
    if !child.wait()?.success() {
        bail!("the keyring refused the token");
    }
    Ok(())
}

fn clear_secret(kind: &str) {
    let _ = std::process::Command::new("secret-tool")
        .args(["clear", SECRET_ATTRS[0], SECRET_ATTRS[1], "kind", kind])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_footer_becomes_a_note() {
        let (text, files) = refused_writes(
            "Here's your plan.\n\n⚠️ File-mutation verifier: 1 file edit(s) FAILED this turn.\n• `/tmp/x/Lisbon weekend.md` — [write_file] Refusing to overwrite",
            "Hermes",
        );
        assert!(text.starts_with("Here's your plan."));
        assert!(!text.contains("verifier"));
        assert!(text.contains("“Lisbon weekend.md”"));
        assert_eq!(files, vec![PathBuf::from("/tmp/x/Lisbon weekend.md")]);
        let (same, none) = refused_writes("All done.", "Hermes");
        assert_eq!(same, "All done.");
        assert!(none.is_empty());
    }

    #[test]
    fn answers_saved_as_files() {
        let dir = std::env::temp_dir().join(format!("umoja-answer-{}", std::process::id()));
        let a = save_answer(&dir, "Plan a week long trip to Marrakech, airfare…", "Day 1").unwrap();
        assert_eq!(a.file_name().unwrap(), "Plan a week long trip to Marrakech, airfare.md");
        let b = save_answer(&dir, "Plan a week long trip to Marrakech, airfare…", "Day 1").unwrap();
        assert_eq!(b.file_name().unwrap(), "Plan a week long trip to Marrakech, airfare 2.md");
        assert!(std::fs::read_to_string(&a).unwrap().contains("Day 1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sandbox_paths_in_host_words() {
        let shared = vec![crate::state::Shared {
            path: "/elsewhere/Finances".into(),
            writable: false,
            inside: "/sandbox/shared/Finances".into(),
        }];
        let text = host_words("Read /sandbox/shared/Finances/bank.csv", &shared);
        assert_eq!(text, "Read /elsewhere/Finances/bank.csv");
        assert!(!host_words("saved /sandbox/workspace/x.md", &shared).contains("/sandbox"));
        assert_eq!(
            plain_text("# Budget\n\n- **Money in:** €2,850\n| a | b |\n|---|---|\n| `x` | 1 |"),
            "Budget\n• Money in: €2,850\na · b\nx · 1"
        );
    }

    #[test]
    fn errors_for_people() {
        let raw = anyhow!("registry error: url https://ghcr.io/v2/kenvandine/umoja-hermes/manifests/latest: manifest unknown")
            .context("Couldn't create Hermes's sandbox");
        let text = for_people(&raw);
        assert!(text.contains("isn't available to download"), "{text}");
        assert!(!text.contains("ghcr"));
        let ours = anyhow!("Hermes needs your permission to set up its sandbox.");
        assert_eq!(for_people(&ours), "Hermes needs your permission to set up its sandbox.");
        let lib = anyhow!("status: Unavailable, message: \"tcp connect error\"");
        assert!(for_people(&lib).ends_with("couldn't start. Try again in a moment."));
        let ctx = anyhow!("os error 2").context("Couldn't create Hermes's sandbox");
        assert_eq!(for_people(&ctx), "Couldn't create Hermes's sandbox.");
    }

    #[test]
    fn carry_on_text_keeps_the_task_and_steps() {
        let t = carry_on_text("Plan a trip", &["Running curl".into(), "Writing plan.md".into()], true);
        assert!(t.contains("The task: Plan a trip"));
        assert!(t.contains("- Writing plan.md"));
        assert!(t.contains("this conversation"));
        assert!(!carry_on_text("x", &[], false).contains("this conversation"));
    }

    #[test]
    fn hosts_from_previews() {
        assert_eq!(
            hosts_in("{\"command\": \"curl -sL https://example.org/x && wget http://a.b:8080/\"}"),
            vec!["example.org:443".to_owned(), "a.b:8080".to_owned()]
        );
    }

    #[test]
    fn steps() {
        assert_eq!(step_text("terminal", "{\"command\": \"curl -s https://x\"}"), "Running curl");
        assert_eq!(step_text("write_file", "x"), "Writing a file");
        assert_eq!(step_text("web_search", ""), "Searching the web");
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("Taxes 2026"), "Taxes-2026");
        assert_eq!(device_for("/sandbox/shared/Taxes-2026"), "umoja-share-taxes-2026");
        assert_eq!(host_port("https://openrouter.ai/api/v1"), Some(("openrouter.ai".into(), 443)));
        assert_eq!(host_port("http://10.0.0.2:8000/v1"), Some(("10.0.0.2".into(), 8000)));
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn changed() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(5);
        let before = Snapshot::from([("/w/a".into(), t0), ("/w/b".into(), t0)]);
        let after = Snapshot::from([("/w/a".into(), t0), ("/w/b".into(), t1), ("/w/c".into(), t1)]);
        let mut got = changed_files(&before, &after);
        got.sort();
        assert_eq!(got, vec![PathBuf::from("/w/b"), PathBuf::from("/w/c")]);
    }
}

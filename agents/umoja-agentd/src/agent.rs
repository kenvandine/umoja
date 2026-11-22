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

use crate::hermes::{self, PendingKind};
use crate::lxd::{Access, Lxd};
use crate::paths::{self, SANDBOX, SANDBOX_SHARED, SANDBOX_WORKSPACE};
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
    /// Tasks with a watcher running.
    watching: Mutex<std::collections::HashSet<String>>,
    /// Serialises setup / pause / remove.
    lifecycle: Mutex<()>,
    /// Bumped on every wake from sleep: watchers reconnect.
    wake: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone)]
enum Note {
    Approval(String),
    Open(PathBuf),
}

type Snapshot = BTreeMap<PathBuf, SystemTime>;

impl Agent {
    pub fn new(conn: zbus::Connection) -> Arc<Self> {
        let mut state = State::load();
        state.workspace = paths::workspace().display().to_string();
        Arc::new(Self {
            state: Mutex::new(state),
            conn,
            runtime: Mutex::new(None),
            gw: RwLock::new(None),
            relay_allowed: Default::default(),
            relay_running: Mutex::new(false),
            notes: Mutex::new(HashMap::new()),
            proposals: Mutex::new(HashMap::new()),
            watching: Mutex::new(Default::default()),
            lifecycle: Mutex::new(()),
            wake: Default::default(),
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
            .ok_or_else(|| anyhow!("Hermes isn't running. Set it up in Ask Umoja's settings first."))
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
                tracing::error!(error = %e, "couldn't start Hermes");
                let mut s = this.state.lock().await;
                s.sandbox = "error".into();
                s.error = format!("{e:#}");
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
            let result = this.bring_up(true).await;
            let mut s = this.state.lock().await;
            match result {
                Ok(()) => {
                    s.setup = Setup::Ready;
                    s.step.clear();
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "setup failed");
                    s.setup = Setup::Error;
                    s.error = format!("{e:#}");
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
                bail!("Hermes needs one more part installed before it can run safely.");
            }
            Access::Denied => {
                self.state.lock().await.needs = "lxd-group".into();
                bail!("Hermes needs your permission to set up its sandbox. Allow it, then log out and back in.");
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
            let rt = Runtime::start(bridge.0).await?;
            *self.runtime.lock().await = Some(rt);
        }
        let gw = {
            let rt = self.runtime.lock().await;
            rt.as_ref().unwrap().connect(Duration::from_secs(60)).await?
        };
        *self.gw.write().await = Some(gw.clone());
        let _ = gw.clone().ensure_profiles().await;

        self.ensure_relay(bridge.0).await;
        self.spawn_poller();

        let paused = self.state.lock().await.paused;
        let exists = gw.clone().list().await?.into_iter().find(|s| s.name == SANDBOX);
        match exists {
            None => {
                self.set_step("Downloading Hermes (first time only)").await;
                let mem = paths::memory_cap_mb();
                gw.clone()
                    .create(
                        SANDBOX,
                        &CreateOptions {
                            image: paths::image(),
                            memory_mb: mem,
                            command: vec!["/usr/local/bin/umoja-hermes-start".into()],
                            labels: vec![("umoja.agent".into(), "hermes".into())],
                            ..Default::default()
                        },
                    )
                    .await
                    .context("Couldn't create Hermes's sandbox")?;
                self.state.lock().await.memory_mb = mem;
            }
            Some(info) if info.state == Phase::Stopped && !paused => {
                self.set_step("Starting Hermes's sandbox").await;
                gw.clone().start(SANDBOX).await?;
            }
            Some(_) => {}
        }
        if paused {
            return Ok(());
        }
        self.wait_running(&gw, Duration::from_secs(20 * 60)).await?;

        if configure {
            self.set_step("Applying guardrails").await;
            self.apply_policy(&gw).await?;
        }
        self.set_step("Sharing ~/Agents/Hermes").await;
        self.mount_folders().await?;

        self.set_step("Connecting Hermes to the model").await;
        if configure || !hermes::health(&gw).await {
            self.write_config(&gw).await?;
            hermes::restart(&gw).await.ok();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while !hermes::health(&gw).await {
            if tokio::time::Instant::now() > deadline {
                bail!("Hermes didn't start. Try again in a moment.");
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
        self.state.lock().await.error.clear();
        self.changed().await;
        Ok(())
    }

    async fn wait_running(&self, gw: &Gateway, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let info = gw.clone().list().await?.into_iter().find(|s| s.name == SANDBOX);
            match info {
                Some(i) if i.state == Phase::Running => return Ok(()),
                Some(i) if i.state == Phase::Failed => {
                    bail!("Hermes's sandbox failed: {}", if i.detail.is_empty() { i.phase } else { i.detail })
                }
                Some(i) => {
                    let mut s = self.state.lock().await;
                    s.sandbox = i.phase.to_lowercase();
                }
                None => bail!("Hermes's sandbox disappeared"),
            }
            if tokio::time::Instant::now() > deadline {
                bail!("Hermes's sandbox took too long to start");
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
            gw.add_rule(SANDBOX, &host, port, None).await.context("model rule")?;
        }
        for (host, port) in preset.open_sites() {
            gw.add_rule(SANDBOX, host, *port, None).await?;
        }
        if messaging.as_ref().is_some_and(|m| m.platform == "telegram") {
            gw.add_rule(SANDBOX, "api.telegram.org", 443, None).await?;
        }
        if let Err(e) = gw.set_auto_approve(SANDBOX, preset.auto_approve()).await {
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
                local_model(),
                "lemonade".to_owned(),
            )
        };
        let local = s.model != "cloud";
        let mut env = vec![
            ("UMOJA_MODEL_URL", url),
            ("UMOJA_MODEL", model),
            ("UMOJA_MODEL_KEY", key),
            ("UMOJA_APPROVALS", s.preset.hermes_mode().to_owned()),
            ("API_SERVER_KEY", s.private.api_key.clone()),
        ];
        if local && std::env::var_os("UMOJA_AGENTS_MODEL_PORT").is_none() {
            env.push(("UMOJA_MODEL_CONTEXT", HERMES_CONTEXT.to_string()));
        }
        let token;
        if let Some(m) = &s.messaging {
            if m.platform == "telegram" {
                token = read_secret("telegram").unwrap_or_default();
                env.push(("TELEGRAM_BOT_TOKEN", token));
                env.push(("TELEGRAM_ALLOWED_USERS", m.allowed.clone()));
            }
        }
        let text: String = env
            .iter()
            .map(|(k, v)| format!("{k}={}\n", shell_quote(v)))
            .collect();
        hermes::write_file(gw, "/sandbox/.config/umoja/hermes.env", &text, 0o600).await
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

    async fn mount_folders(&self) -> Result<()> {
        let workspace = paths::workspace();
        std::fs::create_dir_all(&workspace)?;
        let shared = self.state.lock().await.shared.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let lxd = Lxd::new(&paths::lxd_project());
            let (workload, _) = lxd.sandbox(SANDBOX)?;
            let workload = workload.ok_or_else(|| anyhow!("can't find Hermes's container in LXD"))?;
            let mut wanted = vec![(
                "umoja-workspace".to_owned(),
                workspace.display().to_string(),
                SANDBOX_WORKSPACE.to_owned(),
                false,
            )];
            for s in &shared {
                wanted.push((device_for(&s.inside), s.path.clone(), s.inside.clone(), !s.writable));
            }
            for (device, source, path, readonly) in wanted {
                let have = workload.devices.get(&device);
                if have.is_some_and(|d| d.get("source") == Some(&source)) {
                    continue;
                }
                lxd.add_disk(&workload.name, &device, &source, &path, readonly)
                    .with_context(|| format!("couldn't share {source} with Hermes"))?;
            }
            Ok(())
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
    }

    async fn poll(self: &Arc<Self>, tick: u64) -> Result<()> {
        let Ok(gw) = self.gateway().await else { return Ok(()) };
        let snapshot = gw.clone().snapshot().await?;
        let mine = snapshot.into_iter().find(|s| s.info.name == SANDBOX);
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
        let proposals = mine.map(|s| s.proposals).unwrap_or_default();

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
                let (workload, _) = lxd.sandbox(SANDBOX)?;
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
                    continue;
                }
                known.insert(id.clone(), p.clone());
                let what = task.as_ref().map(|(_, t)| format!(" for “{t}”")).unwrap_or_default();
                let approval = Approval {
                    id: id.clone(),
                    kind: ApprovalKind::Network,
                    task: task.as_ref().map(|(i, _)| i.clone()).unwrap_or_default(),
                    title: format!("Hermes wants to reach {}", p.host),
                    detail: format!(
                        "Hermes tried to reach {}{what}. Umoja held it back because it isn't one of the sites Hermes may use.",
                        p.host,
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
        s.approvals
            .retain(|a| a.kind != ApprovalKind::Network || ids.contains(&a.id));
        if s.approvals.len() != before {
            changed = true;
            refresh_waiting(&mut s);
        }
        drop(s);
        for a in fresh {
            self.announce(&a).await;
        }
        if changed {
            self.changed().await;
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
            Some(Note::Open(path)) => {
                let _ = tokio::process::Command::new("gio").arg("open").arg(&path).spawn();
                self.notes.lock().await.remove(&id);
            }
            None => {}
        }
    }

    // ---- tasks ----

    pub async fn submit(self: &Arc<Self>, prompt: &str) -> Result<String> {
        let prompt = prompt.trim().trim_start_matches("@hermes").trim().to_owned();
        if prompt.is_empty() {
            bail!("Tell Hermes what to do.");
        }
        let (paused, setup) = {
            let s = self.state.lock().await;
            (s.paused, s.setup.clone())
        };
        if setup != Setup::Ready {
            bail!("Hermes isn't set up yet. Set it up in Ask Umoja's settings.");
        }
        if paused {
            bail!("Agents are paused. Resume them from the panel first.");
        }
        let gw = self.gateway().await?;
        let before = snapshot(&paths::workspace());
        let id = format!("t{}", now_ms());
        let task = Task {
            id: id.clone(),
            title: title_for(&prompt),
            prompt: prompt.clone(),
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
        match hermes::submit(&gw, &prompt).await {
            Ok(run) => {
                if let Some(t) = self.state.lock().await.task_mut(&id) {
                    t.run = run;
                    t.step = "Thinking…".into();
                }
                self.changed().await;
                self.watch_task(id.clone(), before);
                Ok(id)
            }
            Err(e) => {
                self.finish(&id, TaskStatus::Failed, &format!("Hermes couldn't start: {e:#}"), Vec::new())
                    .await;
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
                let _ = hermes::bridge(&gw, &["events", &run], None, |v| {
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
            // Running fine a while after waking up: no longer interrupted.
            if woke.is_some_and(|w| w.elapsed() > CARRY_ON_WINDOW) {
                woke = None;
                if let Some(t) = self.state.lock().await.task_mut(id) {
                    t.interrupted = false;
                }
            }
            let r = match self.gateway().await {
                Ok(gw) => hermes::run(&gw, &run).await,
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
                        self.finish(id, TaskStatus::Failed, &format!("Lost track of Hermes: {e:#}"), Vec::new())
                            .await;
                        break;
                    }
                    continue;
                }
            };
            let status = r["status"].as_str().unwrap_or("").to_owned();
            if let Some(session) = r["session_id"].as_str() {
                let mut s = self.state.lock().await;
                if let Some(t) = s.task_mut(id) {
                    if t.session != session {
                        t.session = session.to_owned();
                    }
                }
            }
            for p in hermes::pending_controls(&r) {
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
                        title: "Hermes wants to run a command".into(),
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
                        title: format!("Hermes asks: {}", p.text),
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
                let mut text = hermes::output_text(&r);
                if text.is_empty() && end == TaskStatus::Failed {
                    text = if status == "gone" {
                        "Hermes restarted and lost this task. Ask again to start it over.".to_owned()
                    } else {
                        r["error"].as_str().unwrap_or("Hermes stopped with an error.").to_owned()
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
                        restarting = hermes::stop(&gw, &run).await.is_ok();
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
        while !hermes::health(&gw).await {
            if tokio::time::Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if self.state.lock().await.model != "cloud" {
            preload_local_model().await;
        }
        let text = carry_on_text(&prompt, &log, !session.is_empty());
        match hermes::submit_in(&gw, &text, Some(&session)).await {
            Ok(run) => {
                tracing::info!(task = id, run = %run, "carrying on after an interruption");
                if let Some(t) = self.state.lock().await.task_mut(id) {
                    t.run = run.clone();
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
            if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) {
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
                        if hermes::health(&gw).await {
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
        let (title, ok) = {
            let mut s = self.state.lock().await;
            let removed: Vec<String> = s.approvals.iter().filter(|a| a.task == id && a.kind != ApprovalKind::Network).map(|a| a.id.clone()).collect();
            s.approvals.retain(|a| !removed.contains(&a.id));
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
            format!("Hermes finished “{title}”")
        } else {
            format!("Hermes couldn't finish “{title}”")
        };
        let mut body: String = result.chars().take(220).collect();
        if result.chars().count() > 220 {
            body.push('…');
        }
        let target = files.first().cloned().unwrap_or_else(paths::workspace);
        let label = if files.is_empty() { "Show Folder" } else { "Open" };
        let nid = notify::send(&self.conn, &summary, &body, &[("default", label), ("open", label)], false).await;
        if nid != 0 {
            self.notes.lock().await.insert(nid, Note::Open(target));
        }
    }

    pub async fn stop_task(&self, id: &str) -> Result<()> {
        let run = self
            .state
            .lock()
            .await
            .task_mut(id)
            .map(|t| t.run.clone())
            .ok_or_else(|| anyhow!("no such task"))?;
        let gw = self.gateway().await?;
        hermes::stop(&gw, &run).await
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
                let mut gw = gw.clone();
                if allow {
                    gw.approve(SANDBOX, &p).await?;
                } else {
                    gw.reject(SANDBOX, &p.chunk_id).await?;
                }
            }
            ApprovalKind::Command => {
                let run = self.state.lock().await.task_mut(&a.task).map(|t| t.run.clone()).unwrap_or_default();
                hermes::approve(&gw, &run, &a.answer_key.0, &a.answer_key.1, allow).await?;
            }
            ApprovalKind::Question => {
                bail!("Answer Hermes's question with Reply.");
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
        hermes::clarify(&gw, &run, &a.answer_key.0, &a.answer_key.1, text).await?;
        self.settle(&a).await;
        Ok(())
    }

    async fn settle(&self, a: &Approval) {
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

    pub async fn set_paused(self: &Arc<Self>, paused: bool) -> Result<()> {
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
                let _ = hermes::stop(&gw, &run).await;
            }
            self.state.lock().await.paused = true;
            self.changed().await;
            gw.clone().stop(SANDBOX).await?;
            for t in self.state.lock().await.tasks.iter_mut() {
                if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) {
                    t.status = TaskStatus::Cancelled;
                    t.step = "Stopped (agents paused)".into();
                    t.finished = now();
                }
            }
            self.state.lock().await.approvals.clear();
        } else {
            self.state.lock().await.paused = false;
            self.changed().await;
            gw.clone().start(SANDBOX).await?;
            self.wait_running(&gw, Duration::from_secs(300)).await?;
            self.mount_folders().await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while !hermes::health(&gw).await && tokio::time::Instant::now() < deadline {
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
        hermes::restart(&gw).await
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
        hermes::restart(&gw).await
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
        self.mount_folders().await?;
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
        if let Some(g) = gone {
            tokio::task::spawn_blocking(move || -> Result<()> {
                let lxd = Lxd::new(&paths::lxd_project());
                if let (Some(w), _) = lxd.sandbox(SANDBOX)? {
                    lxd.remove_device(&w.name, &device_for(&g.inside))?;
                }
                Ok(())
            })
            .await??;
        }
        self.changed().await;
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
                bail!("Add your Telegram user id, so only you can talk to Hermes.");
            }
            self.state.lock().await.messaging = Some(Messaging { platform: platform.into(), allowed: allowed.trim().into() });
        } else {
            bail!("Only Telegram is supported for now.");
        }
        self.changed().await;
        let gw = self.gateway().await?;
        self.apply_policy(&gw).await?;
        self.write_config(&gw).await?;
        hermes::restart(&gw).await
    }

    /// Deletes the sandbox and stops OpenShell (tests, or "Remove Hermes").
    pub async fn remove(self: &Arc<Self>) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        if let Ok(gw) = self.gateway().await {
            gw.clone().delete(SANDBOX).await.ok();
            for _ in 0..60 {
                if !gw.clone().list().await.map(|l| l.iter().any(|s| s.name == SANDBOX)).unwrap_or(false) {
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
            s.task_mut(id).and_then(|t| t.files.first().cloned())
        }
        .map(PathBuf::from)
        .unwrap_or_else(paths::workspace);
        tokio::process::Command::new("gio").arg("open").arg(target).spawn()?;
        Ok(())
    }

    /// Gives this account LXD access (joins the lxd group through polkit).
    pub async fn fix_lxd(&self) -> Result<()> {
        let needs = self.state.lock().await.needs.clone();
        let user = paths::user_name();
        // lxd-setup installs/initialises LXD only where needed and adds
        // the account to the lxd group (polkit: org.umoja.agents.lxd-setup).
        let _ = needs;
        let status = tokio::process::Command::new("pkexec")
            .arg(paths::libexec("lxd-setup"))
            .arg(&user)
            .status()
            .await?;
        if !status.success() {
            bail!("That didn't work ({status}).");
        }
        // lxd-setup also adds the user to the lxd group; group membership
        // only reaches this session after a new login.
        let ready = matches!(Lxd::new(&paths::lxd_project()).access(), crate::lxd::Access::Ok);
        let mut s = self.state.lock().await;
        if ready {
            s.needs.clear();
            s.error = "All set. Set up Hermes again.".into();
        } else {
            s.needs = "relogin".into();
            s.error = "Done. Log out and back in, then set up Hermes again.".into();
        }
        drop(s);
        self.changed().await;
        Ok(())
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

/// A task interrupted by sleep is carried on at most this often.
const MAX_CARRY_ON: u32 = 2;
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

/// The Lemonade model the assistant uses, picked the same way.
/// Hermes refuses models with less context than this (its tool-calling
/// floor), so the local model is loaded with this window for it.
const HERMES_CONTEXT: u32 = 65536;

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

/// Asks Umoja's Lemonade to have the local model loaded with Hermes's
/// context window (the assistant's default is smaller). Best effort: if
/// it fails, Hermes reports the context problem itself.
async fn preload_local_model() {
    if std::env::var_os("UMOJA_AGENTS_MODEL_PORT").is_some() {
        return;
    }
    let body = serde_json::json!({ "model_name": local_model(), "ctx_size": HERMES_CONTEXT }).to_string();
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

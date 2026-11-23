//! What the shell shows: setup progress, the sandbox, tasks and the
//! approvals waiting for the user. Sent whole as JSON on every change
//! (StateChanged), kept small, and saved so tasks survive a restart.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Setup {
    /// Never set up: Ask Umoja offers "Set up Hermes".
    #[default]
    None,
    Working,
    Ready,
    /// Stopped with `error`; `needs` says what the user can do.
    Error,
}

/// The guardrail presets, mapped onto OpenShell policy and Hermes approvals.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Ask for every new site and every risky command.
    Careful,
    /// A few read-only research sites are open; ask for the rest.
    #[default]
    Balanced,
    /// OpenShell approves on its own what its prover finds harmless;
    /// only risky requests ask.
    HandsOff,
}

impl Preset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "careful" => Some(Self::Careful),
            "balanced" => Some(Self::Balanced),
            "hands-off" | "handsoff" => Some(Self::HandsOff),
            _ => None,
        }
    }

    /// Sites (host, port) allowed up front, besides the model.
    pub fn open_sites(self) -> &'static [(&'static str, u16)] {
        match self {
            Self::Careful => &[],
            // Web search (Hermes's ddgs backend asks these engines) and a
            // few research sites.
            Self::Balanced | Self::HandsOff => &[
                ("html.duckduckgo.com", 443),
                ("lite.duckduckgo.com", 443),
                ("duckduckgo.com", 443),
                ("www.bing.com", 443),
                ("search.brave.com", 443),
                ("www.mojeek.com", 443),
                ("search.yahoo.com", 443),
                ("www.google.com", 443),
                ("en.wikipedia.org", 443),
                ("api.github.com", 443),
                ("raw.githubusercontent.com", 443),
                ("pypi.org", 443),
                ("files.pythonhosted.org", 443),
            ],
        }
    }

    /// Whether the gateway may approve harmless proposals by itself.
    pub fn auto_approve(self) -> bool {
        matches!(self, Self::HandsOff)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    #[default]
    Running,
    /// Waiting on an approval or a question.
    Waiting,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Hermes's run id.
    #[serde(default)]
    pub run: String,
    pub title: String,
    pub prompt: String,
    pub status: TaskStatus,
    /// What it's doing now: "Running curl", "Writing lisbon.md".
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub steps: u32,
    pub started: u64,
    #[serde(default)]
    pub finished: u64,
    /// Hermes's answer (trimmed), or why it failed.
    #[serde(default)]
    pub result: String,
    /// Files it made or changed in ~/Agents/Hermes, as host paths.
    #[serde(default)]
    pub files: Vec<String>,
    /// Sites it reached (host:port), for "what did it do?".
    #[serde(default)]
    pub sites: Vec<String>,
    /// Tools it ran, in order (name only).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Where the job came from when not from Umoja: "telegram" (the agent
    /// answers chat messages itself; agentd only follows along).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// The task this one follows up on (same conversation), if any.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    /// Hermes's session (its conversation), to carry on in it.
    #[serde(default)]
    pub session: String,
    /// In flight when the computer went to sleep or the session ended:
    /// if its run dies soon after, it's carried on instead of failing.
    #[serde(default)]
    pub interrupted: bool,
    /// How often it was carried on after an interruption.
    #[serde(default)]
    pub resumes: u32,
    /// Its last steps, for the "carry on" message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalKind {
    /// OpenShell held a connection the policy doesn't allow.
    #[default]
    Network,
    /// Hermes wants to run a command its approval rules flag.
    Command,
    /// Hermes asks the user something (clarify).
    Question,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub kind: ApprovalKind,
    /// Task it belongs to, when known.
    #[serde(default)]
    pub task: String,
    /// One line: "Hermes wants to reach example.org".
    pub title: String,
    /// Plain-language detail under it.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub binary: String,
    #[serde(default)]
    pub command: String,
    /// OpenShell's prover flagged it: never offer "always".
    #[serde(default)]
    pub flagged: bool,
    pub created: u64,
    /// Bookkeeping to answer it (chunk id + review token, or Hermes prompt
    /// id + execution generation). Not for display.
    #[serde(default, skip_serializing)]
    pub answer_key: (String, String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Shared {
    pub path: String,
    pub writable: bool,
    /// Where the agent sees it.
    pub inside: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Messaging {
    pub platform: String,
    /// Telegram user ids allowed to talk to Hermes.
    pub allowed: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub setup: Setup,
    /// The agent this account runs: a catalog id ("hermes").
    #[serde(default = "default_agent")]
    pub agent: String,
    /// Its display name, and every agent Umoja can set up (id, name,
    /// description), for the shell. Filled in at start, not saved.
    #[serde(default, skip_deserializing)]
    pub agent_name: String,
    #[serde(default, skip_deserializing)]
    pub agents: Vec<AgentChoice>,
    /// What setup is doing now, for the progress card.
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub error: String,
    /// "lxd-group" (log out and in after joining), "lxd-install"…
    #[serde(default)]
    pub needs: String,
    #[serde(default)]
    pub preset: Preset,
    /// "local" (Lemonade, the default) or "cloud".
    #[serde(default = "local")]
    pub model: String,
    #[serde(default)]
    pub paused: bool,
    /// "pausing" or "resuming" while the sandbox stops or starts.
    #[serde(default, skip_deserializing)]
    pub switching: String,
    /// Sandbox phase as OpenShell reports it: running, stopped, starting…
    #[serde(default)]
    pub sandbox: String,
    #[serde(default)]
    pub memory_mb: u32,
    #[serde(default)]
    pub memory_used_mb: u32,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub approvals: Vec<Approval>,
    #[serde(default)]
    pub shared: Vec<Shared>,
    #[serde(default)]
    pub messaging: Option<Messaging>,
    /// Secrets and ids kept on disk only (never sent over D-Bus).
    #[serde(default, skip_serializing)]
    pub private: Private,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentChoice {
    pub id: String,
    pub name: String,
    pub description: String,
}

fn default_agent() -> String {
    crate::catalog::DEFAULT.into()
}

fn local() -> String {
    "local".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Private {
    /// Hermes API_SERVER_KEY inside the sandbox.
    pub api_key: String,
    /// Approval notifications still on screen: closed by the next daemon,
    /// which asks again for whatever is still waiting.
    #[serde(default)]
    pub approval_notes: Vec<u32>,
}

/// What goes to disk: the state plus the private part.
#[derive(Serialize, Deserialize)]
struct Saved {
    #[serde(flatten)]
    state: State,
    #[serde(default)]
    private: Private,
}

pub const KEEP_TASKS: usize = 50;

impl State {
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(crate::paths::state_file()) else {
            return Self::default();
        };
        match serde_json::from_str::<Saved>(&text) {
            Ok(saved) => {
                let mut s = saved.state;
                s.private = saved.private;
                // Approvals and live phases don't survive a restart: they
                // are fetched again. A task that was running is unknown now.
                s.approvals.clear();
                s.sandbox.clear();
                for t in &mut s.tasks {
                    if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting) {
                        t.status = TaskStatus::Running;
                        t.step = "Reconnecting…".into();
                        t.interrupted = true;
                    }
                }
                if s.setup == Setup::Working {
                    s.setup = Setup::None;
                }
                s
            }
            Err(e) => {
                tracing::warn!(error = %e, "ignoring unreadable agents state");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let dir = crate::paths::state_dir();
        let _ = std::fs::create_dir_all(&dir);
        let saved = Saved {
            state: self.clone(),
            private: self.private.clone(),
        };
        let Ok(json) = serde_json::to_string_pretty(&saved) else { return };
        let path = crate::paths::state_file();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            let _ = std::fs::rename(tmp, path);
        }
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn trim_tasks(&mut self) {
        if self.tasks.len() > KEEP_TASKS {
            let drop = self.tasks.len() - KEEP_TASKS;
            self.tasks.drain(..drop);
        }
    }
}

/// A short title from the prompt: its first line, at most eight words.
pub fn title_for(prompt: &str) -> String {
    let line = prompt
        .trim()
        .trim_start_matches("@hermes")
        .trim_start_matches(&[',', ':', ' '][..])
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut title = words.iter().take(8).copied().collect::<Vec<_>>().join(" ");
    if words.len() > 8 {
        title.push('…');
    }
    let mut chars = title.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Task".into(),
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title_for("@hermes write a haiku about Lisbon"), "Write a haiku about Lisbon");
        assert_eq!(
            title_for("compare the three cheapest flights to Lisbon in March and put it in a doc"),
            "Compare the three cheapest flights to Lisbon in…"
        );
        assert_eq!(title_for("   "), "Task");
    }

    #[test]
    fn private_stays_off_dbus() {
        let mut s = State::default();
        s.private.api_key = "secret".into();
        s.approvals.push(Approval { answer_key: ("chunk".into(), "token".into()), ..Default::default() });
        let json = s.json();
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }

    #[test]
    fn presets() {
        assert_eq!(Preset::parse("hands-off"), Some(Preset::HandsOff));
        assert!(Preset::Careful.open_sites().is_empty());
        assert!(Preset::HandsOff.auto_approve());
    }
}

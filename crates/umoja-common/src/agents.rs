//! AI coding agents (Claude Code, Codex, GitHub Copilot CLI, Antigravity
//! CLI, OpenCode, Gemini CLI), so the assistant can hand a job it can't
//! do well (writing a script, changing code) to one of them.
//!
//! [`KNOWN`] is the one table of what Umoja knows about each agent: its
//! command, how to give it a first prompt, how to install it and how to
//! sign in. Umoja finds the agents on PATH or in the user's own bin
//! folders, uses the one picked in `org.umoja.shell ai-agent` (or the
//! first one found), and starts it in a terminal with the task as its
//! first prompt, in the agent's normal mode, so the agent still asks
//! before it changes anything. `umoja-agent-setup` gives the agents
//! Umoja's skill and tools.
//!
//! Umoja installs an agent only when the user asks (the Install button,
//! or the assistant's install_app with an Allow dialog): from a snap when
//! the vendor or Ubuntu publishes one, else with the vendor's own
//! installer, into `~/.local/bin`, without sudo. The unofficial,
//! strictly confined snaps of some agents can't reach the user's projects
//! or tools, so they are never used. Signing in is always the agent's own
//! flow (a browser or a device code) in a terminal; Umoja never sees a
//! token, and only checks whether the agent's credentials file exists.

use crate::gsettings as gs;
use std::path::{Path, PathBuf};

pub const AGENT_KEY: &str = "ai-agent";

/// How Umoja installs an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// A snap from the vendor or Ubuntu.
    Snap { name: &'static str, classic: bool },
    /// The vendor's own install script, piped to `shell`. It installs for
    /// this user only (`~/.local/bin`), without sudo.
    Script {
        url: &'static str,
        shell: &'static str,
    },
    /// Umoja doesn't offer to install it (Gemini CLI, which Google is
    /// replacing with Antigravity CLI).
    None,
}

/// Whether an agent is ready for jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    NotInstalled,
    /// Installed, but its credentials file is missing.
    SignedOut,
    /// Installed and signed in.
    Ready,
    /// Installed; it keeps its sign-in in the keyring, so Umoja can't
    /// tell whether it is signed in.
    Installed,
}

/// A coding agent Umoja knows how to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Agent {
    /// Command name and settings value, e.g. `claude`.
    pub id: &'static str,
    /// Product name, e.g. "Claude Code".
    pub name: &'static str,
    /// Flag that starts an interactive session on a first prompt; `None`
    /// when the prompt goes straight after the command.
    pub prompt_flag: Option<&'static str>,
    pub install: Install,
    /// Arguments of the agent's sign-in command; empty when the agent
    /// signs in on its first run instead.
    pub login: &'static [&'static str],
    /// Files (relative to home) that exist once the agent is signed in;
    /// empty when it keeps its sign-in in the keyring.
    pub credentials: &'static [&'static str],
    /// Other names people use for it, for "install …" requests.
    pub aliases: &'static [&'static str],
}

pub const KNOWN: &[Agent] = &[
    Agent {
        id: "claude",
        name: "Claude Code",
        prompt_flag: None,
        install: Install::Script {
            url: "https://claude.ai/install.sh",
            shell: "bash",
        },
        login: &["auth", "login"],
        credentials: &[".claude/.credentials.json"],
        aliases: &["claude", "claude code", "claude-code"],
    },
    Agent {
        id: "codex",
        name: "Codex",
        prompt_flag: None,
        install: Install::Script {
            url: "https://chatgpt.com/codex/install.sh",
            shell: "sh",
        },
        login: &["login"],
        credentials: &[".codex/auth.json"],
        aliases: &["codex", "codex cli", "openai codex"],
    },
    Agent {
        id: "copilot",
        name: "GitHub Copilot",
        prompt_flag: Some("-i"),
        install: Install::Script {
            url: "https://gh.io/copilot-install",
            shell: "bash",
        },
        login: &["login"],
        credentials: &[],
        aliases: &[
            "copilot",
            "copilot cli",
            "github copilot",
            "github copilot cli",
            "copilot-cli",
        ],
    },
    Agent {
        id: "agy",
        name: "Antigravity",
        prompt_flag: Some("-i"),
        install: Install::Script {
            url: "https://antigravity.google/cli/install.sh",
            shell: "bash",
        },
        login: &[],
        credentials: &[],
        aliases: &[
            "agy",
            "antigravity",
            "antigravity cli",
            "google antigravity",
            "antigravity-cli",
        ],
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
        prompt_flag: Some("--prompt"),
        install: Install::Snap {
            name: "opencode",
            classic: true,
        },
        login: &["auth", "login"],
        // Its own sign-in, or the key umoja-agent-setup gave it for the
        // assistant's cloud model.
        credentials: &[".local/share/opencode/auth.json", ".local/share/umoja/opencode-cloud.key"],
        aliases: &["opencode", "open code"],
    },
    Agent {
        id: "gemini",
        name: "Gemini CLI",
        prompt_flag: Some("--prompt-interactive"),
        install: Install::None,
        login: &[],
        credentials: &[".gemini/oauth_creds.json"],
        aliases: &["gemini", "gemini cli", "gemini-cli"],
    },
];

impl Agent {
    pub fn by_id(id: &str) -> Option<Agent> {
        KNOWN.iter().copied().find(|a| a.id == id)
    }

    /// The agent a name like "Claude Code" or "copilot cli" means.
    pub fn by_name(name: &str) -> Option<Agent> {
        let n = name.trim().to_lowercase();
        KNOWN
            .iter()
            .copied()
            .find(|a| a.id == n || a.name.to_lowercase() == n || a.aliases.contains(&n.as_str()))
    }

    /// Where the agent's command is, if installed.
    pub fn path(&self) -> Option<PathBuf> {
        find_program(self.id)
    }

    pub fn state(&self) -> State {
        if self.path().is_none() {
            return State::NotInstalled;
        }
        if self.credentials.is_empty() {
            return State::Installed;
        }
        let home = glib::home_dir();
        if self.credentials.iter().any(|c| home.join(c).exists()) {
            State::Ready
        } else {
            State::SignedOut
        }
    }

    pub fn can_install(&self) -> bool {
        self.install != Install::None
    }

    /// The command line that starts it on `task` (empty: no first prompt).
    pub fn argv(&self, program: &Path, task: &str) -> Vec<String> {
        let mut v = vec![program.to_string_lossy().into_owned()];
        let task = task.trim();
        if !task.is_empty() {
            if let Some(flag) = self.prompt_flag {
                v.push(flag.into());
            }
            v.push(task.to_string());
        }
        v
    }

    /// Shell command that signs the agent in: its login command, or the
    /// agent itself when it signs in on its first run. `None` when it
    /// isn't installed.
    pub fn sign_in_command(&self) -> Option<String> {
        let program = self.path()?;
        let mut words = vec![sh_quote(&program.to_string_lossy())];
        words.extend(self.login.iter().map(|w| sh_quote(w)));
        Some(words.join(" "))
    }

    /// Shell command that installs the agent, connects it to Umoja's
    /// skill and tools, and signs it in. `None` when Umoja doesn't
    /// install it.
    pub fn install_command(&self) -> Option<String> {
        let install = match self.install {
            Install::Snap { name, classic } => {
                let args = if classic {
                    format!("{name} --classic")
                } else {
                    name.to_string()
                };
                // snapd asks for the password through polkit; sudo only if
                // there is no polkit agent. While the demo tour records
                // (its flag file exists) passwordless sudo goes first, as
                // nobody is there to answer a polkit dialog.
                format!(
                    "{{ {{ [ -e \"${{XDG_RUNTIME_DIR:-/run/user/$(id -u)}}/umoja-demo\" ] \
                     && sudo -n snap install {args} 2>/dev/null; }} \
                     || snap install {args} || sudo snap install {args}; }}"
                )
            }
            Install::Script { url, shell } => format!(
                "{{ if command -v curl >/dev/null; then curl -fsSL {url}; else wget -qO- {url}; fi; }} | {shell}"
            ),
            Install::None => return None,
        };
        // Installers put the command in ~/.local/bin, which this shell's
        // PATH may lack; the agent's own folders come after it.
        let path = std::env::join_paths(extra_dirs())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut cmd = format!(
            "{install} && export PATH=\"$PATH:{path}\" && umoja-agent-setup {id}",
            id = self.id
        );
        if !self.login.is_empty() {
            let login = format!("{} {}", self.id, self.login.join(" "));
            // Setup may already have signed it in (OpenCode on the
            // assistant's cloud model): only sign in when still needed.
            if self.credentials.is_empty() {
                cmd.push_str(&format!(" && {login}"));
            } else {
                let tests: Vec<String> = self
                    .credentials
                    .iter()
                    .map(|c| format!("[ -s \"$HOME/{c}\" ]"))
                    .collect();
                cmd.push_str(&format!(" && {{ {} || {login}; }}", tests.join(" || ")));
            }
        }
        Some(cmd)
    }
}

fn sh_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+=:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Folders agents' installers use, which a session's PATH often lacks.
fn extra_dirs() -> Vec<PathBuf> {
    let home = glib::home_dir();
    [
        ".local/bin",
        ".npm-global/bin",
        ".bun/bin",
        ".local/share/mise/shims",
        ".opencode/bin",
    ]
    .iter()
    .map(|d| home.join(d))
    .chain([PathBuf::from("/snap/bin")])
    .collect()
}

fn find_program(name: &str) -> Option<PathBuf> {
    if let Some(p) = glib::find_program_in_path(name) {
        return Some(p);
    }
    extra_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Agents installed on this computer, in [`KNOWN`] order.
pub fn installed() -> Vec<Agent> {
    KNOWN
        .iter()
        .copied()
        .filter(|a| a.path().is_some())
        .collect()
}

/// The agent to hand jobs to: the one set in `ai-agent` if it is
/// installed, else the first one found. `None` when the setting is
/// "none" or no agent is installed.
pub fn chosen() -> Option<Agent> {
    let pick = gs::get_string(gs::UMOJA_SHELL, AGENT_KEY, "");
    choose(&pick, &installed())
}

fn choose(pick: &str, installed: &[Agent]) -> Option<Agent> {
    match pick {
        "none" => None,
        "" => installed.first().copied(),
        id => installed
            .iter()
            .copied()
            .find(|a| a.id == id)
            .or_else(|| installed.first().copied()),
    }
}

/// Start `agent` in a terminal on `task`, in `folder` (default: home).
pub fn launch(agent: Agent, task: &str, folder: Option<&Path>) -> Result<(), String> {
    let program = agent
        .path()
        .ok_or_else(|| format!("{} isn't installed", agent.name))?;
    let home = glib::home_dir();
    let dir = folder.filter(|d| d.is_dir()).unwrap_or(&home);
    crate::terminal::spawn_program_in_terminal(&agent.argv(&program, task), dir)
        .map_err(|e| format!("couldn't open a terminal: {e}"))
}

/// Install `agent` in a terminal, then connect it and sign it in there.
pub fn install(agent: Agent) -> Result<(), String> {
    let cmd = agent
        .install_command()
        .ok_or_else(|| format!("Umoja can't install {}", agent.name))?;
    crate::terminal::spawn_in_terminal(&cmd).map_err(|e| format!("couldn't open a terminal: {e}"))
}

/// Sign `agent` in with its own flow, in a terminal. Its login commands
/// open the browser (or print a device code) and wait for it.
pub fn sign_in(agent: Agent) -> Result<(), String> {
    if agent.login.is_empty() {
        // Signs in on its first run: start it with no task.
        return launch(agent, "", None);
    }
    let cmd = agent
        .sign_in_command()
        .ok_or_else(|| format!("{} isn't installed", agent.name))?;
    crate::terminal::spawn_in_terminal(&cmd).map_err(|e| format!("couldn't open a terminal: {e}"))
}

/// Whether a request is a coding job a coding agent does better than the
/// desktop assistant: writing a script or program, changing code.
pub fn looks_like_coding(query: &str) -> bool {
    let q = query.to_lowercase();
    const PHRASES: &[&str] = &[
        "write a script",
        "write me a script",
        "a script that",
        "a script to",
        "bash script",
        "python script",
        "shell script",
        "write a program",
        "write code",
        "write some code",
        "make me an app",
        "build me an app",
        "make an app",
        "build an app",
        "write an app",
        "fix this bug",
        "fix the bug",
        "refactor",
        "unit test",
        "pull request",
        "this repo",
        "my repo",
        "the repo",
        "git commit",
        "rust code",
        "python code",
        "javascript",
        "typescript",
        "compile error",
        "stack trace",
    ];
    PHRASES.iter().any(|p| q.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: Agent = KNOWN[0];
    const CODEX: Agent = KNOWN[1];

    #[test]
    fn choosing() {
        assert_eq!(choose("", &[CODEX, CLAUDE]), Some(CODEX));
        assert_eq!(choose("claude", &[CODEX, CLAUDE]), Some(CLAUDE));
        assert_eq!(choose("gemini", &[CODEX]), Some(CODEX));
        assert_eq!(choose("none", &[CLAUDE]), None);
        assert_eq!(choose("", &[]), None);
    }

    #[test]
    fn command_lines() {
        let p = Path::new("/x/claude");
        assert_eq!(CLAUDE.argv(p, "  fix it "), ["/x/claude", "fix it"]);
        assert_eq!(CLAUDE.argv(p, ""), ["/x/claude"]);
        let g = Agent::by_id("gemini").unwrap();
        assert_eq!(
            g.argv(Path::new("gemini"), "hi"),
            ["gemini", "--prompt-interactive", "hi"]
        );
        let o = Agent::by_id("opencode").unwrap();
        assert_eq!(
            o.argv(Path::new("opencode"), "hi"),
            ["opencode", "--prompt", "hi"]
        );
        for id in ["copilot", "agy"] {
            let a = Agent::by_id(id).unwrap();
            assert_eq!(a.argv(Path::new(id), "hi"), [id, "-i", "hi"]);
        }
    }

    #[test]
    fn names() {
        assert_eq!(Agent::by_name("Claude Code"), Some(CLAUDE));
        assert_eq!(Agent::by_name("github copilot cli").unwrap().id, "copilot");
        assert_eq!(Agent::by_name("Antigravity").unwrap().id, "agy");
        assert_eq!(Agent::by_name(" OpenCode ").unwrap().id, "opencode");
        assert_eq!(Agent::by_name("firefox"), None);
    }

    #[test]
    fn install_commands() {
        // Snap first where the vendor or Ubuntu publishes one.
        let o = Agent::by_id("opencode").unwrap().install_command().unwrap();
        assert!(o.starts_with(
            "{ { [ -e \"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/umoja-demo\" ] \
             && sudo -n snap install opencode --classic 2>/dev/null; } \
             || snap install opencode --classic || sudo snap install opencode --classic; }"
        ));
        assert!(o.contains("&& umoja-agent-setup opencode"));
        // Skips the sign-in when setup already signed it in.
        assert!(o.ends_with(
            "&& { [ -s \"$HOME/.local/share/opencode/auth.json\" ] \
             || [ -s \"$HOME/.local/share/umoja/opencode-cloud.key\" ] || opencode auth login; }"
        ));
        // Else the vendor's installer, for this user only.
        let c = CODEX.install_command().unwrap();
        assert!(c.contains("curl -fsSL https://chatgpt.com/codex/install.sh;"));
        assert!(c.contains("| sh && export PATH="));
        assert!(!c.contains("sudo"));
        let g = Agent::by_id("copilot").unwrap().install_command().unwrap();
        assert!(g.ends_with("&& umoja-agent-setup copilot && copilot login"));
        let a = Agent::by_id("agy").unwrap().install_command().unwrap();
        assert!(a.ends_with("&& umoja-agent-setup agy"));
        assert_eq!(Agent::by_id("gemini").unwrap().install_command(), None);
        // Every agent the setup script knows is in the table and back.
        let script = include_str!("../../../bin/umoja-agent-setup");
        let line = script.lines().find(|l| l.starts_with("AGENTS=(")).unwrap();
        let ids: Vec<&str> = line["AGENTS=(".len()..line.len() - 1].split(' ').collect();
        let known: Vec<&str> = KNOWN.iter().map(|a| a.id).collect();
        assert_eq!(ids, known);
    }

    #[test]
    fn quoting() {
        assert_eq!(
            sh_quote("/home/k/.local/bin/codex"),
            "/home/k/.local/bin/codex"
        );
        assert_eq!(sh_quote("/a b/it's"), "'/a b/it'\\''s'");
    }

    #[test]
    fn coding_requests() {
        assert!(looks_like_coding(
            "Write me a script that renames my photos by date"
        ));
        assert!(looks_like_coding("refactor this function"));
        assert!(!looks_like_coding("turn on dark mode"));
        assert!(!looks_like_coding("why is my fan loud?"));
    }
}

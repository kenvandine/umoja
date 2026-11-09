//! AI coding agents the user already has (Claude Code, Codex, Gemini CLI,
//! OpenCode), so the assistant can hand a job it can't do well (writing a
//! script, changing code) to one of them.
//!
//! Umoja never installs an agent. It finds the ones on PATH or in the
//! user's own bin folders, uses the one picked in `org.umoja.shell
//! ai-agent` (or the first one found), and starts it in a terminal with
//! the task as its first prompt, in the agent's normal mode, so the agent
//! still asks before it changes anything. `umoja-agent-setup` gives the
//! agents Umoja's skill and tools.

use crate::gsettings as gs;
use std::path::{Path, PathBuf};

pub const AGENT_KEY: &str = "ai-agent";

/// A coding agent Umoja knows how to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Agent {
    /// Command name and settings value, e.g. `claude`.
    pub id: &'static str,
    /// Product name, e.g. "Claude Code".
    pub name: &'static str,
}

pub const KNOWN: &[Agent] = &[
    Agent {
        id: "claude",
        name: "Claude Code",
    },
    Agent {
        id: "codex",
        name: "Codex",
    },
    Agent {
        id: "gemini",
        name: "Gemini CLI",
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
    },
];

impl Agent {
    pub fn by_id(id: &str) -> Option<Agent> {
        KNOWN.iter().copied().find(|a| a.id == id)
    }

    /// Where the agent's command is, if installed.
    pub fn path(&self) -> Option<PathBuf> {
        find_program(self.id)
    }

    /// The command line that starts it on `task` (empty: no first prompt).
    pub fn argv(&self, program: &Path, task: &str) -> Vec<String> {
        let mut v = vec![program.to_string_lossy().into_owned()];
        let task = task.trim();
        if !task.is_empty() {
            match self.id {
                "gemini" => v.push("--prompt-interactive".into()),
                "opencode" => v.push("--prompt".into()),
                _ => {}
            }
            v.push(task.to_string());
        }
        v
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

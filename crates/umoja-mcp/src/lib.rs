//! Umoja's Model Context Protocol server.
//!
//! Everything the assistant knows about this desktop, as MCP tools any
//! client can use (the overlay assistant, Claude Code, other agents):
//! apps and how they were installed, settings in plain language, Wi-Fi,
//! Bluetooth, power, windows and workspaces, notifications, files and
//! read-only troubleshooting. Risky tools are confirmed with the user by
//! the server itself (see [`server`]), so every client gets the same
//! rules.
//!
//! ## Adding tools
//!
//! Put them in a module with a `pub fn tools() -> Vec<Tool>` and add it to
//! [`PROVIDERS`]. Use [`Tool::read_only`] for tools that only look,
//! [`Tool::new`] for low-risk changes, and [`Tool::confirm`] /
//! [`Tool::with_risk`] for anything hard to undo.

pub mod actions;
pub mod automations;
pub mod apps;
pub mod calendar;
pub mod desktop;
pub mod diagnostics;
pub mod files;
pub mod notifications;
pub mod pulse;
pub mod recall;
pub mod server;
pub mod settings;
pub mod setups;
pub mod tool;
pub mod undo;

pub use tool::{Outcome, Risk, Tool};

/// Every module that contributes tools.
const PROVIDERS: &[fn() -> Vec<Tool>] = &[
    apps::tools,
    desktop::tools,
    actions::tools,
    settings::tools,
    files::tools,
    calendar::tools,
    setups::tools,
    recall::tools,
    automations::tools,
    notifications::tools,
    diagnostics::tools,
    undo::tools,
];

/// All tools, in registration order.
pub fn registry() -> Vec<Tool> {
    PROVIDERS.iter().flat_map(|p| p()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_are_unique_and_schemas_are_objects() {
        let tools = registry();
        let mut names: Vec<_> = tools.iter().map(|t| t.name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate tool names");
        for t in &tools {
            assert_eq!(t.parameters["type"], "object", "{}", t.name);
            assert!(t.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", t.name);
            assert!(!t.describe(&json!({})).is_empty());
        }
    }

    #[test]
    fn confirm_rules() {
        let tools = registry();
        let risk = |n: &str, a: serde_json::Value| (tools.iter().find(|t| t.name == n).unwrap().risk)(&a);
        assert_eq!(risk("remove_app", json!({"name": "discord"})), Risk::High);
        assert_eq!(risk("close_window", json!({})), Risk::Medium);
        assert_eq!(risk("set_gsetting", json!({})), Risk::Medium);
        assert_eq!(risk("reply_to_notification", json!({})), Risk::Medium);
        assert_eq!(risk("run_command", json!({"command": "ls"})), Risk::Medium);
        assert_eq!(risk("run_command", json!({"command": "sudo apt install vlc"})), Risk::High);
        assert_eq!(risk("set_dark_style", json!({"enabled": true})), Risk::Low);
        assert_eq!(risk("find_app", json!({"name": "discord"})), Risk::Low);
        for t in &tools {
            if t.read_only {
                assert_eq!((t.risk)(&json!({})), Risk::Low, "{} is read-only", t.name);
            }
        }
    }
}

//! File tools: find files by what they're about (umoja-files), open them
//! and show them in Files.

use super::tools::{Risk, Run, ToolSpec, arg_str};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "search_files",
            "Find files in the user's home folder by name or by what they're about, e.g. \"the PDF about the lease from last spring\", \"beach photos\", \"tax spreadsheet 2025\". Pass the user's description as the query, including type and time words. Returns the best matches, best first, with their folder and the start of their text.",
            json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "What the file is about, in the user's words"},
                "limit": {"type": "integer", "description": "Maximum results (default 8)"}
            }, "required": ["query"]}),
            Run::Main(|a| Box::pin(search_files(a))),
        )
        .with_summary(|a| format!("Search files for “{}”", arg_str(a, "query").unwrap_or(""))),
        ToolSpec::new(
            "open_file",
            "Open a file or folder in its default app. Use a path returned by search_files.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Absolute path, or ~/..."}
            }, "required": ["path"]}),
            Run::Main(|a| Box::pin(open_file(a))),
        )
        .with_risk(open_risk)
        .with_summary(|a| format!("Open {}", short(arg_str(a, "path").unwrap_or("file")))),
        ToolSpec::new(
            "show_in_files",
            "Show a file selected in the Files app (its containing folder).",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}),
            Run::Main(|a| Box::pin(show_in_files(a))),
        )
        .with_summary(|a| format!("Show {} in Files", short(arg_str(a, "path").unwrap_or("file")))),
    ]
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => gtk4::glib::home_dir().join(rest),
        None if p == "~" => gtk4::glib::home_dir(),
        None => PathBuf::from(p),
    }
}

fn short(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

fn tilde(p: &str) -> String {
    let home = gtk4::glib::home_dir().to_string_lossy().into_owned();
    match p.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// Opening an executable (a script, a .desktop file) can run it: ask first.
fn open_risk(a: &Value) -> Risk {
    let Some(p) = arg_str(a, "path").map(expand) else { return Risk::Low };
    let exec = std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let desktop = p.extension().is_some_and(|e| e == "desktop");
    if exec || desktop { Risk::Medium } else { Risk::Low }
}

async fn search_files(a: Value) -> String {
    let Some(query) = arg_str(&a, "query") else { return "error: no query".into() };
    let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as u32;
    match crate::files::search(query, limit).await {
        Err(e) => {
            tracing::warn!(error = %e, "file search failed");
            "error: file search isn't available (umoja-files isn't running)".into()
        }
        Ok(hits) if hits.is_empty() => format!("No files matched “{query}”."),
        Ok(hits) => {
            crate::files::show_tool_hits(&hits);
            let list = hits
                .iter()
                .enumerate()
                .map(|(i, h)| format!("{}. {} — {}", i + 1, tilde(&h.path), h.detail))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{list}\n(The first {} are shown to the user as clickable rows under your answer, in this order; name only the best one or two.)",
                hits.len().min(5)
            )
        }
    }
}

async fn open_file(a: Value) -> String {
    let Some(p) = arg_str(&a, "path").map(expand) else { return "error: no path".into() };
    if !p.exists() {
        return format!("error: {} doesn't exist", p.display());
    }
    use gtk4::prelude::*;
    let ctx = gtk4::gdk::Display::default().map(|d| d.app_launch_context());
    match crate::files::open(&p.to_string_lossy(), ctx.as_ref().map(|c| c.upcast_ref())) {
        Ok(()) => {
            super::hide_when_done();
            format!("Opened {}", tilde(&p.to_string_lossy()))
        }
        Err(e) => format!("error: couldn't open {}: {e}", p.display()),
    }
}

async fn show_in_files(a: Value) -> String {
    let Some(p) = arg_str(&a, "path").map(expand) else { return "error: no path".into() };
    if !p.exists() {
        return format!("error: {} doesn't exist", p.display());
    }
    crate::files::show_in_folder(&p.to_string_lossy());
    super::hide_when_done();
    format!("Showing {} in Files", tilde(&p.to_string_lossy()))
}

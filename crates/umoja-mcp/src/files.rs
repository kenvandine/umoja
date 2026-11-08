//! File tools: find files by what they're about (umoja-files), open them
//! and show them in Files.

use crate::tool::{Outcome, Risk, Tool, arg_str, expand, tilde};
use gio::prelude::*;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use umoja_common::dbus::{FILES_BUS_NAME, FILES_INTERFACE, FILES_OBJECT_PATH};

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::read_only(
            "search_files",
            "Find files in the user's home folder by name or by what they're about, e.g. \"the PDF about the lease from last spring\", \"beach photos\", \"tax spreadsheet 2025\". Pass the user's description as the query, including type and time words. Returns the best matches, best first, with their folder and the start of their text.",
            json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "What the file is about, in the user's words"},
                "limit": {"type": "integer", "description": "Maximum results (default 8)"}
            }, "required": ["query"]}),
            |a| Box::pin(search_files(a)),
        )
        .with_summary(|a| format!("Search files for “{}”", arg_str(a, "query").unwrap_or(""))),
        Tool::new(
            "open_file",
            "Open a file or folder in its default app. Use a path returned by search_files.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Absolute path, or ~/..."}
            }, "required": ["path"]}),
            |a| Box::pin(open_file(a)),
        )
        .with_risk(open_risk)
        .with_summary(|a| format!("Open {}", short(arg_str(a, "path").unwrap_or("file")))),
        Tool::new(
            "show_in_files",
            "Show a file selected in the Files app (its containing folder).",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}),
            |a| Box::pin(show_in_files(a)),
        )
        .with_summary(|a| format!("Show {} in Files", short(arg_str(a, "path").unwrap_or("file")))),
    ]
}

fn short(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Opening an executable (a script, a .desktop file) can run it: ask first.
fn open_risk(a: &Value) -> Risk {
    let Some(p) = arg_str(a, "path").map(expand) else { return Risk::Low };
    let exec = std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let desktop = p.extension().is_some_and(|e| e == "desktop");
    if exec || desktop { Risk::Medium } else { Risk::Low }
}

async fn search_files(a: Value) -> Outcome {
    let Some(query) = arg_str(&a, "query") else { return Outcome::err("no query") };
    let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as u32;
    let hits = async {
        let conn = gio::bus_get_future(gio::BusType::Session).await?;
        let reply = conn
            .call_future(
                Some(FILES_BUS_NAME),
                FILES_OBJECT_PATH,
                FILES_INTERFACE,
                "Search",
                Some(&(query, limit).to_variant()),
                Some(glib::VariantTy::new("(a(sssd))").expect("type")),
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await?;
        let (hits,): (Vec<(String, String, String, f64)>,) = reply.get().unwrap_or_default();
        Ok::<_, glib::Error>(hits)
    }
    .await;
    match hits {
        Err(e) => {
            tracing::warn!(error = %e, "file search failed");
            Outcome::err("file search isn't available (umoja-files isn't running)")
        }
        Ok(hits) if hits.is_empty() => Outcome::ok(format!("No files matched “{query}”.")),
        Ok(hits) => {
            let list = hits
                .iter()
                .enumerate()
                .map(|(i, (path, _, detail, _))| format!("{}. {} — {}", i + 1, tilde(path), detail))
                .collect::<Vec<_>>()
                .join("\n");
            let data = json!({"hits": hits.iter().map(|(path, name, detail, score)| json!({
                "path": path, "name": name, "detail": detail, "score": score,
            })).collect::<Vec<_>>()});
            Outcome::ok(format!(
                "{list}\n(The first {} are shown to the user as clickable rows under your answer, in this order; name only the best one or two.)",
                hits.len().min(5)
            ))
            .with_data(data)
        }
    }
}

async fn open_file(a: Value) -> Outcome {
    let Some(p) = arg_str(&a, "path").map(expand) else { return Outcome::err("no path") };
    if !p.exists() {
        return Outcome::err(format!("{} doesn't exist", p.display()));
    }
    let uri = gio::File::for_path(&p).uri();
    match gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        Ok(()) => Outcome::ok(format!("Opened {}", tilde(&p.to_string_lossy()))).hide(),
        Err(e) => Outcome::err(format!("couldn't open {}: {e}", p.display())),
    }
}

async fn show_in_files(a: Value) -> Outcome {
    let Some(p) = arg_str(&a, "path").map(expand) else { return Outcome::err("no path") };
    if !p.exists() {
        return Outcome::err(format!("{} doesn't exist", p.display()));
    }
    let file = gio::File::for_path(&p);
    let uris = vec![file.uri().to_string()];
    let shown = match gio::bus_get_future(gio::BusType::Session).await {
        Ok(conn) => conn
            .call_future(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                Some(&(uris, "").to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await
            .is_ok(),
        Err(_) => false,
    };
    if !shown {
        // No FileManager1 service: open the folder itself.
        if let Some(parent) = file.parent()
            && let Err(e) = gio::AppInfo::launch_default_for_uri(&parent.uri(), None::<&gio::AppLaunchContext>)
        {
            return Outcome::err(e);
        }
    }
    Outcome::ok(format!("Showing {} in Files", tilde(&p.to_string_lossy()))).hide()
}

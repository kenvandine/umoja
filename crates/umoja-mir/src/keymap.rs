//! Keyboard layouts, as GNOME's input sources
//! (`org.gnome.desktop.input-sources`).
//!
//! mutter applies one XKB layout at a time: the first of `mru-sources`
//! that is still in `sources`, with `xkb-options`. Super+Space
//! (`switch-input-source`) moves to the next source, shows a small popup
//! naming it, and puts it first in `mru-sources`, so Settings and the next
//! login agree. Input methods (IBus sources) are not supported; they are
//! skipped. With no input sources configured (gnome-settings-daemon copies
//! them from the system on GNOME's first login, which Umoja never runs) the
//! system keyboard (`/etc/default/keyboard`, what localed writes) applies.

use gio::prelude::*;
use std::ffi::CString;
use std::sync::Mutex;
use umoja_common::gsettings as gs;

pub const INPUT_SOURCES: &str = "org.gnome.desktop.input-sources";

unsafe extern "C" {
    fn umoja_set_keymap(keymap: *const std::ffi::c_char);
}

/// The keymap string last handed to Mir, so unrelated settings changes
/// don't recompile it.
static APPLIED: Mutex<String> = Mutex::new(String::new());

/// The XKB ids ("us", "de+nodeadkeys") of the configured input sources.
fn sources(settings: &gio::Settings, key: &str) -> Vec<String> {
    if !gs::has_key(settings, key) {
        return Vec::new();
    }
    settings
        .value(key)
        .get::<Vec<(String, String)>>()
        .unwrap_or_default()
        .into_iter()
        .filter(|(kind, _)| kind == "xkb")
        .map(|(_, id)| id)
        .filter(|id| !id.is_empty())
        .collect()
}

/// (sources, index of the active one).
fn state() -> Option<(Vec<String>, usize)> {
    let s = gs::settings(INPUT_SOURCES)?;
    let all = sources(&s, "sources");
    if all.is_empty() {
        return None;
    }
    let mru = sources(&s, "mru-sources");
    let index = mru
        .iter()
        .find_map(|m| all.iter().position(|a| a == m))
        .unwrap_or(0);
    Some((all, index))
}

fn xkb_options() -> String {
    gs::settings(INPUT_SOURCES)
        .filter(|s| gs::has_key(s, "xkb-options"))
        .map(|s| s.strv("xkb-options").iter().map(|o| o.to_string()).collect::<Vec<_>>().join(","))
        .unwrap_or_default()
}

/// The system keyboard from /etc/default/keyboard: (layout, variant, options).
fn system_keyboard() -> Option<(String, String, String)> {
    let text = std::fs::read_to_string("/etc/default/keyboard").ok()?;
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_default()
    };
    let layout = get("XKBLAYOUT");
    if layout.is_empty() {
        return None;
    }
    // Several layouts ("us,de"): the first, as an input source would be.
    let first = |v: String| v.split(',').next().unwrap_or("").to_string();
    Some((first(layout), first(get("XKBVARIANT")), get("XKBOPTIONS")))
}

/// Mir's keymap string, `layout[+variant[+options]]`.
fn keymap_string(id: &str, options: &str) -> String {
    let (layout, variant) = id.split_once('+').unwrap_or((id, ""));
    if options.is_empty() {
        if variant.is_empty() { layout.to_string() } else { format!("{layout}+{variant}") }
    } else {
        format!("{layout}+{variant}+{options}")
    }
}

fn set(keymap: String) {
    let mut applied = APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    if *applied == keymap {
        return;
    }
    let Ok(c) = CString::new(keymap.clone()) else { return };
    tracing::info!(keymap, "keyboard layout");
    unsafe { umoja_set_keymap(c.as_ptr()) };
    *applied = keymap;
}

/// Apply the active input source (settings changed, or at startup).
pub fn apply() {
    let options = xkb_options();
    if let Some((all, index)) = state() {
        set(keymap_string(&all[index], &options));
    } else if let Some((layout, variant, sys_options)) = system_keyboard() {
        let id = if variant.is_empty() { layout } else { format!("{layout}+{variant}") };
        let options = if options.is_empty() { sys_options } else { options };
        set(keymap_string(&id, &options));
    }
}

/// Super+Space: the next (or previous) input source.
pub fn switch(backward: bool) {
    let Some((all, index)) = state() else { return };
    if all.len() < 2 {
        return;
    }
    let next = if backward { (index + all.len() - 1) % all.len() } else { (index + 1) % all.len() };
    // Most recently used first, as gnome-shell's InputSourceManager keeps it.
    if let Some(s) = gs::settings(INPUT_SOURCES).filter(|s| gs::has_key(s, "mru-sources")) {
        let mut mru: Vec<(String, String)> = vec![("xkb".into(), all[next].clone())];
        for id in &all {
            if *id != all[next] {
                mru.push(("xkb".into(), id.clone()));
            }
        }
        let _ = s.set_value("mru-sources", &mru.to_variant());
    }
    set(keymap_string(&all[next], &xkb_options()));
    let labels: Vec<String> = all.iter().map(|id| short_name(id)).collect();
    crate::session::overlay_call("InputSourceSwitched", (labels, next as u32));
}

/// gnome-shell's short name for a source: the layout's code ("us" -> "en"
/// is a translation table we don't have, so the code itself, as GNOME
/// falls back to).
fn short_name(id: &str) -> String {
    id.split('+').next().unwrap_or(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_mir_keymaps() {
        assert_eq!(keymap_string("us", ""), "us");
        assert_eq!(keymap_string("de+nodeadkeys", ""), "de+nodeadkeys");
        assert_eq!(keymap_string("us", "ctrl:nocaps"), "us++ctrl:nocaps");
        assert_eq!(keymap_string("fr+azerty", "a,b"), "fr+azerty+a,b");
    }
}

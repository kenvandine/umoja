//! Model picker for the assistant: a button beside the search entry that
//! lists Lemonade's chat models that fit this machine, with their size and
//! whether they're already downloaded. Picking one sets
//! `org.umoja.shell lemonade-model`; the model watcher then downloads it.
//! A second page sets up an OpenAI-compatible cloud model (see `cloud`).

use std::collections::HashSet;
use std::time::Duration;

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use serde::Deserialize;
use umoja_common::assistant_backend as backend;
use umoja_common::{gsettings as gs, i18n, lemonade};

use crate::cloud::CloudForm;
use crate::lemonade::{DEFAULT_MODEL, RUNTIME};

/// The catalog that ships inside the embedded Lemonade build.
const CATALOG: &str = "/usr/lib/umoja/lemonade/resources/server_models.json";

#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub size_gb: f64,
    pub labels: Vec<String>,
}

impl ModelInfo {
    /// Tool-calling models drive the assistant's action chips best.
    pub fn recommended(&self) -> bool {
        self.labels.iter().any(|l| l == "tool-calling")
    }
}

#[derive(Deserialize)]
struct CatalogEntry {
    #[serde(default)]
    id: String,
    /// Only in the server's listing; the bundled catalog has no state.
    #[serde(default)]
    downloaded: bool,
    #[serde(default)]
    recipe: String,
    #[serde(default)]
    suggested: bool,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    size: f64,
}

/// Chat models that run on llama.cpp (any GPU via Vulkan, or CPU) and fit
/// in half the machine's memory, smallest first, recommended ones on top.
pub fn catalog(json: &str, mem_gb: f64) -> Vec<ModelInfo> {
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, CatalogEntry>>(json)
    else {
        return Vec::new();
    };
    pick(map.into_iter().collect(), mem_gb)
}

fn pick(entries: Vec<(String, CatalogEntry)>, mem_gb: f64) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = entries
        .into_iter()
        .filter(|(_, e)| {
            e.suggested
                && e.recipe == "llamacpp"
                && e.labels.iter().any(|l| l == "chat")
                && e.size > 0.0
                && e.size <= mem_gb * 0.5
        })
        .map(|(id, e)| ModelInfo {
            id,
            size_gb: e.size,
            labels: e.labels,
        })
        .collect();
    out.sort_by(|a, b| {
        b.recommended()
            .cmp(&a.recommended())
            .then(a.size_gb.total_cmp(&b.size_gb))
    });
    out
}

/// "Coding agent: [Claude Code ▾]": who gets coding jobs. Lists only the
/// agents installed here, plus None; hidden when there are none.
fn agent_row() -> gtk::Box {
    use umoja_common::agents;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_margin_top(6);
    let installed = agents::installed();
    if installed.is_empty() {
        row.set_visible(false);
        return row;
    }
    let label = gtk::Label::builder()
        .label(i18n::umoja("Coding agent"))
        .xalign(0.0)
        .hexpand(true)
        .tooltip_text(i18n::umoja(
            "The assistant hands coding jobs, like writing a script, to this app",
        ))
        .build();
    let mut names: Vec<String> = installed.iter().map(|a| a.name.to_string()).collect();
    names.push(i18n::umoja("None"));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let drop = gtk::DropDown::from_strings(&refs);
    let current = gs::get_string(gs::UMOJA_SHELL, agents::AGENT_KEY, "");
    let selected = if current == "none" {
        installed.len()
    } else {
        agents::chosen()
            .and_then(|c| installed.iter().position(|a| *a == c))
            .unwrap_or(0)
    };
    drop.set_selected(selected as u32);
    drop.connect_selected_notify(move |d| {
        let value = installed
            .get(d.selected() as usize)
            .map(|a| a.id)
            .unwrap_or("none");
        if let Some(s) = gs::settings(gs::UMOJA_SHELL).filter(|s| gs::has_key(s, agents::AGENT_KEY))
        {
            let _ = s.set_string(agents::AGENT_KEY, value);
        }
    });
    row.append(&label);
    row.append(&drop);
    row
}

/// The model to suggest for this computer: the largest one that is good
/// at tools and fits in a quarter of the memory, so apps keep the rest.
pub fn best_fit(models: &[ModelInfo], mem_gb: f64) -> Option<&ModelInfo> {
    models
        .iter()
        .filter(|m| m.recommended() && m.size_gb <= mem_gb * 0.25)
        .max_by(|a, b| a.size_gb.total_cmp(&b.size_gb))
}

fn mem_total_gb() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<f64>()
                .ok()
        })
        .map(|kb| kb / 1024.0 / 1024.0)
        .unwrap_or(16.0)
}

/// The server's full registry (`GET /api/v1/models?show_all=true`), which
/// includes models the user registered and each one's `downloaded` state.
async fn server_models(mem_gb: f64) -> Option<(Vec<ModelInfo>, HashSet<String>)> {
    #[derive(Deserialize)]
    struct List {
        data: Vec<CatalogEntry>,
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let url = format!("{}/api/v1/models?show_all=true", lemonade::base_url());
    let list: List = client.get(url).send().await.ok()?.json().await.ok()?;
    let downloaded = list
        .data
        .iter()
        .filter(|e| e.downloaded)
        .map(|e| e.id.clone())
        .collect();
    let entries = list.data.into_iter().map(|e| (e.id.clone(), e)).collect();
    Some((pick(entries, mem_gb), downloaded))
}

thread_local! {
    /// The settings button, for [`show_page`].
    static BUTTON: std::cell::RefCell<Option<glib::WeakRef<gtk::MenuButton>>> =
        const { std::cell::RefCell::new(None) };
    /// The page the popover should open on next (instead of the model's).
    static OPEN_ON: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Open the settings popover on a page ("local", "cloud" or "voice").
/// False when the button isn't on screen (Ask Umoja is closed).
pub fn show_page(page: &'static str) -> bool {
    let Some(b) = BUTTON.with(|b| b.borrow().as_ref().and_then(|w| w.upgrade())) else {
        return false;
    };
    if !b.is_mapped() {
        return false;
    }
    OPEN_ON.with(|o| o.set(Some(page)));
    b.popdown();
    b.popup();
    true
}

/// Close the settings popover.
pub fn close() {
    if let Some(b) = BUTTON.with(|b| b.borrow().as_ref().and_then(|w| w.upgrade())) {
        b.popdown();
    }
}

pub fn button() -> gtk::MenuButton {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["model-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .min_content_width(380)
        .child(&list)
        .build();
    let heading = gtk::Label::builder()
        .label(i18n::umoja("Assistant"))
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let note = gtk::Label::builder()
        .label(i18n::umoja(
            "Runs on this computer. New models download in the background.",
        ))
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(44)
        .css_classes(["dim-label", "caption"])
        .build();
    let free = gtk::Button::builder()
        .label(i18n::umoja("Free Memory"))
        .tooltip_text(i18n::umoja(
            "Unload the model until the next question, to give its memory to other apps",
        ))
        .halign(gtk::Align::Start)
        .css_classes(["flat"])
        .build();
    let free_status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(44)
        .hexpand(true)
        .css_classes(["dim-label", "caption"])
        .build();
    {
        let status = free_status.clone();
        free.connect_clicked(move |b| {
            b.set_sensitive(false);
            let (b, status) = (b.clone(), status.clone());
            glib::spawn_future_local(async move {
                let r = gtk::gio::spawn_blocking(lemonade::unload)
                    .await
                    .unwrap_or_else(|_| Err("crashed".into()));
                status.set_text(&match r {
                    Ok(_) => {
                        i18n::umoja("Memory freed. The model loads again with your next question.")
                    }
                    Err(e) => format!("{} ({e})", i18n::umoja("Couldn't free the memory")),
                });
                b.set_sensitive(true);
            });
        });
    }
    let free_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    free_row.append(&free);
    free_row.append(&free_status);
    let local_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    local_page.append(&note);
    local_page.append(&scroller);
    local_page.append(&free_row);

    let popover = gtk::Popover::new();
    let cloud = CloudForm::new(&popover);

    let stack = gtk::Stack::builder()
        .vhomogeneous(false)
        .hhomogeneous(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_titled(&local_page, Some("local"), &i18n::umoja("This Computer"));
    stack.add_titled(&cloud.page, Some("cloud"), &i18n::umoja("Cloud"));
    let voice = crate::voice_picker::VoicePage::new();
    stack.add_titled(&voice.page, Some("voice"), &i18n::umoja("Voice"));
    let switcher = gtk::StackSwitcher::builder()
        .stack(&stack)
        .halign(gtk::Align::Center)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.append(&heading);
    content.append(&switcher);
    content.append(&stack);
    content.append(&agent_row());
    popover.set_child(Some(&content));
    {
        let popover = popover.clone();
        list.connect_row_activated(move |_, row| {
            let id = row.widget_name();
            if let Some(s) = gs::settings(gs::UMOJA_SHELL) {
                let _ = s.set_string("lemonade-model", &id);
                // An older installed schema lacks the key; GIO aborts on unknown keys.
                if gs::has_key(&s, backend::BACKEND_KEY) {
                    let _ = s.set_string(backend::BACKEND_KEY, backend::LOCAL);
                }
            }
            popover.popdown();
        });
    }
    let button = gtk::MenuButton::builder()
        .icon_name("preferences-system-symbolic")
        .tooltip_text(i18n::umoja("Assistant model and voice"))
        .css_classes(["circular", "model-pill"])
        .popover(&popover)
        .build();

    BUTTON.with(|b| b.replace(Some(button.downgrade())));
    popover.connect_show(move |popover| {
        let page = OPEN_ON.with(|o| o.take()).unwrap_or(if backend::current().cloud {
            "cloud"
        } else {
            "local"
        });
        stack.set_visible_child_name(page);
        cloud.load();
        voice.load();
        let list = list.clone();
        let popover = popover.clone();
        let mem = mem_total_gb();
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn(async move {
            let _ = tx.send(server_models(mem).await).await;
        });
        glib::spawn_future_local(async move {
            // Server not up yet: fall back to the catalog Lemonade ships.
            let (models, downloaded) = rx.recv().await.ok().flatten().unwrap_or_else(|| {
                let models = std::fs::read_to_string(CATALOG)
                    .map(|j| catalog(&j, mem))
                    .unwrap_or_default();
                (models, HashSet::new())
            });
            fill(&list, &models, &downloaded, &popover, mem);
        });
    });
    button
}

fn fill(
    list: &gtk::ListBox,
    models: &[ModelInfo],
    downloaded: &HashSet<String>,
    popover: &gtk::Popover,
    mem_gb: f64,
) {
    // The best fit for this computer goes first.
    let best = best_fit(models, mem_gb).map(|m| m.id.clone());
    let mut models: Vec<&ModelInfo> = models.iter().collect();
    if let Some(b) = &best {
        models.sort_by_key(|m| &m.id != b);
    }
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }
    // No check mark while the cloud model is in use.
    let current = if backend::current().cloud {
        String::new()
    } else {
        gs::get_string(gs::UMOJA_SHELL, "lemonade-model", DEFAULT_MODEL)
    };
    for m in models {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(6);
        row.set_margin_end(6);

        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.set_opacity(if m.id == current { 1.0 } else { 0.0 });
        row.append(&check);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let name = gtk::Label::builder()
            .label(&m.id)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        text.append(&name);
        let mut details = vec![format!("{:.1} GB", m.size_gb)];
        if best.as_ref() == Some(&m.id) {
            details.push(i18n::umoja("Best for this computer"));
        } else if m.recommended() {
            details.push(i18n::umoja("Recommended"));
        }
        if m.labels.iter().any(|l| l == "vision") {
            details.push(i18n::umoja("Vision"));
        }
        if m.labels.iter().any(|l| l == "reasoning") {
            details.push(i18n::umoja("Reasoning"));
        }
        let sub = gtk::Label::builder()
            .label(details.join(" · "))
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        text.append(&sub);
        row.append(&text);

        let state = if downloaded.contains(&m.id) {
            gtk::Image::from_icon_name("emblem-ok-symbolic")
        } else {
            gtk::Image::from_icon_name("folder-download-symbolic")
        };
        state.set_tooltip_text(Some(&if downloaded.contains(&m.id) {
            i18n::umoja("Downloaded")
        } else {
            i18n::umoja("Downloads when selected")
        }));
        state.add_css_class("dim-label");
        row.append(&state);

        // The row's name carries the model id for the activation handler.
        let item = gtk::ListBoxRow::builder()
            .child(&row)
            .activatable(true)
            .name(&m.id)
            .build();
        list.append(&item);
    }
    let _ = popover;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_filters_and_orders() {
        let json = r#"{
            "Big-GGUF": {"recipe":"llamacpp","suggested":true,"labels":["chat","tool-calling"],"size":40.0},
            "Small-GGUF": {"recipe":"llamacpp","suggested":true,"labels":["chat"],"size":1.0},
            "Tool-GGUF": {"recipe":"llamacpp","suggested":true,"labels":["chat","tool-calling"],"size":2.5},
            "Npu": {"recipe":"ryzenai-llm","suggested":true,"labels":["chat"],"size":2.0},
            "Hidden-GGUF": {"recipe":"llamacpp","suggested":false,"labels":["chat"],"size":1.0},
            "Embed-GGUF": {"recipe":"llamacpp","suggested":true,"labels":["embeddings"],"size":0.5}
        }"#;
        // (The server's listing has the same fields plus id/downloaded.)
        let ids: Vec<String> = catalog(json, 32.0).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["Tool-GGUF", "Small-GGUF"]);
    }

    #[test]
    fn best_fit_scales_with_memory() {
        let m = |id: &str, size: f64, tools: bool| ModelInfo {
            id: id.into(),
            size_gb: size,
            labels: if tools {
                vec!["chat".into(), "tool-calling".into()]
            } else {
                vec!["chat".into()]
            },
        };
        let models = [
            m("4B", 2.5, true),
            m("8B", 5.0, true),
            m("14B", 9.0, true),
            m("Plain", 3.0, false),
        ];
        assert_eq!(best_fit(&models, 16.0).map(|m| m.id.as_str()), Some("4B"));
        assert_eq!(best_fit(&models, 32.0).map(|m| m.id.as_str()), Some("8B"));
        assert_eq!(best_fit(&models, 64.0).map(|m| m.id.as_str()), Some("14B"));
        assert_eq!(best_fit(&models, 8.0).map(|m| m.id.as_str()), None);
    }

    #[test]
    fn bundled_catalog_has_the_default() {
        let Ok(json) = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/lemonade/x64/resources/server_models.json"
        )) else {
            return; // vendor/ not populated in this checkout
        };
        assert!(catalog(&json, 32.0).iter().any(|m| m.id == DEFAULT_MODEL));
    }
}

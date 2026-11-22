//! "Background agent: [Hermes ▾] [Balanced ▾] Set Up": the personal agent
//! umoja-agents runs in a sandbox, in the settings popover. Picks which
//! agent (Hermes first; whatever umoja-agentd's catalog lists), sets it up
//! with the chosen guardrail preset, and changes the preset afterwards.
//! Hidden when umoja-agents isn't installed. The agent's tasks and consent
//! cards live in the panel's Agents button; this row is only the setup.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use serde_json::Value;
use umoja_common::i18n;

const BUS: &str = "org.umoja.Agents";
const PATH: &str = "/org/umoja/Agents";
const IFACE: &str = "org.umoja.Agents";

/// Preset ids, in the dropdown's order.
const PRESETS: [&str; 3] = ["careful", "balanced", "hands-off"];

pub struct BackgroundAgentRow {
    pub root: gtk::Box,
    /// Which agent; shown only when there's a choice and none is set up.
    agent: gtk::DropDown,
    preset: gtk::DropDown,
    status: gtk::Label,
    action: gtk::Button,
    /// Agent ids, in the agent dropdown's order.
    ids: RefCell<Vec<String>>,
    /// The setup state last read ("none", "working", "ready", "error").
    setup: RefCell<String>,
    /// What the user can do about a failed setup ("lxd-group"…).
    needs: RefCell<String>,
    /// Set while show() fills the dropdowns, so they don't save a pick.
    filling: Cell<bool>,
}

impl BackgroundAgentRow {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(6);
        root.set_visible(false);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let label = gtk::Label::builder()
            .label(i18n::umoja("Background agent"))
            .xalign(0.0)
            .hexpand(true)
            .tooltip_text(i18n::umoja(
                "An agent that works on longer jobs in the background, in a sandbox. Type @agent in Ask Umoja to hand it one.",
            ))
            .build();
        let agent = gtk::DropDown::from_strings(&[]);
        agent.set_visible(false);
        agent.set_tooltip_text(Some(&i18n::umoja("Which agent to set up")));
        let names = [i18n::umoja("Careful"), i18n::umoja("Balanced"), i18n::umoja("Hands-off")];
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let preset = gtk::DropDown::from_strings(&refs);
        preset.set_selected(1);
        preset.set_tooltip_text(Some(&i18n::umoja("How often the agent asks you first")));
        top.append(&label);
        top.append(&agent);
        top.append(&preset);
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .max_width_chars(36)
            .css_classes(["dim-label", "caption"])
            .build();
        let action = gtk::Button::builder().css_classes(["pill", "suggested-action"]).build();
        bottom.append(&status);
        bottom.append(&action);
        root.append(&top);
        root.append(&bottom);
        let row = Rc::new(Self {
            root,
            agent,
            preset,
            status,
            action,
            ids: RefCell::default(),
            setup: RefCell::default(),
            needs: RefCell::default(),
            filling: Cell::new(false),
        });
        row.watch();
        {
            let weak = Rc::downgrade(&row);
            row.agent.connect_selected_notify(move |_| {
                let Some(r) = weak.upgrade() else { return };
                if r.filling.get() {
                    return;
                }
                let Some(id) = r.ids.borrow().get(r.agent.selected() as usize).cloned() else { return };
                let weak = Rc::downgrade(&r);
                call("SetAgent", Some((id,).to_variant()), move |res| {
                    if let Some(r) = weak.upgrade() {
                        match res {
                            Ok(_) => r.refresh(),
                            Err(e) => r.status.set_text(&e),
                        }
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(&row);
            row.preset.connect_selected_notify(move |_| {
                let Some(r) = weak.upgrade() else { return };
                if r.filling.get() {
                    return;
                }
                r.status.set_text(&preset_text(r.preset_id()));
                if r.setup.borrow().as_str() == "ready" {
                    call("SetPreset", Some((r.preset_id(),).to_variant()), |_| {});
                }
            });
        }
        {
            let weak = Rc::downgrade(&row);
            row.action.connect_clicked(move |_| {
                let Some(r) = weak.upgrade() else { return };
                r.action.set_sensitive(false);
                let needs = r.needs.borrow().clone();
                if matches!(needs.as_str(), "lxd-install" | "lxd-group") {
                    // The password prompt; setup carries on by itself after.
                    r.status.set_text(&i18n::umoja("Waiting for your permission…"));
                    let weak = Rc::downgrade(&r);
                    call("FixLxd", None, move |res| {
                        if let Some(r) = weak.upgrade() {
                            r.action.set_sensitive(true);
                            if res.is_err() {
                                r.refresh();
                            }
                        }
                    });
                    return;
                }
                r.status.set_text(&i18n::umoja("Setting up…"));
                let weak = Rc::downgrade(&r);
                call("Setup", Some((r.preset_id(),).to_variant()), move |res| {
                    if let Some(r) = weak.upgrade() {
                        match res {
                            Ok(_) => r.status.set_text(&i18n::umoja(
                                "Setting up. Follow along from the Agents button on the top bar.",
                            )),
                            Err(e) => r.status.set_text(&e),
                        }
                        r.action.set_sensitive(true);
                    }
                });
            });
        }
        row
    }

    fn preset_id(&self) -> &'static str {
        PRESETS.get(self.preset.selected() as usize).copied().unwrap_or("balanced")
    }

    /// Follows StateChanged, so the row shows setup's progress and asks
    /// for what it needs while the settings are open.
    fn watch(self: &Rc<Self>) {
        let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
            return;
        };
        let weak = Rc::downgrade(self);
        // The subscription lives as long as the session bus connection.
        let _ = conn.signal_subscribe(
            Some(BUS),
            Some(IFACE),
            Some("StateChanged"),
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let Some(r) = weak.upgrade() else { return };
                let Some((json,)) = params.get::<(String,)>() else { return };
                if let Ok(state) = serde_json::from_str::<Value>(&json) {
                    r.show(&state);
                }
            },
        );
    }

    /// Reads the agent's state again (each time the popover opens).
    pub fn refresh(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        call("GetState", None, move |res| {
            let Some(r) = weak.upgrade() else { return };
            let Ok(v) = res else {
                // umoja-agents isn't installed (or isn't answering).
                r.root.set_visible(false);
                return;
            };
            let json = v.get::<(String,)>().map(|(s,)| s).unwrap_or_default();
            let state: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
            r.show(&state);
        });
    }

    fn show(&self, state: &Value) {
        self.root.set_visible(true);
        let setup = state["setup"].as_str().unwrap_or("none").to_owned();
        let preset = state["preset"].as_str().unwrap_or("balanced");
        let name = state["agent_name"].as_str().filter(|n| !n.is_empty()).unwrap_or("Hermes").to_owned();
        let current = state["agent"].as_str().unwrap_or_default();
        let agents: Vec<(String, String)> = state["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| Some((x["id"].as_str()?.to_owned(), x["name"].as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        self.filling.set(true);
        let names: Vec<&str> = agents.iter().map(|(_, n)| n.as_str()).collect();
        self.agent.set_model(Some(&gtk::StringList::new(&names)));
        self.agent
            .set_selected(agents.iter().position(|(id, _)| id == current).unwrap_or(0) as u32);
        *self.ids.borrow_mut() = agents.iter().map(|(id, _)| id.clone()).collect();
        self.preset
            .set_selected(PRESETS.iter().position(|p| *p == preset).unwrap_or(1) as u32);
        self.filling.set(false);
        let set_up = matches!(setup.as_str(), "ready" | "working");
        self.agent.set_visible(agents.len() > 1 && !set_up);
        let (text, button) = match setup.as_str() {
            "ready" if state["paused"] == true => (format!("{name}: {}", i18n::umoja("set up, and paused.")), None),
            "ready" => (format!("{name}: {}", preset_text(preset)), None),
            "working" => (
                format!("{}…", state["step"].as_str().unwrap_or(&i18n::umoja("Setting up"))),
                None,
            ),
            "error" => (
                state["error"].as_str().map(str::to_owned).unwrap_or_else(|| i18n::umoja("Setup didn't finish.")),
                match state["needs"].as_str().unwrap_or_default() {
                    "lxd-install" | "lxd-group" => Some(i18n::umoja("Allow")),
                    // Waiting on a new login: nothing to press.
                    "relogin" => None,
                    _ => Some(i18n::umoja("Try Again")),
                },
            ),
            _ => (
                i18n::umoja("{name} isn't set up. It runs in a sandbox and only sees the folders you share.")
                    .replace("{name}", &name),
                Some(i18n::umoja("Set Up")),
            ),
        };
        *self.setup.borrow_mut() = setup;
        *self.needs.borrow_mut() = state["needs"].as_str().unwrap_or_default().to_owned();
        self.status.set_text(&text);
        self.action.set_visible(button.is_some());
        if let Some(b) = button {
            self.action.set_label(&b);
        }
    }
}

fn preset_text(preset: &str) -> String {
    match preset {
        "careful" => i18n::umoja("Asks before every new site and every risky command."),
        "hands-off" => i18n::umoja("Only asks when something looks risky."),
        _ => i18n::umoja("A few research sites are open. Asks for the rest."),
    }
}

/// An org.umoja.Agents call; `done` gets the reply or a message to show.
fn call(method: &'static str, args: Option<glib::Variant>, done: impl FnOnce(Result<glib::Variant, String>) + 'static) {
    // FixLxd waits on the password prompt (and may install LXD).
    let timeout = if method == "FixLxd" { i32::MAX } else { 30_000 };
    glib::spawn_future_local(async move {
        let res = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(conn) => conn
                .call_future(Some(BUS), PATH, IFACE, method, args.as_ref(), None, gio::DBusCallFlags::NONE, timeout)
                .await
                .map_err(|mut e| {
                    gio::DBusError::strip_remote_error(&mut e);
                    e.message().to_string()
                }),
            Err(e) => Err(e.to_string()),
        };
        done(res);
    });
}

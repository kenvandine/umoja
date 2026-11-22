//! "Hermes: [Balanced ▾] Set Up": the background agent (umoja-agents) in
//! the settings popover. Shows whether Hermes is set up, sets it up with
//! the chosen guardrail preset, and changes the preset afterwards. Hidden
//! when umoja-agents isn't installed. Hermes's tasks and consent cards
//! live in the panel's Agents button; this row is only the setup.

use std::cell::Cell;
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

pub struct HermesRow {
    pub root: gtk::Box,
    drop: gtk::DropDown,
    status: gtk::Label,
    action: gtk::Button,
    /// The setup state last read ("none", "working", "ready", "error").
    setup: Rc<std::cell::RefCell<String>>,
    /// Set while refresh() fills the dropdown, so it doesn't save a pick.
    filling: Rc<Cell<bool>>,
}

impl HermesRow {
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
                "Hermes works on longer jobs in the background, in a sandbox. Type @hermes in Ask Umoja to hand it one.",
            ))
            .build();
        let names = [i18n::umoja("Careful"), i18n::umoja("Balanced"), i18n::umoja("Hands-off")];
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let drop = gtk::DropDown::from_strings(&refs);
        drop.set_selected(1);
        drop.set_tooltip_text(Some(&i18n::umoja("How often Hermes asks you first")));
        top.append(&label);
        top.append(&drop);
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
            drop,
            status,
            action,
            setup: Rc::default(),
            filling: Rc::default(),
        });
        {
            let weak = Rc::downgrade(&row);
            row.drop.connect_selected_notify(move |_| {
                let Some(r) = weak.upgrade() else { return };
                if r.filling.get() {
                    return;
                }
                r.status.set_text(&preset_text(r.preset()));
                if r.setup.borrow().as_str() == "ready" {
                    call("SetPreset", Some((r.preset(),).to_variant()), |_| {});
                }
            });
        }
        {
            let weak = Rc::downgrade(&row);
            row.action.connect_clicked(move |_| {
                let Some(r) = weak.upgrade() else { return };
                r.action.set_sensitive(false);
                r.status.set_text(&i18n::umoja("Setting up Hermes…"));
                let weak = Rc::downgrade(&r);
                call("Setup", Some((r.preset(),).to_variant()), move |res| {
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

    fn preset(&self) -> &'static str {
        PRESETS.get(self.drop.selected() as usize).copied().unwrap_or("balanced")
    }

    /// Reads Hermes's state again (each time the popover opens).
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
        self.filling.set(true);
        self.drop
            .set_selected(PRESETS.iter().position(|p| *p == preset).unwrap_or(1) as u32);
        self.filling.set(false);
        let (text, button) = match setup.as_str() {
            "ready" if state["paused"] == true => (i18n::umoja("Set up, and paused."), None),
            "ready" => (preset_text(preset), None),
            "working" => (
                format!("{}…", state["step"].as_str().unwrap_or(&i18n::umoja("Setting up"))),
                None,
            ),
            "error" => (
                state["error"].as_str().map(str::to_owned).unwrap_or_else(|| i18n::umoja("Setup didn't finish.")),
                Some(i18n::umoja("Try Again")),
            ),
            _ => (
                i18n::umoja("Not set up. It runs in a sandbox and only sees the folders you share."),
                Some(i18n::umoja("Set Up")),
            ),
        };
        *self.setup.borrow_mut() = setup;
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
    glib::spawn_future_local(async move {
        let res = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(conn) => conn
                .call_future(
                    Some(BUS),
                    PATH,
                    IFACE,
                    method,
                    args.as_ref(),
                    None,
                    gio::DBusCallFlags::NONE,
                    30_000,
                )
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

//! Clock and calendar, honouring `org.gnome.desktop.interface` clock keys.

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::Cell;
use std::rc::Rc;
use umoja_common::gsettings as gs;

pub struct ClockButton {
    button: gtk::MenuButton,
}

#[derive(Clone, Copy)]
struct Format {
    twelve_hour: bool,
    seconds: bool,
    date: bool,
    weekday: bool,
}

impl Format {
    fn load() -> Self {
        Self {
            twelve_hour: gs::get_string(gs::DESKTOP_INTERFACE, "clock-format", "24h") == "12h",
            seconds: gs::get_bool(gs::DESKTOP_INTERFACE, "clock-show-seconds", false),
            date: gs::get_bool(gs::DESKTOP_INTERFACE, "clock-show-date", true),
            weekday: gs::get_bool(gs::DESKTOP_INTERFACE, "clock-show-weekday", false),
        }
    }

    /// strftime pattern in the style of gnome-shell's top bar.
    fn pattern(self) -> String {
        let mut p = String::new();
        if self.weekday {
            p.push_str("%a ");
        }
        if self.date {
            p.push_str("%b %-d  ");
        }
        p.push_str(if self.twelve_hour { "%-l:%M" } else { "%H:%M" });
        if self.seconds {
            p.push_str(":%S");
        }
        if self.twelve_hour {
            p.push_str(" %p");
        }
        p
    }
}

impl ClockButton {
    pub fn new() -> Self {
        let label = gtk::Label::new(None);
        let calendar = gtk::Calendar::new();
        let popover = gtk::Popover::builder().child(&calendar).build();
        popover.connect_show(move |_| {
            if let Ok(now) = glib::DateTime::now_local() {
                calendar.select_day(&now);
            }
        });
        let button = gtk::MenuButton::builder()
            .child(&label)
            .popover(&popover)
            .css_classes(["flat", "clock"])
            .build();

        let format = Rc::new(Cell::new(Format::load()));
        let refresh = {
            let label = label.clone();
            let format = format.clone();
            move || {
                if let Ok(now) = glib::DateTime::now_local()
                    && let Ok(text) = now.format(&format.get().pattern()) {
                        label.set_text(text.trim());
                    }
            }
        };
        refresh();
        // Re-read the format only when Settings changes it, not every tick.
        let settings = gs::settings(gs::DESKTOP_INTERFACE);
        if let Some(s) = &settings {
            let format = format.clone();
            let refresh = refresh.clone();
            s.connect_changed(None, move |_, key| {
                if key.starts_with("clock-") {
                    format.set(Format::load());
                    refresh();
                }
            });
        }
        glib::timeout_add_seconds_local(1, move || {
            let _keep_alive = &settings;
            refresh();
            glib::ControlFlow::Continue
        });
        Self { button }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.button.clone().upcast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pattern() {
        let f = Format {
            twelve_hour: false,
            seconds: false,
            date: true,
            weekday: false,
        };
        assert_eq!(f.pattern(), "%b %-d  %H:%M");
        let f = Format {
            twelve_hour: true,
            seconds: true,
            date: false,
            weekday: true,
        };
        assert_eq!(f.pattern(), "%a %-l:%M:%S %p");
    }
}

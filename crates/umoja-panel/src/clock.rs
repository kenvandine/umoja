//! Clock and calendar, honouring `org.gnome.desktop.interface` clock keys.
//! 12/24-hour time follows the locale unless `clock-format` is set.

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
            twelve_hour: umoja_common::clock::twelve_hour(),
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
    /// The clock, and gnome-shell's date menu: the notification list on the
    /// left, the calendar on the right, and a dot by the time while there
    /// are notifications you haven't looked at.
    pub fn new(center: &Rc<crate::notifications::Center>) -> Self {
        let label = gtk::Label::new(None);
        let dot = gtk::Label::builder()
            .label("●")
            .css_classes(["clock-unseen"])
            .visible(false)
            .build();
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        face.append(&label);
        face.append(&dot);

        let calendar = gtk::Calendar::new();
        calendar.set_valign(gtk::Align::Start);
        // Settings > Date & Time > Week Numbers, "shown in the dropdown
        // calendar"; follows the switch live like gnome-shell.
        let weekdate = gs::watch(gs::DESKTOP_CALENDAR, "show-weekdate", {
            let calendar = calendar.clone();
            move |s, k| calendar.set_show_week_numbers(s.boolean(k))
        });
        let menu = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        menu.add_css_class("date-menu");
        let list = Rc::new(crate::notifications::MessageList::new(center));
        menu.append(&list.widget());
        // The calendar, and the selected day's events under it.
        let side = gtk::Box::new(gtk::Orientation::Vertical, 12);
        side.append(&calendar);
        let events = crate::events::Events::new(&calendar);
        side.append(&events.widget());
        menu.append(&side);
        let popover = gtk::Popover::builder()
            .child(&menu)
            .has_arrow(false)
            .build();
        crate::menu_gap::below_bar(&popover);
        umoja_common::blur::blur_behind(&popover);
        {
            let dot = dot.clone();
            let list = list.clone();
            let center = Rc::downgrade(center);
            popover.connect_show(move |p| {
                // Banners float above the menu, over the message list: a
                // click on a row's close button landed on the banner.
                if let Some(c) = center.upgrade() {
                    c.block_banners(true);
                }
                dot.set_visible(false);
                // Like gnome-shell's date menu, take the keyboard while open
                // (the quick-reply field needs it).
                crate::notifications::want_keyboard(p, true);
                if let Ok(now) = glib::DateTime::now_local() {
                    calendar.select_day(&now);
                }
                events.refresh(true);
                list.opened();
            });
        }
        {
            let list = list.clone();
            let center = Rc::downgrade(center);
            popover.connect_hide(move |p| {
                if let Some(c) = center.upgrade() {
                    c.block_banners(false);
                }
                crate::notifications::want_keyboard(p, false);
                list.closed();
            });
        }
        {
            let weak = Rc::downgrade(center);
            let seen = Rc::new(Cell::new(0usize));
            let popover = popover.clone();
            center.connect_changed(move || {
                let Some(c) = weak.upgrade() else { return };
                let count = c.notes().len();
                if count == 0 || popover.is_visible() {
                    dot.set_visible(false);
                } else if count > seen.get() {
                    dot.set_visible(true);
                }
                seen.set(count);
            });
        }
        let button = gtk::MenuButton::builder()
            .child(&face)
            .popover(&popover)
            .css_classes(["flat", "clock"])
            .build();

        let format = Rc::new(Cell::new(Format::load()));
        let refresh = {
            let label = label.clone();
            let format = format.clone();
            move || {
                if let Ok(now) = glib::DateTime::now_local()
                    && let Ok(text) = now.format(&format.get().pattern())
                {
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
            let _keep_alive = (&settings, &weekdate);
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

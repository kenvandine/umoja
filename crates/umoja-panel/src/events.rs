//! The date menu's events, as in gnome-shell: under the calendar, the
//! selected day's events from Evolution Data Server (Online Accounts,
//! GNOME Calendar), with days that have events marked in the month grid.
//! Hidden when no calendars are set up.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::calendar::{self, Event};
use umoja_common::i18n;

pub struct Events {
    root: gtk::Box,
    heading: gtk::Label,
    list: gtk::Box,
    calendar: gtk::Calendar,
    /// (year, month) of `month`, and its events.
    month: RefCell<Option<((i32, i32), Vec<Event>)>>,
    /// Bumped per fetch, so a slow answer for a month already left is dropped.
    serial: Cell<u32>,
}

/// Unix time of local midnight on `d`.
fn day_start(d: &glib::DateTime) -> i64 {
    glib::DateTime::from_local(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0)
        .map(|m| m.to_unix())
        .unwrap_or_else(|_| d.to_unix())
}

/// Local midnight at the start of `d`'s month and of the next one.
fn month_range(d: &glib::DateTime) -> (i64, i64) {
    let start = glib::DateTime::from_local(d.year(), d.month(), 1, 0, 0, 0.0);
    match start {
        Ok(s) => {
            let end = s
                .add_months(1)
                .map(|e| e.to_unix())
                .unwrap_or(s.to_unix() + 31 * 86400);
            (s.to_unix(), end)
        }
        Err(_) => (d.to_unix() - 31 * 86400, d.to_unix() + 31 * 86400),
    }
}

impl Events {
    pub fn new(calendar: &gtk::Calendar) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("date-events");
        root.set_visible(false);
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["date-events-heading"])
            .build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.append(&heading);
        root.append(&list);
        let this = Rc::new(Self {
            root,
            heading,
            list,
            calendar: calendar.clone(),
            month: RefCell::default(),
            serial: Cell::new(0),
        });
        let weak = Rc::downgrade(&this);
        calendar.connect_day_selected(move |_| {
            if let Some(e) = weak.upgrade() {
                e.refresh(false);
            }
        });
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Show the selected day; fetch its month first if it isn't loaded
    /// (or always, with `reload`, when the menu opens).
    pub fn refresh(self: &Rc<Self>, reload: bool) {
        let date = self.calendar.date();
        let key = (date.year(), date.month());
        let loaded = self.month.borrow().as_ref().is_some_and(|(k, _)| *k == key);
        if loaded && !reload {
            self.render();
            return;
        }
        if !loaded {
            // Don't show last month's events for this one meanwhile.
            self.month.replace(None);
            self.render();
        }
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        let (start, end) = month_range(&date);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            let events = calendar::fetch(&conn, start, end).await;
            let Some(this) = weak.upgrade() else { return };
            if this.serial.get() != serial {
                return;
            }
            match events {
                Some(list) => {
                    this.month.replace(Some((key, list)));
                }
                None => {
                    this.month.replace(None);
                }
            }
            this.render();
        });
    }

    fn render(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.calendar.clear_marks();
        let month = self.month.borrow();
        let Some((_, events)) = month.as_ref() else {
            self.root.set_visible(false);
            return;
        };
        self.root.set_visible(true);
        let date = self.calendar.date();
        // Mark the month's days that have something on.
        let (m0, _) = month_range(&date);
        for day in 0..31u32 {
            let start = m0 + day as i64 * 86400;
            let Ok(d) = glib::DateTime::from_unix_local(start) else {
                continue;
            };
            if d.month() != date.month() {
                break;
            }
            if !calendar::on_day(events, day_start(&d)).is_empty() {
                self.calendar.mark_day(d.day_of_month() as u32);
            }
        }
        let today = glib::DateTime::now_local().ok();
        let is_today = today
            .as_ref()
            .is_some_and(|t| (t.year(), t.day_of_year()) == (date.year(), date.day_of_year()));
        self.heading.set_text(&if is_today {
            i18n::shell("Today")
        } else {
            date.format("%A, %B %-d")
                .map(|s| s.to_string())
                .unwrap_or_default()
        });
        let day = calendar::on_day(events, day_start(&date));
        if day.is_empty() {
            let none = gtk::Label::builder()
                .label(i18n::shell("No Events"))
                .xalign(0.0)
                .css_classes(["date-events-empty"])
                .build();
            self.list.append(&none);
            return;
        }
        let twelve = umoja_common::clock::twelve_hour();
        for e in day {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
            row.add_css_class("date-event");
            row.append(
                &gtk::Label::builder()
                    .label(if e.summary.is_empty() {
                        i18n::shell("Untitled Event")
                    } else {
                        e.summary.clone()
                    })
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(28)
                    .css_classes(["date-event-title"])
                    .build(),
            );
            row.append(
                &gtk::Label::builder()
                    .label(calendar::time_label(&e, twelve))
                    .xalign(0.0)
                    .css_classes(["date-event-time", "dim-label"])
                    .build(),
            );
            self.list.append(&row);
        }
    }
}

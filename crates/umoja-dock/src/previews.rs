//! Window previews: the list shown when an app with several windows is
//! clicked, and the "All Windows" section of the right-click menu, as on
//! Ubuntu Dock (a live thumbnail with the window title under it).
//!
//! The thumbnails come from umoja-mir (`CaptureWindow`, see
//! `umoja_common::thumbnails`). The dock knows windows through
//! zwlr_foreign_toplevel, which has no id umoja-mir knows, so they are
//! matched to GetWindows entries by title and app id, like placement.rs.
//! Without Umoja's Mir (no captures) the cards keep just their titles.

use crate::tracker::{Command, WindowId, WindowInfo};
use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use umoja_common::dbus;

/// Thumbnail box in the menus, in logical pixels.
pub const THUMB_W: i32 = 240;
pub const THUMB_H: i32 = 150;

/// A window card: the preview (filled in later by `load`) over the title.
/// Returns the card and the box that receives the picture.
pub fn card(title: &str) -> (gtk::Box, gtk::Box) {
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.set_halign(gtk::Align::Center);
    holder.set_visible(false);
    let label = gtk::Label::builder()
        .label(title)
        .max_width_chars(28)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Center)
        .build();
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("dock-window-card");
    card.append(&holder);
    card.append(&label);
    (card, holder)
}

/// `list` in a scroller that grows with it up to most of a screen, for
/// apps with many windows.
pub fn scroller(list: &gtk::Box) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(640)
        .build()
}

/// Capture each window and put its picture in its card.
pub fn load(cards: Vec<(WindowInfo, gtk::Box)>) {
    if cards.is_empty() {
        return;
    }
    glib::spawn_future_local(async move {
        let Some(listed) = fetch_listed().await else {
            return;
        };
        let wanted: Vec<&WindowInfo> = cards.iter().map(|(w, _)| w).collect();
        let ids = match_ids(&wanted, &listed);
        for ((_, holder), id) in cards.iter().zip(ids) {
            let Some(id) = id else { continue };
            // The menu may have closed meanwhile.
            if holder.root().is_none() {
                return;
            }
            let Some(texture) =
                umoja_common::thumbnails::capture(id, umoja_common::thumbnails::SMALL).await
            else {
                continue;
            };
            let picture = umoja_common::thumbnails::picture(&texture, THUMB_W, THUMB_H);
            picture.add_css_class("dock-window-thumb");
            holder.append(&picture);
            holder.set_visible(true);
        }
    });
}

/// umoja-mir's window ids for `wanted`, matched on title (and app id among
/// windows with the same title), each umoja-mir window used once.
fn match_ids(wanted: &[&WindowInfo], listed: &[(u64, String, String)]) -> Vec<Option<u64>> {
    let mut used = vec![false; listed.len()];
    let mut out = vec![None; wanted.len()];
    // Exact title + app id first, then the only unused window of that app
    // (renamed since), then title alone (app id aliases, such as Chrome's).
    for pass in 0..3 {
        for (i, w) in wanted.iter().enumerate() {
            if out[i].is_some() {
                continue;
            }
            let fits = |(app, title): (&str, &str)| match pass {
                0 => title == w.title && app == w.app_id,
                1 => app == w.app_id,
                _ => title == w.title,
            };
            let candidates: Vec<usize> = (0..listed.len())
                .filter(|&j| !used[j] && fits((&listed[j].1, &listed[j].2)))
                .collect();
            if pass == 1 && candidates.len() != 1 {
                continue;
            }
            if let Some(&j) = candidates.first() {
                used[j] = true;
                out[i] = Some(listed[j].0);
            }
        }
    }
    out
}

/// (id, app id, title) of every window umoja-mir lists.
async fn fetch_listed() -> Option<Vec<(u64, String, String)>> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let v = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "GetWindows",
            None,
            Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
        .ok()?;
    let (rows,) = v.get::<(Vec<(u64, String, String, i32, i32, i32, i32, u32)>,)>()?;
    Some(
        rows.into_iter()
            .map(|(id, app, title, ..)| (id, app, title))
            .collect(),
    )
}

/// The window list shown when an app with several windows is clicked.
pub fn popover(windows: &[WindowInfo], commands: crate::tracker::Commands) -> gtk::Popover {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    list.add_css_class("dock-menu");
    let popover = gtk::Popover::builder()
        .child(&scroller(&list))
        .has_arrow(false)
        .build();
    let mut cards = Vec::new();
    for w in windows {
        let title = if w.title.is_empty() {
            w.app_id.clone()
        } else {
            w.title.clone()
        };
        let (card, holder) = card(&title);
        let row = gtk::Button::builder()
            .child(&card)
            .css_classes(["flat", "dock-window"])
            .build();
        let id: WindowId = w.id;
        let commands = commands.clone();
        let popover = popover.downgrade();
        row.connect_clicked(move |_| {
            commands.try_send(Command::Activate(id));
            if let Some(p) = popover.upgrade() {
                p.popdown();
            }
        });
        list.append(&row);
        cards.push((w.clone(), holder));
    }
    // After the popover is up, so the cards are in a window when they load.
    let cards = std::cell::RefCell::new(cards);
    popover.connect_map(move |_| load(cards.take()));
    popover
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: app.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn listed(rows: &[(u64, &str, &str)]) -> Vec<(u64, String, String)> {
        rows.iter()
            .map(|(id, a, t)| (*id, a.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn matches_title_and_app() {
        let a = win(1, "firefox", "Gmail");
        let b = win(2, "firefox", "Sheets");
        let l = listed(&[(10, "firefox", "Sheets"), (11, "firefox", "Gmail")]);
        assert_eq!(match_ids(&[&a, &b], &l), vec![Some(11), Some(10)]);
    }

    #[test]
    fn same_titles_get_different_windows() {
        let a = win(1, "org.gnome.Ptyxis", "ken@umoja: ~");
        let b = win(2, "org.gnome.Ptyxis", "ken@umoja: ~");
        let l = listed(&[(10, "org.gnome.Ptyxis", "ken@umoja: ~"), (11, "org.gnome.Ptyxis", "ken@umoja: ~")]);
        assert_eq!(match_ids(&[&a, &b], &l), vec![Some(10), Some(11)]);
    }

    #[test]
    fn app_id_aliases_match_on_title() {
        // Chrome: com.google.Chrome over foreign-toplevel, google-chrome in GetWindows.
        let a = win(1, "com.google.Chrome", "Inbox");
        let l = listed(&[(10, "google-chrome", "Inbox")]);
        assert_eq!(match_ids(&[&a], &l), vec![Some(10)]);
    }

    #[test]
    fn renamed_window_falls_back_to_the_only_one_of_its_app() {
        let a = win(1, "org.gnome.Nautilus", "Home");
        let l = listed(&[(10, "org.gnome.Nautilus", "Downloads"), (11, "firefox", "Home")]);
        assert_eq!(match_ids(&[&a], &l), vec![Some(10)]);
    }
}

//! Places the bar's menus (Quick Settings, the date menu, app indicator
//! menus) the same way: just below the bar, whatever the size of the
//! button they hang off, with a small gap so the wallpaper shows between
//! the bar and the menu.

use gtk::gdk;
use gtk::graphene;
use gtk::prelude::*;
use gtk4 as gtk;

/// Pixels between the bottom of the bar and the top of a menu.
pub const MENU_GAP: i32 = 6;

pub fn below_bar(popover: &impl IsA<gtk::Popover>) {
    popover.as_ref().connect_show(|p| {
        let Some(anchor) = p.parent() else { return };
        let Some(root) = anchor.root() else { return };
        let bar = root.upcast_ref::<gtk::Widget>();
        let Some(top) = anchor.compute_point(bar, &graphene::Point::new(0.0, 0.0)) else {
            return;
        };
        // Point at a box spanning the whole bar height under the button, so
        // the menu starts at the bar's bottom edge rather than the button's.
        let rect = gdk::Rectangle::new(0, -(top.y() as i32), anchor.width(), bar.height());
        p.set_pointing_to(Some(&rect));
        p.set_position(gtk::PositionType::Bottom);
        p.set_offset(0, MENU_GAP);
    });
}

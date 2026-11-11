//! File results in the overlay, from umoja-files over D-Bus.
//!
//! As the user types, the top matches from the home-folder index appear
//! under the search entry, like GNOME Shell's Files search provider. A row
//! opens the file; the folder button shows it in Files.

use adw::prelude::*;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use umoja_common::dbus::{FILES_BUS_NAME, FILES_INTERFACE, FILES_OBJECT_PATH};
use umoja_common::i18n;

const ROWS: u32 = 5;
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
#[allow(dead_code)] // score is for callers that rank or filter further
pub struct Hit {
    pub path: String,
    pub name: String,
    pub detail: String,
    pub score: f64,
}

/// Ask umoja-files for up to `limit` matches (D-Bus activates it if needed).
pub async fn search(query: &str, limit: u32) -> Result<Vec<Hit>, glib::Error> {
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
    Ok(hits
        .into_iter()
        .map(|(path, name, detail, score)| Hit {
            path,
            name,
            detail,
            score,
        })
        .collect())
}

/// Open a file (or folder) in its default app.
pub fn open(path: &str, ctx: Option<&gio::AppLaunchContext>) -> Result<(), glib::Error> {
    let uri = gio::File::for_path(path).uri();
    gio::AppInfo::launch_default_for_uri(&uri, ctx)
}

/// Select the file in the file manager, like "Open Containing Folder".
pub fn show_in_folder(path: &str) {
    let uri = gio::File::for_path(path).uri();
    let uris = vec![uri.to_string()];
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let r = conn
            .call_future(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                Some(&(uris.clone(), "").to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await;
        if r.is_err() {
            // No FileManager1 service: open the folder itself.
            if let Some(parent) = gio::File::for_uri(&uris[0]).parent() {
                let _ = gio::AppInfo::launch_default_for_uri(
                    &parent.uri(),
                    None::<&gio::AppLaunchContext>,
                );
            }
        }
    });
}

/// The file's icon as Files shows it (from its real content type).
pub fn file_icon(path: &str) -> gio::Icon {
    let file = gio::File::for_path(path);
    file.query_info(
        "standard::icon,standard::content-type",
        gio::FileQueryInfoFlags::NONE,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|i| i.icon())
    .unwrap_or_else(|| {
        let (ctype, _) = gio::content_type_guess(Some(path), &[]);
        gio::content_type_get_icon(&ctype)
    })
}

/// Words that say "I'm looking for a file", so Enter opens the top file
/// instead of asking the assistant.
pub fn looks_like_file_search(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    const STARTERS: &[&str] = &[
        "find ",
        "where is ",
        "where's ",
        "where did i ",
        "open my ",
        "locate ",
        "search for ",
    ];
    const NOUNS: &[&str] = &[
        "file",
        "pdf",
        "document",
        "doc",
        "photo",
        "picture",
        "screenshot",
        "spreadsheet",
        "presentation",
        "folder",
        "video",
        "recording",
        "invoice",
        "receipt",
        "notes",
    ];
    STARTERS.iter().any(|s| q.starts_with(s)) && NOUNS.iter().any(|n| q.contains(n))
}

thread_local! {
    /// The overlay's section, so the assistant's tools can fill it.
    static SECTION: std::cell::RefCell<std::rc::Weak<FilesSection>> = Default::default();
}

/// Show `hits` (from the assistant's search_files call) as the Files list,
/// so the rows match the answer and can be clicked.
pub fn show_tool_hits(hits: &[Hit]) {
    if let Some(s) = SECTION.with(|s| s.borrow().upgrade()) {
        s.generation.set(s.generation.get() + 1);
        s.show_hits(hits.iter().take(ROWS as usize).cloned().collect());
    }
}

/// The "Files" group in the overlay.
pub struct FilesSection {
    pub widget: gtk::Box,
    list: gtk::ListBox,
    generation: Rc<Cell<u64>>,
    first: Rc<std::cell::RefCell<Option<String>>>,
    on_opened: Rc<dyn Fn()>,
}

impl FilesSection {
    /// `on_opened` runs after a file is opened (the overlay hides). This
    /// section is the one the assistant's search_files results go to.
    pub fn new(on_opened: impl Fn() + 'static) -> Rc<Self> {
        let section = Self::new_plain(on_opened);
        SECTION.with(|s| *s.borrow_mut() = Rc::downgrade(&section));
        section
    }

    /// A section for typed searches only (the app grid's).
    pub fn new_plain(on_opened: impl Fn() + 'static) -> Rc<Self> {
        let title = gtk::Label::builder()
            .label(i18n::umoja("Files"))
            .xalign(0.0)
            .css_classes(["heading", "files-heading"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list", "files-list"])
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        widget.set_halign(gtk::Align::Center);
        widget.set_width_request(640);
        widget.append(&title);
        widget.append(&list);
        widget.set_visible(false);
        Rc::new(Self {
            widget,
            list,
            generation: Rc::default(),
            first: Rc::default(),
            on_opened: Rc::new(on_opened),
        })
    }

    pub fn clear(&self) {
        self.generation.set(self.generation.get() + 1);
        self.widget.set_visible(false);
        self.first.replace(None);
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
    }

    /// Search for `query` once typing pauses.
    pub fn update(self: &Rc<Self>, query: &str) {
        let query = query.trim().to_string();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        if query.chars().count() < 3 {
            self.clear();
            return;
        }
        let this = self.clone();
        glib::timeout_add_local_once(DEBOUNCE, move || {
            if this.generation.get() != generation {
                return;
            }
            glib::spawn_future_local(async move {
                let hits = match search(&query, ROWS).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::debug!(error = %e, "file search unavailable");
                        Vec::new()
                    }
                };
                if this.generation.get() != generation {
                    return;
                }
                this.show_hits(hits);
            });
        });
    }

    /// Open the top result. False when there is none.
    pub fn activate_first(&self) -> bool {
        let Some(path) = self.first.borrow().clone() else {
            return false;
        };
        self.open_path(&path);
        true
    }

    fn open_path(&self, path: &str) {
        let ctx = gtk::gdk::Display::default().map(|d| d.app_launch_context());
        if let Err(e) = open(path, ctx.as_ref().map(|c| c.upcast_ref())) {
            tracing::warn!(error = %e, path, "couldn't open file");
            return;
        }
        (self.on_opened)();
    }

    fn show_hits(self: &Rc<Self>, hits: Vec<Hit>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.first.replace(hits.first().map(|h| h.path.clone()));
        self.widget.set_visible(!hits.is_empty());
        for h in hits {
            self.list.append(&self.row(&h));
        }
    }

    fn row(self: &Rc<Self>, h: &Hit) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(&h.name)
            .subtitle(&h.detail)
            .use_markup(false)
            .subtitle_lines(1)
            .activatable(true)
            .tooltip_text(&h.path)
            .build();
        let icon = gtk::Image::from_gicon(&file_icon(&h.path));
        icon.set_pixel_size(32);
        row.add_prefix(&icon);

        let folder = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(i18n::umoja("Show in Files"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let path = h.path.clone();
        let this = Rc::downgrade(self);
        folder.connect_clicked(move |_| {
            show_in_folder(&path);
            if let Some(t) = this.upgrade() {
                (t.on_opened)();
            }
        });
        row.add_suffix(&folder);

        let path = h.path.clone();
        let this = Rc::downgrade(self);
        row.connect_activated(move |_| {
            if let Some(t) = this.upgrade() {
                t.open_path(&path);
            }
        });
        row
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_file_search;

    #[test]
    fn file_search_phrases() {
        assert!(looks_like_file_search(
            "find the PDF about the lease from last spring"
        ));
        assert!(looks_like_file_search("where is my tax spreadsheet"));
        assert!(!looks_like_file_search("find a good text editor"));
        assert!(!looks_like_file_search("how do I set up a VPN?"));
    }
}

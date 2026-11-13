//! The "Voice" page of the assistant settings popover: pick the Piper
//! voice Read Aloud uses, download it (with progress), hear a sample,
//! remove downloaded voices, and set the speed and whether every answer
//! is read out. Offers to install Piper (the piper-tts snap) when it's
//! missing. See [`crate::speech`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use umoja_common::{gsettings as gs, i18n};

use crate::lemonade::RUNTIME;
use crate::speech::{self, VoiceInfo};

/// Rows shown at most; searching narrows the rest down.
const MAX_ROWS: usize = 150;

thread_local! {
    /// The page, for the feature test's hooks.
    static PAGE: RefCell<std::rc::Weak<VoicePage>> = RefCell::new(std::rc::Weak::new());
}

fn page() -> Option<Rc<VoicePage>> {
    PAGE.with(|p| p.borrow().upgrade())
}

/// Feature test: the voices listed now, one key per line.
pub fn test_rows() -> (bool, String) {
    let Some(p) = page() else { return (false, String::new()) };
    let rows: Vec<String> = p.shown().into_iter().map(|v| v.key).collect();
    (!rows.is_empty(), rows.join("\n"))
}

/// Feature test: "current=…;downloading=…;piper=yes|no;downloaded=a,b".
pub fn test_state() -> (bool, String) {
    let Some(p) = page() else { return (false, String::new()) };
    let dir = p.dir.borrow().clone();
    let downloaded: Vec<String> = p
        .voices
        .borrow()
        .iter()
        .filter(|v| dir.as_deref().is_some_and(|d| speech::is_downloaded(d, &v.key)))
        .map(|v| v.key.clone())
        .collect();
    let downloading = p.downloading.borrow().as_ref().map(|(k, b)| format!("{k}@{:.2}", b.fraction()));
    (
        true,
        format!(
            "current={};downloading={};piper={};downloaded={};status={}",
            speech::current_voice(),
            downloading.unwrap_or_default(),
            if dir.is_some() { "yes" } else { "no" },
            downloaded.join(","),
            p.status.text()
        ),
    )
}

/// Feature test: activate a voice's row, as a click does.
pub fn test_pick(key: &str) -> bool {
    let Some(p) = page() else { return false };
    if !p.voices.borrow().iter().any(|v| v.key == key) {
        return false;
    }
    p.pick(key);
    true
}

/// Feature test: the bin button of a downloaded voice.
pub fn test_remove(key: &str) -> bool {
    let Some(p) = page() else { return false };
    let Some(dir) = p.dir.borrow().clone() else { return false };
    if key == speech::current_voice() || !speech::is_downloaded(&dir, key) {
        return false;
    }
    speech::delete_voice(&dir, key);
    p.fill();
    true
}

/// Feature test: type into the search entry.
pub fn test_search(text: &str) -> bool {
    let Some(p) = page() else { return false };
    p.search.set_text(text);
    true
}

pub struct VoicePage {
    pub page: gtk::Box,
    banner: gtk::Box,
    banner_status: gtk::Label,
    install: gtk::Button,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    status: gtk::Label,
    voices: RefCell<Vec<VoiceInfo>>,
    /// Piper's voices folder, once Piper is found.
    dir: RefCell<Option<PathBuf>>,
    /// The voice downloading now, and its progress bar.
    downloading: RefCell<Option<(String, gtk::ProgressBar)>>,
    /// Keeps the setting bindings alive.
    _settings: Option<gio::Settings>,
}

impl VoicePage {
    pub fn new() -> Rc<Self> {
        let banner_label = gtk::Label::builder()
            .label(i18n::umoja(
                "Read Aloud needs Piper, a voice engine that runs on this computer.",
            ))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .build();
        let install = gtk::Button::builder()
            .label(i18n::umoja("Install Piper"))
            .halign(gtk::Align::Start)
            .css_classes(["suggested-action"])
            .build();
        let banner_status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();
        let banner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        banner.add_css_class("voice-banner");
        banner.append(&banner_label);
        banner.append(&install);
        banner.append(&banner_status);
        banner.set_visible(false);

        let note = gtk::Label::builder()
            .label(i18n::umoja(
                "Read Aloud speaks answers in this voice. Voices download when you pick them.",
            ))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Search voices or languages"))
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["model-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(300)
            .min_content_width(380)
            .child(&list)
            .build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();

        // Speed and "read every answer", bound to their keys.
        let settings = gs::settings(speech::SCHEMA);
        let speed = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.5, 2.0, 0.1);
        speed.set_hexpand(true);
        speed.set_draw_value(false);
        speed.add_mark(1.0, gtk::PositionType::Bottom, None);
        let speed_row = setting_row(&i18n::umoja("Speed"), speed.upcast_ref());
        let auto = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let auto_row = setting_row(&i18n::umoja("Read every answer aloud"), auto.upcast_ref());
        auto_row.set_tooltip_text(Some(&i18n::umoja(
            "Not while the screen reader is on: it reads answers already",
        )));
        match &settings {
            Some(s) if gs::has_key(s, "speech-rate") && gs::has_key(s, "speak-answers") => {
                s.bind("speech-rate", &speed.adjustment(), "value").build();
                s.bind("speak-answers", &auto, "active").build();
            }
            _ => {
                speed_row.set_visible(false);
                auto_row.set_visible(false);
            }
        }

        let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&banner);
        page.append(&note);
        page.append(&search);
        page.append(&scroller);
        page.append(&status);
        page.append(&speed_row);
        page.append(&auto_row);

        let this = Rc::new(Self {
            page,
            banner,
            banner_status,
            install,
            search,
            list,
            status,
            voices: RefCell::default(),
            dir: RefCell::default(),
            downloading: RefCell::default(),
            _settings: settings,
        });
        PAGE.with(|p| p.replace(Rc::downgrade(&this)));
        let weak = Rc::downgrade(&this);
        this.search.connect_search_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.fill();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(p) = weak.upgrade() {
                p.pick(&row.widget_name());
            }
        });
        let weak = Rc::downgrade(&this);
        this.install.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.install_piper();
            }
        });
        this
    }

    /// Fill the page: find Piper, then the voice list. Call when the
    /// popover opens.
    pub fn load(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let piper = gio::spawn_blocking(speech::find_piper).await.ok().flatten();
            let Some(p) = weak.upgrade() else { return };
            p.dir.replace(piper.map(|p| p.voices));
            p.update_banner();
            if !p.voices.borrow().is_empty() {
                p.fill();
                return;
            }
            p.set_status(Some(&i18n::umoja("Getting the list of voices…")));
            let (tx, rx) = async_channel::bounded(1);
            RUNTIME.spawn(async move {
                let _ = tx.send(speech::catalog().await).await;
            });
            let result = rx.recv().await.unwrap_or_else(|_| Err("crashed".into()));
            let Some(p) = weak.upgrade() else { return };
            match result {
                Ok(voices) => {
                    p.voices.replace(voices);
                    p.set_status(None);
                }
                Err(e) => p.set_status(Some(&e)),
            }
            p.fill();
        });
    }

    fn update_banner(&self) {
        let missing = self.dir.borrow().is_none();
        self.banner.set_visible(missing);
        self.install.set_visible(speech::can_install_snap());
        if missing && !speech::can_install_snap() {
            self.banner_status.set_visible(true);
            self.banner_status.set_text(&format!(
                "{} sudo snap install {}",
                i18n::umoja("Install it with:"),
                speech::SNAP_NAME
            ));
        }
    }

    fn set_status(&self, text: Option<&str>) {
        self.status.set_visible(text.is_some());
        self.status.set_text(text.unwrap_or_default());
    }

    fn install_piper(self: &Rc<Self>) {
        self.install.set_sensitive(false);
        self.banner_status.set_visible(true);
        self.banner_status
            .set_text(&i18n::umoja("Installing Piper… You may be asked for your password."));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (tx, rx) = async_channel::bounded(1);
            RUNTIME.spawn(async move {
                let _ = tx.send(speech::install_piper().await).await;
            });
            let result = rx.recv().await.unwrap_or_else(|_| Err("crashed".into()));
            let Some(p) = weak.upgrade() else { return };
            p.install.set_sensitive(true);
            match result {
                Ok(()) => {
                    p.banner_status.set_visible(false);
                    p.load();
                }
                Err(e) => p
                    .banner_status
                    .set_text(&format!("{} ({e})", i18n::umoja("Piper wasn't installed"))),
            }
        });
    }

    /// The voices to list: the user's language (plus downloaded and the
    /// current one), or everything matching the search.
    fn shown(&self) -> Vec<VoiceInfo> {
        let query = self.search.text().trim().to_lowercase();
        let current = speech::current_voice();
        let family = user_language();
        let dir = self.dir.borrow().clone();
        let downloaded = |key: &str| dir.as_deref().is_some_and(|d| speech::is_downloaded(d, key));
        let mut out: Vec<VoiceInfo> = self
            .voices
            .borrow()
            .iter()
            .filter(|v| {
                if query.is_empty() {
                    v.family == family || v.key == current || downloaded(&v.key)
                } else {
                    [&v.key, &v.name, &v.language, &v.country]
                        .iter()
                        .any(|f| f.to_lowercase().contains(&query))
                }
            })
            .take(MAX_ROWS)
            .cloned()
            .collect();
        // The voice in use first.
        out.sort_by_key(|v| v.key != current);
        out
    }

    fn fill(self: &Rc<Self>) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let current = speech::current_voice();
        let dir = self.dir.borrow().clone();
        let downloading = self.downloading.borrow().as_ref().map(|(k, _)| k.clone());
        for v in self.shown() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.set_margin_end(6);

            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if v.key == current { 1.0 } else { 0.0 });
            row.append(&check);

            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            let mut title = v.display_name();
            if v.key == speech::DEFAULT_VOICE {
                title = format!("{title} · {}", i18n::umoja("Default"));
            }
            text.append(
                &gtk::Label::builder()
                    .label(&title)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let mut details = vec![if v.country.is_empty() {
                v.language.clone()
            } else {
                format!("{} ({})", v.language, v.country)
            }];
            details.push(quality_label(&v.quality));
            if v.size_bytes > 0 {
                details.push(format!("{} MB", (v.size_bytes + 500_000) / 1_000_000));
            }
            if v.speakers > 1 {
                details.push(i18n::umoja("several speakers"));
            }
            text.append(
                &gtk::Label::builder()
                    .label(details.join(" · "))
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["dim-label", "caption"])
                    .build(),
            );
            if downloading.as_deref() == Some(v.key.as_str()) {
                // Keep showing the same bar while the list redraws.
                if let Some((_, bar)) = self.downloading.borrow().as_ref() {
                    if let Some(parent) = bar.parent() {
                        if let Some(b) = parent.downcast_ref::<gtk::Box>() {
                            b.remove(bar);
                        }
                    }
                    text.append(bar);
                }
            }
            row.append(&text);

            let is_downloaded = dir.as_deref().is_some_and(|d| speech::is_downloaded(d, &v.key));
            if is_downloaded && v.key != current {
                let delete = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text(i18n::umoja("Remove Voice"))
                    .css_classes(["flat", "circular"])
                    .valign(gtk::Align::Center)
                    .build();
                let (weak, key) = (Rc::downgrade(self), v.key.clone());
                delete.connect_clicked(move |_| {
                    if let Some(p) = weak.upgrade() {
                        if let Some(d) = p.dir.borrow().as_deref() {
                            speech::delete_voice(d, &key);
                        }
                        p.fill();
                    }
                });
                row.append(&delete);
            }
            let state = gtk::Image::from_icon_name(if is_downloaded {
                "emblem-ok-symbolic"
            } else {
                "folder-download-symbolic"
            });
            state.set_tooltip_text(Some(&if is_downloaded {
                i18n::umoja("Downloaded")
            } else {
                i18n::umoja("Downloads when selected")
            }));
            state.add_css_class("dim-label");
            row.append(&state);

            let item = gtk::ListBoxRow::builder()
                .child(&row)
                .activatable(true)
                .name(&v.key)
                .build();
            self.list.append(&item);
        }
    }

    /// Download the voice if needed, make it the voice, and play a sample.
    fn pick(self: &Rc<Self>, key: &str) {
        let Some(dir) = self.dir.borrow().clone() else {
            self.banner.set_visible(true);
            self.banner_status.set_visible(true);
            self.banner_status
                .set_text(&i18n::umoja("Install Piper first, then pick a voice."));
            return;
        };
        if self.downloading.borrow().is_some() {
            return;
        }
        if speech::is_downloaded(&dir, key) {
            self.choose(key);
            return;
        }
        let bar = gtk::ProgressBar::builder().margin_top(4).build();
        self.downloading.replace(Some((key.to_owned(), bar.clone())));
        self.fill();
        let (tx, rx) = async_channel::unbounded::<Result<f64, String>>();
        let k = key.to_owned();
        RUNTIME.spawn(async move {
            let progress_tx = tx.clone();
            let result = speech::download_voice(&k, &dir, move |f| {
                let _ = progress_tx.try_send(Ok(f));
            })
            .await;
            let _ = tx.send(result.map(|()| 2.0)).await;
        });
        let weak = Rc::downgrade(self);
        let key = key.to_owned();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                let Some(p) = weak.upgrade() else { return };
                match msg {
                    Ok(f) if f <= 1.0 => bar.set_fraction(f),
                    Ok(_) => {
                        p.downloading.replace(None);
                        p.set_status(None);
                        p.choose(&key);
                        return;
                    }
                    Err(e) if e == speech::CANCELLED => {
                        p.downloading.replace(None);
                        p.set_status(None);
                        p.fill();
                        return;
                    }
                    Err(e) => {
                        p.downloading.replace(None);
                        p.set_status(Some(&format!(
                            "{} ({e})",
                            i18n::umoja("Couldn't download the voice")
                        )));
                        p.fill();
                        return;
                    }
                }
            }
        });
    }

    fn choose(self: &Rc<Self>, key: &str) {
        if let Some(s) = gs::settings(speech::SCHEMA).filter(|s| gs::has_key(s, "speech-voice")) {
            let _ = s.set_string("speech-voice", key);
        }
        self.fill();
        speech::speak_as(&i18n::umoja("Hello! This is how I sound."), key, |_| {});
    }
}

fn setting_row(label: &str, control: &gtk::Widget) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(4);
    let l = gtk::Label::builder().label(label).xalign(0.0).build();
    if !control.hexpands() {
        l.set_hexpand(true);
    }
    row.append(&l);
    row.append(control);
    row
}

fn quality_label(q: &str) -> String {
    match q {
        "x_low" => i18n::umoja("Smallest"),
        "low" => i18n::umoja("Small"),
        "medium" => i18n::umoja("Medium quality"),
        "high" => i18n::umoja("High quality"),
        other => other.to_owned(),
    }
}

/// "en" for en_GB.UTF-8; English for the C locale.
fn user_language() -> String {
    glib::language_names()
        .first()
        .map(|l| l.split(['_', '.', '@']).next().unwrap_or_default().to_owned())
        .filter(|l| !l.is_empty() && l != "C" && l != "POSIX")
        .unwrap_or_else(|| "en".into())
}

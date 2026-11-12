//! umoja-installer: puts Umoja on a disk from the live session.
//!
//! A few questions on a card over the screensaver's "umoja" in dots: which
//! disk, who you are, your password (which also unlocks the disk), and
//! the time zone. Then `umoja-install` (bin/umoja-install) does the work
//! as root and reports progress line by line.
//!
//! `umoja-installer --preview` shows the pages without installing
//! anything (a fake run), for working on the look.

#[path = "../../umoja-screensaver/src/art.rs"]
#[allow(dead_code)]
mod art;
mod system;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;
use system::{Disk, Event};

const APP_ID: &str = "org.umoja.Installer";
const BACKEND: &str = "/usr/libexec/umoja-installer/umoja-install";

#[derive(Clone, Default)]
struct Answers {
    disk: Option<Disk>,
    full_name: String,
    username: String,
    hostname: String,
    password: String,
    timezone: String,
}

struct Ui {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    answers: RefCell<Answers>,
    preview: bool,
    auto: bool,
    // Pages that get filled in as the user goes.
    disk_list: gtk::ListBox,
    disks: RefCell<Vec<Disk>>,
    no_disks: gtk::Label,
    summary: gtk::Label,
    progress_bar: gtk::ProgressBar,
    progress_label: gtk::Label,
    error_label: gtk::Label,
    log: RefCell<Vec<String>>,
    /// An install is under way (or done): never start a second one.
    running: std::cell::Cell<bool>,
    /// When the install started, for "Installed in 1:42" on the last page.
    started: std::cell::Cell<Option<Instant>>,
    done_time: gtk::Label,
}

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-installer");
    let preview = std::env::args().any(|a| a == "--preview");
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("installer.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(move |app| {
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        build(app, preview);
    });
    app.run_with_args::<&str>(&[])
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["page-title"])
        .wrap(true)
        .halign(gtk::Align::Start)
        .build()
}

fn body(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["page-body"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build()
}

fn button(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .css_classes(["pill-button", class])
        .build()
}

fn page() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .build()
}

fn actions(back: Option<&gtk::Button>, next: &gtk::Button) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(12)
        .margin_top(10)
        .halign(gtk::Align::Fill)
        .build();
    let spacer = gtk::Box::builder().hexpand(true).build();
    if let Some(b) = back {
        row.append(b);
    }
    row.append(&spacer);
    row.append(next);
    row
}

fn field(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    b.append(
        &gtk::Label::builder()
            .label(label)
            .css_classes(["field-label"])
            .xalign(0.0)
            .build(),
    );
    b.append(widget);
    b
}

fn build(app: &adw::Application, preview: bool) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Install Umoja")
        .default_width(1100)
        .default_height(760)
        .css_classes(["umoja-installer"])
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .transition_duration(250)
        .vhomogeneous(false)
        .interpolate_size(true)
        .build();

    let ui = Rc::new(Ui {
        window: window.clone(),
        stack: stack.clone(),
        answers: RefCell::new(Answers {
            timezone: system::current_timezone(),
            ..Default::default()
        }),
        preview,
        auto: system::autoinstall() && !preview,
        disk_list: gtk::ListBox::builder()
            .css_classes(["disk-list"])
            .selection_mode(gtk::SelectionMode::Single)
            .build(),
        disks: RefCell::new(vec![]),
        no_disks: body("No disk of at least 24 GB was found."),
        summary: body(""),
        progress_bar: gtk::ProgressBar::builder().css_classes(["install-progress"]).build(),
        progress_label: body("Getting ready"),
        error_label: body(""),
        log: RefCell::new(vec![]),
        running: std::cell::Cell::new(false),
        started: std::cell::Cell::new(None),
        done_time: gtk::Label::builder().css_classes(["install-time"]).build(),
    });

    stack.add_named(&welcome_page(&ui), Some("welcome"));
    stack.add_named(&disk_page(&ui), Some("disk"));
    stack.add_named(&account_page(&ui), Some("account"));
    stack.add_named(&confirm_page(&ui), Some("confirm"));
    stack.add_named(&progress_page(&ui), Some("progress"));
    stack.add_named(&done_page(&ui), Some("done"));
    stack.add_named(&error_page(&ui), Some("error"));

    let card = gtk::Box::builder()
        .css_classes(["installer-card"])
        .halign(gtk::Align::Center)
        // Low on the screen, so the dotted "umoja" shows above it.
        .valign(gtk::Align::End)
        .width_request(520)
        .build();
    card.append(&stack);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&backdrop(&card)));
    overlay.add_overlay(&card);
    window.set_child(Some(&overlay));

    if ui.auto {
        start_auto(&ui);
    }
    window.fullscreen();
    window.present();
}

fn welcome_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Welcome to Umoja"));
    p.append(&body(
        "Umoja is Ubuntu with a new desktop: fast, simple, and with an \
         assistant that runs on this computer.\n\nInstalling takes a few \
         minutes and replaces everything on the disk you choose.",
    ));
    let try_it = button("Try Umoja", "flat");
    let install = button("Install Umoja", "suggested");
    {
        let ui = ui.clone();
        try_it.connect_clicked(move |_| ui.window.close());
    }
    {
        let ui = ui.clone();
        install.connect_clicked(move |_| {
            refresh_disks(&ui);
            ui.stack.set_visible_child_name("disk");
        });
    }
    // "Try Umoja" is off for now: the live session isn't ready to be used.
    try_it.set_visible(false);
    p.append(&actions(Some(&try_it), &install));
    p
}

fn refresh_disks(ui: &Rc<Ui>) {
    let disks = if ui.preview {
        vec![
            Disk {
                path: "/dev/nvme0n1".into(),
                size: 1_000_204_886_016,
                model: "Samsung SSD 990 PRO 1TB".into(),
                transport: "nvme".into(),
                removable: false,
            },
            Disk {
                path: "/dev/sda".into(),
                size: 256_060_514_304,
                model: "SanDisk Ultra".into(),
                transport: "usb".into(),
                removable: true,
            },
        ]
    } else {
        system::disks()
    };
    while let Some(row) = ui.disk_list.row_at_index(0) {
        ui.disk_list.remove(&row);
    }
    for d in &disks {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["disk-row"])
            .build();
        row.append(
            &gtk::Label::builder()
                .label(d.description())
                .css_classes(["disk-name"])
                .xalign(0.0)
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .label(&d.path)
                .css_classes(["disk-path"])
                .xalign(0.0)
                .build(),
        );
        ui.disk_list.append(&row);
    }
    if let Some(first) = ui.disk_list.row_at_index(0) {
        ui.disk_list.select_row(Some(&first));
    }
    ui.no_disks.set_visible(disks.is_empty());
    *ui.disks.borrow_mut() = disks;
}

fn disk_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Where should Umoja go?"));
    p.append(&body(
        "Umoja takes over the whole disk. Everything on it is erased.",
    ));
    let scroller = gtk::ScrolledWindow::builder()
        .child(&ui.disk_list)
        .propagate_natural_height(true)
        .max_content_height(260)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    p.append(&scroller);
    ui.no_disks.add_css_class("warning");
    ui.no_disks.set_visible(false);
    p.append(&ui.no_disks);

    let back = button("Back", "flat");
    let next = button("Next", "suggested");
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| ui.stack.set_visible_child_name("welcome"));
    }
    {
        let ui = ui.clone();
        next.connect_clicked(move |_| {
            let Some(row) = ui.disk_list.selected_row() else {
                return;
            };
            let disk = ui.disks.borrow().get(row.index() as usize).cloned();
            ui.answers.borrow_mut().disk = disk;
            ui.stack.set_visible_child_name("account");
        });
    }
    {
        let next = next.clone();
        ui.disk_list.connect_selected_rows_changed(move |l| {
            next.set_sensitive(l.selected_row().is_some());
        });
    }
    p.append(&actions(Some(&back), &next));
    p
}

fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .placeholder_text(placeholder)
        .css_classes(["field"])
        .build()
}

fn password_entry() -> gtk::PasswordEntry {
    gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .css_classes(["field"])
        .build()
}

fn account_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Who are you?"));

    let name = entry("Your name");
    let user = entry("");
    let host = entry("");
    let pass = password_entry();
    let confirm = password_entry();
    let zones = system::timezones();
    let zone_list = gtk::StringList::new(&zones.iter().map(String::as_str).collect::<Vec<_>>());
    let zone = gtk::DropDown::builder()
        .model(&zone_list)
        .enable_search(true)
        .css_classes(["field"])
        .build();
    // Shown and searched as "New York (America/New_York)": typing any part
    // of it, in any case, finds the zone ("new", "york", "america").
    zone.set_expression(Some(gtk::ClosureExpression::with_callback(
        &[] as &[gtk::Expression],
        |args: &[glib::Value]| {
            args.first()
                .and_then(|v| v.get::<gtk::StringObject>().ok())
                .map(|o| system::timezone_label(&o.string()))
                .unwrap_or_default()
        },
    )));
    zone.set_search_match_mode(gtk::StringFilterMatchMode::Substring);
    let current = ui.answers.borrow().timezone.clone();
    if let Some(i) = zones.iter().position(|z| *z == current) {
        zone.set_selected(i as u32);
    }

    let grid = gtk::Grid::builder().column_spacing(12).row_spacing(10).build();
    grid.attach(&field("Your name", &name), 0, 0, 2, 1);
    grid.attach(&field("User name", &user), 0, 1, 1, 1);
    grid.attach(&field("Computer name", &host), 1, 1, 1, 1);
    grid.attach(&field("Password", &pass), 0, 2, 1, 1);
    grid.attach(&field("Confirm password", &confirm), 1, 2, 1, 1);
    grid.attach(&field("Time zone", &zone), 0, 3, 2, 1);
    for w in [&user, &host] {
        w.set_hexpand(true);
    }
    p.append(&grid);
    p.append(&body(
        "Your password also unlocks the disk when the computer starts, so \
         keep it safe: without it, nobody can read what's on the disk, \
         including you.",
    ));
    let problem = body("");
    problem.add_css_class("warning");
    p.append(&problem);

    // Suggest a user and computer name from the full name until the user
    // types their own.
    let user_edited = Rc::new(std::cell::Cell::new(false));
    let host_edited = Rc::new(std::cell::Cell::new(false));
    {
        let (user, host) = (user.clone(), host.clone());
        let (ue, he) = (user_edited.clone(), host_edited.clone());
        name.connect_changed(move |n| {
            let u = system::username_from(&n.text());
            if !ue.get() {
                user.set_text(&u);
            }
            if !he.get() {
                host.set_text(&if u.is_empty() { String::new() } else { format!("{u}-umoja") });
            }
        });
    }
    {
        let ue = user_edited.clone();
        user.connect_changed(move |u| {
            if u.has_focus() {
                ue.set(true);
            }
        });
        let he = host_edited.clone();
        host.connect_changed(move |h| {
            if h.has_focus() {
                he.set(true);
            }
        });
    }

    let back = button("Back", "flat");
    let next = button("Next", "suggested");
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| ui.stack.set_visible_child_name("disk"));
    }
    {
        let ui = ui.clone();
        let zones = zones.clone();
        next.connect_clicked(move |_| {
            let username = user.text().to_string();
            let hostname = host.text().to_string();
            let password = pass.text().to_string();
            let check = system::check_username(&username)
                .and_then(|_| system::check_hostname(&hostname))
                .and_then(|_| system::check_password(&password, &confirm.text()));
            if let Err(e) = check {
                problem.set_label(e);
                return;
            }
            problem.set_label("");
            let mut a = ui.answers.borrow_mut();
            a.full_name = name.text().trim().to_string();
            a.username = username;
            a.hostname = hostname;
            a.password = password;
            a.timezone = zones
                .get(zone.selected() as usize)
                .cloned()
                .unwrap_or_else(|| "UTC".into());
            drop(a);
            show_summary(&ui);
            ui.stack.set_visible_child_name("confirm");
        });
    }
    p.append(&actions(Some(&back), &next));
    p
}

fn show_summary(ui: &Rc<Ui>) {
    let a = ui.answers.borrow();
    let disk = a.disk.as_ref().map(|d| d.description()).unwrap_or_default();
    let path = a.disk.as_ref().map(|d| d.path.clone()).unwrap_or_default();
    ui.summary.set_markup(&format!(
        "<b>Disk</b>  {}  <small>({})</small>\n<b>Account</b>  {} ({})\n<b>Computer</b>  {}\n<b>Time zone</b>  {}\n\n\
         Everything on this disk will be erased, and the disk encrypted \
         with your password.",
        glib::markup_escape_text(&disk),
        glib::markup_escape_text(&path),
        glib::markup_escape_text(if a.full_name.is_empty() { &a.username } else { &a.full_name }),
        glib::markup_escape_text(&a.username),
        glib::markup_escape_text(&a.hostname),
        glib::markup_escape_text(&a.timezone),
    ));
}

fn confirm_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Ready to install"));
    p.append(&ui.summary);
    let back = button("Back", "flat");
    let install = button("Erase and install", "destructive");
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| ui.stack.set_visible_child_name("account"));
    }
    {
        let ui = ui.clone();
        install.connect_clicked(move |_| run(&ui));
    }
    p.append(&actions(Some(&back), &install));
    p
}

fn progress_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Installing Umoja"));
    p.append(&ui.progress_label);
    p.append(&ui.progress_bar);
    p.append(&body(
        "This takes a few minutes. You can keep using the live session \
         meanwhile.",
    ));
    p
}

fn done_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Umoja is installed"));
    let time = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    time.append(&ui.done_time);
    time.append(
        &gtk::Label::builder()
            .label("from Erase and install to done")
            .css_classes(["install-time-caption"])
            .build(),
    );
    p.append(&time);
    p.append(&body(
        "Restart, remove the installation medium when asked, and type \
         your password to unlock the disk.",
    ));
    let later = button("Keep trying", "flat");
    let restart = button("Restart now", "suggested");
    {
        let ui = ui.clone();
        later.connect_clicked(move |_| ui.window.close());
    }
    restart.connect_clicked(|_| {
        let _ = std::process::Command::new("systemctl").arg("reboot").spawn();
    });
    p.append(&actions(Some(&later), &restart));
    p
}

fn error_page(ui: &Rc<Ui>) -> gtk::Box {
    let p = page();
    p.append(&heading("Umoja couldn't be installed"));
    ui.error_label.add_css_class("warning");
    ui.error_label.set_selectable(true);
    p.append(&ui.error_label);
    let close = button("Close", "flat");
    let again = button("Start over", "suggested");
    {
        let ui = ui.clone();
        close.connect_clicked(move |_| ui.window.close());
    }
    {
        let ui = ui.clone();
        again.connect_clicked(move |_| ui.stack.set_visible_child_name("welcome"));
    }
    p.append(&actions(Some(&close), &again));
    p
}

/// CI: no questions. First disk, account umoja/umoja-install, power off
/// when done so the test can boot the installed disk.
fn start_auto(ui: &Rc<Ui>) {
    // Once per boot, even if the session (and with it this app) restarts.
    let marker = std::path::Path::new(
        &std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
    )
    .join("umoja-autoinstall-started");
    if marker.exists() {
        tracing::warn!("automatic install already ran this boot; not starting another");
        return;
    }
    let _ = std::fs::write(&marker, b"");
    refresh_disks(ui);
    let disk = ui.disks.borrow().first().cloned();
    {
        let mut a = ui.answers.borrow_mut();
        a.disk = disk;
        a.full_name = "Umoja".into();
        a.username = "umoja".into();
        a.hostname = "umoja".into();
        a.password = "umoja-install".into();
        a.timezone = "UTC".into();
    }
    run(ui);
}

fn run(ui: &Rc<Ui>) {
    if ui.running.replace(true) {
        return;
    }
    ui.stack.set_visible_child_name("progress");
    ui.progress_bar.set_fraction(0.0);
    ui.started.set(Some(Instant::now()));
    let a = ui.answers.borrow().clone();
    let Some(disk) = a.disk.clone() else {
        ui.running.set(false);
        fail(ui, "No disk was chosen.");
        return;
    };
    tracing::info!(disk = %disk.path, user = %a.username, tz = %a.timezone, "installing");

    if ui.preview {
        // A fake run for --preview.
        let ui = ui.clone();
        let start = Instant::now();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let p = (start.elapsed().as_secs_f64() / 8.0).min(1.0);
            ui.progress_bar.set_fraction(p);
            ui.progress_label.set_label(if p < 0.1 {
                "Preparing the disk"
            } else if p < 0.8 {
                "Copying Umoja"
            } else {
                "Installing the boot loader"
            });
            if p >= 1.0 {
                show_done(&ui);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        return;
    }

    let mut argv: Vec<String> = vec![];
    let backend = std::env::var("UMOJA_INSTALL_BACKEND").unwrap_or_else(|_| BACKEND.into());
    // The live user has passwordless sudo (casper); pkexec otherwise.
    let as_root = unsafe { libc::getuid() } == 0;
    if !as_root {
        if std::process::Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .is_ok_and(|s| s.success())
        {
            argv.extend(["sudo".into(), "-n".into()]);
        } else {
            argv.push("pkexec".into());
        }
    }
    argv.push(backend);
    argv.extend([
        "--disk".into(),
        disk.path.clone(),
        "--user".into(),
        a.username.clone(),
        "--full-name".into(),
        a.full_name.clone(),
        "--hostname".into(),
        a.hostname.clone(),
        "--timezone".into(),
        a.timezone.clone(),
    ]);
    let argv_os: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let proc = match gio::Subprocess::newv(
        &argv_os,
        gio::SubprocessFlags::STDIN_PIPE
            | gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_MERGE,
    ) {
        Ok(p) => p,
        Err(e) => {
            ui.running.set(false);
            fail(ui, &format!("Couldn't start the installer: {e}"));
            return;
        }
    };
    if let Some(stdin) = proc.stdin_pipe() {
        let line = format!("{}\n", a.password);
        let _ = stdin.write_all(line.as_bytes(), gio::Cancellable::NONE);
        let _ = stdin.close(gio::Cancellable::NONE);
    }
    let Some(stdout) = proc.stdout_pipe() else {
        ui.running.set(false);
        fail(ui, "Couldn't read the installer's progress.");
        return;
    };
    let lines = gio::DataInputStream::new(&stdout);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let mut error = None;
        loop {
            match lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
                Ok(Some(line)) => {
                    let line = line.to_string();
                    tracing::info!(target: "install", "{line}");
                    match system::parse_line(&line) {
                        Event::Progress(p, what) => {
                            ui.progress_bar.set_fraction(p as f64 / 100.0);
                            if !what.is_empty() {
                                ui.progress_label.set_label(&what);
                            }
                        }
                        Event::Error(e) => error = Some(e),
                        Event::Log => {}
                    }
                    let mut log = ui.log.borrow_mut();
                    log.push(line);
                    if log.len() > 200 {
                        log.remove(0);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error.get_or_insert(e.to_string());
                    break;
                }
            }
        }
        let ok = proc.wait_check_future().await.is_ok();
        if ok {
            show_done(&ui);
            if ui.auto {
                let _ = std::process::Command::new("systemctl").arg("poweroff").spawn();
            }
        } else {
            ui.running.set(false);
            let tail: Vec<String> = ui.log.borrow().iter().rev().take(8).rev().cloned().collect();
            let msg = error.unwrap_or_else(|| "The installer stopped with an error.".into());
            fail(&ui, &format!("{msg}\n\n{}", tail.join("\n")));
        }
    });
}

fn fail(ui: &Rc<Ui>, message: &str) {
    tracing::error!("{message}");
    ui.error_label.set_label(message);
    ui.stack.set_visible_child_name("error");
}

/// The screensaver's word in dots, in the space above the card.
fn backdrop(card: &gtk::Box) -> gtk::DrawingArea {
    let card = card.downgrade();
    let word = Rc::new(sample_word().unwrap_or_else(|| art::word_from_mask(1, 1, |_, _| false)));
    let start = Instant::now();
    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    area.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        // Ubuntu aubergine, darker at the bottom.
        let g = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
        g.add_color_stop_rgb(0.0, 0.17, 0.05, 0.14);
        g.add_color_stop_rgb(1.0, 0.05, 0.01, 0.05);
        let _ = cr.set_source(&g);
        let _ = cr.paint();
        // The free space above the card (it sits at the bottom, with a
        // 48px margin), but never less than a third of the screen.
        let card_h = card.upgrade().map(|c| c.height() as f64).unwrap_or(0.0);
        let free = (h - card_h - 2.0 * 48.0).max(h / 3.0);
        // art sizes the word to about a quarter of the height it is given;
        // give it a taller box centred on the free space so the word fills
        // about half of that space.
        let box_h = free * 1.8;
        let elapsed = start.elapsed().as_secs_f64();
        // The screensaver moves the word about each cycle; here it stays
        // centred in the free space.
        let l = art::layout(&word, w, box_h, &art::cycle_at(elapsed, Some(4)));
        cr.save().ok();
        cr.translate(w / 2.0 - l.cx, free / 2.0 - l.cy);
        // The "rings" effect holds the word in place, so it stays calm.
        let frame = art::frame(&word, w, box_h, elapsed, Some(4));
        for m in &frame.marks {
            if m.rgba.3 <= 0.005 || m.radius <= 0.1 {
                continue;
            }
            cr.set_source_rgba(m.rgba.0, m.rgba.1, m.rgba.2, m.rgba.3 * 0.8);
            cr.arc(m.x, m.y, m.radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        cr.restore().ok();
    });
    // 30 fps is plenty for slowly drifting dots.
    let last = std::cell::Cell::new(Instant::now());
    area.add_tick_callback(move |area, _| {
        if last.get().elapsed() >= std::time::Duration::from_millis(33) {
            last.set(Instant::now());
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
    area
}

/// The word "umoja" as cells, from Ubuntu Sans Bold (as the screensaver).
fn sample_word() -> Option<art::Word> {
    use gtk::cairo;
    const SIZE: f64 = 140.0;
    const CELL: f64 = 6.0;
    let mut surface = cairo::ImageSurface::create(cairo::Format::A8, 1400, 320).ok()?;
    let (w, h) = {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.select_font_face("Ubuntu Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(SIZE);
        let ext = cr.text_extents("umoja").ok()?;
        cr.move_to(CELL - ext.x_bearing(), CELL - ext.y_bearing());
        cr.show_text("umoja").ok()?;
        (ext.width() + 2.0 * CELL, ext.height() + 2.0 * CELL)
    };
    surface.flush();
    let stride = surface.stride() as usize;
    let (sw, sh) = (surface.width() as usize, surface.height() as usize);
    let data = surface.data().ok()?;
    let cols = (w / CELL).ceil() as usize;
    let rows = (h / CELL).ceil() as usize;
    Some(art::word_from_mask(cols, rows, |c, r| {
        let x = (c as f64 * CELL + CELL / 2.0) as usize;
        let y = (r as f64 * CELL + CELL / 2.0) as usize;
        x < sw && y < sh && data[y * stride + x] > 110
    }))
}

/// "1:42", "48s": how long the install took, Omarchy style.
fn duration_text(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn show_done(ui: &Rc<Ui>) {
    let secs = ui.started.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    tracing::info!(seconds = secs, "installed");
    ui.done_time.set_label(&format!("Installed in {}", duration_text(secs)));
    ui.stack.set_visible_child_name("done");
}

//! Removable drives, as gnome-shell's automountManager and
//! autorunManager handle them: a drive plugged in is mounted (asking for
//! its passphrase if it is encrypted), and a notification offers to open
//! it in Files.
//!
//! - `org.gnome.desktop.media-handling automount` turns mounting off,
//!   `autorun-never` the notification, and the `autorun-x-content-*`
//!   lists pick, per content type, between starting the default app,
//!   opening Files, doing nothing, or asking (the notification).
//! - Only while our session is the active one and the screen is unlocked;
//!   a drive plugged in while locked is mounted once it is unlocked.
//! - Apps can stop it with an automount inhibitor
//!   (`org.gnome.SessionManager.Inhibit`, flag 16), as Disks does while
//!   formatting.
//! - The drive's eject button ejects it; plugging and unplugging play the
//!   sound theme's device-added/removed-media sounds.

use crate::mount_op::local::Operation;
use gio::prelude::*;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use umoja_common::{gsettings as gs, i18n};

const MEDIA: &str = "org.gnome.desktop.media-handling";
/// How long after mounting a volume its mount may still autorun.
const AUTORUN_EXPIRE_SECS: u32 = 10;
/// GsmInhibitorFlag automount.
const INHIBIT_AUTOMOUNT: u32 = 16;
const SOUND_ID: u32 = 0x4d4e54; // "MNT"

#[derive(Default)]
struct Manager {
    /// Mount operations in flight, by volume.
    ops: RefCell<Vec<(gio::Volume, Operation)>>,
    /// Volumes that may autorun when their mount appears (just
    /// automounted), and when that permission runs out.
    autorun: RefCell<Vec<(gio::Volume, u32)>>,
    autorun_serial: std::cell::Cell<u32>,
    /// Volumes waiting for the screen to be unlocked.
    waiting: RefCell<Vec<gio::Volume>>,
    /// Open "Open with Files" notifications, by mount root URI.
    notes: RefCell<HashMap<String, u32>>,
}

thread_local! {
    static MANAGER: Rc<Manager> = Rc::new(Manager::default());
    static MONITOR: RefCell<Option<gio::VolumeMonitor>> = const { RefCell::new(None) };
}

pub fn start() {
    let monitor = gio::VolumeMonitor::get();
    let m = MANAGER.with(Rc::clone);

    monitor.connect_volume_added({
        let m = Rc::downgrade(&m);
        move |_, volume| {
            if let Some(m) = m.upgrade() {
                m.check_and_mount(volume, true, true, true);
            }
        }
    });
    monitor.connect_volume_removed({
        let m = Rc::downgrade(&m);
        move |_, volume| {
            if let Some(m) = m.upgrade() {
                m.waiting.borrow_mut().retain(|v| v != volume);
                m.autorun.borrow_mut().retain(|(v, _)| v != volume);
                let op = {
                    let mut ops = m.ops.borrow_mut();
                    ops.iter()
                        .position(|(v, _)| v == volume)
                        .map(|i| ops.remove(i).1)
                };
                if let Some(op) = op {
                    op.close();
                }
            }
        }
    });
    monitor.connect_drive_connected(|_, _| {
        if session_active() {
            play(
                "device-added-media",
                &i18n::shell("External drive connected"),
            );
        }
    });
    monitor.connect_drive_disconnected(|_, _| {
        if session_active() {
            play(
                "device-removed-media",
                &i18n::shell("External drive disconnected"),
            );
        }
    });
    monitor.connect_drive_eject_button(|_, drive| {
        if session_active() {
            eject_drive(drive);
        }
    });
    monitor.connect_mount_added({
        let m = Rc::downgrade(&m);
        move |_, mount| {
            if let Some(m) = m.upgrade() {
                m.mount_added(mount);
            }
        }
    });
    monitor.connect_mount_removed({
        let m = Rc::downgrade(&m);
        move |_, mount| {
            if let Some(m) = m.upgrade() {
                let uri = mount.root().uri().to_string();
                if let Some(id) = m.notes.borrow_mut().remove(&uri)
                    && let Some(c) = crate::notification_center()
                {
                    c.close(id, crate::notifications::CloseReason::Closed);
                }
            }
        }
    });

    // What was plugged in before login: mount it, quietly, as gnome-shell's
    // _startupMountAll does.
    {
        let m = Rc::downgrade(&m);
        let monitor = monitor.clone();
        glib::idle_add_local_once(move || {
            if let Some(m) = m.upgrade() {
                for v in monitor.volumes() {
                    m.check_and_mount(&v, false, false, false);
                }
            }
        });
    }
    MONITOR.with(|s| *s.borrow_mut() = Some(monitor));
}

impl Manager {
    fn check_and_mount(
        self: &Rc<Self>,
        volume: &gio::Volume,
        check_session: bool,
        use_mount_op: bool,
        allow_autorun: bool,
    ) {
        if check_session && !session_active() {
            return;
        }
        if crate::lock::locker().is_locked() {
            // The passphrase card can't show over the lock screen.
            self.wait_for_unlock(volume);
            return;
        }
        if inhibited() {
            return;
        }
        if volume.get_mount().is_some() {
            return;
        }
        if !gs::get_bool(MEDIA, "automount", true)
            || !volume.should_automount()
            || !volume.can_mount()
        {
            // A mount may still turn up later (blank optical media); let it
            // autorun if it does soon.
            self.allow_autorun(volume);
            self.expire_autorun(volume);
            return;
        }
        if allow_autorun {
            self.allow_autorun(volume);
        }
        let op = use_mount_op.then(Operation::new);
        self.mount(volume, op);
    }

    fn mount(self: &Rc<Self>, volume: &gio::Volume, op: Option<Operation>) {
        tracing::info!(volume = %volume.name(), "automounting");
        if let Some(op) = &op {
            self.ops.borrow_mut().retain(|(v, _)| v != volume);
            self.ops.borrow_mut().push((volume.clone(), op.clone()));
        }
        let weak = Rc::downgrade(self);
        let v = volume.clone();
        let mount_op = op.as_ref().map(|o| o.mount_op().clone());
        volume.mount(
            gio::MountMountFlags::NONE,
            mount_op.as_ref(),
            gio::Cancellable::NONE,
            move |res| {
                let Some(m) = weak.upgrade() else { return };
                m.mounted(&v, op, res);
            },
        );
    }

    fn mounted(
        self: &Rc<Self>,
        volume: &gio::Volume,
        op: Option<Operation>,
        res: Result<(), glib::Error>,
    ) {
        self.expire_autorun(volume);
        let forget = |m: &Rc<Self>| m.ops.borrow_mut().retain(|(v, _)| v != volume);
        let Err(e) = res else {
            if let Some(op) = &op {
                op.close();
            }
            forget(self);
            return;
        };
        let msg = e.message();
        // gvfs's udisks backend always says G_IO_ERROR_FAILED; the text
        // tells a wrong passphrase apart (the same checks as gnome-shell).
        let wrong_passphrase = msg.contains("No key available with this passphrase")
            || msg.contains("No key available to unlock device")
            || msg.contains("Failed to activate device: Incorrect passphrase")
            || msg.contains("Failed to load device's parameters: Invalid argument");
        if let Some(op) = op.filter(|o| wrong_passphrase && o.answered_password()) {
            // Ask again in the same card; it says the passphrase was wrong.
            self.mount(volume, Some(op));
            return;
        }
        if let Some(op) = self
            .ops
            .borrow()
            .iter()
            .find(|(v, _)| v == volume)
            .map(|(_, o)| o.clone())
        {
            op.close();
        }
        forget(self);
        if msg.contains("does not support the VeraCrypt PIM setting") {
            notify_error(
                &i18n::shell("Unable to unlock volume"),
                &i18n::shell("The installed udisks version does not support the PIM setting"),
            );
        }
        if !e.matches(gio::IOErrorEnum::FailedHandled) {
            tracing::warn!(volume = %volume.name(), error = %e, "could not mount");
        }
    }

    fn wait_for_unlock(self: &Rc<Self>, volume: &gio::Volume) {
        let first = {
            let mut w = self.waiting.borrow_mut();
            if !w.contains(volume) {
                w.push(volume.clone());
            }
            w.len() == 1
        };
        if !first {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(1, move || {
            let Some(m) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if crate::lock::locker().is_locked() {
                return glib::ControlFlow::Continue;
            }
            let volumes: Vec<_> = m.waiting.borrow_mut().drain(..).collect();
            for v in volumes {
                m.check_and_mount(&v, true, true, true);
            }
            glib::ControlFlow::Break
        });
    }

    fn allow_autorun(&self, volume: &gio::Volume) {
        let serial = self.autorun_serial.get().wrapping_add(1);
        self.autorun_serial.set(serial);
        let mut a = self.autorun.borrow_mut();
        a.retain(|(v, _)| v != volume);
        a.push((volume.clone(), serial));
    }

    fn expire_autorun(self: &Rc<Self>, volume: &gio::Volume) {
        let Some(serial) = self
            .autorun
            .borrow()
            .iter()
            .find(|(v, _)| v == volume)
            .map(|(_, s)| *s)
        else {
            return;
        };
        let weak = Rc::downgrade(self);
        let volume = volume.clone();
        glib::timeout_add_seconds_local_once(AUTORUN_EXPIRE_SECS, move || {
            if let Some(m) = weak.upgrade() {
                m.autorun
                    .borrow_mut()
                    .retain(|(v, s)| !(v == &volume && *s == serial));
            }
        });
    }

    fn may_autorun(&self, mount: &gio::Mount) -> bool {
        let Some(volume) = mount.volume() else {
            return false;
        };
        if !self.autorun.borrow().iter().any(|(v, _)| v == &volume) {
            return false;
        }
        let root = mount.root();
        // Skip mounts inside hidden directories.
        if root.is_native()
            && root
                .path()
                .is_some_and(|p| p.to_string_lossy().contains("/."))
        {
            return false;
        }
        volume.identifier("class").as_deref() != Some("network")
    }

    /// autorunManager: when a just-automounted volume's mount appears,
    /// act on its content type's autorun setting.
    fn mount_added(self: &Rc<Self>, mount: &gio::Mount) {
        if !session_active() || !self.may_autorun(mount) {
            return;
        }
        let weak = Rc::downgrade(self);
        let mount_c = mount.clone();
        mount.guess_content_type(false, gio::Cancellable::NONE, move |res| {
            let Some(m) = weak.upgrade() else { return };
            let types: Vec<String> = res
                .map(|t| t.iter().map(|s| s.to_string()).collect::<Vec<String>>())
                .unwrap_or_default()
                .into_iter()
                .filter(|t| t != "x-content/win32-software")
                .collect();
            m.dispatch(&mount_c, &types);
        });
    }

    fn dispatch(self: &Rc<Self>, mount: &gio::Mount, types: &[String]) {
        if gs::get_bool(MEDIA, "autorun-never", false) {
            return;
        }
        let setting = types
            .first()
            .map(|t| autorun_setting(t))
            .unwrap_or(Autorun::Ask);
        let app = match setting {
            Autorun::Ignore => return,
            Autorun::Run => gio::AppInfo::default_for_type(&types[0], false),
            Autorun::Files => gio::AppInfo::default_for_type("inode/directory", false),
            Autorun::Ask => None,
        };
        if let Some(app) = app {
            open_mount(&app, mount);
            return;
        }
        self.notify(mount, types);
    }

    /// gnome-shell's AutorunNotification: "Open with <app>" for each
    /// content type's default app (Files when there are none); clicking
    /// it opens Files.
    fn notify(self: &Rc<Self>, mount: &gio::Mount, types: &[String]) {
        let Some(center) = crate::notification_center() else {
            return;
        };
        let mut apps: Vec<gio::AppInfo> = Vec::new();
        for t in types {
            if let Some(a) = gio::AppInfo::default_for_type(t, false)
                && !apps.iter().any(|b| b.id() == a.id())
            {
                apps.push(a);
            }
        }
        if apps.is_empty()
            && let Some(files) = gio::AppInfo::default_for_type("inode/directory", false)
        {
            apps.push(files);
        }
        let labels: Vec<(String, String)> = apps
            .iter()
            .enumerate()
            .map(|(i, a)| {
                (
                    format!("app-{i}"),
                    i18n::shell("Open with %s").replace("%s", &a.name()),
                )
            })
            .collect();
        let mut actions: Vec<(&str, &str)> = vec![("default", "")];
        actions.extend(labels.iter().map(|(k, l)| (k.as_str(), l.as_str())));
        let eject = i18n::shell("Eject");
        let can_eject = mount.can_eject() || mount.can_unmount();
        if can_eject {
            actions.push(("eject", eject.as_str()));
        }

        let uri = mount.root().uri().to_string();
        let mount_c = mount.clone();
        let icon = mount
            .symbolic_icon()
            .to_string()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "media-removable-symbolic".into());
        let weak = Rc::downgrade(self);
        let uri_c = uri.clone();
        let id = center.notify_with_actions(&mount.name(), "", &icon, &actions, move |action| {
            if let Some(m) = weak.upgrade() {
                m.notes.borrow_mut().remove(&uri_c);
            }
            if action == "eject" {
                eject_mount(&mount_c);
                return;
            }
            let app = action
                .strip_prefix("app-")
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| apps.get(i).cloned())
                .or_else(|| gio::AppInfo::default_for_type("inode/directory", false));
            if let Some(app) = app {
                open_mount(&app, &mount_c);
            }
        });
        if let Some(old) = self.notes.borrow_mut().insert(uri, id) {
            center.close(old, crate::notifications::CloseReason::Closed);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Autorun {
    Run,
    Ignore,
    Files,
    Ask,
}

fn autorun_setting(content_type: &str) -> Autorun {
    let has = |key: &str| gs::get_strv(MEDIA, key).iter().any(|t| t == content_type);
    if has("autorun-x-content-start-app") {
        Autorun::Run
    } else if has("autorun-x-content-ignore") {
        Autorun::Ignore
    } else if has("autorun-x-content-open-folder") {
        Autorun::Files
    } else {
        Autorun::Ask
    }
}

fn open_mount(app: &gio::AppInfo, mount: &gio::Mount) {
    crate::launch::launch_app_info(app, &[mount.root().uri().to_string()]);
}

/// Eject (or unmount) a mount from its notification, with our dialogs for
/// "the drive is busy" and the "don't unplug" notifications.
fn eject_mount(mount: &gio::Mount) {
    let op = Operation::new();
    let name = mount.name().to_string();
    let done = {
        let op = op.clone();
        move |res: Result<(), glib::Error>| {
            op.close();
            if let Err(e) = res
                && !e.matches(gio::IOErrorEnum::FailedHandled)
            {
                notify_error(
                    &i18n::shell("Ejecting drive “%s” failed:").replace("%s", &name),
                    e.message(),
                );
            }
        }
    };
    if mount.can_eject() {
        mount.eject_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(op.mount_op()),
            gio::Cancellable::NONE,
            done,
        );
    } else {
        mount.unmount_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(op.mount_op()),
            gio::Cancellable::NONE,
            done,
        );
    }
}

/// The drive's own eject button: stop or eject it, as gnome-shell does
/// (forced, without asking).
fn eject_drive(drive: &gio::Drive) {
    let log = |res: Result<(), glib::Error>| {
        if let Err(e) = res {
            tracing::warn!(error = %e, "could not eject the drive after its eject button");
        }
    };
    if drive.can_stop() {
        drive.stop(
            gio::MountUnmountFlags::FORCE,
            None::<&gio::MountOperation>,
            gio::Cancellable::NONE,
            log,
        );
    } else if drive.can_eject() {
        drive.eject_with_operation(
            gio::MountUnmountFlags::FORCE,
            None::<&gio::MountOperation>,
            gio::Cancellable::NONE,
            log,
        );
    }
}

fn notify_error(title: &str, body: &str) {
    if let Some(c) = crate::notification_center() {
        c.notify_internal(title, body, "dialog-error-symbolic");
    }
}

fn play(event: &str, description: &str) {
    if !gs::get_bool("org.gnome.desktop.sound", "event-sounds", true) {
        return;
    }
    let theme = gs::get_string("org.gnome.desktop.sound", "theme-name", "freedesktop");
    crate::event_sound::play(SOUND_ID, event, description, &theme);
}

/// Whether our logind session is the one in front (not switched away
/// from), like gnome-session's SessionIsActive.
fn session_active() -> bool {
    thread_local! {
        static SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
    }
    let Some(conn) = crate::bus::system_bus() else {
        return true;
    };
    let path = SESSION.with(|s| {
        if s.borrow().is_none() {
            *s.borrow_mut() = crate::login::session_path(&conn);
        }
        s.borrow().clone()
    });
    let Some(path) = path else { return true };
    crate::bus::get_all(&conn, crate::login::LOGIN1, &path, crate::login::SESSION)
        .and_then(|p| p.bool("Active"))
        .unwrap_or(true)
}

/// An app holds an automount inhibitor.
fn inhibited() -> bool {
    let Some(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok() else {
        return false;
    };
    conn.call_sync(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        "org.gnome.SessionManager",
        "IsInhibited",
        Some(&(INHIBIT_AUTOMOUNT,).to_variant()),
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NONE,
        500,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|r| r.child_value(0).get::<bool>())
    .unwrap_or(false)
}

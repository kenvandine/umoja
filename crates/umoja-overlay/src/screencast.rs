//! Screen recording from the screenshot UI, as gnome-shell's screencast.
//!
//! Recordings land in `~/Videos/Screencasts` as
//! "Screencast From YYYY-MM-DD HH-MM-SS.webm" (VP8, no audio, like GNOME).
//! Frames come from umoja-mir's `zwlr_screencopy_manager_v1` through
//! `wf-recorder`, which is stopped with SIGINT so it finishes the file.
//! While a recording runs, umoja-panel shows GNOME's red stop indicator with
//! the elapsed time (it follows `ScreencastChanged` on org.umoja.Overlay).
//! When it ends a "Screencast recorded" notification opens the video.

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::MonitorExt;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use umoja_common::i18n;

/// `~/Videos/Screencasts/Screencast From 2026-11-09 01-50-16.webm`
pub fn screencast_path() -> PathBuf {
    let dir = glib::user_special_dir(glib::UserDirectory::Videos)
        .unwrap_or_else(|| glib::home_dir().join("Videos"))
        // gnome-shell uses the translated folder name; "Screencasts" is the C locale.
        .join(i18n::shell("Screencasts"));
    let stamp = glib::DateTime::now_local()
        .and_then(|d| d.format("%Y-%m-%d %H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    dir.join(format!(
        "{}.webm",
        i18n::shell("Screencast From %s").replace("%s", &stamp)
    ))
}

/// What to record: a whole monitor, by its output name, or a region of the
/// layout (logical pixels).
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Output(String),
    Region(i32, i32, i32, i32),
}

/// A whole monitor is recorded by output name (`-o`), at its full pixel
/// size, rather than as a region: Mir before umoja13 read a screencopy
/// region as layout coordinates instead of output-local ones, so a region
/// on any monitor but the leftmost came out cropped.
pub fn target_for(region: (i32, i32, i32, i32)) -> Target {
    let (x, y, w, h) = region;
    umoja_common::monitors::all()
        .into_iter()
        .find(|m| {
            let g = m.geometry();
            (g.x(), g.y(), g.width(), g.height()) == (x, y, w, h)
        })
        .and_then(|m| m.connector())
        .map(|c| Target::Output(c.to_string()))
        .unwrap_or(Target::Region(x, y, w, h))
}

/// The wf-recorder command line for `target`.
/// 4:2:0 video needs even sizes, so a region is trimmed by at most a pixel.
pub fn recorder_args(path: &Path, target: &Target) -> Vec<String> {
    let what = match target {
        Target::Output(name) => ["-o".to_string(), name.clone()],
        Target::Region(x, y, w, h) => {
            let (w, h) = ((w & !1).max(2), (h & !1).max(2));
            ["-g".to_string(), format!("{x},{y} {w}x{h}")]
        }
    };
    ["wf-recorder", "-y"]
        .iter()
        .map(|s| s.to_string())
        .chain(what)
        .chain(
            [
                "-c",
                "libvpx",
                "-x",
                "yuv420p",
                // Real-time VP8 so a laptop keeps up at HiDPI sizes.
                "-p",
                "deadline=realtime",
                "-p",
                "cpu-used=8",
                "-p",
                "b=8M",
                "-f",
            ]
            .iter()
            .map(|s| s.to_string()),
        )
        .chain(std::iter::once(path.to_string_lossy().into_owned()))
        .collect()
}

struct Running {
    proc: gio::Subprocess,
    /// Stop was asked for; wf-recorder is still finishing the file.
    stopping: bool,
}

pub struct Recorder {
    app: adw::Application,
    running: RefCell<Option<Running>>,
    listeners: RefCell<Vec<Box<dyn Fn(bool)>>>,
}

impl Recorder {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let rec = Rc::new(Self {
            app: app.clone(),
            running: RefCell::default(),
            listeners: RefCell::default(),
        });
        // The notification's click opens the video, as on GNOME.
        let open = gio::SimpleAction::new("open-screencast", Some(glib::VariantTy::STRING));
        open.connect_activate(|_, target| {
            let Some(path) = target.and_then(|t| t.str().map(String::from)) else {
                return;
            };
            let uri = gio::File::for_path(&path).uri();
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
            {
                tracing::warn!(error = %e, "could not open screencast");
            }
        });
        app.add_action(&open);
        rec
    }

    pub fn is_recording(&self) -> bool {
        self.running.borrow().is_some()
    }

    /// Called with `true` when a recording starts and `false` when it ends.
    pub fn connect_changed(&self, f: impl Fn(bool) + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn emit(&self, active: bool) {
        for f in self.listeners.borrow().iter() {
            f(active);
        }
    }

    /// Start recording `region` (logical layout pixels).
    pub fn start(self: &Rc<Self>, region: (i32, i32, i32, i32)) {
        if self.is_recording() {
            return;
        }
        let path = screencast_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let target = target_for(region);
        let args = recorder_args(&path, &target);
        let argv: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let proc = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDIN_PIPE) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "could not start wf-recorder");
                self.notify_failed(&i18n::umoja(
                    "Screen recording needs the wf-recorder package.",
                ));
                return;
            }
        };
        tracing::info!(path = %path.display(), ?target, "screencast started");
        *self.running.borrow_mut() = Some(Running {
            proc: proc.clone(),
            stopping: false,
        });
        self.emit(true);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let _ = proc.wait_future().await;
            let Some(rec) = weak.upgrade() else { return };
            let stopping = rec.running.borrow_mut().take().is_some_and(|r| r.stopping);
            if !stopping {
                rec.emit(false);
            }
            rec.finished(&path, proc.is_successful());
        });
    }

    /// Stop the recording; wf-recorder writes the file out on SIGINT.
    ///
    /// Like gnome-shell, the indicator goes at once: wf-recorder can take
    /// seconds to exit (it drains its encode queue, and waits for one more
    /// damaged frame from the compositor), and the click looked ignored
    /// until it did. The "Screencast recorded" notification still waits for
    /// the finished file.
    pub fn stop(&self) {
        let proc = {
            let mut running = self.running.borrow_mut();
            let Some(r) = running.as_mut().filter(|r| !r.stopping) else {
                return;
            };
            r.stopping = true;
            r.proc.clone()
        };
        self.emit(false);
        proc.send_signal(libc::SIGINT);
    }

    fn finished(&self, path: &Path, ok: bool) {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let _ = std::fs::remove_file(path);
            tracing::warn!(ok, "screencast produced no video");
            self.notify_failed(&i18n::umoja("The screen could not be recorded."));
            return;
        }
        tracing::info!(path = %path.display(), size, "screencast saved");
        let n = gio::Notification::new(&i18n::shell("Screencast recorded"));
        n.set_body(Some(&i18n::shell("Click here to view the video.")));
        n.set_icon(&gio::ThemedIcon::new("camera-video-symbolic"));
        n.set_default_action_and_target_value(
            "app.open-screencast",
            Some(&path.to_string_lossy().to_variant()),
        );
        self.app.send_notification(Some("screencast"), &n);
    }

    fn notify_failed(&self, body: &str) {
        let n = gio::Notification::new(&i18n::shell("Screencast"));
        n.set_body(Some(body));
        n.set_icon(&gio::ThemedIcon::new("camera-video-symbolic"));
        self.app.send_notification(Some("screencast"), &n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_use_even_region() {
        let a = recorder_args(Path::new("/tmp/x.webm"), &Target::Region(10, 20, 301, 199));
        let g = a.iter().position(|s| s == "-g").unwrap();
        assert_eq!(a[g + 1], "10,20 300x198");
        assert!(!a.contains(&"-o".to_string()));
        assert_eq!(a.last().unwrap(), "/tmp/x.webm");
        assert_eq!(a[0], "wf-recorder");
    }

    #[test]
    fn args_name_a_whole_output() {
        let a = recorder_args(Path::new("/tmp/x.webm"), &Target::Output("DP-2".into()));
        let o = a.iter().position(|s| s == "-o").unwrap();
        assert_eq!(a[o + 1], "DP-2");
        assert!(!a.contains(&"-g".to_string()));
        assert_eq!(a.last().unwrap(), "/tmp/x.webm");
    }
}

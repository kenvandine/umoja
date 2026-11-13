//! Background downloads the overlay runs (the assistant's Lemonade model,
//! Piper voices, the captions model), published on org.umoja.Overlay as
//! `GetDownloads` / `DownloadsChanged` so umoja-panel can show its
//! download indicator: a ring that fills with progress and a card listing
//! each download.
//!
//! Downloaders run on the Tokio runtime and report through a [`Handle`],
//! which is `Send`; the D-Bus side lives on the GTK main loop and sends at
//! most a few updates a second (Lemonade reports ~10k events a second).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use gtk::glib;
use gtk4 as gtk;

/// How long a finished or failed download stays listed, so the panel can
/// show its check (or the failure) before the indicator fades out.
const LINGER: Duration = Duration::from_secs(4);
/// At most this many `DownloadsChanged` a second.
const THROTTLE: Duration = Duration::from_millis(250);

/// The D-Bus type of one download: (id, kind, title, state, bytes done,
/// bytes total (0 = not known yet), cancellable).
pub const ITEM_TYPE: &str = "(ssssttb)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Downloading,
    /// Waiting for an unmetered connection; `StartDownload` starts it anyway.
    Waiting,
    Done,
    Failed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Downloading => "downloading",
            State::Waiting => "waiting",
            State::Done => "done",
            State::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A Lemonade model.
    Model,
    /// A Piper voice.
    Voice,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Model => "model",
            Kind::Voice => "voice",
        }
    }
}

struct Item {
    kind: Kind,
    title: String,
    state: State,
    done: u64,
    total: u64,
    /// None: the download can't be stopped (a non-streaming Lemonade pull).
    cancel: Option<Arc<AtomicBool>>,
    /// Order of appearance, so the list doesn't shuffle.
    seq: u64,
    ended: Option<Instant>,
}

#[derive(Default)]
struct Registry {
    items: HashMap<String, Item>,
    seq: u64,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);
/// Wakes the main-loop publisher; filled in by [`serve`].
static WAKE: LazyLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> =
    LazyLock::new(async_channel::unbounded);

thread_local! {
    /// "Download now" for downloads waiting on a metered connection; GTK
    /// main thread only.
    static STARTERS: std::cell::RefCell<HashMap<String, Box<dyn Fn()>>> =
        std::cell::RefCell::default();
}

fn changed() {
    let _ = WAKE.0.try_send(());
}

fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut r = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut r)
}

/// One running download. Dropping it without [`Handle::finish`] (the task
/// was aborted) takes it off the list.
pub struct Handle {
    id: String,
    cancel: Arc<AtomicBool>,
    ended: bool,
}

/// Start listing a download. `total` is 0 while the size isn't known.
/// `cancellable` says whether the downloader checks [`Handle::cancelled`].
pub fn start(id: &str, kind: Kind, title: &str, total: u64, cancellable: bool) -> Handle {
    let cancel = Arc::new(AtomicBool::new(false));
    with(|r| {
        r.seq += 1;
        let seq = r.seq;
        r.items.insert(
            id.to_owned(),
            Item {
                kind,
                title: title.to_owned(),
                state: State::Downloading,
                done: 0,
                total,
                cancel: cancellable.then(|| cancel.clone()),
                seq,
                ended: None,
            },
        );
    });
    changed();
    Handle {
        id: id.to_owned(),
        cancel,
        ended: false,
    }
}

impl Handle {
    pub fn progress(&self, done: u64, total: u64) {
        let moved = with(|r| {
            let Some(it) = r.items.get_mut(&self.id) else {
                return false;
            };
            let moved = it.done != done || it.total != total;
            it.done = done;
            it.total = total;
            moved
        });
        if moved {
            changed();
        }
    }

    /// The user pressed the download's cancel button.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Done (`ok`) or failed; either way it stays listed for a few seconds.
    /// A cancelled download just goes away.
    pub fn finish(mut self, ok: bool) {
        self.ended = true;
        if self.cancelled() {
            remove(&self.id);
            return;
        }
        with(|r| {
            if let Some(it) = r.items.get_mut(&self.id) {
                it.state = if ok { State::Done } else { State::Failed };
                if ok && it.total > 0 {
                    it.done = it.total;
                }
                it.ended = Some(Instant::now());
            }
        });
        changed();
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.ended {
            remove(&self.id);
        }
    }
}

pub fn remove(id: &str) {
    if with(|r| r.items.remove(id).is_some()) {
        changed();
    }
}

/// List a download that's waiting for an unmetered connection; `start_now`
/// runs when the user presses "Download now" in the panel. GTK main thread.
pub fn waiting(id: &str, kind: Kind, title: &str, start_now: impl Fn() + 'static) {
    STARTERS.with(|s| s.borrow_mut().insert(id.to_owned(), Box::new(start_now)));
    with(|r| {
        r.seq += 1;
        let seq = r.seq;
        r.items.insert(
            id.to_owned(),
            Item {
                kind,
                title: title.to_owned(),
                state: State::Waiting,
                done: 0,
                total: 0,
                cancel: None,
                seq,
                ended: None,
            },
        );
    });
    changed();
}

/// Stop listing a waiting download (it started, or isn't wanted anymore).
pub fn not_waiting(id: &str) {
    STARTERS.with(|s| s.borrow_mut().remove(id));
    let gone = with(|r| {
        if r.items.get(id).is_some_and(|it| it.state == State::Waiting) {
            r.items.remove(id);
            true
        } else {
            false
        }
    });
    if gone {
        changed();
    }
}

/// org.umoja.Overlay.CancelDownload. False when there was nothing to stop.
pub fn cancel(id: &str) -> bool {
    let flag = with(|r| {
        let it = r.items.get_mut(id)?;
        if it.state == State::Waiting {
            return None;
        }
        let flag = it.cancel.clone()?;
        // Off the list at once, so the click shows; the downloader notices
        // at its next chunk.
        r.items.remove(id);
        Some(flag)
    });
    match flag {
        Some(f) => {
            f.store(true, Ordering::Relaxed);
            changed();
            true
        }
        None => false,
    }
}

/// org.umoja.Overlay.StartDownload: "Download now" on a waiting download.
pub fn start_now(id: &str) -> bool {
    let starter = STARTERS.with(|s| s.borrow_mut().remove(id));
    match starter {
        Some(f) => {
            not_waiting(id);
            f();
            true
        }
        None => false,
    }
}

/// Everything listed, oldest first, as `a(ssssttb)`; drops downloads that
/// ended more than [`LINGER`] ago.
pub fn snapshot() -> glib::Variant {
    let now = Instant::now();
    let mut items: Vec<(u64, (String, String, String, String, u64, u64, bool))> = with(|r| {
        r.items
            .retain(|_, it| it.ended.is_none_or(|t| now.duration_since(t) < LINGER));
        r.items
            .iter()
            .map(|(id, it)| {
                (
                    it.seq,
                    (
                        id.clone(),
                        it.kind.as_str().to_owned(),
                        it.title.clone(),
                        it.state.as_str().to_owned(),
                        it.done,
                        it.total,
                        it.cancel.is_some() && it.state == State::Downloading,
                    ),
                )
            })
            .collect()
    });
    items.sort_by_key(|(seq, _)| *seq);
    let items: Vec<_> = items.into_iter().map(|(_, i)| i).collect();
    glib::Variant::array_from_iter_with_type(
        glib::VariantTy::new(ITEM_TYPE).expect("type"),
        items.iter().map(glib::variant::ToVariant::to_variant),
    )
}

/// When the next lingering download should drop off the list.
fn next_expiry() -> Option<Duration> {
    let now = Instant::now();
    with(|r| {
        r.items
            .values()
            .filter_map(|it| it.ended)
            .map(|t| (t + LINGER).saturating_duration_since(now))
            .min()
    })
}

/// Call `emit` with a fresh [`snapshot`] whenever the list changes, at most
/// every [`THROTTLE`]. GTK main thread.
pub fn serve(emit: impl Fn(glib::Variant) + 'static) {
    let rx = WAKE.1.clone();
    glib::spawn_future_local(async move {
        loop {
            // Wait for a change, or for a finished download to expire.
            match next_expiry() {
                Some(d) => {
                    let wait = glib::timeout_future(d + Duration::from_millis(50));
                    futures_util::future::select(Box::pin(rx.recv()), wait).await;
                }
                None => {
                    if rx.recv().await.is_err() {
                        return;
                    }
                }
            }
            while rx.try_recv().is_ok() {}
            emit(snapshot());
            glib::timeout_future(THROTTLE).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<(String, String, u64, u64, bool)> {
        snapshot()
            .iter()
            .filter_map(|v| v.get::<(String, String, String, String, u64, u64, bool)>())
            .map(|(id, _, _, state, done, total, c)| (id, state, done, total, c))
            .collect()
    }

    // One test: the registry is global.
    #[test]
    fn lifecycle() {
        let a = start("model:A", Kind::Model, "A", 0, true);
        let b = start("voice:B", Kind::Voice, "B", 100, false);
        a.progress(50, 200);
        assert_eq!(
            ids(),
            vec![
                ("model:A".into(), "downloading".into(), 50, 200, true),
                ("voice:B".into(), "downloading".into(), 0, 100, false),
            ]
        );
        // B can't be cancelled; A can, and leaves the list at once.
        assert!(!cancel("voice:B"));
        assert!(cancel("model:A"));
        assert!(a.cancelled());
        a.finish(true);
        assert_eq!(ids().len(), 1);
        b.finish(true);
        assert_eq!(
            ids(),
            vec![("voice:B".into(), "done".into(), 100, 100, false)]
        );
        // A dropped handle (aborted task) is removed.
        let c = start("model:C", Kind::Model, "C", 10, true);
        drop(c);
        assert!(!ids().iter().any(|i| i.0 == "model:C"));
        remove("voice:B");
        assert!(ids().is_empty());
    }
}

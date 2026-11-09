//! Starts and stops the umoja-screensaver process for [`crate::idle`].

use crate::session::RUNTIME;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

struct Running {
    pid: i32,
    id: u64,
}

static CHILD: Mutex<Option<Running>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn child() -> std::sync::MutexGuard<'static, Option<Running>> {
    CHILD.lock().unwrap_or_else(|e| e.into_inner())
}

/// `UMOJA_SCREENSAVER` (a development build) or the installed binary.
fn binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("UMOJA_SCREENSAVER") {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from("/usr/bin/umoja-screensaver");
    p.exists().then_some(p)
}

pub fn available() -> bool {
    binary().is_some()
}

pub fn running() -> bool {
    child().is_some()
}

/// Start the screensaver (stopping any that runs). `on_exit(expected)` runs
/// when the process ends: `expected` is false if it ended without
/// [`stop`] (a crash), which leaves the desktop uncovered.
pub fn start(on_exit: impl FnOnce(bool) + Send + 'static) -> bool {
    stop();
    let Some(bin) = binary() else {
        return false;
    };
    let _rt = RUNTIME.enter();
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--managed");
    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(bin = %bin.display(), error = %e, "could not start the screensaver");
            return false;
        }
    };
    let Some(pid) = proc.id() else {
        return false;
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *child() = Some(Running {
        pid: pid as i32,
        id,
    });
    tracing::info!(pid, "screensaver started");
    RUNTIME.spawn(async move {
        let status = proc.wait().await;
        let expected = {
            let mut c = child();
            match c.as_ref() {
                Some(r) if r.id == id => {
                    *c = None;
                    false
                }
                _ => true,
            }
        };
        if !expected {
            tracing::warn!(?status, "screensaver exited by itself");
        }
        on_exit(expected);
    });
    true
}

pub fn stop() {
    if let Some(r) = child().take() {
        tracing::info!(pid = r.pid, "stopping the screensaver");
        // SAFETY: plain kill(2) on the pid of a child we spawned and
        // haven't reaped yet (the wait task reaps it).
        unsafe { libc::kill(r.pid, libc::SIGTERM) };
    }
}

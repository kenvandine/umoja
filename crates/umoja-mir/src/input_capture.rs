//! `org.gnome.Mutter.InputCapture`: lending the keyboard and pointer to
//! another computer.
//!
//! The InputCapture portal (xdg-desktop-portal-gnome asks the user first)
//! is how Deskflow, Input Leap and similar tools share one keyboard and
//! mouse between computers on Wayland. The client puts barriers on outer
//! screen edges ("zones" are the monitors); when the pointer pushes through
//! one, local input is captured (shim/eis.cpp) and goes to the client over
//! libei until it releases it, or the user presses Super+Escape.

use crate::session::RUNTIME;
use crate::tiling::Rect;
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd as StdOwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};
use zbus::{Connection, fdo, interface};

pub const BUS_NAME: &str = "org.gnome.Mutter.InputCapture";
const OBJECT_PATH: &str = "/org/gnome/Mutter/InputCapture";

const CAP_KEYBOARD: u32 = 1;
const CAP_POINTER: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct Barrier {
    id: u32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
}

unsafe extern "C" {
    fn umoja_eis_connect_capture(capabilities: u32, out_fd: *mut i32) -> u64;
    fn umoja_eis_disconnect(handle: u64);
    fn umoja_input_capture_enable(eis: u64, barriers: *const Barrier, n: usize);
    fn umoja_input_capture_disable() -> u32;
    fn umoja_input_capture_release(warp: bool, x: f64, y: f64) -> u32;
}

struct Session {
    owner: String,
    capabilities: u32,
    zones_serial: u32,
    barriers: Vec<Barrier>,
    next_barrier: u32,
    eis: u64,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> = LazyLock::new(Default::default);
/// The session whose barriers are live (one at a time).
static ENABLED: Mutex<Option<String>> = Mutex::new(None);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static CONNECTION: OnceLock<Connection> = OnceLock::new();

enum Event {
    Activated {
        barrier: u32,
        activation: u32,
        x: f64,
        y: f64,
    },
    Deactivated(u32),
}
static EVENTS: OnceLock<tokio::sync::mpsc::UnboundedSender<Event>> = OnceLock::new();

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Session>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

fn enabled() -> std::sync::MutexGuard<'static, Option<String>> {
    ENABLED.lock().unwrap_or_else(|e| e.into_inner())
}

fn failed(msg: impl Into<String>) -> fdo::Error {
    fdo::Error::Failed(msg.into())
}

/// From Mir's input thread: the pointer went through a barrier.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_input_capture_activated(
    barrier_id: u32,
    activation_id: u32,
    x: f64,
    y: f64,
) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(Event::Activated {
            barrier: barrier_id,
            activation: activation_id,
            x,
            y,
        });
    }
}

/// From Mir's input thread: the user took input back (Super+Escape).
#[unsafe(no_mangle)]
pub extern "C" fn umoja_input_capture_deactivated(activation_id: u32) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(Event::Deactivated(activation_id));
    }
}

/// Zones: the monitors, (width, height, x, y) in layout coordinates.
fn zones() -> Vec<(u32, u32, i32, i32)> {
    crate::display_config::outputs()
        .values()
        .map(|o| {
            (
                o.rect.w.max(0) as u32,
                o.rect.h.max(0) as u32,
                o.rect.x,
                o.rect.y,
            )
        })
        .collect()
}

/// A barrier must be a horizontal or vertical line along an outer edge of
/// a zone, where the pointer is held back (as mutter requires).
fn valid_barrier(b: &Barrier) -> bool {
    let rects: Vec<Rect> = crate::display_config::outputs()
        .values()
        .map(|o| o.rect)
        .collect();
    valid_barrier_on(b, &rects)
}

fn valid_barrier_on(b: &Barrier, rects: &[Rect]) -> bool {
    let open = |x: i32, y: i32| {
        !rects
            .iter()
            .any(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
    };
    if b.x1 == b.x2 {
        let (lo, hi) = (b.y1.min(b.y2), b.y1.max(b.y2));
        rects.iter().any(|r| {
            let on_left = b.x1 == r.x && open(r.x - 1, lo);
            let on_right = (b.x1 == r.x + r.w || b.x1 == r.x + r.w - 1) && open(r.x + r.w, lo);
            (on_left || on_right) && lo >= r.y && hi <= r.y + r.h
        })
    } else if b.y1 == b.y2 {
        let (lo, hi) = (b.x1.min(b.x2), b.x1.max(b.x2));
        rects.iter().any(|r| {
            let on_top = b.y1 == r.y && open(lo, r.y - 1);
            let on_bottom = (b.y1 == r.y + r.h || b.y1 == r.y + r.h - 1) && open(lo, r.y + r.h);
            (on_top || on_bottom) && lo >= r.x && hi <= r.x + r.w
        })
    } else {
        false
    }
}

struct InputCapture;

#[interface(name = "org.gnome.Mutter.InputCapture")]
impl InputCapture {
    async fn create_session(
        &self,
        capabilities: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| failed("No sender"))?;
        if capabilities & !(CAP_KEYBOARD | CAP_POINTER) != 0 {
            return Err(fdo::Error::InvalidArgs("Unsupported capabilities".into()));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = format!("{OBJECT_PATH}/Session/u{id}");
        sessions().insert(
            path.clone(),
            Session {
                owner,
                capabilities,
                zones_serial: crate::display_config::layout_serial(),
                barriers: Vec::new(),
                next_barrier: 1,
                eis: 0,
            },
        );
        conn.object_server()
            .at(path.as_str(), CaptureSession { path: path.clone() })
            .await
            .map_err(|e| failed(e.to_string()))?;
        tracing::info!(session = %path, "input capture session created");
        OwnedObjectPath::try_from(path).map_err(|e| failed(e.to_string()))
    }

    #[zbus(property)]
    fn supported_capabilities(&self) -> u32 {
        CAP_KEYBOARD | CAP_POINTER
    }
}

struct CaptureSession {
    path: String,
}

impl CaptureSession {
    fn check(&self, header: &Header<'_>) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string());
        match sessions().get(&self.path) {
            Some(s) if Some(&s.owner) == sender.as_ref() => Ok(()),
            Some(_) => Err(fdo::Error::AccessDenied("Not the session's owner".into())),
            None => Err(failed("Session closed")),
        }
    }
}

#[interface(name = "org.gnome.Mutter.InputCapture.Session")]
impl CaptureSession {
    async fn get_zones(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<(u32, Vec<(u32, u32, i32, i32)>)> {
        self.check(&header)?;
        let serial = crate::display_config::layout_serial();
        if let Some(s) = sessions().get_mut(&self.path) {
            s.zones_serial = serial;
        }
        Ok((serial, zones()))
    }

    async fn add_barrier(
        &self,
        serial: u32,
        position: (i32, i32, i32, i32),
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<u32> {
        self.check(&header)?;
        let (x1, y1, x2, y2) = position;
        let mut b = Barrier {
            id: 0,
            x1,
            y1,
            x2,
            y2,
        };
        if !valid_barrier(&b) {
            return Err(fdo::Error::InvalidArgs(format!(
                "Barrier ({x1}, {y1}, {x2}, {y2}) is not on an outer screen edge"
            )));
        }
        let mut sessions = sessions();
        let s = sessions
            .get_mut(&self.path)
            .ok_or_else(|| failed("Session closed"))?;
        if serial != s.zones_serial {
            return Err(failed("The zones changed: get them again"));
        }
        b.id = s.next_barrier;
        s.next_barrier += 1;
        s.barriers.push(b);
        let barriers = s.barriers.clone();
        let eis = s.eis;
        drop(sessions);
        if enabled().as_deref() == Some(self.path.as_str()) {
            unsafe { umoja_input_capture_enable(eis, barriers.as_ptr(), barriers.len()) };
        }
        Ok(b.id)
    }

    async fn clear_barriers(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        let eis = {
            let mut sessions = sessions();
            let s = sessions
                .get_mut(&self.path)
                .ok_or_else(|| failed("Session closed"))?;
            s.barriers.clear();
            s.eis
        };
        if enabled().as_deref() == Some(self.path.as_str()) {
            unsafe { umoja_input_capture_enable(eis, std::ptr::null(), 0) };
        }
        Ok(())
    }

    async fn enable(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        let (eis, barriers) = {
            let sessions = sessions();
            let s = sessions
                .get(&self.path)
                .ok_or_else(|| failed("Session closed"))?;
            (s.eis, s.barriers.clone())
        };
        let previous = enabled().replace(self.path.clone());
        if let Some(previous) = previous.filter(|p| *p != self.path) {
            // Another session's barriers give way, as it is told.
            if let Some(conn) = CONNECTION.get()
                && let Ok(e) = SignalEmitter::new(conn, previous.as_str())
            {
                let _ = CaptureSession::disabled(&e).await;
            }
        }
        unsafe { umoja_input_capture_enable(eis, barriers.as_ptr(), barriers.len()) };
        tracing::info!(session = %self.path, barriers = barriers.len(), "input capture enabled");
        Ok(())
    }

    async fn disable(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let was_enabled = {
            let mut current = enabled();
            let was = current.as_deref() == Some(self.path.as_str());
            if was {
                *current = None;
            }
            was
        };
        if was_enabled {
            let ended = unsafe { umoja_input_capture_disable() };
            if ended != 0 {
                let _ = CaptureSession::deactivated(&emitter, ended).await;
            }
        }
        Ok(())
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedFd> {
        self.check(&header)?;
        let caps = {
            let sessions = sessions();
            let s = sessions
                .get(&self.path)
                .ok_or_else(|| failed("Session closed"))?;
            if s.eis != 0 {
                return Err(failed("Already connected"));
            }
            s.capabilities
        };
        let mut fd = -1;
        let eis = unsafe { umoja_eis_connect_capture(caps, &mut fd) };
        if eis == 0 || fd < 0 {
            return Err(failed("Could not set up a libei connection"));
        }
        let barriers = match sessions().get_mut(&self.path) {
            Some(s) => {
                s.eis = eis;
                s.barriers.clone()
            }
            None => {
                unsafe { umoja_eis_disconnect(eis) };
                return Err(failed("Session closed"));
            }
        };
        if enabled().as_deref() == Some(self.path.as_str()) {
            unsafe { umoja_input_capture_enable(eis, barriers.as_ptr(), barriers.len()) };
        }
        // SAFETY: the client's end, ours alone.
        let fd = unsafe { StdOwnedFd::from_raw_fd(fd) };
        Ok(OwnedFd::from(fd))
    }

    async fn release(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        if enabled().as_deref() != Some(self.path.as_str()) {
            return Ok(());
        }
        let position = options
            .get("cursor_position")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(f64, f64)>::try_from(v).ok());
        let (x, y) = position.unwrap_or((0.0, 0.0));
        let ended = tokio::task::spawn_blocking(move || unsafe {
            umoja_input_capture_release(position.is_some(), x, y)
        })
        .await
        .unwrap_or(0);
        if ended != 0 {
            let _ = CaptureSession::deactivated(&emitter, ended).await;
        }
        Ok(())
    }

    async fn close(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        close_session(&self.path).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn activated(
        emitter: &SignalEmitter<'_>,
        barrier_id: u32,
        activation_id: u32,
        cursor_position: (f64, f64),
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(emitter: &SignalEmitter<'_>, activation_id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn zones_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn disabled(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

async fn close_session(path: &str) {
    let Some(session) = sessions().remove(path) else {
        return;
    };
    let was_enabled = {
        let mut current = enabled();
        let was = current.as_deref() == Some(path);
        if was {
            *current = None;
        }
        was
    };
    if was_enabled {
        unsafe { umoja_input_capture_disable() };
    }
    if session.eis != 0 {
        let eis = session.eis;
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_eis_disconnect(eis) }).await;
    }
    tracing::info!(session = %path, "input capture session closed");
    if let Some(conn) = CONNECTION.get() {
        if let Ok(e) = SignalEmitter::new(conn, path) {
            let _ = CaptureSession::closed(&e).await;
        }
        let _ = conn.object_server().remove::<CaptureSession, _>(path).await;
    }
}

async fn handle_events(conn: Connection, mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = rx.recv().await {
        let Some(path) = enabled().clone() else {
            continue;
        };
        let Ok(emitter) = SignalEmitter::new(&conn, path.as_str()) else {
            continue;
        };
        match event {
            Event::Activated {
                barrier,
                activation,
                x,
                y,
            } => {
                tracing::info!(session = %path, barrier, "input captured");
                let _ = CaptureSession::activated(&emitter, barrier, activation, (x, y)).await;
            }
            Event::Deactivated(activation) => {
                tracing::info!(session = %path, "input capture released by the user");
                let _ = CaptureSession::deactivated(&emitter, activation).await;
            }
        }
    }
}

/// Monitors changed: barriers no longer mean what they did. Like mutter,
/// tell the sessions (ZonesChanged) and drop the live barriers until the
/// client sets new ones.
async fn watch_zones(conn: Connection) {
    let mut serial = crate::display_config::layout_serial();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let now = crate::display_config::layout_serial();
        if now == serial {
            continue;
        }
        serial = now;
        let paths: Vec<String> = {
            let mut sessions = sessions();
            for s in sessions.values_mut() {
                s.barriers.clear();
            }
            sessions.keys().cloned().collect()
        };
        let live = enabled().clone();
        if let Some(eis) = live.and_then(|p| sessions().get(&p).map(|s| s.eis)) {
            unsafe { umoja_input_capture_enable(eis, std::ptr::null(), 0) };
        }
        for path in paths {
            if let Ok(e) = SignalEmitter::new(&conn, path.as_str()) {
                let _ = CaptureSession::zones_changed(&e).await;
            }
        }
    }
}

async fn watch_owners(conn: Connection) {
    use futures_util::StreamExt;
    let Ok(proxy) = fdo::DBusProxy::new(&conn).await else {
        return;
    };
    let Ok(mut changes) = proxy.receive_name_owner_changed().await else {
        return;
    };
    while let Some(change) = changes.next().await {
        let Ok(args) = change.args() else { continue };
        if args.new_owner.is_some() {
            continue;
        }
        let name = args.name.to_string();
        let gone: Vec<String> = sessions()
            .iter()
            .filter(|(_, s)| s.owner == name)
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            close_session(&path).await;
        }
    }
}

pub async fn serve(conn: &Connection) -> zbus::Result<()> {
    conn.object_server().at(OBJECT_PATH, InputCapture).await?;
    let _ = CONNECTION.set(conn.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = EVENTS.set(tx);
    RUNTIME.spawn(handle_events(conn.clone(), rx));
    RUNTIME.spawn(watch_zones(conn.clone()));
    RUNTIME.spawn(watch_owners(conn.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn barrier(x1: i32, y1: i32, x2: i32, y2: i32) -> Barrier {
        Barrier {
            id: 0,
            x1,
            y1,
            x2,
            y2,
        }
    }

    #[test]
    fn barriers_must_be_on_outer_edges() {
        // Two monitors side by side: 1920x1080 at 0,0 and 1280x1024 at 1920,0.
        let rects = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1280, 1024)];
        // Left edge of the left monitor, right edge of the right one (the
        // portal spec's x = x + width, and x + width - 1).
        assert!(valid_barrier_on(&barrier(0, 0, 0, 1079), &rects));
        assert!(valid_barrier_on(&barrier(3200, 0, 3200, 1023), &rects));
        assert!(valid_barrier_on(&barrier(3199, 100, 3199, 200), &rects));
        // Top and bottom edges.
        assert!(valid_barrier_on(&barrier(0, 0, 1919, 0), &rects));
        assert!(valid_barrier_on(&barrier(0, 1080, 1919, 1080), &rects));
        // Between the monitors the pointer isn't held back.
        assert!(!valid_barrier_on(&barrier(1920, 0, 1920, 1000), &rects));
        // Not along an edge, or diagonal.
        assert!(!valid_barrier_on(&barrier(500, 0, 500, 1079), &rects));
        assert!(!valid_barrier_on(&barrier(0, 0, 100, 100), &rects));
        // Longer than the edge.
        assert!(!valid_barrier_on(&barrier(0, 0, 0, 2000), &rects));
    }
}

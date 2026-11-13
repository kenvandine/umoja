//! `org.gnome.Mutter.ScreenCast`: screen and window sharing.
//!
//! Browsers (Meet, Zoom and Teams on the web), OBS and remote desktop tools
//! share the screen through the ScreenCast portal. Umoja routes that portal
//! to xdg-desktop-portal-gnome (data/portal/umoja-portals.conf), which shows
//! GNOME's own picker (monitors from DisplayConfig, windows from
//! `org.gnome.Shell.Introspect`, see introspect.rs) and then asks the
//! compositor, as it asks mutter, for a PipeWire stream of what was picked.
//! This module is that compositor side; the streams themselves are rendered
//! by shim/screencast.cpp.
//!
//! A session is created, gets one or more streams (RecordMonitor,
//! RecordWindow, RecordArea), is started (each stream becomes a PipeWire
//! node and its id goes out in PipeWireStreamAdded) and is stopped, by its
//! creator or because a shared window closed or its creator left the bus.
//! A session made for a remote desktop session is started and stopped by
//! that one (remote_desktop.rs).

use crate::session::RUNTIME;
use crate::tiling::Rect;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, fdo, interface};

pub const BUS_NAME: &str = "org.gnome.Mutter.ScreenCast";
const OBJECT_PATH: &str = "/org/gnome/Mutter/ScreenCast";
/// mutter's API version: 5 has PipeWireStreamAddedV2.
const VERSION: i32 = 5;

const CURSOR_HIDDEN: u32 = 0;
const CURSOR_METADATA: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CastParams {
    kind: u32,
    window: u64,
    area: Rect,
    scale: f64,
    cursor_mode: u32,
    max_fps: u32,
}

const KIND_AREA: u32 = 0;
const KIND_WINDOW: u32 = 1;

unsafe extern "C" {
    fn umoja_cast_start(handle: u64, params: *const CastParams) -> bool;
    fn umoja_cast_stop(handle: u64);
}

struct Stream {
    path: OwnedObjectPath,
    params: CastParams,
    handle: u64,
}

struct Session {
    /// The unique bus name of the session's creator.
    owner: String,
    streams: Vec<Stream>,
    started: bool,
    /// Started and stopped by this remote desktop session.
    remote_desktop: Option<String>,
}

/// Sessions by object path.
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> = LazyLock::new(Default::default);
/// Running streams: stream handle -> (session path, stream path).
static HANDLES: LazyLock<Mutex<HashMap<u64, (String, OwnedObjectPath)>>> =
    LazyLock::new(Default::default);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static CONNECTION: OnceLock<Connection> = OnceLock::new();

enum Event {
    NodeAdded { handle: u64, node: u32, serial: u64 },
    Closed(u64),
}

static EVENTS: OnceLock<tokio::sync::mpsc::UnboundedSender<Event>> = OnceLock::new();

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Session>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

fn handles() -> std::sync::MutexGuard<'static, HashMap<u64, (String, OwnedObjectPath)>> {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// From the shim, on PipeWire's thread: the stream's node exists.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_cast_node_added(handle: u64, node: u32, serial: u64) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(Event::NodeAdded {
            handle,
            node,
            serial,
        });
    }
}

/// From the shim: the stream ended by itself (its window closed, PipeWire
/// went away). The whole session ends, as in mutter.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_cast_closed(handle: u64) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(Event::Closed(handle));
    }
}

fn prop<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values convert")
}

fn get<'a, T: TryFrom<&'a OwnedValue>>(
    props: &'a HashMap<String, OwnedValue>,
    key: &str,
) -> Option<T> {
    props.get(key).and_then(|v| T::try_from(v).ok())
}

fn get_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .map(str::to_string)
}

fn failed(msg: impl Into<String>) -> fdo::Error {
    fdo::Error::Failed(msg.into())
}

/// Where `rect` mostly is: the output with the largest overlap (or the
/// primary), for its scale.
fn scale_for(rect: Rect) -> f64 {
    let outputs: Vec<(Rect, f64)> = crate::display_config::outputs()
        .values()
        .map(|o| (o.rect, o.scale))
        .collect();
    let primary = crate::display_config::outputs()
        .get(&crate::display_config::primary_output_id())
        .map(|o| o.scale);
    scale_on(rect, &outputs).or(primary).unwrap_or(1.0)
}

/// The scale of the output `rect` overlaps most, if it overlaps any.
fn scale_on(rect: Rect, outputs: &[(Rect, f64)]) -> Option<f64> {
    let overlap = |o: &Rect| {
        let w = (rect.x + rect.w).min(o.x + o.w) - rect.x.max(o.x);
        let h = (rect.y + rect.h).min(o.y + o.h) - rect.y.max(o.y);
        if w > 0 && h > 0 {
            w as i64 * h as i64
        } else {
            0
        }
    };
    outputs
        .iter()
        .max_by_key(|(r, _)| overlap(r))
        .filter(|(r, _)| overlap(r) > 0)
        .map(|(_, s)| *s)
}

/// An area's scale is that of the sharpest output it touches, so nothing
/// in it is shown blurrier than on screen.
fn max_scale_for(rect: Rect) -> f64 {
    let outputs: Vec<(Rect, f64)> = crate::display_config::outputs()
        .values()
        .map(|o| (o.rect, o.scale))
        .collect();
    max_scale_on(rect, &outputs)
}

fn max_scale_on(rect: Rect, outputs: &[(Rect, f64)]) -> f64 {
    outputs
        .iter()
        .filter(|(o, _)| {
            rect.x < o.x + o.w
                && o.x < rect.x + rect.w
                && rect.y < o.y + o.h
                && o.y < rect.y + rect.h
        })
        .map(|(_, s)| *s)
        .fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))))
        .unwrap_or(1.0)
}

fn cursor_mode(props: &HashMap<String, OwnedValue>) -> fdo::Result<u32> {
    let mode = get::<u32>(props, "cursor-mode").unwrap_or(CURSOR_HIDDEN);
    if mode > CURSOR_METADATA {
        return Err(fdo::Error::InvalidArgs(format!(
            "Unknown cursor mode {mode}"
        )));
    }
    Ok(mode)
}

/// What a stream shows, for the remote desktop's absolute pointer motion:
/// stream pixel (x, y) is at layout (rect.x + x / scale, rect.y + y / scale).
pub fn stream_geometry(stream_path: &str) -> Option<(Rect, f64)> {
    let params = {
        let sessions = sessions();
        sessions
            .values()
            .flat_map(|s| s.streams.iter())
            .find(|s| s.path.as_str() == stream_path)
            .map(|s| s.params)?
    };
    if params.kind == KIND_WINDOW {
        let rect = crate::shell::window_rect(params.window)?;
        return Some((rect, params.scale));
    }
    Some((params.area, params.scale))
}

/// Whether anything is being shared right now, for the panel's indicator.
pub fn sharing() -> bool {
    sessions().values().any(|s| s.started)
}

struct ScreenCast;

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| failed("No sender"))?;
        let remote_desktop = get_string(&properties, "remote-desktop-session-id");
        if let Some(id) = &remote_desktop
            && !crate::remote_desktop::session_exists(id)
        {
            return Err(failed(format!("No remote desktop session {id}")));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = format!("{OBJECT_PATH}/Session/u{id}");
        sessions().insert(
            path.clone(),
            Session {
                owner,
                streams: Vec::new(),
                started: false,
                remote_desktop,
            },
        );
        conn.object_server()
            .at(path.as_str(), CastSession { path: path.clone() })
            .await
            .map_err(|e| failed(e.to_string()))?;
        tracing::info!(session = %path, "screen cast session created");
        OwnedObjectPath::try_from(path).map_err(|e| failed(e.to_string()))
    }

    #[zbus(property)]
    fn version(&self) -> i32 {
        VERSION
    }
}

struct CastSession {
    path: String,
}

impl CastSession {
    /// Only the session's creator (the portal) may drive it, as in mutter.
    fn check(&self, header: &Header<'_>) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string());
        match sessions().get(&self.path) {
            Some(s) if Some(&s.owner) == sender.as_ref() => Ok(()),
            Some(_) => Err(fdo::Error::AccessDenied("Not the session's owner".into())),
            None => Err(failed("Session closed")),
        }
    }

    async fn add_stream(
        &self,
        conn: &Connection,
        params: CastParams,
        parameters: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("{OBJECT_PATH}/Stream/u{id}"))
            .map_err(|e| failed(e.to_string()))?;
        {
            let mut sessions = sessions();
            let session = sessions
                .get_mut(&self.path)
                .ok_or_else(|| failed("Session closed"))?;
            if session.started {
                return Err(failed("Session already started"));
            }
            session.streams.push(Stream {
                path: path.clone(),
                params,
                handle: 0,
            });
        }
        conn.object_server()
            .at(path.as_str(), CastStream { parameters })
            .await
            .map_err(|e| failed(e.to_string()))?;
        Ok(path)
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl CastSession {
    async fn start(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        let driven = sessions()
            .get(&self.path)
            .map(|s| s.remote_desktop.is_some())
            .ok_or_else(|| failed("Session closed"))?;
        if driven {
            return Err(failed("Must be started from the remote desktop session"));
        }
        start_session(&self.path).await
    }

    async fn stop(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        let driven = sessions()
            .get(&self.path)
            .map(|s| s.remote_desktop.is_some())
            .unwrap_or(false);
        if driven {
            return Err(failed("Must be stopped from the remote desktop session"));
        }
        close_session(&self.path).await;
        Ok(())
    }

    async fn record_monitor(
        &self,
        connector: String,
        properties: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let outputs = crate::display_config::outputs();
        // An empty connector means the primary monitor.
        let output = if connector.is_empty() {
            outputs.get(&crate::display_config::primary_output_id())
        } else {
            outputs.values().find(|o| o.name == connector)
        }
        .ok_or_else(|| failed(format!("Unknown monitor {connector}")))?;
        let params = CastParams {
            kind: KIND_AREA,
            window: 0,
            area: output.rect,
            scale: output.scale,
            cursor_mode: cursor_mode(&properties)?,
            max_fps: output.refresh.round().clamp(1.0, 60.0) as u32,
        };
        let r = output.rect;
        let parameters = HashMap::from([
            ("position".to_string(), prop((r.x, r.y))),
            ("size".to_string(), prop((r.w, r.h))),
        ]);
        self.add_stream(conn, params, parameters).await
    }

    async fn record_window(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let id = get::<u64>(&properties, "window-id")
            .ok_or_else(|| fdo::Error::InvalidArgs("No window-id".into()))?;
        let rect = tokio::task::spawn_blocking(move || crate::shell::window_rect(id))
            .await
            .ok()
            .flatten()
            .ok_or_else(|| failed("Unknown window"))?;
        let scale = scale_for(rect);
        let params = CastParams {
            kind: KIND_WINDOW,
            window: id,
            area: rect,
            scale,
            cursor_mode: cursor_mode(&properties)?,
            max_fps: 60,
        };
        let parameters = HashMap::from([("size".to_string(), prop((rect.w, rect.h)))]);
        self.add_stream(conn, params, parameters).await
    }

    async fn record_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        properties: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        if width <= 0 || height <= 0 {
            return Err(fdo::Error::InvalidArgs("Empty area".into()));
        }
        let area = Rect::new(x, y, width, height);
        let params = CastParams {
            kind: KIND_AREA,
            window: 0,
            area,
            scale: max_scale_for(area),
            cursor_mode: cursor_mode(&properties)?,
            max_fps: 60,
        };
        let parameters = HashMap::from([
            ("position".to_string(), prop((x, y))),
            ("size".to_string(), prop((width, height))),
        ]);
        self.add_stream(conn, params, parameters).await
    }

    /// Virtual monitors need the compositor to drive an extra output, which
    /// Umoja doesn't do (yet). The portal only offers it to apps that ask.
    async fn record_virtual(
        &self,
        _properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check(&header)?;
        Err(fdo::Error::NotSupported(
            "Virtual monitors are not supported".into(),
        ))
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct CastStream {
    parameters: HashMap<String, OwnedValue>,
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl CastStream {
    /// Streams start and stop with their session; like mutter, these do
    /// nothing on their own.
    async fn start(&self) {}

    async fn stop(&self) {}

    #[zbus(signal, name = "PipeWireStreamAdded")]
    async fn pipewire_stream_added(emitter: &SignalEmitter<'_>, node_id: u32) -> zbus::Result<()>;

    #[zbus(signal, name = "PipeWireStreamAddedV2")]
    async fn pipewire_stream_added_v2(
        emitter: &SignalEmitter<'_>,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn parameters(&self) -> HashMap<String, OwnedValue> {
        self.parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.try_clone().expect("plain values clone")))
            .collect()
    }
}

/// Start every stream of a session (ScreenCast.Session.Start, or the
/// remote desktop session it belongs to).
pub async fn start_session(path: &str) -> fdo::Result<()> {
    let params: Vec<(OwnedObjectPath, CastParams)> = {
        let mut sessions = sessions();
        let session = sessions
            .get_mut(path)
            .ok_or_else(|| failed("Session closed"))?;
        if session.started {
            return Err(failed("Session already started"));
        }
        session.started = true;
        session
            .streams
            .iter()
            .map(|s| (s.path.clone(), s.params))
            .collect()
    };
    for (stream_path, p) in params {
        // Registered before the stream exists, so its node event finds it.
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        handles().insert(handle, (path.to_string(), stream_path.clone()));
        let recorded = {
            let mut sessions = sessions();
            match sessions
                .get_mut(path)
                .and_then(|s| s.streams.iter_mut().find(|s| s.path == stream_path))
            {
                Some(stream) => {
                    stream.handle = handle;
                    true
                }
                None => false,
            }
        };
        if !recorded {
            // Closed meanwhile.
            handles().remove(&handle);
            return Err(failed("Session closed"));
        }
        let started = tokio::task::spawn_blocking(move || unsafe { umoja_cast_start(handle, &p) })
            .await
            .unwrap_or(false);
        if !started {
            tracing::warn!(stream = %stream_path, "screen cast stream failed to start");
            close_session(path).await;
            return Err(failed("Could not start the stream (is PipeWire running?)"));
        }
        if !handles().contains_key(&handle) {
            // The session closed while the stream started: nobody stopped it.
            let _ = tokio::task::spawn_blocking(move || unsafe { umoja_cast_stop(handle) }).await;
            return Err(failed("Session closed"));
        }
    }
    tracing::info!(session = %path, "screen cast started");
    crate::shell::screen_sharing_changed();
    Ok(())
}

/// End a session: its streams stop, Closed goes out and its objects go.
pub async fn close_session(path: &str) {
    let Some(session) = sessions().remove(path) else {
        return;
    };
    let mut stopped = Vec::new();
    for stream in &session.streams {
        if stream.handle != 0 {
            handles().remove(&stream.handle);
            stopped.push(stream.handle);
        }
    }
    if !stopped.is_empty() {
        let _ = tokio::task::spawn_blocking(move || {
            for handle in stopped {
                unsafe { umoja_cast_stop(handle) };
            }
        })
        .await;
    }
    tracing::info!(session = %path, "screen cast session closed");
    let Some(conn) = CONNECTION.get() else {
        return;
    };
    if let Ok(emitter) = SignalEmitter::new(conn, path) {
        let _ = CastSession::closed(&emitter).await;
    }
    let server = conn.object_server();
    for stream in &session.streams {
        let _ = server.remove::<CastStream, _>(stream.path.as_str()).await;
    }
    let _ = server.remove::<CastSession, _>(path).await;
    if session.started {
        crate::shell::screen_sharing_changed();
    }
}

/// The screen cast sessions a remote desktop session drives.
pub fn sessions_of_remote_desktop(id: &str) -> Vec<String> {
    sessions()
        .iter()
        .filter(|(_, s)| s.remote_desktop.as_deref() == Some(id))
        .map(|(p, _)| p.clone())
        .collect()
}

/// End every session (the panel's Stop Sharing).
pub async fn close_all() {
    let paths: Vec<String> = sessions().keys().cloned().collect();
    for path in paths {
        close_session(&path).await;
    }
}

async fn handle_events(conn: Connection, mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = rx.recv().await {
        match event {
            Event::NodeAdded {
                handle,
                node,
                serial,
            } => {
                let Some((_, stream)) = handles().get(&handle).cloned() else {
                    continue;
                };
                tracing::info!(stream = %stream, node, "screen cast stream ready");
                let Ok(emitter) = SignalEmitter::new(&conn, stream.as_str()) else {
                    continue;
                };
                let _ = CastStream::pipewire_stream_added(&emitter, node).await;
                let props = HashMap::from([
                    ("node-id".to_string(), prop(node)),
                    ("node-serial".to_string(), prop(serial)),
                ]);
                let _ = CastStream::pipewire_stream_added_v2(&emitter, props).await;
            }
            Event::Closed(handle) => {
                let Some((session, _)) = handles().get(&handle).cloned() else {
                    continue;
                };
                let rd = sessions()
                    .get(&session)
                    .and_then(|s| s.remote_desktop.clone());
                if let Some(rd) = rd {
                    // The remote desktop session ends with it, as in mutter.
                    crate::remote_desktop::close(&rd).await;
                }
                close_session(&session).await;
            }
        }
    }
}

/// Sessions end when the process that made them leaves the bus.
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
            tracing::info!(session = %path, owner = %name, "screen cast owner left");
            close_session(&path).await;
        }
    }
}

/// Serve the screen cast API on `conn` (which also owns the other
/// mutter/gnome-shell names, see remote_desktop::start).
pub async fn serve(conn: &Connection) -> zbus::Result<()> {
    conn.object_server().at(OBJECT_PATH, ScreenCast).await?;
    let _ = CONNECTION.set(conn.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = EVENTS.set(tx);
    RUNTIME.spawn(handle_events(conn.clone(), rx));
    RUNTIME.spawn(watch_owners(conn.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_scale_follows_the_monitor() {
        // A 2x laptop panel left of a 1x monitor.
        let outputs = [
            (Rect::new(0, 0, 1440, 900), 2.0),
            (Rect::new(1440, 0, 1920, 1080), 1.0),
        ];
        // A window mostly on the external monitor streams at its scale.
        assert_eq!(
            scale_on(Rect::new(1300, 100, 800, 600), &outputs),
            Some(1.0)
        );
        assert_eq!(scale_on(Rect::new(100, 100, 800, 600), &outputs), Some(2.0));
        assert_eq!(scale_on(Rect::new(5000, 0, 10, 10), &outputs), None);
        // An area across both is as sharp as the sharper one.
        assert_eq!(max_scale_on(Rect::new(1000, 0, 1000, 500), &outputs), 2.0);
        assert_eq!(max_scale_on(Rect::new(2000, 0, 100, 100), &outputs), 1.0);
        assert_eq!(max_scale_on(Rect::new(9000, 0, 100, 100), &outputs), 1.0);
    }
}

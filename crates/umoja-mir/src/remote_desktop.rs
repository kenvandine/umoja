//! `org.gnome.Mutter.RemoteDesktop`: remote control of the session.
//!
//! The RemoteDesktop portal (xdg-desktop-portal-gnome, as on GNOME) lets
//! remote desktop tools and screen sharing apps with remote control send
//! pointer, keyboard and touch input, after the user agreed in the portal's
//! dialog. Input arrives as Notify* calls or through a libei connection
//! (ConnectToEIS, eis.rs) and goes into the session's own virtual devices
//! (shim/remote_input.cpp). A remote desktop session usually comes with a
//! screen cast session (screencast.rs) that it starts and stops, and whose
//! streams give absolute pointer and touch positions their meaning.
//!
//! The session ends when its creator stops it or leaves the bus; whatever
//! it held down is released then.

use crate::session::RUNTIME;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};
use zbus::{Connection, fdo, interface};

pub const BUS_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
const OBJECT_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const VERSION: i32 = 1;

const DEVICE_KEYBOARD: u32 = 1;
const DEVICE_POINTER: u32 = 2;
const DEVICE_TOUCHSCREEN: u32 = 4;

const TOUCH_DOWN: u32 = 0;
const TOUCH_MOTION: u32 = 1;
const TOUCH_UP: u32 = 2;

/// NotifyPointerAxis flags.
const AXIS_FINISH: u32 = 1;

unsafe extern "C" {
    fn umoja_remote_input_new() -> u64;
    fn umoja_remote_input_free(handle: u64);
    fn umoja_remote_key(handle: u64, evdev_code: u32, pressed: bool);
    fn umoja_remote_keysym(handle: u64, keysym: u32, pressed: bool) -> bool;
    fn umoja_remote_pointer_motion(handle: u64, dx: f64, dy: f64);
    fn umoja_remote_pointer_absolute(handle: u64, x: f64, y: f64);
    fn umoja_remote_pointer_button(handle: u64, linux_button: u32, pressed: bool);
    fn umoja_remote_pointer_scroll(
        handle: u64,
        dx: f64,
        dy: f64,
        steps_x: i32,
        steps_y: i32,
        finish: bool,
    );
    fn umoja_remote_touch(handle: u64, action: u32, slot: u32, x: f64, y: f64);
}

struct Session {
    path: String,
    owner: String,
    /// The virtual devices, once started.
    input: u64,
    /// libei connections (eis.rs), closed with the session.
    eis: Vec<u64>,
}

/// Sessions by session id (the SessionId property).
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> = LazyLock::new(Default::default);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static CONNECTION: OnceLock<Connection> = OnceLock::new();

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Session>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

fn failed(msg: impl Into<String>) -> fdo::Error {
    fdo::Error::Failed(msg.into())
}

pub fn session_exists(id: &str) -> bool {
    sessions().contains_key(id)
}

/// A started session's input handle.
fn input_of(id: &str) -> fdo::Result<u64> {
    match sessions().get(id) {
        Some(s) if s.input != 0 => Ok(s.input),
        Some(_) => Err(failed("Session not started")),
        None => Err(failed("Session closed")),
    }
}

/// Stream pixel (x, y) of a screen cast stream of this session, in layout
/// coordinates.
fn stream_position(stream: &str, x: f64, y: f64) -> fdo::Result<(f64, f64)> {
    let (rect, scale) = crate::screencast::stream_geometry(stream)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("Unknown stream {stream}")))?;
    let scale = if scale > 0.0 { scale } else { 1.0 };
    Ok((rect.x as f64 + x / scale, rect.y as f64 + y / scale))
}

struct RemoteDesktop;

#[interface(name = "org.gnome.Mutter.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| failed("No sender"))?;
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // Unguessable enough that only the portal, which is told, links a
        // screen cast session to it.
        let id = format!("{:016x}{n:08x}", rand_u64());
        let path = format!("{OBJECT_PATH}/Session/u{n}");
        sessions().insert(
            id.clone(),
            Session {
                path: path.clone(),
                owner,
                input: 0,
                eis: Vec::new(),
            },
        );
        conn.object_server()
            .at(path.as_str(), RemoteSession { id: id.clone() })
            .await
            .map_err(|e| failed(e.to_string()))?;
        tracing::info!(session = %path, "remote desktop session created");
        OwnedObjectPath::try_from(path).map_err(|e| failed(e.to_string()))
    }

    #[zbus(property)]
    fn supported_device_types(&self) -> u32 {
        DEVICE_KEYBOARD | DEVICE_POINTER | DEVICE_TOUCHSCREEN
    }

    #[zbus(property)]
    fn version(&self) -> i32 {
        VERSION
    }
}

fn rand_u64() -> u64 {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    u64::from_ne_bytes(buf)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
}

struct RemoteSession {
    id: String,
}

impl RemoteSession {
    /// Only the session's creator (the portal) may drive it, as in mutter.
    fn check(&self, header: &Header<'_>) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string());
        match sessions().get(&self.id) {
            Some(s) if Some(&s.owner) == sender.as_ref() => Ok(()),
            Some(_) => Err(fdo::Error::AccessDenied("Not the session's owner".into())),
            None => Err(failed("Session closed")),
        }
    }
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop.Session")]
impl RemoteSession {
    #[zbus(property)]
    fn session_id(&self) -> String {
        self.id.clone()
    }

    async fn start(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        {
            let sessions = sessions();
            let s = sessions
                .get(&self.id)
                .ok_or_else(|| failed("Session closed"))?;
            if s.input != 0 {
                return Err(failed("Session already started"));
            }
        }
        let input = tokio::task::spawn_blocking(|| unsafe { umoja_remote_input_new() })
            .await
            .unwrap_or(0);
        if input == 0 {
            close(&self.id).await;
            return Err(failed("Could not create input devices"));
        }
        let stale = match sessions().get_mut(&self.id) {
            Some(s) => {
                s.input = input;
                false
            }
            None => true,
        };
        if stale {
            unsafe { umoja_remote_input_free(input) };
            return Err(failed("Session closed"));
        }
        for cast in crate::screencast::sessions_of_remote_desktop(&self.id) {
            if let Err(e) = crate::screencast::start_session(&cast).await {
                close(&self.id).await;
                return Err(e);
            }
        }
        tracing::info!(session = %self.id, "remote desktop session started");
        Ok(())
    }

    async fn stop(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.check(&header)?;
        close(&self.id).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    async fn notify_keyboard_keycode(
        &self,
        keycode: u32,
        state: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        unsafe { umoja_remote_key(h, keycode, state) };
        Ok(())
    }

    async fn notify_keyboard_keysym(
        &self,
        keysym: u32,
        state: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        if unsafe { umoja_remote_keysym(h, keysym, state) } {
            Ok(())
        } else {
            Err(fdo::Error::InvalidArgs(format!(
                "No key for keysym {keysym:#x} in the keymap"
            )))
        }
    }

    async fn notify_pointer_button(
        &self,
        button: i32,
        state: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        unsafe { umoja_remote_pointer_button(h, button.max(0) as u32, state) };
        Ok(())
    }

    async fn notify_pointer_axis(
        &self,
        dx: f64,
        dy: f64,
        flags: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        unsafe { umoja_remote_pointer_scroll(h, dx, dy, 0, 0, flags & AXIS_FINISH != 0) };
        Ok(())
    }

    async fn notify_pointer_axis_discrete(
        &self,
        axis: u32,
        steps: i32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        let (sx, sy) = match axis {
            0 => (0, steps),
            1 => (steps, 0),
            _ => return Err(fdo::Error::InvalidArgs(format!("Unknown axis {axis}"))),
        };
        unsafe { umoja_remote_pointer_scroll(h, 0.0, 0.0, sx, sy, false) };
        Ok(())
    }

    async fn notify_pointer_motion_relative(
        &self,
        dx: f64,
        dy: f64,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        unsafe { umoja_remote_pointer_motion(h, dx, dy) };
        Ok(())
    }

    async fn notify_pointer_motion_absolute(
        &self,
        stream: String,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        let (lx, ly) = stream_position(&stream, x, y)?;
        unsafe { umoja_remote_pointer_absolute(h, lx, ly) };
        Ok(())
    }

    async fn notify_touch_down(
        &self,
        stream: String,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        let (lx, ly) = stream_position(&stream, x, y)?;
        unsafe { umoja_remote_touch(h, TOUCH_DOWN, slot, lx, ly) };
        Ok(())
    }

    async fn notify_touch_motion(
        &self,
        stream: String,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        let (lx, ly) = stream_position(&stream, x, y)?;
        unsafe { umoja_remote_touch(h, TOUCH_MOTION, slot, lx, ly) };
        Ok(())
    }

    async fn notify_touch_up(
        &self,
        slot: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        unsafe { umoja_remote_touch(h, TOUCH_UP, slot, 0.0, 0.0) };
        Ok(())
    }

    /// A libei connection for this session's input (the RemoteDesktop
    /// portal's ConnectToEIS); options are mutter's (none needed here).
    async fn connect_to_eis(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<OwnedFd> {
        self.check(&header)?;
        let h = input_of(&self.id)?;
        let (eis, fd) = crate::eis::connect(h).map_err(|e| failed(format!("EIS: {e}")))?;
        match sessions().get_mut(&self.id) {
            Some(s) => s.eis.push(eis),
            None => {
                crate::eis::disconnect(eis);
                return Err(failed("Session closed"));
            }
        }
        Ok(fd)
    }

    #[zbus(property)]
    fn caps_lock_state(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn num_lock_state(&self) -> bool {
        false
    }
}

/// End a remote desktop session and the screen casts it drives.
pub async fn close(id: &str) {
    let Some(session) = sessions().remove(id) else {
        return;
    };
    for eis in &session.eis {
        crate::eis::disconnect(*eis);
    }
    if session.input != 0 {
        let input = session.input;
        let _ =
            tokio::task::spawn_blocking(move || unsafe { umoja_remote_input_free(input) }).await;
    }
    for cast in crate::screencast::sessions_of_remote_desktop(id) {
        crate::screencast::close_session(&cast).await;
    }
    tracing::info!(session = %session.path, "remote desktop session closed");
    let Some(conn) = CONNECTION.get() else {
        return;
    };
    if let Ok(emitter) = SignalEmitter::new(conn, session.path.as_str()) {
        let _ = RemoteSession::closed(&emitter).await;
    }
    let _ = conn
        .object_server()
        .remove::<RemoteSession, _>(session.path.as_str())
        .await;
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
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            close(&id).await;
        }
    }
}

/// gnome-shell and mutter's sharing services on one connection: screen
/// casts, remote desktop, input capture and the window list the portal's
/// picker shows. Not on the login screen.
pub fn start() {
    RUNTIME.spawn(async move {
        let built = async {
            let conn = zbus::connection::Builder::session()?.build().await?;
            crate::screencast::serve(&conn).await?;
            crate::introspect::serve(&conn).await?;
            crate::input_capture::serve(&conn).await?;
            conn.object_server().at(OBJECT_PATH, RemoteDesktop).await?;
            let _ = CONNECTION.set(conn.clone());
            RUNTIME.spawn(watch_owners(conn.clone()));
            // Names last, once everything is there to answer.
            for name in [
                crate::screencast::BUS_NAME,
                BUS_NAME,
                crate::input_capture::BUS_NAME,
                crate::introspect::BUS_NAME,
            ] {
                conn.request_name(name).await?;
                crate::session::keep_name(conn.clone(), name);
            }
            zbus::Result::Ok(conn)
        }
        .await;
        match built {
            Ok(conn) => std::mem::forget(conn),
            Err(e) => tracing::warn!(error = %e, "screen sharing services unavailable"),
        }
    });
}

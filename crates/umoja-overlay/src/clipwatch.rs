//! Clipboard and primary-selection watcher.
//!
//! Uses `ext_data_control_v1`, which umoja-mir only offers to Umoja's own
//! shell binaries, so no ordinary app can read the clipboard in the
//! background. It runs on its own Wayland connection and thread and reports
//! every new text selection to the GTK side over a channel.
//!
//! Selections that password managers mark as secret
//! (`x-kde-passwordManagerHint`) are never read.

use async_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};

/// Largest selection we keep, in bytes.
const MAX_BYTES: usize = 256 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub enum ClipEvent {
    /// Ctrl+C / Copy.
    Clipboard(String),
    /// Text the user selected (middle-click paste buffer); empty when the
    /// selection went away.
    Primary(String),
}

struct State {
    conn: Connection,
    /// Mime types announced for each live offer.
    offers: HashMap<ObjectId, Vec<String>>,
    tx: Sender<ClipEvent>,
}

/// Start watching. `None` when the compositor doesn't let us (no
/// `ext_data_control_manager_v1`, e.g. another compositor or an old Mir).
pub fn start() -> Option<Receiver<ClipEvent>> {
    let conn = Connection::connect_to_env()
        .map_err(|e| tracing::warn!(error = %e, "clipboard watcher: no wayland"))
        .ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();
    let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) else {
        tracing::warn!(
            "compositor does not offer ext_data_control_manager_v1; no clipboard history"
        );
        return None;
    };
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=7, ()).ok()?;
    let _device = manager.get_data_device(&seat, &qh, ());
    let (tx, rx) = async_channel::unbounded();
    let mut state = State {
        conn: conn.clone(),
        offers: HashMap::new(),
        tx,
    };
    std::thread::Builder::new()
        .name("umoja-clipboard".into())
        .spawn(move || {
            let _keep = (manager, seat, _device);
            while !state.tx.is_closed() {
                if queue.blocking_dispatch(&mut state).is_err() {
                    break;
                }
            }
            tracing::warn!("clipboard watcher stopped");
        })
        .ok()?;
    Some(rx)
}

/// The text mime type to ask for, or `None` for non-text or secret data.
pub fn pick_mime(mimes: &[String]) -> Option<&str> {
    const SECRET: &[&str] = &[
        "x-kde-passwordManagerHint",
        "application/x-nspasteboard-concealed-type",
    ];
    if mimes.iter().any(|m| SECRET.contains(&m.as_str())) {
        return None;
    }
    const TEXT: &[&str] = &[
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "TEXT",
        "STRING",
    ];
    TEXT.iter()
        .find(|t| mimes.iter().any(|m| m.eq_ignore_ascii_case(t)))
        .copied()
}

impl State {
    fn read_offer(&mut self, offer: &ExtDataControlOfferV1) -> Option<String> {
        let mimes = self.offers.get(&offer.id())?;
        let mime = pick_mime(mimes)?.to_string();
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return None;
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        offer.receive(mime, write.as_fd());
        let _ = self.conn.flush();
        drop(write); // the source holds its own copy now
        read_with_timeout(read).filter(|t| !t.trim().is_empty())
    }

    fn forget(&mut self, offer: Option<&ExtDataControlOfferV1>) {
        if let Some(o) = offer {
            self.offers.remove(&o.id());
            o.destroy();
        }
    }
}

fn read_with_timeout(fd: OwnedFd) -> Option<String> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut p = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_BYTES {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    out.truncate(MAX_BYTES);
    (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => {
                if let Some(text) = id.as_ref().and_then(|o| state.read_offer(o)) {
                    let _ = state.tx.send_blocking(ClipEvent::Clipboard(text));
                }
                state.forget(id.as_ref());
            }
            Event::PrimarySelection { id } => {
                // Nothing selected any more (or a secret): clear it.
                let text = id
                    .as_ref()
                    .and_then(|o| state.read_offer(o))
                    .unwrap_or_default();
                let _ = state.tx.send_blocking(ClipEvent::Primary(text));
                state.forget(id.as_ref());
            }
            Event::Finished => {
                state.tx.close();
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::pick_mime;

    fn v(m: &[&str]) -> Vec<String> {
        m.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prefers_utf8_text() {
        assert_eq!(
            pick_mime(&v(&["text/html", "STRING", "text/plain;charset=utf-8"])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(pick_mime(&v(&["UTF8_STRING", "TEXT"])), Some("UTF8_STRING"));
    }

    #[test]
    fn skips_images_and_secrets() {
        assert_eq!(pick_mime(&v(&["image/png"])), None);
        assert_eq!(
            pick_mime(&v(&["text/plain", "x-kde-passwordManagerHint"])),
            None
        );
    }
}

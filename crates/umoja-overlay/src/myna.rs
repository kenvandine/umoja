//! Myna dictation, for the search box's microphone button.
//!
//! The myna snap's daemon owns `com.canonical.Myna.Dictation` on the
//! session bus. It types what it hears into the focused text field itself
//! (through IBus), so while the overlay is open the transcript lands in the
//! search box like anywhere else; this module only follows whether the
//! daemon is there and whether it is recording, and calls `Toggle`. The top
//! bar's indicator and the HUD pill live in umoja-panel.

use gio::prelude::*;
use umoja_common::dbus;

#[derive(Debug, Clone, PartialEq)]
pub enum MynaEvent {
    Listening(bool),
    Available(bool),
}

pub struct Myna {
    pub events: async_channel::Receiver<MynaEvent>,
    proxy: std::rc::Rc<std::cell::RefCell<Option<gio::DBusProxy>>>,
}

impl Myna {
    /// Follow the daemon in the background (must run on the GTK thread).
    pub fn start() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let keep = slot.clone();
        glib::spawn_future_local(async move {
            let proxy = match gio::DBusProxy::for_bus_future(
                gio::BusType::Session,
                gio::DBusProxyFlags::DO_NOT_AUTO_START
                    | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                None,
                dbus::MYNA_BUS_NAME,
                dbus::MYNA_OBJECT_PATH,
                dbus::MYNA_INTERFACE,
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "myna: no session bus proxy");
                    return;
                }
            };
            let send = move |p: &gio::DBusProxy| {
                let _ = tx.try_send(MynaEvent::Available(p.name_owner().is_some()));
                let _ = tx.try_send(MynaEvent::Listening(listening(p)));
            };
            send(&proxy);
            let s = send.clone();
            // gio-rs doesn't bind this signal; connect it by name.
            proxy.connect_local("g-properties-changed", false, move |args| {
                if let Some(Ok(p)) = args.first().map(|v| v.get::<gio::DBusProxy>()) {
                    s(&p);
                }
                None
            });
            proxy.connect_notify_local(Some("g-name-owner"), move |p, _| send(p));
            *keep.borrow_mut() = Some(proxy);
        });
        Self {
            events: rx,
            proxy: slot,
        }
    }

    pub fn toggle(&self) {
        let Some(proxy) = self.proxy.borrow().clone() else {
            tracing::warn!("myna is not available");
            return;
        };
        proxy.call(
            "Toggle",
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            |r| {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "myna is not available");
                }
            },
        );
    }
}

fn listening(p: &gio::DBusProxy) -> bool {
    p.name_owner().is_some()
        && p.cached_property("State")
            .and_then(|v| v.str().map(|s| s == "recording" || s == "active"))
            .unwrap_or(false)
}

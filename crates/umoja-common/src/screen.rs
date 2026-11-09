//! Wait for the screen to be on before mapping shell surfaces.
//!
//! While umoja-mir has the display powered off (idle blank), Mir's window
//! management has no active outputs. Shell clients that start or restart
//! then (a crash, `systemctl --user restart umoja-panel`) had their layer
//! surfaces placed against no output: the panel was killed with protocol
//! errors and, on 2026-11-09, Mir's Wayland thread locked up for good. So
//! umoja-panel and umoja-dock build their windows only once
//! `org.freedesktop.ScreenSaver.GetActive` (served by umoja-mir, true while
//! the screen is blanked) is false.

use gio::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const NAME: &str = "org.freedesktop.ScreenSaver";
const PATH: &str = "/org/freedesktop/ScreenSaver";
const IFACE: &str = "org.freedesktop.ScreenSaver";
/// Backstop for a compositor that doesn't send `ActiveChanged`.
const POLL_SECONDS: u32 = 2;

/// Is the screen blanked right now? No answer (no compositor service, a
/// test compositor, an old umoja-mir) counts as on.
async fn blanked(conn: &gio::DBusConnection) -> bool {
    let reply = conn
        .call_future(
            Some(NAME),
            PATH,
            IFACE,
            "GetActive",
            None,
            Some(glib::VariantTy::new("(b)").unwrap()),
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
        )
        .await;
    matches!(reply.ok().and_then(|v| v.get::<(bool,)>()), Some((true,)))
}

struct Waiter {
    then: Option<Box<dyn FnOnce()>>,
    hold: Option<gio::ApplicationHoldGuard>,
    conn: gio::DBusConnection,
    sub: Option<gio::SignalSubscriptionId>,
    poll: Option<glib::SourceId>,
}

/// Run `then` (once) and stop listening.
fn finish(w: &Rc<RefCell<Waiter>>) {
    let (then, sub, poll, conn) = {
        let mut w = w.borrow_mut();
        let Some(then) = w.then.take() else {
            return;
        };
        (then, w.sub.take(), w.poll.take(), w.conn.clone())
    };
    if let Some(sub) = sub {
        #[allow(deprecated)]
        conn.signal_unsubscribe(sub);
    }
    if let Some(poll) = poll {
        poll.remove();
    }
    tracing::info!("screen is on; mapping");
    then();
    w.borrow_mut().hold = None;
}

/// Run `then` once the screen is on: straight away when it already is,
/// otherwise when umoja-mir reports it woke up. `app` is held meanwhile so
/// it doesn't quit for want of a window.
pub fn when_on<A, F>(app: &A, then: F)
where
    A: IsA<gio::Application>,
    F: FnOnce() + 'static,
{
    let hold = app.hold();
    glib::spawn_future_local(async move {
        let conn = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(c) => c,
            Err(_) => {
                then();
                drop(hold);
                return;
            }
        };
        if !blanked(&conn).await {
            then();
            drop(hold);
            return;
        }
        tracing::info!("screen is off; waiting for it to come on before mapping");
        let w = Rc::new(RefCell::new(Waiter {
            then: Some(Box::new(then)),
            hold: Some(hold),
            conn: conn.clone(),
            sub: None,
            poll: None,
        }));
        let weak = Rc::downgrade(&w);
        #[allow(deprecated)]
        let sub = conn.signal_subscribe(
            Some(NAME),
            Some(IFACE),
            Some("ActiveChanged"),
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let (Some((false,)), Some(w)) = (params.get::<(bool,)>(), weak.upgrade()) {
                    finish(&w);
                }
            },
        );
        // The poll holds the waiter until finish() removes it.
        let strong = w.clone();
        let poll = glib::timeout_add_seconds_local(POLL_SECONDS, move || {
            let w = strong.clone();
            let conn = w.borrow().conn.clone();
            glib::spawn_future_local(async move {
                if !blanked(&conn).await {
                    finish(&w);
                }
            });
            glib::ControlFlow::Continue
        });
        let mut b = w.borrow_mut();
        b.sub = Some(sub);
        b.poll = Some(poll);
    });
}

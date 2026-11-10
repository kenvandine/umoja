//! Running-window tracking through `zwlr_foreign_toplevel_management_v1`.
//!
//! GTK owns its own Wayland connection, so the tracker opens a second one on
//! a helper thread and forwards events to the GTK main loop over a channel.
//! Commands (activate, close) travel the other way.

use async_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

pub type WindowId = u32;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub activated: bool,
    pub minimized: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Changed(WindowInfo),
    Closed(WindowId),
}

#[derive(Debug, Clone, Copy)]
pub enum Command {
    Activate(WindowId),
    Minimize(WindowId),
    Close(WindowId),
}

pub struct Tracker {
    pub events: Receiver<Event>,
    pub commands: Commands,
}

/// Sends commands to the tracker thread and wakes it, so it can sleep in
/// poll() until the compositor or the dock has something for it (it used to
/// wake every 50 ms to look for commands).
#[derive(Clone)]
pub struct Commands {
    tx: Sender<Command>,
    wake: Arc<OwnedFd>,
}

impl Commands {
    pub fn try_send(&self, cmd: Command) {
        if self.tx.try_send(cmd).is_ok() {
            let one: u64 = 1;
            // SAFETY: an eventfd takes 8-byte writes from a valid buffer.
            unsafe {
                libc::write(
                    self.wake.as_raw_fd(),
                    (&one as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
        }
    }
}

struct State {
    seat: Option<WlSeat>,
    handles: HashMap<WindowId, (ZwlrForeignToplevelHandleV1, WindowInfo)>,
    events: Sender<Event>,
}

/// Start tracking. Returns `None` if the compositor lacks the protocol.
pub fn start() -> Option<Tracker> {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let conn = Connection::connect_to_env()
        .map_err(|e| tracing::warn!(error = %e, "no wayland"))
        .ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();
    if globals
        .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
        .is_err()
    {
        tracing::warn!(
            "compositor does not offer zwlr_foreign_toplevel_manager_v1; running dots disabled"
        );
        return None;
    }
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=7, ()).ok();
    // SAFETY: plain syscall; the fd is owned from here on.
    let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake < 0 {
        tracing::warn!("no eventfd; running dots disabled");
        return None;
    }
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(wake) });
    let thread_wake = wake.clone();
    let mut state = State {
        seat,
        handles: HashMap::new(),
        events: ev_tx,
    };

    std::thread::Builder::new()
        .name("umoja-dock-tracker".into())
        .spawn(move || {
            loop {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    state.apply(cmd);
                }
                if let Err(e) = queue.flush() {
                    tracing::warn!(error = %e, "tracker flush failed");
                    break;
                }
                if let Some(guard) = queue.prepare_read() {
                    let mut fds = [
                        libc::pollfd {
                            fd: guard.connection_fd().as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: thread_wake.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    // Sleep until the compositor sends events or the dock
                    // queues a command.
                    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                    if fds[1].revents & libc::POLLIN != 0 {
                        let mut n: u64 = 0;
                        // SAFETY: reading the 8-byte counter clears it.
                        unsafe {
                            libc::read(
                                thread_wake.as_raw_fd(),
                                (&mut n as *mut u64).cast(),
                                std::mem::size_of::<u64>(),
                            )
                        };
                    }
                    if ready > 0
                        && fds[0].revents & libc::POLLIN != 0
                        && let Err(e) = guard.read()
                    {
                        tracing::warn!(error = %e, "tracker read failed");
                        break;
                    }
                }
                if let Err(e) = queue.dispatch_pending(&mut state) {
                    tracing::warn!(error = %e, "tracker dispatch failed");
                    break;
                }
                if state.events.is_closed() {
                    tracing::warn!("tracker event channel closed");
                    break;
                }
            }
            tracing::warn!("window tracker stopped");
        })
        .ok()?;

    Some(Tracker {
        events: ev_rx,
        commands: Commands { tx: cmd_tx, wake },
    })
}

impl State {
    fn apply(&mut self, cmd: Command) {
        let id = match cmd {
            Command::Activate(id) | Command::Minimize(id) | Command::Close(id) => id,
        };
        let Some((handle, info)) = self.handles.get(&id) else {
            return;
        };
        match cmd {
            Command::Activate(_) => {
                if info.minimized {
                    handle.unset_minimized();
                }
                if let Some(seat) = &self.seat {
                    handle.activate(seat);
                }
            }
            Command::Minimize(_) => handle.set_minimized(),
            Command::Close(_) => handle.close(),
        }
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

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            let id = toplevel.id().protocol_id();
            let info = WindowInfo {
                id,
                ..Default::default()
            };
            state.handles.insert(id, (toplevel, info));
        }
    }

    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::{Event as E, State as S};
        let id = handle.id().protocol_id();
        let Some((_, info)) = state.handles.get_mut(&id) else {
            return;
        };
        match event {
            E::Title { title } => info.title = title,
            E::AppId { app_id } => info.app_id = app_id,
            E::State { state: raw } => {
                let states: Vec<u32> = raw
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                info.activated = states.contains(&(S::Activated as u32));
                info.minimized = states.contains(&(S::Minimized as u32));
            }
            E::Done => {
                let _ = state.events.try_send(Event::Changed(info.clone()));
            }
            E::Closed => {
                if let Some((h, _)) = state.handles.remove(&id) {
                    h.destroy();
                }
                let _ = state.events.try_send(Event::Closed(id));
            }
            _ => {}
        }
    }
}

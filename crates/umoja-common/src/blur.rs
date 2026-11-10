//! Compositor blur behind shell surfaces (`ext-background-effect-v1`).
//!
//! The panel, its menus, notification bubbles and the OSD, the Power Off
//! and PolicyKit dialogs, Alt+Tab, and the dock and its menus are slightly
//! translucent. Design rule: any shell surface that shows text on a
//! see-through background blurs what is behind it. [`blur_behind`] asks the compositor
//! to blur whatever is behind such a window or popover, so it reads as
//! frosted glass. umoja-mir's patched Mir does the blurring (see
//! `packaging/mir/patches/3004_background-blur.patch`): it blurs only where
//! the surface itself is mostly opaque, so the region can simply be "the
//! whole surface" and rounded corners and drop shadows still come out right.
//!
//! On a compositor without the protocol this does nothing.
//!
//! GTK 4.24 and newer speak the protocol themselves: they make an effect
//! object for every surface and set its region from CSS `backdrop-filter`
//! (see panel.css and dock.css). A second object on the same surface is a
//! protocol error that gets the client disconnected, so there we leave it
//! to GTK and do nothing.

use gdk4_wayland::prelude::*;
use gtk4 as gtk;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

/// Bigger than any surface; the compositor clips the region to the surface.
const WHOLE_SURFACE: i32 = 1 << 20;

/// Blur the background behind `native` (a window or popover) whenever it is
/// on screen. Call once, before or after it is first shown.
pub fn blur_behind(native: &(impl IsA<gtk::Native> + IsA<gtk::Widget>)) {
    if gtk_does_blur() {
        return;
    }
    let widget = native.upcast_ref::<gtk::Widget>();
    let applied: Rc<RefCell<Option<Applied>>> = Rc::default();
    {
        let applied = applied.clone();
        widget.connect_map(move |w| apply(w, &applied));
    }
    {
        let applied = applied.clone();
        widget.connect_unrealize(move |_| {
            if let Some(old) = applied.borrow_mut().take() {
                old.effect.destroy();
                flush();
            }
        });
    }
    if widget.is_mapped() {
        apply(widget, &applied);
    }
}

/// Whether this GTK manages ext-background-effect-v1 itself.
fn gtk_does_blur() -> bool {
    (gtk::major_version(), gtk::minor_version()) >= (4, 24)
}

/// The effect object for the surface it was made for. GTK may give a native
/// a new wl_surface when it is realized again.
struct Applied {
    surface: ObjectId,
    effect: ExtBackgroundEffectSurfaceV1,
}

fn apply(widget: &gtk::Widget, applied: &Rc<RefCell<Option<Applied>>>) {
    let Some(native) = widget.native() else {
        return;
    };
    let Some(surface) = native
        .surface()
        .and_then(|s| s.downcast::<gdk4_wayland::WaylandSurface>().ok())
    else {
        return;
    };
    let Some(wl_surface) = surface.wl_surface() else {
        return;
    };
    let mut applied = applied.borrow_mut();
    if applied
        .as_ref()
        .is_some_and(|a| a.surface == wl_surface.id())
    {
        return;
    }
    CLIENT.with(|cell| {
        let mut cell = cell.borrow_mut();
        let client = match &mut *cell {
            Some(client) => client,
            slot @ None => {
                *slot = Some(Client::new(&wl_surface));
                slot.as_mut().unwrap()
            }
        };
        let Some(globals) = &client.globals else {
            return;
        };
        if let Some(old) = applied.take() {
            old.effect.destroy();
        }
        let qh = &client.handle;
        let effect = globals.manager.get_background_effect(&wl_surface, qh, ());
        let region = globals.compositor.create_region(qh, ());
        region.add(0, 0, WHOLE_SURFACE, WHOLE_SURFACE);
        effect.set_blur_region(Some(&region));
        region.destroy();
        // Takes effect with GTK's next commit of the surface.
        let _ = client.queue.dispatch_pending(&mut State);
        let _ = client.conn.flush();
        *applied = Some(Applied {
            surface: wl_surface.id(),
            effect,
        });
    });
}

fn flush() {
    CLIENT.with(|cell| {
        if let Some(client) = &*cell.borrow() {
            let _ = client.conn.flush();
        }
    });
}

thread_local! {
    // GTK and its surfaces live on the main thread, and so does this.
    static CLIENT: RefCell<Option<Client>> = const { RefCell::new(None) };
}

/// Our own event queue on GTK's Wayland connection, and the globals bound
/// on it. `globals` is `None` when the compositor has no blur.
struct Client {
    conn: Connection,
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    globals: Option<Globals>,
}

struct Globals {
    manager: ExtBackgroundEffectManagerV1,
    compositor: WlCompositor,
}

impl Client {
    fn new(surface: &wayland_client::protocol::wl_surface::WlSurface) -> Self {
        let conn = surface
            .backend()
            .upgrade()
            .map(Connection::from_backend)
            .expect("GTK's Wayland connection is alive while its surface is");
        let (globals, queue) = match registry_queue_init::<State>(&conn) {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!(error = %e, "background blur: no registry");
                let queue = conn.new_event_queue();
                let handle = queue.handle();
                return Self {
                    conn,
                    queue,
                    handle,
                    globals: None,
                };
            }
        };
        let handle = queue.handle();
        let bound = (|| {
            Some(Globals {
                manager: globals.bind(&handle, 1..=1, ()).ok()?,
                compositor: globals.bind(&handle, 1..=4, ()).ok()?,
            })
        })();
        if bound.is_none() {
            tracing::info!("background blur: compositor has no ext_background_effect_manager_v1");
        }
        Self {
            conn,
            queue,
            handle,
            globals: bound,
        }
    }
}

struct State;

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

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: <WlRegion as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        _: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `capabilities`: Mir always offers blur, so there is nothing to track.
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectSurfaceV1,
        _: ext_background_effect_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

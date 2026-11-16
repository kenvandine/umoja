// xdg-dialog-v1, keyboard-shortcuts-inhibit-unstable-v1 and
// xwayland-keyboard-grab-unstable-v1 (wm_protocols.h).
//
// Requests arrive on Mir's Wayland thread; the window manager asks from
// its own threads under the WM lock. Everything shared sits behind one
// mutex, and Wayland objects are only touched on the Wayland thread.

#include "wm_protocols.h"

#include "keyboard-shortcuts-inhibit-unstable-v1-server-protocol.h"
#include "surface_effects.h"
#include "umoja_shim.h"
#include "xwayland-keyboard-grab-unstable-v1-server-protocol.h"
#include "xdg-dialog-v1-server-protocol.h"

#include <wayland-server-core.h>

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <unistd.h>
#include <memory>
#include <mutex>
#include <vector>

namespace umoja_wmp {
namespace {

std::mutex mutex;
miral::WaylandExtensions::Context const* context = nullptr;

// ---- xdg-dialog-v1 ----------------------------------------------------------

struct Dialog {
    wl_resource* resource = nullptr; // xdg_dialog_v1
    wl_resource* toplevel = nullptr; // the client's xdg_toplevel, until it goes
    miral::Window window;            // once Mir has made it
    bool modal = false;
    wl_listener toplevel_destroyed{};
};
std::vector<Dialog*> dialogs;

void forget_toplevel(Dialog* d) {
    if (!d->toplevel) return;
    wl_list_remove(&d->toplevel_destroyed.link);
    d->toplevel = nullptr;
}

void on_dialog_toplevel_destroyed(wl_listener* listener, void*) {
    Dialog* d = wl_container_of(listener, d, toplevel_destroyed);
    std::lock_guard lock{mutex};
    wl_list_remove(&d->toplevel_destroyed.link);
    d->toplevel = nullptr;
    d->window = {};
}

void dialog_resource_destroyed(wl_resource* resource) {
    auto* d = static_cast<Dialog*>(wl_resource_get_user_data(resource));
    if (!d) return;
    std::lock_guard lock{mutex};
    forget_toplevel(d);
    dialogs.erase(std::remove(dialogs.begin(), dialogs.end(), d), dialogs.end());
    delete d;
}

void resolve(Dialog* d) {
    if (!d->window && d->toplevel) d->window = miral::window_for(d->toplevel);
}

void dialog_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void dialog_set_modal(wl_client*, wl_resource* resource) {
    auto* d = static_cast<Dialog*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    d->modal = true;
    resolve(d);
}

void dialog_unset_modal(wl_client*, wl_resource* resource) {
    auto* d = static_cast<Dialog*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    d->modal = false;
}

struct xdg_dialog_v1_interface const dialog_impl = {
    dialog_destroy,
    dialog_set_modal,
    dialog_unset_modal,
};

void wm_dialog_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void wm_dialog_get(wl_client* client, wl_resource* wm, uint32_t id, wl_resource* toplevel) {
    std::lock_guard lock{mutex};
    for (auto* d : dialogs)
        if (d->toplevel == toplevel) {
            wl_resource_post_error(wm, XDG_WM_DIALOG_V1_ERROR_ALREADY_USED,
                                   "the xdg_toplevel already has a dialog object");
            return;
        }
    auto* res = wl_resource_create(client, &xdg_dialog_v1_interface, wl_resource_get_version(wm), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    auto* d = new Dialog;
    d->resource = res;
    d->toplevel = toplevel;
    d->toplevel_destroyed.notify = on_dialog_toplevel_destroyed;
    wl_resource_add_destroy_listener(toplevel, &d->toplevel_destroyed);
    dialogs.push_back(d);
    wl_resource_set_implementation(res, &dialog_impl, d, dialog_resource_destroyed);
}

struct xdg_wm_dialog_v1_interface const wm_dialog_impl = {
    wm_dialog_destroy,
    wm_dialog_get,
};

void bind_wm_dialog(wl_client* client, void*, uint32_t version, uint32_t id) {
    auto* res = wl_resource_create(client, &xdg_wm_dialog_v1_interface, static_cast<int>(version), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    wl_resource_set_implementation(res, &wm_dialog_impl, nullptr, nullptr);
}

// ---- keyboard-shortcuts-inhibit ----------------------------------------------

struct Inhibitor {
    wl_resource* resource = nullptr; // zwp_keyboard_shortcuts_inhibitor_v1
    wl_resource* surface = nullptr;  // until the wl_surface goes
    miral::Window window;
    bool active = false;
    bool revoked = false; // Super+Escape took it back
    // An X11 app's keyboard grab passed on by Xwayland: no active/inactive
    // events, and only apps the grab rules allow (umoja_keyboard_grab_allowed).
    bool xwayland = false;
    wl_listener surface_destroyed{};
};
std::vector<Inhibitor*> inhibitors;

void forget_surface(Inhibitor* i) {
    if (!i->surface) return;
    wl_list_remove(&i->surface_destroyed.link);
    i->surface = nullptr;
}

void on_inhibited_surface_destroyed(wl_listener* listener, void*) {
    Inhibitor* i = wl_container_of(listener, i, surface_destroyed);
    std::lock_guard lock{mutex};
    wl_list_remove(&i->surface_destroyed.link);
    i->surface = nullptr;
    i->window = {};
    i->active = false;
}

void inhibitor_resource_destroyed(wl_resource* resource) {
    auto* i = static_cast<Inhibitor*>(wl_resource_get_user_data(resource));
    if (!i) return;
    std::lock_guard lock{mutex};
    forget_surface(i);
    inhibitors.erase(std::remove(inhibitors.begin(), inhibitors.end(), i), inhibitors.end());
    delete i;
}

// GNOME asks the user once per app. Umoja has no dialog: VM and remote
// desktop viewers get the keys (umoja_keyboard_grab_allowed), any other
// app never does (GTK asks on its own for keyboard grabs), and
// Super+Escape takes them back (restore_shortcuts).
// The window trace switch of umoja_shim.cpp ($XDG_RUNTIME_DIR/umoja-debug-windows).
bool tracing() {
    char const* dir = std::getenv("XDG_RUNTIME_DIR");
    return dir && access((std::string{dir} + "/umoja-debug-windows").c_str(), F_OK) == 0;
}

void activate(Inhibitor* i) {
    if (i->active || i->revoked || !i->surface) return;
    if (!i->window) i->window = miral::window_for(i->surface);
    if (!i->window) {
        if (tracing()) std::fprintf(stderr, "umoja-mir: wm: shortcuts inhibitor waits for its window\n");
        return;
    }
    std::string const app = umoja_fx::app_id_of(i->window);
    bool const allowed = umoja_keyboard_grab_allowed(app.c_str(), i->xwayland);
    if (tracing())
        std::fprintf(stderr, "umoja-mir: wm: %s for '%s' %s\n", i->xwayland ? "keyboard grab" : "shortcuts inhibitor",
                     app.c_str(), allowed ? "granted" : "denied");
    if (!allowed) {
        i->revoked = true; // not asked again for this inhibitor
        return;
    }
    i->active = true;
    if (!i->xwayland) zwp_keyboard_shortcuts_inhibitor_v1_send_active(i->resource);
}

void inhibitor_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

struct zwp_keyboard_shortcuts_inhibitor_v1_interface const inhibitor_impl = {
    inhibitor_destroy,
};

void manager_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void manager_inhibit(wl_client* client, wl_resource* manager, uint32_t id, wl_resource* surface, wl_resource*) {
    std::lock_guard lock{mutex};
    for (auto* i : inhibitors)
        if (i->surface == surface) {
            wl_resource_post_error(manager, ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1_ERROR_ALREADY_INHIBITED,
                                   "the surface already has a shortcuts inhibitor");
            return;
        }
    auto* res = wl_resource_create(client, &zwp_keyboard_shortcuts_inhibitor_v1_interface,
                                   wl_resource_get_version(manager), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    auto* i = new Inhibitor;
    i->resource = res;
    i->surface = surface;
    i->surface_destroyed.notify = on_inhibited_surface_destroyed;
    wl_resource_add_destroy_listener(surface, &i->surface_destroyed);
    inhibitors.push_back(i);
    wl_resource_set_implementation(res, &inhibitor_impl, i, inhibitor_resource_destroyed);
    activate(i);
}

struct zwp_keyboard_shortcuts_inhibit_manager_v1_interface const manager_impl = {
    manager_destroy,
    manager_inhibit,
};

// ---- xwayland-keyboard-grab --------------------------------------------------
// Xwayland asks for this when an X11 app grabs the keyboard (a VM viewer,
// a remote desktop): mutter then lets that window have every key, for the
// apps its xwayland-grab-access-rules allow. Umoja does the same through
// the shortcuts inhibitors above.

void grab_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

struct zwp_xwayland_keyboard_grab_v1_interface const grab_impl = {
    grab_destroy,
};

void grab_manager_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void grab_keyboard(wl_client* client, wl_resource* manager, uint32_t id, wl_resource* surface, wl_resource*) {
    auto* res =
        wl_resource_create(client, &zwp_xwayland_keyboard_grab_v1_interface, wl_resource_get_version(manager), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    std::lock_guard lock{mutex};
    auto* i = new Inhibitor;
    i->resource = res;
    i->surface = surface;
    i->xwayland = true;
    i->surface_destroyed.notify = on_inhibited_surface_destroyed;
    wl_resource_add_destroy_listener(surface, &i->surface_destroyed);
    inhibitors.push_back(i);
    wl_resource_set_implementation(res, &grab_impl, i, inhibitor_resource_destroyed);
    activate(i);
}

struct zwp_xwayland_keyboard_grab_manager_v1_interface const grab_manager_impl = {
    grab_manager_destroy,
    grab_keyboard,
};

void bind_grab_manager(wl_client* client, void*, uint32_t version, uint32_t id) {
    auto* res = wl_resource_create(client, &zwp_xwayland_keyboard_grab_manager_v1_interface,
                                   static_cast<int>(version), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    wl_resource_set_implementation(res, &grab_manager_impl, nullptr, nullptr);
}

void bind_manager(wl_client* client, void*, uint32_t version, uint32_t id) {
    auto* res =
        wl_resource_create(client, &zwp_keyboard_shortcuts_inhibit_manager_v1_interface, static_cast<int>(version), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    wl_resource_set_implementation(res, &manager_impl, nullptr, nullptr);
}

std::shared_ptr<void> global(wl_global* g) {
    return std::shared_ptr<void>{g, [](void* p) {
                                     if (p) wl_global_destroy(static_cast<wl_global*>(p));
                                 }};
}

} // namespace

auto dialog_extension() -> miral::WaylandExtensions::Builder {
    return miral::WaylandExtensions::Builder{
        xdg_wm_dialog_v1_interface.name, [](miral::WaylandExtensions::Context const* ctx) -> std::shared_ptr<void> {
            {
                std::lock_guard lock{mutex};
                context = ctx;
            }
            return global(wl_global_create(ctx->display(), &xdg_wm_dialog_v1_interface, 1, nullptr, bind_wm_dialog));
        }};
}

auto shortcuts_inhibit_extension() -> miral::WaylandExtensions::Builder {
    return miral::WaylandExtensions::Builder{
        zwp_keyboard_shortcuts_inhibit_manager_v1_interface.name,
        [](miral::WaylandExtensions::Context const* ctx) -> std::shared_ptr<void> {
            {
                std::lock_guard lock{mutex};
                context = ctx;
            }
            return global(wl_global_create(ctx->display(), &zwp_keyboard_shortcuts_inhibit_manager_v1_interface, 1,
                                           nullptr, bind_manager));
        }};
}

auto xwayland_grab_extension() -> miral::WaylandExtensions::Builder {
    return miral::WaylandExtensions::Builder{
        zwp_xwayland_keyboard_grab_manager_v1_interface.name,
        [](miral::WaylandExtensions::Context const* ctx) -> std::shared_ptr<void> {
            {
                std::lock_guard lock{mutex};
                context = ctx;
            }
            return global(wl_global_create(ctx->display(), &zwp_xwayland_keyboard_grab_manager_v1_interface, 1,
                                           nullptr, bind_grab_manager));
        }};
}

void window_created() {
    miral::WaylandExtensions::Context const* ctx = nullptr;
    {
        std::lock_guard lock{mutex};
        bool pending = false;
        for (auto* d : dialogs) pending |= d->toplevel && !d->window;
        for (auto* i : inhibitors) pending |= i->surface && !i->active && !i->revoked;
        if (pending) ctx = context;
    }
    if (!ctx) return;
    // window_for() reads Wayland objects: on the Wayland thread only.
    ctx->run_on_wayland_mainloop([] {
        std::lock_guard lock{mutex};
        for (auto* d : dialogs) resolve(d);
        for (auto* i : inhibitors) activate(i);
    });
}

bool is_modal(miral::Window const& window) {
    if (!window) return false;
    std::lock_guard lock{mutex};
    return std::any_of(dialogs.begin(), dialogs.end(), [&](Dialog* d) { return d->modal && d->window == window; });
}

bool inhibits_shortcuts(miral::Window const& window) {
    if (!window) return false;
    std::lock_guard lock{mutex};
    return std::any_of(inhibitors.begin(), inhibitors.end(),
                       [&](Inhibitor* i) { return i->active && i->window == window; });
}

void restore_shortcuts(miral::Window const& window) {
    miral::WaylandExtensions::Context const* ctx = nullptr;
    {
        std::lock_guard lock{mutex};
        for (auto* i : inhibitors)
            if (i->active && i->window == window) {
                // Taken back now, so the very next key is the shell's
                // again; the event goes out on the Wayland thread.
                i->active = false;
                i->revoked = true;
                ctx = context;
            }
    }
    if (!ctx) return;
    auto const target = window;
    ctx->run_on_wayland_mainloop([target] {
        std::lock_guard lock{mutex};
        for (auto* i : inhibitors)
            if (i->revoked && !i->xwayland && i->window == target && i->resource)
                zwp_keyboard_shortcuts_inhibitor_v1_send_inactive(i->resource);
    });
}

} // namespace umoja_wmp

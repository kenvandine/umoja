// xdg-session-management-v1: apps that opt in get their windows back where
// they left them, as on GNOME (mutter's meta-wayland-xdg-session*.c).
//
// The protocol objects live on Mir's Wayland thread (session_management.cpp);
// the window manager (umoja_shim.cpp) asks this module, under its own lock,
// what to restore for a new window and tells it how bound windows look now.

#pragma once

#include <miral/application.h>
#include <miral/wayland_extensions.h>
#include <miral/window.h>
#include <mir/geometry/rectangle.h>

#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <vector>

namespace umoja_sm {

// What mutter keeps per toplevel (MetaWaylandXdgToplevelState), plus
// umoja's tile layouts and the monitor by connector.
enum class WindowState : int { floating = 0, maximized = 1, fullscreen = 2, tiled = 3 };

struct SavedToplevel {
    WindowState state = WindowState::floating;
    uint32_t tile_action = 0;               // UMOJA_ACTION_TILE_* when tiled
    mir::geometry::Rectangle floating;      // the window's own (unmaximized, untiled) geometry
    bool minimized = false;
    int32_t workspace = -1;                 // -1: on every workspace / unknown
    std::string output;                     // connector of the monitor it was on
    mir::geometry::Rectangle output_rect;   // that monitor's layout rectangle then
};

// A toplevel the client announced (add_toplevel/restore_toplevel) before its
// first commit, handed to the window manager when Mir creates the window.
struct Pending {
    uint64_t token = 0;
    std::optional<SavedToplevel> restore;
};

// The Wayland global, for miral::WaylandExtensions::add_extension().
auto extension() -> miral::WaylandExtensions::Builder;

// ---- called by the window manager, under its lock -------------------------

// The next toplevel of `app` waiting for its window, if any.
auto take_pending(miral::Application const& app) -> std::optional<Pending>;

// Mir created the window for a token from take_pending().
void bind(uint64_t token, miral::Window const& window);

// Windows in a session, so the window manager can describe them.
auto bound_windows() -> std::vector<miral::Window>;

// Current state of a bound window.
void update(miral::Window const& window, SavedToplevel const& state);

// The window is gone; its last state stays in the session.
void window_closed(miral::Window const& window);

// How to describe every bound window: runs under the window manager's lock
// (it takes it itself), calling update() for each.
void set_snapshotter(std::function<void()> snapshot);

// Something about some window changed: snapshot and write the sessions out
// shortly (debounced, off the compositor's threads).
void changed();

} // namespace umoja_sm

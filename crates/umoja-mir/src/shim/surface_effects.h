// Things the window manager sets on a window's scene surface directly,
// because miral has no call for them: the xdg-shell tiled states, and the
// opacity and scale of window animations. Kept out of umoja_shim.cpp so
// Mir's server headers stay away from miral's toolkit API there.
#pragma once

#include <miral/window.h>

#include <memory>
#include <string>

namespace umoja_fx {

// Edge bits as MirTiledEdge: 1 north, 2 east, 4 south, 8 west.
void set_tiled_edges(miral::Window const& window, unsigned edges);

// Opacity (0..1) and scale about the window's centre (1 = as is).
void set_look(miral::Window const& window, float alpha, float scale);
// The same, and moved by (dx, dy) layout pixels (minimize into the dock).
void set_look_at(miral::Window const& window, float alpha, float scale, float dx, float dy);

// The close animation. Called as a window is deleted (advise_delete_window,
// before Mir removes its surface): Mir patch 3027 keeps drawing the surface
// while it fades and shrinks out. False if there is no surface.
bool begin_close(miral::Window const& window, double ms);
// One frame of every close animation; true while any is still running.
bool step_closing();

// Whether two Windows are the same window. miral::Window's == compares its
// private handle, and miral::window_for() makes a new one each time, so a
// Window from window_for() never equals the window manager's own: compare
// the scene surfaces instead.
inline bool same_window(miral::Window const& a, miral::Window const& b) {
    std::shared_ptr<mir::scene::Surface> const sa = a, sb = b;
    return sa && sa == sb;
}

// The window's application id (for X11 windows, from WM_CLASS).
std::string app_id_of(miral::Window const& window);

} // namespace umoja_fx

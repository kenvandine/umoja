// Things the window manager sets on a window's scene surface directly,
// because miral has no call for them: the xdg-shell tiled states, and the
// opacity and scale of window animations. Kept out of umoja_shim.cpp so
// Mir's server headers stay away from miral's toolkit API there.
#pragma once

#include <miral/window.h>

namespace umoja_fx {

// Edge bits as MirTiledEdge: 1 north, 2 east, 4 south, 8 west.
void set_tiled_edges(miral::Window const& window, unsigned edges);

// Opacity (0..1) and scale about the window's centre (1 = as is).
void set_look(miral::Window const& window, float alpha, float scale);

// The close animation. Called as a window is deleted (advise_delete_window,
// before Mir removes its surface): Mir patch 3027 keeps drawing the surface
// while it fades and shrinks out. False if there is no surface.
bool begin_close(miral::Window const& window, double ms);
// One frame of every close animation; true while any is still running.
bool step_closing();

} // namespace umoja_fx

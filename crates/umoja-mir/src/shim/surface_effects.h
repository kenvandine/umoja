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

} // namespace umoja_fx

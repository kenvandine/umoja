// surface_effects.h: settings miral has no call for, made on the window's
// mir::scene::Surface.

#include "surface_effects.h"

#include <mir/scene/surface.h>

#include <glm/glm.hpp>
#include <glm/gtc/matrix_transform.hpp>

#include <memory>

namespace umoja_fx {

void set_tiled_edges(miral::Window const& window, unsigned edges) {
    std::shared_ptr<mir::scene::Surface> const surface = window;
    if (!surface) return;
    mir::Flags<MirTiledEdge> const flags{static_cast<MirTiledEdge>(edges & 15u)};
    // Mir's xdg-shell sends a configure with the tiled states on a change.
    if (surface->tiled_edges() != flags) surface->set_tiled_edges(flags);
}

void set_look(miral::Window const& window, float alpha, float scale) {
    std::shared_ptr<mir::scene::Surface> const surface = window;
    if (!surface) return;
    surface->set_alpha(alpha);
    // The GL renderer applies a renderable's transformation about its centre.
    surface->set_transformation(glm::scale(glm::mat4{1.0f}, glm::vec3{scale, scale, 1.0f}));
}

} // namespace umoja_fx

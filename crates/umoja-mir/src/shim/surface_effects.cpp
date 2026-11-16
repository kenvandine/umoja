// surface_effects.h: settings miral has no call for, made on the window's
// mir::scene::Surface.

#include "surface_effects.h"

#include <mir/scene/surface.h>

#include <glm/glm.hpp>
#include <glm/gtc/matrix_transform.hpp>

#include <algorithm>
#include <chrono>
#include <memory>
#include <mutex>
#include <vector>

namespace umoja_fx {
namespace {
using Clock = std::chrono::steady_clock;

struct Closing {
    std::shared_ptr<mir::scene::Surface> surface;
    Clock::time_point start;
    double ms;
};

// Asked from Mir's compositor threads (mir_surface_linger_hook) and changed
// from the window manager's animation frames.
std::mutex g_closing_mutex;
std::vector<Closing> g_closing;
}  // namespace

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

std::string app_id_of(miral::Window const& window) {
    std::shared_ptr<mir::scene::Surface> const surface = window;
    return surface ? surface->application_id() : std::string{};
}

bool begin_close(miral::Window const& window, double ms) {
    std::shared_ptr<mir::scene::Surface> const surface = window;
    if (!surface || !surface->visible()) return false;
    std::lock_guard lock{g_closing_mutex};
    g_closing.push_back(Closing{surface, Clock::now(), ms});
    return true;
}

bool step_closing() {
    auto const now = Clock::now();
    // Set the look outside the lock: a change redraws the scene, which asks
    // mir_surface_linger_hook from a compositor thread.
    std::vector<std::pair<std::shared_ptr<mir::scene::Surface>, double>> frames;
    std::vector<std::shared_ptr<mir::scene::Surface>> done;
    bool more;
    {
        std::lock_guard lock{g_closing_mutex};
        for (auto it = g_closing.begin(); it != g_closing.end();) {
            double const t = std::chrono::duration<double, std::milli>(now - it->start).count() / it->ms;
            if (t >= 1.0) {
                done.push_back(it->surface);
                it = g_closing.erase(it);
                continue;
            }
            frames.emplace_back(it->surface, t);
            ++it;
        }
        more = !g_closing.empty();
    }
    for (auto const& [surface, t] : frames) {
        double const e = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);  // ease-out cubic
        float const scale = float(1.0 - 0.1 * e);
        surface->set_alpha(float(1.0 - e));
        surface->set_transformation(glm::scale(glm::mat4{1.0f}, glm::vec3{scale, scale, 1.0f}));
    }
    // The last change redraws once more; the hook then lets the surface go.
    for (auto const& surface : done) surface->set_alpha(0.0f);
    return more;
}

} // namespace umoja_fx

// Mir patch 3027: keep drawing a removed surface while it is closing.
extern "C" __attribute__((visibility("default"))) int mir_surface_linger_hook(void const* surface) {
    std::lock_guard lock{umoja_fx::g_closing_mutex};
    return std::any_of(umoja_fx::g_closing.begin(), umoja_fx::g_closing.end(),
                       [&](umoja_fx::Closing const& c) { return c.surface.get() == surface; })
               ? 1
               : 0;
}

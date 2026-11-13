// Live window thumbnails (org.umoja.Compositor.CaptureWindow) for the Alt+Tab
// switcher, the overview and the dock's window menu.
//
// Mir can only capture an area of the screen with everything that is on it.
// Our Mir patch (packaging/mir 3018_scene-filter-hook.patch) asks the server
// executable's `mir_scene_filter_hook` about every surface whenever it builds
// a compositor's scene. We answer only for a screen shooter of our own,
// while it renders one window: that window (and its child surfaces, such as
// server-side decorations) is shown even when it is hidden (minimized, on
// another workspace), everything else is left out. Every other compositor
// (the outputs, grim/wf-recorder) gets -1, "decide as usual".
//
// Without the patched Mir the hook is never called; we notice and report
// failure, and the shell falls back to what it did before.

#include "umoja_shim.h"

#include <mir/compositor/screen_shooter.h>
#include <mir/compositor/screen_shooter_factory.h>
#include <mir/executor.h>
#include <mir/graphics/buffer.h>
#include <mir/graphics/graphic_buffer_allocator.h>
#include <mir/renderer/sw/pixel_source.h>
#include <mir/scene/surface.h>
#include <mir/server.h>

#include <algorithm>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <stdexcept>

namespace geom = mir::geometry;
namespace mg = mir::graphics;
namespace mrs = mir::renderer::software;

namespace {

mir::Server* g_server = nullptr;

// The screen shooter and buffers, built on first use and kept: a renderer
// costs shader compiles, so we only ever render at a couple of square sizes
// (the patched Mir keeps a renderer per size).
std::mutex g_mutex;
std::unique_ptr<mir::compositor::ScreenShooter> g_shooter;
std::map<int, std::shared_ptr<mg::Buffer>> g_buffers;

}  // namespace

// What the hook shows: for each of our own screen shooters that renders a
// single window (the thumbnail shooter here, window screen casts in
// screencast.cpp), the window it renders. A few slots, looked up without a
// lock: the hook runs for every surface of every frame on every output.
namespace {
struct FilterSlot {
    std::atomic<void const*> compositor{nullptr};
    std::atomic<void const*> target{nullptr};
    std::atomic<bool> consulted{false};
};
constexpr int max_filters = 16;
FilterSlot g_filters[max_filters];
std::atomic<int> g_filter_count{0};
std::mutex g_filter_mutex;

FilterSlot* slot_for(void const* compositor) {
    for (auto& slot : g_filters)
        if (slot.compositor.load(std::memory_order_acquire) == compositor) return &slot;
    return nullptr;
}
}  // namespace

// Called by the patched Mir (surface_stack.cpp) for every surface of every
// scene a compositor builds, on that compositor's thread.
extern "C" __attribute__((visibility("default"))) int mir_scene_filter_hook(void const* compositor,
                                                                           void const* surface) {
    if (g_filter_count.load(std::memory_order_acquire) == 0 || !compositor) return -1;
    FilterSlot* const slot = slot_for(compositor);
    if (!slot) return -1;
    slot->consulted.store(true, std::memory_order_relaxed);
    if (!surface) return 0;  // overlays (drag icons)
    void const* const target = slot->target.load(std::memory_order_relaxed);
    if (!target) return 0;
    if (surface == target) return 1;
    // Children of the window that draw as part of it (decorations).
    auto const s = static_cast<mir::scene::Surface const*>(surface);
    if (auto const parent = s->parent(); parent && parent.get() == target && s->visible()) return 1;
    return 0;
}

// Show only window `target` to `compositor` (one of our screen shooters),
// or let it see everything as usual again with a null target.
void umoja_scene_filter_set(void const* compositor, void const* target) {
    std::lock_guard lock{g_filter_mutex};
    FilterSlot* slot = slot_for(compositor);
    if (!target) {
        if (slot) {
            slot->compositor.store(nullptr, std::memory_order_release);
            slot->target.store(nullptr);
            g_filter_count.fetch_sub(1);
        }
        return;
    }
    if (!slot) {
        slot = slot_for(nullptr);
        if (!slot) throw std::runtime_error{"too many window captures"};
        slot->target.store(target);
        slot->consulted.store(false);
        slot->compositor.store(compositor, std::memory_order_release);
        g_filter_count.fetch_add(1);
        return;
    }
    slot->target.store(target);
}

// Whether the patched Mir asked the hook about `compositor`'s scene since
// it was filtered (false: no scene filter hook, not Umoja's Mir).
bool umoja_scene_filter_consulted(void const* compositor) {
    std::lock_guard lock{g_filter_mutex};
    FilterSlot const* slot = slot_for(compositor);
    return slot && slot->consulted.load();
}

void umoja_capture_setup(mir::Server& server) {
    server.add_init_callback([&server] { g_server = &server; });
}

// Render window `id` (its top left corner at `rect` in the layout) into a
// size x size square, scaled so its longer side fills it; `out` (at least
// size * size * 4 bytes) gets the window's part, BGRA premultiplied, top
// row first, with *out_w x *out_h pixels. Any thread; blocks while the GPU
// renders. False when Mir can't (no patched Mir, nothing to show...).
extern "C" bool umoja_capture_window(uint64_t id, UmojaRect rect, uint32_t size, uint8_t* out, uint32_t* out_w,
                                     uint32_t* out_h) {
    if (!g_server || !out || !out_w || !out_h || rect.w <= 0 || rect.h <= 0) return false;
    int const n = size <= 512 ? 512 : 1024;
    std::lock_guard lock{g_mutex};
    try {
        if (!g_shooter) g_shooter = g_server->the_screen_shooter_factory()->create(mir::immediate_executor);
        auto& buffer = g_buffers[n];
        if (!buffer)
            buffer = g_server->the_buffer_allocator()->alloc_software_buffer(geom::Size{n, n},
                                                                             mir_pixel_format_argb_8888);
        // A square viewport anchored at the window's top left: the renderer
        // (one per buffer size) never letterboxes, and we crop afterwards.
        int const side = std::max(rect.w, rect.h);
        geom::Rectangle const area{{rect.x, rect.y}, {side, side}};

        umoja_scene_filter_set(g_shooter->id(), reinterpret_cast<void const*>(id));
        std::optional<bool> ok;
        g_shooter->capture(mrs::as_write_mappable(buffer), area, glm::mat2{1.0f}, false,
                           [&ok](std::optional<mir::time::Timestamp> t) { ok = t.has_value(); });
        bool const consulted = umoja_scene_filter_consulted(g_shooter->id());
        umoja_scene_filter_set(g_shooter->id(), nullptr);
        // The immediate executor rendered before capture() returned.
        if (!ok.value_or(false)) return false;
        if (!consulted) {
            static std::once_flag warned;
            std::call_once(warned, [] {
                std::fprintf(stderr, "umoja-mir: window capture needs Mir's scene filter hook (Umoja's Mir)\n");
            });
            return false;
        }

        uint32_t const w = std::clamp<uint32_t>((uint32_t)((int64_t)rect.w * n / side), 1, n);
        uint32_t const h = std::clamp<uint32_t>((uint32_t)((int64_t)rect.h * n / side), 1, n);
        auto const mapping = buffer->map_readable();
        auto const src = reinterpret_cast<uint8_t const*>(mapping->data());
        size_t const stride = mapping->stride().as_uint32_t();
        for (uint32_t y = 0; y < h; y++) std::memcpy(out + (size_t)y * w * 4, src + y * stride, (size_t)w * 4);

        // The renderer clears to opaque black, so rounded window corners
        // come out black: clear black corner pixels that touch the edge.
        uint32_t const r = std::min<uint32_t>({std::max<uint32_t>(2, 16 * n / side), w / 2, h / 2});
        auto black = [&](uint32_t x, uint32_t y) {
            uint8_t const* p = out + ((size_t)y * w + x) * 4;
            return p[0] < 8 && p[1] < 8 && p[2] < 8;
        };
        auto clear = [&](uint32_t x, uint32_t y) { std::memset(out + ((size_t)y * w + x) * 4, 0, 4); };
        for (int corner = 0; corner < 4; corner++) {
            bool const right = corner & 1, bottom = corner & 2;
            for (uint32_t dy = 0; dy < r; dy++) {
                uint32_t const y = bottom ? h - 1 - dy : dy;
                for (uint32_t dx = 0; dx < r; dx++) {
                    uint32_t const x = right ? w - 1 - dx : dx;
                    if (!black(x, y)) break;
                    clear(x, y);
                }
            }
        }
        *out_w = w;
        *out_h = h;
        return true;
    } catch (std::exception const& e) {
        if (g_shooter) umoja_scene_filter_set(g_shooter->id(), nullptr);
        std::fprintf(stderr, "umoja-mir: window capture failed: %s\n", e.what());
        return false;
    }
}

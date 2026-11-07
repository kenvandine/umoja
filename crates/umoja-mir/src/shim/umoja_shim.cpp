// MirAL shim for umoja-mir.
//
// This file owns everything that has to be C++: the MirRunner, the Wayland
// extension set, display configuration, Xwayland, and the window-management
// policy. All decisions (which key does what, where a tiled window goes,
// how many workspaces exist, which monitor is "next") are made in Rust
// through the functions declared in umoja_shim.h, so policy can be
// unit-tested without a running compositor.

#include "umoja_shim.h"

#include <miral/configuration_option.h>
#include <miral/display_configuration.h>
#include <miral/minimal_window_manager.h>
#include <miral/output.h>
#include <miral/runner.h>
#include <miral/set_window_management_policy.h>
#include <miral/toolkit_event.h>
#include <miral/wayland_extensions.h>
#include <miral/window_info.h>
#include <miral/window_manager_tools.h>
#include <miral/window_specification.h>
#include <miral/x11_support.h>
#include <miral/zone.h>

#include <algorithm>
#include <cstdio>
#include <exception>
#include <map>
#include <memory>
#include <set>
#include <string>
#include <vector>

using namespace miral;
using namespace miral::toolkit;
namespace geom = mir::geometry;

namespace {

UmojaRect to_rect(geom::Rectangle const& r) {
    return UmojaRect{r.top_left.x.as_int(), r.top_left.y.as_int(), r.size.width.as_int(),
                     r.size.height.as_int()};
}

geom::Rectangle from_rect(UmojaRect const& r) {
    return geom::Rectangle{geom::Point{r.x, r.y}, geom::Size{r.w, r.h}};
}

uint64_t window_id(Window const& w) {
    std::shared_ptr<mir::scene::Surface> s = w;
    return reinterpret_cast<uint64_t>(s.get());
}

geom::Point centre_of(Window const& w) {
    auto const tl = w.top_left();
    auto const sz = w.size();
    return {tl.x.as_int() + sz.width.as_int() / 2, tl.y.as_int() + sz.height.as_int() / 2};
}

uint32_t to_umoja_mods(MirInputEventModifiers m) {
    uint32_t out = 0;
    if (m & mir_input_event_modifier_shift) out |= UMOJA_MOD_SHIFT;
    if (m & mir_input_event_modifier_ctrl) out |= UMOJA_MOD_CTRL;
    if (m & mir_input_event_modifier_alt) out |= UMOJA_MOD_ALT;
    if (m & mir_input_event_modifier_meta) out |= UMOJA_MOD_SUPER;
    return out;
}

// Top-level application windows: what GNOME tiles and puts on workspaces.
// Layer-shell surfaces (panel, dock, overlay) live outside the app layer.
bool is_app_window(WindowInfo const& info) {
    switch (info.type()) {
    case mir_window_type_normal:
    case mir_window_type_freestyle:
        break;
    default:
        return false;
    }
    return info.depth_layer() == mir_depth_layer_application && !info.parent();
}

// Run a callback, never letting an exception escape into Mir's loop.
template <typename F>
void guarded(char const* what, F&& f) {
    try {
        f();
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: %s failed: %s\n", what, e.what());
    } catch (...) {
        std::fprintf(stderr, "umoja-mir: %s failed\n", what);
    }
}

class UmojaWindowManager : public MinimalWindowManager {
public:
    explicit UmojaWindowManager(WindowManagerTools const& tools)
        : MinimalWindowManager{tools, mir_input_event_modifier_meta} {
        workspaces.push_back(this->tools.create_workspace());
    }

    // ---- placement --------------------------------------------------------

    auto place_new_window(ApplicationInfo const& app, WindowSpecification const& requested)
        -> WindowSpecification override {
        auto spec = MinimalWindowManager::place_new_window(app, requested);
        // org.gnome.mutter center-new-windows
        bool const toplevel = !requested.parent().is_set() || !requested.parent().value().lock();
        if (toplevel && umoja_center_new_windows() && spec.size().is_set()) {
            auto const area = tools.active_application_zone().extents();
            auto const sz = spec.size().value();
            spec.top_left() = geom::Point{
                area.top_left.x.as_int() + (area.size.width.as_int() - sz.width.as_int()) / 2,
                area.top_left.y.as_int() + (area.size.height.as_int() - sz.height.as_int()) / 2};
        }
        return spec;
    }

    void advise_new_window(WindowInfo const& info) override {
        MinimalWindowManager::advise_new_window(info);
        if (!is_app_window(info)) return;
        all_windows.insert(info.window());
        update_workspace_membership(info.window());
        relayout_workspaces();
    }

    void advise_delete_window(WindowInfo const& info) override {
        untile(info.window());
        hidden_states.erase(info.window());
        if (all_windows.erase(info.window())) relayout_workspaces(info.window());
        MinimalWindowManager::advise_delete_window(info);
    }

    // ---- input ------------------------------------------------------------

    bool handle_keyboard_event(MirKeyboardEvent const* event) override {
        auto const action = mir_keyboard_event_action(event);
        auto const keysym = mir_keyboard_event_keysym(event);
        auto const mods = to_umoja_mods(mir_keyboard_event_modifiers(event));

        // A lone tap of the overlay key (Super_L by default) toggles the overlay.
        if (umoja_overlay_key_matches(keysym)) {
            if (action == mir_keyboard_action_down) overlay_key_armed = true;
            if (action == mir_keyboard_action_up && overlay_key_armed) {
                overlay_key_armed = false;
                umoja_handle_action(UMOJA_ACTION_TOGGLE_OVERLAY);
                return true;
            }
            return false;
        }
        if (action == mir_keyboard_action_down) overlay_key_armed = false;
        if (action != mir_keyboard_action_down) return MinimalWindowManager::handle_keyboard_event(event);

        uint32_t const bound = umoja_lookup_binding(keysym, mods);
        if (bound != UMOJA_ACTION_NONE && apply(bound)) return true;
        // Alt+Tab / Alt+` / Alt+F4 come from MinimalWindowManager, as in GNOME.
        return MinimalWindowManager::handle_keyboard_event(event);
    }

    bool handle_pointer_event(MirPointerEvent const* event) override {
        auto const action = mir_pointer_event_action(event);
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(event, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(event, mir_pointer_axis_y));

        bool const consumed = MinimalWindowManager::handle_pointer_event(event);

        if (action == mir_pointer_action_button_down) {
            drag_window = tools.active_window();
            drag_origin = drag_window ? drag_window.top_left() : geom::Point{};
        } else if (action == mir_pointer_action_button_up && drag_window) {
            Window w = drag_window;
            drag_window = Window{};
            if (w == tools.active_window() && w.top_left() != drag_origin) {
                auto const area = tools.active_application_zone().extents();
                uint32_t const zone = umoja_edge_zone(px, py, to_rect(area));
                if (zone != UMOJA_ACTION_NONE) apply(zone);
            }
        }
        return consumed;
    }

    // ---- tiling groups ----------------------------------------------------

    void advise_resize(WindowInfo const& info, geom::Size const& size) override {
        MinimalWindowManager::advise_resize(info, size);
        if (applying) return;
        auto const id = window_id(info.window());
        if (!tiled.count(id)) return;

        geom::Rectangle const r{info.window().top_left(), size};
        uint64_t ids[16];
        UmojaRect rects[16];
        size_t n = umoja_tiling_resized(id, to_rect(r), ids, rects, 16);
        applying = true;
        for (size_t i = 0; i < n; ++i) {
            auto it = tiled.find(ids[i]);
            if (it == tiled.end()) continue;
            WindowSpecification spec;
            spec.top_left() = geom::Point{rects[i].x, rects[i].y};
            spec.size() = geom::Size{rects[i].w, rects[i].h};
            guarded("neighbour resize", [&] { tools.modify_window(it->second, spec); });
        }
        applying = false;
    }

    void advise_move_to(WindowInfo const& info, geom::Point top_left) override {
        MinimalWindowManager::advise_move_to(info, top_left);
        if (all_windows.count(info.window())) update_workspace_membership(info.window());
        if (applying) return;
        // The user dragged a tiled window away: it leaves its tile group.
        untile(info.window());
    }

    // ---- outputs ----------------------------------------------------------

    void advise_output_create(Output const& output) override {
        outputs[output.id()] = output.extents();
        report(output);
    }

    void advise_output_update(Output const& updated, Output const&) override {
        outputs[updated.id()] = updated.extents();
        report(updated);
    }

    void advise_output_delete(Output const& output) override {
        auto const gone = output.extents();
        outputs.erase(output.id());
        umoja_output_remove(output.id());
        if (outputs.empty()) return;
        // Like mutter: windows on an unplugged monitor move to the primary one.
        auto const target = primary_output();
        for (auto const& w : all_windows) {
            if (!gone.contains(centre_of(w))) continue;
            auto const moved = umoja_translate_rect(to_rect({w.top_left(), w.size()}), to_rect(gone), to_rect(target));
            WindowSpecification spec;
            spec.top_left() = geom::Point{moved.x, moved.y};
            spec.size() = geom::Size{moved.w, moved.h};
            guarded("evacuate output", [&] { tools.modify_window(tools.info_for(w), spec); });
        }
    }

private:
    // Feed org.gnome.Mutter.DisplayConfig (Settings › Displays).
    static void report(Output const& o) {
        auto const mm = o.physical_size_mm();
        umoja_output_update(o.id(), o.name().c_str(), to_rect(o.extents()), o.scale(), o.refresh_rate(),
                            static_cast<uint32_t>(mm.width), static_cast<uint32_t>(mm.height),
                            static_cast<uint32_t>(o.orientation()));
    }

    bool overlay_key_armed = false;
    bool applying = false;
    Window drag_window;
    geom::Point drag_origin;
    std::map<uint64_t, Window> tiled;

    std::vector<std::shared_ptr<Workspace>> workspaces;
    size_t current = 0;
    std::set<Window> all_windows;
    std::map<Window, MirWindowState> hidden_states;
    std::map<int, geom::Rectangle> outputs;

    // Mir has no primary-monitor concept; use the one at the origin, which is
    // where GNOME's monitors.xml import puts the primary.
    geom::Rectangle primary_output() const {
        for (auto const& [id, r] : outputs)
            if (r.contains(geom::Point{0, 0})) return r;
        return outputs.empty() ? geom::Rectangle{} : outputs.begin()->second;
    }

    bool on_primary(Window const& w) const {
        return outputs.size() < 2 || primary_output().contains(centre_of(w));
    }

    // ---- workspaces -------------------------------------------------------

    bool in_workspace(Window const& w, std::shared_ptr<Workspace> const& ws) {
        bool found = false;
        tools.for_each_workspace_containing(w, [&](std::shared_ptr<Workspace> const& c) { found |= (c == ws); });
        return found;
    }

    bool in_any_workspace(Window const& w) {
        bool found = false;
        tools.for_each_workspace_containing(w, [&](std::shared_ptr<Workspace> const&) { found = true; });
        return found;
    }

    // org.gnome.mutter workspaces-only-on-primary: windows on secondary
    // monitors are on every workspace.
    void update_workspace_membership(Window const& w) {
        bool const sticky = umoja_workspaces_only_on_primary() && !on_primary(w);
        bool const member = in_any_workspace(w);
        if (sticky && member) {
            std::vector<std::shared_ptr<Workspace>> containing;
            tools.for_each_workspace_containing(w, [&](std::shared_ptr<Workspace> const& c) { containing.push_back(c); });
            for (auto const& ws : containing) tools.remove_tree_from_workspace(w, ws);
        } else if (!sticky && !member) {
            tools.add_tree_to_workspace(w, workspaces[current]);
        }
    }

    void hide(Window const& w) {
        auto& info = tools.info_for(w);
        auto const state = info.state();
        if (state == mir_window_state_hidden || state == mir_window_state_minimized) return;
        hidden_states[w] = state;
        WindowSpecification spec;
        spec.state() = mir_window_state_hidden;
        tools.modify_window(info, spec);
    }

    void unhide(Window const& w) {
        auto it = hidden_states.find(w);
        if (it == hidden_states.end()) return;
        WindowSpecification spec;
        spec.state() = it->second;
        hidden_states.erase(it);
        tools.modify_window(tools.info_for(w), spec);
    }

    void switch_to(size_t index) {
        if (index >= workspaces.size() || index == current) return;
        applying = true;
        guarded("switch workspace", [&] {
            std::vector<Window> leaving, arriving;
            tools.for_each_window_in_workspace(workspaces[current], [&](Window const& w) { leaving.push_back(w); });
            tools.for_each_window_in_workspace(workspaces[index], [&](Window const& w) { arriving.push_back(w); });
            for (auto const& w : leaving)
                if (!in_workspace(w, workspaces[index])) hide(w);
            for (auto const& w : arriving) unhide(w);
            current = index;
            if (!arriving.empty()) tools.select_active_window(arriving.back());
        });
        applying = false;
        relayout_workspaces();
    }

    void move_active_to(size_t index) {
        Window w = tools.active_window();
        if (!w || index >= workspaces.size() || !all_windows.count(w)) return;
        if (!in_any_workspace(w)) return; // sticky (secondary monitor)
        tools.remove_tree_from_workspace(w, workspaces[current]);
        tools.add_tree_to_workspace(w, workspaces[index]);
        // GNOME follows the window to its new workspace.
        switch_to(index);
        tools.select_active_window(w);
    }

    // Grow/shrink the workspace list as GNOME's dynamic workspaces do.
    void relayout_workspaces(Window const& dying = Window{}) {
        guarded("relayout workspaces", [&] {
            std::vector<char> occupied(workspaces.size(), 0);
            for (size_t i = 0; i < workspaces.size(); ++i)
                tools.for_each_window_in_workspace(workspaces[i], [&](Window const& w) {
                    if (w != dying) occupied[i] = 1;
                });
            std::vector<bool> occ(occupied.begin(), occupied.end());
            bool flat[64] = {};
            size_t const n = std::min<size_t>(occ.size(), 64);
            for (size_t i = 0; i < n; ++i) flat[i] = occ[i];

            size_t remove[64];
            size_t n_remove = 0;
            size_t const desired = umoja_workspace_layout(flat, n, current, remove, 64, &n_remove);

            for (size_t k = 0; k < n_remove; ++k) {
                size_t const idx = remove[k];
                if (idx >= workspaces.size() || idx == current) continue;
                workspaces.erase(workspaces.begin() + static_cast<long>(idx));
                if (idx < current) --current;
            }
            while (workspaces.size() < desired) workspaces.push_back(tools.create_workspace());
            // Fixed count shrank: fold extra workspaces into the last kept one.
            while (workspaces.size() > desired && workspaces.size() > 1) {
                auto extra = workspaces.back();
                workspaces.pop_back();
                tools.move_workspace_content_to_workspace(workspaces.back(), extra);
                if (current >= workspaces.size()) current = workspaces.size() - 1;
            }
        });
    }

    // ---- actions ----------------------------------------------------------

    void untile(Window const& w) {
        auto const id = window_id(w);
        if (tiled.erase(id)) umoja_tiling_unregister(id);
    }

    // Returns true when the action was handled here.
    bool apply(uint32_t action) {
        uint32_t kind = 0;
        size_t index = 0;
        if (umoja_workspace_resolve(action, current, workspaces.size(), &kind, &index)) {
            if (kind == UMOJA_WORKSPACE_SWITCH) switch_to(index);
            else move_active_to(index);
            return true;
        }

        switch (action) {
        case UMOJA_ACTION_TOGGLE_OVERLAY:
        case UMOJA_ACTION_TOGGLE_DICTATION:
        case UMOJA_ACTION_SCREENSHOT_UI:
        case UMOJA_ACTION_SCREENSHOT:
            umoja_handle_action(action);
            return true;
        default:
            break;
        }

        Window active = tools.active_window();
        if (!active) return false;
        auto& info = tools.info_for(active);

        switch (action) {
        case UMOJA_ACTION_SCREENSHOT_WINDOW:
            umoja_screenshot_window(to_rect({active.top_left(), active.size()}));
            return true;
        case UMOJA_ACTION_CLOSE:
            tools.ask_client_to_close(active);
            return true;
        case UMOJA_ACTION_MINIMIZE:
            return set_state(info, mir_window_state_minimized);
        case UMOJA_ACTION_MAXIMIZE:
            return set_state(info, mir_window_state_maximized);
        case UMOJA_ACTION_UNMAXIMIZE:
            return set_state(info, mir_window_state_restored);
        case UMOJA_ACTION_TOGGLE_MAXIMIZED:
            return set_state(info, info.state() == mir_window_state_maximized ? mir_window_state_restored
                                                                              : mir_window_state_maximized);
        case UMOJA_ACTION_TOGGLE_FULLSCREEN:
            return set_state(info, info.state() == mir_window_state_fullscreen ? mir_window_state_restored
                                                                               : mir_window_state_fullscreen);
        case UMOJA_ACTION_MOVE_TO_MONITOR_LEFT:
        case UMOJA_ACTION_MOVE_TO_MONITOR_RIGHT:
        case UMOJA_ACTION_MOVE_TO_MONITOR_UP:
        case UMOJA_ACTION_MOVE_TO_MONITOR_DOWN:
            return move_to_monitor(info, action);
        default:
            break;
        }

        if (!is_app_window(info)) return false;
        auto const area = tools.active_application_zone().extents();
        UmojaRect out{};
        if (!umoja_tile_geometry(action, to_rect(area), &out)) return false;

        WindowSpecification spec;
        spec.state() = mir_window_state_restored;
        spec.top_left() = geom::Point{out.x, out.y};
        spec.size() = geom::Size{out.w, out.h};
        applying = true;
        guarded("tile", [&] { tools.modify_window(info, spec); });
        applying = false;

        auto const id = window_id(active);
        tiled[id] = active;
        umoja_tiling_register(id, action, out);
        return true;
    }

    bool move_to_monitor(WindowInfo& info, uint32_t action) {
        if (outputs.size() < 2) return false;
        Window const& w = info.window();
        std::vector<int> ids;
        std::vector<UmojaRect> rects;
        geom::Rectangle from{};
        for (auto const& [id, r] : outputs) {
            ids.push_back(id);
            rects.push_back(to_rect(r));
            if (r.contains(centre_of(w))) from = r;
        }
        int32_t const idx = umoja_neighbor_output(action, to_rect(from), rects.data(), rects.size());
        if (idx < 0) return true;
        auto const to = from_rect(rects[static_cast<size_t>(idx)]);
        auto const moved = umoja_translate_rect(to_rect({w.top_left(), w.size()}), to_rect(from), to_rect(to));

        untile(w);
        WindowSpecification spec;
        spec.output_id() = ids[static_cast<size_t>(idx)];
        spec.top_left() = geom::Point{moved.x, moved.y};
        spec.size() = geom::Size{moved.w, moved.h};
        if (info.state() == mir_window_state_maximized || info.state() == mir_window_state_fullscreen) {
            spec.state() = info.state();
            tools.place_and_size_for_state(spec, info);
        }
        tools.modify_window(info, spec);
        return true;
    }

    bool set_state(WindowInfo& info, MirWindowState state) {
        untile(info.window());
        WindowSpecification spec;
        spec.state() = state;
        tools.place_and_size_for_state(spec, info);
        tools.modify_window(info, spec);
        return true;
    }
};

} // namespace

extern "C" int umoja_mir_run(int argc, char const** argv, UmojaConfig const* config) {
    MirRunner runner{argc, argv};

    WaylandExtensions extensions;
    auto const supported = WaylandExtensions::supported();
    for (size_t i = 0; i < config->n_extensions; ++i) {
        std::string const name = config->extensions[i];
        // Newer protocols (e.g. fractional scale) only exist in newer Mir.
        if (!supported.count(name)) {
            std::fprintf(stderr, "umoja-mir: %s not supported by this Mir, skipping\n", name.c_str());
            continue;
        }
        // Shell components are trusted first-party clients; enable for all.
        extensions.conditionally_enable(name, [](WaylandExtensions::EnableInfo const&) { return true; });
    }
    // Match mutter: no xdg-decoration, so GTK/libadwaita apps draw exactly the
    // client-side headerbars they draw on GNOME, and Qt uses its own CSD.
    if (supported.count("zxdg_decoration_manager_v1")) extensions.disable("zxdg_decoration_manager_v1");

    // Persisted monitor layout (~/.config/umoja-mir.display), seeded from
    // GNOME's monitors.xml by the Rust side; Mir reloads it when it changes.
    DisplayConfiguration display_config{runner};

    runner.add_start_callback([&runner] {
        auto display = runner.wayland_display();
        umoja_session_started(display.is_set() ? display.value().c_str() : "");
    });

    runner.set_exception_handler([] {
        try {
            throw;
        } catch (std::exception const& e) {
            std::fprintf(stderr, "umoja-mir: fatal: %s\n", e.what());
        } catch (...) {
            std::fprintf(stderr, "umoja-mir: fatal: unknown exception\n");
        }
    });

    if (config->enable_x11) {
        return runner.run_with({
            display_config,
            display_config.layout_option(),
            extensions,
            X11Support{},
            set_window_management_policy<UmojaWindowManager>(),
        });
    }
    return runner.run_with({
        display_config,
        display_config.layout_option(),
        extensions,
        set_window_management_policy<UmojaWindowManager>(),
    });
}

// MirAL shim for umoja-mir.
//
// This file owns everything that has to be C++: the MirRunner, the Wayland
// extension set, display configuration, Xwayland, and the window-management
// policy. All decisions (which key does what, where a tiled window goes,
// how many workspaces exist, which monitor is "next") are made in Rust
// through the functions declared in umoja_shim.h, so policy can be
// unit-tested without a running compositor.

#include "umoja_shim.h"

#include <miral/application.h>
#include <miral/configuration_option.h>
#include <miral/display_configuration.h>
#include <miral/minimal_window_manager.h>
#include <miral/output.h>
#include <miral/prepend_event_filter.h>
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
#include <chrono>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <functional>
#include <map>
#include <memory>
#include <optional>
#include <set>
#include <string>
#include <thread>
#include <vector>

#include <unistd.h>

// Defined in natural_scroll.cpp (needs Mir's event classes, whose C API
// clashes with miral::toolkit's).
bool umoja_apply_natural_scroll(MirEvent const* event);

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

class UmojaWindowManager;
UmojaWindowManager* g_wm = nullptr;

class UmojaWindowManager : public MinimalWindowManager {
public:
    explicit UmojaWindowManager(WindowManagerTools const& tools)
        : MinimalWindowManager{tools, mir_input_event_modifier_meta} {
        workspaces.push_back(this->tools.create_workspace());
        g_wm = this;
    }

    ~UmojaWindowManager() override {
        if (g_wm == this) g_wm = nullptr;
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
        if (!is_app_window(info)) {
            // Chrome mapped during a drag (tile preview) goes under the window.
            if (moving && elevated) guarded("raise dragged", [&] { tools.raise_tree(moving); });
            return;
        }
        all_windows.insert(info.window());
        stacking.push_back(info.window());
        mru.insert(mru.begin(), info.window());
        update_workspace_membership(info.window());
        relayout_workspaces();
    }

    void advise_delete_window(WindowInfo const& info) override {
        Window const w = info.window();
        if (w == moving) {
            umoja_drag_end();
            moving = Window{};
            elevated = false;
        }
        if (w == press_window) press_window = Window{};
        untile(w);
        untiled.erase(w);
        tile_action.erase(w);
        hidden_states.erase(w);
        erase(stacking, w);
        erase(mru, w);
        umoja_switcher_window_closed(window_id(w));
        if (all_windows.erase(w)) relayout_workspaces(w);
        MinimalWindowManager::advise_delete_window(info);
    }

    void advise_focus_gained(WindowInfo const& info) override {
        MinimalWindowManager::advise_focus_gained(info);
        Window const w = info.window();
        if (!all_windows.count(w)) return;
        erase(mru, w);
        mru.insert(mru.begin(), w);
        // Tiling Assistant's enable-raise-tile-group: a tiled window comes
        // up together with the windows tiled next to it.
        if (raising_group || applying || !tiled.count(window_id(w)) || !umoja_raise_tile_group()) return;
        raising_group = true;
        guarded("raise tile group", [&] {
            for (auto const& [id, other] : tiled)
                if (other != w && visible_here(other)) tools.raise_tree(other);
            tools.raise_tree(w);
        });
        raising_group = false;
    }

    void advise_raise(std::vector<Window> const& windows) override {
        MinimalWindowManager::advise_raise(windows);
        for (auto const& w : windows) {
            if (!all_windows.count(w)) continue;
            erase(stacking, w);
            stacking.push_back(w);
        }
    }

    // ---- input ------------------------------------------------------------

    bool handle_keyboard_event(MirKeyboardEvent const* event) override {
        auto const action = mir_keyboard_event_action(event);
        auto const keysym = mir_keyboard_event_keysym(event);
        auto mods = to_umoja_mods(mir_keyboard_event_modifiers(event));
        if (track_modifiers) mods |= tracked_modifiers(keysym, action);

        // While Alt+Tab is open it sees every key, releases included.
        {
            UmojaSwitchResult res{};
            bool const down = action != mir_keyboard_action_up;
            uint32_t const r = umoja_switcher_key(keysym, mods, down, &res);
            if (r != UMOJA_SWITCH_NOT_OURS) {
                apply_switch(r, res);
                overlay_key_armed = false;
                return true;
            }
        }

        // A lone tap of the overlay key (Super_L by default) opens the
        // Activities overview, as in GNOME.
        if (umoja_overlay_key_matches(keysym)) {
            if (action == mir_keyboard_action_down) overlay_key_armed = true;
            if (action == mir_keyboard_action_up && overlay_key_armed) {
                overlay_key_armed = false;
                umoja_handle_action(UMOJA_ACTION_TOGGLE_OVERVIEW);
                return true;
            }
            return false;
        }
        if (action == mir_keyboard_action_down) overlay_key_armed = false;
        if (action == mir_keyboard_action_up) return MinimalWindowManager::handle_keyboard_event(event);

        uint32_t const bound = umoja_lookup_binding(keysym, mods);
        if (bound != UMOJA_ACTION_NONE && umoja_is_switcher_action(bound)) {
            if (action == mir_keyboard_action_down) open_switcher(bound, mods);
            return true;
        }
        if (action != mir_keyboard_action_down) return MinimalWindowManager::handle_keyboard_event(event);
        if (bound != UMOJA_ACTION_NONE && apply(bound)) return true;
        // Alt+F4 etc. that we do not bind fall through to MinimalWindowManager.
        return MinimalWindowManager::handle_keyboard_event(event);
    }

    bool handle_pointer_event(MirPointerEvent const* event) override {
        auto const action = mir_pointer_event_action(event);
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(event, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(event, mir_pointer_axis_y));

        bool const consumed = MinimalWindowManager::handle_pointer_event(event);

        if (action == mir_pointer_action_button_down) {
            press_window = tools.active_window();
            press_origin = press_window ? press_window.top_left() : geom::Point{};
            press_restore = press_window && was_tiled_or_maximized(press_window);
        } else if (action == mir_pointer_action_motion) {
            if (!moving && press_window && mir_pointer_event_buttons(event) != 0 &&
                press_window.top_left() != press_origin && all_windows.count(press_window)) {
                // Super+drag: MinimalWindowManager moves it without a request.
                begin_move(press_window, px, py, press_restore);
            }
            if (moving) on_move(px, py);
        } else if (action == mir_pointer_action_button_up) {
            if (moving && mir_pointer_event_buttons(event) == 0) end_move();
            press_window = Window{};
        }
        return consumed;
    }

    // A window hidden because it is on another workspace must stay hidden
    // whatever its client asks. Chromium/Electron and Qt apps answer the
    // configure that hiding sends by re-requesting their own state (e.g.
    // "restored"), which Mir would apply and so un-hide the window on the
    // workspace it just left. Remember the request for when it comes back.
    void handle_modify_window(WindowInfo& info, WindowSpecification const& modifications) override {
        auto it = hidden_states.find(info.window());
        if (it == hidden_states.end() || !modifications.state().is_set()) {
            MinimalWindowManager::handle_modify_window(info, modifications);
            return;
        }
        WindowSpecification spec = modifications;
        auto const requested = spec.state().consume();
        if (requested != mir_window_state_hidden) it->second = requested;
        MinimalWindowManager::handle_modify_window(info, spec);
    }

    // Activation requests from windows on another workspace don't pull them
    // onto this one (GNOME shows a "ready" notice instead of switching).
    void handle_raise_window(WindowInfo& info) override {
        if (hidden_states.count(info.window())) return;
        MinimalWindowManager::handle_raise_window(info);
    }

    void handle_request_move(WindowInfo& info, MirInputEvent const* input_event) override {
        MinimalWindowManager::handle_request_move(info, input_event);
        if (!is_app_window(info) || mir_input_event_get_type(input_event) != mir_input_event_type_pointer) return;
        auto const* pev = mir_input_event_get_pointer_event(input_event);
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_y));
        guarded("begin move", [&] { begin_move(info.window(), px, py, was_tiled_or_maximized(info.window())); });
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
        // Windows sliding during a workspace switch are not being moved by
        // the user, and must not turn sticky as they leave the monitor.
        if (animating) return;
        if (all_windows.count(info.window()) && update_workspace_membership(info.window())) relayout_workspaces();
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

    void advise_application_zone_create(Zone const& zone) override { zones[zone.id()] = zone.extents(); }
    void advise_application_zone_update(Zone const& updated, Zone const&) override {
        zones[updated.id()] = updated.extents();
    }
    void advise_application_zone_delete(Zone const& zone) override { zones.erase(zone.id()); }

    // ---- entry points for org.umoja.Compositor (under the WM lock) ---------

    void list_windows(UmojaWindowCallback cb, void* ctx, bool by_mru) {
        Window const active = tools.active_window();
        auto const& order = by_mru ? mru : stacking;
        for (auto const& w : order) {
            auto const& info = tools.info_for(w);
            std::string const app = info.application_id();
            std::string const title = info.name();
            uint32_t flags = 0;
            if (w == active) flags |= UMOJA_WINDOW_FOCUSED;
            if (info.state() == mir_window_state_minimized) flags |= UMOJA_WINDOW_MINIMIZED;
            if (info.state() == mir_window_state_maximized) flags |= UMOJA_WINDOW_MAXIMIZED;
            if (info.state() == mir_window_state_fullscreen) flags |= UMOJA_WINDOW_FULLSCREEN;
            if (on_current_workspace(w)) flags |= UMOJA_WINDOW_CURRENT_WORKSPACE;
            if (tiled.count(window_id(w))) flags |= UMOJA_WINDOW_TILED;
            UmojaWindowDesc const d{window_id(w), app.c_str(), title.c_str(), to_rect({w.top_left(), w.size()}),
                                    flags, workspace_of(w)};
            cb(ctx, &d);
        }
    }

    Window find(uint64_t id) const {
        for (auto const& w : all_windows)
            if (window_id(w) == id) return w;
        return Window{};
    }

    void activate(Window w) {
        if (!w) return;
        guarded("activate", [&] {
            // Windows on another workspace take you there, as in GNOME.
            for (size_t i = 0; i < workspaces.size(); ++i)
                if (i != current && in_workspace(w, workspaces[i])) {
                    switch_to(i);
                    break;
                }
            auto& info = tools.info_for(w);
            if (info.state() == mir_window_state_minimized || info.state() == mir_window_state_hidden) {
                hidden_states.erase(w);
                WindowSpecification spec;
                spec.state() = mir_window_state_restored;
                tools.place_and_size_for_state(spec, info);
                tools.modify_window(info, spec);
            }
            tools.raise_tree(w);
            tools.select_active_window(w);
        });
    }

    void close(Window w) {
        if (w) guarded("close", [&] { tools.ask_client_to_close(w); });
    }

    void switcher_pick(uint32_t index) {
        UmojaSwitchResult res{};
        apply_switch(umoja_switcher_pick(index, &res), res);
    }

    void tile(Window w, uint32_t action) {
        if (w) guarded("tile", [&] { apply_to(w, action, false); });
    }

    void invoke(std::function<void()> const& f) { tools.invoke_under_lock(f); }

private:
    // Feed org.gnome.Mutter.DisplayConfig (Settings › Displays).
    static void report(Output const& o) {
        auto const mm = o.physical_size_mm();
        umoja_output_update(o.id(), o.name().c_str(), to_rect(o.extents()), o.scale(), o.refresh_rate(),
                            static_cast<uint32_t>(mm.width), static_cast<uint32_t>(mm.height),
                            static_cast<uint32_t>(o.orientation()));
    }

    template <typename C>
    static void erase(C& c, Window const& w) {
        c.erase(std::remove(c.begin(), c.end(), w), c.end());
    }

    // Headless tests type through zwp_virtual_keyboard (wtype), whose
    // modifier state Mir does not report on key events; with
    // UMOJA_TRACK_MODIFIERS=1 we follow the modifier keys ourselves.
    bool const track_modifiers = std::getenv("UMOJA_TRACK_MODIFIERS") != nullptr;
    uint32_t held_modifiers = 0;

    uint32_t tracked_modifiers(uint32_t keysym, MirKeyboardAction action) {
        uint32_t bit = 0;
        switch (keysym) {
        case 0xffe1: case 0xffe2: bit = UMOJA_MOD_SHIFT; break;
        case 0xffe3: case 0xffe4: bit = UMOJA_MOD_CTRL; break;
        case 0xffe9: case 0xffea: bit = UMOJA_MOD_ALT; break;
        case 0xffeb: case 0xffec: bit = UMOJA_MOD_SUPER; break;
        default: return held_modifiers;
        }
        uint32_t const before = held_modifiers;
        if (action == mir_keyboard_action_up) held_modifiers &= ~bit;
        else held_modifiers |= bit;
        // A modifier's own event carries the state from before it.
        return before;
    }

    bool overlay_key_armed = false;
    bool applying = false;
    bool raising_group = false;
    std::map<uint64_t, Window> tiled;
    std::map<Window, uint32_t> tile_action;
    // Geometry before a window was tiled, restored when it is dragged out or
    // untiled with the keyboard (Tiling Assistant's "untiledRect").
    std::map<Window, geom::Rectangle> untiled;

    // Pointer press / window move tracking.
    Window press_window;
    geom::Point press_origin;
    bool press_restore = false;
    Window moving;
    bool elevated = false;
    uint32_t drag_target = UMOJA_ACTION_NONE;

    std::vector<Window> stacking; // bottom to top
    std::vector<Window> mru;      // most recently focused first

    std::vector<std::shared_ptr<Workspace>> workspaces;
    size_t current = 0;
    std::set<Window> all_windows;
    std::map<Window, MirWindowState> hidden_states;
    std::map<int, geom::Rectangle> outputs;
    std::map<int, geom::Rectangle> zones;

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

    geom::Rectangle output_at(geom::Point p) const {
        for (auto const& [id, r] : outputs)
            if (r.contains(p)) return r;
        return primary_output();
    }

    // Work area (output minus panel/dock) under a point.
    geom::Rectangle area_at(geom::Point p) {
        for (auto const& [id, r] : zones)
            if (r.contains(p)) return r;
        return tools.active_application_zone().extents();
    }

    bool on_current_workspace(Window const& w) {
        if (hidden_states.count(w)) return false;
        return !in_any_workspace(w) || in_workspace(w, workspaces[current]);
    }

    bool visible_here(Window const& w) {
        auto const state = tools.info_for(w).state();
        return on_current_workspace(w) && state != mir_window_state_minimized && state != mir_window_state_hidden;
    }

    bool was_tiled_or_maximized(Window const& w) {
        if (!all_windows.count(w)) return false;
        auto const state = tools.info_for(w).state();
        return tiled.count(window_id(w)) || state == mir_window_state_maximized ||
               state == mir_window_state_horizmaximized || state == mir_window_state_vertmaximized;
    }

    // ---- Alt+Tab ----------------------------------------------------------

    void open_switcher(uint32_t action, uint32_t mods) {
        bool const all_workspaces =
            action == UMOJA_ACTION_SWITCH_APPLICATIONS || action == UMOJA_ACTION_SWITCH_APPLICATIONS_BACKWARD;
        std::vector<std::string> strings;
        std::vector<Window> list;
        for (auto const& w : mru)
            if (all_workspaces || on_current_workspace(w)) list.push_back(w);
        strings.reserve(list.size() * 2);
        std::vector<UmojaSwitchWindow> entries;
        for (auto const& w : list) {
            auto const& info = tools.info_for(w);
            strings.push_back(info.application_id());
            strings.push_back(info.name());
        }
        for (size_t i = 0; i < list.size(); ++i)
            entries.push_back({window_id(list[i]), strings[2 * i].c_str(), strings[2 * i + 1].c_str()});
        umoja_switcher_open(action, mods, entries.data(), entries.size());
    }

    void apply_switch(uint32_t code, UmojaSwitchResult const& res) {
        switch (code) {
        case UMOJA_SWITCH_ACTIVATE:
            guarded("switch", [&] {
                for (size_t i = 0; i < res.n_ids; ++i) {
                    Window const w = find(res.ids[i]);
                    if (w && visible_here(w)) tools.raise_tree(w);
                }
                activate(find(res.window));
            });
            break;
        case UMOJA_SWITCH_CLOSE:
            for (size_t i = 0; i < res.n_ids; ++i) close(find(res.ids[i]));
            break;
        default:
            break;
        }
    }

    // ---- drag to snap -----------------------------------------------------

    // Helpers that modify windows take Window by value: modify_window() can
    // replace the WindowInfo that a reference like info.window() points into.
    void begin_move(Window w, int32_t px, int32_t py, bool restore) {
        if (moving == w) return;
        if (moving) end_move();
        moving = w;
        drag_target = UMOJA_ACTION_NONE;
        auto& info = tools.info_for(w);

        // Dragging a tiled or maximized window out restores its size, keeping
        // the grabbed point under the pointer (GNOME and Tiling Assistant).
        if (restore) {
            geom::Rectangle const cur{w.top_left(), w.size()};
            geom::Rectangle back = info.restore_rect();
            if (auto it = untiled.find(w); it != untiled.end()) back = it->second;
            int const cw = std::max(1, cur.size.width.as_int());
            int const bw = back.size.width.as_int();
            int const bh = back.size.height.as_int();
            if (bw > 0 && bh > 0) {
                double const fx = double(px - cur.top_left.x.as_int()) / cw;
                WindowSpecification spec;
                spec.state() = mir_window_state_restored;
                spec.size() = geom::Size{bw, bh};
                spec.top_left() = geom::Point{px - int(fx * bw), cur.top_left.y.as_int()};
                untile(w);
                untiled.erase(w);
                tile_action.erase(w);
                applying = true;
                guarded("restore on drag", [&] { tools.modify_window(info, spec); });
                applying = false;
            }
        }

        // Lift the window above the tile preview (which lives in the layer
        // above applications) for the length of the drag.
        WindowSpecification lift;
        lift.depth_layer() = mir_depth_layer_above;
        guarded("lift dragged window", [&] {
            tools.modify_window(info, lift);
            tools.raise_tree(w);
        });
        elevated = true;

        geom::Point const p{px, py};
        umoja_drag_begin(px, py, to_rect({w.top_left(), w.size()}), to_rect(area_at(p)), to_rect(output_at(p)));
    }

    void on_move(int32_t px, int32_t py) {
        geom::Point const p{px, py};
        drag_target = umoja_drag_motion(px, py, to_rect({moving.top_left(), moving.size()}), to_rect(area_at(p)),
                                        to_rect(output_at(p)));
    }

    void end_move() {
        Window const w = moving;
        moving = Window{};
        uint32_t const target = umoja_drag_end();
        if (!w) return;
        if (elevated) {
            elevated = false;
            WindowSpecification drop;
            drop.depth_layer() = mir_depth_layer_application;
            guarded("drop dragged window", [&] {
                tools.modify_window(tools.info_for(w), drop);
                tools.raise_tree(w);
            });
        }
        if (target != UMOJA_ACTION_NONE) apply_to(w, target, false);
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

    int32_t workspace_of(Window const& w) {
        for (size_t i = 0; i < workspaces.size(); ++i)
            if (in_workspace(w, workspaces[i])) return static_cast<int32_t>(i);
        return -1;
    }

    // org.gnome.mutter workspaces-only-on-primary: windows on secondary
    // monitors are on every workspace. Returns whether membership changed.
    bool update_workspace_membership(Window const& w) {
        bool const sticky = umoja_workspaces_only_on_primary() && !on_primary(w);
        bool const member = in_any_workspace(w);
        if (sticky && member) {
            std::vector<std::shared_ptr<Workspace>> containing;
            tools.for_each_workspace_containing(w, [&](std::shared_ptr<Workspace> const& c) { containing.push_back(c); });
            for (auto const& ws : containing) tools.remove_tree_from_workspace(w, ws);
            // Now on every workspace, so it must be visible on this one.
            unhide(w);
            return true;
        }
        if (!sticky && !member) {
            tools.add_tree_to_workspace(w, workspaces[current]);
            return true;
        }
        return false;
    }

    void hide(Window w) {
        auto& info = tools.info_for(w);
        auto const state = info.state();
        if (state == mir_window_state_hidden || state == mir_window_state_minimized) return;
        hidden_states[w] = state;
        WindowSpecification spec;
        spec.state() = mir_window_state_hidden;
        tools.modify_window(info, spec);
    }

    void unhide(Window w) {
        auto it = hidden_states.find(w);
        if (it == hidden_states.end()) return;
        WindowSpecification spec;
        spec.state() = it->second;
        hidden_states.erase(it);
        tools.modify_window(tools.info_for(w), spec);
    }

    // `animate`: GNOME's horizontal slide (the old workspace's windows go
    // out one side, the new one's come in from the other). `pinned` is a
    // window being moved along, which stays still while the rest slide.
    void switch_to(size_t index, bool animate = false, Window const& pinned = Window{}) {
        if (index >= workspaces.size() || index == current) return;
        finish_slide();
        applying = true;
        guarded("switch workspace", [&] {
            std::vector<Window> leaving, arriving;
            tools.for_each_window_in_workspace(workspaces[current], [&](Window const& w) { leaving.push_back(w); });
            tools.for_each_window_in_workspace(workspaces[index], [&](Window const& w) { arriving.push_back(w); });
            int const dir = index > current ? 1 : -1;
            current = index;
            // Drop focus before anything is hidden: hiding the active window
            // makes Mir pass focus on, possibly to a window that is already
            // hidden, which brings that one back on screen.
            tools.select_active_window(Window{});
            Slide slide;
            for (auto const& w : leaving) {
                if (in_workspace(w, workspaces[index])) continue;
                auto const st = tools.info_for(w).state();
                if (!animate || st == mir_window_state_hidden || st == mir_window_state_minimized) hide(w);
                else slide.leaving.emplace_back(w, w.top_left());
            }
            for (auto const& w : arriving) {
                unhide(w);
                auto const st = tools.info_for(w).state();
                if (animate && w != pinned && st != mir_window_state_hidden && st != mir_window_state_minimized)
                    slide.arriving.emplace_back(w, w.top_left());
            }
            focus_current_workspace();
            if (!slide.leaving.empty() || !slide.arriving.empty()) {
                slide.distance = dir * primary_output().size.width.as_int();
                slide.start = std::chrono::steady_clock::now();
                slide.generation = ++slide_generation;
                this->slide = std::move(slide);
                step_slide(this->slide->generation);
                run_slide(this->slide->generation);
            }
        });
        applying = false;
        relayout_workspaces();
    }

    // ---- workspace slide ---------------------------------------------------

    struct Slide {
        std::vector<std::pair<Window, geom::Point>> leaving, arriving;
        int distance = 0;
        std::chrono::steady_clock::time_point start;
        uint64_t generation = 0;
    };
    std::optional<Slide> slide;
    uint64_t slide_generation = 0;
    bool animating = false;
    // gnome-shell's WORKSPACE_SWITCH_TIME.
    // A little longer than gnome-shell's 250 ms: our frames come from a
    // timer, not the display's vsync, and 250 ms read as a jump.
    static constexpr double SLIDE_MS = 400.0;

    // Focus the most recently used visible window on this workspace, or
    // nothing when it is empty (as GNOME does).
    void focus_current_workspace() {
        for (auto const& w : mru) {
            if (!on_current_workspace(w)) continue;
            auto const st = tools.info_for(w).state();
            if (st == mir_window_state_hidden || st == mir_window_state_minimized) continue;
            tools.select_active_window(w);
            return;
        }
        tools.select_active_window(Window{});
    }

    void place(Window const& w, geom::Point p) {
        if (!all_windows.count(w)) return;
        WindowSpecification spec;
        spec.top_left() = p;
        tools.modify_window(tools.info_for(w), spec);
    }

    // One frame; false once the slide is over (or replaced).
    bool step_slide(uint64_t generation) {
        if (!slide || slide->generation != generation) return false;
        double const t = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - slide->start).count() / SLIDE_MS;
        if (t >= 1.0) {
            finish_slide();
            return false;
        }
        double const e = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t); // ease-out cubic
        int const out = static_cast<int>(-slide->distance * e);
        int const in = static_cast<int>(slide->distance * (1.0 - e));
        animating = true;
        guarded("slide", [&] {
            for (auto const& [w, p] : slide->leaving) place(w, geom::Point{p.x.as_int() + out, p.y.as_int()});
            for (auto const& [w, p] : slide->arriving) place(w, geom::Point{p.x.as_int() + in, p.y.as_int()});
        });
        animating = false;
        return true;
    }

    // Put everything where it belongs and hide what left.
    void finish_slide() {
        if (!slide) return;
        auto done = std::move(*slide);
        slide.reset();
        animating = true;
        bool const was_applying = applying;
        applying = true;
        guarded("finish slide", [&] {
            Window const active = tools.active_window();
            bool const active_leaving = std::any_of(done.leaving.begin(), done.leaving.end(),
                                                    [&](auto const& l) { return l.first == active; });
            if (active_leaving) tools.select_active_window(Window{});
            for (auto const& [w, p] : done.leaving) {
                place(w, p);
                if (all_windows.count(w) && !in_workspace(w, workspaces[current])) hide(w);
            }
            for (auto const& [w, p] : done.arriving) place(w, p);
            if (active_leaving || !tools.active_window()) focus_current_workspace();
        });
        applying = was_applying;
        animating = false;
    }

    // Frames come from a helper thread that takes the WM lock for each one.
    void run_slide(uint64_t generation) {
        std::thread([generation] {
            for (;;) {
                std::this_thread::sleep_for(std::chrono::milliseconds(16));
                bool more = false;
                guarded("slide frame", [&] {
                    if (!g_wm) return;
                    g_wm->invoke([&] {
                        if (g_wm) more = g_wm->step_slide(generation);
                    });
                });
                if (!more) return;
            }
        }).detach();
    }

    void move_active_to(size_t index) { move_window_to(tools.active_window(), index); }

public:
    void workspace_state(uint32_t* out_current, uint32_t* out_count) {
        if (out_current) *out_current = static_cast<uint32_t>(current);
        if (out_count) *out_count = static_cast<uint32_t>(workspaces.size());
    }

    // Switch from outside a key binding (org.umoja.Compositor), with the popup.
    void switch_workspace(size_t index) {
        if (index >= workspaces.size()) return;
        switch_to(index, true);
        umoja_workspace_switched(current, workspaces.size());
    }

    void move_to_workspace(Window w, size_t index) {
        if (index >= workspaces.size()) return;
        move_window_to(w, index);
        umoja_workspace_switched(current, workspaces.size());
    }

    // The overview's thumbnails: switch quietly, the overview is the feedback.
    void show_workspace(size_t index) {
        if (index >= workspaces.size()) return;
        switch_to(index);
    }

    // The overview's drag and drop: the window goes, the view stays.
    void send_to_workspace(Window w, size_t index) {
        if (!w || index >= workspaces.size() || !all_windows.count(w)) return;
        finish_slide();
        if (!in_any_workspace(w) || in_workspace(w, workspaces[index])) return;
        guarded("send to workspace", [&] {
            std::vector<std::shared_ptr<Workspace>> containing;
            tools.for_each_workspace_containing(w, [&](std::shared_ptr<Workspace> const& c) { containing.push_back(c); });
            for (auto const& ws : containing) tools.remove_tree_from_workspace(w, ws);
            tools.add_tree_to_workspace(w, workspaces[index]);
            applying = true;
            if (index == current) {
                unhide(w);
                tools.raise_tree(w);
            } else {
                hide(w);
            }
            applying = false;
        });
        relayout_workspaces();
    }

    // org.gnome.mutter dynamic-workspaces / workspaces-only-on-primary or
    // org.gnome.desktop.wm.preferences num-workspaces changed.
    void workspace_settings_changed() {
        guarded("workspace settings", [&] {
            for (auto const& w : all_windows) update_workspace_membership(w);
        });
        relayout_workspaces();
    }

private:
    void move_window_to(Window w, size_t index) {
        if (!w || index >= workspaces.size() || !all_windows.count(w)) return;
        if (!in_any_workspace(w)) return; // sticky (secondary monitor)
        tools.remove_tree_from_workspace(w, workspaces[current]);
        tools.add_tree_to_workspace(w, workspaces[index]);
        // GNOME follows the window to its new workspace: the workspaces
        // slide while the window stays where it is.
        switch_to(index, true, w);
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
            // Fixed count shrank (or dynamic was switched off with more
            // workspaces than num-workspaces): fold extra workspaces into
            // the last kept one, as mutter does.
            bool folded = false;
            while (workspaces.size() > desired && workspaces.size() > 1) {
                auto extra = workspaces.back();
                workspaces.pop_back();
                tools.move_workspace_content_to_workspace(workspaces.back(), extra);
                if (current >= workspaces.size()) current = workspaces.size() - 1;
                folded = true;
            }
            if (folded) {
                // Windows folded into the workspace we're on must show up.
                std::vector<Window> here;
                tools.for_each_window_in_workspace(workspaces[current], [&](Window const& w) { here.push_back(w); });
                applying = true;
                for (auto const& w : here) unhide(w);
                applying = false;
            }
        });
        umoja_workspaces_changed(current, workspaces.size());
    }

    // ---- actions ----------------------------------------------------------

    void untile(Window w) {
        auto const id = window_id(w);
        if (tiled.erase(id)) umoja_tiling_unregister(id);
    }

    // Put a tiled window back where it was before it was tiled.
    bool restore_untiled(WindowInfo& info) {
        Window const w = info.window();
        auto it = untiled.find(w);
        untile(w);
        tile_action.erase(w);
        if (it == untiled.end()) return set_state(info, mir_window_state_restored);
        WindowSpecification spec;
        spec.state() = mir_window_state_restored;
        spec.top_left() = it->second.top_left;
        spec.size() = it->second.size;
        untiled.erase(it);
        applying = true;
        guarded("untile", [&] { tools.modify_window(info, spec); });
        applying = false;
        return true;
    }

    // Returns true when the action was handled here.
    bool apply(uint32_t action) {
        uint32_t kind = 0;
        size_t index = 0;
        if (umoja_workspace_resolve(action, current, workspaces.size(), &kind, &index)) {
            if (kind == UMOJA_WORKSPACE_SWITCH) switch_to(index, true);
            else move_active_to(index);
            // GNOME's workspace switcher popup, for keyboard switches.
            umoja_workspace_switched(current, workspaces.size());
            return true;
        }

        switch (action) {
        case UMOJA_ACTION_TOGGLE_OVERLAY:
        case UMOJA_ACTION_TOGGLE_OVERVIEW:
        case UMOJA_ACTION_TOGGLE_DICTATION:
        case UMOJA_ACTION_SCREENSHOT_UI:
        case UMOJA_ACTION_SCREENSHOT:
        case UMOJA_ACTION_ASK_ABOUT_SCREEN:
        case UMOJA_ACTION_CLIPBOARD_HISTORY:
        case UMOJA_ACTION_SHIFT_OVERVIEW_UP:
        case UMOJA_ACTION_SHIFT_OVERVIEW_DOWN:
            umoja_handle_action(action);
            return true;
        default:
            break;
        }

        Window active = tools.active_window();
        if (!active) return false;
        return apply_to(active, action, true);
    }

    bool apply_to(Window target, uint32_t action, bool from_keyboard) {
        auto& info = tools.info_for(target);
        bool const is_tiled = tiled.count(window_id(target)) > 0;

        switch (action) {
        case UMOJA_ACTION_SCREENSHOT_WINDOW:
            umoja_screenshot_window(to_rect({target.top_left(), target.size()}));
            return true;
        case UMOJA_ACTION_CLOSE:
            tools.ask_client_to_close(target);
            return true;
        case UMOJA_ACTION_MINIMIZE:
            return set_state(info, mir_window_state_minimized, false);
        case UMOJA_ACTION_MAXIMIZE:
            return set_state(info, mir_window_state_maximized, false);
        case UMOJA_ACTION_UNMAXIMIZE:
            // Super+Down: untile, or unmaximize (Tiling Assistant's restore-window).
            if (is_tiled) return restore_untiled(info);
            return set_state(info, mir_window_state_restored);
        case UMOJA_ACTION_TOGGLE_MAXIMIZED:
            return set_state(info, info.state() == mir_window_state_maximized ? mir_window_state_restored
                                                                              : mir_window_state_maximized,
                             false);
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

        if (!is_app_window(info) || !umoja_is_tile_action(action)) return false;

        // Tiling a window into the tile it already has untiles it again
        // (mutter's toggle-tiled-*, Tiling Assistant's toggleTiling()).
        if (from_keyboard && is_tiled) {
            auto it = tile_action.find(target);
            if (it != tile_action.end() && it->second == action) return restore_untiled(info);
        }

        // The work area of the monitor the window is on.
        geom::Rectangle area = tools.active_application_zone().extents();
        for (auto const& [id, r] : zones)
            if (r.contains(centre_of(target))) area = r;
        UmojaRect out{};
        if (!umoja_tile_geometry(action, to_rect(area), &out)) return false;

        if (!is_tiled && !untiled.count(target)) {
            if (info.state() == mir_window_state_restored)
                untiled[target] = geom::Rectangle{target.top_left(), target.size()};
            else
                untiled[target] = info.restore_rect();
        }

        WindowSpecification spec;
        spec.state() = mir_window_state_restored;
        spec.top_left() = geom::Point{out.x, out.y};
        spec.size() = geom::Size{out.w, out.h};
        applying = true;
        guarded("tile", [&] { tools.modify_window(info, spec); });
        applying = false;

        auto const id = window_id(target);
        tiled[id] = target;
        tile_action[target] = action;
        umoja_tiling_register(id, action, out);
        return true;
    }

    bool move_to_monitor(WindowInfo& info, uint32_t action) {
        if (outputs.size() < 2) return false;
        Window const w = info.window();
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

    bool set_state(WindowInfo& info, MirWindowState state, bool forget_untiled = true) {
        Window const w = info.window();
        untile(w);
        tile_action.erase(w);
        if (forget_untiled) untiled.erase(w);
        WindowSpecification spec;
        spec.state() = state;
        tools.place_and_size_for_state(spec, info);
        tools.modify_window(info, spec);
        return true;
    }
};

} // namespace

template <typename F>
void with_wm(char const* what, F&& f) {
    guarded(what, [&] {
        if (!g_wm) return;
        g_wm->invoke([&] {
            if (g_wm) guarded(what, f);
        });
    });
}

extern "C" void umoja_wm_list_windows(UmojaWindowCallback cb, void* ctx, bool mru) {
    if (!cb) return;
    with_wm("list windows", [&] { g_wm->list_windows(cb, ctx, mru); });
}

extern "C" void umoja_wm_activate(uint64_t id) {
    with_wm("activate window", [&] { g_wm->activate(g_wm->find(id)); });
}

extern "C" void umoja_wm_close(uint64_t id) {
    with_wm("close window", [&] { g_wm->close(g_wm->find(id)); });
}

extern "C" void umoja_wm_switcher_pick(uint32_t index) {
    with_wm("switcher pick", [&] { g_wm->switcher_pick(index); });
}

extern "C" void umoja_wm_tile(uint64_t id, uint32_t action) {
    with_wm("tile window", [&] { g_wm->tile(g_wm->find(id), action); });
}

extern "C" void umoja_wm_workspaces(uint32_t* out_current, uint32_t* out_count) {
    if (out_current) *out_current = 0;
    if (out_count) *out_count = 0;
    with_wm("workspaces", [&] { g_wm->workspace_state(out_current, out_count); });
}

extern "C" void umoja_wm_switch_workspace(uint32_t index) {
    with_wm("switch workspace", [&] { g_wm->switch_workspace(index); });
}

extern "C" void umoja_wm_move_to_workspace(uint64_t id, uint32_t index) {
    with_wm("move to workspace", [&] { g_wm->move_to_workspace(g_wm->find(id), index); });
}

extern "C" void umoja_wm_show_workspace(uint32_t index) {
    with_wm("show workspace", [&] { g_wm->show_workspace(index); });
}

extern "C" void umoja_wm_send_to_workspace(uint64_t id, uint32_t index) {
    with_wm("send to workspace", [&] { g_wm->send_to_workspace(g_wm->find(id), index); });
}

extern "C" void umoja_wm_workspace_settings_changed(void) {
    with_wm("workspace settings changed", [&] { g_wm->workspace_settings_changed(); });
}

namespace {
// ext_data_control lets a client read every copy and selection in the
// background, so only Umoja's own shell binaries (umoja-overlay's clipboard
// history) get it, never ordinary apps.
bool is_umoja_shell_client(WaylandExtensions::EnableInfo const& info) {
    pid_t const pid = miral::pid_of(info.app());
    if (pid <= 0) return false;
    char exe[PATH_MAX];
    auto const proc = "/proc/" + std::to_string(pid) + "/exe";
    ssize_t const n = readlink(proc.c_str(), exe, sizeof exe - 1);
    if (n <= 0) return false;
    exe[n] = '\0';
    std::string const path{exe};
    auto const slash = path.rfind('/');
    std::string const name = slash == std::string::npos ? path : path.substr(slash + 1);
    return name.rfind("umoja-", 0) == 0;
}

bool is_privileged_extension(std::string const& name) {
    return name == "ext_data_control_manager_v1" || name == "zwlr_data_control_manager_v1";
}
}

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
        if (is_privileged_extension(name)) {
            extensions.conditionally_enable(name, is_umoja_shell_client);
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
    PrependEventFilter natural_scroll{umoja_apply_natural_scroll};

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

    auto const idle = [](mir::Server& server) { umoja_idle_setup(server); };

    if (config->enable_x11) {
        return runner.run_with({
            idle,
            display_config,
            display_config.layout_option(),
            natural_scroll,
            extensions,
            X11Support{},
            set_window_management_policy<UmojaWindowManager>(),
        });
    }
    return runner.run_with({
        idle,
        display_config,
        display_config.layout_option(),
        natural_scroll,
        extensions,
        set_window_management_policy<UmojaWindowManager>(),
    });
}

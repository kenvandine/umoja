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
#include <miral/cursor_scale.h>
#include <miral/cursor_theme.h>
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
#include <atomic>
#include <chrono>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
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

// Window-management tracing, for debugging how apps (Wine games, ...) ask
// to be placed. On while $XDG_RUNTIME_DIR/umoja-debug-windows exists, so it
// can be switched on and off without restarting the session. Goes to the
// journal (journalctl -t umoja-mir).
bool debug_windows() {
    static auto last = std::chrono::steady_clock::time_point{};
    static bool on = false;
    auto const now = std::chrono::steady_clock::now();
    if (now - last > std::chrono::seconds(1)) {
        last = now;
        char const* dir = std::getenv("XDG_RUNTIME_DIR");
        on = dir && access((std::string{dir} + "/umoja-debug-windows").c_str(), F_OK) == 0;
    }
    return on;
}

std::string describe(WindowSpecification const& s) {
    std::string out;
    char buf[96];
    if (s.state().is_set()) out += " state=" + std::to_string(int(s.state().value()));
    if (s.top_left().is_set()) {
        std::snprintf(buf, sizeof buf, " pos=%d,%d", s.top_left().value().x.as_int(), s.top_left().value().y.as_int());
        out += buf;
    }
    if (s.size().is_set()) {
        std::snprintf(buf, sizeof buf, " size=%dx%d", s.size().value().width.as_int(), s.size().value().height.as_int());
        out += buf;
    }
    if (s.output_id().is_set()) out += " output=" + std::to_string(s.output_id().value());
    if (s.depth_layer().is_set()) out += " layer=" + std::to_string(int(s.depth_layer().value()));
    return out.empty() ? " (nothing)" : out;
}

std::string describe(WindowInfo const& info) {
    auto const w = info.window();
    char buf[160];
    std::snprintf(buf, sizeof buf, "'%.40s' state=%d at %d,%d %dx%d", info.name().c_str(), int(info.state()),
                  w.top_left().x.as_int(), w.top_left().y.as_int(), w.size().width.as_int(), w.size().height.as_int());
    return buf;
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
        // Modified clicks (Super+drag) are ours, following GNOME's
        // mouse-button-modifier; caps_lock is outside the modifiers Mir
        // compares, so its own Super+drag never starts.
        : MinimalWindowManager{tools, mir_input_event_modifier_caps_lock} {
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
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: new window '%s' asked%s, Mir placed%s\n",
                         requested.name().is_set() ? requested.name().value().c_str() : "",
                         describe(requested).c_str(), describe(spec).c_str());
        // org.gnome.mutter center-new-windows
        bool const toplevel = !requested.parent().is_set() || !requested.parent().value().lock();
        // Not a window that maps fullscreen or maximized: Mir has already put
        // it over the monitor or work area, and centring a monitor-sized
        // window in the work area left fullscreen games offset by the panel
        // and dock (36,16).
        bool const placed_by_state = spec.state().is_set() && spec.state().value() != mir_window_state_restored;
        if (toplevel && !placed_by_state && umoja_center_new_windows() && spec.size().is_set()) {
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
        if (key_grab && key_grab->window == w) key_grab.reset();
        above.erase(w);
        fullscreen_lifted.erase(w);
        pinned.erase(w);
        hidden_at.erase(w);
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
        update_fullscreen_layer(w, info.state(), true);
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

    void advise_focus_lost(WindowInfo const& info) override {
        MinimalWindowManager::advise_focus_lost(info);
        update_fullscreen_layer(info.window(), info.state(), false);
    }

    void advise_state_change(WindowInfo const& info, MirWindowState state) override {
        MinimalWindowManager::advise_state_change(info, state);
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: state of %s -> %d\n", describe(info).c_str(), int(state));
        update_fullscreen_layer(info.window(), state, info.window() == tools.active_window());
    }

    // A focused fullscreen window covers the dock and top bar, as in GNOME.
    // Those are layer-shell "top" surfaces, which Mir puts in the layer
    // above applications, so the window goes up to that layer while it is
    // fullscreen and focused, and back down once it is neither. Leaves its
    // layer to the window menu's Always on Top otherwise.
    //
    // This is called from Mir's focus and state callbacks, in the middle of
    // select_active_window()/modify_window(). Changing the window's layer
    // there pulls the scene out from under Mir (it aborted at login when
    // the greeter's fullscreen window took focus), so the change is applied
    // afterwards from a helper thread that takes the WM lock, as the
    // workspace slide does.
    std::set<Window> fullscreen_lifted;

    void update_fullscreen_layer(Window const& w, MirWindowState state, bool focused) {
        if (!all_windows.count(w)) return;
        bool const want = focused && state == mir_window_state_fullscreen;
        if (want == bool(fullscreen_lifted.count(w))) return;
        if (want) fullscreen_lifted.insert(w);
        else fullscreen_lifted.erase(w);
        Window const target = w;
        std::thread([target] {
            guarded("fullscreen layer", [&] {
                if (!g_wm) return;
                g_wm->invoke([&] {
                    if (g_wm) g_wm->apply_layer(target);
                });
            });
        }).detach();
    }

    void apply_layer(Window const& w) {
        if (!all_windows.count(w)) return;
        if (w == moving && elevated) return;  // end_move() puts it back
        auto& info = tools.info_for(w);
        auto const layer = layer_for(w);
        if (info.depth_layer() == layer) return;
        WindowSpecification spec;
        spec.depth_layer() = layer;
        guarded("fullscreen layer", [&] {
            tools.modify_window(info, spec);
            if (fullscreen_lifted.count(w)) tools.raise_tree(w);
        });
    }

    MirDepthLayer layer_for(Window const& w) const {
        return above.count(w) || fullscreen_lifted.count(w) ? mir_depth_layer_above : mir_depth_layer_application;
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
        // mutter's "Above_Tab" is the key above Tab on any layout, so
        // Super+` and Alt+` work on non-US keyboards too.
        uint32_t const bind_sym = mir_keyboard_event_scan_code(event) == KEY_ABOVE_TAB ? XK_GRAVE : keysym;

        // Alt+F7 / Alt+F8: the arrow keys move or resize the window.
        if (key_grab && handle_key_grab(action, keysym, mods)) return true;

        // While Alt+Tab is open it sees every key, releases included.
        {
            UmojaSwitchResult res{};
            bool const down = action != mir_keyboard_action_up;
            uint32_t const r = umoja_switcher_key(bind_sym, mods, down, &res);
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

        // Volume, brightness and playback keys work whatever has focus, as
        // in GNOME. Volume and brightness keep stepping while held. The
        // release is swallowed too, since the client never saw the press.
        uint32_t const bound = umoja_lookup_binding(bind_sym, mods);
        if (uint32_t const media = umoja_media_action(bound); media != UMOJA_MEDIA_NONE) {
            if (action == mir_keyboard_action_down ||
                (action == mir_keyboard_action_repeat && media == UMOJA_MEDIA_REPEATS))
                umoja_media_key(bound);
            return true;
        }

        if (action == mir_keyboard_action_up) return MinimalWindowManager::handle_keyboard_event(event);
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

        uint32_t const buttons = mir_pointer_event_buttons(event);
        uint32_t const pressed = buttons & ~held_buttons;
        held_buttons = buttons;

        // Moving with Alt+F7: the pointer moves the window too, a click drops it.
        if (key_grab) {
            if (action == mir_pointer_action_button_down) {
                key_grab.reset();
                return true;
            }
            if (action == mir_pointer_action_motion) {
                key_grab_pointer(px, py);
                return true;
            }
            if (action == mir_pointer_action_button_up) return true;
        }
        if (action == mir_pointer_action_button_down) {
            umoja_pointer_pressed();
            // Super+click is not a tap of Super: no overview on release.
            overlay_key_armed = false;
        }

        if (mod_grab) return mod_grab_event(action, buttons, px, py);
        if (action == mir_pointer_action_button_down && pressed && !moving &&
            begin_mod_grab(event, pressed, px, py))
            return true;

        bool const consumed = MinimalWindowManager::handle_pointer_event(event);

        // A scripted drag (DragWindow) ignores the real pointer.
        if (action == mir_pointer_action_motion) {
            if (moving && !scripted) on_move(px, py);
            else if (!moving) report_motion(event, px, py);
        } else if (action == mir_pointer_action_button_up) {
            if (moving && !scripted && mir_pointer_event_buttons(event) == 0) end_move();
        }
        return consumed;
    }

    // GNOME's touchscreen gestures (three-finger swipes, edge drags):
    // gestures.rs decides, the workspace swipe happens here.
    bool handle_touch_event(MirTouchEvent const* event) override {
        UmojaTouchPoint pts[16];
        size_t const n = std::min<size_t>(mir_touch_event_point_count(event), 16);
        for (size_t i = 0; i < n; ++i) {
            pts[i].id = mir_touch_event_id(event, i);
            pts[i].action = static_cast<uint32_t>(mir_touch_event_action(event, i));
            pts[i].x = mir_touch_event_axis_value(event, i, mir_touch_axis_x);
            pts[i].y = mir_touch_event_axis_value(event, i, mir_touch_axis_y);
        }
        auto const r = primary_output();
        UmojaRect const screen{r.top_left.x.as_int(), r.top_left.y.as_int(), r.size.width.as_int(),
                               r.size.height.as_int()};
        auto const time = mir_input_event_get_event_time(mir_touch_event_input_event(event));
        UmojaTouchResult res{};
        bool const consume = umoja_touch(pts, n, static_cast<uint64_t>(time), screen, &res);
        apply_swipe(res.swipe, res.progress, res.dir);
        if (consume) return true;
        return MinimalWindowManager::handle_touch_event(event);
    }

    // A window hidden because it is on another workspace must stay hidden
    // whatever its client asks. Chromium/Electron and Qt apps answer the
    // configure that hiding sends by re-requesting their own state (e.g.
    // "restored"), which Mir would apply and so un-hide the window on the
    // workspace it just left. Remember the request for when it comes back.
    //
    // A fullscreen window fills its monitor whatever size it asks for, as
    // mutter does. Wine/Proton games map an X11 window with
    // _NET_WM_STATE_FULLSCREEN and then keep sending ConfigureRequests for
    // their small pre-map size (314x208 for a Unity game); Mir applied those
    // on top of the fullscreen state, leaving a tiny "fullscreen" window at
    // the bottom of the screen until the game resized itself much later.
    void handle_modify_window(WindowInfo& info, WindowSpecification const& modifications) override {
        WindowSpecification spec = modifications;
        bool const trace = debug_windows();
        // Only while the window already is, and stays, fullscreen on the same
        // monitor: for a change *to* fullscreen Mir has already put the
        // monitor's rectangle in the request, and dropping it left a window
        // that went fullscreen a second time at its restored size.
        bool const stays_fullscreen =
            info.state() == mir_window_state_fullscreen &&
            (!spec.state().is_set() || spec.state().value() == mir_window_state_fullscreen) &&
            (!spec.output_id().is_set() || (info.has_output_id() && spec.output_id().value() == info.output_id()));
        if (stays_fullscreen && (spec.size().is_set() || spec.top_left().is_set())) {
            // consume() on an unset optional is a Mir fatal error, and X11
            // ConfigureRequests often carry only one of the two.
            if (spec.size().is_set()) spec.size().consume();
            if (spec.top_left().is_set()) spec.top_left().consume();
            // Snap back to its monitor in case an earlier request got through.
            auto const w = info.window();
            auto const out = output_at(centre_of(w));
            if (out.size != geom::Size{} && out != geom::Rectangle{w.top_left(), w.size()}) {
                spec.top_left() = out.top_left;
                spec.size() = out.size;
            }
        }
        auto it = hidden_states.find(info.window());
        if (it != hidden_states.end() && spec.state().is_set()) {
            auto const requested = spec.state().consume();
            if (requested != mir_window_state_hidden) it->second = requested;
        }
        if (trace)
            std::fprintf(stderr, "umoja-mir: wm: request for %s:%s -> applying%s\n", describe(info).c_str(),
                         describe(modifications).c_str(), describe(spec).c_str());
        MinimalWindowManager::handle_modify_window(info, spec);
    }

    // Activation requests from windows on another workspace don't pull them
    // onto this one (GNOME shows a "ready" notice instead of switching).
    //
    // Other activations of a window on another workspace (the dock's icon
    // via foreign-toplevel, an xdg-activation token from a launcher) take
    // you to its workspace and focus it, as GNOME does. Chromium/Electron
    // and Qt re-raise themselves right after being hidden; those don't.
    void handle_raise_window(WindowInfo& info) override {
        Window const w = info.window();
        if (is_app_window(info) && on_other_workspace(w)) {
            auto const it = hidden_at.find(w);
            if (it != hidden_at.end() && std::chrono::steady_clock::now() - it->second < std::chrono::seconds(1))
                return;
            activate(w);
            return;
        }
        MinimalWindowManager::handle_raise_window(info);
    }

    bool on_other_workspace(Window const& w) {
        if (in_workspace(w, workspaces[current])) return false;
        for (size_t i = 0; i < workspaces.size(); ++i)
            if (i != current && in_workspace(w, workspaces[i])) return true;
        return false;
    }

    void handle_request_move(WindowInfo& info, MirInputEvent const* input_event) override {
        MinimalWindowManager::handle_request_move(info, input_event);
        if (!all_windows.count(info.window()) || mir_input_event_get_type(input_event) != mir_input_event_type_pointer)
            return;
        auto const* pev = mir_input_event_get_pointer_event(input_event);
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_y));
        guarded("begin move", [&] { begin_move(info.window(), px, py, was_tiled_or_maximized(info.window())); });
    }

    // ---- tiling groups ----------------------------------------------------

    void advise_resize(WindowInfo const& info, geom::Size const& size) override {
        MinimalWindowManager::advise_resize(info, size);
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: resize of %s -> %dx%d\n", describe(info).c_str(),
                         size.width.as_int(), size.height.as_int());
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
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: move of %s -> %d,%d\n", describe(info).c_str(),
                         top_left.x.as_int(), top_left.y.as_int());
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
                    switch_to(i, true);
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

    // org.umoja.Compositor.DragWindow: a title-bar drag driven by points
    // instead of the pointer, through the same drag-to-snap path (tile
    // preview, layout picker, drop). Phase 0 grabs the window's title bar
    // and returns the grab point, 1 moves the grab point to (x, y), 2 drops.
    bool script_drag(Window w, uint32_t phase, int32_t x, int32_t y, int32_t* gx, int32_t* gy) {
        switch (phase) {
        case 0: {
            if (!w || moving || !all_windows.count(w)) return false;
            if (tools.info_for(w).state() == mir_window_state_fullscreen) return false;
            int32_t const px = w.top_left().x.as_int() + w.size().width.as_int() / 2;
            int32_t const py = w.top_left().y.as_int() + 16;
            activate(w);
            begin_move(w, px, py, was_tiled_or_maximized(w));
            if (moving != w) return false;
            scripted = true;
            script_last = geom::Point{px, py};
            if (gx) *gx = px;
            if (gy) *gy = py;
            return true;
        }
        case 1: {
            if (!w || moving != w || !scripted) return false;
            geom::Point const p{x, y};
            tools.drag_window(w, p - script_last);
            script_last = p;
            on_move(x, y);
            return true;
        }
        default:
            if (moving && scripted) end_move();
            return true;
        }
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
    // Window menu: Always on Top, Always on Visible Workspace.
    std::set<Window> above;
    std::set<Window> pinned;

    // Alt+F7 / Alt+F8 (mutter's keyboard move and resize grabs).
    struct KeyGrab {
        Window window;
        bool resize = false;
        geom::Rectangle original;
        int hedge = 0; // resize: -1 left edge, +1 right edge, 0 not picked yet
        int vedge = 0; // -1 top, +1 bottom
        bool have_pointer = false;
        geom::Point pointer;
    };
    std::optional<KeyGrab> key_grab;
    bool raising_group = false;
    std::map<uint64_t, Window> tiled;
    std::map<Window, uint32_t> tile_action;
    // Geometry before a window was tiled, restored when it is dragged out or
    // untiled with the keyboard (Tiling Assistant's "untiledRect").
    std::map<Window, geom::Rectangle> untiled;

    // Pointer press / window move tracking.
    uint32_t held_buttons = 0;
    Window moving;
    bool elevated = false;
    // The drag is org.umoja.Compositor.DragWindow's, not the pointer's.
    bool scripted = false;
    geom::Point script_last;
    uint32_t drag_target = UMOJA_ACTION_NONE;

    std::vector<Window> stacking; // bottom to top
    std::vector<Window> mru;      // most recently focused first

    std::vector<std::shared_ptr<Workspace>> workspaces;
    size_t current = 0;
    std::set<Window> all_windows;
    std::map<Window, MirWindowState> hidden_states;
    // When each window was last hidden for a workspace switch.
    std::map<Window, std::chrono::steady_clock::time_point> hidden_at;
    std::map<int, geom::Rectangle> outputs;
    std::map<int, geom::Rectangle> zones;

    // Mir has no primary-monitor concept; umoja-mir tracks it as mutter does
    // (Settings › Displays, monitors.xml, built-in panel). Fall back to the
    // monitor at the origin.
    geom::Rectangle primary_output() const {
        if (auto const it = outputs.find(umoja_primary_output_id()); it != outputs.end()) return it->second;
        for (auto const& [id, r] : outputs)
            if (r.contains(geom::Point{0, 0})) return r;
        return outputs.empty() ? geom::Rectangle{} : outputs.begin()->second;
    }

    bool on_primary(Window const& w) const {
        return outputs.size() < 2 || primary_output().contains(centre_of(w));
    }

    // GNOME's pressure barrier on the outer screen edges (edge_pressure.rs):
    // pass the motion and which edges hold the pointer back.
    void report_motion(MirPointerEvent const* event, int32_t px, int32_t py) {
        if (outputs.empty()) return;
        geom::Point const p{px, py};
        geom::Rectangle const o = output_at(p);
        if (!o.contains(p)) return;
        auto const open = [&](int32_t x, int32_t y) {
            for (auto const& [id, r] : outputs)
                if (r.contains(geom::Point{x, y})) return false;
            return true;
        };
        int32_t const left = o.top_left.x.as_int();
        int32_t const top = o.top_left.y.as_int();
        int32_t const right = left + o.size.width.as_int() - 1;
        int32_t const bottom = top + o.size.height.as_int() - 1;
        uint32_t edges = 0;
        if (px <= left && open(left - 1, py)) edges |= UMOJA_EDGE_LEFT;
        if (px >= right && open(right + 1, py)) edges |= UMOJA_EDGE_RIGHT;
        if (py <= top && open(px, top - 1)) edges |= UMOJA_EDGE_TOP;
        if (py >= bottom && open(px, bottom + 1)) edges |= UMOJA_EDGE_BOTTOM;
        umoja_pointer_motion(px, py, mir_pointer_event_axis_value(event, mir_pointer_axis_relative_x),
                             mir_pointer_event_axis_value(event, mir_pointer_axis_relative_y), edges);
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
        if (umoja_switcher_open(action, mods, entries.data(), entries.size())) preview(umoja_switcher_preview());
    }

    // Window cycler (Alt+Esc): show the selected window on top for now.
    void preview(uint64_t id) {
        if (!id) return;
        Window const w = find(id);
        if (w && visible_here(w)) guarded("cycle preview", [&] { tools.raise_tree(w); });
    }

    void apply_switch(uint32_t code, UmojaSwitchResult const& res) {
        switch (code) {
        case UMOJA_SWITCH_ACTIVATE:
            umoja_pointer_pressed(); // hand the keyboard back from the top bar
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
        case UMOJA_SWITCH_PREVIEW:
            preview(res.window);
            break;
        default:
            break;
        }
    }

    // ---- modifier + click (mutter's window button grabs) --------------------

    // Holding mouse-button-modifier (<Super> in GNOME and Ubuntu) anywhere
    // on a window: left drag moves it, middle drag resizes it from the
    // nearest edge or corner, right click opens the window menu; with
    // resize-with-right-button, right resizes and middle opens the menu.
    // As in mutter (meta_window_handle_ungrabbed_event), the press focuses
    // the window and never reaches the client, and once a drag has started
    // letting go of the modifier does not end it.
    enum class ModGrabKind { menu, pending_move, moving, resizing };
    struct ModGrab {
        Window window;
        ModGrabKind kind;
        uint32_t button;
        geom::Point start;     // pointer at the press
        geom::Point last;      // pointer at the previous motion
        geom::Rectangle rect;  // window at the start of a resize
        bool resized = false;
        bool west = false, east = false, north = false, south = false;
    };
    std::optional<ModGrab> mod_grab;

    // mutter's "shake" threshold before a maximized or tiled window comes
    // loose; a restored window follows the pointer straight away.
    static constexpr int unmaximize_threshold = 8;

    bool begin_mod_grab(MirPointerEvent const* event, uint32_t pressed, int32_t px, int32_t py) {
        uint32_t const want = umoja_mouse_button_modifier();
        if (!want) return false;
        uint32_t const held = to_umoja_mods(mir_pointer_event_modifiers(event));
        if ((held & want) != want) return false;

        geom::Point const p{px, py};
        Window const w = tools.window_at(p);
        if (!w || !all_windows.count(w)) return false;
        auto& info = tools.info_for(w);

        bool const right_resizes = umoja_resize_with_right_button();
        uint32_t const resize_button = right_resizes ? mir_pointer_button_secondary : mir_pointer_button_tertiary;
        uint32_t const menu_button = right_resizes ? mir_pointer_button_tertiary : mir_pointer_button_secondary;
        auto const state = info.state();
        bool const fullscreen = state == mir_window_state_fullscreen;

        if (pressed & menu_button) {
            guarded("modifier click menu", [&] {
                activate(w);
                show_window_menu(w, p);
            });
            // Swallow the release too.
            mod_grab = ModGrab{Window{}, ModGrabKind::menu, menu_button, p, p, {}};
            return true;
        }

        if (pressed & resize_button) {
            if (fullscreen || !resizable(info) || state != mir_window_state_restored) return false;
            geom::Rectangle const r{w.top_left(), w.size()};
            int const x = r.top_left.x.as_int(), y = r.top_left.y.as_int();
            int const ww = r.size.width.as_int(), wh = r.size.height.as_int();
            ModGrab g{w, ModGrabKind::resizing, resize_button, p, p, r};
            g.west = px < x + ww / 3;
            g.east = px > x + 2 * ww / 3;
            g.north = py < y + wh / 3;
            g.south = py > y + 2 * wh / 3;
            // The middle third does nothing in mutter either: the click
            // goes to the window.
            if (!(g.west || g.east || g.north || g.south)) return false;
            guarded("modifier resize", [&] {
                activate(w);
                tools.raise_tree(w);
            });
            mod_grab = g;
            return true;
        }

        if (pressed & mir_pointer_button_primary) {
            if (fullscreen) return false;
            guarded("modifier move", [&] {
                activate(w);
                tools.raise_tree(w);
            });
            mod_grab = ModGrab{w, ModGrabKind::pending_move, mir_pointer_button_primary, p, p, {}};
            return true;
        }
        return false;
    }

    bool mod_grab_event(MirPointerAction action, uint32_t buttons, int32_t px, int32_t py) {
        auto& g = *mod_grab;
        geom::Point const p{px, py};
        bool const gone = g.window && !all_windows.count(g.window);

        if (gone || !(buttons & g.button)) {
            if (g.kind == ModGrabKind::moving && moving) end_move();
            mod_grab.reset();
            return true; // the client never saw the press
        }
        if (action != mir_pointer_action_motion || !g.window) return true;

        switch (g.kind) {
        case ModGrabKind::pending_move: {
            bool const stuck = was_tiled_or_maximized(g.window);
            auto const d = p - g.start;
            if (stuck && std::abs(d.dx.as_int()) < unmaximize_threshold &&
                std::abs(d.dy.as_int()) < unmaximize_threshold)
                return true;
            g.kind = ModGrabKind::moving;
            Window const w = g.window;
            guarded("begin modifier move", [&] { begin_move(w, px, py, stuck); });
            if (!stuck) guarded("modifier move", [&] { tools.drag_window(w, p - g.last); });
            g.last = p;
            if (moving) on_move(px, py);
            return true;
        }
        case ModGrabKind::menu:
            return true;
        case ModGrabKind::moving:
            if (moving == g.window) {
                Window const w = g.window;
                guarded("modifier move", [&] { tools.drag_window(w, p - g.last); });
                on_move(px, py);
            }
            g.last = p;
            return true;
        case ModGrabKind::resizing:
            if (!g.resized) {
                // A resized tile leaves its tile, as with the frame's edges.
                g.resized = true;
                untile(g.window);
                untiled.erase(g.window);
                tile_action.erase(g.window);
            }
            guarded("modifier resize", [&] { apply_mod_resize(g, p); });
            g.last = p;
            return true;
        }
        return true;
    }

    void apply_mod_resize(ModGrab const& g, geom::Point p) {
        auto& info = tools.info_for(g.window);
        int const dx = (p - g.start).dx.as_int(), dy = (p - g.start).dy.as_int();
        int x = g.rect.top_left.x.as_int(), y = g.rect.top_left.y.as_int();
        int const w0 = g.rect.size.width.as_int(), h0 = g.rect.size.height.as_int();
        int w = w0, h = h0;
        if (g.east) w += dx;
        if (g.west) w -= dx;
        if (g.south) h += dy;
        if (g.north) h -= dy;
        w = std::clamp(w, std::max(1, info.min_width().as_int()), std::max(1, info.max_width().as_int()));
        h = std::clamp(h, std::max(1, info.min_height().as_int()), std::max(1, info.max_height().as_int()));
        // The opposite edge stays put.
        if (g.west) x += w0 - w;
        if (g.north) y += h0 - h;
        WindowSpecification spec;
        spec.top_left() = geom::Point{x, y};
        spec.size() = geom::Size{w, h};
        tools.modify_window(info, spec);
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
        scripted = false;
        uint32_t const target = umoja_drag_end();
        if (!w) return;
        if (elevated) {
            elevated = false;
            WindowSpecification drop;
            drop.depth_layer() = layer_for(w);
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
        bool const sticky = (umoja_workspaces_only_on_primary() && !on_primary(w)) || pinned.count(w);
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
        hidden_at[w] = std::chrono::steady_clock::now();
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
        end_swipe_now();
        applying = true;
        guarded("switch workspace", [&] {
            std::vector<Window> leaving, arriving;
            tools.for_each_window_in_workspace(workspaces[current], [&](Window const& w) { leaving.push_back(w); });
            tools.for_each_window_in_workspace(workspaces[index], [&](Window const& w) { arriving.push_back(w); });
            int const dist = (index > current ? 1 : -1) * primary_output().size.width.as_int();
            current = index;
            // Drop focus before anything is hidden: hiding the active window
            // makes Mir pass focus on, possibly to a window that is already
            // hidden, which brings that one back on screen.
            tools.select_active_window(Window{});
            Slide slide;
            for (auto const& w : leaving) {
                if (in_workspace(w, workspaces[index])) continue;
                if (!animate || !showing(w)) hide(w);
                else slide.items.push_back({w, w.top_left(), 0, -dist, true});
            }
            for (auto const& w : arriving) {
                unhide(w);
                if (animate && w != pinned && showing(w)) slide.items.push_back({w, w.top_left(), dist, 0, false});
            }
            focus_current_workspace();
            start_slide(std::move(slide), SLIDE_MS);
        });
        applying = false;
        relayout_workspaces();
    }

    bool showing(Window const& w) {
        auto const st = tools.info_for(w).state();
        return st != mir_window_state_hidden && st != mir_window_state_minimized;
    }

    // ---- workspace slide ---------------------------------------------------

    // A window sliding from offset `from` to `to` (pixels, horizontal) off
    // its real position `base`, hidden at the end if it left the workspace.
    struct SlideItem {
        Window window;
        geom::Point base;
        int from, to;
        bool hide_after;
    };
    struct Slide {
        std::vector<SlideItem> items;
        double ms = 0;
        std::chrono::steady_clock::time_point start;
        uint64_t generation = 0;
    };
    std::optional<Slide> slide;
    uint64_t slide_generation = 0;
    bool animating = false;
    // A little longer than gnome-shell's 250 ms WORKSPACE_SWITCH_TIME: our
    // frames come from a timer, not the display's vsync, and 250 ms read
    // as a jump.
    static constexpr double SLIDE_MS = 400.0;

    // Focus the most recently used visible window on this workspace, or
    // nothing when it is empty (as GNOME does).
    void focus_current_workspace() {
        for (auto const& w : mru) {
            if (!on_current_workspace(w)) continue;
            if (!showing(w)) continue;
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

    void place_at(SlideItem const& it, int offset) {
        place(it.window, geom::Point{it.base.x.as_int() + offset, it.base.y.as_int()});
    }

    void start_slide(Slide slide, double ms) {
        if (slide.items.empty()) return;
        slide.ms = ms;
        slide.start = std::chrono::steady_clock::now();
        slide.generation = ++slide_generation;
        this->slide = std::move(slide);
        step_slide(this->slide->generation);
        run_slide(this->slide->generation);
    }

    // One frame; false once the slide is over (or replaced).
    bool step_slide(uint64_t generation) {
        if (!slide || slide->generation != generation) return false;
        double const t = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - slide->start).count() / slide->ms;
        if (t >= 1.0) {
            finish_slide();
            return false;
        }
        double const e = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t); // ease-out cubic
        animating = true;
        guarded("slide", [&] {
            for (auto const& it : slide->items) place_at(it, static_cast<int>(it.from + (it.to - it.from) * e));
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
            bool const active_leaving = std::any_of(done.items.begin(), done.items.end(),
                                                    [&](auto const& it) { return it.hide_after && it.window == active; });
            if (active_leaving) tools.select_active_window(Window{});
            for (auto const& it : done.items) {
                place(it.window, it.base);
                if (it.hide_after && all_windows.count(it.window) && !in_workspace(it.window, workspaces[current]))
                    hide(it.window);
            }
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

    // ---- workspace swipe (touch gestures) -----------------------------------
    //
    // gnome-shell's WorkspaceAnimation: while three fingers swipe sideways
    // the current workspace and its neighbours follow them; on release the
    // swipe finishes (or snaps back) with the same slide.

    struct Swipe {
        std::vector<SlideItem> here, left, right;
        int width = 0;
        double progress = 0;
    };
    std::optional<Swipe> swipe;

    void collect(size_t index, std::vector<SlideItem>& out, int base_offset) {
        std::vector<Window> ws;
        tools.for_each_window_in_workspace(workspaces[index], [&](Window const& w) { ws.push_back(w); });
        for (auto const& w : ws) {
            if (index != current && in_workspace(w, workspaces[current])) continue;
            if (index != current) unhide(w);
            if (!showing(w)) continue;
            out.push_back({w, w.top_left(), base_offset, base_offset, index != current});
        }
    }

    void place_swipe(double p) {
        int const off = static_cast<int>(-p * swipe->width);
        animating = true;
        guarded("swipe", [&] {
            for (auto const& it : swipe->here) place_at(it, off);
            for (auto const& it : swipe->left) place_at(it, -swipe->width + off);
            for (auto const& it : swipe->right) place_at(it, swipe->width + off);
        });
        animating = false;
    }

public:
    // `flags`: UMOJA_SWIPE_* bits, applied begin, update, end. No switcher
    // popup: as in GNOME the workspaces moving is the feedback.
    void apply_swipe(uint32_t flags, double progress, int32_t dir) {
        if (flags & UMOJA_SWIPE_BEGIN) workspace_swipe_begin();
        if (flags & UMOJA_SWIPE_UPDATE) workspace_swipe_update(progress);
        if (flags & UMOJA_SWIPE_END) workspace_swipe_end(dir);
    }

    void workspace_swipe_begin() {
        if (swipe) return;
        finish_slide();
        Swipe s;
        s.width = primary_output().size.width.as_int();
        swipe = std::move(s);
        applying = true;
        guarded("swipe begin", [&] {
            collect(current, swipe->here, 0);
            if (current > 0) collect(current - 1, swipe->left, -swipe->width);
            if (current + 1 < workspaces.size()) collect(current + 1, swipe->right, swipe->width);
            place_swipe(0);
        });
        applying = false;
    }

    // `progress`: how far towards the next workspace (positive, fingers
    // moving left) or the previous one (negative), in screen widths.
    void workspace_swipe_update(double progress) {
        if (!swipe) return;
        bool const has_left = current > 0, has_right = current + 1 < workspaces.size();
        // Past the first or last workspace it only gives a little.
        if ((progress > 0 && !has_right) || (progress < 0 && !has_left)) progress *= 0.15;
        progress = std::clamp(progress, -1.0, 1.0);
        swipe->progress = progress;
        applying = true;
        place_swipe(progress);
        applying = false;
    }

    // `dir`: -1 previous workspace, 1 next, 0 snap back.
    void workspace_swipe_end(int dir) {
        if (!swipe) return;
        Swipe s = std::move(*swipe);
        swipe.reset();
        if (dir > 0 && current + 1 >= workspaces.size()) dir = 0;
        if (dir < 0 && current == 0) dir = 0;
        int const w = s.width;
        int const now = static_cast<int>(-s.progress * w);
        int const goal = -dir * w;
        applying = true;
        guarded("swipe end", [&] {
            current = static_cast<size_t>(static_cast<long>(current) + dir);
            Slide slide;
            auto add = [&](std::vector<SlideItem> const& items, int base) {
                for (auto it : items) {
                    it.from = base + now;
                    it.to = base + goal;
                    it.hide_after = !in_workspace(it.window, workspaces[current]);
                    slide.items.push_back(it);
                }
            };
            add(s.here, 0);
            add(s.left, -w);
            add(s.right, w);
            if (dir != 0) tools.select_active_window(Window{});
            focus_current_workspace();
            // The rest of the way at the slide's pace.
            double const left_to_go = std::abs(goal - now) / static_cast<double>(std::max(w, 1));
            start_slide(std::move(slide), std::max(120.0, SLIDE_MS * left_to_go));
        });
        applying = false;
        relayout_workspaces();
    }

private:
    // Something else switches workspace mid-swipe: settle the swipe first.
    void end_swipe_now() {
        if (!swipe) return;
        workspace_swipe_end(0);
        finish_slide();
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

        // GNOME custom shortcuts (Myna's dictation key among them).
        if (action >= UMOJA_ACTION_CUSTOM_BASE) {
            umoja_handle_action(action);
            return true;
        }

        switch (action) {
        case UMOJA_ACTION_TOGGLE_OVERLAY:
        case UMOJA_ACTION_TOGGLE_OVERVIEW:
        case UMOJA_ACTION_TOGGLE_DICTATION:
        case UMOJA_ACTION_SCREENSHOT_UI:
        case UMOJA_ACTION_SCREENSHOT:
        case UMOJA_ACTION_SCREEN_RECORDING_UI:
        case UMOJA_ACTION_COPY_TEXT_UI:
        case UMOJA_ACTION_SHOW_SHORTCUTS:
        case UMOJA_ACTION_ASK_ABOUT_SCREEN:
        case UMOJA_ACTION_CLIPBOARD_HISTORY:
        case UMOJA_ACTION_SHIFT_OVERVIEW_UP:
        case UMOJA_ACTION_SHIFT_OVERVIEW_DOWN:
        case UMOJA_ACTION_TOGGLE_MESSAGE_TRAY:
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
        case UMOJA_ACTION_BEGIN_MOVE:
        case UMOJA_ACTION_BEGIN_RESIZE:
            if (!all_windows.count(target)) return false;
            begin_key_grab(target, action == UMOJA_ACTION_BEGIN_RESIZE);
            return true;
        case UMOJA_ACTION_WINDOW_MENU:
            if (!all_windows.count(target)) return false;
            show_window_menu(target);
            return true;
        case UMOJA_ACTION_TOGGLE_ABOVE:
            if (!all_windows.count(target)) return false;
            toggle_above(target);
            return true;
        case UMOJA_ACTION_TOGGLE_STICKY:
            if (!all_windows.count(target)) return false;
            toggle_pinned(target);
            return true;
        default:
            break;
        }

        if (!all_windows.count(target) || !umoja_is_tile_action(action)) return false;

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

    // ---- keyboard move / resize -----------------------------------------------

    static constexpr int KEY_ABOVE_TAB = 41; // evdev KEY_GRAVE
    static constexpr uint32_t XK_GRAVE = 0x60;

    static bool resizable(WindowInfo const& info) {
        return info.min_width() != info.max_width() || info.min_height() != info.max_height();
    }

    void begin_key_grab(Window w, bool resize) {
        auto& info = tools.info_for(w);
        if (info.state() == mir_window_state_fullscreen) return;
        if (resize && !resizable(info)) return;
        // A maximized or tiled window comes back to its own size first.
        if (tiled.count(window_id(w))) restore_untiled(info);
        else if (info.state() != mir_window_state_restored) set_state(info, mir_window_state_restored);
        KeyGrab g;
        g.window = w;
        g.resize = resize;
        g.original = geom::Rectangle{w.top_left(), w.size()};
        key_grab = g;
        if (moving) end_move();
    }

    void key_grab_set(KeyGrab const& g, geom::Rectangle const& r) {
        WindowSpecification spec;
        spec.top_left() = r.top_left;
        spec.size() = r.size;
        applying = true;
        guarded("keyboard move", [&] { tools.modify_window(tools.info_for(g.window), spec); });
        applying = false;
    }

    static bool is_modifier_key(uint32_t keysym) {
        return (keysym >= 0xffe1 && keysym <= 0xffee) || keysym == 0xfe03 || keysym == 0xff7e;
    }

    // Returns true when the key was used by the grab.
    bool handle_key_grab(MirKeyboardAction action, uint32_t keysym, uint32_t mods) {
        if (is_modifier_key(keysym)) return false;
        auto& g = *key_grab;
        if (!all_windows.count(g.window)) {
            key_grab.reset();
            return false;
        }
        if (action == mir_keyboard_action_up) return true;
        int dx = 0, dy = 0;
        switch (keysym) {
        case 0xff51: case 0xff96: dx = -1; break; // Left, KP_Left
        case 0xff53: case 0xff98: dx = 1; break;  // Right
        case 0xff52: case 0xff97: dy = -1; break; // Up
        case 0xff54: case 0xff99: dy = 1; break;  // Down
        case 0xff0d: case 0xff8d: case 0x20:      // Return, KP_Enter, space
            key_grab.reset();
            return true;
        case 0xff1b: // Escape: back to where it started
            key_grab_set(g, g.original);
            key_grab.reset();
            return true;
        default:
            // Any other key ends the grab and goes on as usual (mutter).
            key_grab.reset();
            return false;
        }
        // mutter: 10 px steps, 1 px with Ctrl.
        int const step = (mods & UMOJA_MOD_CTRL) ? 1 : 10;
        geom::Rectangle r{g.window.top_left(), g.window.size()};
        int x = r.top_left.x.as_int(), y = r.top_left.y.as_int();
        int w = r.size.width.as_int(), h = r.size.height.as_int();
        if (!g.resize) {
            x += dx * step;
            y += dy * step;
        } else {
            auto const& info = tools.info_for(g.window);
            int const min_w = std::max(1, info.min_width().as_int()), max_w = info.max_width().as_int();
            int const min_h = std::max(1, info.min_height().as_int()), max_h = info.max_height().as_int();
            // The first arrow on an axis picks that edge (Left: left edge...).
            if (dx && !g.hedge) g.hedge = dx;
            if (dy && !g.vedge) g.vedge = dy;
            if (dx) {
                int const nw = std::clamp(w + dx * step * g.hedge, min_w, std::max(min_w, max_w));
                if (g.hedge < 0) x -= nw - w;
                w = nw;
            }
            if (dy) {
                int const nh = std::clamp(h + dy * step * g.vedge, min_h, std::max(min_h, max_h));
                if (g.vedge < 0) y -= nh - h;
                h = nh;
            }
        }
        key_grab_set(g, geom::Rectangle{geom::Point{x, y}, geom::Size{w, h}});
        return true;
    }

    // Pointer motion during a keyboard move drags the window along.
    void key_grab_pointer(int32_t px, int32_t py) {
        auto& g = *key_grab;
        geom::Point const p{px, py};
        if (g.have_pointer && !g.resize && all_windows.count(g.window)) {
            auto const d = p - g.pointer;
            key_grab_set(g, geom::Rectangle{g.window.top_left() + d, g.window.size()});
        }
        g.pointer = p;
        g.have_pointer = true;
    }

    // ---- window menu (Alt+Space) -----------------------------------------------

    void show_window_menu(Window w) { show_window_menu(w, w.top_left()); }

    void show_window_menu(Window w, geom::Point at) {
        auto const& info = tools.info_for(w);
        uint32_t flags = 0;
        auto const state = info.state();
        if (state == mir_window_state_maximized) flags |= UMOJA_WINDOW_MAXIMIZED;
        if (state == mir_window_state_fullscreen) flags |= UMOJA_WINDOW_FULLSCREEN;
        if (tiled.count(window_id(w))) flags |= UMOJA_WINDOW_TILED;
        if (above.count(w)) flags |= UMOJA_WINDOW_ABOVE;
        if (!in_any_workspace(w)) flags |= UMOJA_WINDOW_STICKY;
        if (resizable(info)) flags |= UMOJA_WINDOW_RESIZABLE;
        int32_t const ws = workspace_of(w);

        // Which "Move to Monitor" entries apply: bits 1 left, 2 right, 4 up, 8 down.
        uint32_t neighbours = 0;
        if (outputs.size() > 1) {
            std::vector<UmojaRect> rects;
            geom::Rectangle from{};
            for (auto const& [id, r] : outputs) {
                rects.push_back(to_rect(r));
                if (r.contains(centre_of(w))) from = r;
            }
            uint32_t const dirs[] = {UMOJA_ACTION_MOVE_TO_MONITOR_LEFT, UMOJA_ACTION_MOVE_TO_MONITOR_RIGHT,
                                     UMOJA_ACTION_MOVE_TO_MONITOR_UP, UMOJA_ACTION_MOVE_TO_MONITOR_DOWN};
            for (uint32_t i = 0; i < 4; ++i)
                if (umoja_neighbor_output(dirs[i], to_rect(from), rects.data(), rects.size()) >= 0)
                    neighbours |= 1u << i;
        }
        // Like mutter: at the window's top-left corner for the keyboard, at
        // the pointer for a modifier click.
        umoja_window_menu(window_id(w), at.x.as_int(), at.y.as_int(), flags,
                          ws < 0 ? static_cast<uint32_t>(current) : static_cast<uint32_t>(ws),
                          static_cast<uint32_t>(workspaces.size()), neighbours);
    }

    void toggle_above(Window w) {
        bool const on = !above.count(w);
        if (on) above.insert(w);
        else above.erase(w);
        WindowSpecification spec;
        spec.depth_layer() = layer_for(w);
        guarded("always on top", [&] {
            tools.modify_window(tools.info_for(w), spec);
            tools.raise_tree(w);
        });
    }

    // "Always on Visible Workspace": on every workspace, like windows on a
    // secondary monitor with workspaces-only-on-primary.
    void toggle_pinned(Window w) {
        if (pinned.count(w)) pinned.erase(w);
        else pinned.insert(w);
        guarded("pin to all workspaces", [&] {
            if (update_workspace_membership(w)) relayout_workspaces();
        });
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

extern "C" bool umoja_wm_script_drag(uint64_t id, uint32_t phase, int32_t x, int32_t y, int32_t* out_x,
                                     int32_t* out_y) {
    bool ok = false;
    with_wm("scripted drag", [&] { ok = g_wm->script_drag(g_wm->find(id), phase, x, y, out_x, out_y); });
    return ok;
}

extern "C" void umoja_wm_workspaces(uint32_t* out_current, uint32_t* out_count) {
    if (out_current) *out_current = 0;
    if (out_count) *out_count = 0;
    with_wm("workspaces", [&] { g_wm->workspace_state(out_current, out_count); });
}

extern "C" void umoja_wm_switch_workspace(uint32_t index) {
    with_wm("switch workspace", [&] { g_wm->switch_workspace(index); });
}

// A touchpad swipe step, from Mir's input thread (see umoja_touchpad_gesture).
extern "C" void umoja_wm_workspace_swipe(uint32_t flags, double progress, int32_t dir) {
    if (flags == 0) return;
    with_wm("touchpad swipe", [&] { g_wm->apply_swipe(flags, progress, dir); });
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

namespace {
// Set while umoja_mir_run is running, for umoja_mir_stop. The mutex keeps
// the runner alive for the duration of a stop() from another thread.
std::mutex g_runner_mutex;
MirRunner* g_runner = nullptr;
std::atomic<int> g_exit_code{-1};
// Mir is already shutting down (e.g. LightDM's SIGTERM): the greeter then
// dies with its connection, and that is not a failure worth reporting.
std::atomic<bool> g_stopping{false};
}

// Greeter mode: the greeter exited, so the compositor does too. Called from
// a Rust thread, possibly while Mir is already shutting down (LightDM
// signalled us and the greeter died with the connection): never let an
// exception unwind into Rust.
extern "C" void umoja_mir_stop(int code) {
    if (g_stopping) return;
    int expected = -1;
    g_exit_code.compare_exchange_strong(expected, code);
    std::lock_guard lock{g_runner_mutex};
    if (!g_runner) return;
    try {
        g_runner->stop();
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: stop failed: %s\n", e.what());
    } catch (...) {
    }
}

extern "C" int umoja_mir_run(int argc, char const** argv, UmojaConfig const* config) {
    MirRunner runner{argc, argv};
    {
        std::lock_guard lock{g_runner_mutex};
        g_runner = &runner;
    }
    struct Clear {
        ~Clear() {
            std::lock_guard lock{g_runner_mutex};
            g_runner = nullptr;
        }
    } clear;

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
    PrependEventFilter idle_activity{umoja_idle_input_filter};

    // The pointer drawn by Mir itself (desktop, server-side cursor shapes):
    // the GNOME cursor theme and size that umoja-session exports, like
    // mutter. Without this Mir uses its built-in cursor. Mir loads themes at
    // 24px and scales them by each output's scale on its own.
    std::string cursor_themes = "Yaru:default";
    if (auto const theme = std::getenv("XCURSOR_THEME"); theme && *theme) {
        cursor_themes = std::string{theme} + ":" + cursor_themes;
    }
    CursorTheme cursor_theme{cursor_themes};
    float cursor_scale = 1.0f;
    if (auto const size = std::getenv("XCURSOR_SIZE")) {
        auto const px = std::atoi(size);
        if (px >= 8 && px <= 256) cursor_scale = px / 24.0f;
    }
    CursorScale cursor_size{cursor_scale};

    runner.add_stop_callback([] { g_stopping = true; });
    runner.add_start_callback([&runner] {
        // Mir has made the X11 sockets by now (Xwayland itself starts on the
        // first connection). Put DISPLAY in our environment so session.rs
        // exports it to systemd and D-Bus activation, as mutter does: apps
        // started from the dock, terminals and snaps all read it from there.
        if (auto const x11 = runner.x11_display(); x11.is_set()) {
            setenv("DISPLAY", x11.value().c_str(), 1);
        } else {
            unsetenv("DISPLAY");
        }
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
    auto const input = [](mir::Server& server) { umoja_input_setup(server); };

    auto const result = [](int code) { return g_exit_code >= 0 ? g_exit_code.load() : code; };
    if (config->enable_x11) {
        return result(runner.run_with({
            idle,
            input,
            display_config,
            display_config.layout_option(),
            natural_scroll,
            idle_activity,
            cursor_theme,
            cursor_size,
            extensions,
            // On its own X11Support only adds an --enable-x11 option, off by
            // default: without this no Xwayland ran, and every X11 app
            // (Steam, Wine/Proton games...) failed to open a display.
            X11Support{}.default_to_enabled(),
            set_window_management_policy<UmojaWindowManager>(),
        }));
    }
    return result(runner.run_with({
        idle,
        input,
        display_config,
        display_config.layout_option(),
        natural_scroll,
        idle_activity,
        cursor_theme,
        cursor_size,
        extensions,
        set_window_management_policy<UmojaWindowManager>(),
    }));
}

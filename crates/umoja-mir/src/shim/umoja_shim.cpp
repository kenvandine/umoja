// MirAL shim for umoja-mir.
//
// This file owns everything that has to be C++: the MirRunner, the Wayland
// extension set, display configuration, Xwayland, and the window-management
// policy. All decisions (which key does what, where a tiled window goes,
// how many workspaces exist, which monitor is "next") are made in Rust
// through the functions declared in umoja_shim.h, so policy can be
// unit-tested without a running compositor.

#include "umoja_shim.h"
#include "session_management.h"

#include <miral/application.h>
#include <miral/application_info.h>
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
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <fstream>
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

geom::Point centre_of(geom::Rectangle const& r) {
    return {r.top_left.x.as_int() + r.size.width.as_int() / 2, r.top_left.y.as_int() + r.size.height.as_int() / 2};
}

geom::Point centre_of(Window const& w) { return centre_of(geom::Rectangle{w.top_left(), w.size()}); }

// Mir's spot for a new window (BasicWindowManager::place_new_surface):
// centred across the work area, optically raised by a sixth of the slack.
geom::Point optical_centre(geom::Rectangle const& area, geom::Size const& size) {
    int const sw = area.size.width.as_int() - size.width.as_int();
    int const sh = area.size.height.as_int() - size.height.as_int();
    return {area.top_left.x.as_int() + sw / 2, area.top_left.y.as_int() + sh / 2 - sh / 6};
}

// mutter's constrain_fully_onscreen/constrain_to_workarea for a new window:
// shrunk to the work area (never under its minimum size) and moved so that
// all of it is inside.
geom::Rectangle fit_inside(geom::Rectangle const& r, geom::Rectangle const& area, geom::Size const& min) {
    int const ax = area.top_left.x.as_int(), ay = area.top_left.y.as_int();
    int const aw = area.size.width.as_int(), ah = area.size.height.as_int();
    if (aw <= 0 || ah <= 0) return r;
    int const w = std::max(std::min(r.size.width.as_int(), aw), std::max(1, min.width.as_int()));
    int const h = std::max(std::min(r.size.height.as_int(), ah), std::max(1, min.height.as_int()));
    int const x = std::clamp(r.top_left.x.as_int(), ax, std::max(ax, ax + aw - w));
    int const y = std::clamp(r.top_left.y.as_int(), ay, std::max(ay, ay + ah - h));
    return {geom::Point{x, y}, geom::Size{w, h}};
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

// The app id the shell (Alt+Tab, overview) looks up a .desktop file by.
//
// info.application_id() is the client's raw app id. For a snap that often
// names no desktop file: Electron snaps report their package.json
// desktopName ("com.Mattermost.Desktop"), while snapd installs the entry as
// mattermost-desktop_mattermost-desktop.desktop, so Alt+Tab drew no icon.
// Mir's foreign-toplevel (what the dock sees) already maps snap clients to
// their desktop file by AppArmor label; do the same here, so both agree.
std::string snap_desktop_id(pid_t pid) {
    if (pid <= 0) return {};
    std::string label;
    for (char const* attr : {"/attr/apparmor/current", "/attr/current"}) {
        std::ifstream in{"/proc/" + std::to_string(pid) + attr};
        if (in && std::getline(in, label) && !label.empty()) break;
        label.clear();
    }
    // "snap.<instance>.<app> (enforce)"; hooks are snap.<instance>.hook.<name>.
    label = label.substr(0, label.find(' '));
    if (label.rfind("snap.", 0) != 0) return {};
    std::string const rest = label.substr(5);
    auto const dot = rest.find('.');
    if (dot == std::string::npos || rest.find('.', dot + 1) != std::string::npos) return {};
    std::string const instance = rest.substr(0, dot);
    std::string const app = rest.substr(dot + 1);
    if (instance.empty() || app.empty()) return {};

    std::filesystem::path const dir{"/var/lib/snapd/desktop/applications"};
    std::error_code ec;
    std::string const id = instance + "_" + app;
    if (std::filesystem::exists(dir / (id + ".desktop"), ec)) return id;
    // The desktop file is named after the snap's meta/gui file, which need
    // not match the app name: find the one whose X-SnapAppName is this app.
    std::string const prefix = instance + "_";
    for (auto const& entry : std::filesystem::directory_iterator{dir, ec}) {
        std::string const name = entry.path().filename().string();
        if (name.rfind(prefix, 0) != 0 || entry.path().extension() != ".desktop") continue;
        std::ifstream in{entry.path()};
        for (std::string line; std::getline(in, line);) {
            if (line == "X-SnapAppName=" + app) return entry.path().stem().string();
            if (line.rfind("[Desktop Action", 0) == 0) break;
        }
    }
    return {};
}

std::string shell_app_id(WindowInfo const& info) {
    // Looked up once per client (keyed with the raw id too, so a reused pid
    // is unlikely to inherit a stale answer): the label doesn't change.
    static std::map<std::pair<pid_t, std::string>, std::string> cache;
    pid_t const pid = miral::pid_of(info.window().application());
    auto const key = std::make_pair(pid, info.application_id());
    auto it = cache.find(key);
    if (it == cache.end()) {
        if (cache.size() > 256) cache.clear();
        it = cache.emplace(key, snap_desktop_id(pid)).first;
    }
    return it->second.empty() ? info.application_id() : it->second;
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
// Time between frames of a workspace slide, in microseconds: one refresh
// of the fastest monitor (set as outputs come and go).
std::atomic<int> g_slide_frame_us{16000};

class UmojaWindowManager : public MinimalWindowManager {
public:
    explicit UmojaWindowManager(WindowManagerTools const& tools)
        // Modified clicks (Super+drag) are ours, following GNOME's
        // mouse-button-modifier; caps_lock is outside the modifiers Mir
        // compares, so its own Super+drag never starts.
        : MinimalWindowManager{tools, mir_input_event_modifier_caps_lock} {
        workspaces.push_back(this->tools.create_workspace());
        g_wm = this;
        // Runs on the session writer's thread, which may take the WM lock.
        umoja_sm::set_snapshotter([] {
            if (g_wm) g_wm->invoke([] {
                if (g_wm) guarded("snapshot sessions", [] { g_wm->snapshot_sessions(); });
            });
        });
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
        bool const toplevel = !requested.parent().is_set() || !requested.parent().value().lock();
        // xdg-session-management: an app restoring this window from its
        // session gets it back as it was (session_management.cpp).
        placing.reset();
        bool restored = false;
        if (toplevel && !requested.aux_rect().is_set() &&
            !(spec.state().is_set() && spec.state().value() == mir_window_state_attached)) {
            guarded("session restore", [&] {
                if (auto const pending = umoja_sm::take_pending(app.application())) placing.emplace(*pending);
                if (placing && placing->restore) restored = apply_restore(spec, *placing->restore);
            });
        }
        if (restored) return spec;
        // A Wayland client's first commit carries no buffer, so Mir places a
        // 640x480 stand-in; the real size only comes with the first buffer
        // (handle_modify_window). Placed from the stand-in, a big window like
        // Firefox's first run hung ~100 px off the bottom and right of a
        // small (250% scaled) screen.
        unsized_placement.reset();
        if (toplevel && !requested.size().is_set() && spec.top_left().is_set() &&
            (!spec.state().is_set() || spec.state().value() == mir_window_state_restored) &&
            (!spec.type().is_set() || spec.type().value() == mir_window_type_normal ||
             spec.type().value() == mir_window_type_freestyle)) {
            geom::Size const stand_in{640, 480};
            auto const area = area_at(centre_of(geom::Rectangle{spec.top_left().value(), stand_in}));
            unsized_placement = spec.top_left().value() - optical_centre(area, stand_in);
            return spec;
        }
        // Like mutter (meta_window_place): a new window opens on the monitor
        // under the pointer. Mir picks the monitor of the focused window.
        if (toplevel && !requested.output_id().is_set()) guarded("place on pointer monitor", [&] { to_pointer_monitor(spec); });
        // org.gnome.mutter center-new-windows
        // Not a window that maps fullscreen or maximized: Mir has already put
        // it over the monitor or work area, and centring a monitor-sized
        // window in the work area left fullscreen games offset by the panel
        // and dock (36,16).
        bool const placed_by_state = spec.state().is_set() && spec.state().value() != mir_window_state_restored;
        if (toplevel && !placed_by_state && umoja_center_new_windows() && spec.size().is_set()) {
            auto const area = spec.top_left().is_set()
                                  ? area_at(centre_of(geom::Rectangle{spec.top_left().value(), spec.size().value()}))
                                  : tools.active_application_zone().extents();
            auto const sz = spec.size().value();
            spec.top_left() = geom::Point{
                area.top_left.x.as_int() + (area.size.width.as_int() - sz.width.as_int()) / 2,
                area.top_left.y.as_int() + (area.size.height.as_int() - sz.height.as_int()) / 2};
        }
        // Keep it all inside the work area, shrunk if it's bigger.
        if (toplevel && !placed_by_state && spec.size().is_set() && spec.top_left().is_set() &&
            !(spec.state().is_set() && spec.state().value() == mir_window_state_attached)) {
            geom::Rectangle const r{spec.top_left().value(), spec.size().value()};
            geom::Size const min{spec.min_width().is_set() ? spec.min_width().value() : geom::Width{1},
                                 spec.min_height().is_set() ? spec.min_height().value() : geom::Height{1}};
            auto const fitted = fit_inside(r, area_at(centre_of(r)), min);
            spec.top_left() = fitted.top_left;
            if (fitted.size != r.size) spec.size() = fitted.size;
        }
        return spec;
    }

    void advise_new_window(WindowInfo const& info) override {
        MinimalWindowManager::advise_new_window(info);
        if (info.visible_on_lock_screen()) {
            lock_surfaces.push_back(info.window());
            if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: lock surface added (%zu)\n", lock_surfaces.size());
        }
        auto const unsized = unsized_placement;
        unsized_placement.reset();
        if (unsized && is_app_window(info)) awaiting_size[info.window()] = *unsized;
        if (!is_app_window(info)) {
            // Chrome mapped during a drag (tile preview) goes under the window.
            if (moving && elevated) guarded("raise dragged", [&] { tools.raise_tree(moving); });
            return;
        }
        all_windows.insert(info.window());
        remember_monitor(info.window());
        stacking.push_back(info.window());
        mru.insert(mru.begin(), info.window());
        if (placing) guarded("session bind", [&] { finish_restore(info.window()); });
        placing.reset();
        update_workspace_membership(info.window());
        relayout_workspaces();
        umoja_windows_changed();
        umoja_sm::changed();
    }

    void advise_delete_window(WindowInfo const& info) override {
        Window const w = info.window();
        if (w == moving) {
            umoja_drag_end();
            moving = Window{};
            elevated = false;
        }
        if (key_grab && key_grab->window == w) key_grab.reset();
        erase(lock_surfaces, w);
        if (lock_focus == w) lock_focus = Window{};
        // Mir refocuses from its MRU list after this returns, which never
        // holds a lock surface that a grab kept from being focused.
        if (!lock_surfaces.empty() && tools.active_window() == w) defer_lock_focus();
        above.erase(w);
        preferred_output.erase(w);
        session_restore.erase(w);
        awaiting_size.erase(w);
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
        if (all_windows.count(w)) guarded("session close", [&] {
            snapshot_session(w);
            umoja_sm::window_closed(w);
            umoja_sm::changed();
        });
        bool const app = all_windows.erase(w);
        if (app) relayout_workspaces(w);
        MinimalWindowManager::advise_delete_window(info);
        if (app) umoja_windows_changed();
    }

    void advise_focus_gained(WindowInfo const& info) override {
        MinimalWindowManager::advise_focus_gained(info);
        Window const w = info.window();
        if (!lock_surfaces.empty()) {
            if (is_lock_surface(w)) lock_focus = w;
            else defer_lock_focus();
        }
        if (!all_windows.count(w)) return;
        umoja_windows_changed();
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
        if (all_windows.count(info.window())) {
            umoja_windows_changed();
            umoja_sm::changed();
        }
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: state of %s -> %d\n", describe(info).c_str(), int(state));
        update_fullscreen_layer(info.window(), state, info.window() == tools.active_window());
    }

    // ---- lock screen focus -------------------------------------------------
    //
    // While the session is locked (ext-session-lock), the keyboard belongs
    // to a lock surface. Mir focuses a lock surface when it maps, but not
    // while another surface holds a keyboard grab: an open shell menu, the
    // screensaver or any keyboard-exclusive layer surface. Those are hidden
    // on the lock screen yet kept the focus, so the password entry showed a
    // cursor but never got a key. Nothing else may hold focus while locked.
    std::vector<Window> lock_surfaces;  // creation order: umoja-panel maps the prompt first
    Window lock_focus;                  // the lock surface that last had focus

    void handle_window_ready(WindowInfo& info) override {
        MinimalWindowManager::handle_window_ready(info);
        if (is_lock_surface(info.window())) guarded("lock focus", [&] { ensure_lock_focus(); });
        else guarded("modal focus", [&] { take_focus_from_grab(info); });
    }

    // A keyboard-exclusive surface that maps while another one holds the
    // keyboard gets it: the newest grab wins, as in wlroots compositors.
    // Mir's select_active_window() never leaves a grabbing window, so a
    // password dialog (polkit, Wi-Fi, keyring, mounts) opened from Ask
    // Umoja, an open shell menu or the screensaver showed but never got a
    // key. Popups of the grabbing window are Mir's to handle; while locked
    // the lock screen keeps the keyboard.
    void take_focus_from_grab(WindowInfo const& info) {
        if (!lock_surfaces.empty() || info.parent()) return;
        if (info.focus_mode() != mir_focus_mode_grabbing || !info.can_be_active()) return;
        Window const w = info.window();
        Window const active = tools.active_window();
        if (!active || active == w) return;
        Window root = active;
        while (auto const parent = tools.info_for(root).parent()) root = parent;
        if (root == w || tools.info_for(root).focus_mode() != mir_focus_mode_grabbing) return;
        if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: new keyboard-exclusive surface takes focus from a grab\n");
        // Selecting no window first drops the old grab (see ensure_lock_focus).
        tools.select_active_window(Window{});
        tools.select_active_window(w);
    }

    bool is_lock_surface(Window const& w) const {
        return w && std::find(lock_surfaces.begin(), lock_surfaces.end(), w) != lock_surfaces.end();
    }

    void ensure_lock_focus() {
        if (lock_surfaces.empty()) return;
        Window const active = tools.active_window();
        if (is_lock_surface(active)) return;
        Window const target = is_lock_surface(lock_focus) ? lock_focus : lock_surfaces.front();
        if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: moving keyboard focus to the lock screen\n");
        // select_active_window() keeps a grabbing window focused; selecting
        // no window first drops the grab unconditionally.
        if (active && tools.info_for(active).focus_mode() == mir_focus_mode_grabbing)
            tools.select_active_window(Window{});
        tools.select_active_window(target);
    }

    // From Mir's focus and delete callbacks, which are inside
    // select_active_window()/remove_window(): apply afterwards, from a
    // helper thread that takes the WM lock (see update_fullscreen_layer).
    void defer_lock_focus() {
        std::thread([] {
            guarded("lock focus", [] {
                if (!g_wm) return;
                g_wm->invoke([] {
                    if (g_wm) g_wm->ensure_lock_focus();
                });
            });
        }).detach();
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
        bool app = false;
        for (auto const& w : windows) {
            if (!all_windows.count(w)) continue;
            erase(stacking, w);
            stacking.push_back(w);
            app = true;
        }
        if (app) umoja_windows_changed();
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

        // The Copilot key sends Super+Shift+F23. It opens Ask Umoja, and
        // the F23 press and release never reach the app. Matched by scan
        // code: layouts map F23 to TouchpadOff or Assistant, or nothing.
        if (mir_keyboard_event_scan_code(event) == KEY_COPILOT_F23) {
            ask_alt_armed = false;
            overlay_key_armed = false;
            if (action == mir_keyboard_action_down) umoja_copilot_key();
            return true;
        }

        // While Alt+Tab is open it sees every key, releases included.
        {
            UmojaSwitchResult res{};
            bool const down = action != mir_keyboard_action_up;
            uint32_t const r = umoja_switcher_key(bind_sym, mods, down, &res);
            if (r != UMOJA_SWITCH_NOT_OURS) {
                apply_switch(r, res);
                overlay_key_armed = false;
                ask_alt_armed = false;
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

        // A lone tap of the left Alt key opens Ask Umoja, as Unity's HUD
        // did, on keyboards without a Copilot key. The press reaches the
        // app as usual; the release is kept back so it doesn't count as a
        // tap there too (Firefox would show its menu bar). Ask Umoja then
        // takes the keyboard, so the app sees Alt go away with the focus.
        if (keysym == XK_ALT_L && action != mir_keyboard_action_repeat) {
            if (action == mir_keyboard_action_down) {
                ask_alt_armed = (mods & ~UMOJA_MOD_ALT) == 0 && umoja_ask_alt_tap_enabled();
                return MinimalWindowManager::handle_keyboard_event(event);
            }
            if (ask_alt_armed) {
                ask_alt_armed = false;
                umoja_handle_action(UMOJA_ACTION_ASK_UMOJA);
                return true;
            }
        } else if (action == mir_keyboard_action_down) {
            ask_alt_armed = false;
        }

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

        last_pointer = geom::Point{px, py};
        have_pointer = true;
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
            // Which surface takes a press: finds invisible surfaces that eat
            // clicks (a stale input region on a transparent shell surface).
            if (debug_windows())
                guarded("trace press", [&] {
                    std::string target = "nothing";
                    if (auto const w = tools.window_at(geom::Point{px, py}))
                        target = describe(tools.info_for(w)) + " app '" + tools.info_for(w).application_id() +
                                 "' pid " + std::to_string(miral::pid_of(w.application()));
                    std::fprintf(stderr, "umoja-mir: wm: press at %d,%d goes to %s\n", px, py, target.c_str());
                });
            umoja_pointer_pressed();
            // Super+click is not a tap of Super: no overview on release.
            overlay_key_armed = false;
            ask_alt_armed = false;
        }

        // The rest of a click that closed a shell menu (below).
        if (menu_click_swallowed) {
            if (action == mir_pointer_action_button_up) {
                if (buttons == 0) menu_click_swallowed = false;
                return true;
            }
            if (action == mir_pointer_action_button_down) return true;
        }
        if (mod_grab) return mod_grab_event(action, buttons, px, py);
        if (action == mir_pointer_action_button_down && pressed && !moving &&
            close_grabbing_menus(geom::Point{px, py})) {
            menu_click_swallowed = buttons != 0;
            return true;
        }
        if (action == mir_pointer_action_button_down && pressed && !moving &&
            begin_mod_grab(event, pressed, px, py))
            return true;

        bool const consumed = MinimalWindowManager::handle_pointer_event(event);

        // A scripted drag (DragWindow) ignores the real pointer. A pointer
        // drag ends when the last button comes up; motion with no button
        // held also ends it, because the release can go missing (a client's
        // move request that lands after the button is already up, or a
        // release taken by a client's drag-and-drop). Without that the drag
        // stayed live and plain hovering near the top of the screen showed
        // the maximize preview, dimming the whole work area.
        if (moving && !scripted && buttons == 0 &&
            (action == mir_pointer_action_motion || action == mir_pointer_action_button_up)) {
            if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: pointer drag ended (no button held)\n");
            end_move();
        } else if (action == mir_pointer_action_motion) {
            if (moving && !scripted) on_move(px, py);
            else if (!moving) report_motion(event, px, py);
        }
        return consumed;
    }

    // ---- shell menus: a click outside closes them ---------------------------
    // The top bar takes the keyboard exclusively while Quick Settings, the
    // date menu or a tray menu is open (KeyboardMode::Exclusive). Mir makes
    // that a grabbing focus mode, and only closes a grabbing xdg_popup when
    // another window becomes active, which a grabbing window never allows:
    // so a click outside left the menu open. Do what gnome-shell does: a
    // press outside the menus and the window they belong to closes them,
    // and the click goes no further. Presses on the bar itself stay with
    // the client (GTK closes or switches menus there).
    static void collect_menus(WindowManagerTools& tools, Window const& w, std::vector<Window>& out) {
        for (auto const& c : tools.info_for(w).children()) {
            collect_menus(tools, c, out);
            auto const& ci = tools.info_for(c);
            if (ci.type() == mir_window_type_menu && ci.is_visible()) out.push_back(c);
        }
    }

    bool close_grabbing_menus(geom::Point p) {
        bool closed = false;
        guarded("close menus on outside click", [&] {
            Window top = tools.active_window();
            if (!top) return;
            while (auto const parent = tools.info_for(top).parent()) top = parent;
            if (tools.info_for(top).focus_mode() != mir_focus_mode_grabbing) return;
            std::vector<Window> menus;
            collect_menus(tools, top, menus);
            if (menus.empty()) return;
            auto const inside = [&](Window const& w) {
                return geom::Rectangle{w.top_left(), w.size()}.contains(p);
            };
            if (inside(top) || std::ranges::any_of(menus, inside)) return;
            // Children first, as Mir closes nested popups.
            for (auto const& m : menus) tools.ask_client_to_close(m);
            closed = true;
        });
        return closed;
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
        use_session_restore(info, spec);
        place_on_first_size(info, spec);
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
        awaiting_size.erase(info.window());
        // Logged before anything can drop it, so a client's move request
        // that goes nowhere still shows up in the window trace.
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: move request from %s (input type %d, tracked %d)\n",
                         describe(info).c_str(), static_cast<int>(mir_input_event_get_type(input_event)),
                         static_cast<int>(all_windows.count(info.window())));
        MinimalWindowManager::handle_request_move(info, input_event);
        if (!all_windows.count(info.window()) || mir_input_event_get_type(input_event) != mir_input_event_type_pointer)
            return;
        auto const* pev = mir_input_event_get_pointer_event(input_event);
        // A request that arrives after the button is already up would start
        // a drag that no release ever ends.
        if (held_buttons == 0) {
            if (debug_windows())
                std::fprintf(stderr, "umoja-mir: wm: move request ignored, no button held\n");
            return;
        }
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_y));
        guarded("begin move", [&] { begin_move(info.window(), px, py, was_tiled_or_maximized(info.window())); });
    }

    // ---- tiling groups ----------------------------------------------------

    void advise_resize(WindowInfo const& info, geom::Size const& size) override {
        MinimalWindowManager::advise_resize(info, size);
        if (all_windows.count(info.window())) {
            umoja_windows_changed();
            umoja_sm::changed();
        }
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
        if (all_windows.count(info.window())) {
            umoja_windows_changed();
            umoja_sm::changed();
        }
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: move of %s -> %d,%d\n", describe(info).c_str(),
                         top_left.x.as_int(), top_left.y.as_int());
        // Windows sliding during a workspace switch are not being moved by
        // the user, and must not turn sticky as they leave the monitor.
        if (animating) return;
        // Only the user's own moves pick a new preferred monitor (mutter:
        // META_MOVE_RESIZE_USER_ACTION), not monitors coming and going.
        if (!rehoming && user_moving(info.window())) remember_monitor(info.window());
        if (all_windows.count(info.window()) && update_workspace_membership(info.window())) relayout_workspaces();
        if (applying) return;
        // The user dragged a tiled window away: it leaves its tile group.
        untile(info.window());
    }

    // ---- outputs ----------------------------------------------------------

    void advise_output_create(Output const& output) override {
        outputs[output.id()] = output.extents();
        output_names[output.id()] = output.name();
        output_refresh[output.id()] = output.refresh_rate();
        update_slide_pace();
        report(output);
        return_windows_to(output.name());
    }

    void advise_output_update(Output const& updated, Output const&) override {
        outputs[updated.id()] = updated.extents();
        output_names[updated.id()] = updated.name();
        output_refresh[updated.id()] = updated.refresh_rate();
        update_slide_pace();
        report(updated);
        return_windows_to(updated.name());
    }

    void advise_output_delete(Output const& output) override {
        auto const gone = output.extents();
        outputs.erase(output.id());
        output_names.erase(output.id());
        output_refresh.erase(output.id());
        update_slide_pace();
        umoja_output_remove(output.id());
        if (outputs.empty()) return;
        // Like mutter: windows on an unplugged monitor move to the primary
        // one. They keep it as their preferred monitor, and go back to it
        // when it is plugged in again (return_windows_to).
        auto const target = primary_output();
        rehoming = true;
        for (auto const& w : all_windows) {
            if (!gone.contains(centre_of(w))) continue;
            move_between(w, gone, target);
        }
        rehoming = false;
    }

    // ---- preferred monitor ------------------------------------------------
    //
    // mutter's window->preferred_logical_monitor: the monitor a window was
    // opened on or last moved to. Taking a monitor away doesn't change it,
    // so when that monitor comes back (meta_window_update_for_monitors_changed)
    // its windows move back to it, at the same relative spot, still maximized
    // or tiled. Kept by connector name, as Mir may number a returning output
    // differently.

    std::map<Window, std::string> preferred_output;
    std::map<int, std::string> output_names;
    std::map<int, double> output_refresh;

    // Workspace slides step once per refresh of the fastest monitor (a
    // fixed 16 ms juddered on 120 and 144 Hz panels), within 240 to 50 Hz.
    void update_slide_pace() {
        double hz = 0;
        for (auto const& [id, r] : output_refresh) hz = std::max(hz, r);
        if (hz < 50) hz = 60;
        if (hz > 240) hz = 240;
        g_slide_frame_us.store(static_cast<int>(1'000'000.0 / hz), std::memory_order_relaxed);
    }
    bool rehoming = false;

    std::string output_name_at(geom::Point p) const {
        for (auto const& [id, r] : outputs)
            if (r.contains(p))
                if (auto const it = output_names.find(id); it != output_names.end()) return it->second;
        return {};
    }

    bool user_moving(Window const& w) const {
        return w == moving || (key_grab && key_grab->window == w) || (mod_grab && mod_grab->window == w);
    }

    void remember_monitor(Window const& w) {
        if (!all_windows.count(w)) return;
        auto const name = output_name_at(centre_of(w));
        if (!name.empty()) preferred_output[w] = name;
    }

    void return_windows_to(std::string const& name) {
        if (name.empty() || all_windows.empty()) return;
        geom::Rectangle home;
        bool found = false;
        for (auto const& [id, n] : output_names)
            if (n == name) {
                home = outputs[id];
                found = true;
            }
        if (!found) return;
        rehoming = true;
        for (auto const& w : all_windows) {
            auto const it = preferred_output.find(w);
            if (it == preferred_output.end() || it->second != name) continue;
            auto const c = centre_of(w);
            if (home.contains(c)) continue;
            move_between(w, output_at(c), home);
        }
        rehoming = false;
    }

    // Move a window from one monitor to the same spot on another, keeping
    // it maximized or fullscreen there.
    void move_between(Window const& w, geom::Rectangle const& from, geom::Rectangle const& to) {
        guarded("move between monitors", [&] {
            auto& info = tools.info_for(w);
            WindowSpecification spec;
            auto const moved = umoja_translate_rect(to_rect({w.top_left(), w.size()}), to_rect(from), to_rect(to));
            spec.top_left() = geom::Point{moved.x, moved.y};
            spec.size() = geom::Size{moved.w, moved.h};
            for (auto const& [id, r] : outputs)
                if (r == to) spec.output_id() = id;
            // As move_to_monitor (Super+Shift+arrows) does.
            if (info.state() == mir_window_state_maximized || info.state() == mir_window_state_fullscreen) {
                spec.state() = info.state();
                tools.place_and_size_for_state(spec, info);
            }
            if (debug_windows())
                std::fprintf(stderr, "umoja-mir: wm: monitor change moves %s ->%s\n", describe(info).c_str(),
                             describe(spec).c_str());
            tools.modify_window(info, spec);
        });
    }

    // The work area of a monitor (the monitor itself until its panel and
    // dock have reserved their space).
    geom::Rectangle area_on(geom::Rectangle const& output) const {
        for (auto const& [id, r] : zones)
            if (output.contains(centre_of(r))) return r;
        return output;
    }

    // ---- xdg-session-management -------------------------------------------
    //
    // mutter's meta_wayland_xdg_session_state_save_window/_restore_window:
    // floating geometry, maximized/tiled/fullscreen, minimized, workspace,
    // and the monitor (here by connector, falling back to the pointer's).

    struct Placing : umoja_sm::Pending {
        explicit Placing(umoja_sm::Pending const& p) : umoja_sm::Pending{p} {}
        geom::Rectangle floating;
        MirWindowState state = mir_window_state_restored; // the state it shows in, once on its workspace
        bool hide = false;                                // created on another workspace
        size_t workspace = 0;
    };
    std::optional<Placing> placing;

    bool apply_restore(WindowSpecification& spec, umoja_sm::SavedToplevel const& saved) {
        if (outputs.empty()) return false;
        Placing& p = *placing;
        // Its monitor if still connected, else where Mir put it (the
        // pointer's monitor), at the same relative spot.
        geom::Rectangle target;
        bool found = false;
        for (auto const& [id, name] : output_names)
            if (name == saved.output && outputs.count(id)) {
                target = outputs[id];
                found = true;
            }
        if (!found) {
            to_pointer_monitor(spec);
            target = spec.top_left().is_set() && spec.size().is_set()
                         ? output_at(centre_of(geom::Rectangle{spec.top_left().value(), spec.size().value()}))
                         : primary_output();
        }
        auto const from = saved.output_rect.size.width.as_int() > 0 ? saved.output_rect : target;
        p.floating = from_rect(umoja_translate_rect(to_rect(saved.floating), to_rect(from), to_rect(target)));
        auto const area = area_on(target);

        switch (saved.state) {
        case umoja_sm::WindowState::maximized:
            p.state = mir_window_state_maximized;
            spec.top_left() = area.top_left;
            spec.size() = area.size;
            break;
        case umoja_sm::WindowState::fullscreen:
            p.state = mir_window_state_fullscreen;
            spec.top_left() = target.top_left;
            spec.size() = target.size;
            break;
        case umoja_sm::WindowState::tiled: {
            UmojaRect out{};
            if (umoja_is_tile_action(saved.tile_action) && umoja_tile_geometry(saved.tile_action, to_rect(area), &out)) {
                spec.top_left() = geom::Point{out.x, out.y};
                spec.size() = geom::Size{out.w, out.h};
                break;
            }
            [[fallthrough]];
        }
        case umoja_sm::WindowState::floating:
            spec.top_left() = p.floating.top_left;
            spec.size() = p.floating.size;
            break;
        }
        if (saved.minimized && p.state == mir_window_state_restored) p.state = mir_window_state_minimized;

        // Its workspace (mutter appends workspaces up to it). Windows on a
        // secondary monitor with workspaces-only-on-primary are on all of them.
        p.workspace = current;
        bool const sticky = umoja_workspaces_only_on_primary() && outputs.size() > 1 && target != primary_output();
        if (saved.workspace >= 0 && !sticky) {
            // Past the last one: appended, as mutter's
            // meta_window_change_workspace_by_index(append) does (dynamic
            // workspaces then tidy up the empty ones in between).
            size_t const want = std::min<size_t>(static_cast<size_t>(saved.workspace), 35);
            while (workspaces.size() <= want) workspaces.push_back(tools.create_workspace());
            p.workspace = want;
        }
        p.hide = p.workspace != current;
        spec.state() = p.hide ? mir_window_state_hidden : p.state;
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: session restore -> workspace %zu%s\n", p.workspace,
                         describe(spec).c_str());
        return true;
    }

    void finish_restore(Window const& w) {
        Placing const p = *placing;
        umoja_sm::bind(p.token, w);
        if (!p.restore) return;
        // Unmaximizing or unminimizing goes back to its own size; Mir would
        // make one up (WindowInfo::restore_rect), so we keep it until then.
        if (p.state != mir_window_state_restored) session_restore[w] = p.floating;
        if (p.restore->state == umoja_sm::WindowState::tiled && umoja_is_tile_action(p.restore->tile_action)) {
            auto const id = window_id(w);
            tiled[id] = w;
            tile_action[w] = p.restore->tile_action;
            untiled[w] = p.floating;
            umoja_tiling_register(id, p.restore->tile_action, to_rect({w.top_left(), w.size()}));
        }
        if (p.hide) {
            tools.add_tree_to_workspace(w, workspaces[p.workspace]);
            hidden_states[w] = p.state;
            hidden_at[w] = std::chrono::steady_clock::now();
        }
    }

    std::map<Window, geom::Rectangle> session_restore;

    // ---- placing windows that mapped without a size -----------------------
    //
    // Set by place_new_window for the window being created: how far Mir's
    // cascade moved the stand-in from its usual spot.
    std::optional<geom::Displacement> unsized_placement;
    // Windows still waiting for their first real size, with that cascade.
    std::map<Window, geom::Displacement> awaiting_size;

    // mutter places a window when it first maps, at its real size: on the
    // monitor under the pointer, centred (center-new-windows) or where Mir
    // would have put it, and fully inside the work area, shrunk if needed.
    void place_on_first_size(WindowInfo const& info, WindowSpecification& spec) {
        auto const it = awaiting_size.find(info.window());
        if (it == awaiting_size.end()) return;
        auto const state = spec.state().is_set() ? spec.state().value() : info.state();
        if (spec.top_left().is_set() || (state != mir_window_state_restored && state != mir_window_state_hidden)) {
            // Moved, maximized, fullscreened or tiled before it had a size:
            // that already says where it goes.
            awaiting_size.erase(it);
            return;
        }
        if (!spec.size().is_set()) return;
        auto const cascade = it->second;
        awaiting_size.erase(it);
        guarded("place on first size", [&] {
            auto const size = spec.size().value();
            auto const w = info.window();
            geom::Rectangle output = output_at(centre_of(w));
            if (outputs.size() > 1 && have_pointer)
                for (auto const& [id, r] : outputs)
                    if (r.contains(last_pointer)) output = r;
            auto const area = area_on(output);
            geom::Point top_left;
            if (umoja_center_new_windows())
                top_left = {area.top_left.x.as_int() + (area.size.width.as_int() - size.width.as_int()) / 2,
                            area.top_left.y.as_int() + (area.size.height.as_int() - size.height.as_int()) / 2};
            else
                top_left = optical_centre(area, size) + cascade;
            auto const fitted = fit_inside({top_left, size}, area, {info.min_width(), info.min_height()});
            if (debug_windows())
                std::fprintf(stderr, "umoja-mir: wm: first size %dx%d for %s -> %d,%d %dx%d\n", size.width.as_int(),
                             size.height.as_int(), describe(info).c_str(), fitted.top_left.x.as_int(),
                             fitted.top_left.y.as_int(), fitted.size.width.as_int(), fitted.size.height.as_int());
            spec.top_left() = fitted.top_left;
            if (fitted.size != size) spec.size() = fitted.size;
        });
    }

    // A restored window leaving maximized/fullscreen/minimized for the first
    // time takes its saved floating geometry.
    void use_session_restore(WindowInfo const& info, WindowSpecification& spec) {
        if (session_restore.empty() || !spec.state().is_set()) return;
        auto const it = session_restore.find(info.window());
        if (it == session_restore.end()) return;
        if (spec.state().value() != mir_window_state_restored) return;
        if (info.state() == mir_window_state_restored) {
            session_restore.erase(it);
            return;
        }
        spec.top_left() = it->second.top_left;
        spec.size() = it->second.size;
        session_restore.erase(it);
    }

    // How a window in a session looks now.
    void snapshot_session(Window const& w) {
        if (!all_windows.count(w)) return;
        auto& info = tools.info_for(w);
        umoja_sm::SavedToplevel t;
        auto state = info.state();
        if (auto const it = hidden_states.find(w); it != hidden_states.end()) state = it->second;
        geom::Rectangle const now{w.top_left(), w.size()};
        bool const is_tiled = tiled.count(window_id(w)) > 0;
        auto const sr = session_restore.find(w);
        auto const restore_rect = sr != session_restore.end() ? sr->second : info.restore_rect();
        if (state == mir_window_state_minimized) {
            t.minimized = true;
            t.floating = restore_rect;
        } else if (state == mir_window_state_maximized) {
            t.state = umoja_sm::WindowState::maximized;
            t.floating = restore_rect;
        } else if (state == mir_window_state_fullscreen) {
            t.state = umoja_sm::WindowState::fullscreen;
            t.floating = restore_rect;
        } else if (is_tiled) {
            t.state = umoja_sm::WindowState::tiled;
            auto const a = tile_action.find(w);
            t.tile_action = a != tile_action.end() ? a->second : 0;
            auto const u = untiled.find(w);
            t.floating = u != untiled.end() ? u->second : now;
        } else if (state == mir_window_state_restored) {
            t.floating = now;
        } else {
            t.floating = restore_rect;
        }
        t.workspace = pinned.count(w) ? -1 : workspace_of(w);
        auto const c = centre_of(state == mir_window_state_minimized ? t.floating : now);
        t.output = output_name_at(c);
        t.output_rect = output_at(c);
        umoja_sm::update(w, t);
    }

    void snapshot_sessions() {
        for (auto const& w : umoja_sm::bound_windows()) snapshot_session(w);
    }

    // ---- new windows on the pointer's monitor -----------------------------

    geom::Point last_pointer;
    bool have_pointer = false;

    void to_pointer_monitor(WindowSpecification& spec) {
        if (outputs.size() < 2 || !have_pointer || !spec.size().is_set() || !spec.top_left().is_set()) return;
        // App windows only: layer-shell surfaces (panel, dock, overlay) are
        // "attached" and pick their own monitor.
        if (spec.type().is_set() && spec.type().value() != mir_window_type_normal) return;
        if (spec.state().is_set() && spec.state().value() == mir_window_state_attached) return;
        geom::Rectangle target;
        bool found = false;
        for (auto const& [id, r] : outputs)
            if (r.contains(last_pointer)) {
                target = r;
                found = true;
            }
        if (!found) return;
        geom::Rectangle const rect{spec.top_left().value(), spec.size().value()};
        auto const from = output_at(centre_of(rect));
        if (from == target) return;
        auto const state = spec.state().is_set() ? spec.state().value() : mir_window_state_restored;
        if (state == mir_window_state_fullscreen) {
            spec.top_left() = target.top_left;
            spec.size() = target.size;
            return;
        }
        auto const from_area = area_on(from);
        auto const to_area = area_on(target);
        if (state == mir_window_state_maximized) {
            spec.top_left() = to_area.top_left;
            spec.size() = to_area.size;
            return;
        }
        auto const moved = umoja_translate_rect(to_rect(rect), to_rect(from_area), to_rect(to_area));
        spec.top_left() = geom::Point{moved.x, moved.y};
        spec.size() = geom::Size{moved.w, moved.h};
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
            std::string const app = shell_app_id(info);
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
                use_session_restore(info, spec);
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

    void move_pointer(int32_t x, int32_t y) {
        guarded("move pointer", [&] { tools.move_cursor_to(geom::PointF{float(x), float(y)}); });
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
    bool ask_alt_armed = false;
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
    // A press closed shell menus: its release (and other buttons) are eaten.
    bool menu_click_swallowed = false;
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
            strings.push_back(shell_app_id(info));
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
        awaiting_size.erase(w);
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
            // In monitor widths: every monitor slides its own windows by its
            // own width, clipped to itself, as gnome-shell's per-monitor
            // WorkspaceAnimation groups do. Nothing crosses onto a neighbour.
            double const dist = index > current ? 1.0 : -1.0;
            current = index;
            // Drop focus before anything is hidden: hiding the active window
            // makes Mir pass focus on, possibly to a window that is already
            // hidden, which brings that one back on screen.
            tools.select_active_window(Window{});
            Slide slide;
            for (auto const& w : leaving) {
                if (in_workspace(w, workspaces[index])) continue;
                if (!animate || !showing(w)) hide(w);
                else slide.items.push_back(slide_item(w, 0, -dist, true));
            }
            for (auto const& w : arriving) {
                if (animate && w != pinned && will_show(w)) {
                    // Off to the side (and clipped) before it is unhidden, so
                    // it never shows for a frame at its real position.
                    auto it = slide_item(w, dist, 0, false);
                    place_at(it, it.from);
                    unhide(w);
                    slide.items.push_back(it);
                } else {
                    unhide(w);
                }
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

    // Whether the window is on screen once unhide() has run.
    bool will_show(Window const& w) {
        auto const it = hidden_states.find(w);
        if (it == hidden_states.end()) return showing(w);
        return it->second != mir_window_state_hidden && it->second != mir_window_state_minimized;
    }

    // ---- workspace slide ---------------------------------------------------

    // A window sliding from offset `from` to `to` (in widths of its
    // monitor, horizontal) off its real position `base`, hidden at the end
    // if it left the workspace. While it slides it is clipped to `monitor`,
    // so it never shows on the monitor next door (Mir draws a window on
    // every output it overlaps).
    struct SlideItem {
        Window window;
        geom::Point base;
        geom::Rectangle monitor;
        double from, to;
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

    void place_at(SlideItem const& it, double offset) {
        int const px = static_cast<int>(std::lround(offset * it.monitor.size.width.as_int()));
        place(it.window, geom::Point{it.base.x.as_int() + px, it.base.y.as_int()});
    }

    // The window's slide on the monitor it is on, clip set.
    SlideItem slide_item(Window const& w, double from, double to, bool hide_after) {
        SlideItem it{w, w.top_left(), output_at(centre_of(w)), from, to, hide_after};
        tools.info_for(w).clip_area(mir::optional_value<geom::Rectangle>{it.monitor});
        return it;
    }

    void unclip(Window const& w) {
        if (!all_windows.count(w)) return;
        tools.info_for(w).clip_area(mir::optional_value<geom::Rectangle>{});
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
            for (auto const& it : slide->items) place_at(it, it.from + (it.to - it.from) * e);
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
                unclip(it.window);
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
            // On a steady beat, so time spent waiting for the WM lock
            // doesn't stretch the frames after it.
            auto next = std::chrono::steady_clock::now();
            for (;;) {
                next += std::chrono::microseconds(g_slide_frame_us.load(std::memory_order_relaxed));
                auto const now = std::chrono::steady_clock::now();
                if (next < now) next = now;
                std::this_thread::sleep_until(next);
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
        double progress = 0;
    };
    std::optional<Swipe> swipe;

    // `base_offset` in monitor widths (-1 left neighbour, 1 right).
    void collect(size_t index, std::vector<SlideItem>& out, double base_offset) {
        std::vector<Window> ws;
        tools.for_each_window_in_workspace(workspaces[index], [&](Window const& w) { ws.push_back(w); });
        for (auto const& w : ws) {
            if (index != current && in_workspace(w, workspaces[current])) continue;
            if (!will_show(w)) continue;
            auto it = slide_item(w, base_offset, base_offset, index != current);
            // Neighbours go off to the side before they are unhidden.
            if (index != current) {
                place_at(it, base_offset);
                unhide(w);
            }
            out.push_back(it);
        }
    }

    void place_swipe(double p) {
        animating = true;
        guarded("swipe", [&] {
            for (auto const& it : swipe->here) place_at(it, -p);
            for (auto const& it : swipe->left) place_at(it, -1.0 - p);
            for (auto const& it : swipe->right) place_at(it, 1.0 - p);
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
        swipe = Swipe{};
        applying = true;
        guarded("swipe begin", [&] {
            collect(current, swipe->here, 0);
            if (current > 0) collect(current - 1, swipe->left, -1.0);
            if (current + 1 < workspaces.size()) collect(current + 1, swipe->right, 1.0);
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
        double const now = -s.progress;
        double const goal = -dir;
        applying = true;
        guarded("swipe end", [&] {
            current = static_cast<size_t>(static_cast<long>(current) + dir);
            Slide slide;
            auto add = [&](std::vector<SlideItem> const& items, double base) {
                for (auto it : items) {
                    it.from = base + now;
                    it.to = base + goal;
                    it.hide_after = !in_workspace(it.window, workspaces[current]);
                    slide.items.push_back(it);
                }
            };
            add(s.here, 0);
            add(s.left, -1.0);
            add(s.right, 1.0);
            if (dir != 0) tools.select_active_window(Window{});
            focus_current_workspace();
            // The rest of the way at the slide's pace.
            double const left_to_go = std::abs(goal - now);
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
        umoja_sm::changed();
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
    static constexpr int KEY_COPILOT_F23 = 193; // evdev KEY_F23
    static constexpr uint32_t XK_ALT_L = 0xffe9;
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
        remember_monitor(w);
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
        use_session_restore(info, spec);
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

extern "C" bool umoja_wm_window_rect(uint64_t id, UmojaRect* out) {
    bool found = false;
    with_wm("window rect", [&] {
        Window const w = g_wm->find(id);
        if (!w || !out) return;
        *out = to_rect({w.top_left(), w.size()});
        found = true;
    });
    return found;
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

// org.umoja.Compositor.MovePointer: put the pointer somewhere out of the
// way (tools/demo parks it so it isn't over what a clip shows).
extern "C" void umoja_wm_move_pointer(int32_t x, int32_t y) {
    with_wm("move pointer", [&] { g_wm->move_pointer(x, y); });
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
    // Like mutter: apps that save their session get their windows back
    // (xdg-session-management-v1, session_management.cpp).
    extensions.add_extension(umoja_sm::extension());

    // Persisted monitor layout (~/.config/umoja-mir.display), seeded from
    // GNOME's monitors.xml by the Rust side; Mir reloads it when it changes.
    DisplayConfiguration display_config{runner};
    PrependEventFilter natural_scroll{umoja_apply_natural_scroll};
    PrependEventFilter idle_activity{umoja_idle_input_filter};
    // Prepended last, so first: captured input (the input capture portal)
    // reaches nothing else, not even the idle timer or shortcuts.
    PrependEventFilter input_capture{umoja_input_capture_filter};

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
    auto const capture = [](mir::Server& server) {
        umoja_capture_setup(server);
        umoja_screencast_setup(server);
        umoja_remote_input_setup(server);
    };

    auto const result = [](int code) { return g_exit_code >= 0 ? g_exit_code.load() : code; };
    if (config->enable_x11) {
        return result(runner.run_with({
            idle,
            input,
            capture,
            display_config,
            display_config.layout_option(),
            natural_scroll,
            idle_activity,
            input_capture,
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
        capture,
        display_config,
        display_config.layout_option(),
        natural_scroll,
        idle_activity,
        input_capture,
        cursor_theme,
        cursor_size,
        extensions,
        set_window_management_policy<UmojaWindowManager>(),
    }));
}

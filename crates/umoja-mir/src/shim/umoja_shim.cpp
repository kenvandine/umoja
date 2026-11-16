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
#include "surface_effects.h"
#include "wm_protocols.h"

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

int64_t steady_ms() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

// Mir patch 3026 reports activations the user asked for (a dock's
// foreign-toplevel activate, a valid xdg-activation token) just before Mir
// raises the window, so handle_raise_window can tell them from an app
// raising itself. Set on Mir's Wayland thread.
std::atomic<void const*> g_user_activation{nullptr};
std::atomic<int64_t> g_user_activation_ms{0};
// Mir has those hooks at all (an older Mir: the old one-second rule).
std::atomic<bool> g_activation_hooks{false};

// A process's executable name ("umoja-overlay", "Xwayland"), cached.
std::string process_name(pid_t pid) {
    static std::mutex m;
    static std::map<pid_t, std::string> cache;
    if (pid <= 0) return {};
    std::lock_guard lock{m};
    if (auto const it = cache.find(pid); it != cache.end()) return it->second;
    if (cache.size() > 256) cache.clear();
    char exe[PATH_MAX];
    auto const proc = "/proc/" + std::to_string(pid) + "/exe";
    ssize_t const n = readlink(proc.c_str(), exe, sizeof exe - 1);
    std::string name;
    if (n > 0) {
        exe[n] = '\0';
        std::string const path{exe};
        auto const slash = path.rfind('/');
        name = slash == std::string::npos ? path : path.substr(slash + 1);
        // A replaced binary (package upgrade) reads "name (deleted)".
        if (auto const sp = name.find(" (deleted)"); sp != std::string::npos) name.erase(sp);
    }
    cache[pid] = name;
    return name;
}

// org.gnome.desktop.wm.preferences focus settings, read at most once a
// second (pointer motion asks on every event in sloppy mode).
UmojaFocusSettings focus_settings() {
    static auto last = std::chrono::steady_clock::time_point{};
    static UmojaFocusSettings cached{UMOJA_FOCUS_CLICK, false, 0, true, false};
    auto const now = std::chrono::steady_clock::now();
    if (now - last > std::chrono::seconds(1)) {
        last = now;
        umoja_focus_settings(&cached);
    }
    return cached;
}

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
        // mutter's first fit (place.c) when new windows aren't centred: the
        // first spot on the monitor that covers no other window.
        bool const app_type = !spec.type().is_set() || spec.type().value() == mir_window_type_normal ||
                              spec.type().value() == mir_window_type_freestyle;
        if (toplevel && app_type && !placed_by_state && !umoja_center_new_windows() && spec.size().is_set() &&
            spec.top_left().is_set()) {
            geom::Rectangle const r{spec.top_left().value(), spec.size().value()};
            if (auto const p = first_fit(area_at(centre_of(r)), r.size)) spec.top_left() = *p;
        }
        // Keep it all inside the work area, shrunk if it's bigger.
        if (toplevel && !placed_by_state && spec.size().is_set() && spec.top_left().is_set() &&
            !(spec.state().is_set() && spec.state().value() == mir_window_state_attached)) {
            geom::Rectangle const r{spec.top_left().value(), spec.size().value()};
            geom::Size const min{spec.min_width().is_set() ? spec.min_width().value() : geom::Width{1},
                                 spec.min_height().is_set() ? spec.min_height().value() : geom::Height{1}};
            auto const area = area_at(centre_of(r));
            auto const fitted = fit_inside(r, area, min);
            spec.top_left() = fitted.top_left;
            if (fitted.size != r.size) spec.size() = fitted.size;
            // org.gnome.mutter auto-maximize: a window that asks for nearly
            // the whole work area opens maximized.
            bool const fixed_size = spec.max_width().is_set() && spec.min_width().is_set() &&
                                    spec.max_width().value() == spec.min_width().value();
            if (app_type && !fixed_size && auto_maximizes(r.size, area)) {
                pending_auto_max = fitted;
                spec.state() = mir_window_state_maximized;
                spec.top_left() = area.top_left;
                spec.size() = area.size;
            }
        }
        return spec;
    }

    // org.gnome.mutter auto-maximize: mutter maximizes a new window that
    // covers at least 80% of the work area both ways (MAX_UNMAXIMIZED...).
    static bool auto_maximizes(geom::Size const& size, geom::Rectangle const& area) {
        if (!umoja_auto_maximize()) return false;
        int const aw = area.size.width.as_int(), ah = area.size.height.as_int();
        if (aw <= 0 || ah <= 0) return false;
        return size.width.as_int() * 10 >= aw * 8 && size.height.as_int() * 10 >= ah * 8;
    }

    // Set by place_new_window for a window it maximized: the floating
    // rectangle it gets back when unmaximized.
    std::optional<geom::Rectangle> pending_auto_max;

    // mutter's find_first_fit: the work area's centre if free, then next
    // to (right of, below) each window on the monitor, then its top left.
    std::optional<geom::Point> first_fit(geom::Rectangle const& area, geom::Size const& size) {
        std::vector<geom::Rectangle> others;
        for (auto const& w : stacking)
            if (all_windows.count(w) && visible_here(w)) {
                geom::Rectangle const r{w.top_left(), w.size()};
                if (area.overlaps(r)) others.push_back(r);
            }
        auto const fits = [&](geom::Point p) {
            geom::Rectangle const r{p, size};
            if (!area.contains(r)) return false;
            return std::none_of(others.begin(), others.end(), [&](auto const& o) { return o.overlaps(r); });
        };
        std::vector<geom::Point> candidates;
        candidates.push_back(optical_centre(area, size));
        std::sort(others.begin(), others.end(), [](auto const& a, auto const& b) {
            return std::pair{a.top_left.y.as_int(), a.top_left.x.as_int()} <
                   std::pair{b.top_left.y.as_int(), b.top_left.x.as_int()};
        });
        for (auto const& o : others) {
            candidates.push_back({o.top_left.x.as_int() + o.size.width.as_int(), o.top_left.y.as_int()});
            candidates.push_back({o.top_left.x.as_int(), o.top_left.y.as_int() + o.size.height.as_int()});
        }
        candidates.push_back(area.top_left);
        for (auto const& p : candidates)
            if (fits(p)) return p;
        return std::nullopt;
    }

    void advise_new_window(WindowInfo const& info) override {
        MinimalWindowManager::advise_new_window(info);
        umoja_wmp::window_created();
        auto const auto_max = pending_auto_max;
        pending_auto_max.reset();
        if (info.visible_on_lock_screen()) {
            lock_surfaces.push_back(info.window());
            if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: lock surface added (%zu)\n", lock_surfaces.size());
        }
        auto const unsized = unsized_placement;
        unsized_placement.reset();
        if (unsized && is_app_window(info)) awaiting_size[info.window()] = *unsized;
        if (!is_app_window(info)) {
            if (process_name(miral::pid_of(info.window().application())) == "umoja-dock")
                dock_windows.push_back(info.window());
            // Chrome mapped during a drag (tile preview) goes under the window.
            if (moving && elevated) guarded("raise dragged", [&] { tools.raise_tree(moving); });
            return;
        }
        all_windows.insert(info.window());
        if (auto_max) session_restore[info.window()] = *auto_max;
        // A new window ends Show Desktop, as in mutter.
        desktop_hidden.clear();
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
        erase(dock_windows, w);
        if (lock_focus == w) lock_focus = Window{};
        // Mir refocuses from its MRU list after this returns, which never
        // holds a lock surface that a grab kept from being focused.
        if (!lock_surfaces.empty() && tools.active_window() == w) defer_lock_focus();
        above.erase(w);
        preferred_output.erase(w);
        away.erase(w);
        retile_pending.erase(w);
        session_restore.erase(w);
        awaiting_size.erase(w);
        fullscreen_lifted.erase(w);
        pinned.erase(w);
        hidden_at.erase(w);
        tile_before_max.erase(w);
        desktop_hidden.erase(w);
        animations.erase(w);
        // GNOME's close effect: the window fades and shrinks out while Mir
        // (patch 3027) keeps drawing its surface.
        guarded("close animation", [&] { animate_close(info); });
        if (attention.erase(w)) umoja_windows_changed();
        if (all_windows.count(w)) umoja_window_gone(window_id(w));
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
        if (app) {
            umoja_windows_changed();
            schedule_fullscreen_refresh();
        }
    }

    // Focus without raising: sloppy and mouse focus, and raise-on-click
    // off (org.gnome.desktop.wm.preferences).
    bool suppress_raise = false;

    void advise_focus_gained(WindowInfo const& info) override {
        if (suppress_raise) application_selector_advise_focus_gained(info);
        else MinimalWindowManager::advise_focus_gained(info);
        Window const w = info.window();
        if (!lock_surfaces.empty()) {
            if (is_lock_surface(w)) lock_focus = w;
            else defer_lock_focus();
        }
        if (!all_windows.count(w)) return;
        umoja_windows_changed();
        erase(mru, w);
        mru.insert(mru.begin(), w);
        clear_attention(w);
        update_fullscreen_layer(w, info.state(), true);
        // Tiling Assistant's enable-raise-tile-group: a tiled window comes
        // up together with the windows tiled next to it.
        if (suppress_raise || raising_group || applying || !tiled.count(window_id(w)) || !umoja_raise_tile_group())
            return;
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
        Window const w = info.window();
        bool const app = all_windows.count(w) && !is_shell_process(w);
        bool takes_focus = true;
        if (app) {
            // focus-new-windows "strict": a new window from another app
            // than the focused one doesn't take focus; it asks for attention.
            Window const active = tools.active_window();
            if (focus_settings().new_windows_strict && active && all_windows.count(active) &&
                active.application() != w.application())
                takes_focus = false;
            // An app opening while the overview holds the keyboard (launched
            // from the dock, a notification, Ask Umoja) closes the overview.
            if (takes_focus && info.can_be_active()) guarded("overview grab", [&] { release_overlay_grab(); });
            guarded("open animation", [&] { animate(w, Anim::open); });
        }
        if (takes_focus) MinimalWindowManager::handle_window_ready(info);
        else guarded("attention", [&] { mark_attention(w); });
        if (is_lock_surface(w)) guarded("lock focus", [&] { ensure_lock_focus(); });
        else guarded("modal focus", [&] { take_focus_from_grab(info); });
    }

    // A window of Umoja's own shell (overlay, panel, dock), not an app.
    static bool is_shell_process(Window const& w) {
        return process_name(miral::pid_of(w.application())).rfind("umoja-", 0) == 0;
    }

    // The overview and Ask Umoja are keyboard-exclusive layer surfaces of
    // umoja-overlay, and Mir never moves focus off a grabbing window: so a
    // window activated from the dock or a notification stayed unfocused
    // behind the overview. GNOME leaves the overview on any outside
    // activation; drop the grab and tell the overlay (O6).
    void release_overlay_grab() {
        if (!lock_surfaces.empty()) return;
        Window const active = tools.active_window();
        if (!active) return;
        Window root = active;
        while (auto const parent = tools.info_for(root).parent()) root = parent;
        if (tools.info_for(root).focus_mode() != mir_focus_mode_grabbing) return;
        if (process_name(miral::pid_of(root.application())) != "umoja-overlay") return;
        if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: outside activation releases the overview's keyboard\n");
        tools.select_active_window(Window{});
        umoja_overview_dismiss();
    }

    // ---- attention (GNOME's "is ready", windowAttentionHandler.js) ----------

    std::set<Window> attention;

    void mark_attention(Window const& w) {
        if (!all_windows.count(w) || attention.count(w)) return;
        // Not while the app itself has focus (one of its windows).
        if (Window const active = tools.active_window(); active && active.application() == w.application()) return;
        attention.insert(w);
        auto const& info = tools.info_for(w);
        std::string const app = shell_app_id(info);
        std::string const title = info.name();
        if (debug_windows()) std::fprintf(stderr, "umoja-mir: wm: %s wants attention\n", describe(info).c_str());
        umoja_window_attention(window_id(w), app.c_str(), title.c_str());
        umoja_windows_changed();
    }

    void clear_attention(Window const& w) {
        if (!attention.erase(w)) return;
        umoja_window_attention_cleared(window_id(w));
        umoja_windows_changed();
    }

public:
    // Mir patch 3026: an xdg-activation request with no valid token.
    void activation_refused(Window w) {
        if (!w || !all_windows.count(w)) return;
        if (w == tools.active_window()) return;
        // A window that hasn't shown yet (Electron asks before its first
        // frame): handle_window_ready decides whether it takes focus.
        if (w.size().width.as_int() <= 0 || w.size().height.as_int() <= 0) return;
        mark_attention(w);
    }

    // Mir patch 3025: a right-click on a client-drawn title bar. (x, y)
    // are surface-local; the click itself is a better spot when known.
    void client_window_menu(Window w, int32_t x, int32_t y) {
        if (!w || !all_windows.count(w)) return;
        geom::Rectangle const r{w.top_left(), w.size()};
        geom::Point at = r.top_left + geom::Displacement{x, y};
        if (have_pointer && r.contains(last_pointer)) at = last_pointer;
        show_window_menu(w, at);
    }


    // A keyboard-exclusive surface that maps while another one holds the
    // keyboard gets it: the newest grab wins, as in wlroots compositors.
    // Mir's select_active_window() never leaves a grabbing window, so a
    // password dialog (polkit, Wi-Fi, keyring, mounts) opened from Ask
    // Umoja, an open shell menu or the screensaver showed but never got a
    // key. Popups are Mir's to handle: a grabbing xdg_popup (a GTK popover,
    // such as Ask Umoja's settings) is a menu, and a layer surface's popup
    // may have no parent here, so taking focus for it dropped the very grab
    // it belongs to and Mir closed it at once. While locked the lock screen
    // keeps the keyboard.
    void take_focus_from_grab(WindowInfo const& info) {
        if (!lock_surfaces.empty() || info.parent()) return;
        switch (info.type()) {
        case mir_window_type_menu:
        case mir_window_type_tip:
        case mir_window_type_gloss:
        case mir_window_type_satellite:
        case mir_window_type_inputmethod:
            return;
        default:
            break;
        }
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

    //
    // As mutter decides it (check_fullscreen_func): per monitor, from the
    // stacking order, not from focus. A monitor is "in fullscreen" while the
    // topmost window of the current workspace on it is fullscreen, focused
    // or not, so clicking a window on the other monitor no longer brings
    // the panel and dock back over a video (M2).
    void update_fullscreen_layer(Window const& w, MirWindowState, bool) {
        if (all_windows.count(w)) schedule_fullscreen_refresh();
    }

    static std::atomic<bool>& fullscreen_refresh_pending() {
        static std::atomic<bool> pending{false};
        return pending;
    }

    void schedule_fullscreen_refresh() {
        if (fullscreen_refresh_pending().exchange(true)) return;
        std::thread([] {
            guarded("fullscreen layer", [] {
                if (!g_wm) return;
                g_wm->invoke([] {
                    fullscreen_refresh_pending() = false;
                    if (g_wm) guarded("fullscreen layer", [] { g_wm->refresh_fullscreen(); });
                });
            });
        }).detach();
    }

    void refresh_fullscreen() {
        // Mid slide windows are off their spots; finish_slide() comes back.
        if (slide || swipe) return;
        std::set<Window> want;
        for (auto const& [id, out] : outputs) {
            for (auto it = stacking.rbegin(); it != stacking.rend(); ++it) {
                Window const& w = *it;
                if (!all_windows.count(w) || !visible_here(w) || !out.contains(centre_of(w))) continue;
                if (tools.info_for(w).state() == mir_window_state_fullscreen) want.insert(w);
                break;
            }
        }
        if (want == fullscreen_lifted) return;
        std::vector<Window> changed;
        for (auto const& w : fullscreen_lifted)
            if (!want.count(w)) changed.push_back(w);
        for (auto const& w : want)
            if (!fullscreen_lifted.count(w)) changed.push_back(w);
        fullscreen_lifted = want;
        for (auto const& w : changed) apply_layer(w);
    }

    // A fullscreen window covers this monitor (no hot corner there).
    bool monitor_in_fullscreen(geom::Rectangle const& output) const {
        return std::any_of(fullscreen_lifted.begin(), fullscreen_lifted.end(),
                           [&](Window const& w) { return output.contains(centre_of(w)); });
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
        if (app) {
            umoja_windows_changed();
            schedule_fullscreen_refresh();
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
        // The headless test (UMOJA_TRACK_MODIFIERS) traces every key.
        if (track_modifiers) {
            Window const f = tools.active_window();
            std::fprintf(stderr,
                         "umoja-mir: key %s sym 0x%x mods 0x%x held 0x%x bound %u focus '%s' inhibited %d grab %d\n",
                         action == mir_keyboard_action_down ? "down" : action == mir_keyboard_action_up ? "up" : "repeat",
                         keysym, mods, held_modifiers, umoja_lookup_binding(bind_sym, mods),
                         f ? tools.info_for(f).application_id().c_str() : "", f && umoja_wmp::inhibits_shortcuts(f) ? 1 : 0,
                         key_grab ? 1 : 0);
        }

        // Holding Super shows the dock's Super+1..9 numbers (Ubuntu Dock's
        // hotkeys-overlay). Only the change is sent; not while the focused
        // window has the shortcuts (below), but a release always is.
        if (action != mir_keyboard_action_repeat && umoja_overlay_key_matches(keysym)) {
            bool const down = action == mir_keyboard_action_down;
            if (down != super_held_sent) {
                Window const f = tools.active_window();
                if (!down || !(f && umoja_wmp::inhibits_shortcuts(f))) {
                    super_held_sent = down;
                    umoja_super_held(down);
                }
            }
        }

        // Alt+F7 / Alt+F8: the arrow keys move or resize the window.
        if (key_grab && handle_key_grab(action, keysym, mods)) return true;

        // keyboard-shortcuts-inhibit (virtual machines, remote desktops):
        // the focused window gets every key, Super and Alt+Tab included,
        // except mutter's restore-shortcuts (Super+Escape), which takes the
        // keys back. Volume and brightness keys keep working.
        if (Window const focused = tools.active_window(); focused && umoja_wmp::inhibits_shortcuts(focused)) {
            overlay_key_armed = false;
            ask_alt_armed = false;
            if (keysym == XK_ESCAPE && (mods & UMOJA_MOD_SUPER)) {
                if (action == mir_keyboard_action_down) umoja_wmp::restore_shortcuts(focused);
                return true;
            }
            uint32_t const bound = umoja_lookup_binding(bind_sym, mods);
            if (uint32_t const media = umoja_media_action(bound); media != UMOJA_MEDIA_NONE) {
                if (action == mir_keyboard_action_down ||
                    (action == mir_keyboard_action_repeat && media == UMOJA_MEDIA_REPEATS))
                    umoja_media_key(bound);
                return true;
            }
            return false;
        }

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
                return false;
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

        if (action == mir_keyboard_action_up) return false;
        if (bound != UMOJA_ACTION_NONE && umoja_is_switcher_action(bound)) {
            if (action == mir_keyboard_action_down) open_switcher(bound, mods);
            return true;
        }
        if (action != mir_keyboard_action_down) return false;
        if (bound != UMOJA_ACTION_NONE && apply(bound)) return true;
        // Not MinimalWindowManager's built-in Alt+F4/Alt+Tab/Alt+`: a key
        // the user unbound in Settings goes to the app, as in GNOME (F5).
        return false;
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

        // Super+scroll switches workspace (handleWorkspaceScroll).
        if (action == mir_pointer_action_motion && !moving && workspace_scroll(event)) return true;

        auto const focus = focus_settings();
        if (action == mir_pointer_action_button_down && pressed && !moving) {
            bool swallow = false;
            guarded("press on window", [&] { swallow = press_on_window(geom::Point{px, py}); });
            if (swallow) {
                menu_click_swallowed = buttons != 0;
                return true;
            }
        }

        // raise-on-click off: a click focuses without raising.
        suppress_raise = action == mir_pointer_action_button_down && !focus.raise_on_click;
        bool const consumed = MinimalWindowManager::handle_pointer_event(event);
        suppress_raise = false;

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
            else if (!moving) {
                report_motion(event, px, py);
                if (buttons == 0 && focus.mode != UMOJA_FOCUS_CLICK)
                    guarded("focus follows mouse", [&] { focus_follows_mouse(geom::Point{px, py}, focus); });
            }
        }
        return consumed;
    }

    // ---- Super+scroll -----------------------------------------------------

    std::chrono::steady_clock::time_point last_workspace_scroll;

    bool workspace_scroll(MirPointerEvent const* event) {
        float const v = mir_pointer_event_axis_value(event, mir_pointer_axis_vscroll);
        float const h = mir_pointer_event_axis_value(event, mir_pointer_axis_hscroll);
        if (v == 0.0f && h == 0.0f) return false;
        uint32_t const want = umoja_mouse_button_modifier();
        if (!want || to_umoja_mods(mir_pointer_event_modifiers(event)) != want) return false;
        // Holding Super to scroll is not a tap of it.
        overlay_key_armed = false;
        // gnome-shell's SCROLL_TIMEOUT_TIME: one switch per 150 ms.
        auto const now = std::chrono::steady_clock::now();
        if (now - last_workspace_scroll < std::chrono::milliseconds(150)) return true;
        last_workspace_scroll = now;
        float const d = v != 0.0f ? v : h;
        guarded("scroll workspace", [&] {
            apply(d > 0 ? UMOJA_ACTION_WORKSPACE_NEXT : UMOJA_ACTION_WORKSPACE_PREV);
        });
        return true;
    }

    // ---- a press on a window ------------------------------------------------

    // Returns true when the press must go no further: the window has a
    // modal dialog (xdg-dialog-v1), which comes up and takes focus instead.
    // Also checks the clicked app is still answering (F8).
    bool press_on_window(geom::Point p) {
        Window const under = tools.window_at(p);
        if (!under) return false;
        Window root = under;
        while (auto const parent = tools.info_for(root).parent()) root = parent;
        if (!all_windows.count(root)) return false;
        check_alive(root);
        if (under != root) return false;
        if (Window const dialog = modal_child(root)) {
            tools.select_active_window(dialog);
            tools.raise_tree(root);
            return true;
        }
        return false;
    }

    Window modal_child(Window const& w) {
        for (auto const& c : tools.info_for(w).children()) {
            if (auto const nested = modal_child(c)) return nested;
            auto const& ci = tools.info_for(c);
            if (ci.is_visible() && umoja_wmp::is_modal(c)) return c;
        }
        return Window{};
    }

    // xdg_wm_base ping for "App is not responding" (attention.rs). Not for
    // X11 windows: Xwayland has no xdg_wm_base, so it would never answer.
    void check_alive(Window const& w) {
        if (!all_windows.count(w)) return;
        pid_t const pid = miral::pid_of(w.application());
        if (pid <= 0 || process_name(pid) == "Xwayland" || is_shell_process(w)) return;
        auto const& info = tools.info_for(w);
        std::string const app = shell_app_id(info);
        std::string const title = info.name();
        umoja_check_alive(window_id(w), pid, app.c_str(), title.c_str());
    }

    // ---- focus follows mouse (focus-mode sloppy / mouse) --------------------

    Window hovered;
    uint64_t auto_raise_generation = 0;

    void focus_follows_mouse(geom::Point p, UmojaFocusSettings const& cfg) {
        if (!lock_surfaces.empty() || key_grab || slide || swipe) return;
        Window const under = tools.window_at(p);
        Window root = under;
        if (root)
            while (auto const parent = tools.info_for(root).parent()) root = parent;
        Window const active = tools.active_window();
        // An open menu or a keyboard grab (the overview, a shell menu) keeps
        // focus, as a GNOME grab does.
        if (active) {
            Window top = active;
            while (auto const parent = tools.info_for(top).parent()) top = parent;
            if (tools.info_for(top).focus_mode() == mir_focus_mode_grabbing) return;
            std::vector<Window> menus;
            collect_menus(tools, top, menus);
            if (!menus.empty()) return;
        }
        if (!under) {
            // Mouse mode: the desktop takes focus away (sloppy keeps it).
            if (cfg.mode == UMOJA_FOCUS_MOUSE && hovered && active && all_windows.count(active))
                tools.select_active_window(Window{});
            hovered = Window{};
            return;
        }
        if (under == hovered) return;
        hovered = under;
        // The panel, dock and overview are not focused by hovering.
        if (!all_windows.count(root) || under == active) return;
        suppress_raise = true;
        tools.select_active_window(under);
        suppress_raise = false;
        if (!cfg.auto_raise) return;
        uint64_t const gen = ++auto_raise_generation;
        Window const target = under;
        int const delay = std::clamp<int>(static_cast<int>(cfg.auto_raise_delay_ms), 0, 10000);
        std::thread([gen, target, delay] {
            std::this_thread::sleep_for(std::chrono::milliseconds(delay));
            guarded("auto raise", [&] {
                if (!g_wm) return;
                g_wm->invoke([&] {
                    if (g_wm && g_wm->auto_raise_generation == gen && g_wm->tools.active_window() == target)
                        guarded("auto raise", [&] { g_wm->tools.raise_tree(target); });
                });
            });
        }).detach();
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
        // Gestures are measured on the monitor the touch began on (M6).
        if (n > 0 && pts[0].action == static_cast<uint32_t>(mir_touch_action_down) && n == 1)
            touch_screen = output_at(geom::Point{static_cast<int>(pts[0].x), static_cast<int>(pts[0].y)});
        bool const known = std::any_of(outputs.begin(), outputs.end(), [&](auto const& o) { return o.second == touch_screen; });
        auto const r = known ? touch_screen : primary_output();
        UmojaRect const screen{r.top_left.x.as_int(), r.top_left.y.as_int(), r.size.width.as_int(),
                               r.size.height.as_int()};
        auto const time = mir_input_event_get_event_time(mir_touch_event_input_event(event));
        UmojaTouchResult res{};
        bool const consume = umoja_touch(pts, n, static_cast<uint64_t>(time), screen, &res);
        apply_swipe(res.swipe, res.progress, res.dir);
        if (consume) return true;
        return MinimalWindowManager::handle_touch_event(event);
    }

    geom::Rectangle touch_screen;

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
        bool retiled = false;
        if (spec.state().is_set() && all_windows.count(info.window()))
            guarded("client state", [&] { retiled = client_state_change(info, spec); });
        // Unminimized by a request (the dock's foreign-toplevel click): it
        // grows back out of its dock icon, as from activate().
        bool const unminimized = all_windows.count(info.window()) && info.state() == mir_window_state_minimized &&
                                 spec.state().is_set() && spec.state().value() != mir_window_state_minimized &&
                                 spec.state().value() != mir_window_state_hidden;
        if (trace)
            std::fprintf(stderr, "umoja-mir: wm: request for %s:%s -> applying%s\n", describe(info).c_str(),
                         describe(modifications).c_str(), describe(spec).c_str());
        bool const was_applying = applying;
        if (retiled) applying = true; // a move back into the tile isn't a drag out of it
        MinimalWindowManager::handle_modify_window(info, spec);
        applying = was_applying;
        if (unminimized && info.state() != mir_window_state_minimized)
            guarded("restore animation", [&] { animate(info.window(), Anim::restore); });
    }

    // Activation requests from windows on another workspace don't pull them
    // onto this one (GNOME shows a "ready" notice instead of switching).
    //
    // Other activations of a window on another workspace (the dock's icon
    // via foreign-toplevel, an xdg-activation token from a launcher) take
    // you to its workspace and focus it, as GNOME does. Chromium/Electron
    // and Qt re-raise themselves right after being hidden; those don't.
    //
    // With Mir patch 3026 a raise is known to be the user's (dock,
    // valid token) or not (an app raising itself, F3): the latter never
    // switches workspace and asks for attention instead (F2).
    void handle_raise_window(WindowInfo& info) override {
        Window const w = info.window();
        bool const user = take_user_activation(w);
        if (user) guarded("overview grab", [&] { release_overlay_grab(); });
        if (is_app_window(info) && on_other_workspace(w)) {
            if (user) {
                activate(w);
                return;
            }
            auto const it = hidden_at.find(w);
            if (it != hidden_at.end() && std::chrono::steady_clock::now() - it->second < std::chrono::seconds(1))
                return;
            if (!g_activation_hooks) activate(w);
            else mark_attention(w);
            return;
        }
        if (user && info.state() == mir_window_state_minimized) {
            activate(w);
            return;
        }
        MinimalWindowManager::handle_raise_window(info);
    }

    static bool take_user_activation(Window const& w) {
        std::shared_ptr<mir::scene::Surface> const s = w;
        void const* expected = s.get();
        if (!expected || steady_ms() - g_user_activation_ms.load() > 2000) return false;
        return g_user_activation.compare_exchange_strong(expected, nullptr);
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
        // Touch moves stay Mir's.
        if (!all_windows.count(info.window()) || mir_input_event_get_type(input_event) != mir_input_event_type_pointer) {
            MinimalWindowManager::handle_request_move(info, input_event);
            return;
        }
        auto const* pev = mir_input_event_get_pointer_event(input_event);
        // A request that arrives after the button is already up would start
        // a drag that no release ever ends.
        if (held_buttons == 0) {
            if (debug_windows())
                std::fprintf(stderr, "umoja-mir: wm: move request ignored, no button held\n");
            return;
        }
        if (mod_grab || moving) return;
        int32_t const px = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_x));
        int32_t const py = static_cast<int32_t>(mir_pointer_event_axis_value(pev, mir_pointer_axis_y));
        // Our own move, not Mir's pointer gesture: Mir's stops as soon as a
        // modifier changes (pressing Shift mid-drag froze the window, F6),
        // and ours keeps the title bar on screen and snaps to edges.
        Window const w = info.window();
        guarded("begin move", [&] { begin_move(w, px, py, was_tiled_or_maximized(w)); });
        if (moving != w) return;
        uint32_t const primary = mir_pointer_button_primary;
        uint32_t const button = (held_buttons & primary) ? primary : held_buttons;
        ModGrab g{w, ModGrabKind::moving, button, geom::Point{px, py}, geom::Point{px, py}, {}};
        g.from_client = true;
        mod_grab = g;
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
        if (!rehoming && user_moving(info.window())) {
            remember_monitor(info.window());
            away.erase(info.window());
            retile_pending.erase(info.window());
        }
        if (all_windows.count(info.window()) && update_workspace_membership(info.window())) relayout_workspaces();
        if (applying) return;
        // The user dragged a tiled window away: it leaves its tile group.
        untile(info.window());
    }

    // ---- outputs ----------------------------------------------------------

    // Mir reports one display configuration as a run of create/update/delete
    // calls, and an unplug is often followed by a second configuration
    // (monitors.xml layout for the monitors that are left). Windows are
    // moved once things settle (settle_outputs), not call by call: moving
    // them while half the new layout is known put them in the wrong place.
    void advise_output_create(Output const& output) override {
        outputs[output.id()] = output.extents();
        output_names[output.id()] = output.name();
        output_refresh[output.id()] = output.refresh_rate();
        update_slide_pace();
        report(output);
        schedule_output_settle();
    }

    void advise_output_update(Output const& updated, Output const&) override {
        outputs[updated.id()] = updated.extents();
        output_names[updated.id()] = updated.name();
        output_refresh[updated.id()] = updated.refresh_rate();
        update_slide_pace();
        report(updated);
        schedule_output_settle();
    }

    void advise_output_delete(Output const& output) override {
        outputs.erase(output.id());
        output_names.erase(output.id());
        output_refresh.erase(output.id());
        update_slide_pace();
        umoja_output_remove(output.id());
        schedule_output_settle();
    }

    // ---- monitors coming and going ------------------------------------------
    //
    // mutter's meta_window_update_for_monitors_changed: a window whose
    // preferred monitor (below) is connected goes to it; otherwise it stays
    // on its monitor if that is still there, following it if the layout
    // moved it; otherwise it goes to the primary monitor. A window leaving
    // a monitor that went away remembers exactly where it was there, and
    // gets that back when the monitor returns (mutter keeps the window's
    // unconstrained rect for the same reason), unless the user has moved it
    // in between. Maximized, fullscreen and tiled windows stay so.
    //
    // Mir also "removes" every output when the screen blanks (power mode
    // off). With no output left nothing moves, and when they come back on
    // the layout is the one windows were placed for, so nothing moves then
    // either.

    // Output connector name -> extents, as windows were last placed for.
    std::map<std::string, geom::Rectangle> settled_layout;

    struct Away {
        std::string output;         // the monitor it left
        geom::Rectangle output_rect; // that monitor's extents then
        geom::Rectangle rect;        // where the window was on it
    };
    std::map<Window, Away> away;

    // Tiled windows moved to a monitor whose panel and dock haven't
    // reserved their space yet: tiled again once its work area is known.
    std::set<Window> retile_pending;

    static std::atomic<uint64_t>& output_generation() {
        static std::atomic<uint64_t> g{0};
        return g;
    }

    // From Mir's display configuration callbacks: apply from a helper
    // thread that takes the WM lock once Mir has been quiet for a moment.
    void schedule_output_settle() {
        auto const gen = ++output_generation();
        std::thread([gen] {
            std::this_thread::sleep_for(std::chrono::milliseconds(300));
            if (output_generation().load() != gen) return;
            guarded("settle outputs", [] {
                if (!g_wm) return;
                g_wm->invoke([] {
                    if (g_wm) guarded("settle outputs", [] { g_wm->settle_outputs(); });
                });
            });
        }).detach();
    }

    std::string output_key(int id) const {
        auto const it = output_names.find(id);
        if (it != output_names.end() && !it->second.empty()) return it->second;
        return "#" + std::to_string(id);
    }

    std::map<std::string, geom::Rectangle> current_layout() const {
        std::map<std::string, geom::Rectangle> layout;
        for (auto const& [id, r] : outputs) layout[output_key(id)] = r;
        return layout;
    }

    std::string primary_output_name() const {
        auto const primary = primary_output();
        for (auto const& [id, r] : outputs)
            if (r == primary) return output_key(id);
        return {};
    }

    void settle_outputs() {
        auto const now = current_layout();
        if (now.empty()) return; // screen off, or nothing connected: wait
        if (animating) {
            // Mid workspace slide windows are off their real spots.
            schedule_output_settle();
            return;
        }
        auto const before = settled_layout.empty() ? now : settled_layout;
        settled_layout = now;
        if (before == now) return;
        if (debug_windows()) {
            std::string b, n;
            for (auto const& [name, r] : before) b += " " + name;
            for (auto const& [name, r] : now) n += " " + name;
            std::fprintf(stderr, "umoja-mir: wm: monitors changed:%s ->%s\n", b.c_str(), n.c_str());
        }
        std::vector<Window> const windows{all_windows.begin(), all_windows.end()};
        auto const primary = primary_output_name();
        bool const was_rehoming = rehoming;
        rehoming = true;
        for (auto const& w : windows) {
            guarded("monitor change", [&] { rehome(w, before, now, primary); });
        }
        rehoming = was_rehoming;
    }

    void rehome(Window const& w, std::map<std::string, geom::Rectangle> const& before,
                std::map<std::string, geom::Rectangle> const& now, std::string const& primary) {
        if (!all_windows.count(w)) return;
        geom::Rectangle const rect{w.top_left(), w.size()};
        auto const c = centre_of(rect);

        // The monitor it was on, in the layout it was placed for.
        std::string from_name;
        geom::Rectangle from;
        for (auto const& [name, r] : before)
            if (r.contains(c)) {
                from_name = name;
                from = r;
            }
        if (from_name.empty()) {
            // Not on any monitor of the old layout (placed since, or off
            // screen): where it is now, if anywhere.
            for (auto const& [name, r] : now)
                if (r.contains(c)) {
                    from_name = name;
                    from = r;
                }
        }

        std::string target;
        auto const pref = preferred_output.find(w);
        if (pref != preferred_output.end() && now.count(pref->second)) target = pref->second;
        else if (!from_name.empty() && now.count(from_name)) target = from_name;
        else target = primary;
        auto const to_it = now.find(target);
        if (to_it == now.end()) return;
        auto const to = to_it->second;
        if (from_name.empty()) from = to; // clamp it onto its monitor

        if (from_name == target && from == to && to.contains(c)) return;

        // Leaving a monitor that went away: remember the spot on it.
        if (!from_name.empty() && !now.count(from_name)) away[w] = Away{from_name, from, rect};

        geom::Rectangle dest;
        auto const a = away.find(w);
        if (a != away.end() && a->second.output == target) {
            // Back to the monitor it left: the same spot. A returning
            // monitor may first show up somewhere else and only then get
            // its stored position and scale, so the spot is kept until the
            // monitor is back exactly as it was.
            dest = translate(a->second.rect, a->second.output_rect, to);
            from = a->second.output_rect;
            if (a->second.output_rect == to) away.erase(a);
        } else {
            dest = translate(rect, from, to);
        }
        place_on_monitor(w, from, to, dest);
    }

    static geom::Rectangle translate(geom::Rectangle const& r, geom::Rectangle const& from, geom::Rectangle const& to) {
        // Same size: just shift it, exactly (Rust's relative maths can
        // round a pixel off). Otherwise scale the spot and fit it inside.
        if (from.size == to.size && from.contains(centre_of(r))) return geom::Rectangle{r.top_left + (to.top_left - from.top_left), r.size};
        return from_rect(umoja_translate_rect(to_rect(r), to_rect(from), to_rect(to)));
    }

    // Put a window at `dest` on monitor `to` (it came from `from`), keeping
    // it maximized, fullscreen or tiled there.
    void place_on_monitor(Window const& w, geom::Rectangle const& from, geom::Rectangle const& to,
                          geom::Rectangle const& dest) {
        auto& info = tools.info_for(w);
        WindowSpecification spec;
        spec.top_left() = dest.top_left;
        spec.size() = dest.size;
        for (auto const& [id, r] : outputs)
            if (r == to) spec.output_id() = id;

        auto const id = window_id(w);
        auto const action = tile_action.find(w);
        bool const is_tiled = tiled.count(id) && action != tile_action.end();
        if (auto const u = untiled.find(w); u != untiled.end()) u->second = translate(u->second, from, to);

        auto const state = info.state();
        if (state == mir_window_state_fullscreen) {
            // Mir moves a fullscreen window between outputs as a state
            // change with a new output_id.
            spec.state() = state;
            tools.place_and_size_for_state(spec, info);
        } else if (state == mir_window_state_maximized || state == mir_window_state_horizmaximized ||
                   state == mir_window_state_vertmaximized) {
            // Not as a state change: setting the state it already has keeps
            // it attached to the old output's display area, and Mir's next
            // relayout pulls it back there. An output_id alone re-attaches
            // it to the new one.
            WindowSpecification sized = spec;
            sized.state() = state;
            tools.place_and_size_for_state(sized, info);
            if (sized.top_left().is_set()) spec.top_left() = sized.top_left().value();
            if (sized.size().is_set()) spec.size() = sized.size().value();
        } else if (is_tiled) {
            bool const known = has_zone_on(to);
            UmojaRect out{};
            if (umoja_tile_geometry(action->second, to_rect(area_on(to)), &out)) {
                spec.top_left() = geom::Point{out.x, out.y};
                spec.size() = geom::Size{out.w, out.h};
            }
            if (known) retile_pending.erase(w);
            else retile_pending.insert(w);
        }
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: monitor change moves %s ->%s\n", describe(info).c_str(),
                         describe(spec).c_str());
        bool const was_applying = applying;
        applying = true; // a tiled window stays tiled
        tools.modify_window(info, spec);
        applying = was_applying;
        if (is_tiled) {
            umoja_tiling_unregister(id);
            umoja_tiling_register(id, action->second, to_rect({w.top_left(), w.size()}));
        }
    }

    bool has_zone_on(geom::Rectangle const& output) const {
        for (auto const& [id, r] : zones)
            if (output.contains(centre_of(r))) return true;
        return false;
    }

    // Zone callbacks come from inside Mir's own relayout: tile afterwards.
    void schedule_retile() {
        if (retile_pending.empty()) return;
        std::thread([] {
            guarded("retile", [] {
                if (!g_wm) return;
                g_wm->invoke([] {
                    if (g_wm) guarded("retile", [] { g_wm->retile_moved(); });
                });
            });
        }).detach();
    }

    void retile_moved() {
        std::vector<Window> const pending{retile_pending.begin(), retile_pending.end()};
        for (auto const& w : pending) {
            auto const id = window_id(w);
            auto const action = tile_action.find(w);
            if (!all_windows.count(w) || !tiled.count(id) || action == tile_action.end()) {
                retile_pending.erase(w);
                continue;
            }
            auto const output = output_at(centre_of(w));
            if (!has_zone_on(output)) continue;
            retile_pending.erase(w);
            UmojaRect out{};
            if (!umoja_tile_geometry(action->second, to_rect(area_on(output)), &out)) continue;
            WindowSpecification spec;
            spec.top_left() = geom::Point{out.x, out.y};
            spec.size() = geom::Size{out.w, out.h};
            bool const was_applying = applying;
            applying = true;
            tools.modify_window(tools.info_for(w), spec);
            applying = was_applying;
            umoja_tiling_unregister(id);
            umoja_tiling_register(id, action->second, out);
        }
    }

    // ---- preferred monitor ------------------------------------------------
    //
    // mutter's window->preferred_logical_monitor: the monitor a window was
    // opened on or last moved to. Taking a monitor away doesn't change it,
    // so when that monitor comes back (meta_window_update_for_monitors_changed)
    // its windows move back to it (settle_outputs). Kept by connector name,
    // as Mir may number a returning output differently.

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
            umoja_fx::set_tiled_edges(w, 15);
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
            else if (auto const p = first_fit(area, size))
                top_left = *p;
            else
                top_left = optical_centre(area, size) + cascade;
            auto const fitted = fit_inside({top_left, size}, area, {info.min_width(), info.min_height()});
            if (resizable(info) && auto_maximizes(size, area)) {
                // Unmaximizing goes back to the size it asked for.
                session_restore[w] = fitted;
                spec.top_left() = fitted.top_left;
                spec.state() = mir_window_state_maximized;
                tools.place_and_size_for_state(spec, info);
                return;
            }
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

    void advise_application_zone_create(Zone const& zone) override {
        zones[zone.id()] = zone.extents();
        schedule_retile();
    }
    void advise_application_zone_update(Zone const& updated, Zone const&) override {
        zones[updated.id()] = updated.extents();
        schedule_retile();
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
        // Leaving Show Desktop, and the overview if it holds the keyboard.
        desktop_hidden.clear();
        guarded("overview grab", [&] { release_overlay_grab(); });
        cancel_animation(w);
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
                animate(w, Anim::restore);
            }
            tools.raise_tree(w);
            tools.select_active_window(w);
        });
    }

    void close(Window w) {
        if (!w) return;
        guarded("check alive", [&] { check_alive(w); });
        guarded("close", [&] { tools.ask_client_to_close(w); });
    }

    int32_t window_pid(Window const& w) const { return w ? miral::pid_of(w.application()) : 0; }

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

    // Where the pointer last was, in layout pixels (GetPointer).
    bool pointer_position(int32_t* x, int32_t* y) const {
        if (!have_pointer) return false;
        *x = last_pointer.x.as_int();
        *y = last_pointer.y.as_int();
        return true;
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
            move_dragged(w, p);
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
    bool super_held_sent = false;
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
        float const dx = mir_pointer_event_axis_value(event, mir_pointer_axis_relative_x);
        float const dy = mir_pointer_event_axis_value(event, mir_pointer_axis_relative_y);
        umoja_pointer_motion(px, py, dx, dy, edges);

        // GNOME's hot corner (Layout.HotCorner): the top-left corner of
        // every monitor that has one (no monitor beyond either edge), top
        // right in right-to-left locales; off while a fullscreen window
        // covers the monitor. hot_corner.rs measures the pressure.
        bool const rtl = umoja_workspaces_rtl();
        int32_t const cx = rtl ? right : left;
        int32_t const outside = rtl ? right + 1 : left - 1;
        bool const corner = open(outside, top) && open(cx, top - 1) && !monitor_in_fullscreen(o);
        constexpr int32_t barrier = 32; // about a top bar's height, as gnome-shell's
        bool const on_side = corner && px == cx && py < top + barrier;
        bool const on_top = corner && py <= top && (rtl ? px > right - barrier : px < left + barrier);
        umoja_hot_corner_motion(rtl ? -dx : dx, dy, on_side, on_top);
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
        // org.gnome.shell.app-switcher / window-switcher
        // current-workspace-only; Alt+` follows the app switcher's (A1, A2).
        bool const only_here = umoja_switcher_current_workspace_only(action);
        std::vector<std::string> strings;
        std::vector<Window> list;
        for (auto const& w : mru)
            if (!only_here || on_current_workspace(w)) list.push_back(w);
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
        // Started by the client's xdg_toplevel.move (a title-bar drag): the
        // client saw the press, so it gets the release too.
        bool from_client = false;
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
            bool const from_client = g.from_client;
            if (g.kind == ModGrabKind::moving && moving) end_move();
            mod_grab.reset();
            return !from_client; // else the client never saw the press
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
            guarded("begin modifier move", [&] { begin_move(w, g.start.x.as_int(), g.start.y.as_int(), stuck); });
            if (moving == w) guarded("modifier move", [&] { move_dragged(w, p); });
            g.last = p;
            if (moving) on_move(px, py);
            return true;
        }
        case ModGrabKind::menu:
            return true;
        case ModGrabKind::moving:
            if (moving == g.window) {
                Window const w = g.window;
                guarded("move", [&] { move_dragged(w, p); });
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
        move_offset = w.top_left() - p;
        umoja_drag_begin(px, py, to_rect({w.top_left(), w.size()}), to_rect(area_at(p)), to_rect(output_at(p)));
    }

    // Where the window's top left sits relative to the pointer in a move.
    geom::Displacement move_offset;

    // Follow the pointer, keeping the title bar reachable (mutter's
    // constrain_titlebar_visible, P5) and sticking to the edges of the
    // work area and of other windows (edge-resistance.c, P8).
    void move_dragged(Window w, geom::Point pointer) {
        if (!all_windows.count(w)) return;
        auto const want = constrain_move(w, pointer + move_offset, pointer);
        if (want != w.top_left()) tools.drag_window(w, want - w.top_left());
    }

    geom::Point constrain_move(Window const& w, geom::Point want, geom::Point pointer) {
        int x = want.x.as_int(), y = want.y.as_int();
        int const ww = w.size().width.as_int(), wh = w.size().height.as_int();
        auto const area = area_at(pointer);
        auto const out = output_at(pointer);
        int const ax = area.top_left.x.as_int(), ay = area.top_left.y.as_int();
        int const ar = ax + area.size.width.as_int(), ab = ay + area.size.height.as_int();

        constexpr int snap = 12;
        std::vector<int> xs{ax, ar}, ys{ay, ab};
        for (auto const& o : stacking) {
            if (o == w || !all_windows.count(o) || !visible_here(o)) continue;
            geom::Rectangle const r{o.top_left(), o.size()};
            if (!out.overlaps(r)) continue;
            xs.push_back(r.top_left.x.as_int());
            xs.push_back(r.top_left.x.as_int() + r.size.width.as_int());
            ys.push_back(r.top_left.y.as_int());
            ys.push_back(r.top_left.y.as_int() + r.size.height.as_int());
        }
        auto const stick = [&](int& v, int len, std::vector<int> const& edges) {
            int best = snap + 1, to = v;
            for (int e : edges) {
                if (int const d = std::abs(v - e); d < best) best = d, to = e;           // leading edge
                if (int const d = std::abs(v + len - e); d < best) best = d, to = e - len; // trailing edge
            }
            if (best <= snap) v = to;
        };
        stick(x, ww, xs);
        stick(y, wh, ys);

        // The title bar never goes above the work area, and a strip of the
        // window stays on the monitor.
        constexpr int keep = 64;
        int const ox = out.top_left.x.as_int(), oy = out.top_left.y.as_int();
        int const orr = ox + out.size.width.as_int(), ob = oy + out.size.height.as_int();
        y = std::max(y, ay);
        y = std::min(y, ob - std::min(keep, wh));
        x = std::clamp(x, ox - ww + std::min(keep, ww), std::max(ox, orr - std::min(keep, ww)));
        return {x, y};
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
        slide_sign = umoja_workspaces_rtl() ? -1.0 : 1.0;
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
    // gnome-shell's WORKSPACE_SWITCH_TIME (W5). Frames are paced to the
    // fastest monitor's refresh (g_slide_frame_us).
    static constexpr double SLIDE_MS = 250.0;
    // -1 in right-to-left locales: the next workspace is to the left (W4).
    double slide_sign = 1.0;

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
        int const px = static_cast<int>(std::lround(slide_sign * offset * it.monitor.size.width.as_int()));
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
        schedule_fullscreen_refresh();
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
        // Right to left: fingers moving right go to the next workspace.
        if (umoja_workspaces_rtl()) {
            progress = -progress;
            dir = -dir;
        }
        if (flags & UMOJA_SWIPE_BEGIN) workspace_swipe_begin();
        if (flags & UMOJA_SWIPE_UPDATE) workspace_swipe_update(progress);
        if (flags & UMOJA_SWIPE_END) workspace_swipe_end(dir);
    }

    void workspace_swipe_begin() {
        if (swipe) return;
        finish_slide();
        slide_sign = umoja_workspaces_rtl() ? -1.0 : 1.0;
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

    // A new, empty workspace at `index` (GNOME's insertWorkspace); the
    // ones from there on move along by one.
    void insert_workspace(size_t index) {
        index = std::min(index, workspaces.size());
        workspaces.insert(workspaces.begin() + static_cast<long>(index), tools.create_workspace());
        if (index <= current) ++current;
        umoja_workspaces_changed(current, workspaces.size());
    }

    // The overview's drop between two thumbnails (O2): a new workspace
    // there, with the window on it. A window on a secondary monitor (on
    // every workspace) comes to the primary monitor, as GNOME's
    // moveWindowToMonitorAndWorkspace does.
    bool send_to_new_workspace(Window w, size_t index) {
        if (!w || !all_windows.count(w) || pinned.count(w)) return false;
        finish_slide();
        if (!in_any_workspace(w)) {
            auto const from = output_at(centre_of(w));
            auto const to = primary_output();
            if (from == to) return false;
            guarded("to primary", [&] { place_on_monitor(w, from, to, translate({w.top_left(), w.size()}, from, to)); });
            remember_monitor(w);
            away.erase(w);
            update_workspace_membership(w);
            if (!in_any_workspace(w)) return false;
        }
        insert_workspace(index);
        send_to_workspace(w, std::min(index, workspaces.size() - 1));
        return true;
    }

    // The overview's drop inside a thumbnail or on a monitor's card (O4,
    // M1): the window goes to (x, y), on whichever monitor that is, and to
    // `workspace` (-1: stays on its own). Maximized, fullscreen and tiled
    // windows keep their state and only change monitor.
    bool place_window(Window w, int32_t workspace, int32_t x, int32_t y) {
        if (!w || !all_windows.count(w)) return false;
        finish_slide();
        auto& info = tools.info_for(w);
        geom::Rectangle const rect{w.top_left(), w.size()};
        geom::Point const tl{x, y};
        auto const from = output_at(centre_of(rect));
        auto const to = output_at(centre_of(geom::Rectangle{tl, rect.size}));
        auto state = info.state();
        if (auto const it = hidden_states.find(w); it != hidden_states.end()) state = it->second;
        bool const fixed = tiled.count(window_id(w)) || state == mir_window_state_maximized ||
                           state == mir_window_state_fullscreen || state == mir_window_state_horizmaximized ||
                           state == mir_window_state_vertmaximized;
        guarded("place window", [&] {
            if (fixed) {
                if (from != to) place_on_monitor(w, from, to, translate(rect, from, to));
                return;
            }
            auto const fitted = fit_inside({tl, rect.size}, area_on(to), {info.min_width(), info.min_height()});
            WindowSpecification spec;
            spec.top_left() = fitted.top_left;
            for (auto const& [id, r] : outputs)
                if (r == to && from != to) spec.output_id() = id;
            tools.modify_window(info, spec);
        });
        remember_monitor(w);
        away.erase(w);
        bool const changed = update_workspace_membership(w);
        if (workspace >= 0 && static_cast<size_t>(workspace) < workspaces.size() && in_any_workspace(w))
            send_to_workspace(w, static_cast<size_t>(workspace));
        else if (changed)
            relayout_workspaces();
        umoja_windows_changed();
        return true;
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
        if (tiled.erase(id)) {
            umoja_tiling_unregister(id);
            // No longer tiled for the app either (P1).
            umoja_fx::set_tiled_edges(w, 0);
        }
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
        // GNOME: moving a window left from the first workspace, with
        // dynamic workspaces, makes a new first workspace for it (W1).
        if (action == UMOJA_ACTION_MOVE_TO_WORKSPACE_PREV && current == 0 && umoja_workspaces_dynamic()) {
            Window const active = tools.active_window();
            if (active && all_windows.count(active) && in_any_workspace(active)) {
                bool others = false;
                tools.for_each_window_in_workspace(workspaces[0], [&](Window const& w) { others |= w != active; });
                if (others) {
                    insert_workspace(0);
                    move_window_to(active, 0);
                    umoja_workspace_switched(current, workspaces.size());
                    return true;
                }
            }
        }

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
        case UMOJA_ACTION_RUN_DIALOG:
        case UMOJA_ACTION_TOGGLE_QUICK_SETTINGS:
        case UMOJA_ACTION_FOCUS_ACTIVE_NOTIFICATION:
        case UMOJA_ACTION_SWITCH_MONITOR:
        case UMOJA_ACTION_SWITCH_INPUT_SOURCE:
        case UMOJA_ACTION_SWITCH_INPUT_SOURCE_BACKWARD:
            umoja_handle_action(action);
            return true;
        case UMOJA_ACTION_SHOW_DESKTOP:
            toggle_show_desktop();
            return true;
        default:
            break;
        }
        // Super+1..9 and Super+Ctrl+1..9 (the dock's apps).
        if (action >= UMOJA_ACTION_SWITCH_TO_APP_BASE && action < UMOJA_ACTION_NEW_WINDOW_APP_BASE + 100) {
            umoja_handle_action(action);
            return true;
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
            check_alive(target);
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
        case UMOJA_ACTION_RAISE:
            tools.raise_tree(target);
            return true;
        case UMOJA_ACTION_LOWER:
            lower(target);
            return true;
        case UMOJA_ACTION_RAISE_OR_LOWER:
            if (all_windows.count(target) && topmost_here(target)) lower(target);
            else tools.raise_tree(target);
            return true;
        case UMOJA_ACTION_MAXIMIZE_VERTICALLY:
            return set_state(info, info.state() == mir_window_state_vertmaximized ? mir_window_state_restored
                                                                                    : mir_window_state_vertmaximized);
        case UMOJA_ACTION_MAXIMIZE_HORIZONTALLY:
            return set_state(info, info.state() == mir_window_state_horizmaximized ? mir_window_state_restored
                                                                                     : mir_window_state_horizmaximized);
        case UMOJA_ACTION_MOVE_TO_CENTER:
        case UMOJA_ACTION_MOVE_TO_CORNER_NW:
        case UMOJA_ACTION_MOVE_TO_CORNER_NE:
        case UMOJA_ACTION_MOVE_TO_CORNER_SW:
        case UMOJA_ACTION_MOVE_TO_CORNER_SE:
        case UMOJA_ACTION_MOVE_TO_SIDE_N:
        case UMOJA_ACTION_MOVE_TO_SIDE_S:
        case UMOJA_ACTION_MOVE_TO_SIDE_E:
        case UMOJA_ACTION_MOVE_TO_SIDE_W:
            if (!all_windows.count(target)) return false;
            return move_within_area(target, action);
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
        // xdg-shell's tiled states (P1): GTK and libadwaita square their
        // corners and drop the shadow, as on GNOME.
        umoja_fx::set_tiled_edges(target, 15);
        return true;
    }

    // ---- Show Desktop (S1) ----------------------------------------------------

    // The windows Show Desktop minimized, and the state each had.
    std::map<Window, MirWindowState> desktop_hidden;

    void toggle_show_desktop() {
        if (!desktop_hidden.empty()) {
            auto const hidden = std::move(desktop_hidden);
            desktop_hidden.clear();
            Window top;
            applying = true;
            // A copy: restoring a window raises it, which reorders stacking
            // (advise_raise), and only the first window came back.
            std::vector<Window> const order{stacking.begin(), stacking.end()};
            for (auto const& w : order) {
                auto const it = hidden.find(w);
                if (it == hidden.end() || !all_windows.count(w) || !on_current_workspace(w)) continue;
                auto& info = tools.info_for(w);
                if (info.state() != mir_window_state_minimized) continue;
                WindowSpecification spec;
                spec.state() = it->second;
                tools.place_and_size_for_state(spec, info);
                guarded("show desktop", [&] { tools.modify_window(info, spec); });
                top = w;
            }
            applying = false;
            for (auto const& w : mru)
                if (hidden.count(w) && visible_here(w)) {
                    tools.select_active_window(w);
                    break;
                }
            (void)top;
            return;
        }
        // Nothing takes focus while the desktop shows (F1).
        tools.select_active_window(Window{});
        std::vector<Window> const here{stacking.begin(), stacking.end()};
        applying = true;
        for (auto const& w : here) {
            if (!all_windows.count(w) || !visible_here(w)) continue;
            auto& info = tools.info_for(w);
            desktop_hidden[w] = info.state();
            WindowSpecification spec;
            spec.state() = mir_window_state_minimized;
            guarded("show desktop", [&] { tools.modify_window(info, spec); });
        }
        applying = false;
    }

    // ---- raise, lower, move to a corner (S4) ----------------------------------

    bool topmost_here(Window const& w) {
        for (auto it = stacking.rbegin(); it != stacking.rend(); ++it)
            if (visible_here(*it)) return *it == w;
        return false;
    }

    void lower(Window const& w) {
        guarded("lower", [&] { tools.send_tree_to_back(w); });
        if (!all_windows.count(w)) return;
        erase(stacking, w);
        stacking.insert(stacking.begin(), w);
        umoja_windows_changed();
        schedule_fullscreen_refresh();
    }

    // mutter's move-to-center, move-to-corner-* and move-to-side-*.
    bool move_within_area(Window w, uint32_t action) {
        auto& info = tools.info_for(w);
        if (info.state() != mir_window_state_restored) return true;
        auto const area = area_on(output_at(centre_of(w)));
        int const ax = area.top_left.x.as_int(), ay = area.top_left.y.as_int();
        int const aw = area.size.width.as_int(), ah = area.size.height.as_int();
        int const ww = w.size().width.as_int(), wh = w.size().height.as_int();
        int const left = ax, right = ax + aw - ww, cx = ax + (aw - ww) / 2;
        int const top = ay, bottom = ay + ah - wh, cy = ay + (ah - wh) / 2;
        int x = w.top_left().x.as_int(), y = w.top_left().y.as_int();
        switch (action) {
        case UMOJA_ACTION_MOVE_TO_CENTER: x = cx; y = cy; break;
        case UMOJA_ACTION_MOVE_TO_CORNER_NW: x = left; y = top; break;
        case UMOJA_ACTION_MOVE_TO_CORNER_NE: x = right; y = top; break;
        case UMOJA_ACTION_MOVE_TO_CORNER_SW: x = left; y = bottom; break;
        case UMOJA_ACTION_MOVE_TO_CORNER_SE: x = right; y = bottom; break;
        case UMOJA_ACTION_MOVE_TO_SIDE_N: x = cx; y = top; break;
        case UMOJA_ACTION_MOVE_TO_SIDE_S: x = cx; y = bottom; break;
        case UMOJA_ACTION_MOVE_TO_SIDE_E: x = right; y = cy; break;
        case UMOJA_ACTION_MOVE_TO_SIDE_W: x = left; y = cy; break;
        default: return false;
        }
        untile(w);
        tile_action.erase(w);
        WindowSpecification spec;
        spec.top_left() = geom::Point{x, y};
        tools.modify_window(info, spec);
        return true;
    }

    // ---- window animations (P7) ------------------------------------------------
    //
    // GNOME's effects, kept quick: a new window fades and grows in (150 ms);
    // a minimized one shrinks into its app's dock icon and a restored one
    // grows back out of it (220 ms; in place, fading, when its app has no
    // icon). org.gnome.desktop.interface enable-animations turns them off.
    // Frames come from a helper thread under the WM lock, as the workspace
    // slide's do.

    enum class Anim { open, minimize, restore };
    struct Animation {
        Anim kind;
        std::chrono::steady_clock::time_point start;
        double ms;
        // The dock icon it flies into or out of, and the window's own
        // rectangle, for minimize and restore.
        std::optional<geom::Rectangle> icon;
        geom::Rectangle rect;
    };
    static constexpr double OPEN_MS = 150.0;
    static constexpr double MINIMIZE_MS = 300.0;

    // umoja-dock's windows (one per monitor), for the minimize target.
    std::vector<Window> dock_windows;

    // The window's app icon on the dock of its monitor (or on any dock), in
    // layout pixels.
    std::optional<geom::Rectangle> dock_icon_for(WindowInfo const& info) {
        std::string const app = info.application_id();
        if (app.empty()) return {};
        Window const& w = info.window();
        geom::Point const centre{w.top_left().x.as_int() + w.size().width.as_int() / 2,
                                 w.top_left().y.as_int() + w.size().height.as_int() / 2};
        std::optional<geom::Rectangle> any;
        for (auto const& d : dock_windows) {
            geom::Rectangle const dock{d.top_left(), d.size()};
            for (auto const& [id, r] : outputs) {
                if (!r.contains(dock.top_left)) continue;
                UmojaRect const mon{r.top_left.x.as_int(), r.top_left.y.as_int(), r.size.width.as_int(),
                                    r.size.height.as_int()};
                UmojaRect local{};
                if (!umoja_dock_icon(app.c_str(), mon, &local) || local.w <= 0 || local.h <= 0) break;
                geom::Rectangle const icon{{dock.top_left.x.as_int() + local.x, dock.top_left.y.as_int() + local.y},
                                           {local.w, local.h}};
                if (r.contains(centre)) return icon;
                if (!any) any = icon;
                break;
            }
        }
        return any;
    }

    // Window `w` at progress `e` (0 = as it is, 1 = at the icon).
    void fly(Window const& w, Animation const& a, double e) {
        auto const& icon = *a.icon;
        double const wx = a.rect.top_left.x.as_int() + a.rect.size.width.as_int() / 2.0;
        double const wy = a.rect.top_left.y.as_int() + a.rect.size.height.as_int() / 2.0;
        double const ix = icon.top_left.x.as_int() + icon.size.width.as_int() / 2.0;
        double const iy = icon.top_left.y.as_int() + icon.size.height.as_int() / 2.0;
        double const sx = icon.size.width.as_int() / std::max(1.0, double(a.rect.size.width.as_int()));
        double const sy = icon.size.height.as_int() / std::max(1.0, double(a.rect.size.height.as_int()));
        // Keep the window's shape: shrink to the icon's larger side.
        double const s = std::max(sx, sy);
        float const scale = float(1.0 + (s - 1.0) * e);
        // Mostly opaque on the way, gone as it lands.
        float const alpha = float(1.0 - e * e * 0.9);
        umoja_fx::set_look_at(w, alpha, scale, float((ix - wx) * e), float((iy - wy) * e));
    }
    std::map<Window, Animation> animations;
    bool animations_running = false;

    void animate(Window const& w, Anim kind) {
        if (!all_windows.count(w)) return;
        if (!umoja_animations_enabled()) {
            if (kind == Anim::minimize) finish_minimize(w);
            return;
        }
        Animation a{kind, std::chrono::steady_clock::now(), OPEN_MS, std::nullopt, {w.top_left(), w.size()}};
        if (kind != Anim::open) {
            a.ms = MINIMIZE_MS;
            guarded("dock icon", [&] { a.icon = dock_icon_for(tools.info_for(w)); });
        }
        animations[w] = a;
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: animate %s %s, dock icon %s\n",
                         kind == Anim::open ? "open" : kind == Anim::minimize ? "minimize" : "restore",
                         describe(tools.info_for(w)).c_str(),
                         a.icon ? (std::to_string(a.icon->top_left.x.as_int()) + "," +
                                   std::to_string(a.icon->top_left.y.as_int()) + " " +
                                   std::to_string(a.icon->size.width.as_int()) + "x" +
                                   std::to_string(a.icon->size.height.as_int())).c_str()
                                : "none");
        if (kind == Anim::open || (kind == Anim::restore && !a.icon)) umoja_fx::set_look(w, 0.0f, 0.94f);
        if (kind == Anim::restore && a.icon) fly(w, a, 1.0);
        run_animations();
    }

    // A normal window or dialog that is on screen as it closes.
    void animate_close(WindowInfo const& info) {
        if (!umoja_animations_enabled()) return;
        auto const type = info.type();
        if (type != mir_window_type_normal && type != mir_window_type_dialog) return;
        if (info.state() == mir_window_state_minimized || info.state() == mir_window_state_hidden) return;
        bool const started = umoja_fx::begin_close(info.window(), 150.0);
        if (debug_windows())
            std::fprintf(stderr, "umoja-mir: wm: animate close %s: %s\n", describe(info).c_str(),
                         started ? "started" : "not visible");
        if (started) run_animations();
    }

    // Start the frame thread unless it is running.
    void run_animations() {
        if (animations_running) return;
        animations_running = true;
        std::thread([] {
            auto next = std::chrono::steady_clock::now();
            for (;;) {
                next += std::chrono::microseconds(g_slide_frame_us.load(std::memory_order_relaxed));
                auto const now = std::chrono::steady_clock::now();
                if (next < now) next = now;
                std::this_thread::sleep_until(next);
                bool more = false;
                guarded("animation frame", [&] {
                    if (!g_wm) return;
                    g_wm->invoke([&] {
                        if (g_wm) more = g_wm->step_animations();
                    });
                });
                if (!more) return;
            }
        }).detach();
    }

    // Stop a window's animation and show it as it is.
    void cancel_animation(Window const& w) {
        if (!animations.erase(w)) return;
        umoja_fx::set_look(w, 1.0f, 1.0f);
    }

public:
    bool step_animations() {
        auto const now = std::chrono::steady_clock::now();
        std::vector<Window> minimized;
        for (auto it = animations.begin(); it != animations.end();) {
            Window const w = it->first;
            if (!all_windows.count(w)) {
                it = animations.erase(it);
                continue;
            }
            double const t = std::chrono::duration<double, std::milli>(now - it->second.start).count() / it->second.ms;
            if (t >= 1.0) {
                bool const was_minimize = it->second.kind == Anim::minimize;
                it = animations.erase(it);
                if (was_minimize) minimized.push_back(w);
                else umoja_fx::set_look(w, 1.0f, 1.0f);
                continue;
            }
            if (debug_windows())
                std::fprintf(stderr, "umoja-mir: wm: animation frame t=%.2f\n", t);
            double const out = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t); // ease-out cubic
            // The flight to and from the dock covers a long way: ease in and
            // out, so no frame jumps much further than the average (ease-in
            // cubic moved a fifth of the way in its last frame: choppy).
            double const io = t < 0.5 ? 4.0 * t * t * t : 1.0 - std::pow(-2.0 * t + 2.0, 3.0) / 2.0;
            auto const& a = it->second;
            if (a.kind == Anim::minimize && a.icon) fly(w, a, io);
            else if (a.kind == Anim::restore && a.icon) fly(w, a, 1.0 - io);
            else if (a.kind == Anim::minimize) umoja_fx::set_look(w, float(1.0 - out), float(1.0 - 0.15 * out));
            else umoja_fx::set_look(w, float(out), float(0.94 + 0.06 * out));
            ++it;
        }
        for (auto const& w : minimized) {
            guarded("minimize", [&] { finish_minimize(w); });
            umoja_fx::set_look(w, 1.0f, 1.0f);
        }
        bool const closing = umoja_fx::step_closing();
        if (animations.empty() && !closing) animations_running = false;
        return animations_running;
    }

private:
    void finish_minimize(Window const& w) {
        if (!all_windows.count(w)) return;
        auto& info = tools.info_for(w);
        if (info.state() == mir_window_state_minimized || info.state() == mir_window_state_hidden) return;
        if (w == tools.active_window()) focus_away_from(w);
        WindowSpecification spec;
        spec.state() = mir_window_state_minimized;
        tools.place_and_size_for_state(spec, info);
        tools.modify_window(info, spec);
    }

    // mutter: the active window going away (minimized) passes focus to the
    // most recent window still shown on this workspace, or to nothing. Mir
    // would pick any window, minimized ones included (F1).
    void focus_away_from(Window const& w) {
        for (auto const& o : mru) {
            if (o == w || !on_current_workspace(o) || !showing(o)) continue;
            tools.select_active_window(o);
            return;
        }
        tools.select_active_window(Window{});
    }

    // A client changed its own state (title-bar double-click, its
    // minimize button). Returns true when it went back into its tile.
    bool client_state_change(WindowInfo& info, WindowSpecification& spec) {
        Window const w = info.window();
        auto const to = spec.state().value();
        auto const id = window_id(w);
        if (to == mir_window_state_minimized) {
            // Its own minimize button shrinks it into the dock too: the
            // state is applied when the animation ends (finish_minimize).
            bool const active = w == tools.active_window();
            if (showing(w) && info.state() != mir_window_state_minimized && umoja_animations_enabled()) {
                spec.state().consume();
                if (active) focus_away_from(w);
                animate(w, Anim::minimize);
                return false;
            }
            cancel_animation(w);
            if (active) focus_away_from(w);
            return false;
        }
        if ((to == mir_window_state_maximized || to == mir_window_state_fullscreen) && tiled.count(id)) {
            // Maximizing a tile remembers it (P2).
            if (auto const a = tile_action.find(w); a != tile_action.end()) tile_before_max[w] = a->second;
            untile(w);
            tile_action.erase(w);
            return false;
        }
        if (to != mir_window_state_restored) return false;
        if (info.state() != mir_window_state_maximized && info.state() != mir_window_state_fullscreen) return false;
        auto const it = tile_before_max.find(w);
        if (it == tile_before_max.end()) return false;
        uint32_t const action = it->second;
        tile_before_max.erase(it);
        UmojaRect out{};
        if (!umoja_tile_geometry(action, to_rect(area_on(output_at(centre_of(w)))), &out)) return false;
        spec.top_left() = geom::Point{out.x, out.y};
        spec.size() = geom::Size{out.w, out.h};
        tiled[id] = w;
        tile_action[w] = action;
        umoja_tiling_register(id, action, out);
        umoja_fx::set_tiled_edges(w, 15);
        return true;
    }

    // Tiles that were maximized, to go back to on unmaximize (P2).
    std::map<Window, uint32_t> tile_before_max;

    // ---- keyboard move / resize -----------------------------------------------

    static constexpr int KEY_ABOVE_TAB = 41; // evdev KEY_GRAVE
    static constexpr int KEY_COPILOT_F23 = 193; // evdev KEY_F23
    static constexpr uint32_t XK_ALT_L = 0xffe9;
    static constexpr uint32_t XK_GRAVE = 0x60;
    static constexpr uint32_t XK_ESCAPE = 0xff1b;

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
            y = std::max(y, area_at(centre_of(g.window)).top_left.y.as_int());
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
        away.erase(w);
        retile_pending.erase(w);
        return true;
    }

    bool set_state(WindowInfo& info, MirWindowState state, bool forget_untiled = true) {
        Window const w = info.window();
        auto const id = window_id(w);
        // Maximizing a tile remembers the tile, unmaximizing goes back into
        // it (P2), as mutter does.
        if (state == mir_window_state_maximized && tiled.count(id))
            if (auto const a = tile_action.find(w); a != tile_action.end()) tile_before_max[w] = a->second;
        if (state == mir_window_state_restored && info.state() == mir_window_state_maximized) {
            if (auto const it = tile_before_max.find(w); it != tile_before_max.end()) {
                uint32_t const action = it->second;
                tile_before_max.erase(it);
                WindowSpecification spec;
                spec.state() = mir_window_state_restored;
                applying = true;
                guarded("unmaximize to tile", [&] { tools.modify_window(info, spec); });
                applying = false;
                return apply_to(w, action, false);
            }
        }
        if (state != mir_window_state_maximized) tile_before_max.erase(w);
        untile(w);
        tile_action.erase(w);
        if (forget_untiled) untiled.erase(w);
        // A window hidden on another workspace stays hidden: minimizing it
        // only changes what it comes back as when that workspace is shown
        // (else unhide() would restore it there, minimized or not).
        if (state == mir_window_state_minimized) {
            if (auto const it = hidden_states.find(w); it != hidden_states.end()) {
                it->second = state;
                return true;
            }
            // Focus moves on now, then the window shrinks away (P7, F1).
            if (all_windows.count(w) && showing(w)) {
                if (w == tools.active_window()) focus_away_from(w);
                animate(w, Anim::minimize);
                return true;
            }
        }
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

extern "C" bool umoja_wm_pointer(int32_t* out_x, int32_t* out_y) {
    bool ok = false;
    *out_x = 0;
    *out_y = 0;
    with_wm("pointer", [&] { ok = g_wm->pointer_position(out_x, out_y); });
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

extern "C" bool umoja_wm_send_to_new_workspace(uint64_t id, uint32_t index) {
    bool ok = false;
    with_wm("send to new workspace", [&] { ok = g_wm->send_to_new_workspace(g_wm->find(id), index); });
    return ok;
}

extern "C" bool umoja_wm_place_window(uint64_t id, int32_t workspace, int32_t x, int32_t y) {
    bool ok = false;
    with_wm("place window", [&] { ok = g_wm->place_window(g_wm->find(id), workspace, x, y); });
    return ok;
}

extern "C" int32_t umoja_wm_window_pid(uint64_t id) {
    int32_t pid = 0;
    with_wm("window pid", [&] { pid = g_wm->window_pid(g_wm->find(id)); });
    return pid;
}

// ---- hooks Umoja's Mir patches look up with dlsym(RTLD_DEFAULT) ------------
// Called on Mir's Wayland thread; anything that needs the window manager
// goes through a helper thread that takes the WM lock (never take it here:
// the Wayland thread may be inside a call that already holds it).

// Patch 3026: the user activated `surface` (dock, valid activation token);
// Mir raises it right after this returns.
extern "C" void mir_user_activation_hook(void const* surface) {
    g_activation_hooks = true;
    g_user_activation_ms = steady_ms();
    g_user_activation = surface;
}

// Patch 3026: `surface` asked to be activated without a valid token.
extern "C" void mir_activation_refused_hook(void const* surface) {
    g_activation_hooks = true;
    auto const id = reinterpret_cast<uint64_t>(surface);
    std::thread([id] { with_wm("activation refused", [&] { g_wm->activation_refused(g_wm->find(id)); }); }).detach();
}

// Patch 3025: xdg_toplevel.show_window_menu (right-click on a header bar).
extern "C" void mir_show_window_menu_hook(void const* surface, int32_t x, int32_t y) {
    auto const id = reinterpret_cast<uint64_t>(surface);
    std::thread([id, x, y] {
        with_wm("window menu request", [&] { g_wm->client_window_menu(g_wm->find(id), x, y); });
    }).detach();
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

bool is_xwayland_client(WaylandExtensions::EnableInfo const& info) {
    pid_t const pid = miral::pid_of(info.app());
    if (pid <= 0) return false;
    char exe[PATH_MAX];
    auto const proc = "/proc/" + std::to_string(pid) + "/exe";
    ssize_t const n = readlink(proc.c_str(), exe, sizeof exe - 1);
    if (n <= 0) return false;
    exe[n] = '\0';
    std::string const path{exe};
    auto const slash = path.rfind('/');
    return (slash == std::string::npos ? path : path.substr(slash + 1)) == "Xwayland";
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
    // Modal dialogs, and VM / remote desktop viewers taking every key
    // (wm_protocols.cpp).
    extensions.add_extension(umoja_wmp::dialog_extension());
    extensions.add_extension(umoja_wmp::shortcuts_inhibit_extension());
    // An X11 VM or remote desktop viewer's keyboard grab, from Xwayland only.
    extensions.add_extension(umoja_wmp::xwayland_grab_extension());
    extensions.conditionally_enable("zwp_xwayland_keyboard_grab_manager_v1", is_xwayland_client);

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

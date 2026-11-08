// C ABI between the Rust side of umoja-mir and the MirAL C++ shim.
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    int32_t x, y, w, h;
} UmojaRect;

// Keep in sync with `tiling::Action` in Rust.
enum {
    UMOJA_ACTION_NONE = 0,
    UMOJA_ACTION_TILE_LEFT = 1,
    UMOJA_ACTION_TILE_RIGHT = 2,
    UMOJA_ACTION_MAXIMIZE = 3,
    UMOJA_ACTION_UNMAXIMIZE = 4,
    UMOJA_ACTION_MINIMIZE = 5,
    UMOJA_ACTION_TOGGLE_OVERLAY = 6,
    UMOJA_ACTION_TILE_TOP_LEFT = 7,
    UMOJA_ACTION_TILE_TOP_RIGHT = 8,
    UMOJA_ACTION_TILE_BOTTOM_LEFT = 9,
    UMOJA_ACTION_TILE_BOTTOM_RIGHT = 10,
    UMOJA_ACTION_CLOSE = 11,
    UMOJA_ACTION_TOGGLE_DICTATION = 12,
    UMOJA_ACTION_TOGGLE_MAXIMIZED = 13,
    UMOJA_ACTION_TOGGLE_FULLSCREEN = 14,
    UMOJA_ACTION_WORKSPACE_PREV = 15,
    UMOJA_ACTION_WORKSPACE_NEXT = 16,
    UMOJA_ACTION_WORKSPACE_LAST = 17,
    UMOJA_ACTION_MOVE_TO_WORKSPACE_PREV = 18,
    UMOJA_ACTION_MOVE_TO_WORKSPACE_NEXT = 19,
    UMOJA_ACTION_MOVE_TO_WORKSPACE_LAST = 20,
    UMOJA_ACTION_MOVE_TO_MONITOR_LEFT = 21,
    UMOJA_ACTION_MOVE_TO_MONITOR_RIGHT = 22,
    UMOJA_ACTION_MOVE_TO_MONITOR_UP = 23,
    UMOJA_ACTION_MOVE_TO_MONITOR_DOWN = 24,
    UMOJA_ACTION_SCREENSHOT_UI = 25,
    UMOJA_ACTION_SCREENSHOT = 26,
    UMOJA_ACTION_SCREENSHOT_WINDOW = 27,
    UMOJA_ACTION_TILE_TOP_HALF = 28,
    UMOJA_ACTION_TILE_BOTTOM_HALF = 29,
    UMOJA_ACTION_SWITCH_APPLICATIONS = 30,
    UMOJA_ACTION_SWITCH_APPLICATIONS_BACKWARD = 31,
    UMOJA_ACTION_SWITCH_GROUP = 32,
    UMOJA_ACTION_SWITCH_GROUP_BACKWARD = 33,
    UMOJA_ACTION_SWITCH_WINDOWS = 34,
    UMOJA_ACTION_SWITCH_WINDOWS_BACKWARD = 35,
    UMOJA_ACTION_TOGGLE_OVERVIEW = 36,
    UMOJA_ACTION_ASK_ABOUT_SCREEN = 37,
    UMOJA_ACTION_CLIPBOARD_HISTORY = 38,
    UMOJA_ACTION_SHIFT_OVERVIEW_UP = 39,
    UMOJA_ACTION_SHIFT_OVERVIEW_DOWN = 40,
    // 100 + n: switch to workspace n; 200 + n: move window to workspace n.
    UMOJA_ACTION_SWITCH_TO_WORKSPACE_BASE = 100,
    UMOJA_ACTION_MOVE_TO_WORKSPACE_BASE = 200,
};

enum {
    UMOJA_WORKSPACE_SWITCH = 1,
    UMOJA_WORKSPACE_MOVE_WINDOW = 2,
};

// Our own modifier bits (independent of Mir's enum values).
enum {
    UMOJA_MOD_SHIFT = 1 << 0,
    UMOJA_MOD_CTRL = 1 << 1,
    UMOJA_MOD_ALT = 1 << 2,
    UMOJA_MOD_SUPER = 1 << 3,
};

// Alt+Tab switcher (src/shell.rs, src/switcher.rs).
typedef struct {
    uint64_t id;
    char const* app_id;
    char const* title;
} UmojaSwitchWindow;

#define UMOJA_SWITCH_MAX 64
typedef struct {
    uint64_t window;                // to activate
    uint64_t ids[UMOJA_SWITCH_MAX]; // to raise first (activate) or to close
    size_t n_ids;
} UmojaSwitchResult;

enum {
    UMOJA_SWITCH_NOT_OURS = 0,
    UMOJA_SWITCH_CONSUMED = 1,
    UMOJA_SWITCH_ACTIVATE = 2,
    UMOJA_SWITCH_CLOSE = 3,
    UMOJA_SWITCH_CANCEL = 4,
};

// org.umoja.Compositor window listing (src/shell.rs).
typedef struct {
    uint64_t id;
    char const* app_id;
    char const* title;
    UmojaRect rect;
    uint32_t flags;
    // 0-based workspace, or -1 when the window is on every workspace
    // (a secondary monitor with workspaces-only-on-primary).
    int32_t workspace;
} UmojaWindowDesc;

enum {
    UMOJA_WINDOW_FOCUSED = 1 << 0,
    UMOJA_WINDOW_MINIMIZED = 1 << 1,
    UMOJA_WINDOW_CURRENT_WORKSPACE = 1 << 2,
    UMOJA_WINDOW_MAXIMIZED = 1 << 3,
    UMOJA_WINDOW_FULLSCREEN = 1 << 4,
    UMOJA_WINDOW_TILED = 1 << 5,
};

typedef void (*UmojaWindowCallback)(void* ctx, UmojaWindowDesc const* desc);

typedef struct {
    // Wayland extensions to force-enable (layer shell, foreign toplevel...).
    const char* const* extensions;
    size_t n_extensions;
    bool enable_x11;
} UmojaConfig;

// Implemented by the shim, called from Rust main().
int umoja_mir_run(int argc, char const** argv, UmojaConfig const* config);

// Implemented in Rust (src/ffi.rs), called by the shim.
uint32_t umoja_lookup_binding(uint32_t keysym, uint32_t modifiers);
bool umoja_overlay_key_matches(uint32_t keysym);
bool umoja_tile_geometry(uint32_t action, UmojaRect area, UmojaRect* out);
void umoja_tiling_register(uint64_t id, uint32_t action, UmojaRect rect);
void umoja_tiling_unregister(uint64_t id);
size_t umoja_tiling_resized(uint64_t id, UmojaRect new_rect, uint64_t* out_ids, UmojaRect* out_rects, size_t max);
void umoja_handle_action(uint32_t action);
bool umoja_workspace_resolve(uint32_t code, size_t current, size_t count, uint32_t* out_kind, size_t* out_index);
size_t umoja_workspace_layout(bool const* occupied, size_t n, size_t current, size_t* out_remove, size_t max_remove,
                              size_t* out_n_remove);
bool umoja_workspaces_only_on_primary(void);
bool umoja_center_new_windows(void);
int32_t umoja_neighbor_output(uint32_t action, UmojaRect from, UmojaRect const* rects, size_t n);
UmojaRect umoja_translate_rect(UmojaRect window, UmojaRect from, UmojaRect to);
void umoja_screenshot_window(UmojaRect rect);
void umoja_output_update(int32_t id, char const* name, UmojaRect extents, float scale, double refresh, uint32_t width_mm,
                         uint32_t height_mm, uint32_t orientation);
void umoja_output_remove(int32_t id);
void umoja_session_started(char const* wayland_display);

// Scroll source classes for umoja_natural_scroll (peripherals.rs).
enum {
    UMOJA_SCROLL_TOUCHPAD = 1,
    UMOJA_SCROLL_MOUSE = 2,
};
// True when scrolls from this source should be inverted (GNOME's
// org.gnome.desktop.peripherals.{touchpad,mouse} natural-scroll).
bool umoja_natural_scroll(uint32_t source);
bool umoja_is_switcher_action(uint32_t action);
bool umoja_switcher_open(uint32_t action, uint32_t mods, UmojaSwitchWindow const* windows, size_t n);
uint32_t umoja_switcher_key(uint32_t keysym, uint32_t mods, bool down, UmojaSwitchResult* out);
uint32_t umoja_switcher_pick(uint32_t index, UmojaSwitchResult* out);
void umoja_switcher_window_closed(uint64_t id);
void umoja_switcher_cancel(void);
void umoja_drag_begin(int32_t px, int32_t py, UmojaRect window, UmojaRect area, UmojaRect monitor);
uint32_t umoja_drag_motion(int32_t px, int32_t py, UmojaRect window, UmojaRect area, UmojaRect monitor);
uint32_t umoja_drag_end(void);
bool umoja_raise_tile_group(void);
void umoja_workspace_switched(size_t current, size_t count);
// The current workspace or the number of workspaces may have changed.
void umoja_workspaces_changed(size_t current, size_t count);
bool umoja_is_tile_action(uint32_t action);

// Implemented by the shim, called from Rust's D-Bus thread. Each takes the
// window manager lock, so they are safe from any thread except Mir's own.
void umoja_wm_list_windows(UmojaWindowCallback cb, void* ctx, bool mru);
void umoja_wm_activate(uint64_t id);
void umoja_wm_close(uint64_t id);
void umoja_wm_switcher_pick(uint32_t index);
void umoja_wm_tile(uint64_t id, uint32_t action);
void umoja_wm_workspaces(uint32_t* out_current, uint32_t* out_count);
void umoja_wm_switch_workspace(uint32_t index);
void umoja_wm_move_to_workspace(uint64_t id, uint32_t index);
// Without the switcher popup and, for the window, without following it
// (the overview's thumbnails and drag and drop).
void umoja_wm_show_workspace(uint32_t index);
void umoja_wm_send_to_workspace(uint64_t id, uint32_t index);
// Workspace GSettings changed: re-apply count and primary-only membership.
void umoja_wm_workspace_settings_changed(void);

// Screen blanking (src/idle.rs, src/shim/idle.cpp). Seconds before the
// outputs turn off, unlocked and on the lock screen; 0 = never.
void umoja_idle_set_timeouts(int32_t off_seconds, int32_t locked_off_seconds);
enum {
    UMOJA_IDLE_DIM = 1,
    UMOJA_IDLE_OFF = 2,
    UMOJA_IDLE_WAKE = 3,
    UMOJA_IDLE_LOCKED = 4, // ext-session-lock engaged
    UMOJA_IDLE_UNLOCKED = 5,
};
// Mir's idle or session-lock state changed (called on a Mir thread).
void umoja_idle_state(uint32_t state);

#ifdef __cplusplus
}

namespace mir { class Server; }
// Hooks Mir's idle handler up (idle.cpp).
void umoja_idle_setup(mir::Server& server);
#endif

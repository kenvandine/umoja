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
uint32_t umoja_edge_zone(int32_t px, int32_t py, UmojaRect area);
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

#ifdef __cplusplus
}
#endif

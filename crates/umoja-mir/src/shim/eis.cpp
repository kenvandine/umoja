// libei connections (EIS, the server side of libei), for the remote desktop
// and input capture portals (src/eis.rs, src/remote_desktop.rs,
// src/input_capture.rs).
//
// Remote desktop: the client (a remote desktop tool) emulates input on
// devices we offer, and what it sends goes into its session's virtual
// devices (remote_input.cpp).
//
// Input capture: the other way round. Once the pointer pushes through one
// of the client's barriers on a screen edge (Deskflow, Input Leap and the
// like put the other computer's screen there), local input is captured: it
// stops reaching apps here and goes to the client through our devices,
// until the client releases it (or the user presses Super+Escape, as in
// GNOME).

#include "umoja_shim.h"

#include <libeis.h>
#include <linux/input-event-codes.h>
#include <mir_toolkit/events/event.h>
#include <mir_toolkit/events/input/input_event.h>
#include <mir_toolkit/events/input/keyboard_event.h>
#include <mir_toolkit/events/input/pointer_event.h>
#include <xkbcommon/xkbcommon-keysyms.h>

#include <fcntl.h>
#include <poll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <unistd.h>

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

std::string umoja_remote_keymap_string();

namespace {

enum class Mode { remote_desktop, input_capture };

struct Connection;
// The input capture client went away (eis.cpp, "Input capture" below).
void capture_client_gone(Connection* c);

struct Connection {
    Mode mode;
    uint64_t remote = 0;  // remote_desktop: the session's input
    uint32_t capabilities = 0;  // input_capture: UMOJA_CAPTURE_*
    std::vector<UmojaRegion> regions;

    std::recursive_mutex mutex;  // libeis isn't thread-safe
    eis* ctx = nullptr;
    int stop_fd = -1;
    std::thread thread;
    std::vector<eis_device*> devices;  // ours, referenced
    eis_device* capture_device = nullptr;
    bool emulating = false;
    uint32_t sequence = 0;

    ~Connection() {
        if (stop_fd >= 0) {
            uint64_t one = 1;
            [[maybe_unused]] auto r = write(stop_fd, &one, sizeof one);
        }
        if (thread.joinable()) thread.join();
        std::lock_guard lock{mutex};
        for (auto* d : devices) eis_device_unref(d);
        devices.clear();
        if (ctx) eis_unref(ctx);
        if (stop_fd >= 0) close(stop_fd);
    }

    void run();
    void handle(eis_event* event);
    void add_devices(eis_seat* seat, eis_event* bind);
};

int keymap_fd(size_t* size) {
    std::string const keymap = umoja_remote_keymap_string();
    if (keymap.empty()) return -1;
    int const fd = memfd_create("umoja-eis-keymap", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) return -1;
    size_t const n = keymap.size() + 1;
    if (ftruncate(fd, (off_t)n) != 0 || write(fd, keymap.c_str(), n) != (ssize_t)n) {
        close(fd);
        return -1;
    }
    fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_SEAL);
    *size = n;
    return fd;
}

void Connection::add_devices(eis_seat* seat, eis_event* bind) {
    auto const has = [&](eis_device_capability cap) {
        return eis_event_seat_has_capability(bind, cap) && eis_seat_has_capability(seat, cap);
    };
    // A fresh set of devices for what the client bound.
    for (auto* d : devices) {
        eis_device_remove(d);
        eis_device_unref(d);
    }
    devices.clear();
    capture_device = nullptr;

    auto make = [&](char const* name) {
        eis_device* d = eis_seat_new_device(seat);
        eis_device_configure_name(d, name);
        eis_device_configure_type(d, EIS_DEVICE_TYPE_VIRTUAL);
        return d;
    };
    bool const pointer = has(EIS_DEVICE_CAP_POINTER);
    bool const absolute = has(EIS_DEVICE_CAP_POINTER_ABSOLUTE);
    bool const keyboard = has(EIS_DEVICE_CAP_KEYBOARD);
    bool const touch = has(EIS_DEVICE_CAP_TOUCH);
    bool const button = has(EIS_DEVICE_CAP_BUTTON);
    bool const scroll = has(EIS_DEVICE_CAP_SCROLL);

    if (mode == Mode::input_capture) {
        // One device carries everything that is captured.
        if (!pointer && !keyboard) return;
        eis_device* d = make("Umoja captured input");
        if (pointer) eis_device_configure_capability(d, EIS_DEVICE_CAP_POINTER);
        if (button) eis_device_configure_capability(d, EIS_DEVICE_CAP_BUTTON);
        if (scroll) eis_device_configure_capability(d, EIS_DEVICE_CAP_SCROLL);
        if (keyboard) {
            eis_device_configure_capability(d, EIS_DEVICE_CAP_KEYBOARD);
            size_t size = 0;
            int const fd = keymap_fd(&size);
            if (fd >= 0) {
                eis_keymap* km = eis_device_new_keymap(d, EIS_KEYMAP_TYPE_XKB, fd, size);
                eis_keymap_add(km);
                eis_keymap_unref(km);
                close(fd);
            }
        }
        eis_device_add(d);
        // Paused until input is captured.
        devices.push_back(d);
        capture_device = d;
        return;
    }

    if (pointer || button || scroll) {
        eis_device* d = make("Umoja remote pointer");
        if (pointer) eis_device_configure_capability(d, EIS_DEVICE_CAP_POINTER);
        if (button) eis_device_configure_capability(d, EIS_DEVICE_CAP_BUTTON);
        if (scroll) eis_device_configure_capability(d, EIS_DEVICE_CAP_SCROLL);
        eis_device_add(d);
        eis_device_resume(d);
        devices.push_back(d);
    }
    if (absolute || touch) {
        // Absolute positions are in layout coordinates, one region per
        // monitor (with the stream's mapping id, for clients that match
        // regions to streams).
        eis_device* d = make(touch && !absolute ? "Umoja remote touchscreen" : "Umoja remote absolute pointer");
        if (absolute) eis_device_configure_capability(d, EIS_DEVICE_CAP_POINTER_ABSOLUTE);
        if (touch) eis_device_configure_capability(d, EIS_DEVICE_CAP_TOUCH);
        if (absolute && button) eis_device_configure_capability(d, EIS_DEVICE_CAP_BUTTON);
        if (absolute && scroll) eis_device_configure_capability(d, EIS_DEVICE_CAP_SCROLL);
        for (auto const& r : regions) {
            eis_region* region = eis_device_new_region(d);
            eis_region_set_offset(region, (uint32_t)std::max(0, r.x), (uint32_t)std::max(0, r.y));
            eis_region_set_size(region, (uint32_t)r.w, (uint32_t)r.h);
            eis_region_set_physical_scale(region, r.scale > 0 ? r.scale : 1.0);
            eis_region_add(region);
            eis_region_unref(region);
        }
        eis_device_add(d);
        eis_device_resume(d);
        devices.push_back(d);
    }
    if (keyboard) {
        eis_device* d = make("Umoja remote keyboard");
        eis_device_configure_capability(d, EIS_DEVICE_CAP_KEYBOARD);
        size_t size = 0;
        int const fd = keymap_fd(&size);
        if (fd >= 0) {
            eis_keymap* km = eis_device_new_keymap(d, EIS_KEYMAP_TYPE_XKB, fd, size);
            eis_keymap_add(km);
            eis_keymap_unref(km);
            close(fd);
        }
        eis_device_add(d);
        eis_device_resume(d);
        devices.push_back(d);
    }
}

void Connection::handle(eis_event* event) {
    switch (eis_event_get_type(event)) {
    case EIS_EVENT_CLIENT_CONNECT: {
        eis_client* client = eis_event_get_client(event);
        // Remote desktop clients send input; input capture clients receive.
        bool const right_way = mode == Mode::remote_desktop ? eis_client_is_sender(client)
                                                             : !eis_client_is_sender(client);
        if (!right_way) {
            eis_client_disconnect(client);
            break;
        }
        eis_client_connect(client);
        eis_seat* seat = eis_client_new_seat(client, "default");
        if (mode == Mode::remote_desktop) {
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_POINTER);
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_POINTER_ABSOLUTE);
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_KEYBOARD);
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_TOUCH);
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_BUTTON);
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_SCROLL);
        } else {
            if (capabilities & UMOJA_CAPTURE_POINTER) {
                eis_seat_configure_capability(seat, EIS_DEVICE_CAP_POINTER);
                eis_seat_configure_capability(seat, EIS_DEVICE_CAP_BUTTON);
                eis_seat_configure_capability(seat, EIS_DEVICE_CAP_SCROLL);
            }
            if (capabilities & UMOJA_CAPTURE_KEYBOARD) eis_seat_configure_capability(seat, EIS_DEVICE_CAP_KEYBOARD);
        }
        eis_seat_add(seat);
        eis_seat_unref(seat);
        break;
    }
    case EIS_EVENT_CLIENT_DISCONNECT:
        eis_client_disconnect(eis_event_get_client(event));
        if (mode == Mode::remote_desktop && remote) umoja_remote_input_release(remote);
        if (mode == Mode::input_capture) capture_client_gone(this);
        break;
    case EIS_EVENT_SEAT_BIND:
        add_devices(eis_event_get_seat(event), event);
        break;
    case EIS_EVENT_DEVICE_CLOSED: {
        eis_device* d = eis_event_get_device(event);
        for (auto it = devices.begin(); it != devices.end(); ++it)
            if (*it == d) {
                if (capture_device == d) capture_device = nullptr;
                eis_device_remove(d);
                eis_device_unref(d);
                devices.erase(it);
                break;
            }
        break;
    }
    default:
        break;
    }
    if (mode != Mode::remote_desktop || !remote) return;
    switch (eis_event_get_type(event)) {
    case EIS_EVENT_POINTER_MOTION:
        umoja_remote_pointer_motion(remote, eis_event_pointer_get_dx(event), eis_event_pointer_get_dy(event));
        break;
    case EIS_EVENT_POINTER_MOTION_ABSOLUTE:
        umoja_remote_pointer_absolute(remote, eis_event_pointer_get_absolute_x(event),
                                      eis_event_pointer_get_absolute_y(event));
        break;
    case EIS_EVENT_BUTTON_BUTTON:
        umoja_remote_pointer_button(remote, eis_event_button_get_button(event),
                                    eis_event_button_get_is_press(event));
        break;
    case EIS_EVENT_SCROLL_DELTA:
        umoja_remote_pointer_scroll(remote, eis_event_scroll_get_dx(event), eis_event_scroll_get_dy(event), 0, 0,
                                    false);
        break;
    case EIS_EVENT_SCROLL_STOP:
    case EIS_EVENT_SCROLL_CANCEL:
        umoja_remote_pointer_scroll(remote, 0, 0, 0, 0, true);
        break;
    case EIS_EVENT_SCROLL_DISCRETE:
        // In 120ths of a wheel click.
        umoja_remote_pointer_scroll(remote, 0, 0, eis_event_scroll_get_discrete_dx(event) / 120,
                                    eis_event_scroll_get_discrete_dy(event) / 120, false);
        break;
    case EIS_EVENT_KEYBOARD_KEY:
        umoja_remote_key(remote, eis_event_keyboard_get_key(event), eis_event_keyboard_get_key_is_press(event));
        break;
    case EIS_EVENT_TOUCH_DOWN:
        umoja_remote_touch(remote, UMOJA_TOUCH_DOWN, eis_event_touch_get_id(event), eis_event_touch_get_x(event),
                           eis_event_touch_get_y(event));
        break;
    case EIS_EVENT_TOUCH_MOTION:
        umoja_remote_touch(remote, UMOJA_TOUCH_MOTION, eis_event_touch_get_id(event), eis_event_touch_get_x(event),
                           eis_event_touch_get_y(event));
        break;
    case EIS_EVENT_TOUCH_UP:
        umoja_remote_touch(remote, UMOJA_TOUCH_UP, eis_event_touch_get_id(event), 0, 0);
        break;
    default:
        break;
    }
}

void Connection::run() {
    pollfd fds[2] = {{eis_get_fd(ctx), POLLIN, 0}, {stop_fd, POLLIN, 0}};
    while (true) {
        if (poll(fds, 2, -1) < 0) {
            if (errno == EINTR) continue;
            break;
        }
        if (fds[1].revents) break;
        std::lock_guard lock{mutex};
        eis_dispatch(ctx);
        while (eis_event* e = eis_get_event(ctx)) {
            handle(e);
            eis_event_unref(e);
        }
    }
}

std::mutex g_mutex;
std::map<uint64_t, std::shared_ptr<Connection>> g_connections;
uint64_t g_next = 1;

std::shared_ptr<Connection> find(uint64_t handle) {
    std::lock_guard lock{g_mutex};
    auto const it = g_connections.find(handle);
    return it == g_connections.end() ? nullptr : it->second;
}

uint64_t open(std::shared_ptr<Connection> c, int* out_fd) {
    c->ctx = eis_new(nullptr);
    if (!c->ctx || eis_setup_backend_fd(c->ctx) != 0) return 0;
    int const fd = eis_backend_fd_add_client(c->ctx);
    if (fd < 0) return 0;
    c->stop_fd = eventfd(0, EFD_CLOEXEC);
    if (c->stop_fd < 0) {
        close(fd);
        return 0;
    }
    c->thread = std::thread{[raw = c.get()] { raw->run(); }};
    std::lock_guard lock{g_mutex};
    uint64_t const handle = g_next++;
    g_connections.emplace(handle, std::move(c));
    *out_fd = fd;
    return handle;
}

// ---- Input capture ----------------------------------------------------------

std::mutex g_capture_mutex;
bool g_capture_enabled = false;
// Barriers are set: the only state in which pointer motion needs a closer
// look. Lets the event filter pass every other motion event without
// taking g_capture_mutex.
std::atomic<bool> g_capture_armed{false};
std::atomic<uint64_t> g_capture_eis{0};
std::vector<UmojaBarrier> g_barriers;
std::atomic<bool> g_captured{false};
std::atomic<uint32_t> g_activation{0};
uint32_t g_next_activation = 1;

// Mir pointer button bit -> evdev code.
uint32_t evdev_button(MirPointerButton b) {
    switch (b) {
    case mir_pointer_button_primary: return BTN_LEFT;
    case mir_pointer_button_secondary: return BTN_RIGHT;
    case mir_pointer_button_tertiary: return BTN_MIDDLE;
    case mir_pointer_button_back: return BTN_BACK;
    case mir_pointer_button_forward: return BTN_FORWARD;
    case mir_pointer_button_side: return BTN_SIDE;
    case mir_pointer_button_extra: return BTN_EXTRA;
    case mir_pointer_button_task: return BTN_TASK;
    default: return 0;
    }
}

MirPointerButtons g_last_buttons = 0;

// The client that input is captured for disconnected without releasing
// it: give the keyboard and pointer back, or they stay swallowed until
// Super+Escape. Called on the connection's thread with its mutex held, so
// this must not take g_capture_mutex (the event filter takes that first
// and the connection's mutex second).
void capture_client_gone(Connection* c) {
    if (find(g_capture_eis.load()).get() != c) return;
    if (!g_captured.exchange(false)) return;
    c->emulating = false;
    umoja_input_capture_deactivated(g_activation.load());
}

void with_capture_device(std::function<void(eis_device*)> const& f) {
    auto const c = find(g_capture_eis);
    if (!c) return;
    std::lock_guard lock{c->mutex};
    if (c->capture_device && c->emulating) f(c->capture_device);
}

void start_emulating(uint32_t activation) {
    auto const c = find(g_capture_eis);
    if (!c) return;
    std::lock_guard lock{c->mutex};
    if (!c->capture_device) return;
    eis_device_resume(c->capture_device);
    eis_device_start_emulating(c->capture_device, activation);
    c->emulating = true;
}

void stop_emulating() {
    auto const c = find(g_capture_eis);
    if (!c) return;
    std::lock_guard lock{c->mutex};
    if (!c->capture_device || !c->emulating) return;
    eis_device_stop_emulating(c->capture_device);
    eis_device_pause(c->capture_device);
    c->emulating = false;
}

// Does pointer position (x, y), moving by (dx, dy), push through a barrier?
// Barriers lie on the outer edges of monitors; the pointer is held there.
bool hits(UmojaBarrier const& b, float x, float y, float dx, float dy) {
    if (b.x1 == b.x2) {  // vertical
        int const lo = std::min(b.y1, b.y2), hi = std::max(b.y1, b.y2);
        if (y < lo || y > hi + 1) return false;
        if (dx < 0) return x <= b.x1 + 0.5f;           // left edge at x1
        if (dx > 0) return x >= b.x1 - 1.5f;           // right edge, x1 = width (or width - 1)
        return false;
    }
    if (b.y1 == b.y2) {  // horizontal
        int const lo = std::min(b.x1, b.x2), hi = std::max(b.x1, b.x2);
        if (x < lo || x > hi + 1) return false;
        if (dy < 0) return y <= b.y1 + 0.5f;
        if (dy > 0) return y >= b.y1 - 1.5f;
        return false;
    }
    return false;
}

}  // namespace

extern "C" uint64_t umoja_eis_connect_remote(uint64_t remote, UmojaRegion const* regions, size_t n, int* out_fd) {
    if (!out_fd) return 0;
    try {
        auto c = std::make_shared<Connection>();
        c->mode = Mode::remote_desktop;
        c->remote = remote;
        c->regions.assign(regions, regions + n);
        return open(std::move(c), out_fd);
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: EIS connection failed: %s\n", e.what());
        return 0;
    }
}

extern "C" uint64_t umoja_eis_connect_capture(uint32_t capabilities, int* out_fd) {
    if (!out_fd) return 0;
    try {
        auto c = std::make_shared<Connection>();
        c->mode = Mode::input_capture;
        c->capabilities = capabilities;
        return open(std::move(c), out_fd);
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: EIS connection failed: %s\n", e.what());
        return 0;
    }
}

extern "C" void umoja_eis_disconnect(uint64_t handle) {
    std::shared_ptr<Connection> c;
    {
        std::lock_guard lock{g_mutex};
        auto const it = g_connections.find(handle);
        if (it == g_connections.end()) return;
        c = std::move(it->second);
        g_connections.erase(it);
    }
    // Joins its thread; never from that thread.
    c.reset();
}

extern "C" void umoja_input_capture_enable(uint64_t eis, UmojaBarrier const* barriers, size_t n) {
    std::lock_guard lock{g_capture_mutex};
    g_capture_enabled = true;
    g_capture_eis = eis;
    g_barriers.assign(barriers, barriers + n);
    g_capture_armed.store(n > 0, std::memory_order_release);
}

// Stop capturing (and stop offering barriers). Returns the activation that
// ended, or 0 if input wasn't captured.
extern "C" uint32_t umoja_input_capture_disable(void) {
    std::lock_guard lock{g_capture_mutex};
    g_capture_enabled = false;
    g_capture_armed.store(false, std::memory_order_release);
    g_barriers.clear();
    uint32_t ended = 0;
    if (g_captured.exchange(false)) {
        stop_emulating();
        ended = g_activation;
    }
    g_capture_eis = 0;
    return ended;
}

// The client is done: local input works again, the pointer going to
// (x, y) when `warp`. Returns the activation that ended, or 0.
extern "C" uint32_t umoja_input_capture_release(bool warp, double x, double y) {
    uint32_t ended = 0;
    {
        std::lock_guard lock{g_capture_mutex};
        if (g_captured.exchange(false)) {
            stop_emulating();
            ended = g_activation;
        }
    }
    if (ended && warp) umoja_wm_move_pointer((int32_t)std::lround(x), (int32_t)std::lround(y));
    return ended;
}

// The first event filter (umoja_shim.cpp): true consumes the event.
extern "C" bool umoja_input_capture_filter(MirEvent const* event) {
    if (mir_event_get_type(event) != mir_event_type_input) return false;
    if (!g_captured.load(std::memory_order_acquire)) {
        // Only pointer motion can start a capture.
        auto const* input = mir_event_get_input_event(event);
        if (mir_input_event_get_type(input) != mir_input_event_type_pointer) return false;
        auto const* p = mir_input_event_get_pointer_event(input);
        if (mir_pointer_event_action(p) != mir_pointer_action_motion) return false;
        if (!g_capture_armed.load(std::memory_order_acquire)) return false;
        std::unique_lock lock{g_capture_mutex};
        if (!g_capture_enabled || g_barriers.empty()) return false;
        float const x = mir_pointer_event_axis_value(p, mir_pointer_axis_x);
        float const y = mir_pointer_event_axis_value(p, mir_pointer_axis_y);
        float const dx = mir_pointer_event_axis_value(p, mir_pointer_axis_relative_x);
        float const dy = mir_pointer_event_axis_value(p, mir_pointer_axis_relative_y);
        for (auto const& b : g_barriers) {
            if (!hits(b, x, y, dx, dy)) continue;
            g_activation = g_next_activation++;
            g_last_buttons = mir_pointer_event_buttons(p);
            g_captured.store(true, std::memory_order_release);
            uint32_t const barrier = b.id, activation = g_activation;
            start_emulating(activation);
            lock.unlock();
            umoja_input_capture_activated(barrier, activation, x, y);
            return true;
        }
        return false;
    }

    auto const* input = mir_event_get_input_event(event);
    switch (mir_input_event_get_type(input)) {
    case mir_input_event_type_key: {
        auto const* k = mir_input_event_get_keyboard_event(input);
        auto const action = mir_keyboard_event_action(k);
        if (action == mir_keyboard_action_modifiers) return true;
        bool const down = action != mir_keyboard_action_up;
        // Super+Escape always gets the keyboard and pointer back.
        if (down && mir_keyboard_event_keysym(k) == XKB_KEY_Escape &&
            (mir_keyboard_event_modifiers(k) & mir_input_event_modifier_meta)) {
            uint32_t const ended = umoja_input_capture_release(false, 0, 0);
            if (ended) umoja_input_capture_deactivated(ended);
            return true;
        }
        if (action == mir_keyboard_action_repeat) return true;  // the client repeats itself
        int const code = mir_keyboard_event_scan_code(k);
        with_capture_device([&](eis_device* d) {
            eis_device_keyboard_key(d, (uint32_t)code, down);
            eis_device_frame(d, eis_now(eis_device_get_context(d)));
        });
        return true;
    }
    case mir_input_event_type_pointer: {
        auto const* p = mir_input_event_get_pointer_event(input);
        float const dx = mir_pointer_event_axis_value(p, mir_pointer_axis_relative_x);
        float const dy = mir_pointer_event_axis_value(p, mir_pointer_axis_relative_y);
        float const sx = mir_pointer_event_axis_value(p, mir_pointer_axis_hscroll);
        float const sy = mir_pointer_event_axis_value(p, mir_pointer_axis_vscroll);
        MirPointerButtons const buttons = mir_pointer_event_buttons(p);
        with_capture_device([&](eis_device* d) {
            if (dx != 0 || dy != 0) eis_device_pointer_motion(d, dx, dy);
            MirPointerButtons const changed = buttons ^ g_last_buttons;
            for (int bit = 0; bit < 8; bit++) {
                auto const b = static_cast<MirPointerButton>(1 << bit);
                if (changed & b)
                    if (uint32_t const code = evdev_button(b)) eis_device_button_button(d, code, buttons & b);
            }
            if (sx != 0 || sy != 0) eis_device_scroll_delta(d, sx, sy);
            eis_device_frame(d, eis_now(eis_device_get_context(d)));
        });
        g_last_buttons = buttons;
        return true;
    }
    default:
        return true;  // touch, tablets: not for the apps here either
    }
}

// Remote desktop input: virtual keyboard, pointer and touchscreen devices
// for org.gnome.Mutter.RemoteDesktop sessions and their libei clients
// (src/remote_desktop.rs). What a remote desktop tool (or a screen sharing
// app's "remote control") sends arrives here as input from a device of its
// own, like Mir's wlr virtual pointer and keyboard do.
//
// Each session's devices are its own: when a session ends, whatever it still
// holds down (keys, buttons, touches) is released, so a dropped connection
// can't leave Ctrl pressed.

#include "umoja_shim.h"

#include <mir/events/scroll_axis.h>
#include <mir/events/touch_contact.h>
#include <mir/executor.h>
#include <mir/input/device.h>
#include <mir/input/event_builder.h>
#include <mir/input/input_device_registry.h>
#include <mir/input/input_sink.h>
#include <mir/input/virtual_input_device.h>
#include <mir/server.h>
#include <mir_toolkit/events/enums.h>

#include <linux/input-event-codes.h>
#include <xkbcommon/xkbcommon.h>

#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <map>
#include <memory>
#include <mutex>
#include <set>

namespace geom = mir::geometry;
namespace mi = mir::input;
namespace mev = mir::events;

namespace {

mir::Server* g_server = nullptr;

auto now() -> std::chrono::nanoseconds {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch());
}

std::optional<MirPointerButtons> mir_button(uint32_t linux_button) {
    switch (linux_button) {
    case BTN_LEFT: return mir_pointer_button_primary;
    case BTN_RIGHT: return mir_pointer_button_secondary;
    case BTN_MIDDLE: return mir_pointer_button_tertiary;
    case BTN_BACK: return mir_pointer_button_back;
    case BTN_FORWARD: return mir_pointer_button_forward;
    case BTN_SIDE: return mir_pointer_button_side;
    case BTN_TASK: return mir_pointer_button_task;
    case BTN_EXTRA: return mir_pointer_button_extra;
    default: return std::nullopt;
    }
}

class Remote {
public:
    Remote()
        : keyboard{std::make_shared<mi::VirtualInputDevice>("umoja-remote-keyboard", mi::DeviceCapability::keyboard)},
          pointer{std::make_shared<mi::VirtualInputDevice>("umoja-remote-pointer", mi::DeviceCapability::pointer)},
          touch{std::make_shared<mi::VirtualInputDevice>("umoja-remote-touch", mi::DeviceCapability::touchscreen |
                                                                                 mi::DeviceCapability::multitouch)} {
        auto const registry = g_server->the_input_device_registry();
        registry->add_device(keyboard);
        registry->add_device(pointer);
        registry->add_device(touch);
    }

    ~Remote() {
        release_all();
        // After the releases, on the same executor, so nothing races them.
        mir::linearising_executor.spawn([keyboard = keyboard, pointer = pointer, touch = touch,
                                         registry = g_server->the_input_device_registry()] {
            registry->remove_device(keyboard);
            registry->remove_device(pointer);
            registry->remove_device(touch);
        });
    }

    void key(uint32_t code, bool pressed) {
        {
            std::lock_guard lock{mutex};
            if (pressed ? !keys.insert(code).second : keys.erase(code) == 0) return;
        }
        send_key(code, pressed);
    }

    void motion(double dx, double dy) {
        MirPointerButtons const buttons = current_buttons();
        mir::linearising_executor.spawn([pointer = pointer, buttons, dx, dy] {
            pointer->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->pointer_event(now(), mir_pointer_action_motion, buttons, std::nullopt,
                                                          geom::DisplacementF{dx, dy}, mir_pointer_axis_source_none,
                                                          {}, {}));
            });
        });
    }

    void absolute(double x, double y) {
        MirPointerButtons const buttons = current_buttons();
        mir::linearising_executor.spawn([pointer = pointer, buttons, x, y] {
            pointer->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->pointer_event(now(), mir_pointer_action_motion, buttons,
                                                          geom::PointF{x, y}, geom::DisplacementF{},
                                                          mir_pointer_axis_source_none, {}, {}));
            });
        });
    }

    void button(uint32_t linux_button, bool pressed) {
        auto const b = mir_button(linux_button);
        if (!b) return;
        MirPointerButtons after;
        {
            std::lock_guard lock{mutex};
            bool const held = buttons & *b;
            if (held == pressed) return;
            buttons = pressed ? (buttons | *b) : (buttons & ~*b);
            after = buttons;
        }
        auto const action = pressed ? mir_pointer_action_button_down : mir_pointer_action_button_up;
        mir::linearising_executor.spawn([pointer = pointer, action, after] {
            pointer->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->pointer_event(now(), action, after, std::nullopt, geom::DisplacementF{},
                                                          mir_pointer_axis_source_none, {}, {}));
            });
        });
    }

    // Smooth scrolling (dx, dy in pixels-ish, like libinput's finger
    // scrolling) or wheel clicks (discrete steps of 15 "pixels").
    void scroll(double dx, double dy, int32_t steps_x, int32_t steps_y, bool finish) {
        mev::ScrollAxisH h;
        mev::ScrollAxisV v;
        MirPointerAxisSource source = mir_pointer_axis_source_finger;
        if (steps_x || steps_y) {
            source = mir_pointer_axis_source_wheel;
            h.discrete = geom::DeltaX{steps_x};
            h.value120 = geom::DeltaX{steps_x * 120};
            h.precise = geom::DeltaXF{steps_x * 15.0f};
            v.discrete = geom::DeltaY{steps_y};
            v.value120 = geom::DeltaY{steps_y * 120};
            v.precise = geom::DeltaYF{steps_y * 15.0f};
        } else {
            h.precise = geom::DeltaXF{(float)dx};
            v.precise = geom::DeltaYF{(float)dy};
            h.stop = finish && dx == 0;
            v.stop = finish && dy == 0;
        }
        MirPointerButtons const buttons = current_buttons();
        mir::linearising_executor.spawn([pointer = pointer, buttons, source, h, v] {
            pointer->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->pointer_event(now(), mir_pointer_action_motion, buttons, std::nullopt,
                                                          geom::DisplacementF{}, source, h, v));
            });
        });
    }

    void touch_event(uint32_t action, uint32_t slot, double x, double y) {
        std::vector<mev::TouchContact> contacts;
        {
            std::lock_guard lock{mutex};
            MirTouchAction mir_action;
            switch (action) {
            case UMOJA_TOUCH_DOWN:
                if (touches.count(slot)) return;
                touches[slot] = geom::PointF{x, y};
                mir_action = mir_touch_action_down;
                break;
            case UMOJA_TOUCH_MOTION:
                if (!touches.count(slot)) return;
                touches[slot] = geom::PointF{x, y};
                mir_action = mir_touch_action_change;
                break;
            default:
                if (!touches.count(slot)) return;
                mir_action = mir_touch_action_up;
                break;
            }
            for (auto const& [id, pos] : touches) {
                MirTouchAction const a = id == slot ? mir_action : mir_touch_action_change;
                contacts.emplace_back((MirTouchId)id, a, mir_touch_tooltype_finger, pos, 1.0f, 0.0f, 0.0f, 0.0f);
            }
            if (mir_action == mir_touch_action_up) touches.erase(slot);
        }
        send_touch(std::move(contacts));
    }

    // Let go of everything still held.
    void release_all() {
        std::set<uint32_t> held_keys;
        MirPointerButtons held_buttons;
        std::vector<mev::TouchContact> lifted;
        {
            std::lock_guard lock{mutex};
            held_keys.swap(keys);
            held_buttons = buttons;
            buttons = 0;
            for (auto const& [id, pos] : touches)
                lifted.emplace_back((MirTouchId)id, mir_touch_action_up, mir_touch_tooltype_finger, pos, 0.0f, 0.0f,
                                    0.0f, 0.0f);
            touches.clear();
        }
        for (uint32_t code : held_keys) send_key(code, false);
        if (held_buttons) {
            mir::linearising_executor.spawn([pointer = pointer] {
                pointer->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                    sink->handle_input(builder->pointer_event(now(), mir_pointer_action_button_up, 0, std::nullopt,
                                                              geom::DisplacementF{}, mir_pointer_axis_source_none,
                                                              {}, {}));
                });
            });
        }
        // One up event per lifted contact.
        for (auto const& contact : lifted) send_touch({contact});
    }

private:
    std::shared_ptr<mi::VirtualInputDevice> const keyboard, pointer, touch;
    std::mutex mutex;
    std::set<uint32_t> keys;
    MirPointerButtons buttons = 0;
    std::map<uint32_t, geom::PointF> touches;

    MirPointerButtons current_buttons() {
        std::lock_guard lock{mutex};
        return buttons;
    }

    void send_key(uint32_t code, bool pressed) {
        auto const action = pressed ? mir_keyboard_action_down : mir_keyboard_action_up;
        mir::linearising_executor.spawn([keyboard = keyboard, action, code] {
            keyboard->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->key_event(now(), action, 0, (int)code));
            });
        });
    }

    void send_touch(std::vector<mev::TouchContact> contacts) {
        mir::linearising_executor.spawn([touch = touch, contacts = std::move(contacts)] {
            touch->if_started_then([&](mi::InputSink* sink, mi::EventBuilder* builder) {
                sink->handle_input(builder->touch_event(now(), contacts));
            });
        });
    }
};

std::mutex g_remotes_mutex;
std::map<uint64_t, std::shared_ptr<Remote>> g_remotes;
uint64_t g_next = 1;

std::shared_ptr<Remote> remote(uint64_t handle) {
    std::lock_guard lock{g_remotes_mutex};
    auto const it = g_remotes.find(handle);
    return it == g_remotes.end() ? nullptr : it->second;
}

template <typename F>
void with_remote(uint64_t handle, char const* what, F&& f) {
    try {
        if (auto const r = remote(handle)) f(*r);
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: remote input (%s) failed: %s\n", what, e.what());
    }
}

// ---- Keysyms ----------------------------------------------------------------

// Where a keysym is on the keyboard: the evdev key and the level (0 plain,
// 1 Shift, 2 AltGr, 3 Shift+AltGr) in the first layout of the keymap Mir
// uses (its default, or $MIR_SERVER_KEYMAP "layout+variant").
struct KeyPosition {
    uint32_t code;
    uint32_t level;
};

std::mutex g_keymap_mutex;
std::map<uint32_t, KeyPosition> g_keysyms;
std::string g_keymap_string;
bool g_keymap_loaded = false;

void load_keymap() {
    g_keymap_loaded = true;
    xkb_context* const ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (!ctx) return;
    std::string layout, variant;
    if (char const* env = std::getenv("MIR_SERVER_KEYMAP")) {
        std::string const s{env};
        auto const plus = s.find('+');
        layout = s.substr(0, plus);
        if (plus != std::string::npos) variant = s.substr(plus + 1);
    }
    xkb_rule_names names{};
    names.layout = layout.empty() ? nullptr : layout.c_str();
    names.variant = variant.empty() ? nullptr : variant.c_str();
    xkb_keymap* const keymap = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (keymap) {
        if (char* str = xkb_keymap_get_as_string(keymap, XKB_KEYMAP_FORMAT_TEXT_V1)) {
            g_keymap_string = str;
            std::free(str);
        }
        xkb_keycode_t const min = xkb_keymap_min_keycode(keymap), max = xkb_keymap_max_keycode(keymap);
        for (xkb_keycode_t kc = min; kc <= max; kc++) {
            xkb_level_index_t const levels = xkb_keymap_num_levels_for_key(keymap, kc, 0);
            for (xkb_level_index_t level = 0; level < levels && level < 4; level++) {
                xkb_keysym_t const* syms;
                int const n = xkb_keymap_key_get_syms_by_level(keymap, kc, 0, level, &syms);
                for (int i = 0; i < n; i++) {
                    auto const it = g_keysyms.find(syms[i]);
                    // Prefer the plainest way to type it.
                    if (it == g_keysyms.end() || it->second.level > level)
                        g_keysyms[syms[i]] = KeyPosition{kc - 8, level};
                }
            }
        }
        xkb_keymap_unref(keymap);
    }
    xkb_context_unref(ctx);
}

std::optional<KeyPosition> key_for(uint32_t keysym) {
    std::lock_guard lock{g_keymap_mutex};
    if (!g_keymap_loaded) load_keymap();
    auto const it = g_keysyms.find(keysym);
    if (it == g_keysyms.end()) return std::nullopt;
    return it->second;
}

}  // namespace

std::string umoja_remote_keymap_string() {
    std::lock_guard lock{g_keymap_mutex};
    if (!g_keymap_loaded) load_keymap();
    return g_keymap_string;
}

void umoja_remote_input_setup(mir::Server& server) {
    server.add_init_callback([&server] { g_server = &server; });
}

extern "C" uint64_t umoja_remote_input_new(void) {
    if (!g_server) return 0;
    try {
        auto r = std::make_shared<Remote>();
        std::lock_guard lock{g_remotes_mutex};
        uint64_t const handle = g_next++;
        g_remotes.emplace(handle, std::move(r));
        return handle;
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: remote input devices failed: %s\n", e.what());
        return 0;
    }
}

extern "C" void umoja_remote_input_free(uint64_t handle) {
    std::shared_ptr<Remote> r;
    {
        std::lock_guard lock{g_remotes_mutex};
        auto const it = g_remotes.find(handle);
        if (it == g_remotes.end()) return;
        r = std::move(it->second);
        g_remotes.erase(it);
    }
    try {
        r.reset();
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: remote input teardown failed: %s\n", e.what());
    }
}

extern "C" void umoja_remote_input_release(uint64_t handle) {
    with_remote(handle, "release", [&](Remote& r) { r.release_all(); });
}

extern "C" void umoja_remote_key(uint64_t handle, uint32_t evdev_code, bool pressed) {
    with_remote(handle, "key", [&](Remote& r) { r.key(evdev_code, pressed); });
}

extern "C" bool umoja_remote_keysym(uint64_t handle, uint32_t keysym, bool pressed) {
    auto const pos = key_for(keysym);
    if (!pos) return false;
    with_remote(handle, "keysym", [&](Remote& r) {
        // The level's modifiers go down before the key and up after it.
        uint32_t const shift = pos->level & 1 ? KEY_LEFTSHIFT : 0;
        uint32_t const altgr = pos->level & 2 ? KEY_RIGHTALT : 0;
        if (pressed) {
            if (shift) r.key(shift, true);
            if (altgr) r.key(altgr, true);
            r.key(pos->code, true);
        } else {
            r.key(pos->code, false);
            if (altgr) r.key(altgr, false);
            if (shift) r.key(shift, false);
        }
    });
    return true;
}

extern "C" void umoja_remote_pointer_motion(uint64_t handle, double dx, double dy) {
    with_remote(handle, "motion", [&](Remote& r) { r.motion(dx, dy); });
}

extern "C" void umoja_remote_pointer_absolute(uint64_t handle, double x, double y) {
    with_remote(handle, "absolute motion", [&](Remote& r) { r.absolute(x, y); });
}

extern "C" void umoja_remote_pointer_button(uint64_t handle, uint32_t linux_button, bool pressed) {
    with_remote(handle, "button", [&](Remote& r) { r.button(linux_button, pressed); });
}

extern "C" void umoja_remote_pointer_scroll(uint64_t handle, double dx, double dy, int32_t steps_x, int32_t steps_y,
                                            bool finish) {
    with_remote(handle, "scroll", [&](Remote& r) { r.scroll(dx, dy, steps_x, steps_y, finish); });
}

extern "C" void umoja_remote_touch(uint64_t handle, uint32_t action, uint32_t slot, double x, double y) {
    with_remote(handle, "touch", [&](Remote& r) { r.touch_event(action, slot, x, y); });
}

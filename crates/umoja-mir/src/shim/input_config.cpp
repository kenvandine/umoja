// Touchpad and mouse settings from GNOME, applied through libinput.
//
// Mir's libinput backend leaves every device at libinput's defaults
// (tap-to-click off, for one) unless told otherwise, and MirAL's
// InputConfiguration is the way to tell it: it applies to the devices
// present now and, through its device observer, to ones plugged in later.
// The Rust side (`peripherals.rs`) reads org.gnome.desktop.peripherals.*
// and calls umoja_input_apply() at start-up and whenever Settings changes.
//
// Kept in its own file so mir::Server doesn't mix with the MirAL toolkit
// API that umoja_shim.cpp imports with `using namespace`.

#include "umoja_shim.h"

#include <miral/input_configuration.h>
#include <mir/input/device.h>
#include <mir/input/input_device_hub.h>
#include <mir/input/input_device_observer.h>
#include <mir/input/mir_pointer_config.h>
#include <mir/server.h>

#include <memory>
#include <mutex>
#include <optional>
#include <utility>

namespace {

std::mutex g_mutex;
miral::InputConfiguration g_config;
std::optional<UmojaInputSettings> g_pending;
bool g_ready = false;

// The touchpad's own pointer settings, for TouchpadObserver. Its own lock:
// the observer runs under the input hub's lock, which apply() takes while
// holding g_mutex.
std::mutex g_touchpad_mutex;
std::optional<std::pair<MirPointerAcceleration, double>> g_touchpad_pointer;

MirPointerAcceleration acceleration(int32_t profile) {
    return profile == UMOJA_ACCEL_FLAT ? mir_pointer_acceleration_none : mir_pointer_acceleration_adaptive;
}

// Called with g_mutex held, once the server's input hub exists.
void apply(UmojaInputSettings const& s) {
    // Mouse settings reach every pointer, touchpads included, so the
    // touchpad's own speed and profile go on afterwards. Handedness has no
    // touchpad override in Mir: touchpads follow the mouse, which is
    // GNOME's default ("mouse") for the touchpad's left-handed key.
    auto mouse = g_config.mouse();
    mouse.handedness(s.left_handed ? mir_pointer_handedness_left : mir_pointer_handedness_right);
    mouse.acceleration(acceleration(s.mouse_accel_profile));
    mouse.acceleration_bias(s.mouse_speed);
    g_config.mouse(mouse);

    {
        std::lock_guard lock{g_touchpad_mutex};
        g_touchpad_pointer = std::pair{acceleration(s.touchpad_accel_profile), s.touchpad_speed};
    }
    auto touchpad = g_config.touchpad();
    touchpad.tap_to_click(s.tap_to_click);
    touchpad.disable_while_typing(s.disable_while_typing);
    touchpad.disable_with_external_mouse(s.disable_with_external_mouse);
    touchpad.middle_mouse_button_emulation(s.middle_click_emulation);
    touchpad.acceleration(acceleration(s.touchpad_accel_profile));
    touchpad.acceleration_bias(s.touchpad_speed);
    switch (s.click_method) {
    case UMOJA_CLICK_NONE: touchpad.click_mode(mir_touchpad_click_mode_none); break;
    case UMOJA_CLICK_AREAS: touchpad.click_mode(mir_touchpad_click_mode_area_to_click); break;
    case UMOJA_CLICK_FINGERS: touchpad.click_mode(mir_touchpad_click_mode_finger_count); break;
    default: break;  // "default": leave the device's own method
    }
    switch (s.scroll_method) {
    case UMOJA_SCROLL_METHOD_NONE: touchpad.scroll_mode(mir_touchpad_scroll_mode_none); break;
    case UMOJA_SCROLL_METHOD_TWO_FINGER: touchpad.scroll_mode(mir_touchpad_scroll_mode_two_finger_scroll); break;
    case UMOJA_SCROLL_METHOD_EDGE: touchpad.scroll_mode(mir_touchpad_scroll_mode_edge_scroll); break;
    default: break;
    }
    g_config.touchpad(touchpad);
}

// MirAL configures a device that appears later (or comes back after
// resume) with the touchpad settings first and the mouse ones second, and
// the mouse's speed and profile reach touchpads too. Put the touchpad's
// back. Registered after MirAL's observer, so it runs after it.
struct TouchpadObserver : mir::input::InputDeviceObserver {
    void device_added(std::shared_ptr<mir::input::Device> const& device) override {
        if (!contains(device->capabilities(), mir::input::DeviceCapability::touchpad)) return;
        std::optional<std::pair<MirPointerAcceleration, double>> wanted;
        {
            std::lock_guard lock{g_touchpad_mutex};
            wanted = g_touchpad_pointer;
        }
        if (!wanted) return;
        auto const current = device->pointer_configuration();
        if (!current.is_set()) return;
        MirPointerConfig config{current.value()};
        config.acceleration(wanted->first);
        config.cursor_acceleration_bias(wanted->second);
        device->apply_pointer_configuration(config);
    }
    void device_changed(std::shared_ptr<mir::input::Device> const&) override {}
    void device_removed(std::shared_ptr<mir::input::Device> const&) override {}
    void changes_complete() override {}
};

}  // namespace

void umoja_input_setup(mir::Server& server) {
    g_config(server);
    // Runs after InputConfiguration's own init callback (registered first),
    // so the input hub is wired up by then.
    server.add_init_callback([&server] {
        server.the_input_device_hub()->add_observer(std::make_shared<TouchpadObserver>());
        std::lock_guard lock{g_mutex};
        g_ready = true;
        if (g_pending) apply(*g_pending);
    });
}

extern "C" void umoja_input_apply(UmojaInputSettings const* settings) {
    if (!settings) return;
    std::lock_guard lock{g_mutex};
    g_pending = *settings;
    if (g_ready) apply(*settings);
}

// Natural scrolling for umoja-mir.
//
// Kept in its own translation unit: <mir/events/pointer_event.h> pulls in
// Mir's global mir_toolkit C API, whose functions clash with the
// miral::toolkit overloads umoja_shim.cpp imports with `using namespace`.

#include "umoja_shim.h"

#include <mir/events/pointer_event.h>
#include <mir_toolkit/events/event.h>

namespace {
template <typename Axis>
Axis inverted(Axis axis) {
    axis.precise = decltype(axis.precise){-axis.precise.as_value()};
    axis.discrete = decltype(axis.discrete){-axis.discrete.as_value()};
    axis.value120 = decltype(axis.value120){-axis.value120.as_value()};
    return axis;
}
}  // namespace

// Natural scrolling. Mir never enables libinput's natural-scroll option,
// and a negative scroll speed only negates the precise value (not the
// wheel's discrete/value120 steps that clients prefer), so flip all three
// here, before the event reaches the window manager or any client.
bool umoja_apply_natural_scroll(MirEvent const* event) {
    if (mir_event_get_type(event) != mir_event_type_input) return false;
    auto const* input = mir_event_get_input_event(event);
    if (mir_input_event_get_type(input) != mir_input_event_type_pointer) return false;
    // The event is owned by Mir's input pipeline and was never const; this
    // filter runs first, so later consumers see the rewritten axes.
    auto* pointer = const_cast<MirPointerEvent*>(mir_input_event_get_pointer_event(input));

    uint32_t source = 0;
    switch (pointer->axis_source()) {
    case mir_pointer_axis_source_finger:
        source = UMOJA_SCROLL_TOUCHPAD;
        break;
    case mir_pointer_axis_source_wheel:
    case mir_pointer_axis_source_wheel_tilt:
    case mir_pointer_axis_source_continuous:
        source = UMOJA_SCROLL_MOUSE;
        break;
    default:
        return false;
    }
    if (!umoja_natural_scroll(source)) return false;
    pointer->set_v_scroll(inverted(pointer->v_scroll()));
    pointer->set_h_scroll(inverted(pointer->h_scroll()));
    return false;
}

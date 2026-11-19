# 23: Deliver libinput touchpad gestures to the shell

PR title:      input: expose libinput touchpad swipe/pinch/hold gestures to the shell
Umoja patch:   3001_touchpad-gesture-hook.patch
Applies to canonical/mir main (d8f3edb): no (libinput_device.cpp changed); and the dlsym hook must become a real API
Depends on:    nothing
Kind:          new event type / API. Biggest API change in the set: OPEN AN ISSUE FIRST and agree the shape.
Before submitting:
  - Our patch calls a C function exported by the server executable (mir_touchpad_gesture_hook) straight from the evdev platform thread. Upstream needs either:
      (a) a new input event type (MirTouchpadGestureEvent with kind/phase/fingers/dx/dy/scale/angle) dispatched through the normal input pipeline, so event filters and WindowManagementPolicy can see it (a big change touching mir_toolkit events, the input dispatcher and MirAL); or
      (b) a narrower MirAL hook, e.g. `miral::TouchpadGestures` with `on_swipe/on_pinch/on_hold` callbacks, delivered on the main loop.
    (a) is the "proper" Mir way; (b) is much smaller. Ask which they prefer before writing it.
  - Also consider forwarding gestures to clients via zwp_pointer_gestures_v1 (GTK and Firefox use it for pinch zoom / swipe back). That is a separate PR on top of (a).

---------------------------------------- PR description ----------------------------------------

Related: #??? (issue: "Touchpad gestures are dropped")

## What's new?

Mir's evdev platform drops libinput's gesture events (`LIBINPUT_EVENT_GESTURE_SWIPE_*`, `PINCH_*`, `HOLD_*`), so a Mir shell can't offer the three/four-finger swipes users expect (overview, workspace switching), and clients can't get pinch-to-zoom.

This PR passes them on:

- the libinput device translates swipe, pinch and hold begin/update/end (and cancel) into a new touchpad gesture event carrying finger count, unaccelerated dx/dy, scale and rotation delta, and the libinput timestamp;
- the event goes through the normal input pipeline, so event filters and the window management policy can consume it (`WindowManagementPolicy::handle_touchpad_gesture_event`, defaulting to "not handled").

With no consumer, gestures are dropped as before.

## How to test

1. A test shell whose policy logs gesture events.
2. Three-finger swipe on a touchpad: begin, a run of updates with dx/dy, end. Pinch: scale and angle. Lift mid-gesture: cancelled.
3. With the default policies nothing changes.

## Checklist

- [ ] Tests added and pass
- [ ] Adequate documentation added

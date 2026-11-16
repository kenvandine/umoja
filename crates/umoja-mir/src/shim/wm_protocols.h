// Small window-management protocols Mir 2.28 does not implement:
// xdg-dialog-v1 (which dialogs are modal) and
// keyboard-shortcuts-inhibit-unstable-v1 (virtual machine and remote
// desktop viewers taking Super, Alt+Tab and the rest while focused).
#pragma once

#include <miral/wayland_extensions.h>
#include <miral/window.h>

namespace umoja_wmp {

auto dialog_extension() -> miral::WaylandExtensions::Builder;
auto shortcuts_inhibit_extension() -> miral::WaylandExtensions::Builder;
// xwayland-keyboard-grab-unstable-v1, for Xwayland only: an X11 app's
// keyboard grab works like a shortcuts inhibitor, for the apps GNOME's
// xwayland-grab-access-rules allow.
auto xwayland_grab_extension() -> miral::WaylandExtensions::Builder;

// A window was created: match it to dialogs and inhibitors that were set
// up before it mapped (GTK marks a dialog modal before its first commit).
void window_created();

// `window` is a dialog its app marked modal.
bool is_modal(miral::Window const& window);

// `window` holds an active shortcuts inhibitor: its keys go to it, not to
// the shell's shortcuts.
bool inhibits_shortcuts(miral::Window const& window);

// Super+Escape (mutter's restore-shortcuts): `window` loses its inhibitor
// until it asks again.
void restore_shortcuts(miral::Window const& window);

} // namespace umoja_wmp

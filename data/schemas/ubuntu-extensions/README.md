# Ubuntu extension schemas

Copied unchanged from Ubuntu's `gnome-shell-ubuntu-extensions`
(51.26.10.6ubuntu, stonking). `umoja-desktop` installs them, compiled
with Ubuntu's dock defaults and `90_umoja.gschema.override`, into
`/usr/share/umoja/gsettings/glib-2.0/schemas`. Umoja's dock, compositor and Settings > Ubuntu Desktop read the
Ubuntu Dock (`dash-to-dock`), Tiling Assistant and AppIndicators keys, so a
system without GNOME Shell, which `ubuntu-umoja-desktop` installs, still
needs these schemas and Ubuntu's dock defaults (`10_ubuntu-dock.gschema.override`).

`bin/umoja-session` appends `/usr/share/umoja/gsettings` to `XDG_DATA_DIRS`
only when `gnome-shell-ubuntu-extensions` isn't installed, so with GNOME
next to Umoja the extension's own schemas are used, as before.

To refresh, extract a newer `gnome-shell-ubuntu-extensions` deb and copy
`usr/share/glib-2.0/schemas/*` here.

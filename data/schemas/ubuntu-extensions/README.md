# Ubuntu extension schemas

Copied unchanged from Ubuntu's `gnome-shell-ubuntu-extensions`
(51.26.10.6ubuntu, stonking) and shipped by the `umoja-ubuntu-dock-settings`
package. Umoja's dock, compositor and Settings > Ubuntu Desktop read the
Ubuntu Dock (`dash-to-dock`), Tiling Assistant and AppIndicators keys, so a
system without GNOME Shell, which `ubuntu-umoja-desktop` installs, still
needs these schemas and Ubuntu's dock defaults (`10_ubuntu-dock.gschema.override`).

Where `gnome-shell-ubuntu-extensions` is installed (Umoja next to GNOME), it
provides the same files instead; the two packages conflict.

To refresh, extract a newer `gnome-shell-ubuntu-extensions` deb and copy
`usr/share/glib-2.0/schemas/*` here.

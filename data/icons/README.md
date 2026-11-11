# Icons from gnome-shell

`hicolor/scalable/{actions,status}` holds every icon in gnome-shell's
`data/icons/scalable`, unchanged, taken from gnome-shell 51.0. gnome-shell
draws these from its own resources rather than an icon theme, so Umoja keeps
its own copies to look right without GNOME Shell installed.

They are installed to `/usr/share/umoja/icons/hicolor`, and the panel, dock,
overlay and greeter add `/usr/share/umoja/icons` to GTK's icon theme search
path at startup (`umoja_common::icons::add_shell_icons`). They join the
hicolor theme, so an icon theme that has its own version of a name still
wins. Set `UMOJA_ICON_DIR=$PWD/data/icons` to use them from a checkout.

To refresh: copy `data/icons/scalable` from the matching gnome-shell tag
over `hicolor/scalable`.

License: GPL-2.0-or-later (gnome-shell), which is fine alongside Umoja's
GPL-3.0-or-later. See debian/copyright.

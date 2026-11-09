---
name: umoja
description: >
  Use when changing, configuring, troubleshooting or explaining the Umoja desktop
  on this computer (an Ubuntu desktop shell on Mir and GTK4 that behaves like
  Ubuntu's GNOME session). Covers settings, the top bar, dock, overlay, assistant,
  keyboard shortcuts, workspaces, displays, wallpaper, notifications, screenshots,
  file search, the local AI model (Lemonade), logs and restarting shell parts.
  Triggers: Umoja, umoja-panel, umoja-dock, umoja-overlay, umoja-mir, umoja-mcp,
  desktop settings, gsettings, dock, top bar, hot corner, keybinding, wallpaper,
  dark mode, night light, "my desktop", "the shell crashed".
---

# Umoja

Umoja is the desktop session on this machine. It reads the same GSettings keys
as Ubuntu's GNOME session, so almost every change is a GSettings change, not a
config file edit. It runs as separate processes, so one part can be restarted
without logging out.

## First choice: the umoja-mcp tools

If the `umoja` MCP server is connected (`umoja-mcp`), use its tools instead of
shell commands. They know the system and enforce Umoja's confirm rules:

- Settings: `list_settings`, `describe_setting`, `change_setting`,
  `reset_setting`, `open_settings`
- Apps: `find_app`, `list_apps`, `launch_app`, `install_app`, `remove_app`
  (handles snap, deb, Flatpak, AppImage and hand-made launchers)
- Desktop: windows, workspaces, dark style, power mode, Wi-Fi, Bluetooth,
  volume, brightness, media, Do Not Disturb, Night Light, wallpaper, session
  (lock, log out, suspend, restart, power off)
- Troubleshooting: read-only diagnostics reports (CPU, thermals, battery,
  network, audio, disk, failed services, journal errors)
- Files: `search_files` (semantic search of the home folder)

`umoja-mcp --list` prints every tool and its risk level. Not connected? Run
`umoja-agent-setup` once, or `claude mcp add --scope user umoja -- umoja-mcp`.

## Safety rules

- **Never edit anything under `/usr`** (`/usr/share/umoja`, `/usr/lib/umoja`,
  `/usr/share/glib-2.0/schemas`). It belongs to the `umoja-desktop` package and
  is replaced on upgrade. Reading it is fine.
- Prefer `gsettings set` (or `change_setting`) over writing dconf or config
  files. Read the current value first and tell the user the old value so the
  change can be undone; `gsettings reset SCHEMA KEY` restores the default.
- Restart only the unit you changed, by name: `systemctl --user restart
  umoja-panel`. Never run broad `systemctl --user stop` patterns, never
  `pkill` by pattern, and do not restart `umoja-mir` (the compositor) unless the
  user agrees: every open app closes.
- Privileged commands (`sudo`, `apt`, `snap`) need a terminal so the user can
  type a password. Say what will run before running it.
- Don't turn off the user's privacy choices (clipboard history, file search,
  local-only assistant) without being asked.

## Where things live

| What | Where |
| --- | --- |
| Umoja's own settings | `gsettings list-recursively org.umoja.shell` (assistant model and backend, cloud URL/model, file search, clipboard history, hotkeys, tiling corners) |
| Dock | `org.gnome.shell.extensions.dash-to-dock` (position `dock-position`, size `dash-max-icon-size`, `extend-height` for panel mode, autohide); pinned apps are `org.gnome.shell favorite-apps` |
| Look | `org.gnome.desktop.interface` (`color-scheme`, `accent-color`, `text-scaling-factor`, fonts, `clock-show-*`, `show-battery-percentage`, `enable-hot-corners`, `enable-animations`) |
| Wallpaper | `org.gnome.desktop.background picture-uri` and `picture-uri-dark` |
| Window manager | `org.gnome.desktop.wm.preferences` (`button-layout`, focus mode), `org.gnome.mutter` (`dynamic-workspaces`, `workspaces-only-on-primary`, `edge-tiling`, `center-new-windows`) |
| Shortcuts | `org.gnome.desktop.wm.keybindings`, `org.gnome.mutter.keybindings`, `org.gnome.shell.keybindings`, `org.gnome.settings-daemon.plugins.media-keys` (custom shortcuts under `custom-keybindings`) |
| Power, idle, lock | `org.gnome.settings-daemon.plugins.power`, `org.gnome.desktop.session idle-delay`, `org.gnome.desktop.screensaver` |
| Notifications | `org.gnome.desktop.notifications` (`show-banners` is Do Not Disturb) |
| Night Light | `org.gnome.settings-daemon.plugins.color` |
| Displays | Use Settings > Displays (`gnome-control-center display`); Umoja stores the layout in `~/.config/umoja-mir.display` |
| Local AI server | Lemonade on `http://127.0.0.1:13345/api/v1` (OpenAI-compatible), config `~/.config/umoja/lemonade/`, port override `~/.config/umoja/lemonade.env`, models in `~/.cache/umoja/lemonade` |
| File search index | `~/.cache/umoja/files`; `umoja-files --search "words"` |
| Agent skill and MCP setup | `umoja-agent-setup --status` |

Umoja-only defaults are applied with a `:Umoja` GSettings override (the session
sets `XDG_CURRENT_DESKTOP=Umoja:ubuntu:GNOME`), so a value the user sets always
wins and a stock GNOME session on the same machine is unaffected.

## The shell's parts

| Unit (`systemctl --user`) | What it is |
| --- | --- |
| `umoja-mir` (started by the session, not a unit) | Compositor: windows, workspaces, keybindings, displays, screen blanking. Logs: `journalctl --user -t umoja-mir` |
| `umoja-panel` | Top bar, Quick Settings, calendar, notifications, lock screen, polkit password dialog |
| `umoja-dock` | Dock |
| `umoja-overlay` | Super overlay: app grid, search, assistant, clipboard history, screenshot UI |
| `umoja-lemonade` | Local AI model server |
| `umoja-files` | File search indexer |
| `xdg-desktop-portal-umoja` | Portal backend (screenshots, settings for sandboxed apps) |

Logs for a unit: `journalctl --user -u umoja-panel -b`. The hostname may be
"umoja", so grep for the unit, not the word.

## Common asks

- "Move the dock to the bottom": `gsettings set org.gnome.shell.extensions.dash-to-dock dock-position BOTTOM`
- "Pin an app": append its `.desktop` id to `org.gnome.shell favorite-apps`.
- "Add a shortcut that runs X": add a custom keybinding under
  `org.gnome.settings-daemon.plugins.media-keys custom-keybindings`, as GNOME
  Settings does; Umoja runs those too.
- "Use a bigger assistant model": the overlay's model button, or
  `gsettings set org.umoja.shell lemonade-model '<name>'`.
- "Something crashed": check `coredumpctl list` and `/var/crash` (Apport),
  then the unit's journal. A shell part restarts itself; the compositor
  restarts in place up to 3 times in 10 minutes.

User documentation for the AI features:
https://github.com/kenvandine/umoja/blob/main/docs/ai-features.md

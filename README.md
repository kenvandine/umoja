# Umoja

An AI-native desktop shell for Ubuntu 26.10, written in Rust on top of Mir
and GTK4. It behaves like Ubuntu's GNOME session (same settings, keyboard
shortcuts, dock, Quick Settings, screenshot UI and display settings), but
every piece runs in its own process so one crash never takes the session
down. It also adds a local AI assistant (Lemonade, see
[AI features](docs/ai-features.md)) and voice dictation (Myna).

## Layout

| Path | What it is |
| --- | --- |
| `crates/umoja-mir` | Wayland compositor. A thin MirAL C++ shim (`src/shim/`) with all policy in Rust: GNOME keybindings, half/quarter tiling with group resize, dynamic workspaces, multi-monitor, `org.gnome.Mutter.DisplayConfig` for Settings › Displays, HiDPI defaults. |
| `crates/umoja-panel` | Top bar: Activities, clock/calendar, AppIndicator tray (`org.kde.StatusNotifierWatcher` with native dbusmenu menus), network (NetworkManager), volume and battery (UPower) indicators, Quick Settings with Wi-Fi/Wired tiles and a network list, the lock screen, notifications, and the PolicyKit authentication agent (the "Authentication Required" password dialog). |
| `crates/umoja-dock` | Ubuntu Dock work-alike driven by the `dash-to-dock` GSettings keys and `favorite-apps`, with running dots from `zwlr_foreign_toplevel`. |
| `crates/umoja-overlay` | App grid + search, Lemonade assistant with action chips, Myna dictation, and the GNOME-style screenshot UI. |
| `crates/umoja-greeter` | LightDM greeter over a blurred copy of the user's (or Ubuntu's default) wallpaper. |
| `crates/umoja-portal` | `xdg-desktop-portal-umoja`: Settings (GNOME keys incl. `button-layout`), Screenshot, ScreenCast (stub). |
| `crates/umoja-common` | GSettings, i18n (reuses gnome-shell/GTK/control-center translations), HiDPI rules, shared types. |
| `data/` | Session file, GSettings schema, systemd user units, portal + LightDM config. |
| `debian/` | Packaging for `umoja-desktop` and `umoja-greeter`. |

## Building

```sh
sudo apt build-dep ./          # or install the Build-Depends in debian/control
make                           # cargo build --release --workspace
make test                      # headless unit tests
make greeter-preview           # greeter UI with sample users, no LightDM needed
make nested                    # run the compositor in a window of your session
```

Packages: `make deb` (binary) or `make source-package` (what the PPA gets).
Both run `make vendor` first, which vendors crates and downloads the pinned
Lemonade build (checksums in `lemonade.sha256`), because Launchpad builds
offline.

## Embedded Lemonade

`umoja-desktop` ships its own copy of Lemonade's embeddable server in
`/usr/lib/umoja/lemonade` and runs it per user as `umoja-lemonade.service`
on **127.0.0.1:13345**, with config in `~/.config/umoja/lemonade` and models
in `~/.cache/umoja/lemonade`. It never uses Lemonade's default port 13305,
so the `lemonade-server` package can be installed alongside it. To change
the port, put `UMOJA_LEMONADE_PORT=<port>` in `~/.config/umoja/lemonade.env`.

The GPU in most laptops shares memory with the desktop, so Umoja seeds
Lemonade with limits from `/usr/share/umoja/lemonade/defaults.json`: a 16k
token context (Lemonade's auto-tune would otherwise size it to fill VRAM and
GTT), one request at a time, and one loaded model per type. llama.cpp's
Vulkan buffers go in GTT rather than the VRAM carve-out the compositor needs
(`UMOJA_LEMONADE_PREFER_HOST_MEMORY=0` in `lemonade.env` turns that off).
Anything in `~/.config/umoja/lemonade/config.json` overrides these defaults.

If the compositor crashes during a session, `umoja-session` restarts it in
place (up to 3 times in 10 minutes) instead of logging you out. Open apps
are lost, but the panel, dock, overlay and Lemonade come back.

## AI features

Press **Super** and type a question or a request: the assistant answers,
and can open apps, change settings, arrange windows, read what's on screen
(**Super+Shift+A**), work with the clipboard history (**Super+Shift+V**),
summarize and reply to notifications, troubleshoot the computer, find
files by what they're about, and find or remove apps however they were
installed. Everything runs on the embedded Lemonade by default; an
OpenAI-compatible cloud model is optional. The desktop tools come from
`umoja-mcp`, an MCP server other AI apps (Claude Code, for example) can use
too. See
[docs/ai-features.md](docs/ai-features.md) for how to use each feature,
what the assistant asks before acting, and what stays on your computer.

## Login screen

Installing `umoja-greeter` on a standard Ubuntu desktop switches the
display manager from GDM to LightDM (on first install only; it applies at
the next boot) and selects the Umoja greeter and session. Removing the
package switches back to GDM.

## Publishing

`.github/workflows/packages.yml` builds `.deb`s for amd64 and arm64 on
every PR, and uploads a signed source package to
[ppa:ken-vandine/umoja](https://launchpad.net/~ken-vandine/+archive/ubuntu/umoja)
on pushes to `main` (snapshot versions) and `v*` tags (releases). It needs
these repository secrets: `PPA_GPG_PRIVATE_KEY`, `PPA_GPG_PASSPHRASE`,
`PPA_SIGNER_NAME`, `PPA_SIGNER_EMAIL` (the key must be registered with your
Launchpad account).

## Testing on a real machine

```sh
sudo lightdm --test-mode --debug       # greeter in a window
gsettings set org.gnome.shell.extensions.dash-to-dock dock-position 'BOTTOM'
gsettings set org.gnome.desktop.interface color-scheme 'prefer-dark'
```

Then log into the Umoja session, press Super for the overlay, Print for the
screenshot UI, and try Settings › Displays to change scale.

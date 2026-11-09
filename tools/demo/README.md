# umoja-demo: a hands-free tour, captured

`umoja-demo` drives a live Umoja session through its main features and
captures screenshots and short clips, then writes a Markdown page ready to
commit.

```sh
sudo apt install ffmpeg          # optional: GIFs + MP4s (otherwise clips stay .webm)
tools/demo/umoja-demo            # from a terminal inside your Umoja session
```

Keep your hands off the keyboard and mouse once the countdown ends (a real
pointer move ends the screensaver preview early, and clicks land on whatever
is open). The full tour takes about three minutes.

## What it shows

| Scene | How it is driven | Output |
| --- | --- | --- |
| `desktop` | opens up to three apps (Files, Text Editor, Calculator...), tiles two side by side, sends the third to the next workspace | `images/desktop.png` |
| `app-grid` | `org.umoja.Overlay.Show` | `images/app-grid.png` |
| `overview` | `Overlay.ToggleOverview`, `ShiftOverview` up/down | `images/overview.png`, `videos/overview.*` |
| `workspaces` | `org.umoja.Compositor.SwitchWorkspace` there and back | `images/workspace-2.png`, `videos/workspaces.*` |
| `quick-settings` | `org.umoja.Panel.OpenMenu("quick-settings")` | `images/quick-settings.png` |
| `notifications` | three `org.freedesktop.Notifications.Notify` bubbles, then the date menu | `images/notifications.png`, `images/calendar.png`, `videos/notifications.*` |
| `shortcuts` | `Overlay.ToggleShortcuts` (Super+/) | `images/shortcuts.png` |
| `screenshot-ui` | `Overlay.ShowScreenshotUI` / `HideScreenshotUI` | `images/screenshot-ui.png` |
| `screensaver` | `umoja-screensaver` preview for 7 s | `images/screensaver.png`, `videos/screensaver.*` |
| `assistant` | warms the local Lemonade model (port 13345), then `Overlay.Ask(...)`, which always answers with the local model | `images/assistant-N.png`, `videos/assistant.*` |

Stills come from `grim` at logical size (`--hidpi` for full resolution),
clips from `wf-recorder` on the primary monitor. After the tour, ffmpeg turns
each clip into a GIF (GitHub plays those inline) and an MP4 (linked from the
GIF). The output directory gets:

- `README.md`: the whole tour, every shot and clip with a caption.
- `snippet.md`: a short gallery with paths from the repository root, to paste
  into the top-level README.
- `images/`, `videos/`.

It writes to `docs/demo/` in this checkout by default (`--out DIR` to
change). A rerun replaces only the files the previous run wrote.

## Safety

- No keyboard or mouse input is injected; everything goes over D-Bus.
- It holds an `org.freedesktop.ScreenSaver` idle inhibitor while it runs.
- Before every step it checks that the screen is not blanked
  (`ScreenSaver.GetActive`) or locked (logind `LockedHint`). If it is, the
  tour stops and only tidies up, again over D-Bus.
- At the end (or on Ctrl+C) it closes the windows it opened, the
  notifications it sent and any menu or overlay, goes back to the workspace
  you started on, and turns Do Not Disturb back on if it was on.

## Options

```text
--only SCENE...       run just these scenes (e.g. --only assistant)
--skip SCENE...       leave scenes out
--ask "QUESTION"      ask this instead of the default questions (repeatable)
--answer-wait 15      seconds to let each answer stream in
--effect rain         screensaver effect (umoja-screensaver --list-effects)
--output eDP-1        monitor to capture (default: the primary one)
--no-video            stills only
--no-apps             don't open any apps
--pace 1.5            stretch every pause (slow machine, slow model)
--dry-run             print the steps without touching the desktop
```

The screenshot UI scene needs an overlay with `HideScreenshotUI` (umoja
newer than 2026.46.3); with an older one it is skipped.

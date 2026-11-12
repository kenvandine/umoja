# umoja-demo: a hands-free tour, captured

`umoja-demo` drives a live Umoja session through its main features and
captures screenshots and short clips, then writes a Markdown page ready to
commit.

```sh
sudo apt install ffmpeg          # optional: GIFs + MP4s (otherwise clips stay .webm)
tools/demo/umoja-demo            # from a terminal inside your Umoja session
```

Before each scene it moves the pointer to the right screen edge, out of the
shots (needs the compositor's `MovePointer`; with an older build, park the
pointer in a corner yourself). It minimizes the terminal you started it from (and brings it back at the
end), and keeps every window floating, never maximized or full screen, so the
wallpaper shows. Keep your hands off the keyboard and mouse once the countdown ends (a real
pointer move ends the screensaver preview early, and clicks land on whatever
is open). The full tour takes about three minutes.

## What it shows

Ask Umoja and the assistant come first, while viewers are still watching;
the classic desktop tour follows. The notifications scene sits among them
because `ai-reply` and `ai-summary` need its bubbles. The trailer follows the
same order (without `ai-install`: `ai-btop` already shows an install).

The test hooks some scenes use (`Overlay.AnswerConfirm`, `AskRows`,
`AskActivate`, `TestAction`) only answer while the tour's
`$XDG_RUNTIME_DIR/umoja-demo` flag file exists.

| Scene | How it is driven | Output |
| --- | --- | --- |
| `ask-umoja` | `Overlay.ToggleAsk` over Text Editor (the empty card: its launcher actions, frequent picks, suggestions), then `Overlay.AskType` types "dark", "12*7+5", "sound" and a question; nothing is picked, so nothing changes | `images/ask-umoja.png`, `images/ask-switch.png`, `images/ask-sum.png`, `images/ask-settings.png`, `images/ask-question.png`, `videos/ask-umoja.*` |
| `ai-btop` | Types "btop" in Ask Umoja. btop isn't installed, so Umoja offers to install it (snap first, else the deb); the scene allows that with `Overlay.AnswerConfirm(true)`, then allows the "Run btop in a terminal" prompt and films btop running. **This one really installs btop.** Afterwards it quits that btop, closes its window and removes btop again with `sudo -n` so the next run can film it; without passwordless sudo btop stays installed and later runs skip the scene until you remove it. `AnswerConfirm` only works while the scene's `$XDG_RUNTIME_DIR/umoja-demo` flag file exists | `images/ai-btop-1.png`, `images/ai-btop-2.png`, `images/ai-btop-running.png`, `videos/ai-btop.*` |
| `ai-opencode` | The coding agent, from Ask Umoja. Needs the assistant on a cloud model (`assistant-backend` 'cloud') and the tour's own folder. If OpenCode isn't installed: "Install OpenCode", allowing its prompts; the terminal runs the snap install and `umoja-agent-setup opencode`, which gives OpenCode the assistant's cloud model (no sign-in), and the scene waits for `~/.config/opencode/opencode.json` to have the `umoja` provider. Then it opens Ask Umoja's settings and picks OpenCode as the Coding agent (`TestAction("settings-open")`, `"agent-pick:opencode"`; `ai-agent` is put back at the end), and asks "Write an app in Rust: a hello world GTK 4 window, in a new project called hello-gtk in ~/Umoja Tour/Projects. Don't compile it." without naming OpenCode, allowing the hand-off; OpenCode runs in a terminal in that folder, whose `opencode.json` lets it write files and run `cargo new` but nothing else (no builds, no downloads), so it never stops on a prompt. Ends on `src/main.rs` in Text Editor. **The first run really installs OpenCode, and it stays installed**; later runs film only the coding job unless you pass `--reinstall-agent` (removes the snap first with `sudo -n`) | `images/ai-opencode-install.png`, `images/ai-opencode-installed.png`, `images/ai-opencode-default.png`, `images/ai-opencode-agent.png`, `images/ai-opencode.png`, `videos/ai-opencode.*` |
| `ai-tiling` | "Put Files on the left half of the screen and Text Editor on the right half" (floated again after) | `images/ai-tiling.png`, `videos/ai-tiling.*` |
| `ai-setups` | tiles Files left and Text Editor right, asks "Save this window layout as my Writing setup", floats and moves both windows, then types "writing" in Ask Umoja and picks the Setup row (`Overlay.AskRows` / `AskActivate`), which tiles them back. The saved setup is removed again (`~/.config/umoja/setups.json` put back as it was) | `images/ai-setups-saved.png`, `images/ai-setups-row.png`, `images/ai-setups.png`, `videos/ai-setups.*` |
| `ai-settings` | starts from light style with Night Light off, then "Turn on Night Light and switch to dark style"; the answer's Undo button (`Overlay.TestAction("undo")`) puts the style back on camera (both put back straight after) | `images/ai-settings.png`, `images/ai-settings-undo.png`, `videos/ai-settings.*` |
| `ai-rewrite` | opens a scratch window (`tools/test/testpad.py`, titled "Note to Priya") with a hasty note, selects it, `Overlay.AskAboutScreen` (Super+Shift+A), picks "Make It More Formal", then presses Replace, which types the rewrite over the selection | `images/ai-rewrite-rows.png`, `images/ai-rewrite.png`, `images/ai-rewrite-replaced.png`, `videos/ai-rewrite.*` |
| `ai-captions` | `Overlay.ToggleCaptions`, then `Overlay.Speak` reads a short update in Umoja's read-aloud voice (Piper) and the captions follow it; Summarize (`TestAction("captions-summarize")`) hands the transcript to the assistant. Needs parec and the Piper voice; the clips have no sound, so the captions carry it | `images/ai-captions.png`, `images/ai-captions-summary.png`, `videos/ai-captions.*` |
| `ai-install` | "I need VLC" (`--app`): stops at Ask Umoja's "Allow Umoja to do this?" confirmation and cancels it; nothing is installed | `images/ai-install.png`, `videos/ai-install.*` |
| `ai-updates` | "Are there any updates for my computer?", then "Install all available updates", which stops at the Allow confirmation and is cancelled | `images/ai-updates*.png`, `videos/ai-updates*.*` |
| `ai-troubleshoot` | "How is my battery doing?" (answered from a live system report; "why is it slow" only finds the tour's own recorder) | `images/ai-troubleshoot.png`, `videos/ai-troubleshoot.*` |
| `notifications` | clears the notification list (keep it with `--keep-notifications`), then three `org.freedesktop.Notifications.Notify` bubbles, then the date menu | `images/notifications.png`, `images/calendar.png`, `videos/notifications.*` |
| `ai-reply` | "Open a reply to Priya's notification with a few drafts": opens the reply field with drafts; nothing is sent | `images/ai-reply.png`, `videos/ai-reply.*` |
| `ai-summary` | "Summarize my recent notifications..." | `images/ai-summary.png`, `videos/ai-summary.*` |
| `desktop` | opens up to three apps (Files, Text Editor, Calculator...), drags two into a floating layout, sends the third to the next workspace | `images/desktop.png` |
| `tiling` | `org.umoja.Compositor.DragWindow`: drags one window to the left edge and one to the right (halves), one to the top-right corner (quarter), then both back out | `images/tiling-halves.png`, `images/tiling-quarter.png`, `videos/tiling.*` |
| `app-grid` | `org.umoja.Overlay.Show` (apps and files only; the assistant is in Ask Umoja) | `images/app-grid.png` |
| `overview` | `Overlay.ToggleOverview`, `ShiftOverview` up/down | `images/overview.png`, `videos/overview.*` |
| `workspaces` | `org.umoja.Compositor.SwitchWorkspace` there and back | `images/workspace-2.png`, `videos/workspaces.*` |
| `quick-settings` | `org.umoja.Panel.OpenMenu("quick-settings")` | `images/quick-settings.png` |
| `shortcuts` | `Overlay.ToggleShortcuts` (Super+/) | `images/shortcuts.png` |
| `screenshot-ui` | `Overlay.ShowScreenshotUI` / `HideScreenshotUI` | `images/screenshot-ui.png` |
| `screensaver` | `umoja-screensaver` preview for 7 s | `images/screensaver.png`, `videos/screensaver.*` |

The assistant uses whatever backend you've set up (local or cloud;
`--local` forces the local model). Each question is typed into the search
box of Ask Umoja letter by letter (`Overlay.AskWith`), the tour waits for the overlay's
`AnswerFinished` signal instead of a fixed time, and an answer only counts
when every tool step succeeded and it mentions what it should; otherwise the
clip is thrown away and it asks again (`--retries`, default 3). Hiding the
card always declines a pending "Allow" confirmation, so the install and update
asks can't go through.

`--only assistant` / `--skip assistant` select `ask-umoja` and all the `ai-*` scenes.
`--ask "QUESTION"` (repeatable) replaces them with your own questions.

Stills come from `grim` at logical size (`--hidpi` for full resolution),
clips from `wf-recorder` on the primary monitor. After the tour, ffmpeg turns
each clip into a GIF (GitHub plays those inline) and an MP4 (linked from the
GIF). The output directory gets:

- `README.md`: the whole tour, every shot and clip with a caption.
- `videos/trailer.mp4`: a title card, then the best clips in story order
  (assistant tiling, drag tiling, settings, install, updates, reply,
  overview, workspaces, notifications, troubleshooting, screensaver), each
  captioned, in real time (`--no-trailer` to skip).
- `snippet.md`: a short gallery with root-relative links (`/docs/demo/...`),
  so it renders both on its own and pasted into the top-level README.
- `images/`, `videos/`.

It writes to `docs/demo/` in this checkout by default (`--out DIR` to
change). A rerun replaces only the files the previous run wrote.

## Safety

- No keyboard or mouse input is injected; everything goes over D-Bus. The
  tiling drags are done by the compositor itself (`DragWindow` runs the same
  drag-to-snap code as a title-bar drag and ignores the real pointer while it
  runs). `MovePointer` warps the pointer without sending any motion or
  clicks to apps.
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
--answer-timeout 60   give up on an answer after this long
--retries 3           asks per question until the answer checks out
--local               ask the local model instead of your configured backend
--no-typing           put the question in at once instead of typing it
--app VLC             the app for "I need ..." (never installed)
--no-trailer          skip videos/trailer.mp4
--trailer-only        rebuild videos/trailer.mp4 from the last run in --out
--effect rain         screensaver effect (umoja-screensaver --list-effects)
--output eDP-1        monitor to capture (default: the primary one)
--no-video            stills only
--no-apps             don't open any apps
--no-demo-folder      don't create ~/Umoja Tour (a tidy folder Files and Text Editor show)
--keep-terminal       don't minimize the terminal the tour runs in
--pace 1.5            stretch every pause (slow machine, slow model)
--dry-run             print the steps without touching the desktop
```

Two scenes need a newer build than the one you may have installed: the
screenshot UI scene needs the overlay's `HideScreenshotUI` (2026.46.4+), and
the tiling scene and floating layout need the compositor's `DragWindow`
and parking the pointer needs `MovePointer` (both after 2026.46.4). Without them those steps are skipped. The
`ask-umoja` scene needs the overlay's `AskType` (after 2026.46.5) and `ai-btop` needs `AnswerConfirm` (newer still); each is skipped
without it.

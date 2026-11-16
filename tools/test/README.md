# On-device tests

Two scripts: [`umoja-feature-test`](#feature-test) for the desktop and AI
features, and [`umoja-wm-test`](#window-management-test) for window
management.

## Feature test

`umoja-feature-test` drives each new desktop and AI feature on a real
Umoja session the way a person would use it. It records a video and takes
screenshots while it works, then checks what actually happened. It is
built like [`tools/demo/umoja-demo`](../demo): everything goes over D-Bus
and `umoja-mcp`, and it never injects key presses or mouse clicks.

### Running it

1. Install the Umoja debs you want to test, log out and log back in.
2. Open a terminal in the Umoja session and run:

   ```
   tools/test/umoja-feature-test                 # every test
   tools/test/umoja-feature-test --only emoji rewrite
   tools/test/umoja-feature-test --skip captions automations
   tools/test/umoja-feature-test --list
   ```

3. Keep your hands off the keyboard and mouse until it prints the report
   path. A full run takes about 10 to 15 minutes, mostly spent waiting for
   the assistant and for the automation's timer.

Options: `--out DIR`, `--no-video` (screenshots only),
`--answer-timeout SECONDS` (how long to wait for an assistant answer,
150 by default), `--countdown SECONDS`.

### What it writes

By default to `~/umoja-feature-test-<time>/`:

| File | What's in it |
| --- | --- |
| `REPORT.md` | pass, fail or skip for each test, every check with its evidence, and the pictures |
| `results.json` | the same, for a script or Claude to read |
| `images/*.png` | screenshots at the moments that matter |
| `videos/*.webm` | one clip per test, with the sound the computer played (`--no-audio` to leave it out) |
| `audio/*.wav` | what the read-aloud tests heard from the speakers |
| `journal.txt` | the journal of the Umoja units during the run |
| `log.txt` | what the script did, step by step |

The exit status is 1 if any check failed.

### The tests

| Test | What it does and checks |
| --- | --- |
| `undo-button` | Asks the assistant to switch dark style, confirms, then presses the answer's **Undo** button. The setting goes back. |
| `undo-row` | Changes Night Light through `umoja-mcp`, then runs **Undo Last Change** from Ask Umoja. |
| `undo-words` | Changes a setting, then says "undo that" to the assistant. |
| `emoji` | Types `:party` and "rocket" in Ask Umoja, picks an emoji row, and checks that the emoji lands in the test pad's text. |
| `rewrite` | Selects a sentence full of mistakes in the test pad, presses Super+Shift+A, picks **Fix Spelling & Grammar**, presses **Replace**, and reads the corrected text back from the app. |
| `screenshot-names` | Takes a screenshot of a window showing big, known words, and checks that the file is renamed after its content. |
| `auto-dnd-fullscreen` | Sends notifications while a window is full screen. They must not pop up, and one summary must arrive afterwards. |
| `auto-dnd-recording` | The same while recording the screen. It has no clip of its own, because the test itself is the recording. |
| `calendar` | Opens the date menu and asks the assistant what's on today. If no calendar is set up, it checks that this is said plainly. |
| `setups` | Tiles two small GNOME apps, saves them as setup "Feature Test 4711", closes them, brings the setup back from Ask Umoja, and compares the tiles. |
| `automations` | Asks for an automation in plain words that runs two minutes from now, waits, and checks that it ran. |
| `captions` | Speaks a known sentence (with `espeak-ng` or `spd-say`), checks the live captions, and asks for a summary. |
| `read-aloud` | Asks for an answer that is a known sentence, presses its **Read Aloud** button, checks that it turns into a stop button and that Piper runs with the chosen voice (Alan by default; the first run downloads it), records the speakers and checks that Whisper hears the sentence. Then checks that pressing the button again, and `StopSpeaking`, stop it. |
| `read-aloud-assistant` | Asks the assistant to read a sentence out loud and checks that it used `read_aloud` and that the speech was heard the same way. |
| `voice-picker` | Opens the **Voice** page of the assistant settings, checks the list, searches, picks another voice (downloading it with a progress bar), hears its sample, switches back and removes the other voice. |
| `speech-settings` | Turns on **Read every answer aloud** and checks an answer is read out by itself, then checks that speed 1.6 reads the same sentence clearly faster. |
| `activity-recall` | Turns recall on for a moment, shows a window with a unique title and sends a notification, then asks the assistant to find both. It is skipped if recall is already on, so your history is never touched. |
| `user-guide` | Opens the User Guide from Ask Umoja and checks that a viewer window appears. |

### Safety

- Run it on the test laptop, not on a computer you're working on. It opens
  and closes windows and changes settings.
- The test hooks it uses (`AskRows`, `AskActivate`, `TestAction` and
  `AnswerConfirm` on `org.umoja.Overlay`) answer only while
  `$XDG_RUNTIME_DIR/umoja-demo` exists. The script creates that file and
  removes it.
- It holds an idle inhibitor, and before every test it checks that the
  screen is neither blanked nor locked. It stops if either is true.
- It puts back what it changed: dark style, Night Light, Do Not Disturb,
  activity recall, and the test setup and automation. Every name it creates
  contains `4711`, so leftovers are easy to spot.
- Its windows are a small GTK app, [`testpad.py`](testpad.py), and a few
  stock GNOME apps (Calculator, Characters, Clocks, Weather, Fonts).

Clips are recorded on the empty last workspace, so your own windows (and
whatever they show) stay out of them, and notifications earlier tests sent
are closed before each clip starts.

### What it needs

`python3-gi`, `grim`, `wf-recorder`, `wl-clipboard`, and for the captions
test `espeak-ng` (or `speech-dispatcher`) and `pulseaudio-utils`. The
read-aloud tests need the `piper-tts` snap and Lemonade's Whisper model
(the one captions use); sound is recorded from the default output's
monitor, so leave the volume up and other audio off. The
assistant tests use whichever model is set up, so a slow local model needs a
bigger `--answer-timeout`.

## Window management test

`umoja-wm-test` drives window management the way a person does, with real
key presses, pointer moves, clicks, drags and scrolls, and checks what the
compositor, the overview, the dock and the apps report afterwards. Every
monitor is recorded while each test runs, so the clips show what happened
on each screen. It works with one monitor or several: the tests that need a
second monitor are skipped on a single display.

```
tools/test/umoja-wm-test                       # every test
tools/test/umoja-wm-test --only hot-corner thumbnail-drag
tools/test/umoja-wm-test --skip not-responding
tools/test/umoja-wm-test --list
```

Keep your hands off the keyboard and mouse until it prints the report path.
A full run takes about 10 minutes. Options: `--out DIR`, `--no-video`,
`--countdown SECONDS`.

It writes to `~/umoja-wm-test-<time>/`: `REPORT.md` and `results.json`
(every check with its evidence), `videos/<test>-<output>.mp4` (one clip per
test and monitor), `images/` (a screenshot of every monitor at the moments
that matter), `setup.json` (monitors, workspaces, package versions),
`journal.txt` and `log.txt`. Exit status 1 means a check failed, 2 that it
stopped because the screen blanked or locked.

How it drives the session:

- Input goes through `org.gnome.Mutter.RemoteDesktop`, umoja-mir's own
  virtual keyboard and pointer (what a remote desktop app uses). Pointer
  positions are absolute layout pixels: umoja-mir takes an empty stream name
  in `NotifyPointerMotionAbsolute` to mean that.
- It reads what is on screen from `OverviewState` on `org.umoja.Overlay`
  and `TestIcons` on `org.umoja.Dock`. Both answer only while
  `$XDG_RUNTIME_DIR/umoja-demo` exists; the script creates and removes it.
- Its windows are [`testpad.py`](testpad.py) under two app ids, with
  desktop files in `~/.local/share/applications` so the dock shows them. It
  removes them at the end and puts back every setting it changes (hot
  corners, focus mode, dock scroll and isolate settings).
- It never applies a display mode (the Super+P test closes the switcher with
  Escape), never presses Force Quit, and stops before any input if the
  screen is blanked or locked.

The overview and workspace tests show every workspace, so run it on a test
machine whose windows you don't mind in the clips.

| Test | What it checks |
| --- | --- |
| `hot-corner` | Hot corner opens the overview only when it is on (O11) |
| `overview-monitor` | Overview on the primary monitor, a card on every other (O12, M1) |
| `overview-close-button` | Closing a window from the overview keeps it in place (O12) |
| `thumbnail-drag` | Drag a small window from one thumbnail to another (O1) |
| `thumbnail-place` | Drop a window inside a thumbnail to place it there (O4) |
| `new-workspace-gap` | Drop between two thumbnails makes a workspace there (O2) |
| `current-thumbnail-click` | Clicking the current workspace's thumbnail closes the overview (O5) |
| `overview-keys` | Arrow keys pick a window in the overview, Enter goes to it (O9) |
| `overview-follows-windows` | The overview follows windows opening and closing (O6) |
| `overview-cross-monitor` | Drag a window to another monitor in the overview (M1) (two monitors) |
| `super-scroll` | Super+scroll switches workspace (W3) |
| `activities-scroll` | Scrolling on the workspace dots in the top bar switches workspace (W6) |
| `workspace-left-creates` | Moving a window left from the first workspace makes a new one (W1) |
| `show-desktop` | Super+D hides every window and brings them back (S1) |
| `run-dialog` | Alt+F2 opens Run a Command (S2) |
| `display-switcher` | Super+P shows the display modes (M3) |
| `input-source` | Super+Space switches keyboard layout with a popup (S5) |
| `quick-settings-key` | Super+S opens Quick Settings (S3) |
| `focus-notification` | Super+N focuses the newest notification; Enter runs it (S3) |
| `attention` | A window that asks for focus while another is used gets a mark, not focus (F2, F3) |
| `minimize-focus` | Minimizing the active window never brings back a minimized one (F1) |
| `focus-follows-mouse` | focus-mode sloppy: the window under the pointer gets focus (F4) |
| `modal-dialog` | A modal dialog takes focus when its parent is clicked (F7) |
| `not-responding` | A window that stops answering gets the not-responding dialog (F8) |
| `shortcut-inhibit` | An app can take the system shortcuts; Super+Escape takes them back (F9) |
| `alt-tab` | Alt+Tab shows on the primary monitor and switches window (M4, A1) |
| `tiled-state` | Tiled windows are told they are tiled (P1) |
| `maximize-keeps-tile` | Maximize a tiled window, restore: back to the tile (P2) |
| `first-fit` | New windows go where they don't cover others, when there is room (P3) |
| `auto-maximize` | A window nearly the size of the screen opens maximized (P4) |
| `title-bar-on-screen` | A window can't be dragged with its title bar off screen (P5) |
| `edge-snap` | Windows snap to the screen edge while moved (P8) |
| `window-menu-csd` | Right-click on an app's own title bar opens the window menu (P6) |
| `animations` | Windows animate when they open, minimize and come back (P7) |
| `fullscreen-per-monitor` | A fullscreen window stays on top of its monitor (M2) (two monitors) |
| `dock-focus-or-previews` | A dock click focuses an app with several windows first (D1) |
| `dock-middle-click` | Middle-click on a dock icon opens a new window (D2) |
| `dock-scroll` | scroll-action cycle-windows: scrolling on an icon cycles its windows (D2) |
| `dock-super-number` | Super+number activates the dock's apps (D3) |
| `dock-numbers` | Holding Super shows the dock's Super+N numbers; a tap doesn't |
| `dock-isolate-workspaces` | isolate-workspaces: the dock shows only this workspace's windows (D4) |

Some results need a look at the clips or screenshots, and the report says
which: Quick Settings opening on Super+S, the dock's previews on a second
click, the window animations, and the top bar staying off a fullscreen
window on the other monitor.

It needs `python3-gi`, `gir1.2-gtk-4.0`, `grim` and `wf-recorder`.

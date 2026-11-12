# On-device feature test

`umoja-feature-test` drives each new desktop and AI feature on a real
Umoja session the way a person would use it. It records a video and takes
screenshots while it works, then checks what actually happened. It is
built like [`tools/demo/umoja-demo`](../demo): everything goes over D-Bus
and `umoja-mcp`, and it never injects key presses or mouse clicks.

## Running it

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

## What it writes

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

## The tests

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

## Safety

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

## What it needs

`python3-gi`, `grim`, `wf-recorder`, `wl-clipboard`, and for the captions
test `espeak-ng` (or `speech-dispatcher`) and `pulseaudio-utils`. The
read-aloud tests need the `piper-tts` snap and Lemonade's Whisper model
(the one captions use); sound is recorded from the default output's
monitor, so leave the volume up and other audio off. The
assistant tests use whichever model is set up, so a slow local model needs a
bigger `--answer-timeout`.

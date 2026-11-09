# Using the AI features

Umoja has an assistant built into the desktop. It answers questions, does
things on the desktop for you, reads what's on your screen when you ask it
to, summarizes your notifications and finds files by what they're about.

By default all of it runs on your computer, on Umoja's own copy of
[Lemonade](https://lemonade-server.ai). You can switch the assistant to a
cloud model if you prefer; see [Using a cloud model](#using-a-cloud-model)
and [Privacy](#privacy).

Voice dictation is handled by Myna, not by Umoja. Set it up with Myna
Settings (`myna-config`), which binds its shortcut (**Super+J** by default)
as a GNOME custom shortcut; Umoja runs GNOME custom shortcuts just as
Ubuntu's GNOME session does, so whatever key Myna Settings chooses works
here. While Myna records, a microphone shows in the top bar (click it to
stop) and Myna's own pill with the voice wave shows at the bottom of the
screen, as in Ubuntu. The overlay's search box also has a microphone button.

## Contents

- [Asking the assistant](#asking-the-assistant)
- [Choosing the model](#choosing-the-model)
- [Using a cloud model](#using-a-cloud-model)
- [Doing things on the desktop](#doing-things-on-the-desktop)
- [Finding and removing apps](#finding-and-removing-apps)
- [Asking about what's on screen](#asking-about-whats-on-screen)
- [Clipboard history](#clipboard-history)
- [Notifications: digest and quick replies](#notifications-digest-and-quick-replies)
- [Troubleshooting your computer](#troubleshooting-your-computer)
- [Finding files](#finding-files)
- [Privacy](#privacy)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Settings reference](#settings-reference)
- [Using Umoja's tools from other AI apps (MCP)](#using-umojas-tools-from-other-ai-apps-mcp)
- [When something doesn't work](#when-something-doesnt-work)

## Asking the assistant

Press **Super** (or click Activities) to open the overlay and start typing.
The search entry says *Search apps, or ask anything…*: the same box finds
apps and talks to the assistant.

- Type an app name and press **Enter** to launch it, as in GNOME.
- Type a question or a request and press **Enter**, or click **Ask the
  assistant** under the entry. Anything ending in `?`, four words or more,
  or starting with words like *how*, *what*, *why*, *turn*, *change*,
  *enable*, *set up* or *help* goes to the assistant. So does a short
  symptom such as "wifi broken" or "fan loud". When no app matches what you
  typed, the **Ask the assistant** button appears too.
- The answer appears under the entry, with a **Copy** button. Each thing
  the assistant does shows up as a progress line ("Turn on Dark Style",
  "Move Terminal to left") with a mark for done, failed or declined.
- **Escape** closes the overlay. Closing it, or asking something new,
  cancels the answer in progress.

The first question after login can take around ten seconds while the model
loads; later ones take a few seconds.

## Choosing the model

The round settings button beside the search entry opens **Assistant
Model**. It has two pages: **This Computer** and **Cloud**.

**This Computer** lists the chat models Lemonade can run here that fit in
half of your computer's memory, smallest first. Models that are good at
using tools are marked **Recommended** and sorted to the top; others may be
marked **Reasoning** or **Vision**. Each row shows its size and whether it
is **Downloaded** or **Downloads when selected**. Click one to use it.

The default is Qwen3-4B-Instruct-2507. Umoja downloads the selected model in
the background, at login or as soon as you pick a new one, so it's ready
before your first question:

- Downloads only start when you are online on a connection that isn't
  metered. On a metered connection Umoja waits.
- A **Downloading AI model** notification shows progress, and the overlay
  shows *Downloading the assistant's model…* until it's done.
- If the download fails you get a notification saying so. It is tried again
  at your next login, or when you pick a different model.

Models are stored in `~/.cache/umoja/lemonade`.

## Using a cloud model

If you'd rather use a larger hosted model, the assistant can talk to any
service with an OpenAI-compatible API: OpenRouter, OpenAI, or a server on
your own network.

1. Open the overlay, click the model button, and choose **Cloud**.
2. Fill in:
   - **Server URL**: the API's base URL, for example
     `https://openrouter.ai/api/v1` or `https://api.openai.com/v1`. Umoja
     adds `/chat/completions` itself. Use `https://`; plain `http://` is
     accepted for a server on your own network.
   - **API Key**: saved in your login keyring (GNOME Keyring), never in a
     settings file. Some local servers don't need one.
   - **Model**: type the model's name, or click the button to load the list
     of models the server offers and pick one. Type in the field to filter
     the list. Models marked **Tools** support tool calling, which the
     assistant needs to do things on the desktop (only providers that
     publish this, such as OpenRouter, show the mark).
3. Click **Test**. It sends one real request, with the assistant's tools,
   through the same connection the assistant uses, so a pass means
   questions and desktop actions will work. If it fails, the message says
   why (wrong key, unknown model, server unreachable…).
4. Click **Use Cloud Model**.

The Cloud page then says *In use: model on host*. To go back to your own
computer, click a model on **This Computer**, or click **Remove** on the
Cloud page, which also deletes the API key from your keyring.

Only the overlay assistant (questions, desktop actions, clipboard
Translate/Reformat/Summarize) uses the cloud model. The notification digest,
quick replies and file search always stay on your computer.

## Doing things on the desktop

Ask in plain words and the assistant does it, instead of telling you how.

| You can ask | For example |
| --- | --- |
| Open apps and Settings pages | "open Firefox", "open the display settings" |
| Dark Style, Power Mode, Wi-Fi, Bluetooth | "turn on dark mode", "switch to power saver", "turn off bluetooth" |
| Arrange windows | "put the terminal on the left", "maximize Firefox", "move this to the other monitor" |
| Workspaces | "go to workspace 2", "move Files to workspace 3" |
| Desktop settings | "make text bigger", "stop the screen dimming so fast", "show battery percentage", "turn on night light", "put the dock at the bottom" |
| Reset a setting | "put the touchpad speed back to default" |
| Close a window | "close the calculator" |
| Find or remove apps | "how is Firefox installed?", "remove Discord" |
| Explain a setting | "what does the hot corner setting do?" |
| Run a command | "how much space is left in my home folder? run df for me" |

The settings it knows by name cover text and cursor size, accent colour,
animations, the top bar clock and battery percentage, accessibility
(high contrast, screen reader, on-screen keyboard, zoom, sticky keys),
screen blanking, dimming, locking and suspend, the power button, Night
Light, notification banners, sounds, touchpad, mouse and keyboard repeat,
the dock, workspaces, the wallpaper, and Umoja's own file search,
clipboard history and shortcuts. It reads the current value first, so
relative requests like "bigger" or "slower" work. For anything else it
searches every GNOME and Umoja setting installed on your computer by name
and description, and can explain any of them (what it does, allowed
values, default and current value).

### What it asks before doing

Most actions happen straight away, because they're easy to undo. A few
open an **Allow the assistant to do this?** dialog first, with a one-line
summary of what it's about to do. **Cancel** is the default; nothing happens
unless you click **Allow**.

| Action | Asks first? |
| --- | --- |
| Open apps, Settings pages, files and folders; change settings by name; Dark Style, Power Mode, Wi-Fi, Bluetooth; arrange, focus and move windows; switch workspaces | No |
| Reading things: window list, settings, notifications, selection, clipboard, current window, diagnostics, file search | No |
| Close a window (unsaved work could be lost) | Yes |
| Run a command | Always |
| Change any other `org.gnome.*` setting by its raw key | Yes |
| Send a reply to a notification | Yes |
| Open a file that is executable or a `.desktop` launcher | Yes |
| Remove an app | Always, with what will be removed |

Commands are handled carefully:

- An ordinary command runs as you, in a sandboxed scope that can't gain
  extra privileges, and its output goes back to the assistant.
- A command that uses `sudo`, `pkexec`, `apt`, `dpkg`, `snap` or `flatpak`
  never runs in the background. The **Allow** button is shown in red, and
  the command opens in your terminal so you can see it and type your
  password yourself.

If you decline, the assistant is told not to try again.

These rules are enforced by Umoja's tool server (see
[Using Umoja's tools from other AI apps](#using-umojas-tools-from-other-ai-apps-mcp)),
not by the model, so they are the same whichever model you use, local or
cloud.

## Finding and removing apps

Apps on Ubuntu can come from several places: snaps, apt (deb packages),
Flatpak, an AppImage you downloaded, or a launcher someone made by hand.
The assistant works out which one it is itself, so you never have to run
`snap list` or `dpkg -l` to find out.

- **"How is Discord installed?"** tells you the source, package name,
  version and, for snaps, the channel and publisher. If an app is installed
  more than one way (say, both a snap and a deb), it says so.
- **"What apps do I have from Flatpak?"** lists installed apps by source.
- **"Remove Discord"** removes it the right way for where it came from.
  The **Allow** dialog (with a red **Allow** button) shows exactly what
  will happen, for example *Runs: snap remove discord*. For a deb it first
  asks apt what else would go and lists those packages too. If an app is
  installed more than one way, the assistant asks which one you mean.

How the removal runs:

| Installed as | Removal |
| --- | --- |
| Snap | `snap remove`; snapd keeps a snapshot of the app's data for a while |
| Deb (apt) | `apt-get remove` (configuration files are kept) |
| Flatpak | `flatpak uninstall`, user or system installation |
| AppImage | The AppImage and its launcher go to the Trash |
| Hand-made launcher | The launcher goes to the Trash; the program it points to stays |

Removing snaps, debs and system Flatpaks needs administrator rights. The
assistant asks for your password the usual way; if it can't show a
password prompt, it opens a terminal running the `sudo` command so you can
type it there.

It won't remove Umoja itself, base and runtime snaps other snaps need
(`core*`, `gnome-*`, `mesa-*`…), or a deb whose removal would take part
of the system with it (apt, sudo, systemd, NetworkManager and similar).

## Asking about what's on screen

The assistant can look at what you're working on, but only when you ask.

- **Super+Shift+A** asks about the text you selected in the last few
  minutes or, with nothing selected, the window you're in. The overlay
  opens with it attached; type a question or just press **Enter** to have
  it explained.
- The paperclip **Ask about something on screen** button beside the
  search entry attaches one of:
  - **Selected Text**
  - **Current Window**
  - **Screen Area…** (the screenshot UI opens so you can drag out an area)
  - **Last Copied Text**

Attachments appear as pills above the answer; hover one to see its text,
or click × to remove it. You can attach several and ask one question
about all of them, for example "compare these two".

Window and area contents are read as text with Tesseract OCR (the
`tesseract-ocr` package, installed by default with Umoja as a recommended
package). If it's missing, the assistant offers a button to install it.
OCR can misread things, so treat the text as approximate.

The assistant can also read the selection, the clipboard and the current
window by itself when you say "this", "what I copied" or "this window"
without attaching anything.

## Clipboard history

**Super+Shift+V**, or the clipboard button beside the search entry, shows
what you copied this session, newest first (up to 50 entries). Each entry
has buttons to copy it again, attach it to a question for the assistant,
or remove it, plus three actions:

- **Translate** into your desktop language
- **Reformat** (tidy up spacing, lists and line breaks)
- **Summarize**

Type in the search entry to filter the list. **Clear History** empties it.

The history:

- is kept in memory only and is gone when you log out;
- skips anything a password manager marks as secret;
- can only be watched by Umoja itself, so other apps can't read your
  clipboard in the background.

To turn it off:

```sh
gsettings set org.umoja.shell clipboard-history false
```

## Notifications: digest and quick replies

Open the notification list from the clock in the top bar.

**Digest.** When two or more notifications arrived since you last looked,
a card at the top says how many and offers **Summarize**. With three or
more, the summary starts by itself. It's a short list, at most five lines,
that merges messages from the same person or app and says which ones need
a reply or an action. **Update** refreshes it.

**Quick replies.** Chat and mail notifications get a **Reply** button,
both on the pop-up and in the list. It opens a reply box with three short
replies drafted from the message, in the message's language, from a direct
answer to a polite holding reply. Pick one, edit it or write your own,
then **Send**.

- Apps that support replies from notifications receive the reply directly.
- For other chat and mail apps (Thunderbird, Telegram, Signal, Slack,
  Element and many more), the reply is copied and the app is brought to
  the front, so you can paste it. A *Reply copied* banner tells you.

You can also ask the assistant: "what did I miss?", "any messages?", or
"reply to Sam that I'll be late". It always asks before sending a reply,
or it can open the reply box with its drafts for you to send yourself.

The digest and quick replies always run on your computer, even when the
assistant uses a cloud model.

## Troubleshooting your computer

Ask about a problem and the assistant checks this computer before
answering:

- "why is my fan so loud?", "why is my laptop slow?"
- "why won't Wi-Fi connect?", "my bluetooth headphones keep disconnecting"
- "no sound from speakers", "battery draining fast", "how hot is my CPU?"
- "my disk is full", "did anything crash?"

For questions like these it first gathers a short report (the progress
line shows what it's checking) covering CPU and memory use, temperatures
and fans, battery and power mode, network, Bluetooth, audio, disk space,
failed services and recent errors in the system log. It then names the
most likely cause and quotes the evidence, such as the process using the
CPU or the log line that shows the failure. It can look deeper at a
particular service or search the log if it needs to.

All of these checks only read. Nothing is restarted, changed or rescanned,
and none of them needs `sudo`. If the fix needs a command, the assistant
suggests one, and running it goes through the usual **Allow** dialog.

## Finding files

Umoja keeps a search index of your home folder so you can find files by
what they're about, not just their names.

- **In the overlay**, matching files appear in a **Files** list as you
  type. Click a row to open the file, or the folder button to **Show in
  Files**. Typing something like "find the PDF about the lease" and
  pressing **Enter** opens the best match.
- **Ask the assistant**: "find my tax spreadsheet from 2025", "where's the
  invoice from the plumber?", "open the beach photos from June". It shows
  the matches in the Files list and can open one or show it in Files.

What's indexed:

- The start of text files, notes, PDFs (needs `poppler-utils`) and
  OpenDocument and Office documents (needs `unzip`). When those tools
  are missing, the files are still found by name.
- Photos, videos and other files by name, folder, type and date.
- Each code project (a git checkout, or a folder with `Cargo.toml`,
  `package.json`, `meson.build`…) as a single entry described by its
  README, rather than every source file.

What's left out: hidden files and folders, anything in `.gitignore` or
`.ignore`, folders containing `.nomedia`, `.trackerignore` or
`CACHEDIR.TAG`, `~/snap`, and package folders such as `node_modules`.

The index is built by the `umoja-files` user service in
`~/.cache/umoja/files`, at idle priority so it doesn't slow you down. It
reads file contents only on mains power; on battery it only keeps the list
of names up to date. The first full index can take a while on a large home
folder, and it uses a small embedding model (nomic-embed-text) that is
downloaded on an unmetered connection.

To turn it off:

```sh
gsettings set org.umoja.shell file-search-enabled false
```

From a terminal, `umoja-files --search "lease pdf"` searches the index.

## Privacy

- **Local by default.** Questions, screen and clipboard text, the
  notification digest, quick replies and file search all go to Umoja's
  embedded Lemonade server, which listens only on your own computer
  (`127.0.0.1:13345`). Nothing is sent anywhere else.
- **Cloud is opt-in.** If you choose **Use Cloud Model**, what you ask the
  overlay assistant goes to the provider you configured: your question,
  any text you attached (selection, window or area text, clipboard), the
  troubleshooting report for troubleshooting questions, and the results of
  the tools it calls (for example your window titles, notification text or
  file names when you ask about them). Read your provider's privacy policy.
  Requests identify themselves as "Umoja Desktop".
- **Always local:** the notification digest, quick replies and the file
  search index never use the cloud model.
- **Your screen is only read when you ask**, with Super+Shift+A, an
  attachment, or a question about "this window". Captures are turned into
  text and not kept.
- **Clipboard history** lives in memory only, skips secrets, and is off
  with one setting.
- **Your API key** is in your keyring, not in GSettings or any file.
  **Remove** deletes it.
- **Model downloads** only happen on unmetered connections.

## Keyboard shortcuts

| Shortcut | What it does |
| --- | --- |
| **Super** | Open the overlay: search apps or ask the assistant |
| **Super+Shift+A** | Ask about the selected text or the current window |
| **Super+Shift+V** | Show the clipboard history |
| **Super+J** | Start or stop Myna dictation (set in Myna Settings) |
| **Escape** | Close the overlay |

The two Umoja shortcuts can be changed with the `ask-about-screen-hotkey`
and `clipboard-history-hotkey` keys below. Myna's shortcut is changed in
Myna Settings.

## Settings reference

All in the `org.umoja.shell` schema, for example
`gsettings set org.umoja.shell clipboard-history false`.

| Key | Default | What it does |
| --- | --- | --- |
| `lemonade-model` | `'Qwen3-4B-Instruct-2507-GGUF'` | The local chat model (set by the model picker) |
| `assistant-backend` | `'local'` | `'local'` or `'cloud'` |
| `cloud-base-url` | `''` | Base URL of the OpenAI-compatible service |
| `cloud-model` | `''` | Model name requested from it |
| `clipboard-history` | `true` | Keep a clipboard history |
| `file-search-enabled` | `true` | Index the home folder for file search |
| `file-search-embedding-model` | `'nomic-embed-text-v1-GGUF'` | Embedding model for file search; changing it rebuilds the index |
| `ask-about-screen-hotkey` | `['<Super><Shift>a']` | Ask about the screen |
| `clipboard-history-hotkey` | `['<Super><Shift>v']` | Clipboard history |
| `dictation-hotkey` | `[]` | Extra key for Myna dictation (Myna Settings owns Super+J) |

The embedded Lemonade server's port and memory limits are covered in
[Embedded Lemonade](../README.md#embedded-lemonade) in the README.

## Using Umoja's tools from other AI apps (MCP)

Everything the assistant can do on the desktop comes from `umoja-mcp`, a
[Model Context Protocol](https://modelcontextprotocol.io) server installed
with Umoja. The overlay starts it for you. Any other MCP client, such as
Claude Code, can use it too and gets the same tools and the same
**Allow** rules:

```sh
claude mcp add umoja -- umoja-mcp
```

Other clients take a stdio server with the command `umoja-mcp` and no
arguments. Risky tools ask for your OK through the client when it supports
MCP elicitation, and otherwise with a desktop dialog; with neither, they
refuse to run.

From a terminal:

- `umoja-mcp --list` lists the tools and their risk.
- `umoja-mcp call find_app '{"name": "discord"}'` runs one tool (risky
  ones still ask first).
- `umoja-mcp --dry-run` serves as usual but only pretends to change
  things, for testing a client.

The tools: apps (`find_app`, `list_apps`, `launch_app`, `remove_app`),
settings (`list_settings`, `describe_setting`, `change_setting`,
`reset_setting`, `set_gsetting`, `open_settings`), `set_dark_style`,
`set_power_mode`, `set_wifi`, `set_bluetooth` and `desktop_status`,
windows and workspaces, notifications, file search, `run_command`, and the
read-only troubleshooting reports. The overlay adds three of its own:
reading the selection, the clipboard history and the current window.

## When something doesn't work

- **The assistant says it can't connect.** Check the local server with
  `systemctl --user status umoja-lemonade`. It starts with your session;
  `systemctl --user restart umoja-lemonade` restarts it. With a cloud model,
  open the Cloud page and click **Test**: the message says whether the
  server, key or model is the problem.
- **"Downloading the assistant's model…" never finishes.** The download
  waits for an unmetered connection. In Settings › Network, make sure
  your connection isn't set as metered.
- **The assistant answers but doesn't do anything.** Pick a model marked
  **Recommended** (local) or **Tools** (cloud); models without tool
  calling can only answer in words.
- **Reading the window fails.** Install OCR with
  `sudo apt install tesseract-ocr`.
- **Files aren't found by their contents.** The index only reads contents
  on mains power; plug in and give it time. `systemctl --user status
  umoja-files` shows whether the service is running. Install
  `poppler-utils` for PDFs.
- **The assistant can't do anything on the desktop** (it only answers in
  words, and the log says `umoja-mcp isn't available`). Check that
  `umoja-mcp --list` runs; it ships in the `umoja-desktop` package.
- **No digest or Reply button.** The digest needs at least two unseen
  notifications. Reply is offered for chat and mail messages, not for
  system notices or progress bars.

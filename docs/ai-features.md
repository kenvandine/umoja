# Using the AI features

Umoja has an assistant built into the desktop, **Ask Umoja**. Press the
**Copilot** key (or tap **Alt** on a keyboard without one) and a card
floats over the window you're in: type to launch apps, flip switches, open
Settings pages, do sums and find files, or ask it anything. It answers
questions, does things on the desktop for you, reads what's on your screen
when you ask it to, summarizes your notifications and finds files by what
they're about.

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
screen, as in Ubuntu. Ask Umoja's card also has a microphone button.

## Contents

- [Ask Umoja](#ask-umoja)
- [Asking the assistant](#asking-the-assistant)
- [Choosing the model](#choosing-the-model)
- [Using a cloud model](#using-a-cloud-model)
- [Hearing answers read aloud](#hearing-answers-read-aloud)
- [Doing things on the desktop](#doing-things-on-the-desktop)
- [Finding and removing apps](#finding-and-removing-apps)
- [Asking about what's on screen](#asking-about-whats-on-screen)
- [Copying text from the screen](#copying-text-from-the-screen)
- [Clipboard history](#clipboard-history)
- [Notifications: digest and quick replies](#notifications-digest-and-quick-replies)
- [Troubleshooting your computer](#troubleshooting-your-computer)
- [When an app crashes](#when-an-app-crashes)
- [Finding files](#finding-files)
- [Handing coding jobs to a coding agent](#handing-coding-jobs-to-a-coding-agent)
- [Privacy](#privacy)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Settings reference](#settings-reference)
- [Using Umoja's tools from other AI apps (MCP)](#using-umojas-tools-from-other-ai-apps-mcp)
- [Using the local model from other apps](#using-the-local-model-from-other-apps)
- [When something doesn't work](#when-something-doesnt-work)

## Ask Umoja

Press the **Copilot** key to open Ask Umoja. On a keyboard without one, a
quick tap of the left **Alt** key opens it instead, as Unity's HUD did
(holding Alt for a shortcut, or Alt+Tab, doesn't). Once Umoja has seen a
Copilot key on your computer, Alt is left to your apps. **Super** still
opens the overview and app grid, which have no assistant.

The card floats over the window you were in and shows its app as a chip
beside the entry ("Firefox"): that app's launcher actions ("New Private
Window") come first, and questions are asked with it in mind. **Backspace**
in the empty entry, or a click on the chip, searches everything instead.

With nothing typed, the card lists the app's launcher actions, the things
you pick most often and a few suggestions (ask about this window, the
clipboard history, a screenshot). As you type, rows come up for:

- **Apps**, and the launcher actions of the app you name ("firefox private").
- **Switches**: Dark Style, Wi-Fi, Bluetooth, Do Not Disturb and Night
  Light, with their current state. "wifi off" or "turn on bluetooth" offers
  exactly that.
- **Desktop actions**: power modes, mute, play/pause, next track, empty the
  Trash, lock, suspend, log out, restart, power off, a screenshot or screen
  recording, the clipboard history.
- **Settings pages**: Wi-Fi, Sound, Displays, Keyboard and the rest.
- **Sums**: "12*7+5" or "15% of 80"; **Enter** copies the result.
- **Files** from the search index (see [Finding files](#finding-files)).
- **Ask Umoja: "…"**, which hands the text to the assistant. It goes first
  when what you typed reads like a question and nothing else is a clear
  match.
- **Search the web for "…"**, in your default browser.

**↑** and **↓** move through the rows, **Enter** runs the selected one,
**Tab** asks the assistant whatever is selected, and **Escape** or a click
outside the card closes it. Everything except asking runs straight away
through Umoja's tool server, without the model, so it takes well under a
second. The same confirmation rules apply as when the assistant does it
(see [What it asks before doing](#what-it-asks-before-doing)): logging out
or emptying the Trash still asks first.

Ask Umoja remembers which rows you pick (only which rows, never what you
typed) in `~/.local/share/umoja/ask-history.json`, so the ones you use most
move up. It keeps nothing while Settings › Privacy › **Remember App Usage**
is off.

### Screen readers

Ask Umoja works with Orca. The card is a dialog called "Ask Umoja"; the
selected row is read out as you move (its name, what kind of row it is,
whether a switch is on), and confirmations and finished answers are read
out too.

## Asking the assistant

Open [Ask Umoja](#ask-umoja), type a question or a request and press
**Enter** on the **Ask Umoja** row, or **Tab** from anywhere. Anything
ending in `?`, four words or more, or starting with words like *how*,
*what*, *why*, *turn*, *change*, *enable*, *set up* or *help* puts that row
first. So does a short symptom such as "wifi broken" or "fan loud".

- The answer appears in the card under your question, with a **Copy**
  button. Each thing
  the assistant does shows up as a progress line ("Turn on Dark Style",
  "Move Terminal to left") with a mark for done, failed or declined.
- When the answer ends with a question ("btop is already installed. Do you
  want me to open it?"), the entry turns into a reply box (*Reply to
  Umoja…*): type your answer and press **Enter**. Yes-or-no questions also
  get **Yes** and **No** buttons. Until you close the card or clear the
  entry, anything you ask is a follow-up, so the assistant remembers the
  last few exchanges ("and vlc too").
- **Escape** closes the card. Closing it, or asking something new, cancels
  the answer in progress.

The first question after login can take around ten seconds while the model
loads; later ones take a few seconds.

## Choosing the model

The round settings button beside the search entry opens **Assistant**
settings. It has three pages: **This Computer** and **Cloud** pick the
model, and **Voice** picks the read-aloud voice (see
[Choosing a voice](#choosing-a-voice)).

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
- A **Downloading AI model** notification shows progress, and Ask Umoja
  shows *Downloading the assistant's model…* until it's done.
- If the download fails you get a notification saying so. It is tried again
  at your next login, or when you pick a different model.

The model that suits this computer best is marked **Best for this
computer** and listed first: the largest recommended model that fits in a
quarter of your memory, so your apps keep the rest.

**Free Memory**, under the list, unloads the model when you need its memory
for something else (a game, a big build, a virtual machine). It loads again
by itself with your next question, which then takes a few seconds longer.
You can also ask the assistant to "free up the AI model's memory".

The model also unloads by itself after 10 minutes without a question,
including questions from notification digests and quick replies. Change the
wait with `gsettings set org.umoja.shell assistant-idle-unload <minutes>`
(0 keeps it loaded).

Models are stored in `~/.cache/umoja/lemonade`.

## Using a cloud model

If you'd rather use a larger hosted model, the assistant can talk to any
service with an OpenAI-compatible API: OpenRouter, OpenAI, or a server on
your own network.

1. Open Ask Umoja, click the model button at the bottom of the card, and
   choose **Cloud**.
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

The Cloud page then says *In use: model on host*; with OpenRouter it also
shows how much credit is left. To go back to your own
computer, click a model on **This Computer**, or click **Remove** on the
Cloud page, which also deletes the API key from your keyring.

Only the overlay assistant (questions, desktop actions, clipboard
Translate/Reformat/Summarize) uses the cloud model. The notification digest,
quick replies and file search always stay on your computer.

## Hearing answers read aloud

The speaker button next to an answer's copy button reads the answer out;
press it again (it shows a stop square meanwhile) to stop. You can also
ask the assistant to "read that to me". Code blocks are skipped and
Markdown marks are left out.

The voice is [Piper](https://github.com/OHF-Voice/piper1-gpl)'s British
"Alan", which runs on this computer. Piper comes from the `piper-tts` snap.
A deb can't depend on a snap, so the first time something is read out
without it, Umoja installs the snap (you're asked for your password, as for
any software install). If that's declined or there's no internet,
speech-dispatcher's voice reads instead. To install it yourself:

```sh
sudo snap install piper-tts
```

### Choosing a voice

Click the round settings button beside Ask Umoja's entry and pick
the **Voice** page. It lists the Piper voices for your language, with their
size and whether they're downloaded; search to find voices in other
languages. Picking a voice downloads it (the bar shows how far), makes it
the voice and plays a short sample. The bin button removes a downloaded
voice you no longer use. The same tab sets the **Speed** and **Read every
answer aloud**. If Piper isn't installed yet, the tab offers
**Install Piper**.

Voices come from [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices)
on Hugging Face, into `~/snap/piper-tts/common/voices` (or
`~/.local/share/umoja/voices` for a `piper` installed another way, such as
`pip install piper-tts`). The same settings work from a terminal:

```sh
gsettings set org.umoja.shell speech-voice en_US-lessac-medium
gsettings set org.umoja.shell speech-rate 1.2       # a little faster
gsettings set org.umoja.shell speak-answers true    # read every answer
```

This is separate from the screen reader: Orca keeps its own voice, and
`speak-answers` is skipped while Orca is on, since it reads answers
already.

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
| Log out, lock, suspend, restart, power off | "log me out", "lock the screen", "restart the computer", "shut down" |
| Screenshots | "take a screenshot of this window", "screenshot the whole screen", "screenshot an area", "screenshot Firefox in 3 seconds" |
| Screen recording | "record my screen", "stop recording" |
| Volume and sound devices | "turn it down", "volume to 30%", "mute my mic", "play sound through my headphones" |
| Screen brightness | "make the screen brighter", "brightness 40%" |
| Music and video | "pause the music", "next song", "what's playing?" |
| Do Not Disturb, Night Light, airplane mode | "turn on do not disturb", "night light off", "airplane mode on" |
| Wi-Fi networks and Bluetooth devices | "connect to the CoffeeShop Wi-Fi", "connect my headphones", "disconnect the speaker" |
| The web | "open ubuntu.com", "search the web for GTK 4 layer shell" |
| Folders and the wallpaper | "open my Downloads", "show the Trash", "use this photo as my background" |
| The Trash, clearing a folder, USB drives | "empty the trash", "clear out my Downloads", "eject the USB stick" |
| A frozen app | "Firefox is frozen, force quit it" |
| Install apps and updates | "install VLC", "are there any updates?", "install the updates" |
| The date, time and this computer | "what time is it?", "what week is it?", "which Ubuntu version is this?", "what's my IP address?", "how much RAM do I have?" |
| The clipboard | "copy that command to the clipboard" |
| Find or remove apps | "how is Firefox installed?", "remove Discord" |
| Explain a setting | "what does the hot corner setting do?" |
| Run a command | "list the files in my Projects folder with their sizes" |

Each of these is a single step with its own tool, so the assistant never
asks you to run or approve a shell command for them. When one needs your
OK, the dialog says what will happen in plain words ("Take a screenshot
of the Firefox window", "Power off the computer", "Install vlc"), not the
command behind it. If a model still tries a command such as `systemctl
poweroff`, `grim` or `apt install vlc`, Umoja's tool server turns it into
the matching action instead of asking you to approve the command. Web
searches open DuckDuckGo in your default browser.

### Screenshots

"Take a screenshot" captures the window you were working in before you
opened Ask Umoja. The card gets out of the way first, so it isn't in
the picture, and comes back afterwards. You can also ask for the whole
screen, for an area (the screenshot UI opens so you can drag one out), for
another window by name ("screenshot the terminal"), or for a short delay.
Only when you ask for a picture of the assistant itself ("screenshot the
overlay") does it stay on screen. Screenshots are saved to
Pictures/Screenshots and copied to the clipboard, with the usual
*Screenshot captured* notification, exactly like the **Print** key.

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
ask **Allow Umoja to do this?** first, inside the card, with a one-line
summary of what it's about to do. **Cancel** has the focus; nothing happens
unless you pick **Allow**. Closing the card declines.

| Action | Asks first? |
| --- | --- |
| Open apps, Settings pages, files, folders and web pages; change settings by name; Dark Style, Power Mode, Wi-Fi, Bluetooth, airplane mode, Do Not Disturb, Night Light; volume, sound device, brightness, media playback; join a Wi-Fi network, connect a Bluetooth device; set the wallpaper; eject a drive; copy to the clipboard; arrange, focus and move windows; switch workspaces; stop a screen recording | No |
| Reading things: window list, settings, notifications, selection, clipboard, current window, diagnostics, file search, date and time, system details, available updates, what's playing | No |
| Take a screenshot, start a screen recording | Yes |
| Close a window, force quit an app (unsaved work could be lost) | Yes |
| Empty the Trash (says how many items; an empty Trash doesn't ask) | Yes |
| Clear a folder (moves its contents to the Trash, lists what goes; only folders inside your home folder) | Yes |
| Log out, suspend, restart, power off (locking doesn't ask) | Yes |
| Install an app (says which package, version and publisher) or install updates (lists them) | Always, with a red **Allow** button |
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

**"Install VLC"** works the same way in reverse. The assistant looks for
a snap first (as App Center does), then for a deb in the Ubuntu archive,
and the **Allow** dialog says what it found, for example *vlc 3.0.21 from
the Snap Store, published by VideoLAN*. Snaps that run without the
sandbox (classic confinement) say so. If nothing has that exact name, it
suggests close matches. **"Are there any updates?"** lists waiting deb and
snap updates, and **"install the updates"** shows that list in the dialog
and then runs the upgrade and a snap refresh, asking for your password
the usual way.

It won't remove Umoja itself, base and runtime snaps other snaps need
(`core*`, `gnome-*`, `mesa-*`…), or a deb whose removal would take part
of the system with it (apt, sudo, systemd, NetworkManager and similar).

## Asking about what's on screen

The assistant can look at what you're working on, but only when you ask.

- **Super+Shift+A** asks about the text you selected in the last few
  minutes or, with nothing selected, the window you're in. Ask Umoja
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

## Copying text from the screen

**Super+Ctrl+Print** opens the screenshot UI in **Copy Text** mode (the
third button beside Screenshot and Screencast; you can also switch to it
after pressing **Print**). Drag out an area, or pick Screen or Window, and
click the capture button: the text in it is read with Tesseract OCR and put
on the clipboard, and a *Text copied* notification shows the start of it.
Nothing is saved to Pictures. This is handy for phone numbers or addresses
in images, or an error message in a dialog you can't select.

It needs the `tesseract-ocr` package (installed by default as a
recommended package); without it the notification says how to install it.
OCR can misread things, so check what you paste.

## Clipboard history

**Super+Shift+V**, or the clipboard button at the bottom of Ask Umoja, shows
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

## When an app crashes

When a program crashes, a notification says so, for example *Files quit
unexpectedly*, with two buttons:

- **Ask Why** (or click the notification) opens Ask Umoja and asks the
  assistant why it crashed. It reads the crash report and explains it in
  plain words: whether the computer was out of memory, whether the crash
  started right after a software update, whether it keeps happening, and
  the top of the stack trace when the report has one. If it can't tell the
  exact cause, it says so rather than guessing.
- **Don't Show for This App** stops these notifications for that program.

This always runs on the local model, even when the assistant is set to a
cloud model, so crash reports never leave your computer. You can also ask
at any time: "did anything crash today?", "why does Files keep crashing?".
If the crash is in an Ubuntu package, the assistant can open Ubuntu's crash
reporter ("send the Files crash to Ubuntu"), which shows what would be
sent and only sends it if you choose to.

Crashes come from Ubuntu's own records: Apport's reports in `/var/crash`,
and `systemd-coredump` (installed with Umoja as a recommended package),
which also records repeat crashes and a stack trace. One notification is
shown per program per minute, so a program stuck in a crash loop doesn't
flood you. To turn them off:

```sh
gsettings set org.umoja.shell crash-notifications false
```

To bring back a program you muted, remove it from
`gsettings get org.umoja.shell crash-notifications-muted`.

## Finding files

Umoja keeps a search index of your home folder so you can find files by
what they're about, not just their names.

- **In Ask Umoja**, matching files appear as rows as you type; **Enter**
  opens one. Something like "find the PDF about the lease" puts the files
  first.
- **In the app grid**, matching files appear in a **Files** list under the
  entry. Click a row to open the file, or the folder button to **Show in
  Files**.
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

## Handing coding jobs to a coding agent

The assistant is good at doing things on the desktop, but a small local
model isn't the right tool for writing a script or changing code. If you
have an AI coding agent installed (Claude Code, Codex, Gemini CLI or
OpenCode), the assistant hands those jobs to it:

- Ask for a coding job ("write me a script that renames my photos by
  date") and the assistant offers to open your coding agent with the task.
  The **Allow** dialog says which agent and what it will be asked.
- Under any answer to a coding question there is also a **Continue in
  Claude Code** button (named after your agent).
- The agent opens in a terminal with your request as its first message, in
  its normal mode, so it asks before it changes or runs anything. It
  already knows Umoja through the skill and tools `umoja-agent-setup`
  added.

Pick which agent gets these jobs under **Coding agent** in the model
button's popover (only installed agents are listed; **None** turns the
hand-off off), or with `gsettings set org.umoja.shell ai-agent claude`.
Umoja never installs an agent for you. The `ai-agent-hotkey` key (unset by
default) can open the agent with a keyboard shortcut.

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
- **Always local:** the notification digest, quick replies, crash
  diagnosis from a crash notification and the file search index never use
  the cloud model.
- **Your screen is only read when you ask**, with Super+Shift+A, an
  attachment, or a question about "this window". Captures are turned into
  text and not kept.
- **Clipboard history** lives in memory only, skips secrets, and is off
  with one setting.
- **Ask Umoja's usage history** keeps only which rows you pick, on your
  computer, and nothing while Remember App Usage is off.
- **Your API key** is in your keyring, not in GSettings or any file.
  **Remove** deletes it.
- **Model downloads** only happen on unmetered connections.

## Keyboard shortcuts

| Shortcut | What it does |
| --- | --- |
| **Copilot** key | Open Ask Umoja |
| tap **Alt** | Open Ask Umoja, on keyboards without a Copilot key |
| **Super** | Open the overview and app grid (apps and files, no assistant) |
| **Super+Shift+A** | Ask about the selected text or the current window |
| **Super+Shift+V** | Show the clipboard history |
| **Super+Ctrl+Print** | Copy the text in a screen area |
| **Super+J** | Start or stop Myna dictation (set in Myna Settings) |
| **Escape** | Close Ask Umoja |

The three Umoja shortcuts can be changed with the `ask-about-screen-hotkey`,
`clipboard-history-hotkey` and `copy-text-hotkey` keys below. Myna's shortcut is changed in
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
| `copy-text-hotkey` | `['<Super><Control>Print']` | Copy the text in a screen area |
| `ai-agent` | `''` | Coding agent for coding jobs: `claude`, `codex`, `gemini`, `opencode`, `none`; empty picks the first installed |
| `ai-agent-hotkey` | `[]` | Open the coding agent in a terminal |
| `crash-notifications` | `true` | Notify when an app crashes, with **Ask Why** |
| `crash-notifications-muted` | `[]` | Programs whose crashes don't notify |
| `dictation-hotkey` | `[]` | Extra key for Myna dictation (Myna Settings owns Super+J) |
| `ask-umoja-alt-tap` | `true` | A tap of Alt opens Ask Umoja, until a Copilot key is seen |
| `ask-umoja-copilot-seen` | `false` | Set on the first Copilot key press; reset it to use Alt again |
| `speech-voice` | `'en_GB-alan-medium'` | Piper voice for reading answers aloud |
| `speech-rate` | `1.0` | How fast answers are read (0.5 to 2.0) |
| `speak-answers` | `false` | Read every answer aloud (not while Orca runs) |

The embedded Lemonade server's port and memory limits are covered in
[Embedded Lemonade](../README.md#embedded-lemonade) in the README.

## Using Umoja's tools from other AI apps (MCP)

Everything the assistant can do on the desktop comes from `umoja-mcp`, a
[Model Context Protocol](https://modelcontextprotocol.io) server installed
with Umoja. The overlay starts it for you. Any other MCP client, such as
Claude Code, can use it too and gets the same tools and the same
**Allow** rules.

**AI coding agents are connected for you.** At login, `umoja-agent-setup`
looks for Claude Code, Codex, Gemini CLI and OpenCode. For each one it finds
for the first time, it:

- registers `umoja-mcp` with it (`claude mcp add --scope user`, Codex's
  `config.toml`, Gemini's `settings.json`, OpenCode's `opencode.json`), and
- links Umoja's agent skill into its skills folder (and `~/.agents/skills`),
  so the agent knows how Umoja is configured: GSettings rather than config
  files, which unit to restart, where the logs are, and never to edit
  `/usr`.

A notification says which agents were connected. It never installs an
agent, never changes an entry it didn't write, and an agent you install
later is connected at your next login. From a terminal:

- `umoja-agent-setup --status` shows what's connected.
- `umoja-agent-setup --remove` (or `--remove claude`) undoes it, and that
  agent is then left alone at login.
- `umoja-agent-setup claude` connects one again.

The skill itself is in `/usr/share/umoja/agents/skills/umoja`. Agents
started this way still ask before risky actions: the **Allow** rules below
are enforced by `umoja-mcp`, not by the agent.

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
`set_power_mode`, `set_wifi`, `set_bluetooth`, `desktop_status`, `session`,
windows and workspaces, notifications, file search, `run_command`, and the
read-only troubleshooting reports. The overlay adds three of its own:
reading the selection, the clipboard history and the current window.

## Using the local model from other apps

Umoja's Lemonade server speaks the OpenAI API, so other apps on this
computer can use the same local model: point them at
`http://127.0.0.1:13345/api/v1` with any API key, and use the model name
from `gsettings get org.umoja.shell lemonade-model` (for example
`Qwen3-4B-Instruct-2507-GGUF`). `curl http://127.0.0.1:13345/api/v1/models`
lists the models that are downloaded. It only listens on this computer, and
the port can be changed in `~/.config/umoja/lemonade.env` (see
[Embedded Lemonade](../README.md#embedded-lemonade)).

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

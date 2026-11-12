# 11. When something goes wrong, and going further

## First, ask

Many problems can be explained by the assistant itself. "Why won't my
Wi-Fi connect?", "why is everything slow?", "no sound from my speakers".
It checks the computer before answering. See
[Troubleshooting your computer](07-everyday-ai.md#troubleshooting-your-computer).

## Common problems

**The assistant says it can't connect.**
The local AI server may have stopped. In a terminal, run
`systemctl --user restart umoja-lemonade`. With a cloud model, open the
Cloud page and click **Test**. The message says whether the server, the
key or the model is the problem.

**"Downloading the assistant's model…" never finishes.**
Models only download on connections that aren't metered. In **Settings ›
Network**, check that your connection isn't set as metered.

**The assistant answers but doesn't actually do anything.**
Choose a model marked **Recommended** (on This Computer) or **Tools** (on
Cloud). Other models can only answer in words.

**Live captions say speech-to-text isn't answering.**
The speech model may still be downloading. Give it a minute. Captions also
need the `pulseaudio-utils` package (installed with Umoja), and some sound
playing.

**Rewrite or emoji didn't type anything.**
A few apps don't accept typed text from other programs. The text is copied
instead, so press **Ctrl+V**.

**Calendar events don't show.**
Add the calendar in **Settings › Online Accounts** or GNOME Calendar. The
date menu only shows events when at least one calendar is set up.

**Reading a window or copying text fails.**
Install OCR: `sudo apt install tesseract-ocr`.

**Files aren't found by what's in them.**
The index only reads file contents while the computer is plugged in. Give
it time on a big home folder. For PDFs, install `poppler-utils`.

**An automation didn't run.**
Ask "what automations do I have?" to check it's turned on. Automations run
only while you're logged in. `journalctl --user -u umoja-automations`
shows what they did.

**Notifications didn't pop up.**
Check **Do Not Disturb** in the date menu. Remember that pop-ups also wait
while you record the screen or use an app full screen.

**An app crashed, or the whole screen went black.**
Each part of the desktop (top bar, dock, Ask Umoja) runs separately and
restarts by itself if it crashes. If the display itself crashes, Umoja
restarts it in place instead of logging you out. Your open apps are lost
but the session comes back.

## Going further

### Using Umoja's tools from other AI apps

Everything the assistant can do comes from `umoja-mcp`, an
[MCP](https://modelcontextprotocol.io) server. Other AI apps can use it
too, and they get the same "ask first" rules. Claude Code, Codex, GitHub
Copilot, Antigravity, OpenCode and Gemini CLI are connected for you at
login. `umoja-agent-setup
--status` shows which are connected, and `umoja-agent-setup --remove`
disconnects them. For other apps, add a stdio MCP server with the command
`umoja-mcp`.

From a terminal, `umoja-mcp --list` lists every tool and how risky it is.

### Using the local model from other apps

The local AI server speaks the OpenAI API at
`http://127.0.0.1:13345/api/v1`, so other apps on this computer can use
the same model. Use any API key, and the model name from
`gsettings get org.umoja.shell lemonade-model`.

### More detail

- [Using the AI features](../ai-features.md) is the detailed reference for
  everything the assistant does and asks.
- The [project README](../../README.md) covers installing, removing and
  building Umoja.

## Getting help

Umoja is a personal project. Bug reports and ideas are welcome in
[GitHub issues](https://github.com/kenvandine/umoja/issues). Say which
version you have (`apt policy umoja-desktop` shows it) and what you were
doing.

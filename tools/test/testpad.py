#!/usr/bin/python3
"""A scratch text window for umoja-feature-test.

A plain GTK 4 window with one text area. umoja-feature-test drives it over
stdin, one command per line:

    text <words>      replace the text (and select none of it)
    select            select all of it (which makes it the primary selection)
    title <words>     set the window title
    big               large type (for screenshot OCR)
    font <points>     type of that size (umoja-demo: readable in a video)
    fullscreen / unfullscreen
    present           ask to be focused
    present-in <s>    ask to be focused after that many seconds (with no
                      activation token: umoja-wm-test's attention test)
    hang <s>          block the main loop that long (not responding)
    dialog            open a modal dialog over the window
    close-dialog
    inhibit           take the system shortcuts (keyboard-shortcuts-inhibit)
    uninhibit
    state             reply with the window's toplevel state flags
    active            reply whether the window and its dialog have the
                      keyboard: "ok active <window 0/1> <dialog 0/1/->"
    quit

Every change to the text is written to the file given as the first
argument, so the test can read exactly what Umoja typed into a real app.
`--app-id ID` and `--size WxH` set the application id and the window's
first size (umoja-wm-test).
"""

import os
import stat
import sys
import time

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

ARGS = sys.argv[1:]


def option(name, default):
    if name in ARGS:
        i = ARGS.index(name)
        value = ARGS[i + 1] if i + 1 < len(ARGS) else default
        del ARGS[i:i + 2]
        return value
    return default


APP_ID = option("--app-id", "org.umoja.FeatureTestPad")
SIZE = tuple(int(v) for v in option("--size", "760x420").split("x"))
OUT = ARGS[0] if ARGS else "/dev/null"


class Pad(Gtk.Application):
    def __init__(self):
        # NON_UNIQUE: a test may open two pads, and a second one must not
        # just hand over to the first and exit.
        super().__init__(application_id=APP_ID, flags=Gio.ApplicationFlags.NON_UNIQUE)
        self.win = None
        self.view = None
        self.dialog = None

    def do_activate(self):
        self.win = Gtk.ApplicationWindow(application=self, title="Umoja feature test pad")
        self.win.set_default_size(*SIZE)
        self.view = Gtk.TextView(wrap_mode=Gtk.WrapMode.WORD_CHAR, left_margin=16, right_margin=16,
                                 top_margin=16, bottom_margin=16)
        self.view.get_buffer().connect("changed", self.save)
        scroll = Gtk.ScrolledWindow(child=self.view)
        self.win.set_child(scroll)
        self.css = Gtk.CssProvider()
        Gtk.StyleContext.add_provider_for_display(self.win.get_display(), self.css,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        self.win.present()
        self.view.grab_focus()
        # Commands come down a pipe from the test. A pad the dock or the
        # overview launched has /dev/null there, which reads as the end
        # straight away: that pad just stays open.
        mode = os.fstat(sys.stdin.fileno()).st_mode
        if stat.S_ISFIFO(mode) or stat.S_ISSOCK(mode):
            GLib.io_add_watch(GLib.IOChannel.unix_new(sys.stdin.fileno()), GLib.PRIORITY_DEFAULT,
                              GLib.IOCondition.IN | GLib.IOCondition.HUP, self.command)
        self.save()
        print("ready", flush=True)

    def save(self, *_):
        buf = self.view.get_buffer()
        text = buf.get_text(buf.get_start_iter(), buf.get_end_iter(), True)
        with open(OUT, "w", encoding="utf-8") as f:
            f.write(text)

    pending = b""

    def command(self, source, cond):
        # Raw reads: Python's buffered readline() could swallow a second
        # command that arrived with the first, and the watch never fired
        # for it again. Every command is answered, even one that fails, so
        # the test never waits on a pad that went deaf.
        data = os.read(sys.stdin.fileno(), 4096)
        if not data:
            self.quit()
            return False
        self.pending += data
        while b"\n" in self.pending:
            raw, _, self.pending = self.pending.partition(b"\n")
            try:
                more = self.run_command(raw.decode("utf-8", "replace"))
            except Exception as e:  # noqa: BLE001
                print(f"error {type(e).__name__}: {e}", flush=True)
                more = True
            if not more:
                return False
        return True

    def run_command(self, line):
        cmd, _, arg = line.partition(" ")
        buf = self.view.get_buffer()
        if cmd == "text":
            buf.set_text(arg.replace("\\n", "\n"))
            buf.place_cursor(buf.get_end_iter())
        elif cmd == "select":
            self.view.grab_focus()
            buf.select_range(buf.get_start_iter(), buf.get_end_iter())
        elif cmd == "title":
            self.win.set_title(arg)
        elif cmd == "big":
            self.css.load_from_string("textview { font-size: 34pt; }")
        elif cmd == "font":
            self.css.load_from_string(f"textview {{ font-size: {float(arg or 11):g}pt; }}")
        elif cmd == "fullscreen":
            self.win.fullscreen()
        elif cmd == "unfullscreen":
            self.win.unfullscreen()
        elif cmd == "present":
            self.win.present()
            self.view.grab_focus()
        elif cmd == "present-in":
            GLib.timeout_add(int(float(arg or 1) * 1000), lambda: (self.win.present(), False)[1])
        elif cmd == "hang":
            # Answer first, then stop answering anything (pings included).
            print(f"ok {cmd}", flush=True)
            GLib.idle_add(lambda: (time.sleep(float(arg or 10)), False)[1])
            return True
        elif cmd == "dialog":
            self.dialog = Gtk.Window(title=f"{self.win.get_title()} dialog", transient_for=self.win,
                                     modal=True, default_width=320, default_height=160)
            self.dialog.set_child(Gtk.Label(label="A modal dialog"))
            self.dialog.present()
        elif cmd == "close-dialog":
            if self.dialog:
                self.dialog.destroy()
                self.dialog = None
        elif cmd == "inhibit":
            self.win.get_surface().inhibit_system_shortcuts(None)
        elif cmd == "uninhibit":
            self.win.get_surface().restore_system_shortcuts()
        elif cmd == "state":
            state = int(self.win.get_surface().get_state())
            print(f"ok state {state}", flush=True)
            return True
        elif cmd == "active":
            # GetWindows lists toplevels only, so a dialog's focus is
            # asked of GTK itself.
            dialog = "-" if self.dialog is None else str(int(self.dialog.is_active()))
            print(f"ok active {int(self.win.is_active())} {dialog}", flush=True)
            return True
        elif cmd == "quit":
            self.quit()
            return False
        print(f"ok {cmd}", flush=True)
        return True


if __name__ == "__main__":
    sys.exit(Pad().run([]))

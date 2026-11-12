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
    quit

Every change to the text is written to the file given as the only argument,
so the test can read exactly what Umoja typed into a real app.
"""

import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else "/dev/null"


class Pad(Gtk.Application):
    def __init__(self):
        # NON_UNIQUE: a test may open two pads, and a second one must not
        # just hand over to the first and exit.
        super().__init__(application_id="org.umoja.FeatureTestPad",
                         flags=Gio.ApplicationFlags.NON_UNIQUE)
        self.win = None
        self.view = None

    def do_activate(self):
        self.win = Gtk.ApplicationWindow(application=self, title="Umoja feature test pad")
        self.win.set_default_size(760, 420)
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
        GLib.io_add_watch(GLib.IOChannel.unix_new(sys.stdin.fileno()), GLib.PRIORITY_DEFAULT,
                          GLib.IOCondition.IN | GLib.IOCondition.HUP, self.command)
        self.save()
        print("ready", flush=True)

    def save(self, *_):
        buf = self.view.get_buffer()
        text = buf.get_text(buf.get_start_iter(), buf.get_end_iter(), True)
        with open(OUT, "w", encoding="utf-8") as f:
            f.write(text)

    def command(self, source, cond):
        line = sys.stdin.readline()
        if not line:
            self.quit()
            return False
        cmd, _, arg = line.rstrip("\n").partition(" ")
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
        elif cmd == "quit":
            self.quit()
            return False
        print(f"ok {cmd}", flush=True)
        return True


if __name__ == "__main__":
    sys.exit(Pad().run([]))

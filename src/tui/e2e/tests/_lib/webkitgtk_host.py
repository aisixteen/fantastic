"""THE WEBKITGTK HOST (E88, #52) — a real WebKitGTK webview, the one the Tauri
app uses on Linux (gi `WebKit2` 4.1 on GTK 3), driven over stdio. It runs under
the SYSTEM python (`/usr/bin/python3 -I`, where python3-gi lives), never the
e2e venv; `webkitgtk.py` is its client.

    [xvfb-run -a] /usr/bin/python3 -I webkitgtk_host.py --url URL --width 1400 --height 900 [--init-script FILE]

The webview keeps its defaults, as the Tauri shell has them (no WEBKIT_*
variables; `enable-webgl` on). stdout, one JSON object a line, each after the mark `@webkitgtk ` (anything
else there is the X server's, GTK's or WebKit's own output):

    {"ready": true, "ua": <navigator.userAgent>}     the page's load FINISHED
    {"id": n, "value": <the result as JSON>}         an evaluation's answer
    {"id": n, "error": "…"}                          …or its failure

stdin, one JSON object a line: `{"id": n, "js": "<expression>"}` evaluates
`return await (<expression>);` (call_async_javascript_function, WebKitGTK ≥
2.40); `{"cmd": "quit"}` exits.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2  # noqa: E402


#: Every protocol line starts with this: the X server `xvfb-run` starts shares
#: our stdout, and what it (or GTK, WebKit, Mesa) prints there is not protocol.
MARK = "@webkitgtk "


def say(obj: dict) -> None:
    sys.stdout.write(MARK + json.dumps(obj) + "\n")
    sys.stdout.flush()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--width", type=int, default=1400)
    ap.add_argument("--height", type=int, default=900)
    ap.add_argument("--init-script")
    a = ap.parse_args()

    ucm = WebKit2.UserContentManager()
    if a.init_script:
        with open(a.init_script, encoding="utf-8") as f:
            src = f.read()
        ucm.add_script(WebKit2.UserScript.new(src, WebKit2.UserContentInjectedFrames.TOP_FRAME,
                                              WebKit2.UserScriptInjectionTime.START, None, None))
    view = WebKit2.WebView.new_with_user_content_manager(ucm)
    view.get_settings().set_enable_webgl(True)
    win = Gtk.Window()
    win.set_default_size(a.width, a.height)
    win.add(view)
    win.connect("destroy", lambda *_: Gtk.main_quit())

    def evaluate(n: object, js: str, then=None) -> None:
        def done(v: WebKit2.WebView, res) -> None:
            try:
                val = v.call_async_javascript_function_finish(res)
                out = json.loads(val.to_json(0)) if val is not None and not val.is_undefined() else None
            except Exception as e:  # noqa: BLE001 — every failure goes back to the client
                why = str(getattr(e, "message", e))
                say({"id": n, "error": why} if then is None else {"ready": False, "error": why})
                return
            if then is not None:
                then(out)
            else:
                say({"id": n, "value": out})

        view.call_async_javascript_function(f"return await ({js});", -1, None, None, None, None, done)

    ready = [False]

    def loaded(v: WebKit2.WebView, ev) -> None:
        if ev == WebKit2.LoadEvent.FINISHED and not ready[0]:
            ready[0] = True
            evaluate(None, "navigator.userAgent", lambda ua: say({"ready": True, "ua": ua}))

    view.connect("load-changed", loaded)

    # stdin read RAW: a buffered readline would keep a second line that came in
    # the same chunk, and the watch would not fire for it
    buf = [b""]

    def on_stdin(src, cond) -> bool:
        chunk = os.read(sys.stdin.fileno(), 65536) if cond & GLib.IO_IN else b""
        if chunk == b"":
            Gtk.main_quit()
            return False
        buf[0] += chunk
        *lines, buf[0] = buf[0].split(b"\n")
        for line in lines:
            if not line.strip():
                continue
            msg = json.loads(line)
            if msg.get("cmd") == "quit":
                Gtk.main_quit()
                return False
            evaluate(msg["id"], msg["js"])
        return True

    GLib.io_add_watch(sys.stdin, GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, on_stdin)
    win.show_all()
    view.load_uri(a.url)
    Gtk.main()
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Pair two real qrtx devices from a WebKit browser (WebKitGTK), to catch
Safari-engine problems that Chromium-based e2e tests miss.

    xvfb-run -a python3 tests/webkit.py path/to/qrtx [--relaydots]

Needs PyGObject with WebKit2 4.1 (webkit2gtk-4.1) and network access to the
n0 relays. Prints the page state as it changes and the Diagnostics log at the end.
"""
import functools
import http.server
import os
import re
import subprocess
import sys
import threading
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2  # noqa: E402

SITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "site")
QRTX = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/qrtx")
RELAYDOTS = "--relaydots" in sys.argv
TIMEOUT = 90


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm"}

    def log_message(self, *args):
        pass


def serve():
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=SITE))
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{httpd.server_address[1]}/"


def start_device(name, site):
    env = {**os.environ, "QRTX_SITE": site, "QRTX_NAME": name}
    p = subprocess.Popen([QRTX, "pipe"], env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    p.stdin.close()
    buf = b""
    deadline = time.time() + 30
    while time.time() < deadline:
        buf += p.stderr.read1(4096)
        m = re.search(rb"(https?://\S+#Q1\S+)", buf)
        if m:
            threading.Thread(target=lambda: p.stderr.read(), daemon=True).start()
            return p, m.group(1).decode()
    raise SystemExit(f"{name} printed no code:\n{buf.decode(errors='replace')}")


def main():
    site = serve()
    a, url_a = start_device("wk-a", site)
    b, url_b = start_device("wk-b", site)
    page_url = url_a.replace(site, site + ("?relaydots" if RELAYDOTS else ""), 1)
    print(f"site {site}; opening the first code in WebKit {WebKit2.get_major_version()}.{WebKit2.get_minor_version()}{' (relaydots)' if RELAYDOTS else ''}")

    win = Gtk.Window()
    view = WebKit2.WebView()
    view.get_settings().set_enable_write_console_messages_to_stdout(False)
    win.add(view)
    win.set_default_size(420, 900)
    win.show_all()
    view.load_uri(page_url)

    started = time.time()
    state = {"added": False, "last": None, "result": 1}

    def js(script, then=None):
        def done(v, res):
            try:
                value = v.evaluate_javascript_finish(res)
                out = value.to_string() if value is not None else None
            except GLib.Error as e:
                out = f"<js error: {e.message}>"
            if then:
                then(out)
        view.evaluate_javascript(script, -1, None, None, None, done)

    def finish(code):
        state["result"] = code
        def show(log):
            print("\n===== Diagnostics log =====")
            print(log)
            Gtk.main_quit()
        js("document.querySelector('#diag-log').textContent", show)

    def poll():
        if time.time() - started > TIMEOUT:
            print("TIMEOUT")
            finish(1)
            return False
        if not state["added"] and time.time() - started > 3:
            state["added"] = True
            js(f"window.qrtx && window.qrtx.addTicket({url_b!r})")

        def got(s):
            if s and s != state["last"]:
                print(f"{time.time() - started:6.1f}s {s}")
                state["last"] = s
            if s and ('"phase":"done"' in s or '"phase":"failed"' in s):
                finish(0 if '"phase":"done"' in s else 1)
        js("JSON.stringify(window.qrtx ? {phase: qrtx.state.phase, slots: qrtx.state.slots.map(s => [s.status, s.error || ''])} : null)", got)
        return True

    GLib.timeout_add(1000, poll)
    Gtk.main()
    for p in (a, b):
        p.kill()
    sys.exit(state["result"])


main()

#!/usr/bin/env python3
"""Run the nsh-ext Linux acceptance on a real GTK 4 / WebKitGTK 6 surface.

The script owns the fixture and the resources around it:

* a local HTTP server on an ephemeral 127.0.0.1 port (never a fixed port)
  serves a fast page and a slowly streamed page;
* a second ephemeral socket is bound without listening, reserving an address
  that refuses connections instead of returning an HTTP error status;
* the pinned ``nsh`` binary is started in the example directory with
  ``nsh test . --target linux``, receiving both addresses through the
  app's granted environment variables;
* the child runs in its own process group, so a timeout or Ctrl-C takes its
  hosts and windows with it, and the HTTP server is shut down on every path.

``--target linux`` is always part of the invocation and the tests require
the app's own platform to be Linux, so a headless run fails instead of
passing quietly. The last test run saves a real native PNG under the
example's ignored ``.nsh/`` directory; the script verifies it exists and is
a PNG, then prints its path.

Needs a display: run it under ``xvfb-run -a`` in a session without one.

    xvfb-run -a python3 examples/nshm-linux/acceptance.py \\
        --nsh /path/to/pinned/nsh
"""

from __future__ import annotations

import argparse
import http.server
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
PNG = HERE / ".nsh" / "acceptance-loaded.png"

OK_PAGE = b"""<!doctype html>
<html><head><meta charset="utf-8"><title>ok</title></head>
<body><h1>nsh-ext WebView</h1><p>The fast page loaded.</p></body></html>
"""

# A known-length body, streamed in pieces: WebKit reports its own estimates
# while the document is still arriving.
SLOW_HEAD = (
    b"""<!doctype html>
<html><head><meta charset="utf-8"><title>slow</title></head>
<body><h1>slow page</h1>"""
)
SLOW_CHUNK = b"<!--" + b"x" * (32 * 1024 - 8) + b"-->\n"
SLOW_CHUNKS = 64
SLOW_DELAY = 0.03
SLOW_BODY = SLOW_HEAD + SLOW_CHUNK * SLOW_CHUNKS + b"</body></html>"


class Fixture(http.server.BaseHTTPRequestHandler):
    """The fast and slow pages; every other path is a 404."""

    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's name
        path = self.path.split("?", 1)[0]
        if path in ("/", "/ok"):
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(OK_PAGE)))
            self.end_headers()
            self._write(OK_PAGE)
            return
        if path == "/slow":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(SLOW_BODY)))
            self.end_headers()
            # Header first, then the body in chunks, so WebKit's estimate
            # changes while the navigation is still loading.
            if not self._write(SLOW_HEAD):
                return
            for _ in range(SLOW_CHUNKS):
                if not self._write(SLOW_CHUNK):
                    return
                time.sleep(SLOW_DELAY)
            self._write(b"</body></html>")
            return
        body = b"not found\n"
        self.send_response(404)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self._write(body)

    def _write(self, data: bytes) -> bool:
        try:
            self.wfile.write(data)
            self.wfile.flush()
            return True
        except (BrokenPipeError, ConnectionResetError, OSError):
            # The browser retired the view mid-load (a cancellation test).
            self.close_connection = True
            return False


def ephemeral_server() -> tuple[http.server.ThreadingHTTPServer, int]:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, name="fixture", daemon=True).start()
    return server, server.server_address[1]


def refused_socket() -> socket.socket:
    """Reserve a loopback port without listening, so no other process can take it."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", 0))
        return sock
    except OSError:
        sock.close()
        raise


def resolve_nsh(given: str) -> Path:
    if os.sep in given or (os.altsep and os.altsep in given):
        path = Path(given).expanduser()
    else:
        found = shutil.which(given)
        if found is None:
            raise SystemExit(f"acceptance: no nsh executable named {given!r} on PATH")
        path = Path(found)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise SystemExit(f"acceptance: {path} is not an executable file")
    return path


def run(nsh: Path, filter: str | None, trace: bool, timeout: float) -> int:
    if not (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")):
        raise SystemExit(
            "acceptance: no display (DISPLAY/WAYLAND_DISPLAY); run under `xvfb-run -a python3 ...`"
        )
    PNG.unlink(missing_ok=True)
    PNG.parent.mkdir(parents=True, exist_ok=True)

    server, port = ephemeral_server()
    child: subprocess.Popen[bytes] | None = None
    refusal: socket.socket | None = None
    try:
        base = f"http://127.0.0.1:{port}/"
        refusal = refused_socket()
        refused = f"http://127.0.0.1:{refusal.getsockname()[1]}/"
        env = os.environ.copy()
        env["NSH_EXT_FIXTURE_BASE"] = base
        env["NSH_EXT_FIXTURE_REFUSED"] = refused

        command = [str(nsh), "test", ".", "--target", "linux"]
        if filter:
            command += ["--filter", filter]
        if trace:
            command.append("--trace")
        print(f"acceptance: fixture {base} (refused {refused})", file=sys.stderr)
        print("acceptance: " + " ".join(command), file=sys.stderr)
        child = subprocess.Popen(
            command,
            cwd=HERE,
            env=env,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            code = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            print(f"acceptance: nsh test exceeded {timeout:g}s", file=sys.stderr)
            _stop(child)
            return 1
        except KeyboardInterrupt:
            _stop(child)
            print("acceptance: interrupted", file=sys.stderr)
            return 130

        if code != 0:
            print(f"acceptance: nsh test exited {code}", file=sys.stderr)
            return code

        if filter is not None:
            print("acceptance: filtered run; the full run captures the native screenshot")
            return 0
        if not PNG.is_file():
            print(f"acceptance: no native screenshot at {PNG}", file=sys.stderr)
            return 1
        with PNG.open("rb") as file:
            header = file.read(8)
        if header != b"\x89PNG\r\n\x1a\n" or PNG.stat().st_size <= 8:
            print(f"acceptance: {PNG} is not a non-empty PNG", file=sys.stderr)
            return 1
        print(f"acceptance: native screenshot {PNG} ({PNG.stat().st_size} bytes)")
        return 0
    finally:
        if child is not None and child.poll() is None:
            _stop(child)
        if refusal is not None:
            refusal.close()
        server.shutdown()
        server.server_close()


def _stop(child: subprocess.Popen[bytes]) -> None:
    """Ends the child and everything it started (its hosts and windows)."""
    for sig, wait in ((signal.SIGTERM, 5.0), (signal.SIGKILL, 5.0)):
        try:
            os.killpg(child.pid, sig)
        except ProcessLookupError:
            return
        try:
            child.wait(timeout=wait)
            return
        except subprocess.TimeoutExpired:
            continue


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--nsh",
        required=True,
        help="the pinned nsh binary that runs the acceptance (a path, or a name on PATH)",
    )
    parser.add_argument("--filter", help="run only tests whose name contains this")
    parser.add_argument("--trace", action="store_true", help="pass --trace to nsh test")
    parser.add_argument(
        "--timeout",
        type=float,
        default=900.0,
        help="how long nsh test may take, in seconds (default: 900)",
    )
    args = parser.parse_args()
    return run(resolve_nsh(args.nsh), args.filter, args.trace, args.timeout)


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Looks at the running screen through a real browser, and can talk to it.

`scripts/check-screen.mjs` runs the page against a DOM it implements itself,
which catches most things and cannot catch everything: it fires handlers
directly, so it never proves the wiring between a real click and the endpoint.
This drives Chrome instead — a real mouse press and release at the button's real
coordinates — and reports what the page then says.

Three defects were found this way and by nothing else: a message drawn as "#0"
because `Number(null)` is 0, every message drawn twice, and a belief panel
rendering four hundred evidence chips.

It needs a browser, which is not always available, and it needs that browser to
be listening:

  chromium --headless=new --remote-debugging-port=9223 \
      --user-data-dir=/tmp/velora-cdp --no-sandbox about:blank &

  python3 scripts/screen-in-browser.py                       # look
  python3 scripts/screen-in-browser.py --say "who are you?"   # and talk
  python3 scripts/screen-in-browser.py --close                # tidy up tabs

Nothing outside the standard library is used, so there is nothing to install.
"""

import base64
import json
import os
import socket
import struct
import sys
import time
import urllib.request

HOST, PORT = "127.0.0.1", 9223


# ----------------------------------------------------------------- WebSocket --


class WebSocket:
    """Just enough of RFC 6455 to talk to one debugging target."""

    def __init__(self, url):
        _, _, rest = url.partition("://")
        hostport, _, path = rest.partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=30)
        self.path = "/" + path

        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {self.path} HTTP/1.1\r\n"
            f"Host: {hostport}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())

        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk:
                raise RuntimeError("the debugging socket closed during handshake")
            head += chunk

        if b" 101 " not in head.split(b"\r\n")[0]:
            raise RuntimeError("the debugging socket refused the upgrade: " + head.decode(errors="replace"))

        self.next_id = 0

    def send(self, method, **params):
        self.next_id += 1
        message = json.dumps({"id": self.next_id, "method": method, "params": params})
        payload = message.encode()
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))

        length = len(payload)
        header = b"\x81"  # FIN + text
        if length < 126:
            header += struct.pack("!B", 0x80 | length)
        elif length < (1 << 16):
            header += struct.pack("!BH", 0x80 | 126, length)
        else:
            header += struct.pack("!BQ", 0x80 | 127, length)

        self.sock.sendall(header + mask + masked)
        return self.next_id

    def _read_exactly(self, count):
        got = b""
        while len(got) < count:
            chunk = self.sock.recv(count - len(got))
            if not chunk:
                raise RuntimeError("the debugging socket closed mid-message")
            got += chunk
        return got

    def _read_frame(self):
        first, second = self._read_exactly(2)
        fin = first & 0x80
        opcode = first & 0x0F
        length = second & 0x7F

        if length == 126:
            length = struct.unpack("!H", self._read_exactly(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exactly(8))[0]

        if second & 0x80:
            mask = self._read_exactly(4)
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(self._read_exactly(length)))
        else:
            body = self._read_exactly(length)

        return fin, opcode, body

    def receive(self):
        """One whole message, joining the fragments a big one arrives in."""
        body = b""
        while True:
            fin, opcode, part = self._read_frame()
            if opcode == 0x8:
                raise RuntimeError("the debugging socket was closed")
            if opcode in (0x9, 0xA):  # ping/pong
                continue
            body += part
            if fin:
                return body.decode(errors="replace")

    def wait_for(self, want_id, seconds=30, events=None):
        """Reads until the reply to `want_id`, collecting events on the way."""
        deadline = time.time() + seconds
        self.sock.settimeout(max(0.5, deadline - time.time()))

        while time.time() < deadline:
            try:
                message = json.loads(self.receive())
            except (socket.timeout, TimeoutError):
                break
            except RuntimeError:
                break

            if message.get("id") == want_id:
                return message
            if events is not None and "method" in message:
                events.append(message)

        return None

    def call(self, method, seconds=30, events=None, **params):
        reply = self.wait_for(self.send(method, **params), seconds, events)
        if reply is None:
            raise RuntimeError(f"{method} did not answer")
        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error']}")
        return reply.get("result", {})


# ---------------------------------------------------------------------- CDP --


def new_tab(url="about:blank"):
    request = urllib.request.Request(f"http://{HOST}:{PORT}/json/new?{url}", method="PUT")
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.load(response)["webSocketDebuggerUrl"]


def open_page(url, settle=5.0):
    """A connected page at `url`, having actually got there.

    `/json/new?<url>` is not reliable — it returns a target that is sometimes
    still `about:blank` when the caller starts asking it questions, which
    silently turns every probe into a probe of an empty page. Navigating
    explicitly and checking is the difference between testing the screen and
    testing nothing.
    """
    # `/json/new?<url>` often hands back a tab that is still `about:blank`, and
    # a probe of a blank page looks exactly like a page that renders nothing —
    # which is how a working screen gets reported as broken. So the tab is sent
    # there deliberately and checked, and it is reconnected to if the navigation
    # tears the session down, which it does.
    target = new_tab("about:blank")
    page = WebSocket(target)
    page.call("Runtime.enable")

    try:
        # Navigating destroys the execution context, so this call's own reply
        # never arrives. That is expected, and not a failure.
        evaluate(page, "location.href = " + json.dumps(url))
    except (RuntimeError, OSError):
        pass

    for attempt in range(40):
        time.sleep(0.5)

        try:
            if evaluate(page, "location.href") == url:
                break
        except (RuntimeError, OSError):
            # The session went with the context. Attach to the same tab again.
            try:
                page.sock.close()
            except OSError:
                pass
            page = WebSocket(target)
            page.call("Runtime.enable")
    else:
        raise RuntimeError(f"the tab never reached {url}")

    # It has arrived; now let it fetch its snapshot and build the screen.
    time.sleep(settle)
    return page


def close_all():
    """Closes every page the harness opened, so tabs do not accumulate."""
    with urllib.request.urlopen(f"http://{HOST}:{PORT}/json/list", timeout=10) as response:
        targets = json.load(response)

    for target in targets:
        if target.get("type") != "page" or target.get("url") == "about:blank":
            continue
        try:
            urllib.request.urlopen(
                f"http://{HOST}:{PORT}/json/close/{target['id']}", timeout=10
            ).read()
        except Exception:
            pass


def evaluate(socket_, expression):
    result = socket_.call(
        "Runtime.evaluate",
        expression=expression,
        returnByValue=True,
        awaitPromise=False,
    )
    return result.get("result", {}).get("value")


def click_at(page, element_id):
    """A real press and release, at the element's real coordinates."""
    box = evaluate(
        page,
        "(() => { const node = document.getElementById(" + json.dumps(element_id) + ");"
        " const rect = node.getBoundingClientRect();"
        " return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2,"
        "          width: rect.width, height: rect.height }; })()",
    )

    if not box or box["width"] == 0:
        raise RuntimeError(f"#{element_id} has no size, so it cannot be clicked")

    for kind in ("mousePressed", "mouseReleased"):
        page.call(
            "Input.dispatchMouseEvent",
            type=kind,
            x=box["x"],
            y=box["y"],
            button="left",
            buttons=1,
            clickCount=1,
        )


def say(page, text, wait=12.0):
    """Types into the box and presses send, the way a person would."""
    before = evaluate(page, "document.querySelectorAll('#chat-list > *').length")

    evaluate(
        page,
        "(() => { const box = document.getElementById('say');"
        " box.value = " + json.dumps(text) + ";"
        " box.dispatchEvent(new Event('input', { bubbles: true })); })()",
    )
    click_at(page, "send")
    time.sleep(wait)

    after = evaluate(page, "document.querySelectorAll('#chat-list > *').length")
    return before, after


def main():
    argv = sys.argv[1:]
    said = None
    if "--say" in argv:
        at = argv.index("--say")
        said = argv[at + 1]
        del argv[at:at + 2]

    url = argv[0] if argv else "http://127.0.0.1:7799/"
    out = argv[1] if len(argv) > 1 else "/tmp/velora-screen.png"

    events = []
    page = open_page(url)

    # The page fetches its snapshot asynchronously; `open_page` has already
    # waited, so what is read is a built screen rather than a half-built one.

    facts = {
        "url": evaluate(page, "location.href"),
        "title": evaluate(page, "document.title"),
        "timeline_rows": evaluate(page, "document.querySelectorAll('#timeline > *').length"),
        "timeline_first": evaluate(page, "document.querySelector('#timeline > *')?.textContent || ''"),
        "chat_bubbles": evaluate(page, "document.querySelectorAll('#chat-list > *').length"),
        "chat_text": evaluate(page, "document.querySelector('#chat-list')?.textContent || ''"),
        "belief_rows": evaluate(page, "document.querySelectorAll('#beliefs > *').length"),
        "beliefs_text": evaluate(page, "document.querySelector('#beliefs')?.textContent || ''"),
        "about_text": evaluate(page, "document.querySelector('#about')?.textContent || ''"),
        "connection": evaluate(page, "document.querySelector('#conn-text')?.textContent || ''"),
        "body_chars": evaluate(page, "document.body.textContent.length"),
        "stylesheet_rules": evaluate(
            page,
            "(() => { try { return document.styleSheets[0].cssRules.length } catch (e) { return -1 } })()",
        ),
        "empty_note": evaluate(page, "document.querySelector('#timeline-empty')?.hidden"),
    }

    # Extra expressions may be asked for on the command line.
    for expression in sys.argv[3:]:
        print(expression, "=>", json.dumps(evaluate(page, expression))[:600])

    if said is not None:
        before, after = say(page, said)
        print(f"\nsaid {said!r}: chat went from {before} to {after} bubbles")
        print(evaluate(page, "document.getElementById('chat-list').textContent")[-400:])

    shot = page.call("Page.captureScreenshot", format="png", captureBeyondViewport=False)
    with open(out, "wb") as handle:
        handle.write(base64.b64decode(shot["data"]))

    print(json.dumps(facts, indent=2)[:4000])

    troubles = [
        event["method"]
        for event in events
        if event["method"] in ("Runtime.exceptionThrown", "Log.entryAdded")
    ]
    if troubles:
        print("\nPROBLEMS:", troubles)
    else:
        print("\nno exceptions or console entries")

    print("\nwrote", out, os.path.getsize(out), "bytes")


def close_stale_pages():
    """Closes the tabs earlier runs left behind."""
    import urllib.request as request

    with request.urlopen(f"http://{HOST}:{PORT}/json/list", timeout=10) as response:
        targets = json.load(response)

    closed = 0
    for target in targets:
        if target.get("type") != "page":
            continue
        try:
            request.urlopen(f"http://{HOST}:{PORT}/json/close/{target['id']}", timeout=10).read()
            closed += 1
        except Exception:
            pass
    return closed




if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--close":
        print("closed", close_stale_pages(), "tabs")
    else:
        main()


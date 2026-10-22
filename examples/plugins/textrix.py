"""Write Textrix plugins in Python: standard library only, Python 3.8+.

    import textrix

    bot = textrix.Plugin("says pong to !ping")

    @bot.on_message
    def heard(msg):
        if msg["body"] == "!ping":
            bot.send(msg["room"]["id"], "pong", reply_to=msg["event_id"])

    bot.run()

Textrix runs the plugin as `[plugins.<name>]` in config.toml says, and talks
to it over stdin and stdout, one JSON-RPC message per line (PLUGINS.md,
github.com/seamoss/Textrix, has the details). Don't print to stdout yourself: stderr is the plugin's log.

Textrix keeps this file up to date in its plugins folder; edits to it are
overwritten.
"""

import json
import os
import select
import sys
import time
import traceback

__all__ = ["Plugin", "TextrixError"]

PROTOCOL = 1


class TextrixError(Exception):
    """Textrix said no: not a room this plugin may use, too fast, not sent…"""


class Plugin:
    def __init__(self, description=""):
        self.description = description
        #: `settings` from config.toml, once started.
        self.settings = {}
        #: The rooms it may use, once started: dicts with id, name, server…
        self.rooms = []
        self.name = None
        self._commands = {}
        self._handlers = {"message": [], "reaction": [], "member": []}
        self._started = []
        self._timers = []
        self._next_id = 1
        self._waiting = []
        self._buffer = b""

    # ---- what it does ----------------------------------------------------

    def command(self, name, args="", help=""):
        """`/name`: the function gets (args, room); room is None outside
        the rooms this plugin may use."""

        def add(fn):
            self._commands[name] = (args, help, fn)
            return fn

        return add

    def on_message(self, fn):
        """Messages from others in its rooms: dicts with room, event_id,
        sender, nick, body, highlight, ts, thread_root, reply_to."""
        self._handlers["message"].append(fn)
        return fn

    def on_reaction(self, fn):
        self._handlers["reaction"].append(fn)
        return fn

    def on_member(self, fn):
        """Someone joined (`joined` true) or left a room."""
        self._handlers["member"].append(fn)
        return fn

    def on_start(self, fn):
        """Once Textrix has started it (settings and rooms are known)."""
        self._started.append(fn)
        return fn

    def every(self, seconds):
        """Call the function every so many seconds."""

        def add(fn):
            self._timers.append([time.monotonic() + seconds, seconds, fn])
            return fn

        return add

    # ---- asking Textrix ----------------------------------------------------

    def send(self, room, text, reply_to=None, thread=None, notice=False, markdown=None):
        """Post in a room (its ID or name), as Markdown unless told not to.
        Returns the message's event ID once the server has it."""
        params = {"room": room, "text": text, "notice": notice}
        if reply_to:
            params["reply_to"] = reply_to
        if thread:
            params["thread"] = thread
        if markdown is not None:
            params["markdown"] = markdown
        return self.call("send", **params)["event_id"]

    def edit(self, room, event_id, text):
        self.call("edit", room=room, event_id=event_id, text=text)

    def react(self, room, event_id, key):
        self.call("react", room=room, event_id=event_id, key=key)

    def typing(self, room, typing=True):
        self.notify("typing", room=room, typing=typing)

    def status(self, text):
        """Its item on the status bar (None takes it away)."""
        self.notify("status", text=text)

    def log(self, text, error=False):
        """A line in Textrix's status window."""
        self.notify("log", text=text, level="error" if error else "info")

    def history(self, room, limit=20):
        """The room's latest messages, oldest first."""
        return self.call("history", room=room, limit=limit)["messages"]

    def call(self, method, **params):
        """Ask Textrix and wait for its answer."""
        id = self._next_id
        self._next_id += 1
        self._write({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        while True:
            message = self._read(None)
            if message is None:
                raise TextrixError("Textrix went away")
            if message.get("id") == id and "method" not in message:
                if "error" in message:
                    raise TextrixError(message["error"].get("message", "an error"))
                return message.get("result")
            self._waiting.append(message)

    def notify(self, method, **params):
        """Tell Textrix something that needs no answer."""
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    # ---- running ---------------------------------------------------------

    def run(self):
        """Answer Textrix until it says goodbye."""
        while True:
            message = self._waiting.pop(0) if self._waiting else self._read(self._until_timer())
            if message is None:
                return
            if message != "tick":
                self._handle(message)
            self._run_timers()

    def _handle(self, message):
        method = message.get("method")
        params = message.get("params") or {}
        if method == "initialize":
            self.settings = params.get("settings") or {}
            self.rooms = params.get("rooms") or []
            self.name = params.get("name")
            commands = [{"name": n, "args": a, "help": h} for n, (a, h, _) in self._commands.items()]
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": {"description": self.description, "commands": commands}})
            for fn in self._started:
                self._safely(fn)
        elif method == "shutdown":
            sys.exit(0)
        elif method == "command":
            entry = self._commands.get(params.get("name"))
            if entry:
                self._safely(entry[2], params.get("args", ""), params.get("room"))
        elif method in self._handlers:
            for fn in self._handlers[method]:
                self._safely(fn, params)
        elif "id" in message and method:
            self._write({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "no method " + method}})

    def _safely(self, fn, *args):
        try:
            fn(*args)
        except TextrixError as e:
            self.log(f"{fn.__name__}: {e}", error=True)
        except Exception:
            traceback.print_exc(file=sys.stderr)
            self.log(f"{fn.__name__} failed; see the plugin's log", error=True)

    def _until_timer(self):
        if not self._timers:
            return None
        return max(0.0, min(t[0] for t in self._timers) - time.monotonic())

    def _run_timers(self):
        now = time.monotonic()
        for timer in self._timers:
            if timer[0] <= now:
                timer[0] = now + timer[1]
                self._safely(timer[2])

    def _write(self, message):
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    def _read(self, timeout):
        """The next message; "tick" when the timeout passes; None at the end."""
        fd = sys.stdin.fileno()
        deadline = None if timeout is None else time.monotonic() + timeout
        while b"\n" not in self._buffer:
            left = None if deadline is None else max(0.0, deadline - time.monotonic())
            ready, _, _ = select.select([fd], [], [], left)
            if not ready:
                return "tick"
            chunk = os.read(fd, 65536)
            if not chunk:
                return None
            self._buffer += chunk
        line, self._buffer = self._buffer.split(b"\n", 1)
        try:
            return json.loads(line)
        except ValueError:
            return self._read(timeout)

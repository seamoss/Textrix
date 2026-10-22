# Textrix plugins

A plugin is a program Textrix starts and talks to: it hears what happens in
the rooms you grant it, adds `/commands`, and can post, reply, react, edit
and show a status-bar item there. It can be written in anything that reads
and writes lines on stdin and stdout; Python is the easy way, with the
helper Textrix provides.

Plugins are the trusted half of Textrix's extensions. Scripts (Rhai, in
`~/.textrix/scripts`) are sandboxed: no programs, no network. A plugin is a
program, so it can do anything a program you run can do. What it can do
**in Matrix** is limited to the rooms config.toml grants it, and it posts
as you. Only run plugins you trust.

## Start one

In a room, `/plugin new greeter` writes `~/.textrix/plugins/greeter.py` (a
starter that says hello and answers `!ping`), adds this to config.toml,
granted the room you're in, and starts it:

```toml
[plugins.greeter]
command = "python3 greeter.py"   # run with sh -c, in ~/.textrix/plugins
rooms = ["Team Tests"]           # names, aliases or IDs; "*" for every room
# enabled = false                # don't start it with Textrix
# [plugins.greeter.settings]     # anything; handed to it when it starts
# repo = "~/code/thing"
```

Edit the file, then `/plugin restart greeter`. `/reload` picks up changes
to config.toml (new plugins start, removed ones stop, changed ones restart).

| Command | |
|---|---|
| `/plugin` | the plugins, whether they're running, their rooms and commands |
| `/plugin new <name>` | a starter plugin, as above |
| `/plugin start` / `stop` / `restart <name>` | |
| `/plugin log <name>` | the end of its log (`~/.textrix/plugins/<name>.log`: whatever it prints to stderr) |

Scripts can't run `/plugin`.

## In Python

`textrix.py` sits in the plugins folder (Textrix keeps it up to date), so a
plugin there can `import textrix`:

```python
import textrix

bot = textrix.Plugin("says pong to !ping")

@bot.on_message
def heard(msg):
    if msg["body"] == "!ping":
        bot.send(msg["room"]["id"], "pong", reply_to=msg["event_id"])

@bot.command("roll", args="[sides]", help="Roll a die")
def roll(args, room):
    import random
    if room:
        bot.send(room["id"], f"🎲 {random.randint(1, int(args or 6))}")

@bot.every(3600)
def hourly():
    bot.status(f"{len(bot.rooms)} rooms")

bot.run()
```

| | |
|---|---|
| `@bot.on_message`, `@bot.on_reaction`, `@bot.on_member` | what others do in its rooms (dicts, as below) |
| `@bot.command(name, args="", help="")` | `/name`: called with (args, room); room is `None` outside its rooms |
| `@bot.on_start`, `@bot.every(seconds)` | |
| `bot.send(room, text, reply_to=None, thread=None, notice=False, markdown=None)` | returns the event ID once the server has it |
| `bot.edit(room, event_id, text)`, `bot.react(room, event_id, key)` | |
| `bot.typing(room, typing=True)`, `bot.status(text)`, `bot.log(text, error=False)` | |
| `bot.history(room, limit=20)` | the room's latest messages, oldest first |
| `bot.settings`, `bot.rooms`, `bot.name` | known once it's started |

Errors from Textrix (not a room it may use, too fast, couldn't send) raise
`textrix.TextrixError`; in a handler, they're logged to the status window.

## The protocol

JSON-RPC 2.0, one message per line, UTF-8: Textrix writes to the plugin's
stdin, the plugin writes to stdout. Its stderr goes to its log. Its working
directory is the plugins folder, and `TEXTRIX_PLUGIN` (its name) and
`TEXTRIX_PROTOCOL` (`1`) are in its environment.

### Starting and stopping

Textrix's first message is a request, `initialize` (id 0):

```json
{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
  "protocol": 1, "textrix": "0.17.0", "name": "greeter",
  "settings": {"repo": "~/code/thing"},
  "rooms": [{"id": "!abc:example.org", "name": "Team Tests", "server": "home", "dm": false, "encrypted": true}]}}
```

The plugin answers with what it is and the commands it adds (names that
clash with Textrix's own or another plugin's are refused):

```json
{"jsonrpc": "2.0", "id": 0, "result": {"description": "says hello",
  "commands": [{"name": "greeter", "args": "[name]", "help": "Say hello"}]}}
```

Until it answers, it hears nothing. To stop it, Textrix sends `shutdown`
(a notification), and ends the program half a second later.

### What Textrix tells a plugin

Notifications, for what other people do in its rooms (never your own
messages, so a plugin can't answer itself):

- `message`: `room` (as in `initialize`), `event_id`, `sender`, `nick`,
  `body`, `own`, `highlight` (it mentions you), `ts` (milliseconds),
  `thread_root`, `reply_to`.
- `reaction`: `room`, `nick`, `key`, `target` (the event), `text` (what was
  reacted to).
- `member`: `room`, `user`, `nick`, `joined` (true, or false for leaving).
- `command`: `name`, `args`, `room` (`null` outside its rooms).

### What a plugin can ask

Requests (with an `id`, answered) or notifications (without, not
answered). `room` is a room ID or name, and must be one it's granted.

| Method | Params | Result |
|---|---|---|
| `send` | `room`, `text`, `notice`, `markdown`, `reply_to`, `thread` (a thread's root) | `{"event_id": …}` once the server has it |
| `edit` | `room`, `event_id` (one of your messages), `text` | `null` |
| `react` | `room`, `event_id`, `key` | `null` |
| `typing` | `room`, `typing` | `null` |
| `status` | `text` (empty or null: none) | `null` |
| `log` | `text`, `level` (`"error"`) | `null` |
| `rooms` | | `{"rooms": [...]}` |
| `history` | `room`, `limit` (≤100) | `{"messages": [...]}`, oldest first, as in `message` |

Sends, edits and reactions are limited to 20 every 10 seconds. Errors:

| Code | |
|---|---|
| -32601 | no such method |
| -32602 | missing or bad params |
| -32000 | not a room it may use, or not a message it can touch |
| -32001 | too fast |
| -32002 | couldn't be sent (the server refused, or isn't connected) |

Messages it sends go out like yours: they show at once in the room (as
sending), wait in the send queue when the server can't be reached, and are
end-to-end encrypted in encrypted rooms.

## Coming

Plugins speaking as their own Matrix account (a bot, rather than you), and
on that, a plugin that takes coding requests in a room to a coding agent
and opens pull requests.

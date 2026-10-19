# Scripting

Textrix runs scripts the way irssi does: drop a file in
`~/.textrix/scripts/` and it can add commands and react to messages.
Scripts are written in [Rhai](https://rhai.rs/book/), a small language
that looks like a mix of Rust and JavaScript.

```rhai
// ~/.textrix/scripts/shout.rhai
command("shout", "Say it in capitals", |args, ctx| {
    say(args.to_upper() + "!");
});
```

Now `/shout hello` sends "HELLO!" to the room you're in.

Every `.rhai` file in the scripts directory loads at startup
(`scripts = false` in `config.toml` turns that off). There are working
examples in [`examples/scripts`](examples/scripts): copy one into
`~/.textrix/scripts/` and `/script load <name>`.

## Managing scripts

| Command | |
|---|---|
| `/script` | Lists the loaded scripts, with their commands and hooks |
| `/script load <name>` | Loads `~/.textrix/scripts/<name>.rhai` (the name without `.rhai`), or a path: anything with a `/` or ending in `.rhai`, like `./seen.rhai` |
| `/script reload [name]` | Reads one script's file again (or every file in the directory), replacing what was loaded |
| `/script unload <name>` | Takes a script out |
| `/script exec <code>` | Runs a line of Rhai once, e.g. `/script exec 6 * 7`. Stores, settings, status items and timers belong to a script's file, so they're for loaded scripts only |

A script that fails to load, or whose handler fails, says why in the
status window; the rest keep working.

## What a script can do

A script's top level runs when it loads. That's where it registers
handlers, and what it prints there shows in the status window.

| Function | |
|---|---|
| `command(name, help, \|args, ctx\| …)` | Adds `/name`. `args` is the text after the command; `ctx` is the room it was typed in. The help shows in `/help` and under the input line as you type. Built-in commands and your aliases come first, so a script can't replace them |
| `on_message(\|msg\| …)` | Called for each new message from someone else, in every room |
| `on_highlight(\|msg\| …)` | Called for each message that mentions you or a `/hilight` word |
| `on_join(\|m\| …)`, `on_leave(\|m\| …)` | Called when someone joins or leaves a room: `m.nick`, `m.user` (their Matrix ID) and the `ctx` fields |
| `on_reaction(\|r\| …)` | Called when someone reacts: `r.nick`, `r.key` (the emoji), `r.target` (the message's ID), `r.text` (its text, if it's loaded) and the `ctx` fields |
| `every(secs, \|\| …)`, `after(secs, \|\| …)` | Calls the function every `secs` seconds, or once after them (between a second and a week; fractions are fine; up to 32 timers a script). Returns an ID |
| `cancel(id)` | Stops a timer; `true` if it was running |
| `print(text)` | A line only you see, in the room the script is working in |
| `say(text)` | Sends a message to that room (Markdown, like typing it) |
| `send(room, text)` | Sends to another room: by its whole name as the window list shows it, or an `#alias:server`, `!id:server` or `@user:server` (their DM, started if you haven't one). Those go out from the account of the room the script is working in |
| `run(command)` | Runs a Textrix command, e.g. `run("/markread all")` |
| `bell()` | Rings the bell, following your `bell` settings. It rings as a mention would, so not if `bell_for` leaves out `"mentions"` |
| `notify(title, body)` | A desktop notification, following your `notifications` setting |
| `matches(text, pattern)` | Whether `text` matches a [regular expression](https://docs.rs/regex/latest/regex/#syntax) |
| `random(n)` | A number from 0 to n − 1 |
| `now()` | The time, in seconds since 1970 (to store, or subtract) |
| `status(text)` | Sets this script's item in the status bar; `status("")` takes it away |
| `status(\|\| …)` | A status bar item worked out once a second: the function returns its text |
| `setting(name, default, help)` | Declares a setting and returns its value (see [Settings](#settings)) |
| `setting(name)` | A setting's value now |
| `store_set(key, value)`, `store_get(key)`, `store_remove(key)`, `store_keys()` | This script's store, kept across restarts (see [Keeping things](#keeping-things)); `store_get` of a missing key is `()` |

"The room the script is working in" is the room a command was typed in, or
the one a message arrived in.

### `ctx`

| Field | |
|---|---|
| `ctx.room` | The window's name, as the window list shows it |
| `ctx.room_id` | The room's ID, e.g. `!abc:example.org` (empty in the status window) |
| `ctx.server` | Which of your servers, by its name in the dotfile |

### `msg`

A message has the `ctx` fields too, plus:

| Field | |
|---|---|
| `msg.body` | Its text (a caption or file name for attachments) |
| `msg.nick` | Who sent it, as Textrix shows them (your `/nickname` for them, if you gave one) |
| `msg.sender` | Their Matrix ID, e.g. `@sam:example.org` |
| `msg.highlight` | Whether it mentions you or one of your `/hilight` words |
| `msg.dm` | Whether it's in a direct message |

### Keeping things

Variables that a handler uses are kept as long as the script is loaded, so
a script can remember things between messages. Reloading it starts over.

```rhai
let count = 0;

on_message(|msg| { count += 1; });

command("count", "Messages since this script loaded", |args, ctx| {
    print(`${count} messages so far`);
});
```

To remember things across restarts, use the script's store: text,
numbers, true/false, and arrays and maps of them.

```rhai
command("note", "Keep a note: /note <text>", |args, ctx| {
    let notes = store_get("notes") ?? [];
    notes.push(args);
    store_set("notes", notes);
});
```

Each script's store is a JSON file in `~/.textrix/script-data/`, saved a
second after it changes and readable only by you, up to 1 MB. It isn't
encrypted like Textrix's own store, so think twice before keeping what
people said in one.

### Settings

A script can have settings, which `/set` lists with the rest as
`script.name`, changes for the session (`/set watchwords.words lunch`), and
`/save` writes to `config.toml`:

```toml
[script_settings.watchwords]
words = "deploy|outage|lunch"
```

```rhai
setting("words", "deploy|outage", "Words to watch for, separated by |");

on_message(|msg| {
    if matches(msg.body, "(?i)\\b(" + setting("words") + ")\\b") { bell(); }
});
```

The default decides the kind of value (text, a whole number, a decimal
number, or true/false); a value of the wrong kind in `config.toml` is
ignored, and `/set` refuses one. Read the setting with `setting(name)` when
you use it, rather than once at the top, so `/set` changes take effect
straight away.

### The status bar

`status("text")` puts an item in the status bar, after the others, until
the script changes it, clears it with `status("")`, or unloads. Given a
function, Textrix asks it once a second:

```rhai
let started = now();
status(|| `up ${(now() - started) / 60}m`);
```

A function that fails loses its item (and says why, once).

### Timers

A timer's function runs on Textrix's once-a-second clock, so a timer is
accurate to about a second. What it prints shows in the window you're in.
Reloading a script replaces its timers; unloading stops them.

```rhai
// The time in Tokyo can't be had, but how long you've been up can.
let started = now();
every(3600, || print(`${(now() - started) / 3600} hours in`));
```

## Limits

Scripts run inside Textrix, so they're kept on a short leash:

- They can't read or write files, open network connections, or run
  programs. `eval` is switched off.
- A handler that runs too long (more than 500,000 operations: an endless
  loop, say) is stopped, with an error in the status window. Strings are
  kept under 64 KB, arrays and maps under 10,000 items, and calls 48
  deep.
- Each script can send at most 5 things (`say`, `send`, `run`) in any 10
  seconds; anything beyond that is dropped, and Textrix says so.
- Scripts don't hear your own messages or reactions, so a script that
  answers them can't end up answering itself. Scripts and aliases calling each other
  stop after 8 levels.

They are still your code, running as you. `run` can do anything you can
type, so be careful with what you build from other people's messages:
`run("/join " + msg.body)` lets anyone in the room choose where you go.

## Rhai in brief

The [Rhai book](https://rhai.rs/book/) has everything. Things that come up
in scripts:

- Strings: `` `${name} said ${text}` `` fills in values;
  `text.to_upper()`, `text.to_lower()`, `text.contains("x")`,
  `text.starts_with("x")`, `text.split(" ")`, `text.len()`.
  `text.trim()` trims the string in place rather than returning a new one.
- Numbers: `parse_int("42")`, `n.to_string()`.
- Maps: `let m = #{}; m["key"] = value; if "key" in m { … }`.
- Arrays: `let a = []; a.push(x); for x in a { … }`.
- Time: `let t = timestamp();` then `t.elapsed` is the seconds since.
- Handlers can `return` early.

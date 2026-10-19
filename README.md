# Textrix

An irssi-style terminal client for Matrix, in Rust (matrix-rust-sdk + ratatui).
Numbered windows, a status bar with activity, slash commands, nick completion,
and end-to-end encryption.

## Install

```sh
brew install seamoss/textrix/textrix      # macOS, Linux (x86_64, ARM); with completions and a man page
cargo install --git https://github.com/seamoss/Textrix   # anywhere with Rust 1.96+
```

Or download a build for macOS (universal, or per chip) or Linux (x86_64,
ARM) from the [releases](https://github.com/seamoss/Textrix/releases).
Then make your dotfile (below) and run `textrix`; `textrix --check` tells
you whether everything's in place.

```text
textrix [--env FILE] [--home DIR] [--theme NAME]
textrix --check | --completions SHELL | --man | --version
```

`--check` reads the dotfile (not its secrets), config.toml and the
keychain and reports, without connecting. `--theme` picks colours for one
run. `--env` and `--home` work like `TEXTRIX_ENV` and `TEXTRIX_HOME`.

## Setup

Connections come only from a private dotfile. No file, no connections.

| Build              | Dotfile                         |
|--------------------|---------------------------------|
| `cargo run` (debug)| `.textrix` in the repo (git-ignored) |
| release binary     | `~/.textrix/.env`               |
| either             | `TEXTRIX_ENV=/path/to/file`     |

```sh
cp .env.example .textrix && chmod 600 .textrix   # then fill it in
cargo run
```

Each server is a `TEXTRIX_<NAME>_*` group (`HOMESERVER`, `USER`, `PASSWORD`,
`RECOVERY_KEY`, `KEYS_PASSPHRASE`); see `.env.example`. Every server in the file connects on
startup. Secrets are read only when the API call needs them and are wiped
afterwards. They are never logged, shown, or typed into the client, and the
file is refused while other users can read it. Real environment variables
override the file, so `op run --env-file … -- textrix` works with
`op://` references.

State lives in `~/.textrix/` (`TEXTRIX_HOME` overrides it): each server's
encrypted store under `accounts/<name>/`, window order in `layout.json`,
logs in `textrix.log` (`TEXTRIX_LOG=debug` for more).

### The local store is encrypted

Each server's store (encryption keys, room state, the history cache) is
encrypted at rest with a random key that Textrix keeps, together with the
login session, in the system keychain: the macOS Keychain, or the Secret
Service via `secret-tool` on Linux. Nothing that could read your messages
sits on disk in the clear, and `/logout` removes the keychain item too.

A store from an older Textrix (`store/` and `session.json`) is converted
once at startup: Textrix exports its room keys, signs in as a new session
with the dotfile password, imports the keys, signs the old session out and
deletes the old files; the recovery key verifies the new session as usual.
If anything fails before the new session exists, it keeps using the old
store and tries again next time. Without a keychain the store stays
unencrypted (Textrix says so); `encrypt_store = false` keeps new stores
unencrypted too.

## Encryption

- Rooms are encrypted and decrypted transparently. Keys live in the local
  SQLite crypto store.
- `RECOVERY_KEY` is applied after the first sync (or run `/recover`). It
  verifies this device via cross-signing and downloads your key backup, so
  older history decrypts.
- `/verify` asks your other sessions to verify this one. `/verify accept`
  answers a request from Element, then use `/verify confirm` or
  `/verify mismatch` after comparing the emoji.
- The status bar shows `<server>: unverified` until this device is trusted.
- `/whois <user>` shows identity and per-device trust.
- `/keys export [file]` saves every room key this device holds (to
  Downloads by default) and `/keys import <file>` loads keys, in Element's
  format, so files move both ways. The passphrase comes from
  `TEXTRIX_<NAME>_KEYS_PASSPHRASE` in the dotfile, never typed; exports are
  new files only you can read and never overwrite anything.
- `/devices` lists your sessions (most recently active first, verified or
  not, where they were last seen). `/devices rename <id> <name>` renames
  one; `/devices delete <id>` shows which session it is, and
  `/devices delete <id> confirm` signs it out. The server asks for your
  password again for that, which Textrix answers from the dotfile, like at
  login. To sign out this session, use `/logout`.

## Keys

| Key | Action |
|---|---|
| Alt+1…0, Alt+Q…P | window 1–20 |
| Ctrl+N / Ctrl+P, Alt+←/→ | next / previous window |
| Alt+A | jump to the most important active window |
| Ctrl+K, Alt+K | **quick switcher**: type part of a room's name (letters in order, like `tt` for Team Tests) or its number; ↑/↓ choose, Enter goes there. Closed windows are included. With nothing typed it lists windows with activity first, then where you were most recently, so Ctrl+K Enter flips back to the last window. Ctrl+K still cuts to the end of the line when there's text after the cursor |
| PgUp / PgDn | scroll (older history loads at the top) |
| Tab | complete commands, nicks (adds a mention), `:emoji` shortcodes, server names, file paths |
| Up / Down | input history |
| Alt+Up | **select** a message: ↑/↓ PgUp/PgDn g/G move; `r` reply, `e` edit, `d` delete, `+` react, `t` thread, `f` forward, `p` pin/unpin, `z` view an image full screen or play audio or video, `s` save attachment, `o` open link, `v` spoilers, `c` copy, `i` details, Esc leaves |
| Shift+Up, or Ctrl+E on an empty line | **edit** your messages: ↑/↓ pick one (it loads into the input line), Enter saves, clear + Enter deletes, Esc restores your draft |
| Shift+Enter (Alt+Enter in Terminal.app/tmux) | new line in the message |
| F1 / F2 / F3 | full-screen help (type to filter) / window list / member list |
| F6 / Shift+F6 | focus the next / previous split pane |
| Ctrl+A/E/B/F/U/K/W/Y/D | emacs-style editing; Ctrl+C clears the line |
| Ctrl+L | redraw |

**Mouse** (`mouse = true`): the wheel scrolls the pane under the pointer
(older history loads at the top) or, in selection mode, moves the
selection. Click a window in the list to go there, a pane to focus it, a
message to select it (click again to unselect), a link to open it, a name
in the member list to mention them, or the input line to move the cursor.
While the mouse is on, hold Shift (Option in iTerm2 and Terminal.app) and
drag to select text, or use `c` in selection mode to copy a message.

The terminal's tab or window title follows the window you're in (the
focused pane when split), as set by `title`: `{window}`, `{number}`,
`{server}`, and `{activity}`, which adds ` [3]` while three other windows
have new messages. Inside tmux it's the pane title. The title you had is
put back when Textrix exits.

While you type a command, its usage appears in dim text after the cursor.

What you've typed stays with its window: switch away and it's waiting when
you come back (`…` in the window list marks windows with unsent text). A
paste of more than 5 lines asks `y/n` before it's sent, so a stray
clipboard can't end up in a room; `n` leaves it in the input line to edit
(`paste_confirm_lines`, 0 turns it off). Lines you type with Shift+Enter
never ask.

On macOS, set your terminal's Option key to act as Meta/Esc+ for the Alt
bindings.

## Commands

`/help` lists everything; `/help <command>` shows usage.

| Area | Commands |
|---|---|
| Rooms | `/create [-public] [-space] <name>`, `/join`, `/name`, `/topic`, `/info`, `/rooms`, `/invites`, `/decline`, `/part`, `/forget`, `/invite`, `/kick`, `/ban`, `/unban`, `/names` |
| Room settings | `/joinrule <invite\|public\|knock\|restricted <space>>`, `/guests <on\|off>`, `/history <joined\|invited\|shared\|world>`, `/publish <on\|off>` (room directory), `/upgrade [version] [confirm]` (shows versions first; confirming moves you to the new room); `/info` shows them all |
| Directory | `/list [search]` (public rooms) |
| Spaces | `/spaces`, `/space <name>` (browse, including rooms you haven't joined), `/space <name> add\|remove` (the current room), `/space <name> leave`, `/create -space <name>`; the window list groups rooms under their spaces without renumbering them |
| Links | `/urls` (numbered, newest first), `/open [N]`; `o` in selection mode. The page title of a message's first link shows under it (`↳ Title — Site`), fetched by your homeserver's URL preview so the site never sees you. That's on in unencrypted rooms, which the server reads anyway; `link_titles = "all"` adds encrypted rooms (the server then sees those links), `"off"` stops it. Synapse needs `url_preview_enabled` for this; Textrix tells you once if it's off |
| Forwarding | `f` in selection mode opens a "Forward to" list of that server's other rooms (closed ones too), filtered as you type like the switcher; Enter sends a copy and leaves you where you are. `/forward [room]` does the same for the latest message here. The copy keeps formatting and attachments but not reply or thread links or mentions, so nobody is pinged again; an edited message goes as its edited text. Forwarding out of an encrypted room into one that isn't says so |
| Pins | `p` in selection mode pins a message for everyone in the room, or unpins it (needs the power level to change pins). Pinned messages show `[pinned]`, and the topic bar counts them. `/pins` lists them, newest pin first; `/jump N` goes to one |
| Threads | `/threads` (the room's threads), `/thread [N]`, or `t` on a message in selection mode (starts a thread if it has none) opens a thread window: root first, then replies; typing there replies in the thread. Thread roots show `[N replies]`; replies also appear in the room with their context |
| Search | `/search <text>` across every room, newest first, `/jump N` to land on a result in selection mode. It searches a local index of decrypted messages (servers can't search encrypted rooms): new messages go in as they arrive, and older history is read back gently in the background, up to 20,000 messages a room. The index is encrypted with the same keychain key as the store, follows edits and deletions, and is removed by `/logout` or `search_index = false`. Without an encrypted store there's no index, and `/search` covers loaded history; `/search -deep <text>` reads each room's last 500 messages |
| Messages | plain text (Markdown), `/me`, `/notice`, `/msg`, `/query`, `/reply [nick] <text>`, `/react [nick] <emoji>`, `/edit [text]`, `/redact`, `/lastlog [-n N] [-nick nick] <text>` |
| Files | Drop files on the terminal (any kind, one or several) and Textrix asks "upload notes.pdf (1.2 MB) to Night Owls? y/n"; `n` leaves the paths in the input line as text. `/upload <path>` (Tab completes paths; dropped/escaped paths work), `/download [nick]` (saves to Downloads, never overwrites) |
| Windows | `/window <n\|list\|close\|move n\|goto name>`, `/wc`, `/wl`, `/clear`, `/sidebar`, `/nicklist`, `/theme [name]` |
| Reading | `/markread` marks this room read, `/markread all` every room: clears activity and unread counts, moves the new-messages divider to the end, and updates your read marker (and receipt, per `read_receipts`) so other clients agree |
| Aliases | `/alias tt /window goto Team Tests` makes `/tt`. irssi-style: `$0`, `$1`… are its arguments, `$1-` the first onwards, `$*` all of them (with none of these, arguments go on the end), and `;` runs several commands (`/alias rr /markread all; /window 1`). Aliases are saved in `config.toml` under `[aliases]`, the rest of the file left as it is; `/alias` lists them, `/unalias <name>` removes one. They can't replace built-in commands |
| Your names | `/window name Ops` shows this window as "Ops" everywhere in Textrix (window list, status bar, pane titles, switcher, the terminal title) without renaming the room for anyone else; the room's own name still finds it in the switcher and `/window goto`. `/window name` on its own puts it back. Names are kept in `layout.json` |
| Nicknames | `/nickname sam Sammy` shows that person as "Sammy" everywhere in Textrix: on their messages and replies to them, read markers, typing notices, the member list, notifications, and their DM window. Only you see it; Tab finds them by either name but inserts their own, so a nickname never ends up in a message. `/nickname sam` takes it off, `/nickname` lists them. Kept in `layout.json` |
| Split panes | `/split <window>` (below) or `/vsplit <window>` (beside) shows another window at the same time, up to 4; each pane has its own title bar, and `▶` marks the one you're typing in. Going to a window that's already in a pane focuses it; any other window replaces the focused pane's. `/unsplit` closes the focused pane (the window stays open), `/unsplit all` keeps only it. The split you quit with is back next time. `/layout save <name>` keeps the panes (and whether the window and member lists show) to bring back with `/layout load <name>` or `/layout <name>`; `/layout` lists them, `/layout delete <name>` removes one |
| Account | `/servers`, `/connect`, `/disconnect`, `/logout`, `/nick`, `/away [-all] [message]`, `/ignore`, `/unignore`, `/whois`, `/verify`, `/recover`, `/avatar <image>\|-remove`, `/roomavatar <image>\|-remove`, `/version [check]`, `/devices [rename <id> <name> \| delete <id> [confirm]]`, `/keys export [file] \| import <file>`, `/quit` |
| Scripts | `/script` lists them, `/script load\|unload\|reload <name>`, `/script exec <code>`; see [Scripts](#scripts) |
| Notifications | `/notify [all\|mentions\|mute\|default]`, `/hilight [word]`, `/dehilight <word>`, `/bell [on\|off\|away\|sound\|visual\|both\|test]` (see below) |

Editing follows Matrix's rules: only your own text messages, and only where
your power level lets you post. Deleting someone else's message needs
moderator rights; Textrix checks before asking you to confirm.

When several servers are connected, prefix a command with `-<server>` while in
the status window (e.g. `/join -work #ops:work.example`).

### Joins and leaves

Runs of two or more joins, leaves and name changes (on the same day) show
as one line, like `alice, bob and 5 others joined · dave left · eve →
erin`. Kicks, bans, invitations and requests to join always get their own
line. `fold_joins = false` shows every change; `show_joins = false` hides
them all.

### Formatting

Formatted messages render as styled text: bold, italic, strikethrough,
inline code and code blocks, quotes (`▎`), bullet and numbered lists,
headings, coloured text, links (with the address when the text differs),
mention pills, and spoilers (hidden until you press `v` in selection mode).
Code blocks are syntax-coloured when they say their language (```` ```rust ````,
or a `#!` line), in colours that follow the theme: a monochrome theme
uses shades of its one colour. What you send is Markdown (`markdown = true`) and shows formatted right away.
Emoji shortcodes (`:fire:`, `:+1:`, GitHub/Slack names) turn into emoji when
you send, react or edit, except inside backticks; `:thu` + Tab completes
them, showing which shortcode you're on as you cycle.

### Chat logs

Off by default: logs hold decrypted messages in plaintext. `log = true` (or
`/log on` for just this session) writes irssi-style daily files to
`~/.textrix/logs/<server>/<room>/<YYYY-MM-DD>.log`, readable only by you,
covering what arrives while you're connected: messages, emotes, notices,
joins and parts, edits, deletions and thread replies.

### Sync

New sessions use Simplified Sliding Sync (MSC4186) when the server offers it,
which loads big accounts faster; otherwise Textrix long-polls the classic
`/sync`. A session keeps the method it started with: Synapse can't carry a
classic session's encryption stream position over to sliding sync, so an
existing session stays on classic sync (to switch, `/logout` and reconnect).
Sliding sync has no presence, so presence dots there only update via
`/whois`. Force a method with `sync = "classic"` or `"sliding"`.

When the connection drops, it's a netsplit: `Netsplit 0x4f02.com ⇄ you,
quits: …`, then `Netsplit over after 2m 13s, joins: …` once it's back.

### History at startup

Messages that arrive while Textrix runs are kept in the account's local
store (the SDK's event cache), so at the next launch each window shows its
recent messages straight away, before the server has answered. If more
arrived while Textrix was closed than the first sync delivers, that room's
cached lines are dropped and its history loads from the server as usual,
so you never see old and new messages joined across a gap. Older history
still loads from the server when you scroll up.

The cache lives in the encrypted store; `/logout` deletes it, and
`history_cache = false` turns it off and clears it.

### Images

The window list shows each room's avatar (a DM falls back to the other
person's) as a tiny image in terminals with graphics, and a coloured initial
tile elsewhere.

Images show inline under their message. Textrix asks the terminal what it
supports at startup: kitty graphics (kitty, Ghostty, WezTerm), iTerm2 inline
images, or sixel, falling back to coloured half-block characters elsewhere
(e.g. Terminal.app). Encrypted images use the sender's thumbnail when there
is one; otherwise the image itself is fetched (up to 20 MB) and decoded with
size and memory limits. Inside tmux, graphics need
`set -g allow-passthrough on`. `s` in selection mode or `/download` saves the
original.

`z` on an image in selection mode, or a click on it, opens it full screen:
the original is fetched (up to 50 MB) and decoded in memory only, never
written to disk. `+`/`-` or the mouse wheel zoom (up to 16×), the arrow
keys (or `hjkl`) move around, `0` fits it again, and Esc closes. With
kitty, iTerm2 or sixel graphics it's sharp; with half-blocks, zooming in is
how you see the detail.

### Voice messages and video

Voice messages show their length and waveform, `[voice] 0:12 ▁▃▅▇▅▃▁`, and
other audio its length. `z` (or Space) on one in selection mode plays it,
and again stops it; `/play [nick]` plays the latest here, `/play stop`
stops. The status bar shows `▶ playing` meanwhile. Textrix fetches and
decrypts it, decodes Element's Ogg Opus itself, and hands it to the
system's player (`afplay` on macOS; `pw-play`, `paplay`, `mpv`, `ffplay`
or `aplay` on Linux, or whatever `audio_player` says) from a private file
that's deleted as soon as it stops.

Videos show their length and size, `[video] clip.mp4 (0:42, 1920×1080)`,
and their poster frame inline when the sender included one. `z` (or
Space, or `/play`) fetches and decrypts one (up to 200 MB) and opens it
in mpv if you have it, otherwise in the app your system uses for that
kind of file (QuickTime Player on macOS, which can't play `.webm`; set
`video_player`, e.g. to `"mpv"`, for one that can). The decrypted copy
stays in a private folder until Textrix closes.

### Reading state

- A red **new messages** divider marks where you stopped reading. It starts at
  your shared read marker (the same one Element uses) and moves when you leave
  a window or send a message.
- `✓ nick` after a line shows how far other members have read.
- Presence: coloured dots in the member list and on DM windows in the
  window list (green online, yellow away, grey offline); a DM's topic bar
  says how the other person is doing ("away: lunch", "last seen 3h ago");
  `/whois` includes it. `✎` in the window list means someone is typing there;
  typing notices clear when that person's message arrives, and expire after
  45 seconds without an update.
- Like irssi's "is away": when you write to someone who's away or offline
  (in your DM with them, by mentioning them, or replying to them), a line
  says so ("sam is away: lunch") once, and again only when that changes.
  It uses the presence your server shares, so not on sliding sync.
- Messages that mention you or a highlight word are collected, from every
  room and in time order, in a **mentions** window (listed under status;
  `/mentions` opens it). Click one, or select it and press Enter, to go to
  the message in its room. Mentions found when history loads join it too.
- Highlights and DMs from other windows (or any window while the terminal is
  in the background) ring the bell and pop a desktop notification in
  iTerm2, Ghostty, WezTerm and kitty. Inside tmux, notifications need
  `set -g allow-passthrough on`, and `c` (copy) in selection mode needs
  `set -g set-clipboard on`.
- Textrix moves your read marker as you read, and sends read receipts as set
  by `read_receipts`.

### Notifications

By default, mentions of you, highlight words and direct messages alert you
(bell, desktop notification, and mentions go to the mentions window); other messages
just light up the window list. `/notify` changes that per room, stored as
Matrix push rules so Element and your phone follow the same setting:

| `/notify …` | Effect |
|---|---|
| `all` | every message alerts you |
| `mentions` | only mentions and highlight words alert; other messages show quietly |
| `mute` | nothing alerts, even mentions; the room is dimmed in the window list |
| `default` | back to the default |

The bell rings for mentions and DMs (and every message in rooms set to
`/notify all`), new invitations, verification requests, messages that
failed to send, and finished uploads and downloads; `bell_for` picks which.
A burst rings once (rings are at least two seconds apart). `/bell off`,
`/bell away` (only while the terminal is in the background, in terminals
that report focus) and `/bell visual` (flash the status bar instead of the
sound) change it for the session; `/bell test` tries it. Inside tmux the
bell marks the window in tmux's status line.

`/hilight <word>` highlights a word in every room, like a mention (also
synced, so your other clients notify on it too); `/hilight` lists them and
`/dehilight <word>` removes one. `/info` shows a room's setting.

### Colours

`theme = "default"` suits dark terminal backgrounds, `"light"` light ones,
`"mono"` uses no colour at all (bold, underline and reverse video). The
rest are monochrome monitors in green (phosphor), blue, red (Virtual Boy)
or amber, each three ways: on black (`"green"`), on deep blue with every panel
framed in double lines like an 80s airport terminal (`"green-deep"`), or
on black with thin neon outlines (`"green-tron"`). `/theme blue-deep`
tries one for the session; `/theme` lists them. A `[colors]` table changes single
parts, by colour name (`lightcyan`), `#rrggbb`, or a 256-colour number:

```toml
theme = "light"

[colors]
bar_bg = "#203050"
link = "lightcyan"
nicks = "red, 214, #00aa88, blue"   # the colours nicks are picked from
```

The keys: `bar_fg`, `bar_bg`, `bar_accent`, `bar_alert` (the topic and
status bars), `pane_fg`, `pane_bg` (unfocused split panes), `dim`,
`accent`, `frame`, `own_nick`, `highlight`, `highlight_text`, `error`,
`divider`, `link`, `mention`, `code`, `code_comment`, `code_keyword`,
`code_string`, `code_number`, `code_function`, `code_type` (syntax colours
in code blocks), `badge` (reply counts), `pinned`,
`receipts`, `matched`, `current_fg`, `current_bg`, `shown`, `inactive`,
`activity_event`, `activity` (the window list), `op`, `voice`, `online`,
`away`, `offline` (the member list), and `nicks`. A typo stops Textrix
with the list of keys rather than being ignored.

### Scripts

Like irssi's Perl scripts, but in [Rhai](https://rhai.rs): each `.rhai`
file in `~/.textrix/scripts/` loads at startup and can add commands and
react to messages.

```rhai
command("shout", "Say it in capitals", |args, ctx| say(args.to_upper() + "!"));

on_message(|msg| {
    if matches(msg.body, "(?i)\\bpizza\\b") { bell(); }
});
```

Scripts can keep things across restarts, have settings that `/set` and
`/save` handle like the rest, and put items in the status bar. They're
sandboxed (no files, network or programs, and a runaway loop is stopped)
and rate-limited to 5 messages per 10 seconds.
[SCRIPTING.md](SCRIPTING.md) has the whole API, and
[examples/scripts](examples/scripts) has dice, `/seen`, watch words and a
DM counter to start from.

## Optional preferences

`~/.textrix/config.toml` holds your preferences (all keys optional).
`/set` lists them and `/set <name> <value>` changes one for the session
(`/set theme blue-deep`); `/save` writes the session's settings back into
the file, including what `/theme`, `/bell`, `/log`, F2 and F3 changed,
leaving your comments alone. After editing the file by hand, `/reload`
applies it (and picks up servers added to or removed from the dotfile):

```toml
timestamp_format = "%H:%M"
show_joins = true
link_titles = "unencrypted" # page titles under links: "all", "off"
fold_joins = true          # runs of joins/leaves/renames become one line
markdown = true
send_typing = true
read_receipts = "public"   # "private" (only your clients see it) or "off"
bell = true                # false, or "away": only while the terminal is in the background
bell_style = "sound"       # "visual" flashes the status bar, "both" does both
bell_for = ["mentions", "invites", "verification", "failed_sends", "transfers"]
notifications = "auto"     # or "osc9", "osc777", "osc99" (kitty), "off"
notify_preview = true      # include message text in notifications
log = false                # chat logs (plaintext!); /log on|off per session
log_dir = "~/.textrix/logs"
sync = "auto"              # or "sliding", "classic"
image_previews = true      # inline images
image_rows = 10            # their maximum height
audio_player = ""          # e.g. "mpv --no-video"; "" uses the system's player
video_player = ""          # e.g. "mpv" or "open -a IINA"; "" uses mpv, else the system's app
sidebar = true             # window list (F2)
sidebar_avatars = true     # room avatars there (coloured initials without graphics)
nicklist = true            # member list (F3)
mouse = true               # wheel and clicks (hold Shift/Option to select text)
paste_confirm_lines = 5    # ask before sending longer pastes (0: never)
theme = "default"          # light, mono, green, blue, red, amber (+ -deep, -tron); [colors] overrides
title = "{window}{activity} — Textrix"   # the terminal's title; "" leaves it alone
history_cache = true       # show recent messages instantly at startup
encrypt_store = true       # encrypt the local store, key in the system keychain
check_updates = false      # once a day, ask GitHub whether a newer release is out
search_index = true        # encrypted local index of messages for /search
scripts = true             # load ~/.textrix/scripts/*.rhai at startup
scrollback = 5000
history_page = 50
completion_char = ":"
```

## Secrets

If you grew up on mIRC and irssi, a few things in here may feel familiar.
They're not in `/help`.

## Building and releases

`cargo build --release` gives a single binary at `target/release/textrix`
(release builds read `~/.textrix/.env`). CI runs `cargo fmt --check`, clippy
and the tests on every push to `master`.

Releases come from `master`: bump `version` in `Cargo.toml` and push. CI
then builds macOS (Apple silicon and Intel) and Linux x86_64 archives, tags
`vX.Y.Z`, and publishes a GitHub release with them and a filled-in
Homebrew formula (`textrix.rb`, from `packaging/homebrew/`). Running the
Release workflow by hand releases the current version if it isn't out yet.
For the tap, copy `textrix.rb` into the `Formula/` directory of
`seamoss/homebrew-textrix`.

Textrix is MIT licensed; see `LICENSE`.

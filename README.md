# Textrix

An irssi-style terminal client for Matrix, in Rust (matrix-rust-sdk + ratatui).
Numbered windows, a status bar with activity, slash commands, nick completion,
and end-to-end encryption.

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
`RECOVERY_KEY`); see `.env.example`. Every server in the file connects on
startup. Secrets are read only when the API call needs them and are wiped
afterwards. They are never logged, shown, or typed into the client, and the
file is refused while other users can read it. Real environment variables
override the file, so `op run --env-file … -- textrix` works with
`op://` references.

State lives in `~/.textrix/` (`TEXTRIX_HOME` overrides it): per-server session
and crypto store under `accounts/<name>/`, window order in `layout.json`,
logs in `textrix.log` (`TEXTRIX_LOG=debug` for more).

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

## Keys

| Key | Action |
|---|---|
| Alt+1…0, Alt+Q…P | window 1–20 |
| Ctrl+N / Ctrl+P, Alt+←/→ | next / previous window |
| Alt+A | jump to the most important active window |
| PgUp / PgDn | scroll (older history loads at the top) |
| Tab | complete commands, nicks (adds a mention), server names, file paths |
| Up / Down | input history |
| Alt+Up | **select** a message: ↑/↓ PgUp/PgDn g/G move; `r` reply, `e` edit, `d` delete, `+` react, `s` save attachment, `c` copy, `i` details, Esc leaves |
| Shift+Up, or Ctrl+E on an empty line | **edit** your messages: ↑/↓ pick one (it loads into the input line), Enter saves, clear + Enter deletes, Esc restores your draft |
| Shift+Enter (Alt+Enter in Terminal.app/tmux) | new line in the message |
| F1 / F2 / F3 | help / window list / member list |
| Ctrl+A/E/B/F/U/K/W/Y/D | emacs-style editing; Ctrl+C clears the line |
| Ctrl+L | redraw |

While you type a command, its usage appears in dim text after the cursor.

On macOS, set your terminal's Option key to act as Meta/Esc+ for the Alt
bindings.

## Commands

`/help` lists everything; `/help <command>` shows usage.

| Area | Commands |
|---|---|
| Rooms | `/create [-public] <name>`, `/join`, `/name`, `/topic`, `/info`, `/rooms`, `/invites`, `/decline`, `/part`, `/forget`, `/invite`, `/kick`, `/ban`, `/unban`, `/names` |
| Directory | `/list [search]` (public rooms) |
| Links | `/urls` (numbered, newest first), `/open [N]`; `o` in selection mode |
| Messages | plain text (Markdown), `/me`, `/notice`, `/msg`, `/query`, `/reply [nick] <text>`, `/react [nick] <emoji>`, `/edit [text]`, `/redact`, `/lastlog [-n N] [-nick nick] <text>` |
| Files | `/upload <path>` (Tab completes paths; dropped/escaped paths work), `/download [nick]` (saves to Downloads, never overwrites) |
| Windows | `/window <n\|list\|close\|move n\|goto name>`, `/wc`, `/wl`, `/clear`, `/sidebar`, `/nicklist` |
| Account | `/servers`, `/connect`, `/disconnect`, `/logout`, `/nick`, `/away [-all] [message]`, `/ignore`, `/unignore`, `/whois`, `/verify`, `/recover`, `/quit` |

Editing follows Matrix's rules: only your own text messages, and only where
your power level lets you post. Deleting someone else's message needs
moderator rights; Textrix checks before asking you to confirm.

When several servers are connected, prefix a command with `-<server>` while in
the status window (e.g. `/join -work #ops:work.example`).

### Formatting

Formatted messages render as styled text: bold, italic, strikethrough,
inline code and code blocks, quotes (`▎`), bullet and numbered lists,
headings, coloured text, links (with the address when the text differs),
mention pills, and spoilers (hidden until you press `v` in selection mode).
What you send is Markdown (`markdown = true`) and shows formatted right away.

### Images

Images show inline under their message. Textrix asks the terminal what it
supports at startup: kitty graphics (kitty, Ghostty, WezTerm), iTerm2 inline
images, or sixel, falling back to coloured half-block characters elsewhere
(e.g. Terminal.app). Encrypted images use the sender's thumbnail when there
is one; otherwise the image itself is fetched (up to 20 MB) and decoded with
size and memory limits. Inside tmux, graphics need
`set -g allow-passthrough on`. `s` in selection mode or `/download` saves the
original.

### Reading state

- A red **new messages** divider marks where you stopped reading. It starts at
  your shared read marker (the same one Element uses) and moves when you leave
  a window or send a message.
- `✓ nick` after a line shows how far other members have read.
- Presence: coloured dots in the member list and on DM windows in the
  window list (green online, yellow away, grey offline); a DM's topic bar
  says how the other person is doing ("away: lunch", "last seen 3h ago");
  `/whois` includes it. `✎` in the window list means someone is typing there.
- Highlights and DMs from other windows (or any window while the terminal is
  in the background) are copied into the status window, ring the bell, and
  pop a desktop notification in iTerm2, Ghostty, WezTerm and kitty. Inside
  tmux, notifications need `set -g allow-passthrough on`, and `c` (copy) in
  selection mode needs `set -g set-clipboard on`.
- Textrix moves your read marker as you read, and sends read receipts as set
  by `read_receipts`.

## Optional preferences

`~/.textrix/config.toml` (all keys optional):

```toml
timestamp_format = "%H:%M"
show_joins = true
markdown = true
send_typing = true
read_receipts = "public"   # "private" (only your clients see it) or "off"
bell = true
notifications = "auto"     # or "osc9", "osc777", "osc99" (kitty), "off"
notify_preview = true      # include message text in notifications
image_previews = true      # inline images
image_rows = 10            # their maximum height
sidebar = true             # window list (F2)
nicklist = true            # member list (F3)
scrollback = 5000
history_page = 50
completion_char = ":"
```

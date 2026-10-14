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
| Tab | complete commands, nicks (adds a mention), server names |
| Up / Down | input history |
| Ctrl+A/E/B/F/U/K/W/Y/D | emacs-style editing; Ctrl+C clears the line |
| Ctrl+L | redraw |

On macOS, set your terminal's Option key to act as Meta/Esc+ for the Alt
bindings.

## Commands

`/help` lists everything; `/help <command>` shows usage.

| Area | Commands |
|---|---|
| Rooms | `/create [-public] <name>`, `/join`, `/name`, `/topic`, `/info`, `/rooms`, `/invites`, `/decline`, `/part`, `/forget`, `/invite`, `/kick`, `/ban`, `/unban`, `/names` |
| Messages | plain text (Markdown), `/me`, `/notice`, `/msg`, `/query`, `/reply [nick] <text>`, `/react [nick] <emoji>`, `/edit`, `/redact` |
| Files | `/upload <path>` (Tab completes paths; dropped/escaped paths work), `/download [nick]` (saves to Downloads, never overwrites) |
| Windows | `/window <n\|list\|close\|move n\|goto name>`, `/wc`, `/wl`, `/clear` |
| Account | `/servers`, `/connect`, `/disconnect`, `/logout`, `/nick`, `/whois`, `/verify`, `/recover`, `/quit` |

When several servers are connected, prefix a command with `-<server>` while in
the status window (e.g. `/join -work #ops:work.example`).

### Reading state

- A red **new messages** divider marks where you stopped reading. It starts at
  your shared read marker (the same one Element uses) and moves when you leave
  a window or send a message.
- `✓ nick` after a line shows how far other members have read.
- Highlights and DMs from other windows are copied into the status window and
  ring the terminal bell.
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
scrollback = 5000
history_page = 50
completion_char = ":"
```

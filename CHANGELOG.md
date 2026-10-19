# Changelog

Every release of Textrix, newest first. Versions follow
[semantic versioning](https://semver.org); each one is a tag (`vX.Y.Z`) and
a [GitHub release](https://github.com/seamoss/Textrix/releases) with builds
for macOS and Linux, published from `master` when the version in
`Cargo.toml` changes. A release needs its section here, which becomes its
release notes.

## [0.2.0] - 2026-10-18

### Added
- Scripts, irssi-style, in [Rhai](https://rhai.rs): every `.rhai` file in
  `~/.textrix/scripts/` loads at startup and can add commands (`command`)
  and react to new messages (`on_message`) by printing, saying, sending to
  other rooms, running commands, ringing the bell or notifying.
  `/script` lists, loads, unloads and reloads them; `/script exec` runs a
  line once. Scripts are sandboxed (no files, network or programs, and
  long-running code is stopped), can send at most 5 things in 10 seconds,
  and never hear your own messages. `scripts = false` stops them loading.
  [SCRIPTING.md](https://github.com/seamoss/Textrix/blob/master/SCRIPTING.md) has the API, and `examples/scripts` has
  `/shout`, `/roll` dice, `/seen` and watch words.

## [0.1.4] - 2026-10-18

### Added
- Command-line options: `--check` (checks the dotfile, config.toml, the
  keychain and each server's store, then exits, without connecting or
  reading secrets), `--theme <name>` for one run, `--env <file>` and
  `--home <dir>`, plus `--completions <shell>` and `--man`. Homebrew
  installs the shell completions and the man page.
- An opt-in update notice: with `check_updates = true`, Textrix asks
  GitHub once a day whether a newer release is out and says so in the
  status window. `/version` shows the version; `/version check` asks now.
- Builds for Linux on ARM, and one macOS build for both Apple silicon and
  Intel, which Homebrew uses.
- The website has a changelog page, made from this file.

## [0.1.3] - 2026-10-18

### Added
- Amber monochrome-monitor themes: `amber`, `amber-deep` (on deep blue
  with double-line frames) and `amber-tron`.
- `/avatar <image>` sets your profile picture and `/roomavatar <image>`
  the room's; `-remove` takes either off. The file has to really be an
  image (checked before uploading), up to 10 MB; Tab completes the path.

### Changed
- A dropped connection is a netsplit now, irssi-style: "Netsplit
  0x4f02.com ⇄ you, quits: …" when it goes and "Netsplit over after
  2m 13s, joins: …" when it's back, in the status window and the window
  you're in.

## [0.1.2] - 2026-10-16

### Added
- `/set` lists every setting, or changes one for the session, checked
  like config.toml (`/set theme red-tron`, `/set image_rows 14`); Tab
  completes the names.
- `/save` writes this session's settings into config.toml: what you
  changed with `/set`, `/theme`, `/bell`, `/log` and the F2/F3 panel
  toggles. Only changed lines are written; comments and everything else
  in the file stay as they are.

## [0.1.1] - 2026-10-16

### Added
- Monochrome-monitor themes in green (phosphor), blue and red (Virtual
  Boy), each three ways:
  - `green`, `blue`, `red`: the colour on black.
  - `green-deep`, `blue-deep`, `red-deep`: on deep blue, with every panel
    framed in double lines, like an 80s airport terminal.
  - `green-tron`, `blue-tron`, `red-tron`: on black with thin neon outlines.
  Set `theme = "red-tron"` in config.toml, or try one with `/theme`.
- Themes can frame the window list, panes and member list; a framed pane
  carries its name, and the focused one lights up.

### Changed
- Install with Homebrew: `brew install seamoss/textrix/textrix`.

## [0.1.0] - 2026-10-16

The first release.

### Added
- **Connections** come only from a private dotfile (`~/.textrix/.env`):
  several servers at once, secrets read only for the API call that needs
  them, environment variables (e.g. from a password manager) override it.
- **Encryption**: end-to-end encrypted rooms, emoji verification, recovery
  key and key backup, `/keys export` and `/keys import` in Element's
  format. The local store is encrypted with a key kept in the system
  keychain.
- **irssi-style windows**: numbered windows, a window list grouped by
  server and space, a member list, split panes, the Ctrl+K switcher, your
  own names for windows (`/window name`) and people (`/nickname`), drafts
  kept per window, and the terminal title following where you are.
- **Messages**: replies, reactions, edits, deletions, threads in their own
  windows, Markdown, rich text, emoji shortcodes, mentions, forwarding,
  pinned messages, read receipts, typing notices, file uploads and
  downloads, inline images with a full-screen zoomable viewer, link titles,
  and folded joins and leaves.
- **Selection mode** (Alt+Up) to act on any message, and mouse support.
- **Notifications**: highlights, per-room levels shared with other clients
  (`/notify`), highlight words (`/hilight`), a mentions window, desktop
  notifications through the terminal, and a configurable bell.
- **Rooms**: create, join, leave, invite, moderate, room settings, upgrades,
  the room directory, spaces, and `/search` across rooms.
- **Account**: presence and `/away` (with irssi-style "is away" notes),
  `/ignore`, sessions (`/devices`), `/whois`, `/alias`, `/markread`.
- **Look**: colour themes and `[colors]` overrides, instant history at
  startup, opt-in chat logs, and a few secrets.

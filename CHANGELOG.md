# Changelog

Every release of Textrix, newest first. Versions follow
[semantic versioning](https://semver.org); each one is a tag (`vX.Y.Z`) and
a [GitHub release](https://github.com/seamoss/Textrix/releases) with builds
for macOS and Linux, published from `master` when the version in
`Cargo.toml` changes. A release needs its section here, which becomes its
release notes.

## [0.1.1] - 2026-10-16

### Added
- Monochrome-monitor themes, one colour on black: `green` (phosphor),
  `blue`, and `red` (Virtual Boy). `theme = "red"` in config.toml, or
  `/theme red` for the session.

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

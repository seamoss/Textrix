# Changelog

Every release of Textrix, newest first. Versions follow
[semantic versioning](https://semver.org); each one is a tag (`vX.Y.Z`) and
a [GitHub release](https://github.com/seamoss/Textrix/releases) with builds
for macOS and Linux, published from `master` when the version in
`Cargo.toml` changes. A release needs its section here, which becomes its
release notes.

## [0.16.2] - 2026-10-26

### Fixed
- Esc typed just before another key no longer eats it: the two can arrive
  together and read as Alt+key (typing quickly, or through tmux), which
  turned Esc then `/quit` into a message saying "quit". An Alt+key that
  means nothing is now Esc, then the key; one that means something still
  does it (Esc 1 goes to window 1, as in irssi), and from selection mode
  too.

## [0.16.1] - 2026-10-26

### Fixed
- Messages written offline and still waiting when Textrix quit show again
  as "(waiting to send)" when it starts; they were kept and sent, but not
  shown until they went.
- Not reaching the server says why in a few words ("connection refused",
  "the connection dropped", "can't look up its address") instead of
  printing the whole sync URL or a link to a library's manual.
- Starting offline shows your display name, saved from last time, rather
  than your Matrix ID.
- When the network comes back, Textrix notices within a quarter minute
  (it waited up to a minute), and at once when a waiting message gets
  through.

## [0.16.0] - 2026-10-26

### Added
- Voice messages: `/voice` (or Ctrl+R) records one; the input line shows
  `● REC 0:07`, Enter sends it and Esc throws it away (ten minutes at
  most). It goes as a proper voice message, Ogg Opus with its length and
  waveform, the way Element sends them. On macOS Textrix records from the
  microphone itself and the system's `afconvert` encodes it (macOS asks
  once whether your terminal may use the microphone); on Linux the sound
  system's recorder (`pw-record`, `parecord`, `arecord`, or sox) records
  and `opusenc` or ffmpeg encodes, else it goes as a WAV.
- Offline: Textrix starts without a network, with your rooms and what's
  cached in them to read, and says it can't reach the server (then that
  it's back). Messages you send meanwhile show as "(waiting to send)" and
  go when the server can be reached: they wait in the account's store, so
  quitting doesn't lose them either.
- Theme `shockwave`, on the wild shelf: an 80s club at 2am, magenta bars,
  an acid-green current window and every other neon at once.

### Changed
- Messages go through matrix-sdk's send queue, which retries when the
  connection drops instead of failing at once.
- Older history only loads while the server can be reached, so being
  offline doesn't print an error every quarter minute.

### Fixed
- A first sliding sync that can't reach the server (starting offline) is
  retried, instead of being taken as the server lacking sliding sync and
  switching to classic sync for good.
## [0.15.2] - 2026-10-21

### Changed
- Sent and read look different: your message gets a dim `✓` once the
  server has it, and `✓✓ sam` (in green) once sam has read it. Read marks
  were a single `✓` before, so a message that had only arrived looked no
  different from one that had been read.

### Fixed
- A message you'd just sent could show as read by someone who hadn't read
  it: a read receipt for something Textrix doesn't draw as a line (a
  reaction, an edit) was put on the newest line. It now goes on the
  newest line from before it was sent.

## [0.15.1] - 2026-10-21

### Fixed
- Verifying with Element no longer opens the phone's camera. Textrix told
  Element it could verify by QR code as well as by emoji (the SDK's
  default), so Element offered to scan a code Textrix never shows. It now
  asks for and accepts emoji only, and Element goes straight to them.

## [0.15.0] - 2026-10-20

### Added
- An emoji picker: `+` on a message in selection mode opens it, with your
  recent reactions first, then your custom emoji, then every emoji by
  group. Arrows choose, typing searches by name or shortcode, Enter or a
  click reacts; with nothing matching, Enter reacts with what you typed.
- `notify_command`: a command of yours run for each notification
  (mentions, DMs, rooms set to `/notify all`), with who, where and what in
  `TEXTRIX_*` environment variables, never in the command itself. Push
  mentions to your phone, say. Four at most at once, 30 seconds each; a
  failure is reported once.
- `theme = "auto"`: Textrix asks the terminal for its background colour
  (or reads `COLORFGBG`) and takes `theme_dark` or `theme_light`, by
  default `default` and `light`. Each terminal that attaches to a
  background Textrix is asked too.
- Alt+R (or `/jump new`) jumps to the new-messages line in the room, the
  last message you'd read selected above it, loading history back to it.
- `/changelog [N]`: what changed in the latest N releases (10 unless you
  say), from GitHub's release notes, formatted, newest at the bottom.
- Seventeen themes, fifty-one in all:
  - `miami-heat`, `cemetery` (or `cemetary`), `summer-evening`, `abyss`,
    `noir` and `espresso` on the nouveau shelf;
  - `gameboy`, `teletext`, `bsod` and `hotdog-stand` on the classic shelf;
  - a wild shelf: `vaporwave`, `radioactive` and `disco`;
  - an accessible shelf: `colorblind` and `colorblind-light` for red-green
    colour blindness (blue and orange, never red against green; online is
    blue, away orange), `tritan` for blue-yellow colour blindness, and
    `high-contrast`. Checked against simulated colour blindness.
- The website's themes have a page of their own, to try them on: pick one
  and see the whole screen in it, with a link to it to send someone.

### Changed
- Wrapped list items and quotes line up under their own text (quote bars
  repeated), and a word that ends right at the edge of the window stays on
  its row.
- Scripts can't change settings that run programs (`notify_command`,
  `editor`, `video_player`), as they can't run `/exec`.

### Fixed
- Activity numbers on the status bar no longer vanish when their colour
  is the bar's (Game Boy, Hot Dog Stand, vaporwave).

## [0.14.0] - 2026-10-20

### Added
- Home (F4, or `/home`): what's waiting for you in one place, over the
  messages. Your mentions from the last day (the latest five), every room
  with unread messages and what was said there last (mentions first,
  then the most recent), and your invitations. ↑/↓ and Enter, a click, or
  the row's number (1–9) goes there: to the message, the room, or, for an
  invitation, joining it; `d` twice declines one. Esc or F4 goes back.
- The media gallery (`/media`): the room's pictures, videos, voice
  messages and files as tiles, newest first. Pictures show as small
  pictures, the rest as boxes saying what they are and how big. Arrow
  keys or the mouse move through them; Enter opens one (a picture in the
  viewer, audio and video played, a file saved) and `s` saves any of them.
  Near the end of what's loaded, it fetches more of the room's history.
- Themes: `purple`, `purple-deep` and `purple-tron` join the monitor
  themes, and `synthwave` (or `outrun`) is the 80s as remembered: neon pink
  and cyan on a midnight-purple sky, the sun setting in yellow and orange.
  Thirty-four in all.

### Changed
- Upgrade instructions say `brew update && brew upgrade textrix`. On its
  own, Homebrew looks at taps like Textrix's only about once a day, so
  `brew upgrade` alone can miss a release from the last few hours.

## [0.13.0] - 2026-10-20

### Added
- Link pictures: under a link's title, the page's picture (its
  `og:image`), up to 24×6 cells and lined up with the message. It comes
  from the homeserver's URL preview along with the title, so the site
  still never sees you, and it shows wherever titles do (`link_titles`).
  `link_images = false` keeps the titles and leaves the pictures out.
- Edit history: `i` on an edited message in selection mode lists what it
  said before each edit, oldest first, with when, then what it says now.
- Spoilers when sending: `||the butler did it||` sends that part as a
  spoiler, hidden until revealed (in Textrix, `v`; in Element, a click).
  Bars inside code are left as they are.
- `/exec <command>`, as in irssi: runs a shell command and shows what it
  printed in the current window; `/exec -o <command>` sends it to the room
  instead (several lines go as a code block). Commands get no input, are
  stopped after 30 seconds, and their output is cut at 500 lines; colour
  codes are taken out. Scripts can't run it.

## [0.12.2] - 2026-10-20

### Changed
- Smaller to download and much quicker to build: releases are optimised
  for size with thin LTO, which uses every core (the old fat LTO ran its
  last step on one, which took most of a release's half hour), and the
  image library is built with only the formats chats send (JPEG, PNG, GIF,
  WebP, BMP), not encoders and formats Textrix never shows. The download
  goes from about 22 to 19 MB.

## [0.12.1] - 2026-10-20

### Fixed
- `u` on a message in the mentions window opens the sender's profile
  (it did nothing there).
- `/theme import` of a file named like a word `/theme` takes (`list`,
  `here`…) says so, instead of calling it a built-in theme's name.
- 0.12.0 wasn't released on its own (GitHub refused to tag it); this
  release includes it: the profile pane.

### Changed
- F1 lists the keys of the theme list, voting and the profile pane, and
  `/help` describes `/theme here` and `/theme space`, Enter on a poll,
  where Ctrl+X finds your editor, and `/layout <name>`. The status bar's
  selection-mode keys include `u`.
- The website: the features of 0.10–0.12, the profile screenshot, the
  nick column in its drawn screen, and the version of the latest release
  that's out (not one still building). The README and SCRIPTING.md
  brought up to date with them.

## [0.12.0] - 2026-10-20

### Added
- A profile pane: click a name in the member list or a nick on a message
  (or press `u` on a message in selection mode) to see who they are,
  beside the member list: their picture, their name and yours for them,
  Matrix ID, presence, role in the room, and whether you've verified them
  and their devices. ↑/↓ goes through the member list; `m` opens a DM,
  `@` mentions them, and `n`, `i` and `v` put `/nickname`, `/ignore` and
  `/verify` in the input line to send. Esc closes it.

### Changed
- Clicking a name in the member list opens their profile instead of
  mentioning them; `@` in the profile mentions them.
- Releases join the universal macOS build on Linux (llvm-lipo), where it
  doesn't wait in the macOS runners' queue.

## [0.11.0] - 2026-10-20

### Added
- Polls. Element's (and the stable kind) show their answers under the
  question, with results as bars, ✓ by yours, and how many voted: while
  open when the poll shows results, otherwise once it's ended. Select one
  and press Enter to vote (↑/↓ and Enter, a number, or Space to tick
  several where allowed). `/poll <question> | <answer> | <answer>…`
  starts one the way Element does, and `/poll end` ends your latest.
- Stickers show like images, inline under their name, and open in the
  viewer.
- Custom emoji from image packs (the room's, your own, ones you've made
  global) show by their `:shortcode:`, in messages and reactions. Tab
  completes them, and they go out as the pictures other clients show, in
  messages, replies, edits and reactions.
- `/later <when> <text>` sends a message later: `10m`, `1h30m`, `18:30`,
  `tomorrow 9:00`. `/later` lists what's waiting and `/later cancel
  <N|all>` takes it back. Textrix sends them while it runs (in the
  background is fine), and keeps them on disk sealed with the store's
  keychain secret.

## [0.10.0] - 2026-10-20

### Added
- Ctrl+X writes the message in your editor: `editor` in config.toml, else
  `$VISUAL`, else `$EDITOR`, else vi. Save and quit, and the text is back
  in the input line to send. It works on a private temporary file,
  deleted when the editor closes; in background mode the editor runs in
  the attached terminal.
- An away log, like irssi's: while you're `/away`, direct messages
  collect in the mentions window too, and coming back says what came in
  ("while you were away (1h 5m): 3 mentions (Team Tests) and 2 DMs
  (sam)") and opens it.
- Rooms can keep a theme of their own: `/theme here <name>` (or Ctrl+T in
  the theme list, for "just here") and `/theme here off`; `/theme space
  <space> <name>` does it for a space's rooms, unless they have their own.
  The screen takes the theme of the room you're in. They're kept in
  layout.json.
- The status bar is yours to arrange: `statusbar` lists its items in
  order (`time`, `date`, `nick`, `window`, `users`, `activity`, `typing`,
  `scripts`, `upgrade`, `playing`, `servers`, `theme`, `more`), and
  `statusbar2` adds a second bar above it. Prompts always show.

### Changed
- A full redraw (Ctrl+L, a new theme) no longer asks the terminal where
  its cursor is first, which it doesn't need to know.

## [0.9.7] - 2026-10-20

### Changed
- System messages, the `-!-` lines (Textrix's own, errors, joins and
  leaves, topic and room changes, day changes), are in italic in every
  window and theme, so they stand apart from the conversation; the time
  stays upright. `system_italic = false` keeps them upright.

## [0.9.6] - 2026-10-20

### Fixed
- A nick cut short in the nick column ends in "… " with a space before
  the `>`, so fonts whose "…" spills out of its cell don't draw it over
  the `>`.
- 0.9.5 wasn't built as a release of its own; this release includes it
  ("is away" notes only when they're news).

## [0.9.5] - 2026-10-20

### Changed
- "Is away" notes only come when they're news. Presence is a guess (a
  missed heartbeat reads as offline, and some people and servers don't
  share it), so a note now needs a status they set ("away: lunch") or an
  absence of half an hour or more ("offline, last seen 3h ago"). Nobody
  who's said something in the last 15 minutes gets one, and the same note
  comes at most once an hour, unless their status changes: a moment back
  online no longer makes it new again. `away_notes = false` turns them
  off.

## [0.9.4] - 2026-10-19

### Fixed
- "Day changed" lines are laid out like Textrix's other lines: `-!-` in
  the nick column under the timestamps, the bar in line with the rest,
  and quieter in rooms.

## [0.9.3] - 2026-10-19

### Changed
- Nicks sit in a column, irssi-style: `<sam>`, then spaces up to a bar, so
  every message starts in the same place and its wrapped lines (code
  blocks, pictures and link titles too) stay on their side of the bar.
  Nicks longer than 12 characters are cut short; `nick_column` sets the
  width, and 0 puts nicks inline as before. Joins show `-->` in the
  column, leaves `<--`. Panes too narrow for the column keep nicks inline.
- Textrix's own lines in a room (replies to your commands, errors,
  netsplits) are set apart from the conversation: a blank row before and
  after each run of them, in the theme's quiet colour and italic (errors
  keep their colour). Joins, leaves and topic changes happened in the
  room, so they stay as they were.

## [0.9.2] - 2026-10-19

### Changed
- `/theme` on its own opens the theme list with the live preview, the
  same as `/themes`, instead of printing the themes in the status window;
  `/theme list` prints them now. No theme file can be called `list`,
  `save` or `import`.

## [0.9.1] - 2026-10-19

### Added
- `/themes` opens a list of every theme (yours too) at the right of the
  screen. Moving through it with ↑/↓, PgUp/PgDn or the mouse shows each
  one on the whole screen as you go, with a line about it; typing narrows
  the list. Enter keeps the one you're on for this session (`/save` keeps
  it for good) and Esc puts back the one you had.

### Fixed
- The quick switcher and the help screen take the theme's background,
  instead of the terminal's, in themes that bring their own (Delphi's
  white, Turbo's blue).

## [0.9.0] - 2026-10-19

### Added
- Thirteen new themes. Classic: `turbo` (Turbo Pascal's DOS blue, grey
  menu bars, double-line windows), `procomm` (a DOS terminal program
  dialling a BBS: grey on black, ANSI colours, a cyan status line), `c64`,
  `amiga` (Workbench 1.3), `delphi` (Delphi 7's editor in Windows grey),
  `mirc`, `solarized-dark` and `solarized-light`. Nouveau: `monokai`,
  `dracula`, `nord`, `gruvbox` and `catppuccin` (Mocha). And two more
  monitors: `mono-tron`, Tron in greys, and `ibm5151`, the IBM monochrome
  display's #00FF66 green with its panels framed in plain ASCII.
- Your own themes: a file in `~/.textrix/themes/<name>.toml` starts from a
  built-in theme (`base`), changes any colour (`[colors]`, which now also
  takes `background`, `foreground`, `border` and `border_focus`), and can
  pick the outlines (`none`, `plain`, `rounded`, `double`, `thick` or
  `ascii`). `/theme <name>`, `theme = "<name>"` and `--theme <name>` use it
  like a built-in one. `/theme save <name>` writes the look you have now
  to a theme file to share, and `/theme import <file>` checks one you were
  sent and adds it. `/theme` lists every theme by shelf, yours included,
  and Tab completes their names.
- Screenshots in the README and on the website, and a gallery of every
  theme there. `scripts/screenshots.sh` draws them from the real interface
  and made-up rooms, without an account or a network.

### Changed
- The Tron themes are on pure black (#000000), not the terminal's own
  "black", which many colour schemes make a dark grey.
- In terminals without 24-bit colour, themes (and `#rrggbb` colours in
  `[colors]`) use the nearest of the 256 colours instead of sending
  colours the terminal can't show.
- `/help` lists the other names commands have had all along (`/fwd`,
  `/read`, `/sessions`, `/pinned`, `/highlight`, `/unhilight`), and its
  usage lines for `/search`, `/upgrade`, `/logout`, `/devices` and
  `/layout` say what they actually take. F1 lists the word-movement keys
  and Enter in the mentions window.
- The README covers upgrading (`brew upgrade textrix` once the tap is
  added), and the Homebrew caveats mention `textrix --check` and
  `/restart`.

### Fixed
- `/alias` no longer accepts names that already belong to a command
  (`/alias read …`, `/alias wc …`), which saved aliases that never ran.
- The `/roll` example script takes `2D6` as well as `2d6`, and `/remind`
  no longer counts reminders that have already gone off.

## [0.8.1] - 2026-10-19

### Changed
- The window list has sections: **Unreads** at the top (rooms and DMs
  with messages you haven't read, mentions first), then **Rooms** (under
  their spaces, if any) and **DMs**, each by name. A window moves up to
  Unreads when messages arrive and back once you've read them; its number
  stays the same throughout.

## [0.8.0] - 2026-10-19

### Added
- A running Textrix notices when the `textrix` it was started as is
  replaced (`brew upgrade`, `cargo build`) and says so in the status bar:
  `[0.8.1 installed: /restart]`. Nothing goes over the network for this.
- `/restart` becomes the newly installed version in place: the same
  terminal, back in the window you were in, every window's unsent text
  kept. In the background it restarts itself, and the attached terminal
  follows it to the new version.

### Changed
- With `check_updates = true`, Textrix asks GitHub for new releases once
  an hour (it was once a day) and shows `[0.8.1 is out]` in the status bar
  until you have it. It's still off unless you turn it on.

### Fixed
- Background mode works with a `--home` too long for a Unix socket's path:
  the socket goes in your temp folder instead.

## [0.7.1] - 2026-10-19

### Added
- `/close` shuts a room you own down for everyone. It first says what it
  will do (or what your power level doesn't allow); `/close confirm` makes
  the room invite-only, takes it off the directory, removes its addresses,
  withdraws invites, removes every member less powerful than you, then
  leaves and forgets it. If you're an admin on your Synapse homeserver, the
  server also deletes the room and blocks it from coming back.

## [0.7.0] - 2026-10-18

### Added
- Images you send carry a thumbnail and a blurhash, as Element sends them,
  so they show quickly in other clients (phones especially), and videos a
  poster frame (Quick Look on macOS, ffmpeg elsewhere), their length and
  size.
- Links are real hyperlinks (OSC 8) in terminals that have them: hovering
  shows where they go, Cmd/Ctrl+click opens them, and a link wrapped over
  several lines is one link. `hyperlinks = false` turns it off.
- Search filters: `/search from:sam in:"Book Club" after:2026-01
  before:2026-06-30 pizza`, any part optional, over loaded history, the
  index and `-deep`.
- Resizable panes: drag the line between two panes (or the upper pane's
  title bar), or `/resize +5`, `/resize -5`, `/resize =`. Sizes are kept
  with the split and in saved layouts.
- Script timers and hooks: `every(secs, f)`, `after(secs, f)` and
  `cancel(id)`; `on_join`, `on_leave`, `on_reaction` and `on_highlight`.
  A new example, `/remind 10 stretch`.

### Changed
- The website lists what's arrived since: background mode, the search
  index, voice and video, drag and drop, layouts.

## [0.6.5] - 2026-10-18

### Fixed
- Pictures are scaled down smoothly: inline previews and room avatars
  were shrunk by skipping pixels, which made photos (especially your own
  uploads, sent without a thumbnail) look rough in every terminal.

### Changed
- In terminals without graphics (Terminal.app, VS Code, Claude's desktop
  app), pictures are drawn with sextants (2×3 blocks a character) or
  quadrants (2×2) in two colours, two or three times the detail of the
  half-blocks before; `image_text` picks. Without 24-bit colour they use
  the 256-colour palette.
- The status window says how pictures are drawn in this terminal.

## [0.6.4] - 2026-10-18

### Added
- Ctrl+C twice quits: on an empty input line, the first asks "press
  Ctrl+C again to quit" and a second within two seconds does. In the
  background it detaches instead, leaving Textrix running. With text in
  the line, Ctrl+C still clears it.

### Changed
- 0.6.3 and 0.6.2 weren't built as releases of their own; this release
  includes them.

## [0.6.3] - 2026-10-18

### Fixed
- Started without a terminal (from a script, say), Textrix says it needs
  one instead of crashing.

### Changed
- Checked on Linux (Debian, in `scripts/linux.sh`): the tests pass, the
  keychain (Secret Service) holds the store key, and background mode
  starts, refuses a second one and stops.
- 0.6.2 wasn't built as a release of its own; this release includes it.

## [0.6.2] - 2026-10-18

### Fixed
- Dropping a file and pressing Enter uploads it: Enter (or `y`) answers
  the upload question; `n` keeps the path, which Enter then sends as a
  message rather than running it as a command. File names with unusual
  spaces (macOS screenshots put a narrow one before "PM") and paths a
  terminal types without escaping its spaces are recognised, and a path
  that still ends up as a command says how to upload instead of "unknown
  command".

### Added
- `scripts/linux.sh` tries Textrix on Linux in Docker: the tests, or
  Textrix itself with a Linux keychain, notifications and audio players.

## [0.6.1] - 2026-10-18

### Fixed
- Some voice messages from Element on phones made Textrix's Opus decoder
  fail (and a development build crash). Players that read Ogg Opus
  themselves now get it as it is: `afplay` on macOS 15 and later, mpv,
  ffplay and VLC. Textrix decodes it only for the others, and one it can't
  decode is an error saying so.
- A failure in a background task no longer prints over the screen or
  turns mouse support off; it goes to the log.

### Changed
- 0.6.0 (background mode) and 0.5.2 (drag and drop) weren't built as
  releases of their own; this release includes them.

## [0.6.0] - 2026-10-18

### Added
- Background mode, like irssi in screen: `textrix --daemon` runs Textrix
  with no terminal, staying connected, logging, and notifying through the
  system's notifications while none is attached. `textrix --attach` (or
  plain `textrix` while it runs) shows it in this terminal as you left it;
  `/detach` leaves it running, as does closing the terminal; attaching
  from another terminal takes it over; `/quit` or `textrix --stop` stops
  it. The terminal and the background Textrix talk over a socket only you
  can open.

### Changed
- Only one Textrix runs per home at a time: a second is refused, since two
  would share one store and can scramble its encryption keys.
- 0.5.2 (drag and drop to upload) wasn't built as a release of its own;
  this release includes it.

## [0.5.2] - 2026-10-18

### Added
- Drag and drop to upload: drop files of any kind (one or several) on the
  terminal and Textrix asks "upload notes.pdf (1.2 MB) to Night Owls? y/n".
  `n` leaves the paths in the input line, to send as text. It works
  whether the terminal pastes a dropped file's path or types it.

## [0.5.1] - 2026-10-18

### Added
- Videos play: `z` or Space in selection mode (or `/play`) fetches and
  decrypts one and opens it in mpv if it's installed, otherwise in the
  system's app for that kind of file; `video_player` picks another. The
  decrypted copy stays in a private folder only until Textrix closes.
- Videos show their length and size (`[video] clip.mp4 (0:42, 1920×1080)`)
  and their poster frame inline when the sender included one.

### Changed
- 0.3.0 to 0.5.0 were never built as releases of their own; this release
  includes everything in them.

## [0.5.0] - 2026-10-18

### Added
- A search index: `/search` finds messages from any time in any room,
  encrypted ones included, straight away. Textrix keeps decrypted messages
  in a local index, encrypted with the same keychain key as its store: new
  messages go in as they arrive, and older history is read back in the
  background (a page every few seconds, up to 20,000 messages a room).
  Edits and deletions are followed; `/logout` removes it, and
  `search_index = false` turns it off and deletes it.
- Layouts: the split you quit with comes back next time, and `/layout save
  <name>` keeps the panes (and whether the window and member lists show)
  to bring back with `/layout load <name>`.

## [0.4.0] - 2026-10-18

### Added
- Script stores: `store_set`, `store_get`, `store_remove` and `store_keys`
  keep a script's things across restarts, in a private JSON file per
  script in `~/.textrix/script-data/`.
- Script settings: `setting(name, default, help)` declares one; `/set`
  lists and changes them as `script.name`, Tab completes them, and `/save`
  writes them under `[script_settings.<script>]` in config.toml.
- Status bar items from scripts: `status(text)`, or `status(function)`
  for one worked out once a second.
- `now()` for scripts: the time in seconds.
- The watch words example has its words as a setting, `/seen` remembers
  across restarts, and a new example counts direct messages in the status
  bar.

## [0.3.0] - 2026-10-18

### Added
- Syntax colouring in code blocks that name their language (```` ```rust ````,
  `python`, `shell`, TOML, TypeScript and about 200 more, or a `#!` line),
  in colours that follow the theme: monochrome themes use shades of their
  one colour, `mono` uses bold and dim. `[colors]` takes `code_keyword`,
  `code_string`, `code_comment`, `code_number`, `code_function` and
  `code_type`.
- Voice messages show their length and waveform (`[voice] 0:12 ▁▃▅▇▅▃▁`),
  other audio its length. `z` or Space in selection mode plays one (again
  stops it), as do `/play [nick]` and `/play stop`. Element's Ogg Opus is
  decoded by Textrix, so the system's own player can play it (`afplay` on
  macOS; `pw-play`, `paplay`, `mpv`, `ffplay` or `aplay` on Linux;
  `audio_player` picks another). The decrypted audio is in a private file
  only while it plays.

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
  example.org ⇄ you, quits: …" when it goes and "Netsplit over after
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

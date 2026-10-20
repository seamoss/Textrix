//! Filesystem layout and (optional) UI preferences.
//!
//! Everything lives under `~/.textrix/` (override with `TEXTRIX_HOME`):
//!
//! ```text
//! .env                    server definitions + secrets (mode 600; debug
//!                         builds use `.textrix` in the repo instead)
//! config.toml             UI preferences (optional)
//! layout.json             window order
//! textrix.log             debug log
//! accounts/<name>/        per-server session + encrypted-state store
//! ```

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::theme::{Theme, ThemeId};

#[derive(Clone, Debug)]
pub struct Paths {
    pub base: PathBuf,
    pub env_file: PathBuf,
}

impl Paths {
    /// `home` and `env_file` (from --home and --env) win over TEXTRIX_HOME
    /// and TEXTRIX_ENV.
    pub fn discover(home: Option<PathBuf>, env_file: Option<PathBuf>) -> Result<Self> {
        let base = match home.or_else(|| std::env::var_os("TEXTRIX_HOME").map(PathBuf::from)) {
            Some(dir) => dir,
            None => dirs::home_dir().context("cannot locate home directory")?.join(".textrix"),
        };
        let env_file = match env_file.or_else(|| std::env::var_os("TEXTRIX_ENV").map(PathBuf::from)) {
            Some(path) => path,
            None if cfg!(debug_assertions) => Path::new(env!("CARGO_MANIFEST_DIR")).join(".textrix"),
            None => base.join(".env"),
        };
        Ok(Self { base, env_file })
    }

    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.toml")
    }

    /// Theme files, `<name>.toml`.
    pub fn themes_dir(&self) -> PathBuf {
        self.base.join("themes")
    }

    pub fn log_dir(&self, configured: Option<&str>) -> PathBuf {
        match configured {
            Some(dir) => match dir.strip_prefix("~/").zip(dirs::home_dir()) {
                Some((rest, home)) => home.join(rest),
                None => PathBuf::from(dir),
            },
            None => self.base.join("logs"),
        }
    }

    pub fn layout_file(&self) -> PathBuf {
        self.base.join("layout.json")
    }

    pub fn account_dir(&self, name: &str) -> PathBuf {
        self.base.join("accounts").join(name)
    }
}

/// Create a directory only the current user can enter.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Replace a file with `data`, readable only by the current user (written
/// beside it first, so a crash never leaves half a file).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(&tmp).and_then(|mut f| f.write_all(data)).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// strftime format for the timestamp column.
    pub timestamp_format: String,
    /// Say so when you write to someone who's away or long gone ("sam is
    /// away: lunch"), when it's news.
    pub away_notes: bool,
    /// System messages (`-!-` lines: Textrix's own, joins and leaves,
    /// topic changes, day changes) in italic, in every theme.
    pub system_italic: bool,
    /// The editor Ctrl+X opens on your message; empty uses $VISUAL, then
    /// $EDITOR, then vi.
    pub editor: String,
    /// The status bar's items, in order (see [`STATUS_ITEMS`]).
    pub statusbar: Vec<String>,
    /// A second bar, above it, if this lists anything.
    pub statusbar2: Vec<String>,
    /// Nicks in a column this many characters wide (longer ones are cut
    /// short), so every message starts in line; 0 puts them inline,
    /// `<nick> text`.
    pub nick_column: usize,
    /// Show joins, parts, and nick changes.
    pub show_joins: bool,
    /// Page titles under links, fetched by your homeserver: "unencrypted"
    /// (rooms the server can read anyway), "all", or "off".
    pub link_titles: LinkTitles,
    /// Make links real hyperlinks (OSC 8) in terminals that have them.
    pub hyperlinks: bool,
    /// Fold runs of joins, leaves and renames into one summary line.
    pub fold_joins: bool,
    /// Render outgoing messages as Markdown.
    pub markdown: bool,
    /// Tell others when you are typing.
    pub send_typing: bool,
    /// Read receipts for the window you are looking at: "public" (others
    /// see how far you've read), "private" (only your own clients), "off".
    pub read_receipts: ReceiptMode,
    /// Ring the terminal bell: true, false, or "away" (only while the
    /// terminal is in the background).
    pub bell: BellMode,
    /// "sound" (the terminal's bell), "visual" (the status bar flashes) or
    /// "both".
    pub bell_style: BellStyle,
    /// What rings: "mentions" (mentions, DMs, rooms set to /notify all),
    /// "invites", "verification", "failed_sends", "transfers".
    pub bell_for: Vec<BellKind>,
    /// Desktop notifications for highlights and DMs while you're looking
    /// elsewhere: "auto" (detect the terminal), "osc9", "osc777", "osc99"
    /// (kitty), or "off".
    pub notifications: NotifyMode,
    /// Include the message text in notifications.
    pub notify_preview: bool,
    /// Lines kept per window.
    pub scrollback: usize,
    /// Events fetched per history request.
    pub history_page: u16,
    /// Character appended after a nick completed at the start of the line.
    pub completion_char: String,
    /// Show images inline (kitty, iTerm2 or sixel graphics where the
    /// terminal supports them, coloured half-blocks otherwise).
    pub image_previews: bool,
    /// Maximum height of an inline image, in rows.
    pub image_rows: u16,
    /// How pictures are drawn in a terminal without graphics: "auto",
    /// "sextants", "quadrants" or "halfblocks".
    pub image_text: crate::picture::TextMode,
    /// The command that plays audio, the file on the end ("" picks the
    /// system's own: afplay on macOS, pw-play, paplay… on Linux).
    pub audio_player: String,
    /// The command that plays video, the file on the end ("" picks mpv if
    /// it's installed, else the system's app for the file).
    pub video_player: String,
    /// Write irssi-style daily logs of what arrives while connected. They
    /// hold decrypted messages in plaintext, so this is off by default.
    pub log: bool,
    /// Where logs go (default `~/.textrix/logs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
    /// How to receive updates: "auto" (sliding sync when the server offers
    /// it, else classic), "sliding", or "classic".
    pub sync: SyncMode,
    /// Window list on the left (F2 or /sidebar toggles it).
    pub sidebar: bool,
    /// Member list on the right of group rooms (F3 or /nicklist toggles it).
    pub nicklist: bool,
    /// Room avatars in the window list (initials where images can't show).
    pub sidebar_avatars: bool,
    /// Wheel scrolling and clicking (hold Shift, or Option in iTerm2, to
    /// select text while it's on).
    pub mouse: bool,
    /// Ask before sending a paste longer than this many lines (0: never).
    pub paste_confirm_lines: usize,
    /// Encrypt each account's local store (keys, session, history cache)
    /// with a key kept in the system keychain.
    pub encrypt_store: bool,
    /// Load the scripts in ~/.textrix/scripts at startup.
    pub scripts: bool,
    /// Keep an encrypted local index of decrypted messages for /search
    /// (needs the encrypted store).
    pub search_index: bool,
    /// Once an hour, ask GitHub whether a newer Textrix is out.
    pub check_updates: bool,
    /// Keep recent messages in the local store so windows fill instantly at
    /// startup. They're stored decrypted, next to the keys and session
    /// that can decrypt them anyway; off clears what's stored.
    pub history_cache: bool,
    /// The terminal's title: {window}, {number}, {server}, and {activity}
    /// (" [3]" when other windows have new messages). Empty leaves the
    /// title alone.
    pub title: String,
    /// Colours: a built-in theme ("default", "light", "green-tron",
    /// "turbo", "dracula"… /theme lists them) or a theme file's name, from
    /// `themes/<name>.toml` next to this file.
    pub theme: ThemeId,
    /// Your own commands (`/alias`): name -> what it runs.
    pub aliases: BTreeMap<String, String>,
    /// Scripts' settings, by script: `[script_settings.watchwords]`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub script_settings: BTreeMap<String, toml::Table>,
    /// Colour overrides, e.g. `link = "lightcyan"`, `bar_bg = "#203050"`.
    pub colors: BTreeMap<String, String>,
    /// `theme` with `colors` applied (worked out when loading).
    #[serde(skip)]
    pub palette: Theme,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timestamp_format: "%H:%M".into(),
            nick_column: 12,
            statusbar: DEFAULT_STATUSBAR.map(String::from).to_vec(),
            statusbar2: Vec::new(),
            editor: String::new(),
            system_italic: true,
            away_notes: true,
            show_joins: true,
            link_titles: LinkTitles::Unencrypted,
            hyperlinks: true,
            fold_joins: true,
            markdown: true,
            send_typing: true,
            read_receipts: ReceiptMode::Public,
            bell: BellMode::On,
            bell_style: BellStyle::Sound,
            bell_for: BellKind::ALL.to_vec(),
            notifications: NotifyMode::Auto,
            notify_preview: true,
            scrollback: 5000,
            history_page: 50,
            completion_char: ":".into(),
            log: false,
            log_dir: None,
            sync: SyncMode::Auto,
            image_previews: true,
            image_rows: 10,
            image_text: crate::picture::TextMode::Auto,
            audio_player: String::new(),
            video_player: String::new(),
            sidebar: true,
            nicklist: true,
            sidebar_avatars: true,
            mouse: true,
            paste_confirm_lines: 5,
            history_cache: true,
            check_updates: false,
            scripts: true,
            search_index: true,
            title: "{window}{activity} — Textrix".into(),
            encrypt_store: true,
            theme: ThemeId::default(),
            colors: BTreeMap::new(),
            aliases: BTreeMap::new(),
            script_settings: BTreeMap::new(),
            palette: Theme::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BellMode {
    On,
    Off,
    /// Only while the terminal is in the background.
    Away,
}

impl Serialize for BellMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            BellMode::On => s.serialize_bool(true),
            BellMode::Off => s.serialize_bool(false),
            BellMode::Away => s.serialize_str("away"),
        }
    }
}

/// `bell = true`, `false` or `"away"` (also "on"/"off").
impl<'de> Deserialize<'de> for BellMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Flag(true) => Ok(BellMode::On),
            Raw::Flag(false) => Ok(BellMode::Off),
            Raw::Word(word) => match word.as_str() {
                "on" => Ok(BellMode::On),
                "off" => Ok(BellMode::Off),
                "away" => Ok(BellMode::Away),
                other => Err(serde::de::Error::custom(format!("bell is true, false or \"away\", not {other:?}"))),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkTitles {
    Off,
    /// Only where the server already sees the links.
    Unencrypted,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BellStyle {
    Sound,
    Visual,
    Both,
}

/// Things that can ring the bell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BellKind {
    /// Mentions, highlight words, DMs, and rooms set to /notify all.
    Mentions,
    Invites,
    Verification,
    FailedSends,
    /// Finished uploads and downloads.
    Transfers,
}

impl BellKind {
    pub const ALL: [BellKind; 5] =
        [BellKind::Mentions, BellKind::Invites, BellKind::Verification, BellKind::FailedSends, BellKind::Transfers];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptMode {
    Public,
    Private,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Auto,
    Sliding,
    Classic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    Auto,
    Osc9,
    Osc777,
    Osc99,
    Off,
}

/// Write the settings in `cfg` that differ from what config.toml says into
/// it, keeping the rest of the file (comments, layout, other keys) as it is.
/// Returns the keys written. Tables ([colors], [aliases]) are left alone:
/// they're edited in the file, or by /alias.
pub fn save_settings(path: &Path, cfg: &Config) -> Result<Vec<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let on_disk: Config =
        toml::from_str(&text).with_context(|| format!("{} has an error; fix it first", path.display()))?;
    let (want, have) = (cfg.to_table()?, on_disk.to_table()?);
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    let mut written = Vec::new();
    for (key, value) in &want {
        if value.is_table() || have.get(key) == Some(value) {
            continue;
        }
        let value: toml_edit::Value = value.to_string().parse().with_context(|| format!("writing {key}"))?;
        doc[key.as_str()] = toml_edit::Item::Value(value);
        written.push(key.clone());
    }
    for (script, settings) in &cfg.script_settings {
        for (name, value) in settings {
            if on_disk.script_settings.get(script).and_then(|t| t.get(name)) == Some(value) {
                continue;
            }
            let all = doc.entry("script_settings").or_insert_with(|| implicit_table().into());
            let table = all.as_table_mut().context("script_settings in config.toml isn't a table")?;
            let table = table.entry(script).or_insert_with(|| toml_edit::Table::new().into());
            let table = table.as_table_mut().with_context(|| format!("script_settings.{script} isn't a table"))?;
            let value: toml_edit::Value = value.to_string().parse().with_context(|| format!("writing {name}"))?;
            table[name.as_str()] = toml_edit::Item::Value(value);
            written.push(format!("{script}.{name}"));
        }
    }
    if !written.is_empty() {
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(written)
}

/// A table that only appears as the prefix of its subtables
/// (`[script_settings.x]`, with no `[script_settings]` line).
fn implicit_table() -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    table
}

/// Add, change (`Some`) or remove (`None`) an alias in config.toml, keeping
/// the rest of the file (comments and layout) as it is.
pub fn save_alias(path: &Path, name: &str, expansion: Option<&str>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    let aliases = doc
        .entry("aliases")
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .with_context(|| format!("aliases in {} isn't a table", path.display()))?;
    match expansion {
        Some(expansion) => {
            aliases.insert(name, toml_edit::value(expansion));
        }
        None => {
            aliases.remove(name);
            if aliases.is_empty() {
                doc.remove("aliases");
            }
        }
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// What a status bar can show.
pub const STATUS_ITEMS: [(&str, &str); 13] = [
    ("time", "the time"),
    ("date", "the date"),
    ("nick", "your name on this window's server"),
    ("window", "this window's number and name, (+E) when encrypted"),
    ("users", "how many are in this room"),
    ("activity", "windows with something new: Act: 2,7"),
    ("typing", "who's typing here"),
    ("scripts", "scripts' items"),
    ("upgrade", "a newer Textrix installed, or out"),
    ("playing", "audio playing"),
    ("servers", "away, unverified, and connection trouble"),
    ("theme", "the theme on screen"),
    ("more", "scrolled up: there's more below"),
];

pub const DEFAULT_STATUSBAR: [&str; 10] =
    ["time", "nick", "window", "activity", "typing", "scripts", "upgrade", "playing", "servers", "more"];

impl Config {
    /// Settings that the types alone don't check.
    fn check(&self) -> Result<()> {
        for (key, items) in [("statusbar", &self.statusbar), ("statusbar2", &self.statusbar2)] {
            if let Some(bad) = items.iter().find(|i| !STATUS_ITEMS.iter().any(|(name, _)| name == i)) {
                let names: Vec<&str> = STATUS_ITEMS.iter().map(|(n, _)| *n).collect();
                anyhow::bail!("{key}: there's no “{bad}”; the items are {}", names.join(", "));
            }
        }
        Ok(())
    }

    /// Every setting as TOML.
    pub fn to_table(&self) -> Result<toml::Table> {
        Ok(toml::Table::try_from(self)?)
    }

    /// Settings from TOML, checked like config.toml (unknown keys and wrong
    /// types are errors); theme files come from `themes`.
    pub fn from_table(table: toml::Table, themes: &Path) -> Result<Self> {
        let mut cfg: Self = table.try_into()?;
        cfg.check()?;
        cfg.palette = Theme::load(&cfg.theme, &cfg.colors, themes)?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                cfg.check().with_context(|| format!("in {}", path.display()))?;
                let themes = path.with_file_name("themes");
                cfg.palette =
                    Theme::load(&cfg.theme, &cfg.colors, &themes).with_context(|| format!("in {}", path.display()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bell_settings_parse() {
        let parse = |text: &str| toml::from_str::<Config>(text);
        assert_eq!(parse("bell = true").unwrap().bell, BellMode::On);
        assert_eq!(parse("bell = false").unwrap().bell, BellMode::Off);
        assert_eq!(parse("bell = \"away\"").unwrap().bell, BellMode::Away);
        assert!(parse("bell = \"loud\"").is_err());
        let cfg = parse("bell_style = \"both\"\nbell_for = [\"invites\", \"failed_sends\"]").unwrap();
        assert_eq!(cfg.bell_style, BellStyle::Both);
        assert_eq!(cfg.bell_for, [BellKind::Invites, BellKind::FailedSends]);
        assert_eq!(Config::default().bell_for.len(), 5, "everything rings by default");
    }

    #[test]
    fn save_writes_only_changed_settings_and_keeps_the_file() {
        let dir = std::env::temp_dir().join(format!("textrix-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\nbell = false   # quiet\n\n[aliases]\ntt = \"/window 10\"\n").unwrap();
        let mut cfg = Config::load(&path).unwrap();
        assert!(save_settings(&path, &cfg).unwrap().is_empty(), "nothing changed");

        cfg.theme = crate::theme::ThemeName::RedTron.into();
        cfg.image_rows = 12;
        let written = save_settings(&path, &cfg).unwrap();
        assert_eq!(written, ["image_rows", "theme"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\nbell = false   # quiet\n"), "{text}");
        assert!(text.contains("theme = \"red-tron\"") && text.contains("image_rows = 12"), "{text}");
        assert!(text.contains("[aliases]\ntt = \"/window 10\""), "{text}");
        let back = Config::load(&path).unwrap();
        assert_eq!(
            (back.theme, back.image_rows, back.bell),
            (crate::theme::ThemeName::RedTron.into(), 12, BellMode::Off)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let table = Config::default().to_table().unwrap();
        assert_eq!(table["bell"], toml::Value::Boolean(true));
        assert_eq!(table["theme"], toml::Value::String("default".into()));
        let mut changed = table.clone();
        changed.insert("bell".into(), toml::Value::String("away".into()));
        assert_eq!(Config::from_table(changed, Path::new("")).unwrap().bell, BellMode::Away);
        let mut wrong = table;
        wrong.insert("belll".into(), toml::Value::Boolean(true));
        assert!(Config::from_table(wrong, Path::new("")).is_err(), "unknown settings are errors");
    }
}

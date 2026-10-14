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
use serde::Deserialize;

use crate::theme::{Theme, ThemeName};

#[derive(Clone, Debug)]
pub struct Paths {
    pub base: PathBuf,
    pub env_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let base = match std::env::var_os("TEXTRIX_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir().context("cannot locate home directory")?.join(".textrix"),
        };
        let env_file = match std::env::var_os("TEXTRIX_ENV") {
            Some(path) => PathBuf::from(path),
            None if cfg!(debug_assertions) => Path::new(env!("CARGO_MANIFEST_DIR")).join(".textrix"),
            None => base.join(".env"),
        };
        Ok(Self { base, env_file })
    }

    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.toml")
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

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// strftime format for the timestamp column.
    pub timestamp_format: String,
    /// Show joins, parts, and nick changes.
    pub show_joins: bool,
    /// Render outgoing messages as Markdown.
    pub markdown: bool,
    /// Tell others when you are typing.
    pub send_typing: bool,
    /// Read receipts for the window you are looking at: "public" (others
    /// see how far you've read), "private" (only your own clients), "off".
    pub read_receipts: ReceiptMode,
    /// Ring the terminal bell on highlights and direct messages.
    pub bell: bool,
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
    /// Write irssi-style daily logs of what arrives while connected. They
    /// hold decrypted messages in plaintext, so this is off by default.
    pub log: bool,
    /// Where logs go (default `~/.textrix/logs`).
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
    /// Built-in colours: "default" (dark backgrounds), "light", or "mono".
    pub theme: ThemeName,
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
            show_joins: true,
            markdown: true,
            send_typing: true,
            read_receipts: ReceiptMode::Public,
            bell: true,
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
            sidebar: true,
            nicklist: true,
            sidebar_avatars: true,
            mouse: true,
            paste_confirm_lines: 5,
            theme: ThemeName::Default,
            colors: BTreeMap::new(),
            palette: Theme::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptMode {
    Public,
    Private,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Auto,
    Sliding,
    Classic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    Auto,
    Osc9,
    Osc777,
    Osc99,
    Off,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                cfg.palette = Theme::build(cfg.theme, &cfg.colors).with_context(|| format!("in {}", path.display()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

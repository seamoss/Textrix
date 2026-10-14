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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

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
    /// Lines kept per window.
    pub scrollback: usize,
    /// Events fetched per history request.
    pub history_page: u16,
    /// Character appended after a nick completed at the start of the line.
    pub completion_char: String,
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
            scrollback: 5000,
            history_page: 50,
            completion_char: ":".into(),
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

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

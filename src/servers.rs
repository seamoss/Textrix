//! Server definitions from the private dotfile (`~/.textrix/.env`).
//!
//! Only the non-secret fields (homeserver, user) are parsed into long-lived
//! state. Secrets are looked up at the moment an API call needs them, wrapped
//! in [`SecretString`] (zeroized on drop, redacted in `Debug`), and handed
//! straight to the SDK.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use secrecy::SecretString;
use zeroize::Zeroizing;

const PREFIX: &str = "TEXTRIX_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Homeserver,
    User,
    Password,
    RecoveryKey,
    KeysPassphrase,
}

impl Field {
    const ALL: [Field; 5] =
        [Field::Homeserver, Field::User, Field::Password, Field::RecoveryKey, Field::KeysPassphrase];

    fn suffix(self) -> &'static str {
        match self {
            Field::Homeserver => "_HOMESERVER",
            Field::User => "_USER",
            Field::Password => "_PASSWORD",
            Field::RecoveryKey => "_RECOVERY_KEY",
            Field::KeysPassphrase => "_KEYS_PASSPHRASE",
        }
    }

    fn is_secret(self) -> bool {
        matches!(self, Field::Password | Field::RecoveryKey | Field::KeysPassphrase)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Secret {
    Password,
    RecoveryKey,
    /// Encrypts room key export files (`/keys`).
    KeysPassphrase,
}

impl From<Secret> for Field {
    fn from(s: Secret) -> Self {
        match s {
            Secret::Password => Field::Password,
            Secret::RecoveryKey => Field::RecoveryKey,
            Secret::KeysPassphrase => Field::KeysPassphrase,
        }
    }
}

/// A server as defined in the dotfile, minus its secrets.
#[derive(Clone, Debug)]
pub struct ServerDef {
    /// Lower-cased name used by `/connect <name>`.
    pub name: String,
    pub homeserver: Option<String>,
    pub user: String,
}

impl ServerDef {
    /// The homeserver to contact: the explicit one, or the server part of
    /// the user ID (resolved via `.well-known`).
    pub fn homeserver_or_server_name(&self) -> Option<&str> {
        self.homeserver.as_deref().or_else(|| self.user.split_once(':').map(|(_, server)| server))
    }
}

#[derive(Default, Debug)]
pub struct Servers {
    pub defs: Vec<ServerDef>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct EnvFile {
    path: PathBuf,
}

impl EnvFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parse server definitions. Secret values are skipped without being
    /// copied out of the (zeroized) file buffer. A missing file is an error:
    /// the dotfile is the only source of connections.
    pub fn servers(&self) -> Result<Servers> {
        let mut out = Servers::default();
        let Some(text) = self.read()? else {
            bail!("no server config at {}", self.path.display());
        };

        let mut defs: Vec<ServerDef> = Vec::new();

        for (lineno, line) in text.lines().enumerate() {
            let Some((key, raw_value)) = split_assignment(line) else { continue };
            let Some((name, field)) = parse_key(key) else {
                if key.starts_with(PREFIX) {
                    out.warnings.push(format!("{}:{}: unknown key {key}", self.path.display(), lineno + 1));
                }
                continue;
            };
            let idx = match defs.iter().position(|d| d.name == name) {
                Some(i) => i,
                None => {
                    defs.push(ServerDef { name, homeserver: None, user: String::new() });
                    defs.len() - 1
                }
            };
            if field.is_secret() {
                continue;
            }
            let value = unquote(raw_value);
            let value = env_override(key).unwrap_or(value);
            match field {
                Field::Homeserver => defs[idx].homeserver = Some(value).filter(|v| !v.is_empty()),
                Field::User => defs[idx].user = value,
                Field::Password | Field::RecoveryKey | Field::KeysPassphrase => unreachable!(),
            }
        }

        for def in defs {
            if def.user.is_empty() {
                out.warnings.push(format!(
                    "server '{}' has no TEXTRIX_{}_USER; skipping it",
                    def.name,
                    def.name.to_uppercase()
                ));
            } else {
                out.defs.push(def);
            }
        }
        if out.defs.is_empty() {
            let why = out.warnings.iter().map(|w| format!("\n  {w}")).collect::<String>();
            bail!("{} defines no usable servers{why}", self.path.display());
        }
        Ok(out)
    }

    /// Fetch one secret for one server. Process environment wins over the
    /// file. Returns `Ok(None)` when unset or empty.
    pub fn secret(&self, server: &str, which: Secret) -> Result<Option<SecretString>> {
        let key = format!("{PREFIX}{}{}", server.to_uppercase(), Field::from(which).suffix());

        if let Some(value) = std::env::var_os(&key) {
            let value = Zeroizing::new(value.into_string().unwrap_or_default());
            return Ok(non_empty(&value));
        }

        let Some(text) = self.read()? else { return Ok(None) };
        self.check_private()?;
        for line in text.lines() {
            if let Some((k, raw)) = split_assignment(line)
                && k == key
            {
                let value = Zeroizing::new(unquote(raw));
                return Ok(non_empty(&value));
            }
        }
        Ok(None)
    }

    fn read(&self) -> Result<Option<Zeroizing<String>>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(Some(Zeroizing::new(s))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Refuse to hand out secrets from a file other users can read, the
    /// same way ssh treats private keys.
    #[cfg(unix)]
    fn check_private(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&self.path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is accessible by other users (mode {:o}); run: chmod 600 {}",
                self.path.display(),
                mode & 0o777,
                self.path.display()
            );
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn check_private(&self) -> Result<()> {
        Ok(())
    }
}

fn non_empty(value: &str) -> Option<SecretString> {
    (!value.is_empty()).then(|| SecretString::from(value.to_owned()))
}

fn env_override(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `TEXTRIX_WORK_PASSWORD` -> `("work", Password)`.
fn parse_key(key: &str) -> Option<(String, Field)> {
    let rest = key.strip_prefix(PREFIX)?;
    Field::ALL.into_iter().find_map(|field| {
        let name = rest.strip_suffix(field.suffix())?;
        (!name.is_empty()).then(|| (name.to_lowercase(), field))
    })
}

/// Split `KEY=value` (optionally prefixed with `export`), ignoring blanks
/// and comments. The value is returned raw (still quoted).
fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

fn unquote(raw: &str) -> String {
    if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.split_once('\'')) {
        return inner.0.to_owned();
    }
    if let Some(inner) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        return out;
    }
    // Unquoted: a ` #` starts a trailing comment.
    match raw.find(" #") {
        Some(i) => raw[..i].trim_end().to_owned(),
        None => raw.to_owned(),
    }
}

/// Create or replace a file readable only by the current user.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_into_server_and_field() {
        assert_eq!(parse_key("TEXTRIX_0X4F02_USER"), Some(("0x4f02".into(), Field::User)));
        assert_eq!(parse_key("TEXTRIX_MY_WORK_RECOVERY_KEY"), Some(("my_work".into(), Field::RecoveryKey)));
        assert_eq!(parse_key("TEXTRIX_HOME_KEYS_PASSPHRASE"), Some(("home".into(), Field::KeysPassphrase)));
        assert_eq!(parse_key("TEXTRIX__USER"), None);
        assert_eq!(parse_key("OTHER_USER"), None);
    }

    #[test]
    fn values_unquote() {
        assert_eq!(unquote("plain # comment"), "plain");
        assert_eq!(unquote("'single # kept'"), "single # kept");
        assert_eq!(unquote(r#""dq \"x\" # kept""#), "dq \"x\" # kept");
        assert_eq!(split_assignment("export A = b"), Some(("A", "b")));
        assert_eq!(split_assignment("# A=b"), None);
    }

    #[test]
    fn servers_skip_secrets_and_read_them_on_demand() {
        let dir = std::env::temp_dir().join(format!("textrix-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        write_private(&path, b"TEXTRIX_A_USER=@me:a.org\nTEXTRIX_A_PASSWORD='hunter2'\nTEXTRIX_B_HOMESERVER=x\n")
            .unwrap();
        let env = EnvFile::new(path.clone());
        let servers = env.servers().unwrap();
        assert_eq!(servers.defs.len(), 1);
        assert_eq!(servers.defs[0].homeserver_or_server_name(), Some("a.org"));
        assert_eq!(servers.warnings.len(), 1, "server b has no user");

        use secrecy::ExposeSecret;
        let pw = env.secret("a", Secret::Password).unwrap().unwrap();
        assert_eq!(pw.expose_secret(), "hunter2");
        assert!(env.secret("a", Secret::RecoveryKey).unwrap().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(env.secret("a", Secret::Password).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
        assert!(EnvFile::new(path).servers().is_err(), "missing file is an error");
    }
}

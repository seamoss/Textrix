//! Bots: Textrix running in the background as another Matrix account (the
//! bot's), from a home of its own (`~/.textrix/bots/<name>`), doing what its
//! plugins do. Its rooms and keys stay out of yours, it runs whether or not
//! you do, and `textrix bot attach <name>` shows Textrix as the bot.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

/// Where bots live.
pub fn dir(base: &Path) -> PathBuf {
    base.join("bots")
}

/// A bot's home: its config.toml, store, plugins and log.
pub fn home(base: &Path, name: &str) -> PathBuf {
    dir(base).join(name)
}

/// The bot's account details: its own dotfile, never yours.
pub fn env_file(home: &Path) -> PathBuf {
    home.join(".env")
}

/// The bots there are, by name.
pub fn names(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir(base))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("config.toml").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

pub fn running(home: &Path) -> bool {
    crate::remote::alive(&crate::remote::socket_path(home))
}

/// Make a bot's home: a dotfile to put its account in, a config.toml that
/// suits a bot (no bells or notifications; invitations accepted from
/// `invited_by`), and a plugins folder.
pub fn create(base: &Path, name: &str, invited_by: &[String]) -> Result<PathBuf> {
    ensure!(crate::plugin::name_ok(name), "“{name}” won't do as a bot's name: letters, digits, - and _");
    let home = home(base, name);
    ensure!(!home.exists(), "there's already a bot {name} ({})", home.display());
    crate::config::create_private_dir(&dir(base))?;
    crate::config::create_private_dir(&home)?;
    std::fs::create_dir_all(home.join("plugins")).context("making its plugins folder")?;

    // Its account on your server, if we know yours.
    let server = invited_by.iter().find_map(|id| id.split_once(':').map(|(_, s)| s.to_owned()));
    let user = format!("@{name}:{}", server.as_deref().unwrap_or("example.org"));
    let env = format!(
        "# The Matrix account the bot {name} uses. Make it on your server first\n\
         # (it's a separate account: don't use your own), then fill these in.\n\
         TEXTRIX_BOT_USER={user}\n\
         TEXTRIX_BOT_PASSWORD=\n\
         # TEXTRIX_BOT_HOMESERVER=https://matrix.example.org   # if it isn't found from the user ID\n"
    );
    crate::servers::write_private(&env_file(&home), env.as_bytes())?;

    let invited: Vec<String> = invited_by.iter().map(|id| format!("{id:?}")).collect();
    let config = format!(
        "# {name}: a bot. Textrix in the background as its own Matrix account,\n\
         # doing what its plugins do. `textrix bot attach {name}` shows it.\n\
         notifications = \"off\"\n\
         bell = false\n\
         check_updates = false\n\
         # It joins rooms when these Matrix IDs invite it (\"*\": anyone; careful).\n\
         accept_invites_from = [{}]\n\
         \n\
         # Its plugins (PLUGINS.md), e.g. from `textrix bot attach {name}` then /plugin new:\n\
         # [plugins.helper]\n\
         # command = \"python3 helper.py\"\n\
         # rooms = [\"*\"]\n",
        invited.join(", ")
    );
    std::fs::write(home.join("config.toml"), config).context("writing its config.toml")?;
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bot_gets_a_home_of_its_own() {
        let base = std::env::temp_dir().join(format!("textrix-bots-{}", std::process::id()));
        let home = create(&base, "codebot", &["@me:example.org".to_owned()]).unwrap();
        assert_eq!(home, base.join("bots/codebot"));
        assert_eq!(names(&base), ["codebot"]);
        let env = std::fs::read_to_string(env_file(&home)).unwrap();
        assert!(env.contains("TEXTRIX_BOT_USER=@codebot:example.org"), "on your server: {env}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(env_file(&home)).unwrap().permissions().mode() & 0o777, 0o600);
        let cfg = crate::config::Config::load(&home.join("config.toml")).unwrap();
        assert_eq!(cfg.accept_invites_from, ["@me:example.org"]);
        assert_eq!(cfg.notifications, crate::config::NotifyMode::Off);
        assert!(!running(&home));
        assert!(create(&base, "codebot", &[]).is_err(), "not over another");
        assert!(create(&base, "../x", &[]).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}

//! `textrix setup`: the dotfile for your first account, or another one, by
//! answering a few questions. It never asks for your password: Textrix's
//! secrets come only from the dotfile (or a password manager), never typed
//! into it, so setup leaves that line for you to fill in, and offers your
//! editor to do it.
//!
//! An existing dotfile is only ever added to, never rewritten.

use std::{
    io::{BufRead, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use matrix_sdk::{Client, ruma::OwnedUserId};

use crate::{
    config::{Config, Paths, create_private_dir},
    servers::{EnvFile, Secret, write_private},
};

/// How long the homeserver has to answer.
const WAIT: Duration = Duration::from_secs(20);

pub async fn run(paths: &Paths) -> Result<()> {
    let env = EnvFile::new(paths.env_file.clone());
    println!("Textrix setup: a Matrix account in {}.", paths.env_file.display());
    println!("It never asks for your password: that goes in that file (or a password manager), never into Textrix.");
    println!();
    let existing = if paths.env_file.exists() { env.servers().map(|s| s.defs).unwrap_or_default() } else { Vec::new() };
    if !existing.is_empty() {
        println!("Already there:");
        for def in &existing {
            println!("  {:<12} {}", def.name, def.user);
        }
        if !yes(&ask("Add another account? [y/N]", "n")?) {
            return check(paths, existing.first().map(|d| d.name.as_str()));
        }
    }

    let user = loop {
        match user_id(&ask("Your Matrix ID (like @you:example.org)", "")?) {
            Ok(user) => break user,
            Err(e) => println!("  {e}"),
        }
    };
    if let Some(def) = existing.iter().find(|d| d.user == user.as_str()) {
        println!("{user} is there already, as {}.", def.name);
        return check(paths, Some(&def.name));
    }

    let mut server = user.server_name().to_string();
    let homeserver = loop {
        println!("Finding {server}'s homeserver…");
        match find_homeserver(&server).await {
            Ok(found) => break found,
            Err(e) => {
                println!("  {e:#}");
                server = ask("Its homeserver's address (like https://matrix.example.org)", "")?;
                if server.is_empty() {
                    bail!("no homeserver: run textrix setup again when you know it");
                }
            }
        }
    };
    println!("  {homeserver}: signs in with a password ✓");

    let taken: Vec<String> = existing.iter().map(|d| d.name.clone()).collect();
    let suggested = server_name(user.server_name().as_str(), &taken);
    let name = loop {
        let answer = ask(&format!("A short name for it, for /connect <name> [{suggested}]"), &suggested)?;
        match check_name(&answer, &taken) {
            Ok(name) => break name,
            Err(e) => println!("  {e}"),
        }
    };

    write_account(&paths.env_file, &name, &user, &homeserver)?;
    let key = format!("TEXTRIX_{}_PASSWORD", name.to_uppercase());
    println!();
    println!("Wrote {} (only you can read it).", paths.env_file.display());
    println!("Now your password: on the line {key}=, or leave it empty and run");
    println!("Textrix with a password manager filling it in (the file says how).");
    let editor = crate::editor::command(&Config::load(&paths.config_file()).map(|c| c.editor).unwrap_or_default());
    // Only offered in a terminal: an editor needs one.
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && yes(&ask(&format!("Open it in {editor} now? [Y/n]"), "y")?) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&paths.env_file)
            .status()
            .with_context(|| format!("running {editor}"))?;
        if !status.success() {
            println!("  {editor} stopped with {status}");
        }
    }
    check(paths, Some(&name))
}

/// What's in place for `name`, and what's next.
fn check(paths: &Paths, name: Option<&str>) -> Result<()> {
    let env = EnvFile::new(paths.env_file.clone());
    println!();
    if let Some(name) = name {
        // Read only to say whether it's there; dropped (and wiped) at once.
        match env.secret(name, Secret::Password) {
            Ok(Some(_)) => println!("{name}: password set ✓"),
            Ok(None) => println!(
                "{name}: no password yet. Add it to TEXTRIX_{}_PASSWORD, or run Textrix through your password manager.",
                name.to_uppercase()
            ),
            Err(e) => println!("{name}: {e:#}"),
        }
    }
    println!("Next: run textrix to sign in (textrix --check looks things over first).");
    if cfg!(target_os = "macos") {
        println!("textrix app install adds Textrix to your Applications, for Alfred, Spotlight and the Dock.");
    }
    Ok(())
}

/// A question on the terminal; `default` when the answer's empty.
fn ask(question: &str, default: &str) -> Result<String> {
    print!("{question}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("setup needs answers: run it in a terminal");
    }
    let line = line.trim();
    Ok(if line.is_empty() { default.to_owned() } else { line.to_owned() })
}

fn yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// A Matrix ID from what was typed: `@you:example.org`, or without the @.
pub fn user_id(typed: &str) -> Result<OwnedUserId, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("type your Matrix ID: @you:example.org".into());
    }
    let full = if typed.starts_with('@') { typed.to_owned() } else { format!("@{typed}") };
    OwnedUserId::try_from(full.as_str())
        .map_err(|_| format!("“{typed}” isn't a Matrix ID: it looks like @you:example.org"))
}

/// The homeserver for `server` (found through its .well-known, the same way
/// signing in does), and that it signs in with a password.
async fn find_homeserver(server: &str) -> Result<String> {
    let found = async {
        let client = Client::builder().server_name_or_homeserver_url(server).build().await?;
        let homeserver = client.homeserver().to_string();
        let types = client.matrix_auth().get_login_types().await?;
        anyhow::Ok((homeserver, types))
    };
    let (homeserver, types) = tokio::time::timeout(WAIT, found)
        .await
        .map_err(|_| anyhow::anyhow!("{server} didn't answer in {}s", WAIT.as_secs()))??;
    use matrix_sdk::ruma::api::client::session::get_login_types::v3::LoginType;
    if !types.flows.iter().any(|f| matches!(f, LoginType::Password(_))) {
        bail!("{homeserver} doesn't sign in with a password (only single sign-on), which Textrix needs for now");
    }
    Ok(homeserver.trim_end_matches('/').to_owned())
}

/// A name for the server from its domain ("example.org" → "example"), not
/// one already `taken`.
pub fn server_name(domain: &str, taken: &[String]) -> String {
    let first = domain.split(['.', ':']).find(|p| !p.is_empty()).unwrap_or("home").to_lowercase();
    let mut base: String = first.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if base.trim_matches('_').is_empty() {
        base = "home".into();
    }
    (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}{n}") }).find(|n| !taken.contains(n)).unwrap()
}

/// A name that can be part of TEXTRIX_<NAME>_* lines.
pub fn check_name(typed: &str, taken: &[String]) -> Result<String, String> {
    let name = typed.trim().to_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("letters, digits and _ only".into());
    }
    if taken.contains(&name) {
        return Err(format!("{name} is taken already"));
    }
    Ok(name)
}

/// The dotfile lines for one account, its password left for you.
pub fn account_lines(name: &str, user: &OwnedUserId, homeserver: &str) -> String {
    let n = name.to_uppercase();
    format!(
        "# {user}, from textrix setup\n\
         TEXTRIX_{n}_HOMESERVER={homeserver}\n\
         TEXTRIX_{n}_USER={user}\n\
         TEXTRIX_{n}_PASSWORD=\n\
         # Optional: your recovery key (Element's Security Key), to read encrypted history\n\
         TEXTRIX_{n}_RECOVERY_KEY=\n"
    )
}

const HEADER: &str = "\
# Textrix's servers: every one here is connected at startup. Only you can
# read this file (Textrix won't read secrets from it otherwise).
#
# Secrets can come from a password manager instead of living here: a real
# environment variable of the same name wins over the file, e.g.
#   op run --env-file ~/.textrix/.env -- textrix
# with values like TEXTRIX_HOME_PASSWORD=\"op://Private/Matrix/password\".
";

/// Put the account in the dotfile: a new one (private, with a header) or
/// added to the end of the one there.
pub fn write_account(path: &Path, name: &str, user: &OwnedUserId, homeserver: &str) -> Result<()> {
    let lines = account_lines(name, user, homeserver);
    if !path.exists() {
        if let Some(dir) = path.parent() {
            create_private_dir(dir)?;
        }
        return write_private(path, format!("{HEADER}\n{lines}").as_bytes());
    }
    let mut file =
        std::fs::OpenOptions::new().append(true).open(path).with_context(|| format!("{}", path.display()))?;
    let before = std::fs::read(path).unwrap_or_default();
    let gap = if before.ends_with(b"\n\n") || before.is_empty() {
        ""
    } else if before.ends_with(b"\n") {
        "\n"
    } else {
        "\n\n"
    };
    file.write_all(format!("{gap}{lines}").as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_ids_with_or_without_the_at() {
        assert_eq!(user_id("@sam:example.org").unwrap().as_str(), "@sam:example.org");
        assert_eq!(user_id(" sam:example.org ").unwrap().as_str(), "@sam:example.org");
        assert!(user_id("sam").unwrap_err().contains("isn't a Matrix ID"));
        assert!(user_id("").is_err());
    }

    #[test]
    fn names_come_from_the_domain_and_dont_clash() {
        assert_eq!(server_name("example.org", &[]), "example");
        assert_eq!(server_name("Matrix.ORG", &["matrix".into()]), "matrix2");
        assert_eq!(server_name("my-server.net", &[]), "my_server");
        assert_eq!(server_name("localhost:8448", &[]), "localhost");
        assert_eq!(check_name("Work", &[]), Ok("work".into()));
        assert!(check_name("my server", &[]).is_err());
        assert!(check_name("home", &["home".into()]).unwrap_err().contains("taken"));
    }

    #[test]
    fn the_dotfile_is_made_private_and_only_added_to() {
        let dir = std::env::temp_dir().join(format!("textrix-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".env");
        let sam = user_id("@sam:example.org").unwrap();
        write_account(&path, "example", &sam, "https://matrix.example.org").unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Textrix's servers"), "{text}");
        assert!(text.contains("TEXTRIX_EXAMPLE_USER=@sam:example.org\nTEXTRIX_EXAMPLE_PASSWORD=\n"), "{text}");

        // Another: added, the rest as it was.
        let jo = user_id("@jo:work.example").unwrap();
        write_account(&path, "work", &jo, "https://matrix.work.example").unwrap();
        let both = std::fs::read_to_string(&path).unwrap();
        assert!(both.starts_with(&text), "nothing rewritten");
        let servers = EnvFile::new(path.clone()).servers().unwrap();
        let names: Vec<&str> = servers.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["example", "work"]);
        assert_eq!(servers.defs[1].homeserver.as_deref(), Some("https://matrix.work.example"));
        // The password lines are there, empty: nothing to hand out yet.
        assert!(EnvFile::new(path.clone()).secret("work", Secret::Password).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

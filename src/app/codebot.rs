//! `/codebot`: codebot (`examples/plugins/codebot.py`) in a bot of its own,
//! set up and started from here. `/codebot start <repo>` in a room writes
//! the plugin and its settings into the bot, grants it the room, starts the
//! bot (or restarts it, to read them) and invites it in. Making the bot's
//! Matrix account is still yours: Textrix can't make accounts.

use std::path::{Path, PathBuf};

use super::*;
use crate::plugin::{self, PluginConfig};

/// The bot codebot runs in, and its plugin's name there.
const BOT: &str = "codebot";

/// `/codebot help`: how to talk to it (CODEBOT.md has the rest).
const HELP: &[&str] = &[
    "codebot takes coding requests in a room, has a coding agent do them on a branch of its own, and opens a pull request",
    "  /codebot start <repo>   in a room: set it up there with that checkout (another repo: that room's), and start it",
    "  /codebot                how it's set up · /codebot stop · /codebot log (the end of its log)",
    "  in the room: codebot: <what to change>   (or !code <…>; codebot in <repo>: <…> for another repository)",
    "  in its thread: more to do carries on with the same pull request; go (if it waits for one), stop",
    "  codebot: status · codebot: repos · codebot: help",
    "  it runs the tests after the agent, hands failures back (fix_rounds), and never merges: read its pull requests",
    "  the whole story: CODEBOT.md in Textrix's repository",
];

impl App {
    /// `/codebot [start [repo] | stop | log]`.
    pub(super) fn cmd_codebot(&mut self, args: &str) -> Result<(), String> {
        if self.in_script > 0 {
            return Err("scripts can't manage bots, which run programs".into());
        }
        let (sub, rest) = split_first(args.trim());
        let home = crate::bots::home(&self.paths.base, BOT);
        match sub {
            "" | "status" => self.codebot_status(&home),
            "start" => self.codebot_start(&home, rest.trim()),
            "stop" => {
                self.bot_steps(BOT, &["stop"]);
                Ok(())
            }
            "log" => self.codebot_log(&home),
            "help" => {
                for line in HELP {
                    self.info(*line);
                }
                Ok(())
            }
            _ => Err("usage: /codebot [start [repo] | stop | log | help]".into()),
        }
    }

    /// Where codebot is, and what's next.
    fn codebot_status(&mut self, home: &Path) -> Result<(), String> {
        if !home.join("config.toml").is_file() {
            self.info("no codebot yet: /codebot start <repo> in the room it's for sets it up");
            return Ok(());
        }
        let user = bot_user(home);
        let running = crate::bots::running(home);
        self.info(format!(
            "codebot ({}): {}",
            user.as_deref().unwrap_or("no account in its .env yet"),
            if running { "running" } else { "stopped" }
        ));
        let cfg = crate::config::Config::load(&home.join("config.toml")).map_err(|e| format!("{e:#}"))?;
        let Some(plugin) = cfg.plugins.get(BOT) else {
            self.info("  not set up: /codebot start <repo> in the room it's for");
            return Ok(());
        };
        let text = |table: &toml::Table, key: &str| table.get(key).and_then(toml::Value::as_str).map(str::to_owned);
        let describe = |table: &toml::Table| {
            let base = text(table, "base").unwrap_or_else(|| "main".into());
            let tests = text(table, "test").map_or_else(|| "no tests".into(), |t| format!("tests `{t}`"));
            format!("pull requests to {base}, {tests}")
        };
        let settings = &plugin.settings;
        match text(settings, "repo").filter(|r| !r.is_empty()) {
            Some(main) => self.info(format!("  repo {main} (the main one): {}", describe(settings))),
            None if settings.get("repos").is_none() => self.info("  no repository yet: /codebot start <repo>"),
            None => {}
        }
        if let Some(repos) = settings.get("repos").and_then(toml::Value::as_table) {
            for (name, entry) in repos.iter().filter_map(|(n, e)| e.as_table().map(|e| (n, e))) {
                let rooms: Vec<String> = entry
                    .get("rooms")
                    .and_then(toml::Value::as_array)
                    .map(|r| r.iter().filter_map(toml::Value::as_str).map(|r| self.room_title(r)).collect())
                    .unwrap_or_default();
                let path = text(entry, "path").unwrap_or_default();
                let whose = if rooms.is_empty() { String::new() } else { format!(", {}'s", rooms.join(", ")) };
                self.info(format!("  repo {name}: {path}{whose}: {}", describe(entry)));
            }
        }
        let agent =
            settings.get("agent").map_or_else(|| "claude".into(), |a| a.as_str().map_or(a.to_string(), str::to_owned));
        let rounds = settings.get("fix_rounds").and_then(toml::Value::as_integer).unwrap_or(2);
        self.info(format!("  agent {agent}; {rounds} more go{} at failing tests", if rounds == 1 { "" } else { "es" }));
        let rooms: Vec<String> = plugin.rooms.iter().map(|r| self.room_title(r)).collect();
        self.info(format!("  rooms {}", if rooms.is_empty() { "none".into() } else { rooms.join(", ") }));
        if let Some(allow) = plugin.settings.get("allow") {
            self.info(format!("  takes work from {allow}"));
        }
        if !running {
            self.info("  /codebot start starts it");
        }
        Ok(())
    }

    /// Set codebot up in this room and start it: the bot made if it isn't
    /// there yet (then its account is yours to make), the plugin and its
    /// settings written, the room granted, the bot started or restarted,
    /// and invited here.
    fn codebot_start(&mut self, home: &Path, repo: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx().map_err(|_| "go to the room codebot is for first".to_owned())?;
        if !home.join("config.toml").is_file() {
            let yours: Vec<String> = crate::servers::EnvFile::new(self.paths.env_file.clone())
                .servers()
                .map(|s| s.defs.into_iter().map(|d| d.user).collect())
                .unwrap_or_default();
            crate::bots::create(&self.paths.base, BOT, &yours).map_err(|e| format!("{e:#}"))?;
            let server = yours.first().and_then(|id| id.split_once(':')).map_or("your server", |(_, s)| s);
            self.info(format!(
                "made the bot codebot in {}. Make it a Matrix account on {server} (its own, not yours), put it in {}, \
                 then /codebot start again",
                home.display(),
                crate::bots::env_file(home).display()
            ));
            return Ok(());
        }
        let Setup { user, changed, repo: named } = set_up(home, room_id.as_str(), repo)?;
        let running = crate::bots::running(home);
        match (running, changed) {
            (false, _) => self.bot_steps(BOT, &["start"]),
            (true, true) => self.bot_steps(BOT, &["stop", "start"]),
            (true, false) => {}
        }
        let here = self.win().members.iter().any(|m| m.user_id == user);
        if !here {
            acct.moderate(room_id, Moderation::Invite, user, None);
        }
        let what = named.map(|name| format!(" on {name}")).unwrap_or_default();
        self.info(format!(
            "codebot{what}: {}{}; \"codebot: <what to do>\" here gives it work, /codebot says how it's set up",
            match (running, changed) {
                (false, _) => "starting",
                (true, true) => "restarting with its new settings",
                (true, false) => "running, set up as before",
            },
            if here { "" } else { ", invited here" }
        ));
        Ok(())
    }

    /// The end of codebot's log: what it printed, errors too.
    fn codebot_log(&mut self, home: &Path) -> Result<(), String> {
        let path = home.join("plugins").join(format!("{BOT}.log"));
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        self.info(format!("the end of {}:", path.display()));
        for line in &lines[lines.len().saturating_sub(20)..] {
            self.info(format!("  {line}"));
        }
        Ok(())
    }

    /// A room's name, if it's one of ours, else as given.
    fn room_title(&self, room: &str) -> String {
        self.windows
            .iter()
            .find(|w| w.room.as_ref().is_some_and(|k| k.room_id.as_str() == room))
            .map_or_else(|| room.to_owned(), |w| w.title().to_owned())
    }

    /// `textrix bot <step> <name>` for each step in turn, saying what each
    /// said in the status window. The bot gets this Textrix's environment
    /// (its PATH finds the agent, git and gh), and always its own --env.
    pub(super) fn bot_steps(&mut self, name: &str, steps: &[&str]) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => return self.error(format!("couldn't find textrix to run: {e}")),
        };
        let (base, env) = (self.paths.base.clone(), self.paths.env_file.clone());
        let steps: Vec<String> = steps.iter().map(|s| (*s).to_owned()).collect();
        let (name, tx) = (name.to_owned(), self.tx.clone());
        tokio::spawn(async move {
            for step in steps {
                let mut command = tokio::process::Command::new(&exe);
                command.arg("--home").arg(&base).arg("--env").arg(&env);
                command.args(["bot", &step, &name]).stdin(std::process::Stdio::null());
                let (ok, said) = match command.output().await {
                    Ok(out) => {
                        let text = String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned();
                        (out.status.success(), text.lines().map(ToOwned::to_owned).collect::<Vec<_>>())
                    }
                    Err(e) => (false, vec![format!("couldn't run textrix bot: {e}")]),
                };
                for text in said {
                    let line = if ok {
                        Line::info(text)
                    } else {
                        Line::error(text.trim_start_matches("textrix: ").to_owned())
                    };
                    let _ = tx.send(AppEvent::Notice { account: None, room_id: None, line });
                }
                if !ok {
                    break;
                }
            }
        });
    }
}

/// What `set_up` did: the bot's Matrix ID, whether its config.toml or
/// codebot itself changed (so it restarts), and the repository it was
/// given, by the name codebot knows it by.
#[derive(Debug)]
struct Setup {
    user: OwnedUserId,
    changed: bool,
    repo: Option<String>,
}

/// codebot written into the bot at `home` for `room`: its program and the
/// helper (this version's), `[plugins.codebot]` made if it isn't, the room
/// added, and with `repo`, that checkout with its base branch and tests:
/// the main one if there's none yet (or it's that one), otherwise one of
/// `repos`, this room's.
fn set_up(home: &Path, room: &str, repo: &str) -> Result<Setup, String> {
    let user = bot_user(home)
        .ok_or_else(|| format!("put codebot's Matrix account in {} first", crate::bots::env_file(home).display()))?;
    let user = OwnedUserId::try_from(user.as_str()).map_err(|_| format!("{user} isn't a Matrix ID"))?;
    let config_path = home.join("config.toml");
    let current = crate::config::Config::load(&config_path).map_err(|e| format!("{e:#}"))?;
    let settings = current.plugins.get(BOT).map(|p| p.settings.clone()).unwrap_or_default();
    let main = settings.get("repo").and_then(toml::Value::as_str).filter(|r| !r.is_empty()).map(expand_home);
    let repos = settings.get("repos").and_then(toml::Value::as_table).cloned().unwrap_or_default();
    let mut set = toml::Table::new();
    let mut named = None;
    if !repo.is_empty() {
        let checkout = expand_home(repo);
        if !checkout.join(".git").exists() {
            return Err(format!("{} isn't a git checkout", checkout.display()));
        }
        let mut facts = toml::Table::new();
        facts.insert("base".into(), base_branch(&checkout).into());
        if let Some(test) = guess_tests(&checkout) {
            facts.insert("test".into(), test.into());
        }
        let main_name = main.as_deref().map(repo_name);
        let existing = repos.iter().find(|(_, t)| {
            t.get("path").and_then(toml::Value::as_str).is_some_and(|p| same_place(&expand_home(p), &checkout))
        });
        if main.as_deref().is_none_or(|m| same_place(m, &checkout)) && (main.is_some() || repos.is_empty()) {
            // The main one: every room's, unless a room has its own.
            set.insert("repo".into(), checkout.display().to_string().into());
            set.extend(facts);
            named = Some(repo_name(&checkout));
        } else {
            let name = match existing {
                Some((name, _)) => name.clone(),
                None => {
                    let base = repo_name(&checkout);
                    let taken = |n: &str| repos.contains_key(n) || main_name.as_deref() == Some(n);
                    (1..)
                        .map(|i| if i == 1 { base.clone() } else { format!("{base}-{i}") })
                        .find(|n| !taken(n))
                        .unwrap()
                }
            };
            facts.insert("path".into(), checkout.display().to_string().into());
            facts.insert("rooms".into(), toml::Value::Array(vec![room.into()]));
            let mut one = toml::Table::new();
            one.insert(name.clone(), toml::Value::Table(facts));
            set.insert("repos".into(), toml::Value::Table(one));
            named = Some(name);
        }
    } else if main.is_none() && repos.is_empty() {
        return Err("which repository? /codebot start ~/code/project (a git checkout whose origin is on GitHub)".into());
    }
    // It takes work from whoever it takes invitations from: you.
    let (program, template) =
        plugin::template("codebot", BOT, &current.accept_invites_from).ok_or("no codebot template")?;
    let plugins = home.join("plugins");
    plugin::install_helper(&plugins).map_err(|e| format!("{e:#}"))?;
    // Textrix's own: kept to this version's (change what it does in its
    // settings, or copy it under another name).
    let file = plugins.join(format!("{BOT}.py"));
    let updated = std::fs::read_to_string(&file).ok().as_deref() != Some(program.as_str());
    if updated {
        std::fs::write(&file, program).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    let made =
        PluginConfig { command: format!("python3 {BOT}.py"), rooms: Vec::new(), enabled: true, settings: template };
    let changed =
        crate::config::update_plugin(&config_path, BOT, &made, Some(room), &set).map_err(|e| format!("{e:#}"))?;
    Ok(Setup { user, changed: changed || updated, repo: named })
}

/// What codebot calls a checkout: its folder's name, as codebot.py does.
fn repo_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let name: String = name.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    if name.is_empty() { "repo".into() } else { name }
}

/// Whether two paths are the same folder.
fn same_place(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The bot's Matrix ID, from its own dotfile (only the user line is read).
fn bot_user(home: &Path) -> Option<String> {
    let servers = crate::servers::EnvFile::new(crate::bots::env_file(home)).servers().ok()?;
    servers.defs.into_iter().map(|d| d.user).find(|u| u.starts_with('@') && u.contains(':'))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// The branch a checkout's pull requests go to: origin's default, or the
/// one it's on.
fn base_branch(checkout: &Path) -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    git(&["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .and_then(|b| b.strip_prefix("origin/").map(ToOwned::to_owned))
        .or_else(|| git(&["branch", "--show-current"]))
        .unwrap_or_else(|| "main".into())
}

/// How a project's tests run, going by what's in it.
fn guess_tests(checkout: &Path) -> Option<&'static str> {
    let has = |name: &str| checkout.join(name).exists();
    let any_file = |test: &dyn Fn(&str) -> bool| {
        std::fs::read_dir(checkout).into_iter().flatten().flatten().any(|e| e.file_name().to_str().is_some_and(test))
    };
    if has("Cargo.toml") {
        Some("cargo test")
    } else if has("package.json") {
        Some("npm test")
    } else if has("go.mod") {
        Some("go test ./...")
    } else if std::fs::read_to_string(checkout.join("pyproject.toml")).is_ok_and(|t| t.contains("pytest")) {
        Some("python3 -m pytest -q")
    } else if has("pyproject.toml") || has("setup.py") || any_file(&|n| n.starts_with("test_") && n.ends_with(".py")) {
        Some("python3 -m unittest -q")
    } else if std::fs::read_to_string(checkout.join("Makefile"))
        .is_ok_and(|t| t.lines().any(|l| l.starts_with("test:")))
    {
        Some("make test")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codebot_is_written_into_its_bot_for_the_room() {
        let base = std::env::temp_dir().join(format!("textrix-codebot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = crate::bots::create(&base, BOT, &["@me:example.org".to_owned()]).unwrap();
        let checkout = base.join("project");
        std::fs::create_dir_all(&checkout).unwrap();
        let git = std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&checkout).status();
        assert!(git.unwrap().success());
        std::fs::write(checkout.join("test_todo.py"), "").unwrap();
        let repo = checkout.display().to_string();

        assert!(set_up(&home, "!r:example.org", "").unwrap_err().starts_with("which repository?"));
        let done = set_up(&home, "!r:example.org", &repo).unwrap();
        assert_eq!(done.user.as_str(), "@codebot:example.org");
        assert!(done.changed);
        assert_eq!(done.repo.as_deref(), Some("project"));
        assert!(home.join("plugins/codebot.py").is_file() && home.join("plugins/textrix.py").is_file());
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(text.contains("# codebot: a bot."), "the rest of the file as it was: {text}");
        let cfg = crate::config::Config::load(&home.join("config.toml")).unwrap();
        let plugin = &cfg.plugins[BOT];
        assert_eq!(
            (plugin.command.as_str(), plugin.rooms.as_slice()),
            ("python3 codebot.py", &["!r:example.org".to_owned()][..])
        );
        let setting = |key: &str| plugin.settings[key].clone();
        assert_eq!(setting("repo").as_str(), Some(repo.as_str()));
        assert_eq!(setting("base").as_str(), Some("main"));
        assert_eq!(setting("test").as_str(), Some("python3 -m unittest -q"));
        assert_eq!(setting("agent").as_str(), Some("claude"));
        assert_eq!(
            setting("allow"),
            toml::Value::Array(vec!["@me:example.org".into()]),
            "you, who it takes invitations from"
        );

        assert!(!set_up(&home, "!r:example.org", &repo).unwrap().changed, "nothing new");
        std::fs::write(home.join("plugins/codebot.py"), "# last version's").unwrap();
        assert!(set_up(&home, "!r:example.org", &repo).unwrap().changed, "codebot brought up to date: a restart");
        let again = set_up(&home, "!s:example.org", "").unwrap();
        assert!(again.changed && again.repo.is_none(), "another room, the same repository");
        let cfg = crate::config::Config::load(&home.join("config.toml")).unwrap();
        assert_eq!(cfg.plugins[BOT].rooms, ["!r:example.org", "!s:example.org"]);
        assert_eq!(cfg.plugins[BOT].settings["repo"].as_str(), Some(repo.as_str()));

        // Another checkout, in another room: one of `repos`, that room's.
        let other = base.join("Other");
        std::fs::create_dir_all(&other).unwrap();
        let git = std::process::Command::new("git").args(["init", "-q", "-b", "trunk"]).current_dir(&other).status();
        assert!(git.unwrap().success());
        std::fs::write(other.join("Cargo.toml"), "").unwrap();
        let added = set_up(&home, "!t:example.org", &other.display().to_string()).unwrap();
        assert_eq!(added.repo.as_deref(), Some("other"));
        let cfg = crate::config::Config::load(&home.join("config.toml")).unwrap();
        let settings = &cfg.plugins[BOT].settings;
        assert_eq!(settings["repo"].as_str(), Some(repo.as_str()), "the main one stays");
        let entry = &settings["repos"]["other"];
        assert_eq!(entry["path"].as_str(), Some(other.display().to_string().as_str()));
        assert_eq!((entry["base"].as_str(), entry["test"].as_str()), (Some("trunk"), Some("cargo test")));
        assert_eq!(entry["rooms"], toml::Value::Array(vec!["!t:example.org".into()]));
        // The same checkout from another room: that room too, not a copy.
        let via = other.join("..").join("Other").display().to_string();
        assert_eq!(set_up(&home, "!u:example.org", &via).unwrap().repo.as_deref(), Some("other"));
        let cfg = crate::config::Config::load(&home.join("config.toml")).unwrap();
        let repos = cfg.plugins[BOT].settings["repos"].as_table().unwrap().clone();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos["other"]["rooms"], toml::Value::Array(vec!["!t:example.org".into(), "!u:example.org".into()]));
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            text.contains("[plugins.codebot.settings.repos.other]")
                && !text.contains("[plugins.codebot.settings.repos]\n"),
            "{text}"
        );

        assert!(set_up(&home, "!r:example.org", "/nowhere/at/all").unwrap_err().contains("isn't a git checkout"));
        std::fs::remove_file(crate::bots::env_file(&home)).unwrap();
        assert!(set_up(&home, "!r:example.org", "").unwrap_err().contains("Matrix account"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn codebot_never_goes_in_your_own_textrix() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let paths = crate::config::Paths { base: std::env::temp_dir(), env_file: std::env::temp_dir().join("none") };
        let env = crate::servers::EnvFile::new(paths.env_file.clone());
        let mut app = App::new(crate::config::Config::default(), paths, env, crate::servers::Servers::default(), tx);
        let refused = app.cmd_plugin("new helper codebot").unwrap_err();
        assert!(refused.contains("/codebot start"), "{refused}");
    }

    #[test]
    fn tests_are_guessed_from_the_project() {
        let dir = std::env::temp_dir().join(format!("textrix-guess-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(guess_tests(&dir), None);
        std::fs::write(dir.join("test_todo.py"), "").unwrap();
        assert_eq!(guess_tests(&dir), Some("python3 -m unittest -q"));
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(guess_tests(&dir), Some("cargo test"), "a Rust project first");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

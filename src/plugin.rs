//! Plugins: programs Textrix starts and talks to over stdin and stdout, one
//! JSON-RPC 2.0 message per line, the way language servers and MCP do.
//!
//! A plugin is any program in any language, trusted like anything else you
//! run (unlike scripts, which are sandboxed Rhai). What it may do in Matrix
//! is what config.toml grants it: the rooms it hears, and posts in. The
//! protocol is described in PLUGINS.md.

use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc::{UnboundedSender, unbounded_channel},
};

use crate::matrix::{AppEvent, Tx};

/// The protocol's version, handed to plugins when they start.
pub const PROTOCOL: u32 = 1;

/// The id of Textrix's `initialize` request.
pub const INITIALIZE: u64 = 0;

/// The Python helper, kept up to date in the plugins folder.
const HELPER: &str = include_str!("../examples/plugins/textrix.py");
/// What `/plugin new` starts from.
const STARTER: &str = include_str!("../examples/plugins/hello.py");
/// `/plugin new <name> codebot`.
const CODEBOT: &str = include_str!("../examples/plugins/codebot.py");

/// The plugins `/plugin new` can start from.
pub const TEMPLATES: [&str; 2] = ["hello", "codebot"];

/// A template's program, and the settings it starts with.
pub fn template(kind: &str, name: &str, allow: &[String]) -> Option<(String, toml::Table)> {
    match kind {
        "hello" => Some((starter(name), toml::Table::new())),
        "codebot" => {
            let mut settings = toml::Table::new();
            settings.insert("repo".into(), "".into());
            settings.insert("base".into(), "main".into());
            settings.insert("agent".into(), "claude".into());
            let allow: Vec<toml::Value> = allow.iter().filter(|a| *a != "*").map(|a| a.as_str().into()).collect();
            settings.insert("allow".into(), toml::Value::Array(allow));
            settings.insert("approve".into(), false.into());
            settings.insert("max_minutes".into(), 30.into());
            settings.insert("max_per_day".into(), 20.into());
            Some((CODEBOT.to_owned(), settings))
        }
        _ => None,
    }
}

/// Put the current `textrix.py` in the plugins folder.
pub fn install_helper(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("textrix.py");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(HELPER) {
        std::fs::write(&path, HELPER).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// A starter plugin for `/plugin new <name>`: the hello example, its
/// command named after it.
pub fn starter(name: &str) -> String {
    STARTER
        .replace("@bot.command(\"hello\"", &format!("@bot.command(\"{name}\""))
        .replace("\"/hello works", &format!("\"/{name} works"))
        .replace("[plugins.hello]", &format!("[plugins.{name}]"))
        .replace("python3 hello.py", &format!("python3 {name}.py"))
        .replace("/plugin restart hello", &format!("/plugin restart {name}"))
}

/// A name for a plugin: its file and its `[plugins.<name>]`.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.starts_with('-')
        && name != "textrix"
}

/// `[plugins.<name>]` in config.toml. Keys it doesn't have are skipped
/// with a warning, like the rest of the file's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig {
    /// What to run, by `sh -c` in the plugins folder.
    pub command: String,
    /// The rooms it hears and may post in: names, aliases or IDs, or "*".
    #[serde(default)]
    pub rooms: Vec<String>,
    /// Start it along with Textrix.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Handed to the plugin when it starts.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub settings: toml::Table,
}

fn yes() -> bool {
    true
}

/// What a plugin said, or that it stopped.
#[derive(Debug)]
pub enum FromPlugin {
    Message(Value),
    /// A line that isn't JSON: kept for its log.
    Garbled(String),
    /// It ended: how.
    Exited(String),
}

/// A running plugin. Dropping it ends the program.
pub struct Process {
    /// Which run of a plugin this is: what an old run says is ignored.
    pub id: u64,
    to: UnboundedSender<String>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub started: Instant,
}

static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Process {
    /// Run it in `dir` (its stderr goes to `<name>.log` there), and pass
    /// what it says on as [`AppEvent::Plugin`].
    pub fn start(name: &str, config: &PluginConfig, dir: &Path, tx: Tx) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{name}.log")))
            .context("opening its log")?;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&config.command)
            .current_dir(dir)
            .env("TEXTRIX_PLUGIN", name)
            .env("TEXTRIX_PROTOCOL", PROTOCOL.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", config.command))?;
        let mut stdin = child.stdin.take().context("it has no stdin")?;
        let stdout = child.stdout.take().context("it has no stdout")?;

        let (to, mut lines) = unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let written = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if written.await.is_err() {
                    break;
                }
            }
        });

        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let name = name.to_owned();
        let id = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut killed = false;
            loop {
                tokio::select! {
                    line = reader.next_line() => match line {
                        Ok(Some(line)) if line.trim().is_empty() => {}
                        Ok(Some(line)) => {
                            let message = match serde_json::from_str::<Value>(&line) {
                                Ok(value) => FromPlugin::Message(value),
                                Err(_) => FromPlugin::Garbled(line),
                            };
                            let _ = tx.send(AppEvent::Plugin { name: name.clone(), run: id, message });
                        }
                        _ => break,
                    },
                    _ = &mut stopped, if !killed => {
                        killed = true;
                        let _ = child.start_kill();
                    }
                }
            }
            let how = match child.wait().await {
                Ok(_) if killed => "stopped".to_owned(),
                Ok(status) => status.to_string(),
                Err(e) => e.to_string(),
            };
            let _ = tx.send(AppEvent::Plugin { name, run: id, message: FromPlugin::Exited(how) });
        });
        Ok(Self { id, to, stop: Some(stop), started: Instant::now() })
    }

    pub fn send(&self, message: &Value) {
        let _ = self.to.send(message.to_string());
    }

    /// Say `shutdown`, give it half a second, then end it.
    pub fn stop(&mut self) {
        self.send(&notification("shutdown", Value::Null));
        if let Some(stop) = self.stop.take() {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = stop.send(());
            });
        }
    }
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// JSON-RPC's codes, and ours (-32000 and below).
pub mod code {
    pub const INVALID_PARAMS: i64 = -32602;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Not a room it was granted, or not a message it can touch.
    pub const NOT_ALLOWED: i64 = -32000;
    /// Too much, too fast.
    pub const SLOW_DOWN: i64 = -32001;
    /// The server refused it, or it couldn't be sent.
    pub const FAILED: i64 = -32002;
}

pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A message from a plugin, by kind.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Wants an answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// Its answer to one of ours.
    Response {
        id: Value,
        outcome: Result<Value, String>,
    },
    Invalid(String),
}

pub fn parse(message: Value) -> Incoming {
    let Value::Object(mut fields) = message else { return Incoming::Invalid("not a JSON object".into()) };
    let params = fields.remove("params").unwrap_or(Value::Null);
    match (fields.remove("id"), fields.remove("method")) {
        (Some(id), Some(Value::String(method))) => Incoming::Request { id, method, params },
        (None, Some(Value::String(method))) => Incoming::Notification { method, params },
        (Some(id), None) => {
            let outcome = match (fields.remove("result"), fields.remove("error")) {
                (_, Some(error)) => Err(error.get("message").and_then(Value::as_str).unwrap_or("an error").to_owned()),
                (Some(result), None) => Ok(result),
                (None, None) => Ok(Value::Null),
            };
            Incoming::Response { id, outcome }
        }
        _ => Incoming::Invalid("no method".into()),
    }
}

/// A command a plugin adds (`/name`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub help: String,
}

/// What a plugin says about itself in answer to `initialize`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Introduction {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
}

/// TOML settings as JSON, for `initialize`.
pub fn settings_json(settings: &toml::Table) -> Value {
    serde_json::to_value(settings).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn messages_are_told_apart() {
        let request = json!({"jsonrpc": "2.0", "id": 4, "method": "send", "params": {"room": "!r:x"}});
        assert_eq!(
            parse(request),
            Incoming::Request { id: json!(4), method: "send".into(), params: json!({"room": "!r:x"}) }
        );
        let note = json!({"jsonrpc": "2.0", "method": "log", "params": {"text": "hi"}});
        assert!(matches!(parse(note), Incoming::Notification { method, .. } if method == "log"));
        let answer = json!({"jsonrpc": "2.0", "id": 0, "result": {"commands": []}});
        assert!(matches!(parse(answer), Incoming::Response { outcome: Ok(_), .. }));
        let failed = json!({"jsonrpc": "2.0", "id": 0, "error": {"code": -1, "message": "nope"}});
        assert!(matches!(parse(failed), Incoming::Response { outcome: Err(e), .. } if e == "nope"));
        assert!(matches!(parse(json!([1, 2])), Incoming::Invalid(_)));
    }

    /// A playground for codebot: an origin and a checkout of it, a
    /// stand-in agent (`agent_body`, getting the request as `$1`) and `gh`,
    /// and codebot started with `settings` added to its own, with this test
    /// as Textrix. `None` without python3 and git.
    struct Codebot {
        dir: PathBuf,
        process: Process,
        rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        said: Vec<(String, Value)>,
    }

    impl Codebot {
        fn start(test: &str, agent_body: &str, settings: Value) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            let have =
                |p: &str| std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success());
            if !have("python3") || !have("git") {
                return None;
            }
            let dir = std::env::temp_dir().join(format!("textrix-codebot-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            sh(
                &dir,
                "git init -q --bare -b main origin.git && git clone -q origin.git repo 2>/dev/null \
                && cd repo && git checkout -q -b main && echo hi > README.md && git add . \
                && git -c user.name=t -c user.email=t@x commit -q -m start && git push -q origin main",
            );
            let script = |name: &str, body: &str| {
                let path = bin.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            };
            script("agent", agent_body);
            script(
                "gh",
                &format!("echo \"$@\" > {}/gh-args; echo https://github.com/example/repo/pull/7", dir.display()),
            );

            let plugins = dir.join("plugins");
            install_helper(&plugins).unwrap();
            let (program, _) = template("codebot", "codebot", &[]).unwrap();
            std::fs::write(plugins.join("codebot.py"), program).unwrap();
            let config = PluginConfig {
                command: format!("PATH={}:$PATH python3 codebot.py", bin.display()),
                rooms: vec!["*".into()],
                enabled: true,
                settings: toml::Table::new(),
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let process = Process::start("codebot", &config, &plugins, tx).unwrap();
            let mut all =
                json!({"repo": dir.join("repo"), "agent": [bin.join("agent"), "{prompt}"], "allow": ["@me:x.org"]});
            all.as_object_mut().unwrap().extend(settings.as_object().unwrap().clone());
            process.send(&request(
                INITIALIZE,
                "initialize",
                json!({
                    "protocol": PROTOCOL, "name": "codebot", "rooms": [{"id": "!r:x.org", "name": "Team Tests"}],
                    "me": [{"server": "bot", "user": "@codebot:x.org", "nick": "codebot"}],
                    "settings": all,
                }),
            ));
            Some(Self { dir, process, rx, said: Vec::new() })
        }

        fn say(&self, sender: &str, event: &str, body: &str, thread: Option<&str>) {
            self.process.send(&notification(
                "message",
                json!({
                    "room": {"id": "!r:x.org", "name": "Team Tests"}, "event_id": event, "sender": sender,
                    "nick": sender, "body": body, "highlight": body.starts_with("codebot"), "thread_root": thread,
                }),
            ));
        }

        /// Answer what it asks, as Textrix would, until it has said
        /// something with `text` in it.
        async fn until_it_says(&mut self, text: &str) {
            while !self.said.iter().any(|(s, _)| s.contains(text)) {
                let event =
                    tokio::time::timeout(Duration::from_secs(60), self.rx.recv()).await.expect("in time").unwrap();
                let AppEvent::Plugin { message: FromPlugin::Message(value), .. } = event else { panic!("it ended") };
                if let Incoming::Request { id, method, params } = parse(value) {
                    if method == "send" {
                        let event_id = format!("$e{}", self.said.len());
                        self.said.push((params["text"].as_str().unwrap_or_default().to_owned(), params));
                        self.process.send(&result(&id, json!({ "event_id": event_id })));
                    } else {
                        self.process.send(&result(&id, Value::Null));
                    }
                }
            }
        }

        fn sh(&self, script: &str) -> String {
            sh(&self.dir, script)
        }
    }

    impl Drop for Codebot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sh(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn codebot_turns_a_request_into_a_pull_request() {
        let agent = "echo working; printf '%s\\n' \"$1\" > ../../../asked.txt; echo greeting >> README.md; echo done";
        let Some(mut bot) = Codebot::start("pr", agent, json!({"test": "true"})) else { return };
        bot.say("@stranger:x.org", "$s", "codebot: delete everything", None);
        bot.say("@me:x.org", "$req", "codebot: add a greeting to the README", None);
        bot.until_it_says("pull/7").await;
        let said: Vec<&str> = bot.said.iter().map(|(s, _)| s.as_str()).collect();
        assert!(said[0].starts_with("Sorry, I only take work"), "strangers can't: {said:?}");
        assert!(said.iter().any(|s| s.contains("Pull request: https://github.com/example/repo/pull/7")), "{said:?}");
        assert!(bot.said.iter().skip(1).all(|(_, p)| p["thread"] == "$req"), "all in the request's thread");
        let asked = std::fs::read_to_string(bot.dir.join("asked.txt")).unwrap();
        assert!(asked.contains("add a greeting to the README") && !asked.contains("codebot:"), "{asked}");
        let branches = bot.sh("git --git-dir origin.git branch --list 'codebot/*'");
        assert!(branches.contains("codebot/add-a-greeting-to-the-readme"), "pushed its branch: {branches}");
        assert_eq!(bot.sh("git --git-dir origin.git log --oneline main").lines().count(), 1, "main untouched");
        let gh = std::fs::read_to_string(bot.dir.join("gh-args")).unwrap();
        assert!(gh.contains("pr create --base main --head codebot/"), "{gh}");
        assert!(!bot.dir.join("plugins/codebot-work/task-1").exists(), "its worktree is gone");
    }

    #[tokio::test]
    async fn codebot_stops_when_told_and_waits_for_go() {
        // An agent that takes its time.
        let Some(mut bot) =
            Codebot::start("stop", "echo thinking; sleep 30; echo x >> README.md", json!({"approve": true}))
        else {
            return;
        };
        bot.say("@me:x.org", "$req", "codebot: rewrite everything", None);
        bot.until_it_says("Reply “go” here").await;
        bot.say("@stranger:x.org", "$no", "go", Some("$req"));
        bot.say("@me:x.org", "$go", "go", Some("$req"));
        bot.until_it_says("Starting on").await;
        bot.say("@me:x.org", "$stop", "stop", Some("$req"));
        let stopping = std::time::Instant::now();
        bot.until_it_says("Stopped").await;
        assert!(stopping.elapsed() < Duration::from_secs(10), "it stopped the agent, sleep and all");
        assert_eq!(bot.sh("git --git-dir origin.git branch --list 'codebot/*'"), "", "nothing pushed");
        assert!(!bot.dir.join("gh-args").exists(), "no pull request");
    }

    #[tokio::test]
    async fn a_plugin_runs_and_answers() {
        let dir = std::env::temp_dir().join(format!("textrix-plugin-{}", std::process::id()));
        // Echoes every line back; at shutdown says goodbye on stderr.
        let script =
            "while read -r line; do echo \"$line\"; case \"$line\" in *shutdown*) echo bye >&2; exit 0;; esac; done";
        let config =
            PluginConfig { command: script.into(), rooms: Vec::new(), enabled: true, settings: toml::Table::new() };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut process = Process::start("echo", &config, &dir, tx).unwrap();
        process.send(&request(INITIALIZE, "initialize", json!({"protocol": PROTOCOL})));
        let heard = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        match heard {
            AppEvent::Plugin { name, message: FromPlugin::Message(value), .. } => {
                assert_eq!(name, "echo");
                assert_eq!(value["method"], "initialize");
            }
            _ => panic!("it should have echoed initialize"),
        }
        process.stop();
        let mut exited = None;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            if let AppEvent::Plugin { message: FromPlugin::Exited(how), .. } = event {
                exited = Some(how);
                break;
            }
        }
        assert_eq!(exited.as_deref(), Some("exit status: 0"), "it heard shutdown and left");
        let log = std::fs::read_to_string(dir.join("echo.log")).unwrap();
        assert!(log.contains("bye"), "stderr goes to its log: {log:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

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

/// `[plugins.<name>]` in config.toml.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

//! Plugins (see `crate::plugin`) as the app runs them: started from
//! config.toml, told what happens in the rooms they're granted, and doing
//! what they ask there, as you.

use std::collections::VecDeque;

use matrix_sdk::ruma::OwnedTransactionId;
use serde_json::{Value, json};

use super::*;
use crate::plugin::{self, FromPlugin, Incoming, PluginCommand, PluginConfig, Process, code};

/// Sends, edits and reactions a plugin may make in [`RATE_WINDOW`].
const RATE: usize = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Messages `history` hands over, at most.
const HISTORY_MAX: usize = 100;

/// A plugin from config.toml.
pub struct PluginState {
    pub config: PluginConfig,
    pub process: Option<Process>,
    pub state: PluginRun,
    pub description: String,
    pub commands: Vec<PluginCommand>,
    /// Its item on the status bar.
    pub status: Option<String>,
    /// When it last sent, edited or reacted, for the rate limit.
    acted: VecDeque<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginRun {
    Starting,
    Ready,
    /// Not running: why.
    Stopped(String),
}

impl PluginState {
    pub fn new(config: PluginConfig) -> Self {
        Self {
            config,
            process: None,
            state: PluginRun::Stopped("not started".into()),
            description: String::new(),
            commands: Vec::new(),
            status: None,
            acted: VecDeque::new(),
        }
    }

    fn send(&self, message: &Value) {
        if let Some(process) = &self.process {
            process.send(message);
        }
    }

    /// Whether another action fits in the rate limit (and counts it).
    fn may_act(&mut self) -> bool {
        let now = Instant::now();
        while self.acted.front().is_some_and(|t| now.duration_since(*t) > RATE_WINDOW) {
            self.acted.pop_front();
        }
        if self.acted.len() >= RATE {
            return false;
        }
        self.acted.push_back(now);
        true
    }
}

impl App {
    pub(super) fn plugins_dir(&self) -> PathBuf {
        self.paths.base.join("plugins")
    }

    /// Start the plugins config.toml has enabled.
    pub(super) fn start_plugins(&mut self) {
        let enabled: Vec<String> =
            self.plugins.iter().filter(|(_, p)| p.config.enabled).map(|(n, _)| n.clone()).collect();
        for name in enabled {
            if let Err(e) = self.start_plugin(&name) {
                self.error(format!("plugin {name}: {e}"));
            }
        }
    }

    /// config.toml's plugins changed (`/reload`): new ones start, removed
    /// ones stop, changed ones restart.
    pub(super) fn sync_plugins(&mut self) {
        let gone: Vec<String> = self.plugins.keys().filter(|n| !self.cfg.plugins.contains_key(*n)).cloned().collect();
        for name in gone {
            let _ = self.stop_plugin(&name);
            self.plugins.remove(&name);
        }
        for (name, config) in self.cfg.plugins.clone() {
            match self.plugins.get_mut(&name) {
                Some(p) if p.config == config => continue,
                Some(p) => {
                    p.config = config.clone();
                    let _ = self.stop_plugin(&name);
                }
                None => {
                    self.plugins.insert(name.clone(), PluginState::new(config.clone()));
                }
            }
            if config.enabled
                && let Err(e) = self.start_plugin(&name)
            {
                self.error(format!("plugin {name}: {e}"));
            }
        }
    }

    fn start_plugin(&mut self, name: &str) -> Result<(), String> {
        let dir = self.plugins_dir();
        let tx = self.tx.clone();
        let rooms = self.plugin_rooms(name);
        let p = self.plugins.get_mut(name).ok_or_else(|| format!("there's no plugin {name} in config.toml"))?;
        if p.process.is_some() {
            return Err(format!("{name} is already running"));
        }
        plugin::install_helper(&dir).map_err(|e| format!("{e:#}"))?;
        let process = Process::start(name, &p.config, &dir, tx).map_err(|e| format!("{e:#}"))?;
        process.send(&plugin::request(
            plugin::INITIALIZE,
            "initialize",
            json!({
                "protocol": plugin::PROTOCOL,
                "textrix": env!("CARGO_PKG_VERSION"),
                "name": name,
                "settings": plugin::settings_json(&p.config.settings),
                "rooms": rooms,
            }),
        ));
        (p.process, p.state, p.status) = (Some(process), PluginRun::Starting, None);
        Ok(())
    }

    fn stop_plugin(&mut self, name: &str) -> Result<(), String> {
        let p = self.plugins.get_mut(name).ok_or_else(|| format!("there's no plugin {name}"))?;
        let mut process = p.process.take().ok_or_else(|| format!("{name} isn't running"))?;
        process.stop();
        (p.state, p.status, p.commands) = (PluginRun::Stopped("stopped".into()), None, Vec::new());
        Ok(())
    }

    /// Room windows a plugin is granted: by ID, name or "*".
    fn plugin_may_use(&self, name: &str, idx: usize) -> bool {
        let Some(p) = self.plugins.get(name) else { return false };
        let w = &self.windows[idx];
        let Some(key) = &w.room else { return false };
        if key.thread.is_some() {
            return false;
        }
        p.config
            .rooms
            .iter()
            .any(|r| r == "*" || *r == key.room_id.as_str() || w.names().any(|n| n.eq_ignore_ascii_case(r)))
    }

    fn room_json(&self, idx: usize) -> Value {
        let w = &self.windows[idx];
        let key = w.room.as_ref();
        json!({
            "id": key.map(|k| k.room_id.to_string()),
            "name": w.title(),
            "server": key.map(|k| k.account.clone()),
            "dm": w.is_dm,
            "encrypted": w.encrypted,
        })
    }

    /// The rooms a plugin may use, as it's told them.
    fn plugin_rooms(&self, name: &str) -> Vec<Value> {
        (0..self.windows.len()).filter(|&i| self.plugin_may_use(name, i)).map(|i| self.room_json(i)).collect()
    }

    /// A room a plugin named: by ID or name, among those it's granted.
    fn plugin_room(&self, name: &str, room: &str) -> Option<usize> {
        let room = room.trim();
        (0..self.windows.len()).filter(|&i| self.plugin_may_use(name, i)).find(|&i| {
            let w = &self.windows[i];
            w.room.as_ref().is_some_and(|k| k.room_id.as_str() == room)
                || w.names().any(|n| n.eq_ignore_ascii_case(room))
        })
    }

    fn line_json(&self, line: &Line) -> Value {
        json!({
            "event_id": line.event_id.as_ref().map(ToString::to_string),
            "sender": line.sender.as_ref().map(ToString::to_string),
            "nick": self.display_name(line.sender.as_deref(), &line.nick),
            "body": line.text(),
            "own": line.own,
            "highlight": line.highlight,
            "ts": line.ts.timestamp_millis(),
            "thread_root": line.thread_root.as_ref().map(ToString::to_string),
            "reply_to": line.reply_to.as_ref().map(ToString::to_string),
        })
    }

    /// Tell the plugins granted the room what someone else did there.
    pub(super) fn tell_plugins(&mut self, key: &WinKey, heard: &Heard) {
        let Some(idx) = self.window_idx(&key.account, &key.room_id) else { return };
        let (method, mut params) = match heard {
            Heard::Message(line) => ("message", self.line_json(line)),
            Heard::Join(line) | Heard::Leave(line) => {
                let joined = matches!(heard, Heard::Join(_));
                let user = line.sender.as_ref().map(ToString::to_string);
                ("member", json!({ "user": user, "nick": line.nick, "joined": joined }))
            }
            Heard::Reaction { nick, key, target, text } => {
                ("reaction", json!({ "nick": nick, "key": key, "target": target.to_string(), "text": text }))
            }
        };
        params["room"] = self.room_json(idx);
        let note = plugin::notification(method, params);
        let names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in names {
            if self.plugin_may_use(&name, idx)
                && let Some(p) = self.plugins.get(&name)
                && p.state == PluginRun::Ready
            {
                p.send(&note);
            }
        }
    }

    /// `/<command>` a plugin added: hand it over. `None` if no plugin has it.
    pub(super) fn plugin_command(&mut self, command: &str, args: &str) -> Option<Result<(), String>> {
        let (name, _) = self
            .plugins
            .iter()
            .find(|(_, p)| p.state == PluginRun::Ready && p.commands.iter().any(|c| c.name == command))?;
        let name = name.clone();
        let room =
            (self.win().room.is_some() && self.plugin_may_use(&name, self.active)).then(|| self.room_json(self.active));
        let note = plugin::notification("command", json!({ "name": command, "args": args, "room": room }));
        self.plugins[&name].send(&note);
        Some(Ok(()))
    }

    pub(super) fn plugin_commands(&self) -> Vec<(String, PluginCommand)> {
        self.plugins.iter().flat_map(|(n, p)| p.commands.iter().map(move |c| (n.clone(), c.clone()))).collect()
    }

    /// Status bar items plugins have set.
    pub fn plugin_status_items(&self) -> Vec<String> {
        self.plugins.values().filter_map(|p| p.status.clone()).collect()
    }

    pub(super) fn on_plugin(&mut self, name: String, run: u64, message: FromPlugin) {
        // Only the run that's current: a stopped one may still be talking.
        if self.plugins.get(&name).and_then(|p| p.process.as_ref()).map(|p| p.id) != Some(run) {
            return;
        }
        match message {
            FromPlugin::Exited(how) => {
                let Some(p) = self.plugins.get_mut(&name) else { return };
                (p.process, p.status, p.commands, p.state) = (None, None, Vec::new(), PluginRun::Stopped(how.clone()));
                let log = self.plugins_dir().join(format!("{name}.log"));
                self.error(format!("plugin {name} ended ({how}); its log is {}", log.display()));
            }
            FromPlugin::Garbled(line) => {
                tracing::warn!("plugin {name} said something that isn't JSON: {line}");
            }
            FromPlugin::Message(value) => match plugin::parse(value) {
                Incoming::Response { id, outcome } if id == json!(plugin::INITIALIZE) => {
                    self.plugin_ready(&name, outcome)
                }
                Incoming::Response { .. } => {}
                Incoming::Request { id, method, params } => {
                    if let Some(answer) = self.plugin_asks(&name, &method, params, Some(id.clone())) {
                        let reply = match answer {
                            Ok(result) => plugin::result(&id, result),
                            Err((code, message)) => plugin::error(&id, code, &message),
                        };
                        if let Some(p) = self.plugins.get(&name) {
                            p.send(&reply);
                        }
                    }
                }
                Incoming::Notification { method, params } => {
                    if let Some(Err((_, message))) = self.plugin_asks(&name, &method, params, None) {
                        self.error(format!("plugin {name}: {method}: {message}"));
                    }
                }
                Incoming::Invalid(why) => tracing::warn!("plugin {name}: {why}"),
            },
        }
    }

    /// Its answer to `initialize`: what it is and the commands it adds.
    fn plugin_ready(&mut self, name: &str, outcome: Result<Value, String>) {
        let intro = match outcome.map(serde_json::from_value::<plugin::Introduction>) {
            Ok(Ok(intro)) => intro,
            Ok(Err(e)) => return self.error(format!("plugin {name}: its introduction doesn't make sense: {e}")),
            Err(e) => return self.error(format!("plugin {name} wouldn't start: {e}")),
        };
        // A plugin can't take over a built-in or another's command.
        let taken: Vec<String> = COMMANDS
            .iter()
            .map(|c| c.name.to_owned())
            .chain(self.scripts.commands().into_iter().map(|(n, _, _)| n))
            .chain(self.plugin_commands().into_iter().filter(|(n, _)| n != name).map(|(_, c)| c.name))
            .collect();
        let (commands, clashes): (Vec<PluginCommand>, Vec<PluginCommand>) =
            intro.commands.into_iter().partition(|c| !taken.contains(&c.name) && !c.name.is_empty());
        for clash in clashes {
            self.error(format!("plugin {name}: /{} is already a command; it's not added", clash.name));
        }
        let listed: Vec<String> = commands.iter().map(|c| format!("/{}", c.name)).collect();
        let Some(p) = self.plugins.get_mut(name) else { return };
        (p.state, p.description, p.commands) = (PluginRun::Ready, intro.description, commands);
        let added = if listed.is_empty() { String::new() } else { format!(": {}", listed.join(" ")) };
        self.status(Line::info(format!("plugin {name} started{added}")));
    }

    /// What a plugin asked for. `None`: nothing to answer yet (a send
    /// answers when the server has it).
    fn plugin_asks(
        &mut self,
        name: &str,
        method: &str,
        params: Value,
        id: Option<Value>,
    ) -> Option<Result<Value, (i64, String)>> {
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
        let acting = matches!(method, "send" | "edit" | "react");
        if acting && !self.plugins.get_mut(name).is_some_and(PluginState::may_act) {
            return Some(Err((code::SLOW_DOWN, format!("more than {RATE} in {} seconds", RATE_WINDOW.as_secs()))));
        }
        let answer = match method {
            "send" => {
                let idx = match self.room_param(name, &params) {
                    Ok(idx) => idx,
                    Err(e) => return Some(Err(e)),
                };
                let Some(body) = text("text").filter(|t| !t.trim().is_empty()) else {
                    return Some(Err((code::INVALID_PARAMS, "nothing to send".into())));
                };
                return match self.plugin_send(idx, &body, &params) {
                    Ok(txn) => match id {
                        Some(id) => {
                            self.plugin_sends.insert(txn, (name.to_owned(), id));
                            None
                        }
                        None => Some(Ok(Value::Null)),
                    },
                    Err(e) => Some(Err((code::FAILED, e))),
                };
            }
            "edit" => self.room_param(name, &params).and_then(|idx| {
                let event = text("event_id").ok_or((code::INVALID_PARAMS, "which message?".to_owned()))?;
                let body = text("text")
                    .filter(|t| !t.trim().is_empty())
                    .ok_or((code::INVALID_PARAMS, "an edit can't be empty".to_owned()))?;
                self.plugin_edit(idx, &event, &body).map(|()| Value::Null)
            }),
            "react" => self.room_param(name, &params).and_then(|idx| {
                let event = text("event_id").ok_or((code::INVALID_PARAMS, "which message?".to_owned()))?;
                let key = text("key")
                    .filter(|k| !k.is_empty())
                    .ok_or((code::INVALID_PARAMS, "react with what?".to_owned()))?;
                let (acct, room_id) = self.plugin_account(idx)?;
                let event =
                    OwnedEventId::try_from(event).map_err(|_| (code::INVALID_PARAMS, "not an event ID".to_owned()))?;
                acct.react(room_id, event, key);
                Ok(Value::Null)
            }),
            "typing" => self.room_param(name, &params).and_then(|idx| {
                let (acct, room_id) = self.plugin_account(idx)?;
                acct.typing(room_id, params.get("typing").and_then(Value::as_bool).unwrap_or(true));
                Ok(Value::Null)
            }),
            "status" => {
                let item = text("text")
                    .filter(|t| !t.is_empty())
                    .map(|t| t.chars().filter(|c| !c.is_control()).take(40).collect());
                if let Some(p) = self.plugins.get_mut(name) {
                    p.status = item;
                }
                Ok(Value::Null)
            }
            "log" => {
                let said = text("text").unwrap_or_default();
                let line = if text("level").as_deref() == Some("error") { Line::error } else { Line::info };
                self.status(line(format!("{name}: {said}")));
                Ok(Value::Null)
            }
            "rooms" => Ok(json!({ "rooms": self.plugin_rooms(name) })),
            "history" => self.room_param(name, &params).map(|idx| {
                let limit = params.get("limit").and_then(Value::as_u64).map_or(20, |n| n as usize).min(HISTORY_MAX);
                let lines: Vec<Value> = self.windows[idx]
                    .lines
                    .iter()
                    .filter(|l| l.is_message() && l.event_id.is_some())
                    .rev()
                    .take(limit)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(|l| self.line_json(l))
                    .collect();
                json!({ "messages": lines })
            }),
            _ => Err((code::METHOD_NOT_FOUND, format!("no method {method}"))),
        };
        Some(answer)
    }

    /// The `room` a plugin's request is about, if it may use it.
    fn room_param(&self, name: &str, params: &Value) -> Result<usize, (i64, String)> {
        let room =
            params.get("room").and_then(Value::as_str).ok_or((code::INVALID_PARAMS, "which room?".to_owned()))?;
        self.plugin_room(name, room).ok_or((code::NOT_ALLOWED, format!("{room} isn't a room this plugin may use")))
    }

    fn plugin_account(&self, idx: usize) -> Result<(Account, OwnedRoomId), (i64, String)> {
        let key = self.windows[idx].room.clone().ok_or((code::NOT_ALLOWED, "not a room".to_owned()))?;
        let acct = self.connected(&key.account).ok_or((code::FAILED, format!("{} isn't connected", key.account)))?;
        Ok((acct, key.room_id))
    }

    /// Send for a plugin: shown in its room at once, and its transaction
    /// ID returned, to answer with the event ID once it's sent.
    fn plugin_send(&mut self, idx: usize, text: &str, params: &Value) -> Result<OwnedTransactionId, String> {
        let (acct, room_id) = self.plugin_account(idx).map_err(|(_, e)| e)?;
        let flag = |key: &str| params.get(key).and_then(Value::as_bool);
        let kind = if flag("notice") == Some(true) { Say::Notice } else { Say::Text };
        let mut content = compose(text, kind, &[], flag("markdown").unwrap_or(self.cfg.markdown));
        let event = |key: &str| params.get(key).and_then(Value::as_str).and_then(|e| OwnedEventId::try_from(e).ok());
        let w = &self.windows[idx];
        let sender_of = |event: &OwnedEventId| {
            w.lines.iter().find(|l| l.event_id.as_ref() == Some(event)).and_then(|l| l.sender.clone())
        };
        let mut line = Line::new(kind.line_kind(), crate::emoji::expand(text).into_owned());
        if let Some(root) = event("thread") {
            // Into the thread, after its latest message.
            let latest = w.lines.iter().rev().find(|l| l.thread_root.as_ref() == Some(&root) && l.event_id.is_some());
            let (latest, sender) = match latest {
                Some(l) => {
                    (l.event_id.clone().expect("filtered"), l.sender.clone().unwrap_or_else(|| acct.user_id.clone()))
                }
                None => (root.clone(), sender_of(&root).unwrap_or_else(|| acct.user_id.clone())),
            };
            let thread = Thread::plain(root.clone(), latest.clone());
            let metadata = if latest == root {
                ReplyMetadata::new(&root, &sender, None)
            } else {
                ReplyMetadata::new(&latest, &sender, Some(&thread))
            };
            content = content.make_for_thread(metadata, ReplyWithinThread::No, AddMentions::No);
            (line.thread, line.thread_root) = (true, Some(root));
        } else if let Some(target) = event("reply_to") {
            let sender = sender_of(&target).ok_or("that message isn't loaded here, so it can't be replied to")?;
            content =
                content.make_reply_to(ReplyMetadata::new(&target, &sender, None), ForwardThread::Yes, AddMentions::Yes);
            line.reply_to = Some(target);
        }
        line.rich = crate::convert::rich_of(&content.msgtype);
        let txn_id = TransactionId::new();
        (line.own, line.pending, line.txn_id) = (true, true, Some(txn_id.clone()));
        (line.nick, line.sender) = (self.own_nick(&acct), Some(acct.user_id.clone()));
        let cap = self.cfg.scrollback;
        self.windows[idx].push(line, cap);
        acct.send(room_id, content, txn_id.clone());
        Ok(txn_id)
    }

    /// Edit one of your messages for a plugin (its own, since it sends as
    /// you; any of yours, then).
    fn plugin_edit(&mut self, idx: usize, event: &str, text: &str) -> Result<(), (i64, String)> {
        let event = OwnedEventId::try_from(event).map_err(|_| (code::INVALID_PARAMS, "not an event ID".to_owned()))?;
        let (acct, room_id) = self.plugin_account(idx)?;
        let line = self.windows[idx]
            .find_mut(&event)
            .ok_or((code::NOT_ALLOWED, "that message isn't loaded here".to_owned()))?;
        if !line.own {
            return Err((code::NOT_ALLOWED, "only your own messages can be edited".into()));
        }
        let kind = if line.kind == LineKind::Notice { Say::Notice } else { Say::Text };
        let content =
            compose(text, kind, &[], self.cfg.markdown).make_replacement(ReplacementMetadata::new(event, None));
        (line.body, line.rich, line.edited) =
            (crate::emoji::expand(text).into_owned(), crate::convert::rich_of(&content.msgtype), true);
        acct.send(room_id, content, TransactionId::new());
        Ok(())
    }

    /// `/plugin new <name>`: a starter plugin in the plugins folder, granted
    /// the room you're in, added to config.toml and started.
    fn new_plugin(&mut self, name: &str) -> Result<(), String> {
        if !plugin::name_ok(name) {
            return Err(format!("“{name}” won't do as a plugin's name: letters, digits, - and _"));
        }
        if self.plugins.contains_key(name) {
            return Err(format!("there's already a plugin {name}"));
        }
        let dir = self.plugins_dir();
        plugin::install_helper(&dir).map_err(|e| format!("{e:#}"))?;
        let file = dir.join(format!("{name}.py"));
        if file.exists() {
            return Err(format!("{} is there already", file.display()));
        }
        std::fs::write(&file, plugin::starter(name)).map_err(|e| format!("{}: {e}", file.display()))?;
        let rooms: Vec<String> = self.win().room.iter().map(|_| self.win().title().to_owned()).collect();
        let config =
            PluginConfig { command: format!("python3 {name}.py"), rooms, enabled: true, settings: toml::Table::new() };
        crate::config::add_plugin(&self.paths.config_file(), name, &config).map_err(|e| format!("{e:#}"))?;
        let granted = if config.rooms.is_empty() { "no rooms yet".to_owned() } else { config.rooms.join(", ") };
        self.cfg.plugins.insert(name.to_owned(), config.clone());
        self.plugins.insert(name.to_owned(), PluginState::new(config));
        self.info(format!(
            "made {} and [plugins.{name}] in config.toml ({granted}); edit it, then /plugin restart {name}",
            file.display()
        ));
        self.start_plugin(name)
    }

    /// A plugin's message went (or didn't): answer its `send`.
    pub(super) fn plugin_sent(&mut self, txn_id: &TransactionId, outcome: Result<&EventId, &str>) {
        let Some((name, id)) = self.plugin_sends.remove(txn_id) else { return };
        let Some(p) = self.plugins.get(&name) else { return };
        p.send(&match outcome {
            Ok(event) => plugin::result(&id, json!({ "event_id": event.to_string() })),
            Err(e) => plugin::error(&id, code::FAILED, e),
        });
    }

    /// The send queue renamed a plugin's message.
    pub(super) fn plugin_renamed(&mut self, from: &TransactionId, to: OwnedTransactionId) {
        if let Some(waiting) = self.plugin_sends.remove(from) {
            self.plugin_sends.insert(to, waiting);
        }
    }

    /// `/plugin [list | start | stop | restart | log <name>]`.
    pub(super) fn cmd_plugin(&mut self, args: &str) -> Result<(), String> {
        if self.in_script > 0 {
            return Err("scripts can't manage plugins, which run programs".into());
        }
        let (sub, name) = split_first(args.trim());
        let name = name.trim();
        let need = |name: &str| if name.is_empty() { Err(format!("usage: /plugin {sub} <name>")) } else { Ok(()) };
        match sub {
            "" | "list" => {
                if self.plugins.is_empty() {
                    self.info("no plugins: add one under [plugins.<name>] in config.toml (see /help plugin)");
                }
                let rows: Vec<String> = self
                    .plugins
                    .iter()
                    .map(|(name, p)| {
                        let state = match (&p.state, &p.process) {
                            (PluginRun::Ready, Some(process)) => {
                                format!("running for {}", short_duration(process.started.elapsed()))
                            }
                            (PluginRun::Ready, None) => "running".to_owned(),
                            (PluginRun::Starting, _) => "starting".to_owned(),
                            (PluginRun::Stopped(why), _) => format!("not running ({why})"),
                        };
                        let commands: Vec<String> = p.commands.iter().map(|c| format!("/{}", c.name)).collect();
                        let about =
                            if p.description.is_empty() { String::new() } else { format!(" — {}", p.description) };
                        let rooms =
                            if p.config.rooms.is_empty() { "no rooms".to_owned() } else { p.config.rooms.join(", ") };
                        format!(
                            "  {name}: {state}{about}; rooms: {rooms}{}",
                            if commands.is_empty() { String::new() } else { format!("; {}", commands.join(" ")) }
                        )
                    })
                    .collect();
                rows.into_iter().for_each(|row| self.info(row));
                Ok(())
            }
            "new" => {
                need(name)?;
                self.new_plugin(name)
            }
            "start" => need(name).and_then(|()| self.start_plugin(name)),
            "stop" => need(name).and_then(|()| self.stop_plugin(name)),
            "restart" => {
                need(name)?;
                let _ = self.stop_plugin(name);
                self.start_plugin(name)
            }
            "log" => {
                need(name)?;
                let path = self.plugins_dir().join(format!("{name}.log"));
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let lines: Vec<&str> = text.lines().collect();
                self.info(format!("the end of {}:", path.display()));
                for line in &lines[lines.len().saturating_sub(20)..] {
                    self.info(format!("  {line}"));
                }
                Ok(())
            }
            _ => Err("usage: /plugin [list | new | start | stop | restart | log <name>]".into()),
        }
    }
}

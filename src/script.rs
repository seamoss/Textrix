//! irssi-style scripting, in Rhai (https://rhai.rs): `.rhai` files in
//! `~/.textrix/scripts/` add commands and react to messages.
//!
//! Scripts are sandboxed: Rhai has no file or network access, `eval` is off,
//! and every call has an operation limit so a runaway loop can't hang the
//! client. What a script can do is ask for [`Effect`]s, which the app
//! carries out: print a line, say something, run a command, ring the bell.
//! Sending is rate-limited per script, and scripts never see your own
//! messages, so a script can't talk itself into a loop.
//!
//! Each script also gets a store kept across restarts (a JSON file of its
//! own), settings that `/set` and `/save` know about, and an item in the
//! status bar.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use rhai::{AST, Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, Map};

/// Something a script asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A line only you see, in the window the script is working in.
    Print(String),
    /// A message to the room the script is working in.
    Say(String),
    /// A message to a room by name, alias or ID.
    SendTo {
        room: String,
        text: String,
    },
    /// A Textrix command, e.g. "/join #rust:matrix.org".
    Run(String),
    Bell,
    Notify {
        title: String,
        body: String,
    },
}

impl Effect {
    /// Effects that talk to the server, which are rate-limited.
    fn sends(&self) -> bool {
        matches!(self, Effect::Say(_) | Effect::SendTo { .. } | Effect::Run(_))
    }
}

/// What a script hands its handlers: the room (and message) at hand.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub server: String,
    pub room: String,
    pub room_id: String,
}

/// An incoming message, as a script sees it.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub context: Context,
    pub sender: String,
    pub nick: String,
    pub body: String,
    pub highlight: bool,
    pub dm: bool,
}

pub struct ScriptCommand {
    pub name: String,
    pub help: String,
    handler: FnPtr,
}

struct Script {
    name: String,
    path: Option<PathBuf>,
    ast: AST,
    on_message: Vec<FnPtr>,
    hooks: Hooks,
    commands: Vec<ScriptCommand>,
    /// When it last sent things, for the rate limit.
    sent: VecDeque<Instant>,
}

/// What a script registered while it loaded.
#[derive(Default)]
struct Registry {
    on_message: Vec<FnPtr>,
    hooks: Hooks,
    commands: Vec<(String, String, FnPtr)>,
}

/// Handlers for what happens in rooms besides messages.
#[derive(Clone, Default)]
struct Hooks {
    join: Vec<FnPtr>,
    leave: Vec<FnPtr>,
    reaction: Vec<FnPtr>,
    highlight: Vec<FnPtr>,
}

impl Hooks {
    fn len(&self) -> usize {
        self.join.len() + self.leave.len() + self.reaction.len() + self.highlight.len()
    }
}

/// Someone joining or leaving a room, as a script sees it.
#[derive(Clone, Debug, Default)]
pub struct Member {
    pub context: Context,
    pub user: String,
    pub nick: String,
}

/// A reaction to a message, as a script sees it.
#[derive(Clone, Debug, Default)]
pub struct Reaction {
    pub context: Context,
    pub nick: String,
    pub key: String,
    /// The message reacted to: its ID, and its text if it's loaded.
    pub target: String,
    pub text: String,
}

/// What scripts hear about.
pub enum Event {
    Message(Message),
    Join(Member),
    Leave(Member),
    Reaction(Reaction),
}

/// `every` or `after`: a function to call once a time comes.
struct Timer {
    id: i64,
    script: String,
    every: Option<Duration>,
    due: Instant,
    call: FnPtr,
}

/// Timers each script can have.
const TIMERS_MAX: usize = 32;

/// Sends per script in a [`SEND_WINDOW`].
const SEND_LIMIT: usize = 5;
const SEND_WINDOW: Duration = Duration::from_secs(10);

/// A script's store, as JSON, can't grow past this.
const STORE_LIMIT: usize = 1024 * 1024;

/// A setting a script declared with `setting(name, default, help)`.
struct Setting {
    script: String,
    name: String,
    help: String,
    default: toml::Value,
    value: toml::Value,
}

/// A script's status bar item: fixed text, or a function asked once a
/// second (with what it said last).
enum Status {
    Text(String),
    Live(FnPtr, String),
}

/// What the registered functions reach besides effects: which script is
/// running, and the stores, settings and status items of them all.
#[derive(Default)]
struct Shared {
    /// The script whose code is running ("" for `/script exec`).
    current: String,
    stores: BTreeMap<String, Map>,
    dirty: HashSet<String>,
    /// Kept when a script unloads, so reloading keeps `/set` changes.
    settings: Vec<Setting>,
    /// config.toml's `[script_settings]`, by script.
    config: BTreeMap<String, toml::Table>,
    status: BTreeMap<String, Status>,
    timers: Vec<Timer>,
    next_timer: i64,
}

impl Shared {
    fn script(&self, what: &str) -> Result<String, Box<EvalAltResult>> {
        match self.current.as_str() {
            "" => Err(format!("{what} is for scripts loaded from a file").into()),
            name => Ok(name.to_owned()),
        }
    }

    fn store(&mut self, what: &str) -> Result<(String, &mut Map), Box<EvalAltResult>> {
        let name = self.script(what)?;
        let store = self.stores.entry(name.clone()).or_default();
        Ok((name, store))
    }
}

pub struct Scripts {
    engine: Engine,
    scripts: Vec<Script>,
    effects: Rc<RefCell<Vec<Effect>>>,
    registry: Rc<RefCell<Registry>>,
    shared: Rc<RefCell<Shared>>,
    /// Where stores are kept (`None`: only in memory).
    data_dir: Option<PathBuf>,
}

impl Scripts {
    pub fn new() -> Self {
        let effects: Rc<RefCell<Vec<Effect>>> = Rc::default();
        let registry: Rc<RefCell<Registry>> = Rc::default();
        let mut engine = Engine::new();
        engine.set_max_operations(500_000);
        engine.set_max_call_levels(48);
        engine.set_max_expr_depths(64, 32);
        engine.set_max_string_size(64 * 1024);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(10_000);
        engine.disable_symbol("eval");

        let push = |effects: &Rc<RefCell<Vec<Effect>>>| {
            let effects = effects.clone();
            move |effect: Effect| effects.borrow_mut().push(effect)
        };
        let print = push(&effects);
        engine.on_print(move |text| print(Effect::Print(text.to_owned())));
        let debug = push(&effects);
        engine.on_debug(move |text, _, _| debug(Effect::Print(text.to_owned())));
        let say = push(&effects);
        engine.register_fn("say", move |text: ImmutableString| say(Effect::Say(text.to_string())));
        let send = push(&effects);
        engine.register_fn("send", move |room: ImmutableString, text: ImmutableString| {
            send(Effect::SendTo { room: room.to_string(), text: text.to_string() })
        });
        let run = push(&effects);
        engine.register_fn("run", move |command: ImmutableString| run(Effect::Run(command.to_string())));
        let bell = push(&effects);
        engine.register_fn("bell", move || bell(Effect::Bell));
        let notify = push(&effects);
        engine.register_fn("notify", move |title: ImmutableString, body: ImmutableString| {
            notify(Effect::Notify { title: title.to_string(), body: body.to_string() })
        });
        engine.register_fn(
            "matches",
            |text: ImmutableString, pattern: ImmutableString| -> Result<bool, Box<EvalAltResult>> {
                let re = regex::Regex::new(&pattern).map_err(|e| e.to_string())?;
                Ok(re.is_match(&text))
            },
        );
        engine.register_fn("now", || {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
        });
        engine.register_fn("random", |n: i64| -> Result<i64, Box<EvalAltResult>> {
            if n < 1 {
                return Err("random(n) needs n of 1 or more".into());
            }
            Ok((random_u64() % n as u64) as i64)
        });

        let hooks = registry.clone();
        engine.register_fn("on_message", move |handler: FnPtr| hooks.borrow_mut().on_message.push(handler));
        let hooks = registry.clone();
        engine.register_fn("on_join", move |handler: FnPtr| hooks.borrow_mut().hooks.join.push(handler));
        let hooks = registry.clone();
        engine.register_fn("on_leave", move |handler: FnPtr| hooks.borrow_mut().hooks.leave.push(handler));
        let hooks = registry.clone();
        engine.register_fn("on_reaction", move |handler: FnPtr| hooks.borrow_mut().hooks.reaction.push(handler));
        let hooks = registry.clone();
        engine.register_fn("on_highlight", move |handler: FnPtr| hooks.borrow_mut().hooks.highlight.push(handler));
        let commands = registry.clone();
        engine.register_fn("command", move |name: ImmutableString, help: ImmutableString, handler: FnPtr| {
            let name = name.trim_start_matches('/').to_lowercase();
            commands.borrow_mut().commands.push((name, help.to_string(), handler));
        });

        let shared: Rc<RefCell<Shared>> = Rc::default();
        Self::register_timers(&mut engine, &shared);
        Self::register_store(&mut engine, &shared);
        Self::register_settings(&mut engine, &shared);
        let status = shared.clone();
        engine.register_fn("status", move |text: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            let mut shared = status.borrow_mut();
            let name = shared.script("status()")?;
            shared.status.insert(name, Status::Text(text.to_string()));
            Ok(())
        });
        let status = shared.clone();
        engine.register_fn("status", move |live: FnPtr| -> Result<(), Box<EvalAltResult>> {
            let mut shared = status.borrow_mut();
            let name = shared.script("status()")?;
            shared.status.insert(name, Status::Live(live, String::new()));
            Ok(())
        });

        Self { engine, scripts: Vec::new(), effects, registry, shared, data_dir: None }
    }

    /// `every(secs, f)` and `after(secs, f)` call `f` each `secs` seconds,
    /// or once after them, and return an ID that `cancel(id)` takes.
    fn register_timers(engine: &mut Engine, shared: &Rc<RefCell<Shared>>) {
        fn add(shared: &Rc<RefCell<Shared>>, secs: f64, call: FnPtr, repeat: bool) -> Result<i64, Box<EvalAltResult>> {
            let mut shared = shared.borrow_mut();
            let script = shared.script("a timer")?;
            if shared.timers.iter().filter(|t| t.script == script).count() >= TIMERS_MAX {
                return Err(format!("{TIMERS_MAX} timers at most per script").into());
            }
            // Timers run on the once-a-second clock.
            let after = Duration::from_secs_f64(secs.clamp(1.0, 7.0 * 24.0 * 3600.0));
            shared.next_timer += 1;
            let id = shared.next_timer;
            shared.timers.push(Timer { id, script, every: repeat.then_some(after), due: Instant::now() + after, call });
            Ok(id)
        }
        for (name, repeat) in [("every", true), ("after", false)] {
            let timers = shared.clone();
            engine.register_fn(name, move |secs: i64, f: FnPtr| add(&timers, secs as f64, f, repeat));
            let timers = shared.clone();
            engine.register_fn(name, move |secs: f64, f: FnPtr| add(&timers, secs, f, repeat));
        }
        let timers = shared.clone();
        engine.register_fn("cancel", move |id: i64| {
            let mut shared = timers.borrow_mut();
            let before = shared.timers.len();
            shared.timers.retain(|t| t.id != id);
            shared.timers.len() != before
        });
    }

    /// `store_get`, `store_set`, `store_remove` and `store_keys`: a map per
    /// script that outlasts restarts.
    fn register_store(engine: &mut Engine, shared: &Rc<RefCell<Shared>>) {
        let get = shared.clone();
        engine.register_fn("store_get", move |key: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            Ok(get.borrow_mut().store("the store")?.1.get(key.as_str()).cloned().unwrap_or(Dynamic::UNIT))
        });
        let set = shared.clone();
        engine.register_fn(
            "store_set",
            move |key: ImmutableString, value: Dynamic| -> Result<(), Box<EvalAltResult>> {
                if !storable(&value) {
                    return Err("store_set keeps text, numbers, true/false, arrays and maps".into());
                }
                let mut shared = set.borrow_mut();
                let (name, store) = shared.store("the store")?;
                store.insert(key.as_str().into(), value);
                shared.dirty.insert(name);
                Ok(())
            },
        );
        let remove = shared.clone();
        engine.register_fn("store_remove", move |key: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            let mut shared = remove.borrow_mut();
            let (name, store) = shared.store("the store")?;
            let old = store.remove(key.as_str()).unwrap_or(Dynamic::UNIT);
            shared.dirty.insert(name);
            Ok(old)
        });
        let keys = shared.clone();
        engine.register_fn("store_keys", move || -> Result<Array, Box<EvalAltResult>> {
            Ok(keys.borrow_mut().store("the store")?.1.keys().map(|k| Dynamic::from(k.to_string())).collect())
        });
    }

    /// `setting(name, default, help)` declares a setting and returns its
    /// value; `setting(name)` reads it as it is now.
    fn register_settings(engine: &mut Engine, shared: &Rc<RefCell<Shared>>) {
        let declare = shared.clone();
        engine.register_fn(
            "setting",
            move |name: ImmutableString,
                  default: Dynamic,
                  help: ImmutableString|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let mut shared = declare.borrow_mut();
                let script = shared.script("setting()")?;
                let default = to_toml(&default).ok_or("a setting's default must be text, a number or true/false")?;
                let configured = shared.config.get(&script).and_then(|t| t.get(name.as_str())).cloned();
                let earlier = shared.settings.iter().position(|s| s.script == script && s.name == name.as_str());
                // This session's value (from before a reload), else config.toml's, else the default.
                let value = earlier
                    .map(|i| shared.settings[i].value.clone())
                    .into_iter()
                    .chain(configured)
                    .find_map(|v| same_kind(&default, v))
                    .unwrap_or_else(|| default.clone());
                let setting = Setting { script, name: name.to_string(), help: help.to_string(), default, value };
                let result = from_toml(&setting.value);
                match earlier {
                    Some(i) => shared.settings[i] = setting,
                    None => shared.settings.push(setting),
                }
                Ok(result)
            },
        );
        let read = shared.clone();
        engine.register_fn("setting", move |name: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            let shared = read.borrow();
            let script = shared.script("setting()")?;
            let setting = shared.settings.iter().find(|s| s.script == script && s.name == name.as_str());
            let setting =
                setting.ok_or_else(|| format!("no setting {name}; declare it with setting(name, default, help)"))?;
            Ok(from_toml(&setting.value))
        });
    }

    /// Keep stores in `dir` (one JSON file per script).
    pub fn set_data_dir(&mut self, dir: PathBuf) {
        self.data_dir = Some(dir);
    }

    fn store_path(&self, name: &str) -> Option<PathBuf> {
        Some(self.data_dir.as_ref()?.join(format!("{name}.json")))
    }

    /// Write the stores that changed. Returns any errors.
    pub fn save_stores(&mut self) -> Vec<String> {
        let dirty: Vec<String> = self.shared.borrow_mut().dirty.drain().collect();
        let mut errors = Vec::new();
        for name in dirty {
            let Some(path) = self.store_path(&name) else { continue };
            let result = (|| -> anyhow::Result<()> {
                let store = self.shared.borrow().stores.get(&name).cloned().unwrap_or_default();
                let json = serde_json::to_string(&Dynamic::from_map(store))?;
                anyhow::ensure!(json.len() <= STORE_LIMIT, "its store is over 1 MB, so it wasn't saved");
                crate::config::write_private(&path, json.as_bytes())
            })();
            if let Err(e) = result {
                errors.push(format!("script {name}: {e:#}"));
            }
        }
        errors
    }

    /// config.toml's `[script_settings]`: values for settings scripts
    /// declare, now or when they next load.
    pub fn configure(&mut self, config: &BTreeMap<String, toml::Table>) {
        let mut shared = self.shared.borrow_mut();
        for setting in &mut shared.settings {
            if let Some(value) = config.get(&setting.script).and_then(|t| t.get(&setting.name)) {
                setting.value = same_kind(&setting.default, value.clone()).unwrap_or_else(|| setting.default.clone());
            }
        }
        shared.config = config.clone();
    }

    /// The loaded scripts' settings: (`script.name`, value, help).
    pub fn settings(&self) -> Vec<(String, toml::Value, String)> {
        let loaded: HashSet<&str> = self.scripts.iter().map(|s| s.name.as_str()).collect();
        let shared = self.shared.borrow();
        shared
            .settings
            .iter()
            .filter(|s| loaded.contains(s.script.as_str()))
            .map(|s| (format!("{}.{}", s.script, s.name), s.value.clone(), s.help.clone()))
            .collect()
    }

    /// `/set script.name value` for this session.
    pub fn set_setting(&mut self, key: &str, value: toml::Value) -> Result<toml::Value, String> {
        let loaded: HashSet<String> = self.scripts.iter().map(|s| s.name.clone()).collect();
        let mut shared = self.shared.borrow_mut();
        let setting = shared
            .settings
            .iter_mut()
            .find(|s| loaded.contains(&s.script) && format!("{}.{}", s.script, s.name) == key)
            .ok_or_else(|| format!("no setting {key}; /set lists them"))?;
        let kind = setting.default.type_str();
        setting.value = same_kind(&setting.default, value).ok_or_else(|| format!("{key} takes {kind}"))?;
        Ok(setting.value.clone())
    }

    /// `[script_settings]` as this session has them: config.toml's, with
    /// every declared setting that's been changed from its default.
    pub fn settings_table(&self) -> BTreeMap<String, toml::Table> {
        let shared = self.shared.borrow();
        let mut table = shared.config.clone();
        for s in &shared.settings {
            let in_file = table.get(&s.script).is_some_and(|t| t.contains_key(&s.name));
            if in_file || s.value != s.default {
                table.entry(s.script.clone()).or_default().insert(s.name.clone(), s.value.clone());
            }
        }
        table
    }

    /// The status bar items, in script order.
    pub fn status_items(&self) -> Vec<String> {
        let shared = self.shared.borrow();
        self.scripts
            .iter()
            .filter_map(|s| match shared.status.get(&s.name)? {
                Status::Text(text) | Status::Live(_, text) => Some(text.clone()),
            })
            .filter(|text| !text.is_empty())
            .collect()
    }

    /// Ask the live status items for their text (once a second).
    pub fn refresh_status(&mut self) -> (Vec<Effect>, Vec<String>) {
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        for index in 0..self.scripts.len() {
            let name = self.scripts[index].name.clone();
            let live = match self.shared.borrow().status.get(&name) {
                Some(Status::Live(f, _)) => f.clone(),
                _ => continue,
            };
            self.shared.borrow_mut().current = name.clone();
            let result = live.call::<Dynamic>(&self.engine, &self.scripts[index].ast, ());
            let effects = self.take_effects();
            match result {
                Ok(text) => {
                    let text = if text.is_unit() { String::new() } else { text.to_string() };
                    if let Some(Status::Live(_, last)) = self.shared.borrow_mut().status.get_mut(&name) {
                        *last = text.lines().next().unwrap_or_default().to_owned();
                    }
                    out.extend(self.limit(index, effects));
                }
                Err(e) => {
                    // One error, not one a second: the item goes.
                    self.shared.borrow_mut().status.remove(&name);
                    errors.push(format!("{name}: status: {e}"));
                }
            }
        }
        (out, errors)
    }

    /// The scripts directory's `.rhai` files, sorted.
    pub fn available(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| Some(e.ok()?.path())).collect())
            .unwrap_or_default();
        files.retain(|p| p.extension().is_some_and(|e| e == "rhai"));
        files.sort();
        files
    }

    pub fn load_file(&mut self, path: &Path) -> Result<Vec<Effect>, String> {
        let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "script".into());
        self.load(&name, Some(path.to_owned()), &source)
    }

    /// Compile and run a script's top level (where it registers its
    /// commands and hooks), replacing one of the same name. Returns what
    /// it printed or asked for while loading.
    pub fn load(&mut self, name: &str, path: Option<PathBuf>, source: &str) -> Result<Vec<Effect>, String> {
        *self.registry.borrow_mut() = Registry::default();
        let ast = self.engine.compile(source).map_err(|e| format!("{name}: {e}"))?;
        self.load_store(name)?;
        let (old_status, first_new_timer) = {
            let mut shared = self.shared.borrow_mut();
            shared.current = name.to_owned();
            (shared.status.remove(name), shared.next_timer + 1)
        };
        let result = self.engine.run_ast(&ast);
        let registry = std::mem::take(&mut *self.registry.borrow_mut());
        let effects = self.take_effects();
        // The new version's timers replace the old one's; a broken new
        // version leaves the old one (its item and timers) running.
        let failed = result.is_err();
        self.shared.borrow_mut().timers.retain(|t| t.script != name || (t.id >= first_new_timer) != failed);
        if let Err(e) = result {
            if let Some(old) = old_status {
                self.shared.borrow_mut().status.insert(name.to_owned(), old);
            }
            return Err(format!("{name}: {e}"));
        }
        let commands =
            registry.commands.into_iter().map(|(name, help, handler)| ScriptCommand { name, help, handler }).collect();
        self.scripts.retain(|s| s.name != name);
        self.scripts.push(Script {
            name: name.to_owned(),
            path,
            ast,
            on_message: registry.on_message,
            hooks: registry.hooks,
            commands,
            sent: VecDeque::new(),
        });
        Ok(effects)
    }

    /// Read a script's store from disk, the first time it loads.
    fn load_store(&mut self, name: &str) -> Result<(), String> {
        if self.shared.borrow().stores.contains_key(name) {
            return Ok(());
        }
        let store = match self.store_path(name).map(std::fs::read_to_string) {
            Some(Ok(json)) => serde_json::from_str::<Dynamic>(&json)
                .ok()
                .and_then(|d| d.try_cast::<Map>())
                .ok_or_else(|| format!("{name}: its store isn't a JSON object; move it aside to start afresh"))?,
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{name}: its store: {e}")),
            _ => Map::new(),
        };
        self.shared.borrow_mut().stores.insert(name.to_owned(), store);
        Ok(())
    }

    pub fn unload(&mut self, name: &str) -> bool {
        let before = self.scripts.len();
        self.scripts.retain(|s| s.name != name);
        let mut shared = self.shared.borrow_mut();
        shared.status.remove(name);
        shared.timers.retain(|t| t.script != name);
        self.scripts.len() != before
    }

    /// (name, file, commands, message hooks) for each loaded script.
    pub fn list(&self) -> Vec<(String, Option<PathBuf>, Vec<String>, usize)> {
        self.scripts
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.path.clone(),
                    s.commands.iter().map(|c| c.name.clone()).collect(),
                    s.on_message.len() + s.hooks.len(),
                )
            })
            .collect()
    }

    pub fn path_of(&self, name: &str) -> Option<PathBuf> {
        self.scripts.iter().find(|s| s.name == name)?.path.clone()
    }

    /// Every script command: (name, help, script).
    pub fn commands(&self) -> Vec<(String, String, String)> {
        self.scripts
            .iter()
            .flat_map(|s| s.commands.iter().map(move |c| (c.name.clone(), c.help.clone(), s.name.clone())))
            .collect()
    }

    /// Run a script command, if a script has one called `name`.
    pub fn run_command(&mut self, name: &str, args: &str, context: &Context) -> Option<Result<Vec<Effect>, String>> {
        let (index, handler) = self
            .scripts
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.commands.iter().find(|c| c.name == name).map(|c| (i, c.handler.clone())))?;
        let script = &self.scripts[index];
        self.shared.borrow_mut().current = script.name.clone();
        let result =
            handler.call::<Dynamic>(&self.engine, &script.ast, (ImmutableString::from(args), context.to_map()));
        let effects = self.take_effects();
        Some(match result {
            Ok(_) => Ok(self.limit(index, effects)),
            Err(e) => Err(format!("{}: {e}", self.scripts[index].name)),
        })
    }

    /// Hand a message to every script's hooks. Returns what they asked for
    /// and any errors.
    #[cfg(test)]
    pub fn on_message(&mut self, message: &Message) -> (Vec<Effect>, Vec<String>) {
        self.on_event(&Event::Message(message.clone()))
    }

    /// Hand something that happened to the scripts' hooks for it. Returns
    /// what they asked for and any errors.
    pub fn on_event(&mut self, event: &Event) -> (Vec<Effect>, Vec<String>) {
        let map = match event {
            Event::Message(m) => m.to_map(),
            Event::Join(m) | Event::Leave(m) => m.to_map(),
            Event::Reaction(r) => r.to_map(),
        };
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        for index in 0..self.scripts.len() {
            let script = &self.scripts[index];
            let handlers: Vec<FnPtr> = match event {
                Event::Message(m) if m.highlight => {
                    script.on_message.iter().chain(&script.hooks.highlight).cloned().collect()
                }
                Event::Message(_) => script.on_message.clone(),
                Event::Join(_) => script.hooks.join.clone(),
                Event::Leave(_) => script.hooks.leave.clone(),
                Event::Reaction(_) => script.hooks.reaction.clone(),
            };
            self.shared.borrow_mut().current = script.name.clone();
            for handler in handlers {
                let result = handler.call::<Dynamic>(&self.engine, &self.scripts[index].ast, (map.clone(),));
                let effects = self.take_effects();
                match result {
                    Ok(_) => out.extend(self.limit(index, effects)),
                    Err(e) => errors.push(format!("{}: {e}", self.scripts[index].name)),
                }
            }
        }
        (out, errors)
    }

    /// Call the timers that are due (once a second).
    pub fn run_timers(&mut self) -> (Vec<Effect>, Vec<String>) {
        let now = Instant::now();
        let due: Vec<(i64, String, FnPtr)> = self
            .shared
            .borrow()
            .timers
            .iter()
            .filter(|t| t.due <= now)
            .map(|t| (t.id, t.script.clone(), t.call.clone()))
            .collect();
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        for (id, script, call) in due {
            // Run it again later, or not at all.
            {
                let mut shared = self.shared.borrow_mut();
                match shared.timers.iter().position(|t| t.id == id) {
                    Some(i) => match shared.timers[i].every {
                        Some(every) => shared.timers[i].due = now + every,
                        None => drop(shared.timers.remove(i)),
                    },
                    // Cancelled by an earlier one.
                    None => continue,
                }
            }
            let Some(index) = self.scripts.iter().position(|s| s.name == script) else { continue };
            self.shared.borrow_mut().current = script.clone();
            let result = call.call::<Dynamic>(&self.engine, &self.scripts[index].ast, ());
            let effects = self.take_effects();
            match result {
                Ok(_) => out.extend(self.limit(index, effects)),
                Err(e) => {
                    // One error, not one a second.
                    self.shared.borrow_mut().timers.retain(|t| t.id != id);
                    errors.push(format!("{script}: timer: {e}"));
                }
            }
        }
        (out, errors)
    }

    /// `/script exec`: run a snippet once, with the same functions.
    pub fn exec(&mut self, source: &str) -> Result<Vec<Effect>, String> {
        self.shared.borrow_mut().current.clear();
        let result = self.engine.eval::<Dynamic>(source);
        let mut effects = self.take_effects();
        let value = result.map_err(|e| e.to_string())?;
        if !value.is_unit() {
            effects.push(Effect::Print(value.to_string()));
        }
        Ok(effects)
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.borrow_mut())
    }

    /// Drop sends beyond the script's allowance, saying so once.
    fn limit(&mut self, index: usize, effects: Vec<Effect>) -> Vec<Effect> {
        let script = &mut self.scripts[index];
        let now = Instant::now();
        while script.sent.front().is_some_and(|t| now - *t > SEND_WINDOW) {
            script.sent.pop_front();
        }
        let mut out = Vec::new();
        let mut dropped = false;
        for effect in effects {
            if effect.sends() {
                if script.sent.len() >= SEND_LIMIT {
                    dropped = true;
                    continue;
                }
                script.sent.push_back(now);
            }
            out.push(effect);
        }
        if dropped {
            out.push(Effect::Print(format!(
                "script {} is sending too fast ({SEND_LIMIT} per {}s); some of it was dropped",
                script.name,
                SEND_WINDOW.as_secs()
            )));
        }
        out
    }
}

/// Whether a value can go in a store (as JSON).
fn storable(value: &Dynamic) -> bool {
    if value.is_unit() || value.is_bool() || value.is_int() || value.is_float() || value.is_string() || value.is_char()
    {
        return true;
    }
    if let Some(array) = value.read_lock::<Array>() {
        return array.iter().all(storable);
    }
    value.read_lock::<Map>().is_some_and(|map| map.values().all(storable))
}

fn to_toml(value: &Dynamic) -> Option<toml::Value> {
    if let Ok(text) = value.as_immutable_string_ref() {
        return Some(toml::Value::String(text.to_string()));
    }
    value
        .as_int()
        .map(toml::Value::Integer)
        .or_else(|_| value.as_float().map(toml::Value::Float))
        .or_else(|_| value.as_bool().map(toml::Value::Boolean))
        .ok()
}

fn from_toml(value: &toml::Value) -> Dynamic {
    match value {
        toml::Value::String(s) => s.clone().into(),
        toml::Value::Integer(n) => (*n).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        _ => Dynamic::UNIT,
    }
}

/// `value` as the same kind as `default` (a whole number will do for a
/// decimal one), or `None`.
fn same_kind(default: &toml::Value, value: toml::Value) -> Option<toml::Value> {
    match (default, value) {
        (toml::Value::Float(_), toml::Value::Integer(n)) => Some(toml::Value::Float(n as f64)),
        (default, value) if default.same_type(&value) => Some(value),
        _ => None,
    }
}

/// A small xorshift generator seeded from the clock: fine for dice, not
/// for secrets.
fn random_u64() -> u64 {
    thread_local!(static STATE: std::cell::Cell<u64> = std::cell::Cell::new(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) | 1
    ));
    STATE.with(|state| {
        let mut x = state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.set(x);
        x
    })
}

impl Context {
    fn to_map(&self) -> Map {
        let mut map = Map::new();
        map.insert("server".into(), self.server.clone().into());
        map.insert("room".into(), self.room.clone().into());
        map.insert("room_id".into(), self.room_id.clone().into());
        map
    }
}

impl Member {
    fn to_map(&self) -> Map {
        let mut map = self.context.to_map();
        map.insert("user".into(), self.user.clone().into());
        map.insert("nick".into(), self.nick.clone().into());
        map
    }
}

impl Reaction {
    fn to_map(&self) -> Map {
        let mut map = self.context.to_map();
        map.insert("nick".into(), self.nick.clone().into());
        map.insert("key".into(), self.key.clone().into());
        map.insert("target".into(), self.target.clone().into());
        map.insert("text".into(), self.text.clone().into());
        map
    }
}

impl Message {
    fn to_map(&self) -> Map {
        let mut map = self.context.to_map();
        map.insert("sender".into(), self.sender.clone().into());
        map.insert("nick".into(), self.nick.clone().into());
        map.insert("body".into(), self.body.clone().into());
        map.insert("highlight".into(), self.highlight.into());
        map.insert("dm".into(), self.dm.into());
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(body: &str) -> Message {
        Message {
            context: Context { server: "home".into(), room: "Night Owls".into(), room_id: "!a:x.org".into() },
            sender: "@sam:x.org".into(),
            nick: "sam".into(),
            body: body.into(),
            ..Message::default()
        }
    }

    #[test]
    fn scripts_add_commands_and_hear_messages() {
        let mut scripts = Scripts::new();
        let loaded = scripts
            .load(
                "demo",
                None,
                r#"
                print("demo loaded");
                command("shout", "Say it louder", |args, ctx| say(args.to_upper() + "!"));
                on_message(|msg| {
                    if matches(msg.body, "(?i)\\bpizza\\b") {
                        print(msg.nick + " mentioned pizza in " + msg.room);
                        bell();
                    }
                });
            "#,
            )
            .unwrap();
        assert_eq!(loaded, [Effect::Print("demo loaded".into())]);
        assert_eq!(scripts.commands(), [("shout".into(), "Say it louder".into(), "demo".into())]);

        let said = scripts.run_command("shout", "hello", &Context::default()).unwrap().unwrap();
        assert_eq!(said, [Effect::Say("HELLO!".into())]);
        assert!(scripts.run_command("nope", "", &Context::default()).is_none());

        let (heard, errors) = scripts.on_message(&message("who wants Pizza?"));
        assert!(errors.is_empty());
        assert_eq!(heard, [Effect::Print("sam mentioned pizza in Night Owls".into()), Effect::Bell]);
        assert!(scripts.on_message(&message("salad")).0.is_empty());
    }

    #[test]
    fn scripts_are_sandboxed_and_limited() {
        let mut scripts = Scripts::new();
        assert!(scripts.load("loop", None, "loop {}").unwrap_err().contains("loop"), "runaway loops stop");
        assert!(scripts.load("eval", None, r#"eval("1")"#).is_err(), "no eval");
        assert!(scripts.load("broken", None, "let x = ;").is_err());

        scripts.load("chatty", None, r#"command("spam", "", |a, c| { for i in 0..10 { say("hi") } });"#).unwrap();
        let effects = scripts.run_command("spam", "", &Context::default()).unwrap().unwrap();
        let says = effects.iter().filter(|e| matches!(e, Effect::Say(_))).count();
        assert_eq!(says, SEND_LIMIT, "sending is rate-limited");
        assert!(matches!(effects.last(), Some(Effect::Print(text)) if text.contains("too fast")));
    }

    #[test]
    fn reloading_replaces_and_exec_runs_snippets() {
        let mut scripts = Scripts::new();
        scripts.load("a", None, r#"command("one", "", |a, c| print("1"));"#).unwrap();
        scripts.load("a", None, r#"command("two", "", |a, c| print("2"));"#).unwrap();
        let names: Vec<String> = scripts.commands().into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["two"], "the new version replaces the old");
        assert!(scripts.unload("a") && !scripts.unload("a"));
        assert_eq!(scripts.exec("40 + 2").unwrap(), [Effect::Print("42".into())]);
        assert_eq!(scripts.exec(r#"print("hi")"#).unwrap(), [Effect::Print("hi".into())]);

        let counter = r#"
            let count = 0;
            on_message(|msg| { count += 1; });
            command("count", "", |args, ctx| print(`${count} messages so far`));
        "#;
        scripts.load("count", None, counter).unwrap();
        scripts.on_message(&message("one"));
        scripts.on_message(&message("two"));
        let counted = scripts.run_command("count", "", &Context::default()).unwrap();
        assert_eq!(counted, Ok(vec![Effect::Print("2 messages so far".into())]), "handlers keep state");
        scripts.load("count", None, counter).unwrap();
        let counted = scripts.run_command("count", "", &Context::default()).unwrap();
        assert_eq!(counted, Ok(vec![Effect::Print("0 messages so far".into())]), "reloading starts over");
    }

    #[test]
    fn stores_outlast_restarts() {
        let dir = std::env::temp_dir().join(format!("textrix-store-{}", std::process::id()));
        let source = r#"
            command("remember", "", |args, ctx| {
                store_set("last", args);
                store_set("count", (store_get("count") ?? 0) + 1);
            });
            command("recall", "", |args, ctx| print(`${store_get("last")} (${store_get("count")})`));
            command("forget", "", |args, ctx| { store_remove("last"); print(store_keys().len().to_string()); });
            command("bad", "", |args, ctx| store_set("t", timestamp()));
        "#;
        let mut scripts = Scripts::new();
        scripts.set_data_dir(dir.clone());
        scripts.load("memo", None, source).unwrap();
        for words in ["hi", "there"] {
            scripts.run_command("remember", words, &Context::default()).unwrap().unwrap();
        }
        assert!(scripts.save_stores().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("memo.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "stores may hold messages: only you can read them");
        }

        // A new session, as after a restart.
        let mut later = Scripts::new();
        later.set_data_dir(dir.clone());
        later.load("memo", None, source).unwrap();
        let mut run = |name: &str| later.run_command(name, "", &Context::default()).unwrap();
        assert_eq!(run("recall"), Ok(vec![Effect::Print("there (2)".into())]));
        assert_eq!(run("forget"), Ok(vec![Effect::Print("1".into())]));
        assert!(run("bad").unwrap_err().contains("store_set keeps text"), "only what JSON can hold");
        assert!(later.exec(r#"store_get("x")"#).unwrap_err().contains("scripts loaded from a file"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_come_from_config_and_set() {
        let mut scripts = Scripts::new();
        let file = toml::Table::from_iter([
            ("words".to_owned(), toml::Value::String("pizza".into())),
            ("loud".to_owned(), toml::Value::String("yes".into())),
        ]);
        scripts.configure(&BTreeMap::from([("watch".to_owned(), file)]));
        let source = r#"
            let words = setting("words", "deploy", "Words to watch for");
            setting("loud", true, "Ring the bell too");
            setting("limit", 1.5, "A number");
            print(words);
            command("show", "", |a, c| print(`${setting("words")} ${setting("loud")} ${setting("limit")}`));
        "#;
        assert_eq!(scripts.load("watch", None, source), Ok(vec![Effect::Print("pizza".into())]), "config.toml wins");
        let show = |scripts: &mut Scripts| scripts.run_command("show", "", &Context::default()).unwrap().unwrap();
        assert_eq!(show(&mut scripts), [Effect::Print("pizza true 1.5".into())], "the wrong kind falls back");
        let names: Vec<String> = scripts.settings().into_iter().map(|(name, _, _)| name).collect();
        assert_eq!(names, ["watch.words", "watch.loud", "watch.limit"]);

        assert_eq!(scripts.set_setting("watch.limit", toml::Value::Integer(3)), Ok(toml::Value::Float(3.0)));
        assert_eq!(
            scripts.set_setting("watch.loud", toml::Value::String("x".into())),
            Err("watch.loud takes boolean".into())
        );
        assert!(scripts.set_setting("watch.nope", toml::Value::Integer(1)).is_err());
        scripts.set_setting("watch.words", toml::Value::String("outage".into())).unwrap();
        assert_eq!(show(&mut scripts), [Effect::Print("outage true 3.0".into())]);
        assert_eq!(
            scripts.load("watch", None, source).unwrap(),
            [Effect::Print("outage".into())],
            "reloading keeps it"
        );

        let table = &scripts.settings_table()["watch"];
        assert_eq!(table["words"], toml::Value::String("outage".into()));
        assert_eq!(table["loud"], toml::Value::Boolean(true), "the file's mistake is corrected when saved");
        assert_eq!(table["limit"], toml::Value::Float(3.0));
        assert!(scripts.exec(r#"setting("words")"#).is_err());
    }

    #[test]
    fn status_items_show_text_or_ask_a_function() {
        let mut scripts = Scripts::new();
        scripts.load("a", None, r#"status("fixed");"#).unwrap();
        scripts.load("b", None, r#"let n = 0; status(|| { n += 1; `tick ${n}` });"#).unwrap();
        assert_eq!(scripts.status_items(), ["fixed"], "a live item shows once it's been asked");
        scripts.refresh_status();
        scripts.refresh_status();
        assert_eq!(scripts.status_items(), ["fixed", "tick 2"]);

        scripts.load("c", None, r#"status(|| { throw "oops" });"#).unwrap();
        let (_, errors) = scripts.refresh_status();
        assert!(errors.len() == 1 && errors[0].contains("c: status"), "{errors:?}");
        assert!(scripts.refresh_status().1.is_empty(), "a failing item goes, so it fails once");
        assert!(scripts.load("b", None, r#"status("new"); throw "no";"#).is_err());
        assert_eq!(scripts.status_items(), ["fixed", "tick 4"], "a broken reload keeps the old item");
        scripts.unload("a");
        assert_eq!(scripts.status_items(), ["tick 4"]);
    }

    /// Make every timer due now.
    fn time_passes(scripts: &mut Scripts) {
        let now = Instant::now();
        scripts.shared.borrow_mut().timers.iter_mut().for_each(|t| t.due = now);
    }

    #[test]
    fn timers_run_repeat_and_cancel() {
        let mut scripts = Scripts::new();
        let source = r#"
            let ticks = 0;
            let id = every(1, || { ticks += 1; print(`tick ${ticks}`); });
            after(60, || print("later"));
            command("stop", "", |a, c| print(cancel(id).to_string()));
        "#;
        scripts.load("clock", None, source).unwrap();
        assert!(scripts.run_timers().0.is_empty(), "nothing is due yet");
        time_passes(&mut scripts);
        assert_eq!(scripts.run_timers().0, [Effect::Print("tick 1".into()), Effect::Print("later".into())]);
        time_passes(&mut scripts);
        assert_eq!(scripts.run_timers().0, [Effect::Print("tick 2".into())], "after runs once, every again");
        assert_eq!(
            scripts.run_command("stop", "", &Context::default()).unwrap().unwrap(),
            [Effect::Print("true".into())]
        );
        time_passes(&mut scripts);
        assert!(scripts.run_timers().0.is_empty(), "cancelled");

        // Reloading swaps a script's timers; a broken reload keeps them.
        scripts.load("clock", None, r#"every(5, || print("new"));"#).unwrap();
        assert!(scripts.load("clock", None, r#"every(5, || print("newer")); throw "no";"#).is_err());
        time_passes(&mut scripts);
        assert_eq!(scripts.run_timers().0, [Effect::Print("new".into())]);
        scripts.load("broken", None, r#"every(1, || { throw "oops" });"#).unwrap();
        time_passes(&mut scripts);
        let (_, errors) = scripts.run_timers();
        assert!(errors.iter().any(|e| e.contains("broken: timer")), "{errors:?}");
        time_passes(&mut scripts);
        assert!(scripts.run_timers().1.is_empty(), "a failing timer stops, so it fails once");
        scripts.unload("clock");
        time_passes(&mut scripts);
        assert!(scripts.run_timers().0.is_empty(), "unloading stops them");
        assert!(scripts.exec("after(1, || 1)").unwrap_err().contains("scripts loaded from a file"));
    }

    #[test]
    fn hooks_hear_joins_leaves_reactions_and_highlights() {
        let mut scripts = Scripts::new();
        let source = r#"
            on_join(|m| print(`hello ${m.nick} (${m.user}) in ${m.room}`));
            on_leave(|m| print(`bye ${m.nick}`));
            on_reaction(|r| print(`${r.nick} ${r.key} "${r.text}"`));
            on_highlight(|msg| print(`you were mentioned by ${msg.nick}`));
        "#;
        scripts.load("greeter", None, source).unwrap();
        let context = Context { room: "Night Owls".into(), ..Context::default() };
        let member = Member { context: context.clone(), user: "@sam:x.org".into(), nick: "sam".into() };
        assert_eq!(
            scripts.on_event(&Event::Join(member.clone())).0,
            [Effect::Print("hello sam (@sam:x.org) in Night Owls".into())]
        );
        assert_eq!(scripts.on_event(&Event::Leave(member)).0, [Effect::Print("bye sam".into())]);
        let reaction =
            Reaction { context, nick: "sam".into(), key: "🍕".into(), target: "$1".into(), text: "lunch?".into() };
        assert_eq!(scripts.on_event(&Event::Reaction(reaction)).0, [Effect::Print("sam 🍕 \"lunch?\"".into())]);
        assert!(scripts.on_message(&message("hi")).0.is_empty(), "only highlights");
        let mention = Message { highlight: true, ..message("hi you") };
        assert_eq!(scripts.on_message(&mention).0, [Effect::Print("you were mentioned by sam".into())]);
        assert_eq!(scripts.list()[0].3, 4, "four hooks");
    }

    #[test]
    fn the_example_scripts_work() {
        let mut scripts = Scripts::new();
        for (name, source) in [
            ("shout", include_str!("../examples/scripts/shout.rhai")),
            ("roll", include_str!("../examples/scripts/roll.rhai")),
            ("seen", include_str!("../examples/scripts/seen.rhai")),
            ("watchwords", include_str!("../examples/scripts/watchwords.rhai")),
            ("dms", include_str!("../examples/scripts/dms.rhai")),
            ("remind", include_str!("../examples/scripts/remind.rhai")),
        ] {
            assert_eq!(scripts.load(name, None, source), Ok(vec![]), "{name} loads quietly");
        }
        let here = Context { room: "Night Owls".into(), ..Context::default() };
        let mut run = |name: &str, args: &str| scripts.run_command(name, args, &here).unwrap().unwrap();

        assert_eq!(run("shout", "hello"), [Effect::Say("HELLO!".into())]);
        assert!(matches!(&run("shout", "")[..], [Effect::Print(usage)] if usage.starts_with("usage")));

        for _ in 0..4 {
            let [Effect::Say(said)] = &run("roll", "3d6")[..] else { panic!("one roll") };
            let total: i64 = said.rsplit(' ').next().unwrap().parse().unwrap();
            assert!(said.starts_with("🎲 3d6: ") && (3..=18).contains(&total), "{said}");
        }
        let [Effect::Say(one)] = &run("roll", "")[..] else { panic!("one roll") };
        assert!(one.starts_with("🎲 1d6: "), "{one}");
        assert!(matches!(&run("roll", "lots")[..], [Effect::Print(usage)] if usage.starts_with("usage")));

        let (heard, errors) = scripts.on_message(&message("deploy is done, pizza time"));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            heard,
            [
                Effect::Bell,
                Effect::Notify { title: "sam in Night Owls".into(), body: "deploy is done, pizza time".into() }
            ]
        );
        let heard = scripts.on_message(&message("redeployment"));
        assert!(heard.0.is_empty() && heard.1.is_empty(), "whole words only: {heard:?}");

        let seen = scripts.run_command("seen", "Sam", &here).unwrap().unwrap();
        let [Effect::Print(seen)] = &seen[..] else { panic!("{seen:?}") };
        assert_eq!(seen, "sam was in Night Owls 0s ago");
        let never = scripts.run_command("seen", "jo_bloggs", &here).unwrap().unwrap();
        assert!(matches!(&never[..], [Effect::Print(text)] if text.contains("haven't seen jo_bloggs")));

        assert_eq!(scripts.set_setting("watchwords.words", toml::Value::String("lunch".into())).map(|_| ()), Ok(()));
        assert!(scripts.on_message(&message("deploy is done")).0.is_empty(), "the setting is read each time");
        assert_eq!(scripts.on_message(&message("lunch?")).0.len(), 2);

        let dm = |nick: &str| Message { dm: true, nick: nick.into(), ..message("hi") };
        for nick in ["sam", "jo_bloggs", "sam"] {
            scripts.on_message(&dm(nick));
        }
        assert_eq!(scripts.status_items(), ["DMs: 2"]);
        let read = scripts.run_command("dms", "", &here).unwrap().unwrap();
        assert_eq!(read, [Effect::Print("direct messages from sam, jo_bloggs".into())]);
        assert!(scripts.status_items().is_empty());

        let set = scripts.run_command("remind", "10 stretch your legs", &here).unwrap().unwrap();
        assert_eq!(set, [Effect::Print("I'll remind you in 10 min: stretch your legs".into())]);
        assert!(scripts.run_timers().0.is_empty(), "not yet");
        time_passes(&mut scripts);
        let due = scripts.run_timers().0;
        assert!(due.contains(&Effect::Bell) && due.contains(&Effect::Print("⏰ stretch your legs".into())), "{due:?}");
    }
}

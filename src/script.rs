//! irssi-style scripting, in Rhai (https://rhai.rs): `.rhai` files in
//! `~/.textrix/scripts/` add commands and react to messages.
//!
//! Scripts are sandboxed: Rhai has no file or network access, `eval` is off,
//! and every call has an operation limit so a runaway loop can't hang the
//! client. What a script can do is ask for [`Effect`]s, which the app
//! carries out: print a line, say something, run a command, ring the bell.
//! Sending is rate-limited per script, and scripts never see your own
//! messages, so a script can't talk itself into a loop.

use std::{
    cell::RefCell,
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use rhai::{AST, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, Map};

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
    commands: Vec<ScriptCommand>,
    /// When it last sent things, for the rate limit.
    sent: VecDeque<Instant>,
}

/// What a script registered while it loaded.
#[derive(Default)]
struct Registry {
    on_message: Vec<FnPtr>,
    commands: Vec<(String, String, FnPtr)>,
}

/// Sends per script in a [`SEND_WINDOW`].
const SEND_LIMIT: usize = 5;
const SEND_WINDOW: Duration = Duration::from_secs(10);

pub struct Scripts {
    engine: Engine,
    scripts: Vec<Script>,
    effects: Rc<RefCell<Vec<Effect>>>,
    registry: Rc<RefCell<Registry>>,
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
        engine.register_fn("random", |n: i64| -> Result<i64, Box<EvalAltResult>> {
            if n < 1 {
                return Err("random(n) needs n of 1 or more".into());
            }
            Ok((random_u64() % n as u64) as i64)
        });

        let hooks = registry.clone();
        engine.register_fn("on_message", move |handler: FnPtr| hooks.borrow_mut().on_message.push(handler));
        let commands = registry.clone();
        engine.register_fn("command", move |name: ImmutableString, help: ImmutableString, handler: FnPtr| {
            let name = name.trim_start_matches('/').to_lowercase();
            commands.borrow_mut().commands.push((name, help.to_string(), handler));
        });

        Self { engine, scripts: Vec::new(), effects, registry }
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
        let result = self.engine.run_ast(&ast);
        let registry = std::mem::take(&mut *self.registry.borrow_mut());
        let effects = self.take_effects();
        result.map_err(|e| format!("{name}: {e}"))?;
        let commands =
            registry.commands.into_iter().map(|(name, help, handler)| ScriptCommand { name, help, handler }).collect();
        self.unload(name);
        self.scripts.push(Script {
            name: name.to_owned(),
            path,
            ast,
            on_message: registry.on_message,
            commands,
            sent: VecDeque::new(),
        });
        Ok(effects)
    }

    pub fn unload(&mut self, name: &str) -> bool {
        let before = self.scripts.len();
        self.scripts.retain(|s| s.name != name);
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
                    s.on_message.len(),
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
    pub fn on_message(&mut self, message: &Message) -> (Vec<Effect>, Vec<String>) {
        let map = message.to_map();
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        for index in 0..self.scripts.len() {
            for handler in self.scripts[index].on_message.clone() {
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

    /// `/script exec`: run a snippet once, with the same functions.
    pub fn exec(&mut self, source: &str) -> Result<Vec<Effect>, String> {
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
    fn the_example_scripts_work() {
        let mut scripts = Scripts::new();
        for (name, source) in [
            ("shout", include_str!("../examples/scripts/shout.rhai")),
            ("roll", include_str!("../examples/scripts/roll.rhai")),
            ("seen", include_str!("../examples/scripts/seen.rhai")),
            ("watchwords", include_str!("../examples/scripts/watchwords.rhai")),
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
        assert!(seen.starts_with("sam was in Night Owls 0s ago: redeployment"), "{seen}");
        let never = scripts.run_command("seen", "jo_bloggs", &here).unwrap().unwrap();
        assert!(matches!(&never[..], [Effect::Print(text)] if text.contains("haven't seen jo_bloggs")));
    }
}

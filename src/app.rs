//! Application state: windows, accounts, keys and slash commands.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use matrix_sdk::ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, TransactionId, UserId,
    events::room::message::{AddMentions, ForwardThread, ReplacementMetadata, ReplyMetadata, RoomMessageEventContent},
};
use ratatui::DefaultTerminal;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    compose::{Say, compose},
    config::{Config, Paths},
    convert::attachment,
    input::{Candidate, Input},
    matrix::{self, Account, AppEvent, Moderation, Tx, VerifyAnswer},
    model::{AccountName, Activity, Item, Line, LineKind, Member, RoomMeta},
    servers::{EnvFile, ServerDef, Servers},
    ui,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WinKey {
    pub account: String,
    pub room_id: OwnedRoomId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Syncing,
    Online,
    Lagging,
    Offline,
}

pub struct AccountState {
    pub def: ServerDef,
    pub user: String,
    pub nick: Option<String>,
    pub status: ConnStatus,
    /// `Some(false)` once we know this device is not cross-signed.
    pub verified: Option<bool>,
    pub account: Option<Account>,
}

#[derive(Clone, Debug)]
enum History {
    Unloaded,
    Loading,
    More(String),
    Done,
}

pub struct Window {
    /// `None` for the status window.
    pub room: Option<WinKey>,
    pub name: String,
    pub topic: Option<String>,
    pub encrypted: bool,
    pub is_dm: bool,
    pub lines: VecDeque<Line>,
    pub activity: Activity,
    /// Rows scrolled up from the bottom.
    pub scroll: usize,
    /// Set by the renderer when the oldest loaded line is on screen.
    pub at_top: bool,
    pub unseen_below: bool,
    pub typing: Vec<String>,
    pub members: Vec<Member>,
    members_requested: bool,
    /// Server-side unread counts from the last room update.
    unread: (u64, u64),
    history: History,
    history_retry: Option<Instant>,
    last_receipt: Option<OwnedEventId>,
    /// The last event you had seen: a "new messages" divider follows it.
    pub marker: Option<OwnedEventId>,
    /// How far other members have read: user -> (nick, event).
    pub read_by: HashMap<OwnedUserId, (String, OwnedEventId)>,
    /// Edits/reactions/redactions whose target is not loaded yet.
    orphans: HashMap<OwnedEventId, Vec<Item>>,
}

const ORPHANS_MAX: usize = 1000;

impl Window {
    fn status() -> Self {
        Self::new(None, "status".into())
    }

    fn room(key: WinKey, meta: &RoomMeta) -> Self {
        let mut w = Self::new(Some(key), meta.name.clone());
        w.update(meta);
        w
    }

    fn new(room: Option<WinKey>, name: String) -> Self {
        Self {
            room,
            name,
            topic: None,
            encrypted: false,
            is_dm: false,
            lines: VecDeque::new(),
            activity: Activity::None,
            scroll: 0,
            at_top: false,
            unseen_below: false,
            typing: Vec::new(),
            members: Vec::new(),
            members_requested: false,
            unread: (0, 0),
            history: History::Unloaded,
            history_retry: None,
            last_receipt: None,
            marker: None,
            read_by: HashMap::new(),
            orphans: HashMap::new(),
        }
    }

    /// Refresh room details. Returns the activity implied by the server's
    /// unread counts having grown (messages that arrived while we were away).
    fn update(&mut self, meta: &RoomMeta) -> Activity {
        self.name = meta.name.clone();
        self.topic = meta.topic.clone();
        self.encrypted = meta.encrypted;
        self.is_dm = meta.is_dm;
        if self.marker.is_none() {
            self.marker = meta.fully_read.clone();
        }
        let (unread, highlights) = self.unread;
        self.unread = (meta.unread, meta.highlights);
        if meta.highlights > highlights || (meta.is_dm && meta.unread > unread) {
            Activity::Highlight
        } else if meta.unread > unread {
            Activity::Message
        } else {
            Activity::None
        }
    }

    pub fn account(&self) -> Option<&str> {
        self.room.as_ref().map(|k| k.account.as_str())
    }

    fn find_mut(&mut self, id: &EventId) -> Option<&mut Line> {
        self.lines.iter_mut().rev().find(|l| l.event_id.as_deref() == Some(id))
    }

    fn contains(&self, id: &EventId) -> bool {
        self.lines.iter().rev().any(|l| l.event_id.as_deref() == Some(id))
    }

    /// The newest loaded event, for read markers.
    fn latest_event(&self) -> Option<OwnedEventId> {
        self.lines.iter().rev().find(|l| !l.pending && l.event_id.is_some()).and_then(|l| l.event_id.clone())
    }

    /// Append a line, merging our own local echo and skipping duplicates.
    /// Returns whether a new line was added.
    fn push(&mut self, mut line: Line, cap: usize) -> bool {
        // Our own message coming back from the server replaces its local
        // echo, whether or not the send acknowledgement got here first: the
        // server copy carries what the echo can't (e.g. the uploaded media).
        if let (Some(txn), Some(id)) = (&line.txn_id, &line.event_id)
            && let Some(echo) = self.lines.iter_mut().rev().find(|l| {
                l.own && l.txn_id.as_ref() == Some(txn) && l.event_id.as_ref().is_none_or(|e| e == id)
            })
        {
            line.reply_nick = echo.reply_nick.take();
            line.reply_snippet = echo.reply_snippet.take();
            line.reactions = std::mem::take(&mut echo.reactions);
            if echo.edited || echo.redacted {
                (line.body, line.edited, line.redacted) = (std::mem::take(&mut echo.body), echo.edited, echo.redacted);
            }
            *echo = line;
            return false;
        }
        if let Some(id) = &line.event_id {
            if self.contains(id) {
                return false;
            }
            if let Some(items) = self.orphans.remove(id) {
                items.iter().for_each(|item| annotate(&mut line, item));
            }
        }
        if let Some(target) = &line.reply_to
            && let Some(parent) = self.lines.iter().rev().find(|l| l.event_id.as_ref() == Some(target))
        {
            (line.reply_nick, line.reply_snippet) = (Some(parent.nick.clone()), Some(snippet(&parent.body)));
        }
        self.lines.push_back(line);
        while self.lines.len() > cap {
            self.lines.pop_front();
        }
        true
    }

    /// Apply one timeline item. Returns the line if one was appended.
    fn apply(&mut self, item: Item, cap: usize, live: bool) -> Option<Line> {
        let target = match item {
            Item::Line(line) => {
                let copy = line.clone();
                return self.push(line, cap).then_some(copy);
            }
            ref other => other.target().expect("annotation has a target").to_owned(),
        };
        if let Some(line) = self.find_mut(&target) {
            annotate(line, &item);
            return None;
        }
        match item {
            Item::Edit { fallback, .. } if live => {
                let copy = fallback.clone();
                self.push(fallback, cap).then_some(copy)
            }
            other => {
                self.orphan(target, other);
                None
            }
        }
    }

    fn orphan(&mut self, target: OwnedEventId, item: Item) {
        if self.orphans.len() >= ORPHANS_MAX {
            self.orphans.clear();
        }
        self.orphans.entry(target).or_default().push(item);
    }

    /// Merge a page of history (chronological). Pages are normally older
    /// than everything loaded, but the first page is "the latest events" and
    /// can overlap or run past what sync has delivered so far, so new lines
    /// are placed relative to events the window already has.
    fn merge_history(&mut self, items: Vec<Item>) {
        let mut annotations = Vec::new();
        let mut before_first_anchor: Vec<Line> = Vec::new();
        let mut insert_at: Option<usize> = None;
        let mut seen: HashSet<OwnedEventId> = HashSet::new();

        for item in items {
            let mut line = match item {
                Item::Line(line) => line,
                other => {
                    annotations.push(other);
                    continue;
                }
            };
            if let Some(id) = &line.event_id {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(pos) = self.lines.iter().position(|l| l.event_id.as_ref() == Some(id)) {
                    // Already loaded: earlier new lines go right before it,
                    // later ones right after it.
                    let earlier = before_first_anchor.len();
                    for (offset, line) in before_first_anchor.drain(..).enumerate() {
                        self.lines.insert(pos + offset, line);
                    }
                    insert_at = Some(pos + earlier + 1);
                    continue;
                }
                if let Some(items) = self.orphans.remove(id) {
                    items.iter().for_each(|item| annotate(&mut line, item));
                }
            }
            match insert_at.as_mut() {
                Some(at) => {
                    self.lines.insert(*at, line);
                    *at += 1;
                }
                None => before_first_anchor.push(line),
            }
        }
        // No overlap with what is loaded: the whole page is older.
        for line in before_first_anchor.into_iter().rev() {
            self.lines.push_front(line);
        }

        for item in annotations {
            let Some(target) = item.target().map(ToOwned::to_owned) else { continue };
            match self.find_mut(&target) {
                Some(line) => annotate(line, &item),
                None => self.orphan(target, item),
            }
        }

        let parents: HashMap<OwnedEventId, (String, String)> = self
            .lines
            .iter()
            .filter_map(|l| Some((l.event_id.clone()?, (l.nick.clone(), snippet(&l.body)))))
            .collect();
        for line in self.lines.iter_mut().filter(|l| l.reply_nick.is_none()) {
            if let Some((nick, text)) = line.reply_to.as_ref().and_then(|t| parents.get(t)) {
                (line.reply_nick, line.reply_snippet) = (Some(nick.clone()), Some(text.clone()));
            }
        }
    }

    fn utd_events(&self, sessions: Option<&[String]>) -> Vec<OwnedEventId> {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Utd)
            .filter(|l| match (sessions, &l.utd_session) {
                (None, _) => true,
                (Some(sessions), Some(s)) => sessions.contains(s),
                (Some(_), None) => false,
            })
            .filter_map(|l| l.event_id.clone())
            .collect()
    }

    fn last_message(&self, own: bool) -> Option<&Line> {
        self.lines.iter().rev().find(|l| l.own == own && is_target(l))
    }

    /// For replies and reactions: the last message from someone else, or
    /// your own when you are talking to yourself.
    fn last_message_to_answer(&self) -> Option<&Line> {
        self.last_message(false).or_else(|| self.lines.iter().rev().find(|l| is_target(l)))
    }

    /// `/reply mikey hi` or `/react mikey 👍`: the last message from a nick
    /// named at the start of `args` (most recent speaker wins), else the
    /// last message to answer. Returns the target and the remaining text.
    fn answer_target<'a>(&self, args: &'a str) -> (Option<&Line>, &'a str) {
        let lower = args.to_lowercase();
        let mut tried = HashSet::new();
        for line in self.lines.iter().rev().filter(|l| is_target(l)) {
            if line.nick.is_empty() || !tried.insert(line.nick.as_str()) {
                continue;
            }
            let Some(after) = lower.strip_prefix(&line.nick.to_lowercase()) else { continue };
            if !after.starts_with([' ', ':', ',']) {
                continue;
            }
            // Lowercasing keeps the char count, so map the match back by chars.
            let skip = args.char_indices().nth(line.nick.chars().count()).map_or(args.len(), |(i, _)| i);
            let rest = args[skip..].trim_start_matches([':', ',']).trim_start();
            if !rest.is_empty() {
                return (Some(line), rest);
            }
        }
        (self.last_message_to_answer(), args)
    }

    /// The newest attachment, optionally from one nick.
    fn last_media(&self, nick: Option<&str>) -> Option<&Line> {
        self.lines.iter().rev().find(|l| {
            l.media.is_some() && !l.redacted && nick.is_none_or(|n| l.nick.eq_ignore_ascii_case(n))
        })
    }
}

/// The first line of a message, shortened for reply context.
fn snippet(body: &str) -> String {
    let first = body.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(40).collect();
    if first.chars().count() > 40 || body.contains('\n') {
        out.push('…');
    }
    out
}

fn is_target(l: &Line) -> bool {
    l.is_message() && l.kind != LineKind::Utd && !l.pending && !l.redacted && l.event_id.is_some()
}

fn annotate(line: &mut Line, item: &Item) {
    match item {
        Item::Edit { body, fallback, .. } if !line.redacted => {
            line.body = body.clone();
            line.kind = fallback.kind;
            line.edited = true;
        }
        Item::Redact { .. } => {
            line.body = "[message deleted]".into();
            line.redacted = true;
            line.edited = false;
            line.media = None;
            line.reactions.clear();
        }
        Item::React { key, nick, .. } => match line.reactions.iter_mut().find(|(k, _)| k == key) {
            Some((_, nicks)) if !nicks.contains(nick) => nicks.push(nick.clone()),
            Some(_) => {}
            None => line.reactions.push((key.clone(), vec![nick.clone()])),
        },
        _ => {}
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Layout {
    windows: Vec<WinKey>,
    closed: Vec<WinKey>,
}

pub struct App {
    pub cfg: Config,
    paths: Paths,
    env: EnvFile,
    pub accounts: BTreeMap<AccountName, AccountState>,
    pub windows: Vec<Window>,
    pub active: usize,
    pub input: Input,
    tx: Tx,
    pub quit: bool,
    pub bell: bool,
    pub redraw_all: bool,
    pub view_width: u16,
    pub view_height: u16,
    layout: Layout,
    typing_since: Option<(WinKey, Instant)>,
    /// Mentions captured from the input line being submitted.
    mentions: Vec<(String, OwnedUserId)>,
}

struct Cmd {
    name: &'static str,
    args: &'static str,
    help: &'static str,
}

const COMMANDS: &[Cmd] = &[
    Cmd { name: "help", args: "[command]", help: "Show commands, or help for one" },
    Cmd { name: "join", args: "<#alias:server|!room>", help: "Join a room or accept an invite (alias /j)" },
    Cmd { name: "create", args: "[-public] <name>", help: "Create a room (encrypted and invite-only unless -public)" },
    Cmd { name: "name", args: "<new name>", help: "Rename this room" },
    Cmd { name: "topic", args: "[text]", help: "Show or set the room topic" },
    Cmd { name: "info", args: "", help: "Show this room's ID, aliases, encryption and settings" },
    Cmd { name: "rooms", args: "", help: "List every joined room, including closed windows" },
    Cmd { name: "invites", args: "", help: "List pending invitations" },
    Cmd { name: "decline", args: "<!room:server>", help: "Reject an invitation" },
    Cmd { name: "part", args: "", help: "Leave the current room (alias /leave)" },
    Cmd { name: "forget", args: "", help: "Leave this room and remove it from your room list" },
    Cmd { name: "query", args: "<@user:server>", help: "Open a direct-message window" },
    Cmd { name: "msg", args: "<@user|#alias|!room> <text>", help: "Send without switching windows" },
    Cmd { name: "me", args: "<action>", help: "Send an emote" },
    Cmd { name: "notice", args: "<text>", help: "Send a notice" },
    Cmd { name: "say", args: "<text>", help: "Send text as-is, even if it starts with / (or type //)" },
    Cmd { name: "reply", args: "[nick] <text>", help: "Reply to the last message (from nick, if given)" },
    Cmd { name: "react", args: "[nick] <emoji>", help: "React to the last message (from nick, if given)" },
    Cmd { name: "edit", args: "<text>", help: "Replace your last message" },
    Cmd { name: "redact", args: "", help: "Delete your last message (alias /del)" },
    Cmd { name: "upload", args: "<path>", help: "Send a file or image (Tab completes paths)" },
    Cmd { name: "download", args: "[nick]", help: "Save the last attachment (from nick) to Downloads" },
    Cmd { name: "names", args: "", help: "List room members" },
    Cmd { name: "whois", args: "<nick|@user>", help: "Show a user's profile and device trust" },
    Cmd { name: "nick", args: "<name>", help: "Set your display name on this server" },
    Cmd { name: "invite", args: "<nick|@user>", help: "Invite a user to this room" },
    Cmd { name: "kick", args: "<nick|@user> [reason]", help: "Remove a user from this room" },
    Cmd { name: "ban", args: "<nick|@user> [reason]", help: "Ban a user from this room" },
    Cmd { name: "unban", args: "<@user>", help: "Lift a ban" },
    Cmd {
        name: "window",
        args: "<n>|list|close|move <n>|goto <name>",
        help: "Manage windows (alias /win; /wc closes, /wl lists)",
    },
    Cmd { name: "clear", args: "", help: "Clear this window's scrollback" },
    Cmd { name: "servers", args: "", help: "List dotfile servers and their status" },
    Cmd { name: "connect", args: "<server>", help: "Connect a server from the dotfile" },
    Cmd { name: "disconnect", args: "<server>", help: "Stop syncing a server (session is kept)" },
    Cmd { name: "logout", args: "<server>", help: "Log out and delete the local session and keys" },
    Cmd {
        name: "verify",
        args: "[accept|confirm|mismatch|cancel|@user]",
        help: "Emoji-verify this device (no args) or a user",
    },
    Cmd { name: "recover", args: "[server]", help: "Unlock history with the dotfile's RECOVERY_KEY" },
    Cmd { name: "quit", args: "", help: "Exit Textrix (alias /exit)" },
];

const ALIASES: &[(&str, &str)] = &[
    ("j", "join"),
    ("leave", "part"),
    ("q", "query"),
    ("m", "msg"),
    ("win", "window"),
    ("w", "window"),
    ("del", "redact"),
    ("delete", "redact"),
    ("exit", "quit"),
    ("h", "help"),
];

/// Server-name arguments, for tab completion.
const SERVER_COMMANDS: &[&str] = &["connect", "disconnect", "logout", "recover"];

impl App {
    pub fn new(cfg: Config, paths: Paths, env: EnvFile, servers: Servers, tx: Tx) -> Self {
        let layout = std::fs::read(paths.layout_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let accounts = servers
            .defs
            .into_iter()
            .map(|def| {
                let state = AccountState {
                    user: def.user.clone(),
                    def: def.clone(),
                    nick: None,
                    status: ConnStatus::Offline,
                    verified: None,
                    account: None,
                };
                (AccountName::from(def.name.as_str()), state)
            })
            .collect();
        let mut app = Self {
            cfg,
            paths,
            env,
            accounts,
            windows: vec![Window::status()],
            active: 0,
            input: Input::default(),
            tx,
            quit: false,
            bell: false,
            redraw_all: false,
            view_width: 80,
            view_height: 24,
            layout,
            typing_since: None,
            mentions: Vec::new(),
        };
        app.status(Line::info(format!("Textrix {} — irssi-style Matrix client", env!("CARGO_PKG_VERSION"))));
        app.status(Line::info(format!("servers from {}", app.env.path().display())));
        for warning in servers.warnings {
            app.status(Line::error(warning));
        }
        app.status(Line::info("type /help for commands; Alt+1..0 or Ctrl+N/Ctrl+P switch windows"));
        app
    }

    /// Connect every server defined in the dotfile.
    pub fn start(&mut self) {
        let names: Vec<AccountName> = self.accounts.keys().cloned().collect();
        for name in names {
            self.connect(&name);
        }
    }

    fn connect(&mut self, name: &str) {
        let Some(state) = self.accounts.get_mut(name) else { return };
        state.status = ConnStatus::Connecting;
        let (def, paths, env, tx) = (state.def.clone(), self.paths.clone(), self.env.clone(), self.tx.clone());
        let homeserver = def.homeserver_or_server_name().unwrap_or("?").to_owned();
        self.status(Line::info(format!("{name}: connecting to {homeserver} as {}", def.user)));
        let name = AccountName::from(name);
        tokio::spawn(async move {
            let event = match matrix::connect(def, paths, env, tx.clone()).await {
                Ok(account) => AppEvent::Connected(account),
                Err(e) => AppEvent::ConnectFailed { account: name, error: format!("{e:#}") },
            };
            let _ = tx.send(event);
        });
    }

    pub fn servers_summary(&self) -> String {
        let parts: Vec<String> = self
            .accounts
            .iter()
            .map(|(name, s)| {
                let status = match s.status {
                    ConnStatus::Online => "online",
                    ConnStatus::Connecting => "connecting",
                    ConnStatus::Syncing => "syncing",
                    ConnStatus::Lagging => "lagging",
                    ConnStatus::Offline => "offline",
                };
                format!("{name} ({status})")
            })
            .collect();
        parts.join(", ")
    }

    pub fn shutdown(&mut self) {
        self.stop_typing();
        self.save_layout();
        for state in self.accounts.values() {
            if let Some(account) = &state.account {
                account.stop();
            }
        }
    }

    // ---- helpers ---------------------------------------------------------

    fn win(&self) -> &Window {
        &self.windows[self.active]
    }

    fn win_mut(&mut self) -> &mut Window {
        &mut self.windows[self.active]
    }

    fn print(&mut self, line: Line) {
        let cap = self.cfg.scrollback;
        self.win_mut().push(line, cap);
    }

    fn info(&mut self, text: impl Into<String>) {
        self.print(Line::info(text));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.print(Line::error(text));
    }

    fn status(&mut self, line: Line) {
        let cap = self.cfg.scrollback;
        let raise = if line.kind == LineKind::Error { Activity::Message } else { Activity::Event };
        let status = &mut self.windows[0];
        status.push(line, cap);
        if self.active != 0 {
            status.activity = status.activity.max(raise);
        }
    }

    fn connected(&self, name: &str) -> Option<Account> {
        self.accounts.get(name)?.account.clone()
    }

    fn window_idx(&self, account: &str, room_id: &OwnedRoomId) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.room.as_ref().is_some_and(|k| k.account == account && k.room_id == *room_id))
    }

    /// The account and room of the active window.
    fn room_ctx(&self) -> Result<(Account, OwnedRoomId), String> {
        let key = self.win().room.as_ref().ok_or("not in a room window")?;
        let account = self.connected(&key.account).ok_or_else(|| format!("{} is not connected", key.account))?;
        Ok((account, key.room_id.clone()))
    }

    /// The active window's room, for routing command output there.
    fn here(&self) -> Option<OwnedRoomId> {
        self.win().room.as_ref().map(|k| k.room_id.clone())
    }

    /// The account a command applies to: `-server` prefix, else the active
    /// window's server, else the only connected server.
    fn pick_account<'a>(&self, args: &'a str) -> Result<(Account, &'a str), String> {
        if let Some(rest) = args.trim_start().strip_prefix('-') {
            let (name, rest) = split_first(rest);
            let name = name.to_lowercase();
            if self.accounts.contains_key(name.as_str()) {
                return self.connected(&name).map(|a| (a, rest)).ok_or_else(|| format!("{name} is not connected"));
            }
        }
        if let Some(name) = self.win().account() {
            return self.connected(name).map(|a| (a, args)).ok_or_else(|| format!("{name} is not connected"));
        }
        let mut connected = self.accounts.values().filter_map(|s| s.account.clone());
        match (connected.next(), connected.next()) {
            (Some(account), None) => Ok((account, args)),
            (None, _) => Err("no server is connected".into()),
            (Some(_), Some(_)) => {
                Err("several servers are connected: switch to a room on the one you mean, or use -<server>".into())
            }
        }
    }

    fn resolve_user(&self, arg: &str) -> Result<OwnedUserId, String> {
        if arg.starts_with('@') {
            return UserId::parse(arg).map_err(|e| format!("invalid user ID {arg}: {e}"));
        }
        let wanted = arg.to_lowercase();
        let win = self.win();
        win.members
            .iter()
            .find(|m| m.name.to_lowercase() == wanted)
            .map(|m| m.user_id.clone())
            .or_else(|| {
                win.lines.iter().rev().find(|l| l.nick.to_lowercase() == wanted).and_then(|l| l.sender.clone())
            })
            .ok_or_else(|| format!("no one called {arg} here; use the full @user:server ID"))
    }

    fn own_nick(&self, account: &Account) -> String {
        self.accounts
            .get(&*account.name)
            .and_then(|s| s.nick.clone())
            .unwrap_or_else(|| account.user_id.localpart().to_owned())
    }

    // ---- matrix events ---------------------------------------------------

    pub fn on_event(&mut self, ev: AppEvent) {
        let cap = self.cfg.scrollback;
        match ev {
            AppEvent::Connected(account) => {
                let name = account.name.clone();
                self.status(Line::info(format!(
                    "{name}: logged in as {} on {} (device {})",
                    account.user_id,
                    account.homeserver,
                    account.client.device_id().map(|d| d.to_string()).unwrap_or_default()
                )));
                if let Some(state) = self.accounts.get_mut(&name) {
                    state.status = ConnStatus::Syncing;
                    account.start();
                    state.account = Some(account);
                }
            }
            AppEvent::ConnectFailed { account, error } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Offline;
                    if let Some(acct) = state.account.take() {
                        acct.stop();
                    }
                }
                self.status(Line::error(format!("{account}: {error}")));
            }
            AppEvent::Synced { account, first } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Online;
                }
                let text = if first { "synced" } else { "connection restored" };
                self.status(Line::info(format!("{account}: {text}")));
            }
            AppEvent::SyncError { account, error } => {
                let was_lagging = self.accounts.get(&account).is_some_and(|s| s.status == ConnStatus::Lagging);
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Lagging;
                }
                if !was_lagging {
                    self.status(Line::error(format!("{account}: sync failed ({error}); retrying")));
                }
            }
            AppEvent::Rooms { account, mut rooms } => {
                rooms.sort_by_cached_key(|m| (m.is_dm, m.name.to_lowercase()));
                for meta in rooms {
                    self.upsert_room(&account, meta);
                }
            }
            AppEvent::RoomGone { account, room_id } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone() };
                self.layout.closed.retain(|k| *k != key);
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    let name = self.windows[idx].name.clone();
                    self.remove_window(idx);
                    self.status(Line::info(format!("{account}: you left {name}")));
                }
            }
            AppEvent::Timeline { account, room_id, items, live } => self.on_timeline(&account, &room_id, items, live),
            AppEvent::History { account, room_id, items, end, failed } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if failed {
                    w.history = end.map_or(History::Unloaded, History::More);
                    w.history_retry = Some(Instant::now() + Duration::from_secs(15));
                } else {
                    w.merge_history(items);
                    w.history = end.map_or(History::Done, History::More);
                    w.history_retry = None;
                }
            }
            AppEvent::Receipts { account, room_id, receipts, live } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                for (user, nick, event_id) in receipts {
                    // Receipts often point at events we don't draw (reactions,
                    // hidden state); a live one means "read up to now".
                    let target = match (w.contains(&event_id), live) {
                        (true, _) | (false, false) => event_id,
                        (false, true) => match w.latest_event() {
                            Some(latest) => latest,
                            None => continue,
                        },
                    };
                    w.read_by.insert(user, (nick, target));
                }
            }
            AppEvent::UploadProgress { account, room_id, txn_id, percent } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                if let Some(line) =
                    self.windows[idx].lines.iter_mut().rev().find(|l| l.pending && l.txn_id.as_ref() == Some(&txn_id))
                {
                    line.progress = Some(percent);
                }
            }
            AppEvent::Typing { account, room_id, nicks } => {
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].typing = nicks;
                }
            }
            AppEvent::Members { account, room_id, members, show } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                self.windows[idx].members = members;
                if show {
                    let lines = self.names_lines(idx);
                    for line in lines {
                        self.windows[idx].push(line, cap);
                    }
                }
            }
            AppEvent::Focus { account, room_id } => {
                let idx = match self.window_idx(&account, &room_id) {
                    Some(idx) => Some(idx),
                    None => self.reopen(&account, &room_id),
                };
                if let Some(idx) = idx {
                    self.switch_to(idx);
                }
            }
            AppEvent::Sent { account, room_id, txn_id, event_id } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if w.contains(&event_id) {
                    // The sync echo won the race; drop the stale local echo.
                    w.lines.retain(|l| !(l.pending && l.txn_id.as_ref() == Some(&txn_id)));
                } else if let Some(line) = w.lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id)) {
                    line.pending = false;
                    line.progress = None;
                    line.event_id = Some(event_id);
                }
            }
            AppEvent::SendFailed { account, room_id, txn_id, error } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if let Some(line) = w.lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id)) {
                    line.pending = false;
                    line.progress = None;
                    line.body.push_str("  [not sent]");
                }
                w.push(Line::error(format!("not sent: {error}")), cap);
            }
            AppEvent::KeysReceived { account, room_id, sessions } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let events = self.windows[idx].utd_events(Some(&sessions));
                if let (false, Some(acct)) = (events.is_empty(), self.connected(&account)) {
                    acct.redecrypt(room_id, events);
                }
            }
            AppEvent::Recovered { account } => {
                let Some(acct) = self.connected(&account) else { return };
                for w in &self.windows {
                    if let Some(key) = w.room.as_ref().filter(|k| *k.account == *account) {
                        let events = w.utd_events(None);
                        if !events.is_empty() {
                            acct.redecrypt(key.room_id.clone(), events);
                        }
                    }
                }
            }
            AppEvent::Redecrypted { account, room_id, item } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                match item {
                    Item::Line(line) if line.kind != LineKind::Utd => {
                        let Some(id) = line.event_id.clone() else { return };
                        if let Some(existing) = w.find_mut(&id) {
                            let reactions = std::mem::take(&mut existing.reactions);
                            *existing = line;
                            existing.reactions = reactions;
                        }
                    }
                    Item::Line(_) => {}
                    other => {
                        w.apply(other, cap, false);
                    }
                }
            }
            AppEvent::Profile { account, nick } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.nick = Some(nick);
                }
            }
            AppEvent::Trust { account, verified } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.verified = Some(verified);
                }
            }
            AppEvent::LoggedOut { account } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.account = None;
                    state.status = ConnStatus::Offline;
                }
                while let Some(idx) = self.windows.iter().position(|w| w.account() == Some(&*account)) {
                    self.remove_window(idx);
                }
                self.layout.windows.retain(|k| *k.account != *account);
                self.layout.closed.retain(|k| *k.account != *account);
                self.status(Line::info(format!("{account}: logged out; local session and keys removed")));
            }
            AppEvent::Notice { account, room_id, line } => {
                let idx = match (account, room_id) {
                    (Some(account), Some(room_id)) => self.window_idx(&account, &room_id),
                    _ => None,
                };
                match idx {
                    Some(idx) => {
                        self.windows[idx].push(line, cap);
                    }
                    None => self.status(line),
                }
            }
        }
    }

    fn on_timeline(&mut self, account: &str, room_id: &OwnedRoomId, items: Vec<Item>, live: bool) {
        let idx = match self.window_idx(account, room_id) {
            Some(idx) => idx,
            None => {
                let wakes = live
                    && items.iter().any(|i| matches!(i, Item::Line(l) if l.is_message() && !l.own));
                match wakes.then(|| self.reopen(account, room_id)).flatten() {
                    Some(idx) => idx,
                    None => return,
                }
            }
        };
        let cap = self.cfg.scrollback;
        let width = self.view_width;
        let viewing = idx == self.active;
        let mut ring = false;
        let mut mirrored = Vec::new();
        for item in items {
            let Some(line) = self.windows[idx].apply(item, cap, live) else { continue };
            let w = &mut self.windows[idx];
            if w.scroll > 0 {
                w.scroll += ui::line_height(&line, width, &self.cfg);
                w.unseen_below = true;
            }
            // Sending a message means you've read everything before it.
            if let (Some(sender), Some(event_id)) = (&line.sender, &line.event_id)
                && line.is_message()
                && !line.own
            {
                w.read_by.insert(sender.clone(), (line.nick.clone(), event_id.clone()));
            }
            if !live || line.own {
                continue;
            }
            let level = if line.highlight || (w.is_dm && line.is_message()) {
                Activity::Highlight
            } else if line.is_message() {
                Activity::Message
            } else if line.kind == LineKind::Membership && !self.cfg.show_joins {
                Activity::None
            } else {
                Activity::Event
            };
            if !viewing || w.scroll > 0 {
                w.activity = w.activity.max(level);
            }
            if level == Activity::Highlight && !viewing {
                ring = true;
                // Like irssi's hilight window: copy it to the status window.
                let mut copy = Line::new(line.kind, line.body.clone());
                (copy.ts, copy.nick, copy.sender) = (line.ts, format!("{}/{}", w.name, line.nick), line.sender.clone());
                copy.highlight = true;
                mirrored.push(copy);
            }
        }
        for copy in mirrored {
            self.windows[0].push(copy, cap);
            if self.active != 0 {
                self.windows[0].activity = Activity::Highlight;
            }
        }
        self.bell |= ring && self.cfg.bell;
    }

    fn upsert_room(&mut self, account: &str, meta: RoomMeta) {
        let idx = match self.window_idx(account, &meta.room_id) {
            Some(idx) => idx,
            None => {
                let key = WinKey { account: account.to_owned(), room_id: meta.room_id.clone() };
                if self.layout.closed.contains(&key) {
                    return;
                }
                self.insert_window(Window::new(Some(key), meta.name.clone()))
            }
        };
        let level = self.windows[idx].update(&meta);
        if idx != self.active {
            let w = &mut self.windows[idx];
            w.activity = w.activity.max(level);
        }
    }

    /// Bring back a closed room window, e.g. when someone talks in it.
    fn reopen(&mut self, account: &str, room_id: &OwnedRoomId) -> Option<usize> {
        let meta = self.connected(account)?.quick_meta(room_id)?;
        let key = WinKey { account: account.to_owned(), room_id: room_id.clone() };
        self.layout.closed.retain(|k| *k != key);
        let idx = self.insert_window(Window::room(key, &meta));
        self.windows[idx].unread = (0, 0);
        Some(idx)
    }

    /// Insert a room window at its saved position (or at the end).
    fn insert_window(&mut self, window: Window) -> usize {
        let rank = |k: &WinKey| self.layout.windows.iter().position(|x| x == k);
        let key = window.room.as_ref().expect("room window");
        let pos = match rank(key) {
            Some(r) => self
                .windows
                .iter()
                .position(|w| w.room.as_ref().is_some_and(|k| rank(k).is_none_or(|kr| kr > r)))
                .unwrap_or(self.windows.len()),
            None => self.windows.len(),
        };
        self.windows.insert(pos, window);
        if pos <= self.active {
            self.active += 1;
        }
        pos
    }

    fn remove_window(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        self.windows.remove(idx);
        if self.active >= idx && self.active > 0 {
            self.active -= 1;
        }
    }

    fn save_layout(&self) {
        let mut windows: Vec<WinKey> = self.windows.iter().filter_map(|w| w.room.clone()).collect();
        // Keep saved positions for servers that are not connected right now.
        for key in &self.layout.windows {
            if !windows.contains(key) && self.connected(&key.account).is_none() {
                windows.push(key.clone());
            }
        }
        let layout = Layout { windows, closed: self.layout.closed.clone() };
        if let Ok(json) = serde_json::to_vec_pretty(&layout)
            && let Err(e) = std::fs::write(self.paths.layout_file(), json)
        {
            tracing::warn!("saving layout: {e}");
        }
    }

    fn names_lines(&self, idx: usize) -> Vec<Line> {
        let w = &self.windows[idx];
        let mut lines = vec![Line::info(format!("Users {} ({})", w.name, w.members.len()))];
        let col = w.members.iter().map(|m| m.name.chars().count()).max().unwrap_or(0).clamp(4, 20);
        let per_row = ((self.view_width as usize).saturating_sub(10) / (col + 4)).max(1);
        for chunk in w.members.chunks(per_row) {
            let row: Vec<String> = chunk
                .iter()
                .map(|m| {
                    let name: String = m.name.chars().take(col).collect();
                    format!("[{}{name:<col$}]", m.prefix)
                })
                .collect();
            lines.push(Line::info(row.join(" ")));
        }
        lines
    }

    // ---- per-frame upkeep ------------------------------------------------

    /// Called after every draw: lazy history, member lists, read markers.
    pub fn after_draw(&mut self) {
        let page = self.cfg.history_page;
        let receipts = self.cfg.read_receipts;
        let Some(key) = self.win().room.clone() else { return };
        let Some(acct) = self.connected(&key.account) else { return };
        let w = &mut self.windows[self.active];

        if !w.members_requested {
            w.members_requested = true;
            acct.members(key.room_id.clone(), false);
        }

        let retry_ok = w.history_retry.is_none_or(|t| Instant::now() >= t);
        if w.at_top && retry_ok {
            let from = match &w.history {
                History::Unloaded => Some(None),
                History::More(token) => Some(Some(token.clone())),
                History::Loading | History::Done => None,
            };
            if let Some(from) = from {
                w.history = History::Loading;
                acct.paginate(key.room_id.clone(), from, page);
            }
        }

        if w.scroll == 0
            && let Some(event_id) = w.latest_event()
            && w.last_receipt.as_ref() != Some(&event_id)
        {
            w.last_receipt = Some(event_id.clone());
            acct.mark_read(key.room_id, event_id, receipts);
        }
    }

    // ---- terminal input --------------------------------------------------

    pub fn on_terminal(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => {
                self.input.end_completion();
                self.input.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.typed();
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.code != Tab {
            self.input.end_completion();
        }
        let mut edited = false;
        match key.code {
            Tab => {
                let candidates = self.candidates();
                self.input.complete(move || candidates);
            }
            Enter => self.submit(),
            Char(c) if alt => match c {
                '1'..='9' => self.switch_to(c as usize - '1' as usize),
                '0' => self.switch_to(9),
                'a' => self.next_active(),
                'b' => self.input.word_left(),
                'f' => self.input.word_right(),
                c => {
                    if let Some(i) = "qwertyuiop".find(c) {
                        self.switch_to(10 + i);
                    }
                }
            },
            Char(c) if ctrl => match c {
                'a' => self.input.home(),
                'e' => self.input.end(),
                'b' => self.input.left(),
                'f' => self.input.right(),
                'n' => self.cycle_window(1),
                'p' => self.cycle_window(-1),
                'l' => self.redraw_all = true,
                'd' | 'h' | 'u' | 'k' | 'w' | 'y' | 'c' => {
                    match c {
                        'd' => self.input.delete(),
                        'h' => self.input.backspace(),
                        'u' => self.input.kill_to_start(),
                        'k' => self.input.kill_to_end(),
                        'w' => self.input.kill_word(),
                        'y' => self.input.yank(),
                        _ => self.input.clear(),
                    }
                    edited = true;
                }
                _ => {}
            },
            Char(c) => {
                self.input.insert(c);
                edited = true;
            }
            Backspace => {
                self.input.backspace();
                edited = true;
            }
            Delete => {
                self.input.delete();
                edited = true;
            }
            Left if alt => self.cycle_window(-1),
            Right if alt => self.cycle_window(1),
            Left if ctrl => self.input.word_left(),
            Right if ctrl => self.input.word_right(),
            Left => self.input.left(),
            Right => self.input.right(),
            Home => self.input.home(),
            End => self.input.end(),
            Up => self.input.history_prev(),
            Down => self.input.history_next(),
            PageUp => self.scroll(true),
            PageDown => self.scroll(false),
            _ => {}
        }
        if edited {
            self.typed();
        }
    }

    fn scroll(&mut self, up: bool) {
        let page = (self.view_height as usize / 2).max(1);
        let w = self.win_mut();
        w.scroll = if up { w.scroll + page } else { w.scroll.saturating_sub(page) };
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.windows.len() || idx == self.active {
            return;
        }
        self.stop_typing();
        // Everything in the window you leave counts as seen.
        let left = &mut self.windows[self.active];
        if let Some(latest) = left.latest_event() {
            left.marker = Some(latest);
        }
        self.active = idx;
        self.windows[idx].activity = Activity::None;
    }

    fn cycle_window(&mut self, step: isize) {
        let n = self.windows.len() as isize;
        self.switch_to((self.active as isize + step).rem_euclid(n) as usize);
    }

    /// Alt+A: jump to the most important window with activity.
    fn next_active(&mut self) {
        let best = self.windows.iter().map(|w| w.activity).max().unwrap_or_default();
        if best == Activity::None {
            return;
        }
        let n = self.windows.len();
        if let Some(idx) =
            (1..=n).map(|step| (self.active + step) % n).find(|&i| self.windows[i].activity == best)
        {
            self.switch_to(idx);
        }
    }

    /// Send typing notifications while composing a message in a room.
    fn typed(&mut self) {
        if !self.cfg.send_typing {
            return;
        }
        let Some(key) = self.win().room.clone() else { return };
        if self.input.is_empty() || self.input.chars().first() == Some(&'/') {
            self.stop_typing();
            return;
        }
        let now = Instant::now();
        if let Some((k, since)) = &self.typing_since
            && *k == key
            && now.duration_since(*since) < Duration::from_secs(3)
        {
            return;
        }
        if let Some(acct) = self.connected(&key.account) {
            acct.typing(key.room_id.clone(), true);
        }
        self.typing_since = Some((key, now));
    }

    fn stop_typing(&mut self) {
        if let Some((key, _)) = self.typing_since.take()
            && let Some(acct) = self.connected(&key.account)
        {
            acct.typing(key.room_id, false);
        }
    }

    fn candidates(&self) -> Vec<Candidate> {
        let (word, at_start) = self.input.completion_word();
        let lower = word.to_lowercase();

        if at_start && let Some(prefix) = lower.strip_prefix('/') {
            let mut names: Vec<&str> =
                COMMANDS.iter().map(|c| c.name).chain(ALIASES.iter().map(|a| a.0)).chain(["wc", "wl"]).collect();
            names.sort_unstable();
            return names
                .into_iter()
                .filter(|n| n.starts_with(prefix))
                .map(|n| Candidate { text: format!("/{n} "), user: None })
                .collect();
        }

        let text = self.input.text();
        let first = text.split_whitespace().next().unwrap_or("");
        if SERVER_COMMANDS.iter().any(|c| first == format!("/{c}")) {
            return self
                .accounts
                .keys()
                .filter(|n| n.starts_with(&lower))
                .map(|n| Candidate { text: format!("{n} "), user: None })
                .collect();
        }
        if first == "/upload" {
            return path_candidates(&word);
        }

        if word.is_empty() {
            return Vec::new();
        }
        let suffix = if at_start { format!("{} ", self.cfg.completion_char) } else { " ".to_owned() };
        let win = self.win();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        // Recent speakers first, like irssi.
        let speakers = win
            .lines
            .iter()
            .rev()
            .filter(|l| !l.own && l.is_message())
            .filter_map(|l| Some((l.nick.as_str(), l.sender.as_ref()?)));
        let members = win.members.iter().map(|m| (m.name.as_str(), &m.user_id));
        for (nick, user) in speakers.chain(members) {
            let matches = if word.starts_with('@') {
                user.as_str().starts_with(&word)
            } else {
                nick.to_lowercase().starts_with(&lower)
            };
            if matches && seen.insert(user.clone()) {
                let text = if word.starts_with('@') { user.to_string() } else { nick.to_owned() };
                out.push(Candidate { text: format!("{text}{suffix}"), user: Some(user.clone()) });
            }
        }
        out
    }

    // ---- commands --------------------------------------------------------

    fn submit(&mut self) {
        self.mentions = std::mem::take(&mut self.input.mentions);
        let text = self.input.take();
        self.stop_typing();
        if text.trim().is_empty() {
            return;
        }
        self.win_mut().scroll = 0;
        if let Some(rest) = text.strip_prefix("//") {
            self.say(&format!("/{rest}"), Say::Text);
        } else if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd);
        } else {
            self.say(&text, Say::Text);
        }
        self.mentions.clear();
    }

    fn command(&mut self, line: &str) {
        let (name, args) = split_first(line);
        let name = name.to_lowercase();
        let name = ALIASES.iter().find(|(alias, _)| *alias == name).map_or(name.as_str(), |(_, real)| *real);
        let result = match name {
            "help" => {
                self.cmd_help(args);
                Ok(())
            }
            "say" => {
                self.say(args, Say::Text);
                Ok(())
            }
            "me" => {
                self.say(args, Say::Emote);
                Ok(())
            }
            "notice" => {
                self.say(args, Say::Notice);
                Ok(())
            }
            "msg" => self.cmd_msg(args),
            "reply" => self.cmd_reply(args),
            "edit" => self.cmd_edit(args),
            "redact" => self.cmd_redact(),
            "react" => self.cmd_react(args),
            "upload" => self.cmd_upload(args),
            "download" => self.cmd_download(args),
            "join" => self.pick_account(args).and_then(|(acct, target)| {
                let target = target.trim();
                if target.is_empty() {
                    return Err("usage: /join <#alias:server|!room>".into());
                }
                acct.join(target.to_owned());
                Ok(())
            }),
            "create" => self.cmd_create(args),
            "name" => self.room_ctx().and_then(|(acct, room_id)| {
                let name = args.trim();
                if name.is_empty() {
                    return Err("usage: /name <new name>".into());
                }
                acct.rename(room_id, name.to_owned());
                Ok(())
            }),
            "info" => self.room_ctx().map(|(acct, room_id)| acct.room_info(room_id)),
            "rooms" => self.pick_account(args).map(|(acct, _)| acct.list_rooms(self.here())),
            "invites" => self.pick_account(args).map(|(acct, _)| acct.list_invites(self.here())),
            "decline" => self.pick_account(args).and_then(|(acct, room)| {
                let room_id =
                    RoomId::parse(room.trim()).map_err(|_| "usage: /decline <!room:server> (see /invites)")?;
                acct.decline(self.here(), room_id);
                Ok(())
            }),
            "part" => self.room_ctx().map(|(acct, room_id)| acct.leave(room_id)),
            "forget" => self.room_ctx().map(|(acct, room_id)| acct.forget(room_id)),
            "query" => self.pick_account(args).and_then(|(acct, who)| {
                let user = self.resolve_user(who.trim())?;
                acct.query(user);
                Ok(())
            }),
            "topic" => self.cmd_topic(args),
            "names" => self.room_ctx().map(|(acct, room_id)| acct.members(room_id, true)),
            "whois" => self.pick_account(args).and_then(|(acct, who)| {
                let user = self.resolve_user(who.trim())?;
                acct.whois(self.here(), user);
                Ok(())
            }),
            "nick" => self.pick_account(args).and_then(|(acct, nick)| {
                let nick = nick.trim();
                if nick.is_empty() {
                    return Err("usage: /nick <name>".into());
                }
                acct.set_nick(nick.to_owned());
                Ok(())
            }),
            "invite" => self.cmd_moderate(Moderation::Invite, args),
            "kick" => self.cmd_moderate(Moderation::Kick, args),
            "ban" => self.cmd_moderate(Moderation::Ban, args),
            "unban" => self.cmd_moderate(Moderation::Unban, args),
            "window" => self.cmd_window(args),
            "wc" => self.cmd_window("close"),
            "wl" => self.cmd_window("list"),
            "clear" => {
                let w = self.win_mut();
                w.lines.clear();
                w.scroll = 0;
                w.history = History::Done;
                Ok(())
            }
            "servers" => {
                self.cmd_servers();
                Ok(())
            }
            "connect" => self.cmd_connect(args),
            "disconnect" => self.cmd_disconnect(args),
            "logout" => self.server_arg(args).map(|acct| acct.logout()),
            "recover" => self.server_arg(args).map(|acct| acct.recover()),
            "verify" => self.cmd_verify(args),
            "quit" => {
                self.quit = true;
                Ok(())
            }
            other => Err(format!("unknown command /{other}; try /help")),
        };
        if let Err(e) = result {
            self.error(e);
        }
    }

    fn say(&mut self, text: &str, kind: Say) {
        if text.trim().is_empty() {
            return;
        }
        let (acct, room_id) = match self.room_ctx() {
            Ok(ctx) => ctx,
            Err(e) => return self.error(e),
        };
        let content = compose(text, kind, &self.mentions, self.cfg.markdown);
        self.send_content(acct, room_id, content, text, kind.line_kind(), None);
    }

    /// Show a local echo, then send.
    fn send_content(
        &mut self,
        acct: Account,
        room_id: OwnedRoomId,
        content: RoomMessageEventContent,
        text: &str,
        kind: LineKind,
        reply_to: Option<OwnedEventId>,
    ) {
        let txn_id = TransactionId::new();
        let mut line = Line::new(kind, text);
        line.reply_to = reply_to;
        self.echo(&acct, line, &txn_id);
        acct.send(room_id, content, txn_id);
    }

    /// Show our own pending line. Talking also means you've caught up, so
    /// the "new messages" divider goes away.
    fn echo(&mut self, acct: &Account, mut line: Line, txn_id: &TransactionId) {
        line.own = true;
        line.pending = true;
        line.txn_id = Some(txn_id.to_owned());
        line.nick = self.own_nick(acct);
        line.sender = Some(acct.user_id.clone());
        let w = self.win_mut();
        w.marker = w.latest_event();
        self.print(line);
    }

    fn cmd_msg(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (target, text) = split_first(args);
        if target.is_empty() || text.trim().is_empty() {
            return Err("usage: /msg <@user|#alias|!room> <text>".into());
        }
        let target = if target.starts_with(['@', '#', '!']) {
            target.to_owned()
        } else {
            self.resolve_user(target)?.to_string()
        };
        acct.send_to(target, compose(text, Say::Text, &self.mentions, self.cfg.markdown));
        Ok(())
    }

    fn cmd_reply(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (target, text) = self.win().answer_target(args);
        if text.trim().is_empty() {
            return Err("usage: /reply [nick] <text>".into());
        }
        let target = target.ok_or("nothing to reply to")?;
        let (event_id, sender) =
            (target.event_id.clone().expect("filtered"), target.sender.clone().ok_or("unknown sender")?);
        let text = text.to_owned();
        let content = compose(&text, Say::Text, &self.mentions, self.cfg.markdown).make_reply_to(
            ReplyMetadata::new(&event_id, &sender, None),
            ForwardThread::Yes,
            AddMentions::Yes,
        );
        self.send_content(acct, room_id, content, &text, LineKind::Message, Some(event_id));
        Ok(())
    }

    fn cmd_edit(&mut self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("usage: /edit <text>".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        let target = self.win().last_message(true).ok_or("you have no message here to edit")?;
        let event_id = target.event_id.clone().expect("filtered");
        let kind = if target.kind == LineKind::Emote { Say::Emote } else { Say::Text };
        let content = compose(text, kind, &self.mentions, self.cfg.markdown)
            .make_replacement(ReplacementMetadata::new(event_id.clone(), None));
        if let Some(line) = self.win_mut().find_mut(&event_id) {
            line.body = text.to_owned();
            line.edited = true;
        }
        acct.send(room_id, content, TransactionId::new());
        Ok(())
    }

    fn cmd_redact(&mut self) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let target = self.win().last_message(true).ok_or("you have no message here to delete")?;
        acct.redact(room_id, target.event_id.clone().expect("filtered"));
        Ok(())
    }

    fn cmd_react(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (target, key) = self.win().answer_target(args.trim());
        let key = key.trim();
        if key.is_empty() {
            return Err("usage: /react [nick] <emoji>".into());
        }
        let target = target.ok_or("nothing to react to")?;
        acct.react(room_id, target.event_id.clone().expect("filtered"), key.to_owned());
        Ok(())
    }

    fn cmd_upload(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if args.trim().is_empty() {
            return Err("usage: /upload <path>".into());
        }
        let path = parse_path(args);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let kind = match mime_guess::from_path(&path).first_or_octet_stream().type_().as_str() {
            kind @ ("image" | "video" | "audio") => kind.to_owned(),
            _ => "file".to_owned(),
        };
        let txn_id = TransactionId::new();
        let mut line = Line::new(LineKind::Message, attachment(&kind, None, &name));
        line.progress = Some(0);
        self.echo(&acct, line, &txn_id);
        self.info(format!("uploading {name} ({})", matrix::human_size(meta.len())));
        acct.upload(room_id, path, txn_id);
        Ok(())
    }

    fn cmd_download(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let nick = Some(args.trim()).filter(|n| !n.is_empty());
        let line = self.win().last_media(nick).ok_or_else(|| match nick {
            Some(nick) => format!("no attachment from {nick} here"),
            None => "no attachment here".to_owned(),
        })?;
        let media = line.media.clone().expect("filtered");
        acct.download(room_id, media);
        Ok(())
    }

    fn cmd_create(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (public, name) = match args.trim().strip_prefix("-public") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        if name.is_empty() {
            return Err("usage: /create [-public] <name>".into());
        }
        acct.create_room(name.to_owned(), public);
        Ok(())
    }

    fn cmd_topic(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if args.trim().is_empty() {
            let text = match &self.win().topic {
                Some(topic) => format!("Topic for {}: {topic}", self.win().name),
                None => format!("No topic set for {}", self.win().name),
            };
            self.info(text);
        } else {
            acct.set_topic(room_id, args.trim().to_owned());
        }
        Ok(())
    }

    fn cmd_moderate(&mut self, action: Moderation, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (who, reason) = split_first(args);
        if who.is_empty() {
            return Err(format!("usage: /{} <nick|@user>", format!("{action:?}").to_lowercase()));
        }
        let user = self.resolve_user(who)?;
        let reason = Some(reason.trim()).filter(|r| !r.is_empty()).map(str::to_owned);
        acct.moderate(room_id, action, user, reason);
        Ok(())
    }

    fn cmd_window(&mut self, args: &str) -> Result<(), String> {
        let (sub, rest) = split_first(args);
        match sub {
            "" | "list" => {
                let rows: Vec<String> = self
                    .windows
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let server = w.account().map(|a| format!(" ({a})")).unwrap_or_default();
                        let enc = if w.encrypted { " [E2E]" } else { "" };
                        format!("{:>3}: {}{server}{enc}", i + 1, w.name)
                    })
                    .collect();
                for row in rows {
                    self.info(row);
                }
                let closed = self.layout.closed.len();
                if closed > 0 {
                    self.info(format!("{closed} closed window(s) reopen when someone talks; /rooms lists all"));
                }
            }
            "close" => {
                let Some(key) = self.win().room.clone() else {
                    return Err("the status window can't be closed".into());
                };
                if !self.layout.closed.contains(&key) {
                    self.layout.closed.push(key);
                }
                self.remove_window(self.active);
                self.save_layout();
            }
            "move" => {
                let n: usize = rest.trim().parse().map_err(|_| "usage: /window move <n>")?;
                if self.active == 0 {
                    return Err("the status window stays at 1".into());
                }
                let target = n.clamp(2, self.windows.len()) - 1;
                let w = self.windows.remove(self.active);
                self.windows.insert(target, w);
                self.active = target;
                self.save_layout();
            }
            "goto" => self.goto_name(rest.trim())?,
            n if n.parse::<usize>().is_ok() => {
                let n: usize = n.parse().unwrap_or(1);
                if n == 0 || n > self.windows.len() {
                    return Err(format!("no window {n}"));
                }
                self.switch_to(n - 1);
            }
            name => self.goto_name(name)?,
        }
        Ok(())
    }

    fn goto_name(&mut self, name: &str) -> Result<(), String> {
        let wanted = name.to_lowercase();
        let idx = self
            .windows
            .iter()
            .position(|w| w.name.to_lowercase() == wanted)
            .or_else(|| self.windows.iter().position(|w| w.name.to_lowercase().contains(&wanted)))
            .ok_or_else(|| format!("no window matching {name}"))?;
        self.switch_to(idx);
        Ok(())
    }

    fn cmd_servers(&mut self) {
        let rows: Vec<String> = self
            .accounts
            .iter()
            .map(|(name, s)| {
                let hs = s.def.homeserver_or_server_name().unwrap_or("?");
                format!("{name}: {} on {hs} — {:?}", s.def.user, s.status)
            })
            .collect();
        for row in rows {
            self.info(row);
        }
    }

    fn cmd_connect(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().to_lowercase();
        let state = self.accounts.get(name.as_str()).ok_or_else(|| {
            if name.is_empty() { "usage: /connect <server>".into() } else { format!("no server named {name} in the dotfile") }
        })?;
        if state.account.is_some() || state.status == ConnStatus::Connecting {
            return Err(format!("{name} is already connected"));
        }
        self.connect(&name);
        Ok(())
    }

    fn cmd_disconnect(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().to_lowercase();
        let state = self.accounts.get_mut(name.as_str()).ok_or("usage: /disconnect <server>")?;
        let account = state.account.take().ok_or_else(|| format!("{name} is not connected"))?;
        account.stop();
        state.status = ConnStatus::Offline;
        self.status(Line::info(format!("{name}: disconnected")));
        Ok(())
    }

    /// `/logout <server>` and `/recover [server]`.
    fn server_arg(&self, args: &str) -> Result<Account, String> {
        let name = args.trim().to_lowercase();
        if name.is_empty() {
            return self.pick_account("").map(|(a, _)| a);
        }
        self.connected(&name).ok_or_else(|| format!("{name} is not connected"))
    }

    fn cmd_verify(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        match args.trim() {
            "" => acct.verify_request(None),
            "accept" => acct.verify_accept(),
            "confirm" | "yes" => acct.verify_answer(VerifyAnswer::Confirm),
            "mismatch" | "no" => acct.verify_answer(VerifyAnswer::Mismatch),
            "cancel" => acct.verify_answer(VerifyAnswer::Cancel),
            who => acct.verify_request(Some(self.resolve_user(who)?)),
        }
        Ok(())
    }

    fn cmd_help(&mut self, args: &str) {
        let wanted = args.trim().trim_start_matches('/').to_lowercase();
        if wanted.is_empty() {
            self.info("Commands (Tab completes; /help <command> for details):");
            let rows: Vec<String> = COMMANDS.iter().map(|c| format!("  /{:<10} {}", c.name, c.help)).collect();
            for row in rows {
                self.info(row);
            }
            self.info("Keys: Alt+1..0/Alt+Q..P window 1-20, Ctrl+N/P next/prev, Alt+A next active,");
            self.info("      PgUp/PgDn scroll, Tab complete, Up/Down history, Ctrl+A/E/U/K/W/Y edit");
            return;
        }
        let real = ALIASES.iter().find(|(a, _)| *a == wanted).map_or(wanted.as_str(), |(_, r)| *r);
        match COMMANDS.iter().find(|c| c.name == real) {
            Some(c) => {
                let text = format!("/{} {} — {}", c.name, c.args, c.help);
                self.info(text);
            }
            None => self.error(format!("no command /{wanted}")),
        }
    }
}

/// Split off the first whitespace-separated word.
fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// A path as typed or dropped into the terminal: quoted, backslash-escaped,
/// `~/…`, or a `file://` URL.
fn parse_path(arg: &str) -> PathBuf {
    let arg = arg.trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| arg.strip_prefix(*q).and_then(|a| a.strip_suffix(*q)))
        .map(str::to_owned);
    let raw = unquoted.unwrap_or_else(|| {
        let mut out = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.extend(chars.next()),
                c => out.push(c),
            }
        }
        out
    });
    let raw = match raw.strip_prefix("file://") {
        Some(url_path) => percent_decode(url_path),
        None => raw,
    };
    expand_tilde(&raw)
}

fn expand_tilde(p: &str) -> PathBuf {
    match (p.strip_prefix('~'), dirs::home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(p),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tab completion for `/upload`: entries of the directory being typed.
fn path_candidates(word: &str) -> Vec<Candidate> {
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let dir = if dir_part.is_empty() { PathBuf::from(".") } else { expand_tilde(dir_part) };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<Candidate> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = Path::new(&dir).join(&name).is_dir();
            let text = if is_dir { format!("{dir_part}{name}/") } else { format!("{dir_part}{name} ") };
            Some(Candidate { text, user: None })
        })
        .collect();
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

/// The main loop: draw, then wait for a key, a Matrix event, or the clock.
pub async fn run(terminal: &mut DefaultTerminal, mut app: App, mut rx: UnboundedReceiver<AppEvent>) -> Result<()> {
    let mut keys = EventStream::new();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    app.start();
    loop {
        if std::mem::take(&mut app.redraw_all) {
            terminal.clear()?;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if std::mem::take(&mut app.bell) {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07").and_then(|()| out.flush());
        }
        app.after_draw();
        if app.quit {
            break;
        }
        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(ev)) => app.on_terminal(ev),
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(ev) = rx.recv() => {
                app.on_event(ev);
                while let Ok(ev) = rx.try_recv() {
                    app.on_event(ev);
                }
            }
            _ = clock.tick() => {}
        }
    }
    app.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, body: &str) -> Line {
        let mut line = Line::new(LineKind::Message, body);
        line.event_id = Some(OwnedEventId::try_from(id).unwrap());
        line.nick = "alice".into();
        line
    }

    fn ids(w: &Window) -> Vec<&str> {
        w.lines.iter().map(|l| l.event_id.as_ref().unwrap().as_str()).collect()
    }

    #[test]
    fn local_echo_is_replaced_by_server_copy() {
        let mut w = Window::status();
        let txn = TransactionId::new();
        let mut echo = Line::new(LineKind::Message, "hi");
        (echo.own, echo.pending, echo.txn_id) = (true, true, Some(txn.clone()));
        assert!(w.push(echo, 100));

        let mut server = msg("$a", "hi");
        server.txn_id = Some(txn);
        assert!(!w.push(server, 100), "echo is merged, not appended");
        assert_eq!(w.lines.len(), 1);
        assert!(!w.lines[0].pending);
    }

    #[test]
    fn server_copy_upgrades_an_acknowledged_echo() {
        let mut w = Window::status();
        let txn = TransactionId::new();
        let mut echo = Line::new(LineKind::Message, "[file] notes.txt");
        (echo.own, echo.pending, echo.txn_id) = (true, true, Some(txn.clone()));
        w.push(echo, 100);
        // The send acknowledgement lands first...
        let line = w.lines.back_mut().unwrap();
        (line.pending, line.event_id) = (false, Some("$up".try_into().unwrap()));
        // ...then the synced copy, which knows about the attachment.
        let mut server = msg("$up", "[file] notes.txt");
        (server.own, server.txn_id) = (true, Some(txn));
        server.media = Some(crate::model::Media {
            source: matrix_sdk::ruma::events::room::MediaSource::Plain("mxc://x/y".into()),
            filename: "notes.txt".into(),
        });
        assert!(!w.push(server, 100));
        assert_eq!(w.lines.len(), 1);
        assert!(w.last_media(None).is_some());
    }

    #[test]
    fn history_dedupes_and_applies_edits_and_orphans() {
        let mut w = Window::status();
        w.push(msg("$c", "newest"), 100);
        // A reaction arrives live for a message that is not loaded yet.
        w.apply(Item::React { target: "$a".try_into().unwrap(), key: "👍".into(), nick: "bob".into() }, 100, true);
        let edit = Item::Edit { target: "$a".try_into().unwrap(), body: "fixed".into(), fallback: msg("$e", "fixed") };
        w.merge_history(vec![Item::Line(msg("$a", "typo")), edit, Item::Line(msg("$c", "newest"))]);

        let bodies: Vec<&str> = w.lines.iter().map(|l| l.body.as_str()).collect();
        assert_eq!(bodies, vec!["fixed", "newest"]);
        assert!(w.lines[0].edited);
        assert_eq!(w.lines[0].reactions, vec![("👍".to_owned(), vec!["bob".to_owned()])]);
    }

    #[test]
    fn first_history_page_can_be_newer_than_sync() {
        // Sync delivered the first two events; the latest-events page also
        // carries the ones after them.
        let mut w = Window::status();
        w.push(msg("$1", "created"), 100);
        w.push(msg("$2", "joined"), 100);
        let page = ["$0", "$1", "$2", "$3", "$4"].map(|id| Item::Line(msg(id, id))).to_vec();
        w.merge_history(page);
        assert_eq!(ids(&w), vec!["$0", "$1", "$2", "$3", "$4"]);
    }

    #[test]
    fn older_page_goes_on_top() {
        let mut w = Window::status();
        w.push(msg("$5", "five"), 100);
        w.merge_history(["$3", "$4"].map(|id| Item::Line(msg(id, id))).to_vec());
        assert_eq!(ids(&w), vec!["$3", "$4", "$5"]);
    }

    #[test]
    fn replies_carry_parent_context() {
        let mut w = Window::status();
        w.push(msg("$p", "the original question, which is fairly long indeed"), 100);
        let mut reply = msg("$r", "answer");
        reply.reply_to = Some("$p".try_into().unwrap());
        w.push(reply, 100);
        let r = &w.lines[1];
        assert_eq!(r.reply_nick.as_deref(), Some("alice"));
        assert_eq!(r.reply_snippet.as_deref(), Some("the original question, which is fairly l…"));
    }

    #[test]
    fn answer_target_picks_named_nick() {
        let mut w = Window::status();
        let mut bob = msg("$b", "from bob");
        bob.nick = "Bob Smith".into();
        w.push(bob, 100);
        w.push(msg("$a", "from alice"), 100);

        let (target, rest) = w.answer_target("bob smith: sure thing");
        assert_eq!(target.unwrap().body, "from bob");
        assert_eq!(rest, "sure thing");

        let (target, rest) = w.answer_target("sure thing");
        assert_eq!(target.unwrap().body, "from alice", "defaults to the last message");
        assert_eq!(rest, "sure thing");
    }

    #[test]
    fn dropped_paths_are_unescaped() {
        assert_eq!(parse_path(r"/tmp/My\ Cat.png"), PathBuf::from("/tmp/My Cat.png"));
        assert_eq!(parse_path("'/tmp/a b.txt'"), PathBuf::from("/tmp/a b.txt"));
        assert_eq!(parse_path("file:///tmp/a%20b.txt"), PathBuf::from("/tmp/a b.txt"));
        let home = dirs::home_dir().unwrap();
        assert_eq!(parse_path("~/x.png"), home.join("x.png"));
    }

    #[test]
    fn split_first_word() {
        assert_eq!(split_first("  join  #a:b  "), ("join", "#a:b  "));
        assert_eq!(split_first("quit"), ("quit", ""));
    }
}

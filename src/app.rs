//! Application state: windows, accounts, keys and slash commands.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::ruma::{
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, TransactionId, UserId,
    events::{
        relation::Thread,
        room::history_visibility::HistoryVisibility,
        room::message::{
            AddMentions, ForwardThread, ReplacementMetadata, ReplyMetadata, ReplyWithinThread, RoomMessageEventContent,
        },
    },
    room::{AllowRule, JoinRule, Restricted},
};
use ratatui::{
    DefaultTerminal,
    layout::{Position, Size},
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    chatlog::ChatLog,
    compose::{Say, compose},
    config::{BellKind, BellMode, BellStyle, Config, NotifyMode, Paths},
    convert::attachment,
    eggs,
    input::{Candidate, Draft, Input},
    links,
    matrix::{self, Account, AppEvent, Moderation, Tx, VerifyAnswer},
    model::{
        AccountName, Activity, Item, Line, LineKind, Media, Member, Permissions, Presence, PresenceKind, RoomMeta,
        SpaceInfo, ago,
    },
    notify,
    servers::{EnvFile, ServerDef, Servers},
    switcher::{self, Switcher},
    theme::{Theme, ThemeName},
    ui,
    viewer::{self, Viewer},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WinKey {
    pub account: String,
    pub room_id: OwnedRoomId,
    /// Set for a thread window: the thread's root event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<OwnedEventId>,
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
    /// Away message set with `/away`.
    pub away: Option<String>,
    pub account: Option<Account>,
}

#[derive(Clone, Debug)]
enum History {
    Unloaded,
    Loading,
    More(String),
    Done,
}

/// Which window this is, independent of its position: stays valid while
/// windows open, close and move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WinRef {
    Status,
    Mentions,
    Room(WinKey),
}

pub struct Window {
    /// `None` for the status and mentions windows.
    pub room: Option<WinKey>,
    /// The window collecting mentions from every room.
    pub is_mentions: bool,
    pub name: String,
    pub topic: Option<String>,
    pub encrypted: bool,
    pub is_dm: bool,
    pub dm_user: Option<OwnedUserId>,
    pub avatar: Option<OwnedMxcUri>,
    pub lines: VecDeque<Line>,
    pub activity: Activity,
    /// Rows scrolled up from the bottom.
    pub scroll: usize,
    /// Set by the renderer when the oldest loaded line is on screen.
    pub at_top: bool,
    pub unseen_below: bool,
    pub typing: Vec<(OwnedUserId, String)>,
    /// When the typing list last changed, to expire it if updates stop.
    typing_at: Option<Instant>,
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
    /// What your power level allows here, once the member list loaded.
    pub perms: Option<Permissions>,
    /// Edits/reactions/redactions whose target is not loaded yet.
    orphans: HashMap<OwnedEventId, Vec<Item>>,
    /// `/jump` target: keep loading history until this event shows up.
    seek: Option<OwnedEventId>,
    /// Columns it was last drawn at (0 until it's been on screen).
    pub width: u16,
    /// Pinned messages (`m.room.pinned_events`).
    pub pinned: Vec<OwnedEventId>,
    /// What you were typing here, kept while you're in another window.
    unsent: Option<Unsent>,
    /// Your own name for it (`/window name`), shown instead of `name`.
    pub local_name: Option<String>,
    /// Your nickname for the person in a direct-message window.
    pub person: Option<String>,
}

#[derive(Debug)]
struct Unsent {
    text: Option<Draft>,
    compose: Option<Compose>,
}

const ORPHANS_MAX: usize = 1000;
const TYPING_EXPIRY: Duration = Duration::from_secs(45);

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
            is_mentions: false,
            name,
            topic: None,
            encrypted: false,
            is_dm: false,
            dm_user: None,
            avatar: None,
            lines: VecDeque::new(),
            activity: Activity::None,
            scroll: 0,
            at_top: false,
            unseen_below: false,
            typing: Vec::new(),
            typing_at: None,
            members: Vec::new(),
            members_requested: false,
            unread: (0, 0),
            history: History::Unloaded,
            history_retry: None,
            last_receipt: None,
            marker: None,
            read_by: HashMap::new(),
            perms: None,
            orphans: HashMap::new(),
            seek: None,
            width: 0,
            pinned: Vec::new(),
            unsent: None,
            local_name: None,
            person: None,
        }
    }

    /// Refresh room details. Returns the activity implied by the server's
    /// unread counts having grown (messages that arrived while we were away).
    fn update(&mut self, meta: &RoomMeta) -> Activity {
        self.name = meta.name.clone();
        self.topic = meta.topic.clone();
        self.encrypted = meta.encrypted;
        self.is_dm = meta.is_dm;
        self.dm_user = meta.dm_user.clone();
        self.avatar = meta.avatar.clone();
        self.pinned = meta.pinned.clone();
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

    pub fn id(&self) -> WinRef {
        match (&self.room, self.is_mentions) {
            (Some(key), _) => WinRef::Room(key.clone()),
            (None, true) => WinRef::Mentions,
            (None, false) => WinRef::Status,
        }
    }

    /// The name to show: yours for the window, or for the person in a
    /// DM, else the room's.
    pub fn title(&self) -> &str {
        self.local_name.as_deref().or(self.person.as_deref()).unwrap_or(&self.name)
    }

    /// Both names, for finding a window by either.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.local_name.as_deref()).chain(self.person.as_deref())
    }

    pub fn has_draft(&self) -> bool {
        self.unsent.is_some()
    }

    /// The root event, if this is a thread window.
    pub fn thread_root(&self) -> Option<&EventId> {
        self.room.as_ref()?.thread.as_deref()
    }

    /// The DM partner's presence, if known.
    pub fn dm_presence<'a>(&self, app: &'a App) -> Option<&'a Presence> {
        let user = self.dm_user.as_ref()?;
        app.presence_of(self.account()?, user)
    }

    /// Unread messages according to the server.
    pub fn unread(&self) -> u64 {
        self.unread.0
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
            && let Some(echo) = self
                .lines
                .iter_mut()
                .rev()
                .find(|l| l.own && l.txn_id.as_ref() == Some(txn) && l.event_id.as_ref().is_none_or(|e| e == id))
        {
            line.reply_nick = echo.reply_nick.take();
            line.reply_sender = echo.reply_sender.take();
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
            (line.reply_nick, line.reply_snippet) = (Some(parent.nick.clone()), Some(snippet(&parent.text())));
            line.reply_sender = parent.sender.clone();
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

    /// Apply an edit/reaction/redaction only if its target is here.
    fn annotate_existing(&mut self, item: &Item) {
        if let Some(target) = item.target().map(ToOwned::to_owned)
            && let Some(line) = self.find_mut(&target)
        {
            annotate(line, item);
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

        type Parent = (String, Option<OwnedUserId>, String);
        let parents: HashMap<OwnedEventId, Parent> = self
            .lines
            .iter()
            .filter_map(|l| Some((l.event_id.clone()?, (l.nick.clone(), l.sender.clone(), snippet(&l.text())))))
            .collect();
        for line in self.lines.iter_mut().filter(|l| l.reply_nick.is_none()) {
            if let Some((nick, sender, text)) = line.reply_to.as_ref().and_then(|t| parents.get(t)) {
                (line.reply_nick, line.reply_sender, line.reply_snippet) =
                    (Some(nick.clone()), sender.clone(), Some(text.clone()));
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
        self.lines
            .iter()
            .rev()
            .find(|l| l.media.is_some() && !l.redacted && nick.is_none_or(|n| l.nick.eq_ignore_ascii_case(n)))
    }
}

/// `12: Room  Oct 13 19:27 <nick> what they said…`
fn search_row(n: usize, room: &str, line: &Line) -> String {
    let today = Local::now().date_naive();
    let when = if line.ts.date_naive() == today { line.ts.format("%H:%M") } else { line.ts.format("%b %d %H:%M") };
    let room: String = room.chars().take(18).collect();
    format!("{n:>2}: {room}  {when} <{}> {}", line.nick, snippet(&line.text()))
}

fn thread_name(root_text: &str) -> String {
    let short: String = root_text.lines().next().unwrap_or("").chars().take(20).collect();
    if short.is_empty() { "⤷ thread".into() } else { format!("⤷ {short}") }
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
            line.rich = fallback.rich.clone();
            line.kind = fallback.kind;
            line.edited = true;
        }
        Item::Redact { .. } => {
            line.body = "[message deleted]".into();
            line.redacted = true;
            line.edited = false;
            line.media = None;
            line.rich = None;
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

/// An inline image preview's state.
pub enum LinkTitle {
    Loading,
    Ready(String),
    /// The page has no title, or it couldn't be fetched.
    None,
}

pub enum Preview {
    Loading,
    Ready(Protocol),
    Failed,
}

/// Encoded previews kept in memory; older ones are fetched again (from the
/// SDK's media cache) if they scroll back into view.
const PREVIEWS_MAX: usize = 64;

/// What the next plain-text submit does, chosen in selection mode.
#[derive(Clone, Debug)]
enum Compose {
    Reply { event: OwnedEventId, sender: OwnedUserId, nick: String },
    React { event: OwnedEventId, nick: String },
}

/// An edit in progress: ↑/↓ walk through your own messages (each loads
/// into the input line), Enter saves, Esc puts your draft back.
#[derive(Clone, Debug)]
struct EditSession {
    key: WinKey,
    event: OwnedEventId,
    original: String,
    emote: bool,
    /// What was in the input line before editing started.
    draft: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Layout {
    windows: Vec<WinKey>,
    closed: Vec<WinKey>,
    /// Your own window names (`/window name`).
    #[serde(default)]
    names: Vec<(WinKey, String)>,
    /// Your own names for people (`/nickname`), seen only by you.
    #[serde(default)]
    nicknames: BTreeMap<OwnedUserId, String>,
}

/// Several windows on screen at once, like irssi's split windows. Panes
/// hold window identities (`None` is the status window), so they survive
/// windows opening, closing and moving. The active window is always one of
/// them: the focused pane.
pub struct Split {
    pub panes: Vec<WinRef>,
    pub side_by_side: bool,
}

const PANES_MAX: usize = 4;
/// Link titles kept in memory.
const LINK_TITLES_MAX: usize = 2000;
/// The shortest time between two rings.
const BELL_GAP: Duration = Duration::from_secs(2);
/// How long the visual bell flashes.
const FLASH: Duration = Duration::from_millis(250);
/// Messages the mentions window keeps.
const MENTIONS_MAX: usize = 500;
/// Rows one notch of the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

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
    /// Ring the terminal bell after this frame.
    pub bell: bool,
    /// The bell for this session (`/bell`), starting from the config.
    bell_mode: BellMode,
    bell_style: BellStyle,
    last_bell: Option<Instant>,
    /// The visual bell: the status bar flashes until then.
    pub flash_until: Option<Instant>,
    /// The terminal title last set.
    last_title: Option<String>,
    pub redraw_all: bool,
    pub show_sidebar: bool,
    pub show_nicklist: bool,
    pub view_width: u16,
    pub view_height: u16,
    layout: Layout,
    typing_since: Option<(WinKey, Instant)>,
    /// Mentions captured from the input line being submitted.
    mentions: Vec<(String, OwnedUserId)>,
    /// Whether the terminal has focus (when it reports focus changes).
    focused: bool,
    notify_mode: NotifyMode,
    /// (title, body) waiting to be written after the next draw.
    pub notifications: Vec<(String, String)>,
    /// The message picked in selection mode (Alt+Up) in the active window.
    pub select: Option<OwnedEventId>,
    /// Waiting for y/n before deleting this message.
    pub confirm_delete: Option<OwnedEventId>,
    /// A long paste waiting for y/n before it's sent: its line count.
    pub confirm_paste: Option<usize>,
    /// The edit to go back to if a delete started from it is declined.
    resume_edit: Option<EditSession>,
    editing: Option<EditSession>,
    compose: Option<(WinKey, Compose)>,
    /// Text for the clipboard, written after the next draw.
    pub clipboard: Option<String>,
    /// The F1 help screen, while open.
    pub help: Option<HelpView>,
    /// The full-screen image viewer.
    pub viewer: Option<Viewer>,
    pub split: Option<Split>,
    /// People whose away status you've been told about (irssi's "is
    /// away"), and what it was.
    away_noted: HashMap<OwnedUserId, String>,
    /// Where each message in the mentions window came from.
    mention_origins: HashMap<OwnedEventId, WinKey>,
    /// How many aliases deep the current command runs.
    alias_depth: usize,
    /// How much of the Konami code has been typed.
    konami: usize,
    pub switcher: Option<Switcher>,
    /// Windows you were in, most recent first (for the switcher).
    recent: Vec<WinRef>,
    /// Rooms with their own notification mode, per server.
    pub push_modes: HashMap<AccountName, HashMap<OwnedRoomId, RoomNotificationMode>>,
    /// Where the last frame drew things, for the mouse.
    pub hits: ui::Hits,
    /// Terminal graphics support, probed at startup; `None` disables previews.
    pub picker: Option<Picker>,
    pub previews: HashMap<OwnedEventId, Preview>,
    preview_order: VecDeque<OwnedEventId>,
    /// Images the renderer wants, fetched after the draw.
    pub preview_requests: Vec<(String, OwnedEventId, Media)>,
    /// Page titles under links, by URL.
    pub link_titles: HashMap<String, LinkTitle>,
    /// Links to ask for, by server: (account, URL).
    pub link_title_requests: Vec<(String, String)>,
    /// Servers that don't do link previews.
    link_titles_off: HashSet<String>,
    /// Other people's presence, per server.
    pub presence: HashMap<AccountName, HashMap<OwnedUserId, Presence>>,
    /// Joined spaces per server, for grouping the window list.
    pub spaces: HashMap<AccountName, Vec<SpaceInfo>>,
    /// Chat logging, when enabled.
    log: Option<ChatLog>,
    /// Window-list avatars by (server, room), with the URL they came from.
    pub avatars: HashMap<(String, OwnedRoomId), (OwnedMxcUri, Preview)>,
    /// Results of the last `/threads`, for `/thread N`.
    thread_hits: Vec<(WinKey, OwnedEventId)>,
    /// Results of the last `/search`, for `/jump N`.
    search_hits: Vec<(WinKey, OwnedEventId)>,
    /// Where `/search` printed its results, and how many deep searches are
    /// still running.
    search_out: Option<WinKey>,
    search_pending: usize,
}

struct Cmd {
    name: &'static str,
    args: &'static str,
    help: &'static str,
}

const COMMANDS: &[Cmd] = &[
    Cmd { name: "help", args: "[command]", help: "Show commands, or help for one" },
    Cmd { name: "join", args: "<#alias:server|!room>", help: "Join a room or accept an invite (alias /j)" },
    Cmd {
        name: "create",
        args: "[-public] [-space] <name>",
        help: "Create a room (encrypted and invite-only unless -public), or a space",
    },
    Cmd { name: "spaces", args: "", help: "List your spaces and their rooms" },
    Cmd {
        name: "space",
        args: "<name> [add|remove|leave]",
        help: "Browse a space, add/remove the current room, or leave the space",
    },
    Cmd { name: "name", args: "<new name>", help: "Rename this room" },
    Cmd { name: "topic", args: "[text]", help: "Show or set the room topic" },
    Cmd { name: "info", args: "", help: "Show this room's ID, aliases, encryption and settings" },
    Cmd {
        name: "joinrule",
        args: "<invite|public|knock|restricted <space>>",
        help: "Who can join: by invite, anyone, by request, or members of a space",
    },
    Cmd { name: "guests", args: "<on|off>", help: "Allow or forbid guest accounts" },
    Cmd {
        name: "history",
        args: "<joined|invited|shared|world>",
        help: "Who can read past messages: members since joining/invited, all members, or anyone",
    },
    Cmd { name: "publish", args: "<on|off>", help: "List the room in the server's room directory" },
    Cmd { name: "upgrade", args: "[version] [confirm]", help: "Show room versions, or move the room to a new version" },
    Cmd { name: "rooms", args: "", help: "List every joined room, including closed windows" },
    Cmd { name: "invites", args: "", help: "List pending invitations" },
    Cmd { name: "decline", args: "<!room:server>", help: "Reject an invitation" },
    Cmd { name: "part", args: "", help: "Leave the current room (alias /leave)" },
    Cmd { name: "threads", args: "", help: "List this room's threads" },
    Cmd { name: "mentions", args: "", help: "Open the window collecting messages that mention you" },
    Cmd { name: "pins", args: "", help: "List this room's pinned messages (p in selection mode pins)" },
    Cmd {
        name: "thread",
        args: "[N]",
        help: "Open thread N from /threads, or the latest thread here (t in selection mode)",
    },
    Cmd { name: "forget", args: "", help: "Leave this room and remove it from your room list" },
    Cmd { name: "query", args: "<@user:server>", help: "Open a direct-message window" },
    Cmd { name: "msg", args: "<@user|#alias|!room> <text>", help: "Send without switching windows" },
    Cmd { name: "me", args: "<action>", help: "Send an emote" },
    Cmd { name: "notice", args: "<text>", help: "Send a notice" },
    Cmd { name: "say", args: "<text>", help: "Send text as-is, even if it starts with / (or type //)" },
    Cmd { name: "reply", args: "[nick] <text>", help: "Reply to the last message (from nick, if given)" },
    Cmd { name: "react", args: "[nick] <emoji>", help: "React to the last message (from nick, if given)" },
    Cmd {
        name: "edit",
        args: "[text]",
        help: "Edit a message: no text opens the editor (also Shift+Up, or Ctrl+E on an empty line)",
    },
    Cmd { name: "redact", args: "", help: "Delete your last message (alias /del)" },
    Cmd {
        name: "forward",
        args: "[room]",
        help: "Send a copy of the latest message to another room (f in selection mode picks one)",
    },
    Cmd { name: "upload", args: "<path>", help: "Send a file or image (Tab completes paths)" },
    Cmd { name: "download", args: "[nick]", help: "Save the last attachment (from nick) to Downloads" },
    Cmd { name: "lastlog", args: "[-n N] [-nick nick] <text>", help: "Search this window's loaded scrollback" },
    Cmd {
        name: "search",
        args: "[-deep] <text>",
        help: "Search all rooms' loaded history (-deep: last 500 messages per room)",
    },
    Cmd { name: "jump", args: "<N>", help: "Go to /search result N and select it" },
    Cmd { name: "urls", args: "", help: "List links in this window, newest first" },
    Cmd { name: "open", args: "[N]", help: "Open link N from /urls in your browser (default: newest)" },
    Cmd { name: "list", args: "[search]", help: "Browse the server's public room directory" },
    Cmd { name: "away", args: "[-all] [message]", help: "Set away with a message, or come back (no message)" },
    Cmd { name: "ignore", args: "[nick|@user]", help: "Hide a user everywhere, or list ignored users" },
    Cmd {
        name: "notify",
        args: "[all|mentions|mute|default]",
        help: "What alerts you in this room (shared with your other clients)",
    },
    Cmd { name: "hilight", args: "[word]", help: "Highlight a word in every room, or list highlight words" },
    Cmd { name: "dehilight", args: "<word>", help: "Stop highlighting a word" },
    Cmd { name: "unignore", args: "<@user>", help: "Stop ignoring a user" },
    Cmd { name: "names", args: "", help: "List room members" },
    Cmd { name: "whois", args: "<nick|@user>", help: "Show a user's profile and device trust" },
    Cmd { name: "nick", args: "<name>", help: "Set your display name on this server" },
    Cmd {
        name: "nickname",
        args: "[nick|@user [name]]",
        help: "Your own name for someone, shown only to you (no name takes it off)",
    },
    Cmd { name: "invite", args: "<nick|@user>", help: "Invite a user to this room" },
    Cmd { name: "kick", args: "<nick|@user> [reason]", help: "Remove a user from this room" },
    Cmd { name: "ban", args: "<nick|@user> [reason]", help: "Ban a user from this room" },
    Cmd { name: "unban", args: "<@user>", help: "Lift a ban" },
    Cmd {
        name: "window",
        args: "<n>|list|close|move <n>|goto <name>|name [your name]",
        help: "Manage windows (alias /win; /wc closes, /wl lists)",
    },
    Cmd { name: "split", args: "<window>", help: "Show another window in a pane below (F6 moves between panes)" },
    Cmd { name: "vsplit", args: "<window>", help: "Show another window in a pane beside this one" },
    Cmd { name: "unsplit", args: "[all]", help: "Close this pane (the window stays open), or all but this one" },
    Cmd { name: "markread", args: "[all]", help: "Mark this room read (all: every room), for your other clients too" },
    Cmd {
        name: "alias",
        args: "[name [command]]",
        help: "Your own command: /alias tt /window goto Tetrix Tests ($0 $1 $1- $* take arguments, ; chains)",
    },
    Cmd { name: "unalias", args: "<name>", help: "Remove an alias" },
    Cmd { name: "clear", args: "", help: "Clear this window's scrollback" },
    Cmd { name: "sidebar", args: "", help: "Toggle the window list (F2)" },
    Cmd {
        name: "bell",
        args: "[on|off|away|sound|visual|both|test]",
        help: "The bell for this session: when it rings, sound or a flash, or try it",
    },
    Cmd {
        name: "theme",
        args: "[default|light|mono]",
        help: "Switch colours for this session (theme = … in config.toml makes it stick)",
    },
    Cmd { name: "nicklist", args: "", help: "Toggle the member list (F3)" },
    Cmd { name: "servers", args: "", help: "List dotfile servers and their status" },
    Cmd { name: "log", args: "[on|off]", help: "Chat logging for this session (log = true in config makes it stick)" },
    Cmd {
        name: "reload",
        args: "",
        help: "Re-read config.toml and the server dotfile (connects new servers, drops removed ones)",
    },
    Cmd { name: "connect", args: "<server>", help: "Connect a server from the dotfile" },
    Cmd { name: "disconnect", args: "<server>", help: "Stop syncing a server (session is kept)" },
    Cmd { name: "logout", args: "<server>", help: "Log out and delete the local session and keys" },
    Cmd {
        name: "verify",
        args: "[accept|confirm|mismatch|cancel|@user]",
        help: "Emoji-verify this device (no args) or a user",
    },
    Cmd { name: "recover", args: "[server]", help: "Unlock history with the dotfile's RECOVERY_KEY" },
    Cmd {
        name: "keys",
        args: "export [file] | import <file>",
        help: "Save or load room keys (Element's format, passphrase from the dotfile)",
    },
    Cmd {
        name: "devices",
        args: "[rename <id> <name> | delete <id>]",
        help: "List your sessions, rename one, or sign one out (asks to confirm)",
    },
    Cmd { name: "quit", args: "", help: "Exit Textrix (alias /exit)" },
];

/// Commands that aren't in /help on purpose.
const EGGS: &[&str] = &["slap", "tableflip", "unflip", "shrug", "8ball", "bofh"];

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

pub const KEYS: &[(&str, &str)] = &[
    ("Alt+1…0, Alt+Q…P", "go to window 1–20"),
    ("Ctrl+N / Ctrl+P, Alt+← / Alt+→", "next / previous window"),
    ("Alt+A", "jump to the most important window with activity"),
    ("Ctrl+K, Alt+K", "quick switcher: type part of a room's name (or its number), Enter goes there"),
    ("PgUp / PgDn", "scroll; older history loads at the top"),
    ("Tab", "complete commands, nicks (adds a mention), :emoji shortcodes, server names, file paths"),
    ("Up / Down", "input history"),
    ("Shift+Enter, Alt+Enter", "new line in the message"),
    ("Alt+Up", "select a message (keys below)"),
    ("Shift+Up, Ctrl+E on an empty line", "edit your messages (keys below)"),
    ("F1 / F2 / F3", "this help / window list / member list"),
    ("F6 / Shift+F6", "focus the next / previous split pane"),
    ("Mouse", "wheel scrolls; click a window, pane, message (selects), link (opens), image (views), name or the input"),
    ("Ctrl+A / Ctrl+E / Ctrl+B / Ctrl+F", "start / end / left / right in the input line"),
    ("Ctrl+U / Ctrl+K / Ctrl+W / Ctrl+Y", "cut to start / cut to end / cut word / paste cut"),
    ("Ctrl+C", "clear the input line"),
    ("Ctrl+L", "redraw the screen"),
];

pub const SELECT_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓, PgUp / PgDn, g / G", "move between messages"),
    ("r", "reply"),
    ("e", "edit (your own messages)"),
    ("d", "delete (asks first)"),
    ("+", "react"),
    ("t", "open the thread (or start one)"),
    ("p", "pin or unpin it for everyone in the room (if allowed)"),
    ("f", "forward a copy to another room (pick it like the switcher)"),
    ("z", "view the image full screen: + / - zoom, arrows move, 0 fits, Esc closes"),
    ("s", "save the attachment"),
    ("o", "open the link"),
    ("v", "show spoilers"),
    ("c", "copy the text"),
    ("i", "sender, time and event ID"),
    ("Esc", "leave selection mode"),
];

pub const EDIT_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓", "pick one of your messages; it loads into the input line"),
    ("Enter", "save the edit"),
    ("clear the text, Enter", "delete the message (asks first)"),
    ("Esc", "cancel and get back what you were typing"),
];

/// The full-screen help (F1): scroll position and a filter you type.
#[derive(Default)]
pub struct HelpView {
    pub scroll: usize,
    pub filter: String,
}

pub enum HelpRow {
    Section(&'static str),
    Entry(String, String),
}

/// Help contents, keeping only entries (and their sections) that match.
pub fn help_rows(filter: &str) -> Vec<HelpRow> {
    let filter = filter.to_lowercase();
    let mut sections: Vec<(&'static str, Vec<(String, String)>)> = vec![
        ("Keys", KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        ("Selecting messages (Alt+Up)", SELECT_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        ("Editing (Shift+Up)", EDIT_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
    ];
    let commands = COMMANDS
        .iter()
        .map(|c| {
            let aliases: Vec<String> =
                ALIASES.iter().filter(|(_, real)| *real == c.name).map(|(a, _)| format!("/{a}")).collect();
            let also = if aliases.is_empty() { String::new() } else { format!(" (also {})", aliases.join(", ")) };
            (format!("/{} {}", c.name, c.args).trim_end().to_owned(), format!("{}{also}", c.help))
        })
        .collect();
    sections.push(("Commands", commands));

    let mut rows = Vec::new();
    for (title, entries) in sections {
        let matching: Vec<(String, String)> = entries
            .into_iter()
            .filter(|(k, d)| {
                filter.is_empty() || k.to_lowercase().contains(&filter) || d.to_lowercase().contains(&filter)
            })
            .collect();
        if !matching.is_empty() {
            rows.push(HelpRow::Section(title));
            rows.extend(matching.into_iter().map(|(k, d)| HelpRow::Entry(k, d)));
        }
    }
    rows
}

/// Server-name arguments, for tab completion.
const SERVER_COMMANDS: &[&str] = &["connect", "disconnect", "logout", "recover"];

impl App {
    pub fn new(cfg: Config, paths: Paths, env: EnvFile, servers: Servers, tx: Tx) -> Self {
        let (bell_mode, bell_style) = (cfg.bell, cfg.bell_style);
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
                    away: None,
                    account: None,
                };
                (AccountName::from(def.name.as_str()), state)
            })
            .collect();
        let (show_sidebar, show_nicklist) = (cfg.sidebar, cfg.nicklist);
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
            bell_mode,
            bell_style,
            last_bell: None,
            flash_until: None,
            last_title: None,
            redraw_all: false,
            show_sidebar,
            show_nicklist,
            view_width: 80,
            view_height: 24,
            layout,
            typing_since: None,
            mentions: Vec::new(),
            focused: true,
            notify_mode: NotifyMode::Off,
            notifications: Vec::new(),
            select: None,
            confirm_delete: None,
            confirm_paste: None,
            resume_edit: None,
            editing: None,
            compose: None,
            clipboard: None,
            help: None,
            viewer: None,
            split: None,
            away_noted: HashMap::new(),
            mention_origins: HashMap::new(),
            alias_depth: 0,
            konami: 0,
            switcher: None,
            recent: Vec::new(),
            push_modes: HashMap::new(),
            hits: ui::Hits::default(),
            picker: None,
            previews: HashMap::new(),
            preview_order: VecDeque::new(),
            preview_requests: Vec::new(),
            link_titles: HashMap::new(),
            link_title_requests: Vec::new(),
            link_titles_off: HashSet::new(),
            presence: HashMap::new(),
            spaces: HashMap::new(),
            log: None,
            avatars: HashMap::new(),
            thread_hits: Vec::new(),
            search_hits: Vec::new(),
            search_out: None,
            search_pending: 0,
        };
        app.notify_mode = notify::resolve(app.cfg.notifications);
        app.apply_logging();
        app.status(Line::info(format!(
            "Textrix {} — irssi-style Matrix client. {}",
            env!("CARGO_PKG_VERSION"),
            eggs::pick(&eggs::TAGLINES)
        )));
        app.status(Line::info(format!("servers from {}", app.env.path().display())));
        for warning in servers.warnings {
            app.status(Line::error(warning));
        }
        app.status(Line::info(
            "F1 or /help for commands; Alt+1..0 or Ctrl+N/P switch windows; Alt+Up selects messages",
        ));
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
        let opts = matrix::ConnectOptions {
            sync_mode: self.cfg.sync,
            history_cache: self.cfg.history_cache,
            encrypt_store: self.cfg.encrypt_store,
        };
        let homeserver = def.homeserver_or_server_name().unwrap_or("?").to_owned();
        self.status(Line::info(format!("{name}: connecting to {homeserver} as {}", def.user)));
        let name = AccountName::from(name);
        tokio::spawn(async move {
            let event = match matrix::connect(def, paths, env, opts, tx.clone()).await {
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

    pub fn presence_of(&self, account: &str, user: &UserId) -> Option<&Presence> {
        self.presence.get(account)?.get(user)
    }

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

    /// The room's own window (not its thread windows).
    fn window_idx(&self, account: &str, room_id: &OwnedRoomId) -> Option<usize> {
        self.windows.iter().position(|w| {
            w.room.as_ref().is_some_and(|k| k.account == account && k.room_id == *room_id && k.thread.is_none())
        })
    }

    fn thread_idx(&self, account: &str, room_id: &OwnedRoomId, root: &EventId) -> Option<usize> {
        self.windows.iter().position(|w| {
            w.room
                .as_ref()
                .is_some_and(|k| k.account == account && k.room_id == *room_id && k.thread.as_deref() == Some(root))
        })
    }

    /// The room's window and any of its thread windows.
    fn room_windows(&self, account: &str, room_id: &OwnedRoomId) -> Vec<usize> {
        (0..self.windows.len())
            .filter(|&i| self.windows[i].room.as_ref().is_some_and(|k| k.account == account && k.room_id == *room_id))
            .collect()
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
            .or_else(|| win.lines.iter().rev().find(|l| l.nick.to_lowercase() == wanted).and_then(|l| l.sender.clone()))
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
            AppEvent::LinkTitle { url, title } => {
                if self.link_titles.len() > LINK_TITLES_MAX {
                    self.link_titles.clear();
                }
                self.link_titles.insert(url, title.map_or(LinkTitle::None, LinkTitle::Ready));
            }
            AppEvent::LinkTitlesOff { account } => {
                if self.link_titles_off.insert(account.to_string()) {
                    self.status(Line::info(format!(
                        "{account}: the server doesn't offer link previews, so links won't show page titles \
                         (Synapse: url_preview_enabled)"
                    )));
                }
            }
            AppEvent::FullImage { event_id, image } => {
                if let Some(v) = self.viewer.as_mut().filter(|v| v.event == event_id) {
                    v.image = match image {
                        Ok(image) => viewer::Loading::Ready(image),
                        Err(e) => viewer::Loading::Failed(e),
                    };
                }
            }
            AppEvent::ViewerFrame { event_id, view, protocol } => {
                if let Some(v) = self.viewer.as_mut().filter(|v| v.event == event_id && v.pending == Some(view)) {
                    v.pending = None;
                    match protocol {
                        Ok(protocol) => v.frame = Some((view, protocol)),
                        Err(e) => v.image = viewer::Loading::Failed(e),
                    }
                }
            }
            AppEvent::PushRules { account, modes } => {
                self.push_modes.insert(account, modes);
            }
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
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                self.layout.closed.retain(|k| *k != key);
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    let name = self.windows[idx].title().to_owned();
                    self.remove_window(idx);
                    self.status(Line::info(format!("{account}: you left {name}")));
                }
            }
            AppEvent::Timeline { account, room_id, items, live, limited } => {
                // Messages were skipped since the cache was saved: drop the
                // cached ones rather than show them joined to these, and
                // load history from the server as usual.
                if limited && let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].lines.retain(|l| !l.cached);
                }
                self.on_timeline(&account, &room_id, items, live)
            }
            AppEvent::Cached { account, room_id, items } => {
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].merge_history(items.clone());
                    self.record_old_mentions(&account, &room_id, &items);
                }
            }
            AppEvent::History { account, room_id, items, end, failed } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if failed {
                    w.history = end.map_or(History::Unloaded, History::More);
                    w.history_retry = Some(Instant::now() + Duration::from_secs(15));
                } else {
                    w.history = end.map_or(History::Done, History::More);
                    w.history_retry = None;
                    w.merge_history(items.clone());
                    self.record_old_mentions(&account, &room_id, &items);
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
            AppEvent::SearchResults { account, room_id, hits } => {
                self.search_pending = self.search_pending.saturating_sub(1);
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                let room = self
                    .window_idx(&account, &room_id)
                    .map(|i| self.windows[i].title().to_owned())
                    .or_else(|| self.connected(&account).and_then(|a| a.quick_meta(&room_id)).map(|m| m.name))
                    .unwrap_or_else(|| room_id.to_string());
                let rows: Vec<Line> = hits
                    .into_iter()
                    .filter_map(|line| {
                        let event = line.event_id.clone()?;
                        let n = self.search_hits.len() + 1;
                        self.search_hits.push((key.clone(), event));
                        Some(Line::info(search_row(n, &room, &line)))
                    })
                    .collect();
                let done = (self.search_pending == 0).then(|| {
                    Line::info(format!("search done: {} result(s); /jump N to go there", self.search_hits.len()))
                });
                let out = self.search_out.clone().and_then(|k| self.window_idx(&k.account, &k.room_id)).unwrap_or(0);
                for line in rows.into_iter().chain(done) {
                    self.windows[out].push(line, cap);
                }
            }
            AppEvent::Thread { account, room_id, root, items } => {
                let Some(idx) = self.thread_idx(&account, &room_id, &root) else { return };
                let w = &mut self.windows[idx];
                w.merge_history(items);
                // Root first, then replies in order (pages can arrive out of order).
                w.lines.make_contiguous().sort_by_key(|l| (l.event_id.as_ref() != Some(&root), l.ts));
                w.history = History::Done;
                if let Some(first) = w.lines.iter().find(|l| l.event_id.as_ref() == Some(&root)) {
                    let text = first.text();
                    w.name = thread_name(&text);
                    w.topic = Some(format!("Thread: <{}> {}", first.nick, text.lines().next().unwrap_or("")));
                }
            }
            AppEvent::Pins { account, room_id, lines } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                // Numbered like search results, so /jump N goes there.
                self.search_hits.clear();
                let mut rows = vec![Line::info(match lines.len() {
                    0 => "no pinned messages here; p on a message in selection mode pins it".to_owned(),
                    n => format!("{n} pinned message(s), newest pin first; /jump N goes there"),
                })];
                for line in &lines {
                    let Some(event) = line.event_id.clone() else { continue };
                    self.search_hits.push((key.clone(), event));
                    let when = line.ts.format("%Y-%m-%d %H:%M");
                    let n = self.search_hits.len();
                    let row = if line.nick.is_empty() {
                        format!("{n:>2}: {}", line.text())
                    } else {
                        format!("{n:>2}: {when} <{}> {}", line.nick, snippet(&line.text()))
                    };
                    rows.push(Line::info(row));
                }
                let out = self.window_idx(&account, &room_id).unwrap_or(0);
                for row in rows {
                    self.windows[out].push(row, cap);
                }
            }
            AppEvent::Threads { account, room_id, roots } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                self.thread_hits.clear();
                let mut rows = Vec::new();
                for (n, root) in roots.iter().enumerate() {
                    let Some(event) = root.event_id.clone() else { continue };
                    self.thread_hits.push((key.clone(), event));
                    let replies = match root.thread_count {
                        Some(1) => " [1 reply]".to_owned(),
                        Some(c) => format!(" [{c} replies]"),
                        None => String::new(),
                    };
                    rows.push(Line::info(format!("{:>2}: <{}> {}{replies}", n + 1, root.nick, snippet(&root.text()))));
                }
                let out = self.window_idx(&account, &room_id).unwrap_or(0);
                if rows.is_empty() {
                    rows.push(Line::info("no threads in this room"));
                } else {
                    rows.push(Line::info("/thread N opens one"));
                }
                for row in rows {
                    self.windows[out].push(row, cap);
                }
            }
            AppEvent::Spaces { account, mut spaces } => {
                spaces.sort_by_cached_key(|s| s.name.to_lowercase());
                self.spaces.insert(account, spaces);
            }
            AppEvent::Presence { account, updates } => {
                // Back online: the next time they're away is news again.
                for (user, presence) in &updates {
                    if presence.kind == PresenceKind::Online {
                        self.away_noted.remove(user);
                    }
                }
                self.presence.entry(account).or_default().extend(updates);
            }
            AppEvent::Avatar { account, room_id, url, protocol } => {
                let preview = protocol.map_or(Preview::Failed, Preview::Ready);
                self.avatars.insert((account.to_string(), room_id), (url, preview));
            }
            AppEvent::ImagePreview { event_id, protocol } => {
                let preview = protocol.map_or(Preview::Failed, Preview::Ready);
                self.previews.insert(event_id.clone(), preview);
                self.preview_order.push_back(event_id);
                while self.preview_order.len() > PREVIEWS_MAX {
                    if let Some(old) = self.preview_order.pop_front() {
                        self.previews.remove(&old);
                    }
                }
            }
            AppEvent::Typing { account, room_id, nicks } => {
                for idx in self.room_windows(&account, &room_id) {
                    (self.windows[idx].typing, self.windows[idx].typing_at) = (nicks.clone(), Some(Instant::now()));
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
            AppEvent::Permissions { account, room_id, perms } => {
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].perms = Some(perms);
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
            AppEvent::Bell(kind) => self.ring(kind),
            AppEvent::SendFailed { account, room_id, txn_id, error } => {
                self.ring(BellKind::FailedSends);
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
                let mut events: Vec<OwnedEventId> = self
                    .room_windows(&account, &room_id)
                    .into_iter()
                    .flat_map(|i| self.windows[i].utd_events(Some(&sessions)))
                    .collect();
                events.dedup();
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
                for idx in self.room_windows(&account, &room_id) {
                    let w = &mut self.windows[idx];
                    match &item {
                        Item::Line(line) if line.kind != LineKind::Utd => {
                            let Some(id) = line.event_id.clone() else { continue };
                            if let Some(existing) = w.find_mut(&id) {
                                let reactions = std::mem::take(&mut existing.reactions);
                                *existing = line.clone();
                                existing.reactions = reactions;
                            }
                        }
                        Item::Line(_) => {}
                        other => w.annotate_existing(other),
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
            AppEvent::Away { account, message } => {
                let text = match &message {
                    Some(msg) => format!("{account}: you are now away: {msg}"),
                    None => format!("{account}: you are back"),
                };
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.away = message;
                }
                self.status(Line::info(text));
            }
            AppEvent::Ignored { account, user_id } => {
                for w in self.windows.iter_mut().filter(|w| w.account() == Some(&*account)) {
                    w.lines.retain(|l| l.sender.as_ref() != Some(&user_id));
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
                let wakes = live && items.iter().any(|i| matches!(i, Item::Line(l) if l.is_message() && !l.own));
                match wakes.then(|| self.reopen(account, room_id)).flatten() {
                    Some(idx) => idx,
                    None => return,
                }
            }
        };
        let cap = self.cfg.scrollback;
        let width = match self.windows[idx].width {
            0 => self.view_width,
            w => w,
        };
        let viewing = self.on_screen(idx);
        let mode = self.room_mode(idx);
        let muted = mode == Some(RoomNotificationMode::Mute);
        // Mentions-only and muted rooms keep ordinary messages quiet.
        let quiet = muted || mode == Some(RoomNotificationMode::MentionsAndKeywordsOnly);
        let mut ring = false;
        let mut mentions = Vec::new();

        if live && let Some(log) = self.log.as_mut() {
            let name = self.windows[idx].name.clone();
            for item in &items {
                if let Err(e) = log.write(account, &name, room_id, item) {
                    self.log = None;
                    self.status(Line::error(format!("chat logging stopped: {e}")));
                    break;
                }
            }
        }

        // Open thread windows of this room get their thread's messages too.
        let threads: Vec<(usize, OwnedEventId)> = self
            .room_windows(account, room_id)
            .into_iter()
            .filter_map(|i| Some((i, self.windows[i].thread_root()?.to_owned())))
            .collect();
        for (tidx, root) in &threads {
            for item in &items {
                match item {
                    Item::Line(line)
                        if line.thread_root.as_ref() == Some(root) || line.event_id.as_ref() == Some(root) =>
                    {
                        let added = self.windows[*tidx].push(line.clone(), cap);
                        if added && live && !line.own && !muted && !self.on_screen(*tidx) {
                            let level = if line.highlight { Activity::Highlight } else { Activity::Message };
                            self.windows[*tidx].activity = self.windows[*tidx].activity.max(level);
                        }
                    }
                    Item::Line(_) => {}
                    other => self.windows[*tidx].annotate_existing(other),
                }
            }
        }

        for item in items {
            let Some(line) = self.windows[idx].apply(item, cap, live) else { continue };
            if live
                && let Some(root) = &line.thread_root
                && let Some(root_line) = self.windows[idx].find_mut(root)
            {
                root_line.thread_count = Some(root_line.thread_count.unwrap_or(0) + 1);
            }
            if is_mention(&line) {
                mentions.push(line.clone());
            }
            let w = &mut self.windows[idx];
            if w.scroll > 0 {
                w.scroll += ui::line_height(&line, width, &self.cfg);
                w.unseen_below = true;
            }
            // Someone who just sent a message has stopped typing.
            w.typing.retain(|(user, _)| line.sender.as_ref() != Some(user));
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
            let mut level = if line.highlight || (w.is_dm && line.is_message() && !quiet) {
                Activity::Highlight
            } else if line.is_message() && !quiet {
                Activity::Message
            } else if line.kind == LineKind::Membership && !self.cfg.show_joins {
                Activity::None
            } else {
                Activity::Event
            };
            if muted {
                level = level.min(Activity::Event);
            }
            if !viewing || w.scroll > 0 {
                w.activity = w.activity.max(level);
            }
            // Highlights and DMs alert you, and so does every message in a
            // room set to /notify all.
            let alert =
                level == Activity::Highlight || (mode == Some(RoomNotificationMode::AllMessages) && line.is_message());
            if !alert || (viewing && self.focused) {
                continue;
            }
            ring = true;
            if self.notify_mode != NotifyMode::Off {
                let nick = line.sender.as_ref().and_then(|u| self.layout.nicknames.get(u)).unwrap_or(&line.nick);
                let title = if w.is_dm { nick.clone() } else { format!("{nick} in {}", w.title()) };
                let body = if self.cfg.notify_preview { line.body.clone() } else { "new message".to_owned() };
                self.notifications.push((title, body));
            }
        }
        // Like irssi's hilight window, but every mention, newest last.
        for line in mentions {
            self.record_mention(idx, &line, live && !viewing);
        }
        if ring {
            self.ring(BellKind::Mentions);
        }
    }

    /// Ring the bell for `kind`, as `bell`, `bell_style` and `bell_for` say.
    /// Rings come at least [`BELL_GAP`] apart, so a burst rings once.
    fn ring(&mut self, kind: BellKind) {
        let now = Instant::now();
        let allowed = match self.bell_mode {
            BellMode::On => true,
            BellMode::Off => false,
            BellMode::Away => !self.focused,
        };
        if !allowed || !self.cfg.bell_for.contains(&kind) || self.last_bell.is_some_and(|t| now - t < BELL_GAP) {
            return;
        }
        self.last_bell = Some(now);
        if matches!(self.bell_style, BellStyle::Sound | BellStyle::Both) {
            self.bell = true;
        }
        if matches!(self.bell_style, BellStyle::Visual | BellStyle::Both) {
            self.flash_until = Some(now + FLASH);
        }
    }

    /// The terminal title for where you are, from the `title` template.
    pub fn terminal_title(&self) -> Option<String> {
        if self.cfg.title.trim().is_empty() {
            return None;
        }
        let w = self.win();
        let busy = (0..self.windows.len())
            .filter(|&i| !self.on_screen(i) && self.windows[i].activity >= Activity::Message)
            .count();
        let activity = if busy > 0 { format!(" [{busy}]") } else { String::new() };
        Some(
            self.cfg
                .title
                .replace("{window}", w.title())
                .replace("{number}", &(self.active + 1).to_string())
                .replace("{server}", w.account().unwrap_or_default())
                .replace("{activity}", &activity),
        )
    }

    /// ↑↑↓↓←→←→BA: the phosphor theme, or back again.
    fn konami_unlocked(&mut self) {
        if self.input.text() == "b" {
            self.input.clear();
        }
        let back = self.cfg.theme == ThemeName::Phosphor;
        let name = if back { ThemeName::Default } else { ThemeName::Phosphor };
        if let Ok(palette) = Theme::build(name, &self.cfg.colors) {
            (self.cfg.palette, self.cfg.theme, self.redraw_all) = (palette, name, true);
        }
        self.info(if back {
            "Phosphor off. The code still works."
        } else {
            "+30 lives. Phosphor on: the screen glows green until you type it again."
        });
    }

    /// The hidden ones: mIRC's trout, emoticon classics, the 8-ball, and an
    /// excuse for today. `None` if `name` isn't one.
    fn egg_command(&mut self, name: &str, args: &str) -> Option<Result<(), String>> {
        let args = args.trim();
        let with = |art: &str| if args.is_empty() { art.to_owned() } else { format!("{args} {art}") };
        Some(match name {
            "slap" if args.is_empty() => Err("slap whom?".into()),
            "slap" => {
                self.say_as_is(&eggs::slap(args), Say::Emote);
                Ok(())
            }
            "tableflip" => {
                self.say_as_is(&with("(╯°□°)╯︵ ┻━┻"), Say::Text);
                Ok(())
            }
            "unflip" => {
                self.say_as_is(&with("┬─┬ノ( º _ ºノ)"), Say::Text);
                Ok(())
            }
            "shrug" => {
                self.say_as_is(&with("¯\\_(ツ)_/¯"), Say::Text);
                Ok(())
            }
            "8ball" => {
                let asked = if args.is_empty() { String::new() } else { format!("“{args}” ") };
                self.info(format!("{asked}The 8-ball says: {}", eggs::pick(&eggs::EIGHT_BALL)));
                Ok(())
            }
            "bofh" => {
                self.info(format!("Today's excuse: {}.", eggs::pick(&eggs::EXCUSES)));
                Ok(())
            }
            _ => return None,
        })
    }

    /// Send text exactly as typed, without Markdown (for ¯\_(ツ)_/¯).
    fn say_as_is(&mut self, text: &str, kind: Say) {
        let markdown = std::mem::replace(&mut self.cfg.markdown, false);
        self.say(text, kind);
        self.cfg.markdown = markdown;
    }

    /// `/bell [on|off|away|sound|visual|both|test]` for this session.
    fn cmd_bell(&mut self, args: &str) -> Result<(), String> {
        match args.trim() {
            "" => {}
            "on" => self.bell_mode = BellMode::On,
            "off" => self.bell_mode = BellMode::Off,
            "away" => self.bell_mode = BellMode::Away,
            "sound" => self.bell_style = BellStyle::Sound,
            "visual" => self.bell_style = BellStyle::Visual,
            "both" => self.bell_style = BellStyle::Both,
            "test" => {
                self.last_bell = None;
                let mode = std::mem::replace(&mut self.bell_mode, BellMode::On);
                let kinds = std::mem::replace(&mut self.cfg.bell_for, vec![BellKind::Mentions]);
                self.ring(BellKind::Mentions);
                (self.bell_mode, self.cfg.bell_for) = (mode, kinds);
                return Ok(());
            }
            _ => return Err("usage: /bell [on|off|away|sound|visual|both|test]".into()),
        }
        let when = match self.bell_mode {
            BellMode::On => "on",
            BellMode::Off => "off",
            BellMode::Away => "only while the terminal is in the background",
        };
        let how = match self.bell_style {
            BellStyle::Sound => "sound",
            BellStyle::Visual => "the status bar flashes",
            BellStyle::Both => "sound and a flash",
        };
        self.info(format!("bell: {when}; {how} (bell = … in config.toml makes it stick)"));
        Ok(())
    }

    /// Copy a message that mentions you into the mentions window (made at
    /// the end of the list the first time), in time order. `alert` lights
    /// the window up.
    fn record_mention(&mut self, from: usize, line: &Line, alert: bool) {
        let (Some(key), Some(event)) = (self.windows[from].room.clone(), line.event_id.clone()) else { return };
        let key = WinKey { thread: None, ..key };
        if self.mention_origins.contains_key(&event) {
            return;
        }
        let room =
            self.window_idx(&key.account, &key.room_id).map_or(self.windows[from].title(), |i| self.windows[i].title());
        let mut copy = line.clone();
        let nick = self.display_name(line.sender.as_deref(), &line.nick);
        (copy.nick, copy.highlight, copy.cached) = (format!("{room}/{nick}"), true, false);
        let idx = self.mentions_window();
        let w = &mut self.windows[idx];
        // In time order, after the window's own notes.
        let at = w.lines.iter().rposition(|l| l.event_id.is_none() || l.ts <= copy.ts).map_or(0, |p| p + 1);
        w.lines.insert(at, copy);
        while w.lines.len() > MENTIONS_MAX {
            if let Some(old) = w.lines.pop_front().and_then(|l| l.event_id) {
                self.mention_origins.remove(&old);
            }
        }
        self.mention_origins.insert(event, key);
        if alert && !self.on_screen(idx) {
            self.windows[idx].activity = Activity::Highlight;
        }
    }

    /// Mentions found in loaded history (the cache, scrolling back) join the
    /// window quietly.
    fn record_old_mentions(&mut self, account: &str, room_id: &OwnedRoomId, items: &[Item]) {
        let Some(idx) = self.window_idx(account, room_id) else { return };
        for item in items {
            if let Item::Line(line) = item
                && is_mention(line)
            {
                self.record_mention(idx, line, false);
            }
        }
    }

    /// The mentions window, created at the end of the list if needed (so
    /// window numbers don't shift).
    fn mentions_window(&mut self) -> usize {
        if let Some(idx) = self.windows.iter().position(|w| w.is_mentions) {
            return idx;
        }
        let mut w = Window::new(None, "mentions".into());
        w.is_mentions = true;
        w.lines.push_back(Line::info(
            "Messages mentioning you or your highlight words, from every room. Click one, or select it (Alt+Up) \
             and press Enter, to go to it.",
        ));
        self.windows.push(w);
        self.windows.len() - 1
    }

    fn upsert_room(&mut self, account: &str, meta: RoomMeta) {
        if meta.is_space {
            // Spaces are listed by /spaces and group the window list instead.
            if let Some(idx) = self.window_idx(account, &meta.room_id) {
                self.remove_window(idx);
            }
            return;
        }
        let idx = match self.window_idx(account, &meta.room_id) {
            Some(idx) => idx,
            None => {
                let key = WinKey { account: account.to_owned(), room_id: meta.room_id.clone(), thread: None };
                if self.layout.closed.contains(&key) {
                    return;
                }
                self.insert_window(Window::new(Some(key), meta.name.clone()))
            }
        };
        let level = self.windows[idx].update(&meta);
        let person = self.windows[idx].dm_user.as_ref().and_then(|u| self.layout.nicknames.get(u)).cloned();
        self.windows[idx].person = person;
        if !self.on_screen(idx) {
            let w = &mut self.windows[idx];
            w.activity = w.activity.max(level);
        }
    }

    /// Bring back a closed room window, e.g. when someone talks in it.
    fn reopen(&mut self, account: &str, room_id: &OwnedRoomId) -> Option<usize> {
        let meta = self.connected(account)?.quick_meta(room_id)?;
        let key = WinKey { account: account.to_owned(), room_id: room_id.clone(), thread: None };
        self.layout.closed.retain(|k| *k != key);
        let idx = self.insert_window(Window::room(key, &meta));
        self.windows[idx].unread = (0, 0);
        Some(idx)
    }

    /// Insert a room window at its saved position (or at the end).
    fn insert_window(&mut self, mut window: Window) -> usize {
        if let Some(key) = &window.room {
            window.local_name = self.layout.names.iter().find(|(k, _)| k == key).map(|(_, n)| n.clone());
        }
        window.person = window.dm_user.as_ref().and_then(|u| self.layout.nicknames.get(u)).cloned();
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
        let removed = self.windows.remove(idx).id();
        if self.active >= idx && self.active > 0 {
            self.active -= 1;
        }
        // Its pane goes too, unless focus has to land somewhere new.
        let active = self.windows[self.active].id();
        if let Some(split) = &mut self.split
            && let Some(slot) = split.panes.iter().position(|k| *k == removed)
        {
            if split.panes.contains(&active) {
                split.panes.remove(slot);
            } else {
                split.panes[slot] = active;
            }
        }
        if self.split.as_ref().is_some_and(|s| s.panes.len() < 2) {
            self.split = None;
        }
    }

    fn save_layout(&self) {
        // Thread windows are opened on demand and not restored.
        let mut windows: Vec<WinKey> =
            self.windows.iter().filter_map(|w| w.room.clone()).filter(|k| k.thread.is_none()).collect();
        // Keep saved positions for servers that are not connected right now.
        for key in &self.layout.windows {
            if !windows.contains(key) && self.connected(&key.account).is_none() {
                windows.push(key.clone());
            }
        }
        let layout = Layout {
            windows,
            closed: self.layout.closed.clone(),
            names: self.layout.names.clone(),
            nicknames: self.layout.nicknames.clone(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&layout)
            && let Err(e) = std::fs::write(self.paths.layout_file(), json)
        {
            tracing::warn!("saving layout: {e}");
        }
    }

    fn names_lines(&self, idx: usize) -> Vec<Line> {
        let w = &self.windows[idx];
        let mut lines = vec![Line::info(format!("Users {} ({})", w.title(), w.members.len()))];
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

    /// Whether window-list avatars can be real images (a 2-cell picture in
    /// half-blocks would be noise, so those terminals get initials).
    pub fn avatar_images(&self) -> bool {
        self.cfg.sidebar_avatars && self.picker.as_ref().is_some_and(|p| p.protocol_type() != ProtocolType::Halfblocks)
    }

    /// Fetch avatars for rooms in the window list that don't have one yet
    /// (or whose avatar changed).
    fn request_avatars(&mut self) {
        let Some(picker) = self.picker.clone().filter(|_| self.show_sidebar && self.avatar_images()) else { return };
        let wanted: Vec<(String, OwnedRoomId, OwnedMxcUri)> = self
            .windows
            .iter()
            .filter_map(|w| {
                let key = w.room.as_ref().filter(|k| k.thread.is_none())?;
                let url = w.avatar.clone()?;
                let current = self.avatars.get(&(key.account.clone(), key.room_id.clone()));
                (current.is_none_or(|(have, _)| *have != url)).then(|| (key.account.clone(), key.room_id.clone(), url))
            })
            .collect();
        for (account, room_id, url) in wanted {
            let Some(acct) = self.connected(&account) else { continue };
            self.avatars.insert((account, room_id.clone()), (url.clone(), Preview::Loading));
            acct.fetch_avatar(room_id, url, picker.clone(), Size::new(2, 1));
        }
    }

    /// Called after every draw: lazy history, member lists, read markers,
    /// image previews, avatars.
    pub fn after_draw(&mut self) {
        self.request_avatars();
        for (account, url) in std::mem::take(&mut self.link_title_requests) {
            match self.connected(&account) {
                Some(acct) if !self.link_titles_off.contains(&account) => acct.fetch_link_title(url),
                _ => drop(self.link_titles.remove(&url)),
            }
        }
        if let (Some(v), Some(picker)) = (self.viewer.as_mut(), self.picker.clone())
            && let Some((image, view)) = v.wanted()
        {
            v.pending = Some(view);
            let (tx, event_id) = (self.tx.clone(), v.event.clone());
            tokio::task::spawn_blocking(move || {
                let protocol = viewer::render(&image, &view, &picker).map_err(|e| format!("{e:#}"));
                let _ = tx.send(AppEvent::ViewerFrame { event_id, view, protocol });
            });
        }
        // Typing notices normally end with an update from the server; don't
        // let a missed one leave someone "typing" forever.
        for w in &mut self.windows {
            if !w.typing.is_empty() && w.typing_at.is_some_and(|t| t.elapsed() > TYPING_EXPIRY) {
                w.typing.clear();
            }
        }
        if !self.preview_requests.is_empty()
            && let Some(picker) = self.picker.clone()
        {
            let cells = Size::new(self.view_width.saturating_sub(8).clamp(8, 64), self.cfg.image_rows.max(2));
            for (account, event_id, media) in std::mem::take(&mut self.preview_requests) {
                match self.connected(&account) {
                    Some(acct) => acct.fetch_preview(event_id, media, picker.clone(), cells),
                    // Asked again once the server is back.
                    None => drop(self.previews.remove(&event_id)),
                }
            }
        }
        for idx in self.screen_windows() {
            self.window_upkeep(idx);
        }
    }

    /// Members, older history and read markers for a window on screen.
    fn window_upkeep(&mut self, idx: usize) {
        let page = self.cfg.history_page;
        let receipts = self.cfg.read_receipts;
        let focused = idx == self.active;
        let Some(key) = self.windows[idx].room.clone() else { return };
        let Some(acct) = self.connected(&key.account) else { return };
        let w = &mut self.windows[idx];

        if !w.members_requested {
            w.members_requested = true;
            acct.members(key.room_id.clone(), false);
        }

        // A /jump target that isn't loaded yet: keep paging back until it is.
        if let Some(target) = w.seek.clone() {
            if w.contains(&target) {
                w.seek = None;
                if focused {
                    self.select = Some(target);
                }
                return;
            }
            if matches!(w.history, History::Done) {
                w.seek = None;
                self.info("couldn't reach that message in this room's history");
                return;
            }
        }
        let retry_ok = w.history_retry.is_none_or(|t| Instant::now() >= t);
        if (w.at_top || w.seek.is_some()) && retry_ok {
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
            && w.thread_root().is_none()
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
                self.input.paste(&text);
                self.typed();
            }
            Event::FocusGained => self.focused = true,
            Event::FocusLost => self.focused = false,
            Event::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollUp => self.on_wheel(at, true),
            MouseEventKind::ScrollDown => self.on_wheel(at, false),
            MouseEventKind::Down(MouseButton::Left) => self.on_click(at),
            _ => {}
        }
    }

    /// The wheel scrolls the pane under the pointer (older history loads
    /// at the top), or moves the selection in selection mode.
    fn on_wheel(&mut self, at: Position, up: bool) {
        if let Some(v) = self.viewer.as_mut() {
            v.zoom_by(if up { 1 } else { -1 });
            return;
        }
        if let Some(sw) = self.switcher.as_mut() {
            sw.selected = if up { sw.selected.saturating_sub(1) } else { sw.selected + 1 };
            return;
        }
        if let Some(help) = self.help.as_mut() {
            help.scroll =
                if up { help.scroll.saturating_sub(WHEEL_ROWS) } else { help.scroll.saturating_add(WHEEL_ROWS) };
            return;
        }
        let Some(idx) = self.hits.panes.iter().find(|p| p.area.contains(at)).map(|p| p.window) else { return };
        if idx == self.active && self.select.is_some() {
            return self.move_select(if up { -1 } else { 1 });
        }
        let w = &mut self.windows[idx];
        w.scroll = if up { w.scroll + WHEEL_ROWS } else { w.scroll.saturating_sub(WHEEL_ROWS) };
    }

    /// Clicks: a window in the list goes there; a pane gets the focus; a
    /// message is selected (again to unselect); a link opens; a member's
    /// name is added to your message; the input line moves the cursor.
    fn on_click(&mut self, at: Position) {
        if self.viewer.is_some() {
            return;
        }
        if self.switcher.is_some() {
            // A row goes there; a click anywhere else closes the switcher.
            let picked = self.hits.switcher.as_ref().and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at)));
            match picked {
                Some(&(_, n)) => self.switcher_go(Some(n)),
                None if !self.hits.switcher.as_ref().is_some_and(|(area, _)| area.contains(at)) => {
                    self.switcher = None;
                }
                None => {}
            }
            return;
        }
        if self.help.is_some() || self.confirm_delete.is_some() || self.confirm_paste.is_some() {
            return;
        }
        if let Some(&(_, idx)) = self.hits.sidebar.iter().find(|(r, _)| r.contains(at)) {
            return self.switch_to(idx);
        }
        if let Some(&(_, n)) = self.hits.nicklist.iter().find(|(r, _)| r.contains(at)) {
            return self.mention_member(n);
        }
        if let Some((area, text_x, skip)) = self.hits.input
            && area.contains(at)
        {
            return self.input.click(skip, at.x.saturating_sub(text_x) as usize);
        }
        let Some(pane) = self.hits.panes.iter().find(|p| p.area.contains(at)) else { return };
        let idx = pane.window;
        let row = pane.rows.get((at.y - pane.area.y) as usize).cloned().unwrap_or_default();
        if idx != self.active {
            self.switch_to(idx);
        }
        if self.is_editing() {
            return;
        }
        if row.image
            && let Some(line) = row
                .event
                .as_ref()
                .and_then(|e| self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(e)))
                .cloned()
        {
            if let Err(e) = self.open_viewer(&line) {
                self.error(e);
            }
            return;
        }
        if self.win().is_mentions
            && let Some(event) = row.event.clone()
            && let Some(key) = self.mention_origins.get(&event).cloned()
        {
            self.select = None;
            if let Err(e) = self.jump_to(key, event) {
                self.error(e);
            }
            return;
        }
        if let Some(event) = &row.event
            && let Some((_, _, text)) = row.links.iter().find(|(from, to, _)| (*from..*to).contains(&at.x))
            && let Some(url) = self.link_in(event, text)
        {
            return self.open_url(url);
        }
        self.select = match row.event {
            Some(event) if self.select.as_ref() != Some(&event) && self.selectable().contains(&event) => Some(event),
            _ => None,
        };
    }

    /// The link a clicked piece of underlined text belongs to (a wrapped
    /// link shows in pieces).
    fn link_in(&self, event: &EventId, fragment: &str) -> Option<String> {
        let fragment = fragment.trim().trim_matches(|c: char| "()<>[]\"'".contains(c));
        if fragment.chars().count() < 3 {
            return None;
        }
        let line = self.win().lines.iter().rev().find(|l| l.event_id.as_deref() == Some(event))?;
        let text = line.text();
        let url = links::urls(&line.body).into_iter().chain(links::urls(&text)).find(|u| u.contains(fragment));
        url.map(ToOwned::to_owned)
    }

    /// A click on the member list: add that person to your message as a
    /// mention, like completing their name.
    fn mention_member(&mut self, n: usize) {
        let Some(m) = self.win().members.get(n) else { return };
        let (name, user) = (m.name.clone(), m.user_id.clone());
        self.input.end_completion();
        let text = if self.input.is_empty() {
            format!("{name}{} ", self.cfg.completion_char)
        } else {
            let after_word = self.input.cursor() > 0
                && self.input.chars().get(self.input.cursor() - 1).is_some_and(|c| !c.is_whitespace());
            format!("{}{name} ", if after_word { " " } else { "" })
        };
        self.input.insert_str(&text);
        if !self.input.mentions.iter().any(|(_, u)| *u == user) {
            self.input.mentions.push((name, user));
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(v) = self.viewer.as_mut() {
            match key.code {
                Esc | Enter | Char('q' | 'z') => self.close_viewer(),
                Char('c') if ctrl => self.close_viewer(),
                Char('+' | '=') => v.zoom_by(1),
                Char('-' | '_') => v.zoom_by(-1),
                Char('0') => v.zoom_by(0),
                Left | Char('h') => v.pan(-1.0, 0.0),
                Right | Char('l') => v.pan(1.0, 0.0),
                Up | Char('k') => v.pan(0.0, -1.0),
                Down | Char('j') => v.pan(0.0, 1.0),
                _ => {}
            }
            return;
        }
        if let Some(help) = self.help.as_mut() {
            match key.code {
                Esc if !help.filter.is_empty() => {
                    help.filter.clear();
                    help.scroll = 0;
                }
                Esc | F(1) => self.help = None,
                Char('c') if ctrl => self.help = None,
                Up => help.scroll = help.scroll.saturating_sub(1),
                Down => help.scroll += 1,
                PageUp => help.scroll = help.scroll.saturating_sub(10),
                PageDown => help.scroll += 10,
                Home => help.scroll = 0,
                End => help.scroll = usize::MAX,
                Backspace => {
                    help.filter.pop();
                    help.scroll = 0;
                }
                Char(c) if !ctrl && !alt => {
                    help.filter.push(c);
                    help.scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if self.switcher.is_some() {
            return self.on_switcher_key(key);
        }
        if self.select.is_none() && (key.modifiers - KeyModifiers::SHIFT).is_empty() {
            self.konami = eggs::konami_step(self.konami, key.code);
            if self.konami == eggs::KONAMI.len() {
                self.konami = 0;
                return self.konami_unlocked();
            }
        } else {
            self.konami = 0;
        }
        if self.confirm_paste.take().is_some() {
            // Declining leaves the text in the input line to edit.
            if matches!(key.code, Char('y' | 'Y')) {
                self.input.accept_paste();
                self.submit();
            }
            return;
        }
        if let Some(event) = self.confirm_delete.take() {
            let resume = self.resume_edit.take();
            if matches!(key.code, Char('y' | 'Y')) {
                self.select = None;
                match self.room_ctx() {
                    Ok((acct, room_id)) => acct.redact(room_id, event),
                    Err(e) => self.error(e),
                }
                if let Some(session) = resume {
                    self.input.set_text(&session.draft);
                }
            } else if let Some(session) = resume {
                // Declined: back to editing that message.
                self.input.set_text(&session.original);
                self.editing = Some(session);
            }
            return;
        }
        if self.select.is_some() {
            self.on_select_key(key);
            return;
        }
        // Shift+Enter (or Alt+Enter) starts a new line in the message.
        if key.code == Enter && key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
            self.input.end_completion();
            self.input.insert('\n');
            self.typed();
            return;
        }
        if self.editing.is_some() {
            match key.code {
                Up => return self.step_edit(true),
                Down => return self.step_edit(false),
                Esc => return self.cancel_edit(),
                Enter => return self.finish_edit(),
                _ => {}
            }
        }
        if key.code == Up && alt {
            self.enter_select();
            return;
        }
        if (key.code == Up && key.modifiers.contains(KeyModifiers::SHIFT))
            || (ctrl && key.code == Char('e') && self.input.is_empty())
        {
            self.start_edit(None);
            return;
        }
        if key.code == Esc {
            if self.compose.take().is_some() {
                self.input.clear();
            }
            return;
        }
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
                'k' => self.switcher = Some(Switcher::default()),
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
                // With nothing after the cursor to cut, Ctrl+K opens the
                // switcher.
                'k' if self.input.cursor() == self.input.chars().len() => {
                    self.switcher = Some(Switcher::default());
                }
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
            F(1) => self.help = Some(HelpView::default()),
            F(2) => self.show_sidebar = !self.show_sidebar,
            F(3) => self.show_nicklist = !self.show_nicklist,
            F(6) if key.modifiers.contains(KeyModifiers::SHIFT) => self.cycle_pane(-1),
            F(6) => self.cycle_pane(1),
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
        self.select = None;
        self.confirm_delete = None;
        self.resume_edit = None;
        self.cancel_edit();
        // Unsent text (and a reply or reaction being written) stays with
        // the window you leave, and comes back when you return.
        let unsent = Unsent { text: self.input.stash(), compose: self.compose.take().map(|(_, c)| c) };
        let left_key = self.windows[self.active].id();
        self.recent.retain(|k| *k != left_key);
        self.recent.insert(0, left_key);
        self.recent.truncate(64);
        let left = &mut self.windows[self.active];
        left.unsent = (unsent.text.is_some() || unsent.compose.is_some()).then_some(unsent);
        // Everything in the window you leave counts as seen.
        if let Some(latest) = left.latest_event() {
            left.marker = Some(latest);
        }
        // A window already in a pane gets the focus; otherwise it replaces
        // the focused pane's window.
        let to = self.windows[idx].id();
        if let Some(split) = &mut self.split
            && !split.panes.contains(&to)
        {
            let from = self.windows[self.active].id();
            match split.panes.iter().position(|k| *k == from) {
                Some(slot) => split.panes[slot] = to,
                None => split.panes.push(to),
            }
        }
        self.active = idx;
        self.windows[idx].activity = Activity::None;
        if let Some(unsent) = self.windows[idx].unsent.take() {
            if let Some(text) = unsent.text {
                self.input.restore(text);
            }
            if let (Some(compose), Some(key)) = (unsent.compose, self.windows[idx].room.clone()) {
                self.compose = Some((key, compose));
            }
        }
    }

    /// Windows on screen, in pane order (just the active one unless split).
    pub fn screen_windows(&self) -> Vec<usize> {
        let Some(split) = &self.split else { return vec![self.active] };
        split.panes.iter().filter_map(|key| self.windows.iter().position(|w| w.id() == *key)).collect()
    }

    /// Your nicknames for people.
    pub fn nicknames(&self) -> &BTreeMap<OwnedUserId, String> {
        &self.layout.nicknames
    }

    /// What to call someone: your nickname for them, else `fallback`.
    pub fn display_name<'a>(&'a self, user: Option<&UserId>, fallback: &'a str) -> &'a str {
        user.and_then(|u| self.layout.nicknames.get(u)).map_or(fallback, String::as_str)
    }

    /// `/nickname [who [name]]`: your own name for someone, shown in place of
    /// theirs everywhere in Textrix and never sent anywhere.
    fn cmd_nickname(&mut self, args: &str) -> Result<(), String> {
        let (who, name) = split_first(args.trim());
        if who.is_empty() {
            if self.layout.nicknames.is_empty() {
                self.info("no nicknames yet; /nickname <nick|@user> <name> gives someone one");
            }
            let rows: Vec<String> = self.layout.nicknames.iter().map(|(u, n)| format!("  {n} → {u}")).collect();
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        let user = self.resolve_user(who)?;
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(48).collect();
        let text = if name.is_empty() {
            match self.layout.nicknames.remove(&user) {
                Some(old) => format!("{user} is no longer “{old}” here"),
                None => return Err(format!("you haven't given {user} a nickname")),
            }
        } else {
            self.layout.nicknames.insert(user.clone(), name.clone());
            format!("{user} shows as “{name}” here (only you see it; /nickname {who} takes it off)")
        };
        for w in &mut self.windows {
            if w.dm_user.as_ref() == Some(&user) {
                w.person = self.layout.nicknames.get(&user).cloned();
            }
        }
        self.save_layout();
        self.info(text);
        Ok(())
    }

    /// The window's room's own notification mode (`None`: the default).
    pub fn room_mode(&self, idx: usize) -> Option<RoomNotificationMode> {
        let key = self.windows[idx].room.as_ref()?;
        self.push_modes.get(key.account.as_str())?.get(&key.room_id).copied()
    }

    pub fn on_screen(&self, idx: usize) -> bool {
        idx == self.active || self.split.as_ref().is_some_and(|s| s.panes.contains(&self.windows[idx].id()))
    }

    /// Switcher entries for what's typed: every window, and closed rooms.
    /// With nothing typed the current window is left out, so Enter goes
    /// back to the previous one.
    pub fn switcher_entries(&self) -> Vec<switcher::Entry> {
        let Some(sw) = &self.switcher else { return Vec::new() };
        // Forwarding lists the other rooms on the message's server.
        let forward_from = match &sw.purpose {
            switcher::Purpose::Forward { account, room_id, .. } => Some((account.as_str(), room_id)),
            switcher::Purpose::Go => None,
        };
        let wanted = |key: Option<&WinKey>| match (forward_from, key) {
            (None, _) => true,
            (Some((account, from)), Some(k)) => k.account == account && k.room_id != *from && k.thread.is_none(),
            (Some(_), None) => false,
        };
        let several = self.accounts.len() > 1;
        let recency = |key: &WinRef| self.recent.iter().position(|k| k == key).unwrap_or(usize::MAX);
        let mut candidates = Vec::new();
        for (i, w) in self.windows.iter().enumerate() {
            if (i == self.active && sw.query.trim().is_empty()) || !wanted(w.room.as_ref()) {
                continue;
            }
            let detail = match w.account() {
                Some(account) if several => account.to_owned(),
                _ => String::new(),
            };
            candidates.push(switcher::Candidate {
                entry: switcher::Entry {
                    target: switcher::Target::Window(i),
                    name: w.title().to_owned(),
                    detail: match (&w.local_name, detail.is_empty()) {
                        (Some(_), true) => w.name.clone(),
                        (Some(_), false) => format!("{} · {detail}", w.name),
                        (None, _) => detail,
                    },
                    activity: w.activity,
                    unread: w.unread(),
                    matched: Vec::new(),
                    aka: w.local_name.as_ref().map(|_| w.name.clone()),
                },
                number: Some(i + 1),
                recency: recency(&w.id()),
            });
        }
        for key in self.layout.closed.iter().filter(|k| k.thread.is_none() && wanted(Some(k))) {
            let Some(meta) = self.connected(&key.account).and_then(|a| a.quick_meta(&key.room_id)) else { continue };
            let detail = if several { format!("{} · closed", key.account) } else { "closed".to_owned() };
            candidates.push(switcher::Candidate {
                entry: switcher::Entry {
                    target: switcher::Target::Closed { account: key.account.clone(), room_id: key.room_id.clone() },
                    name: meta.name,
                    detail,
                    activity: Activity::None,
                    unread: meta.unread,
                    matched: Vec::new(),
                    aka: None,
                },
                number: None,
                recency: usize::MAX,
            });
        }
        switcher::rank(&sw.query, candidates)
    }

    fn on_switcher_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(sw) = self.switcher.as_mut() else { return };
        match key.code {
            Esc => self.switcher = None,
            Char('c' | 'k') if ctrl => self.switcher = None,
            Enter => self.switcher_go(None),
            Up | BackTab => sw.selected = sw.selected.saturating_sub(1),
            Char('p') if ctrl => sw.selected = sw.selected.saturating_sub(1),
            Down | Tab => sw.selected += 1,
            Char('n') if ctrl => sw.selected += 1,
            Backspace => {
                sw.query.pop();
                sw.selected = 0;
            }
            Char('u') if ctrl => {
                sw.query.clear();
                sw.selected = 0;
            }
            Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                sw.query.push(c);
                sw.selected = 0;
            }
            _ => {}
        }
    }

    /// Go to the chosen switcher entry (or entry `pick`) and close it.
    fn switcher_go(&mut self, pick: Option<usize>) {
        let entries = self.switcher_entries();
        let Some(sw) = self.switcher.take() else { return };
        let Some(entry) = entries.get(pick.unwrap_or(sw.selected).min(entries.len().saturating_sub(1))) else {
            return;
        };
        if let switcher::Purpose::Forward { account, room_id, event, edited } = sw.purpose {
            let to = match &entry.target {
                switcher::Target::Window(idx) => self.windows[*idx].room.as_ref().map(|k| k.room_id.clone()),
                switcher::Target::Closed { room_id, .. } => Some(room_id.clone()),
            };
            match (self.connected(&account), to) {
                (Some(acct), Some(to)) => acct.forward(room_id, event, edited, to, entry.name.clone()),
                (None, _) => self.error(format!("{account} is not connected")),
                (_, None) => {}
            }
            return;
        }
        match entry.target.clone() {
            switcher::Target::Window(idx) => self.switch_to(idx),
            switcher::Target::Closed { account, room_id } => match self.reopen(&account, &room_id) {
                Some(idx) => self.switch_to(idx),
                None => self.error(format!("{} isn't available right now", entry.name)),
            },
        }
    }

    /// F6 / Shift+F6: focus the next or previous pane.
    fn cycle_pane(&mut self, step: isize) {
        let panes = self.screen_windows();
        if panes.len() < 2 {
            return;
        }
        let pos = panes.iter().position(|&i| i == self.active).unwrap_or(0) as isize;
        self.switch_to(panes[(pos + step).rem_euclid(panes.len() as isize) as usize]);
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
        if let Some(idx) = (1..=n).map(|step| (self.active + step) % n).find(|&i| self.windows[i].activity == best) {
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

    // ---- selection mode --------------------------------------------------

    fn selectable(&self) -> Vec<OwnedEventId> {
        self.win().lines.iter().filter(|l| l.is_message() && !l.pending).filter_map(|l| l.event_id.clone()).collect()
    }

    fn selected_line(&self) -> Option<&Line> {
        let id = self.select.as_ref()?;
        self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(id))
    }

    fn enter_select(&mut self) {
        match self.selectable().pop() {
            Some(last) => self.select = Some(last),
            None => self.info("no messages to select here"),
        }
    }

    /// Move the selection; moving below the newest message leaves selection
    /// mode. At the top, the renderer pulls in older history.
    fn move_select(&mut self, delta: isize) {
        let ids = self.selectable();
        let Some(pos) = self.select.as_ref().and_then(|s| ids.iter().position(|id| id == s)) else {
            self.select = None;
            return;
        };
        let target = pos as isize + delta;
        if target >= ids.len() as isize {
            self.select = None;
            self.win_mut().scroll = 0;
        } else {
            self.select = Some(ids[target.max(0) as usize].clone());
        }
    }

    fn on_select_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(line) = self.selected_line().cloned() else {
            self.select = None;
            return;
        };
        let event = line.event_id.clone().expect("selectable lines have ids");
        let key_here = self.win().room.clone();
        match key.code {
            Up | Char('k') => self.move_select(-1),
            Down | Char('j') => self.move_select(1),
            PageUp => self.move_select(-10),
            PageDown => self.move_select(10),
            Home | Char('g') => self.select = self.selectable().first().cloned(),
            End | Char('G') => self.select = self.selectable().pop(),
            Enter if self.win().is_mentions => {
                self.select = None;
                let key =
                    self.mention_origins.get(&event).cloned().ok_or_else(|| "that message's room is gone".to_owned());
                if let Err(e) = key.and_then(|key| self.jump_to(key, event)) {
                    self.error(e);
                }
            }
            Esc | Char('q') | Enter => self.select = None,
            Char('r') => match (line.sender.clone(), key_here) {
                (Some(sender), Some(here)) => {
                    self.compose = Some((here, Compose::Reply { event, sender, nick: line.nick.clone() }));
                    self.select = None;
                }
                _ => self.error("can't reply to that"),
            },
            Char('e') => {
                self.select = None;
                self.start_edit(Some(event));
            }
            Char('d') => self.request_delete(event, None),
            Char('+' | '=') => {
                if let Some(here) = key_here {
                    self.compose = Some((here, Compose::React { event, nick: line.nick.clone() }));
                    self.input.clear();
                    self.select = None;
                }
            }
            Char('s') => match (line.media.clone(), self.room_ctx()) {
                (Some(media), Ok((acct, room_id))) => acct.download(room_id, media),
                (None, _) => self.error("that message has no attachment"),
                (_, Err(e)) => self.error(e),
            },
            Char('t') => {
                self.select = None;
                let root = line.thread_root.clone().unwrap_or(event);
                if let Err(e) = self.open_thread(root) {
                    self.error(e);
                }
            }
            Char('z') => {
                if let Err(e) = self.open_viewer(&line) {
                    self.error(e);
                }
            }
            Char('f') => {
                self.select = None;
                if let Err(e) = self.start_forward(&line) {
                    self.error(e);
                }
            }
            Char('p') => {
                let pin = !self.win().pinned.contains(&event);
                match (self.win().perms, self.room_ctx()) {
                    (Some(perms), _) if !perms.can_pin => {
                        self.error("your power level doesn't allow pinning messages here")
                    }
                    (_, Ok((acct, room_id))) => acct.set_pinned(room_id, event, pin),
                    (_, Err(e)) => self.error(e),
                }
            }
            Char('v') => {
                if let Some(target) = self.win_mut().find_mut(&event) {
                    target.reveal = !target.reveal;
                }
            }
            Char('o') => match links::urls(&line.body).first() {
                Some(url) => self.open_url((*url).to_owned()),
                None => self.error("that message has no link"),
            },
            Char('c') => {
                self.clipboard = Some(line.body.clone());
                self.info(format!("copied {} characters to the clipboard", line.body.chars().count()));
            }
            Char('i') => {
                let sender = line.sender.as_ref().map(|s| s.to_string()).unwrap_or_default();
                self.info(format!("{} [{sender}] at {}", line.nick, line.ts.format("%Y-%m-%d %H:%M:%S")));
                self.info(format!(" event {event}"));
            }
            _ => {}
        }
    }

    /// `<nick> “first words…”` of the message awaiting delete confirmation.
    pub fn delete_summary(&self) -> Option<String> {
        let id = self.confirm_delete.as_ref()?;
        let line = self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(id))?;
        Some(format!("<{}> “{}”", line.nick, snippet(&line.body)))
    }

    /// Ask y/n before deleting, after checking your power level allows it.
    fn request_delete(&mut self, event: OwnedEventId, resume: Option<EditSession>) {
        let own = self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(&event)).is_some_and(|l| l.own);
        let allowed = self.win().perms.is_none_or(|p| if own { p.can_redact_own } else { p.can_redact_other });
        if !allowed {
            if let Some(session) = resume {
                self.input.set_text(&session.original);
                self.editing = Some(session);
            }
            return self.error(if own {
                "your power level doesn't allow deleting messages here"
            } else {
                "only moderators can delete other people's messages here"
            });
        }
        self.confirm_delete = Some(event);
        self.resume_edit = resume;
    }

    // ---- edit mode -------------------------------------------------------

    /// Your own text messages in this window, oldest first.
    fn editable(&self) -> Vec<&Line> {
        self.win()
            .lines
            .iter()
            .filter(|l| l.own && is_target(l) && l.media.is_none())
            .filter(|l| matches!(l.kind, LineKind::Message | LineKind::Emote | LineKind::Notice))
            .collect()
    }

    /// Matrix only accepts edits from a message's sender, and only if your
    /// power level lets you post in the room.
    fn check_can_edit(&self) -> Result<(), String> {
        self.room_ctx()?;
        if self.win().perms.is_some_and(|p| !p.can_send) {
            return Err("your power level here doesn't allow sending messages, so you can't edit either".into());
        }
        Ok(())
    }

    /// Shift+Up, Ctrl+E on an empty line, `/edit`, or `e` in selection mode.
    fn start_edit(&mut self, at: Option<OwnedEventId>) {
        if let Err(e) = self.check_can_edit() {
            return self.error(e);
        }
        let editable = self.editable();
        let target = match &at {
            Some(id) => editable.iter().find(|l| l.event_id.as_ref() == Some(id)).copied(),
            None => editable.last().copied(),
        };
        let Some(line) = target.cloned() else {
            return self.error(match at {
                Some(_) => "only your own text messages can be edited",
                None => "you have no message here to edit",
            });
        };
        let draft = match self.editing.take() {
            Some(session) => session.draft,
            None => self.input.text(),
        };
        self.load_edit(&line, draft);
    }

    fn load_edit(&mut self, line: &Line, draft: String) {
        let (Some(key), Some(event)) = (self.win().room.clone(), line.event_id.clone()) else { return };
        let emote = line.kind == LineKind::Emote;
        self.editing = Some(EditSession { key, event, original: line.body.clone(), emote, draft });
        self.input.set_text(&line.body);
    }

    /// ↑ to an older message of yours, ↓ to a newer one; ↓ past the newest
    /// stops editing.
    fn step_edit(&mut self, older: bool) {
        let Some(session) = self.editing.clone() else { return };
        let editable: Vec<Line> = self.editable().into_iter().cloned().collect();
        let Some(pos) = editable.iter().position(|l| l.event_id.as_ref() == Some(&session.event)) else {
            return self.cancel_edit();
        };
        match (older, pos) {
            (true, 0) => self.info("that's your oldest loaded message here; PgUp loads more history"),
            (true, p) => self.load_edit(&editable[p - 1], session.draft),
            (false, p) if p + 1 < editable.len() => self.load_edit(&editable[p + 1], session.draft),
            (false, _) => self.cancel_edit(),
        }
    }

    fn cancel_edit(&mut self) {
        if let Some(session) = self.editing.take() {
            self.input.set_text(&session.draft);
        }
    }

    /// Enter: send the edit; unchanged text just closes the editor, and an
    /// emptied message offers to delete it instead.
    fn finish_edit(&mut self) {
        let Some(session) = self.editing.take() else { return };
        self.mentions = std::mem::take(&mut self.input.mentions);
        let text = self.input.text();
        if text.trim().is_empty() {
            let event = session.event.clone();
            return self.request_delete(event, Some(session));
        }
        self.input.set_text(&session.draft);
        if text != session.original
            && let Err(e) = self.send_edit(session.event, session.emote, &text)
        {
            self.error(e);
        }
        self.mentions.clear();
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The message to highlight: selected, being edited, or about to be
    /// deleted.
    pub fn focus_event(&self) -> Option<OwnedEventId> {
        let here = self.win().room.as_ref();
        self.confirm_delete
            .clone()
            .or_else(|| self.select.clone())
            .or_else(|| self.editing.as_ref().filter(|s| Some(&s.key) == here).map(|s| s.event.clone()))
    }

    /// Dim text after the cursor: matching commands while typing a command
    /// name, then that command's usage once it's complete.
    pub fn input_hint(&self) -> Option<String> {
        if let Some(hint) = self.input.completion_hint() {
            return Some(format!("  {hint}"));
        }
        if self.editing.is_some() {
            return Some("  Enter saves · ↑/↓ your other messages · Esc cancels".into());
        }
        if self.compose_label().is_some() {
            return Some("Enter sends · Esc cancels".into());
        }
        let text = self.input.text();
        if let Some(hint) = eggs::clippy(&text) {
            return Some(format!("  {hint}"));
        }
        let typed = text.strip_prefix('/').filter(|t| !t.starts_with('/') && !t.contains('\n'))?;
        let (word, rest) = match typed.find(' ') {
            Some(i) => (&typed[..i], Some(&typed[i + 1..])),
            None => (typed, None),
        };
        let word = word.to_lowercase();
        let real = ALIASES.iter().find(|(a, _)| *a == word).map_or(word.as_str(), |(_, r)| *r);
        let real = match real {
            "wc" | "wl" => "window",
            other => other,
        };
        let exact = COMMANDS.iter().find(|c| c.name == real);
        match (exact, rest) {
            (Some(cmd), Some(rest)) if rest.trim().is_empty() => Some(format!("{} — {}", cmd.args, cmd.help)),
            (Some(_), Some(_)) => None,
            (Some(cmd), None) => Some(format!(" {} — {}", cmd.args, cmd.help)),
            (None, _) if let Some(expansion) = self.cfg.aliases.get(&word) => Some(format!("  → {expansion}")),
            (None, None) => {
                let names: Vec<&str> =
                    COMMANDS.iter().map(|c| c.name).filter(|n| n.starts_with(word.as_str())).collect();
                (!names.is_empty()).then(|| format!("  {}", names.join(" ")))
            }
            (None, _) if EGGS.contains(&word.as_str()) => None,
            (None, Some(_)) => Some("  unknown command; /help lists them".into()),
        }
    }

    /// The prompt label while a reply/edit/reaction target is chosen.
    pub fn compose_label(&self) -> Option<String> {
        if let Some(session) = &self.editing {
            let editable = self.editable();
            let pos = editable.iter().position(|l| l.event_id.as_ref() == Some(&session.event))?;
            let when = editable[pos].ts.format("%H:%M");
            return Some(format!("edit {when} {}/{}", editable.len() - pos, editable.len()));
        }
        let (key, compose) = self.compose.as_ref()?;
        if self.win().room.as_ref() != Some(key) {
            return None;
        }
        Some(match compose {
            Compose::Reply { nick, .. } => format!("reply to {nick}"),
            Compose::React { nick, .. } => format!("react to {nick}"),
        })
    }

    fn candidates(&self) -> Vec<Candidate> {
        let (word, at_start) = self.input.completion_word();
        let lower = word.to_lowercase();

        if at_start && let Some(prefix) = lower.strip_prefix('/') {
            let mut names: Vec<&str> = COMMANDS
                .iter()
                .map(|c| c.name)
                .chain(ALIASES.iter().map(|a| a.0))
                .chain(["wc", "wl"])
                .chain(self.cfg.aliases.keys().map(String::as_str))
                .collect();
            names.sort_unstable();
            return names
                .into_iter()
                .filter(|n| n.starts_with(prefix))
                .map(|n| Candidate { text: format!("/{n} "), user: None, hint: None })
                .collect();
        }

        let text = self.input.text();
        let first = text.split_whitespace().next().unwrap_or("");
        if SERVER_COMMANDS.iter().any(|c| first == format!("/{c}")) {
            return self
                .accounts
                .keys()
                .filter(|n| n.starts_with(&lower))
                .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                .collect();
        }
        let second = text.split_whitespace().nth(1).unwrap_or("");
        if first == "/upload" || (first == "/keys" && matches!(second, "export" | "import") && !at_start) {
            return path_candidates(&word);
        }

        if word.is_empty() {
            return Vec::new();
        }
        if let Some(prefix) = word.strip_prefix(':') {
            return crate::emoji::complete(prefix.trim_end_matches(':'))
                .into_iter()
                .map(|(emoji, code)| Candidate { text: emoji.to_owned(), user: None, hint: Some(format!(":{code}:")) })
                .collect();
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
            // Your nickname for someone finds them too, but what's inserted
            // is their own name: nicknames never leave Textrix.
            let matches = if word.starts_with('@') {
                user.as_str().starts_with(&word)
            } else {
                nick.to_lowercase().starts_with(&lower)
                    || self.layout.nicknames.get(user).is_some_and(|n| n.to_lowercase().starts_with(&lower))
            };
            if matches && seen.insert(user.clone()) {
                let text = if word.starts_with('@') { user.to_string() } else { nick.to_owned() };
                out.push(Candidate { text: format!("{text}{suffix}"), user: Some(user.clone()), hint: None });
            }
        }
        out
    }

    // ---- commands --------------------------------------------------------

    fn submit(&mut self) {
        let limit = self.cfg.paste_confirm_lines;
        if let Some(lines) = self.input.pasted_lines()
            && limit > 0
            && lines > limit
        {
            self.confirm_paste = Some(lines);
            return;
        }
        self.mentions = std::mem::take(&mut self.input.mentions);
        let text = self.input.take();
        self.stop_typing();
        if text.trim().is_empty() {
            return;
        }
        if let Some((key, target)) = self.compose.take()
            && self.win().room.as_ref() == Some(&key)
            && !text.starts_with('/')
        {
            let result = match target {
                Compose::Reply { event, sender, .. } => self.send_reply(event, sender, &text),
                Compose::React { event, .. } => self.send_react(event, text.trim()),
            };
            if let Err(e) = result {
                self.error(e);
            }
            self.mentions.clear();
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
            "edit" if args.trim().is_empty() => {
                self.start_edit(None);
                Ok(())
            }
            "edit" => self.cmd_edit(args),
            "redact" => self.cmd_redact(),
            "react" => self.cmd_react(args),
            "upload" => self.cmd_upload(args),
            "lastlog" => self.cmd_lastlog(args),
            "search" => self.cmd_search(args),
            "jump" => self.cmd_jump(args),
            "urls" => {
                self.cmd_urls();
                Ok(())
            }
            "open" => self.cmd_open(args),
            "list" => self.pick_account(args).map(|(acct, search)| {
                let search = Some(search.trim()).filter(|s| !s.is_empty()).map(str::to_owned);
                acct.list_public(self.here(), search);
            }),
            "away" => self.cmd_away(args),
            "notify" => self.cmd_notify(args),
            "nickname" => self.cmd_nickname(args),
            "bell" => self.cmd_bell(args),
            "keys" => self.cmd_keys(args),
            "forward" | "fwd" => self.cmd_forward(args),
            "markread" | "read" => self.cmd_markread(args),
            "theme" => self.cmd_theme(args),
            "devices" | "sessions" => self.cmd_devices(args),
            "hilight" | "highlight" => self.pick_account(args).map(|(acct, word)| match word.trim() {
                "" => acct.list_highlights(self.here()),
                word => acct.set_highlight(self.here(), word.to_owned(), true),
            }),
            "dehilight" | "unhilight" => self.pick_account(args).and_then(|(acct, word)| {
                if word.trim().is_empty() {
                    return Err("usage: /dehilight <word>".to_owned());
                }
                acct.set_highlight(self.here(), word.trim().to_owned(), false);
                Ok(())
            }),
            "ignore" => self.pick_account(args).and_then(|(acct, who)| {
                match who.trim() {
                    "" => acct.list_ignored(self.here()),
                    who => acct.ignore(self.here(), self.resolve_user(who)?, true),
                }
                Ok(())
            }),
            "unignore" => self.pick_account(args).and_then(|(acct, who)| {
                let who = who.trim();
                if who.is_empty() {
                    return Err("usage: /unignore <@user>".into());
                }
                acct.ignore(self.here(), self.resolve_user(who)?, false);
                Ok(())
            }),
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
            "spaces" => self.cmd_spaces(),
            "space" => self.cmd_space(args),
            "name" => self.room_ctx().and_then(|(acct, room_id)| {
                let name = args.trim();
                if name.is_empty() {
                    return Err("usage: /name <new name>".into());
                }
                acct.rename(room_id, name.to_owned());
                Ok(())
            }),
            "info" => self.room_ctx().map(|(acct, room_id)| acct.room_info(room_id)),
            "mentions" => {
                let idx = self.mentions_window();
                self.switch_to(idx);
                Ok(())
            }
            "pins" | "pinned" => self.room_ctx().map(|(acct, room_id)| acct.pins(room_id)),
            "joinrule" => self.cmd_joinrule(args),
            "guests" => self.room_ctx().and_then(|(acct, room_id)| {
                acct.set_guest_access(room_id, on_off(args, "usage: /guests <on|off>")?);
                Ok(())
            }),
            "history" => self.room_ctx().and_then(|(acct, room_id)| {
                let visibility = match args.trim() {
                    "joined" => HistoryVisibility::Joined,
                    "invited" => HistoryVisibility::Invited,
                    "shared" => HistoryVisibility::Shared,
                    "world" | "world_readable" => HistoryVisibility::WorldReadable,
                    _ => return Err("usage: /history <joined|invited|shared|world>".into()),
                };
                acct.set_history_visibility(room_id, visibility);
                Ok(())
            }),
            "publish" => self.room_ctx().and_then(|(acct, room_id)| {
                acct.set_published(room_id, on_off(args, "usage: /publish <on|off>")?);
                Ok(())
            }),
            "upgrade" => self.room_ctx().map(|(acct, room_id)| {
                let (version, rest) = split_first(args);
                let version = Some(version).filter(|v| !v.is_empty()).map(str::to_owned);
                acct.upgrade(room_id, version, rest.trim() == "confirm");
            }),
            "rooms" => self.pick_account(args).map(|(acct, _)| acct.list_rooms(self.here())),
            "invites" => self.pick_account(args).map(|(acct, _)| acct.list_invites(self.here())),
            "decline" => self.pick_account(args).and_then(|(acct, room)| {
                let room_id =
                    RoomId::parse(room.trim()).map_err(|_| "usage: /decline <!room:server> (see /invites)")?;
                acct.decline(self.here(), room_id);
                Ok(())
            }),
            "part" if self.win().thread_root().is_some() => {
                Err("this is a thread window: /wc closes it (/part in the room's own window leaves the room)".into())
            }
            "part" => self.room_ctx().map(|(acct, room_id)| acct.leave(room_id)),
            "thread" => self.cmd_thread(args),
            "threads" => self.room_ctx().map(|(acct, room_id)| acct.list_threads(room_id)),
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
            "split" => self.cmd_split(args, false),
            "vsplit" => self.cmd_split(args, true),
            "unsplit" => self.cmd_unsplit(args),
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
            "sidebar" => {
                self.show_sidebar = !self.show_sidebar;
                Ok(())
            }
            "nicklist" => {
                self.show_nicklist = !self.show_nicklist;
                Ok(())
            }
            "reload" => {
                self.cmd_reload();
                Ok(())
            }
            "log" => {
                match args.trim() {
                    "on" => {
                        self.cfg.log = true;
                        self.apply_logging();
                    }
                    "off" => {
                        self.cfg.log = false;
                        self.apply_logging();
                    }
                    "" => {
                        let text = match &self.log {
                            Some(log) => format!("logging to {}", log.dir().display()),
                            None => "chat logging is off (/log on)".to_owned(),
                        };
                        self.info(text);
                    }
                    _ => return self.error("usage: /log [on|off]"),
                }
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
            "alias" => self.cmd_alias(args),
            "unalias" => self.cmd_unalias(args),
            other => match self.cfg.aliases.get(other).cloned() {
                Some(template) => self.run_alias(&template, args),
                None => {
                    let other = other.to_owned();
                    self.egg_command(&other, args)
                        .unwrap_or_else(|| Err(format!("unknown command /{other}; try /help")))
                }
            },
        };
        if let Err(e) = result {
            self.error(e);
        }
    }

    /// Run an alias's commands; aliases may use other aliases, a few deep.
    fn run_alias(&mut self, template: &str, args: &str) -> Result<(), String> {
        if self.alias_depth >= 8 {
            return Err("aliases call each other too deeply (a loop?)".into());
        }
        self.alias_depth += 1;
        for cmd in expand_alias(template, args) {
            self.command(&cmd);
        }
        self.alias_depth -= 1;
        Ok(())
    }

    fn cmd_alias(&mut self, args: &str) -> Result<(), String> {
        let (name, expansion) = split_first(args.trim());
        let name = name.trim_start_matches('/').to_lowercase();
        if name.is_empty() {
            if self.cfg.aliases.is_empty() {
                self.info("no aliases yet; e.g. /alias tt /window goto Tetrix Tests");
            }
            let rows: Vec<String> = self.cfg.aliases.iter().map(|(n, e)| format!("  /{n} → {e}")).collect();
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        if expansion.trim().is_empty() {
            let text = match self.cfg.aliases.get(&name) {
                Some(e) => format!("/{name} → {e}"),
                None => format!("no alias /{name}"),
            };
            self.info(text);
            return Ok(());
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err("alias names are letters, digits, - and _".into());
        }
        if COMMANDS.iter().any(|c| c.name == name) || ALIASES.iter().any(|(a, _)| *a == name) {
            return Err(format!("/{name} is a built-in command"));
        }
        let expansion = expansion.trim().to_owned();
        crate::config::save_alias(&self.paths.config_file(), &name, Some(&expansion)).map_err(|e| format!("{e:#}"))?;
        self.info(format!("/{name} → {expansion} (saved in {})", self.paths.config_file().display()));
        self.cfg.aliases.insert(name, expansion);
        Ok(())
    }

    fn cmd_unalias(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().trim_start_matches('/').to_lowercase();
        if self.cfg.aliases.remove(&name).is_none() {
            return Err(format!("no alias /{name}; /alias lists them"));
        }
        crate::config::save_alias(&self.paths.config_file(), &name, None).map_err(|e| format!("{e:#}"))?;
        self.info(format!("removed /{name}"));
        Ok(())
    }

    fn say(&mut self, text: &str, kind: Say) {
        if text.trim().is_empty() {
            return;
        }
        let (acct, room_id) = match self.room_ctx() {
            Ok(ctx) => ctx,
            Err(e) => return self.error(e),
        };
        let content = self.as_thread_message(compose(text, kind, &self.mentions, self.cfg.markdown));
        self.send_content(acct, room_id, content, text, kind.line_kind(), None);
        self.note_away(None);
    }

    /// Like irssi's "nick is away": after you write to someone (in their
    /// DM, by mentioning them, or replying to them) who's away or offline,
    /// say so once, and again only when that changes.
    fn note_away(&mut self, replied_to: Option<OwnedUserId>) {
        let Some(account) = self.win().account().map(ToOwned::to_owned) else { return };
        let own = self.connected(&account).map(|a| a.user_id);
        let mut people: Vec<OwnedUserId> = self.win().dm_user.iter().cloned().collect();
        people.extend(self.mentions.iter().map(|(_, user)| user.clone()));
        people.extend(replied_to);
        people.dedup();
        for user in people.into_iter().filter(|u| Some(u) != own.as_ref()) {
            let Some(presence) = self.presence_of(&account, &user) else { continue };
            let status = presence.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let (text, key) = match presence.kind {
                PresenceKind::Online => continue,
                PresenceKind::Away => ("away".to_owned(), format!("away {status:?}")),
                PresenceKind::Offline => match presence.last_active {
                    Some(at) => {
                        (format!("offline, last seen {}", ago(Local::now() - at)), format!("offline {status:?}"))
                    }
                    None => ("offline".to_owned(), format!("offline {status:?}")),
                },
            };
            if self.away_noted.get(&user) == Some(&key) {
                continue;
            }
            let fallback = user.localpart().to_owned();
            let nick = self.win().members.iter().find(|m| m.user_id == user).map_or(fallback, |m| m.name.clone());
            let name = self.display_name(Some(&user), &nick).to_owned();
            let line = match status {
                Some(status) => format!("{name} is {text}: {status}"),
                None => format!("{name} is {text}"),
            };
            self.away_noted.insert(user, key);
            self.info(line);
        }
    }

    /// In a thread window: (root, latest event, its sender), for sending
    /// into the thread.
    fn thread_context(&self) -> Option<(OwnedEventId, OwnedEventId, OwnedUserId)> {
        let w = self.win();
        let root = w.thread_root()?.to_owned();
        let latest = w.lines.iter().rev().find(|l| l.event_id.is_some() && !l.pending && l.is_message());
        let fallback_sender = || self.connected(w.account()?).map(|a| a.user_id);
        let (event, sender) = match latest {
            Some(l) => (l.event_id.clone()?, l.sender.clone().or_else(fallback_sender)?),
            None => (root.clone(), fallback_sender()?),
        };
        Some((root, event, sender))
    }

    /// Wrap content as a thread message when sending from a thread window.
    fn as_thread_message(&self, content: RoomMessageEventContent) -> RoomMessageEventContent {
        let Some((root, latest, sender)) = self.thread_context() else { return content };
        let thread = Thread::plain(root.clone(), latest.clone());
        let metadata = if latest == root {
            ReplyMetadata::new(&root, &sender, None)
        } else {
            ReplyMetadata::new(&latest, &sender, Some(&thread))
        };
        content.make_for_thread(metadata, ReplyWithinThread::No, AddMentions::No)
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
        let mut line = Line::new(kind, crate::emoji::expand(text).into_owned());
        line.reply_to = reply_to;
        if let Some(root) = self.win().thread_root() {
            line.thread = true;
            line.thread_root = Some(root.to_owned());
        }
        line.rich = crate::convert::rich_of(&content.msgtype);
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
        let (target, text) = self.win().answer_target(args);
        if text.trim().is_empty() {
            return Err("usage: /reply [nick] <text>".into());
        }
        let target = target.ok_or("nothing to reply to")?;
        let (event_id, sender) =
            (target.event_id.clone().expect("filtered"), target.sender.clone().ok_or("unknown sender")?);
        let text = text.to_owned();
        self.send_reply(event_id, sender, &text)
    }

    fn send_reply(&mut self, event_id: OwnedEventId, sender: OwnedUserId, text: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        // In a thread window the reply stays in the thread.
        let thread = self.win().thread_root().map(|root| Thread::plain(root.to_owned(), event_id.clone()));
        let content = compose(text, Say::Text, &self.mentions, self.cfg.markdown).make_reply_to(
            ReplyMetadata::new(&event_id, &sender, thread.as_ref()),
            ForwardThread::Yes,
            AddMentions::Yes,
        );
        self.send_content(acct, room_id, content, text, LineKind::Message, Some(event_id));
        self.note_away(Some(sender));
        Ok(())
    }

    fn cmd_edit(&mut self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("usage: /edit <text>".into());
        }
        let target = self.win().last_message(true).ok_or("you have no message here to edit")?;
        let (event_id, emote) = (target.event_id.clone().expect("filtered"), target.kind == LineKind::Emote);
        self.send_edit(event_id, emote, text)
    }

    fn send_edit(&mut self, event_id: OwnedEventId, emote: bool, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("an edit can't be empty; use d in selection mode or /redact to delete".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        let kind = if emote { Say::Emote } else { Say::Text };
        let content = compose(text, kind, &self.mentions, self.cfg.markdown)
            .make_replacement(ReplacementMetadata::new(event_id.clone(), None));
        let rich = crate::convert::rich_of(&content.msgtype);
        if let Some(line) = self.win_mut().find_mut(&event_id) {
            line.body = crate::emoji::expand(text).into_owned();
            line.rich = rich;
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
        let (target, key) = self.win().answer_target(args.trim());
        let key = key.trim();
        if key.is_empty() {
            return Err("usage: /react [nick] <emoji>".into());
        }
        let target = target.ok_or("nothing to react to")?;
        let event_id = target.event_id.clone().expect("filtered");
        let key = key.to_owned();
        self.send_react(event_id, &key)
    }

    fn send_react(&mut self, event_id: OwnedEventId, key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("type an emoji (or any text) to react with".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        acct.react(room_id, event_id, crate::emoji::expand(key).into_owned());
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

    fn cmd_lastlog(&mut self, args: &str) -> Result<(), String> {
        const USAGE: &str = "usage: /lastlog [-n N] [-nick nick] <text>";
        let (mut limit, mut nick, mut rest) = (None, None, args.trim());
        loop {
            if let Some(r) = rest.strip_prefix("-n ") {
                let (n, r) = split_first(r);
                limit = Some(n.parse::<usize>().map_err(|_| USAGE)?);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("-nick ") {
                let (n, r) = split_first(r);
                nick = Some(n.to_lowercase());
                rest = r;
            } else {
                break;
            }
        }
        let needle = rest.trim().to_lowercase();
        if needle.is_empty() && nick.is_none() {
            return Err(USAGE.into());
        }
        let today = Local::now().date_naive();
        let w = self.win();
        let loaded = w.lines.iter().filter(|l| l.event_id.is_some()).count();
        let mut hits: Vec<String> = w
            .lines
            .iter()
            .filter(|l| l.event_id.is_some() && !l.redacted)
            .filter(|l| nick.as_ref().is_none_or(|n| l.nick.to_lowercase() == *n))
            .filter(|l| needle.is_empty() || l.body.to_lowercase().contains(&needle))
            .map(|l| {
                let when = if l.ts.date_naive() == today { l.ts.format("%H:%M") } else { l.ts.format("%b %d %H:%M") };
                let who = match l.kind {
                    LineKind::Message | LineKind::Utd => format!("<{}> ", l.nick),
                    LineKind::Emote => format!("* {} ", l.nick),
                    LineKind::Notice => format!("-{}- ", l.nick),
                    _ => String::new(),
                };
                format!("{when} {who}{}", l.body)
            })
            .collect();
        let found = hits.len();
        if let Some(n) = limit {
            hits = hits.split_off(found.saturating_sub(n));
        }
        if found == 0 {
            self.info(format!("Lastlog: no matches in {loaded} loaded lines (PgUp loads older history)"));
            return Ok(());
        }
        self.info(format!("Lastlog: {found} match(es) in {loaded} loaded lines"));
        for hit in hits {
            self.info(hit);
        }
        self.info("End of Lastlog");
        Ok(())
    }

    /// Links in this window, newest first, without repeats.
    fn window_urls(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for line in self.win().lines.iter().rev().filter(|l| l.event_id.is_some() && !l.redacted) {
            for url in links::urls(&line.body).into_iter().rev() {
                if seen.insert(url.to_owned()) {
                    out.push((url.to_owned(), line.nick.clone()));
                }
            }
        }
        out
    }

    fn cmd_urls(&mut self) {
        let urls = self.window_urls();
        if urls.is_empty() {
            return self.info("no links in this window's loaded history");
        }
        let rows: Vec<String> =
            urls.iter().take(20).enumerate().map(|(i, (url, nick))| format!("{:>2}: {url}  <{nick}>", i + 1)).collect();
        for row in rows {
            self.info(row);
        }
        self.info("/open N opens one in your browser");
    }

    fn cmd_open(&mut self, args: &str) -> Result<(), String> {
        let n: usize = match args.trim() {
            "" => 1,
            n => n.parse().map_err(|_| "usage: /open [N] (numbers from /urls)")?,
        };
        let urls = self.window_urls();
        let (url, _) = urls.get(n.saturating_sub(1)).ok_or_else(|| format!("no link {n} here; see /urls"))?;
        self.open_url(url.clone());
        Ok(())
    }

    fn open_url(&mut self, url: String) {
        match links::open(&url) {
            Ok(()) => self.info(format!("opening {url}")),
            Err(e) => self.error(format!("couldn't open {url}: {e}")),
        }
    }

    fn cmd_search(&mut self, args: &str) -> Result<(), String> {
        let (deep, needle) = match args.trim().strip_prefix("-deep") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        if needle.is_empty() {
            return Err("usage: /search [-deep] <text>".into());
        }
        let needle = needle.to_lowercase();
        self.search_hits.clear();
        self.search_out = self.win().room.clone();

        if deep {
            let rooms: Vec<(Account, OwnedRoomId)> = self
                .windows
                .iter()
                .filter_map(|w| {
                    let key = w.room.as_ref()?;
                    Some((self.connected(&key.account)?, key.room_id.clone()))
                })
                .collect();
            if rooms.is_empty() {
                return Err("no connected rooms to search".into());
            }
            self.search_pending = rooms.len();
            self.info(format!("searching the last 500 messages of {} room(s) for \"{needle}\"…", rooms.len()));
            for (acct, room_id) in rooms {
                acct.search_history(room_id, needle.clone(), 5);
            }
            return Ok(());
        }

        // Newest first across every window's loaded history.
        let mut found: Vec<(WinKey, String, Line)> = self
            .windows
            .iter()
            .filter_map(|w| Some((w.room.clone()?, w)))
            .flat_map(|(key, w)| {
                w.lines
                    .iter()
                    .filter(|l| l.event_id.is_some() && l.is_message() && !l.redacted)
                    .filter(|l| l.text().to_lowercase().contains(&needle))
                    .map(move |l| (key.clone(), w.title().to_owned(), l.clone()))
            })
            .collect();
        found.sort_by_key(|(_, _, line)| std::cmp::Reverse(line.ts));
        found.truncate(50);
        if found.is_empty() {
            self.info(format!("no loaded messages match \"{needle}\"; try /search -deep"));
            return Ok(());
        }
        let rows: Vec<String> = found
            .into_iter()
            .enumerate()
            .map(|(i, (key, room, line))| {
                self.search_hits.push((key, line.event_id.clone().expect("filtered")));
                search_row(i + 1, &room, &line)
            })
            .collect();
        for row in rows {
            self.info(row);
        }
        self.info("/jump N goes to a result");
        Ok(())
    }

    fn cmd_jump(&mut self, args: &str) -> Result<(), String> {
        let n: usize = args.trim().parse().map_err(|_| "usage: /jump <N> (numbers from /search)")?;
        let (key, event) = self.search_hits.get(n.wrapping_sub(1)).cloned().ok_or_else(|| format!("no result {n}"))?;
        self.jump_to(key, event)
    }

    /// Go to a message: its room's window (reopened if closed), selected, or
    /// older history loads until it's there.
    fn jump_to(&mut self, key: WinKey, event: OwnedEventId) -> Result<(), String> {
        let idx = match self.window_idx(&key.account, &key.room_id) {
            Some(idx) => idx,
            None => self.reopen(&key.account, &key.room_id).ok_or("that room isn't available")?,
        };
        self.switch_to(idx);
        if self.win().contains(&event) {
            self.select = Some(event);
        } else {
            self.win_mut().seek = Some(event);
            self.info("loading older history to reach that message…");
        }
        Ok(())
    }

    fn cmd_away(&mut self, args: &str) -> Result<(), String> {
        let (all, message) = match args.trim().strip_prefix("-all") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        let message = Some(message).filter(|m| !m.is_empty()).map(str::to_owned);
        if all {
            let accounts: Vec<Account> = self.accounts.values().filter_map(|s| s.account.clone()).collect();
            if accounts.is_empty() {
                return Err("no server is connected".into());
            }
            accounts.iter().for_each(|acct| acct.set_away(message.clone()));
        } else {
            self.pick_account("")?.0.set_away(message);
        }
        Ok(())
    }

    /// Open (or switch to) the window for the thread starting at `root` in
    /// the current room.
    fn open_thread(&mut self, root: OwnedEventId) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let account = acct.name.to_string();
        if let Some(idx) = self.thread_idx(&account, &room_id, &root) {
            self.switch_to(idx);
            return Ok(());
        }
        let key = WinKey { account: account.clone(), room_id: room_id.clone(), thread: Some(root.clone()) };
        let mut w = Window::new(Some(key), thread_name(""));
        if let Some(room) = self.window_idx(&account, &room_id).map(|i| &self.windows[i]) {
            (w.encrypted, w.is_dm, w.members, w.perms) = (room.encrypted, room.is_dm, room.members.clone(), room.perms);
            w.topic = Some(format!("Thread in {}", room.name));
            if let Some(first) = room.lines.iter().find(|l| l.event_id.as_ref() == Some(&root)) {
                w.name = thread_name(&first.text());
            }
        }
        // History comes from the thread's relations, not room pagination.
        w.members_requested = true;
        w.history = History::Loading;
        let idx = self.insert_window(w);
        self.switch_to(idx);
        acct.load_thread(room_id, root);
        Ok(())
    }

    fn cmd_thread(&mut self, args: &str) -> Result<(), String> {
        match args.trim() {
            "" => {
                if self.win().thread_root().is_some() {
                    return Err("you're in a thread; /threads lists the room's others".into());
                }
                let root = self
                    .win()
                    .lines
                    .iter()
                    .rev()
                    .find_map(|l| l.thread_root.clone().or_else(|| l.thread_count.and(l.event_id.clone())))
                    .ok_or("no threads in the loaded history; /threads asks the server")?;
                self.open_thread(root)
            }
            n => {
                let n: usize = n.parse().map_err(|_| "usage: /thread [N] (numbers from /threads)")?;
                let (key, root) =
                    self.thread_hits.get(n.wrapping_sub(1)).cloned().ok_or_else(|| format!("no thread {n}"))?;
                let idx = self.window_idx(&key.account, &key.room_id).ok_or("that room's window is closed")?;
                self.switch_to(idx);
                self.open_thread(root)
            }
        }
    }

    fn cmd_joinrule(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (rule, rest) = split_first(args);
        let rule = match rule {
            "invite" => JoinRule::Invite,
            "public" => JoinRule::Public,
            "knock" => JoinRule::Knock,
            "restricted" => {
                let (_, space) = self.find_space(rest.trim())?;
                JoinRule::Restricted(Restricted::new(vec![AllowRule::room_membership(space.room_id)]))
            }
            _ => return Err("usage: /joinrule <invite|public|knock|restricted <space>>".into()),
        };
        acct.set_join_rule(room_id, rule);
        Ok(())
    }

    fn cmd_create(&mut self, args: &str) -> Result<(), String> {
        let (acct, mut rest) = self.pick_account(args)?;
        let (mut public, mut space) = (false, false);
        loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("-public") {
                (public, rest) = (true, r);
            } else if let Some(r) = rest.strip_prefix("-space") {
                (space, rest) = (true, r);
            } else {
                break;
            }
        }
        let name = rest.trim();
        if name.is_empty() {
            return Err("usage: /create [-public] [-space] <name>".into());
        }
        acct.create_room(name.to_owned(), public, space);
        Ok(())
    }

    /// The space a name refers to on the current window's server.
    fn find_space(&self, name: &str) -> Result<(Account, SpaceInfo), String> {
        let (acct, _) = self.pick_account("")?;
        let wanted = name.to_lowercase();
        let spaces = self.spaces.get(&acct.name).map(Vec::as_slice).unwrap_or_default();
        let space = spaces
            .iter()
            .find(|s| s.name.to_lowercase() == wanted)
            .or_else(|| spaces.iter().find(|s| s.name.to_lowercase().contains(&wanted)))
            .cloned()
            .ok_or_else(|| format!("no space matching {name}; /spaces lists them"))?;
        Ok((acct, space))
    }

    fn cmd_spaces(&mut self) -> Result<(), String> {
        let (acct, _) = self.pick_account("")?;
        let spaces = self.spaces.get(&acct.name).cloned().unwrap_or_default();
        if spaces.is_empty() {
            self.info("you're not in any spaces; /create -space <name> makes one");
            return Ok(());
        }
        for space in spaces {
            self.info(format!("◆ {} ({} room(s))", space.name, space.children.len()));
            for child in &space.children {
                let row = match self.window_idx(&acct.name, child) {
                    Some(idx) => format!("   {:>2}: {}", idx + 1, self.windows[idx].title()),
                    None => match acct.quick_meta(child) {
                        Some(meta) => format!("       {} (window closed)", meta.name),
                        None => format!("       {child} (not joined)"),
                    },
                };
                self.info(row);
            }
        }
        self.info("/space <name> browses one, including rooms you haven't joined");
        Ok(())
    }

    fn cmd_space(&mut self, args: &str) -> Result<(), String> {
        let args = args.trim();
        let (name, action) = match args.rsplit_once(' ') {
            Some((name, action @ ("add" | "remove" | "leave"))) => (name.trim(), Some(action)),
            _ => (args, None),
        };
        if name.is_empty() {
            return Err("usage: /space <name> [add|remove|leave]".into());
        }
        let (acct, space) = self.find_space(name)?;
        match action {
            None => acct.space_hierarchy(self.here(), space.room_id),
            Some("leave") => acct.forget(space.room_id),
            Some(action) => {
                let (_, room_id) = self.room_ctx()?;
                acct.space_child(self.here(), space.room_id, room_id, action == "add");
            }
        }
        Ok(())
    }

    fn cmd_topic(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if args.trim().is_empty() {
            let text = match &self.win().topic {
                Some(topic) => format!("Topic for {}: {topic}", self.win().title()),
                None => format!("No topic set for {}", self.win().title()),
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
                        let real = if w.local_name.is_some() { format!(" ({})", w.name) } else { String::new() };
                        format!("{:>3}: {}{real}{server}{enc}", i + 1, w.title())
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
                if self.win().is_mentions {
                    self.mention_origins.clear();
                    self.remove_window(self.active);
                    return Ok(());
                }
                let Some(key) = self.win().room.clone() else {
                    return Err("the status window can't be closed".into());
                };
                if key.thread.is_none() && !self.layout.closed.contains(&key) {
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
            "name" => {
                let Some(key) = self.win().room.clone() else {
                    return Err("only room and conversation windows can be renamed".into());
                };
                let name: String = rest.trim().chars().filter(|c| !c.is_control()).take(64).collect();
                self.layout.names.retain(|(k, _)| *k != key);
                let text = if name.is_empty() {
                    self.win_mut().local_name = None;
                    format!("back to the room's own name, {}", self.win().name)
                } else {
                    self.layout.names.push((key, name.clone()));
                    let text = format!(
                        "{} is now {name} here (only you see this; /window name puts it back)",
                        self.win().name
                    );
                    self.win_mut().local_name = Some(name);
                    text
                };
                self.save_layout();
                self.info(text);
            }
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

    fn find_window(&self, name: &str) -> Result<usize, String> {
        if let Ok(n) = name.parse::<usize>() {
            return (1..=self.windows.len()).contains(&n).then(|| n - 1).ok_or_else(|| format!("no window {n}"));
        }
        let wanted = name.to_lowercase();
        self.windows
            .iter()
            .position(|w| w.names().any(|n| n.to_lowercase() == wanted))
            .or_else(|| self.windows.iter().position(|w| w.names().any(|n| n.to_lowercase().contains(&wanted))))
            .ok_or_else(|| format!("no window matching {name}"))
    }

    /// `/split` and `/vsplit`: show another window in a new pane. Focus
    /// stays where you are typing; F6 moves it. With no window, an existing
    /// split just changes direction.
    fn cmd_devices(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (sub, rest) = split_first(args.trim());
        match sub {
            "" | "list" => acct.list_devices(self.here()),
            "rename" => {
                let (id, name) = split_first(rest);
                if id.is_empty() || name.trim().is_empty() {
                    return Err("usage: /devices rename <id> <name>".into());
                }
                acct.rename_device(self.here(), id.into(), name.trim().to_owned());
            }
            "delete" | "remove" | "signout" => {
                let (id, confirm) = split_first(rest);
                if id.is_empty() {
                    return Err("usage: /devices delete <id> (IDs from /devices)".into());
                }
                acct.delete_device(self.here(), id.into(), confirm.trim() == "confirm");
            }
            _ => return Err("usage: /devices [rename <id> <name> | delete <id>]".into()),
        }
        Ok(())
    }

    /// Clear activity and unread counts, move the "new messages" divider to
    /// the end, and tell the server (read marker, plus a receipt as set by
    /// `read_receipts`).
    fn cmd_markread(&mut self, args: &str) -> Result<(), String> {
        let all = match args.trim() {
            "" => false,
            "all" => true,
            _ => return Err("usage: /markread [all]".into()),
        };
        let targets: Vec<usize> = if all { (0..self.windows.len()).collect() } else { vec![self.active] };
        let mode = self.cfg.read_receipts;
        let mut rooms = 0;
        for idx in targets {
            let w = &mut self.windows[idx];
            w.activity = Activity::None;
            let latest = w.latest_event();
            if let Some(latest) = &latest {
                w.marker = Some(latest.clone());
            }
            // Thread windows share their room's read marker.
            let Some(key) = w.room.clone().filter(|k| k.thread.is_none()) else { continue };
            w.unread = (0, 0);
            w.last_receipt = latest.clone();
            if let Some(acct) = self.connected(&key.account) {
                acct.mark_room_read(key.room_id, latest, mode);
                rooms += 1;
            }
        }
        if all {
            self.info(format!("marked {rooms} room(s) read"));
        }
        Ok(())
    }

    fn cmd_theme(&mut self, args: &str) -> Result<(), String> {
        let name = match args.trim() {
            "" => {
                let custom = if self.cfg.colors.is_empty() { "" } else { ", with your [colors] changes" };
                self.info(format!(
                    "theme: {}{custom}; /theme default|light|mono switches",
                    format!("{:?}", self.cfg.theme).to_lowercase()
                ));
                return Ok(());
            }
            "default" | "dark" => ThemeName::Default,
            "light" => ThemeName::Light,
            "mono" => ThemeName::Mono,
            "phosphor" => ThemeName::Phosphor,
            other => return Err(format!("no theme {other}; there's default, light and mono")),
        };
        self.cfg.palette = Theme::build(name, &self.cfg.colors).map_err(|e| format!("{e:#}"))?;
        self.cfg.theme = name;
        self.redraw_all = true;
        Ok(())
    }

    /// Show `line`'s image full screen; the original is fetched for it.
    fn open_viewer(&mut self, line: &Line) -> Result<(), String> {
        let media = line.media.clone().filter(|m| m.is_image && !line.redacted).ok_or("that message has no image")?;
        let event = line.event_id.clone().ok_or("that image isn't sent yet")?;
        if self.picker.is_none() {
            return Err("images are off (image_previews = false in config.toml)".into());
        }
        let key = self.win().room.clone().or_else(|| self.mention_origins.get(&event).cloned());
        let acct = key.and_then(|k| self.connected(&k.account)).ok_or("that image's server isn't connected")?;
        acct.fetch_full_image(event.clone(), media.clone());
        self.viewer = Some(Viewer::new(event, format!("{} · {}", line.nick, media.filename)));
        Ok(())
    }

    fn close_viewer(&mut self) {
        self.viewer = None;
        // Terminal graphics can outlive the frame they were drawn in.
        self.redraw_all = true;
    }

    /// Open the switcher to choose where `line` goes.
    fn start_forward(&mut self, line: &Line) -> Result<(), String> {
        let event = line.event_id.clone().ok_or("that message isn't sent yet")?;
        // From the mentions window, the message's own room.
        let key = self.win().room.clone().or_else(|| self.mention_origins.get(&event).cloned());
        let key = key.ok_or("open the message's room to forward it")?;
        let edited = line.edited.then(|| line.body.clone());
        let purpose = switcher::Purpose::Forward { account: key.account, room_id: key.room_id, event, edited };
        self.switcher = Some(Switcher { purpose, ..Switcher::default() });
        Ok(())
    }

    /// `/forward [room]`: the latest message here, to the named room or one
    /// picked in the switcher.
    fn cmd_forward(&mut self, args: &str) -> Result<(), String> {
        let line = self.win().lines.iter().rev().find(|l| is_target(l)).cloned().ok_or("no message here to forward")?;
        self.start_forward(&line)?;
        if !args.trim().is_empty() {
            let sw = self.switcher.as_mut().expect("just opened");
            sw.query = args.trim().to_owned();
            if self.switcher_entries().is_empty() {
                self.switcher = None;
                return Err(format!("no room matching {} to forward to", args.trim()));
            }
            self.switcher_go(None);
        }
        Ok(())
    }

    fn cmd_keys(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (sub, file) = split_first(args.trim());
        match (sub, file.trim()) {
            ("export", "") => {
                let dir = dirs::download_dir().or_else(dirs::home_dir).ok_or("no Downloads folder; give a file")?;
                let name = format!("textrix-keys-{}-{}.txt", acct.name, Local::now().format("%Y-%m-%d"));
                acct.export_keys(self.here(), dir.join(name));
            }
            ("export", file) => acct.export_keys(self.here(), parse_path(file)),
            ("import", "") => return Err("usage: /keys import <file>".into()),
            ("import", file) => acct.import_keys(self.here(), parse_path(file)),
            _ => return Err("usage: /keys export [file] | /keys import <file>".into()),
        }
        Ok(())
    }

    fn cmd_notify(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if self.win().thread_root().is_some() {
            return Err("notifications are set for the whole room; use /notify in its window".into());
        }
        let mode = match args.trim() {
            "" => {
                let what = matrix::describe_notify(self.room_mode(self.active), self.win().is_dm);
                self.info(format!("notifications in {}: {what}", self.win().title()));
                return Ok(());
            }
            "all" => Some(RoomNotificationMode::AllMessages),
            "mentions" => Some(RoomNotificationMode::MentionsAndKeywordsOnly),
            "mute" | "off" => Some(RoomNotificationMode::Mute),
            "default" => None,
            _ => return Err("usage: /notify [all|mentions|mute|default]".into()),
        };
        acct.set_notify_mode(self.here(), room_id, mode);
        Ok(())
    }

    fn cmd_split(&mut self, args: &str, side_by_side: bool) -> Result<(), String> {
        let target = args.trim();
        if target.is_empty() {
            let Some(split) = &mut self.split else {
                let name = if side_by_side { "vsplit" } else { "split" };
                return Err(format!("usage: /{name} <window number or name>"));
            };
            split.side_by_side = side_by_side;
            return Ok(());
        }
        let idx = self.find_window(target)?;
        if self.on_screen(idx) {
            return Err(format!("{} is already on screen", self.windows[idx].title()));
        }
        let current = self.windows[self.active].id();
        let split = self.split.get_or_insert_with(|| Split { panes: vec![current.clone()], side_by_side });
        if split.panes.len() >= PANES_MAX {
            return Err(format!("{PANES_MAX} panes at most; /unsplit closes this one"));
        }
        split.side_by_side = side_by_side;
        let slot = split.panes.iter().position(|k| *k == current).map_or(split.panes.len(), |i| i + 1);
        split.panes.insert(slot, self.windows[idx].id());
        self.windows[idx].activity = Activity::None;
        Ok(())
    }

    fn cmd_unsplit(&mut self, args: &str) -> Result<(), String> {
        let Some(split) = &mut self.split else { return Err("nothing is split; /split <window> adds a pane".into()) };
        if args.trim() == "all" {
            self.split = None;
            return Ok(());
        }
        let current = self.windows[self.active].id();
        let slot = split.panes.iter().position(|k| *k == current).unwrap_or(0);
        split.panes.remove(slot);
        let next = split.panes[slot.min(split.panes.len() - 1)].clone();
        if split.panes.len() < 2 {
            self.split = None;
        }
        if let Some(idx) = self.windows.iter().position(|w| w.id() == next) {
            self.switch_to(idx);
        }
        Ok(())
    }

    fn goto_name(&mut self, name: &str) -> Result<(), String> {
        let wanted = name.to_lowercase();
        let idx = self
            .windows
            .iter()
            .position(|w| w.names().any(|n| n.to_lowercase() == wanted))
            .or_else(|| self.windows.iter().position(|w| w.names().any(|n| n.to_lowercase().contains(&wanted))))
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

    /// Start or stop chat logging to match the config.
    fn apply_logging(&mut self) {
        let dir = self.paths.log_dir(self.cfg.log_dir.as_deref());
        match (self.cfg.log, &self.log) {
            (true, Some(log)) if log.dir() == dir => {}
            (true, _) => {
                self.status(Line::info(format!("logging messages to {} (decrypted, plaintext)", dir.display())));
                self.log = Some(ChatLog::new(dir));
            }
            (false, Some(_)) => {
                self.log = None;
                self.status(Line::info("chat logging off"));
            }
            (false, None) => {}
        }
    }

    /// `/reload`: apply config.toml and the dotfile without restarting. A
    /// broken file leaves the current settings in place.
    fn cmd_reload(&mut self) {
        match Config::load(&self.paths.config_file()) {
            Ok(cfg) => {
                if cfg.image_previews && self.picker.is_none() {
                    self.info("image previews start after a restart (graphics support is checked at startup)");
                }
                if !cfg.image_previews {
                    self.picker = None;
                    self.previews.clear();
                }
                self.notify_mode = notify::resolve(cfg.notifications);
                (self.show_sidebar, self.show_nicklist) = (cfg.sidebar, cfg.nicklist);
                (self.bell_mode, self.bell_style) = (cfg.bell, cfg.bell_style);
                if cfg.mouse != self.cfg.mouse {
                    set_mouse_capture(cfg.mouse);
                }
                self.cfg = cfg;
                self.apply_logging();
                self.info(format!("reloaded {}", self.paths.config_file().display()));
            }
            Err(e) => self.error(format!("{e:#}; keeping the current settings")),
        }

        let servers = match self.env.servers() {
            Ok(servers) => servers,
            Err(e) => return self.error(format!("{e:#}; keeping the current servers")),
        };
        for warning in servers.warnings {
            self.status(Line::error(warning));
        }
        let names: HashSet<&str> = servers.defs.iter().map(|d| d.name.as_str()).collect();
        let removed: Vec<AccountName> = self.accounts.keys().filter(|n| !names.contains(&***n)).cloned().collect();
        for name in removed {
            if let Some(acct) = self.accounts.remove(&name).and_then(|s| s.account) {
                acct.stop();
            }
            while let Some(idx) = self.windows.iter().position(|w| w.account() == Some(&*name)) {
                self.remove_window(idx);
            }
            self.status(Line::info(format!("{name}: no longer in the dotfile; disconnected")));
        }
        for def in servers.defs {
            match self.accounts.get_mut(def.name.as_str()) {
                Some(state) => {
                    if state.def.user != def.user || state.def.homeserver != def.homeserver {
                        let name = def.name.clone();
                        (state.user, state.def) = (def.user.clone(), def);
                        self.status(Line::info(format!(
                            "{name}: account details changed; /disconnect {name} and /connect {name} to use them"
                        )));
                    }
                }
                None => {
                    let name = AccountName::from(def.name.as_str());
                    let state = AccountState {
                        user: def.user.clone(),
                        def,
                        nick: None,
                        status: ConnStatus::Offline,
                        verified: None,
                        away: None,
                        account: None,
                    };
                    self.accounts.insert(name.clone(), state);
                    self.status(Line::info(format!("{name}: new in the dotfile")));
                    self.connect(&name);
                }
            }
        }
    }

    fn cmd_connect(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().to_lowercase();
        let state = self.accounts.get(name.as_str()).ok_or_else(|| {
            if name.is_empty() {
                "usage: /connect <server>".into()
            } else {
                format!("no server named {name} in the dotfile")
            }
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
            self.info("Commands (Tab completes; /help <command> for details; F1 for a searchable list):");
            let rows: Vec<String> = COMMANDS.iter().map(|c| format!("  /{:<10} {}", c.name, c.help)).collect();
            for row in rows {
                self.info(row);
            }
            for (title, keys) in
                [("Keys", KEYS), ("Selecting messages (Alt+Up)", SELECT_KEYS), ("Editing (Shift+Up)", EDIT_KEYS)]
            {
                self.info(format!("{title}:"));
                for (key, what) in keys {
                    self.info(format!("  {key:<34} {what}"));
                }
            }
            self.info("F1 shows all of this full-screen, with a filter");
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

fn on_off(args: &str, usage: &str) -> Result<bool, String> {
    match args.trim() {
        "on" | "yes" | "true" => Ok(true),
        "off" | "no" | "false" => Ok(false),
        _ => Err(usage.to_owned()),
    }
}

/// Split off the first whitespace-separated word.
/// irssi-style alias expansion: `$0`, `$1`… are the arguments, `$1-` the
/// first onwards, `$*` all of them, `$$` a dollar sign; with none of those,
/// the arguments go on the end. `;` separates commands (`\;` is a literal
/// one). Returns the commands without their leading `/`.
fn expand_alias(template: &str, args: &str) -> Vec<String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut out = String::new();
    let mut used_args = false;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&';') {
            out.push('\u{0}');
            chars.next();
            continue;
        }
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('*') => {
                chars.next();
                out.push_str(args.trim());
                used_args = true;
            }
            Some(d) if d.is_ascii_digit() => {
                chars.next();
                let n = d.to_digit(10).unwrap_or(0) as usize;
                if chars.peek() == Some(&'-') {
                    chars.next();
                    out.push_str(&words.get(n..).unwrap_or_default().join(" "));
                } else {
                    out.push_str(words.get(n).copied().unwrap_or_default());
                }
                used_args = true;
            }
            _ => out.push('$'),
        }
    }
    if !used_args && !args.trim().is_empty() {
        out.push(' ');
        out.push_str(args.trim());
    }
    out.split(';')
        .map(|cmd| cmd.replace('\u{0}', ";").trim().trim_start_matches('/').to_owned())
        .filter(|cmd| !cmd.is_empty())
        .collect()
}

/// Someone else's message that highlights you (a mention or highlight
/// word, per your push rules).
fn is_mention(line: &Line) -> bool {
    line.highlight && !line.own && line.is_message()
}

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
    let unquoted =
        ['"', '\''].iter().find_map(|q| arg.strip_prefix(*q).and_then(|a| a.strip_suffix(*q))).map(str::to_owned);
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
            Some(Candidate { text, user: None, hint: None })
        })
        .collect();
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

/// The main loop: draw, then wait for a key, a Matrix event, or the clock.
/// Turn the terminal's mouse reporting on or off.
pub fn set_mouse_capture(on: bool) {
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    let mut out = std::io::stdout();
    let result =
        if on { crossterm::execute!(out, EnableMouseCapture) } else { crossterm::execute!(out, DisableMouseCapture) };
    if let Err(e) = result {
        tracing::warn!("mouse capture: {e}");
    }
}

async fn flash_end(until: Option<Instant>) {
    match until {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

pub async fn run(terminal: &mut DefaultTerminal, mut app: App, mut rx: UnboundedReceiver<AppEvent>) -> Result<()> {
    let mut keys = EventStream::new();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    app.start();
    loop {
        if std::mem::take(&mut app.redraw_all) {
            terminal.clear()?;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        let title = app.terminal_title();
        if title != app.last_title {
            match &title {
                Some(title) => notify::set_title(title),
                // Turned off (by /reload): back to the terminal's own title.
                None => {
                    notify::pop_title();
                    notify::push_title();
                }
            }
            app.last_title = title;
        }
        if std::mem::take(&mut app.bell) {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07").and_then(|()| out.flush());
        }
        for (title, body) in std::mem::take(&mut app.notifications) {
            notify::send(app.notify_mode, &title, &body);
        }
        if let Some(text) = app.clipboard.take() {
            notify::copy_to_clipboard(&text);
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
            // Redraw when the visual bell's flash ends.
            _ = flash_end(app.flash_until) => app.flash_until = None,
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
            is_image: false,
            thumbnail: None,
            size: None,
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

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let paths = Paths { base: std::env::temp_dir(), env_file: std::env::temp_dir().join("none") };
        App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx)
    }

    fn room_window(app: &mut App, room: &str, name: &str) -> usize {
        let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
        app.windows.push(Window::new(Some(key), name.into()));
        app.windows.len() - 1
    }

    #[test]
    fn clicks_find_wrapped_links_and_select_messages() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        app.switch_to(a);
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        let mut line = msg("$1", &format!("see {url}."));
        line.sender = Some("@alice:x.org".try_into().unwrap());
        app.windows[a].lines.push_back(line);
        let event: OwnedEventId = "$1".try_into().unwrap();
        // The second half of a link wrapped onto the next row still opens it.
        assert_eq!(app.link_in(&event, "wiki/Rust_(programming_language)").as_deref(), Some(url));
        assert_eq!(app.link_in(&event, "(https://en.wikipedia"), Some(url.to_owned()));
        assert_eq!(app.link_in(&event, "alice"), None, "underlined text that isn't a link");

        let row = ui::RowHit { event: Some(event.clone()), links: Vec::new(), image: false };
        let area = ratatui::layout::Rect::new(0, 0, 40, 5);
        app.hits.panes.push(ui::PaneHits { area, window: a, rows: vec![ui::RowHit::default(), row] });
        app.on_click(Position::new(10, 1));
        assert_eq!(app.select.as_ref(), Some(&event), "a click selects the message");
        app.on_click(Position::new(10, 1));
        assert_eq!(app.select, None, "clicking it again unselects");
        app.on_click(Position::new(10, 1));
        app.on_click(Position::new(10, 0));
        assert_eq!(app.select, None, "an empty row unselects");

        app.on_wheel(Position::new(10, 2), true);
        assert_eq!(app.windows[a].scroll, WHEEL_ROWS);
        app.on_wheel(Position::new(10, 2), false);
        assert_eq!(app.windows[a].scroll, 0);
    }

    #[test]
    fn notification_modes_quiet_or_raise_alerts() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
        let incoming = |id: &str| {
            let mut line = msg(id, "hello");
            line.sender = Some("@bob:x.org".try_into().unwrap());
            vec![Item::Line(line)]
        };
        let set_mode = |app: &mut App, mode| {
            app.push_modes.insert("t".into(), HashMap::from([(room.clone(), mode)]));
            (app.windows[a].activity, app.bell) = (Activity::None, false);
        };

        app.on_timeline("t", &room, incoming("$1"), true);
        assert_eq!(app.windows[a].activity, Activity::Message);
        assert!(!app.bell, "ordinary messages don't alert by default");

        set_mode(&mut app, RoomNotificationMode::MentionsAndKeywordsOnly);
        app.on_timeline("t", &room, incoming("$2"), true);
        assert_eq!(app.windows[a].activity, Activity::Event, "shown quietly");

        set_mode(&mut app, RoomNotificationMode::Mute);
        let mut mention = incoming("$3");
        if let Item::Line(line) = &mut mention[0] {
            line.highlight = true;
        }
        app.on_timeline("t", &room, mention, true);
        assert_eq!(app.windows[a].activity, Activity::Event);
        assert!(!app.bell, "muted rooms never alert");

        set_mode(&mut app, RoomNotificationMode::AllMessages);
        app.on_timeline("t", &room, incoming("$4"), true);
        assert_eq!(app.windows[a].activity, Activity::Message);
        assert!(app.bell, "every message alerts in a room set to all");
    }

    #[test]
    fn aliases_expand_like_irssi() {
        assert_eq!(expand_alias("/window goto Tetrix Tests", ""), ["window goto Tetrix Tests"]);
        assert_eq!(expand_alias("/msg @bob:x.org", "hi there"), ["msg @bob:x.org hi there"], "args appended");
        assert_eq!(expand_alias("/msg $0 says $1-", "@a:x b c d"), ["msg @a:x says b c d"]);
        assert_eq!(expand_alias("/me waves at $*, costs $$5", "bob"), ["me waves at bob, costs $5"]);
        assert_eq!(expand_alias("/markread all; /window 1", ""), ["markread all", "window 1"]);
        assert_eq!(expand_alias("/say a\\;b", ""), ["say a;b"]);
        assert_eq!(expand_alias("/say $3", "a b"), ["say"], "missing arguments are empty");
    }

    #[test]
    fn aliases_run_save_and_refuse_loops() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-alias-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        std::fs::write(app.paths.config_file(), "# my settings\nbell = false\n").unwrap();
        app.cmd_alias("tt /theme mono").unwrap();
        let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert!(saved.starts_with("# my settings\nbell = false\n"), "the rest of the file is kept: {saved}");
        assert!(saved.contains("[aliases]") && saved.contains("tt = \"/theme mono\""), "{saved}");
        assert!(app.cmd_alias("join /say no").is_err(), "built-ins can't be replaced");

        app.command("tt");
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Mono, "the alias ran");
        app.cfg.aliases.insert("loop".into(), "/loop".into());
        app.command("loop");
        assert!(app.windows[0].lines.iter().any(|l| l.body.contains("too deeply")));

        app.cmd_unalias("tt").unwrap();
        let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert_eq!(saved, "# my settings\nbell = false\n", "no empty [aliases] left behind");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mentions_collect_in_their_own_window_and_jump_back() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
        let mention = |id: &str, mins: i64| {
            let mut line = msg(id, "hey you");
            line.highlight = true;
            line.ts = Local::now() - chrono::Duration::minutes(mins);
            Item::Line(line)
        };
        app.on_timeline("t", &room, vec![Item::Line(msg("$plain", "hi"))], true);
        assert!(!app.windows.iter().any(|w| w.is_mentions), "no window until something mentions you");
        app.on_timeline("t", &room, vec![mention("$new", 0)], true);
        let m = app.windows.iter().position(|w| w.is_mentions).unwrap();
        assert_eq!(m, app.windows.len() - 1, "added at the end, so numbers don't shift");
        assert_eq!(app.windows[m].activity, Activity::Highlight);
        assert_eq!(ui::sidebar_entries(&app)[1], ui::SidebarEntry::Window(m), "listed under status");

        // Older mentions found in history slot in before it; repeats are ignored.
        let history = vec![mention("$old", 60), mention("$new", 0)];
        app.on_event(AppEvent::History {
            account: "t".into(),
            room_id: room.clone(),
            items: history,
            end: None,
            failed: false,
        });
        let ids: Vec<&str> =
            app.windows[m].lines.iter().filter_map(|l| l.event_id.as_ref().map(|e| e.as_str())).collect();
        assert_eq!(ids, ["$old", "$new"]);
        assert!(app.windows[m].lines.back().unwrap().nick.starts_with("alpha/"));

        app.switch_to(m);
        app.select = Some("$old".try_into().unwrap());
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, a, "Enter goes to the room");
        assert_eq!(app.select.as_ref().map(|e| e.as_str()), Some("$old"), "with the message selected");
    }

    #[test]
    fn link_titles_show_under_their_message() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        app.windows[a].lines.push_back(msg("$1", "docs at https://matrix.org/docs today"));
        app.switch_to(a);
        let draw = |app: &mut App| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| ui::draw(f, app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..buffer.area.height)
                .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        draw(&mut app);
        assert_eq!(app.link_title_requests, [("t".to_owned(), "https://matrix.org/docs".to_owned())], "asked once");
        app.on_event(AppEvent::LinkTitle {
            url: "https://matrix.org/docs".into(),
            title: Some("Docs — Matrix".into()),
        });
        assert!(draw(&mut app).contains("↳ Docs — Matrix"));

        app.windows[a].encrypted = true;
        assert!(!draw(&mut app).contains("↳ Docs"), "not in encrypted rooms unless link_titles = \"all\"");
    }

    #[test]
    fn writing_to_someone_away_says_so_once() {
        let mut app = test_app();
        let mikey: OwnedUserId = "@mikey:x.org".try_into().unwrap();
        let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
        let mut dm = Window::new(Some(key), "mikey".into());
        (dm.is_dm, dm.dm_user) = (true, Some(mikey.clone()));
        app.windows.push(dm);
        app.switch_to(1);
        let away = |status: &str| Presence { kind: PresenceKind::Away, status: Some(status.into()), last_active: None };
        let set = |app: &mut App, p: Presence| {
            app.on_event(AppEvent::Presence { account: "t".into(), updates: vec![(mikey.clone(), p)] })
        };
        let notes = |app: &App| app.windows[1].lines.iter().filter(|l| l.body.contains(" is ")).count();

        app.note_away(None);
        assert_eq!(notes(&app), 0, "nothing known, nothing said");
        set(&mut app, away("lunch"));
        app.note_away(None);
        app.note_away(None);
        assert_eq!(app.windows[1].lines.back().unwrap().body, "mikey is away: lunch");
        assert_eq!(notes(&app), 1, "once, not every message");
        set(&mut app, away("in a meeting"));
        app.note_away(None);
        assert_eq!(notes(&app), 2, "a new status is news");
        set(&mut app, Presence { kind: PresenceKind::Online, status: None, last_active: None });
        app.note_away(None);
        set(&mut app, away("in a meeting"));
        app.note_away(None);
        assert_eq!(notes(&app), 3, "away again after being back");
    }

    #[test]
    fn nicknames_show_only_here() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-nicknames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        let mikey: OwnedUserId = "@mikey:x.org".try_into().unwrap();
        let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
        let mut dm = Window::new(Some(key), "mikey".into());
        (dm.is_dm, dm.dm_user) = (true, Some(mikey.clone()));
        let mut line = msg("$1", "hi");
        (line.nick, line.sender) = ("mikey".into(), Some(mikey.clone()));
        dm.lines.push_back(line);
        app.windows.push(dm);
        app.switch_to(1);

        app.cmd_nickname("@mikey:x.org Mike").unwrap();
        assert_eq!(app.windows[1].title(), "Mike", "the DM is named after them");
        assert_eq!(app.display_name(Some(&mikey), "mikey"), "Mike");
        let drawn = ui::render_line_with_names(&app.windows[1].lines[0], 80, &app.cfg, app.nicknames());
        assert!(drawn.contains("<Mike>"), "{drawn}");

        app.input.set_text("Mi");
        let candidates = app.candidates();
        assert_eq!(candidates[0].text, "mikey: ", "completion inserts their real name");
        assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Mike"));

        app.cmd_nickname("@mikey:x.org").unwrap();
        assert_eq!(app.windows[1].title(), "mikey");
        assert!(app.cmd_nickname("@mikey:x.org").is_err(), "nothing to take off");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn easter_eggs() {
        let mut app = test_app();
        for code in eggs::KONAMI {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Phosphor);
        assert!(app.input.is_empty(), "the b doesn't stay in the input line");
        for code in eggs::KONAMI {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Default, "again turns it off");

        app.command("8ball will it ship");
        assert!(app.windows[0].lines.back().unwrap().body.contains("The 8-ball says:"));
        app.command("bofh");
        assert!(app.windows[0].lines.back().unwrap().body.starts_with("Today's excuse:"));
        app.command("slap");
        assert!(app.windows[0].lines.back().unwrap().body.contains("slap whom"));

        app.input.set_text("Dear team,");
        assert!(app.input_hint().unwrap().contains("writing a letter"));
        app.input.set_text("/slap bob");
        assert!(app.input_hint().is_none(), "no 'unknown command' spoiler");
        assert!(!COMMANDS.iter().any(|c| EGGS.contains(&c.name)), "not in /help");
    }

    #[test]
    fn windows_can_have_your_own_names() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        let a = room_window(&mut app, "!a:x.org", "Disaster Control");
        app.switch_to(a);
        app.cmd_window("name Ops").unwrap();
        assert_eq!(app.windows[a].title(), "Ops");
        assert_eq!(app.find_window("ops"), Ok(a));
        assert_eq!(app.find_window("disaster"), Ok(a), "the room's own name still finds it");
        assert_eq!(app.terminal_title().unwrap(), "Ops — Textrix");

        // Kept in the layout, so it comes back with the window.
        let key = app.windows[a].room.clone().unwrap();
        app.remove_window(a);
        let idx = app.insert_window(Window::new(Some(key), "Disaster Control".into()));
        assert_eq!(app.windows[idx].title(), "Ops");
        assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Ops"));

        app.switch_to(idx);
        app.cmd_window("name").unwrap();
        assert_eq!(app.windows[idx].title(), "Disaster Control", "back to the room's name");
        app.switch_to(0);
        assert!(app.cmd_window("name Home").is_err(), "the status window keeps its name");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bells_follow_the_settings_and_dont_pile_up() {
        let mut app = test_app();
        app.ring(BellKind::Mentions);
        assert!(app.bell && app.flash_until.is_none());
        app.bell = false;
        app.ring(BellKind::Invites);
        assert!(!app.bell, "a burst rings once");

        app.last_bell = None;
        app.cfg.bell_for = vec![BellKind::Mentions];
        app.ring(BellKind::Transfers);
        assert!(!app.bell, "not in bell_for");

        app.cmd_bell("away").unwrap();
        app.focused = true;
        app.ring(BellKind::Mentions);
        assert!(!app.bell, "away: quiet while you're looking");
        app.focused = false;
        app.ring(BellKind::Mentions);
        assert!(app.bell);

        (app.bell, app.last_bell) = (false, None);
        app.cmd_bell("visual").unwrap();
        app.ring(BellKind::Mentions);
        assert!(!app.bell && app.flash_until.is_some(), "flashes instead of beeping");
        assert!(app.cmd_bell("loud").is_err());
    }

    #[test]
    fn forwarding_offers_the_other_rooms() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        room_window(&mut app, "!b:x.org", "beta");
        app.switch_to(a);
        assert!(app.cmd_forward("").is_err(), "nothing to forward yet");
        app.windows[a].lines.push_back(msg("$1", "share me"));
        app.cmd_forward("").unwrap();
        let names: Vec<String> = app.switcher_entries().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["beta"], "not status, mentions or the room it came from");
        app.switcher = None;
        assert!(app.cmd_forward("zzz").is_err());
        assert!(app.switcher.is_none(), "a failed /forward leaves nothing open");
    }

    #[test]
    fn markread_clears_activity_and_moves_the_divider() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let b = room_window(&mut app, "!b:x.org", "beta");
        for idx in [a, b] {
            app.windows[idx].lines.push_back(msg(&format!("$x{idx}"), "hi"));
            (app.windows[idx].activity, app.windows[idx].unread) = (Activity::Message, (3, 0));
        }
        app.switch_to(a);
        app.cmd_markread("").unwrap();
        assert_eq!((app.windows[a].activity, app.windows[a].unread()), (Activity::None, 0));
        assert_eq!(app.windows[a].marker.as_ref().map(|e| e.as_str()), Some("$x1"));
        assert_eq!(app.windows[b].activity, Activity::Message, "only this room");
        app.cmd_markread("all").unwrap();
        assert_eq!((app.windows[b].activity, app.windows[b].unread()), (Activity::None, 0));
        assert!(app.cmd_markread("everything").is_err());
    }

    #[test]
    fn cached_history_is_dropped_when_the_sync_skipped_messages() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
        let cached = |id: &str| {
            let mut line = msg(id, "from last time");
            line.cached = true;
            Item::Line(line)
        };
        let timeline = |id: &str, limited| AppEvent::Timeline {
            account: "t".into(),
            room_id: room.clone(),
            items: vec![Item::Line(msg(id, "new"))],
            live: true,
            limited,
        };
        app.on_event(AppEvent::Cached {
            account: "t".into(),
            room_id: room.clone(),
            items: vec![cached("$1"), cached("$2")],
        });
        app.on_event(timeline("$3", false));
        assert_eq!(ids(&app.windows[a]), ["$1", "$2", "$3"], "a complete sync carries on from the cache");

        app.on_event(timeline("$9", true));
        assert_eq!(ids(&app.windows[a]), ["$3", "$9"], "a gap means the cached lines no longer join up");
    }

    #[test]
    fn long_pastes_ask_before_sending() {
        let mut app = test_app();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.on_terminal(Event::Paste("1\r\n2\n3\n4\n5\n6\n7".into()));
        app.on_key(enter);
        assert_eq!(app.confirm_paste, Some(7));
        app.on_key(key('n'));
        assert_eq!((app.confirm_paste, app.input.text().lines().count()), (None, 7), "kept to edit");
        app.on_key(enter);
        app.on_key(key('y'));
        assert!(app.confirm_paste.is_none() && app.input.is_empty(), "sent");

        // Lines you type yourself, and short pastes, just go.
        app.input.set_text("a\nb\nc\nd\ne\nf\ng");
        app.on_key(enter);
        assert!(app.confirm_paste.is_none() && app.input.is_empty());
        app.on_terminal(Event::Paste("one\ntwo".into()));
        app.on_key(enter);
        assert!(app.confirm_paste.is_none() && app.input.is_empty());
    }

    #[test]
    fn unsent_text_stays_with_its_window() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let b = room_window(&mut app, "!b:x.org", "beta");
        app.switch_to(a);
        app.input.set_text("half a thought");
        let key = app.windows[a].room.clone().unwrap();
        let event: OwnedEventId = "$1".try_into().unwrap();
        app.compose = Some((key, Compose::React { event, nick: "bob".into() }));

        app.switch_to(b);
        assert!(app.input.is_empty() && app.compose.is_none(), "the other window starts clean");
        assert!(app.windows[a].has_draft());
        app.input.set_text("for beta");
        app.switch_to(a);
        assert_eq!(app.input.text(), "half a thought");
        assert!(app.compose.is_some(), "the reaction being written comes back too");
        assert!(!app.windows[a].has_draft());
        app.switch_to(b);
        assert_eq!(app.input.text(), "for beta");
        app.input.clear();
        app.switch_to(a);
        assert!(!app.windows[b].has_draft(), "an empty line leaves nothing behind");
    }

    #[test]
    fn split_panes_follow_focus_and_window_changes() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "alpha");
        let b = room_window(&mut app, "!b:x.org", "beta");
        let c = room_window(&mut app, "!c:x.org", "gamma");
        app.switch_to(a);
        assert!(app.cmd_split("", false).is_err(), "nothing to change yet");
        assert!(app.cmd_split("alpha", false).is_err(), "already on screen");
        app.cmd_split("beta", false).unwrap();
        assert_eq!(app.screen_windows(), [a, b]);
        assert_eq!(app.active, a, "focus stays where you're typing");

        app.cycle_pane(1);
        assert_eq!(app.active, b);
        // Switching to a hidden window swaps it into the focused pane...
        app.switch_to(c);
        assert_eq!(app.screen_windows(), [a, c]);
        // ...and to a visible one just moves the focus.
        app.switch_to(a);
        assert_eq!((app.active, app.screen_windows()), (a, vec![a, c]));

        app.cmd_split("3", true).unwrap();
        assert_eq!(app.screen_windows(), [a, b, c]);
        assert!(app.split.as_ref().unwrap().side_by_side);

        // Closing a shown window drops its pane; panes track identity, not
        // position, so the others keep their windows.
        app.remove_window(b);
        let (a, c) = (a, c - 1);
        assert_eq!(app.screen_windows(), [a, c]);
        app.cmd_unsplit("").unwrap();
        assert!(app.split.is_none(), "one pane left means no split");
        assert_eq!(app.active, c);
        assert!(app.cmd_unsplit("").is_err());
    }

    #[test]
    fn input_hints_guide_command_typing() {
        let mut app = test_app();
        app.input.set_text("/re");
        let hint = app.input_hint().unwrap();
        for name in ["reply", "react", "redact", "recover"] {
            assert!(hint.contains(name), "{hint}");
        }
        app.input.set_text("/reply ");
        assert!(app.input_hint().unwrap().starts_with("[nick] <text>"));
        app.input.set_text("/wc");
        assert!(app.input_hint().unwrap().contains("Manage windows"));
        app.input.set_text("/reply hi there");
        assert!(app.input_hint().is_none(), "no hint once arguments are typed");
        app.input.set_text("hello");
        assert!(app.input_hint().is_none());
    }

    #[test]
    fn edit_mode_walks_your_own_messages_and_restores_the_draft() {
        let mut app = test_app();
        let key = WinKey { account: "t".into(), room_id: "!r:x.org".try_into().unwrap(), thread: None };
        app.windows.push(Window::new(Some(key), "room".into()));
        app.active = 1;
        let mine = |id: &str, body: &str| {
            let mut l = msg(id, body);
            l.own = true;
            l
        };
        let (first, second) = (mine("$1", "first of mine"), mine("$3", "second of mine"));
        app.win_mut().push(first, 100);
        app.win_mut().push(msg("$2", "someone else"), 100);
        app.win_mut().push(second.clone(), 100);
        app.input.set_text("half-typed draft");

        app.load_edit(&second, app.input.text());
        assert_eq!(app.input.text(), "second of mine");
        assert_eq!(app.compose_label().unwrap(), format!("edit {} 1/2", second.ts.format("%H:%M")));

        app.step_edit(true);
        assert_eq!(app.input.text(), "first of mine", "skips other people's messages");
        app.step_edit(false);
        assert_eq!(app.input.text(), "second of mine");
        app.step_edit(false);
        assert!(!app.is_editing(), "down past the newest leaves edit mode");
        assert_eq!(app.input.text(), "half-typed draft");
    }

    #[test]
    fn shift_or_alt_enter_starts_a_new_line() {
        let mut app = test_app();
        app.input.set_text("first");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.input.insert_str("second");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(app.input.text(), "first\nsecond\n");
    }

    #[test]
    fn search_spans_rooms_newest_first_and_jumps() {
        let mut app = test_app();
        for (room, id, body, mins_ago) in
            [("!a:x.org", "$old", "deploy the thing", 30), ("!b:x.org", "$new", "Deploy finished", 5)]
        {
            let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
            app.windows.push(Window::new(Some(key), room.into()));
            let mut line = msg(id, body);
            line.ts = Local::now() - chrono::TimeDelta::minutes(mins_ago);
            app.windows.last_mut().unwrap().push(line, 100);
            app.windows.last_mut().unwrap().push(msg(&format!("{id}x"), "unrelated"), 100);
        }
        app.cmd_search("deploy").unwrap();
        let hits: Vec<&str> = app.search_hits.iter().map(|(_, e)| e.as_str()).collect();
        assert_eq!(hits, vec!["$new", "$old"]);

        app.cmd_jump("2").unwrap();
        assert_eq!(app.active, 1, "switched to the first room");
        assert_eq!(app.select.as_ref().map(|e| e.as_str()), Some("$old"));
        assert!(app.cmd_jump("9").is_err());
    }

    #[test]
    fn window_list_groups_rooms_by_space_without_renumbering() {
        use crate::ui::{
            SidebarEntry::{Header, Window as Win},
            sidebar_entries,
        };
        let mut app = test_app();
        for (room, name) in [("!b:x.org", "loose room"), ("!a:x.org", "in space")] {
            let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
            app.windows.push(Window::new(Some(key), name.into()));
        }
        assert_eq!(sidebar_entries(&app), vec![Win(0), Win(1), Win(2)], "no spaces, no headers");

        let space = SpaceInfo {
            room_id: "!s:x.org".try_into().unwrap(),
            name: "Work".into(),
            children: vec!["!a:x.org".try_into().unwrap()],
        };
        app.spaces.insert("t".into(), vec![space]);
        assert_eq!(
            sidebar_entries(&app),
            vec![Win(0), Header("◆ Work".into()), Win(2), Header("◇ other".into()), Win(1)]
        );
    }

    #[test]
    fn thread_replies_reach_room_and_thread_windows() {
        let mut app = test_app();
        let room_id: OwnedRoomId = "!r:x.org".try_into().unwrap();
        let root: OwnedEventId = "$root".try_into().unwrap();
        let room_key = WinKey { account: "t".into(), room_id: room_id.clone(), thread: None };
        let thread_key = WinKey { thread: Some(root.clone()), ..room_key.clone() };
        app.windows.push(Window::new(Some(room_key), "room".into()));
        app.windows.push(Window::new(Some(thread_key), "⤷ thread".into()));
        app.windows[1].push(msg("$root", "shall we?"), 100);
        app.windows[2].push(msg("$root", "shall we?"), 100);

        let mut reply = msg("$r1", "yes");
        (reply.thread, reply.thread_root) = (true, Some(root.clone()));
        let other = msg("$x", "unrelated chatter");
        let react = Item::React { target: "$r1".try_into().unwrap(), key: "👍".into(), nick: "bob".into() };
        app.on_timeline("t", &room_id, vec![Item::Line(reply), Item::Line(other), react], true);

        let bodies = |w: &Window| w.lines.iter().map(|l| l.body.clone()).collect::<Vec<_>>();
        assert_eq!(bodies(&app.windows[1]), vec!["shall we?", "yes", "unrelated chatter"]);
        assert_eq!(bodies(&app.windows[2]), vec!["shall we?", "yes"], "only the thread's messages");
        assert_eq!(app.windows[1].lines[0].thread_count, Some(1), "root shows the new reply");
        assert_eq!(app.windows[2].lines[1].reactions.len(), 1, "reactions reach the thread window");
        assert_eq!(app.window_idx("t", &room_id), Some(1), "room lookups skip thread windows");
    }

    #[tokio::test]
    async fn reload_applies_config_and_dotfile_changes() {
        let dir = std::env::temp_dir().join(format!("textrix-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths { base: dir.clone(), env_file: dir.join(".env") };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file.clone()), Servers::default(), tx);
        assert!(app.show_sidebar);

        crate::servers::write_private(&paths.env_file, b"TEXTRIX_NEW_USER=@me:localhost\n").unwrap();
        std::fs::write(paths.config_file(), "sidebar = false\n").unwrap();
        app.cmd_reload();
        assert!(!app.show_sidebar, "config applied");
        assert!(app.accounts.contains_key("new"), "new dotfile server picked up");

        std::fs::write(paths.config_file(), "sidebar = maybe\n").unwrap();
        crate::servers::write_private(&paths.env_file, b"TEXTRIX_OTHER_USER=@me:localhost\n").unwrap();
        app.cmd_reload();
        assert!(!app.show_sidebar, "a broken config keeps the current settings");
        assert!(!app.accounts.contains_key("new") && app.accounts.contains_key("other"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn help_filter_keeps_matching_entries_and_their_sections() {
        let titles = |rows: &[HelpRow]| -> Vec<&str> {
            rows.iter().filter_map(|r| if let HelpRow::Section(t) = r { Some(*t) } else { None }).collect()
        };
        let all = help_rows("");
        assert_eq!(titles(&all), vec!["Keys", "Selecting messages (Alt+Up)", "Editing (Shift+Up)", "Commands"]);
        let upgrade = help_rows("upgrade");
        assert_eq!(titles(&upgrade), vec!["Commands"]);
        assert!(upgrade.iter().any(|r| matches!(r, HelpRow::Entry(k, _) if k.starts_with("/upgrade"))));
        let leave = help_rows("/leave");
        assert!(
            leave.iter().any(|r| matches!(r, HelpRow::Entry(k, d) if k.starts_with("/part") && d.contains("/leave")))
        );
        assert!(help_rows("zzzz-nothing").is_empty());
    }

    #[test]
    fn typing_clears_when_that_person_speaks() {
        let mut app = test_app();
        let room_id: OwnedRoomId = "!r:x.org".try_into().unwrap();
        app.windows.push(Window::new(
            Some(WinKey { account: "t".into(), room_id: room_id.clone(), thread: None }),
            "r".into(),
        ));
        let (alice, bob): (OwnedUserId, OwnedUserId) =
            ("@alice:x.org".try_into().unwrap(), "@bob:x.org".try_into().unwrap());
        app.on_event(AppEvent::Typing {
            account: "t".into(),
            room_id: room_id.clone(),
            nicks: vec![(alice.clone(), "alice".into()), (bob.clone(), "bob".into())],
        });
        assert_eq!(app.windows[1].typing.len(), 2);
        let mut spoke = msg("$1", "done typing");
        spoke.sender = Some(alice);
        app.on_timeline("t", &room_id, vec![Item::Line(spoke)], true);
        assert_eq!(app.windows[1].typing, vec![(bob, "bob".to_owned())], "alice spoke, so they aren't typing");
        assert!(on_off("on", "").unwrap() && !on_off("off", "").unwrap() && on_off("maybe", "usage").is_err());
    }

    #[test]
    fn split_first_word() {
        assert_eq!(split_first("  join  #a:b  "), ("join", "#a:b  "));
        assert_eq!(split_first("quit"), ("quit", ""));
    }
}

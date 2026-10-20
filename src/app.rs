//! Application state: windows, accounts, keys and slash commands.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures_util::{Stream, StreamExt};
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
    Terminal,
    backend::Backend,
    layout::{Position, Rect, Size},
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    audio,
    chatlog::ChatLog,
    compose::{Say, compose},
    config::{BellKind, BellMode, BellStyle, Config, NotifyMode, Paths},
    convert::attachment,
    eggs,
    input::{Candidate, Draft, Input},
    links,
    matrix::{self, Account, AppEvent, Moderation, Tx, VerifyAnswer},
    model::{
        AccountName, Activity, Fold, Item, Line, LineKind, Media, MediaKind, Member, Permissions, Presence,
        PresenceKind, RoomMeta, SpaceInfo, ago,
    },
    notify,
    picture::{Painter, Picture},
    script::{self, Scripts},
    servers::{EnvFile, ServerDef, Servers},
    switcher::{self, Switcher},
    theme::{Theme, ThemeFile, ThemeId, ThemeName},
    ui, update,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Away or offline this long is news; less is likely a missed heartbeat.
const AWAY_NEWS: chrono::TimeDelta = chrono::TimeDelta::minutes(30);
/// Someone who spoke this recently is around, whatever their presence says.
const SPOKE_LATELY: chrono::TimeDelta = chrono::TimeDelta::minutes(15);
/// The same "is away" note at most this often.
const AWAY_REPEAT: Duration = Duration::from_secs(3600);
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

    /// `/reply sam hi` or `/react sam 👍`: the last message from a nick
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

/// What `/theme` does other than pick a theme, so no theme file can be
/// called these.
const THEME_WORDS: [&str; 3] = ["list", "save", "import"];

/// `/themes` (or `/theme` on its own): every theme in a list, each tried
/// on the whole screen as you move to it.
pub struct ThemePicker {
    pub entries: Vec<ThemeChoice>,
    /// What's typed, narrowing the list.
    pub query: String,
    /// Which of [`ThemePicker::shown`] is chosen.
    pub selected: usize,
    /// Why the chosen theme can't be shown (a broken theme file).
    pub error: Option<String>,
    /// The theme you had, and its colours, for Esc.
    before: (ThemeId, Theme),
}

pub struct ThemeChoice {
    pub id: ThemeId,
    /// "classic", "yours"…
    pub shelf: &'static str,
    pub about: String,
}

impl ThemePicker {
    /// The themes that match what's typed (in their names or what they're
    /// about).
    pub fn shown(&self) -> Vec<&ThemeChoice> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.id.name().contains(&query) || e.about.to_lowercase().contains(&query))
            .collect()
    }

    /// The theme you had when it opened.
    pub fn had(&self) -> &ThemeId {
        &self.before.0
    }
}

pub enum Preview {
    Loading,
    Ready(Picture),
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

/// Panes and panels to put back: the split you had (restored at startup)
/// or one saved with `/layout save`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SavedLayout {
    /// Empty: one window, no split.
    #[serde(default)]
    panes: Vec<WinRef>,
    #[serde(default)]
    side_by_side: bool,
    focus: Option<WinRef>,
    /// The panes' shares of the room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sizes: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidebar: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nicklist: Option<bool>,
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
    /// The split on screen when Textrix last closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    split: Option<SavedLayout>,
    /// `/layout save <name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    layouts: BTreeMap<String, SavedLayout>,
}

/// Something for scripts to hear about, from a room's timeline.
enum Heard {
    Message(Line),
    Join(Line),
    Leave(Line),
    Reaction { nick: String, key: String, target: OwnedEventId, text: String },
}

/// Several windows on screen at once, like irssi's split windows. Panes
/// hold window identities (`None` is the status window), so they survive
/// windows opening, closing and moving. The active window is always one of
/// them: the focused pane.
pub struct Split {
    pub panes: Vec<WinRef>,
    pub side_by_side: bool,
    /// Each pane's share of the room (in cells when last resized; they
    /// scale with the terminal).
    pub sizes: Vec<u16>,
}

impl Split {
    pub fn new(panes: Vec<WinRef>, side_by_side: bool) -> Self {
        let sizes = vec![PANE_SHARE; panes.len()];
        Self { panes, side_by_side, sizes }
    }

    /// Each pane's share; equal ones if they've got out of step.
    pub fn weights(&self) -> Vec<u16> {
        if self.sizes.len() == self.panes.len() { self.sizes.clone() } else { vec![PANE_SHARE; self.panes.len()] }
    }

    /// A new pane at `slot`, taking half the room of the one before it.
    fn insert(&mut self, slot: usize, pane: WinRef) {
        let mut sizes = self.weights();
        let from = slot.saturating_sub(1).min(sizes.len().saturating_sub(1));
        let half = sizes.get(from).map_or(PANE_SHARE, |s| (s / 2).max(1));
        if let Some(s) = sizes.get_mut(from) {
            *s = (*s - half).max(1);
        }
        sizes.insert(slot.min(sizes.len()), half);
        self.panes.insert(slot, pane);
        self.sizes = sizes;
    }

    fn remove(&mut self, slot: usize) {
        let mut sizes = self.weights();
        self.panes.remove(slot);
        sizes.remove(slot);
        self.sizes = sizes;
    }
}

/// A pane's share of the room before any resizing.
const PANE_SHARE: u16 = 100;
/// The smallest a pane can be made: rows stacked, columns side by side.
const PANE_MIN: (u16, u16) = (3, 12);

const PANES_MAX: usize = 4;
/// Link titles kept in memory.
const LINK_TITLES_MAX: usize = 2000;
/// The shortest time between two rings.
const BELL_GAP: Duration = Duration::from_secs(2);
/// How long the visual bell flashes.
const FLASH: Duration = Duration::from_millis(250);
/// How soon a second Ctrl+C has to come to quit.
const QUIT_WINDOW: Duration = Duration::from_secs(2);
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
    /// The hyperlinks last written over the screen's links.
    last_links: String,
    pub redraw_all: bool,
    pub show_sidebar: bool,
    pub show_nicklist: bool,
    pub view_width: u16,
    pub view_height: u16,
    layout: Layout,
    /// Last session's split, waiting for its windows to open.
    pending_split: Option<SavedLayout>,
    /// Servers that have said which rooms they have.
    rooms_loaded: HashSet<String>,
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
    /// Files dropped into the input line, waiting for y/n to upload.
    pub confirm_upload: Option<Vec<PathBuf>>,
    /// Text whose upload was declined, so Enter then sends it as it is.
    upload_declined: Option<String>,
    /// When Ctrl+C was pressed on an empty input line: again soon quits.
    quit_armed: Option<Instant>,
    /// The divider being dragged (between drawn panes k and k + 1).
    dragging: Option<usize>,
    /// The textrix this was started as, and as it was last seen on disk.
    pub install: Option<crate::install::Install>,
    /// A newer textrix installed under this one: its version ("" when it
    /// didn't say).
    installed: Option<String>,
    /// A newer release on GitHub (`check_updates`).
    released: Option<String>,
    /// Asking a changed textrix its version.
    probing: bool,
    /// `/restart`: quit to become the textrix on disk.
    pub restart: bool,
    /// Seconds the clock has ticked.
    clock_ticks: u64,
    /// Where you were before a `/restart`, waiting for its window.
    pending_resume: Option<crate::install::Resume>,
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
    /// The audio playing, and how to stop it.
    pub playing: Option<(OwnedEventId, tokio::task::AbortHandle)>,
    pub split: Option<Split>,
    /// Loaded scripts (~/.textrix/scripts).
    pub scripts: Scripts,
    /// What to tell scripts about after this event.
    script_messages: Vec<(WinKey, Heard)>,
    /// Servers whose connection dropped, and since when (netsplits).
    splits: HashMap<String, Instant>,
    /// People whose away status you've been told about (irssi's "is
    /// away"), and what it was.
    /// Who an "is away" note was shown for: their status then, and when.
    away_noted: HashMap<OwnedUserId, (String, Instant)>,
    /// Where each message in the mentions window came from.
    mention_origins: HashMap<OwnedEventId, WinKey>,
    /// How many aliases deep the current command runs.
    alias_depth: usize,
    /// How much of the Konami code has been typed.
    konami: usize,
    pub switcher: Option<Switcher>,
    pub theme_picker: Option<ThemePicker>,
    /// Windows you were in, most recent first (for the switcher).
    recent: Vec<WinRef>,
    /// Rooms with their own notification mode, per server.
    pub push_modes: HashMap<AccountName, HashMap<OwnedRoomId, RoomNotificationMode>>,
    /// Where the last frame drew things, for the mouse.
    pub hits: ui::Hits,
    /// Terminal graphics support, probed at startup; `None` disables previews.
    /// How pictures are drawn here (`None`: they aren't).
    pub picker: Option<Painter>,
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
    /// The `/search -deep` being answered, to narrow what comes back.
    search_query: Option<crate::search::Query>,
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
    Cmd { name: "upgrade", args: "[<version> confirm]", help: "Show room versions, or move the room to a new version" },
    Cmd {
        name: "close",
        args: "[confirm]",
        help: "As its owner, shut the room down for everyone (says what it'll do first; confirm does it)",
    },
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
    Cmd {
        name: "play",
        args: "[nick | stop]",
        help: "Play the last voice message, audio or video (from nick); stop stops audio",
    },
    Cmd { name: "lastlog", args: "[-n N] [-nick nick] <text>", help: "Search this window's loaded scrollback" },
    Cmd {
        name: "search",
        args: "[-deep] [from:name] [in:room] [after:date] [before:date] [text]",
        help: "Search every room (the encrypted local index, and loaded history; -deep also reads open rooms' last 500)",
    },
    Cmd { name: "jump", args: "<N>", help: "Go to result N of /search or /pins and select it" },
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
    Cmd { name: "avatar", args: "<image> | -remove", help: "Set or remove your profile picture" },
    Cmd { name: "roomavatar", args: "<image> | -remove", help: "Set or remove this room's picture" },
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
    Cmd {
        name: "resize",
        args: "+N | -N | =",
        help: "Grow or shrink this pane by N rows (columns side by side), or make them even; or drag the line between",
    },
    Cmd {
        name: "layout",
        args: "[<name> | save <name> | load <name> | delete <name>]",
        help: "Saved arrangements of split panes and panels; /layout lists them",
    },
    Cmd { name: "markread", args: "[all]", help: "Mark this room read (all: every room), for your other clients too" },
    Cmd {
        name: "alias",
        args: "[name [command]]",
        help: "Your own command: /alias tt /window goto Team Tests ($0 $1 $1- $* take arguments, ; chains)",
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
        args: "[name | list | save <name> | import <file>]",
        help: "Colours: no name opens the list, each shown as you move to it; list prints them; save writes this look \
               to a theme file, import adds one",
    },
    Cmd {
        name: "themes",
        args: "",
        help: "Pick a theme from a list, each shown as you move to it; Enter keeps it for the session (/save for good)",
    },
    Cmd { name: "nicklist", args: "", help: "Toggle the member list (F3)" },
    Cmd { name: "servers", args: "", help: "List dotfile servers and their status" },
    Cmd { name: "log", args: "[on|off]", help: "Chat logging for this session (log = true in config makes it stick)" },
    Cmd {
        name: "set",
        args: "[name [value]]",
        help: "List the settings, or change one for this session (/save keeps it)",
    },
    Cmd { name: "save", args: "", help: "Save this session's settings (theme, bell, panels…) to config.toml" },
    Cmd {
        name: "reload",
        args: "",
        help: "Re-read config.toml and the server dotfile (connects new servers, drops removed ones)",
    },
    Cmd { name: "connect", args: "<server>", help: "Connect a server from the dotfile" },
    Cmd { name: "disconnect", args: "<server>", help: "Stop syncing a server (session is kept)" },
    Cmd { name: "logout", args: "[server]", help: "Log out and delete the local session and keys" },
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
        args: "[rename <id> <name> | delete <id> [confirm]]",
        help: "List your sessions, rename one, or sign one out (asks to confirm)",
    },
    Cmd { name: "version", args: "[check]", help: "Show the version; check asks GitHub for a newer one" },
    Cmd {
        name: "script",
        args: "[list|load <name>|unload <name>|reload [name]|exec <code>]",
        help: "Manage scripts (.rhai files in ~/.textrix/scripts)",
    },
    Cmd {
        name: "detach",
        args: "",
        help: "Leave Textrix running in the background (started with textrix --daemon); textrix --attach comes back",
    },
    Cmd {
        name: "restart",
        args: "",
        help: "Start Textrix again in place (to run a newly installed version), back where you were",
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
    ("fwd", "forward"),
    ("read", "markread"),
    ("sessions", "devices"),
    ("highlight", "hilight"),
    ("unhilight", "dehilight"),
    ("pinned", "pins"),
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
    ("Alt+B / Alt+F, Ctrl+← / Ctrl+→", "back / forward a word"),
    ("Home / End, Ctrl+D", "start / end of the line; delete the character under the cursor"),
    ("Ctrl+U / Ctrl+K / Ctrl+W / Ctrl+Y", "cut to start / cut to end / cut word / paste cut"),
    ("Ctrl+C", "clear the input line; on an empty one, twice quits (detaches, in the background)"),
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
    ("z or Space", "play a voice message, audio (again stops it) or a video"),
    ("s", "save the attachment"),
    ("o", "open the link"),
    ("v", "show spoilers"),
    ("c", "copy the text"),
    ("i", "sender, time and event ID"),
    ("Enter", "in the mentions window: go to the message in its room"),
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
        let pending_resume = std::fs::read(crate::install::resume_file(&paths.base))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let (bell_mode, bell_style) = (cfg.bell, cfg.bell_style);
        let layout: Layout = std::fs::read(paths.layout_file())
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
            last_links: String::new(),
            redraw_all: false,
            show_sidebar,
            show_nicklist,
            view_width: 80,
            view_height: 24,
            pending_split: layout.split.clone(),
            rooms_loaded: HashSet::new(),
            layout,
            typing_since: None,
            mentions: Vec::new(),
            focused: true,
            notify_mode: NotifyMode::Off,
            notifications: Vec::new(),
            select: None,
            confirm_delete: None,
            confirm_paste: None,
            confirm_upload: None,
            upload_declined: None,
            quit_armed: None,
            dragging: None,
            install: None,
            installed: None,
            released: None,
            probing: false,
            restart: false,
            clock_ticks: 0,
            pending_resume,
            resume_edit: None,
            editing: None,
            compose: None,
            clipboard: None,
            help: None,
            viewer: None,
            playing: None,
            split: None,
            scripts: Scripts::new(),
            script_messages: Vec::new(),
            splits: HashMap::new(),
            away_noted: HashMap::new(),
            mention_origins: HashMap::new(),
            alias_depth: 0,
            konami: 0,
            switcher: None,
            theme_picker: None,
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
            search_query: None,
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
        if self.cfg.check_updates {
            self.watch_for_updates();
        }
        let _ = std::fs::remove_file(crate::install::resume_file(&self.paths.base));
        // Audio left over from a crash; anything playing deletes its own.
        let _ = std::fs::remove_dir_all(self.paths.base.join("tmp"));
        self.scripts.set_data_dir(self.paths.base.join("script-data"));
        self.scripts.configure(&self.cfg.script_settings);
        if self.cfg.scripts {
            for path in Scripts::available(&self.scripts_dir()) {
                self.load_script(&path);
            }
        }
    }

    fn scripts_dir(&self) -> PathBuf {
        self.paths.base.join("scripts")
    }

    fn load_script(&mut self, path: &Path) {
        match self.scripts.load_file(path) {
            Ok(effects) => {
                let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                self.status(Line::info(format!("script {name} loaded")));
                self.apply_effects(effects, None);
            }
            Err(e) => self.status(Line::error(format!("script {e}"))),
        }
    }

    /// The room a script is working in, for its handlers.
    fn script_context(&self, key: Option<&WinKey>) -> script::Context {
        let idx = key.and_then(|k| self.windows.iter().position(|w| w.room.as_ref() == Some(k)));
        let w = &self.windows[idx.unwrap_or(self.active)];
        script::Context {
            server: w.account().unwrap_or_default().to_owned(),
            room: w.title().to_owned(),
            room_id: w.room.as_ref().map(|k| k.room_id.to_string()).unwrap_or_default(),
        }
    }

    /// Send text to a room's window (echoed when it's the one you're in).
    fn send_text_to(&mut self, key: WinKey, text: &str) {
        if self.win().room.as_ref() == Some(&key) {
            return self.say(text, Say::Text);
        }
        match self.connected(&key.account) {
            Some(acct) => {
                acct.send(key.room_id, compose(text, Say::Text, &[], self.cfg.markdown), TransactionId::new())
            }
            None => self.error(format!("{} is not connected", key.account)),
        }
    }

    /// Carry out what a script asked for, in the room it was working in
    /// (`None`: the window you're in).
    fn apply_effects(&mut self, effects: Vec<script::Effect>, room: Option<WinKey>) {
        let idx = room
            .as_ref()
            .and_then(|k| self.windows.iter().position(|w| w.room.as_ref() == Some(k)))
            .unwrap_or(self.active);
        let cap = self.cfg.scrollback;
        for effect in effects {
            match effect {
                script::Effect::Print(text) => {
                    self.windows[idx].push(Line::info(text), cap);
                }
                script::Effect::Say(text) => match self.windows[idx].room.clone() {
                    Some(key) => self.send_text_to(key, &text),
                    None => self.error("a script tried to say something outside a room"),
                },
                script::Effect::SendTo { room, text } => {
                    if room.starts_with(['!', '#', '@']) {
                        let account = self.windows[idx].account().map(ToOwned::to_owned);
                        match account.and_then(|a| self.connected(&a)) {
                            Some(acct) => acct.send_to(room, compose(&text, Say::Text, &[], self.cfg.markdown)),
                            None => self.error("a script's send() needs a connected server"),
                        }
                    } else {
                        // Exact names only: a partial match could send to the wrong room.
                        let wanted = room.to_lowercase();
                        let found = self.windows.iter().find(|w| w.names().any(|n| n.to_lowercase() == wanted));
                        match found.and_then(|w| w.room.clone()) {
                            Some(key) => self.send_text_to(key, &text),
                            None => self.error(format!("a script tried to send to {room}, which isn't a room here")),
                        }
                    }
                }
                script::Effect::Run(command) => {
                    if self.alias_depth >= 8 {
                        self.error("scripts and aliases call each other too deeply");
                        continue;
                    }
                    self.alias_depth += 1;
                    self.command(command.trim_start_matches('/'));
                    self.alias_depth -= 1;
                }
                script::Effect::Bell => self.ring(BellKind::Mentions),
                script::Effect::Notify { title, body } => {
                    if self.notifying() {
                        self.notifications.push((title, body));
                    }
                }
            }
        }
    }

    /// Hand what others did (messages, joins and leaves, reactions) to the
    /// scripts' hooks.
    fn run_message_scripts(&mut self) {
        for (key, heard) in std::mem::take(&mut self.script_messages) {
            let context = self.script_context(Some(&key));
            let dm = self.window_idx(&key.account, &key.room_id).is_some_and(|i| self.windows[i].is_dm);
            let member = |line: &Line, name: &str| script::Member {
                context: context.clone(),
                user: line.sender.as_ref().map(ToString::to_string).unwrap_or_default(),
                nick: self.display_name(line.sender.as_deref(), name).to_owned(),
            };
            let event = match heard {
                Heard::Message(line) => script::Event::Message(script::Message {
                    context: context.clone(),
                    sender: line.sender.as_ref().map(ToString::to_string).unwrap_or_default(),
                    nick: self.display_name(line.sender.as_deref(), &line.nick).to_owned(),
                    body: line.text(),
                    highlight: line.highlight,
                    dm,
                }),
                Heard::Join(line) => {
                    let name = match &line.fold {
                        Some(Fold::Joined(name)) => name.clone(),
                        _ => line.nick.clone(),
                    };
                    script::Event::Join(member(&line, &name))
                }
                Heard::Leave(line) => {
                    let name = match &line.fold {
                        Some(Fold::Left(name)) => name.clone(),
                        _ => line.nick.clone(),
                    };
                    script::Event::Leave(member(&line, &name))
                }
                Heard::Reaction { nick, key: emoji, target, text } => script::Event::Reaction(script::Reaction {
                    context: context.clone(),
                    nick,
                    key: emoji,
                    target: target.to_string(),
                    text,
                }),
            };
            let (effects, errors) = self.scripts.on_event(&event);
            for e in errors {
                self.status(Line::error(format!("script {e}")));
            }
            self.apply_effects(effects, Some(key));
        }
    }

    /// `/script [list|load <name>|unload <name>|reload [name]|exec <code>]`.
    fn cmd_script(&mut self, args: &str) -> Result<(), String> {
        let (sub, rest) = split_first(args.trim());
        let rest = rest.trim();
        match sub {
            "" | "list" => {
                let scripts = self.scripts.list();
                if scripts.is_empty() {
                    self.info(format!("no scripts loaded; put .rhai files in {}", self.scripts_dir().display()));
                }
                for (name, path, commands, hooks) in scripts {
                    let commands: Vec<String> = commands.iter().map(|c| format!("/{c}")).collect();
                    let commands = if commands.is_empty() { "no commands".to_owned() } else { commands.join(" ") };
                    let hooks = if hooks > 0 { format!(", {hooks} message hook(s)") } else { String::new() };
                    let from = path.map(|p| format!(" ({})", p.display())).unwrap_or_default();
                    self.info(format!("  {name}: {commands}{hooks}{from}"));
                }
            }
            "load" if !rest.is_empty() => {
                let path = if rest.contains('/') || rest.ends_with(".rhai") {
                    parse_path(rest)
                } else {
                    self.scripts_dir().join(format!("{rest}.rhai"))
                };
                self.load_script(&path);
            }
            "unload" if !rest.is_empty() => {
                if !self.scripts.unload(rest) {
                    return Err(format!("no script {rest} loaded"));
                }
                self.info(format!("script {rest} unloaded"));
            }
            "reload" => {
                let paths: Vec<PathBuf> = if rest.is_empty() {
                    Scripts::available(&self.scripts_dir())
                } else {
                    vec![self.scripts.path_of(rest).ok_or_else(|| format!("no script {rest} loaded from a file"))?]
                };
                for path in paths {
                    self.load_script(&path);
                }
            }
            "exec" if !rest.is_empty() => {
                let effects = self.scripts.exec(rest).map_err(|e| format!("script: {e}"))?;
                let here = self.win().room.clone();
                self.apply_effects(effects, here);
            }
            _ => return Err("usage: /script [list | load <name> | unload <name> | reload [name] | exec <code>]".into()),
        }
        Ok(())
    }

    /// `check_updates`: once an hour (remembered across restarts), ask GitHub
    /// for the latest release.
    fn watch_for_updates(&self) {
        let (tx, stamp) = (self.tx.clone(), self.paths.base.join("update-check"));
        tokio::spawn(async move {
            loop {
                let due =
                    update::last_checked(&stamp).and_then(|t| t.elapsed().ok()).is_none_or(|age| age >= update::EVERY);
                if due {
                    match update::latest().await {
                        Ok(latest) => {
                            update::mark_checked(&stamp);
                            let _ = tx.send(AppEvent::Update { latest: Some(latest), asked: false });
                        }
                        Err(e) => tracing::debug!("update check failed: {e:#}"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            }
        });
    }

    fn connect(&mut self, name: &str) {
        let Some(state) = self.accounts.get_mut(name) else { return };
        state.status = ConnStatus::Connecting;
        let (def, paths, env, tx) = (state.def.clone(), self.paths.clone(), self.env.clone(), self.tx.clone());
        let opts = matrix::ConnectOptions {
            sync_mode: self.cfg.sync,
            history_cache: self.cfg.history_cache,
            encrypt_store: self.cfg.encrypt_store,
            search_index: self.cfg.search_index,
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
        self.scripts.save_stores();
        // Videos handed to a player stay until now.
        let _ = std::fs::remove_dir_all(self.paths.base.join("tmp"));
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
            AppEvent::Update { latest, asked } => {
                let current = env!("CARGO_PKG_VERSION");
                match latest {
                    Some((version, url)) if update::newer(&version, current) => {
                        if self.released.as_deref() != Some(&version) {
                            self.status(Line::info(format!(
                                "Textrix {version} is out (you have {current}): brew upgrade textrix, or {url}"
                            )));
                        }
                        self.released = Some(version);
                    }
                    Some(_) if asked => self.info(format!("Textrix {current} is the latest")),
                    None if asked => self.error("couldn't reach GitHub to check for updates"),
                    _ => {}
                }
            }
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
            AppEvent::Attached { picker } => {
                // Images are drawn afresh for this terminal.
                self.picker = picker.filter(|_| self.cfg.image_previews).map(|p| Painter::new(p, self.cfg.image_text));
                self.note_images();
                self.previews.clear();
                self.avatars.clear();
                self.viewer = None;
                self.notify_mode = notify::resolve(self.cfg.notifications);
                // This terminal may have more colours, or fewer.
                if let Ok(palette) = Theme::load(&self.cfg.theme, &self.cfg.colors, &self.paths.themes_dir()) {
                    self.cfg.palette = palette;
                }
                (self.redraw_all, self.last_title) = (true, None);
                if self.cfg.mouse {
                    set_mouse_capture(true);
                }
            }
            AppEvent::Quit => self.quit = true,
            AppEvent::Installed { version } => {
                self.probing = false;
                let current = env!("CARGO_PKG_VERSION");
                let text = match version.as_deref() {
                    Some(v) if v != current => {
                        format!("Textrix {v} is installed (this is {current}): /restart switches to it")
                    }
                    _ => "a new build of Textrix is installed: /restart runs it".to_owned(),
                };
                if self.installed.is_none() {
                    self.status(Line::info(text));
                }
                self.installed = Some(version.filter(|v| v != current).unwrap_or_default());
            }
            AppEvent::Detached => {
                self.status(Line::info("detached: Textrix carries on in the background (textrix --attach)"));
            }
            AppEvent::Played { event_id, result } => {
                if self.playing.as_ref().is_some_and(|(playing, _)| *playing == event_id) {
                    self.playing = None;
                }
                if let Err(e) = result {
                    self.error(e);
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
                match self.splits.remove(&*account) {
                    Some(since) if !first => {
                        let rooms = self.split_rooms(&account);
                        let text = format!("Netsplit over after {}, joins: {rooms}", short_duration(since.elapsed()));
                        self.netsplit_note(&account, Line::info(text));
                    }
                    _ => self.status(Line::info(format!("{account}: synced"))),
                }
            }
            AppEvent::SyncError { account, error } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Lagging;
                }
                if !self.splits.contains_key(&*account) {
                    // irssi-style: the server split from you, taking your rooms.
                    self.splits.insert(account.to_string(), Instant::now());
                    let server = self.server_host(&account);
                    let rooms = self.split_rooms(&account);
                    let text = format!("Netsplit {server} ⇄ you, quits: {rooms} ({error}; rejoining when it's back)");
                    self.netsplit_note(&account, Line::error(text));
                }
            }
            AppEvent::Rooms { account, mut rooms } => {
                rooms.sort_by_cached_key(|m| (m.is_dm, m.name.to_lowercase()));
                for meta in rooms {
                    self.upsert_room(&account, meta);
                }
                self.rooms_loaded.insert(account.to_string());
                self.restore_split();
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
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &items);
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
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &items);
                }
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
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &hits.iter().cloned().map(Item::Line).collect::<Vec<_>>());
                }
                self.search_pending = self.search_pending.saturating_sub(1);
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                let room = self
                    .window_idx(&account, &room_id)
                    .map(|i| self.windows[i].title().to_owned())
                    .or_else(|| self.connected(&account).and_then(|a| a.quick_meta(&room_id)).map(|m| m.name))
                    .unwrap_or_else(|| room_id.to_string());
                let query = self.search_query.clone().unwrap_or_default();
                let rows: Vec<Line> = hits
                    .into_iter()
                    .filter(|line| self.search_matches(&query, &room, line))
                    .collect::<Vec<_>>()
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
        let notifying = self.notifying();

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

        let script_key = self.windows[idx].room.clone().map(|k| WinKey { thread: None, ..k });
        for item in items {
            if live
                && let Item::React { target, key, nick, own: false } = &item
                && let Some(room) = script_key.clone()
            {
                let text = self.windows[idx].find_mut(target).map(|l| l.text()).unwrap_or_default();
                let heard = Heard::Reaction { nick: nick.clone(), key: key.clone(), target: target.clone(), text };
                self.script_messages.push((room, heard));
            }
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
            if live
                && !line.own
                && let Some(key) = script_key.clone()
            {
                let heard = match &line.fold {
                    _ if line.is_message() && !line.redacted => Some(Heard::Message(line.clone())),
                    Some(Fold::Joined(_)) => Some(Heard::Join(line.clone())),
                    Some(Fold::Left(_)) => Some(Heard::Leave(line.clone())),
                    _ => None,
                };
                self.script_messages.extend(heard.map(|h| (key, h)));
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
            if notifying {
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
        self.run_message_scripts();
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

    /// ↑↑↓↓←→←→BA: the green theme, or back again.
    fn konami_unlocked(&mut self) {
        if self.input.text() == "b" {
            self.input.clear();
        }
        let green = ThemeId::Builtin(ThemeName::Green);
        let back = self.cfg.theme == green;
        let id = if back { ThemeId::default() } else { green };
        if let Ok(palette) = Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()) {
            (self.cfg.palette, self.cfg.theme, self.redraw_all) = (palette, id, true);
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

    /// Where a server lives, for netsplit lines: "example.org".
    fn server_host(&self, account: &str) -> String {
        let url = self.connected(account).map(|a| a.homeserver).unwrap_or_else(|| account.to_owned());
        let host = url.split("://").last().unwrap_or(&url);
        host.trim_end_matches('/').split('/').next().unwrap_or(host).to_owned()
    }

    /// The server's rooms, as a netsplit lists its nicks.
    fn split_rooms(&self, account: &str) -> String {
        let rooms: Vec<&str> = self
            .windows
            .iter()
            .filter(|w| w.room.as_ref().is_some_and(|k| k.account == account && k.thread.is_none()))
            .map(|w| w.title())
            .collect();
        match rooms.len() {
            0 => "no rooms".to_owned(),
            n if n <= 4 => rooms.join(", "),
            n => format!("{} +{}", rooms[..3].join(", "), n - 3),
        }
    }

    /// In the status window, and the window you're in if it's that server's.
    fn netsplit_note(&mut self, account: &str, line: Line) {
        if self.active != 0 && self.win().account() == Some(account) {
            let cap = self.cfg.scrollback;
            self.windows[self.active].push(line.clone(), cap);
        }
        self.status(line);
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
                split.remove(slot);
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
        let split = match &self.pending_split {
            // Never restored (its server didn't connect): keep it for next time.
            Some(pending) => Some(pending.clone()),
            None => self.split.is_some().then(|| self.current_layout(false)),
        };
        let layout = Layout {
            windows,
            closed: self.layout.closed.clone(),
            names: self.layout.names.clone(),
            nicknames: self.layout.nicknames.clone(),
            split,
            layouts: self.layout.layouts.clone(),
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
        self.cfg.sidebar_avatars && self.picker.as_ref().is_some_and(Painter::graphics)
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
    /// The links on screen in the last frame, with where they go.
    pub fn screen_links(&self) -> Vec<crate::hyperlinks::Link> {
        let mut out = Vec::new();
        for pane in &self.hits.panes {
            for (i, row) in pane.rows.iter().enumerate() {
                let Some(event) = &row.event else { continue };
                for (from, to, text) in &row.links {
                    if let Some(url) = self.link_in_window(pane.window, event, text).filter(|u| links::is_safe(u)) {
                        let (x, y, width) = (*from, pane.area.y + i as u16, to.saturating_sub(*from));
                        out.push(crate::hyperlinks::Link { x, y, width, url });
                    }
                }
            }
        }
        out
    }

    /// Say how pictures are drawn in this terminal.
    pub fn note_images(&mut self) {
        let Some(painter) = &self.picker else { return };
        let note = if painter.graphics() {
            format!("images: {}", painter.describe())
        } else {
            format!(
                "images: {}, as this terminal can't show pictures (kitty, Ghostty, WezTerm and iTerm2 can); \
                 image_text in /set changes how",
                painter.describe()
            )
        };
        self.status(Line::info(note));
    }

    /// Whether to notify: as the terminal allows, or, in the background
    /// with no terminal attached, through the system (unless notifications
    /// are off altogether).
    fn notifying(&self) -> bool {
        if crate::term::attached() {
            self.notify_mode != NotifyMode::Off
        } else {
            self.cfg.notifications != NotifyMode::Off
        }
    }

    /// Once a second: last session's split if it's waiting, scripts' live
    /// status items, and their stores saved if they changed.
    pub fn on_clock(&mut self) {
        // A server that never connects shouldn't hold the split back.
        self.restore_split();
        self.clock_ticks += 1;
        if self.clock_ticks.is_multiple_of(30) {
            self.watch_install();
        }
        let (effects, mut errors) = self.scripts.refresh_status();
        self.apply_effects(effects, None);
        let (effects, timer_errors) = self.scripts.run_timers();
        self.apply_effects(effects, None);
        errors.extend(timer_errors);
        errors.extend(self.scripts.save_stores());
        for e in errors {
            self.status(Line::error(format!("script {e}")));
        }
    }

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
                let empty = self.input.is_empty();
                self.input.paste(&text);
                self.typed();
                // A dropped file arrives as its path.
                if empty {
                    self.offer_upload();
                }
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
            // Dragging the line between two panes moves it.
            MouseEventKind::Down(MouseButton::Left) if let Some(k) = self.divider_at(at) => self.dragging = Some(k),
            MouseEventKind::Drag(MouseButton::Left) if let Some(k) = self.dragging => {
                let side = self.split.as_ref().is_some_and(|s| s.side_by_side);
                if let (Some((a, _)), Some((b, _))) =
                    (self.hits.splits.get(k).copied(), self.hits.splits.get(k + 1).copied())
                {
                    let (start, at, pair) =
                        if side { (a.x, at.x, a.width + b.width) } else { (a.y, at.y, a.height + b.height) };
                    let cells = self.clamp_pane(i32::from(at) - i32::from(start) + 1, pair);
                    self.set_divider(k, cells);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = None,
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
        if let Some(p) = self.theme_picker.as_mut() {
            p.selected = if up { p.selected.saturating_sub(1) } else { p.selected + 1 };
            return self.try_picked_theme();
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
        if self.theme_picker.is_some() {
            // A theme shows it, again keeps it; a click outside puts back
            // the one you had.
            let hits = self.hits.theme_picker.as_ref();
            let row = hits.and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at))).map(|&(_, n)| n);
            let inside = hits.is_some_and(|(area, _)| area.contains(at));
            match (row, self.theme_picker.as_mut()) {
                (Some(n), Some(p)) if p.selected == n => self.close_theme_picker(true),
                (Some(n), Some(p)) => {
                    p.selected = n;
                    self.try_picked_theme();
                }
                _ if !inside => self.close_theme_picker(false),
                _ => {}
            }
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
        if self.help.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_paste.is_some()
            || self.confirm_upload.is_some()
        {
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
        self.link_in_window(self.active, event, fragment)
    }

    fn link_in_window(&self, window: usize, event: &EventId, fragment: &str) -> Option<String> {
        let fragment = fragment.trim().trim_matches(|c: char| "()<>[]\"'".contains(c));
        if fragment.chars().count() < 3 {
            return None;
        }
        let line = self.windows.get(window)?.lines.iter().rev().find(|l| l.event_id.as_deref() == Some(event))?;
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
        if !(ctrl && key.code == Char('c')) {
            self.quit_armed = None;
        }
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
        if self.theme_picker.is_some() {
            return self.on_theme_picker_key(key);
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
        if let Some(files) = self.confirm_upload.take() {
            // Dropping files and pressing Enter uploads them.
            if matches!(key.code, Char('y' | 'Y') | Enter) {
                self.input.take();
                for path in files {
                    if let Err(e) = self.upload_file(path) {
                        self.error(e);
                    }
                }
            } else {
                // Declined: it stays as text, and Enter sends it as text.
                self.upload_declined = Some(self.input.text());
            }
            return;
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
                'c' if self.input.is_empty() && self.compose.is_none() => self.ctrl_c_quits(),
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
                None => split.insert(split.panes.len(), to),
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
            Char('z' | ' ') if line.media.as_ref().is_some_and(|m| m.playable()) => {
                if let Err(e) = self.play(&line) {
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
            (None, _)
                if let Some((_, help, script)) = self.scripts.commands().into_iter().find(|(n, _, _)| *n == word) =>
            {
                Some(format!("  {help} ({script}.rhai)"))
            }
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
            let script_names: Vec<String> = self.scripts.commands().into_iter().map(|(n, _, _)| n).collect();
            let mut names: Vec<&str> = COMMANDS
                .iter()
                .map(|c| c.name)
                .chain(ALIASES.iter().map(|a| a.0))
                .chain(["wc", "wl"])
                .chain(self.cfg.aliases.keys().map(String::as_str))
                .chain(script_names.iter().map(String::as_str))
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
        if first == "/set" && text.split_whitespace().count() <= 2 && !at_start {
            let mut names: Vec<String> =
                self.effective_config().to_table().map(|t| t.keys().cloned().collect()).unwrap_or_default();
            names.retain(|n| n != "script_settings");
            names.extend(self.scripts.settings().into_iter().map(|(name, _, _)| name));
            names.push("log_dir".into());
            names.sort();
            return names
                .into_iter()
                .filter(|n| n.starts_with(&lower))
                .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                .collect();
        }
        if first == "/upload" || (first == "/keys" && matches!(second, "export" | "import") && !at_start) {
            return path_candidates(&word);
        }
        if matches!(first, "/avatar" | "/roomavatar") && !at_start {
            return path_candidates(&word);
        }
        if first == "/theme" && !at_start {
            let words = text.split_whitespace().count();
            if words <= 1 || (words == 2 && !text.ends_with(' ')) {
                let yours = crate::theme::files(&self.paths.themes_dir());
                let names = ThemeName::ALL.iter().map(|(n, _)| *n).chain(yours.iter().map(String::as_str));
                return names
                    .chain(THEME_WORDS)
                    .filter(|n| n.starts_with(&lower))
                    .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                    .collect();
            }
            return if second == "import" { path_candidates(&word) } else { Vec::new() };
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

    /// Ctrl+C on an empty input line: once asks, and again within two
    /// seconds quits (in the background, detaches, as closing the terminal
    /// would).
    fn ctrl_c_quits(&mut self) {
        if self.quit_armed.is_some_and(|at| at.elapsed() < QUIT_WINDOW) {
            self.quit_armed = None;
            if !crate::remote::detach("detached; textrix --attach comes back to it") {
                self.quit = true;
            }
        } else {
            self.quit_armed = Some(Instant::now());
        }
    }

    /// The status bar's "press Ctrl+C again" while it's waiting.
    pub fn quit_prompt(&self) -> Option<&'static str> {
        self.quit_armed.filter(|at| at.elapsed() < QUIT_WINDOW).map(|_| match crate::term::remote_client() {
            Some(_) => "press Ctrl+C again to detach (Textrix keeps running; /quit stops it)",
            None => "press Ctrl+C again to quit",
        })
    }

    /// Ask whether to upload what's in the input line, if it's nothing but
    /// paths to files (as dropping files on the terminal leaves it).
    fn offer_upload(&mut self) {
        let text = self.input.text();
        if self.win().room.is_none() || self.upload_declined.as_ref() == Some(&text) {
            return;
        }
        if let Some(files) = dropped_files(&text) {
            self.confirm_upload = Some(files);
        }
    }

    /// What the upload prompt asks about: "notes.pdf (1.2 MB)" or "3 files
    /// (4.5 MB)".
    pub fn upload_summary(&self) -> Option<String> {
        let files = self.confirm_upload.as_ref()?;
        let size: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        let what = match files.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            many => format!("{} files", many.len()),
        };
        Some(format!("{what} ({})", matrix::human_size(size)))
    }

    fn submit(&mut self) {
        // Terminals that type a dropped path rather than paste it.
        self.offer_upload();
        if self.confirm_upload.is_some() {
            return;
        }
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
        // Paths you chose not to upload go as a message, not a command.
        if self.upload_declined.take().is_some_and(|declined| declined == text) {
            self.say(&text, Say::Text);
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
            "script" => self.cmd_script(args),
            "version" => {
                self.info(format!("Textrix {}", env!("CARGO_PKG_VERSION")));
                match args.trim() {
                    "check" => {
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let latest = update::latest().await.ok();
                            let _ = tx.send(AppEvent::Update { latest, asked: true });
                        });
                        Ok(())
                    }
                    "" => Ok(()),
                    _ => Err("usage: /version [check]".into()),
                }
            }
            "avatar" => self.pick_account(args).and_then(|(acct, image)| {
                let image = avatar_arg(image, "/avatar")?;
                acct.set_avatar(self.here(), image);
                Ok(())
            }),
            "roomavatar" => {
                let image = avatar_arg(args, "/roomavatar");
                image.and_then(|image| self.room_ctx().map(|(acct, room_id)| acct.set_room_avatar(room_id, image)))
            }
            "set" => self.cmd_set(args),
            "save" => self.cmd_save(),
            "nickname" => self.cmd_nickname(args),
            "bell" => self.cmd_bell(args),
            "keys" => self.cmd_keys(args),
            "forward" => self.cmd_forward(args),
            "markread" => self.cmd_markread(args),
            "theme" => self.cmd_theme(args),
            "themes" if args.trim().is_empty() => {
                self.open_theme_picker();
                Ok(())
            }
            "themes" => self.cmd_theme(args),
            "devices" => self.cmd_devices(args),
            "hilight" => self.pick_account(args).map(|(acct, word)| match word.trim() {
                "" => acct.list_highlights(self.here()),
                word => acct.set_highlight(self.here(), word.to_owned(), true),
            }),
            "dehilight" => self.pick_account(args).and_then(|(acct, word)| {
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
            "play" => self.cmd_play(args),
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
            "pins" => self.room_ctx().map(|(acct, room_id)| acct.pins(room_id)),
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
            "close" => self.room_ctx().map(|(acct, room_id)| acct.close_room(room_id, args.trim() == "confirm")),
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
            "resize" => self.cmd_resize(args),
            "layout" => self.cmd_layout(args),
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
            "detach" => match crate::remote::detach("detached; textrix --attach comes back to it") {
                true => Ok(()),
                false => Err("/detach is for Textrix in the background: start it with textrix --daemon".into()),
            },
            "restart" => self.cmd_restart(),
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
                    let here = self.win().room.clone();
                    let context = self.script_context(here.as_ref());
                    match self.scripts.run_command(&other, args, &context) {
                        Some(Ok(effects)) => {
                            self.apply_effects(effects, here);
                            Ok(())
                        }
                        Some(Err(e)) => Err(format!("script {e}")),
                        None => self.egg_command(&other, args).unwrap_or_else(|| {
                            Err(if other.contains('/') {
                                format!(
                                    "/{other} isn't a command, and not a file here either: drop files on an empty \
                                     input line, or /upload <path>"
                                )
                            } else {
                                format!("unknown command /{other}; try /help")
                            })
                        }),
                    }
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
                self.info("no aliases yet; e.g. /alias tt /window goto Team Tests");
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
        let builtin = COMMANDS.iter().any(|c| c.name == name)
            || ALIASES.iter().any(|(a, _)| *a == name)
            || ["wc", "wl"].contains(&name.as_str())
            || EGGS.contains(&name.as_str());
        if builtin {
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
    /// DM, by mentioning them, or replying to them) who's away or gone, say
    /// so, when it's news. Presence is a guess (a missed heartbeat reads as
    /// offline, and some people and servers don't share it), so only a
    /// status they set or a long absence counts; never for someone who's
    /// just been talking; and the same note at most once an hour.
    fn note_away(&mut self, replied_to: Option<OwnedUserId>) {
        if !self.cfg.away_notes {
            return;
        }
        let Some(account) = self.win().account().map(ToOwned::to_owned) else { return };
        let own = self.connected(&account).map(|a| a.user_id);
        let mut people: Vec<OwnedUserId> = self.win().dm_user.iter().cloned().collect();
        people.extend(self.mentions.iter().map(|(_, user)| user.clone()));
        people.extend(replied_to);
        people.dedup();
        for user in people.into_iter().filter(|u| Some(u) != own.as_ref()) {
            let Some(presence) = self.presence_of(&account, &user) else { continue };
            let status = presence.status.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned);
            let gone = presence.last_active.map(|at| Local::now() - at).filter(|g| *g >= AWAY_NEWS);
            let text = match (presence.kind, gone) {
                (PresenceKind::Online, _) => continue,
                (PresenceKind::Away, Some(gone)) => format!("away, last active {}", ago(gone)),
                (PresenceKind::Away, None) if status.is_some() => "away".to_owned(),
                (PresenceKind::Offline, Some(gone)) => format!("offline, last seen {}", ago(gone)),
                // A missed heartbeat, or presence they don't share.
                _ => continue,
            };
            if self.spoke_lately(&account, &user) {
                continue;
            }
            if let Some((said, at)) = self.away_noted.get(&user) {
                let new_status = status.as_ref().is_some_and(|s| s != said);
                if at.elapsed() < AWAY_REPEAT && !new_status {
                    continue;
                }
            }
            let fallback = user.localpart().to_owned();
            let nick = self.win().members.iter().find(|m| m.user_id == user).map_or(fallback, |m| m.name.clone());
            let name = self.display_name(Some(&user), &nick).to_owned();
            let line = match &status {
                Some(status) => format!("{name} is {text}: {status}"),
                None => format!("{name} is {text}"),
            };
            self.away_noted.insert(user, (status.unwrap_or_default(), Instant::now()));
            self.info(line);
        }
    }

    /// Whether `user` said something in the last few minutes, in any of
    /// `account`'s windows: then they're around, whatever their presence
    /// says.
    fn spoke_lately(&self, account: &str, user: &UserId) -> bool {
        let since = Local::now() - SPOKE_LATELY;
        self.windows.iter().filter(|w| w.account() == Some(account)).any(|w| {
            w.lines
                .iter()
                .rev()
                .take_while(|l| l.ts >= since)
                .any(|l| l.is_message() && l.sender.as_deref() == Some(user))
        })
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
        if args.trim().is_empty() {
            return Err("usage: /upload <path>".into());
        }
        self.upload_file(parse_path(args))
    }

    fn upload_file(&mut self, path: PathBuf) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
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

    /// `/play [nick | stop]`: the latest audio or video here (from nick).
    fn cmd_play(&mut self, args: &str) -> Result<(), String> {
        let arg = args.trim();
        if arg == "stop" {
            let (_, playing) = self.playing.take().ok_or("nothing is playing")?;
            playing.abort();
            self.info("stopped");
            return Ok(());
        }
        let nick = Some(arg).filter(|n| !n.is_empty());
        let line = self
            .win()
            .lines
            .iter()
            .rev()
            .find(|l| {
                l.media.as_ref().is_some_and(|m| m.playable())
                    && !l.redacted
                    && nick.is_none_or(|n| l.nick.eq_ignore_ascii_case(n))
            })
            .cloned()
            .ok_or_else(|| match nick {
                Some(nick) => format!("no audio or video from {nick} here"),
                None => "no audio or video here".to_owned(),
            })?;
        self.play(&line)
    }

    /// Play a voice message, other audio, or a video. Audio already
    /// playing stops (and playing it again just stops it).
    fn play(&mut self, line: &Line) -> Result<(), String> {
        let (media, event) = match (&line.media, &line.event_id) {
            (Some(media), Some(event)) if media.playable() && !line.redacted => (media.clone(), event.clone()),
            _ => return Err("that message isn't audio or video".into()),
        };
        let nick = self.display_name(line.sender.as_deref(), &line.nick).to_owned();
        if media.kind == MediaKind::Video {
            let (acct, _) = self.room_ctx()?;
            let player = audio::video_player(&self.cfg.video_player)
                .ok_or("no video player found; set video_player in config.toml (e.g. \"mpv\")")?;
            let size = media.size.map(|s| format!(" ({})", matrix::human_size(s))).unwrap_or_default();
            self.info(format!("fetching {nick}'s {}{size} for {}…", media.filename, player[0]));
            acct.play(event, media, player, self.paths.base.join("tmp"));
            return Ok(());
        }
        if let Some((playing, handle)) = self.playing.take() {
            handle.abort();
            if playing == event {
                self.info("stopped");
                return Ok(());
            }
        }
        let (acct, _) = self.room_ctx()?;
        let player = audio::player(&self.cfg.audio_player)
            .ok_or("no audio player found; set audio_player in config.toml (e.g. \"mpv --no-video\")")?;
        let what = if line.body.starts_with("[voice]") { "voice message".to_owned() } else { media.filename.clone() };
        self.playing = Some((event.clone(), acct.play(event, media, player, self.paths.base.join("tmp"))));
        self.info(format!("playing {nick}'s {what}; /play stop stops it"));
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
        let (deep, rest) = match args.trim().strip_prefix("-deep") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        let query = crate::search::Query::parse(rest)?;
        if query.is_empty() {
            return Err(crate::search::USAGE.into());
        }
        let needle = query.describe();
        self.search_hits.clear();
        self.search_out = self.win().room.clone();

        if deep {
            let rooms: Vec<(Account, OwnedRoomId)> = self
                .windows
                .iter()
                .filter(|w| w.room.as_ref().is_some_and(|k| k.thread.is_none()))
                .filter(|w| query.room.as_deref().is_none_or(|r| w.title().to_lowercase().contains(r)))
                .filter_map(|w| {
                    let key = w.room.as_ref()?;
                    Some((self.connected(&key.account)?, key.room_id.clone()))
                })
                .collect();
            if rooms.is_empty() {
                return Err("no connected rooms to search".into());
            }
            self.search_pending = rooms.len();
            self.info(format!("searching the last 500 messages of {} room(s) for {needle}…", rooms.len()));
            for (acct, room_id) in rooms {
                acct.search_history(room_id, query.text.clone(), 5);
            }
            self.search_query = Some(query);
            return Ok(());
        }

        // Newest first across every window's loaded history.
        let this: &Self = self;
        let mut found: Vec<(WinKey, String, Line)> = this
            .windows
            .iter()
            .filter_map(|w| Some((w.room.clone()?, w)))
            .flat_map(|(key, w)| {
                let query = &query;
                w.lines
                    .iter()
                    .filter(|l| l.event_id.is_some() && l.is_message() && !l.redacted)
                    .filter(move |l| this.search_matches(query, w.title(), l))
                    .map(move |l| (key.clone(), w.title().to_owned(), l.clone()))
            })
            .collect();
        let (indexed, reading) = self.search_indexes(&query, &mut found);
        found.sort_by_key(|(_, _, line)| std::cmp::Reverse(line.ts));
        let more = found.len().saturating_sub(50);
        found.truncate(50);
        let searched = match (indexed, reading) {
            (0, _) => String::new(),
            (n, false) => format!(" (searched {n} indexed messages)"),
            (n, true) => format!(" (searched {n} indexed messages; older history is still being indexed)"),
        };
        if found.is_empty() {
            let hint = if indexed == 0 { "; try /search -deep" } else { "" };
            self.info(format!("no messages match {needle}{searched}{hint}"));
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
        let more = if more > 0 { format!("{more} more not shown; ") } else { String::new() };
        self.info(format!("{more}/jump N goes to a result{searched}"));
        Ok(())
    }

    /// Add each connected server's search index hits to `found` (the ones
    /// not already there). Returns how many messages the indexes hold, and
    /// whether any is still reading older history.
    /// Whether a loaded message matches a search, by its room's name.
    fn search_matches(&self, query: &crate::search::Query, room: &str, line: &Line) -> bool {
        let sender = line.sender.as_ref().map(|s| s.as_str()).unwrap_or_default();
        let nickname = self.display_name(line.sender.as_deref(), &line.nick);
        query.matches(room, &[&line.nick, nickname], sender, line.ts.timestamp_millis(), &line.text())
    }

    fn search_indexes(&self, query: &crate::search::Query, found: &mut Vec<(WinKey, String, Line)>) -> (usize, bool) {
        let (mut indexed, mut reading) = (0, false);
        let have: HashSet<OwnedEventId> = found.iter().filter_map(|(_, _, l)| l.event_id.clone()).collect();
        for (name, state) in &self.accounts {
            let Some(acct) = state.account.as_ref() else { continue };
            // Room names, looked up once each, and only for `in:`.
            let titles: std::cell::RefCell<HashMap<String, String>> = Default::default();
            let title = |room: &str| -> String {
                if let Some(title) = titles.borrow().get(room) {
                    return title.clone();
                }
                let title = OwnedRoomId::try_from(room)
                    .ok()
                    .and_then(|id| {
                        let open = self.window_idx(name, &id).map(|i| self.windows[i].title().to_owned());
                        open.or_else(|| acct.quick_meta(&id).map(|m| m.name))
                    })
                    .unwrap_or_else(|| room.to_owned());
                titles.borrow_mut().insert(room.to_owned(), title.clone());
                title
            };
            let matches = |e: &crate::index::Entry| {
                let room = if query.room.is_some() { title(&e.room) } else { String::new() };
                let nickname =
                    OwnedUserId::try_from(e.sender.as_str()).ok().and_then(|u| self.layout.nicknames.get(&u).cloned());
                let names = [e.nick.as_str(), nickname.as_deref().unwrap_or_default()];
                query.matches(&room, &names, &e.sender, e.ts, &e.body)
            };
            let Some((hits, len, more)) = acct.search_index(matches, 200) else {
                continue;
            };
            (indexed, reading) = (indexed + len, reading || more);
            for entry in hits {
                let (Ok(room_id), Ok(event)) = (OwnedRoomId::try_from(entry.room), OwnedEventId::try_from(entry.event))
                else {
                    continue;
                };
                if have.contains(&event) {
                    continue;
                }
                let room = self
                    .window_idx(name, &room_id)
                    .map(|i| self.windows[i].title().to_owned())
                    .or_else(|| state.account.as_ref()?.quick_meta(&room_id).map(|m| m.name))
                    .unwrap_or_else(|| room_id.to_string());
                let mut line = Line::new(LineKind::Message, entry.body);
                line.event_id = Some(event);
                line.nick = entry.nick;
                line.sender = OwnedUserId::try_from(entry.sender).ok();
                if let Some(ts) = chrono::TimeZone::timestamp_millis_opt(&Local, entry.ts).single() {
                    line.ts = ts;
                }
                found.push((WinKey { account: name.to_string(), room_id, thread: None }, room, line));
            }
        }
        (indexed, reading)
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
            _ => return Err("usage: /devices [rename <id> <name> | delete <id> [confirm]]".into()),
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

    /// `/themes`: the list, at the theme you have.
    fn open_theme_picker(&mut self) {
        let dir = self.paths.themes_dir();
        let mut entries: Vec<ThemeChoice> = ThemeName::ALL
            .iter()
            .map(|&(_, name)| ThemeChoice { id: name.into(), shelf: name.group(), about: name.about().to_owned() })
            .collect();
        for name in crate::theme::files(&dir) {
            let about = match ThemeFile::read(&ThemeFile::path(&dir, &name)) {
                Ok(file) => file.description.unwrap_or_else(|| "one of yours".into()),
                Err(_) => "can't be used (/theme says why)".into(),
            };
            entries.push(ThemeChoice { id: ThemeId::File(name), shelf: "yours", about });
        }
        let selected = entries.iter().position(|e| e.id == self.cfg.theme).unwrap_or(0);
        let before = (self.cfg.theme.clone(), self.cfg.palette.clone());
        self.theme_picker = Some(ThemePicker { entries, query: String::new(), selected, error: None, before });
    }

    fn on_theme_picker_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(p) = self.theme_picker.as_mut() else { return };
        let last = p.shown().len().saturating_sub(1);
        let was = p.shown().get(p.selected).map(|e| e.id.clone());
        let typed = |p: &mut ThemePicker, edit: &dyn Fn(&mut String)| {
            edit(&mut p.query);
            // Stay on the same theme if it still matches.
            p.selected = p.shown().iter().position(|e| Some(&e.id) == was.as_ref()).unwrap_or(0);
        };
        match key.code {
            Esc => return self.close_theme_picker(false),
            Char('c') if ctrl => return self.close_theme_picker(false),
            Enter => return self.close_theme_picker(true),
            Up | BackTab => p.selected = p.selected.saturating_sub(1),
            Char('p') if ctrl => p.selected = p.selected.saturating_sub(1),
            Down | Tab => p.selected = (p.selected + 1).min(last),
            Char('n') if ctrl => p.selected = (p.selected + 1).min(last),
            PageUp => p.selected = p.selected.saturating_sub(10),
            PageDown => p.selected = (p.selected + 10).min(last),
            Home => p.selected = 0,
            End => p.selected = last,
            Backspace => typed(p, &|q| {
                q.pop();
            }),
            Char('u') if ctrl => typed(p, &|q| q.clear()),
            Char(c) if !ctrl && !alt => typed(p, &|q| q.push(c)),
            _ => return,
        }
        self.try_picked_theme();
    }

    /// Show the chosen theme on the whole screen.
    fn try_picked_theme(&mut self) {
        let Some(p) = self.theme_picker.as_mut() else { return };
        p.selected = p.selected.min(p.shown().len().saturating_sub(1));
        let Some(id) = p.shown().get(p.selected).map(|e| e.id.clone()) else { return };
        match Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()) {
            Ok(palette) => {
                (self.cfg.palette, self.cfg.theme, p.error) = (palette, id, None);
                self.redraw_all = true;
            }
            Err(e) => p.error = Some(format!("{e:#}")),
        }
    }

    /// Enter keeps the theme on screen for the session; Esc puts back the
    /// one you had.
    fn close_theme_picker(&mut self, keep: bool) {
        let Some(p) = self.theme_picker.take() else { return };
        if keep && p.error.is_none() {
            let changed = if *p.had() == self.cfg.theme { "" } else { " for this session; /save keeps it" };
            self.info(format!("theme: {}{changed}", self.cfg.theme));
        } else {
            if let Some(error) = &p.error {
                self.error(error.clone());
            }
            (self.cfg.theme, self.cfg.palette) = p.before;
        }
        self.redraw_all = true;
    }

    fn cmd_theme(&mut self, args: &str) -> Result<(), String> {
        let args = args.trim();
        let (sub, rest) = args.split_once(' ').map_or((args, ""), |(a, b)| (a, b.trim()));
        match sub {
            "" => {
                self.open_theme_picker();
                return Ok(());
            }
            "list" => {
                self.list_themes();
                return Ok(());
            }
            "save" => return self.save_theme(rest),
            "import" => return self.import_theme(rest),
            _ => {}
        }
        let id = ThemeId::parse(args).map_err(|e| format!("{e:#}"))?;
        self.cfg.palette =
            Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()).map_err(|e| format!("{e:#}"))?;
        self.cfg.theme = id;
        self.redraw_all = true;
        Ok(())
    }

    fn list_themes(&mut self) {
        let custom = if self.cfg.colors.is_empty() { "" } else { ", with your [colors] changes" };
        self.info(format!("theme: {}{custom}", self.cfg.theme));
        let mut shelves: Vec<(&str, Vec<&str>)> = Vec::new();
        for (name, theme) in ThemeName::ALL {
            match shelves.last_mut() {
                Some((group, names)) if *group == theme.group() => names.push(name),
                _ => shelves.push((theme.group(), vec![name])),
            }
        }
        for (group, names) in shelves {
            self.info(format!("  {group}: {}", names.join(", ")));
        }
        let dir = self.paths.themes_dir();
        let yours = crate::theme::files(&dir);
        if !yours.is_empty() {
            self.info(format!("  yours, in {}:", dir.display()));
        }
        for name in yours {
            let about = match ThemeFile::read(&ThemeFile::path(&dir, &name)) {
                Ok(file) => file.description.map(|d| format!(": {d}")).unwrap_or_default(),
                Err(e) => format!(" (can't be used: {})", e.root_cause()),
            };
            self.info(format!("    {name}{about}"));
        }
        self.info(
            "/theme on its own picks one with a preview; /theme <name> tries one (theme = \"<name>\" in \
             config.toml keeps it); /theme save <name> writes this look to a theme file to share, /theme import \
             <file> adds one",
        );
    }

    /// `/theme save <name>`: the theme in use, with your `[colors]`, as a
    /// theme file.
    fn save_theme(&mut self, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("usage: /theme save <name>".into());
        }
        if !matches!(ThemeId::parse(name).map_err(|e| format!("{e:#}"))?, ThemeId::File(_)) {
            return Err(format!("{name} is a built-in theme; pick another name"));
        }
        if THEME_WORDS.contains(&name) {
            return Err(format!("/theme {name} means something else; pick another name"));
        }
        let dir = self.paths.themes_dir();
        let path = ThemeFile::path(&dir, name);
        if path.exists() && self.cfg.theme != ThemeId::File(name.to_owned()) {
            return Err(format!("there's already a theme {name} ({}); pick another name", path.display()));
        }
        let mut file = ThemeFile::of(&self.cfg.theme, &self.cfg.colors, &dir).map_err(|e| format!("{e:#}"))?;
        file.name = Some(name.to_owned());
        let text = file.to_toml(name).map_err(|e| format!("{e:#}"))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&tmp, text))
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        self.info(format!("saved this look as {}: /theme {name} uses it, and the file shares it", path.display()));
        Ok(())
    }

    /// `/theme import <file>`: a theme file someone sent you, checked and
    /// copied into the themes folder.
    fn import_theme(&mut self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            return Err("usage: /theme import <file>".into());
        }
        let from = parse_path(arg);
        let file = ThemeFile::read(&from).map_err(|e| format!("{e:#}"))?;
        let stem = from.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let name = file.name.as_deref().unwrap_or(stem);
        match ThemeId::parse(name).map_err(|e| format!("{e:#}"))? {
            ThemeId::File(_) if !THEME_WORDS.contains(&name) => {}
            _ => return Err(format!("{name} is a built-in theme's name; change the file's name = line")),
        }
        let dir = self.paths.themes_dir();
        let to = ThemeFile::path(&dir, name);
        if to.exists() {
            return Err(format!("there's already a theme {name} ({})", to.display()));
        }
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::copy(&from, &to))
            .map_err(|e| format!("copying it to {}: {e}", to.display()))?;
        self.info(format!("added theme {name}: /theme {name} tries it"));
        Ok(())
    }

    /// Show `line`'s image full screen; the original is fetched for it.
    fn open_viewer(&mut self, line: &Line) -> Result<(), String> {
        let media = line
            .media
            .clone()
            .filter(|m| m.kind == MediaKind::Image && !line.redacted)
            .ok_or("that message has no image")?;
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
        let split = self.split.get_or_insert_with(|| Split::new(vec![current.clone()], side_by_side));
        if split.panes.len() >= PANES_MAX {
            return Err(format!("{PANES_MAX} panes at most; /unsplit closes this one"));
        }
        split.side_by_side = side_by_side;
        let slot = split.panes.iter().position(|k| *k == current).map_or(split.panes.len(), |i| i + 1);
        split.insert(slot, self.windows[idx].id());
        self.windows[idx].activity = Activity::None;
        Ok(())
    }

    /// What's on screen, as a layout to save.
    fn current_layout(&self, panels: bool) -> SavedLayout {
        SavedLayout {
            panes: self.split.as_ref().map(|s| s.panes.clone()).unwrap_or_default(),
            side_by_side: self.split.as_ref().is_some_and(|s| s.side_by_side),
            focus: Some(self.windows[self.active].id()),
            sizes: self.split.as_ref().map(Split::weights).unwrap_or_default(),
            sidebar: panels.then_some(self.show_sidebar),
            nicklist: panels.then_some(self.show_nicklist),
        }
    }

    /// Put a layout on screen: its panes whose windows are open, its focus
    /// and its panels. Returns how many panes couldn't be shown.
    fn apply_layout(&mut self, saved: &SavedLayout) -> usize {
        let open = |app: &Self, r: &WinRef| app.windows.iter().position(|w| w.id() == *r);
        let present: Vec<WinRef> = saved.panes.iter().filter(|r| open(self, r).is_some()).cloned().collect();
        // Sizes for the panes that are there (if they were saved).
        let sizes: Vec<u16> = match saved.sizes.len() == saved.panes.len() {
            true => {
                saved.panes.iter().zip(&saved.sizes).filter(|(r, _)| open(self, r).is_some()).map(|(_, s)| *s).collect()
            }
            false => vec![PANE_SHARE; present.len()],
        };
        let missing = saved.panes.len() - present.len();
        self.show_sidebar = saved.sidebar.unwrap_or(self.show_sidebar);
        self.show_nicklist = saved.nicklist.unwrap_or(self.show_nicklist);
        self.split = None;
        if let Some(idx) = saved.focus.as_ref().and_then(|f| open(self, f)) {
            self.switch_to(idx);
        }
        if present.len() >= 2 {
            let first = present[0].clone();
            self.split = Some(Split { panes: present, side_by_side: saved.side_by_side, sizes });
            // The focus is always one of the panes.
            if !self.split.as_ref().is_some_and(|s| s.panes.contains(&self.windows[self.active].id()))
                && let Some(idx) = open(self, &first)
            {
                self.switch_to(idx);
            }
        } else if let Some(idx) = present.first().and_then(|r| open(self, r)) {
            self.switch_to(idx);
        }
        missing
    }

    /// Last session's split, once its windows are open (or their servers
    /// have said which rooms they have: a room you've left is skipped).
    fn restore_split(&mut self) {
        self.resume();
        let Some(saved) = &self.pending_split else { return };
        let waiting = saved.panes.iter().chain(&saved.focus).any(|r| match r {
            WinRef::Room(key) => {
                !self.windows.iter().any(|w| w.id() == *r)
                    && !self.rooms_loaded.contains(&key.account)
                    && self.accounts.get(key.account.as_str()).is_some_and(|s| s.status != ConnStatus::Offline)
            }
            _ => false,
        });
        if waiting {
            return;
        }
        if let Some(saved) = self.pending_split.take() {
            self.apply_layout(&saved);
        }
    }

    /// After `/restart`: back to the window you were in, with what you were
    /// typing, once that window's there (or its server has said it isn't).
    fn resume(&mut self) {
        let Some(mut resume) = self.pending_resume.take() else { return };
        let open = |app: &Self, r: &WinRef| app.windows.iter().position(|w| w.id() == *r);
        // A room whose server hasn't said what rooms it has may still come.
        let coming = |app: &Self, r: &WinRef| {
            matches!(r, WinRef::Room(key)
                if !app.rooms_loaded.contains(&key.account)
                    && app.accounts.get(key.account.as_str()).is_some_and(|s| s.status != ConnStatus::Offline))
        };
        // The window first: switching saves the input line into the window
        // left, which would cover a draft put back there.
        if let Some(window) = resume.window.take() {
            match open(self, &window) {
                Some(idx) => self.switch_to(idx),
                None if coming(self, &window) => resume.window = Some(window),
                None => {}
            }
        }
        let mut later = Vec::new();
        for (window, text) in std::mem::take(&mut resume.drafts) {
            match open(self, &window) {
                // The window you're in has its draft in the input line.
                Some(idx) if idx == self.active => self.input.restore(crate::input::Draft::from_text(&text)),
                Some(idx) => {
                    let draft = Some(crate::input::Draft::from_text(&text));
                    self.windows[idx].unsent = Some(Unsent { text: draft, compose: None });
                }
                None if coming(self, &window) => later.push((window, text)),
                None => {}
            }
        }
        resume.drafts = later;
        if resume.window.is_some() || !resume.drafts.is_empty() {
            self.pending_resume = Some(resume);
        }
    }

    /// Every half minute: has the textrix on disk changed (`brew upgrade`,
    /// `cargo build`)? Then ask it its version.
    fn watch_install(&mut self) {
        let Some(install) = self.install.clone().filter(|i| i.replaced()) else { return };
        if std::mem::replace(&mut self.probing, true) {
            return;
        }
        // From now on, changes are measured from this one.
        self.install = crate::install::Install::at(install.path.clone()).or(Some(install.clone()));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let version = install.version().await;
            let _ = tx.send(AppEvent::Installed { version });
        });
    }

    /// `/restart`: save everything as on quitting, and become the textrix
    /// on disk (a newer one, if it's been upgraded), back where you were.
    fn cmd_restart(&mut self) -> Result<(), String> {
        if self.install.is_none() {
            return Err("can't find the textrix this was started as, to start it again".into());
        }
        let mut drafts: Vec<(WinRef, String)> =
            self.windows.iter().filter_map(|w| Some((w.id(), w.unsent.as_ref()?.text.as_ref()?.text()))).collect();
        if !self.input.is_empty() {
            drafts.push((self.windows[self.active].id(), self.input.text()));
        }
        let resume = crate::install::Resume { window: Some(self.windows[self.active].id()), drafts };
        let json = serde_json::to_vec(&resume).map_err(|e| e.to_string())?;
        crate::config::write_private(&crate::install::resume_file(&self.paths.base), &json)
            .map_err(|e| format!("{e:#}"))?;
        (self.restart, self.quit) = (true, true);
        Ok(())
    }

    /// What the status bar says about upgrades: one installed under you,
    /// or one released.
    pub fn upgrade_note(&self) -> Option<String> {
        match (&self.installed, &self.released) {
            (Some(v), _) if v.is_empty() => Some("new build: /restart".into()),
            (Some(v), _) => Some(format!("{v} installed: /restart")),
            (None, Some(v)) => Some(format!("{v} is out")),
            _ => None,
        }
    }

    /// `/layout [save|load|delete <name>]`: named arrangements of split
    /// panes and panels.
    fn cmd_layout(&mut self, args: &str) -> Result<(), String> {
        let (sub, name) = split_first(args.trim());
        let name = name.trim();
        match (sub, name) {
            ("" | "list", _) => {
                if self.layout.layouts.is_empty() {
                    self.info("no saved layouts; /layout save <name> keeps the panes and panels you have now");
                }
                let rows: Vec<String> = self
                    .layout
                    .layouts
                    .iter()
                    .map(|(name, l)| {
                        let panes: Vec<String> = l.panes.iter().map(|r| self.ref_title(r)).collect();
                        let panes = if panes.is_empty() { "one window".to_owned() } else { panes.join(" | ") };
                        format!("  {name}: {panes}{}", if l.side_by_side { " (side by side)" } else { "" })
                    })
                    .collect();
                rows.into_iter().for_each(|row| self.info(row));
            }
            ("save", name) if !name.is_empty() => {
                self.layout.layouts.insert(name.to_owned(), self.current_layout(true));
                self.save_layout();
                self.info(format!("layout {name} saved; /layout load {name} brings it back"));
            }
            ("load", name) if !name.is_empty() => {
                let saved = self.layout.layouts.get(name).cloned().ok_or_else(|| format!("no layout {name}"))?;
                self.pending_split = None;
                match self.apply_layout(&saved) {
                    0 => {}
                    n => self.info(format!("{n} of its windows aren't open, so they were left out")),
                }
            }
            ("delete", name) if !name.is_empty() => {
                self.layout.layouts.remove(name).ok_or_else(|| format!("no layout {name}"))?;
                self.save_layout();
                self.info(format!("layout {name} deleted"));
            }
            (name, "") if self.layout.layouts.contains_key(name) => return self.cmd_layout(&format!("load {name}")),
            _ => return Err("usage: /layout [save <name> | load <name> | delete <name>]".into()),
        }
        Ok(())
    }

    /// A window's name, by identity.
    fn ref_title(&self, r: &WinRef) -> String {
        match self.windows.iter().find(|w| w.id() == *r) {
            Some(w) => w.title().to_owned(),
            None => match r {
                WinRef::Status => "status".into(),
                WinRef::Mentions => "mentions".into(),
                WinRef::Room(key) => key.room_id.to_string(),
            },
        }
    }

    /// `/resize [+N | -N | =]`: grow or shrink the focused pane by N rows
    /// (columns side by side), or make them all the same.
    fn cmd_resize(&mut self, args: &str) -> Result<(), String> {
        const USAGE: &str = "usage: /resize +N | -N | = (the focused pane, in rows or columns)";
        let Some(split) = &mut self.split else { return Err("nothing is split; /split <window> adds a pane".into()) };
        let arg = args.trim();
        if arg == "=" {
            split.sizes = vec![PANE_SHARE; split.panes.len()];
            return Ok(());
        }
        let delta: i32 = arg.strip_prefix('+').unwrap_or(arg).parse().map_err(|_| USAGE)?;
        let active = self.windows[self.active].id();
        let slot = split.panes.iter().position(|k| *k == active).ok_or(USAGE)?;
        let k = self.hits.splits.iter().position(|(_, s)| *s == slot).ok_or("the panes aren't drawn yet")?;
        // Against the pane after it, or the one before for the last.
        let (a, b) = if k + 1 < self.hits.splits.len() { (k, k + 1) } else { (k.saturating_sub(1), k) };
        let cells = |r: Rect| if split.side_by_side { r.width } else { r.height };
        let (ca, cb) = (cells(self.hits.splits[a].0), cells(self.hits.splits[b].0));
        let grown = i32::from(if a == k { ca } else { cb }) + delta;
        let new_k = self.clamp_pane(grown, ca + cb);
        let new_a = if a == k { new_k } else { ca + cb - new_k };
        self.set_divider(a, new_a);
        Ok(())
    }

    /// A pane's size kept between the smallest a pane can be and what
    /// leaves the other pane its smallest.
    fn clamp_pane(&self, cells: i32, pair: u16) -> u16 {
        let side = self.split.as_ref().is_some_and(|s| s.side_by_side);
        let min = if side { PANE_MIN.1 } else { PANE_MIN.0 };
        (cells.max(i32::from(min)) as u16).min(pair.saturating_sub(min).max(min))
    }

    /// Give drawn pane `a` (of `hits.splits`) `cells`, and the one after it
    /// the rest of what the two had.
    fn set_divider(&mut self, a: usize, cells: u16) {
        let Some(split) = &mut self.split else { return };
        if a + 1 >= self.hits.splits.len() {
            return;
        }
        let size = |r: Rect| if split.side_by_side { r.width } else { r.height };
        let pair = size(self.hits.splits[a].0) + size(self.hits.splits[a + 1].0);
        let mut sizes = split.weights();
        // Shares in cells, as drawn, so the two change and the rest stay.
        for (area, slot) in &self.hits.splits {
            if let Some(s) = sizes.get_mut(*slot) {
                *s = size(*area).max(1);
            }
        }
        let (sa, sb) = (self.hits.splits[a].1, self.hits.splits[a + 1].1);
        if let (true, true) = (sa < sizes.len(), sb < sizes.len()) {
            sizes[sa] = cells.max(1);
            sizes[sb] = pair.saturating_sub(cells).max(1);
        }
        split.sizes = sizes;
    }

    /// The divider (between drawn panes `k` and `k + 1`) at `at`, if any:
    /// the line or bar between them.
    fn divider_at(&self, at: Position) -> Option<usize> {
        let side = self.split.as_ref()?.side_by_side;
        // Framed panes meet at two frame lines; plain ones at the line on
        // the right-hand pane's edge, or the upper pane's title bar.
        let framed = self.cfg.palette.outlines.framed();
        self.hits.splits.windows(2).position(|pair| {
            let (a, b) = (pair[0].0, pair[1].0);
            if side {
                let from = if framed { a.right().saturating_sub(1) } else { b.x };
                (from..=b.x).contains(&at.x) && (a.y..a.bottom()).contains(&at.y)
            } else {
                let to = if framed { b.y } else { a.bottom().saturating_sub(1) };
                (a.bottom().saturating_sub(1)..=to).contains(&at.y) && (a.x..a.right()).contains(&at.x)
            }
        })
    }

    fn cmd_unsplit(&mut self, args: &str) -> Result<(), String> {
        let Some(split) = &mut self.split else { return Err("nothing is split; /split <window> adds a pane".into()) };
        if args.trim() == "all" {
            self.split = None;
            return Ok(());
        }
        let current = self.windows[self.active].id();
        let slot = split.panes.iter().position(|k| *k == current).unwrap_or(0);
        split.remove(slot);
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

    /// Switch to new settings, from config.toml or /set.
    fn apply_config(&mut self, cfg: Config) {
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
        if cfg.image_text != self.cfg.image_text
            && let Some(painter) = self.picker.as_mut()
        {
            *painter = Painter::new(painter.picker.clone(), cfg.image_text);
            self.previews.clear();
            self.viewer = None;
        }
        if cfg.theme != self.cfg.theme || cfg.colors != self.cfg.colors {
            self.redraw_all = true;
        }
        self.scripts.configure(&cfg.script_settings);
        self.cfg = cfg;
        self.apply_logging();
    }

    /// The settings as they stand in this session, after /theme, /bell,
    /// F2, F3 and the like.
    fn effective_config(&self) -> Config {
        let mut cfg = self.cfg.clone();
        (cfg.bell, cfg.bell_style) = (self.bell_mode, self.bell_style);
        (cfg.sidebar, cfg.nicklist) = (self.show_sidebar, self.show_nicklist);
        cfg.script_settings = self.scripts.settings_table();
        cfg
    }

    /// `/set`: list the settings, show one, or change one for the session
    /// (checked like config.toml). Values are TOML (`12`, `true`,
    /// `["mentions"]`); anything else is taken as text.
    fn cmd_set(&mut self, args: &str) -> Result<(), String> {
        let current = self.effective_config().to_table().map_err(|e| format!("{e:#}"))?;
        let (key, value) = split_first(args.trim());
        let value = value.trim();
        let script_settings = self.scripts.settings();
        if key.is_empty() {
            self.info("Settings (/set <name> <value> changes one for this session, /save keeps them):");
            let mut rows: Vec<String> = current
                .iter()
                .filter(|(k, _)| *k != "script_settings")
                .map(|(k, v)| match v.as_table() {
                    Some(t) => {
                        format!("  [{k}] {} entr{} (edit config.toml)", t.len(), if t.len() == 1 { "y" } else { "ies" })
                    }
                    None => format!("  {k} = {v}"),
                })
                .collect();
            if !script_settings.is_empty() {
                rows.push("Script settings:".into());
                rows.extend(script_settings.iter().map(|(k, v, help)| format!("  {k} = {v}  ({help})")));
            }
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        let parse = |value: &str| {
            toml::from_str::<toml::Table>(&format!("v = {value}"))
                .ok()
                .and_then(|mut t| t.remove("v"))
                .unwrap_or_else(|| toml::Value::String(value.to_owned()))
        };
        if let Some((_, now, help)) = script_settings.iter().find(|(k, _, _)| k == key) {
            if value.is_empty() {
                self.info(format!("{key} = {now}  ({help})"));
            } else {
                let set = self.scripts.set_setting(key, parse(value))?;
                self.info(format!("{key} = {set} for this session; /save keeps it"));
            }
            return Ok(());
        }
        if value.is_empty() {
            let text = match current.get(key) {
                Some(v) => format!("{key} = {v}"),
                None => format!("no setting {key}; /set lists them"),
            };
            self.info(text);
            return Ok(());
        }
        if current.get(key).is_some_and(toml::Value::is_table) {
            return Err(format!("[{key}] is a table: edit it in config.toml, then /reload"));
        }
        let parsed = parse(value);
        let mut table = current;
        table.insert(key.to_owned(), parsed.clone());
        let cfg = Config::from_table(table, &self.paths.themes_dir()).map_err(|e| format!("{key}: {e:#}"))?;
        self.apply_config(cfg);
        self.info(format!("{key} = {parsed} for this session; /save keeps it"));
        Ok(())
    }

    /// `/save`: write this session's settings into config.toml.
    fn cmd_save(&mut self) -> Result<(), String> {
        let path = self.paths.config_file();
        let written = crate::config::save_settings(&path, &self.effective_config()).map_err(|e| format!("{e:#}"))?;
        let text = match written.len() {
            0 => format!("{} already has these settings", path.display()),
            _ => format!("saved {} to {}", written.join(", "), path.display()),
        };
        self.info(text);
        Ok(())
    }

    /// `/reload`: apply config.toml and the dotfile without restarting. A
    /// broken file leaves the current settings in place.
    fn cmd_reload(&mut self) {
        match Config::load(&self.paths.config_file()) {
            Ok(cfg) => {
                self.apply_config(cfg);
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
            let scripted = self.scripts.commands();
            if !scripted.is_empty() {
                self.info("Script commands:");
                for (name, help, script) in scripted {
                    self.info(format!("  /{name:<10} {help} ({script}.rhai)"));
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

/// "45s", "2m 13s", "1h 5m".
fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// `<image>` or `-remove` for /avatar and /roomavatar.
fn avatar_arg(args: &str, command: &str) -> Result<Option<PathBuf>, String> {
    match args.trim() {
        "" => Err(format!("usage: {command} <image> | -remove")),
        "-remove" => Ok(None),
        path => Ok(Some(parse_path(path))),
    }
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

/// Files dropped (or pasted) on the terminal: `text` is nothing but
/// absolute paths to files that exist, as terminals write them (quoted or
/// backslash-escaped, or `file://` URLs; separated by spaces or lines).
fn dropped_files(text: &str) -> Option<Vec<PathBuf>> {
    let file = |word: &str| {
        let bare = word.trim_start_matches(['\'', '"']);
        let absolute = bare.starts_with('/') || bare.starts_with("~/") || bare.starts_with("file://");
        let path = parse_path(word);
        (absolute && std::fs::metadata(&path).is_ok_and(|m| m.is_file())).then_some(path)
    };
    let words = split_unescaped(text);
    if words.is_empty() || words.len() > 20 {
        return None;
    }
    // Some terminals type one path with its spaces unescaped.
    words.iter().map(|word| file(word)).collect::<Option<Vec<_>>>().or_else(|| Some(vec![file(text.trim())?]))
}

/// Split at whitespace that isn't backslash-escaped or quoted, keeping
/// each piece as written.
fn split_unescaped(text: &str) -> Vec<String> {
    let (mut words, mut word, mut quote, mut escaped) = (Vec::new(), String::new(), None::<char>, false);
    for c in text.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quote != Some('\'') => {
                word.push(c);
                escaped = true;
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                word.push(c);
            }
            c if Some(c) == quote => {
                quote = None;
                word.push(c);
            }
            // Only plain spaces and line breaks: file names can hold others
            // (macOS puts a narrow no-break space before "PM" in screenshots).
            ' ' | '\t' | '\n' | '\r' if quote.is_none() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
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
    let mut out = Vec::new();
    let result =
        if on { crossterm::queue!(out, EnableMouseCapture) } else { crossterm::queue!(out, DisableMouseCapture) };
    match result {
        Ok(()) => crate::term::write(&out),
        Err(e) => tracing::warn!("mouse capture: {e}"),
    }
}

async fn flash_end(until: Option<Instant>) {
    match until {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// `keys` is the terminal's input: this terminal's, or in the background,
/// the attached one's.
pub async fn run<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut rx: UnboundedReceiver<AppEvent>,
    mut keys: impl Stream<Item = std::io::Result<Event>> + Unpin,
) -> Result<bool>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    app.start();
    loop {
        // In the background with no terminal attached, nothing is drawn.
        let shown = crate::term::attached();
        if shown {
            if std::mem::take(&mut app.redraw_all) {
                terminal.clear()?;
                app.last_links.clear();
            }
            let frame = terminal.draw(|f| ui::draw(f, &mut app))?;
            // Unchanged links keep their hyperlinks: the frame left them be.
            let links = if app.cfg.hyperlinks {
                crate::hyperlinks::overlay(frame.buffer, &app.screen_links())
            } else {
                String::new()
            };
            if links != app.last_links {
                crate::term::write(links.as_bytes());
                app.last_links = links;
            }
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
        }
        if std::mem::take(&mut app.bell) {
            crate::term::write(b"\x07");
        }
        for (title, body) in std::mem::take(&mut app.notifications) {
            if shown {
                notify::send(app.notify_mode, &title, &body);
            } else {
                notify::send_system(&title, &body);
            }
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
            _ = clock.tick() => app.on_clock(),
            // Redraw when the visual bell's flash ends.
            _ = flash_end(app.flash_until) => app.flash_until = None,
        }
    }
    let restart = app.restart;
    app.shutdown();
    Ok(restart)
}

#[cfg(test)]
mod shots;

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
            kind: crate::model::MediaKind::File,
            mimetype: None,
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
        w.apply(
            Item::React { target: "$a".try_into().unwrap(), key: "👍".into(), nick: "bob".into(), own: false },
            100,
            true,
        );
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
    fn dropped_files_are_offered_for_upload() {
        let dir = std::env::temp_dir().join(format!("textrix-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("My Notes.pdf"), dir.join("song.flac"));
        std::fs::write(&a, b"%PDF").unwrap();
        std::fs::write(&b, vec![0u8; 2048]).unwrap();
        let escaped = |p: &std::path::Path| p.display().to_string().replace(' ', "\\ ");
        let both = format!("{} '{}'", escaped(&a), b.display());
        assert_eq!(dropped_files(&both), Some(vec![a.clone(), b.clone()]), "escaped and quoted, any kind of file");
        let urls = format!("file://{}\nfile://{}", a.display().to_string().replace(' ', "%20"), b.display());
        assert_eq!(dropped_files(&urls), Some(vec![a.clone(), b.clone()]), "file:// URLs, one per line");
        assert_eq!(dropped_files(&format!("see {}", escaped(&a))), None, "text with a path in it is text");
        assert_eq!(dropped_files(&dir.display().to_string()), None, "folders aren't uploaded");
        assert_eq!(dropped_files("Cargo.toml"), None, "only absolute paths, as terminals drop them");
        let shot = dir.join("Screenshot 2026-10-18 at 9.31.02\u{202f}PM.png");
        std::fs::write(&shot, b"png").unwrap();
        assert_eq!(dropped_files(&escaped(&shot)), Some(vec![shot.clone()]), "a narrow space isn't a separator");
        assert_eq!(
            dropped_files(&shot.display().to_string()),
            Some(vec![shot.clone()]),
            "nor are unescaped spaces in one path"
        );
        app_hint_for_paths(&dir);

        let mut app = test_app();
        let room = room_window(&mut app, "!a:x.org", "Night Owls");
        app.switch_to(room);
        app.on_terminal(Event::Paste(both.clone()));
        assert_eq!(app.confirm_upload.as_deref(), Some(&[a.clone(), b.clone()][..]));
        assert_eq!(app.upload_summary().as_deref(), Some("2 files (2.0 KB)"));
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.confirm_upload.is_none());
        assert_eq!(app.input.text(), both, "declined: the paths stay, as text");
        app.submit();
        assert!(app.confirm_upload.is_none(), "and Enter doesn't ask again");
        let last = &app.windows[room].lines.back().unwrap().body;
        assert!(!last.contains("command"), "it's sent as a message, not run as a command: {last}");

        app.input.take();
        app.input.set_text(&escaped(&a));
        app.submit();
        assert!(app.confirm_upload.is_some(), "a typed path asks on Enter instead of running /Users…");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty() && app.confirm_upload.is_none());
        let last = &app.windows[room].lines.back().unwrap().body;
        assert!(last.contains("not connected"), "it tried to upload: {last}");

        app.switch_to(0);
        app.on_terminal(Event::Paste(escaped(&a)));
        assert!(app.confirm_upload.is_none(), "not outside a room");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A path that isn't a file, typed as if a command, says so.
    fn app_hint_for_paths(dir: &std::path::Path) {
        let mut app = test_app();
        app.command(format!("{}/nothing.pdf", dir.display()).trim_start_matches('/'));
        let last = &app.windows[0].lines.back().unwrap().body;
        assert!(last.contains("isn't a command, and not a file here"), "{last}");
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
    fn textrix_lines_stand_apart_in_rooms() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        app.switch_to(a);
        let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
        let say = |app: &mut App, id: &str, body: &str| {
            let items = vec![Item::Line(msg(id, body))];
            app.on_event(AppEvent::Timeline {
                account: "t".into(),
                room_id: room.clone(),
                items,
                live: true,
                limited: false,
            });
        };
        say(&mut app, "$1", "before");
        app.info("theme: dracula");
        app.error("that went wrong");
        say(&mut app, "$2", "after");
        app.show_sidebar = false;
        app.show_nicklist = false;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> =
            (0..buf.area.height).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
        let at = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap_or_else(|| panic!("{text}: {rows:#?}"));
        let (before, theme, wrong, after) = (at("before"), at("theme: dracula"), at("that went wrong"), at("after"));
        assert_eq!([theme - before, wrong - theme, after - wrong], [2, 1, 2], "a blank row either side of the run");
        assert!(rows[before + 1].trim().is_empty() && rows[wrong + 1].trim().is_empty());
        let x = rows[theme].find("theme").unwrap() as u16;
        let cell = &buf[(x, theme as u16)];
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::ITALIC) && cell.fg == app.cfg.palette.dim.fg.unwrap(),
            "quieter"
        );
        assert!(
            buf[(rows[wrong].find("that").unwrap() as u16, wrong as u16)]
                .modifier
                .contains(ratatui::style::Modifier::ITALIC)
        );
    }

    #[test]
    fn the_theme_picker_shows_each_theme_as_you_move() {
        use crate::theme::ThemeName;
        let dir = std::env::temp_dir().join(format!("textrix-picker-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(dir.join("themes/broken.toml"), "base = \"plaid\"").unwrap();
        let mut app = test_app();
        app.paths.base = dir.clone();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let draw = |app: &mut App| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
            terminal.draw(|f| ui::draw(f, app)).unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        app.command("theme");
        assert!(app.theme_picker.is_some(), "/theme on its own opens it too");
        let screen = draw(&mut app);
        assert!(screen.contains("Themes") && screen.contains("• default") && screen.contains("Classic"), "{screen}");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.cfg.theme, ThemeName::Light.into(), "moving to a theme shows it");
        assert_eq!(app.cfg.palette.pane, Theme::build(ThemeName::Light, &BTreeMap::new()).unwrap().pane);
        app.on_key(key(KeyCode::Esc));
        assert!(app.theme_picker.is_none());
        assert_eq!(app.cfg.theme, ThemeName::Default.into(), "Esc puts back the one you had");

        app.command("themes");
        for c in "drac".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.cfg.theme, ThemeName::Dracula.into(), "typing narrows the list to it");
        app.on_key(key(KeyCode::Enter));
        assert!(app.theme_picker.is_none());
        assert_eq!(app.cfg.theme, ThemeName::Dracula.into(), "Enter keeps it");
        assert!(app.windows[0].lines.back().unwrap().body.contains("/save keeps it"));

        // Your files are on the list; a broken one says why and isn't kept.
        app.command("themes");
        app.on_key(key(KeyCode::End));
        assert!(app.theme_picker.as_ref().unwrap().error.as_ref().is_some_and(|e| e.contains("plaid")));
        assert_eq!(app.cfg.theme, ThemeName::Dracula.into());
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.cfg.theme, ThemeName::Dracula.into());

        // A click shows a theme, a second click keeps it.
        app.command("themes");
        draw(&mut app);
        let (_, rows) = app.hits.theme_picker.clone().unwrap();
        let (rect, n) = rows.iter().find(|(_, n)| *n == 3).copied().unwrap();
        let click = |app: &mut App| app.on_click(Position::new(rect.x + 2, rect.y));
        click(&mut app);
        assert_eq!((app.theme_picker.as_ref().unwrap().selected, &app.cfg.theme), (n, &ThemeName::Green.into()));
        click(&mut app);
        assert!(app.theme_picker.is_none() && app.cfg.theme == ThemeName::Green.into());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn themes_are_saved_and_imported() {
        let dir = std::env::temp_dir().join(format!("textrix-theme-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = test_app();
        app.paths.base = dir.clone();
        app.cmd_theme("dracula").unwrap();
        app.cfg.colors.insert("link".into(), "red".into());
        app.cmd_theme("save vampire").unwrap();
        let saved = std::fs::read_to_string(dir.join("themes/vampire.toml")).unwrap();
        assert!(saved.contains("base = \"dracula\"") && saved.contains("link = \"red\""), "{saved}");
        assert!(app.cmd_theme("save nord").is_err(), "not under a built-in name");
        assert!(app.cmd_theme("save list").is_err(), "nor a word /theme takes");
        app.cmd_theme("list").unwrap();
        assert!(app.windows[0].lines.iter().any(|l| l.body.contains("vampire")), "/theme list prints them");
        app.cmd_theme("vampire").unwrap();
        assert_eq!(app.cfg.theme, ThemeId::File("vampire".into()));
        app.cmd_theme("save vampire").unwrap();
        app.cmd_theme("default").unwrap();
        assert!(app.cmd_theme("save vampire").is_err(), "not over another theme");

        // Sent to a friend, who imports it by its name = line.
        let sent = dir.join("sent.toml");
        std::fs::write(&sent, saved.replace("name = \"vampire\"", "name = \"bat\"")).unwrap();
        app.cmd_theme(&format!("import {}", sent.display())).unwrap();
        app.cmd_theme("bat").unwrap();
        assert!(app.cmd_theme(&format!("import {}", sent.display())).is_err(), "already there");
        std::fs::write(&sent, "base = \"plaid\"").unwrap();
        assert!(app.cmd_theme(&format!("import {}", sent.display())).unwrap_err().contains("plaid"));
        assert!(app.cmd_theme("nosuch").is_err());
        assert!(app.cmd_theme("../../etc/passwd").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn panes_resize_by_command_and_by_dragging() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        room_window(&mut app, "!b:x.org", "Book Club");
        app.switch_to(a);
        app.cmd_split("Book Club", false).unwrap();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let mut heights = |app: &mut App| -> Vec<u16> {
            terminal.draw(|f| ui::draw(f, app)).unwrap();
            app.hits.splits.iter().map(|(r, _)| r.height).collect()
        };
        let even = heights(&mut app);
        assert!(even.len() == 2 && even[0].abs_diff(even[1]) <= 1, "{even:?}");
        let room = even[0] + even[1];

        app.cmd_resize("+4").unwrap();
        assert_eq!(heights(&mut app), [even[0] + 4, room - even[0] - 4], "the focused pane grows");
        app.cmd_resize("-100").unwrap();
        assert_eq!(heights(&mut app)[0], PANE_MIN.0, "not past the smallest");
        app.cmd_resize("=").unwrap();
        assert_eq!(heights(&mut app), even);
        assert!(app.cmd_resize("lots").is_err());

        // Drag the upper pane's title bar down to row 20.
        let (top, bar) = (app.hits.splits[0].0.y, app.hits.splits[0].0.bottom() - 1);
        let mouse = |kind, row| MouseEvent { kind, column: 30, row, modifiers: KeyModifiers::NONE };
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), bar));
        assert_eq!(app.dragging, Some(0));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 20));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 20));
        assert_eq!(app.dragging, None);
        let dragged = heights(&mut app);
        assert_eq!(dragged[0], 20 - top + 1);

        // Saved layouts keep the sizes.
        let dir = std::env::temp_dir().join(format!("textrix-resize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        app.cmd_layout("save tall").unwrap();
        app.cmd_resize("=").unwrap();
        app.cmd_layout("tall").unwrap();
        assert_eq!(heights(&mut app), dragged);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn layouts_are_saved_and_come_back() {
        let dir = std::env::temp_dir().join(format!("textrix-layouts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
        let start = || {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                Config::default(),
                paths.clone(),
                EnvFile::new(paths.env_file.clone()),
                Servers::default(),
                tx,
            );
            let a = room_window(&mut app, "!a:x.org", "Night Owls");
            let b = room_window(&mut app, "!b:x.org", "Book Club");
            (app, a, b)
        };
        let (mut app, a, b) = start();
        app.switch_to(a);
        app.cmd_split("Book Club", true).unwrap();
        app.cmd_layout("save reading").unwrap();
        app.cmd_unsplit("all").unwrap();
        app.show_sidebar = false;
        app.cmd_layout("load reading").unwrap();
        let split = app.split.as_ref().unwrap();
        assert!(split.side_by_side && split.panes.len() == 2);
        assert!(app.show_sidebar, "the panels come back too");
        assert_eq!(app.active, a, "and the focus");
        app.cmd_unsplit("all").unwrap();
        app.cmd_layout("reading").unwrap();
        assert!(app.split.is_some(), "/layout <name> loads it");
        assert!(app.cmd_layout("load nope").is_err());

        // The split you quit with is back next time, once its rooms are.
        app.switch_to(b);
        app.save_layout();
        let (mut next, _, b) = start();
        assert!(next.split.is_none() && next.pending_split.is_some());
        next.restore_split();
        assert_eq!(next.split.as_ref().map(|s| s.panes.len()), Some(2));
        assert_eq!(next.active, b);
        assert!(next.layout.layouts.contains_key("reading"));
        next.cmd_layout("delete reading").unwrap();
        assert!(next.layout.layouts.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
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
        // Every row of a wrapped link becomes a hyperlink to the whole of it.
        let pieces =
            [(30, 40, "https://en.wikipedia".to_owned()), (2, 34, "wiki/Rust_(programming_language)".to_owned())];
        let rows = vec![
            ui::RowHit { event: Some(event.clone()), links: vec![pieces[0].clone()], image: false },
            ui::RowHit { event: Some(event.clone()), links: vec![pieces[1].clone()], image: false },
        ];
        app.hits.panes = vec![ui::PaneHits { area: ratatui::layout::Rect::new(0, 3, 40, 2), window: a, rows }];
        let links = app.screen_links();
        assert_eq!(links.iter().map(|l| (l.x, l.y, l.width)).collect::<Vec<_>>(), [(30, 3, 10), (2, 4, 32)]);
        assert!(links.iter().all(|l| l.url == url));
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
        assert_eq!(expand_alias("/window goto Team Tests", ""), ["window goto Team Tests"]);
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
        for taken in ["read /say no", "fwd /say no", "wc /say no", "shrug /say no"] {
            assert!(app.cmd_alias(taken).is_err(), "{taken}: other names for built-ins neither");
        }

        app.command("tt");
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Mono.into(), "the alias ran");
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
    fn writing_to_someone_away_says_so_when_its_news() {
        let mut app = test_app();
        let sam: OwnedUserId = "@sam:x.org".try_into().unwrap();
        let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
        let mut dm = Window::new(Some(key), "sam".into());
        (dm.is_dm, dm.dm_user) = (true, Some(sam.clone()));
        app.windows.push(dm);
        app.switch_to(1);
        let away = |status: &str| Presence { kind: PresenceKind::Away, status: Some(status.into()), last_active: None };
        let set = |app: &mut App, p: Presence| {
            app.on_event(AppEvent::Presence { account: "t".into(), updates: vec![(sam.clone(), p)] })
        };
        let notes = |app: &App| app.windows[1].lines.iter().filter(|l| l.body.contains(" is ")).count();
        let ago = |mins| Some(Local::now() - chrono::TimeDelta::minutes(mins));
        let presence =
            |kind, status: Option<&str>, last_active| Presence { kind, status: status.map(Into::into), last_active };

        app.note_away(None);
        assert_eq!(notes(&app), 0, "nothing known, nothing said");
        set(&mut app, presence(PresenceKind::Offline, None, None));
        app.note_away(None);
        set(&mut app, presence(PresenceKind::Offline, None, ago(3)));
        app.note_away(None);
        set(&mut app, presence(PresenceKind::Away, None, ago(5)));
        app.note_away(None);
        assert_eq!(notes(&app), 0, "hidden presence, a missed heartbeat, a few idle minutes: not news");

        set(&mut app, away("lunch"));
        app.note_away(None);
        app.note_away(None);
        assert_eq!(app.windows[1].lines.back().unwrap().body, "sam is away: lunch");
        assert_eq!(notes(&app), 1, "once, not every message");
        set(&mut app, away("in a meeting"));
        app.note_away(None);
        assert_eq!(notes(&app), 2, "a new status is news");
        set(&mut app, presence(PresenceKind::Online, None, None));
        set(&mut app, away("in a meeting"));
        app.note_away(None);
        assert_eq!(notes(&app), 2, "a blip online doesn't make it news again");

        // An hour on, long gone is worth saying again.
        app.away_noted.get_mut(&sam).unwrap().1 = Instant::now().checked_sub(AWAY_REPEAT).unwrap();
        set(&mut app, presence(PresenceKind::Offline, None, ago(180)));
        app.note_away(None);
        assert_eq!(app.windows[1].lines.back().unwrap().body, "sam is offline, last seen 3h ago");

        // Someone who's just been talking is around, whatever it says.
        app.away_noted.clear();
        let mut said = Line::new(LineKind::Message, "back");
        (said.sender, said.ts) = (Some(sam.clone()), Local::now() - chrono::TimeDelta::minutes(2));
        app.windows[1].lines.push_back(said);
        app.note_away(None);
        assert_eq!(notes(&app), 3);

        app.windows[1].lines.pop_back();
        app.cfg.away_notes = false;
        app.note_away(None);
        assert_eq!(notes(&app), 3, "away_notes = false: never");
    }

    #[test]
    fn nicknames_show_only_here() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-nicknames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        let sam: OwnedUserId = "@sam:x.org".try_into().unwrap();
        let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
        let mut dm = Window::new(Some(key), "sam".into());
        (dm.is_dm, dm.dm_user) = (true, Some(sam.clone()));
        let mut line = msg("$1", "hi");
        (line.nick, line.sender) = ("sam".into(), Some(sam.clone()));
        dm.lines.push_back(line);
        app.windows.push(dm);
        app.switch_to(1);

        app.cmd_nickname("@sam:x.org Bear").unwrap();
        assert_eq!(app.windows[1].title(), "Bear", "the DM is named after them");
        assert_eq!(app.display_name(Some(&sam), "sam"), "Bear");
        let drawn = ui::render_line_with_names(&app.windows[1].lines[0], 80, &app.cfg, app.nicknames());
        assert!(drawn.contains("<Bear>"), "{drawn}");

        app.input.set_text("Be");
        let candidates = app.candidates();
        assert_eq!(candidates[0].text, "sam: ", "completion inserts their real name");
        assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Bear"));

        app.cmd_nickname("@sam:x.org").unwrap();
        assert_eq!(app.windows[1].title(), "sam");
        assert!(app.cmd_nickname("@sam:x.org").is_err(), "nothing to take off");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn script_settings_go_through_set_and_save() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-script-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        std::fs::write(app.paths.config_file(), "# mine\n\n[script_settings.watch]\nloud = true\n").unwrap();
        app.cfg = Config::load(&app.paths.config_file()).unwrap();
        app.scripts.configure(&app.cfg.script_settings);
        let source = r#"setting("words", "deploy", "Words to watch for"); setting("loud", false, "Ring too");"#;
        app.scripts.load("watch", None, source).unwrap();

        app.cmd_set("").unwrap();
        let listed: Vec<&str> = app.windows[0].lines.iter().map(|l| l.body.as_str()).collect();
        assert!(listed.contains(&"  watch.words = \"deploy\"  (Words to watch for)"), "{listed:#?}");
        assert!(listed.contains(&"  watch.loud = true  (Ring too)"), "config.toml's value");
        assert!(!listed.iter().any(|l| l.contains("[script_settings]")), "shown as settings, not a table");
        app.cmd_set("watch.words pizza|outage").unwrap();
        assert!(app.cmd_set("watch.loud maybe").unwrap_err().contains("takes boolean"));
        app.cmd_set("image_rows 12").unwrap();
        assert_eq!(app.scripts.settings()[0].1, toml::Value::String("pizza|outage".into()), "other /sets keep it");

        app.cmd_save().unwrap();
        let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert!(saved.contains("# mine\n") && saved.contains("image_rows = 12"), "{saved}");
        assert!(saved.contains("[script_settings.watch]\nloud = true\nwords = \"pizza|outage\"\n"), "{saved}");
        let back = Config::load(&app.paths.config_file()).unwrap();
        assert_eq!(back.script_settings["watch"]["words"], toml::Value::String("pizza|outage".into()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_and_save_keep_what_you_changed() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("textrix-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.paths.base = dir.clone();
        std::fs::write(app.paths.config_file(), "# my config\nmarkdown = false\n").unwrap();
        app.cfg = Config::load(&app.paths.config_file()).unwrap();

        app.cmd_set("image_rows 14").unwrap();
        assert_eq!(app.cfg.image_rows, 14);
        app.cmd_set("bell away").unwrap();
        assert_eq!(app.bell_mode, crate::config::BellMode::Away, "words are taken as text");
        assert!(app.cmd_set("image_rows lots").is_err(), "checked like config.toml");
        assert!(app.cmd_set("belll true").is_err(), "unknown settings are errors");
        assert!(app.cmd_set("aliases x").is_err(), "tables are edited in the file");
        app.cmd_theme("blue-deep").unwrap();
        app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));

        app.cmd_save().unwrap();
        let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
        assert!(saved.starts_with("# my config\nmarkdown = false\n"), "{saved}");
        for line in ["image_rows = 14", "bell = \"away\"", "theme = \"blue-deep\"", "sidebar = false"] {
            assert!(saved.contains(line), "{line} missing from:\n{saved}");
        }
        let back = Config::load(&app.paths.config_file()).unwrap();
        assert!(!back.markdown && back.image_rows == 14 && !back.sidebar);
        app.cmd_save().unwrap();
        assert!(app.windows[0].lines.back().unwrap().body.contains("already has these settings"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dropped_connections_are_netsplits() {
        let mut app = test_app();
        for (room, name) in [("!a:x.org", "Night Owls"), ("!b:x.org", "Book Club")] {
            room_window(&mut app, room, name);
        }
        let error = || AppEvent::SyncError { account: "t".into(), error: "connection refused".into() };
        app.on_event(error());
        app.on_event(error());
        let status: Vec<&str> = app.windows[0].lines.iter().map(|l| l.body.as_str()).collect();
        let splits: Vec<&&str> = status.iter().filter(|b| b.starts_with("Netsplit t ⇄ you")).collect();
        assert_eq!(splits.len(), 1, "once per split: {status:?}");
        assert!(splits[0].contains("quits: Night Owls, Book Club"), "{}", splits[0]);
        app.on_event(AppEvent::Synced { account: "t".into(), first: false });
        let over = &app.windows[0].lines.back().unwrap().body;
        assert!(over.starts_with("Netsplit over after 0s, joins: Night Owls, Book Club"), "{over}");
        assert_eq!(short_duration(Duration::from_secs(133)), "2m 13s");
        assert_eq!(short_duration(Duration::from_secs(3900)), "1h 5m");
    }

    #[test]
    fn scripts_add_commands_and_react_to_messages() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        app.switch_to(a);
        let source = r#"
            command("shout", "Say it louder", |args, ctx| print(args.to_upper() + " in " + ctx.room));
            command("join", "Can't replace built-ins", |args, ctx| print("hijacked"));
            on_message(|msg| if msg.body.contains("ping") { print("pong for " + msg.nick); bell(); });
        "#;
        app.scripts.load("demo", None, source).unwrap();
        app.command("shout hello");
        assert_eq!(app.windows[a].lines.back().unwrap().body, "HELLO in Night Owls");
        app.command("join #x:y.org");
        assert!(!app.windows[a].lines.iter().any(|l| l.body == "hijacked"), "built-ins win");

        let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
        let mut line = msg("$9", "anyone? ping");
        (line.nick, line.sender) = ("sam".into(), Some("@sam:x.org".try_into().unwrap()));
        app.on_timeline("t", &room, vec![Item::Line(line)], true);
        assert_eq!(app.windows[a].lines.back().unwrap().body, "pong for sam");
        assert!(app.bell);

        let mut own = msg("$10", "ping myself");
        own.own = true;
        app.on_timeline("t", &room, vec![Item::Line(own)], true);
        assert_ne!(app.windows[a].lines.back().unwrap().body, "pong for sam", "scripts don't hear your own messages");

        // Joins and reactions from the timeline reach hooks too (not your own).
        app.scripts
            .load(
                "hooks",
                None,
                r#"on_join(|m| print("hi " + m.nick)); on_reaction(|r| print(r.nick + r.key + r.text));"#,
            )
            .unwrap();
        let mut join = Line::new(LineKind::Event, "sam joined");
        (join.event_id, join.sender) = (Some("$j".try_into().unwrap()), Some("@sam:x.org".try_into().unwrap()));
        join.fold = Some(Fold::Joined("sam".into()));
        let react = |own| Item::React { target: "$9".try_into().unwrap(), key: "🍕".into(), nick: "sam".into(), own };
        app.on_timeline("t", &room, vec![Item::Line(join), react(true), react(false)], true);
        let said: Vec<&str> = app.windows[a].lines.iter().rev().take(3).map(|l| l.body.as_str()).collect();
        assert!(said.contains(&"hi sam") && said.contains(&"sam🍕anyone? ping"), "{said:?}");
        assert_eq!(app.windows[a].lines.iter().filter(|l| l.body.starts_with("sam🍕")).count(), 1, "not your own");

        app.input.set_text("/sho");
        assert_eq!(app.candidates()[0].text, "/shout ");
        app.input.set_text("/shout");
        assert!(app.input_hint().unwrap().contains("Say it louder (demo.rhai)"));
        app.cmd_script("exec 6 * 7").unwrap();
        assert_eq!(app.windows[a].lines.back().unwrap().body, "42");
        app.cmd_script(r#"exec send("Night", "hi")"#).unwrap();
        let last = &app.windows[a].lines.back().unwrap().body;
        assert!(last.contains("send to Night, which isn't a room here"), "send() needs a whole name: {last}");
    }

    #[test]
    fn play_finds_the_latest_audio() {
        let mut app = test_app();
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        app.switch_to(a);
        assert_eq!(app.cmd_play(""), Err("no audio or video here".into()));
        assert_eq!(app.cmd_play("stop"), Err("nothing is playing".into()));
        let mut voice = msg("$v", "[voice] 0:03 ▁▄█");
        voice.nick = "sam".into();
        voice.media = Some(Media {
            source: matrix_sdk::ruma::events::room::MediaSource::Plain("mxc://x/v".into()),
            filename: "Voice message.ogg".into(),
            kind: MediaKind::Audio,
            mimetype: Some("audio/ogg".into()),
            thumbnail: None,
            size: None,
        });
        app.windows[a].push(voice, 100);
        assert_eq!(app.cmd_play("jo_bloggs"), Err("no audio or video from jo_bloggs here".into()));
        // Found; with no server connected here it stops short of playing.
        assert!(app.cmd_play("sam").unwrap_err().contains("not connected"), "found sam's");
    }

    #[test]
    fn restart_comes_back_where_you_were() {
        let dir = std::env::temp_dir().join(format!("textrix-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
        let start = || {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                Config::default(),
                paths.clone(),
                EnvFile::new(paths.env_file.clone()),
                Servers::default(),
                tx,
            );
            let a = room_window(&mut app, "!a:x.org", "Night Owls");
            (app, a)
        };
        let (mut app, a) = start();
        assert!(app.cmd_restart().is_err(), "nowhere to restart from");
        app.install = crate::install::Install::at(std::env::current_exe().unwrap());
        app.switch_to(a);
        app.input.set_text("half a thought");
        app.switch_to(0);
        app.input.set_text("status draft");
        app.switch_to(a);
        app.cmd_restart().unwrap();
        assert!(app.restart && app.quit);

        let (mut next, a) = start();
        assert!(next.pending_resume.is_some());
        next.restore_split();
        assert_eq!((next.active, next.input.text()), (a, "half a thought".to_owned()), "back, draft and all");
        next.switch_to(0);
        assert_eq!(next.input.text(), "status draft", "and the other windows' drafts");
        assert!(next.pending_resume.is_none());

        // The status bar's upgrade note.
        assert_eq!(next.upgrade_note(), None);
        next.released = Some("9.9.9".into());
        assert_eq!(next.upgrade_note().as_deref(), Some("9.9.9 is out"));
        next.on_event(AppEvent::Installed { version: Some("9.9.9".into()) });
        assert_eq!(next.upgrade_note().as_deref(), Some("9.9.9 installed: /restart"));
        next.on_event(AppEvent::Installed { version: Some(env!("CARGO_PKG_VERSION").into()) });
        assert_eq!(next.upgrade_note().as_deref(), Some("new build: /restart"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ctrl_c_twice_quits() {
        let mut app = test_app();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.input.set_text("a draft");
        app.on_key(ctrl_c);
        assert!(app.input.is_empty() && app.quit_prompt().is_none(), "with text, it clears the line");
        app.on_key(ctrl_c);
        assert_eq!(app.quit_prompt(), Some("press Ctrl+C again to quit"));
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(app.quit_prompt().is_none() && !app.quit, "any other key calls it off");
        app.on_key(ctrl_c);
        app.quit_armed = app.quit_armed.map(|at| at - Duration::from_secs(3));
        app.on_key(ctrl_c);
        assert!(!app.quit, "too slow: that one only asks again");
        app.on_key(ctrl_c);
        assert!(app.quit);
    }

    #[test]
    fn easter_eggs() {
        let mut app = test_app();
        for code in eggs::KONAMI {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Green.into());
        assert!(app.input.is_empty(), "the b doesn't stay in the input line");
        for code in eggs::KONAMI {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(app.cfg.theme, crate::theme::ThemeName::Default.into(), "again turns it off");

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
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        app.switch_to(a);
        app.cmd_window("name Ops").unwrap();
        assert_eq!(app.windows[a].title(), "Ops");
        assert_eq!(app.find_window("ops"), Ok(a));
        assert_eq!(app.find_window("night"), Ok(a), "the room's own name still finds it");
        assert_eq!(app.terminal_title().unwrap(), "Ops — Textrix");

        // Kept in the layout, so it comes back with the window.
        let key = app.windows[a].room.clone().unwrap();
        app.remove_window(a);
        let idx = app.insert_window(Window::new(Some(key), "Night Owls".into()));
        assert_eq!(app.windows[idx].title(), "Ops");
        assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Ops"));

        app.switch_to(idx);
        app.cmd_window("name").unwrap();
        assert_eq!(app.windows[idx].title(), "Night Owls", "back to the room's name");
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

        // Narrowed by room, sender and date.
        let found = |app: &mut App, query: &str| -> Vec<String> {
            app.cmd_search(query).unwrap();
            app.search_hits.iter().map(|(_, e)| e.to_string()).collect()
        };
        assert_eq!(found(&mut app, "deploy in:!b"), ["$new"]);
        assert_eq!(found(&mut app, "from:alice in:!a"), ["$oldx", "$old"], "everything alice said there");
        assert!(found(&mut app, "deploy from:sam").is_empty());
        assert!(found(&mut app, "deploy before:2020").is_empty());
        assert_eq!(found(&mut app, "deploy after:yesterday").len(), 2);
        assert!(app.cmd_search("after:soon").unwrap_err().contains("isn't a date"));
        assert!(app.cmd_search("").unwrap_err().starts_with("usage"));
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
        assert_eq!(sidebar_entries(&app), vec![Win(0), Header("Rooms".into()), Win(2), Win(1)], "by name");

        let space = SpaceInfo {
            room_id: "!s:x.org".try_into().unwrap(),
            name: "Work".into(),
            children: vec!["!a:x.org".try_into().unwrap()],
        };
        app.spaces.insert("t".into(), vec![space]);
        assert_eq!(
            sidebar_entries(&app),
            vec![Win(0), Header("Rooms".into()), Header("◆ Work".into()), Win(2), Header("◇ other".into()), Win(1)]
        );
    }

    #[test]
    fn window_list_puts_unreads_first_then_rooms_then_dms() {
        use crate::ui::{
            SidebarEntry::{Header, Window as Win},
            sidebar_entries,
        };
        let mut app = test_app();
        let night = room_window(&mut app, "!n:x.org", "Night Owls");
        let sam = room_window(&mut app, "!s:x.org", "sam");
        let book = room_window(&mut app, "!b:x.org", "Book Club");
        let jo = room_window(&mut app, "!j:x.org", "jo_bloggs");
        app.windows[sam].is_dm = true;
        app.windows[jo].is_dm = true;
        let rooms_then_dms =
            vec![Win(0), Header("Rooms".into()), Win(book), Win(night), Header("DMs".into()), Win(jo), Win(sam)];
        assert_eq!(sidebar_entries(&app), rooms_then_dms);

        // New messages bring a window up to Unreads, mentions first.
        app.windows[night].activity = Activity::Message;
        app.windows[sam].activity = Activity::Highlight;
        app.windows[jo].activity = Activity::Event;
        assert_eq!(
            sidebar_entries(&app),
            vec![
                Win(0),
                Header("Unreads".into()),
                Win(sam),
                Win(night),
                Header("Rooms".into()),
                Win(book),
                Header("DMs".into()),
                Win(jo),
            ],
            "joins and leaves aren't unread messages"
        );

        // Reading them sends them back.
        app.switch_to(sam);
        app.switch_to(night);
        app.windows[jo].activity = Activity::None;
        assert_eq!(sidebar_entries(&app), rooms_then_dms);
        assert_eq!(app.active, night, "the numbers stay what they were");
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
        let react =
            Item::React { target: "$r1".try_into().unwrap(), key: "👍".into(), nick: "bob".into(), own: false };
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

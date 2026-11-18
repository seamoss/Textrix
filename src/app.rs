//! Application state: windows, accounts, keys and slash commands.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::{Local, TimeZone};
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
    emoji_picker::{self, EmojiPicker},
    gallery::{self, Gallery},
    home::{self, Home, Invitation},
    input::{Candidate, Draft, Input},
    links,
    matrix::{self, Account, AppEvent, Moderation, Tx, VerifyAnswer, VerifyStage},
    model::{
        AccountName, Activity, Fold, Item, Line, LineKind, Media, MediaKind, Member, Permissions, Poll, Presence,
        PresenceKind, RoomMeta, Scheduled, SpaceInfo, ago,
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
    /// First in the window list (`m.favourite`).
    pub favourite: bool,
    /// Bridges to other networks: a ⇄ in the window list, named in the
    /// topic bar.
    pub bridges: Vec<crate::model::Bridge>,
    /// Marked unread, to come back to: in Unreads until you open it.
    pub marked_unread: bool,
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
    /// The media gallery wants more of the room's history.
    pub wants_older: bool,
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
            favourite: false,
            bridges: Vec::new(),
            marked_unread: false,
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
            wants_older: false,
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
        self.favourite = meta.favourite;
        self.bridges = meta.bridges.clone();
        self.marked_unread = meta.marked_unread;
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

    /// Older history is on its way.
    pub fn loading_older(&self) -> bool {
        matches!(self.history, History::Loading)
    }

    /// All of the room's history is loaded.
    pub fn history_done(&self) -> bool {
        matches!(self.history, History::Done)
    }

    fn find_mut(&mut self, id: &EventId) -> Option<&mut Line> {
        self.lines.iter_mut().rev().find(|l| l.event_id.as_deref() == Some(id))
    }

    fn contains(&self, id: &EventId) -> bool {
        self.lines.iter().rev().any(|l| l.event_id.as_deref() == Some(id))
    }

    /// The newest loaded event, for read markers.
    fn latest_event(&self) -> Option<OwnedEventId> {
        self.latest_event_by(None)
    }

    /// The latest event from `ts` or before (any, without a time).
    fn latest_event_by(&self, ts: Option<chrono::DateTime<Local>>) -> Option<OwnedEventId> {
        let lines = self.lines.iter().rev().filter(|l| !l.pending && l.event_id.is_some());
        lines.into_iter().find(|l| ts.is_none_or(|ts| l.ts <= ts)).and_then(|l| l.event_id.clone())
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
        // An older edit than the one shown (from loading history) is one
        // of the earlier versions; a newer one replaces the text, which
        // joins them.
        Item::Edit { body, fallback, .. } if !line.redacted && line.edited_at.is_some_and(|now| fallback.ts < now) => {
            let at = line.versions.partition_point(|(t, _)| *t <= fallback.ts);
            line.versions.insert(at, (fallback.ts, body.clone()));
        }
        Item::Edit { body, fallback, .. } if !line.redacted => {
            let since = line.edited_at.unwrap_or(line.ts);
            line.versions.push((since, std::mem::replace(&mut line.body, body.clone())));
            line.edited_at = Some(fallback.ts);
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
        Item::PollVote { user, at, answers, own, .. } => {
            if let Some(poll) = line.poll.as_mut() {
                poll.vote(user, *at, answers, *own);
            }
        }
        // Only whoever started a poll ends it.
        Item::PollEnd { user, at, .. } if line.sender.as_ref() == Some(user) => {
            if let Some(poll) = line.poll.as_mut() {
                poll.ended.get_or_insert(*at);
            }
        }
        _ => {}
    }
}

/// An inline image preview's state.
pub enum LinkTitle {
    Loading,
    /// Its title, and its preview picture when there is one.
    Ready(String, Option<Picture>),
    /// The page has no title, or it couldn't be fetched.
    None,
}

/// The profile pane: someone, and what's known about them so far.
pub struct ProfileView {
    pub account: String,
    pub user: OwnedUserId,
    /// Their name here (the member list's, or the message's).
    pub name: String,
    pub info: crate::matrix::UserInfo,
    pub picture: Option<Picture>,
    /// Their place in the member list, when it was opened from there: ↑/↓
    /// moves through it.
    pub member: Option<usize>,
}

/// A link's preview picture, at most this many cells.
pub const LINK_PICTURE: (u16, u16) = (24, 6);

/// The profile pane's picture, in cells.
pub const PROFILE_PICTURE: (u16, u16) = (16, 8);

/// Custom emoji: (shortcode, picture).
type Emoji = Vec<(String, OwnedMxcUri)>;
/// Stickers, from the same packs.
type Stickers = Vec<crate::stickers::Sticker>;

/// An image from the clipboard, saved to send: its file, "1280×720 PNG,
/// 340 KB", and a small picture of it (when the terminal can show one).
pub struct Pasted {
    pub file: PathBuf,
    pub summary: String,
    pub picture: Option<Picture>,
}

/// The verification panel, over whatever window you're in.
pub struct VerifyPanel {
    pub account: AccountName,
    pub stage: VerifyStage,
}

/// Voting in a poll: its answers to pick from (Enter in selection mode).
pub struct VotePicker {
    pub question: String,
    /// (id, text).
    pub answers: Vec<(String, String)>,
    /// How many one vote may pick.
    pub max: usize,
    /// The answers picked so far (several, when `max` allows).
    pub picked: BTreeSet<usize>,
    pub selected: usize,
    key: WinKey,
    poll: OwnedEventId,
    stable: bool,
}

/// What `/theme` does other than pick a theme, so no theme file can be
/// called these.
const THEME_WORDS: [&str; 6] = ["list", "save", "import", "here", "space", "edit"];

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
    /// The room in focus, when there is one: Ctrl+T picks for just it.
    pub room: Option<String>,
    /// Picking for just that room, not everywhere.
    pub just_here: bool,
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

/// How tall a pasted image's picture is, above the input line.
pub const PASTED_ROWS: u16 = 6;

/// Encoded previews kept in memory; older ones are fetched again (from the
/// SDK's media cache) if they scroll back into view.
const PREVIEWS_MAX: usize = 64;

/// What the next plain-text submit does, chosen in selection mode.
#[derive(Clone, Debug)]
enum Compose {
    Reply { event: OwnedEventId, sender: OwnedUserId, nick: String },
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
    /// Rooms' and spaces' own themes (`/theme here`, `/theme space`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    themes: Vec<(WinKey, ThemeId)>,
    /// Your latest reactions, newest first: emoji, or `:code:` for custom
    /// ones (the emoji picker's Recent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_emoji: Vec<String>,
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
    /// An Escape held a moment: it may be a mouse report's, come apart.
    esc_hold: Option<EscHold>,
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
    /// `/list`: rooms to join, from a server's directory or a space.
    pub directory: Option<crate::directory::Directory>,
    pub theme_picker: Option<ThemePicker>,
    /// `/theme edit`: the theme on screen, a colour at a time.
    pub theme_editor: Option<crate::theme_editor::ThemeEditor>,
    /// Reacting with `+`: the emoji to pick from.
    pub emoji_picker: Option<EmojiPicker>,
    /// `/sticker`: stickers to choose from, in a grid of their pictures.
    pub sticker_picker: Option<crate::stickers::StickerPicker>,
    /// Sticker pictures the picker drew a space for and wants.
    pub sticker_requests: Vec<OwnedMxcUri>,
    pub vote: Option<VotePicker>,
    /// A verification under way: accept it, compare emoji, see it end.
    pub verify_panel: Option<VerifyPanel>,
    /// An image pasted from the clipboard, waiting for Enter to send it.
    pub pasted: Option<Pasted>,
    pub profile: Option<ProfileView>,
    /// `/media`: a room's media as tiles, over the messages.
    pub gallery: Option<Gallery>,
    /// The home window (`/home`, F4), over the messages.
    pub home: Option<Home>,
    /// Pending invitations, every server's.
    pub invites: Vec<Invitation>,
    /// Gallery pictures the renderer wants, fetched after the draw.
    pub tile_requests: Vec<(OwnedEventId, Media)>,
    /// Plugins from config.toml, running or not.
    pub plugins: BTreeMap<String, plugins::PluginState>,
    /// Plugins' messages on their way: our transaction ID → (plugin, the
    /// request to answer).
    plugin_sends: HashMap<matrix_sdk::ruma::OwnedTransactionId, (String, serde_json::Value)>,
    /// Rooms joined because `accept_invites_from` trusts who invited us.
    joining: HashSet<OwnedRoomId>,
    /// Messages the send queue sent before we heard its name for them.
    sent_early: HashMap<matrix_sdk::ruma::OwnedTransactionId, OwnedEventId>,
    /// A voice message being recorded, and the window it's for.
    pub recording: Option<(WinKey, crate::voice::Recording)>,
    /// `notify_command`s running, and whether the last one failed.
    hooks: (Arc<AtomicUsize>, Arc<AtomicBool>),
    /// Messages to send later (`/later`), soonest first.
    later: Vec<(AccountName, Scheduled)>,
    /// Custom emoji per server: (yours and global, each room's own).
    custom_emoji: HashMap<AccountName, (Emoji, HashMap<OwnedRoomId, Emoji>)>,
    /// Each server's stickers (from the same packs): yours and the global
    /// ones, and each room's. Not there until they're loaded.
    stickers: HashMap<AccountName, (Stickers, HashMap<OwnedRoomId, Stickers>)>,
    /// Ctrl+X: this text, to open in your editor.
    pub editor_request: Option<String>,
    /// A script's command is running (with what it calls): no `/exec`.
    in_script: usize,
    /// When you went /away: from then, DMs collect with your mentions.
    away_since: Option<chrono::DateTime<Local>>,
    /// When you last pressed a key, clicked, pasted or came back to the
    /// terminal: auto-away counts from it.
    last_input: Instant,
    /// Auto-away set the away you're in (so your being back ends it).
    auto_away: bool,
    /// The theme `cfg.palette` is: yours, or the focused room's own.
    palette_for: ThemeId,
    /// The terminal reports keys the enhanced way (kitty's protocol).
    pub enhanced_keys: bool,
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
    Cmd {
        name: "media",
        args: "",
        help: "This room's pictures, videos, audio and files as tiles, newest first: Enter opens, s saves, Tab its links",
    },
    Cmd {
        name: "favourite",
        args: "[room]",
        help: "Make this room or DM (or another) a favourite, or not: first in the window list, and in Element's Favourites",
    },
    Cmd {
        name: "links",
        args: "",
        help: "This room's links, newest first: Enter opens, c copies, m goes to the message, / filters, Tab its media",
    },
    Cmd { name: "home", args: "", help: "What's waiting: the latest mentions, unread rooms and invitations (F4)" },
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
    Cmd {
        name: "np",
        args: "",
        help: "Say what you're listening to (Music or Spotify on macOS, playerctl on Linux; np_format, np_command)",
    },
    Cmd { name: "notice", args: "<text>", help: "Send a notice" },
    Cmd { name: "say", args: "<text>", help: "Send text as-is, even if it starts with / (or type //)" },
    Cmd { name: "reply", args: "[nick] <text>", help: "Reply to the last message (from nick, if given)" },
    Cmd { name: "react", args: "[nick] <emoji>", help: "React to the last message (from nick, if given)" },
    Cmd {
        name: "sticker",
        args: "[name]",
        help: "Send a sticker from your image packs or the room's: by name, or choose from their pictures",
    },
    Cmd {
        name: "edit",
        args: "[text]",
        help: "Edit a message: no text picks one of yours to edit (also Shift+Up, or Ctrl+E on an empty line)",
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
    Cmd {
        name: "exec",
        args: "[-o] <command>",
        help: "Run a shell command and show what it prints here; -o sends that to the room (not from scripts)",
    },
    Cmd {
        name: "bot",
        args: "[list | new <name> | start <name> | stop <name>]",
        help: "Bots: Textrix in the background as another Matrix account, doing what its plugins do",
    },
    Cmd {
        name: "unread",
        args: "[room]",
        help: "Mark this room (or another) unread, to come back to: in Unreads until you open it, in every client",
    },
    Cmd {
        name: "codebot",
        args: "[start [repo] | stop | log]",
        help: "codebot in a bot of its own: /codebot start <repo> in a room sets it up there and starts it",
    },
    Cmd {
        name: "plugin",
        args: "[list | new <name> [hello|codebot] | start <name> | stop <name> | restart <name> | log <name>]",
        help: "Plugins from config.toml: programs that hear and post in the rooms you grant them",
    },
    Cmd {
        name: "voice",
        args: "",
        help: "Record a voice message here (Ctrl+R too): Enter sends it, Esc throws it away",
    },
    Cmd {
        name: "paste",
        args: "",
        help: "Paste the image on the clipboard (Ctrl+V too): Enter sends it, with what you type as its caption",
    },
    Cmd {
        name: "later",
        args: "[<10m|1h30m|18:30|tomorrow 9:00> <text> | cancel <N|all>]",
        help: "Send a message here later (while Textrix runs); no text lists what's waiting",
    },
    Cmd {
        name: "poll",
        args: "<question> | <answer> | <answer>…  or  end",
        help: "Start a poll here (Enter on one in selection mode votes), or end your latest",
    },
    Cmd { name: "lastlog", args: "[-n N] [-nick nick] <text>", help: "Search this window's loaded scrollback" },
    Cmd {
        name: "search",
        args: "[-deep] [from:name] [in:room] [after:date] [before:date] [text]",
        help: "Search every room (the encrypted local index, and loaded history; -deep also reads open rooms' last 500)",
    },
    Cmd {
        name: "jump",
        args: "<N> | new",
        help: "Go to result N of /search or /pins and select it; new: to the first unread message here (Alt+R)",
    },
    Cmd { name: "urls", args: "", help: "List links in this window, newest first" },
    Cmd { name: "open", args: "[N]", help: "Open link N from /urls in your browser (default: newest)" },
    Cmd {
        name: "list",
        args: "[server | search]",
        help: "Rooms to join: your server's public directory (or another server's), then your spaces' rooms with Tab",
    },
    Cmd {
        name: "away",
        args: "[-all] [message]",
        help: "Set away with a message, or come back (no message); auto_away does it after you've been idle",
    },
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
        args: "[name | list | edit | here [name|off] | space <space> <name|off> | save <name> | import <file>]",
        help: "Colours: no name opens the list, each shown as you move to it; list prints them; edit changes the one on \
               screen a colour at a time; here and space give a room or a space's rooms their own; save writes this \
               look to a theme file, import adds one",
    },
    Cmd {
        name: "themes",
        args: "",
        help: "Pick a theme from a list, each shown as you move to it; Enter keeps it for the session (/save for good)",
    },
    Cmd { name: "nicklist", args: "", help: "Toggle the member list (F3)" },
    Cmd { name: "servers", args: "", help: "List dotfile servers and their status" },
    Cmd { name: "log", args: "[on|off]", help: "Chat logging for this session (/save keeps it)" },
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
    Cmd { name: "changelog", args: "[N]", help: "What changed in the latest N releases (10), from GitHub" },
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
    ("fav", "favourite"),
    ("favorite", "favourite"),
];

pub const KEYS: &[(&str, &str)] = &[
    ("Alt+1…0, Alt+Q…P", "go to window 1–20"),
    ("Ctrl+N / Ctrl+P, Alt+← / Alt+→", "next / previous window"),
    ("Alt+A", "jump to the most important window with activity"),
    ("Alt+R", "jump to the new-messages line in this room (where you'd read up to), loading history back to it"),
    ("Ctrl+K, Alt+K", "quick switcher: type part of a room's name (or its number), Enter goes there"),
    ("PgUp / PgDn", "scroll; older history loads at the top"),
    ("Tab", "complete commands, nicks (adds a mention), :emoji shortcodes, server names, file paths"),
    ("Up / Down", "input history"),
    ("Shift+Enter, Alt+Enter", "new line in the message"),
    ("Alt+Up", "select a message (keys below)"),
    ("Shift+Up, Ctrl+E on an empty line", "edit your messages (keys below)"),
    ("F1 / F2 / F3 / F4", "this help / window list / member list / home: what's waiting for you"),
    ("F6 / Shift+F6", "focus the next / previous split pane"),
    (
        "Mouse",
        "wheel scrolls; click a window, pane, message (selects), link (opens), image (views), a name or nick (who they are), or the input",
    ),
    ("Ctrl+A / Ctrl+E / Ctrl+B / Ctrl+F", "start / end / left / right in the input line"),
    ("Alt+B / Alt+F, Ctrl+← / Ctrl+→", "back / forward a word"),
    ("Home / End, Ctrl+D", "start / end of the line; delete the character under the cursor"),
    ("Ctrl+U / Ctrl+K / Ctrl+W / Ctrl+Y", "cut to start / cut to end / cut word / paste cut"),
    ("Ctrl+C", "clear the input line; on an empty one, twice quits (detaches, in the background)"),
    (
        "Ctrl+X",
        "write the message in your editor (editor in config.toml, $VISUAL, $EDITOR or vi); quitting it brings the text back",
    ),
    ("Ctrl+L", "redraw the screen"),
    ("Ctrl+R", "record a voice message: Enter sends it, Esc throws it away"),
    ("Ctrl+V", "paste an image from the clipboard: Enter sends it (what you type is its caption), Esc drops it"),
];

pub const SELECT_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓, PgUp / PgDn, g / G", "move between messages"),
    ("r", "reply"),
    ("e", "edit (your own messages)"),
    ("d", "delete (asks first)"),
    ("+", "react: pick an emoji (arrows, or type to search), Enter sends it"),
    ("t", "open the thread (or start one)"),
    ("p", "pin or unpin it for everyone in the room (if allowed)"),
    ("f", "forward a copy to another room (pick it like the switcher)"),
    ("z", "view the image full screen: + / - zoom, arrows move, 0 fits, Esc closes"),
    ("z or Space", "play a voice message, audio (again stops it) or a video"),
    ("s", "save the attachment"),
    ("o", "open the link"),
    ("v", "show spoilers"),
    ("c", "copy the text"),
    ("i", "sender, time and event ID, and what an edited one said before"),
    ("u", "who sent it: their profile pane"),
    ("Enter", "vote in a poll; in the mentions window, go to the message in its room"),
    ("Esc", "leave selection mode"),
];

pub const GALLERY_KEYS: &[(&str, &str)] = &[
    ("arrows, PgUp / PgDn, g / G", "move between tiles (older history loads as you go)"),
    ("Enter, z", "view a picture, play audio or a video, save a file"),
    ("s", "save it to Downloads"),
    ("click", "choose a tile; again opens it"),
    ("Esc", "back to the messages"),
];

pub const HOME_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓", "choose"),
    ("Enter, 1–9, click", "go there; on an invitation, join it"),
    ("d, d", "decline an invitation"),
    ("Esc, F4", "back to the messages"),
];

pub const THEME_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓, PgUp / PgDn, Home / End", "move: each theme shows on the whole screen"),
    ("type", "narrow the list"),
    ("Ctrl+T", "pick for just this room, or everywhere"),
    ("Enter", "keep it (for the session: /save keeps it for good)"),
    ("Esc", "put back the one you had"),
];

pub const VOTE_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓", "choose an answer"),
    ("1…9", "that answer (and vote, where you pick one)"),
    ("Space", "tick or untick one, where you can pick several"),
    ("Enter", "vote"),
    ("Esc", "close without voting"),
];

pub const PROFILE_KEYS: &[(&str, &str)] = &[
    ("↑ / ↓", "the next member in the list"),
    ("m", "message them (a DM)"),
    ("@", "mention them"),
    ("n / i / v", "/nickname, /ignore or /verify them, in the input line to send"),
    ("Esc", "close"),
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
        ("The theme list (/theme)", THEME_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        ("The media gallery (/media)", GALLERY_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        ("Home (/home, F4)", HOME_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        ("Voting (Enter on a poll)", VOTE_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect()),
        (
            "A profile (click a name, or u)",
            PROFILE_KEYS.iter().map(|(k, d)| ((*k).to_owned(), (*d).to_owned())).collect(),
        ),
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
        let palette_for = cfg.theme.clone();
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
            esc_hold: None,
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
            directory: None,
            theme_picker: None,
            theme_editor: None,
            emoji_picker: None,
            sticker_picker: None,
            sticker_requests: Vec::new(),
            vote: None,
            verify_panel: None,
            pasted: None,
            profile: None,
            gallery: None,
            home: None,
            invites: Vec::new(),
            tile_requests: Vec::new(),
            recording: None,
            plugins: BTreeMap::new(),
            plugin_sends: HashMap::new(),
            joining: HashSet::new(),
            sent_early: HashMap::new(),
            hooks: Default::default(),
            custom_emoji: HashMap::new(),
            stickers: HashMap::new(),
            later: Vec::new(),
            editor_request: None,
            enhanced_keys: false,
            away_since: None,
            last_input: Instant::now(),
            auto_away: false,
            in_script: 0,
            palette_for,
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
        app.note_ignored_settings();
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
        self.plugins =
            self.cfg.plugins.iter().map(|(n, c)| (n.clone(), plugins::PluginState::new(c.clone()))).collect();
        self.start_plugins();
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
                    (self.alias_depth, self.in_script) = (self.alias_depth + 1, self.in_script + 1);
                    self.command(command.trim_start_matches('/'));
                    (self.alias_depth, self.in_script) = (self.alias_depth - 1, self.in_script - 1);
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
            self.tell_plugins(&key, &heard);
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
                .replace("{activity}", &activity)
                .replace("{version}", env!("CARGO_PKG_VERSION")),
        )
    }

    /// Where a server lives, for netsplit lines: "example.org".
    fn server_host(&self, account: &str) -> String {
        let url = self
            .connected(account)
            .map(|a| a.homeserver)
            .or_else(|| self.accounts.get(account)?.def.homeserver_or_server_name().map(str::to_owned))
            .unwrap_or_else(|| account.to_owned());
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

    // ---- per-frame upkeep ------------------------------------------------

    /// Whether window-list avatars can be real images (a 2-cell picture in
    /// half-blocks would be noise, so those terminals get initials).
    pub fn avatar_images(&self) -> bool {
        self.cfg.sidebar_avatars && self.picker.as_ref().is_some_and(Painter::graphics)
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

    // ---- terminal input --------------------------------------------------

    /// Your nicknames for people.
    pub fn nicknames(&self) -> &BTreeMap<OwnedUserId, String> {
        &self.layout.nicknames
    }

    /// What to call someone: your nickname for them, else `fallback`.
    pub fn display_name<'a>(&'a self, user: Option<&UserId>, fallback: &'a str) -> &'a str {
        user.and_then(|u| self.layout.nicknames.get(u)).map_or(fallback, String::as_str)
    }

    /// The window's room's own notification mode (`None`: the default).
    pub fn room_mode(&self, idx: usize) -> Option<RoomNotificationMode> {
        let key = self.windows[idx].room.as_ref()?;
        self.push_modes.get(key.account.as_str())?.get(&key.room_id).copied()
    }

    pub fn on_screen(&self, idx: usize) -> bool {
        idx == self.active || self.split.as_ref().is_some_and(|s| s.panes.contains(&self.windows[idx].id()))
    }

    // ---- selection mode --------------------------------------------------

    // ---- edit mode -------------------------------------------------------

    // ---- commands --------------------------------------------------------
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

/// How long `/exec` waits, and how much of what it prints it keeps.
const EXEC_LIMIT: Duration = Duration::from_secs(30);

/// Settings whose value is a command Textrix runs.
const RUNS_PROGRAMS: [&str; 3] = ["notify_command", "editor", "video_player"];
const EXEC_LINES: usize = 500;
const EXEC_BYTES: usize = 64 * 1024;

/// Run `command` with `sh -c` (no input), and what it printed (stdout,
/// then stderr) without terminal escapes, cut to size; with its exit code.
async fn run_exec(command: &str) -> Result<(String, Option<i32>), String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start it: {e}"))?;
    let out = tokio::time::timeout(EXEC_LIMIT, child.wait_with_output())
        .await
        .map_err(|_| format!("stopped after {} seconds", EXEC_LIMIT.as_secs()))?
        .map_err(|e| e.to_string())?;
    let mut bytes = out.stdout;
    bytes.extend_from_slice(&out.stderr);
    bytes.truncate(EXEC_BYTES);
    let text = strip_escapes(&String::from_utf8_lossy(&bytes));
    let mut lines: Vec<&str> = text.lines().collect();
    let more = lines.len().saturating_sub(EXEC_LINES);
    lines.truncate(EXEC_LINES);
    let mut text = lines.join("\n");
    if more > 0 {
        text.push_str(&format!("\n… and {more} more lines"));
    }
    Ok((text, out.status.code()))
}

/// Text without terminal escape sequences (colours, cursor moves).
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final letter.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                        if c == '\u{1b}' {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `/later`'s time: `10m`, `1h30m`, `90s`, `2d` from now; `18:30` (the
/// next one); `tomorrow 9:00`. With the text after it.
fn parse_later(args: &str, now: chrono::DateTime<Local>) -> Option<(chrono::DateTime<Local>, &str)> {
    let (first, rest) = args.split_once(char::is_whitespace)?;
    let (when, text) =
        if first == "tomorrow" { rest.trim_start().split_once(char::is_whitespace)? } else { (first, rest) };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((h, m)) = when.split_once(':') {
        let time = chrono::NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)?;
        let mut day = now.date_naive();
        if first == "tomorrow" {
            day = day.succ_opt()?;
        }
        let mut due = Local.from_local_datetime(&day.and_time(time)).earliest()?;
        if due <= now {
            due = Local.from_local_datetime(&day.succ_opt()?.and_time(time)).earliest()?;
        }
        return Some((due, text));
    }
    if first == "tomorrow" {
        return None;
    }
    // 1h30m: numbers each with a unit.
    let mut secs = 0i64;
    let mut number = String::new();
    for c in when.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: i64 = std::mem::take(&mut number).parse().ok()?;
        secs += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return None,
        };
    }
    if !number.is_empty() || secs <= 0 {
        return None;
    }
    Some((now + chrono::TimeDelta::seconds(secs), text))
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

/// How long an Escape waits for the rest of a mouse report.
const ESC_WAIT: Duration = Duration::from_millis(30);

/// An Escape from the terminal, and what's come after it so far.
struct EscHold {
    at: Instant,
    text: String,
}

/// The rest of an SGR mouse report whose Escape came on its own
/// (`[<65;247;37M`: button 65, the wheel down, at column 247, row 37): the
/// event, `None` while more could still make one, `Err` when it can't.
fn split_mouse(text: &str) -> Result<Option<MouseEvent>, ()> {
    let Some(body) = text.strip_prefix("[<") else {
        return if "[<".starts_with(text) { Ok(None) } else { Err(()) };
    };
    let (fields, end) = match body.chars().last() {
        Some(end @ ('M' | 'm')) => (&body[..body.len() - 1], Some(end)),
        _ => (body, None),
    };
    let parts: Vec<&str> = fields.split(';').collect();
    let digits = |p: &&str| p.chars().all(|c| c.is_ascii_digit());
    if parts.len() > 3 || !parts.iter().all(digits) || parts[..parts.len() - 1].iter().any(|p| p.is_empty()) {
        return Err(());
    }
    let Some(end) = end else { return Ok(None) };
    let [code, x, y] = parts[..] else { return Err(()) };
    let number = |p: &str| p.parse::<u16>().map_err(drop);
    let (code, column, row) = (number(code)?, number(x)?.saturating_sub(1), number(y)?.saturating_sub(1));
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [(4, KeyModifiers::SHIFT), (8, KeyModifiers::ALT), (16, KeyModifiers::CONTROL)] {
        if code & bit != 0 {
            modifiers |= modifier;
        }
    }
    let button = |n: u16| match n & 3 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None,
    };
    let kind = match code & !28 {
        64 => MouseEventKind::ScrollUp,
        65 => MouseEventKind::ScrollDown,
        66 => MouseEventKind::ScrollLeft,
        67 => MouseEventKind::ScrollRight,
        n if n & 32 != 0 => button(n).map_or(MouseEventKind::Moved, MouseEventKind::Drag),
        n => match (button(n), end) {
            (Some(b), 'M') => MouseEventKind::Down(b),
            (Some(b), _) => MouseEventKind::Up(b),
            (None, _) => return Err(()),
        },
    };
    Ok(Some(MouseEvent { kind, column, row, modifiers }))
}

/// `/list matrix.org`: a server's name (a dot, no spaces, not an address
/// or ID), rather than something to search for.
fn looks_like_server(text: &str) -> bool {
    !text.is_empty()
        && text.contains('.')
        && !text.contains(char::is_whitespace)
        && !text.starts_with(['#', '!', '@'])
        && <&matrix_sdk::ruma::ServerName>::try_from(text).is_ok()
}

/// Wait until `until`, or forever.
async fn until(until: Option<Instant>) {
    match until {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// `keys` is the terminal's input: this terminal's, or in the background,
/// the attached one's.
/// `new_keys` makes the stream of terminal events: again after the editor
/// (Ctrl+X) has had the keyboard.
pub async fn run<B, K>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut rx: UnboundedReceiver<AppEvent>,
    mut new_keys: impl FnMut() -> K,
) -> Result<bool>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    K: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    let mut keys = new_keys();
    // When a moving picture on screen shows its next frame.
    let mut next_frame: Option<Instant> = None;
    app.start();
    loop {
        // In the background with no terminal attached, nothing is drawn.
        let shown = crate::term::attached();
        app.follow_room_theme();
        if shown {
            if std::mem::take(&mut app.redraw_all) {
                // Everything afresh. Not Terminal::clear, which first asks
                // the terminal where its cursor is: a full screen doesn't
                // need to know, and right after the editor the answer can
                // go astray.
                let size = terminal.size()?;
                terminal.resize(Rect::new(0, 0, size.width, size.height))?;
                app.last_links.clear();
            }
            let frame = terminal.draw(|f| ui::draw(f, &mut app))?;
            // Moving pictures rest while the terminal is in the background.
            next_frame = crate::picture::next_frame().filter(|_| app.focused);
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
        if let Some(text) = app.editor_request.take() {
            let editor = crate::editor::command(&app.cfg.editor);
            match crate::term::remote_client() {
                Some(client) => {
                    let _ = client.send(crate::remote::ToClient::Edit { text, editor });
                }
                None if shown => {
                    // The editor gets the keyboard; a new stream takes it back.
                    drop(keys);
                    let saved = crate::editor::edit(&editor, &text, app.enhanced_keys).map_err(|e| format!("{e:#}"));
                    keys = new_keys();
                    app.on_event(AppEvent::Edited(saved));
                    continue;
                }
                None => {}
            }
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
            _ = until(app.flash_until) => app.flash_until = None,
            // An Escape held for the rest of a mouse report that didn't come.
            _ = until(app.esc_hold.as_ref().map(|h| h.at + ESC_WAIT)) => app.esc_timeout(),
            // A moving picture's next frame.
            _ = until(next_frame.filter(|_| shown)) => {}
            // The room browser's search, once typing rests.
            _ = until(app.directory_due()) => app.fetch_directory(),
        }
    }
    let restart = app.restart;
    app.shutdown();
    Ok(restart)
}

mod codebot;
mod commands;
mod events;
mod input;
mod layout;
mod messages;
mod panels;
mod plugins;
mod settings;
#[cfg(test)]
mod shots;
mod themes;

#[cfg(test)]
mod tests;

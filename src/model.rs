//! Plain data shared between the Matrix side and the UI.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedTransactionId, OwnedUserId, UserId, events::room::MediaSource,
};

/// The dotfile name of a server, e.g. `"m4trix"`.
pub type AccountName = Arc<str>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Message,
    Emote,
    Notice,
    /// Joins, parts, nick changes (hidden when `show_joins = false`).
    Membership,
    /// Topic/name changes and other room state.
    Event,
    /// Client messages.
    Info,
    Error,
    /// An encrypted event we could not decrypt (yet).
    Utd,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub ts: DateTime<Local>,
    pub kind: LineKind,
    pub event_id: Option<OwnedEventId>,
    /// Set on our own messages until the server echo replaces them.
    pub txn_id: Option<OwnedTransactionId>,
    pub sender: Option<OwnedUserId>,
    pub nick: String,
    pub body: String,
    pub own: bool,
    pub highlight: bool,
    pub pending: bool,
    pub edited: bool,
    pub redacted: bool,
    pub thread: bool,
    /// The root of the thread this message is in.
    pub thread_root: Option<OwnedEventId>,
    /// Replies in the thread this message starts (from the server's summary).
    pub thread_count: Option<u64>,
    pub reply_to: Option<OwnedEventId>,
    /// Filled in by the window when the replied-to line is known.
    pub reply_nick: Option<String>,
    /// Who wrote the message this replies to (for your nicknames).
    pub reply_sender: Option<OwnedUserId>,
    pub reply_snippet: Option<String>,
    /// Attachment carried by an image/file/audio/video message.
    pub media: Option<Media>,
    /// Upload progress of our own pending attachment.
    pub progress: Option<u8>,
    /// Our own message, kept to send when the server can be reached.
    pub waiting: bool,
    /// Formatted version of `body`, when the message had HTML formatting.
    pub rich: Option<Vec<crate::richtext::Span>>,
    /// Spoilers shown (toggled with `v` in selection mode).
    pub reveal: bool,
    /// Megolm session needed to decrypt a `Utd` line.
    pub utd_session: Option<String>,
    /// Reaction key -> reacting nicks.
    pub reactions: Vec<(String, Vec<String>)>,
    /// Shown from the local cache at startup, before the server answered.
    pub cached: bool,
    /// A join, leave or rename, which can fold into its neighbours.
    pub fold: Option<Fold>,
    /// A poll this message starts, with the votes seen so far.
    pub poll: Option<Box<Poll>>,
    /// Earlier versions of an edited message: when each was written, and
    /// its text, oldest first.
    pub versions: Vec<(DateTime<Local>, String)>,
    /// When the text shown now was written, if it's an edit.
    pub edited_at: Option<DateTime<Local>>,
}

/// Membership noise that runs of fold into one summary line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fold {
    Joined(String),
    Left(String),
    Renamed { from: String, to: String },
}

impl Line {
    pub fn new(kind: LineKind, body: impl Into<String>) -> Self {
        Self {
            ts: Local::now(),
            kind,
            event_id: None,
            txn_id: None,
            sender: None,
            nick: String::new(),
            body: body.into(),
            own: false,
            highlight: false,
            pending: false,
            edited: false,
            redacted: false,
            thread: false,
            thread_root: None,
            thread_count: None,
            reply_to: None,
            reply_nick: None,
            reply_sender: None,
            reply_snippet: None,
            media: None,
            progress: None,
            waiting: false,
            rich: None,
            reveal: false,
            utd_session: None,
            reactions: Vec::new(),
            cached: false,
            fold: None,
            poll: None,
            versions: Vec::new(),
            edited_at: None,
        }
    }

    pub fn info(body: impl Into<String>) -> Self {
        Self::new(LineKind::Info, body)
    }

    pub fn error(body: impl Into<String>) -> Self {
        Self::new(LineKind::Error, body)
    }

    /// What the message says, formatted text flattened (for snippets).
    pub fn text(&self) -> String {
        match &self.rich {
            Some(spans) if !self.redacted => crate::richtext::plain(spans),
            _ => self.body.clone(),
        }
    }

    pub fn is_message(&self) -> bool {
        matches!(self.kind, LineKind::Message | LineKind::Emote | LineKind::Notice | LineKind::Utd)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// Shown inline, and full screen with `z`.
    Image,
    /// Played in a video player; shows its poster frame inline if it has one.
    Video,
    /// Played through the system's audio player.
    Audio,
    File,
}

impl Media {
    /// Whether it has something to show inline: an image, or a video's
    /// poster frame.
    pub fn has_picture(&self) -> bool {
        self.kind == MediaKind::Image || (self.kind == MediaKind::Video && self.thumbnail.is_some())
    }

    /// Whether it may be a moving picture, worth fetching whole rather than
    /// a (still) thumbnail: a GIF, unless its sender says it's still, or
    /// whatever its sender says moves.
    pub fn may_move(&self) -> bool {
        let gif = self.mimetype.as_deref() == Some("image/gif") || self.filename.to_lowercase().ends_with(".gif");
        self.kind == MediaKind::Image && self.animated.unwrap_or(gif)
    }

    pub fn playable(&self) -> bool {
        matches!(self.kind, MediaKind::Audio | MediaKind::Video)
    }

    /// An extension players can go by: the file name's, or one for its MIME type.
    pub fn extension(&self) -> Option<String> {
        let sane = |e: &str| (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric());
        let from_name = self.filename.rsplit_once('.').map(|(_, e)| e.to_lowercase()).filter(|e| sane(e));
        from_name.or_else(|| {
            let mime = self.mimetype.as_deref()?;
            let ext = mime_guess::get_mime_extensions_str(mime)?.first()?;
            sane(ext).then(|| (*ext).to_owned())
        })
    }
}

#[derive(Clone, Debug)]
pub struct Media {
    pub source: MediaSource,
    pub filename: String,
    pub kind: MediaKind,
    /// The sender's MIME type, e.g. `video/mp4`.
    pub mimetype: Option<String>,
    /// A sender-provided thumbnail (the only kind encrypted images have).
    pub thumbnail: Option<MediaSource>,
    pub size: Option<u64>,
    /// Whether the sender says the image moves (`is_animated`): GIFs are
    /// taken to unless it says not.
    pub animated: Option<bool>,
}

/// A poll (Element's `org.matrix.msc3381` events, or the stable
/// `m.poll` ones), with the votes seen so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Poll {
    pub question: String,
    /// (id, text), in order.
    pub answers: Vec<(String, String)>,
    /// How many answers one vote may pick.
    pub max: usize,
    /// Results show while it's open, not only once it's ended.
    pub disclosed: bool,
    /// Started with the stable events, so votes go back that way.
    pub stable: bool,
    /// Each voter's latest vote: when (ms since 1970), and what.
    pub votes: BTreeMap<OwnedUserId, (u64, Vec<String>)>,
    /// You, once you've voted.
    pub me: Option<OwnedUserId>,
    /// When it ended (ms since 1970).
    pub ended: Option<u64>,
}

impl Poll {
    /// A vote, unless it came after the end, or before one of theirs
    /// already counted.
    pub fn vote(&mut self, user: &UserId, at: u64, answers: &[String], own: bool) {
        if self.ended.is_some_and(|end| at > end) || self.votes.get(user).is_some_and(|(t, _)| *t > at) {
            return;
        }
        if own {
            self.me = Some(user.to_owned());
        }
        self.votes.insert(user.to_owned(), (at, answers.to_vec()));
    }

    /// What a vote counts for: answers the poll has, up to `max`; none
    /// when it picked none of them (a spoiled vote).
    fn counted<'a>(&self, picked: &'a [String]) -> Vec<&'a str> {
        let mut out: Vec<&str> = Vec::new();
        for id in picked {
            if self.answers.iter().any(|(a, _)| a == id) && !out.contains(&id.as_str()) {
                out.push(id);
            }
        }
        out.truncate(self.max.max(1));
        out
    }

    /// Votes for each answer, in order.
    pub fn counts(&self) -> Vec<usize> {
        let mut counts = vec![0; self.answers.len()];
        for (_, picked) in self.votes.values() {
            for id in self.counted(picked) {
                if let Some(i) = self.answers.iter().position(|(a, _)| a == id) {
                    counts[i] += 1;
                }
            }
        }
        counts
    }

    /// How many voted (spoiled votes aside).
    pub fn voters(&self) -> usize {
        self.votes.values().filter(|(_, picked)| !self.counted(picked).is_empty()).count()
    }

    /// The answers you picked.
    pub fn mine(&self) -> Vec<&str> {
        self.me.as_ref().and_then(|me| self.votes.get(me)).map(|(_, picked)| self.counted(picked)).unwrap_or_default()
    }

    /// Whether results show: disclosed polls while open, every poll once
    /// it's ended.
    pub fn results_shown(&self) -> bool {
        self.disclosed || self.ended.is_some()
    }
}

/// A message to send later (`/later`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scheduled {
    pub room_id: OwnedRoomId,
    /// The room's name when it was scheduled, for the list.
    pub room: String,
    /// When to send it (seconds since 1970).
    pub due: i64,
    pub text: String,
}

/// One converted timeline event.
#[derive(Clone, Debug)]
pub enum Item {
    Line(Line),
    /// `m.replace`: `fallback` is shown if the original is not loaded.
    Edit {
        target: OwnedEventId,
        body: String,
        fallback: Line,
    },
    Redact {
        target: OwnedEventId,
    },
    React {
        target: OwnedEventId,
        key: String,
        nick: String,
        /// Yours.
        own: bool,
    },
    /// A vote in the poll `target` starts.
    PollVote {
        target: OwnedEventId,
        user: OwnedUserId,
        at: u64,
        answers: Vec<String>,
        own: bool,
    },
    /// The poll `target` starts has ended (said by `user`).
    PollEnd {
        target: OwnedEventId,
        user: OwnedUserId,
        at: u64,
    },
}

impl Item {
    /// The event an edit, redaction or reaction applies to.
    pub fn target(&self) -> Option<&EventId> {
        match self {
            Item::Line(_) => None,
            Item::Edit { target, .. }
            | Item::Redact { target }
            | Item::React { target, .. }
            | Item::PollVote { target, .. }
            | Item::PollEnd { target, .. } => Some(target),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceKind {
    Online,
    Away,
    Offline,
}

/// Someone's presence, as last reported by the server.
#[derive(Clone, Debug)]
pub struct Presence {
    pub kind: PresenceKind,
    pub status: Option<String>,
    pub last_active: Option<DateTime<Local>>,
}

impl Presence {
    /// `online`, `away: lunch`, `last seen 3h ago`…
    pub fn describe(&self) -> String {
        let base = match self.kind {
            PresenceKind::Online => "online".to_owned(),
            PresenceKind::Away => "away".to_owned(),
            PresenceKind::Offline => match self.last_active {
                Some(at) => format!("last seen {}", ago(Local::now() - at)),
                None => "offline".to_owned(),
            },
        };
        match self.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(status) => format!("{base}: {status}"),
            None => base,
        }
    }
}

pub fn ago(elapsed: chrono::TimeDelta) -> String {
    let mins = elapsed.num_minutes();
    match mins {
        ..1 => "just now".to_owned(),
        1..60 => format!("{mins}m ago"),
        60..1440 => format!("{}h ago", mins / 60),
        _ => format!("{}d ago", mins / 1440),
    }
}

/// A joined space and the rooms it lists.
#[derive(Clone, Debug)]
pub struct SpaceInfo {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub children: Vec<OwnedRoomId>,
}

#[derive(Clone, Debug)]
pub struct RoomMeta {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub topic: Option<String>,
    pub encrypted: bool,
    pub is_dm: bool,
    /// The other person in a direct-message room.
    pub dm_user: Option<OwnedUserId>,
    /// Spaces group rooms; they don't get a chat window.
    pub is_space: bool,
    /// The room's avatar, or the other person's for a DM without one.
    pub avatar: Option<OwnedMxcUri>,
    /// Where you stopped reading (`m.fully_read`), shared with other clients.
    pub fully_read: Option<OwnedEventId>,
    pub unread: u64,
    pub highlights: u64,
    /// `m.room.pinned_events`, oldest pin first.
    pub pinned: Vec<OwnedEventId>,
    /// Tagged `m.favourite` (Element's Favourites): first in the window list.
    pub favourite: bool,
    /// Bridges to other chat networks: where its messages also go.
    pub bridges: Vec<Bridge>,
    /// Marked unread (`m.marked_unread`), here or in another client: to
    /// come back to.
    pub marked_unread: bool,
}

/// A bridge to another chat network, from the room's `m.bridge` state
/// (MSC2346; mautrix's bridges send `uk.half-shot.bridge` too).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bridge {
    /// "Discord", "Telegram", "IRC"…
    pub protocol: String,
    /// The server, guild or workspace there.
    pub network: Option<String>,
    /// The channel or chat there.
    pub channel: Option<String>,
    /// The bridge's bot account.
    pub bot: Option<String>,
}

impl Bridge {
    /// From a bridge state event (as JSON). `None` for one with no
    /// protocol, which is how a bridge is taken away.
    pub fn from_event(event: &serde_json::Value) -> Option<Self> {
        let content = event.get("content")?;
        let name = |key: &str| {
            let part = content.get(key)?;
            let text = part.get("displayname").and_then(serde_json::Value::as_str).map(str::trim);
            let text = text.filter(|t| !t.is_empty()).map(ToOwned::to_owned);
            text.or_else(|| {
                // Only an id ("telegram"): as a name.
                let id = part.get("id").and_then(serde_json::Value::as_str)?.trim();
                let mut chars = id.chars();
                let first = chars.next()?;
                Some(first.to_uppercase().chain(chars).collect())
            })
        };
        let bot = content.get("bridgebot").and_then(serde_json::Value::as_str).map(ToOwned::to_owned);
        Some(Self { protocol: name("protocol")?, network: name("network"), channel: name("channel"), bot })
    }

    /// "Discord · Night Owls Guild · #general".
    pub fn describe(&self) -> String {
        let mut parts = vec![self.protocol.as_str()];
        parts.extend(self.network.as_deref());
        parts.extend(self.channel.as_deref().filter(|c| Some(*c) != self.network.as_deref()));
        parts.join(" · ")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Activity {
    #[default]
    None,
    /// State changes, joins/parts.
    Event,
    Message,
    Highlight,
}

/// What your power level lets you do in a room.
#[derive(Clone, Copy, Debug)]
pub struct Permissions {
    /// Sending (and therefore editing) messages.
    pub can_send: bool,
    pub can_redact_own: bool,
    pub can_redact_other: bool,
    pub can_pin: bool,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub user_id: OwnedUserId,
    /// `@` for admins, `+` for moderators, ' ' otherwise.
    pub prefix: char,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridges_say_where_messages_also_go() {
        let event = serde_json::json!({
            "type": "m.bridge",
            "state_key": "fi.mau.discord://example/123",
            "content": {
                "bridgebot": "@discordbot:example.org",
                "protocol": {"id": "discordgo", "displayname": "Discord"},
                "network": {"id": "123", "displayname": "Night Owls Guild"},
                "channel": {"id": "456", "displayname": "#general"},
            },
        });
        let bridge = Bridge::from_event(&event).unwrap();
        assert_eq!(bridge.describe(), "Discord · Night Owls Guild · #general");
        assert_eq!(bridge.bot.as_deref(), Some("@discordbot:example.org"));
        // Only ids: named after them.
        let ids = serde_json::json!({"content": {"protocol": {"id": "telegram"}, "channel": {"id": "Book Club"}}});
        assert_eq!(Bridge::from_event(&ids).unwrap().describe(), "Telegram · Book Club");
        // A DM bridged one to one has the same name for both.
        let same = serde_json::json!({"content": {"protocol": {"displayname": "Signal"}, "network": {"displayname": "sam"}, "channel": {"displayname": "sam"}}});
        assert_eq!(Bridge::from_event(&same).unwrap().describe(), "Signal · sam");
        // Taken away (empty content), or not a bridge at all.
        assert_eq!(Bridge::from_event(&serde_json::json!({"content": {}})), None);
        assert_eq!(Bridge::from_event(&serde_json::json!({"content": {"protocol": {"displayname": "  "}}})), None);
    }

    #[test]
    fn gifs_and_what_their_senders_say_moves_may_move() {
        let image = |filename: &str, mimetype: Option<&str>, animated| Media {
            source: MediaSource::Plain("mxc://example.org/a".into()),
            filename: filename.into(),
            kind: MediaKind::Image,
            mimetype: mimetype.map(Into::into),
            thumbnail: None,
            size: None,
            animated,
        };
        assert!(image("party.gif", Some("image/gif"), None).may_move());
        assert!(image("PARTY.GIF", None, None).may_move(), "by its name");
        assert!(!image("party.gif", Some("image/gif"), Some(false)).may_move(), "its sender says it's still");
        assert!(image("sticker.webp", Some("image/webp"), Some(true)).may_move());
        assert!(!image("photo.jpg", Some("image/jpeg"), None).may_move());
        let video = Media { kind: MediaKind::Video, ..image("clip.gif", Some("image/gif"), None) };
        assert!(!video.may_move(), "videos have players");
    }

    #[test]
    fn polls_count_each_voters_latest_vote() {
        let user = |n: &str| OwnedUserId::try_from(format!("@{n}:x.org")).unwrap();
        let ids = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut poll =
            Poll { answers: vec![("a".into(), "Yes".into()), ("b".into(), "No".into())], max: 1, ..Poll::default() };
        poll.vote(&user("sam"), 1, &ids(&["a"]), false);
        poll.vote(&user("sam"), 3, &ids(&["b"]), false);
        poll.vote(&user("sam"), 2, &ids(&["a"]), false);
        poll.vote(&user("jo"), 4, &ids(&["b", "a"]), true);
        poll.vote(&user("eve"), 5, &ids(&["zzz"]), false);
        assert_eq!(poll.counts(), [0, 2], "sam's latest, jo's first pick only, eve's spoiled");
        assert_eq!((poll.voters(), poll.mine()), (2, vec!["b"]));
        poll.ended = Some(6);
        poll.vote(&user("dee"), 7, &ids(&["a"]), false);
        assert_eq!(poll.counts(), [0, 2], "nothing after the end");
        assert!(poll.results_shown());
    }

    #[test]
    fn presence_reads_naturally() {
        let at = |mins| Some(Local::now() - chrono::TimeDelta::minutes(mins));
        let p =
            |kind, status: Option<&str>, last_active| Presence { kind, status: status.map(Into::into), last_active };
        assert_eq!(p(PresenceKind::Online, None, None).describe(), "online");
        assert_eq!(p(PresenceKind::Away, Some("lunch"), None).describe(), "away: lunch");
        assert_eq!(p(PresenceKind::Offline, None, at(5)).describe(), "last seen 5m ago");
        assert_eq!(p(PresenceKind::Offline, None, at(180)).describe(), "last seen 3h ago");
        assert_eq!(p(PresenceKind::Offline, None, at(3000)).describe(), "last seen 2d ago");
        assert_eq!(p(PresenceKind::Offline, Some("  "), None).describe(), "offline");
    }
}

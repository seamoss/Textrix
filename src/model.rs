//! Plain data shared between the Matrix side and the UI.

use std::sync::Arc;

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedTransactionId, OwnedUserId, events::room::MediaSource,
};

/// The dotfile name of a server, e.g. `"0x4f02"`.
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
    pub reply_snippet: Option<String>,
    /// Attachment carried by an image/file/audio/video message.
    pub media: Option<Media>,
    /// Upload progress of our own pending attachment.
    pub progress: Option<u8>,
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
            reply_snippet: None,
            media: None,
            progress: None,
            rich: None,
            reveal: false,
            utd_session: None,
            reactions: Vec::new(),
            cached: false,
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

#[derive(Clone, Debug)]
pub struct Media {
    pub source: MediaSource,
    pub filename: String,
    /// Images get inline previews.
    pub is_image: bool,
    /// A sender-provided thumbnail (the only kind encrypted images have).
    pub thumbnail: Option<MediaSource>,
    pub size: Option<u64>,
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
    },
}

impl Item {
    /// The event an edit, redaction or reaction applies to.
    pub fn target(&self) -> Option<&EventId> {
        match self {
            Item::Line(_) => None,
            Item::Edit { target, .. } | Item::Redact { target } | Item::React { target, .. } => Some(target),
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

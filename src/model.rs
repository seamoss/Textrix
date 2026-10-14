//! Plain data shared between the Matrix side and the UI.

use std::sync::Arc;

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, events::room::MediaSource,
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
            reply_to: None,
            reply_nick: None,
            reply_snippet: None,
            media: None,
            progress: None,
            rich: None,
            reveal: false,
            utd_session: None,
            reactions: Vec::new(),
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
    Edit { target: OwnedEventId, body: String, fallback: Line },
    Redact { target: OwnedEventId },
    React { target: OwnedEventId, key: String, nick: String },
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

#[derive(Clone, Debug)]
pub struct RoomMeta {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub topic: Option<String>,
    pub encrypted: bool,
    pub is_dm: bool,
    /// Where you stopped reading (`m.fully_read`), shared with other clients.
    pub fully_read: Option<OwnedEventId>,
    pub unread: u64,
    pub highlights: u64,
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
}

#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub user_id: OwnedUserId,
    /// `@` for admins, `+` for moderators, ' ' otherwise.
    pub prefix: char,
}

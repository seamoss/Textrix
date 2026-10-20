//! The home window (`/home`, F4): what's waiting for you everywhere, the
//! latest mentions, rooms with unread messages and invitations, with a key
//! to go to each.

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};

use crate::app::WinKey;
use crate::model::Activity;

/// Mentions shown, at most, and how old they may be.
pub const MENTIONS: usize = 5;
pub const MENTIONS_FOR: chrono::TimeDelta = chrono::TimeDelta::days(1);

#[derive(Default)]
pub struct Home {
    pub selected: usize,
    /// The invitation `d` was pressed on: pressing it again declines.
    pub declining: Option<OwnedRoomId>,
    /// What the last key did, shown until the next.
    pub note: Option<String>,
}

/// A room you've been invited to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub account: String,
    pub room_id: OwnedRoomId,
    pub name: String,
    /// Who invited you, when the server says.
    pub inviter: Option<String>,
}

/// One row, under its section.
#[derive(Clone, Debug)]
pub enum Row {
    Mention {
        key: WinKey,
        event: OwnedEventId,
        room: String,
        nick: String,
        text: String,
        ts: DateTime<Local>,
    },
    Unread {
        window: usize,
        name: String,
        count: u64,
        activity: Activity,
        /// The last message: who, what, when.
        last: Option<(String, String, DateTime<Local>)>,
    },
    Invite(Invitation),
}

impl Row {
    pub fn section(&self) -> &'static str {
        match self {
            Row::Mention { .. } => "Mentions",
            Row::Unread { .. } => "Unread",
            Row::Invite(_) => "Invites",
        }
    }
}

/// How long ago, in a few characters: `now`, `14m`, `3h`, `2d`.
pub fn age(ts: DateTime<Local>, now: DateTime<Local>) -> String {
    let mins = (now - ts).num_minutes().max(0);
    match mins {
        0 => "now".into(),
        1..60 => format!("{mins}m"),
        60..1440 => format!("{}h", mins / 60),
        _ => format!("{}d", mins / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_short() {
        let now = Local::now();
        let ago = |mins| age(now - chrono::TimeDelta::minutes(mins), now);
        assert_eq!([ago(0), ago(14), ago(180), ago(60 * 50)], ["now", "14m", "3h", "2d"]);
    }
}

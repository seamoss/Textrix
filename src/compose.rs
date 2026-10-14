//! Outgoing message content: Markdown, and mentions for nicks inserted by
//! tab completion.

use matrix_sdk::ruma::{
    OwnedUserId,
    events::{
        Mentions,
        room::message::{
            EmoteMessageEventContent, FormattedBody, MessageType, NoticeMessageEventContent, RoomMessageEventContent,
            TextMessageEventContent,
        },
    },
};

use crate::model::LineKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Say {
    Text,
    Emote,
    Notice,
}

impl Say {
    pub fn line_kind(self) -> LineKind {
        match self {
            Say::Text => LineKind::Message,
            Say::Emote => LineKind::Emote,
            Say::Notice => LineKind::Notice,
        }
    }
}

pub fn compose(text: &str, kind: Say, mentions: &[(String, OwnedUserId)], markdown: bool) -> RoomMessageEventContent {
    let text = &*crate::emoji::expand(text);
    let mentioned: Vec<&(String, OwnedUserId)> =
        mentions.iter().filter(|(nick, _)| !nick.is_empty() && text.contains(nick.as_str())).collect();

    let html = if markdown || !mentioned.is_empty() {
        let mut source = if markdown { text.to_owned() } else { escape_markdown(text) };
        for (nick, user) in &mentioned {
            let needle = if markdown { nick.clone() } else { escape_markdown(nick) };
            let link = format!("[{}](https://matrix.to/#/{user})", escape_markdown(nick));
            source = source.replacen(&needle, &link, 1);
        }
        FormattedBody::markdown(&source).map(|f| f.body)
    } else {
        None
    };

    let msgtype = match (kind, html) {
        (Say::Text, Some(html)) => MessageType::Text(TextMessageEventContent::html(text, html)),
        (Say::Text, None) => MessageType::Text(TextMessageEventContent::plain(text)),
        (Say::Emote, Some(html)) => MessageType::Emote(EmoteMessageEventContent::html(text, html)),
        (Say::Emote, None) => MessageType::Emote(EmoteMessageEventContent::plain(text)),
        (Say::Notice, Some(html)) => MessageType::Notice(NoticeMessageEventContent::html(text, html)),
        (Say::Notice, None) => MessageType::Notice(NoticeMessageEventContent::plain(text)),
    };

    let content = RoomMessageEventContent::new(msgtype);
    if mentioned.is_empty() {
        content
    } else {
        content.add_mentions(Mentions::with_user_ids(mentioned.into_iter().map(|(_, u)| u.clone())))
    }
}

fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\`*_{}[]()#+-.!<>|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_of(content: &RoomMessageEventContent) -> Option<String> {
        match &content.msgtype {
            MessageType::Text(t) => t.formatted.as_ref().map(|f| f.body.clone()),
            _ => None,
        }
    }

    #[test]
    fn plain_text_stays_plain() {
        let content = compose("hello there", Say::Text, &[], true);
        assert_eq!(content.body(), "hello there");
        assert!(html_of(&content).is_none());
        assert!(content.mentions.is_none());
    }

    #[test]
    fn completed_nicks_become_mention_pills() {
        let alice: OwnedUserId = "@alice:x.org".try_into().unwrap();
        let content = compose("Alice: hi", Say::Text, &[("Alice".into(), alice.clone())], false);
        assert_eq!(content.body(), "Alice: hi");
        let html = html_of(&content).expect("mention renders as html");
        assert!(html.contains(r#"<a href="https://matrix.to/#/@alice:x.org">Alice</a>"#), "{html}");
        assert!(content.mentions.unwrap().user_ids.contains(&alice));
    }

    #[test]
    fn shortcodes_become_emoji() {
        let content = compose("ship it :rocket: `:keep:`", Say::Text, &[], true);
        assert_eq!(content.body(), "ship it 🚀 `:keep:`");
    }

    #[test]
    fn mentions_not_in_text_are_dropped() {
        let bob: OwnedUserId = "@bob:x.org".try_into().unwrap();
        let content = compose("nobody here", Say::Text, &[("bob".into(), bob)], true);
        assert!(content.mentions.is_none());
    }
}

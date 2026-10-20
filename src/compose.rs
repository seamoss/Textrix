//! Outgoing message content: Markdown, mentions for nicks inserted by
//! tab completion, and custom emoji.

use matrix_sdk::ruma::{
    OwnedMxcUri, OwnedUserId,
    events::{
        Mentions,
        room::message::{
            EmoteMessageEventContent, FormattedBody, MessageFormat, MessageType, NoticeMessageEventContent,
            RoomMessageEventContent, TextMessageEventContent,
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

/// ¯\_(ツ)_/¯. Markdown reads its `\_` as an escaped underscore and drops
/// the backslash (Element's composer does the same), leaving `SHRUG_BROKEN`.
pub const SHRUG: &str = "¯\\_(ツ)_/¯";
pub const SHRUG_BROKEN: &str = "¯_(ツ)_/¯";

pub fn compose(text: &str, kind: Say, mentions: &[(String, OwnedUserId)], markdown: bool) -> RoomMessageEventContent {
    let text = &*crate::emoji::expand(text);
    let mentioned: Vec<&(String, OwnedUserId)> =
        mentions.iter().filter(|(nick, _)| !nick.is_empty() && text.contains(nick.as_str())).collect();

    let html = if markdown || !mentioned.is_empty() {
        // A typed shrug keeps its arm in the formatted version too.
        let mut source = if markdown { text.replace(SHRUG, &escape_markdown(SHRUG)) } else { escape_markdown(text) };
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

    let content = spoilers(RoomMessageEventContent::new(msgtype));
    if mentioned.is_empty() {
        content
    } else {
        content.add_mentions(Mentions::with_user_ids(mentioned.into_iter().map(|(_, u)| u.clone())))
    }
}

/// Custom emoji (`:shortcode:`s from `emoji`) in a message as the pictures
/// they stand for, `<img data-mx-emoticon>`, the way other clients send
/// them. The plain text keeps the shortcodes, and code stays as it is.
pub fn custom_emoji(mut content: RoomMessageEventContent, emoji: &[(String, OwnedMxcUri)]) -> RoomMessageEventContent {
    let (body, formatted) = match &content.msgtype {
        MessageType::Text(c) => (&c.body, &c.formatted),
        MessageType::Emote(c) => (&c.body, &c.formatted),
        MessageType::Notice(c) => (&c.body, &c.formatted),
        _ => return content,
    };
    if !emoji.iter().any(|(code, _)| body.contains(&format!(":{code}:"))) {
        return content;
    }
    let html = match formatted {
        Some(f) if f.format == MessageFormat::Html => f.body.clone(),
        _ => html_escape(body).replace('\n', "<br>"),
    };
    let with = emoticons(&html, emoji);
    if with == html {
        return content;
    }
    let body = body.clone();
    content.msgtype = match content.msgtype {
        MessageType::Emote(_) => MessageType::Emote(EmoteMessageEventContent::html(body, with)),
        MessageType::Notice(_) => MessageType::Notice(NoticeMessageEventContent::html(body, with)),
        _ => MessageType::Text(TextMessageEventContent::html(body, with)),
    };
    content
}

/// `html` with each known `:shortcode:` outside `<code>` and `<pre>` as
/// its picture.
fn emoticons(html: &str, emoji: &[(String, OwnedMxcUri)]) -> String {
    outside_code(html, |text| shortcodes(text, emoji))
}

/// `html` with `change` applied to its text outside `<code>` and `<pre>`.
fn outside_code(html: &str, change: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_code = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            let tag = &rest[..end];
            let name: String =
                tag[1..].trim_start_matches('/').chars().take_while(char::is_ascii_alphanumeric).collect();
            if name.eq_ignore_ascii_case("code") || name.eq_ignore_ascii_case("pre") {
                if tag.starts_with("</") {
                    in_code = in_code.saturating_sub(1);
                } else if !tag.ends_with("/>") {
                    in_code += 1;
                }
            }
            out.push_str(tag);
            rest = &rest[end..];
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        let text = &rest[..end];
        if in_code > 0 {
            out.push_str(text);
        } else {
            out.push_str(&change(text));
        }
        rest = &rest[end..];
    }
    out
}

fn shortcodes(text: &str, emoji: &[(String, OwnedMxcUri)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let found = after.find(':').and_then(|len| {
            let code = &after[..len];
            let safe = !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || "_+-.".contains(c));
            emoji.iter().find(|(c, _)| safe && c == code).map(|(c, picture)| (len, c, picture))
        });
        match found {
            Some((len, code, picture)) => {
                out.push_str(&format!(
                    "<img data-mx-emoticon src=\"{picture}\" alt=\":{code}:\" title=\":{code}:\" height=\"32\" />"
                ));
                rest = &after[len + 1..];
            }
            None => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `||text||` as a spoiler, the way Element marks them: hidden until
/// whoever's reading chooses to see it. The plain text keeps the bars.
fn spoilers(mut content: RoomMessageEventContent) -> RoomMessageEventContent {
    let (body, formatted) = match &content.msgtype {
        MessageType::Text(c) => (&c.body, &c.formatted),
        MessageType::Emote(c) => (&c.body, &c.formatted),
        MessageType::Notice(c) => (&c.body, &c.formatted),
        _ => return content,
    };
    if body.matches("||").count() < 2 {
        return content;
    }
    let html = match formatted {
        Some(f) if f.format == MessageFormat::Html => f.body.clone(),
        _ => html_escape(body).replace('\n', "<br>"),
    };
    let with = outside_code(&html, |text| {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("||") {
            let after = &rest[start + 2..];
            match after.find("||").filter(|&end| end > 0 && !after[..end].trim().is_empty()) {
                Some(end) => {
                    out.push_str(&rest[..start]);
                    out.push_str(&format!("<span data-mx-spoiler>{}</span>", &after[..end]));
                    rest = &after[end + 2..];
                }
                None => break,
            }
        }
        out.push_str(rest);
        out
    });
    if with == html {
        return content;
    }
    let body = body.clone();
    content.msgtype = match content.msgtype {
        MessageType::Emote(_) => MessageType::Emote(EmoteMessageEventContent::html(body, with)),
        MessageType::Notice(_) => MessageType::Notice(NoticeMessageEventContent::html(body, with)),
        _ => MessageType::Text(TextMessageEventContent::html(body, with)),
    };
    content
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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

    #[test]
    fn bars_make_spoilers() {
        let sent = compose("the butler ||did it|| and `||code||` stays", Say::Text, &[], true);
        let html = html_of(&sent).unwrap();
        assert!(html.contains("<span data-mx-spoiler>did it</span>"), "{html}");
        assert!(html.contains("<code>||code||</code>"), "not in code: {html}");
        let MessageType::Text(text) = &sent.msgtype else { panic!() };
        assert_eq!(text.body, "the butler ||did it|| and `||code||` stays");
        let plain = compose("a ||b|| c", Say::Text, &[], false);
        assert_eq!(html_of(&plain).unwrap(), "a <span data-mx-spoiler>b</span> c");
        for none in ["a || b", "x |||| y", "or || ||"] {
            assert!(html_of(&compose(none, Say::Text, &[], false)).is_none(), "{none}");
        }
    }

    #[test]
    fn custom_emoji_go_as_pictures() {
        let emoji = vec![("blobcat".to_owned(), OwnedMxcUri::from("mxc://x.org/blob"))];
        let sent = custom_emoji(compose("hi :blobcat: and `:blobcat:` and :nope:", Say::Text, &[], true), &emoji);
        let html = html_of(&sent).unwrap();
        assert_eq!(html.matches("data-mx-emoticon").count(), 1, "not in code: {html}");
        assert!(html.contains(r#"src="mxc://x.org/blob" alt=":blobcat:""#) && html.contains(":nope:"), "{html}");
        let MessageType::Text(text) = &sent.msgtype else { panic!() };
        assert_eq!(text.body, "hi :blobcat: and `:blobcat:` and :nope:", "the plain text keeps the shortcodes");

        let plain = custom_emoji(compose("a <b> :blobcat:\nline", Say::Emote, &[], false), &emoji);
        assert!(matches!(plain.msgtype, MessageType::Emote(_)));
        let html = html_of(&plain).unwrap();
        assert!(html.starts_with("a &lt;b&gt; <img") && html.ends_with("<br>line"), "{html}");
        let none = compose("nothing here", Say::Text, &[], false);
        assert!(html_of(&custom_emoji(none, &emoji)).is_none(), "nothing to change");
    }

    fn html_of(content: &RoomMessageEventContent) -> Option<String> {
        match &content.msgtype {
            MessageType::Text(t) => t.formatted.as_ref().map(|f| f.body.clone()),
            MessageType::Emote(e) => e.formatted.as_ref().map(|f| f.body.clone()),
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
    fn shrugs_keep_their_arm() {
        let alone = compose(&format!("well {SHRUG}"), Say::Text, &[], true);
        assert_eq!(alone.body(), format!("well {SHRUG}"));
        assert!(html_of(&alone).is_none_or(|html| html.contains(SHRUG)), "{:?}", html_of(&alone));
        let formatted = compose(&format!("**sure** {SHRUG}"), Say::Text, &[], true);
        let html = html_of(&formatted).unwrap();
        assert!(html.contains(&format!("<strong>sure</strong> {SHRUG}")), "{html}");
    }

    #[test]
    fn mentions_not_in_text_are_dropped() {
        let bob: OwnedUserId = "@bob:x.org".try_into().unwrap();
        let content = compose("nobody here", Say::Text, &[("bob".into(), bob)], true);
        assert!(content.mentions.is_none());
    }
}

//! Matrix timeline events -> irssi-style lines.

use std::collections::HashMap;

use chrono::{DateTime, Local};
use matrix_sdk::{
    Room,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    ruma::{
        MilliSecondsSinceUnixEpoch, OwnedTransactionId, OwnedUserId, UserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent, SyncStateEvent,
            message::TextContentBlock,
            poll::{start::PollKind, unstable_start::UnstablePollStartEventContent},
            room::{
                member::MembershipChange,
                message::{AudioMessageEventContent, MessageFormat, MessageType, Relation, VideoMessageEventContent},
                redaction::SyncRoomRedactionEvent,
            },
        },
    },
};
use serde::Deserialize;

use crate::{
    model::{Fold, Item, Line, LineKind, Media, MediaKind, Poll},
    richtext,
};

pub struct Converter<'a> {
    room: &'a Room,
    own: &'a UserId,
    nicks: HashMap<OwnedUserId, String>,
}

#[derive(Deserialize)]
struct Unsigned {
    transaction_id: Option<OwnedTransactionId>,
    #[serde(rename = "m.relations")]
    relations: Option<BundledRelations>,
}

/// The server's summary of the thread an event starts.
#[derive(Deserialize)]
struct BundledRelations {
    #[serde(rename = "m.thread")]
    thread: Option<ThreadSummary>,
}

#[derive(Deserialize)]
struct ThreadSummary {
    count: u64,
}

impl<'a> Converter<'a> {
    pub fn new(room: &'a Room, own: &'a UserId) -> Self {
        Self { room, own, nicks: HashMap::new() }
    }

    /// Use a display name from member state that came with the events
    /// (history pages), in preference to the store.
    pub fn seed_nick(&mut self, user: OwnedUserId, nick: String) {
        self.nicks.insert(user, nick);
    }

    /// Current display name of a room member, falling back to the localpart.
    pub async fn nick(&mut self, user: &UserId) -> String {
        if let Some(nick) = self.nicks.get(user) {
            return nick.clone();
        }
        let nick = match self.room.get_member_no_sync(user).await {
            Ok(Some(member)) => member.name().to_owned(),
            _ => user.localpart().to_owned(),
        };
        self.nicks.insert(user.to_owned(), nick.clone());
        nick
    }

    pub async fn convert(&mut self, ev: &TimelineEvent) -> Option<Item> {
        let any = match ev.raw().deserialize() {
            Ok(any) => any,
            Err(e) => {
                tracing::debug!("skipping undeserializable event: {e}");
                return None;
            }
        };
        let sender = any.sender().to_owned();
        let own = sender == self.own;
        let highlight = !own && ev.push_actions().is_some_and(|actions| actions.iter().any(|a| a.is_highlight()));
        let unsigned = ev.raw().get_field::<Unsigned>("unsigned").ok().flatten();
        let thread_count = unsigned.as_ref().and_then(|u| u.relations.as_ref()?.thread.as_ref()).map(|t| t.count);
        let txn_id = unsigned.and_then(|u| u.transaction_id);

        let mut line = Line::new(LineKind::Message, "");
        line.ts = to_local(any.origin_server_ts());
        line.event_id = Some(any.event_id().to_owned());
        line.nick = self.nick(&sender).await;
        line.sender = Some(sender);
        line.own = own;
        line.highlight = highlight;
        line.txn_id = txn_id;
        line.thread_count = thread_count.filter(|n| *n > 0);

        if let TimelineEventKind::UnableToDecrypt { utd_info, .. } = &ev.kind {
            line.kind = LineKind::Utd;
            line.utd_session = utd_info.session_id.clone();
            line.body = "** unable to decrypt: waiting for keys **".into();
            return Some(Item::Line(line));
        }

        match any {
            AnySyncTimelineEvent::MessageLike(msg) => match message_like(msg, line) {
                // A custom emoji's reaction key is its picture: show its name.
                Some(Item::React { target, key, nick, own }) if key.starts_with("mxc://") => {
                    let content = ev.raw().get_field::<serde_json::Value>("content").ok().flatten();
                    Some(Item::React { target, key: custom_reaction(content.as_ref()), nick, own })
                }
                other => other,
            },
            AnySyncTimelineEvent::State(state) => self.state(state, line).await,
        }
    }

    async fn state(&mut self, state: AnySyncStateEvent, mut line: Line) -> Option<Item> {
        let actor = line.nick.clone();
        line.highlight = false;
        line.kind = LineKind::Event;
        line.body = match state {
            AnySyncStateEvent::RoomMember(SyncStateEvent::Original(ev)) => {
                let target = &ev.state_key;
                let localpart = target.localpart();
                let current = ev.content.displayname.as_deref();
                let previous = ev.unsigned.prev_content.as_ref().and_then(|p| p.displayname.as_deref());
                let name = current.or(previous).unwrap_or(localpart);
                let reason = ev.content.reason.as_deref().map(|r| format!(" [{r}]")).unwrap_or_default();
                line.kind = LineKind::Membership;
                match ev.membership_change() {
                    MembershipChange::Joined | MembershipChange::InvitationAccepted => {
                        line.fold = Some(Fold::Joined(name.to_owned()));
                        format!("{name} [{target}] has joined")
                    }
                    MembershipChange::Left => {
                        line.fold = Some(Fold::Left(name.to_owned()));
                        format!("{name} [{target}] has left{reason}")
                    }
                    MembershipChange::Kicked => format!("{name} was kicked by {actor}{reason}"),
                    MembershipChange::Banned | MembershipChange::KickedAndBanned => {
                        format!("{name} was banned by {actor}{reason}")
                    }
                    MembershipChange::Unbanned => format!("{name} was unbanned by {actor}"),
                    MembershipChange::Invited => format!("{actor} invited {name} [{target}]"),
                    MembershipChange::InvitationRejected => format!("{name} rejected the invitation"),
                    MembershipChange::InvitationRevoked => format!("{actor} revoked the invitation for {name}"),
                    MembershipChange::Knocked => format!("{name} [{target}] is asking to join{reason}"),
                    MembershipChange::KnockAccepted => format!("{actor} accepted {name}'s request to join"),
                    MembershipChange::KnockRetracted => format!("{name} withdrew their request to join"),
                    MembershipChange::KnockDenied => format!("{actor} denied {name}'s request to join"),
                    MembershipChange::ProfileChanged { displayname_change: Some(change), .. } => {
                        let old = change.old.unwrap_or(localpart);
                        let new = change.new.unwrap_or(localpart);
                        line.fold = Some(Fold::Renamed { from: old.to_owned(), to: new.to_owned() });
                        format!("{old} is now known as {new}")
                    }
                    _ => return None,
                }
            }
            AnySyncStateEvent::RoomTopic(SyncStateEvent::Original(ev)) => {
                format!("{actor} changed the topic to: {}", ev.content.topic)
            }
            AnySyncStateEvent::RoomName(SyncStateEvent::Original(ev)) => {
                format!("{actor} changed the room name to {}", ev.content.name)
            }
            AnySyncStateEvent::RoomCreate(_) => format!("{actor} created the room"),
            AnySyncStateEvent::RoomEncryption(_) => format!("{actor} enabled end-to-end encryption"),
            AnySyncStateEvent::RoomTombstone(SyncStateEvent::Original(ev)) => {
                format!("room upgraded to {}: {}", ev.content.replacement_room, ev.content.body)
            }
            _ => return None,
        };
        Some(Item::Line(line))
    }
}

fn message_like(msg: AnySyncMessageLikeEvent, mut line: Line) -> Option<Item> {
    match msg {
        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(ev)) => {
            let content = ev.content;
            match content.relates_to {
                Some(Relation::Replacement(replacement)) => {
                    let (kind, body) = render_msgtype(&replacement.new_content.msgtype);
                    line.kind = kind;
                    line.body = body.clone();
                    line.rich = rich_of(&replacement.new_content.msgtype);
                    line.edited = true;
                    return Some(Item::Edit { target: replacement.event_id, body, fallback: line });
                }
                Some(Relation::Reply(reply)) => line.reply_to = Some(reply.in_reply_to.event_id),
                Some(Relation::Thread(thread)) => {
                    // Show what it answers: the quoted message for an explicit
                    // in-thread reply, otherwise the thread's root.
                    line.thread = true;
                    line.thread_root = Some(thread.event_id.clone());
                    line.reply_to = match thread.in_reply_to {
                        Some(r) if !thread.is_falling_back => Some(r.event_id),
                        _ => Some(thread.event_id),
                    };
                }
                _ => {}
            }
            let (kind, body) = render_msgtype(&content.msgtype);
            line.kind = kind;
            line.body = if line.reply_to.is_some() { strip_reply_fallback(&body).to_owned() } else { body };
            line.media = media_of(&content.msgtype);
            line.rich = rich_of(&content.msgtype);
            Some(Item::Line(line))
        }
        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Redacted(_)) => {
            line.redacted = true;
            line.body = "[message deleted]".into();
            Some(Item::Line(line))
        }
        AnySyncMessageLikeEvent::RoomEncrypted(_) => {
            line.kind = LineKind::Utd;
            line.body = "** unable to decrypt **".into();
            Some(Item::Line(line))
        }
        AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(ev)) => {
            let rel = ev.content.relates_to;
            Some(Item::React { target: rel.event_id, key: rel.key, nick: line.nick, own: line.own })
        }
        AnySyncMessageLikeEvent::RoomRedaction(SyncRoomRedactionEvent::Original(ev)) => {
            let target = ev.redacts.or(ev.content.redacts)?;
            Some(Item::Redact { target })
        }
        // Polls: Element's unstable events, and the stable ones.
        AnySyncMessageLikeEvent::UnstablePollStart(SyncMessageLikeEvent::Original(ev)) => {
            let UnstablePollStartEventContent::New(content) = ev.content else { return None };
            let start = content.poll_start;
            let poll = Poll {
                question: start.question.text,
                answers: start.answers.iter().map(|a| (a.id.clone(), a.text.clone())).collect(),
                max: usize::try_from(u64::from(start.max_selections)).unwrap_or(1),
                disclosed: matches!(start.kind, PollKind::Disclosed),
                ..Poll::default()
            };
            Some(poll_line(line, poll))
        }
        AnySyncMessageLikeEvent::PollStart(SyncMessageLikeEvent::Original(ev)) => {
            let start = ev.content.poll;
            let text = |t: &TextContentBlock| t.find_plain().unwrap_or_default().to_owned();
            let poll = Poll {
                question: text(&start.question.text),
                answers: start.answers.iter().map(|a| (a.id.clone(), text(&a.text))).collect(),
                max: usize::try_from(u64::from(start.max_selections)).unwrap_or(1),
                disclosed: matches!(start.kind, PollKind::Disclosed),
                stable: true,
                ..Poll::default()
            };
            Some(poll_line(line, poll))
        }
        AnySyncMessageLikeEvent::UnstablePollResponse(SyncMessageLikeEvent::Original(ev)) => Some(Item::PollVote {
            target: ev.content.relates_to.event_id,
            user: ev.sender,
            at: ev.origin_server_ts.get().into(),
            answers: ev.content.poll_response.answers,
            own: line.own,
        }),
        AnySyncMessageLikeEvent::PollResponse(SyncMessageLikeEvent::Original(ev)) => Some(Item::PollVote {
            target: ev.content.relates_to.event_id,
            user: ev.sender,
            at: ev.origin_server_ts.get().into(),
            answers: ev.content.selections.to_vec(),
            own: line.own,
        }),
        AnySyncMessageLikeEvent::UnstablePollEnd(SyncMessageLikeEvent::Original(ev)) => Some(Item::PollEnd {
            target: ev.content.relates_to.event_id,
            user: ev.sender,
            at: ev.origin_server_ts.get().into(),
        }),
        AnySyncMessageLikeEvent::PollEnd(SyncMessageLikeEvent::Original(ev)) => Some(Item::PollEnd {
            target: ev.content.relates_to.event_id,
            user: ev.sender,
            at: ev.origin_server_ts.get().into(),
        }),
        // A sticker is a picture with a name: shown like an image.
        AnySyncMessageLikeEvent::Sticker(SyncMessageLikeEvent::Original(ev)) => {
            let content = ev.content;
            line.body = format!("[sticker] {}", content.body);
            line.media = Some(Media {
                source: content.source.into(),
                filename: content.body,
                kind: MediaKind::Image,
                mimetype: content.info.mimetype.clone(),
                thumbnail: content.info.thumbnail_source.clone(),
                size: content.info.size.map(u64::from),
            });
            Some(Item::Line(line))
        }
        _ => None,
    }
}

/// A custom emoji reaction, by the shortcode clients send with it.
fn custom_reaction(content: Option<&serde_json::Value>) -> String {
    let code = content.and_then(|c| c.get("shortcode").or_else(|| c.get("com.beeper.reaction.shortcode")));
    match code.and_then(|c| c.as_str()) {
        Some(code) => format!(":{}:", code.trim_matches(':')),
        None => ":emoji:".to_owned(),
    }
}

/// The message that starts `poll`: its question, for logs, search and
/// notifications, with the poll itself to draw.
fn poll_line(mut line: Line, poll: Poll) -> Item {
    line.body = format!("[poll] {}", poll.question);
    line.poll = Some(Box::new(poll));
    Item::Line(line)
}

/// Styled spans for messages that carry HTML formatting.
pub fn rich_of(msgtype: &MessageType) -> Option<Vec<richtext::Span>> {
    let formatted = match msgtype {
        MessageType::Text(c) => c.formatted.as_ref(),
        MessageType::Emote(c) => c.formatted.as_ref(),
        MessageType::Notice(c) => c.formatted.as_ref(),
        _ => None,
    }?;
    let mut spans = (formatted.format == MessageFormat::Html).then(|| richtext::render(&formatted.body)).flatten()?;
    // Element's Markdown eats the shrug's arm in the formatted version, but
    // the plain one still has it: put it back.
    if msgtype.body().contains(crate::compose::SHRUG) {
        for span in &mut spans {
            span.text = span.text.replace(crate::compose::SHRUG_BROKEN, crate::compose::SHRUG);
        }
    }
    Some(spans)
}

fn render_msgtype(msgtype: &MessageType) -> (LineKind, String) {
    match msgtype {
        MessageType::Text(c) => (LineKind::Message, c.body.clone()),
        MessageType::Emote(c) => (LineKind::Emote, c.body.clone()),
        MessageType::Notice(c) => (LineKind::Notice, c.body.clone()),
        MessageType::ServerNotice(c) => (LineKind::Notice, c.body.clone()),
        MessageType::Image(c) => (LineKind::Message, attachment("image", c.filename.as_deref(), &c.body)),
        MessageType::Video(c) => (LineKind::Message, video_text(c)),
        MessageType::Audio(c) => (LineKind::Message, audio_text(c)),
        MessageType::File(c) => (LineKind::Message, attachment("file", c.filename.as_deref(), &c.body)),
        MessageType::Location(c) => (LineKind::Message, format!("[location] {} ({})", c.body, c.geo_uri)),
        MessageType::VerificationRequest(_) => (LineKind::Event, "requested key verification".into()),
        other => (LineKind::Message, other.body().to_owned()),
    }
}

/// `[image] photo.png`, plus the caption when the body is one (Matrix uses
/// `body` as the caption once `filename` is set and differs).
pub fn attachment(kind: &str, filename: Option<&str>, body: &str) -> String {
    match filename {
        Some(name) if name != body => format!("[{kind}] {name} — {body}"),
        _ => format!("[{kind}] {body}"),
    }
}

/// `[voice] 0:12 ▁▃▅▇▅▃▁` for a voice message, `[audio] song.mp3 (3:05)`
/// for other audio.
fn audio_text(c: &AudioMessageEventContent) -> String {
    let length = c.audio.as_ref().map(|a| a.duration).or_else(|| c.info.as_ref()?.duration);
    if c.voice.is_some() {
        let amplitudes: Vec<u64> =
            c.audio.iter().flat_map(|a| &a.waveform).map(|amplitude| u64::from(amplitude.get())).collect();
        let parts = [length.map(crate::audio::duration).unwrap_or_default(), crate::audio::waveform(&amplitudes, 24)];
        return format!("[voice] {}", parts.join(" ").trim()).trim_end().to_owned();
    }
    let text = attachment("audio", c.filename.as_deref(), &c.body);
    match length {
        Some(length) if !length.is_zero() => format!("{text} ({})", crate::audio::duration(length)),
        _ => text,
    }
}

/// `[video] clip.mp4 (0:42, 1920×1080)`.
fn video_text(c: &VideoMessageEventContent) -> String {
    let text = attachment("video", c.filename.as_deref(), &c.body);
    let info = c.info.as_deref();
    let mut details = Vec::new();
    if let Some(length) = info.and_then(|i| i.duration).filter(|d| !d.is_zero()) {
        details.push(crate::audio::duration(length));
    }
    if let (Some(w), Some(h)) = (info.and_then(|i| i.width), info.and_then(|i| i.height)) {
        details.push(format!("{w}×{h}"));
    }
    if details.is_empty() { text } else { format!("{text} ({})", details.join(", ")) }
}

fn media_of(msgtype: &MessageType) -> Option<Media> {
    let (source, filename, body) = match msgtype {
        MessageType::Image(c) => (&c.source, &c.filename, &c.body),
        MessageType::Video(c) => (&c.source, &c.filename, &c.body),
        MessageType::Audio(c) => (&c.source, &c.filename, &c.body),
        MessageType::File(c) => (&c.source, &c.filename, &c.body),
        _ => return None,
    };
    let size = |s: Option<matrix_sdk::ruma::UInt>| s.map(u64::from);
    let (kind, thumbnail, size, mimetype) = match msgtype {
        MessageType::Image(c) => {
            let info = c.info.as_deref();
            let thumbnail = info.and_then(|i| i.thumbnail_source.clone());
            (MediaKind::Image, thumbnail, size(info.and_then(|i| i.size)), info.and_then(|i| i.mimetype.clone()))
        }
        MessageType::Video(c) => {
            let info = c.info.as_deref();
            let thumbnail = info.and_then(|i| i.thumbnail_source.clone());
            (MediaKind::Video, thumbnail, size(info.and_then(|i| i.size)), info.and_then(|i| i.mimetype.clone()))
        }
        MessageType::Audio(c) => {
            let info = c.info.as_deref();
            (MediaKind::Audio, None, size(info.and_then(|i| i.size)), info.and_then(|i| i.mimetype.clone()))
        }
        MessageType::File(c) => {
            let info = c.info.as_deref();
            (MediaKind::File, None, size(info.and_then(|i| i.size)), info.and_then(|i| i.mimetype.clone()))
        }
        _ => return None,
    };
    Some(Media {
        source: source.clone(),
        filename: filename.clone().unwrap_or_else(|| body.clone()),
        kind,
        mimetype,
        thumbnail,
        size,
    })
}

/// Drop the legacy `> <@user> quoted text` block that older clients prepend
/// to replies.
fn strip_reply_fallback(body: &str) -> &str {
    if !body.starts_with("> ") {
        return body;
    }
    let mut rest = body;
    while let Some(line_end) = rest.find('\n') {
        if !rest.starts_with('>') {
            break;
        }
        rest = &rest[line_end + 1..];
    }
    rest.strip_prefix('\n').unwrap_or(rest)
}

pub fn to_local(ts: MilliSecondsSinceUnixEpoch) -> DateTime<Local> {
    DateTime::from_timestamp_millis(i64::from(ts.0)).map(|t| t.with_timezone(&Local)).unwrap_or_else(Local::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_emoji_reactions_go_by_their_shortcode() {
        assert_eq!(custom_reaction(Some(&serde_json::json!({ "shortcode": ":blobcat:" }))), ":blobcat:");
        assert_eq!(custom_reaction(Some(&serde_json::json!({ "com.beeper.reaction.shortcode": "party" }))), ":party:");
        assert_eq!(custom_reaction(None), ":emoji:");
    }

    #[test]
    fn elements_polls_come_in() {
        let convert = |json: serde_json::Value| {
            let ev: AnySyncMessageLikeEvent = serde_json::from_value(json).unwrap();
            message_like(ev, Line::new(LineKind::Message, "")).unwrap()
        };
        let start = convert(serde_json::json!({
            "type": "org.matrix.msc3381.poll.start", "event_id": "$p", "sender": "@sam:x.org", "origin_server_ts": 1,
            "content": {
                "org.matrix.msc3381.poll.start": {
                    "question": { "org.matrix.msc1767.text": "Pizza tonight?" },
                    "kind": "org.matrix.msc3381.poll.disclosed", "max_selections": 1,
                    "answers": [ { "id": "a1", "org.matrix.msc1767.text": "Yes" }, { "id": "a2", "org.matrix.msc1767.text": "No" } ]
                },
                "org.matrix.msc1767.text": "Pizza tonight?\n1. Yes\n2. No"
            }
        }));
        let Item::Line(line) = start else { panic!("a line") };
        let poll = line.poll.unwrap();
        assert_eq!(line.body, "[poll] Pizza tonight?");
        assert_eq!(
            (poll.question.as_str(), poll.answers.len(), poll.max, poll.disclosed),
            ("Pizza tonight?", 2, 1, true)
        );

        let vote = convert(serde_json::json!({
            "type": "org.matrix.msc3381.poll.response", "event_id": "$v", "sender": "@jo:x.org", "origin_server_ts": 5,
            "content": { "org.matrix.msc3381.poll.response": { "answers": ["a1"] },
                "m.relates_to": { "rel_type": "m.reference", "event_id": "$p" } }
        }));
        assert!(
            matches!(vote, Item::PollVote { ref target, at: 5, ref answers, .. } if target == "$p" && answers == &["a1"])
        );
        let end = convert(serde_json::json!({
            "type": "org.matrix.msc3381.poll.end", "event_id": "$e", "sender": "@sam:x.org", "origin_server_ts": 9,
            "content": { "org.matrix.msc3381.poll.end": {}, "org.matrix.msc1767.text": "Ended",
                "m.relates_to": { "rel_type": "m.reference", "event_id": "$p" } }
        }));
        assert!(matches!(end, Item::PollEnd { at: 9, .. }));
    }

    #[test]
    fn stickers_are_pictures() {
        let ev: AnySyncMessageLikeEvent = serde_json::from_value(serde_json::json!({
            "type": "m.sticker", "event_id": "$s", "sender": "@sam:x.org", "origin_server_ts": 1,
            "content": { "body": "Party parrot", "url": "mxc://x.org/parrot",
                "info": { "mimetype": "image/webp", "w": 256, "h": 256, "size": 4096 } }
        }))
        .unwrap();
        let Some(Item::Line(line)) = message_like(ev, Line::new(LineKind::Message, "")) else { panic!("a line") };
        assert_eq!(line.body, "[sticker] Party parrot");
        let media = line.media.unwrap();
        assert!(media.has_picture() && media.mimetype.as_deref() == Some("image/webp"), "{media:?}");
    }

    #[test]
    fn voice_messages_show_length_and_waveform() {
        use std::time::Duration;

        use matrix_sdk::ruma::events::room::{
            MediaSource,
            message::{
                AudioInfo, AudioMessageEventContent, MessageType, UnstableAmplitude, UnstableAudioDetailsContentBlock,
                UnstableVoiceContentBlock,
            },
        };
        let source = MediaSource::Plain("mxc://x.org/a".into());
        let mut voice = AudioMessageEventContent::new("Voice message".into(), source.clone());
        let waveform = [0, 256, 512, 1024, 512, 256, 0].map(UnstableAmplitude::new).to_vec();
        voice.audio = Some(UnstableAudioDetailsContentBlock::new(Duration::from_millis(11_600), waveform));
        voice.voice = Some(UnstableVoiceContentBlock::new());
        let (_, text) = super::render_msgtype(&MessageType::Audio(voice));
        assert_eq!(text, "[voice] 0:12 ▁▂▄█▄▂▁");
        let media = super::media_of(&MessageType::Audio(AudioMessageEventContent::new("x".into(), source.clone())));
        assert!(media.is_some_and(|m| m.kind == crate::model::MediaKind::Audio));

        let mut song = AudioMessageEventContent::new("song.mp3".into(), source);
        let mut info = AudioInfo::new();
        info.duration = Some(Duration::from_secs(185));
        song.info = Some(Box::new(info));
        assert_eq!(super::render_msgtype(&MessageType::Audio(song)).1, "[audio] song.mp3 (3:05)");
    }

    #[test]
    fn videos_show_length_size_and_poster() {
        use std::time::Duration;

        use matrix_sdk::ruma::{
            UInt,
            events::room::{
                MediaSource,
                message::{MessageType, VideoInfo, VideoMessageEventContent},
            },
        };
        let mut info = VideoInfo::new();
        info.duration = Some(Duration::from_secs(42));
        (info.width, info.height) = (Some(UInt::from(1920u32)), Some(UInt::from(1080u32)));
        info.mimetype = Some("video/mp4".into());
        info.size = Some(UInt::from(5_000_000u32));
        info.thumbnail_source = Some(MediaSource::Plain("mxc://x.org/poster".into()));
        let mut clip = VideoMessageEventContent::new("clip".into(), MediaSource::Plain("mxc://x.org/v".into()));
        clip.info = Some(Box::new(info));
        let clip = MessageType::Video(clip);
        assert_eq!(super::render_msgtype(&clip).1, "[video] clip (0:42, 1920×1080)");
        let media = super::media_of(&clip).unwrap();
        assert!(media.playable() && media.has_picture(), "plays, and shows its poster frame");
        assert_eq!(media.extension().as_deref(), Some("mp4"), "from the MIME type when the name has none");
        assert_eq!(media.size, Some(5_000_000));

        let bare = MessageType::Video(VideoMessageEventContent::new(
            "x.webm".into(),
            MediaSource::Plain("mxc://x.org/w".into()),
        ));
        let media = super::media_of(&bare).unwrap();
        assert!(!media.has_picture(), "no poster, nothing inline: the video itself is never fetched for it");
        assert_eq!(media.extension().as_deref(), Some("webm"));
        assert_eq!(super::render_msgtype(&bare).1, "[video] x.webm");
    }

    #[test]
    fn shrugs_from_element_get_their_arm_back() {
        use crate::compose::{SHRUG, SHRUG_BROKEN};
        use matrix_sdk::ruma::events::room::message::{MessageType, TextMessageEventContent};
        let body = format!("**sure** {SHRUG}");
        let html = format!("<strong>sure</strong> {SHRUG_BROKEN}");
        let spans = super::rich_of(&MessageType::Text(TextMessageEventContent::html(body, html))).unwrap();
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains(SHRUG), "{text}");
        // A message that really says it without the arm is left alone.
        let html = format!("<em>meh</em> {SHRUG_BROKEN}");
        let spans = super::rich_of(&MessageType::Text(TextMessageEventContent::html("meh", html))).unwrap();
        assert!(spans.iter().any(|s| s.text.contains(SHRUG_BROKEN)));
    }

    #[test]
    fn reply_fallback_is_removed() {
        assert_eq!(strip_reply_fallback("> <@a:b> hi\n> there\n\nreply"), "reply");
        assert_eq!(strip_reply_fallback("no quote"), "no quote");
        assert_eq!(strip_reply_fallback("> just a quote"), "> just a quote");
    }
}

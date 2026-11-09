//! `/media`: a room's pictures, videos, voice messages and files as tiles,
//! newest first, to look through and open; and its other tab, `/links`,
//! the links posted there, as a list.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{OwnedEventId, OwnedUserId};

use crate::app::WinKey;
use crate::model::{Line, MediaKind};
use crate::picture::Picture;

/// A tile's picture, in cells; two rows of caption go under it.
pub const PICTURE: (u16, u16) = (20, 6);
/// A tile with its caption, and the gap after it.
pub const TILE: (u16, u16) = (PICTURE.0 + 2, PICTURE.1 + 3);
/// Pictures kept; ones scrolled far away are fetched again on the way back.
const KEPT: usize = 120;

pub struct Gallery {
    /// The window whose media it shows.
    pub key: WinKey,
    /// The chosen tile, by its message, so it stays put as more arrive.
    pub selected: Option<OwnedEventId>,
    /// The first row of tiles on screen.
    pub top: usize,
    /// Tiles across, and rows of them on screen, as last drawn.
    pub across: usize,
    pub rows: usize,
    pub pictures: HashMap<OwnedEventId, Thumb>,
    order: VecDeque<OwnedEventId>,
    /// When it opened: Textrix's notes since then show at the bottom.
    pub opened: DateTime<Local>,
    pub tab: Tab,
    /// The links tab: the chosen link (where it was posted, and what), the
    /// first one on screen and how many fit.
    pub link: Option<(OwnedEventId, String)>,
    pub link_top: usize,
    pub link_rows: usize,
    /// What the links are narrowed to (`/` starts it), and whether you're
    /// still typing it.
    pub filter: String,
    pub filtering: bool,
}

/// What it shows: the room's media, or its links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Media,
    Links,
}

/// A link posted in the room, where it was posted last.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub url: String,
    pub event_id: OwnedEventId,
    pub sender: Option<OwnedUserId>,
    pub nick: String,
    pub ts: DateTime<Local>,
}

pub enum Thumb {
    Loading,
    Ready(Picture),
    Failed,
}

/// Which way to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

impl Gallery {
    pub fn new(key: WinKey) -> Self {
        Self {
            key,
            selected: None,
            top: 0,
            across: 1,
            rows: 1,
            pictures: HashMap::new(),
            order: VecDeque::new(),
            opened: Local::now(),
            tab: Tab::Media,
            link: None,
            link_top: 0,
            link_rows: 1,
            filter: String::new(),
            filtering: false,
        }
    }

    /// Where the chosen link is among `links` (the first when it's gone).
    pub fn link_index(&self, links: &[Link]) -> usize {
        let chosen = self.link.as_ref();
        chosen.and_then(|(id, url)| links.iter().position(|l| l.event_id == *id && l.url == *url)).unwrap_or(0)
    }

    /// Move up or down the links: left and right do nothing in a list.
    pub fn step_link(&mut self, links: &[Link], way: Move) {
        let Some(last) = links.len().checked_sub(1) else { return };
        let (at, page) = (self.link_index(links), self.link_rows.max(1));
        let to = match way {
            Move::Up => at.saturating_sub(1),
            Move::Down => at + 1,
            Move::PageUp => at.saturating_sub(page),
            Move::PageDown => at + page,
            Move::First => 0,
            Move::Last => last,
            Move::Left | Move::Right => at,
        };
        let link = &links[to.min(last)];
        self.link = Some((link.event_id.clone(), link.url.clone()));
    }

    /// Scroll so the chosen link is on screen.
    pub fn follow_link(&mut self, index: usize) {
        let rows = self.link_rows.max(1);
        if index < self.link_top {
            self.link_top = index;
        } else if index >= self.link_top + rows {
            self.link_top = index + 1 - rows;
        }
    }

    /// Where the chosen tile is among `items` (the first when it's gone).
    pub fn index(&self, items: &[&Line]) -> usize {
        let id = self.selected.as_ref();
        id.and_then(|id| items.iter().position(|l| l.event_id.as_ref() == Some(id))).unwrap_or(0)
    }

    pub fn step(&mut self, items: &[&Line], way: Move) {
        let Some(last) = items.len().checked_sub(1) else { return };
        let (at, across, page) = (self.index(items), self.across.max(1), self.across.max(1) * self.rows.max(1));
        let to = match way {
            Move::Left => at.saturating_sub(1),
            Move::Right => at + 1,
            // Off the top row stays put; past the last row goes to the last.
            Move::Up if at < across => at,
            Move::Up => at - across,
            Move::Down => at + across,
            Move::PageUp => at.saturating_sub(page),
            Move::PageDown => at + page,
            Move::First => 0,
            Move::Last => last,
        };
        self.selected = items[to.min(last)].event_id.clone();
    }

    /// Scroll so the chosen tile's row is on screen.
    pub fn follow(&mut self, index: usize) {
        let (row, rows) = (index / self.across.max(1), self.rows.max(1));
        if row < self.top {
            self.top = row;
        } else if row >= self.top + rows {
            self.top = row + 1 - rows;
        }
    }

    /// A picture that was asked for has come (or couldn't be had).
    pub fn store(&mut self, event_id: OwnedEventId, picture: Option<Picture>) {
        if !matches!(self.pictures.get(&event_id), Some(Thumb::Loading)) {
            return;
        }
        self.pictures.insert(event_id.clone(), picture.map_or(Thumb::Failed, Thumb::Ready));
        self.order.push_back(event_id);
        while self.order.len() > KEPT {
            if let Some(old) = self.order.pop_front() {
                self.pictures.remove(&old);
            }
        }
    }
}

/// A window's media, newest first.
pub fn items(lines: &VecDeque<Line>) -> Vec<&Line> {
    lines.iter().rev().filter(|l| !l.redacted && l.event_id.is_some() && l.media.is_some()).collect()
}

/// A window's links, newest message first (a message's in the order it
/// has them), each once, where it was posted last.
pub fn links(lines: &VecDeque<Line>) -> Vec<Link> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in lines.iter().rev().filter(|l| !l.redacted && l.is_message()) {
        let Some(event_id) = &line.event_id else { continue };
        for url in crate::links::urls(&line.body) {
            if seen.insert(url) {
                out.push(Link {
                    url: url.to_owned(),
                    event_id: event_id.clone(),
                    sender: line.sender.clone(),
                    nick: line.nick.clone(),
                    ts: line.ts,
                });
            }
        }
    }
    out
}

/// Whether a link is one the filter asks for: in its address, its page's
/// title, or who posted it, any case.
pub fn wanted(link: &Link, title: Option<&str>, who: &str, filter: &str) -> bool {
    let filter = filter.to_lowercase();
    [link.url.as_str(), title.unwrap_or(""), who].iter().any(|text| text.to_lowercase().contains(&filter))
}

/// What a tile with no picture says it is, big: `PDF`, `voice`…
pub fn label(line: &Line) -> String {
    let Some(media) = &line.media else { return String::new() };
    match media.kind {
        MediaKind::Image => "image".into(),
        MediaKind::Video => "▶ video".into(),
        MediaKind::Audio if line.body.starts_with("[voice]") => "♪ voice".into(),
        MediaKind::Audio => "♪ audio".into(),
        MediaKind::File => media.extension().map_or_else(|| "file".into(), |e| e.to_uppercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LineKind, Media};
    use matrix_sdk::ruma::events::room::MediaSource;

    fn media_line(n: usize, kind: MediaKind, name: &str) -> Line {
        let mut line = Line::new(LineKind::Message, format!("[file] {name}"));
        line.event_id = Some(format!("$m{n}").try_into().unwrap());
        line.media = Some(Media {
            source: MediaSource::Plain(format!("mxc://example.org/{n}").into()),
            filename: name.into(),
            kind,
            mimetype: None,
            thumbnail: None,
            size: None,
        });
        line
    }

    #[test]
    fn newest_first_and_only_media() {
        let mut lines = VecDeque::new();
        lines.push_back(media_line(1, MediaKind::Image, "a.png"));
        lines.push_back(Line::new(LineKind::Message, "just words"));
        lines.push_back(media_line(2, MediaKind::File, "notes.pdf"));
        let mut gone = media_line(3, MediaKind::Image, "b.png");
        gone.redacted = true;
        lines.push_back(gone);
        let names: Vec<&str> = items(&lines).iter().map(|l| l.media.as_ref().unwrap().filename.as_str()).collect();
        assert_eq!(names, ["notes.pdf", "a.png"]);
        assert_eq!(label(items(&lines)[0]), "PDF");
    }

    #[test]
    fn moving_through_the_grid() {
        let lines: VecDeque<Line> = (0..10).map(|n| media_line(n, MediaKind::Image, "x.png")).collect();
        let items = items(&lines);
        let key = WinKey { account: "t".into(), room_id: "!r:example.org".try_into().unwrap(), thread: None };
        let mut g = Gallery::new(key);
        (g.across, g.rows) = (4, 2);
        let at = |g: &Gallery| g.index(&items);
        assert_eq!(at(&g), 0, "the newest to start with");
        g.step(&items, Move::Up);
        assert_eq!(at(&g), 0);
        g.step(&items, Move::Down);
        g.step(&items, Move::Right);
        assert_eq!(at(&g), 5);
        g.step(&items, Move::Down);
        assert_eq!(at(&g), 9, "past the last row: the last tile");
        g.follow(at(&g));
        assert_eq!(g.top, 1, "its row scrolled into view");
        g.step(&items, Move::PageUp);
        assert_eq!(at(&g), 1);
        g.follow(at(&g));
        assert_eq!(g.top, 0);
        g.step(&items, Move::Last);
        assert_eq!(at(&g), 9);
    }

    #[test]
    fn links_come_newest_first_each_once() {
        let line = |id: &str, body: &str| {
            let mut line = Line::new(LineKind::Message, body);
            line.event_id = Some(id.try_into().unwrap());
            line.nick = "sam".into();
            line
        };
        let lines: VecDeque<Line> = [
            line("$1", "https://a.example/one and https://b.example/two"),
            line("$2", "no links"),
            line("$3", "again https://a.example/one"),
        ]
        .into();
        let all = links(&lines);
        let found: Vec<(&str, &str)> = all.iter().map(|l| (l.url.as_str(), l.event_id.as_str())).collect();
        assert_eq!(found, [("https://a.example/one", "$3"), ("https://b.example/two", "$1")]);

        let one = &all[0];
        assert!(wanted(one, Some("The One Page"), "sam", "one page"));
        assert!(wanted(one, None, "Sam", "SAM"), "who posted it, any case");
        assert!(!wanted(one, None, "sam", "two"));

        let mut g = Gallery::new(WinKey { account: "t".into(), room_id: "!r:x.org".try_into().unwrap(), thread: None });
        g.step_link(&all, Move::Down);
        assert_eq!(g.link_index(&all), 1);
        g.step_link(&all, Move::Down);
        assert_eq!(g.link_index(&all), 1, "stays at the last");
        g.link_rows = 1;
        g.follow_link(1);
        assert_eq!(g.link_top, 1);
    }
}

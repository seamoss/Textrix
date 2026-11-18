//! `/list`: rooms to join, in a panel. A server's public directory (yours,
//! or another's: `/list matrix.org`), or the rooms of a space you're in;
//! Tab goes from one to the next. Typing searches (a server, a moment after
//! you stop; a space's rooms at once), ↑/↓ chooses, the end loads more, and
//! Enter joins (or goes to a room you're in).

use std::time::{Duration, Instant};

use matrix_sdk::ruma::OwnedRoomId;

/// How long typing rests before a server is asked.
pub const SEARCH_WAIT: Duration = Duration::from_millis(350);

/// Where the rooms come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A server's public directory: yours (`None`) or another's.
    Server(Option<String>),
    /// A space's rooms, joined or not.
    Space { id: OwnedRoomId, name: String },
}

impl Source {
    pub fn title(&self) -> String {
        match self {
            Source::Server(None) => "your server".into(),
            Source::Server(Some(server)) => server.clone(),
            Source::Space { name, .. } => name.clone(),
        }
    }
}

/// A room in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirRoom {
    pub room_id: OwnedRoomId,
    /// Its address (`#room:server`), when it has one.
    pub alias: Option<String>,
    pub name: String,
    pub topic: Option<String>,
    pub members: u64,
    /// You're in it already.
    pub joined: bool,
    /// A space, not a room to talk in.
    pub space: bool,
    /// How you'd get in: "public", "knock", "space members"…
    pub how: String,
    /// Anyone can read it without joining.
    pub readable: bool,
}

/// What to ask for next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    Server { server: Option<String>, search: Option<String>, since: Option<String> },
    Space { id: OwnedRoomId, from: Option<String> },
}

pub struct Directory {
    pub account: String,
    /// What Tab goes through: a server's directory, then your spaces.
    pub sources: Vec<Source>,
    pub at: usize,
    pub query: String,
    /// The rooms, and the source and search they're for.
    pub rooms: Vec<DirRoom>,
    pub results_for: Option<(usize, String)>,
    pub selected: usize,
    /// Where more begins, if there is more.
    pub next: Option<String>,
    pub total: Option<u64>,
    pub loading: bool,
    pub error: Option<String>,
    /// When to ask (typing waits for a pause).
    pub due: Option<Instant>,
}

impl Directory {
    pub fn new(account: String, sources: Vec<Source>, query: String) -> Self {
        Self {
            account,
            sources,
            at: 0,
            query,
            rooms: Vec::new(),
            results_for: None,
            selected: 0,
            next: None,
            total: None,
            loading: false,
            error: None,
            due: Some(Instant::now()),
        }
    }

    pub fn source(&self) -> &Source {
        &self.sources[self.at]
    }

    /// What Tab would go to.
    pub fn next_source(&self) -> Option<&Source> {
        (self.sources.len() > 1).then(|| &self.sources[(self.at + 1) % self.sources.len()])
    }

    /// The rooms to show: a server's as it found them for the search; a
    /// space's, those the search matches (by name, address or topic).
    pub fn shown(&self) -> Vec<&DirRoom> {
        if !matches!(self.source(), Source::Space { .. }) {
            return self.rooms.iter().collect();
        }
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        self.rooms
            .iter()
            .filter(|r| {
                let text =
                    format!("{} {} {}", r.name, r.alias.as_deref().unwrap_or(""), r.topic.as_deref().unwrap_or(""))
                        .to_lowercase();
                words.iter().all(|w| text.contains(w))
            })
            .collect()
    }

    pub fn chosen(&self) -> Option<&DirRoom> {
        self.shown().get(self.selected).copied()
    }

    /// The search changed: a server is asked again after a pause; a space
    /// filters what it has.
    pub fn typed(&mut self) {
        self.selected = 0;
        if matches!(self.source(), Source::Server(_)) {
            self.due = Some(Instant::now() + SEARCH_WAIT);
        }
    }

    /// The next (or previous) place to look, from the top.
    pub fn switch(&mut self, back: bool) {
        let n = self.sources.len();
        if n < 2 {
            return;
        }
        self.at = if back { (self.at + n - 1) % n } else { (self.at + 1) % n };
        (self.rooms, self.results_for, self.next, self.total, self.error) = (Vec::new(), None, None, None, None);
        (self.selected, self.loading, self.due) = (0, false, Some(Instant::now()));
    }

    pub fn select(&mut self, by: isize) {
        let last = self.shown().len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
        // Near the end of what's loaded: the next page.
        if self.selected + 5 >= self.shown().len() && self.next.is_some() && !self.loading && self.due.is_none() {
            self.due = Some(Instant::now());
        }
    }

    /// What to ask for, now it's time: from the top for a new search or
    /// place, or the next page.
    pub fn fetch(&mut self) -> Option<Fetch> {
        self.due = None;
        let search = Some(self.query.trim().to_owned()).filter(|q| !q.is_empty());
        let current = self.results_for.as_ref() == Some(&(self.at, self.query_key()));
        let more = if current { self.next.clone() } else { None };
        if current && more.is_none() {
            return None;
        }
        self.loading = true;
        Some(match self.source().clone() {
            Source::Server(server) => Fetch::Server { server, search, since: more },
            Source::Space { id, .. } => Fetch::Space { id, from: more },
        })
    }

    /// What a space's rooms are kept by: not the search, which filters here.
    fn query_key(&self) -> String {
        if matches!(self.source(), Source::Space { .. }) { String::new() } else { self.query.trim().to_owned() }
    }

    /// Rooms arrived for `source` and `query`: the first page (a fresh
    /// list), or more. Ones for what's no longer asked are dropped.
    pub fn arrived(&mut self, source: usize, query: String, rooms: Result<Page, String>) {
        let key = if matches!(self.sources.get(source), Some(Source::Space { .. })) { String::new() } else { query };
        if source != self.at || key != self.query_key() {
            return;
        }
        self.loading = false;
        match rooms {
            Ok(page) => {
                if self.results_for.as_ref() != Some(&(source, key.clone())) {
                    (self.rooms, self.selected) = (Vec::new(), 0);
                }
                for room in page.rooms {
                    if !self.rooms.iter().any(|had| had.room_id == room.room_id) {
                        self.rooms.push(room);
                    }
                }
                (self.next, self.total, self.error) = (page.next, page.total.or(self.total), None);
                self.results_for = Some((source, key));
            }
            Err(error) => self.error = Some(error),
        }
    }
}

/// One answer: rooms, where more begins, and roughly how many in all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub rooms: Vec<DirRoom>,
    pub next: Option<String>,
    pub total: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(n: u32, name: &str) -> DirRoom {
        DirRoom {
            room_id: format!("!r{n}:example.org").try_into().unwrap(),
            alias: Some(format!("#{}:example.org", name.to_lowercase().replace(' ', "-"))),
            name: name.into(),
            topic: Some(format!("all about {name}")),
            members: u64::from(n),
            joined: false,
            space: false,
            how: "public".into(),
            readable: true,
        }
    }

    fn page(rooms: Vec<DirRoom>, next: Option<&str>) -> Result<Page, String> {
        Ok(Page { rooms, next: next.map(Into::into), total: Some(99) })
    }

    #[test]
    fn a_server_is_asked_after_typing_rests_and_old_answers_are_dropped() {
        let space = Source::Space { id: "!s:example.org".try_into().unwrap(), name: "Hobbies".into() };
        let mut d = Directory::new("t".into(), vec![Source::Server(None), space], String::new());
        assert_eq!(d.fetch(), Some(Fetch::Server { server: None, search: None, since: None }));
        d.arrived(0, String::new(), page(vec![room(1, "Night Owls"), room(2, "Book Club")], Some("p2")));
        assert_eq!(d.shown().len(), 2);
        // Typing: asked again once it rests, from the top.
        d.query = "owl".into();
        d.typed();
        assert!(d.due.is_some_and(|at| at > Instant::now()), "after a pause");
        assert_eq!(d.fetch(), Some(Fetch::Server { server: None, search: Some("owl".into()), since: None }));
        // The answer for the old search, late: dropped.
        d.arrived(0, String::new(), page(vec![room(3, "Stale")], None));
        assert_eq!(d.shown().len(), 2);
        d.arrived(0, "owl".into(), page(vec![room(1, "Night Owls")], None));
        assert_eq!(d.shown().iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Night Owls"]);
        assert_eq!(d.fetch(), None, "nothing more to ask");
    }

    #[test]
    fn the_end_of_the_list_loads_more() {
        let mut d = Directory::new("t".into(), vec![Source::Server(Some("matrix.org".into()))], String::new());
        d.fetch();
        d.arrived(0, String::new(), page((1..=10).map(|n| room(n, &format!("R{n}"))).collect(), Some("p2")));
        d.select(3);
        assert!(d.due.is_none(), "not near the end yet");
        d.select(3);
        assert!(d.due.is_some(), "near it: the next page");
        let Some(Fetch::Server { server, since, .. }) = d.fetch() else { panic!() };
        assert_eq!((server.as_deref(), since.as_deref()), (Some("matrix.org"), Some("p2")));
        d.arrived(0, String::new(), page(vec![room(10, "R10"), room(11, "R11")], None));
        assert_eq!(d.shown().len(), 11, "added to, without repeats");
        assert_eq!(d.selected, 6, "where you were");
    }

    #[test]
    fn a_spaces_rooms_are_searched_where_they_are() {
        let space = Source::Space { id: "!s:example.org".try_into().unwrap(), name: "Hobbies".into() };
        let mut d = Directory::new("t".into(), vec![Source::Server(None), space.clone()], String::new());
        d.switch(false);
        assert_eq!(d.source(), &space);
        assert_eq!(d.next_source(), Some(&Source::Server(None)));
        assert_eq!(d.fetch(), Some(Fetch::Space { id: "!s:example.org".try_into().unwrap(), from: None }));
        d.arrived(1, String::new(), page(vec![room(1, "Garden Club"), room(2, "Synth Nerds")], None));
        d.query = "synth".into();
        d.typed();
        assert!(d.due.is_none(), "no asking: it filters what it has");
        assert_eq!(d.chosen().map(|r| r.name.as_str()), Some("Synth Nerds"));
        d.query = "about garden".into();
        assert_eq!(d.shown().len(), 1, "topics count too");
    }
}

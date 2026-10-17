//! The quick switcher (Ctrl+K / Alt+K): fuzzy-find a window or a closed
//! room and go there.

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};

use crate::model::Activity;

/// The open switcher: what's typed, which entry is chosen, and what
/// choosing does.
#[derive(Debug, Default)]
pub struct Switcher {
    pub query: String,
    pub selected: usize,
    pub purpose: Purpose,
}

#[derive(Debug, Default)]
pub enum Purpose {
    /// Go to the window.
    #[default]
    Go,
    /// Send a copy of a message to the room (same server only).
    Forward {
        account: String,
        room_id: OwnedRoomId,
        event: OwnedEventId,
        /// The text as edited, when the message was edited since.
        edited: Option<String>,
    },
}

/// Where an entry leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Window(usize),
    /// A joined room whose window was closed.
    Closed {
        account: String,
        room_id: OwnedRoomId,
    },
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub target: Target,
    pub name: String,
    /// Server, "closed", …
    pub detail: String,
    pub activity: Activity,
    pub unread: u64,
    /// Positions (in characters) of `name` that matched the query.
    pub matched: Vec<usize>,
    /// The room's own name when you've named the window yourself, which
    /// finds it too.
    pub aka: Option<String>,
}

/// A candidate before ranking: `recency` is 0 for the window you were in
/// last, and grows from there.
pub struct Candidate {
    pub entry: Entry,
    pub number: Option<usize>,
    pub recency: usize,
}

/// Entries matching `query`, best first: match quality, then activity,
/// then how recently you were there.
pub fn rank(query: &str, candidates: Vec<Candidate>) -> Vec<Entry> {
    let query = query.trim();
    let mut scored: Vec<(i32, Candidate)> = candidates
        .into_iter()
        .filter_map(|mut c| {
            // Typing a number finds that window (exactly that one first).
            let number = c.number.map(|n| n.to_string()).unwrap_or_default();
            let by_number = match query.chars().all(|ch| ch.is_ascii_digit()) && !query.is_empty() {
                true if number == query => 300,
                true if number.starts_with(query) => 200,
                _ => 0,
            };
            let by_aka = || c.entry.aka.as_deref().and_then(|aka| fuzzy(query, aka)).map(|(s, _)| (s - 1, Vec::new()));
            let (score, matched) = match fuzzy(query, &c.entry.name).or_else(by_aka) {
                Some(found) => found,
                None if by_number > 0 => (0, Vec::new()),
                None => return None,
            };
            c.entry.matched = matched;
            Some((score + by_number, c))
        })
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then(b.entry.activity.cmp(&a.entry.activity))
            .then(a.recency.cmp(&b.recency))
            .then(a.number.unwrap_or(usize::MAX).cmp(&b.number.unwrap_or(usize::MAX)))
    });
    scored.into_iter().map(|(_, c)| c.entry).collect()
}

/// Case-insensitive fuzzy match: (score, matched character positions), or
/// `None` if `query`'s characters don't all appear in order. A run of the
/// query inside the name scores highest, more so at the start of a word.
pub fn fuzzy(query: &str, name: &str) -> Option<(i32, Vec<usize>)> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let lower: Vec<char> = name.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    let word_start = |i: usize| i == 0 || !lower[i - 1].is_alphanumeric();
    let compact: Vec<(usize, char)> = lower.iter().copied().enumerate().filter(|(_, c)| !c.is_whitespace()).collect();

    // A contiguous run (ignoring spaces in the name), best at a word start.
    let runs: Vec<usize> = (0..compact.len())
        .filter(|&s| {
            compact.len() - s >= query.len()
                && compact[s..s + query.len()].iter().map(|(_, c)| *c).eq(query.iter().copied())
        })
        .collect();
    let run = runs.iter().copied().find(|&s| word_start(compact[s].0)).or(runs.first().copied());
    let run_at_word = run.is_some_and(|s| word_start(compact[s].0));
    if let Some(start) = run.filter(|_| run_at_word) {
        let positions: Vec<usize> = compact[start..start + query.len()].iter().map(|(i, _)| *i).collect();
        return Some((130 - positions[0].min(30) as i32, positions));
    }

    // The characters in order, taking the starts of words where possible:
    // all initials ("tt" for Tetrix Tests) beats a run inside a word.
    let mut positions = Vec::new();
    let mut from = 0;
    let mut score = 0;
    let mut initials = true;
    for &q in &query {
        let Some(pos) = (from..lower.len())
            .find(|&i| lower[i] == q && word_start(i))
            .or_else(|| (from..lower.len()).find(|&i| lower[i] == q))
        else {
            break;
        };
        initials &= word_start(pos);
        score += if word_start(pos) { 8 } else { 1 };
        if positions.last().is_some_and(|&p: &usize| p + 1 == pos) {
            score += 4;
        }
        positions.push(pos);
        from = pos + 1;
    }
    let complete = positions.len() == query.len();
    if complete && initials {
        return Some((90 + query.len() as i32, positions));
    }
    if let Some(start) = run {
        let positions: Vec<usize> = compact[start..start + query.len()].iter().map(|(i, _)| *i).collect();
        return Some((60 - positions[0].min(30) as i32, positions));
    }
    complete.then_some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, number: usize, recency: usize, activity: Activity) -> Candidate {
        Candidate {
            entry: Entry {
                target: Target::Window(number - 1),
                name: name.into(),
                detail: String::new(),
                activity,
                unread: 0,
                matched: Vec::new(),
                aka: None,
            },
            number: Some(number),
            recency,
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn fuzzy_prefers_runs_and_word_starts() {
        assert_eq!(fuzzy("tests", "Tetrix Tests").unwrap().1, vec![7, 8, 9, 10, 11]);
        assert_eq!(fuzzy("tt", "Tetrix Tests").unwrap().1, vec![0, 7], "word starts over the nearest t");
        assert_eq!(fuzzy("tetrixtests", "Tetrix Tests").unwrap().1.len(), 11, "spaces don't break a run");
        assert!(fuzzy("xyz", "Tetrix Tests").is_none());
        let (word, _) = fuzzy("con", "Disaster Control").unwrap();
        let (inner, _) = fuzzy("ont", "Disaster Control").unwrap();
        assert!(word > inner);
    }

    #[test]
    fn ranking_uses_score_activity_recency_and_numbers() {
        let all = || {
            vec![
                cand("status", 1, 9, Activity::None),
                cand("Disaster Control", 2, 1, Activity::None),
                cand("LLM Party", 3, 0, Activity::None),
                cand("Tetrix Tests", 10, 2, Activity::Message),
            ]
        };
        // Nothing typed: activity first, then where you were most recently.
        assert_eq!(names(&rank("", all())), ["Tetrix Tests", "LLM Party", "Disaster Control", "status"]);
        assert_eq!(names(&rank("con", all())), ["Disaster Control"]);
        let mut renamed = all();
        (renamed[1].entry.name, renamed[1].entry.aka) = ("Ops".into(), Some("Disaster Control".into()));
        assert_eq!(names(&rank("disaster", renamed)), ["Ops"], "the room's own name finds a renamed window");
        let mut with_dm = all();
        with_dm.push(cand("butts_mcgee", 4, 3, Activity::None));
        assert_eq!(names(&rank("tt", with_dm))[0], "Tetrix Tests", "initials beat a run inside a word");
        assert_eq!(names(&rank("1", all()))[..2], ["status", "Tetrix Tests"], "numbers find windows");
    }
}

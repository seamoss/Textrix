//! The single-line editor at the bottom of the screen (emacs-style keys,
//! history, irssi-style tab completion).

use matrix_sdk::ruma::OwnedUserId;
use unicode_width::UnicodeWidthChar;

/// A tab-completion candidate. `user` is set for nicks so the message can
/// carry a proper mention.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub text: String,
    pub user: Option<OwnedUserId>,
    /// Shown while cycling (e.g. an emoji's shortcode).
    pub hint: Option<String>,
}

struct Completion {
    start: usize,
    end: usize,
    candidates: Vec<Candidate>,
    idx: usize,
}

#[derive(Default)]
pub struct Input {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: Vec<char>,
    kill: Vec<char>,
    completion: Option<Completion>,
    /// Nicks inserted by completion, used to build `m.mentions`.
    pub mentions: Vec<(String, OwnedUserId)>,
}

const HISTORY_MAX: usize = 500;

impl Input {
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn chars(&self) -> &[char] {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Display width of the text before the cursor.
    pub fn cursor_width(&self) -> usize {
        self.buf[..self.cursor].iter().map(|c| display_width(*c)).sum()
    }

    /// Take the line for submission, recording it in history.
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > HISTORY_MAX {
                self.history.remove(0);
            }
        }
        self.clear();
        text
    }

    /// Replace the line (e.g. with a message to edit), cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.clear();
        self.insert_str(text);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.completion = None;
        self.mentions.clear();
    }

    /// Any key other than Tab ends a completion cycle.
    pub fn end_completion(&mut self) {
        self.completion = None;
    }

    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| *c != '\r') {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buf.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start(self.cursor);
    }

    pub fn word_right(&mut self) {
        let mut i = self.cursor;
        while i < self.buf.len() && self.buf[i].is_whitespace() {
            i += 1;
        }
        while i < self.buf.len() && !self.buf[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    pub fn kill_to_start(&mut self) {
        self.kill = self.buf.drain(..self.cursor).collect();
        self.cursor = 0;
    }

    pub fn kill_to_end(&mut self) {
        self.kill = self.buf.drain(self.cursor..).collect();
    }

    pub fn kill_word(&mut self) {
        let start = self.word_start(self.cursor);
        self.kill = self.buf.drain(start..self.cursor).collect();
        self.cursor = start;
    }

    pub fn yank(&mut self) {
        let kill = self.kill.clone();
        for c in kill {
            self.insert(c);
        }
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        self.buf = self.history[pos].chars().collect();
        self.cursor = self.buf.len();
    }

    pub fn history_next(&mut self) {
        let Some(pos) = self.history_pos else { return };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.buf = self.history[pos + 1].chars().collect();
        } else {
            self.history_pos = None;
            self.buf = std::mem::take(&mut self.draft);
        }
        self.cursor = self.buf.len();
    }

    /// The word being completed and whether it starts the line.
    pub fn completion_word(&self) -> (String, bool) {
        let start = self.word_start(self.cursor);
        (self.buf[start..self.cursor].iter().collect(), start == 0)
    }

    /// Put the cursor at a clicked column of the visible text, `skip`
    /// characters being scrolled out of view on the left.
    pub fn click(&mut self, skip: usize, col: usize) {
        self.completion = None;
        let mut pos = skip.min(self.buf.len());
        let mut used = 0;
        while pos < self.buf.len() && used + display_width(self.buf[pos]) <= col {
            used += display_width(self.buf[pos]);
            pos += 1;
        }
        self.cursor = pos;
    }

    /// While cycling through candidates that carry a hint: "hint  2/5".
    pub fn completion_hint(&self) -> Option<String> {
        let c = self.completion.as_ref()?;
        let hint = c.candidates[c.idx].hint.as_ref()?;
        Some(format!("{hint}  {}/{} · Tab next", c.idx + 1, c.candidates.len()))
    }

    /// Tab: start a completion with `candidates`, or cycle the current one.
    pub fn complete(&mut self, candidates: impl FnOnce() -> Vec<Candidate>) {
        if let Some(c) = &mut self.completion {
            c.idx = (c.idx + 1) % c.candidates.len();
        } else {
            let candidates = candidates();
            if candidates.is_empty() {
                return;
            }
            let start = self.word_start(self.cursor);
            self.completion = Some(Completion { start, end: self.cursor, candidates, idx: 0 });
        }
        let c = self.completion.as_mut().expect("completion just set");
        let pick = c.candidates[c.idx].clone();
        let replacement: Vec<char> = pick.text.chars().collect();
        let len = replacement.len();
        self.buf.splice(c.start..c.end, replacement);
        c.end = c.start + len;
        self.cursor = c.end;
        // A single match has nothing to cycle through: the next Tab starts
        // over (e.g. descending into a completed directory).
        if c.candidates.len() == 1 {
            self.completion = None;
        }
        if let Some(user) = pick.user {
            let nick = pick.text.trim_end().trim_end_matches(|ch: char| ch.is_ascii_punctuation()).to_owned();
            if !self.mentions.iter().any(|(_, u)| *u == user) {
                self.mentions.push((nick, user));
            }
        }
    }

    fn word_start(&self, from: usize) -> usize {
        let mut i = from;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }
}

/// Terminal width of a character as the editor draws it.
pub fn display_width(c: char) -> usize {
    match c {
        '\n' => 1, // drawn as ↵
        c => c.width().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Input {
        let mut input = Input::default();
        input.insert_str(s);
        input
    }

    #[test]
    fn clicks_place_the_cursor_by_column() {
        let mut input = typed("a日本b");
        input.click(0, 0);
        assert_eq!(input.cursor(), 0);
        input.click(0, 2);
        assert_eq!(input.cursor(), 1, "inside a wide character lands before it");
        input.click(0, 3);
        assert_eq!(input.cursor(), 2);
        input.click(1, 0);
        assert_eq!(input.cursor(), 1, "columns count from the first visible character");
        input.click(0, 99);
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn kill_word_and_yank() {
        let mut input = typed("hello brave world");
        input.kill_word();
        assert_eq!(input.text(), "hello brave ");
        input.home();
        input.yank();
        assert_eq!(input.text(), "worldhello brave ");
    }

    #[test]
    fn history_round_trip_restores_draft() {
        let mut input = typed("first");
        input.take();
        input.insert_str("draft");
        input.history_prev();
        assert_eq!(input.text(), "first");
        input.history_next();
        assert_eq!(input.text(), "draft");
    }

    #[test]
    fn completion_cycles_and_records_mentions() {
        let user: OwnedUserId = "@alice:x.org".try_into().unwrap();
        let cands = vec![
            Candidate { text: "alice: ".into(), user: Some(user.clone()), hint: None },
            Candidate { text: "alfred: ".into(), user: None, hint: Some(":alfred:".into()) },
        ];
        let mut input = typed("al");
        input.complete(|| cands.clone());
        assert_eq!(input.text(), "alice: ");
        input.complete(|| unreachable!());
        assert_eq!(input.text(), "alfred: ");
        assert_eq!(input.mentions, vec![("alice".to_owned(), user)]);
    }
}

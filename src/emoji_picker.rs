//! The emoji picker (`+` in selection mode): your recent reactions, your
//! custom emoji, then every emoji by group, searched as you type, to react
//! with.

use matrix_sdk::ruma::{OwnedEventId, OwnedMxcUri};
use unicode_width::UnicodeWidthStr;

/// Recent reactions kept.
pub const RECENT: usize = 24;

/// What reacting sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Emoji(&'static str),
    /// A custom emoji: its shortcode and picture.
    Custom(String, OwnedMxcUri),
}

impl Pick {
    /// As drawn, and as kept in the recent list: the emoji, or `:code:`.
    pub fn label(&self) -> String {
        match self {
            Pick::Emoji(e) => (*e).to_owned(),
            Pick::Custom(code, _) => format!(":{code}:"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub section: &'static str,
    pub pick: Pick,
    /// What it's called, for the line at the bottom.
    pub about: String,
}

pub struct EmojiPicker {
    /// The message to react to (in the window it opened in), and who sent it.
    pub event: OwnedEventId,
    pub nick: String,
    pub query: String,
    pub selected: usize,
    /// The first row on screen, and the columns rows are laid out in, as
    /// last drawn.
    pub top: usize,
    pub width: u16,
    custom: Vec<(String, OwnedMxcUri)>,
    recent: Vec<String>,
}

/// A row of the grid: a section's name, or entries with their columns
/// (from, width).
#[derive(Debug, PartialEq, Eq)]
pub enum Row {
    Heading(&'static str),
    Items(Vec<(u16, u16, usize)>),
}

impl EmojiPicker {
    pub fn new(event: OwnedEventId, nick: String, custom: Vec<(String, OwnedMxcUri)>, recent: Vec<String>) -> Self {
        Self { event, nick, query: String::new(), selected: 0, top: 0, width: 40, custom, recent }
    }

    /// What there is to pick: with nothing typed, recent ones, yours and
    /// every group; else what matches, best first.
    pub fn entries(&self) -> Vec<Entry> {
        let query = self.query.trim().trim_matches(':').to_lowercase();
        if !query.is_empty() {
            return self.matches(&query);
        }
        let mut out: Vec<Entry> = self
            .recent
            .iter()
            .filter_map(|label| self.pick_for(label))
            .map(|pick| Entry { section: "Recent", about: about(&pick), pick })
            .collect();
        out.extend(self.custom.iter().map(|(code, mxc)| {
            let pick = Pick::Custom(code.clone(), mxc.clone());
            Entry { section: "Yours", about: about(&pick), pick }
        }));
        for group in emojis::Group::iter() {
            let section = group_name(group);
            out.extend(group.emojis().filter(|e| shown(e)).map(|e| Entry {
                section,
                pick: Pick::Emoji(e.as_str()),
                about: about_emoji(e),
            }));
        }
        out
    }

    fn matches(&self, query: &str) -> Vec<Entry> {
        // 0 an exact shortcode, 1 the start of one, 2 in one, 3 in the name.
        let rank = |codes: &mut dyn Iterator<Item = &str>, name: &str| {
            codes
                .map(|c| {
                    if c == query {
                        0
                    } else if c.starts_with(query) {
                        1
                    } else if c.contains(query) {
                        2
                    } else {
                        4
                    }
                })
                .min()
                .unwrap_or(4)
                .min(if name.to_lowercase().contains(query) { 3 } else { 4 })
        };
        let mut found: Vec<(u8, Entry)> = self
            .custom
            .iter()
            .map(|(code, mxc)| {
                let pick = Pick::Custom(code.clone(), mxc.clone());
                (
                    rank(&mut std::iter::once(code.to_lowercase().as_str()), ""),
                    Entry { section: "Found", about: about(&pick), pick },
                )
            })
            .collect();
        found.extend(emojis::iter().filter(|e| shown(e)).map(|e| {
            let entry = Entry { section: "Found", pick: Pick::Emoji(e.as_str()), about: about_emoji(e) };
            (rank(&mut e.shortcodes(), e.name()), entry)
        }));
        found.retain(|(rank, _)| *rank < 4);
        // Stable: within a rank, emoji keep Unicode's order.
        found.sort_by_key(|(rank, _)| *rank);
        found.into_iter().map(|(_, e)| e).collect()
    }

    /// A recent reaction, if it's still to be had (custom emoji can go).
    fn pick_for(&self, label: &str) -> Option<Pick> {
        match label.strip_prefix(':').and_then(|l| l.strip_suffix(':')) {
            Some(code) => self.custom.iter().find(|(c, _)| c == code).map(|(c, m)| Pick::Custom(c.clone(), m.clone())),
            None => emojis::get(label).map(|e| Pick::Emoji(e.as_str())),
        }
    }

    pub fn step(&mut self, entries: &[Entry], rows: &[Row], way: Way) {
        let last = entries.len().saturating_sub(1);
        let at = self.selected.min(last);
        let row_of =
            |n: usize| rows.iter().position(|r| matches!(r, Row::Items(items) if items.iter().any(|i| i.2 == n)));
        // Up and down keep to the column, as near as the row allows.
        let vertical = |by: isize| -> usize {
            let Some(here) = row_of(at) else { return at };
            let Row::Items(items) = &rows[here] else { return at };
            let x = items.iter().find(|i| i.2 == at).map_or(0, |i| i.0);
            let item_rows: Vec<usize> = (0..rows.len()).filter(|&r| matches!(rows[r], Row::Items(_))).collect();
            let pos = item_rows.iter().position(|&r| r == here).unwrap_or(0) as isize;
            let to = (pos + by).clamp(0, item_rows.len() as isize - 1) as usize;
            let Row::Items(items) = &rows[item_rows[to]] else { return at };
            items.iter().min_by_key(|i| i.0.abs_diff(x)).map_or(at, |i| i.2)
        };
        self.selected = match way {
            Way::Left => at.saturating_sub(1),
            Way::Right => (at + 1).min(last),
            Way::Up => vertical(-1),
            Way::Down => vertical(1),
            Way::PageUp => vertical(-5),
            Way::PageDown => vertical(5),
            Way::First => 0,
            Way::Last => last,
        };
    }

    /// Scroll so the chosen one's row is on screen.
    pub fn follow(&mut self, rows: &[Row], height: usize) {
        let height = height.max(1);
        let Some(row) = rows.iter().position(|r| matches!(r, Row::Items(i) if i.iter().any(|i| i.2 == self.selected)))
        else {
            return;
        };
        // A section's first row brings its heading along.
        let row = if row > 0 && matches!(rows[row - 1], Row::Heading(_)) && row - 1 < self.top { row - 1 } else { row };
        if row < self.top {
            self.top = row;
        } else if row >= self.top + height {
            self.top = row + 1 - height;
        }
    }
}

/// Which way to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

/// The entries in rows `width` wide: emoji two columns and a space, custom
/// ones as their `:code:`, each section under its name.
pub fn layout(entries: &[Entry], width: u16) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut items: Vec<(u16, u16, usize)> = Vec::new();
    let mut x = 0u16;
    for (n, entry) in entries.iter().enumerate() {
        if n == 0 || entries[n - 1].section != entry.section {
            if !items.is_empty() {
                rows.push(Row::Items(std::mem::take(&mut items)));
            }
            rows.push(Row::Heading(entry.section));
            x = 0;
        }
        let w = (entry.pick.label().width() as u16).clamp(2, width.max(2));
        if x > 0 && x + w > width {
            rows.push(Row::Items(std::mem::take(&mut items)));
            x = 0;
        }
        items.push((x, w, n));
        x += w + 1;
    }
    if !items.is_empty() {
        rows.push(Row::Items(items));
    }
    rows
}

/// Emoji most terminals and fonts can draw in two columns: not sequences
/// joined with ZWJ (families, professions), which many draw as several,
/// and not ones newer than Unicode 14.
fn shown(e: &emojis::Emoji) -> bool {
    !e.as_str().contains('\u{200D}') && e.unicode_version() <= emojis::UnicodeVersion::new(14, 0)
}

fn about_emoji(e: &emojis::Emoji) -> String {
    match e.shortcode() {
        Some(code) => format!("{} :{code}:", e.name()),
        None => e.name().to_owned(),
    }
}

fn about(pick: &Pick) -> String {
    match pick {
        Pick::Emoji(e) => emojis::get(e).map_or_else(String::new, about_emoji),
        Pick::Custom(code, _) => format!(":{code}: (a custom emoji)"),
    }
}

fn group_name(group: emojis::Group) -> &'static str {
    use emojis::Group::*;
    match group {
        SmileysAndEmotion => "Smileys",
        PeopleAndBody => "People",
        AnimalsAndNature => "Animals & nature",
        FoodAndDrink => "Food & drink",
        TravelAndPlaces => "Travel & places",
        Activities => "Activities",
        Objects => "Objects",
        Symbols => "Symbols",
        Flags => "Flags",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(recent: &[&str]) -> EmojiPicker {
        let custom = vec![("blobcat".to_owned(), OwnedMxcUri::from("mxc://example.org/blobcat"))];
        let recent = recent.iter().map(|s| (*s).to_owned()).collect();
        EmojiPicker::new("$e".try_into().unwrap(), "sam".into(), custom, recent)
    }

    #[test]
    fn recent_then_yours_then_every_group() {
        let p = picker(&["🔥", ":blobcat:", ":gone:"]);
        let entries = p.entries();
        let labels: Vec<String> = entries.iter().take(4).map(|e| e.pick.label()).collect();
        assert_eq!(labels, ["🔥", ":blobcat:", ":blobcat:", "😀"], "a custom emoji that's gone drops out");
        assert_eq!(entries[0].section, "Recent");
        assert_eq!(entries[2].section, "Yours");
        assert!(entries.iter().any(|e| e.section == "Flags"));
        assert!(!entries.iter().any(|e| e.pick.label().contains('\u{200D}')), "no ZWJ sequences");
    }

    #[test]
    fn typing_finds_by_shortcode_and_name() {
        let mut p = picker(&[]);
        p.query = ":fire".into();
        let found = p.entries();
        assert_eq!(found[0].pick, Pick::Emoji("🔥"), "the exact shortcode first");
        assert_eq!(found[0].about, "fire :fire:");
        p.query = "blob".into();
        assert_eq!(p.entries()[0].pick.label(), ":blobcat:");
        p.query = "thumbs up".into();
        assert!(p.entries().iter().any(|e| e.pick == Pick::Emoji("👍")), "by name too");
        p.query = "zzzqqq".into();
        assert!(p.entries().is_empty());
    }

    #[test]
    fn rows_flow_and_arrows_keep_the_column() {
        let mut p = picker(&["🔥", "👍", "🎉", "😂", "❤\u{fe0f}"]);
        let entries = p.entries();
        let rows = layout(&entries, 9);
        assert_eq!(rows[0], Row::Heading("Recent"));
        assert_eq!(rows[1], Row::Items(vec![(0, 2, 0), (3, 2, 1), (6, 2, 2)]));
        assert_eq!(rows[2], Row::Items(vec![(0, 2, 3), (3, 2, 4)]));
        assert_eq!(rows[3], Row::Heading("Yours"));
        p.selected = 2;
        p.step(&entries, &rows, Way::Down);
        assert_eq!(p.selected, 4, "the nearest in the row below");
        p.step(&entries, &rows, Way::Down);
        assert_eq!(p.selected, 5, "over the heading, into Yours");
        p.step(&entries, &rows, Way::Up);
        assert_eq!(p.selected, 3, "back up, at the start of the row");
        p.step(&entries, &rows, Way::Left);
        assert_eq!(p.selected, 2, "left goes back along the list");
    }
}

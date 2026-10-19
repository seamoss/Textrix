//! `/search` queries: words to find, narrowed by who said them (`from:`),
//! where (`in:`) and when (`after:`, `before:`), e.g.
//! `from:sam in:"Book Club" after:2026-01 before:2026-06 pizza`.

use chrono::{Datelike, Local, NaiveDate, TimeZone};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// What the message says (any case); empty: anything.
    pub text: String,
    /// A name as shown, a nickname, or a Matrix ID or its user part.
    pub from: Option<String>,
    /// Part of the room's name.
    pub room: Option<String>,
    /// On or after this (milliseconds since 1970).
    pub after: Option<i64>,
    /// Before this.
    pub before: Option<i64>,
}

pub const USAGE: &str =
    "usage: /search [-deep] [from:name] [in:room] [after:2026-01] [before:2026-06-30] [text] (quote names with spaces)";

impl Query {
    pub fn parse(input: &str) -> Result<Query, String> {
        let mut query = Query::default();
        let mut words = Vec::new();
        for word in split_quoted(input) {
            let (key, value) = match word.split_once(':') {
                Some((key, value)) if !value.is_empty() => (key.to_lowercase(), unquote(value)),
                _ => (String::new(), String::new()),
            };
            match key.as_str() {
                "from" => query.from = Some(value.trim_start_matches('@').to_lowercase()),
                "in" => query.room = Some(value.to_lowercase()),
                "after" | "since" => query.after = Some(day(&value)?),
                "before" | "until" => query.before = Some(day(&value)?),
                _ => words.push(unquote(&word)),
            }
        }
        query.text = words.join(" ").to_lowercase();
        Ok(query)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.from.is_none()
            && self.room.is_none()
            && self.after.is_none()
            && self.before.is_none()
    }

    /// Whether a message matches: its room's name, the names its sender
    /// goes by (as shown, your nickname), their Matrix ID, when, and what
    /// it says.
    pub fn matches(&self, room: &str, names: &[&str], sender: &str, ts: i64, text: &str) -> bool {
        let from = self.from.as_deref().is_none_or(|from| {
            let sender = sender.to_lowercase();
            let local = sender.trim_start_matches('@').split(':').next().unwrap_or_default();
            names.iter().any(|n| n.to_lowercase() == from) || local == from || sender.trim_start_matches('@') == from
        });
        from && self.room.as_deref().is_none_or(|r| room.to_lowercase().contains(r))
            && self.after.is_none_or(|after| ts >= after)
            && self.before.is_none_or(|before| ts < before)
            && (self.text.is_empty() || text.to_lowercase().contains(&self.text))
    }

    /// The query as typed, tidied ("pizza from:sam in:book club").
    pub fn describe(&self) -> String {
        let date = |ms: i64| Local.timestamp_millis_opt(ms).single().map(|t| t.format("%Y-%m-%d").to_string());
        let mut parts: Vec<String> = Vec::new();
        if !self.text.is_empty() {
            parts.push(format!("\"{}\"", self.text));
        }
        parts.extend(self.from.as_ref().map(|f| format!("from:{f}")));
        parts.extend(self.room.as_ref().map(|r| format!("in:{r}")));
        parts.extend(self.after.and_then(date).map(|d| format!("after:{d}")));
        parts.extend(self.before.and_then(date).map(|d| format!("before:{d}")));
        parts.join(" ")
    }
}

/// The start of a day, month or year (`2026`, `2026-06`, `2026-06-30`,
/// `today`, `yesterday`), in local time, as milliseconds since 1970.
fn day(value: &str) -> Result<i64, String> {
    let today = Local::now().date_naive();
    let date = match value.to_lowercase().as_str() {
        "today" => Some(today),
        "yesterday" => today.pred_opt(),
        v => {
            let parts: Vec<&str> = v.split(['-', '/']).collect();
            let num = |i: usize| parts.get(i).map(|p| p.parse::<u32>());
            match (num(0), num(1), num(2), parts.len()) {
                (Some(Ok(y)), None, None, 1) => NaiveDate::from_ymd_opt(y as i32, 1, 1),
                (Some(Ok(y)), Some(Ok(m)), None, 2) => NaiveDate::from_ymd_opt(y as i32, m, 1),
                (Some(Ok(y)), Some(Ok(m)), Some(Ok(d)), 3) => NaiveDate::from_ymd_opt(y as i32, m, d),
                _ => None,
            }
        }
    }
    .filter(|d| d.year() > 1970)
    .ok_or_else(|| format!("“{value}” isn't a date: use 2026, 2026-06, 2026-06-30, today or yesterday"))?;
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight exists");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp_millis())
        .ok_or_else(|| format!("“{value}” isn't a date"))
}

/// Split at spaces outside double quotes, keeping the quotes.
fn split_quoted(input: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for c in input.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                word.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn unquote(s: &str) -> String {
    s.replace('"', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i32, m: u32, d: u32) -> i64 {
        Local.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap().timestamp_millis()
    }

    #[test]
    fn queries_parse() {
        let q = Query::parse(r#"from:@Sam in:"Book Club" after:2026-01 before:2026-06-30 pizza "night out""#).unwrap();
        assert_eq!(q.text, "pizza night out");
        assert_eq!((q.from.as_deref(), q.room.as_deref()), (Some("sam"), Some("book club")));
        assert_eq!((q.after, q.before), (Some(ms(2026, 1, 1)), Some(ms(2026, 6, 30))));
        assert_eq!(q.describe(), "\"pizza night out\" from:sam in:book club after:2026-01-01 before:2026-06-30");
        assert_eq!(Query::parse("before:2025").unwrap().before, Some(ms(2025, 1, 1)));
        assert!(Query::parse("after:June").unwrap_err().contains("isn't a date"));
        assert!(Query::parse("after:2026-13").is_err());
        assert!(Query::parse("   ").unwrap().is_empty());
        assert_eq!(
            Query::parse("see https://x.org/a:b").unwrap().text,
            "see https://x.org/a:b",
            "colons in words stay"
        );
    }

    #[test]
    fn messages_match_on_every_part() {
        let q = Query::parse("from:sam in:book after:2026-01 before:2026-06 pizza").unwrap();
        let march = ms(2026, 3, 1);
        assert!(q.matches("Book Club", &["Sammy", "sam"], "@sam:x.org", march, "Pizza tonight?"));
        assert!(q.matches("Book Club", &["Sammy"], "@sam:x.org", march, "pizza"), "by the user part of the ID");
        assert!(!q.matches("Night Owls", &["sam"], "@sam:x.org", march, "pizza"), "another room");
        assert!(!q.matches("Book Club", &["jo_bloggs"], "@jo:x.org", march, "pizza"), "someone else");
        assert!(!q.matches("Book Club", &["sam"], "@sam:x.org", ms(2026, 6, 1), "pizza"), "too late");
        assert!(!q.matches("Book Club", &["sam"], "@sam:x.org", ms(2025, 12, 31), "pizza"), "too early");
        assert!(!q.matches("Book Club", &["sam"], "@sam:x.org", march, "salad"), "something else");
        let anything_from = Query::parse("from:sam:x.org").unwrap();
        assert!(anything_from.matches("Night Owls", &[], "@sam:x.org", march, "whatever"));
    }
}

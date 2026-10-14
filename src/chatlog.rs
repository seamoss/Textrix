//! irssi-style chat logs: `<dir>/<server>/<room>/<YYYY-MM-DD>.log`.
//!
//! Only what arrives while connected is written (history loaded by
//! scrolling is not), so files don't fill with repeats. Logs hold decrypted
//! messages in plaintext; files are created readable by you only.

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
};

use matrix_sdk::ruma::{OwnedEventId, RoomId};

use crate::{
    config::create_private_dir,
    model::{Item, Line, LineKind},
};

const REMEMBERED_MAX: usize = 20_000;

pub struct ChatLog {
    dir: PathBuf,
    /// Events already written (sync can deliver one twice).
    written: HashSet<OwnedEventId>,
}

impl ChatLog {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, written: HashSet::new() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write(&mut self, server: &str, room_name: &str, room_id: &RoomId, item: &Item) -> std::io::Result<()> {
        let Some((date, text)) = self.format(item) else { return Ok(()) };
        let folder = self.dir.join(safe_name(server)).join(room_folder(room_name, room_id));
        create_private_dir(&folder).map_err(std::io::Error::other)?;
        let path = folder.join(format!("{date}.log"));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?.write_all(text.as_bytes())
    }

    /// (date of the file, the lines to append).
    fn format(&mut self, item: &Item) -> Option<(String, String)> {
        let (line, prefix) = match item {
            Item::Line(line) => (line, ""),
            Item::Edit { fallback, .. } => (fallback, "[edit] "),
            Item::Redact { .. } => return Some((today(), format!("{} -!- a message was deleted\n", now_time()))),
            Item::React { .. } => return None,
        };
        let id = line.event_id.clone()?;
        if line.pending || (matches!(item, Item::Line(_)) && !self.written.insert(id)) {
            return None;
        }
        if self.written.len() > REMEMBERED_MAX {
            self.written.clear();
        }
        let thread = if line.thread { "[thread] " } else { "" };
        let text = line.text();
        let body = format!("{prefix}{thread}{text}");
        let who = match line.kind {
            LineKind::Message => format!("<{}> ", line.nick),
            LineKind::Emote => format!(" * {} ", line.nick),
            LineKind::Notice => format!("-{}- ", line.nick),
            LineKind::Utd => return Some(entry(line, &format!("<{}> [unable to decrypt]", line.nick))),
            LineKind::Membership | LineKind::Event | LineKind::Info | LineKind::Error => "-!- ".to_owned(),
        };
        Some(entry(line, &format!("{who}{body}")))
    }
}

fn entry(line: &Line, text: &str) -> (String, String) {
    let time = line.ts.format("%H:%M:%S").to_string();
    // Continuation lines of multi-line messages line up under the text.
    let indent = " ".repeat(time.len() + 1);
    let mut out = String::new();
    for (i, part) in text.split('\n').enumerate() {
        if i == 0 {
            out.push_str(&format!("{time} {part}\n"));
        } else {
            out.push_str(&format!("{indent}{part}\n"));
        }
    }
    (line.ts.format("%Y-%m-%d").to_string(), out)
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn now_time() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// `general-chat.KYsdfd`: readable, and unique per room.
fn room_folder(name: &str, room_id: &RoomId) -> String {
    let id: String = room_id.as_str().trim_start_matches('!').chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect();
    let slug = safe_name(name);
    if slug.is_empty() { id } else { format!("{slug}.{id}") }
}

/// Lowercase letters, digits, `-` and `_` only.
fn safe_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, kind: LineKind, body: &str) -> Line {
        let mut l = Line::new(kind, body);
        l.event_id = Some(id.try_into().unwrap());
        l.nick = "alice".into();
        l
    }

    #[test]
    fn writes_irssi_style_daily_files_once() {
        let dir = std::env::temp_dir().join(format!("textrix-log-{}", std::process::id()));
        let mut log = ChatLog::new(dir.clone());
        let room: &RoomId = "!KYsdfdYxkn:x.org".try_into().unwrap();
        let msg = Item::Line(line("$1", LineKind::Message, "hello\nsecond line"));
        log.write("0x4f02", "Tetrix Tests!", room, &msg).unwrap();
        log.write("0x4f02", "Tetrix Tests!", room, &msg).unwrap();
        log.write("0x4f02", "Tetrix Tests!", room, &Item::Line(line("$2", LineKind::Emote, "waves"))).unwrap();

        let file = dir.join("0x4f02").join("tetrix-tests.KYsdfd").join(format!("{}.log", today()));
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "the repeat isn't written: {text}");
        assert!(lines[0].ends_with(" <alice> hello"));
        assert!(lines[1].starts_with("         second line"));
        assert!(lines[2].ends_with("  * alice waves"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folder_names_are_safe() {
        let room: &RoomId = "!abc123XYZ:x.org".try_into().unwrap();
        assert_eq!(room_folder("../../etc", room), "etc.abc123");
        assert_eq!(room_folder("", room), "abc123");
        assert_eq!(safe_name("Disaster Control"), "disaster-control");
    }
}

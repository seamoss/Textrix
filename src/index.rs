//! A local search index of decrypted messages, so `/search` finds old
//! messages in encrypted rooms (which servers can't search) instantly.
//!
//! It's encrypted at rest with matrix-sdk's store cipher, keyed from the
//! same keychain secret as the account's store, so it's as safe as the
//! store itself. On disk it's an append-only log, `search-index/log`: one
//! encrypted record per line (messages added, messages removed, how far
//! back a room has been read). Opening reads it all into memory, and when
//! much of it is stale it's rewritten.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use matrix_sdk_store_encryption::{EncryptedValueBase64, StoreCipher};
use serde::{Deserialize, Serialize};

/// Rooms stop being read back once they have this many messages indexed.
pub const ROOM_CAP: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "r")]
    pub room: String,
    #[serde(rename = "e")]
    pub event: String,
    /// Milliseconds since 1970.
    #[serde(rename = "t")]
    pub ts: i64,
    #[serde(rename = "s")]
    pub sender: String,
    #[serde(rename = "n")]
    pub nick: String,
    #[serde(rename = "b")]
    pub body: String,
}

/// How far back a room's history has been read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crawl {
    /// Where to carry on from (`None` before the first page).
    pub from: Option<String>,
    /// The start of the room was reached (or its cap).
    pub done: bool,
}

#[derive(Serialize, Deserialize)]
enum Record {
    Add(Vec<Entry>),
    Remove(Vec<String>),
    Crawl(String, Crawl),
}

pub struct SearchIndex {
    cipher: StoreCipher,
    dir: PathBuf,
    log: File,
    entries: Vec<Option<Entry>>,
    by_event: HashMap<String, usize>,
    per_room: HashMap<String, usize>,
    crawls: HashMap<String, Crawl>,
    /// Records in the log that no longer count (replaced or removed).
    stale: usize,
}

impl SearchIndex {
    /// Open (or start) the index in `dir`, encrypted with a cipher that
    /// `key` unlocks. An index `key` can't unlock (after a new login) is
    /// started afresh.
    pub fn open(dir: &Path, key: &[u8]) -> Result<Self> {
        crate::config::create_private_dir(dir)?;
        let key_file = dir.join("key");
        let cipher =
            match std::fs::read(&key_file).ok().and_then(|bytes| StoreCipher::import_with_key(key, &bytes).ok()) {
                Some(cipher) => cipher,
                None => {
                    let _ = std::fs::remove_file(dir.join("log"));
                    let cipher = StoreCipher::new().context("creating the search index's key")?;
                    crate::config::write_private(&key_file, &cipher.export_with_key(key)?)?;
                    cipher
                }
            };
        let path = dir.join("log");
        let mut index = Self {
            cipher,
            dir: dir.to_owned(),
            log: open_log(&path)?,
            entries: Vec::new(),
            by_event: HashMap::new(),
            per_room: HashMap::new(),
            crawls: HashMap::new(),
            stale: 0,
        };
        for line in BufReader::new(File::open(&path)?).lines() {
            match index.decrypt(&line?) {
                Some(record) => index.apply(record),
                // Half a line from a crash, say: dropped at the next rewrite.
                None => index.stale += 1,
            }
        }
        if index.stale > index.by_event.len() / 2 + 1000 {
            index.rewrite()?;
        }
        Ok(index)
    }

    fn decrypt(&self, line: &str) -> Option<Record> {
        let value: EncryptedValueBase64 = serde_json::from_str(line).ok()?;
        let json = self.cipher.decrypt_value_base64_data(value).ok()?;
        serde_json::from_slice(&json).ok()
    }

    fn encrypt(&self, record: &Record) -> Result<String> {
        let value = self.cipher.encrypt_value_base64_data(serde_json::to_vec(record)?)?;
        Ok(serde_json::to_string(&value)? + "\n")
    }

    /// Apply a record to the in-memory index.
    fn apply(&mut self, record: Record) {
        match record {
            Record::Add(entries) => {
                for entry in entries {
                    match self.by_event.get(&entry.event) {
                        Some(&i) => {
                            self.entries[i] = Some(entry);
                            self.stale += 1;
                        }
                        None => {
                            *self.per_room.entry(entry.room.clone()).or_default() += 1;
                            self.by_event.insert(entry.event.clone(), self.entries.len());
                            self.entries.push(Some(entry));
                        }
                    }
                }
            }
            Record::Remove(events) => {
                for event in events {
                    if let Some(i) = self.by_event.remove(&event)
                        && let Some(entry) = self.entries[i].take()
                        && let Some(n) = self.per_room.get_mut(&entry.room)
                    {
                        *n -= 1;
                    }
                    self.stale += 1;
                }
            }
            Record::Crawl(room, crawl) => {
                if self.crawls.insert(room, crawl).is_some() {
                    self.stale += 1;
                }
            }
        }
    }

    fn append(&mut self, record: Record) -> Result<()> {
        let line = self.encrypt(&record)?;
        self.log.write_all(line.as_bytes()).context("writing the search index")?;
        self.apply(record);
        Ok(())
    }

    /// Add messages, or update ones already there (after an edit). Ones
    /// already indexed as they are aren't written again.
    pub fn add(&mut self, entries: Vec<Entry>) -> Result<()> {
        let new: Vec<Entry> = entries
            .into_iter()
            .filter(|e| self.by_event.get(&e.event).is_none_or(|&i| self.entries[i].as_ref() != Some(e)))
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        self.append(Record::Add(new))
    }

    /// A message's new text, if it's indexed.
    pub fn edit(&mut self, event: &str, body: String) -> Result<()> {
        let Some(entry) = self.by_event.get(event).and_then(|&i| self.entries[i].clone()) else { return Ok(()) };
        self.add(vec![Entry { body, ..entry }])
    }

    /// Forget deleted messages.
    pub fn remove(&mut self, events: Vec<String>) -> Result<()> {
        let events: Vec<String> = events.into_iter().filter(|e| self.by_event.contains_key(e)).collect();
        if events.is_empty() {
            return Ok(());
        }
        self.append(Record::Remove(events))
    }

    pub fn crawl(&self, room: &str) -> Crawl {
        self.crawls.get(room).cloned().unwrap_or_default()
    }

    pub fn set_crawl(&mut self, room: &str, crawl: Crawl) -> Result<()> {
        self.append(Record::Crawl(room.to_owned(), crawl))
    }

    pub fn room_count(&self, room: &str) -> usize {
        self.per_room.get(room).copied().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.by_event.len()
    }

    /// Messages containing `needle` (any case), newest first.
    pub fn search(&self, needle: &str, limit: usize) -> Vec<Entry> {
        let needle = needle.to_lowercase();
        let mut hits: Vec<&Entry> =
            self.entries.iter().flatten().filter(|e| e.body.to_lowercase().contains(&needle)).collect();
        hits.sort_by_key(|e| std::cmp::Reverse(e.ts));
        hits.into_iter().take(limit).cloned().collect()
    }

    /// Write the log afresh with only what still counts.
    fn rewrite(&mut self) -> Result<()> {
        let path = self.dir.join("log");
        let tmp = self.dir.join("log.tmp");
        let mut out = String::new();
        let live: Vec<Entry> = self.entries.iter().flatten().cloned().collect();
        for chunk in live.chunks(500) {
            out.push_str(&self.encrypt(&Record::Add(chunk.to_vec()))?);
        }
        for (room, crawl) in &self.crawls {
            out.push_str(&self.encrypt(&Record::Crawl(room.clone(), crawl.clone()))?);
        }
        crate::config::write_private(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, &path).context("rewriting the search index")?;
        self.log = open_log(&path)?;
        self.entries = live.into_iter().map(Some).collect();
        self.by_event = self.entries.iter().enumerate().map(|(i, e)| (e.as_ref().unwrap().event.clone(), i)).collect();
        self.stale = 0;
        Ok(())
    }
}

fn open_log(path: &Path) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(event: &str, ts: i64, body: &str) -> Entry {
        Entry {
            room: "!a:x.org".into(),
            event: event.into(),
            ts,
            sender: "@sam:x.org".into(),
            nick: "sam".into(),
            body: body.into(),
        }
    }

    #[test]
    fn the_index_finds_keeps_and_forgets_messages() {
        let dir = std::env::temp_dir().join(format!("textrix-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = b"the keychain secret";
        let mut index = SearchIndex::open(&dir, key).unwrap();
        index
            .add(vec![entry("$1", 1, "Pizza tonight?"), entry("$2", 2, "no, salad"), entry("$3", 3, "PIZZA!")])
            .unwrap();
        index.edit("$2", "fine, pizza".into()).unwrap();
        index.remove(vec!["$3".into()]).unwrap();
        index.set_crawl("!a:x.org", Crawl { from: Some("t42".into()), done: false }).unwrap();
        let found =
            |index: &SearchIndex| -> Vec<String> { index.search("pizza", 10).into_iter().map(|e| e.body).collect() };
        assert_eq!(found(&index), ["fine, pizza", "Pizza tonight?"], "newest first, any case, edits and deletions");

        // Reopened, as after a restart: the same, read back from the log.
        drop(index);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(!log.to_lowercase().contains("pizza") && !log.contains("@sam"), "nothing readable on disk");
        let mut index = SearchIndex::open(&dir, key).unwrap();
        assert_eq!(found(&index), ["fine, pizza", "Pizza tonight?"]);
        assert_eq!(index.crawl("!a:x.org").from.as_deref(), Some("t42"));
        assert_eq!((index.len(), index.room_count("!a:x.org")), (2, 2));

        let lines = log.lines().count();
        index.add(vec![entry("$1", 1, "Pizza tonight?")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("log")).unwrap().lines().count(),
            lines,
            "unchanged isn't rewritten"
        );
        index.rewrite().unwrap();
        assert_eq!(found(&index), ["fine, pizza", "Pizza tonight?"], "a rewrite keeps what counts");
        assert_eq!(std::fs::read_to_string(dir.join("log")).unwrap().lines().count(), 2, "one batch and the crawl");

        // Another key (a new login) can't read it, and starts afresh.
        drop(index);
        let other = SearchIndex::open(&dir, b"another secret").unwrap();
        assert_eq!(other.len(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

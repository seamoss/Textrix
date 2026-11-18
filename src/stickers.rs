//! Stickers from image packs (MSC2545: `im.ponies.*`, and the stable
//! `m.room.image_pack`): the same packs custom emoji come from, the images
//! meant as stickers. `/sticker <name>` sends one; `/sticker` on its own
//! opens a picker of their pictures, searched as you type.

use std::collections::HashMap;

use matrix_sdk::ruma::OwnedMxcUri;

use crate::gallery::Thumb;

/// A tile's picture, in cells, and the room for its name under it.
pub const PICTURE: (u16, u16) = (12, 6);
/// From one tile to the next: across, and down.
pub const TILE: (u16, u16) = (14, 7);

/// A sticker: what it's called in its pack, what it says it is, its
/// picture, and the picture's details (size, type) as the pack gives them.
#[derive(Clone, Debug, PartialEq)]
pub struct Sticker {
    pub code: String,
    pub body: String,
    pub url: OwnedMxcUri,
    pub info: serde_json::Value,
}

/// A pack's stickers: images marked as stickers, or (with no mark on them
/// or their pack) both stickers and emoji, as the spec has it.
pub fn pack_stickers(content: &serde_json::Value) -> Vec<Sticker> {
    let usage = |v: &serde_json::Value| v["usage"].as_array().map(|u| u.iter().any(|x| x == "sticker"));
    let pack_ok = usage(&content["pack"]).unwrap_or(true);
    let Some(images) = content["images"].as_object() else { return Vec::new() };
    let mut stickers: Vec<Sticker> = images
        .iter()
        .filter(|(_, image)| usage(image).unwrap_or(pack_ok))
        .filter_map(|(code, image)| {
            let url = image["url"].as_str().filter(|u| u.starts_with("mxc://"))?;
            let code = code.trim_matches(':').to_owned();
            let body =
                image["body"].as_str().filter(|b| !b.trim().is_empty()).map_or_else(|| code.clone(), str::to_owned);
            let info = image.get("info").filter(|i| i.is_object()).cloned().unwrap_or_else(|| serde_json::json!({}));
            Some(Sticker { code, body, url: OwnedMxcUri::from(url), info })
        })
        .collect();
    stickers.sort_by(|a, b| a.code.cmp(&b.code));
    stickers
}

/// The sticker `name` means: one called that, or else the only one whose
/// name (or what it says it is) has it in. Several: which they are.
pub fn find<'a>(stickers: &'a [Sticker], name: &str) -> Result<&'a Sticker, String> {
    let name = name.trim().trim_matches(':').to_lowercase();
    if let Some(exact) = stickers.iter().find(|s| s.code.to_lowercase() == name) {
        return Ok(exact);
    }
    let close: Vec<&Sticker> = stickers
        .iter()
        .filter(|s| s.code.to_lowercase().contains(&name) || s.body.to_lowercase().contains(&name))
        .collect();
    match close.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no sticker called {name}; /sticker on its own shows them")),
        many => {
            let names: Vec<&str> = many.iter().take(8).map(|s| s.code.as_str()).collect();
            Err(format!("which? {}{}", names.join(", "), if many.len() > 8 { "…" } else { "" }))
        }
    }
}

/// `/sticker`: the stickers for a room, in a grid, searched as you type.
pub struct StickerPicker {
    pub key: crate::app::WinKey,
    pub stickers: Vec<Sticker>,
    pub query: String,
    pub selected: usize,
    /// Tiles across, and rows of them seen, as last drawn (for ↑/↓).
    pub cols: usize,
    pub rows: usize,
    /// The first row seen.
    pub top: usize,
    /// Their pictures, as they come.
    pub pictures: HashMap<OwnedMxcUri, Thumb>,
}

impl StickerPicker {
    pub fn new(key: crate::app::WinKey, stickers: Vec<Sticker>) -> Self {
        Self { key, stickers, query: String::new(), selected: 0, cols: 1, rows: 1, top: 0, pictures: HashMap::new() }
    }

    /// Keep the chosen one's row in sight.
    pub fn follow(&mut self) {
        let row = self.selected / self.cols.max(1);
        if row < self.top {
            self.top = row;
        } else if row >= self.top + self.rows.max(1) {
            self.top = row + 1 - self.rows.max(1);
        }
    }

    /// A picture came (`None`: it can't be shown).
    pub fn store(&mut self, url: OwnedMxcUri, picture: Option<crate::picture::Picture>) {
        self.pictures.insert(url, picture.map_or(Thumb::Failed, Thumb::Ready));
    }

    /// The stickers the search matches, by name or what they say they are.
    pub fn shown(&self) -> Vec<&Sticker> {
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        self.stickers
            .iter()
            .filter(|s| {
                let text = format!("{} {}", s.code, s.body).to_lowercase();
                words.iter().all(|w| text.contains(w))
            })
            .collect()
    }

    pub fn chosen(&self) -> Option<&Sticker> {
        self.shown().get(self.selected).copied()
    }

    /// Move by `by` tiles (a row is `cols`), staying in the grid.
    pub fn step(&mut self, by: isize) {
        let last = self.shown().len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    pub fn typed(&mut self) {
        (self.selected, self.top) = (0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> serde_json::Value {
        serde_json::json!({
            "pack": { "display_name": "Cats" },
            "images": {
                ":blobcat:": { "url": "mxc://x.org/blob", "body": "a blob cat" },
                "bigcat": { "url": "mxc://x.org/big", "usage": ["sticker"], "info": { "w": 256, "h": 256, "mimetype": "image/png" } },
                "smallcat": { "url": "mxc://x.org/small", "usage": ["emoticon"] },
                "broken": { "url": "https://x.org/nope" },
            }
        })
    }

    #[test]
    fn packs_stickers_are_the_ones_meant_as_stickers() {
        let stickers = pack_stickers(&pack());
        let codes: Vec<&str> = stickers.iter().map(|s| s.code.as_str()).collect();
        assert_eq!(codes, ["bigcat", "blobcat"], "not emoticon-only ones, not ones without an mxc picture");
        assert_eq!(stickers[1].body, "a blob cat");
        assert_eq!(stickers[0].body, "bigcat", "no body: its name");
        assert_eq!(stickers[0].info["w"], 256);
        assert_eq!(stickers[1].info, serde_json::json!({}));
        // A pack marked emoticons only: none.
        let emotes =
            serde_json::json!({ "pack": { "usage": ["emoticon"] }, "images": { "a": { "url": "mxc://x/a" } } });
        assert!(pack_stickers(&emotes).is_empty());
    }

    #[test]
    fn a_sticker_is_found_by_its_name_or_part_of_it() {
        let stickers = pack_stickers(&pack());
        assert_eq!(find(&stickers, ":bigcat:").unwrap().code, "bigcat");
        assert_eq!(find(&stickers, "blob").unwrap().code, "blobcat");
        assert!(find(&stickers, "cat").unwrap_err().starts_with("which? bigcat, blobcat"));
        assert!(find(&stickers, "dog").unwrap_err().contains("no sticker called dog"));
    }

    #[test]
    fn the_picker_searches_and_moves_in_its_grid() {
        let key = crate::app::WinKey { account: "t".into(), room_id: "!r:x.org".try_into().unwrap(), thread: None };
        let mut p = StickerPicker::new(key, pack_stickers(&pack()));
        p.cols = 2;
        p.step(1);
        assert_eq!(p.chosen().map(|s| s.code.as_str()), Some("blobcat"));
        p.step(2);
        assert_eq!(p.selected, 1, "stays in the grid");
        p.step(-5);
        assert_eq!(p.selected, 0);
        p.query = "blob cat".into();
        p.typed();
        assert_eq!(p.shown().len(), 1, "what it says it is counts");
        assert_eq!(p.chosen().map(|s| s.code.as_str()), Some("blobcat"));
    }
}

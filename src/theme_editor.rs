//! `/theme edit`: the theme on screen, one colour at a time, changed as you
//! watch. Each change is a `[colors]` key over the theme, the same as in
//! config.toml; `s` saves the lot as a theme file of your own (to keep, and
//! to send), Esc puts everything back.
//!
//! Colours move in HSL (hue, then how vivid, then how light), worked out from
//! the full-colour theme, so a 256-colour terminal's rounding never creeps in.

use std::collections::BTreeMap;

use ratatui::style::Color;

use crate::theme::{Theme, ThemeId};

/// Every colour role, by where it shows: (heading, `[colors]` key, what it
/// colours).
pub const ROLES: &[(&str, &str, &str)] = &[
    ("Screen", "background", "behind everything"),
    ("Screen", "foreground", "text, unless something says otherwise"),
    ("Screen", "dim", "quiet text: times, hints, joins and leaves"),
    ("Screen", "accent", "Textrix's own lines, and headings"),
    ("Screen", "frame", "panels' frames"),
    ("Screen", "border", "pane outlines"),
    ("Screen", "border_focus", "the outline of the pane you're in"),
    ("Bars", "bar_fg", "status and topic bar text"),
    ("Bars", "bar_bg", "status and topic bar background"),
    ("Bars", "bar_accent", "the status bar's [brackets] and names"),
    ("Bars", "bar_alert", "the status bar's alerts"),
    ("Bars", "pane_fg", "pane title bars' text"),
    ("Bars", "pane_bg", "pane title bars' background"),
    ("Messages", "own_nick", "your nick"),
    ("Messages", "own_text", "your own messages"),
    ("Messages", "highlight", "the nick on a message that mentions you"),
    ("Messages", "highlight_text", "a message that mentions you"),
    ("Messages", "mention", "a name mentioned in a message"),
    ("Messages", "link", "links"),
    ("Messages", "error", "errors"),
    ("Messages", "divider", "the new-messages divider, and day changes"),
    ("Messages", "receipts", "who's read up to here"),
    ("Messages", "pinned", "pinned messages' mark"),
    ("Messages", "matched", "what a search matched"),
    ("Messages", "badge", "counts and badges: reactions, replies"),
    ("Nicks", "nicks", "everyone else's nicks (each person keeps one)"),
    ("Nicks", "op", "admins' @ in the member list"),
    ("Nicks", "voice", "moderators' + in the member list"),
    ("Window list", "current_fg", "the window you're in"),
    ("Window list", "current_bg", "the window you're in, behind it"),
    ("Window list", "shown", "windows on screen in a split"),
    ("Window list", "inactive", "windows with nothing new"),
    ("Window list", "activity_event", "windows with joins and the like"),
    ("Window list", "activity", "windows with new messages"),
    ("Presence", "online", "online"),
    ("Presence", "away", "away"),
    ("Presence", "offline", "offline"),
    ("Code", "code", "code"),
    ("Code", "code_comment", "code: comments"),
    ("Code", "code_keyword", "code: keywords"),
    ("Code", "code_string", "code: strings"),
    ("Code", "code_number", "code: numbers"),
    ("Code", "code_function", "code: functions"),
    ("Code", "code_type", "code: types"),
];

/// The keys that colour a background, where "the terminal's own" is dark.
const BACKGROUNDS: [&str; 4] = ["background", "bar_bg", "pane_bg", "current_bg"];

/// How far one press moves a colour.
pub const HUE_STEP: f64 = 10.0;
pub const FINE_HUE_STEP: f64 = 2.0;
pub const LIGHT_STEP: f64 = 0.04;
pub const VIVID_STEP: f64 = 0.06;

pub struct ThemeEditor {
    /// The theme being changed: the one that was on screen.
    pub base: ThemeId,
    /// The `[colors]` changes over it, starting from yours in config.toml.
    pub colors: BTreeMap<String, String>,
    /// The same theme in full colour, with them.
    pub full: Theme,
    /// Your `[colors]` and the screen as they were, for Esc.
    pub before: (BTreeMap<String, String>, Theme),
    /// The room whose own theme it is, when it's a room's.
    pub room: Option<String>,
    /// Which of `ROLES`.
    pub selected: usize,
    /// A value being typed for the chosen colour.
    pub typing: Option<String>,
    /// The name being typed to save it as.
    pub naming: Option<String>,
    pub error: Option<String>,
}

impl ThemeEditor {
    pub fn key(&self) -> &'static str {
        ROLES[self.selected].1
    }

    pub fn about(&self) -> &'static str {
        ROLES[self.selected].2
    }

    /// The chosen role's colours as they are now (several for `nicks`).
    pub fn current(&self, key: &str) -> Vec<Option<Color>> {
        match self.full.color_of(key) {
            Some(colour) => vec![colour],
            None if key == "nicks" => self.full.nicks.iter().map(|c| Some(*c)).collect(),
            None => Vec::new(),
        }
    }

    /// The chosen role, moved round the colour wheel (`hue`, in degrees),
    /// made more or less vivid and lighter or darker, as a `[colors]` value.
    pub fn moved(&self, hue: f64, vivid: f64, light: f64) -> String {
        let key = self.key();
        let fallback = if BACKGROUNDS.contains(&key) { (0, 0, 0) } else { (0xd0, 0xd0, 0xd0) };
        self.current(key)
            .into_iter()
            .map(|colour| {
                let (h, s, l) = to_hsl(rgb_of(colour, fallback));
                hex(from_hsl(h + hue, (s + vivid).clamp(0.0, 1.0), (l + light).clamp(0.0, 1.0)))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The chosen role as text: what a typed value starts from.
    pub fn value(&self) -> String {
        if let Some(value) = self.colors.get(self.key()) {
            return value.clone();
        }
        let fallback = if BACKGROUNDS.contains(&self.key()) { (0, 0, 0) } else { (0xd0, 0xd0, 0xd0) };
        let shown: Vec<String> = self
            .current(self.key())
            .into_iter()
            .map(|c| if c.is_none() { "default".into() } else { hex(rgb_of(c, fallback)) })
            .collect();
        shown.join(", ")
    }

    /// The changes with `key` set to `value` (`None`: the theme's own), in
    /// full colour, or why that won't do.
    pub fn with(
        &self,
        key: &str,
        value: Option<String>,
        dir: &std::path::Path,
    ) -> Result<(BTreeMap<String, String>, Theme), String> {
        let mut colors = self.colors.clone();
        match value {
            Some(value) => colors.insert(key.to_owned(), value),
            None => colors.remove(key),
        };
        let full = Theme::resolve(&self.base, &colors, dir).map_err(|e| format!("{e:#}"))?;
        Ok((colors, full))
    }

    /// The theme name the save prompt starts with.
    pub fn suggested_name(&self) -> String {
        match &self.base {
            ThemeId::File(name) => name.clone(),
            ThemeId::Auto => format!("{}-mine", crate::theme::auto_choice().name()),
            id => format!("{}-mine", id.name()),
        }
    }
}

/// A colour's red, green and blue: as it is, from the xterm palette, or
/// `fallback` for the terminal's own.
pub fn rgb_of(colour: Option<Color>, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    // The 16 basic colours as xterm draws them.
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let indexed = |n: u8| -> (u8, u8, u8) {
        match n {
            0..=15 => BASIC[usize::from(n)],
            16..=231 => {
                let n = n - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                (level(n / 36), level((n / 6) % 6), level(n % 6))
            }
            _ => {
                let grey = 8 + (n - 232) * 10;
                (grey, grey, grey)
            }
        }
    };
    match colour {
        None | Some(Color::Reset) => fallback,
        Some(Color::Rgb(r, g, b)) => (r, g, b),
        Some(Color::Indexed(n)) => indexed(n),
        Some(Color::Black) => indexed(0),
        Some(Color::Red) => indexed(1),
        Some(Color::Green) => indexed(2),
        Some(Color::Yellow) => indexed(3),
        Some(Color::Blue) => indexed(4),
        Some(Color::Magenta) => indexed(5),
        Some(Color::Cyan) => indexed(6),
        Some(Color::Gray) => indexed(7),
        Some(Color::DarkGray) => indexed(8),
        Some(Color::LightRed) => indexed(9),
        Some(Color::LightGreen) => indexed(10),
        Some(Color::LightYellow) => indexed(11),
        Some(Color::LightBlue) => indexed(12),
        Some(Color::LightMagenta) => indexed(13),
        Some(Color::LightCyan) => indexed(14),
        Some(Color::White) => indexed(15),
    }
}

pub fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Hue in degrees (0–360), saturation and lightness (0–1).
pub fn to_hsl((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let [r, g, b] = [r, g, b].map(|v| f64::from(v) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, s.clamp(0.0, 1.0), l)
}

pub fn from_hsl(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (byte(r), byte(g), byte(b))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::theme::{KEYS, ThemeName};

    fn editor(name: ThemeName) -> ThemeEditor {
        let base = ThemeId::Builtin(name);
        let full = Theme::resolve(&base, &BTreeMap::new(), Path::new("")).unwrap();
        ThemeEditor {
            base,
            colors: BTreeMap::new(),
            before: (BTreeMap::new(), full.clone()),
            full,
            room: None,
            selected: 0,
            typing: None,
            naming: None,
            error: None,
        }
    }

    #[test]
    fn every_colour_key_is_a_role_once() {
        let roles: Vec<&str> = ROLES.iter().map(|(_, key, _)| *key).collect();
        let mut sorted = roles.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), roles.len(), "no role twice");
        let mut keys = KEYS.to_vec();
        keys.sort_unstable();
        assert_eq!(sorted, keys, "every [colors] key, and nothing else");
    }

    #[test]
    fn hsl_goes_there_and_back() {
        for rgb in [(0x72, 0xf1, 0xb8), (255, 0, 0), (0, 0, 0), (255, 255, 255), (12, 34, 56), (128, 128, 128)] {
            let (h, s, l) = to_hsl(rgb);
            assert_eq!(from_hsl(h, s, l), rgb, "{rgb:?}");
        }
        assert_eq!(from_hsl(120.0, 1.0, 0.5), (0, 255, 0));
        assert_eq!(from_hsl(-120.0, 1.0, 0.5), (0, 0, 255), "round the wheel");
        assert_eq!(rgb_of(Some(Color::Indexed(196)), (1, 1, 1)), (255, 0, 0));
        assert_eq!(rgb_of(Some(Color::Indexed(244)), (1, 1, 1)), (128, 128, 128));
        assert_eq!(rgb_of(None, (1, 2, 3)), (1, 2, 3), "the terminal's own");
    }

    #[test]
    fn colours_move_round_the_wheel_and_lighter() {
        let mut e = editor(ThemeName::Synthwave);
        e.selected = ROLES.iter().position(|(_, key, _)| *key == "own_text").unwrap();
        // Synthwave's mint (#72f1b8), a third of the way round: violet-ish.
        let (h, s, l) = to_hsl((0x72, 0xf1, 0xb8));
        assert_eq!(e.moved(0.0, 0.0, 0.0), "#72f1b8", "unmoved, the same");
        assert_eq!(e.moved(120.0, 0.0, 0.0), hex(from_hsl(h + 120.0, s, l)));
        assert_eq!(e.moved(0.0, 0.0, 1.0), "#ffffff", "lightness stops at white");
        // Nicks move together, each its own way round.
        e.selected = ROLES.iter().position(|(_, key, _)| *key == "nicks").unwrap();
        let moved = e.moved(HUE_STEP, 0.0, 0.0);
        assert_eq!(moved.split(", ").count(), e.full.nicks.len());
    }

    #[test]
    fn changes_are_colors_keys_and_bad_ones_say_why() {
        let e = editor(ThemeName::Default);
        let (colors, full) = e.with("link", Some("#ff0000".into()), Path::new("")).unwrap();
        assert_eq!(colors.get("link").map(String::as_str), Some("#ff0000"));
        assert_eq!(full.color_of("link"), Some(Some(Color::Rgb(255, 0, 0))));
        assert!(e.with("link", Some("nope".into()), Path::new("")).unwrap_err().contains("isn't a colour"));
        let (back, _) = e.with("link", None, Path::new("")).unwrap();
        assert!(back.is_empty(), "the theme's own again");
        assert_eq!(e.suggested_name(), "default-mine");
    }
}

//! Colours: a built-in theme (`theme = "default" | "light" | "mono"`),
//! adjusted by a `[colors]` table in config.toml.

use std::{collections::BTreeMap, str::FromStr};

use anyhow::{Result, bail};
use ratatui::{
    style::{Color, Modifier, Style},
    widgets::BorderType,
};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    /// For dark terminal backgrounds.
    #[default]
    Default,
    /// For light terminal backgrounds.
    Light,
    /// No colours: bold, underline and reverse video only.
    Mono,
    /// Green on black, like a classic terminal ("phosphor" works too).
    #[serde(alias = "phosphor")]
    Green,
    /// Blue on black.
    Blue,
    /// Red on black, like a Virtual Boy ("virtualboy" works too).
    #[serde(alias = "virtualboy")]
    Red,
    /// The colour on deep blue, every panel framed in double lines: an
    /// 80s airport terminal.
    #[serde(rename = "green-deep")]
    GreenDeep,
    #[serde(rename = "blue-deep")]
    BlueDeep,
    #[serde(rename = "red-deep")]
    RedDeep,
    /// Black, with thin neon outlines round everything.
    #[serde(rename = "green-tron")]
    GreenTron,
    #[serde(rename = "blue-tron")]
    BlueTron,
    #[serde(rename = "red-tron")]
    RedTron,
}

impl ThemeName {
    pub const ALL: [(&str, ThemeName); 12] = [
        ("default", ThemeName::Default),
        ("light", ThemeName::Light),
        ("mono", ThemeName::Mono),
        ("green", ThemeName::Green),
        ("green-deep", ThemeName::GreenDeep),
        ("green-tron", ThemeName::GreenTron),
        ("blue", ThemeName::Blue),
        ("blue-deep", ThemeName::BlueDeep),
        ("blue-tron", ThemeName::BlueTron),
        ("red", ThemeName::Red),
        ("red-deep", ThemeName::RedDeep),
        ("red-tron", ThemeName::RedTron),
    ];

    /// A theme by name (also "dark", "phosphor", "virtualboy").
    pub fn parse(name: &str) -> Option<ThemeName> {
        match name {
            "dark" => Some(ThemeName::Default),
            "phosphor" => Some(ThemeName::Green),
            "virtualboy" => Some(ThemeName::Red),
            name => ThemeName::ALL.iter().find(|(n, _)| *n == name).map(|(_, t)| *t),
        }
    }

    pub fn name(self) -> &'static str {
        ThemeName::ALL.iter().find(|(_, t)| *t == self).map_or("default", |(n, _)| n)
    }
}

/// The style of every part of the screen.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Laid under everything (empty: the terminal's own colours).
    pub base: Style,
    /// Topic and status bars, and the focused pane's title.
    pub bar: Style,
    /// The brackets around status bar items.
    pub bar_accent: Style,
    /// Warnings and prompts on the status bar.
    pub bar_alert: Style,
    /// Title bars of panes without the focus.
    pub pane: Style,
    /// Timestamps, punctuation, borders, quiet details.
    pub dim: Style,
    /// `-!-` markers and popup titles.
    pub accent: Style,
    /// Popup frames and the input prompt.
    pub frame: Style,
    /// Your own nick.
    pub own: Style,
    /// The nick on a message that mentions you.
    pub highlight: Style,
    /// The text of a message that mentions you.
    pub highlight_text: Style,
    pub error: Style,
    /// The "new messages" divider.
    pub divider: Style,
    pub link: Style,
    pub mention: Style,
    pub code: Style,
    /// "[N replies]".
    pub badge: Style,
    pub pinned: Style,
    /// "✓ nick" read markers.
    pub receipts: Style,
    /// Matched letters in the switcher, keys in the help screen.
    pub matched: Style,
    /// The current window in the window list.
    pub current: Style,
    /// The numbers of windows shown in other panes.
    pub shown: Style,
    /// Window list entries by activity.
    pub inactive: Style,
    pub activity_event: Style,
    pub activity: Style,
    /// Member list prefixes.
    pub op: Style,
    pub voice: Style,
    pub online: Color,
    pub away: Color,
    pub offline: Color,
    /// Nick colours, picked per user.
    pub nicks: Vec<Color>,
    /// Use the colours senders put in formatted messages.
    pub message_colors: bool,
    /// Frame the window list, panes and member list in these lines.
    pub outlines: Option<BorderType>,
    /// The frames' lines, and the focused pane's.
    pub border: Style,
    pub border_focus: Style,
}

/// A dark blue (#005f87) that white text reads well on.
const BAR_BG: Color = Color::Indexed(24);

/// The deep blue behind the airport-terminal themes (#00005f).
const DEEP_BLUE: Color = Color::Indexed(17);

/// A monochrome palette, as 256-colour numbers: shades from brightest to
/// darkest, a hot one for mentions, one for code, and nick colours.
struct Hue {
    shades: [u8; 4],
    hot: u8,
    code: u8,
    nicks: [u8; 7],
}

const GREEN: Hue = Hue { shades: [46, 34, 28, 22], hot: 118, code: 40, nicks: [46, 40, 34, 82, 118, 77, 41] };
const BLUE: Hue = Hue { shades: [39, 32, 25, 17], hot: 123, code: 45, nicks: [39, 45, 33, 75, 81, 117, 27] };
/// Lighter blues, to read on the deep blue.
const BLUE_ON_DEEP: Hue =
    Hue { shades: [123, 81, 74, 25], hot: 231, code: 159, nicks: [123, 159, 117, 87, 81, 195, 153] };
const RED: Hue = Hue { shades: [196, 160, 124, 52], hot: 210, code: 203, nicks: [196, 160, 203, 124, 167, 197, 161] };

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backdrop {
    Black,
    Deep,
    Tron,
}

impl Default for Theme {
    fn default() -> Self {
        let bold = Modifier::BOLD;
        Self {
            base: Style::new(),
            // Fixed 256-colour shades, not the terminal's own "blue" (which
            // many colour schemes make too light for white text).
            bar: Style::new().fg(Color::Indexed(255)).bg(BAR_BG),
            bar_accent: Style::new().fg(Color::Indexed(117)).bg(BAR_BG),
            bar_alert: Style::new().fg(Color::Indexed(221)).bg(BAR_BG),
            pane: Style::new().fg(Color::Indexed(252)).bg(Color::Indexed(238)),
            dim: Style::new().fg(Color::DarkGray),
            accent: Style::new().fg(Color::Cyan).add_modifier(bold),
            frame: Style::new().fg(Color::Cyan),
            own: Style::new().add_modifier(bold),
            highlight: Style::new().fg(Color::Magenta).add_modifier(bold),
            highlight_text: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            divider: Style::new().fg(Color::Red),
            link: Style::new().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
            mention: Style::new().fg(Color::LightBlue).add_modifier(bold),
            code: Style::new().fg(Color::LightYellow),
            badge: Style::new().fg(Color::Cyan),
            pinned: Style::new().fg(Color::Yellow),
            receipts: Style::new().fg(Color::Green).add_modifier(Modifier::DIM),
            matched: Style::new().fg(Color::Yellow).add_modifier(bold),
            current: Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(bold),
            shown: Style::new().fg(Color::Cyan).add_modifier(bold),
            inactive: Style::new().fg(Color::Gray),
            activity_event: Style::new().fg(Color::Cyan),
            activity: Style::new().fg(Color::White).add_modifier(bold),
            op: Style::new().fg(Color::Yellow).add_modifier(bold),
            voice: Style::new().fg(Color::Green),
            online: Color::Green,
            away: Color::Yellow,
            offline: Color::DarkGray,
            nicks: vec![
                Color::Cyan,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Red,
                Color::LightCyan,
                Color::LightGreen,
                Color::LightBlue,
                Color::LightRed,
                Color::LightYellow,
            ],
            message_colors: true,
            outlines: None,
            border: Style::new().fg(Color::DarkGray),
            border_focus: Style::new().fg(Color::Cyan),
        }
    }
}

impl Theme {
    pub fn build(name: ThemeName, overrides: &BTreeMap<String, String>) -> Result<Self> {
        let mut theme = match name {
            ThemeName::Default => Self::default(),
            ThemeName::Light => Self::light(),
            ThemeName::Mono => Self::mono(),
            ThemeName::Green => Self::monochrome(GREEN, Backdrop::Black),
            ThemeName::Blue => Self::monochrome(BLUE, Backdrop::Black),
            ThemeName::Red => Self::monochrome(RED, Backdrop::Black),
            ThemeName::GreenDeep => Self::monochrome(GREEN, Backdrop::Deep),
            ThemeName::BlueDeep => Self::monochrome(BLUE_ON_DEEP, Backdrop::Deep),
            ThemeName::RedDeep => Self::monochrome(RED, Backdrop::Deep),
            ThemeName::GreenTron => Self::monochrome(GREEN, Backdrop::Tron),
            ThemeName::BlueTron => Self::monochrome(BLUE, Backdrop::Tron),
            ThemeName::RedTron => Self::monochrome(RED, Backdrop::Tron),
        };
        for (key, value) in overrides {
            theme.set(key, value)?;
        }
        Ok(theme)
    }

    /// Darker colours that read on a white background.
    fn light() -> Self {
        let bold = Modifier::BOLD;
        Self {
            dim: Style::new().fg(Color::Indexed(244)),
            highlight_text: Style::new().fg(Color::Indexed(130)),
            mention: Style::new().fg(Color::Blue).add_modifier(bold),
            code: Style::new().fg(Color::Indexed(125)),
            pinned: Style::new().fg(Color::Indexed(130)),
            receipts: Style::new().fg(Color::Indexed(28)),
            matched: Style::new().fg(Color::Indexed(130)).add_modifier(bold),
            shown: Style::new().fg(Color::Blue).add_modifier(bold),
            inactive: Style::new().fg(Color::Indexed(240)),
            activity_event: Style::new().fg(Color::Indexed(30)),
            activity: Style::new().fg(Color::Reset).add_modifier(bold),
            badge: Style::new().fg(Color::Indexed(30)),
            accent: Style::new().fg(Color::Indexed(30)).add_modifier(bold),
            frame: Style::new().fg(Color::Indexed(30)),
            op: Style::new().fg(Color::Indexed(130)).add_modifier(bold),
            voice: Style::new().fg(Color::Indexed(28)),
            pane: Style::new().fg(Color::Indexed(235)).bg(Color::Indexed(250)),
            away: Color::Indexed(130),
            offline: Color::Indexed(248),
            online: Color::Indexed(28),
            nicks: [25, 28, 90, 124, 30, 130, 54, 22, 88, 94].map(Color::Indexed).to_vec(),
            ..Self::default()
        }
    }

    /// Attributes only, for monochrome terminals or taste.
    fn mono() -> Self {
        let plain = Style::new();
        let bold = plain.add_modifier(Modifier::BOLD);
        let reversed = plain.add_modifier(Modifier::REVERSED);
        Self {
            base: plain,
            bar: reversed,
            bar_accent: reversed,
            bar_alert: reversed.add_modifier(Modifier::BOLD),
            pane: plain.add_modifier(Modifier::UNDERLINED),
            dim: plain.add_modifier(Modifier::DIM),
            accent: bold,
            frame: plain,
            own: bold,
            highlight: bold.add_modifier(Modifier::UNDERLINED),
            highlight_text: bold,
            error: bold,
            divider: bold,
            link: plain.add_modifier(Modifier::UNDERLINED),
            mention: bold,
            code: plain.add_modifier(Modifier::ITALIC),
            badge: plain,
            pinned: plain,
            receipts: plain.add_modifier(Modifier::DIM),
            matched: bold.add_modifier(Modifier::UNDERLINED),
            current: reversed.add_modifier(Modifier::BOLD),
            shown: bold,
            inactive: plain,
            activity_event: plain.add_modifier(Modifier::ITALIC),
            activity: bold,
            op: bold,
            voice: plain,
            online: Color::Reset,
            away: Color::Reset,
            offline: Color::Reset,
            nicks: vec![Color::Reset],
            message_colors: false,
            outlines: None,
            border: plain.add_modifier(Modifier::DIM),
            border_focus: bold,
        }
    }

    /// One colour, like a monochrome monitor, on black, on deep blue with
    /// double-line frames (80s airport terminal), or on black with thin
    /// neon outlines (Tron).
    fn monochrome(hue: Hue, backdrop: Backdrop) -> Self {
        let [bright, mid, dim, dark] = hue.shades.map(Color::Indexed);
        let hot = Color::Indexed(hue.hot);
        let (paper, outlines) = match backdrop {
            Backdrop::Black => (Color::Black, None),
            Backdrop::Deep => (DEEP_BLUE, Some(BorderType::Double)),
            Backdrop::Tron => (Color::Black, Some(BorderType::Rounded)),
        };
        let g = Style::new().fg(bright);
        let bold = Modifier::BOLD;
        let bar = Style::new().fg(paper).bg(mid);
        Self {
            base: Style::new().fg(bright).bg(paper),
            bar,
            bar_accent: bar.fg(dark),
            bar_alert: bar.add_modifier(bold),
            pane: Style::new().fg(bright).bg(dark),
            dim: Style::new().fg(dim),
            accent: g.add_modifier(bold),
            frame: g,
            own: g.add_modifier(bold),
            highlight: Style::new().fg(hot).add_modifier(bold | Modifier::UNDERLINED),
            highlight_text: Style::new().fg(hot),
            error: g.add_modifier(bold | Modifier::REVERSED),
            divider: g,
            link: g.add_modifier(Modifier::UNDERLINED),
            mention: Style::new().fg(hot).add_modifier(bold),
            code: Style::new().fg(Color::Indexed(hue.code)),
            badge: Style::new().fg(dim),
            pinned: g,
            receipts: Style::new().fg(dim),
            matched: Style::new().fg(hot).add_modifier(bold),
            current: Style::new().fg(paper).bg(bright).add_modifier(bold),
            shown: g.add_modifier(bold),
            inactive: Style::new().fg(mid),
            activity_event: Style::new().fg(mid),
            activity: g.add_modifier(bold),
            op: g.add_modifier(bold),
            voice: g,
            online: bright,
            away: mid,
            offline: dark,
            nicks: hue.nicks.map(Color::Indexed).to_vec(),
            message_colors: false,
            outlines,
            // Tron's lines glow brightest; the airport's are quieter.
            border: Style::new().fg(if backdrop == Backdrop::Tron { mid } else { dim }),
            border_focus: Style::new().fg(if backdrop == Backdrop::Tron { hot } else { bright }).add_modifier(bold),
        }
    }

    /// One `[colors]` entry: a colour name ("lightblue"), "#rrggbb", or a
    /// 256-colour number; `nicks` takes a comma-separated list.
    fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let parse = |v: &str| {
            Color::from_str(v.trim()).map_err(|_| {
                anyhow::anyhow!("[colors] {key}: “{v}” isn't a colour (a name like lightblue, #rrggbb, or 0–255)")
            })
        };
        if key == "nicks" {
            let nicks = value.split(',').filter(|v| !v.trim().is_empty()).map(parse).collect::<Result<Vec<_>>>()?;
            if nicks.is_empty() {
                bail!("[colors] nicks: give at least one colour");
            }
            self.nicks = nicks;
            return Ok(());
        }
        let c = parse(value)?;
        match key {
            "bar_fg" => self.bar = self.bar.fg(c),
            "bar_bg" => {
                self.bar = self.bar.bg(c);
                self.bar_accent = self.bar_accent.bg(c);
                self.bar_alert = self.bar_alert.bg(c);
            }
            "bar_accent" => self.bar_accent = self.bar_accent.fg(c),
            "bar_alert" => self.bar_alert = self.bar_alert.fg(c),
            "pane_fg" => self.pane = self.pane.fg(c),
            "pane_bg" => self.pane = self.pane.bg(c),
            "dim" => self.dim = self.dim.fg(c),
            "accent" => self.accent = self.accent.fg(c),
            "frame" => self.frame = self.frame.fg(c),
            "own_nick" => self.own = self.own.fg(c),
            "highlight" => self.highlight = self.highlight.fg(c),
            "highlight_text" => self.highlight_text = self.highlight_text.fg(c),
            "error" => self.error = self.error.fg(c),
            "divider" => self.divider = self.divider.fg(c),
            "link" => self.link = self.link.fg(c),
            "mention" => self.mention = self.mention.fg(c),
            "code" => self.code = self.code.fg(c),
            "badge" => self.badge = self.badge.fg(c),
            "pinned" => self.pinned = self.pinned.fg(c),
            "receipts" => self.receipts = self.receipts.fg(c),
            "matched" => self.matched = self.matched.fg(c),
            "current_fg" => self.current = self.current.fg(c),
            "current_bg" => self.current = self.current.bg(c),
            "shown" => self.shown = self.shown.fg(c),
            "inactive" => self.inactive = self.inactive.fg(c),
            "activity_event" => self.activity_event = self.activity_event.fg(c),
            "activity" => self.activity = self.activity.fg(c),
            "op" => self.op = self.op.fg(c),
            "voice" => self.voice = self.voice.fg(c),
            "online" => self.online = c,
            "away" => self.away = c,
            "offline" => self.offline = c,
            _ => bail!("[colors] has no “{key}”; the keys are: {}", KEYS.join(", ")),
        }
        Ok(())
    }

    /// The colour for a user (or room), the same every time.
    pub fn nick_color(&self, key: &str) -> Color {
        let hash = key.bytes().fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x01000193));
        self.nicks[hash as usize % self.nicks.len()]
    }
}

const KEYS: &[&str] = &[
    "bar_fg",
    "bar_bg",
    "bar_accent",
    "bar_alert",
    "pane_fg",
    "pane_bg",
    "dim",
    "accent",
    "frame",
    "own_nick",
    "highlight",
    "highlight_text",
    "error",
    "divider",
    "link",
    "mention",
    "code",
    "badge",
    "pinned",
    "receipts",
    "matched",
    "current_fg",
    "current_bg",
    "shown",
    "inactive",
    "activity_event",
    "activity",
    "op",
    "voice",
    "online",
    "away",
    "offline",
    "nicks",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_parse_names_hex_and_numbers() {
        let colors = BTreeMap::from([
            ("bar_bg".to_owned(), "#202040".to_owned()),
            ("link".to_owned(), "lightcyan".to_owned()),
            ("nicks".to_owned(), "red, 214 ,#00ff00".to_owned()),
        ]);
        let theme = Theme::build(ThemeName::Default, &colors).unwrap();
        assert_eq!(theme.bar.bg, Some(Color::Rgb(0x20, 0x20, 0x40)));
        assert_eq!(theme.bar_alert.bg, theme.bar.bg, "the whole bar changes");
        assert_eq!(theme.link.fg, Some(Color::LightCyan));
        assert!(theme.link.add_modifier.contains(Modifier::UNDERLINED), "attributes are kept");
        assert_eq!(theme.nicks, [Color::Red, Color::Indexed(214), Color::Rgb(0, 255, 0)]);

        let typo = BTreeMap::from([("lnk".to_owned(), "red".to_owned())]);
        assert!(Theme::build(ThemeName::Default, &typo).unwrap_err().to_string().contains("link"));
        let bad = BTreeMap::from([("link".to_owned(), "reddish".to_owned())]);
        assert!(Theme::build(ThemeName::Default, &bad).is_err());
    }

    #[test]
    fn bars_use_fixed_colours_with_contrast() {
        // Terminal colour schemes redefine the 16 basic colours; the bars
        // must not depend on them.
        for theme in [Theme::default(), Theme::build(ThemeName::Light, &BTreeMap::new()).unwrap()] {
            for style in [theme.bar, theme.bar_accent, theme.bar_alert, theme.pane] {
                assert!(matches!(style.fg, Some(Color::Indexed(16..)) | None), "{style:?}");
                assert!(matches!(style.bg, Some(Color::Indexed(16..))), "{style:?}");
            }
        }
    }

    #[test]
    fn monochrome_themes_stay_one_colour_on_black() {
        for (text, name, bright) in [
            ("green", ThemeName::Green, 46),
            ("phosphor", ThemeName::Green, 46),
            ("blue", ThemeName::Blue, 39),
            ("red", ThemeName::Red, 196),
            ("virtualboy", ThemeName::Red, 196),
        ] {
            let parsed: ThemeName =
                toml::from_str::<std::collections::HashMap<String, ThemeName>>(&format!("t = \"{text}\""))
                    .unwrap()
                    .remove("t")
                    .unwrap();
            assert_eq!(parsed, name, "{text}");
            let theme = Theme::build(name, &BTreeMap::new()).unwrap();
            assert_eq!(theme.base, Style::new().fg(Color::Indexed(bright)).bg(Color::Black));
            assert!(!theme.message_colors, "senders' colours would break the look");
        }
    }

    #[test]
    fn deep_and_tron_variants_frame_everything() {
        for name in ThemeName::ALL.map(|(n, _)| n) {
            let theme = Theme::build(ThemeName::parse(name).unwrap(), &BTreeMap::new()).unwrap();
            assert_eq!(ThemeName::parse(name).unwrap().name(), name);
            match name.rsplit_once('-').map(|(_, kind)| kind) {
                Some("deep") => {
                    assert_eq!(theme.outlines, Some(BorderType::Double));
                    assert_eq!(theme.base.bg, Some(DEEP_BLUE));
                }
                Some("tron") => {
                    assert_eq!(theme.outlines, Some(BorderType::Rounded));
                    assert_eq!(theme.base.bg, Some(Color::Black));
                }
                _ => assert_eq!(theme.outlines, None, "{name}"),
            }
        }
        assert_eq!(ThemeName::parse("virtualboy"), Some(ThemeName::Red));
        assert!(ThemeName::parse("purple").is_none());
    }

    #[test]
    fn mono_uses_no_colours() {
        let theme = Theme::build(ThemeName::Mono, &BTreeMap::new()).unwrap();
        for style in [theme.bar, theme.highlight, theme.link, theme.current, theme.activity] {
            assert!(style.fg.is_none() && style.bg.is_none());
        }
        assert_eq!(theme.nick_color("@a:x.org"), Color::Reset);
    }

    #[test]
    fn every_key_is_settable() {
        for key in KEYS {
            let value = if *key == "nicks" { "red,blue" } else { "red" };
            Theme::build(ThemeName::Light, &BTreeMap::from([((*key).to_owned(), value.to_owned())])).unwrap();
        }
    }
}

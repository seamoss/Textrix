//! Colours: a built-in theme (`theme = "default" | "light" | "mono"`),
//! adjusted by a `[colors]` table in config.toml.

use std::{collections::BTreeMap, str::FromStr};

use anyhow::{Result, bail};
use ratatui::style::{Color, Modifier, Style};
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
}

/// A dark blue (#005f87) that white text reads well on.
const BAR_BG: Color = Color::Indexed(24);

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
        }
    }
}

impl Theme {
    pub fn build(name: ThemeName, overrides: &BTreeMap<String, String>) -> Result<Self> {
        let mut theme = match name {
            ThemeName::Default => Self::default(),
            ThemeName::Light => Self::light(),
            ThemeName::Mono => Self::mono(),
            ThemeName::Green => Self::green(),
            ThemeName::Blue => Self::blue(),
            ThemeName::Red => Self::red(),
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
        }
    }

    /// One colour on black, like a monochrome monitor: 256-colour shades
    /// from brightest to darkest, a hot one for mentions, another for code,
    /// and the nick palette.
    fn monochrome(shades: [u8; 4], hot: u8, code: u8, nicks: [u8; 7]) -> Self {
        let [bright, mid, dim, dark] = shades.map(Color::Indexed);
        let hot = Color::Indexed(hot);
        let g = Style::new().fg(bright);
        let bold = Modifier::BOLD;
        let bar = Style::new().fg(Color::Black).bg(mid);
        Self {
            base: Style::new().fg(bright).bg(Color::Black),
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
            code: Style::new().fg(Color::Indexed(code)),
            badge: Style::new().fg(dim),
            pinned: g,
            receipts: Style::new().fg(dim),
            matched: Style::new().fg(hot).add_modifier(bold),
            current: Style::new().fg(Color::Black).bg(bright).add_modifier(bold),
            shown: g.add_modifier(bold),
            inactive: Style::new().fg(mid),
            activity_event: Style::new().fg(mid),
            activity: g.add_modifier(bold),
            op: g.add_modifier(bold),
            voice: g,
            online: bright,
            away: mid,
            offline: dark,
            nicks: nicks.map(Color::Indexed).to_vec(),
            message_colors: false,
        }
    }

    /// Green phosphor, like a 1980s terminal.
    fn green() -> Self {
        Self::monochrome([46, 34, 28, 22], 118, 40, [46, 40, 34, 82, 118, 77, 41])
    }

    /// An electric-blue monochrome monitor.
    fn blue() -> Self {
        Self::monochrome([39, 32, 25, 17], 123, 45, [39, 45, 33, 75, 81, 117, 27])
    }

    /// Red LEDs on black, like a Virtual Boy.
    fn red() -> Self {
        Self::monochrome([196, 160, 124, 52], 210, 203, [196, 160, 203, 124, 167, 197, 161])
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

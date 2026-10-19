//! Colours: a built-in theme (`theme = "default"`, `"green-tron"`,
//! `"dracula"`…) or one from a theme file in `~/.textrix/themes`, adjusted
//! by a `[colors]` table in config.toml.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};

use crate::highlight::Syntax;
use ratatui::{
    style::{Color, Modifier, Style},
    symbols::border,
};
use serde::{Deserialize, Serialize};

/// The built-in themes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeName {
    /// For dark terminal backgrounds.
    #[default]
    Default,
    /// For light terminal backgrounds.
    Light,
    /// No colours: bold, underline and reverse video only.
    Mono,
    /// Green on black, like a classic terminal ("phosphor" works too).
    Green,
    /// The colour on deep blue, every panel framed in double lines: an
    /// 80s airport terminal.
    GreenDeep,
    /// Pure black, with thin neon outlines round everything.
    GreenTron,
    Blue,
    BlueDeep,
    BlueTron,
    /// Red on black, like a Virtual Boy ("virtualboy" works too).
    Red,
    RedDeep,
    RedTron,
    Amber,
    AmberDeep,
    AmberTron,
    /// Shades of grey on pure black, with thin outlines: Tron without
    /// the neon.
    MonoTron,
    /// IBM's green monochrome monitor (#00FF66), panels framed in ASCII.
    Ibm5151,
    /// Yellow on DOS blue, grey menu bars, double-line windows: Turbo
    /// Pascal.
    Turbo,
    /// Grey on black in ANSI colours, a cyan status line: a DOS terminal
    /// program dialling a BBS.
    Procomm,
    /// Light blue on Commodore blue.
    C64,
    /// White and orange on Workbench 1.3 blue.
    Amiga,
    /// Delphi 7's editor: black on white, navy and blue, Windows grey.
    Delphi,
    /// mIRC: black on white, in mIRC's colours.
    Mirc,
    SolarizedDark,
    SolarizedLight,
    Monokai,
    Dracula,
    Nord,
    Gruvbox,
    /// Catppuccin Mocha.
    Catppuccin,
}

impl ThemeName {
    pub const ALL: [(&str, ThemeName); 30] = [
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
        ("amber", ThemeName::Amber),
        ("amber-deep", ThemeName::AmberDeep),
        ("amber-tron", ThemeName::AmberTron),
        ("mono-tron", ThemeName::MonoTron),
        ("ibm5151", ThemeName::Ibm5151),
        ("turbo", ThemeName::Turbo),
        ("procomm", ThemeName::Procomm),
        ("c64", ThemeName::C64),
        ("amiga", ThemeName::Amiga),
        ("delphi", ThemeName::Delphi),
        ("mirc", ThemeName::Mirc),
        ("solarized-dark", ThemeName::SolarizedDark),
        ("solarized-light", ThemeName::SolarizedLight),
        ("monokai", ThemeName::Monokai),
        ("dracula", ThemeName::Dracula),
        ("nord", ThemeName::Nord),
        ("gruvbox", ThemeName::Gruvbox),
        ("catppuccin", ThemeName::Catppuccin),
    ];

    /// A theme by name (also "dark", "phosphor", "virtualboy",
    /// "solarized", "catppuccin-mocha", "ibm").
    pub fn parse(name: &str) -> Option<ThemeName> {
        match name {
            "dark" => Some(ThemeName::Default),
            "phosphor" => Some(ThemeName::Green),
            "virtualboy" => Some(ThemeName::Red),
            "solarized" => Some(ThemeName::SolarizedDark),
            "catppuccin-mocha" | "mocha" => Some(ThemeName::Catppuccin),
            "ibm" | "5151" => Some(ThemeName::Ibm5151),
            name => ThemeName::ALL.iter().find(|(n, _)| *n == name).map(|(_, t)| *t),
        }
    }

    pub fn name(self) -> &'static str {
        ThemeName::ALL.iter().find(|(_, t)| *t == self).map_or("default", |(n, _)| n)
    }

    /// A few words about it, for `/themes`.
    pub fn about(self) -> &'static str {
        use ThemeName::*;
        match self {
            Default => "for dark terminals, in their own colours",
            Light => "for light terminals",
            Mono => "no colour: bold, underline and reverse video",
            Green => "green phosphor on black",
            Blue => "blue on black",
            Red => "red on black, like a Virtual Boy",
            Amber => "amber on black",
            GreenDeep | BlueDeep | RedDeep | AmberDeep => "on deep blue in double lines: an 80s airport",
            GreenTron | BlueTron | RedTron | AmberTron => "neon outlines on pure black",
            MonoTron => "Tron in greys, on pure black",
            Ibm5151 => "IBM's green monochrome display, framed in ASCII",
            Turbo => "Turbo Pascal: yellow on DOS blue",
            Procomm => "a DOS terminal program dialling a BBS",
            C64 => "Commodore 64 blues",
            Amiga => "Workbench 1.3: white and orange on blue",
            Delphi => "Delphi 7's editor, in Windows grey",
            Mirc => "mIRC's colours on white",
            SolarizedDark => "Solarized, dark",
            SolarizedLight => "Solarized, light",
            Monokai => "Monokai",
            Dracula => "Dracula",
            Nord => "Nord",
            Gruvbox => "Gruvbox, dark",
            Catppuccin => "Catppuccin Mocha",
        }
    }

    /// Which shelf `/theme` lists it on.
    pub fn group(self) -> &'static str {
        use ThemeName::*;
        match self {
            Default | Light | Mono => "plain",
            Green | GreenDeep | GreenTron | Blue | BlueDeep | BlueTron | Red | RedDeep | RedTron | Amber
            | AmberDeep | AmberTron | MonoTron | Ibm5151 => "monitors",
            Turbo | Procomm | C64 | Amiga | Delphi | Mirc | SolarizedDark | SolarizedLight => "classic",
            Monokai | Dracula | Nord | Gruvbox | Catppuccin => "nouveau",
        }
    }
}

/// The theme in use: a built-in one, or a theme file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Builtin(ThemeName),
    /// `~/.textrix/themes/<name>.toml`.
    File(String),
}

impl Default for ThemeId {
    fn default() -> Self {
        ThemeId::Builtin(ThemeName::Default)
    }
}

impl From<ThemeName> for ThemeId {
    fn from(name: ThemeName) -> Self {
        ThemeId::Builtin(name)
    }
}

impl ThemeId {
    /// A built-in theme, or else a theme file's name (letters, digits, `-`
    /// and `_`, so it can't point anywhere but the themes folder).
    pub fn parse(name: &str) -> Result<Self> {
        if let Some(builtin) = ThemeName::parse(name) {
            return Ok(ThemeId::Builtin(builtin));
        }
        file_name_ok(name)?;
        Ok(ThemeId::File(name.to_owned()))
    }

    pub fn name(&self) -> &str {
        match self {
            ThemeId::Builtin(name) => name.name(),
            ThemeId::File(name) => name,
        }
    }
}

fn file_name_ok(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.starts_with('-');
    if !ok {
        bail!("“{name}” isn't a theme name (letters, digits, - and _, up to 40)");
    }
    Ok(())
}

impl fmt::Display for ThemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for ThemeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for ThemeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        ThemeId::parse(&name).map_err(serde::de::Error::custom)
    }
}

/// How panels are framed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outlines {
    /// No frames: a line between the panels.
    #[default]
    None,
    Plain,
    Rounded,
    Double,
    Thick,
    /// `+--+` and `|`.
    Ascii,
}

impl Outlines {
    pub fn framed(self) -> bool {
        self != Outlines::None
    }

    /// The frame's lines and corners.
    pub fn set(self) -> Option<border::Set<'static>> {
        Some(match self {
            Outlines::None => return None,
            Outlines::Plain => border::PLAIN,
            Outlines::Rounded => border::ROUNDED,
            Outlines::Double => border::DOUBLE,
            Outlines::Thick => border::THICK,
            Outlines::Ascii => border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            },
        })
    }
}

/// A theme file, `~/.textrix/themes/<name>.toml`: a built-in theme to
/// start from and the colours that differ.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    /// What `/theme import` calls it (otherwise the file's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The built-in theme it starts from ("default" if not given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outlines: Option<Outlines>,
    /// Show the colours senders put in their messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_colors: Option<bool>,
    /// Like config.toml's `[colors]`, plus `background` and `foreground`.
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

/// Theme files are small; anything bigger isn't one.
const FILE_MAX: u64 = 64 * 1024;

impl ThemeFile {
    pub fn path(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.toml"))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
        if size > FILE_MAX {
            bail!("{} is too big for a theme ({size} bytes)", path.display());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: Self = toml::from_str(&text).with_context(|| format!("in {}", path.display()))?;
        file.theme().with_context(|| format!("in {}", path.display()))?;
        Ok(file)
    }

    /// The built-in theme it starts from.
    pub fn base(&self) -> Result<ThemeName> {
        match self.base.as_deref() {
            None => Ok(ThemeName::Default),
            Some(name) => ThemeName::parse(name)
                .with_context(|| format!("base: “{name}” isn't a built-in theme (they're listed by /theme)")),
        }
    }

    pub fn theme(&self) -> Result<Theme> {
        let mut theme = Theme::builtin(self.base()?);
        if let Some(outlines) = self.outlines {
            theme.outlines = outlines;
        }
        if let Some(on) = self.message_colors {
            theme.message_colors = on;
        }
        for (key, value) in &self.colors {
            theme.set(key, value)?;
        }
        Ok(theme)
    }

    /// The file as TOML, with a header saying what it is.
    pub fn to_toml(&self, name: &str) -> Result<String> {
        Ok(format!(
            "# A Textrix theme. Save it as ~/.textrix/themes/{name}.toml, or\n\
             # /theme import <this file>; then /theme {name}.\n\n{}",
            toml::to_string_pretty(self)?
        ))
    }

    /// The theme in use as a file: its base, outlines and every colour
    /// changed from it.
    pub fn of(id: &ThemeId, overrides: &BTreeMap<String, String>, dir: &Path) -> Result<Self> {
        let mut file = match id {
            ThemeId::Builtin(name) => ThemeFile { base: Some(name.name().to_owned()), ..ThemeFile::default() },
            ThemeId::File(name) => ThemeFile::read(&ThemeFile::path(dir, name))?,
        };
        file.name = None;
        file.colors.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(file)
    }
}

/// The theme files in `dir`, by name.
pub fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_stem()?.to_str()?.to_owned();
            (path.extension()? == "toml" && file_name_ok(&name).is_ok() && ThemeName::parse(&name).is_none())
                .then_some(name)
        })
        .collect();
    names.sort();
    names
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
    /// Syntax colours in code blocks, by [`Syntax`] (laid over `code`).
    pub syntax: [Style; 6],
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
    pub outlines: Outlines,
    /// The frames' lines, and the focused pane's.
    pub border: Style,
    pub border_focus: Style,
}

/// A dark blue (#005f87) that white text reads well on.
const BAR_BG: Color = Color::Indexed(24);

/// The deep blue behind the airport-terminal themes (#00005f).
const DEEP_BLUE: Color = Color::Indexed(17);

/// Black as black gets (#000000), behind the Tron themes. Not the
/// terminal's own "black", which colour schemes often make a dark grey:
/// the 256-colour cube is left alone.
const PITCH_BLACK: Color = Color::Indexed(16);

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
const AMBER: Hue = Hue { shades: [214, 172, 136, 94], hot: 229, code: 220, nicks: [214, 208, 220, 178, 215, 221, 179] };
const RED: Hue = Hue { shades: [196, 160, 124, 52], hot: 210, code: 203, nicks: [196, 160, 203, 124, 167, 197, 161] };
/// Greys, with white for what matters.
const GREY: Hue = Hue { shades: [252, 246, 241, 236], hot: 231, code: 255, nicks: [252, 255, 249, 231, 250, 253, 247] };

/// A theme with its own background, as `0xRRGGBB` colours given roles.
struct Palette {
    bg: u32,
    fg: u32,
    /// Timestamps, comments, quiet details.
    dim: u32,
    /// Window-list entries with nothing new.
    muted: u32,
    /// Topic and status bars: text, background.
    bar: [u32; 2],
    /// The brackets on the bars, and warnings there.
    bar_accent: u32,
    bar_alert: u32,
    /// Unfocused panes' title bars: text, background.
    pane: [u32; 2],
    /// The current window in the list: text, background.
    current: [u32; 2],
    /// `-!-` markers, popup titles, the focused frame.
    accent: u32,
    /// A message that mentions you: the nick, the text.
    highlight: u32,
    highlight_text: u32,
    error: u32,
    link: u32,
    mention: u32,
    code: u32,
    /// In code blocks: comments, keywords, strings, numbers, functions,
    /// types.
    syntax: [u32; 6],
    /// Online, voice, read markers.
    good: u32,
    /// Away, ops, pins.
    warn: u32,
    /// Frames and the lines between panels.
    border: u32,
    outlines: Outlines,
    message_colors: bool,
    nicks: &'static [u32],
}

fn rgb(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// The IBM 5151's P39 phosphor, in shades.
const IBM_5151: Palette = Palette {
    bg: 0x000000,
    fg: 0x00ff66,
    dim: 0x00994d,
    muted: 0x00cc52,
    bar: [0x000000, 0x00cc52],
    bar_accent: 0x003d1f,
    bar_alert: 0x000000,
    pane: [0x00ff66, 0x00331a],
    current: [0x000000, 0x00ff66],
    accent: 0x00ff66,
    highlight: 0x99ffc2,
    highlight_text: 0xb3ffd1,
    error: 0xb3ffd1,
    link: 0x00ff66,
    mention: 0x99ffc2,
    code: 0x33ff85,
    syntax: [0x00994d, 0x00ff66, 0x00cc52, 0x99ffc2, 0x00ff66, 0x99ffc2],
    good: 0x00ff66,
    warn: 0x00cc52,
    border: 0x00cc52,
    outlines: Outlines::Ascii,
    message_colors: false,
    nicks: &[0x00ff66, 0x33ff85, 0x66ffa3, 0x00cc52, 0x99ffc2, 0x00e65c],
};

/// CGA's sixteen colours, as Borland's DOS IDEs used them.
const TURBO: Palette = Palette {
    bg: 0x0000aa,
    fg: 0xffff55,
    dim: 0xaaaaaa,
    muted: 0x55ffff,
    bar: [0x000000, 0xaaaaaa],
    bar_accent: 0xaa0000,
    bar_alert: 0xaa0000,
    pane: [0x000000, 0x00aaaa],
    current: [0x000000, 0x00aa00],
    accent: 0xffffff,
    highlight: 0xff5555,
    highlight_text: 0xffffff,
    error: 0xff5555,
    link: 0x55ffff,
    mention: 0x55ff55,
    code: 0xffffff,
    syntax: [0xaaaaaa, 0xffffff, 0x55ffff, 0x55ffff, 0xffff55, 0x55ff55],
    good: 0x55ff55,
    warn: 0xffffff,
    border: 0xaaaaaa,
    outlines: Outlines::Double,
    message_colors: true,
    nicks: &[0x55ffff, 0x55ff55, 0xff55ff, 0xff5555, 0xffffff, 0x00aaaa, 0xaaaaaa],
};

/// Light grey on black with the ANSI colours BBSes drew in, and a cyan
/// status line.
const PROCOMM: Palette = Palette {
    bg: 0x000000,
    fg: 0xaaaaaa,
    dim: 0x555555,
    muted: 0xaaaaaa,
    bar: [0x000000, 0x00aaaa],
    bar_accent: 0xffffff,
    bar_alert: 0xaa0000,
    pane: [0xffffff, 0x0000aa],
    current: [0x000000, 0x55ffff],
    accent: 0x55ffff,
    highlight: 0xffff55,
    highlight_text: 0xffffff,
    error: 0xff5555,
    link: 0x5555ff,
    mention: 0x55ff55,
    code: 0x55ff55,
    syntax: [0x00aaaa, 0xffffff, 0x55ff55, 0xff55ff, 0x55ffff, 0xffff55],
    good: 0x55ff55,
    warn: 0xffff55,
    border: 0x555555,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0x55ffff, 0x55ff55, 0xffff55, 0xff55ff, 0xff5555, 0x5555ff, 0xffffff, 0x00aaaa, 0xaa00aa, 0xaa5500],
};

/// The Commodore 64's blues, a little brighter for reading.
const C64: Palette = Palette {
    bg: 0x40318d,
    fg: 0xa59bff,
    dim: 0x7869c4,
    muted: 0x8f84e8,
    bar: [0xffffff, 0x7869c4],
    bar_accent: 0x40318d,
    bar_alert: 0xedf171,
    pane: [0xc8c2ff, 0x5a4ba8],
    current: [0x40318d, 0xa59bff],
    accent: 0x75cec8,
    highlight: 0xedf171,
    highlight_text: 0xffffff,
    error: 0xff8a8a,
    link: 0x75cec8,
    mention: 0xa9ff9f,
    code: 0xffffff,
    syntax: [0x7869c4, 0xffffff, 0xa9ff9f, 0xedf171, 0x75cec8, 0xe0a060],
    good: 0xa9ff9f,
    warn: 0xedf171,
    border: 0x7869c4,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0x75cec8, 0xa9ff9f, 0xedf171, 0xff8a8a, 0xffffff, 0xd696f0, 0xe0a060],
};

/// Workbench 1.3's four colours: blue, white, black and orange.
const AMIGA: Palette = Palette {
    bg: 0x0055aa,
    fg: 0xffffff,
    dim: 0x88aacc,
    muted: 0xc8dcf0,
    bar: [0x0055aa, 0xffffff],
    bar_accent: 0x000022,
    bar_alert: 0x000022,
    pane: [0xffffff, 0x003c78],
    current: [0x000022, 0xff8800],
    accent: 0xff8800,
    highlight: 0xff8800,
    highlight_text: 0xffbb66,
    error: 0xffaaaa,
    link: 0x99ccff,
    mention: 0xff8800,
    code: 0xffddaa,
    syntax: [0x88aacc, 0xffffff, 0xff8800, 0xffbb66, 0x99ccff, 0xffdd88],
    good: 0xffffff,
    warn: 0xff8800,
    border: 0xffffff,
    outlines: Outlines::Plain,
    message_colors: true,
    nicks: &[0xff8800, 0xffffff, 0xffbb66, 0x99ccff, 0xffdd88, 0x88eedd, 0xff9966],
};

/// Delphi 7: its editor's colours in Windows 2000 grey.
const DELPHI: Palette = Palette {
    bg: 0xffffff,
    fg: 0x000000,
    dim: 0x808080,
    muted: 0x404040,
    bar: [0x000000, 0xd4d0c8],
    bar_accent: 0x000080,
    bar_alert: 0x800000,
    pane: [0xffffff, 0x808080],
    current: [0xffffff, 0x0a246a],
    accent: 0x000080,
    highlight: 0xff0000,
    highlight_text: 0x800000,
    error: 0xff0000,
    link: 0x0000ff,
    mention: 0x000080,
    code: 0x000000,
    syntax: [0x000080, 0x000000, 0x0000ff, 0x0000ff, 0x000000, 0x000000],
    good: 0x008000,
    warn: 0x808000,
    border: 0x808080,
    outlines: Outlines::Plain,
    message_colors: true,
    nicks: &[0x000080, 0x008000, 0x800000, 0x800080, 0x008080, 0x808000, 0x0000ff],
};

/// mIRC's colours on white.
const MIRC: Palette = Palette {
    bg: 0xffffff,
    fg: 0x000000,
    dim: 0x7f7f7f,
    muted: 0x000000,
    bar: [0x000000, 0xd4d0c8],
    bar_accent: 0x00007f,
    bar_alert: 0xff0000,
    pane: [0xffffff, 0x7f7f7f],
    current: [0xffffff, 0x00007f],
    accent: 0x009300,
    highlight: 0xff0000,
    highlight_text: 0x7f0000,
    error: 0xff0000,
    link: 0x0000fc,
    mention: 0x9c009c,
    code: 0x00007f,
    syntax: [0x7f7f7f, 0x00007f, 0x009300, 0xfc7f00, 0x0000fc, 0x009393],
    good: 0x009300,
    warn: 0xfc7f00,
    border: 0xd2d2d2,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0x00007f, 0x009300, 0xff0000, 0x7f0000, 0x9c009c, 0xfc7f00, 0x009393, 0x0000fc, 0xff00ff, 0x7f7f7f],
};

const SOLARIZED_ACCENTS: [u32; 8] = [0xb58900, 0xcb4b16, 0xdc322f, 0xd33682, 0x6c71c4, 0x268bd2, 0x2aa198, 0x859900];

const SOLARIZED_DARK: Palette = Palette {
    bg: 0x002b36,
    fg: 0x839496,
    dim: 0x586e75,
    muted: 0x657b83,
    bar: [0x93a1a1, 0x073642],
    bar_accent: 0x268bd2,
    bar_alert: 0xb58900,
    pane: [0x839496, 0x073642],
    current: [0x002b36, 0x268bd2],
    accent: 0x268bd2,
    highlight: 0xd33682,
    highlight_text: 0xb58900,
    error: 0xdc322f,
    link: 0x268bd2,
    mention: 0x6c71c4,
    code: 0x2aa198,
    syntax: [0x586e75, 0x859900, 0x2aa198, 0xd33682, 0x268bd2, 0xb58900],
    good: 0x859900,
    warn: 0xb58900,
    border: 0x586e75,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &SOLARIZED_ACCENTS,
};

const SOLARIZED_LIGHT: Palette = Palette {
    bg: 0xfdf6e3,
    fg: 0x657b83,
    dim: 0x93a1a1,
    muted: 0x657b83,
    bar: [0x586e75, 0xeee8d5],
    bar_accent: 0x268bd2,
    bar_alert: 0xcb4b16,
    pane: [0x586e75, 0xeee8d5],
    current: [0xfdf6e3, 0x268bd2],
    highlight_text: 0xcb4b16,
    code: 0x6c71c4,
    ..SOLARIZED_DARK
};

const MONOKAI: Palette = Palette {
    bg: 0x272822,
    fg: 0xf8f8f2,
    dim: 0x75715e,
    muted: 0xcfcfc2,
    bar: [0xf8f8f2, 0x49483e],
    bar_accent: 0x66d9ef,
    bar_alert: 0xfd971f,
    pane: [0xa59f85, 0x3e3d32],
    current: [0x272822, 0xa6e22e],
    accent: 0x66d9ef,
    highlight: 0xf92672,
    highlight_text: 0xe6db74,
    error: 0xf92672,
    link: 0x66d9ef,
    mention: 0xae81ff,
    code: 0xe6db74,
    syntax: [0x75715e, 0xf92672, 0xe6db74, 0xae81ff, 0xa6e22e, 0x66d9ef],
    good: 0xa6e22e,
    warn: 0xfd971f,
    border: 0x75715e,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0xf92672, 0xfd971f, 0xe6db74, 0xa6e22e, 0x66d9ef, 0xae81ff],
};

const DRACULA: Palette = Palette {
    bg: 0x282a36,
    fg: 0xf8f8f2,
    dim: 0x6272a4,
    muted: 0xbfbfbf,
    bar: [0xf8f8f2, 0x44475a],
    bar_accent: 0xbd93f9,
    bar_alert: 0xffb86c,
    pane: [0xbfbfbf, 0x343746],
    current: [0x282a36, 0xbd93f9],
    accent: 0xbd93f9,
    highlight: 0xff79c6,
    highlight_text: 0xf1fa8c,
    error: 0xff5555,
    link: 0x8be9fd,
    mention: 0x50fa7b,
    code: 0xf1fa8c,
    syntax: [0x6272a4, 0xff79c6, 0xf1fa8c, 0xbd93f9, 0x50fa7b, 0x8be9fd],
    good: 0x50fa7b,
    warn: 0xffb86c,
    border: 0x6272a4,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0x8be9fd, 0x50fa7b, 0xffb86c, 0xff79c6, 0xbd93f9, 0xff5555, 0xf1fa8c],
};

const NORD: Palette = Palette {
    bg: 0x2e3440,
    fg: 0xd8dee9,
    dim: 0x616e88,
    muted: 0xa3abb9,
    bar: [0xeceff4, 0x434c5e],
    bar_accent: 0x88c0d0,
    bar_alert: 0xebcb8b,
    pane: [0xd8dee9, 0x3b4252],
    current: [0x2e3440, 0x88c0d0],
    accent: 0x88c0d0,
    highlight: 0xb48ead,
    highlight_text: 0xebcb8b,
    error: 0xbf616a,
    link: 0x88c0d0,
    mention: 0x81a1c1,
    code: 0x8fbcbb,
    syntax: [0x616e88, 0x81a1c1, 0xa3be8c, 0xb48ead, 0x88c0d0, 0x8fbcbb],
    good: 0xa3be8c,
    warn: 0xebcb8b,
    border: 0x4c566a,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0x88c0d0, 0x81a1c1, 0x8fbcbb, 0xa3be8c, 0xebcb8b, 0xd08770, 0xbf616a, 0xb48ead],
};

const GRUVBOX: Palette = Palette {
    bg: 0x282828,
    fg: 0xebdbb2,
    dim: 0x928374,
    muted: 0xa89984,
    bar: [0xebdbb2, 0x504945],
    bar_accent: 0xfabd2f,
    bar_alert: 0xfe8019,
    pane: [0xa89984, 0x3c3836],
    current: [0x282828, 0xfabd2f],
    accent: 0x8ec07c,
    highlight: 0xd3869b,
    highlight_text: 0xfabd2f,
    error: 0xfb4934,
    link: 0x83a598,
    mention: 0x8ec07c,
    code: 0xfe8019,
    syntax: [0x928374, 0xfb4934, 0xb8bb26, 0xd3869b, 0xfabd2f, 0x83a598],
    good: 0xb8bb26,
    warn: 0xfabd2f,
    border: 0x665c54,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xfe8019],
};

/// Catppuccin Mocha.
const CATPPUCCIN: Palette = Palette {
    bg: 0x1e1e2e,
    fg: 0xcdd6f4,
    dim: 0x7f849c,
    muted: 0xa6adc8,
    bar: [0xcdd6f4, 0x313244],
    bar_accent: 0xcba6f7,
    bar_alert: 0xfab387,
    pane: [0xa6adc8, 0x181825],
    current: [0x1e1e2e, 0xcba6f7],
    accent: 0xcba6f7,
    highlight: 0xf38ba8,
    highlight_text: 0xf9e2af,
    error: 0xf38ba8,
    link: 0x89b4fa,
    mention: 0xb4befe,
    code: 0xfab387,
    syntax: [0x9399b2, 0xcba6f7, 0xa6e3a1, 0xfab387, 0x89b4fa, 0xf9e2af],
    good: 0xa6e3a1,
    warn: 0xf9e2af,
    border: 0x45475a,
    outlines: Outlines::None,
    message_colors: true,
    nicks: &[
        0xf5e0dc, 0xf2cdcd, 0xf5c2e7, 0xcba6f7, 0xf38ba8, 0xeba0ac, 0xfab387, 0xf9e2af, 0xa6e3a1, 0x94e2d5, 0x89dceb,
        0x74c7ec, 0x89b4fa, 0xb4befe,
    ],
};

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
            syntax: syntax_styles(|kind| match kind {
                Syntax::Comment => Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                Syntax::Keyword => Style::new().fg(Color::LightMagenta),
                Syntax::String => Style::new().fg(Color::LightGreen),
                Syntax::Number => Style::new().fg(Color::LightRed),
                Syntax::Function => Style::new().fg(Color::LightBlue),
                Syntax::Type => Style::new().fg(Color::LightCyan),
            }),
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
            outlines: Outlines::None,
            border: Style::new().fg(Color::DarkGray),
            border_focus: Style::new().fg(Color::Cyan),
        }
    }
}

/// A style for each kind of code, in [`Syntax::ALL`] order.
fn syntax_styles(style: impl Fn(Syntax) -> Style) -> [Style; 6] {
    Syntax::ALL.map(style)
}

impl Theme {
    /// The theme `id` names, from `dir` if it's a theme file, with
    /// config.toml's `[colors]` on top, in colours the terminal has.
    pub fn load(id: &ThemeId, overrides: &BTreeMap<String, String>, dir: &Path) -> Result<Self> {
        let mut theme = Self::resolve(id, overrides, dir)?;
        if !crate::term::info().truecolor {
            theme.limit_to_256();
        }
        Ok(theme)
    }

    /// The same in full colour.
    fn resolve(id: &ThemeId, overrides: &BTreeMap<String, String>, dir: &Path) -> Result<Self> {
        let mut theme = match id {
            ThemeId::Builtin(name) => Self::builtin(*name),
            ThemeId::File(name) => {
                let path = ThemeFile::path(dir, name);
                if !path.exists() {
                    bail!("there's no theme {name} (no {}); /theme lists them", path.display());
                }
                ThemeFile::read(&path)?.theme()?
            }
        };
        for (key, value) in overrides {
            theme.set(key, value)?;
        }
        Ok(theme)
    }

    /// A built-in theme with `[colors]` changes, in full colour.
    #[cfg(test)]
    pub fn build(name: ThemeName, overrides: &BTreeMap<String, String>) -> Result<Self> {
        Self::resolve(&ThemeId::Builtin(name), overrides, Path::new(""))
    }

    pub(crate) fn builtin(name: ThemeName) -> Self {
        match name {
            ThemeName::Default => Self::default(),
            ThemeName::Light => Self::light(),
            ThemeName::Mono => Self::mono(),
            ThemeName::MonoTron => Self::monochrome(GREY, Backdrop::Tron),
            ThemeName::Green => Self::monochrome(GREEN, Backdrop::Black),
            ThemeName::Blue => Self::monochrome(BLUE, Backdrop::Black),
            ThemeName::Red => Self::monochrome(RED, Backdrop::Black),
            ThemeName::GreenDeep => Self::monochrome(GREEN, Backdrop::Deep),
            ThemeName::BlueDeep => Self::monochrome(BLUE_ON_DEEP, Backdrop::Deep),
            ThemeName::RedDeep => Self::monochrome(RED, Backdrop::Deep),
            ThemeName::GreenTron => Self::monochrome(GREEN, Backdrop::Tron),
            ThemeName::BlueTron => Self::monochrome(BLUE, Backdrop::Tron),
            ThemeName::RedTron => Self::monochrome(RED, Backdrop::Tron),
            ThemeName::Amber => Self::monochrome(AMBER, Backdrop::Black),
            ThemeName::AmberDeep => Self::monochrome(AMBER, Backdrop::Deep),
            ThemeName::AmberTron => Self::monochrome(AMBER, Backdrop::Tron),
            ThemeName::Ibm5151 => Self::palette(&IBM_5151),
            ThemeName::Turbo => Self::palette(&TURBO),
            ThemeName::Procomm => Self::palette(&PROCOMM),
            ThemeName::C64 => Self::palette(&C64),
            ThemeName::Amiga => Self::palette(&AMIGA),
            ThemeName::Delphi => Self::palette(&DELPHI),
            ThemeName::Mirc => Self::palette(&MIRC),
            ThemeName::SolarizedDark => Self::palette(&SOLARIZED_DARK),
            ThemeName::SolarizedLight => Self::palette(&SOLARIZED_LIGHT),
            ThemeName::Monokai => Self::palette(&MONOKAI),
            ThemeName::Dracula => Self::palette(&DRACULA),
            ThemeName::Nord => Self::palette(&NORD),
            ThemeName::Gruvbox => Self::palette(&GRUVBOX),
            ThemeName::Catppuccin => Self::palette(&CATPPUCCIN),
        }
    }

    /// For terminals without 24-bit colour: the nearest of the 256.
    fn limit_to_256(&mut self) {
        fn fix(c: &mut Color) {
            if let Color::Rgb(r, g, b) = *c {
                *c = Color::Indexed(crate::picture::xterm256(r, g, b));
            }
        }
        fn fix_style(style: &mut Style) {
            style.fg.as_mut().map(fix);
            style.bg.as_mut().map(fix);
        }
        for style in [
            &mut self.base,
            &mut self.bar,
            &mut self.bar_accent,
            &mut self.bar_alert,
            &mut self.pane,
            &mut self.dim,
            &mut self.accent,
            &mut self.frame,
            &mut self.own,
            &mut self.highlight,
            &mut self.highlight_text,
            &mut self.error,
            &mut self.divider,
            &mut self.link,
            &mut self.mention,
            &mut self.code,
            &mut self.badge,
            &mut self.pinned,
            &mut self.receipts,
            &mut self.matched,
            &mut self.current,
            &mut self.shown,
            &mut self.inactive,
            &mut self.activity_event,
            &mut self.activity,
            &mut self.op,
            &mut self.voice,
            &mut self.border,
            &mut self.border_focus,
        ]
        .into_iter()
        .chain(&mut self.syntax)
        {
            fix_style(style);
        }
        for c in [&mut self.online, &mut self.away, &mut self.offline].into_iter().chain(&mut self.nicks) {
            fix(c);
        }
    }

    /// A theme with its own background, from a [`Palette`].
    fn palette(p: &Palette) -> Self {
        let fg = |c: u32| Style::new().fg(rgb(c));
        let bold = Modifier::BOLD;
        let bar = Style::new().fg(rgb(p.bar[0])).bg(rgb(p.bar[1]));
        Self {
            base: Style::new().fg(rgb(p.fg)).bg(rgb(p.bg)),
            bar,
            bar_accent: bar.fg(rgb(p.bar_accent)),
            bar_alert: bar.fg(rgb(p.bar_alert)).add_modifier(bold),
            pane: Style::new().fg(rgb(p.pane[0])).bg(rgb(p.pane[1])),
            dim: fg(p.dim),
            accent: fg(p.accent).add_modifier(bold),
            frame: fg(p.accent),
            own: fg(p.fg).add_modifier(bold),
            highlight: fg(p.highlight).add_modifier(bold),
            highlight_text: fg(p.highlight_text),
            error: fg(p.error),
            divider: fg(p.error),
            link: fg(p.link).add_modifier(Modifier::UNDERLINED),
            mention: fg(p.mention).add_modifier(bold),
            code: fg(p.code),
            syntax: syntax_styles(|kind| {
                let style = fg(p.syntax[kind as usize]);
                match kind {
                    Syntax::Comment => style.add_modifier(Modifier::ITALIC),
                    Syntax::Keyword => style.add_modifier(bold),
                    _ => style,
                }
            }),
            badge: fg(p.accent),
            pinned: fg(p.warn),
            receipts: fg(p.good),
            matched: fg(p.warn).add_modifier(bold),
            current: Style::new().fg(rgb(p.current[0])).bg(rgb(p.current[1])).add_modifier(bold),
            shown: fg(p.accent).add_modifier(bold),
            inactive: fg(p.muted),
            activity_event: fg(p.accent),
            activity: fg(p.fg).add_modifier(bold),
            op: fg(p.warn).add_modifier(bold),
            voice: fg(p.good),
            online: rgb(p.good),
            away: rgb(p.warn),
            offline: rgb(p.dim),
            nicks: p.nicks.iter().map(|&c| rgb(c)).collect(),
            message_colors: p.message_colors,
            outlines: p.outlines,
            border: fg(p.border),
            border_focus: fg(p.accent).add_modifier(bold),
        }
    }

    /// Darker colours that read on a white background.
    fn light() -> Self {
        let bold = Modifier::BOLD;
        Self {
            dim: Style::new().fg(Color::Indexed(244)),
            highlight_text: Style::new().fg(Color::Indexed(130)),
            mention: Style::new().fg(Color::Blue).add_modifier(bold),
            code: Style::new().fg(Color::Indexed(125)),
            syntax: syntax_styles(|kind| match kind {
                Syntax::Comment => Style::new().fg(Color::Indexed(244)).add_modifier(Modifier::ITALIC),
                Syntax::Keyword => Style::new().fg(Color::Indexed(90)),
                Syntax::String => Style::new().fg(Color::Indexed(28)),
                Syntax::Number => Style::new().fg(Color::Indexed(124)),
                Syntax::Function => Style::new().fg(Color::Indexed(25)),
                Syntax::Type => Style::new().fg(Color::Indexed(30)),
            }),
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
            syntax: syntax_styles(|kind| match kind {
                Syntax::Comment => plain.add_modifier(Modifier::DIM),
                Syntax::Keyword => bold,
                _ => plain,
            }),
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
            outlines: Outlines::None,
            border: plain.add_modifier(Modifier::DIM),
            border_focus: bold,
        }
    }

    /// One colour, like a monochrome monitor, on black, on deep blue with
    /// double-line frames (80s airport terminal), or on pure black with
    /// thin neon outlines (Tron).
    fn monochrome(hue: Hue, backdrop: Backdrop) -> Self {
        let [bright, mid, dim, dark] = hue.shades.map(Color::Indexed);
        let hot = Color::Indexed(hue.hot);
        let (paper, outlines) = match backdrop {
            Backdrop::Black => (Color::Black, Outlines::None),
            Backdrop::Deep => (DEEP_BLUE, Outlines::Double),
            Backdrop::Tron => (PITCH_BLACK, Outlines::Rounded),
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
            // Shades of the one colour: brighter for what matters.
            syntax: syntax_styles(|kind| match kind {
                Syntax::Comment => Style::new().fg(dim).add_modifier(Modifier::ITALIC),
                Syntax::Keyword => g.add_modifier(bold),
                Syntax::String => Style::new().fg(mid),
                Syntax::Number | Syntax::Type => Style::new().fg(hot),
                Syntax::Function => g,
            }),
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
            "background" => self.base = self.base.bg(c),
            "foreground" => self.base = self.base.fg(c),
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
            _ if let Some(kind) = Syntax::ALL.into_iter().find(|k| key.strip_prefix("code_") == Some(k.name())) => {
                let style = &mut self.syntax[kind as usize];
                *style = style.fg(c);
            }
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
            "border" => self.border = self.border.fg(c),
            "border_focus" => self.border_focus = self.border_focus.fg(c),
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
    "background",
    "foreground",
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
    "code_comment",
    "code_keyword",
    "code_string",
    "code_number",
    "code_function",
    "code_type",
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
    "border",
    "border_focus",
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
            ("code_keyword".to_owned(), "214".to_owned()),
        ]);
        let theme = Theme::build(ThemeName::Default, &colors).unwrap();
        assert_eq!(theme.bar.bg, Some(Color::Rgb(0x20, 0x20, 0x40)));
        assert_eq!(theme.bar_alert.bg, theme.bar.bg, "the whole bar changes");
        assert_eq!(theme.link.fg, Some(Color::LightCyan));
        assert!(theme.link.add_modifier.contains(Modifier::UNDERLINED), "attributes are kept");
        assert_eq!(theme.nicks, [Color::Red, Color::Indexed(214), Color::Rgb(0, 255, 0)]);
        assert_eq!(theme.syntax[Syntax::Keyword as usize].fg, Some(Color::Indexed(214)));
        assert_eq!(theme.syntax[Syntax::String as usize], Theme::default().syntax[Syntax::String as usize]);

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
            let parsed: ThemeId =
                toml::from_str::<std::collections::HashMap<String, ThemeId>>(&format!("t = \"{text}\""))
                    .unwrap()
                    .remove("t")
                    .unwrap();
            assert_eq!(parsed, ThemeId::Builtin(name), "{text}");
            let theme = Theme::build(name, &BTreeMap::new()).unwrap();
            assert_eq!(theme.base, Style::new().fg(Color::Indexed(bright)).bg(Color::Black));
            assert!(!theme.message_colors, "senders' colours would break the look");
            let hue = [GREEN, BLUE, RED].into_iter().find(|h| h.shades[0] == bright).unwrap();
            for style in theme.syntax {
                let Some(Color::Indexed(n)) = style.fg else { panic!("{style:?}") };
                assert!(hue.shades.contains(&n) || n == hue.hot, "{text}: code in {n} isn't one of its shades");
            }
        }
    }

    #[test]
    fn deep_and_tron_variants_frame_everything() {
        for name in ThemeName::ALL.map(|(n, _)| n) {
            let theme = Theme::build(ThemeName::parse(name).unwrap(), &BTreeMap::new()).unwrap();
            assert_eq!(ThemeName::parse(name).unwrap().name(), name);
            match name.rsplit_once('-').map(|(_, kind)| kind) {
                Some("deep") => {
                    assert_eq!(theme.outlines, Outlines::Double);
                    assert_eq!(theme.base.bg, Some(DEEP_BLUE));
                }
                Some("tron") => {
                    assert_eq!(theme.outlines, Outlines::Rounded);
                    // #000000 whatever the terminal's colour scheme.
                    assert_eq!(theme.base.bg, Some(Color::Indexed(16)), "{name}");
                    assert!(
                        [theme.bar, theme.current].iter().all(|s| s.fg == Some(Color::Indexed(16))),
                        "{name}: text on the bars is the same black"
                    );
                }
                _ if ThemeName::parse(name).unwrap().group() == "monitors" => {
                    assert_eq!(theme.outlines.framed(), name == "ibm5151", "{name}")
                }
                _ => {}
            }
        }
        assert_eq!(ThemeName::parse("virtualboy"), Some(ThemeName::Red));
        assert!(ThemeName::parse("purple").is_none());
    }

    #[test]
    fn the_ibm_is_framed_in_ascii() {
        let theme = Theme::build(ThemeName::Ibm5151, &BTreeMap::new()).unwrap();
        let set = theme.outlines.set().unwrap();
        assert_eq!([set.top_left, set.horizontal_top, set.vertical_left], ["+", "-", "|"]);
        assert_eq!(Theme::builtin(ThemeName::Ibm5151).base.fg, Some(Color::Rgb(0x00, 0xff, 0x66)));
        assert!(Outlines::None.set().is_none());
    }

    /// WCAG contrast between two colours.
    fn contrast(a: Option<Color>, b: Option<Color>) -> f64 {
        let lum = |c: Option<Color>| {
            let Some(Color::Rgb(r, g, b)) = c else { panic!("{c:?}") };
            let lin = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn classic_and_nouveau_themes_read_well() {
        for (name, id) in ThemeName::ALL.into_iter().filter(|(_, t)| matches!(t.group(), "classic" | "nouveau")) {
            let t = Theme::builtin(id);
            let bg = t.base.bg;
            assert!(contrast(t.base.fg, bg) >= 4.0, "{name}: text");
            for (part, style) in [("bar", t.bar), ("current window", t.current), ("pane title", t.pane)] {
                assert!(contrast(style.fg, style.bg) >= 3.0, "{name}: {part}");
            }
            assert!(contrast(t.dim.fg, bg) >= 2.0, "{name}: dim");
            for (part, style) in [
                ("highlight", t.highlight),
                ("highlight_text", t.highlight_text),
                ("link", t.link),
                ("mention", t.mention),
                ("code", t.code),
                ("accent", t.accent),
                ("error", t.error),
                ("inactive", t.inactive),
            ] {
                assert!(contrast(style.fg, bg) >= 3.0, "{name}: {part}");
            }
            for nick in &t.nicks {
                assert!(contrast(Some(*nick), bg) >= 2.5, "{name}: nick {nick:?}");
            }
        }
    }

    #[test]
    fn without_truecolor_themes_use_the_256() {
        let mut theme = Theme::builtin(ThemeName::Dracula);
        theme.limit_to_256();
        let styles = [theme.base, theme.bar, theme.current, theme.link, theme.border].into_iter().chain(theme.syntax);
        for style in styles {
            assert!(
                !matches!(style.fg, Some(Color::Rgb(..))) && !matches!(style.bg, Some(Color::Rgb(..))),
                "{style:?}"
            );
        }
        assert!(theme.nicks.iter().all(|c| matches!(c, Color::Indexed(_))));
    }

    #[test]
    fn theme_files_start_from_a_builtin() {
        let dir = std::env::temp_dir().join(format!("textrix-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("midnight.toml"),
            "name = \"midnight\"\nbase = \"blue-tron\"\noutlines = \"ascii\"\n\n[colors]\nbackground = \"#000010\"\nnicks = \"#7aa2f7, #bb9af7\"\n",
        )
        .unwrap();
        let id = ThemeId::parse("midnight").unwrap();
        assert_eq!(id, ThemeId::File("midnight".into()));
        let theme = Theme::resolve(&id, &BTreeMap::new(), &dir).unwrap();
        assert_eq!(theme.outlines, Outlines::Ascii);
        assert_eq!(theme.base.bg, Some(Color::Rgb(0, 0, 0x10)));
        assert_eq!(theme.nicks.len(), 2);
        assert!(!theme.message_colors, "the rest is blue-tron's");
        assert_eq!(files(&dir), ["midnight"]);

        // Saved again, it reads back the same, with your [colors] in it.
        let mine = BTreeMap::from([("link".to_owned(), "red".to_owned())]);
        let file = ThemeFile::of(&id, &mine, &dir).unwrap();
        assert_eq!(file.base.as_deref(), Some("blue-tron"));
        assert_eq!(file.colors["link"], "red");
        let text = file.to_toml("night").unwrap();
        assert!(text.starts_with("# A Textrix theme"));
        assert_eq!(toml::from_str::<ThemeFile>(&text).unwrap(), file);

        let broken = |text: &str| {
            std::fs::write(dir.join("broken.toml"), text).unwrap();
            format!(
                "{:#}",
                Theme::load(&ThemeId::File("broken".into()), &BTreeMap::new(), &dir).map(|_| ()).unwrap_err()
            )
        };
        assert!(broken("base = \"purple\"").contains("isn't a built-in theme"));
        assert!(broken("colour = 1").contains("unknown field"));
        assert!(broken("[colors]\nlnk = \"red\"").contains("in "), "says which file");
        assert!(Theme::load(&ThemeId::File("nope".into()), &BTreeMap::new(), &dir).is_err());
        for bad in ["../secrets", "a/b", "", "-x", "x.toml"] {
            assert!(ThemeId::parse(bad).is_err(), "{bad}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mono_tron_is_greys_on_black() {
        let theme = Theme::build(ThemeName::MonoTron, &BTreeMap::new()).unwrap();
        let grey = |c: Option<Color>| matches!(c, None | Some(Color::Indexed(16 | 231..=255)));
        for style in [theme.base, theme.bar, theme.highlight, theme.link, theme.current, theme.code, theme.border_focus]
            .into_iter()
            .chain(theme.syntax)
        {
            assert!(grey(style.fg) && grey(style.bg), "{style:?}");
        }
        assert!(theme.nicks.iter().all(|c| grey(Some(*c))));
        assert!(!theme.message_colors);
    }

    #[test]
    fn mono_uses_no_colours() {
        let theme = Theme::build(ThemeName::Mono, &BTreeMap::new()).unwrap();
        for style in
            [theme.bar, theme.highlight, theme.link, theme.current, theme.activity].into_iter().chain(theme.syntax)
        {
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

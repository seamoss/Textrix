//! Pictures in the terminal: real graphics where the terminal has them
//! (kitty, iTerm2 or sixel, through ratatui-image), and elsewhere pictures
//! drawn in text, each character cell split into a grid of blocks in two
//! colours: sextants (2×3, 🬿), quadrants (2×2, ▚) or half-blocks (1×2, ▀).
//! Images are always shrunk with a proper filter, never by skipping pixels.

use image::{DynamicImage, imageops::FilterType};
use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    style::Color,
    widgets::Widget,
};
use ratatui_image::{
    Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};
use serde::{Deserialize, Serialize};

const FILTER: FilterType = FilterType::CatmullRom;

/// How to draw pictures in a terminal without graphics (`image_text`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextMode {
    /// Sextants where the terminal draws them itself, quadrants elsewhere.
    #[default]
    Auto,
    Sextants,
    Quadrants,
    Halfblocks,
}

impl TextMode {
    /// What `Auto` means in a terminal: xterm.js (VS Code, Claude's desktop
    /// app) and the terminals that draw block characters themselves get
    /// sextants; others quadrants, which every font has.
    pub fn resolve(self, term_program: &str) -> TextMode {
        match self {
            TextMode::Auto => match term_program {
                "vscode" | "claude-desktop" | "ghostty" | "WezTerm" | "kitty" => TextMode::Sextants,
                _ => TextMode::Quadrants,
            },
            mode => mode,
        }
    }

    /// Blocks across and down in a cell.
    fn grid(self) -> (u32, u32) {
        match self {
            TextMode::Sextants | TextMode::Auto => (2, 3),
            TextMode::Quadrants => (2, 2),
            TextMode::Halfblocks => (1, 2),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TextMode::Auto => "auto",
            TextMode::Sextants => "sextants",
            TextMode::Quadrants => "quadrants",
            TextMode::Halfblocks => "half-blocks",
        }
    }
}

/// Draws pictures for one terminal.
#[derive(Clone)]
pub struct Painter {
    pub picker: Picker,
    /// How pictures are drawn when the terminal has no graphics.
    pub text: TextMode,
    /// 24-bit colour; otherwise text pictures use the 256-colour palette.
    pub truecolor: bool,
}

impl Painter {
    pub fn new(picker: Picker, mode: TextMode) -> Self {
        let info = crate::term::info();
        Self { picker, text: mode.resolve(&info.term_program), truecolor: info.truecolor }
    }

    pub fn graphics(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    /// "kitty graphics", or how pictures are drawn in text.
    pub fn describe(&self) -> String {
        match self.picker.protocol_type() {
            ProtocolType::Kitty => "kitty graphics".into(),
            ProtocolType::Iterm2 => "iTerm2 images".into(),
            ProtocolType::Sixel => "sixel".into(),
            ProtocolType::Halfblocks => format!("drawn in text ({})", self.text.name()),
        }
    }

    /// The image at most `area` (in cells), and no bigger than it is.
    pub fn fit(&self, image: DynamicImage, area: Size) -> anyhow::Result<Picture> {
        if self.graphics() {
            return Ok(Picture::Graphics(self.picker.new_protocol(image, area, Resize::Fit(Some(FILTER)))?));
        }
        Ok(Picture::Text(self.text_picture(&image, area, false)))
    }

    /// The image as big as `area` allows (the viewer).
    pub fn scale(&self, image: DynamicImage, area: Size) -> anyhow::Result<Picture> {
        if self.graphics() {
            return Ok(Picture::Graphics(self.picker.new_protocol(image, area, Resize::Scale(Some(FILTER)))?));
        }
        Ok(Picture::Text(self.text_picture(&image, area, true)))
    }

    fn text_picture(&self, image: &DynamicImage, area: Size, grow: bool) -> TextPicture {
        let font = self.picker.font_size();
        let (fw, fh) = (f64::from(font.width.max(1)), f64::from(font.height.max(1)));
        let (iw, ih) = (f64::from(image.width().max(1)), f64::from(image.height().max(1)));
        // How many pixels one cell covers, keeping the picture's shape.
        let fits = (f64::from(area.width) * fw / iw).min(f64::from(area.height) * fh / ih);
        let scale = if grow { fits } else { fits.min(1.0) };
        let cols = ((iw * scale / fw).round() as u16).clamp(1, area.width.max(1));
        let rows = ((ih * scale / fh).round() as u16).clamp(1, area.height.max(1));
        let (gx, gy) = self.text.grid();
        let pixels = image.resize_exact(u32::from(cols) * gx, u32::from(rows) * gy, FILTER).to_rgba8();
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        for row in 0..u32::from(rows) {
            for col in 0..u32::from(cols) {
                let block: Vec<[u8; 4]> = (0..gy)
                    .flat_map(|y| (0..gx).map(move |x| (x, y)))
                    .map(|(x, y)| pixels.get_pixel(col * gx + x, row * gy + y).0)
                    .collect();
                cells.push(self.cell(&block));
            }
        }
        TextPicture { size: Size::new(cols, rows), cells }
    }

    /// One cell: the block pattern that splits its pixels best into two
    /// colours, and those colours (`None`: the terminal's background, for
    /// transparent parts).
    fn cell(&self, pixels: &[[u8; 4]]) -> (char, Option<Color>, Option<Color>) {
        let opaque: Vec<bool> = pixels.iter().map(|p| p[3] >= 128).collect();
        let mean = |pick: &dyn Fn(usize) -> bool| -> Option<[u8; 3]> {
            let chosen: Vec<&[u8; 4]> = pixels.iter().enumerate().filter(|(i, _)| pick(*i)).map(|(_, p)| p).collect();
            if chosen.is_empty() {
                return None;
            }
            let n = chosen.len() as u32;
            Some([0, 1, 2].map(|c| (chosen.iter().map(|p| u32::from(p[c])).sum::<u32>() / n) as u8))
        };
        let mask = if opaque.iter().all(|&o| o) {
            // Split along the colour channel that varies most, then move
            // each pixel to the nearer of the two colours once.
            let channel = (0..3)
                .max_by_key(|&c| {
                    let values = pixels.iter().map(|p| p[c]);
                    values.clone().max().unwrap_or(0) - values.min().unwrap_or(0)
                })
                .unwrap_or(0);
            let (lo, hi) = pixels.iter().fold((255, 0), |(lo, hi), p| (lo.min(p[channel]), hi.max(p[channel])));
            if hi - lo < 6 {
                return (' ', None, mean(&|_| true).map(|c| self.color(c)));
            }
            let mid = (u16::from(lo) + u16::from(hi)) / 2;
            let first: Vec<bool> = pixels.iter().map(|p| u16::from(p[channel]) > mid).collect();
            let split: Vec<bool> = match (mean(&|i| first[i]), mean(&|i| !first[i])) {
                (Some(a), Some(b)) => pixels.iter().map(|p| distance(p, a) <= distance(p, b)).collect(),
                _ => first,
            };
            // Either side can be the foreground: draw the smaller one (on a
            // tie, the one with the top-left block), so the same cell always
            // comes out the same.
            let set = split.iter().filter(|&&on| on).count();
            if set * 2 > split.len() || (set * 2 == split.len() && !split[0]) {
                split.iter().map(|on| !on).collect()
            } else {
                split
            }
        } else {
            opaque.clone()
        };
        let fg = mean(&|i| mask[i] && opaque[i]).map(|c| self.color(c));
        let bg = mean(&|i| !mask[i] && opaque[i]).map(|c| self.color(c));
        let bits = mask.iter().enumerate().fold(0u8, |bits, (i, &on)| bits | (u8::from(on) << i));
        (self.glyph(bits), fg, bg)
    }

    fn glyph(&self, bits: u8) -> char {
        const QUADRANTS: [char; 16] = [' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█'];
        const HALVES: [char; 4] = [' ', '▀', '▄', '█'];
        match self.text {
            TextMode::Quadrants => QUADRANTS[usize::from(bits & 15)],
            TextMode::Halfblocks => HALVES[usize::from(bits & 3)],
            TextMode::Sextants | TextMode::Auto => match bits & 63 {
                0 => ' ',
                21 => '▌',
                42 => '▐',
                63 => '█',
                // U+1FB00 onwards, in counting order, without the four above.
                b => char::from_u32(0x1FB00 + u32::from(b) - 1 - u32::from(b > 21) - u32::from(b > 42)).unwrap_or('█'),
            },
        }
    }

    fn color(&self, [r, g, b]: [u8; 3]) -> Color {
        if self.truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(xterm256(r, g, b)) }
    }
}

fn distance(p: &[u8; 4], c: [u8; 3]) -> u32 {
    (0..3).map(|i| (i32::from(p[i]) - i32::from(c[i])).pow(2) as u32).sum()
}

/// The nearest colour in the xterm 256-colour palette (its colour cube or
/// grey ramp).
fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| (0..6).min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(v)).abs()).unwrap_or(0);
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = [LEVELS[ri], LEVELS[gi], LEVELS[bi]];
    let grey_level = ((u32::from(r) + u32::from(g) + u32::from(b)) / 3).saturating_sub(3) / 10;
    let grey_index = grey_level.min(23) as u8;
    let grey = 8 + 10 * grey_index;
    let off = |c: [u8; 3]| distance(&[r, g, b, 0], c);
    if off([grey; 3]) < off(cube) { 232 + grey_index } else { 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8 }
}

/// A picture ready to draw: graphics, or text cells.
pub enum Picture {
    Graphics(Protocol),
    Text(TextPicture),
}

pub struct TextPicture {
    size: Size,
    /// Row by row: the character, its colour and the cell's background.
    cells: Vec<(char, Option<Color>, Option<Color>)>,
}

impl Picture {
    /// Its size in cells.
    pub fn size(&self) -> Size {
        match self {
            Picture::Graphics(protocol) => {
                let area = protocol.size();
                Size::new(area.width, area.height)
            }
            Picture::Text(text) => text.size,
        }
    }
}

impl Widget for &Picture {
    fn render(self, area: Rect, buf: &mut Buffer) {
        match self {
            Picture::Graphics(protocol) => Image::new(protocol).render(area, buf),
            Picture::Text(text) => {
                let width = usize::from(text.size.width.max(1));
                for (i, &(c, fg, bg)) in text.cells.iter().enumerate() {
                    let (x, y) = ((i % width) as u16, (i / width) as u16);
                    if x >= area.width || y >= area.height {
                        continue;
                    }
                    if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                        cell.set_char(c).set_fg(fg.unwrap_or(Color::Reset)).set_bg(bg.unwrap_or(Color::Reset));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    fn painter(text: TextMode) -> Painter {
        Painter { picker: Picker::halfblocks(), text, truecolor: true }
    }

    #[test]
    fn cells_split_into_two_colours() {
        let p = painter(TextMode::Sextants);
        let (red, blue) = ([200, 0, 0, 255], [0, 0, 200, 255]);
        // Left column red, right column blue: the left-half block.
        let (c, fg, bg) = p.cell(&[red, blue, red, blue, red, blue]);
        assert_eq!((c, fg, bg), ('▌', Some(Color::Rgb(200, 0, 0)), Some(Color::Rgb(0, 0, 200))));
        // Top-left alone: the first sextant.
        assert_eq!(p.cell(&[red, blue, blue, blue, blue, blue]).0, '\u{1FB00}');
        // All one colour: a plain cell in that colour.
        assert_eq!(p.cell(&[red; 6]), (' ', None, Some(Color::Rgb(200, 0, 0))));
        // Transparent parts show the terminal's background.
        let clear = [0, 0, 0, 0];
        assert_eq!(p.cell(&[red, red, clear, clear, clear, clear]), ('\u{1FB02}', Some(Color::Rgb(200, 0, 0)), None));
        let q = painter(TextMode::Quadrants);
        assert_eq!(q.cell(&[red, blue, blue, red]).0, '▚');
    }

    #[test]
    fn every_sextant_pattern_has_its_own_glyph() {
        let p = painter(TextMode::Sextants);
        let glyphs: std::collections::HashSet<char> = (0..64).map(|bits| p.glyph(bits)).collect();
        assert_eq!(glyphs.len(), 64);
        assert_eq!(p.glyph(62), '\u{1FB3B}', "the last sextant");
    }

    #[test]
    fn text_pictures_keep_their_shape_and_size() {
        let p = painter(TextMode::Sextants);
        let font = p.picker.font_size();
        // An image exactly 20×5 cells big, given room for more.
        let (w, h) = (u32::from(font.width) * 20, u32::from(font.height) * 5);
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba([9, 9, 9, 255])));
        assert_eq!(p.fit(image.clone(), Size::new(80, 40)).unwrap().size(), Size::new(20, 5), "never grown");
        assert_eq!(
            p.fit(image.clone(), Size::new(10, 40)).unwrap().size(),
            Size::new(10, 3),
            "shrunk to fit, in shape"
        );
        assert_eq!(p.scale(image, Size::new(80, 40)).unwrap().size(), Size::new(80, 20), "the viewer fills the room");
    }

    #[test]
    fn auto_picks_by_terminal_and_256_colours_are_close() {
        assert_eq!(TextMode::Auto.resolve("claude-desktop"), TextMode::Sextants);
        assert_eq!(TextMode::Auto.resolve("Apple_Terminal"), TextMode::Quadrants);
        assert_eq!(TextMode::Halfblocks.resolve("vscode"), TextMode::Halfblocks);
        assert_eq!(xterm256(255, 0, 0), 196);
        assert_eq!(xterm256(0, 0, 0), 16);
        assert_eq!(xterm256(128, 128, 128), 244);
    }
}

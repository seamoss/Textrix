//! Pictures in the terminal: real graphics where the terminal has them
//! (kitty, iTerm2 or sixel, through ratatui-image), and elsewhere pictures
//! drawn in text, each character cell split into a grid of blocks in two
//! colours: sextants (2×3, 🬿), quadrants (2×2, ▚) or half-blocks (1×2, ▀).
//! Images are always shrunk with a proper filter, never by skipping pixels.
//! Moving pictures (GIFs, animated WebP and PNG) are a picture a frame,
//! each drawn in turn: the terminal keeps each frame (kitty graphics), so
//! the next one costs a few cells, not the image again.

use std::{
    io::Cursor,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::bail;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, Limits, imageops::FilterType};
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

/// The most frames a moving picture keeps: longer ones keep every second
/// frame (each shown twice as long), then every fourth, and so on.
const MAX_FRAMES: usize = 200;
/// What its frames may take in the terminal, as RGBA: kitty and Ghostty
/// keep ~320 MB of pictures in all, and drop the oldest past that.
const FRAMES_BUDGET: u64 = 48 * 1024 * 1024;
/// What the viewer's frames may take, as they are, in memory.
const ORIGINAL_BUDGET: u64 = 128 * 1024 * 1024;
/// What the viewer's frames may take in the terminal: one picture at a
/// time, so more than a preview's.
pub const VIEWER_BUDGET: u64 = 96 * 1024 * 1024;
/// Frames read at most, however short they are.
const MAX_SOURCE_FRAMES: usize = 1000;
/// The shortest a frame shows. GIFs that ask for less (0 or 10 ms) mean
/// "as fast as you can", which browsers play at 100 ms.
const MIN_DELAY: Duration = Duration::from_millis(20);
const FAST_DELAY: Duration = Duration::from_millis(100);

/// When the soonest moving picture drawn since the last ask next changes.
static NEXT_FRAME: Mutex<Option<Instant>> = Mutex::new(None);

/// When to draw again for the moving pictures just drawn (`None`: there
/// were none, or they've stopped), and forget it: each drawing asks anew.
pub fn next_frame() -> Option<Instant> {
    NEXT_FRAME.lock().map(|mut next| next.take()).unwrap_or(None)
}

fn frame_due(at: Instant) {
    if let Ok(mut next) = NEXT_FRAME.lock() {
        *next = Some(next.map_or(at, |n| n.min(at)));
    }
}

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

/// Ask the terminal how it draws pictures, and whether its background is
/// light (for `theme = "auto"`): one round trip, before anything else reads
/// stdin. A terminal that doesn't answer gets pictures in text.
pub fn query_terminal() -> (Picker, Option<bool>) {
    let options = ratatui_image::picker::cap_parser::QueryStdioOptions {
        terminal_background_color_osc: true,
        ..Default::default()
    };
    match Picker::from_query_stdio_with_options(options) {
        Ok(picker) => {
            let light = picker.capabilities().iter().find_map(|c| match c {
                ratatui_image::picker::Capability::Background(r, g, b) => Some(crate::term::is_light((*r, *g, *b))),
                _ => None,
            });
            (picker, light)
        }
        Err(_) => (Picker::halfblocks(), None),
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

    /// Whether moving pictures move here: with kitty graphics (each frame
    /// sent once) or in text. iTerm2 and sixel would send the whole image
    /// again for every frame, so they show the first.
    pub fn moves(&self) -> bool {
        matches!(self.picker.protocol_type(), ProtocolType::Kitty | ProtocolType::Halfblocks)
    }

    /// A moving picture (a GIF, or an animated WebP or PNG) at most `area`,
    /// every frame fitted alike; one that doesn't move, its only frame.
    /// Off the UI thread: it decodes every frame.
    pub fn moving(&self, bytes: &[u8], area: Size) -> anyhow::Result<Picture> {
        let font = self.picker.font_size();
        let (frames, loops) = read_frames(bytes, FRAMES_BUDGET, |frame| {
            let picture = self.fit(frame, area)?;
            let size = picture.size();
            let cost = u64::from(size.width) * u64::from(font.width) * u64::from(size.height) * u64::from(font.height);
            Ok((picture, cost * 4))
        })?;
        Ok(Picture::play(frames, loops, Instant::now()))
    }

    /// For a moving picture's frames in the viewer: with kitty graphics,
    /// the image as it is (shrunk only to the screen) and the terminal
    /// stretches it over the cells it fits, so a small GIF's frames stay
    /// small; otherwise as `scale` draws it.
    pub fn stretch(&self, image: DynamicImage, area: Size) -> anyhow::Result<Picture> {
        if self.picker.protocol_type() != ProtocolType::Kitty {
            return self.scale(image, area);
        }
        let font = self.picker.font_size();
        let (fw, fh) = (f64::from(font.width.max(1)), f64::from(font.height.max(1)));
        let (iw, ih) = (f64::from(image.width().max(1)), f64::from(image.height().max(1)));
        let fits = (f64::from(area.width) * fw / iw).min(f64::from(area.height) * fh / ih);
        let cols = ((iw * fits / fw).round() as u16).clamp(1, area.width.max(1));
        let rows = ((ih * fits / fh).round() as u16).clamp(1, area.height.max(1));
        let (most_w, most_h) = (u32::from(cols) * u32::from(font.width), u32::from(rows) * u32::from(font.height));
        let image = if image.width() > most_w || image.height() > most_h {
            image.resize(most_w, most_h, FILTER)
        } else {
            image
        };
        let caps = self.picker.capabilities();
        let kitty = ratatui_image::protocol::kitty::Kitty::new(
            image,
            Size::new(cols, rows),
            kitty_id(),
            self.picker.tmux_detected(),
            caps.contains(&ratatui_image::picker::Capability::KittyCompression),
            caps.contains(&ratatui_image::picker::Capability::KittySharedMemory),
        )?;
        Ok(Picture::Graphics(Protocol::Kitty(kitty)))
    }

    /// What `stretch` sends of a `width`×`height` image for `area`, in bytes.
    pub fn stretched_bytes(&self, width: u32, height: u32, area: Size) -> u64 {
        let font = self.picker.font_size();
        let screen = u64::from(area.width) * u64::from(font.width) * u64::from(area.height) * u64::from(font.height);
        (u64::from(width) * u64::from(height)).min(screen) * 4
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

/// Frames, each with how long it shows, and how many times they play
/// (`None`: for ever).
pub type Played<T> = (Vec<(T, Duration)>, Option<u32>);

/// Every second frame, each with the time of the one it drops.
fn halve<T>(frames: Vec<(T, Duration)>) -> Vec<(T, Duration)> {
    let mut out: Vec<(T, Duration)> = Vec::with_capacity(frames.len() / 2 + 1);
    for (i, (frame, delay)) in frames.into_iter().enumerate() {
        match out.last_mut() {
            Some(last) if i % 2 == 1 => last.1 += delay,
            _ => out.push((frame, delay)),
        }
    }
    out
}

/// A moving picture's frames, each made into what `keep` makes of it (and
/// what that costs, in bytes), with how long it shows; and how many times
/// it plays. Past `MAX_FRAMES` or `budget`, every second frame is kept
/// (each shown twice as long), then every fourth, and so on.
fn read_frames<T>(
    bytes: &[u8],
    budget: u64,
    mut keep: impl FnMut(DynamicImage) -> anyhow::Result<(T, u64)>,
) -> anyhow::Result<Played<T>> {
    let (frames, loops) = frames_of(bytes)?;
    let mut kept: Vec<(T, Duration)> = Vec::new();
    // Every `step`th frame is kept; those between add to its time.
    let (mut step, mut seen, mut cost) = (1, 0, 0);
    for frame in frames.take(MAX_SOURCE_FRAMES) {
        let frame = frame?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay = Duration::from_micros(u64::from(numer) * 1000 / u64::from(denom.max(1)));
        let delay = if delay < MIN_DELAY { FAST_DELAY } else { delay };
        if seen % step == 0 {
            let (made, bytes) = keep(DynamicImage::ImageRgba8(frame.into_buffer()))?;
            cost = bytes;
            kept.push((made, delay));
        } else if let Some(last) = kept.last_mut() {
            last.1 += delay;
        }
        seen += 1;
        if kept.len() > MAX_FRAMES || kept.len() as u64 * cost > budget {
            kept = halve(kept);
            step *= 2;
        }
    }
    if kept.is_empty() {
        bail!("no frames");
    }
    Ok((kept, loops))
}

/// A moving picture's frames as they are, for the viewer, and how many
/// times it plays: what it keeps in memory is capped like a preview's.
pub fn original_frames(bytes: &[u8]) -> anyhow::Result<Played<DynamicImage>> {
    read_frames(bytes, ORIGINAL_BUDGET, |frame| {
        let cost = u64::from(frame.width()) * u64::from(frame.height()) * 4;
        Ok((frame, cost))
    })
}

/// A kitty image ID of our own (ratatui-image picks them at random too).
fn kitty_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    (hasher.finish() as u32).max(1)
}

/// A moving picture's frames, and how many times it plays (`None`: for
/// ever). Refused unless it moves, so a still one takes the usual way.
fn frames_of(bytes: &[u8]) -> anyhow::Result<(image::Frames<'_>, Option<u32>)> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let reader = Cursor::new(bytes);
    let loops = |count: image::metadata::LoopCount| match count {
        image::metadata::LoopCount::Infinite => None,
        image::metadata::LoopCount::Finite(n) => Some(n.get()),
    };
    match image::guess_format(bytes)? {
        ImageFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(reader)?;
            decoder.set_limits(limits)?;
            let count = loops(decoder.loop_count());
            Ok((decoder.into_frames(), count))
        }
        ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(reader)?;
            if !decoder.has_animation() {
                bail!("a still WebP");
            }
            decoder.set_limits(limits)?;
            let count = loops(decoder.loop_count());
            Ok((decoder.into_frames(), count))
        }
        ImageFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::with_limits(reader, limits)?;
            if !decoder.is_apng()? {
                bail!("a still PNG");
            }
            let decoder = decoder.apng()?;
            let count = loops(decoder.loop_count());
            Ok((decoder.into_frames(), count))
        }
        format => bail!("{format:?} doesn't move"),
    }
}

fn distance(p: &[u8; 4], c: [u8; 3]) -> u32 {
    (0..3).map(|i| (i32::from(p[i]) - i32::from(c[i])).pow(2) as u32).sum()
}

/// The nearest colour in the xterm 256-colour palette (its colour cube or
/// grey ramp).
pub(crate) fn xterm256(r: u8, g: u8, b: u8) -> u8 {
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

/// A picture ready to draw: graphics, text cells, or frames of either.
pub enum Picture {
    Graphics(Protocol),
    Text(TextPicture),
    Moving(Moving),
}

/// A moving picture: its frames in turn, from when it was made.
pub struct Moving {
    /// Each frame (graphics or text), and how long it shows.
    frames: Vec<(Picture, Duration)>,
    /// The frames' times added up.
    total: Duration,
    /// Plays this many times, then stays on its last frame (`None`: loops).
    loops: Option<u32>,
    start: Instant,
}

impl Moving {
    /// The frame to show `elapsed` in, and when the next one is due
    /// (`None`: it has finished playing).
    fn frame_at(&self, elapsed: Duration) -> (&Picture, Option<Duration>) {
        let last = &self.frames[self.frames.len() - 1].0;
        let total = self.total.as_micros().max(1);
        if self.loops.is_some_and(|n| elapsed.as_micros() >= total * u128::from(n)) {
            return (last, None);
        }
        let mut into = elapsed.as_micros() % total;
        for (picture, delay) in &self.frames {
            let shows = delay.as_micros();
            if into < shows {
                return (picture, Some(Duration::from_micros((shows - into) as u64)));
            }
            into -= shows;
        }
        (last, Some(self.frames[0].1))
    }
}

pub struct TextPicture {
    size: Size,
    /// Row by row: the character, its colour and the cell's background.
    cells: Vec<(char, Option<Color>, Option<Color>)>,
}

impl Picture {
    /// Frames shown in turn from `start`, playing `loops` times (`None`:
    /// for ever); a single one is just that picture.
    pub fn play(mut frames: Vec<(Picture, Duration)>, loops: Option<u32>, start: Instant) -> Picture {
        if frames.len() < 2 {
            return frames
                .pop()
                .map_or(Picture::Text(TextPicture { size: Size::new(0, 0), cells: Vec::new() }), |f| f.0);
        }
        let total = frames.iter().map(|(_, d)| *d).sum();
        Picture::Moving(Moving { frames, total, loops, start })
    }

    /// Its size in cells.
    pub fn size(&self) -> Size {
        match self {
            Picture::Graphics(protocol) => {
                let area = protocol.size();
                Size::new(area.width, area.height)
            }
            Picture::Text(text) => text.size,
            Picture::Moving(moving) => moving.frames[0].0.size(),
        }
    }

    /// Whether it moves.
    pub fn moves(&self) -> bool {
        matches!(self, Picture::Moving(_))
    }
}

impl Widget for &Picture {
    fn render(self, area: Rect, buf: &mut Buffer) {
        match self {
            Picture::Graphics(protocol) => Image::new(protocol).render(area, buf),
            Picture::Moving(moving) => {
                let now = Instant::now();
                let (frame, next) = moving.frame_at(now.saturating_duration_since(moving.start));
                if let Some(next) = next {
                    frame_due(now + next);
                }
                frame.render(area, buf);
            }
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

    /// A GIF of solid frames, each (colour, delay in ms).
    fn gif(frames: &[([u8; 4], u32)], repeat: image::codecs::gif::Repeat) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            encoder.set_repeat(repeat).unwrap();
            encoder
                .encode_frames(frames.iter().map(|&(colour, ms)| {
                    image::Frame::from_parts(
                        RgbaImage::from_pixel(8, 8, Rgba(colour)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(ms, 1),
                    )
                }))
                .unwrap();
        }
        out
    }

    fn moving(picture: Picture) -> Moving {
        match picture {
            Picture::Moving(moving) => moving,
            _ => panic!("still"),
        }
    }

    /// The frame's background colour, for telling solid frames apart.
    fn colour(picture: &Picture) -> Option<Color> {
        match picture {
            Picture::Text(text) => text.cells[0].2,
            _ => None,
        }
    }

    #[test]
    fn gifs_move_frame_by_frame_and_loop() {
        use image::codecs::gif::Repeat;
        let p = painter(TextMode::Sextants);
        let (red, green, blue) = ([200, 0, 0, 255], [0, 200, 0, 255], [0, 0, 200, 255]);
        // 0 ms means "fast": 100 ms, as browsers play it.
        let m =
            moving(p.moving(&gif(&[(red, 50), (green, 0), (blue, 200)], Repeat::Infinite), Size::new(4, 2)).unwrap());
        let ms = Duration::from_millis;
        assert_eq!(m.frames.iter().map(|(_, d)| *d).collect::<Vec<_>>(), [ms(50), ms(100), ms(200)]);
        assert_eq!(m.total, ms(350));
        let at = |t| {
            let (frame, next) = m.frame_at(ms(t));
            (colour(frame), next)
        };
        assert_eq!(at(0), (Some(Color::Rgb(200, 0, 0)), Some(ms(50))));
        assert_eq!(at(60), (Some(Color::Rgb(0, 200, 0)), Some(ms(90))));
        assert_eq!(at(349), (Some(Color::Rgb(0, 0, 200)), Some(ms(1))));
        assert_eq!(at(350 * 3 + 10), (Some(Color::Rgb(200, 0, 0)), Some(ms(40))), "round again");
        assert!(Picture::Moving(m).moves());

        // Twice through, then the last frame for good.
        let twice = moving(p.moving(&gif(&[(red, 100), (blue, 100)], Repeat::Finite(2)), Size::new(4, 2)).unwrap());
        assert_eq!(twice.loops, Some(2));
        assert_eq!(colour(twice.frame_at(ms(250)).0), Some(Color::Rgb(200, 0, 0)));
        let (last, next) = twice.frame_at(ms(400));
        assert_eq!((colour(last), next), (Some(Color::Rgb(0, 0, 200)), None));
    }

    #[test]
    fn long_gifs_keep_fewer_frames_for_as_long() {
        let p = painter(TextMode::Sextants);
        let frames: Vec<([u8; 4], u32)> = (0..450).map(|i| ([(i % 250) as u8, 0, 0, 255], 40)).collect();
        let m = moving(p.moving(&gif(&frames, image::codecs::gif::Repeat::Infinite), Size::new(4, 2)).unwrap());
        assert!(m.frames.len() <= MAX_FRAMES, "{}", m.frames.len());
        assert!(m.frames.len() > MAX_FRAMES / 2, "{}", m.frames.len());
        assert_eq!(m.total, Duration::from_millis(450 * 40), "the same length");
        // Every fourth frame, each standing for four.
        assert_eq!(m.frames[1].1, Duration::from_millis(160));
    }

    #[test]
    fn still_pictures_take_the_usual_way() {
        let p = painter(TextMode::Sextants);
        let one = gif(&[([9, 9, 9, 255], 100)], image::codecs::gif::Repeat::Infinite);
        assert!(!p.moving(&one, Size::new(4, 2)).unwrap().moves(), "one frame: still");
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        assert!(p.moving(&png, Size::new(4, 2)).is_err(), "a still PNG is a thumbnail's job");
        assert!(p.moves(), "text pictures move");
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

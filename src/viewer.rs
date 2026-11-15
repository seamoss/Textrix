//! The full-screen image viewer (`z` in selection mode, or a click on an
//! image): the original image, decoded in memory, zoomed and moved around.
//! A moving one keeps moving, zoomed or not.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::picture::{Painter, Picture};
use image::DynamicImage;
use matrix_sdk::ruma::OwnedEventId;
use ratatui::layout::Size;

pub const ZOOM_MAX: u32 = 16;

pub struct Viewer {
    pub event: OwnedEventId,
    /// "nick · file name".
    pub title: String,
    pub image: Loading,
    /// 1 shows the whole image; each doubling shows half as much of it.
    pub zoom: u32,
    /// The point of the image at the centre of the view, as fractions of
    /// its width and height.
    pub center: (f64, f64),
    /// Cells the image is drawn into (set while drawing).
    pub area: Size,
    /// The view as last drawn, and the one being prepared.
    pub frame: Option<(View, Picture)>,
    pub pending: Option<View>,
}

pub enum Loading {
    Waiting,
    Ready(Arc<Original>),
    Failed(String),
}

/// The image as sent: one picture, or a moving one's frames.
pub struct Original {
    /// Each frame and how long it shows (one frame: a still picture).
    pub frames: Vec<(DynamicImage, Duration)>,
    /// Plays this many times (`None`: for ever).
    pub loops: Option<u32>,
    /// When it started playing, so zooming doesn't start it again.
    pub start: Instant,
}

impl Original {
    pub fn still(image: DynamicImage) -> Self {
        Self { frames: vec![(image, Duration::ZERO)], loops: None, start: Instant::now() }
    }
}

/// What a frame shows: enough to tell whether it needs drawing again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: u32,
    pub center: (f64, f64),
    pub area: Size,
}

impl Viewer {
    pub fn new(event: OwnedEventId, title: String) -> Self {
        Self {
            event,
            title,
            image: Loading::Waiting,
            zoom: 1,
            center: (0.5, 0.5),
            area: Size::new(0, 0),
            frame: None,
            pending: None,
        }
    }

    pub fn view(&self) -> View {
        View { zoom: self.zoom, center: self.center, area: self.area }
    }

    pub fn zoom_by(&mut self, step: i32) {
        self.zoom = match step {
            s if s > 0 => (self.zoom * 2).min(ZOOM_MAX),
            s if s < 0 => (self.zoom / 2).max(1),
            _ => 1,
        };
        if self.zoom == 1 {
            self.center = (0.5, 0.5);
        }
        self.clamp();
    }

    /// Move by a quarter of what's visible.
    pub fn pan(&mut self, dx: f64, dy: f64) {
        let step = 0.25 / f64::from(self.zoom);
        self.center = (self.center.0 + dx * step, self.center.1 + dy * step);
        self.clamp();
    }

    /// Keep the visible part inside the image.
    fn clamp(&mut self) {
        let half = 0.5 / f64::from(self.zoom);
        self.center = (self.center.0.clamp(half, 1.0 - half), self.center.1.clamp(half, 1.0 - half));
    }

    /// The next view to prepare: when the image is here, what's drawn is
    /// out of date, and no other is being prepared (one at a time, so
    /// quick key presses don't pile up).
    pub fn wanted(&self) -> Option<(Arc<Original>, View)> {
        let Loading::Ready(image) = &self.image else { return None };
        let view = self.view();
        let drawn = self.frame.as_ref().map(|(v, _)| *v);
        (self.pending.is_none() && view.area.width > 0 && drawn != Some(view)).then(|| (image.clone(), view))
    }
}

/// The part of the image a view shows, in pixels: (x, y, width, height).
pub fn crop(width: u32, height: u32, view: &View) -> (u32, u32, u32, u32) {
    let zoom = f64::from(view.zoom);
    let (w, h) = (f64::from(width), f64::from(height));
    let (cw, ch) = ((w / zoom).max(1.0), (h / zoom).max(1.0));
    let x = (view.center.0 * w - cw / 2.0).clamp(0.0, w - cw);
    let y = (view.center.1 * h - ch / 2.0).clamp(0.0, h - ch);
    (x as u32, y as u32, cw as u32, ch as u32)
}

/// Draw a view for the terminal (slow for big images: run it off the UI
/// thread). Every frame of a moving one, where pictures move; as many as
/// fit in what the terminal keeps (every second one, or third…, if not).
pub fn render(original: &Original, view: &View, painter: &Painter) -> anyhow::Result<Picture> {
    let first = &original.frames[0].0;
    let (x, y, w, h) = crop(first.width(), first.height(), view);
    if original.frames.len() < 2 || !painter.moves() {
        return painter.scale(first.crop_imm(x, y, w, h), view.area);
    }
    let all = painter.stretched_bytes(w, h, view.area) * original.frames.len() as u64;
    let every = all.div_ceil(crate::picture::VIEWER_BUDGET).max(1) as usize;
    let mut frames = Vec::new();
    for run in original.frames.chunks(every) {
        let shows = run.iter().map(|(_, d)| *d).sum();
        frames.push((painter.stretch(run[0].0.crop_imm(x, y, w, h), view.area)?, shows));
    }
    Ok(Picture::play(frames, original.loops, original.start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewer() -> Viewer {
        let mut v = Viewer::new("$1".try_into().unwrap(), "x".into());
        v.area = Size::new(80, 24);
        v
    }

    #[test]
    fn zooming_and_panning_stay_inside_the_image() {
        let mut v = viewer();
        v.pan(1.0, 0.0);
        assert_eq!(v.center, (0.5, 0.5), "nothing to move at full view");
        v.zoom_by(1);
        assert_eq!(v.zoom, 2);
        for _ in 0..10 {
            v.pan(1.0, 1.0);
        }
        assert_eq!(v.center, (0.75, 0.75), "stops at the bottom right corner");
        assert_eq!(crop(1000, 800, &v.view()), (500, 400, 500, 400));
        for _ in 0..10 {
            v.zoom_by(1);
        }
        assert_eq!(v.zoom, ZOOM_MAX);
        v.zoom_by(0);
        assert_eq!((v.zoom, v.center), (1, (0.5, 0.5)), "0 resets");
        assert_eq!(crop(1000, 800, &v.view()), (0, 0, 1000, 800));
    }

    fn halfblocks() -> Painter {
        Painter::new(ratatui_image::picker::Picker::halfblocks(), crate::picture::TextMode::Halfblocks)
    }

    #[test]
    fn moving_pictures_keep_moving_in_the_viewer() {
        use image::{Rgba, RgbaImage};
        use ratatui_image::picker::{Picker, ProtocolType};
        let frame =
            |c: [u8; 4]| (DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 18, Rgba(c))), Duration::from_millis(100));
        let original = Original {
            frames: vec![frame([200, 0, 0, 255]), frame([0, 0, 200, 255])],
            loops: None,
            start: Instant::now(),
        };
        let mut v = viewer();
        assert!(render(&original, &v.view(), &halfblocks()).unwrap().moves(), "drawn in text");
        v.zoom_by(1);
        assert!(render(&original, &v.view(), &halfblocks()).unwrap().moves(), "zoomed in too");

        // With kitty graphics the frames go as they are, and the terminal
        // stretches them over the screen.
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let kitty = Painter::new(picker.clone(), crate::picture::TextMode::Auto);
        let shown = render(&original, &viewer().view(), &kitty).unwrap();
        let size = shown.size();
        assert!(shown.moves());
        assert!(size.width <= 80 && size.height <= 24 && (size.width == 80 || size.height == 24), "{size:?}");
        let font = picker.font_size();
        assert!(u32::from(size.width) * u32::from(font.width) > 32, "far bigger than the GIF");
        assert_eq!(kitty.stretched_bytes(32, 18, Size::new(80, 24)), 32 * 18 * 4, "sent at its own size");

        // iTerm2 would send each frame whole: the first one, still.
        picker.set_protocol_type(ProtocolType::Iterm2);
        let iterm = Painter::new(picker, crate::picture::TextMode::Auto);
        assert!(!render(&original, &viewer().view(), &iterm).unwrap().moves());
    }

    #[test]
    fn redraws_only_when_the_view_changes() {
        let mut v = viewer();
        assert!(v.wanted().is_none(), "nothing until the image arrives");
        v.image = Loading::Ready(Arc::new(Original::still(DynamicImage::new_rgb8(10, 10))));
        let (image, view) = v.wanted().unwrap();
        v.pending = Some(view);
        v.zoom_by(1);
        assert!(v.wanted().is_none(), "one at a time: wait for the frame being drawn");
        let protocol = render(&image, &view, &halfblocks()).unwrap();
        (v.pending, v.frame) = (None, Some((view, protocol)));
        assert_eq!(v.wanted().map(|(_, next)| next.zoom), Some(2), "then catch up with the zoom");
        v.pending = Some(v.view());
        let protocol = render(&image, &v.view(), &halfblocks()).unwrap();
        (v.pending, v.frame) = (None, Some((v.view(), protocol)));
        assert!(v.wanted().is_none(), "up to date");
    }
}

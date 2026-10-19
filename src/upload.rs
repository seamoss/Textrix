//! What goes along with an image or video you send, as Element sends it: its
//! size, a blurhash (the blurred placeholder clients show while it loads),
//! and a thumbnail, so other clients (phones especially) show it without
//! fetching the whole file. Videos get a poster frame from the system
//! (Quick Look on macOS, ffmpeg elsewhere if it's there), and their length
//! and size from the MP4 header.

use std::{path::Path, time::Duration};

use image::{DynamicImage, GenericImageView, imageops::FilterType};

/// Thumbnails fit in this, like Element's.
const THUMB_MAX: (u32, u32) = (800, 600);

#[derive(Default)]
pub struct Extras {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<Duration>,
    pub blurhash: Option<String>,
    /// (bytes, MIME type, width, height).
    pub thumbnail: Option<(Vec<u8>, mime::Mime, u32, u32)>,
}

/// An image's size, blurhash and (for big ones) thumbnail. An image that
/// won't decode is still sent, just without them.
pub fn image_extras(data: &[u8]) -> Extras {
    let Ok(image) = crate::matrix::decode_image(data) else { return Extras::default() };
    let (width, height) = image.dimensions();
    let big = width > THUMB_MAX.0 || height > THUMB_MAX.1;
    Extras {
        width: Some(width),
        height: Some(height),
        duration: None,
        blurhash: blurhash(&image),
        thumbnail: big.then(|| thumbnail(&image)).flatten(),
    }
}

/// A video's length and size (MP4 and QuickTime files), and a poster frame
/// with its blurhash when the system can make one.
pub async fn video_extras(path: &Path, data: &[u8]) -> Extras {
    let (duration, size) = mp4_info(data);
    let mut extras = Extras { width: size.map(|s| s.0), height: size.map(|s| s.1), duration, ..Extras::default() };
    // An MP4 with no picture has no frame to show (and Quick Look waits
    // forever for one).
    let mp4 = data.get(4..8) == Some(b"ftyp");
    if mp4 && size.is_none() {
        return extras;
    }
    if let Some(poster) = poster_frame(path).await {
        // Quick Look's PNG has an alpha channel a frame doesn't need.
        let poster = DynamicImage::ImageRgb8(poster.to_rgb8());
        extras.blurhash = blurhash(&poster);
        extras.thumbnail = thumbnail(&poster);
    }
    extras
}

fn blurhash(image: &DynamicImage) -> Option<String> {
    let small = image.thumbnail(32, 32).to_rgba8();
    blurhash::encode(4, 3, small.width(), small.height(), small.as_raw()).ok()
}

/// A JPEG thumbnail, or PNG for images with transparency.
fn thumbnail(image: &DynamicImage) -> Option<(Vec<u8>, mime::Mime, u32, u32)> {
    let small = image.resize(THUMB_MAX.0, THUMB_MAX.1, FilterType::CatmullRom);
    let (width, height) = small.dimensions();
    let mut out = std::io::Cursor::new(Vec::new());
    let mime = if small.color().has_alpha() {
        small.write_to(&mut out, image::ImageFormat::Png).ok()?;
        mime::IMAGE_PNG
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&small.to_rgb8()).ok()?;
        mime::IMAGE_JPEG
    };
    Some((out.into_inner(), mime, width, height))
}

/// A frame of the video, from Quick Look on macOS or ffmpeg elsewhere.
async fn poster_frame(path: &Path) -> Option<DynamicImage> {
    let dir = std::env::temp_dir().join(format!(
        "textrix-poster-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()
    ));
    crate::config::create_private_dir(&dir).ok()?;
    let out = dir.join("poster.png");
    let mut command = if cfg!(target_os = "macos") {
        let mut c = tokio::process::Command::new("/usr/bin/qlmanage");
        c.args(["-t", "-s", "1280", "-o"]).arg(&dir).arg(path);
        c
    } else {
        let mut c = tokio::process::Command::new("ffmpeg");
        c.args(["-v", "quiet", "-y", "-i"]).arg(path).args(["-frames:v", "1"]).arg(&out);
        c
    };
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    command.kill_on_drop(true);
    let made = tokio::time::timeout(Duration::from_secs(8), command.status()).await.is_ok_and(|s| s.is_ok());
    // Quick Look names it after the file: <name>.png.
    let file = if cfg!(target_os = "macos") {
        path.file_name().map(|name| dir.join(format!("{}.png", name.to_string_lossy())))
    } else {
        Some(out)
    };
    let poster = made.then(|| file.and_then(|f| image::open(f).ok())).flatten();
    let _ = std::fs::remove_dir_all(&dir);
    poster
}

/// A video's length (from `mvhd`) and picture size (the largest `tkhd`) in
/// an MP4 or QuickTime file.
pub fn mp4_info(data: &[u8]) -> (Option<Duration>, Option<(u32, u32)>) {
    let (mut duration, mut size) = (None, None::<(u32, u32)>);
    let mut boxes = vec![(0, data.len())];
    while let Some((mut at, end)) = boxes.pop() {
        while at + 8 <= end {
            let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            let kind = &data[at + 4..at + 8];
            let (header, len) = match len {
                1 if at + 16 <= end => (16, u64::from_be_bytes(data[at + 8..at + 16].try_into().unwrap()) as usize),
                0 => (8, end - at),
                len => (8, len),
            };
            if len < header || at + len > end {
                break;
            }
            let body = &data[at + header..at + len];
            match kind {
                b"moov" | b"trak" => boxes.push((at + header, at + len)),
                b"mvhd" => duration = mvhd_duration(body),
                b"tkhd" if body.len() >= 8 => {
                    // Width and height close the box, as 16.16 fixed point.
                    let n = body.len();
                    let w = u32::from_be_bytes(body[n - 8..n - 4].try_into().unwrap()) >> 16;
                    let h = u32::from_be_bytes(body[n - 4..n].try_into().unwrap()) >> 16;
                    if w > 0 && h > 0 && size.is_none_or(|(sw, sh)| w * h > sw * sh) {
                        size = Some((w, h));
                    }
                }
                _ => {}
            }
            at += len;
        }
    }
    (duration, size)
}

fn mvhd_duration(body: &[u8]) -> Option<Duration> {
    let int = |at: usize, n: usize| -> Option<u64> {
        let bytes = body.get(at..at + n)?;
        Some(bytes.iter().fold(0u64, |v, b| v << 8 | u64::from(*b)))
    };
    let (scale, length) = match body.first()? {
        1 => (int(20, 4)?, int(24, 8)?),
        _ => (int(12, 4)?, int(16, 4)?),
    };
    (scale > 0).then(|| Duration::from_millis(length * 1000 / scale))
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    #[test]
    fn big_images_get_a_thumbnail_and_all_a_blurhash() {
        let encode = |image: &DynamicImage| {
            let mut out = std::io::Cursor::new(Vec::new());
            image.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let photo = DynamicImage::ImageRgb8(image::RgbImage::from_fn(1600, 1000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let extras = image_extras(&encode(&photo));
        assert_eq!((extras.width, extras.height), (Some(1600), Some(1000)));
        assert!(extras.blurhash.as_ref().is_some_and(|b| b.len() > 6));
        let (data, mime, w, h) = extras.thumbnail.unwrap();
        assert_eq!((mime, w, h), (mime::IMAGE_JPEG, 800, 500), "fits 800×600, in shape");
        assert!(data.starts_with(&[0xFF, 0xD8]), "a JPEG");

        let icon = DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 64, Rgba([1, 2, 3, 0])));
        let extras = image_extras(&encode(&icon));
        assert!(extras.thumbnail.is_none() && extras.blurhash.is_some(), "small ones need no thumbnail");
        assert!(image_extras(b"not an image").width.is_none(), "it's still sent, without extras");
    }

    #[test]
    fn mp4_headers_give_length_and_size() {
        fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&600u32.to_be_bytes()); // timescale
        mvhd[16..20].copy_from_slice(&25_200u32.to_be_bytes()); // 42 s
        let mut tkhd = vec![0u8; 84];
        tkhd[76..80].copy_from_slice(&(1920u32 << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&(1080u32 << 16).to_be_bytes());
        let sound = atom(b"trak", &atom(b"tkhd", &[0u8; 84]));
        let picture = atom(b"trak", &atom(b"tkhd", &tkhd));
        let moov = atom(b"moov", &[atom(b"mvhd", &mvhd), sound, picture].concat());
        let file = [atom(b"ftyp", b"isom"), moov, atom(b"mdat", &[0; 32])].concat();
        assert_eq!(mp4_info(&file), (Some(Duration::from_secs(42)), Some((1920, 1080))));
        assert_eq!(mp4_info(b"\x00\x00\x00\x08junk and more"), (None, None));

        // Two seconds of 640×360 H.264, written by AVFoundation.
        let real = include_bytes!("../testdata/bands.mp4");
        assert_eq!(mp4_info(real), (Some(Duration::from_secs(2)), Some((640, 360))));
    }

    #[tokio::test]
    async fn videos_get_a_poster_frame_where_the_system_makes_one() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/bands.mp4"));
        let extras = video_extras(path, &std::fs::read(path).unwrap()).await;
        assert_eq!((extras.width, extras.height), (Some(640), Some(360)));
        let made = cfg!(target_os = "macos")
            || std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("ffmpeg").is_file()));
        if made {
            let (_, mime, w, h) = extras.thumbnail.expect("a poster frame");
            assert!(mime == mime::IMAGE_JPEG && w <= 800 && h <= 600 && w > h, "{w}×{h}");
            assert!(extras.blurhash.is_some());
        }
        // A sound-only MP4 (no frame to show) comes back straight away.
        let tone = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/tone.opus"));
        let started = std::time::Instant::now();
        let none = video_extras(tone, b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ").await;
        assert!(none.thumbnail.is_none() && started.elapsed() < Duration::from_secs(2));
    }
}

//! Voice messages and other audio: the waveform drawn in text, and playing
//! through the system's own player. Element's voice messages are Ogg Opus,
//! which macOS's afplay (and plain ALSA) can't always read, so those are
//! decoded here, in pure Rust, into a WAV first.

use std::{io::Cursor, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use opus_decoder::OpusDecoder;

const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Opus always decodes at 48 kHz.
const RATE: u32 = 48_000;

/// Longer audio isn't decoded for playing (20 minutes is ~115 MB of stereo).
const MAX_SECONDS: usize = 20 * 60;

/// `▁▃▅▇▅▃▁`: amplitudes (any scale) squeezed into at most `width` bars,
/// relative to the loudest.
pub fn waveform(amplitudes: &[u64], width: usize) -> String {
    let width = width.min(amplitudes.len());
    if width == 0 {
        return String::new();
    }
    let peaks: Vec<u64> = (0..width)
        .map(|i| {
            let (start, end) = (i * amplitudes.len() / width, (i + 1) * amplitudes.len() / width);
            amplitudes[start..end.max(start + 1)].iter().copied().max().unwrap_or(0)
        })
        .collect();
    let loudest = peaks.iter().copied().max().unwrap_or(0).max(1);
    peaks.iter().map(|&p| LEVELS[(p * 7 / loudest) as usize]).collect()
}

/// `0:12`, `3:05`, `1:02:03`.
pub fn duration(d: Duration) -> String {
    let s = (d.as_millis() as u64).div_ceil(1000);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn is_ogg_opus(data: &[u8]) -> bool {
    data.starts_with(b"OggS") && data.windows(8).take(128).any(|w| w == b"OpusHead")
}

/// Decode an Ogg Opus file (RFC 7845, mono or stereo) into a WAV.
pub fn ogg_opus_to_wav(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = ogg::PacketReader::new(Cursor::new(data));
    let head = reader.read_packet()?.context("the file is empty")?.data;
    ensure!(head.starts_with(b"OpusHead") && head.len() >= 19, "not an Opus file");
    let channels = usize::from(head[9]);
    let pre_skip = usize::from(u16::from_le_bytes([head[10], head[11]]));
    let gain = i16::from_le_bytes([head[16], head[17]]);
    ensure!(head[18] == 0 && (1..=2).contains(&channels), "Opus with more than two channels isn't supported");

    let mut decoder = OpusDecoder::new(RATE, channels)?;
    let mut frame = vec![0i16; OpusDecoder::MAX_FRAME_SIZE_48K * channels];
    let mut pcm: Vec<i16> = Vec::new();
    let mut end = 0;
    while let Some(packet) = reader.read_packet()? {
        if packet.data.starts_with(b"OpusTags") {
            continue;
        }
        let n = decoder.decode(&packet.data, &mut frame, false)?;
        pcm.extend_from_slice(&frame[..n * channels]);
        end = packet.absgp_page();
        ensure!(pcm.len() / channels <= MAX_SECONDS * RATE as usize, "it's too long to play here");
    }
    // The last granule position marks the end: decoders pad the final frame.
    let total = usize::try_from(end).unwrap_or(usize::MAX).saturating_sub(pre_skip);
    let start = (pre_skip * channels).min(pcm.len());
    let mut samples = pcm.split_off(start);
    if total > 0 {
        samples.truncate(total * channels);
    }
    if gain != 0 {
        // Q7.8 decibels.
        let factor = 10f32.powf(f32::from(gain) / (20.0 * 256.0));
        for s in &mut samples {
            *s = (f32::from(*s) * factor).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        }
    }
    Ok(wav(&samples, channels as u16, RATE))
}

/// 16-bit PCM in a WAV file.
fn wav(samples: &[i16], channels: u16, rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// The command that plays an audio file (the file goes on the end):
/// `audio_player` from config.toml, or the system's own.
pub fn player(configured: &str) -> Option<Vec<String>> {
    if !configured.trim().is_empty() {
        return Some(configured.split_whitespace().map(ToOwned::to_owned).collect());
    }
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["afplay"]]
    } else {
        &[
            &["pw-play"],
            &["paplay"],
            &["mpv", "--no-video", "--really-quiet"],
            &["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet"],
            &["aplay", "-q"],
        ]
    };
    let found = |program: &str| {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    candidates.iter().find(|c| found(c[0])).map(|c| c.iter().map(|s| (*s).to_owned()).collect())
}

/// The command that plays a video (the file goes on the end):
/// `video_player` from config.toml, a player if there is one, or the app
/// the system opens that kind of file with.
pub fn video_player(configured: &str) -> Option<Vec<String>> {
    if !configured.trim().is_empty() {
        return Some(configured.split_whitespace().map(ToOwned::to_owned).collect());
    }
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["mpv", "--really-quiet"], &["open"]]
    } else {
        &[
            &["mpv", "--really-quiet"],
            &["vlc", "--play-and-exit"],
            &["ffplay", "-autoexit", "-loglevel", "quiet"],
            &["xdg-open"],
        ]
    };
    let found = |program: &str| {
        program == "open" && cfg!(target_os = "macos")
            || std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    candidates.iter().find(|c| found(c[0])).map(|c| c.iter().map(|s| (*s).to_owned()).collect())
}

/// A file that's deleted when dropped: decrypted audio only exists on disk
/// while it plays.
pub struct TempFile(pub PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveforms_and_durations() {
        assert_eq!(waveform(&[0, 512, 1024, 512, 0], 5), "▁▄█▄▁");
        assert_eq!(waveform(&[10, 20, 30, 40, 50, 60, 70, 80], 4), "▂▄▆█", "squeezed, keeping peaks");
        assert_eq!(waveform(&[3, 3], 10), "██", "never more bars than samples");
        assert_eq!(waveform(&[], 10), "");
        assert_eq!(duration(Duration::from_millis(12_300)), "0:13");
        assert_eq!(duration(Duration::from_secs(185)), "3:05");
        assert_eq!(duration(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn opus_decodes_to_wav() {
        // One second of a 440 Hz tone, encoded by macOS's afconvert.
        let opus = include_bytes!("../testdata/tone.opus");
        assert!(is_ogg_opus(opus) && !is_ogg_opus(b"ID3 mp3"));
        let wav = ogg_opus_to_wav(opus).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let samples: Vec<i16> = wav[44..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        assert_eq!(samples.len(), 48_000, "exactly a second, without the encoder's padding");
        let loudest = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((8_000..16_000).contains(&loudest), "the tone's volume survives: {loudest}");
        // A 440 Hz tone crosses zero 880 times a second.
        let crossings = samples.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!((860..=900).contains(&crossings), "it's still 440 Hz: {crossings}");
        assert!(ogg_opus_to_wav(b"OggS not really").is_err());
    }
}

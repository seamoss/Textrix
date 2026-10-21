//! Recording voice messages (`/voice`, Ctrl+R): from the microphone into
//! Ogg Opus, the way Element records them, with the waveform clients draw.
//!
//! On macOS Textrix records through CoreAudio itself and has the system's
//! `afconvert` encode it (into a CAF file, whose Opus packets are moved
//! into Ogg here). Elsewhere the sound system's own recorder does the
//! recording (`pw-record`, `parecord`, `arecord`, or sox), and `opusenc`
//! or ffmpeg the encoding; with neither, it goes as a WAV.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

/// The longest a voice message can be.
pub const MAX: Duration = Duration::from_secs(10 * 60);
/// Shorter than this is a slip of the finger.
const MIN: Duration = Duration::from_millis(500);
/// Bars in the waveform sent along.
const BARS: usize = 100;

/// A recording, ready to send.
#[derive(Debug)]
pub struct Voice {
    pub file: PathBuf,
    pub duration: Duration,
    /// Loudness, 0 to 1, the loudest 1.
    pub waveform: Vec<f32>,
}

/// A recording in progress.
pub struct Recording {
    pub started: Instant,
    dir: PathBuf,
    how: How,
}

enum How {
    /// Textrix itself, through CoreAudio: say when to stop, and get the
    /// samples (mono) and their rate.
    #[cfg(target_os = "macos")]
    Capture { stop: std::sync::mpsc::Sender<()>, done: std::thread::JoinHandle<Result<(Vec<f32>, u32)>> },
    /// A recorder program writing a WAV file, stopped with SIGINT.
    #[cfg(not(target_os = "macos"))]
    Program { child: tokio::process::Child, wav: PathBuf },
}

/// Start recording, with what's to hand. Files go in `dir`.
pub fn start(dir: &Path) -> Result<Recording> {
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    #[cfg(target_os = "macos")]
    let how = {
        let (stop, stopped) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let done = std::thread::spawn(move || capture(stopped, ready));
        // Whether the microphone opened.
        started.recv().context("recording stopped before it started")??;
        How::Capture { stop, done }
    };
    #[cfg(not(target_os = "macos"))]
    let how = {
        let wav = dir.join(format!("voice-{}.wav", std::process::id()));
        let command = recorder(&wav).context(
            "there's nothing to record with: install PipeWire's pw-record, PulseAudio's parecord, ALSA's \
             arecord or sox",
        )?;
        let child = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", command[0]))?;
        How::Program { child, wav }
    };
    Ok(Recording { started: Instant::now(), dir: dir.to_owned(), how })
}

/// Stop recording; with `keep`, make it a voice message.
pub async fn finish(recording: Recording, keep: bool) -> Result<Option<Voice>> {
    let (samples, rate) = match recording.how {
        #[cfg(target_os = "macos")]
        How::Capture { stop, done } => {
            let _ = stop.send(());
            tokio::task::spawn_blocking(move || done.join())
                .await?
                .map_err(|_| anyhow::anyhow!("recording crashed"))??
        }
        #[cfg(not(target_os = "macos"))]
        How::Program { child, wav } => {
            if let Some(pid) = child.id() {
                // SIGINT lets it finish the file properly.
                let _ = tokio::process::Command::new("kill").args(["-INT", &pid.to_string()]).status().await;
            }
            let out = match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
                Ok(out) => out?,
                Err(_) => bail!("the recorder wouldn't stop"),
            };
            let data = std::fs::read(&wav);
            let _ = std::fs::remove_file(&wav);
            if !keep {
                return Ok(None);
            }
            let data = data.map_err(|_| {
                let said = String::from_utf8_lossy(&out.stderr);
                anyhow::anyhow!("the recorder didn't record: {}", said.lines().next().unwrap_or("no reason given"))
            })?;
            read_wav(&data)?
        }
    };
    if !keep {
        return Ok(None);
    }
    let duration = Duration::from_secs_f64(samples.len() as f64 / f64::from(rate.max(1)));
    ensure!(duration >= MIN, "that was too short to send");
    ensure!(
        samples.iter().any(|s| s.abs() > 1e-4),
        "the microphone gave nothing but silence; if it's blocked, allow your terminal to use it (on macOS: \
         System Settings → Privacy & Security → Microphone)"
    );
    let file = encode(&recording.dir, &samples, rate).await?;
    Ok(Some(Voice { file, duration, waveform: waveform(&samples, BARS) }))
}

/// The waveform clients draw: the loudness of `bars` stretches of the
/// sound, 0 to 1, the loudest 1.
pub fn waveform(samples: &[f32], bars: usize) -> Vec<f32> {
    let bars = bars.min(samples.len());
    let rms: Vec<f32> = (0..bars)
        .map(|i| {
            let part = &samples[i * samples.len() / bars..(i + 1) * samples.len() / bars];
            (part.iter().map(|s| s * s).sum::<f32>() / part.len().max(1) as f32).sqrt()
        })
        .collect();
    let loudest = rms.iter().copied().fold(0.0, f32::max);
    if loudest <= 0.0 {
        return vec![0.0; bars];
    }
    rms.iter().map(|r| r / loudest).collect()
}

/// The sound as Ogg Opus, or failing that as a WAV.
async fn encode(dir: &Path, samples: &[f32], rate: u32) -> Result<PathBuf> {
    let stem = format!("voice-{}", std::process::id());
    let wav = dir.join(format!("{stem}.wav"));
    let pcm: Vec<i16> = samples.iter().map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).collect();
    std::fs::write(&wav, crate::audio::wav(&pcm, 1, rate))?;
    let ogg = dir.join(format!("{stem}.ogg"));
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let encoded = if cfg!(target_os = "macos") {
        // afconvert writes Opus into CAF, not Ogg: move it across.
        let caf = dir.join(format!("{stem}.caf"));
        let made = run(&["afconvert", "-f", "caff", "-d", "opus", "-b", "32000", &path(&wav), &path(&caf)]).await;
        let moved = made.and_then(|()| {
            let data = std::fs::read(&caf)?;
            std::fs::write(&ogg, caf_opus_to_ogg(&data)?)?;
            Ok(())
        });
        let _ = std::fs::remove_file(&caf);
        moved
    } else if crate::audio::on_path("opusenc") {
        run(&["opusenc", "--quiet", "--bitrate", "32", &path(&wav), &path(&ogg)]).await
    } else if crate::audio::on_path("ffmpeg") {
        let (wav, ogg) = (path(&wav), path(&ogg));
        run(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", &wav, "-c:a", "libopus", "-b:a", "32k", &ogg])
            .await
    } else {
        Err(anyhow::anyhow!("nothing to encode Opus with"))
    };
    match encoded {
        Ok(()) => {
            let _ = std::fs::remove_file(&wav);
            Ok(ogg)
        }
        Err(e) => {
            tracing::info!("voice message goes as a WAV: {e:#}");
            let _ = std::fs::remove_file(&ogg);
            Ok(wav)
        }
    }
}

async fn run(command: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {}", command[0]))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        bail!("{} failed: {}", command[0], said.lines().next().unwrap_or("no reason given"));
    }
    Ok(())
}

/// The sound system's own recorder, or sox, writing 48 kHz mono to `wav`.
#[cfg(not(target_os = "macos"))]
fn recorder(wav: &Path) -> Option<Vec<String>> {
    let wav = wav.to_string_lossy();
    let candidates: [&[&str]; 4] = [
        &["pw-record", "--rate", "48000", "--channels", "1", "--format", "s16"],
        &["parecord", "--rate=48000", "--channels=1", "--format=s16le", "--file-format=wav"],
        &["arecord", "-q", "-f", "S16_LE", "-r", "48000", "-c", "1", "-t", "wav"],
        &["sox", "-q", "-d", "-c", "1", "-r", "48000", "-b", "16"],
    ];
    let found = candidates.iter().find(|c| crate::audio::on_path(c[0]))?;
    Some(found.iter().map(|s| (*s).to_owned()).chain(std::iter::once(wav.into_owned())).collect())
}

/// Record from the default microphone until told to stop (or for `MAX`),
/// as mono samples and their rate. Says on `ready` whether it started.
#[cfg(target_os = "macos")]
fn capture(stop: std::sync::mpsc::Receiver<()>, ready: std::sync::mpsc::Sender<Result<()>>) -> Result<(Vec<f32>, u32)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{Arc, Mutex};

    let samples = Arc::new(Mutex::new(Vec::new()));
    let opened = (|| -> Result<(cpal::Stream, u32)> {
        let device = cpal::default_host().default_input_device().context("there's no microphone")?;
        let config = device.default_input_config().context("the microphone won't say how it records")?;
        let (rate, channels) = (config.sample_rate(), usize::from(config.channels()));
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => input::<f32>(&device, config.config(), channels, samples.clone()),
            cpal::SampleFormat::I16 => input::<i16>(&device, config.config(), channels, samples.clone()),
            cpal::SampleFormat::I32 => input::<i32>(&device, config.config(), channels, samples.clone()),
            cpal::SampleFormat::U16 => input::<u16>(&device, config.config(), channels, samples.clone()),
            other => bail!("the microphone records {other:?} samples, which Textrix doesn't take"),
        }?;
        stream.play().context("the microphone wouldn't start")?;
        Ok((stream, rate))
    })();
    let (stream, rate) = match opened {
        Ok(opened) => {
            let _ = ready.send(Ok(()));
            opened
        }
        Err(e) => {
            let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
            return Err(e);
        }
    };
    let _ = stop.recv_timeout(MAX);
    drop(stream);
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    Ok((samples, rate))
}

#[cfg(target_os = "macos")]
fn input<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    out: std::sync::Arc<std::sync::Mutex<Vec<f32>>>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::{Sample, traits::DeviceTrait};
    let keep = move |data: &[T], _: &cpal::InputCallbackInfo| {
        let mut out = out.lock().unwrap();
        // Mono: the channels' average.
        out.extend(
            data.chunks(channels.max(1))
                .map(|frame| frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / frame.len() as f32),
        );
    };
    let stream = device
        .build_input_stream::<T, _, _>(config, keep, |e| tracing::warn!("recording: {e}"), None)
        .context("the microphone wouldn't open")?;
    Ok(stream)
}

/// 16-bit PCM from a WAV file, as mono samples and their rate.
#[cfg(any(not(target_os = "macos"), test))]
fn read_wav(data: &[u8]) -> Result<(Vec<f32>, u32)> {
    ensure!(data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WAVE", "not a WAV file");
    let (mut at, mut format) = (12, None);
    while at + 8 <= data.len() {
        let id = &data[at..at + 4];
        let len = u32::from_le_bytes(data[at + 4..at + 8].try_into()?) as usize;
        let body = &data[at + 8..(at + 8 + len).min(data.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let channels = usize::from(u16::from_le_bytes([body[2], body[3]]));
                let rate = u32::from_le_bytes(body[4..8].try_into()?);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                ensure!(bits == 16 && channels > 0, "the recording isn't 16-bit PCM");
                format = Some((channels, rate));
            }
            b"data" => {
                let (channels, rate) = format.context("the WAV says what it is after its sound")?;
                let samples = body
                    .chunks_exact(2 * channels)
                    .map(|frame| {
                        let sum: f32 = frame.as_chunks::<2>().0.iter().map(|s| f32::from(i16::from_le_bytes(*s))).sum();
                        sum / channels as f32 / f32::from(i16::MAX)
                    })
                    .collect();
                return Ok((samples, rate));
            }
            _ => {}
        }
        // A recorder still writing may leave the length unset (0 or ~0).
        at += 8 + if len == 0 || len > data.len() { data.len() } else { len + (len & 1) };
    }
    bail!("the WAV has no sound in it")
}

/// The Opus packets in a CAF file (as `afconvert` writes them), in Ogg as
/// RFC 7845 has them.
fn caf_opus_to_ogg(caf: &[u8]) -> Result<Vec<u8>> {
    ensure!(caf.starts_with(b"caff"), "not a CAF file");
    let (mut channels, mut frames_per_packet, mut sizes, mut audio, mut priming, mut valid) =
        (1, 960, None, None, 0, 0);
    let mut at = 8;
    while at + 12 <= caf.len() {
        let id = &caf[at..at + 4];
        let len = i64::from_be_bytes(caf[at + 4..at + 12].try_into()?);
        let end = if len < 0 { caf.len() } else { (at + 12 + len as usize).min(caf.len()) };
        let body = &caf[at + 12..end];
        match id {
            b"desc" if body.len() >= 32 => {
                ensure!(&body[8..12] == b"opus", "the CAF file isn't Opus");
                frames_per_packet = u32::from_be_bytes(body[20..24].try_into()?);
                channels = u32::from_be_bytes(body[24..28].try_into()?);
            }
            b"pakt" if body.len() >= 24 => {
                let packets = i64::from_be_bytes(body[..8].try_into()?);
                valid = i64::from_be_bytes(body[8..16].try_into()?);
                priming = i32::from_be_bytes(body[16..20].try_into()?);
                // Each packet's size, base 128, high bit for "more".
                let mut table = Vec::new();
                let mut n = 0u64;
                for &b in &body[24..] {
                    n = (n << 7) | u64::from(b & 0x7f);
                    if b & 0x80 == 0 {
                        table.push(n as usize);
                        n = 0;
                    }
                }
                ensure!(table.len() as i64 >= packets, "the CAF file's packet table is short");
                table.truncate(packets as usize);
                sizes = Some(table);
            }
            b"data" if body.len() >= 4 => audio = Some(&body[4..]),
            _ => {}
        }
        at = end;
    }
    let (sizes, audio) =
        (sizes.context("the CAF file has no packet table")?, audio.context("the CAF file has no sound")?);
    ensure!((1..=2).contains(&channels), "Opus with more than two channels isn't sent here");

    let mut out = Vec::new();
    let mut writer = ogg::PacketWriter::new(Cursor::new(&mut out));
    let serial = 0x7465_7874; // "text"
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&u16::try_from(priming).unwrap_or(0).to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    writer.write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0)?;
    let vendor = b"Textrix";
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    writer.write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)?;

    // Granule positions count 48 kHz samples, the priming ones included;
    // the last one says where the sound ends.
    let end = u64::try_from(i64::from(priming) + valid).unwrap_or(0);
    let mut offset = 0;
    for (n, &size) in sizes.iter().enumerate() {
        let packet = audio.get(offset..offset + size).context("the CAF file's sound is cut short")?;
        offset += size;
        let last = n + 1 == sizes.len();
        let granule = if last { end } else { (n as u64 + 1) * u64::from(frames_per_packet) };
        let info = if last { ogg::PacketWriteEndInfo::EndStream } else { ogg::PacketWriteEndInfo::NormalPacket };
        writer.write_packet(packet.to_vec(), serial, info, granule)?;
    }
    drop(writer);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveforms_are_loudness_with_the_loudest_one() {
        let quiet_then_loud: Vec<f32> = (0..400).map(|i| if i < 200 { 0.1 } else { 0.5 }).collect();
        let bars = waveform(&quiet_then_loud, 4);
        assert!(bars.iter().zip([0.2, 0.2, 1.0, 1.0]).all(|(a, b)| (a - b).abs() < 1e-5), "{bars:?}");
        assert_eq!(waveform(&[0.0; 10], 5), [0.0; 5], "silence is flat, not divided by nothing");
        assert_eq!(waveform(&[0.3, 0.3], 100).len(), 2, "never more bars than samples");
    }

    #[test]
    fn afconverts_opus_moves_into_ogg() {
        let caf = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/tone.caf")).unwrap();
        let ogg = caf_opus_to_ogg(&caf).unwrap();
        assert!(crate::audio::is_ogg_opus(&ogg));
        // Textrix's own player decodes it: half a second of 440 Hz.
        let (samples, rate) = read_wav(&crate::audio::ogg_opus_to_wav(&ogg).unwrap()).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(samples.len(), 24_000, "the priming samples skipped, the padding cut");
        let loud = waveform(&samples, 10);
        assert!(loud[5] > 0.9 && loud[0] < loud[5], "it fades in: {loud:?}");
        assert!(caf_opus_to_ogg(b"not caf").is_err());
    }

    /// The system's encoder, from samples to a file to send.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn samples_become_ogg_opus_with_afconvert() {
        let dir = std::env::temp_dir().join(format!("textrix-voice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone: Vec<f32> =
            (0..44_100).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.4).collect();
        let file = encode(&dir, &tone, 44_100).await.unwrap();
        assert_eq!(file.extension().and_then(|e| e.to_str()), Some("ogg"));
        let ogg = std::fs::read(&file).unwrap();
        let (samples, rate) = read_wav(&crate::audio::ogg_opus_to_wav(&ogg).unwrap()).unwrap();
        assert_eq!(rate, 48_000);
        assert!((47_000..49_000).contains(&samples.len()), "a second at 48 kHz: {}", samples.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wavs_read_as_mono() {
        let stereo = crate::audio::wav(&[16384, -16384, 8192, 8192], 2, 44_100);
        let (samples, rate) = read_wav(&stereo).unwrap();
        assert_eq!(rate, 44_100);
        assert_eq!(samples.len(), 2);
        assert!(samples[0].abs() < 1e-6 && (samples[1] - 0.25).abs() < 1e-3);
        assert!(read_wav(b"RIFF....WAVEjunk").is_err());
    }
}

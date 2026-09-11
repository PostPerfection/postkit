//! PCM from a composition's MainSound, played on the picture clock.
//!
//! Grok's player is picture-only. A DCP still names a sound MXF, so this reads
//! it with asdcplib and feeds a stereo downmix to the default output device.
//! Missing sound, a missing device, or a failed stream leaves the picture
//! running; load never fails for audio.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};

use crate::composition_timeline::{SegmentTrim, SoundSegment};

const STEREO_CHANNELS: usize = 2;
const CENTRE_AND_SURROUND: f32 = 0.707;
const HIGH_WATER_FRAMES: usize = 48_000 / 5; // ~200 ms at 48 kHz
const FEED_WAIT: Duration = Duration::from_millis(5);
const FULL_SCALE_I32: f32 = 2147483648.0;

enum Command {
    Load(Vec<SoundReel>, u32),
    Seek(u64),
    SetPlaying(bool),
    Stop,
    Shutdown,
}

struct SoundReel {
    path: PathBuf,
    first_frame: u64,
    frame_count: u64,
    entry_edit_unit: u32,
}

struct Shared {
    playing: AtomicBool,
    sample_rate: AtomicU32,
    buffer: Mutex<VecDeque<f32>>,
}

pub(super) struct Output {
    commands: Sender<Command>,
    feeder: Mutex<Option<JoinHandle<()>>>,
}

impl Output {
    pub(super) fn new() -> Self {
        let (commands, incoming) = mpsc::channel();
        let shared = Arc::new(Shared {
            playing: AtomicBool::new(false),
            sample_rate: AtomicU32::new(48_000),
            buffer: Mutex::new(VecDeque::new()),
        });
        // open the device on the first Load that has reels. picture-only
        // players (and Windows CI, which has no usable WASAPI device) never
        // touch the host; opening it from every GrokPlayer::new AVs there.
        let feeder = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || feed(shared, incoming))
        };
        Output {
            commands,
            feeder: Mutex::new(Some(feeder)),
        }
    }

    pub(super) fn load(&self, segments: &[SoundSegment], fps: f64) {
        let reels = reels_of(segments, fps);
        if reels.is_empty() {
            let _ = self.commands.send(Command::Stop);
            return;
        }
        let rate = 48_000;
        let _ = self.commands.send(Command::Load(reels, rate));
    }

    pub(super) fn seek(&self, frame: u64) {
        let _ = self.commands.send(Command::Seek(frame));
    }

    pub(super) fn set_playing(&self, playing: bool) {
        let _ = self.commands.send(Command::SetPlaying(playing));
    }

    pub(super) fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(feeder) = self.feeder.lock().unwrap().take() {
            let _ = feeder.join();
        }
    }
}

fn reels_of(segments: &[SoundSegment], fps: f64) -> Vec<SoundReel> {
    let mut first_frame = 0u64;
    let mut reels = Vec::new();
    for segment in segments {
        let (entry, stated) = trim_in_frames(segment.trim.as_ref(), fps);
        let duration = match open_layout(&segment.path) {
            Ok(layout) => layout.edit_units.saturating_sub(entry),
            Err(error) => {
                tracing::warn!("preview sound: {error}");
                continue;
            }
        };
        let frames = stated.min(u64::from(duration));
        if frames == 0 {
            continue;
        }
        reels.push(SoundReel {
            path: segment.path.clone(),
            first_frame,
            frame_count: frames,
            entry_edit_unit: entry,
        });
        first_frame += frames;
    }
    reels
}

fn trim_in_frames(trim: Option<&SegmentTrim>, fps: f64) -> (u32, u64) {
    let Some(trim) = trim else {
        return (0, u64::MAX);
    };
    let entry = (trim.start_seconds * fps).round().max(0.0) as u32;
    let frames = trim
        .length_seconds
        .map(|length| (length * fps).round().max(0.0) as u64)
        .unwrap_or(u64::MAX);
    (entry, frames)
}

fn start_stream(shared: Arc<Shared>) -> Option<Stream> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| try_start_stream(shared)))
        .ok()
        .flatten()
}

fn try_start_stream(shared: Arc<Shared>) -> Option<Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let supported = device.default_output_config().ok()?;
    let mut config = supported.config();
    config.channels = STEREO_CHANNELS as u16;
    shared
        .sample_rate
        .store(config.sample_rate.0, Ordering::Release);
    let playing = Arc::clone(&shared);
    let err_fn = |error| tracing::error!("preview sound: {error}");
    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _| write_f32(&playing, data),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            &config,
            move |data: &mut [i16], _| write_i16(&playing, data),
            err_fn,
            None,
        ),
        _ => {
            tracing::warn!(
                "preview sound: output format {:?} is not f32 or i16",
                supported.sample_format()
            );
            return None;
        }
    };
    match stream {
        Ok(stream) => {
            if let Err(error) = stream.play() {
                tracing::warn!("preview sound: {error}");
                return None;
            }
            Some(stream)
        }
        Err(error) => {
            tracing::warn!("preview sound: {error}");
            None
        }
    }
}

fn write_f32(shared: &Shared, dest: &mut [f32]) {
    if !shared.playing.load(Ordering::Acquire) {
        dest.fill(0.0);
        return;
    }
    let mut buffer = shared.buffer.lock().unwrap();
    for sample in dest {
        *sample = buffer.pop_front().unwrap_or(0.0);
    }
}

fn write_i16(shared: &Shared, dest: &mut [i16]) {
    if !shared.playing.load(Ordering::Acquire) {
        dest.fill(0);
        return;
    }
    let mut buffer = shared.buffer.lock().unwrap();
    for sample in dest {
        let value = buffer.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0);
        *sample = (value * f32::from(i16::MAX)).round() as i16;
    }
}

struct Feeder {
    shared: Arc<Shared>,
    reels: Vec<SoundReel>,
    reader: Option<(PathBuf, asdcplib::pcm::MxfReader, AudioLayout)>,
    next_frame: u64,
    channels: u16,
    bits: u16,
    bytes_per_edit_unit: usize,
    stream: Option<Stream>,
}

struct AudioLayout {
    channels: u16,
    bits: u16,
    bytes_per_edit_unit: usize,
    edit_units: u32,
}

fn feed(shared: Arc<Shared>, commands: mpsc::Receiver<Command>) {
    let mut feeder = Feeder {
        shared,
        reels: Vec::new(),
        reader: None,
        next_frame: 0,
        channels: 0,
        bits: 0,
        bytes_per_edit_unit: 0,
        stream: None,
    };
    loop {
        if feeder.shared.playing.load(Ordering::Acquire) {
            feeder.fill();
        }
        match commands.recv_timeout(FEED_WAIT) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::Load(reels, _rate)) => feeder.load(reels),
            Ok(Command::Seek(frame)) => feeder.seek(frame),
            Ok(Command::SetPlaying(playing)) => {
                feeder.shared.playing.store(playing, Ordering::Release);
            }
            Ok(Command::Stop) => feeder.stop(),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Feeder {
    fn load(&mut self, reels: Vec<SoundReel>) {
        self.stop();
        if let Some(first) = reels.first() {
            match open_layout(&first.path) {
                Ok(layout) => {
                    eprintln!(
                        "[preview] sound: {}ch {}-bit from {}",
                        layout.channels,
                        layout.bits,
                        first.path.display()
                    );
                    self.channels = layout.channels;
                    self.bits = layout.bits;
                    self.bytes_per_edit_unit = layout.bytes_per_edit_unit;
                }
                Err(error) => {
                    tracing::warn!("preview sound: {error}");
                    return;
                }
            }
        }
        self.reels = reels;
        self.next_frame = 0;
        if self.stream.is_none() && !self.reels.is_empty() {
            self.stream = start_stream(Arc::clone(&self.shared));
        }
    }

    fn seek(&mut self, frame: u64) {
        self.next_frame = frame;
        self.shared.buffer.lock().unwrap().clear();
        self.reader = None;
    }

    fn stop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.buffer.lock().unwrap().clear();
        self.reels.clear();
        self.reader = None;
        self.next_frame = 0;
    }

    fn fill(&mut self) {
        let high_water = HIGH_WATER_FRAMES * STEREO_CHANNELS;
        while self.shared.buffer.lock().unwrap().len() < high_water {
            if !self.push_edit_unit() {
                break;
            }
        }
    }

    fn push_edit_unit(&mut self) -> bool {
        let Some((path, entry)) = self.location(self.next_frame) else {
            return false;
        };
        if !self.ensure_reader(&path) {
            return false;
        }
        let Some((_, reader, layout)) = self.reader.as_mut() else {
            return false;
        };
        let mut essence = vec![0u8; layout.bytes_per_edit_unit];
        let read = match reader.read_frame(entry, &mut essence, None, None) {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!("preview sound: {error}");
                return false;
            }
        };
        let mut interleaved = Vec::new();
        unpack_pcm(
            &essence[..read],
            layout.channels as usize,
            layout.bits,
            &mut interleaved,
        );
        let mut stereo = Vec::new();
        downmix(&interleaved, layout.channels as usize, &mut stereo);
        self.shared.buffer.lock().unwrap().extend(stereo);
        self.next_frame += 1;
        true
    }

    fn location(&self, frame: u64) -> Option<(PathBuf, u32)> {
        for reel in &self.reels {
            if frame < reel.first_frame {
                continue;
            }
            let into = frame - reel.first_frame;
            if reel.frame_count != u64::MAX && into >= reel.frame_count {
                continue;
            }
            let entry = reel.entry_edit_unit.saturating_add(into as u32);
            return Some((reel.path.clone(), entry));
        }
        None
    }

    fn ensure_reader(&mut self, path: &Path) -> bool {
        if self
            .reader
            .as_ref()
            .is_some_and(|(open, _, _)| open == path)
        {
            return true;
        }
        match open_reader(path) {
            Ok((reader, layout)) => {
                self.channels = layout.channels;
                self.bits = layout.bits;
                self.bytes_per_edit_unit = layout.bytes_per_edit_unit;
                self.reader = Some((path.to_path_buf(), reader, layout));
                true
            }
            Err(error) => {
                tracing::warn!("preview sound: {error}");
                self.reader = None;
                false
            }
        }
    }
}

fn open_layout(path: &Path) -> Result<AudioLayout, String> {
    let (_, layout) = open_reader(path)?;
    Ok(layout)
}

fn open_reader(path: &Path) -> Result<(asdcplib::pcm::MxfReader, AudioLayout), String> {
    let mut reader = asdcplib::pcm::MxfReader::new();
    reader
        .open_read(&path.to_string_lossy())
        .map_err(|error| error.to_string())?;
    let descriptor = reader
        .audio_descriptor()
        .map_err(|error| error.to_string())?;
    if descriptor.channel_count == 0 || descriptor.block_align == 0 {
        return Err(format!("{} names no pcm", path.display()));
    }
    Ok((
        reader,
        AudioLayout {
            channels: descriptor.channel_count as u16,
            bits: descriptor.quantization_bits as u16,
            bytes_per_edit_unit: descriptor.block_align as usize
                * frames_per_edit_unit(&descriptor) as usize,
            edit_units: descriptor.container_duration,
        },
    ))
}

fn frames_per_edit_unit(descriptor: &asdcplib::pcm::AudioDescriptor) -> u32 {
    let sample_rate = descriptor.audio_sampling_rate.numerator.max(0) as u64;
    let edit = descriptor.edit_rate;
    if edit.numerator <= 0 {
        return 1;
    }
    (sample_rate * edit.denominator.max(1) as u64).div_ceil(edit.numerator as u64) as u32
}

fn unpack_pcm(bytes: &[u8], channels: usize, bits: u16, out: &mut Vec<f32>) {
    if channels == 0 {
        return;
    }
    match bits {
        16 => {
            for sample in bytes.as_chunks::<2>().0 {
                out.push(i16::from_le_bytes(*sample) as f32 / f32::from(i16::MAX));
            }
        }
        24 => {
            for sample in bytes.as_chunks::<3>().0 {
                let value = i32::from_le_bytes([0, sample[0], sample[1], sample[2]]) as f32
                    / FULL_SCALE_I32;
                out.push(value);
            }
        }
        32 => {
            for sample in bytes.as_chunks::<4>().0 {
                out.push(i32::from_le_bytes(*sample) as f32 / FULL_SCALE_I32);
            }
        }
        _ => {}
    }
}

fn downmix(src: &[f32], channels: usize, stereo: &mut Vec<f32>) {
    if channels == 0 {
        return;
    }
    let frames = src.len() / channels;
    for frame in 0..frames {
        let ch = |index: usize| src[frame * channels + index];
        let (left, right) = match channels {
            1 => (ch(0), ch(0)),
            2 => (ch(0), ch(1)),
            n if n >= 6 => {
                let centre = CENTRE_AND_SURROUND * ch(2);
                (
                    ch(0) + centre + CENTRE_AND_SURROUND * ch(4),
                    ch(1) + centre + CENTRE_AND_SURROUND * ch(5),
                )
            }
            _ => (ch(0), ch(1.min(channels - 1))),
        };
        stereo.push(left.clamp(-1.0, 1.0));
        stereo.push(right.clamp(-1.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_point_one_puts_centre_on_both_ears() {
        let src = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let mut stereo = Vec::new();
        downmix(&src, 6, &mut stereo);
        assert_eq!(stereo.len(), 2);
        assert!((stereo[0] - CENTRE_AND_SURROUND).abs() < 1e-6);
        assert!((stereo[1] - CENTRE_AND_SURROUND).abs() < 1e-6);
    }

    #[test]
    fn stereo_passes_through() {
        let src = [0.25, -0.5];
        let mut stereo = Vec::new();
        downmix(&src, 2, &mut stereo);
        assert_eq!(stereo, src);
    }

    #[test]
    fn a_whole_file_trim_starts_at_the_first_edit_unit() {
        assert_eq!(trim_in_frames(None, 24.0), (0, u64::MAX));
    }
}

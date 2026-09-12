//! PCM from a composition's MainSound, and the clock the picture follows.
//!
//! Grok's player is picture-only. A DCP still names a sound MXF, so this reads
//! it with asdcplib and feeds a stereo downmix to the default output device.
//! Missing sound, a missing device, or a failed stream leaves the picture
//! running; load never fails for audio.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, Stream};

use crate::composition_timeline::{SegmentTrim, SoundSegment};

const STEREO_CHANNELS: usize = 2;
const CENTRE_AND_SURROUND: f32 = 0.707;
const QUEUED_SOUND_SECONDS: f64 = 0.5;
const FEED_WAIT: Duration = Duration::from_millis(5);
const FULL_SCALE_I32: f32 = 2147483648.0;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;

enum Command {
    Load(Vec<SoundReel>, f64),
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
    stream_live: AtomicBool,
    reels_loaded: AtomicBool,
    device_sample_rate: AtomicU32,
    seek_frame: AtomicU64,
    emitted_sample_frames: AtomicU64,
    buffer: Mutex<VecDeque<f32>>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            playing: AtomicBool::new(false),
            stream_live: AtomicBool::new(false),
            reels_loaded: AtomicBool::new(false),
            device_sample_rate: AtomicU32::new(DEFAULT_SAMPLE_RATE),
            seek_frame: AtomicU64::new(0),
            emitted_sample_frames: AtomicU64::new(0),
            buffer: Mutex::new(VecDeque::new()),
        }
    }

    fn count_emitted(&self, samples: usize) {
        let sample_frames = (samples / STEREO_CHANNELS) as u64;
        self.emitted_sample_frames
            .fetch_add(sample_frames, Ordering::AcqRel);
    }
}

pub(super) struct Output {
    commands: Sender<Command>,
    shared: Arc<Shared>,
    frames_per_second: AtomicU64,
    feeder: Mutex<Option<JoinHandle<()>>>,
}

impl Output {
    pub(super) fn new() -> Self {
        let (commands, incoming) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        // open the device on the first Load that has reels. picture-only
        // players (and Windows CI, which has no usable WASAPI device) never
        // touch the host; opening it from every GrokPlayer::new AVs there.
        let feeder = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || feed(shared, incoming))
        };
        Output {
            commands,
            shared,
            frames_per_second: AtomicU64::new(0.0f64.to_bits()),
            feeder: Mutex::new(Some(feeder)),
        }
    }

    pub(super) fn load(&self, segments: &[SoundSegment], fps: f64) {
        let reels = reels_of(segments, fps);
        if reels.is_empty() {
            let _ = self.commands.send(Command::Stop);
            return;
        }
        self.frames_per_second
            .store(fps.to_bits(), Ordering::Release);
        self.mark_seek(0);
        let _ = self.commands.send(Command::Load(reels, fps));
    }

    pub(super) fn seek(&self, frame: u64) {
        self.mark_seek(frame);
        let _ = self.commands.send(Command::Seek(frame));
    }

    // where the device has played to, which is what the picture follows
    pub(super) fn media_position_seconds(&self) -> Option<f64> {
        if !self.shared.stream_live.load(Ordering::Acquire)
            || !self.shared.reels_loaded.load(Ordering::Acquire)
        {
            return None;
        }
        let fps = f64::from_bits(self.frames_per_second.load(Ordering::Acquire));
        let sample_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        if fps <= 0.0 || sample_rate == 0 {
            return None;
        }
        let seek_seconds = self.shared.seek_frame.load(Ordering::Acquire) as f64 / fps;
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire) as f64;
        Some(seek_seconds + played / f64::from(sample_rate))
    }

    fn mark_seek(&self, frame: u64) {
        self.shared
            .emitted_sample_frames
            .store(0, Ordering::Release);
        self.shared.seek_frame.store(frame, Ordering::Release);
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

fn start_stream(shared: Arc<Shared>, pcm_sample_rate: u32) -> Option<Stream> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        try_start_stream(shared, pcm_sample_rate)
    }))
    .ok()
    .flatten()
}

fn try_start_stream(shared: Arc<Shared>, pcm_sample_rate: u32) -> Option<Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let default = device.default_output_config().ok()?;
    let supported = device
        .supported_output_configs()
        .ok()
        .and_then(|ranges| {
            ranges
                .filter(|range| {
                    range.channels() >= STEREO_CHANNELS as u16
                        && matches!(range.sample_format(), SampleFormat::F32 | SampleFormat::I16)
                })
                .find_map(|range| range.try_with_sample_rate(SampleRate(pcm_sample_rate)))
        })
        .unwrap_or(default);
    let mut config = supported.config();
    config.channels = STEREO_CHANNELS as u16;
    shared
        .device_sample_rate
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
    {
        let mut buffer = shared.buffer.lock().unwrap();
        for sample in dest.iter_mut() {
            *sample = buffer.pop_front().unwrap_or(0.0);
        }
    }
    shared.count_emitted(dest.len());
}

fn write_i16(shared: &Shared, dest: &mut [i16]) {
    if !shared.playing.load(Ordering::Acquire) {
        dest.fill(0);
        return;
    }
    {
        let mut buffer = shared.buffer.lock().unwrap();
        for sample in dest.iter_mut() {
            let value = buffer.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0);
            *sample = (value * f32::from(i16::MAX)).round() as i16;
        }
    }
    shared.count_emitted(dest.len());
}

struct Feeder {
    shared: Arc<Shared>,
    reels: Vec<SoundReel>,
    reader: Option<(PathBuf, asdcplib::pcm::MxfReader, AudioLayout)>,
    next_frame: u64,
    seek_frame: Option<u64>,
    fps: f64,
    pcm_sample_rate: u32,
    resampler: Option<Resampler>,
    stream: Option<Stream>,
}

struct AudioLayout {
    channels: u16,
    bits: u16,
    bytes_per_edit_unit: usize,
    edit_units: u32,
    sample_rate: u32,
}

fn feed(shared: Arc<Shared>, commands: mpsc::Receiver<Command>) {
    let mut feeder = Feeder {
        shared,
        reels: Vec::new(),
        reader: None,
        next_frame: 0,
        seek_frame: None,
        fps: 0.0,
        pcm_sample_rate: DEFAULT_SAMPLE_RATE,
        resampler: None,
        stream: None,
    };
    loop {
        feeder.fill();
        match commands.recv_timeout(FEED_WAIT) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::Load(reels, fps)) => feeder.load(reels, fps),
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
    fn load(&mut self, reels: Vec<SoundReel>, fps: f64) {
        self.stop();
        let Some(first) = reels.first() else {
            return;
        };
        let (reader, layout) = match open_reader(&first.path) {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!("preview sound: {error}");
                return;
            }
        };
        eprintln!(
            "[preview] sound: {}ch {}-bit {} Hz from {}",
            layout.channels,
            layout.bits,
            layout.sample_rate,
            first.path.display()
        );
        self.pcm_sample_rate = layout.sample_rate;
        self.reader = Some((first.path.clone(), reader, layout));
        self.fps = fps;
        self.next_frame = 0;
        self.seek_frame = Some(0);
        self.reels = reels;
        self.shared.reels_loaded.store(true, Ordering::Release);
        if self.stream.is_none() {
            self.stream = start_stream(Arc::clone(&self.shared), self.pcm_sample_rate);
            self.shared
                .stream_live
                .store(self.stream.is_some(), Ordering::Release);
        }
        self.reset_resampler();
    }

    fn seek(&mut self, frame: u64) {
        self.next_frame = frame;
        self.seek_frame = Some(frame);
        self.shared.buffer.lock().unwrap().clear();
        self.reset_resampler();
    }

    fn stop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.reels_loaded.store(false, Ordering::Release);
        self.shared.buffer.lock().unwrap().clear();
        self.reels.clear();
        self.reader = None;
        self.next_frame = 0;
        self.seek_frame = None;
    }

    fn reset_resampler(&mut self) {
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        self.resampler =
            (device_rate > 0 && self.pcm_sample_rate > 0 && device_rate != self.pcm_sample_rate)
                .then(|| Resampler::new(self.pcm_sample_rate, device_rate));
    }

    fn fill(&mut self) {
        let Some(seek_frame) = self.seek_frame else {
            return;
        };
        if self.reels.is_empty() {
            return;
        }
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        let high_water = (f64::from(device_rate) * QUEUED_SOUND_SECONDS) as usize * STEREO_CHANNELS;
        loop {
            let queued = self.shared.buffer.lock().unwrap().len();
            if queued >= high_water {
                break;
            }
            self.catch_up(seek_frame, queued, device_rate);
            if !self.push_edit_unit() {
                break;
            }
        }
    }

    fn catch_up(&mut self, seek_frame: u64, queued: usize, device_rate: u32) {
        if self.fps <= 0.0 || device_rate == 0 {
            return;
        }
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire);
        let sample_frames = played + (queued / STEREO_CHANNELS) as u64;
        let reached = frame_at_queue_end(seek_frame, sample_frames, self.fps, device_rate);
        if self.next_frame >= reached {
            return;
        }
        tracing::debug!(
            "preview sound: skipping frames {} to {reached} to keep up with the device",
            self.next_frame
        );
        self.next_frame = reached;
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
        let device_samples = match self.resampler.as_mut() {
            Some(resampler) => {
                let mut resampled = Vec::new();
                resampler.push(&stereo, &mut resampled);
                resampled
            }
            None => stereo,
        };
        self.shared.buffer.lock().unwrap().extend(device_samples);
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

fn frame_at_queue_end(seek_frame: u64, sample_frames: u64, fps: f64, sample_rate: u32) -> u64 {
    if fps <= 0.0 || sample_rate == 0 {
        return seek_frame;
    }
    let played = sample_frames as f64 * fps / f64::from(sample_rate);
    seek_frame + played.floor().max(0.0) as u64
}

struct Resampler {
    step: f64,
    position: f64,
    carry: [f32; STEREO_CHANNELS],
}

impl Resampler {
    fn new(source_rate: u32, device_rate: u32) -> Self {
        Resampler {
            step: f64::from(source_rate) / f64::from(device_rate),
            position: 0.0,
            carry: [0.0; STEREO_CHANNELS],
        }
    }

    fn push(&mut self, stereo: &[f32], out: &mut Vec<f32>) {
        let sample_frames = stereo.len() / STEREO_CHANNELS;
        if sample_frames == 0 {
            return;
        }
        let carry = self.carry;
        // the frame before this block, so a partial step carries across blocks
        let frame = |index: f64| -> [f32; STEREO_CHANNELS] {
            if index < 0.0 {
                return carry;
            }
            let start = index as usize * STEREO_CHANNELS;
            [stereo[start], stereo[start + 1]]
        };
        let last = sample_frames as f64 - 1.0;
        while self.position < last {
            let base = self.position.floor();
            let fraction = (self.position - base) as f32;
            let before = frame(base);
            let after = frame(base + 1.0);
            out.push(before[0] + (after[0] - before[0]) * fraction);
            out.push(before[1] + (after[1] - before[1]) * fraction);
            self.position += self.step;
        }
        self.carry = frame(last);
        self.position -= sample_frames as f64;
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
            sample_rate: sample_rate_of(&descriptor),
        },
    ))
}

fn sample_rate_of(descriptor: &asdcplib::pcm::AudioDescriptor) -> u32 {
    let rate = descriptor.audio_sampling_rate;
    if rate.numerator <= 0 || rate.denominator <= 0 {
        return DEFAULT_SAMPLE_RATE;
    }
    (rate.numerator as u32) / (rate.denominator as u32)
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

    #[test]
    fn the_queue_end_is_where_the_device_will_be() {
        assert_eq!(frame_at_queue_end(100, 0, 24.0, 48_000), 100);
        // one edit unit of 24 fps sound is 2000 sample frames
        assert_eq!(frame_at_queue_end(100, 2_000, 24.0, 48_000), 101);
        assert_eq!(frame_at_queue_end(100, 1_999, 24.0, 48_000), 100);
        assert_eq!(frame_at_queue_end(100, 96_000, 24.0, 48_000), 148);
        assert_eq!(frame_at_queue_end(100, 96_000, 0.0, 48_000), 100);
        assert_eq!(frame_at_queue_end(100, 96_000, 24.0, 0), 100);
    }

    #[test]
    fn silence_on_an_empty_queue_still_counts_as_played() {
        let shared = Shared::new();
        shared.playing.store(true, Ordering::Release);
        let mut dest = [1.0f32; 8];
        write_f32(&shared, &mut dest);
        assert_eq!(dest, [0.0; 8]);
        assert_eq!(shared.emitted_sample_frames.load(Ordering::Acquire), 4);
    }

    #[test]
    fn a_paused_callback_does_not_count() {
        let shared = Shared::new();
        let mut dest = [1.0f32; 8];
        write_f32(&shared, &mut dest);
        assert_eq!(shared.emitted_sample_frames.load(Ordering::Acquire), 0);
    }

    #[test]
    fn halving_the_rate_takes_every_other_sample_frame() {
        let mut resampler = Resampler::new(48_000, 24_000);
        let source = [0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        assert_eq!(out, vec![0.0, 10.0, 2.0, 12.0]);
    }

    #[test]
    fn doubling_the_rate_interpolates_between_sample_frames() {
        let mut resampler = Resampler::new(24_000, 48_000);
        let source = [0.0, 0.0, 1.0, -1.0];
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5]);
    }

    #[test]
    fn the_resampler_carries_a_partial_step_into_the_next_block() {
        let mut resampler = Resampler::new(48_000, 32_000);
        let first = [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0];
        let mut out = Vec::new();
        resampler.push(&first, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 1.5, 1.5]);
        let second = [4.0, 4.0, 5.0, 5.0, 6.0, 6.0, 7.0, 7.0];
        out.clear();
        resampler.push(&second, &mut out);
        assert_eq!(out, vec![3.0, 3.0, 4.5, 4.5, 6.0, 6.0]);
    }

    #[test]
    fn the_resampler_emits_about_the_device_rate() {
        let mut resampler = Resampler::new(48_000, 44_100);
        let block: Vec<f32> = (0..4_000).map(|index| index as f32).collect();
        let mut out = Vec::new();
        for _ in 0..24 {
            resampler.push(&block, &mut out);
        }
        let emitted = (out.len() / STEREO_CHANNELS) as i64;
        assert!(
            (emitted - 44_100).abs() <= 2,
            "a second of 48 kHz sound became {emitted} sample frames"
        );
    }
}

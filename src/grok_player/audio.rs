//! PCM from a composition's MainSound, and the clock the picture follows.
//!
//! Grok's player is picture-only. A DCP still names a sound MXF, so this reads
//! it with asdcplib, or reads the WAV a timeline preview renders, and feeds a
//! stereo downmix or a 5.1 or 7.1 routing to the chosen output device.
//! Missing sound, a missing device, or a failed stream leaves the picture
//! running. Encrypted sound with no key for it is the one sound that fails load.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use asdcplib::crypto::AesDecContext;
use asdcplib::pcm::McaLabelKind;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, Host, SampleFormat, SampleRate, Stream, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use zeroize::Zeroize;

use super::MILLISECONDS_PER_SECOND;
use super::atmos::AtmosSound;
use super::levels::{ChannelMeasure, LevelHistory};
use crate::audio_levels::{ChannelLevel, RMS_WINDOW_SECONDS, numbered_channel_label};
use crate::composition_timeline::{SegmentTrim, SoundSegment};
use crate::content_keys::ContentKeys;
use crate::iab::OutputLayout;
use crate::wav_io::{SampleFormat as WavSampleFormat, WavLayout};

pub const MAXIMUM_SOUND_DELAY_MILLISECONDS: i64 = 10_000;

const STEREO_CHANNELS: usize = 2;
const MCA_CHANNEL_TAG_PREFIX: &str = "ch";
const CENTRE_AND_SURROUND: f32 = 0.707;
const QUEUED_SOUND_SECONDS: f64 = 0.5;
const FEED_WAIT: Duration = Duration::from_millis(5);
const FULL_SCALE_I32: f32 = 2147483648.0;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
// the sample sizes unpack_pcm reads
const UNPACKED_SAMPLE_BITS: [u16; 3] = [16, 24, 32];
const BITS_PER_BYTE: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoundOutputLayout {
    #[default]
    Stereo,
    FivePointOne,
    SevenPointOne,
    // the widest of the three the device offers
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoundSource {
    #[default]
    Atmos,
    MainSound,
}

// the track the choice names, or the other one when the composition lacks it
pub(super) fn chosen_sound(
    choice: SoundSource,
    carries: impl Fn(SoundSource) -> bool,
) -> Option<SoundSource> {
    let other = match choice {
        SoundSource::Atmos => SoundSource::MainSound,
        SoundSource::MainSound => SoundSource::Atmos,
    };
    [choice, other].into_iter().find(|source| carries(*source))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speaker {
    Left,
    Right,
    Centre,
    LowFrequency,
    LeftSurround,
    RightSurround,
    LeftRearSurround,
    RightRearSurround,
}

impl Speaker {
    fn lane_name(self) -> &'static str {
        match self {
            Speaker::Left => "L",
            Speaker::Right => "R",
            Speaker::Centre => "C",
            Speaker::LowFrequency => "LFE",
            Speaker::LeftSurround => "Ls",
            Speaker::RightSurround => "Rs",
            Speaker::LeftRearSurround => "Lrs",
            Speaker::RightRearSurround => "Rrs",
        }
    }
}

// SMPTE 429-2 channel order as libdcp's Channel enum has it, None for HI, VI-N, sync and motion
const DEFAULT_DCP_SPEAKERS: [Option<Speaker>; 12] = [
    Some(Speaker::Left),
    Some(Speaker::Right),
    Some(Speaker::Centre),
    Some(Speaker::LowFrequency),
    Some(Speaker::LeftSurround),
    Some(Speaker::RightSurround),
    None,
    None,
    None,
    None,
    Some(Speaker::LeftRearSurround),
    Some(Speaker::RightRearSurround),
];

// alsa-lib's surround51 and surround71 order, which PipeWire's ALSA plugin also assigns to 6 and 8 channels
#[cfg(target_os = "linux")]
const FIVE_POINT_ONE_DEVICE_ORDER: [Speaker; 6] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::LeftSurround,
    Speaker::RightSurround,
    Speaker::Centre,
    Speaker::LowFrequency,
];
#[cfg(target_os = "linux")]
const SEVEN_POINT_ONE_DEVICE_ORDER: [Speaker; 8] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::LeftRearSurround,
    Speaker::RightRearSurround,
    Speaker::Centre,
    Speaker::LowFrequency,
    Speaker::LeftSurround,
    Speaker::RightSurround,
];

// WAVEFORMATEXTENSIBLE speaker mask order
#[cfg(not(target_os = "linux"))]
const FIVE_POINT_ONE_DEVICE_ORDER: [Speaker; 6] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::Centre,
    Speaker::LowFrequency,
    Speaker::LeftSurround,
    Speaker::RightSurround,
];
#[cfg(not(target_os = "linux"))]
const SEVEN_POINT_ONE_DEVICE_ORDER: [Speaker; 8] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::Centre,
    Speaker::LowFrequency,
    Speaker::LeftRearSurround,
    Speaker::RightRearSurround,
    Speaker::LeftSurround,
    Speaker::RightSurround,
];

// the channel orders of iab::OutputLayout
const RENDERED_STEREO: [Speaker; 2] = [Speaker::Left, Speaker::Right];
const RENDERED_FIVE_POINT_ONE: [Speaker; 6] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::Centre,
    Speaker::LowFrequency,
    Speaker::LeftSurround,
    Speaker::RightSurround,
];
const RENDERED_SEVEN_POINT_ONE: [Speaker; 8] = [
    Speaker::Left,
    Speaker::Right,
    Speaker::Centre,
    Speaker::LowFrequency,
    Speaker::LeftSurround,
    Speaker::RightSurround,
    Speaker::LeftRearSurround,
    Speaker::RightRearSurround,
];

fn rendered_speakers(layout: OutputLayout) -> &'static [Speaker] {
    match layout {
        OutputLayout::Stereo => &RENDERED_STEREO,
        OutputLayout::FivePointOne => &RENDERED_FIVE_POINT_ONE,
        OutputLayout::SevenPointOne => &RENDERED_SEVEN_POINT_ONE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMix {
    StereoDownmix,
    Routed(&'static [Speaker]),
}

impl OutputMix {
    fn apply(self, source: &[f32], speakers: &[Option<Speaker>], out: &mut Vec<f32>) {
        match self {
            OutputMix::StereoDownmix => downmix(source, speakers.len(), out),
            OutputMix::Routed(device_order) => route(source, speakers, device_order, out),
        }
    }

    // the Atmos renderer renders to the layout the device took
    fn rendered_layout(self) -> OutputLayout {
        match self {
            OutputMix::StereoDownmix => OutputLayout::Stereo,
            OutputMix::Routed(device_order)
                if device_order.len() == SEVEN_POINT_ONE_DEVICE_ORDER.len() =>
            {
                OutputLayout::SevenPointOne
            }
            OutputMix::Routed(_) => OutputLayout::FivePointOne,
        }
    }
}

pub fn sound_output_device_names() -> Result<Vec<String>, String> {
    let devices = cpal::default_host()
        .output_devices()
        .map_err(|error| error.to_string())?;
    Ok(devices.filter_map(|device| device.name().ok()).collect())
}

enum Command {
    Load(SoundComposition),
    Queue(Option<SoundComposition>),
    Advance,
    Seek(u64),
    SetPlaying(bool),
    SetDevice(Option<String>, Sender<()>),
    SetLayout(SoundOutputLayout, Sender<()>),
    SetSource(SoundSource),
    SetDelay(i64),
    Stop,
    Shutdown,
}

pub(super) struct SoundComposition {
    main_sound: Vec<SoundReel>,
    pub(super) atmos: Vec<SoundReel>,
    // None while neither track has a reel to play
    playing: Option<SoundSource>,
    pub(super) fps: f64,
    // the composition frame of sound frame 0, past a range's in frame
    pub(super) first_frame: u64,
    // the picture's length, the sound is cut or padded with silence to it
    frame_count: u64,
}

// the composition frames the picture plays, frame 0 of the sound is first_frame of the composition
#[derive(Clone, Copy)]
pub(super) struct PlayedFrames {
    pub fps: f64,
    pub first_frame: u64,
    pub frame_count: u64,
}

impl SoundComposition {
    fn of(sound: &CompositionSounds, choice: SoundSource, frames: PlayedFrames) -> Self {
        let reels = |sound: &CompositionSound| {
            reels_within(reels_of(&sound.segments, sound.essence, frames.fps), frames)
        };
        let mut composition = SoundComposition {
            main_sound: reels(&sound.main_sound),
            atmos: sound.atmos.as_ref().map(reels).unwrap_or_default(),
            playing: None,
            fps: frames.fps,
            first_frame: frames.first_frame,
            frame_count: frames.frame_count,
        };
        composition.choose(choice);
        composition
    }

    fn empty() -> Self {
        SoundComposition {
            main_sound: Vec::new(),
            atmos: Vec::new(),
            playing: None,
            fps: 0.0,
            first_frame: 0,
            frame_count: 0,
        }
    }

    fn track(&self, source: SoundSource) -> &[SoundReel] {
        match source {
            SoundSource::MainSound => &self.main_sound,
            SoundSource::Atmos => &self.atmos,
        }
    }

    fn reels(&self) -> &[SoundReel] {
        self.playing.map_or(&[], |source| self.track(source))
    }

    fn choose(&mut self, choice: SoundSource) {
        self.playing = chosen_sound(choice, |source| !self.track(source).is_empty());
    }
}

pub(super) struct SoundReel {
    pub(super) path: PathBuf,
    edit_rate: Option<(i32, i32)>,
    first_frame: u64,
    frame_count: u64,
    pub(super) entry_edit_unit: u32,
    pub(super) key: Option<SoundContentKey>,
}

#[derive(Clone)]
pub(super) struct SoundContentKey([u8; 16]);

impl SoundContentKey {
    pub(super) fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Drop for SoundContentKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SoundContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SoundContentKey(<redacted>)")
    }
}

pub(super) struct KeyedSoundSegment {
    pub(super) segment: SoundSegment,
    pub(super) key: Option<SoundContentKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SoundEssence {
    #[default]
    Pcm,
    Atmos,
}

#[derive(Default)]
pub(super) struct CompositionSound {
    pub(super) segments: Vec<KeyedSoundSegment>,
    pub(super) essence: SoundEssence,
}

// Atmos is there only when every reel carries it
#[derive(Default)]
pub(super) struct CompositionSounds {
    pub(super) main_sound: CompositionSound,
    pub(super) atmos: Option<CompositionSound>,
}

impl CompositionSounds {
    pub(super) fn carries(&self, source: SoundSource) -> bool {
        match source {
            SoundSource::MainSound => !self.main_sound.segments.is_empty(),
            SoundSource::Atmos => self.atmos.is_some(),
        }
    }
}

pub(super) fn keyed_sound(
    segments: Vec<SoundSegment>,
    essence: SoundEssence,
    keys: Option<&ContentKeys>,
) -> Result<CompositionSound, String> {
    let segments = segments
        .into_iter()
        .map(|segment| {
            let key = match essence {
                SoundEssence::Pcm => sound_content_key(&segment, keys)?,
                SoundEssence::Atmos => atmos_content_key(&segment, keys)?,
            };
            Ok(KeyedSoundSegment { segment, key })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(CompositionSound { segments, essence })
}

// an unreadable reel is left for reels_of to skip with a warning
fn sound_content_key(
    segment: &SoundSegment,
    keys: Option<&ContentKeys>,
) -> Result<Option<SoundContentKey>, String> {
    let path = &segment.path;
    let Ok(mut reader) = PcmReader::open(path, segment.edit_rate) else {
        return Ok(None);
    };
    let Ok(info) = reader.writer_info() else {
        return Ok(None);
    };
    if !info.encrypted_essence {
        return Ok(None);
    }
    if matches!(reader, PcmReader::As02 { .. }) {
        return Err(format!(
            "{} is encrypted AS-02 sound, which asdcplib's AS-02 PCM reader cannot decrypt",
            path.display()
        ));
    }
    covering_sound_key(path, &info, keys).map(Some)
}

// an unreadable reel is left for reels_of to skip with a warning
fn atmos_content_key(
    segment: &SoundSegment,
    keys: Option<&ContentKeys>,
) -> Result<Option<SoundContentKey>, String> {
    let Ok(info) = crate::iab::atmos_writer_info(&segment.path) else {
        return Ok(None);
    };
    if !info.encrypted_essence {
        return Ok(None);
    }
    covering_sound_key(&segment.path, &info, keys).map(Some)
}

fn covering_sound_key(
    path: &Path,
    info: &asdcplib::WriterInfo,
    keys: Option<&ContentKeys>,
) -> Result<SoundContentKey, String> {
    let Some(keys) = keys else {
        return Err(format!(
            "{} is encrypted sound and the preview holds no content key for it",
            path.display()
        ));
    };
    let key = keys.covering_key(info, "sound")?;
    Ok(SoundContentKey(*key))
}

struct Shared {
    playing: AtomicBool,
    stream_live: AtomicBool,
    reels_loaded: AtomicBool,
    device_sample_rate: AtomicU32,
    output_channels: AtomicUsize,
    seek_seconds: AtomicU64,
    emitted_sample_frames: AtomicU64,
    buffer: Mutex<VecDeque<f32>>,
    // what the open stream fell back from
    warnings: Mutex<Vec<String>>,
    // what the loaded sound could not play
    track_warnings: Mutex<Vec<String>>,
    level_meter_on: AtomicBool,
    levels: Mutex<LevelHistory>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            playing: AtomicBool::new(false),
            stream_live: AtomicBool::new(false),
            reels_loaded: AtomicBool::new(false),
            device_sample_rate: AtomicU32::new(DEFAULT_SAMPLE_RATE),
            output_channels: AtomicUsize::new(STEREO_CHANNELS),
            seek_seconds: AtomicU64::new(0.0f64.to_bits()),
            emitted_sample_frames: AtomicU64::new(0),
            buffer: Mutex::new(VecDeque::new()),
            warnings: Mutex::new(Vec::new()),
            track_warnings: Mutex::new(Vec::new()),
            level_meter_on: AtomicBool::new(false),
            levels: Mutex::new(LevelHistory::default()),
        }
    }

    fn output_channels(&self) -> usize {
        self.output_channels.load(Ordering::Acquire)
    }

    fn level_window_sample_frames(&self) -> u64 {
        let device_rate = self.device_sample_rate.load(Ordering::Acquire);
        (RMS_WINDOW_SECONDS * f64::from(device_rate)) as u64
    }

    fn count_emitted(&self, samples: usize) {
        let sample_frames = (samples / self.output_channels()) as u64;
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

    // false when the composition has no sound to play
    pub(super) fn load(
        &self,
        sound: &CompositionSounds,
        choice: SoundSource,
        frames: PlayedFrames,
    ) -> bool {
        let composition = SoundComposition::of(sound, choice, frames);
        if composition.playing.is_none() {
            let _ = self.commands.send(Command::Stop);
            return false;
        }
        self.frames_per_second
            .store(frames.fps.to_bits(), Ordering::Release);
        self.mark_seek(0);
        let _ = self.commands.send(Command::Load(composition));
        true
    }

    // the feeder plays it straight after the loaded composition, silent if it has no sound
    pub(super) fn queue(
        &self,
        sound: &CompositionSounds,
        choice: SoundSource,
        frames: PlayedFrames,
    ) {
        let composition = SoundComposition::of(sound, choice, frames);
        let _ = self.commands.send(Command::Queue(Some(composition)));
    }

    pub(super) fn clear_queue(&self) {
        let _ = self.commands.send(Command::Queue(None));
    }

    // the picture moved on to the queued composition
    pub(super) fn advance(&self, fps: f64) {
        self.frames_per_second
            .store(fps.to_bits(), Ordering::Release);
        let _ = self.commands.send(Command::Advance);
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
        let sample_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        if sample_rate == 0 {
            return None;
        }
        let seek_seconds = f64::from_bits(self.shared.seek_seconds.load(Ordering::Acquire));
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire) as f64;
        Some(seek_seconds + played / f64::from(sample_rate))
    }

    fn mark_seek(&self, frame: u64) {
        let fps = f64::from_bits(self.frames_per_second.load(Ordering::Acquire));
        let seek_seconds = if fps > 0.0 { frame as f64 / fps } else { 0.0 };
        self.shared
            .emitted_sample_frames
            .store(0, Ordering::Release);
        self.shared
            .seek_seconds
            .store(seek_seconds.to_bits(), Ordering::Release);
    }

    pub(super) fn set_playing(&self, playing: bool) {
        let _ = self.commands.send(Command::SetPlaying(playing));
    }

    pub(super) fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    // blocks until the new stream is live for the scheduler's next clock
    pub(super) fn set_device(&self, name: Option<String>) {
        let (reply, reopened) = mpsc::channel();
        if self.commands.send(Command::SetDevice(name, reply)).is_ok() {
            let _ = reopened.recv();
        }
    }

    pub(super) fn set_layout(&self, layout: SoundOutputLayout) {
        let (reply, reopened) = mpsc::channel();
        if self
            .commands
            .send(Command::SetLayout(layout, reply))
            .is_ok()
        {
            let _ = reopened.recv();
        }
    }

    // the device clock runs on through the switch
    pub(super) fn set_source(&self, choice: SoundSource) {
        let _ = self.commands.send(Command::SetSource(choice));
    }

    pub(super) fn warnings(&self) -> Vec<String> {
        let mut warnings = self.shared.warnings.lock().unwrap().clone();
        warnings.extend(self.shared.track_warnings.lock().unwrap().iter().cloned());
        warnings
    }

    // takes hold at the next seek
    pub(super) fn set_delay_milliseconds(&self, milliseconds: i64) {
        let _ = self.commands.send(Command::SetDelay(milliseconds));
    }

    pub(super) fn level_meter(&self) -> SoundLevelMeter {
        SoundLevelMeter(Arc::clone(&self.shared))
    }
}

// the levels of the source channels at the position the device has played to
pub(super) struct SoundLevelMeter(Arc<Shared>);

impl SoundLevelMeter {
    // off, the feeder measures nothing
    pub(super) fn set_enabled(&self, enabled: bool) {
        self.0.level_meter_on.store(enabled, Ordering::Release);
        self.0.levels.lock().unwrap().forget();
    }

    // None while the meter is off or nothing with sound is loaded
    pub(super) fn levels(&self) -> Option<Vec<ChannelLevel>> {
        let shared = &self.0;
        if !shared.level_meter_on.load(Ordering::Acquire)
            || !shared.reels_loaded.load(Ordering::Acquire)
        {
            return None;
        }
        let heard =
            shared.playing.load(Ordering::Acquire) && shared.stream_live.load(Ordering::Acquire);
        let played = shared.emitted_sample_frames.load(Ordering::Acquire);
        let window = shared.level_window_sample_frames();
        shared.levels.lock().unwrap().read(played, window, heard)
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

fn reels_of(segments: &[KeyedSoundSegment], essence: SoundEssence, fps: f64) -> Vec<SoundReel> {
    let mut first_frame = 0u64;
    let mut reels = Vec::new();
    for KeyedSoundSegment { segment, key } in segments {
        let (entry, stated) = trim_in_frames(segment.trim.as_ref(), fps);
        let edit_units = match essence {
            SoundEssence::Pcm => {
                open_layout(&segment.path, segment.edit_rate).map(|layout| layout.edit_units)
            }
            SoundEssence::Atmos => crate::iab::AtmosReader::open(
                &segment.path,
                key.as_ref().map(SoundContentKey::bytes),
            )
            .map(|reader| reader.frame_count()),
        };
        let duration = match edit_units {
            Ok(edit_units) => edit_units.saturating_sub(entry),
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
            edit_rate: segment.edit_rate,
            first_frame,
            frame_count: frames,
            entry_edit_unit: entry,
            key: key.clone(),
        });
        first_frame += frames;
    }
    reels
}

// sound edit units are picture frames, so the cut lands on a frame boundary to the sample
fn reels_within(reels: Vec<SoundReel>, frames: PlayedFrames) -> Vec<SoundReel> {
    let end = frames.first_frame + frames.frame_count;
    reels
        .into_iter()
        .filter_map(|reel| {
            let start = reel.first_frame.max(frames.first_frame);
            let stop = (reel.first_frame + reel.frame_count).min(end);
            if start >= stop {
                return None;
            }
            let skipped = u32::try_from(start - reel.first_frame).ok()?;
            Some(SoundReel {
                first_frame: start - frames.first_frame,
                frame_count: stop - start,
                entry_edit_unit: reel.entry_edit_unit.checked_add(skipped)?,
                ..reel
            })
        })
        .collect()
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

struct OpenedStream {
    stream: Stream,
    mix: OutputMix,
}

fn start_stream(
    shared: Arc<Shared>,
    pcm_sample_rate: u32,
    device_name: Option<&str>,
    layout: SoundOutputLayout,
    warnings: &mut Vec<String>,
) -> Option<OpenedStream> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        try_start_stream(shared, pcm_sample_rate, device_name, layout, warnings)
    }))
    .ok()
    .flatten()
}

fn try_start_stream(
    shared: Arc<Shared>,
    pcm_sample_rate: u32,
    device_name: Option<&str>,
    layout: SoundOutputLayout,
    warnings: &mut Vec<String>,
) -> Option<OpenedStream> {
    let host = cpal::default_host();
    let device = output_device(&host, device_name, warnings)?;
    let ranges: Vec<SupportedStreamConfigRange> = device
        .supported_output_configs()
        .map(|ranges| ranges.collect())
        .unwrap_or_default();
    let (supported, mix) = match surround_config(&ranges, layout, pcm_sample_rate, warnings) {
        Some(surround) => surround,
        None => (
            stereo_config(&device, &ranges, pcm_sample_rate)?,
            OutputMix::StereoDownmix,
        ),
    };
    let mut config = supported.config();
    if mix == OutputMix::StereoDownmix {
        config.channels = STEREO_CHANNELS as u16;
    }
    shared
        .device_sample_rate
        .store(config.sample_rate.0, Ordering::Release);
    shared
        .output_channels
        .store(usize::from(config.channels), Ordering::Release);
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
            Some(OpenedStream { stream, mix })
        }
        Err(error) => {
            tracing::warn!("preview sound: {error}");
            None
        }
    }
}

fn warn(warnings: &mut Vec<String>, warning: String) {
    tracing::warn!("preview sound: {warning}");
    warnings.push(warning);
}

fn output_device(host: &Host, name: Option<&str>, warnings: &mut Vec<String>) -> Option<Device> {
    let Some(name) = name else {
        return host.default_output_device();
    };
    let named = host.output_devices().ok().and_then(|mut devices| {
        devices.find(|device| device.name().is_ok_and(|device_name| device_name == name))
    });
    if named.is_none() {
        warn(
            warnings,
            format!("output device {name} is missing, sound plays on the default device"),
        );
    }
    named.or_else(|| host.default_output_device())
}

fn is_writable_format(format: SampleFormat) -> bool {
    matches!(format, SampleFormat::F32 | SampleFormat::I16)
}

fn stereo_config(
    device: &Device,
    ranges: &[SupportedStreamConfigRange],
    pcm_sample_rate: u32,
) -> Option<SupportedStreamConfig> {
    let default = device.default_output_config().ok()?;
    let matching = ranges
        .iter()
        .filter(|range| {
            range.channels() >= STEREO_CHANNELS as u16 && is_writable_format(range.sample_format())
        })
        .find_map(|range| range.try_with_sample_rate(SampleRate(pcm_sample_rate)));
    Some(matching.unwrap_or(default))
}

// None plays the stereo downmix
fn surround_config(
    ranges: &[SupportedStreamConfigRange],
    layout: SoundOutputLayout,
    pcm_sample_rate: u32,
    warnings: &mut Vec<String>,
) -> Option<(SupportedStreamConfig, OutputMix)> {
    let order: &'static [Speaker] = match layout {
        SoundOutputLayout::Stereo => return None,
        SoundOutputLayout::FivePointOne => &FIVE_POINT_ONE_DEVICE_ORDER,
        SoundOutputLayout::SevenPointOne => &SEVEN_POINT_ONE_DEVICE_ORDER,
        SoundOutputLayout::Automatic => match automatic_layout(ranges) {
            SoundOutputLayout::SevenPointOne => &SEVEN_POINT_ONE_DEVICE_ORDER,
            SoundOutputLayout::FivePointOne => &FIVE_POINT_ONE_DEVICE_ORDER,
            _ => return None,
        },
    };
    let offered: Vec<SupportedStreamConfigRange> = ranges
        .iter()
        .filter(|range| {
            usize::from(range.channels()) == order.len()
                && is_writable_format(range.sample_format())
        })
        .copied()
        .collect();
    let at_pcm_rate = offered
        .iter()
        .find_map(|range| range.try_with_sample_rate(SampleRate(pcm_sample_rate)));
    let config = at_pcm_rate.or_else(|| {
        offered
            .into_iter()
            .next()
            .map(SupportedStreamConfigRange::with_max_sample_rate)
    });
    let Some(config) = config else {
        warn(
            warnings,
            format!(
                "the sound device offers no {} channel output, sound plays as a stereo downmix",
                order.len()
            ),
        );
        return None;
    };
    Some((config, OutputMix::Routed(order)))
}

fn automatic_layout(ranges: &[SupportedStreamConfigRange]) -> SoundOutputLayout {
    let offers = |channels: usize| {
        ranges.iter().any(|range| {
            usize::from(range.channels()) == channels && is_writable_format(range.sample_format())
        })
    };
    if offers(SEVEN_POINT_ONE_DEVICE_ORDER.len()) {
        return SoundOutputLayout::SevenPointOne;
    }
    if offers(FIVE_POINT_ONE_DEVICE_ORDER.len()) {
        return SoundOutputLayout::FivePointOne;
    }
    SoundOutputLayout::Stereo
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
    current: SoundComposition,
    queued: Option<SoundComposition>,
    // the composition the fill left while the picture still shows it
    finished: Option<SoundComposition>,
    reader: Option<(PathBuf, SoundReader)>,
    // the current composition's renderer when its sound is Atmos
    atmos: Option<AtmosSound>,
    next_frame: u64,
    // the frame of the current composition the device count starts from
    seek_position: Option<f64>,
    pcm_sample_rate: u32,
    resampler: Option<Resampler>,
    stream: Option<Stream>,
    device_name: Option<String>,
    layout: SoundOutputLayout,
    mix: OutputMix,
    delay_seconds: f64,
    skipped_source_sample_frames: usize,
}

// one edit unit of source channels, before the mix to the device
struct SourceEditUnit {
    interleaved: Vec<f32>,
    speakers: Vec<Option<Speaker>>,
    lane_names: Arc<[String]>,
}

struct SoundReader {
    reader: EditUnitReader,
    layout: AudioLayout,
    decrypt: Option<AesDecContext>,
}

struct AudioLayout {
    channels: u16,
    bits: u16,
    bytes_per_edit_unit: usize,
    edit_units: u32,
    sample_rate: u32,
    speakers: Vec<Option<Speaker>>,
    lane_names: Arc<[String]>,
}

fn feed(shared: Arc<Shared>, commands: mpsc::Receiver<Command>) {
    let mut feeder = Feeder::new(shared);
    loop {
        feeder.fill();
        match commands.recv_timeout(FEED_WAIT) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::Load(composition)) => feeder.load(composition),
            Ok(Command::Queue(composition)) => feeder.queue(composition),
            Ok(Command::Advance) => feeder.advance(),
            Ok(Command::Seek(frame)) => feeder.seek(frame),
            Ok(Command::SetPlaying(playing)) => {
                feeder.shared.playing.store(playing, Ordering::Release);
            }
            Ok(Command::SetDevice(name, reply)) => {
                feeder.device_name = name;
                feeder.reopen();
                let _ = reply.send(());
            }
            Ok(Command::SetLayout(layout, reply)) => {
                feeder.layout = layout;
                feeder.reopen();
                let _ = reply.send(());
            }
            Ok(Command::SetSource(choice)) => feeder.set_source(choice),
            Ok(Command::SetDelay(milliseconds)) => {
                feeder.delay_seconds = milliseconds as f64 / MILLISECONDS_PER_SECOND;
            }
            Ok(Command::Stop) => feeder.stop(),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Feeder {
    fn new(shared: Arc<Shared>) -> Self {
        Feeder {
            shared,
            current: SoundComposition::empty(),
            queued: None,
            finished: None,
            reader: None,
            atmos: None,
            next_frame: 0,
            seek_position: None,
            pcm_sample_rate: DEFAULT_SAMPLE_RATE,
            resampler: None,
            stream: None,
            device_name: None,
            layout: SoundOutputLayout::default(),
            mix: OutputMix::StereoDownmix,
            delay_seconds: 0.0,
            skipped_source_sample_frames: 0,
        }
    }

    fn load(&mut self, composition: SoundComposition) {
        self.stop();
        let Some(playing) = composition.playing else {
            return;
        };
        self.current = composition;
        if let Err(error) = self.open_track(playing) {
            self.warn_track(vec![error]);
            self.current = SoundComposition::empty();
            return;
        }
        self.shared.reels_loaded.store(true, Ordering::Release);
        if self.stream.is_none() {
            self.open_stream();
        }
        self.seek(0);
    }

    // opens the track's first reel, which sets the sample rate a start counts source samples in
    fn open_track(&mut self, source: SoundSource) -> Result<(), String> {
        match source {
            SoundSource::MainSound => {
                let first = self
                    .current
                    .main_sound
                    .first()
                    .ok_or("the composition has no main sound reel")?;
                let opened = open_reader(&first.path, first.edit_rate, first.key.as_ref())?;
                eprintln!(
                    "[preview] sound: {}ch {}-bit {} Hz from {}",
                    opened.layout.channels,
                    opened.layout.bits,
                    opened.layout.sample_rate,
                    first.path.display()
                );
                self.pcm_sample_rate = opened.layout.sample_rate;
                self.reader = Some((first.path.clone(), opened));
                self.atmos = None;
            }
            SoundSource::Atmos => {
                let atmos = AtmosSound::open(&self.current, self.mix.rendered_layout())?;
                eprintln!(
                    "[preview] sound: Atmos at {} Hz from {}",
                    atmos.sample_rate(),
                    self.current.atmos[0].path.display()
                );
                self.pcm_sample_rate = atmos.sample_rate();
                self.atmos = Some(atmos);
                self.reader = None;
            }
        }
        Ok(())
    }

    // the queued composition takes the choice too, and the sound picks up where the device is
    fn set_source(&mut self, choice: SoundSource) {
        let switched_early = self.finished.is_some();
        self.undo_switch();
        if let Some(queued) = self.queued.as_mut() {
            queued.choose(choice);
        }
        let playing = self.current.playing;
        self.current.choose(choice);
        let other_track = self
            .current
            .playing
            .filter(|chosen| playing != Some(*chosen));
        let mut reopened = false;
        if let Some(chosen) = other_track {
            match self.open_track(chosen) {
                Ok(()) => reopened = true,
                Err(error) => {
                    self.warn_track(vec![error]);
                    self.current.playing = playing;
                }
            }
        }
        // the buffer holds the other track, or the start of the composition queued before
        if reopened || switched_early {
            self.resume_where_the_device_is();
        }
    }

    fn queue(&mut self, composition: Option<SoundComposition>) {
        let switched_early = self.finished.is_some();
        self.undo_switch();
        self.queued = composition;
        // the buffer holds the start of the composition that was queued before
        if switched_early {
            self.resume_where_the_device_is();
        }
    }

    fn advance(&mut self) {
        if self.finished.take().is_some() {
            return;
        }
        // the fill had not reached the end of the composition the picture left
        if self.switch_to_queued() {
            self.finished = None;
            self.resume_where_the_device_is();
        }
    }

    fn switch_to_queued(&mut self) -> bool {
        let Some(next) = self.queued.take() else {
            return false;
        };
        let next_fps = next.fps;
        let finished = std::mem::replace(&mut self.current, next);
        self.atmos = None;
        let to_next_frames =
            |frames: f64| (frames - finished.frame_count as f64) * next_fps / finished.fps;
        self.seek_position = self.seek_position.map(to_next_frames);
        self.next_frame = to_next_frames(self.next_frame as f64).round().max(0.0) as u64;
        self.skipped_source_sample_frames = 0;
        self.finished = Some(finished);
        self.log_start("the queued composition follows on");
        true
    }

    fn undo_switch(&mut self) {
        let Some(finished) = self.finished.take() else {
            return;
        };
        let next = std::mem::replace(&mut self.current, finished);
        self.atmos = None;
        let next_fps = next.fps;
        let current = &self.current;
        self.seek_position = self
            .seek_position
            .map(|frames| frames * current.fps / next_fps + current.frame_count as f64);
        self.queued = Some(next);
    }

    fn resume_where_the_device_is(&mut self) {
        let Some(seek_position) = self.seek_position else {
            return;
        };
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        if device_rate == 0 {
            return;
        }
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire) as f64;
        self.start_at(seek_position + played * self.current.fps / f64::from(device_rate));
    }

    // a device or layout change before the first load waits for that load
    fn reopen(&mut self) {
        if self.stream.is_none() && self.current.reels().is_empty() {
            return;
        }
        self.open_stream();
    }

    fn open_stream(&mut self) {
        // an exclusive ALSA device refuses a second stream
        self.stream = None;
        self.shared.buffer.lock().unwrap().clear();
        let mut warnings = Vec::new();
        let opened = start_stream(
            Arc::clone(&self.shared),
            self.pcm_sample_rate,
            self.device_name.as_deref(),
            self.layout,
            &mut warnings,
        );
        *self.shared.warnings.lock().unwrap() = warnings;
        self.shared
            .stream_live
            .store(opened.is_some(), Ordering::Release);
        if let Some(opened) = opened {
            self.stream = Some(opened.stream);
            self.mix = opened.mix;
        }
        if let Some(atmos) = self.atmos.as_mut() {
            atmos.set_layout(self.mix.rendered_layout());
        }
        self.reset_resampler();
    }

    fn seek(&mut self, frame: u64) {
        self.undo_switch();
        self.shared.levels.lock().unwrap().restart();
        self.seek_position = Some(frame as f64);
        self.start_at(frame as f64);
    }

    fn start_at(&mut self, picture_frames: f64) {
        self.reset_resampler();
        if let Some(atmos) = self.atmos.as_mut() {
            atmos.restart();
        }
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        let start = sound_start(
            picture_frames,
            self.current.fps,
            self.delay_seconds,
            self.pcm_sample_rate,
            device_rate,
        );
        self.next_frame = start.edit_unit;
        self.skipped_source_sample_frames = start.skipped_source_sample_frames;
        let silence = start.leading_silence_sample_frames * self.shared.output_channels();
        {
            let mut buffer = self.shared.buffer.lock().unwrap();
            buffer.clear();
            buffer.extend(std::iter::repeat_n(0.0, silence));
        }
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire);
        self.shared.levels.lock().unwrap().discard_unplayed(played);
        self.log_start("starts");
    }

    fn log_start(&self, how: &str) {
        if self.current.fps <= 0.0 {
            return;
        }
        let frame = self.current.first_frame + self.next_frame;
        let sample = (frame as f64 * f64::from(self.pcm_sample_rate) / self.current.fps).round()
            as u64
            + self.skipped_source_sample_frames as u64;
        eprintln!(
            "[preview] sound: {how} at composition frame {frame}, sample {sample} of {} Hz",
            self.pcm_sample_rate
        );
    }

    fn stop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.reels_loaded.store(false, Ordering::Release);
        self.shared.buffer.lock().unwrap().clear();
        self.shared.levels.lock().unwrap().forget();
        self.current = SoundComposition::empty();
        self.queued = None;
        self.finished = None;
        self.reader = None;
        self.atmos = None;
        self.shared.track_warnings.lock().unwrap().clear();
        self.next_frame = 0;
        self.seek_position = None;
    }

    fn warn_track(&self, warnings: Vec<String>) {
        if warnings.is_empty() {
            return;
        }
        let mut track_warnings = self.shared.track_warnings.lock().unwrap();
        for warning in warnings {
            tracing::warn!("preview sound: {warning}");
            track_warnings.push(warning);
        }
    }

    fn reset_resampler(&mut self) {
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        let channels = self.shared.output_channels();
        self.resampler =
            (device_rate > 0 && self.pcm_sample_rate > 0 && device_rate != self.pcm_sample_rate)
                .then(|| Resampler::new(self.pcm_sample_rate, device_rate, channels));
    }

    fn fill(&mut self) {
        if self.seek_position.is_none() {
            return;
        }
        let device_rate = self.shared.device_sample_rate.load(Ordering::Acquire);
        let high_water = (f64::from(device_rate) * QUEUED_SOUND_SECONDS) as usize
            * self.shared.output_channels();
        loop {
            let queued = self.shared.buffer.lock().unwrap().len();
            if queued >= high_water {
                break;
            }
            self.catch_up(queued, device_rate);
            if !self.push_edit_unit() {
                break;
            }
        }
    }

    fn catch_up(&mut self, queued: usize, device_rate: u32) {
        let Some(seek_position) = self.seek_position else {
            return;
        };
        if self.current.fps <= 0.0 || device_rate == 0 {
            return;
        }
        let played = self.shared.emitted_sample_frames.load(Ordering::Acquire);
        let sample_frames = played + (queued / self.shared.output_channels()) as u64;
        let reached = frame_at_queue_end(
            seek_position,
            sample_frames,
            self.current.fps,
            device_rate,
            self.delay_seconds,
        );
        if self.next_frame >= reached {
            return;
        }
        tracing::debug!(
            "preview sound: skipping frames {} to {reached} to keep up with the device",
            self.next_frame
        );
        self.next_frame = reached;
        self.skipped_source_sample_frames = 0;
    }

    fn push_edit_unit(&mut self) -> bool {
        if self.next_frame >= self.current.frame_count && !self.switch_to_queued() {
            return false;
        }
        let location = self.location(self.next_frame);
        let (Some(playing), Some((reel, entry))) = (self.current.playing, location) else {
            self.push_silent_edit_unit();
            return true;
        };
        let source = match playing {
            SoundSource::MainSound => self.read_pcm_edit_unit(reel, entry),
            SoundSource::Atmos => self.render_atmos_edit_unit(),
        };
        let Some(source) = source else {
            return false;
        };
        self.queue_source_edit_unit(source);
        self.next_frame += 1;
        true
    }

    fn read_pcm_edit_unit(&mut self, reel: usize, entry: u32) -> Option<SourceEditUnit> {
        if !self.ensure_reader(reel) {
            return None;
        }
        let (_, open) = self.reader.as_ref()?;
        if open.layout.sample_rate != self.pcm_sample_rate {
            self.pcm_sample_rate = open.layout.sample_rate;
            self.reset_resampler();
        }
        let (_, open) = self.reader.as_mut()?;
        let layout = &open.layout;
        let mut essence = vec![0u8; layout.bytes_per_edit_unit];
        let read = match open
            .reader
            .read_frame(entry, &mut essence, open.decrypt.as_mut())
        {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!("preview sound: {error}");
                return None;
            }
        };
        let mut interleaved = Vec::new();
        unpack_pcm(
            &essence[..read],
            layout.channels as usize,
            layout.bits,
            &mut interleaved,
        );
        Some(SourceEditUnit {
            interleaved,
            speakers: layout.speakers.clone(),
            lane_names: Arc::clone(&layout.lane_names),
        })
    }

    fn render_atmos_edit_unit(&mut self) -> Option<SourceEditUnit> {
        if self.atmos.is_none() {
            match AtmosSound::open(&self.current, self.mix.rendered_layout()) {
                Ok(atmos) => self.atmos = Some(atmos),
                Err(error) => {
                    self.warn_track(vec![error]);
                    return None;
                }
            }
        }
        let atmos = self.atmos.as_mut()?;
        let sample_rate = atmos.sample_rate();
        let rendered = atmos.edit_unit(&self.current.atmos, self.next_frame);
        let speakers = rendered_speakers(atmos.layout());
        self.warn_track(rendered.warnings);
        if sample_rate != self.pcm_sample_rate {
            self.pcm_sample_rate = sample_rate;
            self.reset_resampler();
        }
        Some(SourceEditUnit {
            interleaved: rendered.samples,
            speakers: speakers.iter().copied().map(Some).collect(),
            lane_names: speakers
                .iter()
                .map(|speaker| speaker.lane_name().to_string())
                .collect(),
        })
    }

    fn queue_source_edit_unit(&mut self, source: SourceEditUnit) {
        let SourceEditUnit {
            mut interleaved,
            speakers,
            lane_names,
        } = source;
        let channels = speakers.len();
        let skipped = (self.skipped_source_sample_frames * channels).min(interleaved.len());
        interleaved.drain(..skipped);
        self.skipped_source_sample_frames = 0;
        let measured = self
            .shared
            .level_meter_on
            .load(Ordering::Acquire)
            .then(|| ChannelMeasure::of(&interleaved, channels));
        let mut mixed = Vec::new();
        self.mix.apply(&interleaved, &speakers, &mut mixed);
        let device_samples = match self.resampler.as_mut() {
            Some(resampler) => {
                let mut resampled = Vec::new();
                resampler.push(&mixed, &mut resampled);
                resampled
            }
            None => mixed,
        };
        let (start, end) = self.queue_for_the_device(device_samples);
        if let Some(measure) = measured {
            let played = self.shared.emitted_sample_frames.load(Ordering::Acquire);
            let window = self.shared.level_window_sample_frames();
            self.shared.levels.lock().unwrap().push(
                &lane_names,
                start,
                end,
                measure,
                played,
                window,
            );
        }
    }

    // where the samples will play, in sample frames past the seek the device count started from
    fn queue_for_the_device(&self, samples: Vec<f32>) -> (u64, u64) {
        let sample_frames = (samples.len() / self.shared.output_channels()) as u64;
        let mut buffer = self.shared.buffer.lock().unwrap();
        let queued = (buffer.len() / self.shared.output_channels()) as u64;
        let start = self.shared.emitted_sample_frames.load(Ordering::Acquire) + queued;
        buffer.extend(samples);
        (start, start + sample_frames)
    }

    // a composition whose sound is shorter than its picture
    fn push_silent_edit_unit(&mut self) {
        let device_rate = f64::from(self.shared.device_sample_rate.load(Ordering::Acquire));
        let device_frame_at =
            |edit_unit: u64| (edit_unit as f64 * device_rate / self.current.fps).round() as usize;
        let skipped = (self.skipped_source_sample_frames as f64 * device_rate
            / f64::from(self.pcm_sample_rate))
        .round() as usize;
        let sample_frames = (device_frame_at(self.next_frame + 1)
            - device_frame_at(self.next_frame))
        .saturating_sub(skipped);
        self.skipped_source_sample_frames = 0;
        let samples = sample_frames * self.shared.output_channels();
        let (start, end) = self.queue_for_the_device(vec![0.0; samples]);
        if self.shared.level_meter_on.load(Ordering::Acquire) {
            let played = self.shared.emitted_sample_frames.load(Ordering::Acquire);
            let window = self.shared.level_window_sample_frames();
            self.shared
                .levels
                .lock()
                .unwrap()
                .push_silence(start, end, played, window);
        }
        self.next_frame += 1;
    }

    fn location(&self, frame: u64) -> Option<(usize, u32)> {
        reel_location(self.current.reels(), frame)
    }

    fn ensure_reader(&mut self, reel: usize) -> bool {
        let reel = &self.current.main_sound[reel];
        if self
            .reader
            .as_ref()
            .is_some_and(|(open, _)| open == &reel.path)
        {
            return true;
        }
        match open_reader(&reel.path, reel.edit_rate, reel.key.as_ref()) {
            Ok(opened) => {
                self.reader = Some((reel.path.clone(), opened));
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

// the reel that plays sound frame and the edit unit of it
pub(super) fn reel_location(reels: &[SoundReel], frame: u64) -> Option<(usize, u32)> {
    for (index, reel) in reels.iter().enumerate() {
        if frame < reel.first_frame {
            continue;
        }
        let into = frame - reel.first_frame;
        if reel.frame_count != u64::MAX && into >= reel.frame_count {
            continue;
        }
        let entry = reel.entry_edit_unit.saturating_add(into as u32);
        return Some((index, entry));
    }
    None
}

// the sound edit unit the device reaches once it has played sample_frames past the seek
fn frame_at_queue_end(
    seek_frame: f64,
    sample_frames: u64,
    fps: f64,
    sample_rate: u32,
    delay_seconds: f64,
) -> u64 {
    if fps <= 0.0 || sample_rate == 0 {
        return seek_frame.max(0.0) as u64;
    }
    let played = sample_frames as f64 * fps / f64::from(sample_rate);
    (seek_frame + played - delay_seconds * fps).floor().max(0.0) as u64
}

#[derive(Debug, PartialEq, Eq)]
struct SoundStart {
    leading_silence_sample_frames: usize,
    edit_unit: u64,
    skipped_source_sample_frames: usize,
}

// the sound under a picture frame is delay_seconds earlier in the composition
fn sound_start(
    picture_frames: f64,
    fps: f64,
    delay_seconds: f64,
    source_rate: u32,
    device_rate: u32,
) -> SoundStart {
    if fps <= 0.0 {
        return SoundStart {
            leading_silence_sample_frames: 0,
            edit_unit: picture_frames.max(0.0) as u64,
            skipped_source_sample_frames: 0,
        };
    }
    let start_frames = picture_frames - delay_seconds * fps;
    if start_frames < 0.0 {
        let silence_seconds = -start_frames / fps;
        return SoundStart {
            leading_silence_sample_frames: (silence_seconds * f64::from(device_rate)).round()
                as usize,
            edit_unit: 0,
            skipped_source_sample_frames: 0,
        };
    }
    let edit_unit = start_frames.floor();
    let into_edit_unit_seconds = (start_frames - edit_unit) / fps;
    SoundStart {
        leading_silence_sample_frames: 0,
        edit_unit: edit_unit as u64,
        skipped_source_sample_frames: (into_edit_unit_seconds * f64::from(source_rate)).round()
            as usize,
    }
}

struct Resampler {
    step: f64,
    position: f64,
    channels: usize,
    carry: Vec<f32>,
}

impl Resampler {
    fn new(source_rate: u32, device_rate: u32, channels: usize) -> Self {
        Resampler {
            step: f64::from(source_rate) / f64::from(device_rate),
            position: 0.0,
            channels,
            carry: vec![0.0; channels],
        }
    }

    fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        let channels = self.channels;
        let sample_frames = samples.len() / channels;
        if sample_frames == 0 {
            return;
        }
        let carry = std::mem::take(&mut self.carry);
        // the frame before this block, so a partial step carries across blocks
        let frame = |index: f64| -> &[f32] {
            if index < 0.0 {
                return &carry;
            }
            let start = index as usize * channels;
            &samples[start..start + channels]
        };
        let last = sample_frames as f64 - 1.0;
        while self.position < last {
            let base = self.position.floor();
            let fraction = (self.position - base) as f32;
            let before = frame(base);
            let after = frame(base + 1.0);
            for (before, after) in before.iter().zip(after) {
                out.push(before + (after - before) * fraction);
            }
            self.position += self.step;
        }
        self.carry = frame(last).to_vec();
        self.position -= sample_frames as f64;
    }
}

fn open_layout(path: &Path, edit_rate: Option<(i32, i32)>) -> Result<AudioLayout, String> {
    Ok(open_reader(path, edit_rate, None)?.layout)
}

// AS-DCP sound is wrapped a picture frame an edit unit, AS-02 sound is one clip read in slices of the CPL's edit rate
enum PcmReader {
    AsDcp(asdcplib::pcm::MxfReader),
    As02 {
        reader: asdcplib::as02::pcm::MxfReader,
        edit_rate: (i32, i32),
    },
}

impl PcmReader {
    fn open(path: &Path, edit_rate: Option<(i32, i32)>) -> Result<PcmReader, String> {
        let name = path.to_string_lossy();
        let as02 = matches!(
            asdcplib::essence_type(&name),
            Ok(asdcplib::EssenceType::As02Pcm24b48k | asdcplib::EssenceType::As02Pcm24b96k)
        );
        if !as02 {
            let mut reader = asdcplib::pcm::MxfReader::new();
            reader.open_read(&name).map_err(|error| error.to_string())?;
            return Ok(PcmReader::AsDcp(reader));
        }
        let (numerator, denominator) = edit_rate.ok_or_else(|| {
            format!(
                "{} is AS-02 sound and the CPL gives no edit rate to read it at",
                path.display()
            )
        })?;
        let mut reader = asdcplib::as02::pcm::MxfReader::new();
        reader
            .open_read(
                &name,
                asdcplib::Rational {
                    numerator,
                    denominator,
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(PcmReader::As02 {
            reader,
            edit_rate: (numerator, denominator),
        })
    }

    fn audio_descriptor(&mut self) -> asdcplib::Result<asdcplib::pcm::AudioDescriptor> {
        match self {
            PcmReader::AsDcp(reader) => reader.audio_descriptor(),
            PcmReader::As02 { reader, .. } => reader.audio_descriptor(),
        }
    }

    fn writer_info(&mut self) -> asdcplib::Result<asdcplib::WriterInfo> {
        match self {
            PcmReader::AsDcp(reader) => reader.writer_info(),
            PcmReader::As02 { reader, .. } => reader.writer_info(),
        }
    }

    fn mca_label_subdescriptors(
        &mut self,
    ) -> asdcplib::Result<Vec<asdcplib::pcm::McaLabelSubDescriptor>> {
        match self {
            PcmReader::AsDcp(reader) => reader.mca_label_subdescriptors(),
            PcmReader::As02 { reader, .. } => reader.mca_label_subdescriptors(),
        }
    }

    fn read_frame(
        &mut self,
        edit_unit: u32,
        buffer: &mut [u8],
        decrypt: Option<&mut AesDecContext>,
    ) -> asdcplib::Result<usize> {
        match self {
            PcmReader::AsDcp(reader) => reader.read_frame(edit_unit, buffer, decrypt, None),
            PcmReader::As02 { reader, .. } => reader.read_frame(edit_unit, buffer, decrypt, None),
        }
    }

    // sample frames an edit unit and edit units in the file, at the rate the composition plays the track
    fn edit_units(&self, descriptor: &asdcplib::pcm::AudioDescriptor) -> (u32, u32) {
        match self {
            PcmReader::AsDcp(_) => (
                frames_per_edit_unit(descriptor.audio_sampling_rate, descriptor.edit_rate),
                descriptor.container_duration,
            ),
            PcmReader::As02 {
                edit_rate: (numerator, denominator),
                ..
            } => {
                let edit_rate = asdcplib::Rational {
                    numerator: *numerator,
                    denominator: *denominator,
                };
                let per_edit_unit =
                    frames_per_edit_unit(descriptor.audio_sampling_rate, edit_rate).max(1);
                // an AS-02 descriptor counts the clip in sample frames
                (
                    per_edit_unit,
                    descriptor.container_duration.div_ceil(per_edit_unit),
                )
            }
        }
    }
}

// the timeline's sound reaches the player as a WAV
enum EditUnitReader {
    Mxf(PcmReader),
    Wav(WavEditUnits),
}

impl EditUnitReader {
    fn read_frame(
        &mut self,
        edit_unit: u32,
        buffer: &mut [u8],
        decrypt: Option<&mut AesDecContext>,
    ) -> Result<usize, String> {
        match self {
            EditUnitReader::Mxf(reader) => reader
                .read_frame(edit_unit, buffer, decrypt)
                .map_err(|error| error.to_string()),
            EditUnitReader::Wav(reader) => reader.read_frame(edit_unit, buffer),
        }
    }
}

// edit unit n starts on sample frame floor(n * sample rate / fps), exact at 30000/1001
struct WavEditUnits {
    path: PathBuf,
    file: File,
    data_offset: u64,
    block_align: u64,
    sample_frames: u64,
    sample_rate: u64,
    edit_rate: (u64, u64),
}

impl WavEditUnits {
    fn sample_frame_at(&self, edit_unit: u64) -> u64 {
        let (numerator, denominator) = self.edit_rate;
        let frame = u128::from(edit_unit) * u128::from(self.sample_rate) * u128::from(denominator)
            / u128::from(numerator);
        (frame as u64).min(self.sample_frames)
    }

    fn edit_units(&self) -> u64 {
        let (numerator, denominator) = self.edit_rate;
        (u128::from(self.sample_frames) * u128::from(numerator))
            .div_ceil(u128::from(self.sample_rate) * u128::from(denominator)) as u64
    }

    fn read_frame(&mut self, edit_unit: u32, buffer: &mut [u8]) -> Result<usize, String> {
        let start = self.sample_frame_at(u64::from(edit_unit));
        let end = self.sample_frame_at(u64::from(edit_unit) + 1);
        let bytes = ((end - start) * self.block_align) as usize;
        let read_error = |error: std::io::Error| format!("{}: {error}", self.path.display());
        self.file
            .seek(SeekFrom::Start(self.data_offset + start * self.block_align))
            .map_err(read_error)?;
        self.file
            .read_exact(&mut buffer[..bytes])
            .map_err(read_error)?;
        Ok(bytes)
    }
}

fn open_wav(path: &Path, edit_rate: Option<(i32, i32)>) -> Result<SoundReader, String> {
    let (numerator, denominator) = edit_rate
        .and_then(|(numerator, denominator)| {
            Some((
                u64::try_from(numerator).ok()?,
                u64::try_from(denominator).ok()?,
            ))
        })
        .filter(|(numerator, denominator)| *numerator > 0 && *denominator > 0)
        .ok_or_else(|| {
            format!(
                "{} is a WAV with no frame rate to read it at",
                path.display()
            )
        })?;
    let layout = WavLayout::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let bits = layout.bytes_per_sample * BITS_PER_BYTE;
    if layout.spec.sample_format != WavSampleFormat::Int || !UNPACKED_SAMPLE_BITS.contains(&bits) {
        return Err(format!(
            "{} holds {bits}-bit {:?} samples, and the player reads 16, 24 or 32-bit integer WAVs",
            path.display(),
            layout.spec.sample_format
        ));
    }
    let channels = layout.spec.channels;
    let reader = WavEditUnits {
        path: path.to_path_buf(),
        file: File::open(path).map_err(|error| format!("{}: {error}", path.display()))?,
        data_offset: layout.data.body_offset(),
        block_align: layout.block_align(),
        sample_frames: layout.frames(),
        sample_rate: u64::from(layout.spec.sample_rate),
        edit_rate: (numerator, denominator),
    };
    let sample_frames_per_edit_unit = (reader.sample_rate * denominator).div_ceil(numerator);
    let edit_units = u32::try_from(reader.edit_units())
        .map_err(|_| format!("{} is longer than the player reads", path.display()))?;
    let (speakers, lane_names) = unlabelled_speakers(usize::from(channels));
    Ok(SoundReader {
        layout: AudioLayout {
            channels,
            bits,
            bytes_per_edit_unit: (sample_frames_per_edit_unit * reader.block_align) as usize,
            edit_units,
            sample_rate: layout.spec.sample_rate,
            speakers,
            lane_names: Arc::from(lane_names),
        },
        reader: EditUnitReader::Wav(reader),
        decrypt: None,
    })
}

fn open_reader(
    path: &Path,
    edit_rate: Option<(i32, i32)>,
    key: Option<&SoundContentKey>,
) -> Result<SoundReader, String> {
    // an unreadable file is left for the MXF reader to name
    if crate::wav_io::has_wave_form_type(path).unwrap_or(false) {
        return open_wav(path, edit_rate);
    }
    let mut reader = PcmReader::open(path, edit_rate)?;
    let descriptor = reader
        .audio_descriptor()
        .map_err(|error| error.to_string())?;
    if descriptor.channel_count == 0 || descriptor.block_align == 0 {
        return Err(format!("{} names no pcm", path.display()));
    }
    let (sample_frames_per_edit_unit, edit_units) = reader.edit_units(&descriptor);
    let (speakers, lane_names) = source_speakers(&mut reader, descriptor.channel_count as usize);
    let decrypt = match key {
        Some(SoundContentKey(key)) => {
            let mut decrypt = AesDecContext::new();
            decrypt
                .init_key(key)
                .map_err(|error| format!("AES key init failed: {error}"))?;
            Some(decrypt)
        }
        None => None,
    };
    Ok(SoundReader {
        reader: EditUnitReader::Mxf(reader),
        layout: AudioLayout {
            channels: descriptor.channel_count as u16,
            bits: descriptor.quantization_bits as u16,
            bytes_per_edit_unit: descriptor.block_align as usize
                * sample_frames_per_edit_unit as usize,
            edit_units,
            sample_rate: sample_rate_of(&descriptor),
            speakers,
            lane_names: Arc::from(lane_names),
        },
        decrypt,
    })
}

// a file with no MCA channel labels is taken to be in the default DCP order
fn source_speakers(reader: &mut PcmReader, channels: usize) -> (Vec<Option<Speaker>>, Vec<String>) {
    let labels = reader.mca_label_subdescriptors().unwrap_or_else(|error| {
        tracing::warn!("preview sound: MCA labels unreadable, assuming the default order: {error}");
        Vec::new()
    });
    let labelled: Vec<(usize, String)> = labels
        .into_iter()
        .filter(|label| label.kind == McaLabelKind::AudioChannel)
        .filter_map(|label| {
            let index = label.channel_id?.checked_sub(1)? as usize;
            Some((index, label.tag_symbol))
        })
        .collect();
    if labelled.is_empty() {
        return unlabelled_speakers(channels);
    }
    let mut speakers = vec![None; channels];
    let mut names: Vec<String> = (0..channels).map(numbered_channel_label).collect();
    for (index, tag_symbol) in labelled {
        if index < channels {
            speakers[index] = speaker_of_mca_tag(&tag_symbol);
            names[index] = mca_lane(&tag_symbol).to_string();
        }
    }
    (speakers, names)
}

fn unlabelled_speakers(channels: usize) -> (Vec<Option<Speaker>>, Vec<String>) {
    let speakers = default_dcp_speakers(channels);
    let names = speakers
        .iter()
        .enumerate()
        .map(|(index, speaker)| match speaker {
            Some(speaker) => speaker.lane_name().to_string(),
            None => numbered_channel_label(index),
        })
        .collect();
    (speakers, names)
}

fn default_dcp_speakers(channels: usize) -> Vec<Option<Speaker>> {
    (0..channels)
        .map(|channel| DEFAULT_DCP_SPEAKERS.get(channel).copied().flatten())
        .collect()
}

fn mca_lane(tag_symbol: &str) -> &str {
    tag_symbol
        .strip_prefix(MCA_CHANNEL_TAG_PREFIX)
        .unwrap_or(tag_symbol)
}

// the symbols libdcp's mca_id_to_channel reads
fn speaker_of_mca_tag(tag_symbol: &str) -> Option<Speaker> {
    let symbol = mca_lane(tag_symbol).to_ascii_lowercase();
    match symbol.as_str() {
        "l" => Some(Speaker::Left),
        "r" => Some(Speaker::Right),
        "c" => Some(Speaker::Centre),
        "lfe" => Some(Speaker::LowFrequency),
        "ls" | "lss" => Some(Speaker::LeftSurround),
        "rs" | "rss" => Some(Speaker::RightSurround),
        "lrs" | "lsr" => Some(Speaker::LeftRearSurround),
        "rrs" | "rsr" => Some(Speaker::RightRearSurround),
        _ => None,
    }
}

fn sample_rate_of(descriptor: &asdcplib::pcm::AudioDescriptor) -> u32 {
    let rate = descriptor.audio_sampling_rate;
    if rate.numerator <= 0 || rate.denominator <= 0 {
        return DEFAULT_SAMPLE_RATE;
    }
    (rate.numerator as u32) / (rate.denominator as u32)
}

fn frames_per_edit_unit(sampling_rate: asdcplib::Rational, edit: asdcplib::Rational) -> u32 {
    let sample_rate = sampling_rate.numerator.max(0) as u64;
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

fn route(
    source: &[f32],
    speakers: &[Option<Speaker>],
    device_order: &[Speaker],
    out: &mut Vec<f32>,
) {
    if speakers.is_empty() {
        return;
    }
    let outputs: Vec<Option<usize>> = speakers
        .iter()
        .map(|speaker| speaker.and_then(|speaker| device_position(speaker, device_order)))
        .collect();
    for frame in source.chunks_exact(speakers.len()) {
        let start = out.len();
        out.resize(start + device_order.len(), 0.0);
        let device_frame = &mut out[start..];
        for (sample, output) in frame.iter().zip(&outputs) {
            if let Some(output) = output {
                device_frame[*output] += sample;
            }
        }
        for sample in device_frame {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
}

// a 5.1 device has no rear surrounds
fn device_position(speaker: Speaker, device_order: &[Speaker]) -> Option<usize> {
    let position = |wanted: Speaker| device_order.iter().position(|placed| *placed == wanted);
    position(speaker).or_else(|| match speaker {
        Speaker::LeftRearSurround => position(Speaker::LeftSurround),
        Speaker::RightRearSurround => position(Speaker::RightSurround),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::timeline::Timeline;
    use super::super::timeline::tests::content_keys;
    use super::*;
    use crate::audio_levels::SILENCE_FLOOR_DBFS;
    use crate::mxf_unwrap::tests::{FRAME_COUNT, wrap, write_frames};
    use crate::mxf_wrap::{EssenceType, MxfEncryption, MxfStandard, MxfWrapOptions, mxf_wrap};
    use crate::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};
    use std::collections::BTreeMap;

    const SOUND_KEY: [u8; 16] = [0x33; 16];
    const SOUND_KEY_ID: [u8; 16] = [0x44; 16];
    const SOUND_CHANNELS: u16 = 2;
    const SOUND_BITS: u16 = 16;
    const EDIT_UNITS_PER_SECOND: u32 = 24;
    const SAMPLE_FRAMES_PER_EDIT_UNIT: usize = 2_000;
    const SOUND_SECONDS: u32 = 1;
    const CPL_ID: &str = "cc10cc10-0000-4000-8000-000000000000";
    const PICTURE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SOUND_ID: &str = "55555555-5555-4555-8555-555555555555";
    const PICTURE_SIZE: u32 = 64;
    const NO_DELAY: f64 = 0.0;

    fn sample(index: usize) -> i16 {
        const SAMPLE_STEP: usize = 37;
        ((index * SAMPLE_STEP) % usize::from(u16::MAX)) as i16
    }

    // returns the package and the pcm bytes its first sound edit unit holds
    fn package_with_encrypted_sound(directory: &Path) -> (PathBuf, Vec<u8>) {
        let package = directory.join("package");
        std::fs::create_dir_all(&package).unwrap();
        let (frames, _) = write_frames(directory, "picture");
        wrap(frames, package.join("picture.mxf"), None);

        let wav = directory.join("sound.wav");
        let spec = hound::WavSpec {
            channels: SOUND_CHANNELS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            bits_per_sample: SOUND_BITS,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
        let samples = (DEFAULT_SAMPLE_RATE * SOUND_SECONDS) as usize * usize::from(SOUND_CHANNELS);
        for index in 0..samples {
            writer.write_sample(sample(index)).unwrap();
        }
        writer.finalize().unwrap();
        let first_edit_unit: Vec<u8> = (0..SAMPLE_FRAMES_PER_EDIT_UNIT
            * usize::from(SOUND_CHANNELS))
            .flat_map(|index| sample(index).to_le_bytes())
            .collect();

        let track = mxf_wrap(&MxfWrapOptions {
            input_files: vec![wav],
            output: package.join("sound.mxf"),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::AsDcp,
            fps_num: EDIT_UNITS_PER_SECOND,
            fps_den: 1,
            partition_size: 0,
            encryption: Some(MxfEncryption {
                content_key: SOUND_KEY,
                key_id: SOUND_KEY_ID,
            }),
            mca_config: None,
            resource_ids: Vec::new(),
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        });
        assert!(track.success, "sound wrap failed: {}", track.error);

        let asset = |id: &str, path: &str| AssetMapAsset {
            id: id.into(),
            path: path.into(),
            ..Default::default()
        };
        std::fs::write(
            package.join("ASSETMAP.xml"),
            AssetMap {
                uuid: "bbbbbbbb-0000-4000-8000-000000000000".into(),
                namespace: ns::AM_SMPTE.into(),
                assets: vec![
                    asset(CPL_ID, "CPL.xml"),
                    asset(PICTURE_ID, "picture.mxf"),
                    asset(SOUND_ID, "sound.mxf"),
                ],
                ..Default::default()
            }
            .to_xml(),
        )
        .unwrap();
        std::fs::write(
            package.join("CPL.xml"),
            DcpCpl {
                uuid: CPL_ID.into(),
                namespace: ns::CPL_SMPTE.into(),
                title: "Encrypted Sound".into(),
                reels: vec![DcpCplReel {
                    reel_id: "aaaaaaaa-0000-4000-8000-000000000000".into(),
                    picture_id: PICTURE_ID.into(),
                    picture_edit_rate_num: EDIT_UNITS_PER_SECOND,
                    picture_edit_rate_den: 1,
                    picture_duration: FRAME_COUNT as u64,
                    picture_width: PICTURE_SIZE,
                    picture_height: PICTURE_SIZE,
                    sound_id: Some(SOUND_ID.into()),
                    sound_edit_rate_num: EDIT_UNITS_PER_SECOND,
                    sound_edit_rate_den: 1,
                    sound_duration: FRAME_COUNT as u64,
                    sound_key_id: Some(uuid::Uuid::from_bytes(SOUND_KEY_ID).to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .to_xml(),
        )
        .unwrap();
        (package, first_edit_unit)
    }

    #[test]
    fn encrypted_sound_reads_back_through_the_feeder_with_its_key() {
        let directory = tempfile::tempdir().unwrap();
        let (package, first_edit_unit) = package_with_encrypted_sound(directory.path());
        let keys = content_keys(directory.path(), &[(SOUND_KEY_ID, SOUND_KEY)]);

        let timeline = Timeline::open(&package, Some(&keys), &[]).unwrap();
        let [sound] = timeline.sound.main_sound.segments.as_slice() else {
            panic!("the package names one sound reel");
        };
        let mut opened = open_reader(&sound.segment.path, None, sound.key.as_ref()).unwrap();
        assert!(
            opened.decrypt.is_some(),
            "the feeder builds no decrypt context"
        );
        let mut essence = vec![0u8; opened.layout.bytes_per_edit_unit];
        let read = opened
            .reader
            .read_frame(0, &mut essence, opened.decrypt.as_mut())
            .unwrap();
        assert!(
            essence[..read] == first_edit_unit,
            "the first edit unit differs from the WAV"
        );
    }

    // 48000 Hz at 24000/1001 is 2002 sample frames an edit unit, which an AS-DCP edit unit never is
    const IMF_EDIT_RATE: (i32, i32) = (24000, 1001);
    const IMF_SAMPLE_FRAMES_PER_EDIT_UNIT: usize = 2_002;
    const IMF_EDIT_UNITS: usize = 12;
    const TWENTY_FOUR_BIT_FULL_SCALE: f32 = 8_388_608.0;

    fn as02_sound(directory: &Path) -> PathBuf {
        let wav = directory.join("imf-sound.wav");
        let spec = hound::WavSpec {
            channels: SOUND_CHANNELS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
        let samples =
            IMF_EDIT_UNITS * IMF_SAMPLE_FRAMES_PER_EDIT_UNIT * usize::from(SOUND_CHANNELS);
        for index in 0..samples {
            writer.write_sample(i32::from(sample(index))).unwrap();
        }
        writer.finalize().unwrap();
        let output = directory.join("imf-sound.mxf");
        let track = mxf_wrap(&MxfWrapOptions {
            input_files: vec![wav],
            output: output.clone(),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::As02,
            fps_num: IMF_EDIT_RATE.0 as u32,
            fps_den: IMF_EDIT_RATE.1 as u32,
            partition_size: 0,
            encryption: None,
            mca_config: None,
            resource_ids: Vec::new(),
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        });
        assert!(track.success, "AS-02 sound wrap failed: {}", track.error);
        output
    }

    #[test]
    fn as02_sound_reads_in_edit_units_of_the_cpl_edit_rate() {
        let directory = tempfile::tempdir().unwrap();
        let sound = as02_sound(directory.path());

        let mut opened = open_reader(&sound, Some(IMF_EDIT_RATE), None).unwrap();

        assert_eq!(opened.layout.edit_units as usize, IMF_EDIT_UNITS);
        let mut essence = vec![0u8; opened.layout.bytes_per_edit_unit];
        let read = opened.reader.read_frame(1, &mut essence, None).unwrap();
        let mut interleaved = Vec::new();
        unpack_pcm(
            &essence[..read],
            usize::from(SOUND_CHANNELS),
            opened.layout.bits,
            &mut interleaved,
        );
        assert_eq!(
            interleaved.len(),
            IMF_SAMPLE_FRAMES_PER_EDIT_UNIT * usize::from(SOUND_CHANNELS)
        );
        let second_edit_unit_start = IMF_SAMPLE_FRAMES_PER_EDIT_UNIT * usize::from(SOUND_CHANNELS);
        let expected_first =
            i32::from(sample(second_edit_unit_start)) as f32 / TWENTY_FOUR_BIT_FULL_SCALE;
        assert!(
            (interleaved[0] - expected_first).abs() < 1e-6,
            "{} against {expected_first}",
            interleaved[0]
        );
    }

    #[test]
    fn an_imp_plays_its_as02_sound_for_every_picture_frame() {
        use crate::imp_fixture::{
            FRAMES, SAMPLE_FRAMES_PER_EDIT_UNIT, SOUND_CHANNELS as IMP_CHANNELS, write_imp,
        };
        let directory = tempfile::tempdir().unwrap();
        let cpl = write_imp(&directory.path().join("imp"), false);

        let timeline = Timeline::open(&cpl, None, &[]).unwrap();
        let reels = reels_of(
            &timeline.sound.main_sound.segments,
            SoundEssence::Pcm,
            timeline.fps,
        );

        let [reel] = reels.as_slice() else {
            panic!("the IMP names one sound track file");
        };
        assert_eq!(reel.frame_count, FRAMES);
        let mut opened = open_reader(&reel.path, reel.edit_rate, None).unwrap();
        let mut essence = vec![0u8; opened.layout.bytes_per_edit_unit];
        let read = opened.reader.read_frame(1, &mut essence, None).unwrap();
        let mut interleaved = Vec::new();
        unpack_pcm(
            &essence[..read],
            usize::from(IMP_CHANNELS),
            opened.layout.bits,
            &mut interleaved,
        );
        let second_edit_unit_start = SAMPLE_FRAMES_PER_EDIT_UNIT * usize::from(IMP_CHANNELS);
        let expected =
            crate::imp_fixture::sample(second_edit_unit_start) as f32 / TWENTY_FOUR_BIT_FULL_SCALE;
        assert!(
            (interleaved[0] - expected).abs() < 1e-6,
            "{} against {expected}",
            interleaved[0]
        );
    }

    #[test]
    fn as02_sound_without_an_edit_rate_is_refused_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let sound = as02_sound(directory.path());

        let error = open_reader(&sound, None, None).err().unwrap();

        assert!(error.contains("AS-02 sound"), "{error}");
    }

    #[test]
    fn encrypted_sound_with_no_keys_fails_the_load() {
        let directory = tempfile::tempdir().unwrap();
        let (package, _) = package_with_encrypted_sound(directory.path());
        let error = Timeline::open(&package, None, &[])
            .err()
            .expect("the sound has no key");
        assert_eq!(
            error,
            format!(
                "{} is encrypted sound and the preview holds no content key for it",
                package.join("sound.mxf").display()
            )
        );
    }

    #[test]
    fn keys_that_do_not_cover_the_sound_name_its_key_id() {
        let directory = tempfile::tempdir().unwrap();
        let (package, _) = package_with_encrypted_sound(directory.path());
        let keys = content_keys(directory.path(), &[([0x66; 16], SOUND_KEY)]);
        let error = Timeline::open(&package, Some(&keys), &[])
            .err()
            .expect("the keys do not cover the sound");
        assert_eq!(
            error,
            format!(
                "KDM/keys do not cover sound KeyId {}",
                uuid::Uuid::from_bytes(SOUND_KEY_ID)
            )
        );
    }

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
        assert_eq!(frame_at_queue_end(100.0, 0, 24.0, 48_000, NO_DELAY), 100);
        // one edit unit of 24 fps sound is 2000 sample frames
        assert_eq!(
            frame_at_queue_end(100.0, 2_000, 24.0, 48_000, NO_DELAY),
            101
        );
        assert_eq!(
            frame_at_queue_end(100.0, 1_999, 24.0, 48_000, NO_DELAY),
            100
        );
        assert_eq!(
            frame_at_queue_end(100.0, 96_000, 24.0, 48_000, NO_DELAY),
            148
        );
        assert_eq!(
            frame_at_queue_end(100.0, 96_000, 0.0, 48_000, NO_DELAY),
            100
        );
        assert_eq!(frame_at_queue_end(100.0, 96_000, 24.0, 0, NO_DELAY), 100);
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
        let mut resampler = Resampler::new(48_000, 24_000, STEREO_CHANNELS);
        let source = [0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        assert_eq!(out, vec![0.0, 10.0, 2.0, 12.0]);
    }

    #[test]
    fn doubling_the_rate_interpolates_between_sample_frames() {
        let mut resampler = Resampler::new(24_000, 48_000, STEREO_CHANNELS);
        let source = [0.0, 0.0, 1.0, -1.0];
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5]);
    }

    #[test]
    fn the_resampler_carries_a_partial_step_into_the_next_block() {
        let mut resampler = Resampler::new(48_000, 32_000, STEREO_CHANNELS);
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
        let mut resampler = Resampler::new(48_000, 44_100, STEREO_CHANNELS);
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

    // channel n holds n / 64, exact in f32
    fn numbered_frame(channels: usize) -> Vec<f32> {
        const STEP: f32 = 64.0;
        (1..=channels)
            .map(|channel| channel as f32 / STEP)
            .collect()
    }

    fn numbered(channel: usize) -> f32 {
        numbered_frame(channel)[channel - 1]
    }

    const ALSA_FIVE_POINT_ONE: [Speaker; 6] = [
        Speaker::Left,
        Speaker::Right,
        Speaker::LeftSurround,
        Speaker::RightSurround,
        Speaker::Centre,
        Speaker::LowFrequency,
    ];
    const ALSA_SEVEN_POINT_ONE: [Speaker; 8] = [
        Speaker::Left,
        Speaker::Right,
        Speaker::LeftRearSurround,
        Speaker::RightRearSurround,
        Speaker::Centre,
        Speaker::LowFrequency,
        Speaker::LeftSurround,
        Speaker::RightSurround,
    ];

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_devices_take_the_alsa_channel_order() {
        assert_eq!(FIVE_POINT_ONE_DEVICE_ORDER, ALSA_FIVE_POINT_ONE);
        assert_eq!(SEVEN_POINT_ONE_DEVICE_ORDER, ALSA_SEVEN_POINT_ONE);
    }

    #[test]
    fn sixteen_channels_route_to_seven_point_one_by_label() {
        let mut out = Vec::new();
        route(
            &numbered_frame(16),
            &default_dcp_speakers(16),
            &ALSA_SEVEN_POINT_ONE,
            &mut out,
        );
        // HI, VI-N, sync, motion and sign language play nowhere
        let expected = [1, 2, 11, 12, 3, 4, 5, 6].map(numbered);
        assert_eq!(out, expected);
    }

    #[test]
    fn sixteen_channels_on_five_point_one_fold_the_rears_into_the_surrounds() {
        let mut out = Vec::new();
        route(
            &numbered_frame(16),
            &default_dcp_speakers(16),
            &ALSA_FIVE_POINT_ONE,
            &mut out,
        );
        let expected = [
            numbered(1),
            numbered(2),
            numbered(5) + numbered(11),
            numbered(6) + numbered(12),
            numbered(3),
            numbered(4),
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn six_channels_route_to_five_point_one() {
        let mut out = Vec::new();
        route(
            &numbered_frame(6),
            &default_dcp_speakers(6),
            &ALSA_FIVE_POINT_ONE,
            &mut out,
        );
        assert_eq!(out, [1, 2, 5, 6, 3, 4].map(numbered));
    }

    #[test]
    fn six_channels_on_seven_point_one_leave_the_rears_silent() {
        let mut out = Vec::new();
        route(
            &numbered_frame(6),
            &default_dcp_speakers(6),
            &ALSA_SEVEN_POINT_ONE,
            &mut out,
        );
        let expected = [
            numbered(1),
            numbered(2),
            0.0,
            0.0,
            numbered(3),
            numbered(4),
            numbered(5),
            numbered(6),
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn routing_keeps_sample_frames_apart() {
        let speakers = default_dcp_speakers(2);
        let mut out = Vec::new();
        route(
            &[0.25, -0.25, 0.5, -0.5],
            &speakers,
            &ALSA_FIVE_POINT_ONE,
            &mut out,
        );
        assert_eq!(
            out,
            [
                0.25, -0.25, 0.0, 0.0, 0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.0, 0.0
            ]
        );
    }

    #[test]
    fn the_stereo_mix_is_the_downmix_whatever_the_labels_say() {
        let source = numbered_frame(16);
        let mut stereo = Vec::new();
        OutputMix::StereoDownmix.apply(&source, &default_dcp_speakers(16), &mut stereo);
        let centre = CENTRE_AND_SURROUND * numbered(3);
        let left = numbered(1) + centre + CENTRE_AND_SURROUND * numbered(5);
        let right = numbered(2) + centre + CENTRE_AND_SURROUND * numbered(6);
        assert_eq!(stereo, [left, right]);
    }

    #[test]
    fn mca_tags_name_their_speakers() {
        assert_eq!(speaker_of_mca_tag("chL"), Some(Speaker::Left));
        assert_eq!(speaker_of_mca_tag("chLFE"), Some(Speaker::LowFrequency));
        assert_eq!(speaker_of_mca_tag("chLss"), Some(Speaker::LeftSurround));
        assert_eq!(
            speaker_of_mca_tag("chRrs"),
            Some(Speaker::RightRearSurround)
        );
        assert_eq!(speaker_of_mca_tag("chHI"), None);
        assert_eq!(speaker_of_mca_tag("chVIN"), None);
    }

    fn wrapped_sound(directory: &Path, channels: u16, labels: Option<&str>) -> PathBuf {
        let one_second = DEFAULT_SAMPLE_RATE as usize * usize::from(channels);
        wrapped_samples(directory, "sound", channels, labels, &vec![0; one_second])
    }

    fn wrapped_samples(
        directory: &Path,
        name: &str,
        channels: u16,
        labels: Option<&str>,
        samples: &[i16],
    ) -> PathBuf {
        let wav = directory.join(format!("{name}.wav"));
        let spec = hound::WavSpec {
            channels,
            sample_rate: DEFAULT_SAMPLE_RATE,
            bits_per_sample: SOUND_BITS,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
        let output = directory.join(format!("{name}.mxf"));
        let track = mxf_wrap(&MxfWrapOptions {
            input_files: vec![wav],
            output: output.clone(),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::AsDcp,
            fps_num: EDIT_UNITS_PER_SECOND,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: labels.map(|labels| crate::mxf_wrap::McaConfig {
                labels: labels.to_string(),
                spoken_language: None,
                soundfield_group: None,
            }),
            resource_ids: Vec::new(),
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        });
        assert!(track.success, "sound wrap failed: {}", track.error);
        output
    }

    #[test]
    fn the_feeder_reads_speakers_from_the_mca_labels() {
        let directory = tempfile::tempdir().unwrap();
        let labels = crate::mca::soundfield_to_mca_config(&crate::mca::soundfield_71()).unwrap();
        let sound = wrapped_sound(directory.path(), 8, Some(&labels));
        let opened = open_reader(&sound, None, None).unwrap();
        // the default order would make channels 7 and 8 HI and VI-N
        assert_eq!(
            opened.layout.speakers,
            [
                Speaker::Left,
                Speaker::Right,
                Speaker::Centre,
                Speaker::LowFrequency,
                Speaker::LeftSurround,
                Speaker::RightSurround,
                Speaker::LeftRearSurround,
                Speaker::RightRearSurround,
            ]
            .map(Some)
        );
    }

    #[test]
    fn unlabelled_sound_takes_the_default_dcp_order() {
        let directory = tempfile::tempdir().unwrap();
        let sound = wrapped_sound(directory.path(), 8, None);
        let opened = open_reader(&sound, None, None).unwrap();
        assert_eq!(opened.layout.speakers, default_dcp_speakers(8));
        assert_eq!(opened.layout.speakers[6], None, "channel 7 is HI");
        assert_eq!(
            &*opened.layout.lane_names,
            ["L", "R", "C", "LFE", "Ls", "Rs", "Ch 7", "Ch 8"]
        );
    }

    #[test]
    fn the_meter_names_labelled_channels_by_their_mca_tag() {
        let directory = tempfile::tempdir().unwrap();
        let labels =
            crate::mca::soundfield_to_mca_config(&crate::mca::soundfield_51_with_hi_vi()).unwrap();
        let sound = wrapped_sound(directory.path(), 8, Some(&labels));
        let opened = open_reader(&sound, None, None).unwrap();
        assert_eq!(
            &*opened.layout.lane_names,
            ["L", "R", "C", "LFE", "Ls", "Rs", "HI", "VIN"]
        );
    }

    fn offered(channels: u16, format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(44_100),
            SampleRate(DEFAULT_SAMPLE_RATE),
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn automatic_takes_the_widest_layout_the_device_offers() {
        let stereo = offered(2, SampleFormat::F32);
        let six = offered(6, SampleFormat::I16);
        let eight = offered(8, SampleFormat::F32);
        assert_eq!(
            automatic_layout(&[stereo, six, eight]),
            SoundOutputLayout::SevenPointOne
        );
        assert_eq!(
            automatic_layout(&[stereo, six]),
            SoundOutputLayout::FivePointOne
        );
        assert_eq!(automatic_layout(&[stereo]), SoundOutputLayout::Stereo);
        assert_eq!(automatic_layout(&[]), SoundOutputLayout::Stereo);
        // the feeder writes only f32 and i16
        assert_eq!(
            automatic_layout(&[stereo, offered(8, SampleFormat::U8)]),
            SoundOutputLayout::Stereo
        );
    }

    #[test]
    fn a_surround_layout_opens_with_its_channel_count() {
        let ranges = [offered(2, SampleFormat::F32), offered(6, SampleFormat::F32)];
        let mut warnings = Vec::new();
        let (config, mix) = surround_config(
            &ranges,
            SoundOutputLayout::FivePointOne,
            DEFAULT_SAMPLE_RATE,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(config.channels(), 6);
        assert_eq!(config.sample_rate(), SampleRate(DEFAULT_SAMPLE_RATE));
        assert_eq!(mix, OutputMix::Routed(&FIVE_POINT_ONE_DEVICE_ORDER));
        // a rate the device lacks opens at its highest
        let (config, _) = surround_config(
            &ranges,
            SoundOutputLayout::FivePointOne,
            96_000,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(config.sample_rate(), SampleRate(DEFAULT_SAMPLE_RATE));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_layout_the_device_lacks_plays_the_stereo_downmix() {
        let ranges = [offered(2, SampleFormat::F32), offered(6, SampleFormat::F32)];
        let mut warnings = Vec::new();
        let opened = |layout, warnings: &mut Vec<String>| {
            surround_config(&ranges, layout, DEFAULT_SAMPLE_RATE, warnings)
        };
        assert!(opened(SoundOutputLayout::SevenPointOne, &mut warnings).is_none());
        assert_eq!(
            warnings,
            ["the sound device offers no 8 channel output, sound plays as a stereo downmix"]
        );
        warnings.clear();
        assert!(opened(SoundOutputLayout::Stereo, &mut warnings).is_none());
        let (config, _) = opened(SoundOutputLayout::Automatic, &mut warnings).unwrap();
        assert_eq!(config.channels(), 6);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_delayed_sound_starts_earlier_in_the_composition() {
        // 250 ms at 24 fps is 6 edit units
        assert_eq!(
            sound_start(48.0, 24.0, 0.25, 48_000, 48_000),
            SoundStart {
                leading_silence_sample_frames: 0,
                edit_unit: 42,
                skipped_source_sample_frames: 0,
            }
        );
        // an advanced sound starts later
        assert_eq!(sound_start(48.0, 24.0, -0.5, 48_000, 48_000).edit_unit, 60);
        assert_eq!(
            sound_start(48.0, 24.0, NO_DELAY, 48_000, 48_000),
            SoundStart {
                leading_silence_sample_frames: 0,
                edit_unit: 48,
                skipped_source_sample_frames: 0,
            }
        );
    }

    #[test]
    fn a_delay_inside_an_edit_unit_skips_source_samples() {
        // 10 ms back from frame 48 is 0.76 of edit unit 47, 1520 sample frames into it at 48 kHz
        assert_eq!(
            sound_start(48.0, 24.0, 0.01, 48_000, 44_100),
            SoundStart {
                leading_silence_sample_frames: 0,
                edit_unit: 47,
                skipped_source_sample_frames: 1_520,
            }
        );
    }

    #[test]
    fn a_delay_before_the_first_frame_plays_silence_at_the_device_rate() {
        assert_eq!(
            sound_start(0.0, 24.0, 0.1, 48_000, 44_100),
            SoundStart {
                leading_silence_sample_frames: 4_410,
                edit_unit: 0,
                skipped_source_sample_frames: 0,
            }
        );
        // frame 1 is 41.7 ms in, so 58.3 ms of silence is left
        assert_eq!(
            sound_start(1.0, 24.0, 0.1, 48_000, 48_000).leading_silence_sample_frames,
            2_800
        );
    }

    #[test]
    fn the_picture_runs_the_delay_ahead_of_the_sound_it_plays_over() {
        let (seek_frame, fps, sample_rate, delay) = (48.0, 24.0, 48_000, 0.25);
        let played_sample_frames = 24_000;
        let picture_seconds =
            seek_frame / fps + played_sample_frames as f64 / f64::from(sample_rate);
        let sound_edit_unit =
            frame_at_queue_end(seek_frame, played_sample_frames, fps, sample_rate, delay);
        assert_eq!(picture_seconds, 2.5);
        assert_eq!(sound_edit_unit as f64 / fps, picture_seconds - delay);
        // at the seek itself the device is on the edit unit the feeder starts from
        let start = sound_start(seek_frame, fps, delay, sample_rate, sample_rate);
        assert_eq!(
            frame_at_queue_end(seek_frame, 0, fps, sample_rate, delay),
            start.edit_unit
        );
    }

    #[test]
    fn leading_silence_holds_the_queue_end_at_the_first_edit_unit() {
        let start = sound_start(0.0, 24.0, 2.0, 48_000, 48_000);
        assert_eq!(start.leading_silence_sample_frames, 96_000);
        let silence = start.leading_silence_sample_frames as u64;
        assert_eq!(frame_at_queue_end(0.0, silence / 2, 24.0, 48_000, 2.0), 0);
        assert_eq!(frame_at_queue_end(0.0, silence, 24.0, 48_000, 2.0), 0);
        assert_eq!(
            frame_at_queue_end(0.0, silence + 2_000, 24.0, 48_000, 2.0),
            1
        );
    }

    #[test]
    fn the_resampler_keeps_six_channels_apart() {
        let mut resampler = Resampler::new(48_000, 24_000, 6);
        let source: Vec<f32> = (0..24).map(|index| index as f32).collect();
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        assert_eq!(
            out,
            [
                0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0
            ]
        );
    }

    #[test]
    #[ignore = "needs a sound device"]
    fn lists_the_output_devices() {
        let names = sound_output_device_names().unwrap();
        eprintln!("output devices: {names:#?}");
        assert!(!names.is_empty());
    }

    const COMPOSITION_FPS: f64 = 24.0;
    const SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24: usize = 2_000;

    fn composition_of(sound: Option<&Path>, frame_count: u64) -> SoundComposition {
        let segments: Vec<KeyedSoundSegment> = sound
            .map(|path| KeyedSoundSegment {
                segment: SoundSegment {
                    path: path.to_path_buf(),
                    trim: None,
                    edit_rate: None,
                },
                key: None,
            })
            .into_iter()
            .collect();
        let main_sound = reels_of(&segments, SoundEssence::Pcm, COMPOSITION_FPS);
        SoundComposition {
            playing: (!main_sound.is_empty()).then_some(SoundSource::MainSound),
            main_sound,
            atmos: Vec::new(),
            fps: COMPOSITION_FPS,
            first_frame: 0,
            frame_count,
        }
    }

    fn ramp(edit_units: usize, first: i16) -> Vec<i16> {
        let samples = edit_units * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24 * STEREO_CHANNELS;
        (0..samples)
            .map(|index| first + (index % 997) as i16)
            .collect()
    }

    fn as_played(samples: &[i16]) -> Vec<f32> {
        samples
            .iter()
            .map(|sample| f32::from(*sample) / f32::from(i16::MAX))
            .collect()
    }

    // what the device would play, taking the buffer as fast as the feeder fills it
    fn drain_until_the_feeder_stops(feeder: &mut Feeder) -> Vec<f32> {
        let mut played = Vec::new();
        loop {
            feeder.fill();
            let drained: Vec<f32> = feeder.shared.buffer.lock().unwrap().drain(..).collect();
            if drained.is_empty() {
                return played;
            }
            let channels = feeder.shared.output_channels();
            feeder
                .shared
                .emitted_sample_frames
                .fetch_add((drained.len() / channels) as u64, Ordering::AcqRel);
            played.extend(drained);
        }
    }

    #[test]
    fn the_queued_composition_sound_follows_with_no_gap() {
        const FIRST_SOUND_EDIT_UNITS: usize = 6;
        // two edit units longer than its sound, which the feeder fills with silence
        const FIRST_PICTURE_FRAMES: u64 = 8;
        const NEXT_EDIT_UNITS: usize = 6;
        let directory = tempfile::tempdir().unwrap();
        let first_samples = ramp(FIRST_SOUND_EDIT_UNITS, 1);
        let next_samples = ramp(NEXT_EDIT_UNITS, 5_000);
        let first = wrapped_samples(directory.path(), "first", 2, None, &first_samples);
        let next = wrapped_samples(directory.path(), "next", 2, None, &next_samples);

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition_of(Some(&first), FIRST_PICTURE_FRAMES);
        feeder.queue(Some(composition_of(Some(&next), NEXT_EDIT_UNITS as u64)));
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);

        let padding = (FIRST_PICTURE_FRAMES as usize - FIRST_SOUND_EDIT_UNITS)
            * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24
            * STEREO_CHANNELS;
        let mut expected = as_played(&first_samples);
        expected.extend(std::iter::repeat_n(0.0, padding));
        expected.extend(as_played(&next_samples));
        assert_eq!(played.len(), expected.len());
        assert!(
            played == expected,
            "the sound differs from the two compositions end to end"
        );
    }

    #[test]
    fn a_frame_range_cuts_the_sound_at_its_frame_boundaries_to_the_sample() {
        const REEL_EDIT_UNITS: usize = 4;
        // from the middle of the first reel into the second
        const FIRST_FRAME: u64 = 2;
        const FRAME_COUNT: u64 = 4;
        let directory = tempfile::tempdir().unwrap();
        let first_samples = ramp(REEL_EDIT_UNITS, 1);
        let second_samples = ramp(REEL_EDIT_UNITS, 5_000);
        let segments = [
            wrapped_samples(directory.path(), "first", 2, None, &first_samples),
            wrapped_samples(directory.path(), "second", 2, None, &second_samples),
        ]
        .into_iter()
        .map(|path| KeyedSoundSegment {
            segment: SoundSegment {
                path,
                trim: None,
                edit_rate: None,
            },
            key: None,
        })
        .collect();

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = SoundComposition::of(
            &CompositionSounds {
                main_sound: CompositionSound {
                    segments,
                    essence: SoundEssence::Pcm,
                },
                atmos: None,
            },
            SoundSource::Atmos,
            PlayedFrames {
                fps: COMPOSITION_FPS,
                first_frame: FIRST_FRAME,
                frame_count: FRAME_COUNT,
            },
        );
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);

        let samples_per_frame = SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24 * STEREO_CHANNELS;
        let mut both_reels = first_samples;
        both_reels.extend(second_samples);
        let start = FIRST_FRAME as usize * samples_per_frame;
        let end = (FIRST_FRAME + FRAME_COUNT) as usize * samples_per_frame;
        let expected = as_played(&both_reels[start..end]);
        assert_eq!(played.len(), expected.len());
        assert_eq!(
            played[..samples_per_frame],
            expected[..samples_per_frame],
            "the first frame of sound is not the in frame's"
        );
        assert!(played == expected, "the sound inside the range differs");
    }

    #[test]
    fn a_seek_before_the_boundary_takes_back_the_queued_sound_already_read() {
        let directory = tempfile::tempdir().unwrap();
        let first_samples = ramp(2, 1);
        let next_samples = ramp(2, 5_000);
        let first = wrapped_samples(directory.path(), "first", 2, None, &first_samples);
        let next = wrapped_samples(directory.path(), "next", 2, None, &next_samples);

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition_of(Some(&first), 2);
        feeder.queue(Some(composition_of(Some(&next), 2)));
        feeder.seek(0);
        feeder.fill();
        assert!(
            feeder.finished.is_some(),
            "the fill reached the queued sound"
        );

        feeder.seek(1);
        assert!(feeder.queued.is_some(), "the queued sound is queued again");
        let played = drain_until_the_feeder_stops(&mut feeder);
        let mut expected =
            as_played(&first_samples[SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24 * STEREO_CHANNELS..]);
        expected.extend(as_played(&next_samples));
        assert!(
            played == expected,
            "the seek did not play the rest of the first composition and then the next"
        );
    }

    const TONE_HERTZ: f64 = 1_000.0;
    const HALF_SCALE: f64 = 0.5;
    const HALF_SCALE_DBFS: f64 = -6.0206;
    const HALF_SCALE_SINE_RMS_DBFS: f64 = -9.0309;
    const LEVEL_TOLERANCE_DB: f64 = 0.02;

    // stereo, silent and then a half scale tone on the left
    fn silence_then_tone(silent_edit_units: usize, tone_edit_units: usize) -> Vec<i16> {
        let silent = silent_edit_units * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24;
        let total = silent + tone_edit_units * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24;
        (0..total)
            .flat_map(|frame| {
                let phase = 2.0 * std::f64::consts::PI * TONE_HERTZ * frame as f64
                    / f64::from(DEFAULT_SAMPLE_RATE);
                let left = match frame < silent {
                    true => 0.0,
                    false => HALF_SCALE * phase.sin() * f64::from(i16::MAX),
                };
                [left.round() as i16, 0]
            })
            .collect()
    }

    // the device takes sample_frames from the buffer and the feeder tops it up
    fn play(feeder: &mut Feeder, sample_frames: usize) -> Vec<f32> {
        feeder.fill();
        let channels = feeder.shared.output_channels();
        let played = feeder
            .shared
            .buffer
            .lock()
            .unwrap()
            .drain(..sample_frames * channels)
            .collect();
        feeder
            .shared
            .emitted_sample_frames
            .fetch_add(sample_frames as u64, Ordering::AcqRel);
        feeder.fill();
        played
    }

    fn assert_level(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < LEVEL_TOLERANCE_DB,
            "{actual} dBFS, expected {expected}"
        );
    }

    #[test]
    fn the_meter_reads_what_the_device_plays_not_what_the_feeder_read() {
        const SILENT_EDIT_UNITS: usize = 6;
        const TONE_EDIT_UNITS: usize = 18;
        const INTO_THE_TONE_EDIT_UNITS: usize = 12;
        let directory = tempfile::tempdir().unwrap();
        let samples = silence_then_tone(SILENT_EDIT_UNITS, TONE_EDIT_UNITS);
        let sound = wrapped_samples(directory.path(), "tone", 2, None, &samples);
        let shared = Arc::new(Shared::new());
        shared.playing.store(true, Ordering::Release);
        shared.stream_live.store(true, Ordering::Release);
        shared.reels_loaded.store(true, Ordering::Release);
        let meter = SoundLevelMeter(Arc::clone(&shared));
        meter.set_enabled(true);
        let mut feeder = Feeder::new(shared);
        feeder.current = composition_of(Some(&sound), (SILENT_EDIT_UNITS + TONE_EDIT_UNITS) as u64);
        feeder.seek(0);
        feeder.fill();
        assert!(
            feeder.next_frame > SILENT_EDIT_UNITS as u64,
            "the feeder has not read ahead into the tone"
        );

        play(
            &mut feeder,
            (SILENT_EDIT_UNITS - 1) * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24,
        );
        let in_the_silence = meter.levels().unwrap();
        assert_eq!(in_the_silence[0].label, "L");
        assert_eq!(in_the_silence[0].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_eq!(in_the_silence[0].rms_dbfs, SILENCE_FLOOR_DBFS);

        play(
            &mut feeder,
            INTO_THE_TONE_EDIT_UNITS * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24,
        );
        let in_the_tone = meter.levels().unwrap();
        assert_level(in_the_tone[0].peak_dbfs, HALF_SCALE_DBFS);
        assert_level(in_the_tone[0].rms_dbfs, HALF_SCALE_SINE_RMS_DBFS);
        assert_eq!(in_the_tone[1].label, "R");
        assert_eq!(in_the_tone[1].rms_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn the_meter_off_measures_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let sound = wrapped_samples(directory.path(), "tone", 2, None, &silence_then_tone(0, 2));
        let shared = Arc::new(Shared::new());
        shared.reels_loaded.store(true, Ordering::Release);
        let meter = SoundLevelMeter(Arc::clone(&shared));
        let mut feeder = Feeder::new(Arc::clone(&shared));
        feeder.current = composition_of(Some(&sound), 2);
        feeder.seek(0);
        feeder.fill();

        assert!(meter.levels().is_none());
        assert!(shared.levels.lock().unwrap().read(0, 0, true).is_none());
    }

    // 1601.6 sample frames a frame at 48 kHz, so the edit units differ in length
    const TIMELINE_RATE: (u32, u32) = (30_000, 1_001);
    const CODESTREAM_FILES: usize = 8;
    const FIRST_CLIP: (u64, u64) = (2, 6);
    const SECOND_CLIP: (u64, u64) = (1, 6);

    fn timeline_sample_frame(frame: u64) -> usize {
        let (numerator, denominator) = TIMELINE_RATE;
        (frame * u64::from(DEFAULT_SAMPLE_RATE) * u64::from(denominator) / u64::from(numerator))
            as usize
    }

    // every sample holds its own index, as 24-bit pcm
    fn numbered_timeline_wav(directory: &Path, frames: u64) -> (PathBuf, Vec<f32>) {
        let path = directory.join("timeline-sound.wav");
        let spec = hound::WavSpec {
            channels: SOUND_CHANNELS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        let samples = timeline_sample_frame(frames) * usize::from(SOUND_CHANNELS);
        for index in 0..samples {
            writer.write_sample(index as i32).unwrap();
        }
        writer.finalize().unwrap();
        let played = (0..samples)
            .map(|index| index as f32 / TWENTY_FOUR_BIT_FULL_SCALE)
            .collect();
        (path, played)
    }

    fn codestream_directory(directory: &Path) -> PathBuf {
        let codestreams = directory.join("codestreams");
        std::fs::create_dir_all(&codestreams).unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinema2k_64x64.j2c");
        for index in 0..CODESTREAM_FILES {
            std::fs::copy(&fixture, codestreams.join(format!("frame_{index:04}.j2c"))).unwrap();
        }
        codestreams
    }

    fn clip_sound(
        codestreams: &Path,
        wav: &Path,
        (in_frame, out_frame): (u64, u64),
        timeline_start: u64,
    ) -> SoundComposition {
        let fps = crate::encode::FrameRate::new(TIMELINE_RATE.0, TIMELINE_RATE.1);
        let mut timeline = Timeline::open(codestreams, None, &[]).unwrap();
        timeline.play_codestreams_at(fps);
        timeline
            .play_range(
                super::super::FrameRange {
                    in_frame,
                    out_frame: Some(out_frame),
                },
                codestreams,
            )
            .unwrap();
        timeline
            .play_sound_file(&super::super::SoundFile {
                path: wav.to_path_buf(),
                fps,
                first_frame: timeline_start,
            })
            .unwrap();
        assert_eq!(timeline.fps, fps.as_f64());
        SoundComposition::of(
            &timeline.sound,
            SoundSource::Atmos,
            timeline.played_frames(),
        )
    }

    #[test]
    fn a_timeline_wav_plays_each_clip_from_its_place_on_the_timeline() {
        let first_frames = FIRST_CLIP.1 - FIRST_CLIP.0;
        let total_frames = first_frames + SECOND_CLIP.1 - SECOND_CLIP.0;
        let directory = tempfile::tempdir().unwrap();
        let codestreams = codestream_directory(directory.path());
        let (wav, whole_timeline) = numbered_timeline_wav(directory.path(), total_frames);
        let channels = usize::from(SOUND_CHANNELS);

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = clip_sound(&codestreams, &wav, FIRST_CLIP, 0);
        feeder.queue(Some(clip_sound(
            &codestreams,
            &wav,
            SECOND_CLIP,
            first_frames,
        )));
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);

        let hand_off = timeline_sample_frame(first_frames) * channels;
        assert_eq!(
            played[hand_off], whole_timeline[hand_off],
            "the second clip's first sample is not the WAV's at its timeline frame"
        );
        assert_eq!(played.len(), whole_timeline.len());
        assert!(
            played == whole_timeline,
            "the two clips do not play the WAV end to end"
        );

        const FRAMES_INTO_THE_SECOND_CLIP: u64 = 3;
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = clip_sound(&codestreams, &wav, SECOND_CLIP, first_frames);
        feeder.seek(FRAMES_INTO_THE_SECOND_CLIP);
        let played = drain_until_the_feeder_stops(&mut feeder);
        let sought = timeline_sample_frame(first_frames + FRAMES_INTO_THE_SECOND_CLIP) * channels;
        assert!(
            played == whole_timeline[sought..],
            "a seek in the second clip does not play the WAV from its timeline frame"
        );
    }

    #[test]
    fn a_picture_only_composition_queues_as_silence() {
        let directory = tempfile::tempdir().unwrap();
        let first_samples = ramp(2, 1);
        let first = wrapped_samples(directory.path(), "first", 2, None, &first_samples);

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition_of(Some(&first), 2);
        feeder.queue(Some(composition_of(None, 3)));
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);
        let mut expected = as_played(&first_samples);
        expected.extend(std::iter::repeat_n(
            0.0,
            3 * SAMPLE_FRAMES_PER_EDIT_UNIT_AT_24 * STEREO_CHANNELS,
        ));
        assert!(played == expected);
    }

    use super::super::atmos_package::{
        AtmosDcpReel, SAMPLE_FRAMES_PER_FRAME as ATMOS_SAMPLE_FRAMES, write_atmos_dcp,
    };
    use crate::iab::tests::{centre_bed_frame, validation_stream_frames};
    use crate::iab::{DIRECT_PATH_DELAY, Renderer};

    fn one_atmos_reel(frames: &[Vec<u8>]) -> Vec<AtmosDcpReel> {
        vec![AtmosDcpReel {
            picture_frames: frames.len(),
            atmos_frames: Some(frames.to_vec()),
        }]
    }

    fn atmos_composition(package: &Path, keys: Option<&ContentKeys>) -> SoundComposition {
        let composition = composition_choosing(package, keys, SoundSource::Atmos);
        assert_eq!(composition.playing, Some(SoundSource::Atmos));
        composition
    }

    fn composition_choosing(
        package: &Path,
        keys: Option<&ContentKeys>,
        choice: SoundSource,
    ) -> SoundComposition {
        let timeline = Timeline::open(package, keys, &[]).unwrap();
        SoundComposition::of(&timeline.sound, choice, timeline.played_frames())
    }

    fn played_after_a_seek(composition: SoundComposition, frame: u64) -> (Vec<f32>, Feeder) {
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition;
        feeder.seek(frame);
        (drain_until_the_feeder_stops(&mut feeder), feeder)
    }

    // every frame rendered in order on stereo and a silent frame after, the frames at silenced as silent frames
    fn rendered_continuously(frames: &[Vec<u8>], silenced: &[usize]) -> Vec<f32> {
        let mut silent_frame = crate::iab::parse_frame(&frames[0]).unwrap();
        silent_frame.beds.clear();
        silent_frame.objects.clear();
        let mut renderer = Renderer::new(OutputLayout::Stereo);
        let mut rendered = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let samples = if silenced.contains(&index) {
                renderer.render(&silent_frame, &BTreeMap::new())
            } else {
                let frame = crate::iab::parse_frame(frame).unwrap();
                renderer.render(&frame, &crate::iab::decode_audio(&frame).unwrap())
            };
            rendered.extend(samples.unwrap());
        }
        rendered.extend(renderer.render(&silent_frame, &BTreeMap::new()).unwrap());
        rendered
            .into_iter()
            .map(|sample| sample.clamp(-1.0, 1.0))
            .collect()
    }

    // what plays from frame on with the renderer's delay taken off
    fn heard_from(rendered: &[f32], frame: usize, frame_count: usize) -> &[f32] {
        let start = (frame * ATMOS_SAMPLE_FRAMES + DIRECT_PATH_DELAY) * STEREO_CHANNELS;
        &rendered[start..start + (frame_count - frame) * ATMOS_SAMPLE_FRAMES * STEREO_CHANNELS]
    }

    // stream 35 decorrelates, so its output depends on the frames before
    #[test]
    fn atmos_frame_n_after_a_seek_to_n_plays_as_continuous_rendering_does() {
        for stream in [1, 35] {
            let frames = validation_stream_frames(stream);
            let directory = tempfile::tempdir().unwrap();
            let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);
            let rendered = rendered_continuously(&frames, &[]);
            for seek_frame in [0, 1, 20, frames.len() - 1] {
                let composition = atmos_composition(&dcp.package, None);
                let (played, feeder) = played_after_a_seek(composition, seek_frame as u64);
                assert!(
                    played == heard_from(&rendered, seek_frame, frames.len()),
                    "a seek to frame {seek_frame} of stream {stream} plays other samples"
                );
                assert!(feeder.shared.track_warnings.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn atmos_reels_play_on_as_one_rendering() {
        let frames = validation_stream_frames(1);
        let (first, second) = frames.split_at(frames.len() / 2);
        let reels = [first, second].map(|frames| AtmosDcpReel {
            picture_frames: frames.len(),
            atmos_frames: Some(frames.to_vec()),
        });
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &reels, None);
        let (played, _) = played_after_a_seek(atmos_composition(&dcp.package, None), 0);
        let rendered = rendered_continuously(&frames, &[]);
        assert!(played == heard_from(&rendered, 0, frames.len()));
    }

    #[test]
    fn a_sound_starting_in_frame_n_reaches_the_device_on_frame_ns_first_sample() {
        const FRAMES: usize = 8;
        const ONSET_FRAME: usize = 5;
        const LEVEL: i32 = 1 << 22;
        let frames: Vec<Vec<u8>> = (0..FRAMES)
            .map(|frame| {
                let level = if frame < ONSET_FRAME { 0 } else { LEVEL };
                centre_bed_frame(&vec![level; ATMOS_SAMPLE_FRAMES])
            })
            .collect();
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);
        for seek_frame in [0, 3, ONSET_FRAME] {
            let composition = atmos_composition(&dcp.package, None);
            let (played, _) = played_after_a_seek(composition, seek_frame as u64);
            let onset = played.iter().position(|sample| *sample != 0.0).unwrap() / STEREO_CHANNELS;
            let frame_start = (ONSET_FRAME - seek_frame) * ATMOS_SAMPLE_FRAMES;
            assert!(
                onset.abs_diff(frame_start) <= 1,
                "after a seek to {seek_frame} the sound starts at sample {onset}, frame {ONSET_FRAME} at {frame_start}"
            );
        }
    }

    #[test]
    fn a_failing_atmos_frame_plays_as_silence_with_one_warning_per_error() {
        const UNREADABLE: &[u8] = b"not an IAB frame";
        let mut frames = validation_stream_frames(1);
        let unrendered = validation_stream_frames(41).swap_remove(0);
        let unrendered_error = {
            let frame = crate::iab::parse_frame(&unrendered).unwrap();
            let audio = crate::iab::decode_audio(&frame).unwrap();
            Renderer::new(OutputLayout::Stereo)
                .render(&frame, &audio)
                .unwrap_err()
        };
        let unreadable_error = crate::iab::parse_frame(UNREADABLE).unwrap_err();
        frames[10] = unrendered.clone();
        frames[11] = unrendered;
        frames[20] = UNREADABLE.to_vec();
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);

        let (played, feeder) = played_after_a_seek(atmos_composition(&dcp.package, None), 0);

        let warnings = feeder.shared.track_warnings.lock().unwrap().clone();
        assert_eq!(
            warnings,
            [
                format!("Atmos frame 10 plays as silence: {unrendered_error}"),
                format!("Atmos frame 20 plays as silence: {unreadable_error}"),
            ]
        );
        let rendered = rendered_continuously(&frames, &[10, 11, 20]);
        assert!(played == heard_from(&rendered, 0, frames.len()));
    }

    #[test]
    fn an_encrypted_atmos_dcp_plays_with_its_key_and_fails_as_main_sound_does_without() {
        const ATMOS_KEY: [u8; 16] = [0x3c; 16];
        const ATMOS_KEY_ID: [u8; 16] = [0x4d; 16];
        let frames = validation_stream_frames(1)[..4].to_vec();
        let directory = tempfile::tempdir().unwrap();
        let encryption = MxfEncryption {
            content_key: ATMOS_KEY,
            key_id: ATMOS_KEY_ID,
        };
        let dcp = write_atmos_dcp(
            directory.path(),
            &one_atmos_reel(&frames),
            Some(&encryption),
        );
        let keys = content_keys(directory.path(), &[(ATMOS_KEY_ID, ATMOS_KEY)]);

        let (played, feeder) = played_after_a_seek(atmos_composition(&dcp.package, Some(&keys)), 0);
        assert!(played == heard_from(&rendered_continuously(&frames, &[]), 0, frames.len()));
        assert!(feeder.shared.track_warnings.lock().unwrap().is_empty());

        let atmos_track = dcp.atmos_tracks[0].as_ref().unwrap();
        let error = Timeline::open(&dcp.package, None, &[]).err().unwrap();
        assert_eq!(
            error,
            format!(
                "{} is encrypted sound and the preview holds no content key for it",
                atmos_track.display()
            )
        );
        let unrelated = content_keys(directory.path(), &[([0x5e; 16], ATMOS_KEY)]);
        let error = Timeline::open(&dcp.package, Some(&unrelated), &[])
            .err()
            .unwrap();
        assert_eq!(
            error,
            format!(
                "KDM/keys do not cover sound KeyId {}",
                uuid::Uuid::from_bytes(ATMOS_KEY_ID)
            )
        );
    }

    #[test]
    fn five_point_one_atmos_reaches_the_device_and_the_meter_in_its_channels() {
        const LEVEL: i32 = 1 << 22;
        const FIVE_POINT_ONE_CHANNELS: usize = 6;
        let frames = vec![centre_bed_frame(&vec![LEVEL; ATMOS_SAMPLE_FRAMES]); 4];
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);
        let shared = Arc::new(Shared::new());
        shared
            .output_channels
            .store(FIVE_POINT_ONE_CHANNELS, Ordering::Release);
        shared.playing.store(true, Ordering::Release);
        shared.stream_live.store(true, Ordering::Release);
        shared.reels_loaded.store(true, Ordering::Release);
        let meter = SoundLevelMeter(Arc::clone(&shared));
        meter.set_enabled(true);
        let mut feeder = Feeder::new(shared);
        feeder.mix = OutputMix::Routed(&FIVE_POINT_ONE_DEVICE_ORDER);
        feeder.current = atmos_composition(&dcp.package, None);
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);

        let centre = device_position(Speaker::Centre, &FIVE_POINT_ONE_DEVICE_ORDER).unwrap();
        let level = LEVEL as f32 * 256.0 / FULL_SCALE_I32;
        for (index, sample) in played.iter().enumerate() {
            let expected = if index % FIVE_POINT_ONE_CHANNELS == centre {
                level
            } else {
                0.0
            };
            assert_eq!(*sample, expected, "sample {index}");
        }
        let levels = meter.levels().unwrap();
        let labels: Vec<&str> = levels.iter().map(|level| level.label.as_str()).collect();
        assert_eq!(labels, ["L", "R", "C", "LFE", "Ls", "Rs"]);
        assert!(levels[2].peak_dbfs > SILENCE_FLOOR_DBFS);
        assert_eq!(levels[0].peak_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn ninety_six_kilohertz_atmos_reaches_a_48_kilohertz_device_through_the_resampler() {
        let frames = validation_stream_frames(127);
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);

        let (played, feeder) = played_after_a_seek(atmos_composition(&dcp.package, None), 0);

        assert!(feeder.shared.track_warnings.lock().unwrap().is_empty());
        let mut rendered = Vec::new();
        let mut renderer = Renderer::new(OutputLayout::Stereo);
        let mut silent_frame = crate::iab::parse_frame(&frames[0]).unwrap();
        assert_eq!(silent_frame.sample_rate, crate::iab::SampleRate::Hz96000);
        for frame in &frames {
            let frame = crate::iab::parse_frame(frame).unwrap();
            rendered.extend(
                renderer
                    .render(&frame, &crate::iab::decode_audio(&frame).unwrap())
                    .unwrap(),
            );
        }
        silent_frame.beds.clear();
        silent_frame.objects.clear();
        rendered.extend(renderer.render(&silent_frame, &BTreeMap::new()).unwrap());
        let heard: Vec<f32> = rendered[DIRECT_PATH_DELAY * STEREO_CHANNELS..]
            [..frames.len() * 2 * ATMOS_SAMPLE_FRAMES * STEREO_CHANNELS]
            .iter()
            .map(|sample| sample.clamp(-1.0, 1.0))
            .collect();
        let mut expected = Vec::new();
        Resampler::new(96_000, DEFAULT_SAMPLE_RATE, STEREO_CHANNELS).push(&heard, &mut expected);
        assert_eq!(played.len(), expected.len());
        assert!(played == expected);
    }

    use super::super::atmos_package::main_sound_samples;

    // what the stereo device plays of each reel's main sound, end to end
    fn main_sound_played(reels: &[AtmosDcpReel]) -> Vec<f32> {
        reels
            .iter()
            .enumerate()
            .flat_map(|(index, reel)| as_played(&main_sound_samples(index, reel.picture_frames)))
            .collect()
    }

    fn stereo_samples_before(frame: usize) -> usize {
        frame * ATMOS_SAMPLE_FRAMES * STEREO_CHANNELS
    }

    fn play_frames(feeder: &mut Feeder, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|_| play(feeder, ATMOS_SAMPLE_FRAMES))
            .collect()
    }

    fn assert_the_device_clock_is_where_it_was(feeder: &Feeder, sample_frames: u64) {
        assert_eq!(
            feeder.shared.emitted_sample_frames.load(Ordering::Acquire),
            sample_frames
        );
        assert_eq!(feeder.seek_position, Some(0.0));
    }

    #[test]
    fn a_switch_plays_the_other_track_from_the_frame_the_device_is_at() {
        const MAIN_SOUND_FROM: usize = 5;
        let frames = validation_stream_frames(1);
        let (first, second) = frames.split_at(frames.len() / 2);
        let reels = [first, second].map(|frames| AtmosDcpReel {
            picture_frames: frames.len(),
            atmos_frames: Some(frames.to_vec()),
        });
        // past the reel boundary
        let atmos_again_from = first.len() + 2;
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &reels, None);
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = atmos_composition(&dcp.package, None);
        feeder.seek(0);
        play_frames(&mut feeder, MAIN_SOUND_FROM);

        feeder.set_source(SoundSource::MainSound);
        let played_sample_frames = (MAIN_SOUND_FROM * ATMOS_SAMPLE_FRAMES) as u64;
        assert_the_device_clock_is_where_it_was(&feeder, played_sample_frames);
        let on_the_main_sound = play_frames(&mut feeder, atmos_again_from - MAIN_SOUND_FROM);
        let main_sound = main_sound_played(&reels);
        assert!(
            on_the_main_sound
                == main_sound[stereo_samples_before(MAIN_SOUND_FROM)
                    ..stereo_samples_before(atmos_again_from)],
            "the main sound does not play from frame {MAIN_SOUND_FROM}"
        );

        feeder.set_source(SoundSource::Atmos);
        let played_sample_frames = (atmos_again_from * ATMOS_SAMPLE_FRAMES) as u64;
        assert_the_device_clock_is_where_it_was(&feeder, played_sample_frames);
        let on_atmos = drain_until_the_feeder_stops(&mut feeder);
        let rendered = rendered_continuously(&frames, &[]);
        assert!(
            on_atmos == heard_from(&rendered, atmos_again_from, frames.len()),
            "Atmos does not play from frame {atmos_again_from} as continuous rendering does"
        );
        assert!(feeder.shared.track_warnings.lock().unwrap().is_empty());
    }

    #[test]
    fn back_on_atmos_a_sound_starting_in_frame_n_reaches_the_device_on_its_first_sample() {
        const FRAMES: usize = 8;
        const ATMOS_FROM: usize = 3;
        const ONSET_FRAME: usize = 5;
        const LEVEL: i32 = 1 << 22;
        let frames: Vec<Vec<u8>> = (0..FRAMES)
            .map(|frame| {
                let level = if frame < ONSET_FRAME { 0 } else { LEVEL };
                centre_bed_frame(&vec![level; ATMOS_SAMPLE_FRAMES])
            })
            .collect();
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition_choosing(&dcp.package, None, SoundSource::MainSound);
        feeder.seek(0);
        play_frames(&mut feeder, ATMOS_FROM);

        feeder.set_source(SoundSource::Atmos);
        let played = drain_until_the_feeder_stops(&mut feeder);
        let onset = played.iter().position(|sample| *sample != 0.0).unwrap() / STEREO_CHANNELS;
        let frame_start = (ONSET_FRAME - ATMOS_FROM) * ATMOS_SAMPLE_FRAMES;
        assert!(
            onset.abs_diff(frame_start) <= 1,
            "the sound starts at sample {onset}, frame {ONSET_FRAME} at {frame_start}"
        );
    }

    #[test]
    fn a_switch_from_96_kilohertz_atmos_lands_inside_the_main_sound_frame_the_device_is_in() {
        const SWITCH_AT_SAMPLE_FRAME: usize = 2 * ATMOS_SAMPLE_FRAMES + ATMOS_SAMPLE_FRAMES / 2;
        let frames = validation_stream_frames(127);
        let reels = one_atmos_reel(&frames);
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &reels, None);
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = atmos_composition(&dcp.package, None);
        feeder.seek(0);
        play(&mut feeder, SWITCH_AT_SAMPLE_FRAME);

        feeder.set_source(SoundSource::MainSound);
        let played = drain_until_the_feeder_stops(&mut feeder);
        assert!(
            played == main_sound_played(&reels)[SWITCH_AT_SAMPLE_FRAME * STEREO_CHANNELS..],
            "the 48 kHz main sound does not pick up at the sample the device is at"
        );
    }

    #[test]
    fn the_choice_carries_to_the_queued_composition_both_ways() {
        let frames = validation_stream_frames(1)[..6].to_vec();
        let with_atmos = one_atmos_reel(&frames);
        let without_atmos = [AtmosDcpReel {
            picture_frames: frames.len(),
            atmos_frames: None,
        }];
        let directory = tempfile::tempdir().unwrap();
        let first = write_atmos_dcp(&directory.path().join("first"), &with_atmos, None);
        let second = write_atmos_dcp(&directory.path().join("second"), &with_atmos, None);
        let plain = write_atmos_dcp(&directory.path().join("plain"), &without_atmos, None);
        let main_sound = main_sound_played(&with_atmos);
        let both_main_sounds = [main_sound.clone(), main_sound].concat();

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = composition_choosing(&first.package, None, SoundSource::MainSound);
        feeder.queue(Some(composition_choosing(
            &second.package,
            None,
            SoundSource::MainSound,
        )));
        feeder.seek(0);
        assert!(
            drain_until_the_feeder_stops(&mut feeder) == both_main_sounds,
            "the queued composition with Atmos does not play its main sound"
        );

        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = atmos_composition(&first.package, None);
        feeder.queue(Some(composition_choosing(
            &plain.package,
            None,
            SoundSource::Atmos,
        )));
        feeder.seek(0);
        let played = drain_until_the_feeder_stops(&mut feeder);
        let rendered = rendered_continuously(&frames, &[]);
        let expected = [
            heard_from(&rendered, 0, frames.len()).to_vec(),
            main_sound_played(&without_atmos),
        ]
        .concat();
        assert!(
            played == expected,
            "the queued composition without Atmos does not play its main sound"
        );
        assert!(feeder.shared.track_warnings.lock().unwrap().is_empty());
        let plain_timeline = Timeline::open(&plain.package, None, &[]).unwrap();
        assert_eq!(plain_timeline.atmos_warning, None);

        // the fill reaches the queued composition before the switch
        let mut feeder = Feeder::new(Arc::new(Shared::new()));
        feeder.current = atmos_composition(&first.package, None);
        feeder.queue(Some(atmos_composition(&second.package, None)));
        feeder.seek(0);
        feeder.fill();
        assert!(feeder.finished.is_some());
        feeder.set_source(SoundSource::MainSound);
        assert!(
            drain_until_the_feeder_stops(&mut feeder) == both_main_sounds,
            "a switch does not reach the queued composition"
        );
    }

    #[test]
    fn the_meter_names_the_channels_of_the_track_that_plays() {
        const LEVEL: i32 = 1 << 22;
        const FIVE_POINT_ONE_CHANNELS: usize = 6;
        let frames = vec![centre_bed_frame(&vec![LEVEL; ATMOS_SAMPLE_FRAMES]); 12];
        let directory = tempfile::tempdir().unwrap();
        let dcp = write_atmos_dcp(directory.path(), &one_atmos_reel(&frames), None);
        let shared = Arc::new(Shared::new());
        shared
            .output_channels
            .store(FIVE_POINT_ONE_CHANNELS, Ordering::Release);
        shared.playing.store(true, Ordering::Release);
        shared.stream_live.store(true, Ordering::Release);
        shared.reels_loaded.store(true, Ordering::Release);
        let meter = SoundLevelMeter(Arc::clone(&shared));
        meter.set_enabled(true);
        let mut feeder = Feeder::new(shared);
        feeder.mix = OutputMix::Routed(&FIVE_POINT_ONE_DEVICE_ORDER);
        feeder.current = atmos_composition(&dcp.package, None);
        feeder.seek(0);
        let labels = |meter: &SoundLevelMeter| -> Vec<String> {
            meter
                .levels()
                .unwrap()
                .into_iter()
                .map(|level| level.label)
                .collect()
        };

        play_frames(&mut feeder, 2);
        assert_eq!(labels(&meter), ["L", "R", "C", "LFE", "Ls", "Rs"]);
        feeder.set_source(SoundSource::MainSound);
        feeder.fill();
        assert_eq!(labels(&meter), ["L", "R"]);
        play_frames(&mut feeder, 2);
        let levels = meter.levels().unwrap();
        assert!(levels[0].peak_dbfs > SILENCE_FLOOR_DBFS);
        feeder.set_source(SoundSource::Atmos);
        feeder.fill();
        assert_eq!(labels(&meter), ["L", "R", "C", "LFE", "Ls", "Rs"]);
    }
}

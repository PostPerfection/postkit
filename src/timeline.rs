use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::composition_timeline::{PictureSegment, SegmentTrim, edl_segment, edl_uri};
use crate::encode::{FrameRate, InputType, SourceColour};
use crate::picture_processing::PictureProcessing;
use crate::subtitle_retime::{SrtCue, format_srt, parse_srt};
use crate::wav_io::{SampleFormat, WavReader, WavSpec, WavWriter};

pub const TIMELINE_FORMAT_VERSION: u32 = 2;

const SOUND_SAMPLE_RATE: u32 = 48_000;
const SOUND_BITS_PER_SAMPLE: u16 = 24;
const SOUND_BYTES_PER_SAMPLE: u64 = 3;
const LARGEST_24_BIT_SAMPLE: i32 = (1 << 23) - 1;
const SMALLEST_24_BIT_SAMPLE: i32 = -(1 << 23);
const BITS_PER_BYTE: u16 = 8;
const MILLISECONDS_PER_SECOND: u64 = 1000;
const COPY_BUFFER_BYTES: usize = 1 << 20;
// the sound plays as a second stream beside the joined pictures, not as one more segment after them
const EDL_SEPARATE_STREAM: &str = "!new_stream;!no_clip";
pub const BLACK_GAP_SIZE: &str = "320x180";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub version: u32,
    pub fps: FrameRate,
    pub clips: Vec<Clip>,
    pub sound: SoundTrack,
    pub subtitle: Option<PathBuf>,
    #[serde(default)]
    pub reel_starts: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub picture: PathBuf,
    pub start: u64,
    #[serde(default)]
    pub layer: u32,
    #[serde(default = "full_opacity")]
    pub opacity: f64,
    pub in_frame: u64,
    pub out_frame: u64,
    pub sound: ClipSound,
    pub subtitle: Option<PathBuf>,
    #[serde(default)]
    pub source_colour: SourceColour,
    #[serde(default)]
    pub picture_processing: PictureProcessing,
}

fn full_opacity() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipSound {
    Embedded,
    File(PathBuf),
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SoundTrack {
    PerClip,
    Override(PathBuf),
}

impl Timeline {
    pub fn read(path: &Path) -> Result<Timeline, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(TIMELINE_FORMAT_VERSION)) {
            let found = version.map_or("no version".to_string(), |v| format!("version {v}"));
            return Err(format!(
                "{} is a timeline file of {found}, and this build reads version \
                 {TIMELINE_FORMAT_VERSION}",
                path.display()
            ));
        }
        serde_json::from_value(value)
            .map_err(|e| format!("{} is not a timeline: {e}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        if self.version != TIMELINE_FORMAT_VERSION {
            return Err(format!(
                "the timeline is version {}, and this build writes version \
                 {TIMELINE_FORMAT_VERSION}",
                self.version
            ));
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("cannot serialise the timeline: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    pub fn total_frames(&self) -> u64 {
        self.clips.iter().map(Clip::end).max().unwrap_or(0)
    }

    pub fn clip_starts(&self) -> Vec<u64> {
        self.clips.iter().map(|clip| clip.start).collect()
    }

    pub fn base_clips(&self) -> Vec<(usize, &Clip)> {
        let mut base: Vec<(usize, &Clip)> = self
            .clips
            .iter()
            .enumerate()
            .filter(|(_, clip)| clip.layer == 0)
            .collect();
        base.sort_by_key(|(_, clip)| clip.start);
        base
    }

    pub fn base_gaps(&self) -> Vec<Range<u64>> {
        let mut gaps = Vec::new();
        let mut covered_to = 0;
        for (_, clip) in self.base_clips() {
            if clip.start > covered_to {
                gaps.push(covered_to..clip.start);
            }
            covered_to = clip.end();
        }
        let total = self.total_frames();
        if total > covered_to {
            gaps.push(covered_to..total);
        }
        gaps
    }

    pub fn check(&self) -> Result<(), String> {
        if self.clips.is_empty() {
            return Err("the timeline has no clips".to_string());
        }
        for (index, clip) in self.clips.iter().enumerate() {
            let name = clip_name(index, clip);
            if clip.out_frame <= clip.in_frame {
                return Err(format!(
                    "{name}: the out point {} is at or before the in point {}",
                    clip.out_frame, clip.in_frame
                ));
            }
            match probe_source(&clip.picture).map_err(|e| format!("{name}: {e}"))? {
                SourceLength::Still => check_still_in_point(&name, clip)?,
                SourceLength::Counted { frames, rate } => {
                    check_counted_source(&name, clip, frames, rate, self.fps)?
                }
            }
            if !(0.0..=1.0).contains(&clip.opacity) {
                return Err(format!(
                    "{name}: the opacity {} is outside 0 to 1",
                    clip.opacity
                ));
            }
            if clip.layer > 0 && clip.sound != ClipSound::None {
                return Err(format!(
                    "{name} is on layer {} and carries sound: only layer 0 clips sound",
                    clip.layer
                ));
            }
        }
        self.check_overlaps()?;
        self.check_reel_starts()
    }

    fn check_overlaps(&self) -> Result<(), String> {
        let mut by_layer: Vec<(usize, &Clip)> = self.clips.iter().enumerate().collect();
        by_layer.sort_by_key(|(_, clip)| (clip.layer, clip.start));
        for ((index_a, a), (index_b, b)) in by_layer.iter().zip(by_layer.iter().skip(1)) {
            if a.layer == b.layer && b.start < a.end() {
                return Err(format!(
                    "{} starts at frame {} while {} on layer {} runs to frame {}: clips on one \
                     layer cannot overlap",
                    clip_name(*index_b, b),
                    b.start,
                    clip_name(*index_a, a),
                    a.layer,
                    a.end()
                ));
            }
        }
        Ok(())
    }

    fn check_reel_starts(&self) -> Result<(), String> {
        let total = self.total_frames();
        let mut previous = 0;
        for &start in &self.reel_starts {
            if start == 0 {
                return Err(
                    "reel start 0 is listed, but the first reel always starts there".to_string(),
                );
            }
            if start == previous {
                return Err(format!("reel start {start} is listed twice"));
            }
            if start < previous {
                return Err(format!(
                    "reel start {start} comes after {previous}: reel starts run in order"
                ));
            }
            if start >= total {
                return Err(format!(
                    "reel start {start} is at or past the timeline's end at frame {total}"
                ));
            }
            previous = start;
        }
        Ok(())
    }
}

fn check_still_in_point(name: &str, clip: &Clip) -> Result<(), String> {
    if clip.in_frame != 0 {
        return Err(format!(
            "{name}: the in point is {}, and a still is held from its first frame: its in point \
             is 0 and its out point sets the hold",
            clip.in_frame
        ));
    }
    Ok(())
}

fn check_counted_source(
    name: &str,
    clip: &Clip,
    source_frames: u64,
    source_rate: Option<FrameRate>,
    timeline_rate: FrameRate,
) -> Result<(), String> {
    if clip.in_frame >= source_frames {
        return Err(format!(
            "{name}: the in point {} is past the source's {source_frames} frames",
            clip.in_frame
        ));
    }
    if clip.out_frame > source_frames {
        return Err(format!(
            "{name}: the out point {} is past the source's {source_frames} frames",
            clip.out_frame
        ));
    }
    if let Some(rate) = source_rate
        && rate_differs(rate, timeline_rate)
    {
        return Err(format!(
            "{name} runs at {}/{} fps and the timeline at {}/{} fps",
            rate.numerator, rate.denominator, timeline_rate.numerator, timeline_rate.denominator
        ));
    }
    Ok(())
}

impl Clip {
    fn frame_count(&self) -> u64 {
        self.out_frame.saturating_sub(self.in_frame)
    }

    pub fn end(&self) -> u64 {
        self.start + self.frame_count()
    }
}

pub enum TimelineShape {
    SingleClip(SingleClipBuild),
    MultiClip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SingleClipBuild {
    pub picture: PathBuf,
    pub trim_start_frames: u64,
    pub trim_end_frames: u64,
    pub audio: Option<PathBuf>,
    // the picture's own sound is not taken either
    pub silent: bool,
    pub subtitle: Option<PathBuf>,
    // the subtitle is timed against the clip's source
    pub subtitle_from_clip: bool,
    pub reel_split_frames: Vec<u64>,
    pub still_hold_frames: Option<u64>,
}

pub fn shape(timeline: &Timeline) -> Result<TimelineShape, String> {
    let [clip] = timeline.clips.as_slice() else {
        return Ok(TimelineShape::MultiClip);
    };
    if clip.start != 0 || clip.layer != 0 || clip.opacity != 1.0 {
        return Ok(TimelineShape::MultiClip);
    }
    timeline.check()?;
    let name = clip_name(0, clip);
    if clip.source_colour != SourceColour::default() {
        return Err(format!(
            "{name} carries its own source colour, and a one-clip timeline takes the colour from \
             --source-colourspace"
        ));
    }
    if !clip.picture_processing.is_identity() {
        return Err(format!(
            "{name} carries its own picture processing, and a one-clip timeline takes it from the \
             picture flags"
        ));
    }
    refuse_two_subtitle_sources(timeline)?;
    let (trim_end_frames, still_hold_frames) =
        match probe_source(&clip.picture).map_err(|e| format!("{name}: {e}"))? {
            SourceLength::Still => (0, Some(clip.out_frame)),
            SourceLength::Counted { frames, .. } => (frames - clip.out_frame, None),
        };
    let audio = match (&timeline.sound, &clip.sound) {
        (SoundTrack::Override(path), _) | (SoundTrack::PerClip, ClipSound::File(path)) => {
            Some(path.clone())
        }
        (SoundTrack::PerClip, _) => None,
    };
    Ok(TimelineShape::SingleClip(SingleClipBuild {
        picture: clip.picture.clone(),
        trim_start_frames: clip.in_frame,
        trim_end_frames,
        audio,
        silent: timeline.sound == SoundTrack::PerClip && clip.sound == ClipSound::None,
        subtitle: clip.subtitle.clone().or_else(|| timeline.subtitle.clone()),
        subtitle_from_clip: clip.subtitle.is_some(),
        reel_split_frames: timeline.reel_starts.clone(),
        still_hold_frames,
    }))
}

pub fn refuse_two_subtitle_sources(timeline: &Timeline) -> Result<(), String> {
    let Some((index, clip)) = timeline
        .clips
        .iter()
        .enumerate()
        .find(|(_, clip)| clip.subtitle.is_some())
    else {
        return Ok(());
    };
    match &timeline.subtitle {
        Some(subtitle) => Err(format!(
            "the timeline's subtitle {} and the subtitle of {} both time the subtitle track: keep \
             one",
            subtitle.display(),
            clip_name(index, clip)
        )),
        None => Ok(()),
    }
}

pub fn clip_name(index: usize, clip: &Clip) -> String {
    format!("clip {} ({})", index + 1, clip.picture.display())
}

// 24000/1001 and 48000/2002 are one rate
fn rate_differs(a: FrameRate, b: FrameRate) -> bool {
    u64::from(a.numerator) * u64::from(b.denominator)
        != u64::from(b.numerator) * u64::from(a.denominator)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceLength {
    Counted {
        frames: u64,
        // None for an image sequence, which runs at whatever rate it is given
        rate: Option<FrameRate>,
    },
    // one image file, held for as long as its clip runs
    Still,
}

pub fn probe_source(picture: &Path) -> Result<SourceLength, String> {
    if crate::still::is_still_image(picture) {
        return Ok(SourceLength::Still);
    }
    match crate::encode::detect_input_type(picture) {
        InputType::Video => {
            let info = crate::probe::probe_video(picture)
                .ok_or_else(|| "ffprobe cannot read it".to_string())?;
            if info.total_frames == 0 || info.fps_num == 0 || info.fps_den == 0 {
                return Err(format!(
                    "ffprobe reads {} frames at {}/{} fps from it",
                    info.total_frames, info.fps_num, info.fps_den
                ));
            }
            Ok(SourceLength::Counted {
                frames: u64::from(info.total_frames),
                rate: Some(FrameRate::new(info.fps_num, info.fps_den)),
            })
        }
        InputType::ImageSequence => {
            let frames = crate::encode::find_source_frames(picture)
                .map_err(|e| format!("cannot list {}: {e}", picture.display()))?;
            Ok(SourceLength::Counted {
                frames: frames.len() as u64,
                rate: None,
            })
        }
        InputType::J2kSequence => Ok(SourceLength::Counted {
            frames: j2k_codestreams(&crate::encode::sequence_directory(picture))?.len() as u64,
            rate: None,
        }),
        InputType::PictureMxf => {
            let info = crate::mxf_unwrap::probe_picture_mxf(picture)?;
            Ok(SourceLength::Counted {
                frames: info.frames,
                rate: Some(FrameRate::new(info.edit_rate_num, info.edit_rate_den)),
            })
        }
        InputType::Unknown => Err("cannot tell what kind of picture it is".to_string()),
    }
}

pub(crate) fn j2k_codestreams(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|e| format!("cannot list {}: {e}", directory.display()))?;
    let mut codestreams: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_lowercase);
            path.is_file() && matches!(extension.as_deref(), Some("j2c" | "j2k"))
        })
        .collect();
    codestreams.sort();
    Ok(codestreams)
}

fn sample_at(frame: u64, fps: FrameRate) -> u64 {
    (u128::from(frame) * u128::from(SOUND_SAMPLE_RATE) * u128::from(fps.denominator)
        / u128::from(fps.numerator)) as u64
}

fn milliseconds_at(frame: u64, fps: FrameRate) -> u64 {
    let numerator = u128::from(fps.numerator);
    ((u128::from(frame) * u128::from(MILLISECONDS_PER_SECOND) * u128::from(fps.denominator)
        + numerator / 2)
        / numerator) as u64
}

pub fn render_sound(timeline: &Timeline, output: &Path) -> Result<Option<PathBuf>, String> {
    if let SoundTrack::Override(path) = &timeline.sound {
        return Ok(Some(path.clone()));
    }
    let Some(channels) = shared_channel_count(timeline)? else {
        return Ok(None);
    };
    let spec = WavSpec {
        channels,
        sample_rate: SOUND_SAMPLE_RATE,
        bits_per_sample: SOUND_BITS_PER_SAMPLE,
        sample_format: SampleFormat::Int,
    };
    let fps = timeline.fps;
    let total_samples = sample_at(timeline.total_frames(), fps);
    let mut writer = WavWriter::create(output, spec, total_samples)
        .map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    let write_error = |e: std::io::Error| format!("cannot write {}: {e}", output.display());
    let mut written_to = 0;
    for (index, clip) in timeline.base_clips() {
        let clip_first_sample = sample_at(clip.start, fps);
        write_silence(&mut writer, channels, clip_first_sample - written_to)
            .map_err(write_error)?;
        written_to = sample_at(clip.end(), fps);
        let length = written_to - clip_first_sample;
        let first_sample = sample_at(clip.in_frame, fps);
        let written = match &clip.sound {
            ClipSound::None => Ok(0),
            ClipSound::Embedded => {
                copy_embedded_sound(&clip.picture, first_sample, length, &mut writer)
            }
            ClipSound::File(path) => copy_wav(path, first_sample, length, &mut writer),
        }
        .map_err(|e| format!("{}: {e}", clip_name(index, clip)))?;
        write_silence(&mut writer, channels, length - written).map_err(write_error)?;
    }
    write_silence(&mut writer, channels, total_samples - written_to).map_err(write_error)?;
    writer.finalize().map_err(write_error)?;
    Ok(Some(output.to_path_buf()))
}

pub fn shared_channel_count(timeline: &Timeline) -> Result<Option<u16>, String> {
    let mut first: Option<(usize, u16)> = None;
    for (index, clip) in timeline.clips.iter().enumerate() {
        let channels = match &clip.sound {
            ClipSound::None => continue,
            ClipSound::Embedded => embedded_channel_count(&clip.picture),
            ClipSound::File(path) => wav_channel_count(path),
        }
        .map_err(|e| format!("{}: {e}", clip_name(index, clip)))?;
        match first {
            None => first = Some((index, channels)),
            Some((first_index, first_channels)) if first_channels != channels => {
                return Err(format!(
                    "{} has {first_channels} sound channels and {} has {channels}: every \
                     clip's sound needs the same channel count",
                    clip_name(first_index, &timeline.clips[first_index]),
                    clip_name(index, clip)
                ));
            }
            Some(_) => {}
        }
    }
    Ok(first.map(|(_, channels)| channels))
}

fn embedded_channel_count(picture: &Path) -> Result<u16, String> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0"])
        .args(["-show_entries", "stream=channels", "-of", "csv=p=0"])
        .arg(picture)
        .output()
        .map_err(|e| format!("cannot run ffprobe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe cannot read it: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Err("it has no sound to take: give the clip a WAV or no sound".to_string());
    }
    text.parse()
        .map_err(|_| format!("ffprobe reads the channel count {text:?}"))
}

fn open_wav(path: &Path) -> Result<WavReader, String> {
    let reader =
        WavReader::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let rate = reader.spec().sample_rate;
    if rate != SOUND_SAMPLE_RATE {
        return Err(format!(
            "{} is at {rate} Hz and the timeline's sound at {SOUND_SAMPLE_RATE} Hz",
            path.display()
        ));
    }
    Ok(reader)
}

fn wav_channel_count(path: &Path) -> Result<u16, String> {
    Ok(open_wav(path)?.spec().channels)
}

fn copy_embedded_sound(
    picture: &Path,
    first_sample: u64,
    length: u64,
    writer: &mut WavWriter,
) -> Result<u64, String> {
    let window = format!(
        "aresample={SOUND_SAMPLE_RATE},atrim=start_sample={first_sample}:end_sample={}",
        first_sample + length
    );
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(picture)
        .args([
            "-map",
            "0:a:0",
            "-af",
            &window,
            "-c:a",
            "pcm_s24le",
            "-f",
            "s24le",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let block_align = u64::from(writer.spec().channels) * SOUND_BYTES_PER_SAMPLE;
    let mut bytes_left = length * block_align;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = stdout
            .read(&mut buffer)
            .map_err(|e| format!("cannot read ffmpeg's sound: {e}"))?;
        if read == 0 {
            break;
        }
        let kept = (read as u64).min(bytes_left) as usize;
        writer
            .write_bytes(&buffer[..kept])
            .map_err(|e| format!("cannot write the timeline's sound: {e}"))?;
        bytes_left -= kept as u64;
    }
    let finished = child
        .wait_with_output()
        .map_err(|e| format!("ffmpeg did not finish: {e}"))?;
    if !finished.status.success() {
        return Err(format!(
            "ffmpeg cannot take its sound: {}",
            String::from_utf8_lossy(&finished.stderr).trim()
        ));
    }
    let bytes_written = length * block_align - bytes_left;
    if !bytes_written.is_multiple_of(block_align) {
        return Err(format!(
            "ffmpeg gave {bytes_written} bytes of sound, not whole {block_align}-byte frames"
        ));
    }
    Ok(bytes_written / block_align)
}

fn copy_wav(
    path: &Path,
    first_sample: u64,
    length: u64,
    writer: &mut WavWriter,
) -> Result<u64, String> {
    let mut reader = open_wav(path)?;
    let read_error = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let write_error = |e: std::io::Error| format!("cannot write the timeline's sound: {e}");
    reader.skip_frames(first_sample).map_err(read_error)?;
    let frames = reader.duration().saturating_sub(first_sample).min(length);
    let samples = (frames * u64::from(reader.spec().channels)) as usize;
    match reader.spec().sample_format {
        SampleFormat::Int => {
            let container_bits = reader.layout().bytes_per_sample * BITS_PER_BYTE;
            for sample in reader.samples::<i32>().take(samples) {
                let sample = sample.map_err(read_error)?;
                let widened = if container_bits <= SOUND_BITS_PER_SAMPLE {
                    sample << (SOUND_BITS_PER_SAMPLE - container_bits)
                } else {
                    sample >> (container_bits - SOUND_BITS_PER_SAMPLE)
                };
                writer.write_sample(widened).map_err(write_error)?;
            }
        }
        SampleFormat::Float => {
            for sample in reader.samples::<f32>().take(samples) {
                let sample = sample.map_err(read_error)?;
                let scaled = (f64::from(sample) * f64::from(LARGEST_24_BIT_SAMPLE)).round() as i32;
                writer
                    .write_sample(scaled.clamp(SMALLEST_24_BIT_SAMPLE, LARGEST_24_BIT_SAMPLE))
                    .map_err(write_error)?;
            }
        }
    }
    Ok(frames)
}

fn write_silence(writer: &mut WavWriter, channels: u16, frames: u64) -> std::io::Result<()> {
    let zeros = vec![0u8; COPY_BUFFER_BYTES];
    let mut bytes_left = frames * u64::from(channels) * SOUND_BYTES_PER_SAMPLE;
    while bytes_left > 0 {
        let chunk = bytes_left.min(COPY_BUFFER_BYTES as u64) as usize;
        writer.write_bytes(&zeros[..chunk])?;
        bytes_left -= chunk as u64;
    }
    Ok(())
}

pub fn join_subtitles(timeline: &Timeline, output: &Path) -> Result<Option<PathBuf>, String> {
    if timeline.clips.iter().all(|clip| clip.subtitle.is_none()) {
        return Ok(None);
    }
    let fps = timeline.fps;
    let mut joined: Vec<SrtCue> = Vec::new();
    for (clip, start) in timeline.clips.iter().zip(timeline.clip_starts()) {
        let Some(path) = &clip.subtitle else {
            continue;
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let in_point = milliseconds_at(clip.in_frame, fps);
        let out_point = milliseconds_at(clip.out_frame, fps);
        let place = milliseconds_at(start, fps);
        for cue in parse_srt(&text) {
            if cue.end_ms <= in_point || cue.start_ms >= out_point {
                continue;
            }
            joined.push(SrtCue {
                index: 0,
                start_ms: cue.start_ms.max(in_point) - in_point + place,
                end_ms: cue.end_ms.min(out_point) - in_point + place,
                text: cue.text,
            });
        }
    }
    // the clips are not listed in time order
    joined.sort_by_key(|cue| cue.start_ms);
    for (number, cue) in joined.iter_mut().enumerate() {
        cue.index = number as u32 + 1;
    }
    std::fs::write(output, format_srt(&joined))
        .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    Ok(Some(output.to_path_buf()))
}

pub fn render_black(timeline: &Timeline, output: &Path) -> Result<Option<PathBuf>, String> {
    let fps = timeline.fps;
    let Some(longest_gap) = timeline
        .base_gaps()
        .iter()
        .map(|gap| gap.end - gap.start)
        .max()
    else {
        return Ok(None);
    };
    let black = format!(
        "color=c=black:s={BLACK_GAP_SIZE}:r={}/{}:d={}",
        fps.numerator,
        fps.denominator,
        seconds_at(longest_gap, fps)
    );
    let finished = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &black])
        .args(["-c:v", "ffv1"])
        .arg(output)
        .output()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    if !finished.status.success() {
        return Err(format!(
            "ffmpeg cannot write the black for the gaps to {}: {}",
            output.display(),
            String::from_utf8_lossy(&finished.stderr).trim()
        ));
    }
    Ok(Some(output.to_path_buf()))
}

pub fn preview_renders_clip(clip: &Clip) -> bool {
    clip.layer > 0
        || crate::still::is_still_image(&clip.picture)
        || matches!(
            crate::encode::detect_input_type(&clip.picture),
            InputType::ImageSequence
        )
}

pub fn preview_renders_any_stretch(timeline: &Timeline) -> bool {
    timeline.clips.iter().any(preview_renders_clip)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedStretch {
    pub frames: Range<u64>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayedPicture {
    // `first_frame` counts in the clip's source
    Clip { index: usize, first_frame: u64 },
    Black,
    // from the file's first frame
    Rendered(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayedStretch {
    pub frames: Range<u64>,
    pub picture: PlayedPicture,
}

// the rendered stretches play in place of the layer 0 clips and gaps under them
pub fn played_stretches(timeline: &Timeline, rendered: &[RenderedStretch]) -> Vec<PlayedStretch> {
    let mut rendered = rendered.to_vec();
    rendered.sort_by_key(|stretch| stretch.frames.start);
    let mut played: Vec<PlayedStretch> = rendered
        .iter()
        .map(|stretch| PlayedStretch {
            frames: stretch.frames.clone(),
            picture: PlayedPicture::Rendered(stretch.path.clone()),
        })
        .collect();
    for (index, clip) in timeline.base_clips() {
        for frames in outside_rendered(clip.start..clip.end(), &rendered) {
            let first_frame = clip.in_frame + (frames.start - clip.start);
            played.push(PlayedStretch {
                frames,
                picture: PlayedPicture::Clip { index, first_frame },
            });
        }
    }
    for gap in timeline.base_gaps() {
        for frames in outside_rendered(gap, &rendered) {
            played.push(PlayedStretch {
                frames,
                picture: PlayedPicture::Black,
            });
        }
    }
    played.sort_by_key(|stretch| stretch.frames.start);
    played
}

// `rendered` is in start order
fn outside_rendered(frames: Range<u64>, rendered: &[RenderedStretch]) -> Vec<Range<u64>> {
    let mut parts = Vec::new();
    let mut from = frames.start;
    for stretch in rendered {
        if stretch.frames.end <= from || stretch.frames.start >= frames.end {
            continue;
        }
        if stretch.frames.start > from {
            parts.push(from..stretch.frames.start);
        }
        from = stretch.frames.end;
    }
    if from < frames.end {
        parts.push(from..frames.end);
    }
    parts
}

pub fn mpv_edl_uri(
    timeline: &Timeline,
    sound: Option<&Path>,
    black: Option<&Path>,
    rendered: &[RenderedStretch],
) -> Result<String, String> {
    let fps = timeline.fps;
    let pictures: Vec<PictureSegment> = played_stretches(timeline, rendered)
        .into_iter()
        .map(|played| {
            let frames = played.frames.end - played.frames.start;
            let (path, first_frame) = match played.picture {
                PlayedPicture::Clip { index, first_frame } => {
                    (timeline.clips[index].picture.clone(), first_frame)
                }
                PlayedPicture::Rendered(path) => (path, 0),
                PlayedPicture::Black => {
                    let Some(black) = black else {
                        return Err(format!(
                            "layer 0 has a gap of {frames} frames at frame {}, and no black \
                             source was rendered for it",
                            played.frames.start
                        ));
                    };
                    (black.to_path_buf(), 0)
                }
            };
            Ok(PictureSegment {
                path,
                trim: Some(SegmentTrim {
                    start_seconds: seconds_at(first_frame, fps),
                    length_seconds: Some(seconds_at(frames, fps)),
                }),
            })
        })
        .collect::<Result<_, String>>()?;
    let picture_uri = edl_uri(&pictures);
    let Some(sound) = sound else {
        return Ok(picture_uri);
    };
    let whole_timeline = SegmentTrim {
        start_seconds: 0.0,
        length_seconds: Some(seconds_at(timeline.total_frames(), fps)),
    };
    Ok(format!(
        "{picture_uri};{EDL_SEPARATE_STREAM};{}",
        edl_segment(sound, Some(&whole_timeline))
    ))
}

fn seconds_at(frame: u64, fps: FrameRate) -> f64 {
    frame as f64 * f64::from(fps.denominator) / f64::from(fps.numerator)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: FrameRate = FrameRate {
        numerator: 24,
        denominator: 1,
    };

    fn clip(picture: &Path, start: u64, in_frame: u64, out_frame: u64, sound: ClipSound) -> Clip {
        Clip {
            picture: picture.to_path_buf(),
            start,
            layer: 0,
            opacity: 1.0,
            in_frame,
            out_frame,
            sound,
            subtitle: None,
            source_colour: SourceColour::default(),
            picture_processing: PictureProcessing::default(),
        }
    }

    fn on_layer(mut clip: Clip, layer: u32) -> Clip {
        clip.layer = layer;
        clip
    }

    fn timeline(clips: Vec<Clip>) -> Timeline {
        Timeline {
            version: TIMELINE_FORMAT_VERSION,
            fps: RATE,
            clips,
            sound: SoundTrack::PerClip,
            subtitle: None,
            reel_starts: Vec::new(),
        }
    }

    fn still_sequence(directory: &Path, frames: usize) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        for frame in 0..frames {
            std::fs::write(directory.join(format!("shot_{frame:04}.png")), b"png").unwrap();
        }
        directory.to_path_buf()
    }

    fn wav_of(path: &Path, spec: WavSpec, samples: &[i32]) {
        let mut writer = WavWriter::create(path, spec, samples.len() as u64).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn stereo(bits_per_sample: u16) -> WavSpec {
        WavSpec {
            channels: 2,
            sample_rate: SOUND_SAMPLE_RATE,
            bits_per_sample,
            sample_format: SampleFormat::Int,
        }
    }

    fn read_samples(path: &Path) -> Vec<i32> {
        WavReader::open(path)
            .unwrap()
            .samples::<i32>()
            .map(Result::unwrap)
            .collect()
    }

    const CODESTREAM_FRAMES: u64 = 10;

    fn codestream_directory(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        for frame in 0..CODESTREAM_FRAMES {
            std::fs::write(directory.join(format!("frame_{frame:08}.j2c")), [0u8]).unwrap();
        }
        directory.to_path_buf()
    }

    #[test]
    fn one_clip_maps_its_in_and_out_points_onto_the_trims() {
        let directory = tempfile::tempdir().unwrap();
        let picture = codestream_directory(&directory.path().join("feature"));
        let mut one_clip = timeline(vec![clip(&picture, 0, 2, 7, ClipSound::None)]);
        one_clip.reel_starts = vec![3];

        let TimelineShape::SingleClip(build) = shape(&one_clip).unwrap() else {
            panic!("one clip is the one-picture build");
        };
        assert_eq!(
            build,
            SingleClipBuild {
                picture,
                trim_start_frames: 2,
                trim_end_frames: CODESTREAM_FRAMES - 7,
                audio: None,
                silent: true,
                subtitle: None,
                subtitle_from_clip: false,
                reel_split_frames: vec![3],
                still_hold_frames: None,
            }
        );
    }

    #[test]
    fn one_clip_takes_the_override_or_its_own_sound_and_subtitle() {
        let directory = tempfile::tempdir().unwrap();
        let picture = codestream_directory(&directory.path().join("feature"));
        let clip_wav = directory.path().join("clip.wav");
        let mut with_clip_sound = clip(&picture, 0, 0, 10, ClipSound::File(clip_wav.clone()));
        with_clip_sound.subtitle = Some(directory.path().join("clip.srt"));
        let mut one_clip = timeline(vec![with_clip_sound]);

        let TimelineShape::SingleClip(build) = shape(&one_clip).unwrap() else {
            panic!("one clip is the one-picture build");
        };
        assert_eq!(build.audio, Some(clip_wav));
        assert!(!build.silent);
        assert_eq!(build.subtitle, Some(directory.path().join("clip.srt")));
        assert!(build.subtitle_from_clip);

        let mix = directory.path().join("mix.wav");
        one_clip.sound = SoundTrack::Override(mix.clone());
        one_clip.clips[0].subtitle = None;
        one_clip.subtitle = Some(directory.path().join("timeline.srt"));
        let TimelineShape::SingleClip(build) = shape(&one_clip).unwrap() else {
            panic!("one clip is the one-picture build");
        };
        assert_eq!(build.audio, Some(mix));
        assert_eq!(build.subtitle, Some(directory.path().join("timeline.srt")));
        assert!(!build.subtitle_from_clip);
    }

    #[test]
    fn one_clip_refuses_two_subtitle_sources_and_its_own_colour() {
        let directory = tempfile::tempdir().unwrap();
        let picture = codestream_directory(&directory.path().join("feature"));
        let mut with_subtitle = clip(&picture, 0, 0, 10, ClipSound::None);
        with_subtitle.subtitle = Some(directory.path().join("clip.srt"));
        let mut one_clip = timeline(vec![with_subtitle]);
        one_clip.subtitle = Some(directory.path().join("timeline.srt"));
        let Err(refusal) = shape(&one_clip) else {
            panic!("two subtitle sources have to be refused");
        };
        assert!(
            refusal.contains("timeline.srt") && refusal.contains("clip 1"),
            "{refusal}"
        );

        let mut coloured = clip(&picture, 0, 0, 10, ClipSound::None);
        coloured.source_colour = SourceColour::KeepRgb;
        let Err(refusal) = shape(&timeline(vec![coloured])) else {
            panic!("a clip colour has no single-picture field to go to");
        };
        assert!(refusal.contains("--source-colourspace"), "{refusal}");
    }

    #[test]
    fn more_than_one_clip_is_the_multi_clip_build() {
        let picture = Path::new("/media/a.mov");
        let two_clips = timeline(vec![
            clip(picture, 0, 0, 1, ClipSound::None),
            clip(picture, 1, 0, 1, ClipSound::None),
        ]);
        assert!(matches!(shape(&two_clips), Ok(TimelineShape::MultiClip)));
    }

    #[test]
    fn the_clip_starts_are_the_start_frames_in_clip_order() {
        let picture = Path::new("/media/a.mov");
        let timeline = timeline(vec![
            clip(picture, 0, 10, 34, ClipSound::None),
            clip(picture, 24, 0, 5, ClipSound::None),
            clip(picture, 29, 100, 148, ClipSound::None),
        ]);
        assert_eq!(timeline.clip_starts(), vec![0, 24, 29]);
        assert_eq!(timeline.total_frames(), 77);
    }

    #[test]
    fn a_timeline_file_reads_back_as_written_and_refuses_another_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cut.json");
        let mut written = timeline(vec![clip(
            Path::new("/media/a.mov"),
            0,
            0,
            24,
            ClipSound::File(PathBuf::from("/media/a.wav")),
        )]);
        written.reel_starts = vec![12];
        written.write(&path).unwrap();
        assert_eq!(Timeline::read(&path).unwrap(), written);

        let older = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\": 2", "\"version\": 1");
        std::fs::write(&path, older).unwrap();
        let refused = Timeline::read(&path).unwrap_err();
        assert!(refused.contains("of version 1"), "{refused}");
        assert!(refused.contains("reads version 2"), "{refused}");

        written.version = 1;
        let refused = written.write(&path).unwrap_err();
        assert!(refused.contains("version 1"), "{refused}");
    }

    #[test]
    fn a_hand_written_clip_can_leave_out_the_layer_opacity_colour_and_picture_processing() {
        let text = r#"{
            "version": 2,
            "fps": {"numerator": 25, "denominator": 1},
            "clips": [{"picture": "/media/a.mov", "start": 0, "in_frame": 0, "out_frame": 25,
                       "sound": "Embedded", "subtitle": null}],
            "sound": "PerClip",
            "subtitle": null
        }"#;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cut.json");
        std::fs::write(&path, text).unwrap();
        let read = Timeline::read(&path).unwrap();
        assert_eq!(read.clips[0].layer, 0);
        assert_eq!(read.clips[0].opacity, 1.0);
        assert_eq!(read.clips[0].source_colour, SourceColour::DisplayRgb);
        assert!(read.clips[0].picture_processing.is_identity());
        assert!(read.reel_starts.is_empty());
    }

    #[test]
    fn the_check_refuses_a_bad_clip_by_number_and_path() {
        let directory = tempfile::tempdir().unwrap();
        let stills = still_sequence(&directory.path().join("stills"), 10);
        assert!(
            timeline(Vec::new())
                .check()
                .unwrap_err()
                .contains("no clips")
        );
        assert!(
            timeline(vec![clip(&stills, 0, 0, 10, ClipSound::None)])
                .check()
                .is_ok(),
            "a still sequence takes the timeline's rate"
        );
        for (in_frame, out_frame, expected) in [
            (4, 4, "at or before the in point 4"),
            (10, 12, "the in point 10 is past the source's 10 frames"),
            (2, 11, "the out point 11 is past the source's 10 frames"),
        ] {
            let refused = timeline(vec![
                clip(&stills, 0, 0, 10, ClipSound::None),
                clip(&stills, 10, in_frame, out_frame, ClipSound::None),
            ])
            .check()
            .unwrap_err();
            assert!(refused.contains(expected), "{refused}");
            assert!(
                refused.starts_with(&format!("clip 2 ({})", stills.display())),
                "{refused}"
            );
        }
    }

    fn still_image(path: &Path) -> PathBuf {
        std::fs::write(path, b"png").unwrap();
        path.to_path_buf()
    }

    #[test]
    fn a_still_is_held_from_its_first_frame_and_a_sequence_counts_its_images() {
        let directory = tempfile::tempdir().unwrap();
        let stills = still_sequence(&directory.path().join("stills"), 10);
        let card = still_image(&directory.path().join("card.png"));
        assert_eq!(probe_source(&card).unwrap(), SourceLength::Still);
        assert_eq!(
            probe_source(&stills).unwrap(),
            SourceLength::Counted {
                frames: 10,
                rate: None
            }
        );
        assert!(
            timeline(vec![
                clip(&stills, 0, 0, 10, ClipSound::None),
                clip(&card, 10, 0, 500, ClipSound::None),
            ])
            .check()
            .is_ok(),
            "a still has no frame count for its out point to run past"
        );
        assert_eq!(
            timeline(vec![
                clip(&stills, 0, 0, 10, ClipSound::None),
                clip(&card, 10, 3, 10, ClipSound::None),
            ])
            .check()
            .unwrap_err(),
            format!(
                "clip 2 ({}): the in point is 3, and a still is held from its first frame: its in \
                 point is 0 and its out point sets the hold",
                card.display()
            )
        );
    }

    #[test]
    fn one_still_clip_is_held_for_its_out_point_with_no_trims() {
        let directory = tempfile::tempdir().unwrap();
        let card = still_image(&directory.path().join("card.png"));
        let TimelineShape::SingleClip(build) =
            shape(&timeline(vec![clip(&card, 0, 0, 120, ClipSound::None)])).unwrap()
        else {
            panic!("one clip is the one-picture build");
        };
        assert_eq!(build.picture, card);
        assert_eq!(build.still_hold_frames, Some(120));
        assert_eq!((build.trim_start_frames, build.trim_end_frames), (0, 0));
    }

    #[test]
    fn the_check_refuses_reel_starts_out_of_order_or_past_the_end() {
        let directory = tempfile::tempdir().unwrap();
        let stills = still_sequence(&directory.path().join("stills"), 10);
        let mut timeline = timeline(vec![clip(&stills, 0, 0, 10, ClipSound::None)]);
        for (reel_starts, expected) in [
            (vec![0, 5], "reel start 0"),
            (vec![5, 5], "listed twice"),
            (vec![6, 3], "reel start 3 comes after 6"),
            (vec![10], "at or past the timeline's end at frame 10"),
        ] {
            timeline.reel_starts = reel_starts;
            let refused = timeline.check().unwrap_err();
            assert!(refused.contains(expected), "{refused}");
        }
        timeline.reel_starts = vec![3, 9];
        assert!(timeline.check().is_ok());
    }

    #[test]
    fn rates_compare_as_fractions() {
        assert!(!rate_differs(
            FrameRate::new(24000, 1001),
            FrameRate::new(48000, 2002)
        ));
        assert!(rate_differs(
            FrameRate::new(24000, 1001),
            FrameRate::whole(24)
        ));
    }

    #[test]
    fn clip_sound_is_cut_to_the_frames_and_padded_with_silence() {
        let directory = tempfile::tempdir().unwrap();
        let samples_per_frame = (SOUND_SAMPLE_RATE / RATE.numerator) as usize;
        let first = directory.path().join("first.wav");
        let numbered: Vec<i32> = (0..(4 * samples_per_frame) as i32)
            .flat_map(|frame| [frame, -frame])
            .collect();
        wav_of(&first, stereo(24), &numbered);
        let short = directory.path().join("short.wav");
        wav_of(&short, stereo(16), &[100, -100, 200, -200]);

        let picture = Path::new("/media/a.mov");
        let timeline = timeline(vec![
            clip(picture, 0, 1, 3, ClipSound::File(first)),
            clip(picture, 2, 0, 1, ClipSound::None),
            clip(picture, 3, 0, 1, ClipSound::File(short)),
        ]);
        let output = directory.path().join("timeline.wav");
        let rendered = render_sound(&timeline, &output).unwrap().unwrap();
        assert_eq!(rendered, output);

        let reader = WavReader::open(&output).unwrap();
        assert_eq!(reader.spec(), stereo(24));
        assert_eq!(reader.duration(), (4 * samples_per_frame) as u64);
        let joined = read_samples(&output);
        let first_clip = &joined[..4 * samples_per_frame];
        assert_eq!(
            first_clip[0], samples_per_frame as i32,
            "starts at the in point"
        );
        assert_eq!(first_clip[1], -(samples_per_frame as i32));
        assert_eq!(
            first_clip[4 * samples_per_frame - 2],
            (3 * samples_per_frame - 1) as i32
        );
        let silent_clip = &joined[4 * samples_per_frame..6 * samples_per_frame];
        assert!(silent_clip.iter().all(|&sample| sample == 0));
        let short_clip = &joined[6 * samples_per_frame..];
        assert_eq!(
            &short_clip[..4],
            &[100 << 8, -100 << 8, 200 << 8, -200 << 8]
        );
        assert!(short_clip[4..].iter().all(|&sample| sample == 0));
    }

    #[test]
    fn clip_sound_refuses_a_channel_count_that_differs() {
        let directory = tempfile::tempdir().unwrap();
        let stereo_wav = directory.path().join("stereo.wav");
        wav_of(&stereo_wav, stereo(24), &[0, 0]);
        let mono_wav = directory.path().join("mono.wav");
        wav_of(
            &mono_wav,
            WavSpec {
                channels: 1,
                ..stereo(24)
            },
            &[0],
        );
        let timeline = timeline(vec![
            clip(
                Path::new("/media/a.mov"),
                0,
                0,
                1,
                ClipSound::File(stereo_wav),
            ),
            clip(
                Path::new("/media/b.mov"),
                1,
                0,
                1,
                ClipSound::File(mono_wav),
            ),
        ]);
        let refused = render_sound(&timeline, &directory.path().join("out.wav")).unwrap_err();
        assert!(
            refused.contains("clip 1 (/media/a.mov) has 2 sound channels"),
            "{refused}"
        );
        assert!(refused.contains("clip 2 (/media/b.mov) has 1"), "{refused}");
    }

    #[test]
    fn a_timeline_with_no_clip_sound_renders_none_and_an_override_is_returned_as_is() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.wav");
        let mut timeline = timeline(vec![clip(Path::new("/a.mov"), 0, 0, 1, ClipSound::None)]);
        assert_eq!(render_sound(&timeline, &output).unwrap(), None);
        assert!(!output.exists());

        timeline.sound = SoundTrack::Override(PathBuf::from("/mix/full.wav"));
        assert_eq!(
            render_sound(&timeline, &output).unwrap(),
            Some(PathBuf::from("/mix/full.wav"))
        );
    }

    #[test]
    fn clip_subtitles_are_shifted_to_the_clip_and_cut_to_its_range() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.srt");
        std::fs::write(
            &first,
            "1\n00:00:00,000 --> 00:00:00,900\nbefore the in point\n\n\
             2\n00:00:00,500 --> 00:00:01,500\nstraddles the in point\n\n\
             3\n00:00:02,000 --> 00:00:04,000\nstraddles the out point\n\n\
             4\n00:00:03,000 --> 00:00:03,500\nafter the out point\n",
        )
        .unwrap();
        let second = directory.path().join("second.srt");
        std::fs::write(&second, "1\n00:00:00,250 --> 00:00:00,750\nsecond clip\n").unwrap();

        let picture = Path::new("/media/a.mov");
        let mut clips = vec![
            clip(picture, 0, 24, 72, ClipSound::None),
            clip(picture, 48, 0, 12, ClipSound::None),
            clip(picture, 60, 0, 24, ClipSound::None),
        ];
        clips[0].subtitle = Some(first);
        clips[2].subtitle = Some(second);
        let output = directory.path().join("joined.srt");
        join_subtitles(&timeline(clips), &output).unwrap().unwrap();

        let cues = parse_srt(&std::fs::read_to_string(&output).unwrap());
        let timings: Vec<(u32, u64, u64, &str)> = cues
            .iter()
            .map(|cue| (cue.index, cue.start_ms, cue.end_ms, cue.text.as_str()))
            .collect();
        assert_eq!(
            timings,
            vec![
                (1, 0, 500, "straddles the in point"),
                (2, 1000, 2000, "straddles the out point"),
                (3, 2750, 3250, "second clip"),
            ]
        );
    }

    #[test]
    fn no_clip_subtitle_joins_to_none() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("joined.srt");
        let timeline = timeline(vec![clip(Path::new("/a.mov"), 0, 0, 1, ClipSound::None)]);
        assert_eq!(join_subtitles(&timeline, &output).unwrap(), None);
        assert!(!output.exists());
    }

    #[test]
    fn the_edl_trims_each_clip_and_plays_the_sound_beside_them() {
        let timeline = timeline(vec![
            clip(Path::new("/media/a,b.mov"), 0, 12, 36, ClipSound::None),
            clip(Path::new("/media/c.mov"), 24, 0, 48, ClipSound::None),
        ]);
        assert_eq!(
            mpv_edl_uri(&timeline, Some(Path::new("/work/sound.wav")), None, &[]).unwrap(),
            "edl://%14%/media/a,b.mov,0.5,1;%12%/media/c.mov,0,2;!new_stream;!no_clip;\
             %15%/work/sound.wav,0,3"
        );
        assert_eq!(
            mpv_edl_uri(&timeline, None, None, &[]).unwrap(),
            "edl://%14%/media/a,b.mov,0.5,1;%12%/media/c.mov,0,2"
        );
    }

    #[test]
    fn total_frames_runs_to_the_furthest_end_on_any_layer() {
        let picture = Path::new("/media/a.mov");
        let past_the_base = timeline(vec![
            clip(picture, 0, 0, 24, ClipSound::None),
            on_layer(clip(picture, 12, 0, 36, ClipSound::None), 1),
        ]);
        assert_eq!(past_the_base.total_frames(), 48);
        let with_a_gap = timeline(vec![
            clip(picture, 0, 0, 24, ClipSound::None),
            clip(picture, 36, 0, 12, ClipSound::None),
        ]);
        assert_eq!(with_a_gap.total_frames(), 48);
        assert_eq!(timeline(Vec::new()).total_frames(), 0);
    }

    #[test]
    fn the_check_refuses_a_bad_opacity_upper_layer_sound_and_overlap_on_one_layer() {
        let directory = tempfile::tempdir().unwrap();
        let stills = still_sequence(&directory.path().join("stills"), 10);
        let first = format!("clip 1 ({})", stills.display());
        let second = format!("clip 2 ({})", stills.display());
        for opacity in [1.5, -0.1] {
            let mut faded = clip(&stills, 0, 0, 10, ClipSound::None);
            faded.opacity = opacity;
            assert_eq!(
                timeline(vec![faded]).check().unwrap_err(),
                format!("{first}: the opacity {opacity} is outside 0 to 1")
            );
        }
        let mut not_a_number = clip(&stills, 0, 0, 10, ClipSound::None);
        not_a_number.opacity = f64::NAN;
        let refused = timeline(vec![not_a_number]).check().unwrap_err();
        assert!(refused.contains("is outside 0 to 1"), "{refused}");

        let sounding = on_layer(clip(&stills, 0, 0, 10, ClipSound::Embedded), 2);
        assert_eq!(
            timeline(vec![sounding]).check().unwrap_err(),
            format!("{first} is on layer 2 and carries sound: only layer 0 clips sound")
        );

        let overlapping = timeline(vec![
            clip(&stills, 4, 0, 10, ClipSound::None),
            clip(&stills, 0, 0, 6, ClipSound::None),
        ]);
        assert_eq!(
            overlapping.check().unwrap_err(),
            format!(
                "{first} starts at frame 4 while {second} on layer 0 runs to frame 6: clips on \
                 one layer cannot overlap"
            )
        );

        let layered_with_a_gap = timeline(vec![
            clip(&stills, 0, 0, 10, ClipSound::None),
            clip(&stills, 20, 0, 10, ClipSound::None),
            on_layer(clip(&stills, 5, 0, 10, ClipSound::None), 1),
            on_layer(clip(&stills, 0, 0, 10, ClipSound::None), 3),
        ]);
        assert!(layered_with_a_gap.check().is_ok());
    }

    #[test]
    fn one_clip_off_frame_0_above_layer_0_or_faded_is_the_multi_clip_build() {
        let picture = Path::new("/media/a.mov");
        let late = clip(picture, 24, 0, 1, ClipSound::None);
        let upper = on_layer(clip(picture, 0, 0, 1, ClipSound::None), 1);
        let mut faded = clip(picture, 0, 0, 1, ClipSound::None);
        faded.opacity = 0.5;
        for one_clip in [late, upper, faded] {
            assert!(matches!(
                shape(&timeline(vec![one_clip])),
                Ok(TimelineShape::MultiClip)
            ));
        }
    }

    #[test]
    fn clip_sound_lands_at_the_clip_start_with_silence_in_the_gaps() {
        let directory = tempfile::tempdir().unwrap();
        let samples_per_frame = (SOUND_SAMPLE_RATE / RATE.numerator) as usize;
        let numbered_wav = directory.path().join("numbered.wav");
        let numbered: Vec<i32> = (0..(4 * samples_per_frame) as i32)
            .flat_map(|frame| [frame, -frame])
            .collect();
        wav_of(&numbered_wav, stereo(24), &numbered);

        let picture = Path::new("/media/a.mov");
        let timeline = timeline(vec![
            on_layer(clip(picture, 0, 0, 5, ClipSound::None), 1),
            clip(picture, 2, 1, 3, ClipSound::File(numbered_wav)),
        ]);
        let output = directory.path().join("timeline.wav");
        render_sound(&timeline, &output).unwrap().unwrap();

        let joined = read_samples(&output);
        assert_eq!(joined.len(), 2 * 5 * samples_per_frame);
        let gap_before = &joined[..4 * samples_per_frame];
        assert!(gap_before.iter().all(|&sample| sample == 0));
        let clip_sound = &joined[4 * samples_per_frame..8 * samples_per_frame];
        assert_eq!(clip_sound[0], samples_per_frame as i32, "the in point");
        assert_eq!(clip_sound[1], -(samples_per_frame as i32));
        assert_eq!(
            clip_sound[4 * samples_per_frame - 2],
            (3 * samples_per_frame - 1) as i32
        );
        let gap_after = &joined[8 * samples_per_frame..];
        assert!(gap_after.iter().all(|&sample| sample == 0));
    }

    #[test]
    fn clip_subtitles_shift_by_the_clip_start_across_a_gap_on_any_layer() {
        let directory = tempfile::tempdir().unwrap();
        let title = directory.path().join("title.srt");
        std::fs::write(&title, "1\n00:00:00,000 --> 00:00:00,250\ntitle\n").unwrap();
        let late = directory.path().join("late.srt");
        std::fs::write(&late, "1\n00:00:00,250 --> 00:00:00,750\nlate clip\n").unwrap();

        let picture = Path::new("/media/a.mov");
        let mut clips = vec![
            clip(picture, 0, 0, 12, ClipSound::None),
            on_layer(clip(picture, 12, 0, 12, ClipSound::None), 1),
            clip(picture, 48, 0, 24, ClipSound::None),
        ];
        clips[1].subtitle = Some(title);
        clips[2].subtitle = Some(late);
        let output = directory.path().join("joined.srt");
        join_subtitles(&timeline(clips), &output).unwrap().unwrap();

        let timings: Vec<(u64, u64, String)> =
            parse_srt(&std::fs::read_to_string(&output).unwrap())
                .into_iter()
                .map(|cue| (cue.start_ms, cue.end_ms, cue.text))
                .collect();
        assert_eq!(
            timings,
            vec![
                (500, 750, "title".to_string()),
                (2250, 2750, "late clip".to_string()),
            ]
        );
    }

    #[test]
    fn joined_subtitles_run_in_time_order_whatever_order_the_clips_are_listed_in() {
        let directory = tempfile::tempdir().unwrap();
        let late = directory.path().join("late.srt");
        std::fs::write(&late, "1\n00:00:00,000 --> 00:00:00,500\nlate\n").unwrap();
        let early = directory.path().join("early.srt");
        std::fs::write(&early, "1\n00:00:00,000 --> 00:00:00,500\nearly\n").unwrap();

        let picture = Path::new("/media/a.mov");
        let mut clips = vec![
            clip(picture, 48, 0, 24, ClipSound::None),
            on_layer(clip(picture, 0, 0, 24, ClipSound::None), 1),
        ];
        clips[0].subtitle = Some(late);
        clips[1].subtitle = Some(early);
        let output = directory.path().join("joined.srt");
        join_subtitles(&timeline(clips), &output).unwrap().unwrap();

        let cues: Vec<(u32, u64, String)> = parse_srt(&std::fs::read_to_string(&output).unwrap())
            .into_iter()
            .map(|cue| (cue.index, cue.start_ms, cue.text))
            .collect();
        assert_eq!(
            cues,
            vec![(1, 0, "early".to_string()), (2, 2000, "late".to_string())]
        );
    }

    #[test]
    fn the_black_for_the_gaps_is_as_long_as_the_longest_gap() {
        let directory = tempfile::tempdir().unwrap();
        let picture = Path::new("/media/a.mov");
        let output = directory.path().join("black.mkv");
        let no_gap = timeline(vec![
            clip(picture, 0, 0, 24, ClipSound::None),
            clip(picture, 24, 0, 24, ClipSound::None),
        ]);
        assert_eq!(render_black(&no_gap, &output).unwrap(), None);
        assert!(!output.exists());

        let gaps = timeline(vec![
            clip(picture, 12, 0, 24, ClipSound::None),
            clip(picture, 60, 0, 12, ClipSound::None),
            on_layer(clip(picture, 0, 0, 90, ClipSound::None), 1),
        ]);
        assert_eq!(gaps.base_gaps(), vec![0..12, 36..60, 72..90]);
        assert_eq!(render_black(&gaps, &output).unwrap(), Some(output.clone()));
        assert!(matches!(
            probe_source(&output).unwrap(),
            SourceLength::Counted { frames: 24, .. }
        ));
    }

    #[test]
    fn the_preview_renders_a_timeline_with_an_upper_layer_a_still_or_an_image_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let video = Path::new("/media/a.mov");
        let still = still_image(&directory.path().join("card.png"));
        let sequence = still_sequence(&directory.path().join("shots"), 3);
        let base = clip(video, 0, 0, 24, ClipSound::None);
        let cases = [
            (vec![base.clone()], false, "a plain video"),
            (Vec::new(), false, "an empty timeline"),
            (
                vec![
                    base.clone(),
                    on_layer(clip(video, 4, 0, 8, ClipSound::None), 1),
                ],
                true,
                "a clip on layer 1",
            ),
            (
                vec![base.clone(), clip(&still, 24, 0, 8, ClipSound::None)],
                true,
                "a still on layer 0",
            ),
            (
                vec![clip(&sequence, 0, 0, 3, ClipSound::None), base],
                true,
                "an image sequence on layer 0",
            ),
        ];
        for (clips, renders, what) in cases {
            assert_eq!(
                preview_renders_any_stretch(&timeline(clips)),
                renders,
                "{what}"
            );
        }
    }

    #[test]
    fn a_gap_a_rendered_stretch_covers_plays_it_and_needs_no_black() {
        let timeline = timeline(vec![
            clip(Path::new("/media/a.mov"), 0, 12, 36, ClipSound::None),
            clip(Path::new("/media/c.mov"), 48, 0, 24, ClipSound::None),
        ]);
        let rendered = [
            RenderedStretch {
                frames: 12..24,
                path: PathBuf::from("/cache/one.mkv"),
            },
            RenderedStretch {
                frames: 24..48,
                path: PathBuf::from("/cache/two.mkv"),
            },
        ];
        assert_eq!(
            mpv_edl_uri(&timeline, None, None, &rendered).unwrap(),
            "edl://%12%/media/a.mov,0.5,0.5;%14%/cache/one.mkv,0,0.5;%14%/cache/two.mkv,0,1;\
             %12%/media/c.mov,0,1"
        );
    }

    #[test]
    fn the_edl_fills_a_base_gap_with_black_and_leaves_upper_layers_out() {
        let timeline = timeline(vec![
            clip(Path::new("/media/a.mov"), 0, 12, 36, ClipSound::None),
            on_layer(
                clip(Path::new("/media/title.mov"), 12, 0, 24, ClipSound::None),
                1,
            ),
            clip(Path::new("/media/c.mov"), 48, 0, 24, ClipSound::None),
        ]);
        assert_eq!(
            mpv_edl_uri(&timeline, None, Some(Path::new("/work/black.mkv")), &[]).unwrap(),
            "edl://%12%/media/a.mov,0.5,1;%15%/work/black.mkv,0,1;%12%/media/c.mov,0,1"
        );
        assert_eq!(
            mpv_edl_uri(&timeline, None, None, &[]).unwrap_err(),
            "layer 0 has a gap of 24 frames at frame 24, and no black source was rendered for it"
        );
    }

    #[test]
    fn a_remuxed_dolby_vision_source_probes_the_frames_it_decodes() {
        use crate::dolby_vision::{
            DOLBY_VISION_FIXTURE_FRAMES, DolbyVisionFixtureProfile, write_dolby_vision_fixture,
        };
        let directory = tempfile::tempdir().unwrap();
        let annex_b = write_dolby_vision_fixture(
            directory.path(),
            "dv.hevc",
            DolbyVisionFixtureProfile::Profile81,
            None,
            None,
        )
        .unwrap();
        let mp4 = directory.path().join("dv.mp4");
        let remux = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(&annex_b)
            .args(["-c", "copy"])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(
            remux.status.success(),
            "{}",
            String::from_utf8_lossy(&remux.stderr)
        );
        let decoded = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
            .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
            .arg(&mp4)
            .output()
            .unwrap();
        let decoded: u64 = String::from_utf8_lossy(&decoded.stdout)
            .trim()
            .parse()
            .unwrap();

        let SourceLength::Counted { frames, .. } = probe_source(&mp4).unwrap() else {
            panic!("a video is counted");
        };
        assert_eq!(frames, decoded);
        assert_eq!(decoded, DOLBY_VISION_FIXTURE_FRAMES as u64);
    }
}

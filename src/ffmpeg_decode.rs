use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::encode::{DecodeSource, EncoderInputFormat, FrameRate};
use crate::probe::PixelFormatInfo;

// frames waiting between the source thread and the graph thread
const DECODED_FRAMES_QUEUED: usize = 4;
// frames waiting between the graph thread and the encoder's producer
const FILTERED_FRAMES_QUEUED: usize = 4;

// false takes the ffmpeg program's policy of dropping an undecodable frame
const FAIL_ON_FIRST_DECODE_ERROR: bool = true;
// under the ffmpeg program's policy a run fails past this share of bad frames
const MAX_DECODE_ERROR_RATE: f64 = 2.0 / 3.0;

// the ffmpeg program sizes a device's frame pool with this many extra frames
const EXTRA_DEVICE_FRAMES: i32 = 2;

// the ffmpeg program's frame rate when the filters report none
const FALLBACK_FRAME_RATE: ffi::AVRational = ffi::AVRational { num: 25, den: 1 };
// the ffmpeg program skips a duplication longer than this many frames
const DUPLICATION_LIMIT_FRAMES: f64 = 3600.0 * 30.0 * 30.0;

// hqdn3d needs a GPL FFmpeg
const LIBRARY_DENOISE_FILTER: &str = "atadenoise";

const PICTURE_SOURCE: &str = "in";
const PICTURE_SINK: &str = "out";
const FINDINGS_SINK: &str = "findings";

const BLACK_START_KEY: &str = "lavfi.black_start";
const BLACK_END_KEY: &str = "lavfi.black_end";
const FREEZE_START_KEY: &str = "lavfi.freezedetect.freeze_start";
const FREEZE_END_KEY: &str = "lavfi.freezedetect.freeze_end";

#[cfg(not(target_os = "macos"))]
const DEVICE: (ffi::AVHWDeviceType, ffi::AVPixelFormat, &str) = (
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
    ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
    "CUDA",
);
#[cfg(target_os = "macos")]
const DEVICE: (ffi::AVHWDeviceType, ffi::AVPixelFormat, &str) = (
    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
    ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX,
    "VideoToolbox",
);

pub(crate) struct DecodeRequest<'a> {
    pub input: &'a Path,
    pub decode_source: DecodeSource,
    pub read_source_at: Option<FrameRate>,
    pub hardware_decode: bool,
    // the whole picture chain, without the detection branch
    pub picture_filters: &'a str,
    pub input_format: EncoderInputFormat,
    pub width: u32,
    pub height: u32,
    pub frame_limit: Option<u64>,
    pub detect_picture_findings: bool,
}

pub(crate) enum NextFrame {
    Copied,
    Ended,
}

// what both decode threads report back
#[derive(Default)]
struct Outcome {
    failure: Option<String>,
    detection_lines: Vec<String>,
}

impl Outcome {
    fn fail(&mut self, message: String) {
        self.failure.get_or_insert(message);
    }
}

pub(crate) struct FrameDecoder {
    frames: Option<Receiver<ffmpeg::frame::Video>>,
    stop: Arc<AtomicBool>,
    outcome: Arc<Mutex<Outcome>>,
    threads: Vec<JoinHandle<()>>,
    input: String,
    width: u32,
    height: u32,
    frame_bytes: usize,
    frames_read: u64,
}

pub(crate) struct FinishedDecode {
    pub findings: crate::picture_findings::PictureFindings,
    pub failure: Option<String>,
}

// a system FFmpeg found first on the link path loads under headers it does not match
fn libraries_match_headers() -> Result<(), String> {
    let loaded = unsafe {
        [
            (
                "libavutil",
                ffi::avutil_version(),
                ffi::LIBAVUTIL_VERSION_MAJOR,
            ),
            (
                "libavcodec",
                ffi::avcodec_version(),
                ffi::LIBAVCODEC_VERSION_MAJOR,
            ),
            (
                "libavformat",
                ffi::avformat_version(),
                ffi::LIBAVFORMAT_VERSION_MAJOR,
            ),
            (
                "libavfilter",
                ffi::avfilter_version(),
                ffi::LIBAVFILTER_VERSION_MAJOR,
            ),
        ]
    };
    for (library, version, built_against) in loaded {
        let major = (version >> 16) as i32;
        if major != built_against {
            return Err(format!(
                "{library} {major} is loaded, but postkit was built against {library} {built_against}: \
                 point FFMPEG_DIR and the library path at one FFmpeg"
            ));
        }
    }
    Ok(())
}

fn ready() -> Result<(), String> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    READY
        .get_or_init(|| {
            libraries_match_headers()?;
            // fills in the error texts ffmpeg-next's Display reads
            ffmpeg::init().map_err(|e| format!("FFmpeg's libraries cannot start: {e}"))?;
            unsafe { ffi::av_log_set_level(ffi::AV_LOG_ERROR) };
            Ok(())
        })
        .clone()
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

fn q2d(rational: ffi::AVRational) -> f64 {
    rational.num as f64 / rational.den as f64
}

fn valid(rational: ffi::AVRational) -> bool {
    rational.num > 0 && rational.den > 0
}

fn inverse(rational: ffi::AVRational) -> ffi::AVRational {
    ffi::AVRational {
        num: rational.den,
        den: rational.num,
    }
}

fn rescale(value: i64, from: ffi::AVRational, to: ffi::AVRational) -> i64 {
    unsafe { ffi::av_rescale_q(value, from, to) }
}

// a frame that shares the buffers of `frame`
fn reference(frame: &ffmpeg::frame::Video) -> ffmpeg::frame::Video {
    let mut copy = ffmpeg::frame::Video::empty();
    let referenced = unsafe { ffi::av_frame_ref(copy.as_mut_ptr(), frame.as_ptr()) };
    assert!(referenced >= 0, "out of memory referencing a frame");
    copy
}

// ─── opening a source the way the ffmpeg program does ────────────────────────

fn utf8_path(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| {
        format!(
            "{} is not a UTF-8 path, which FFmpeg cannot open",
            shown(path)
        )
    })
}

fn open_input(path: &Path, source: DecodeSource) -> Result<ffmpeg::format::context::Input, String> {
    let name = utf8_path(path)?;
    let opened = match source {
        DecodeSource::Video => ffmpeg::format::input(&name),
        DecodeSource::ImageList => {
            let mut options = ffmpeg::Dictionary::new();
            options.set("safe", "0");
            let concat = unsafe { ffi::av_find_input_format(c"concat".as_ptr()) };
            if concat.is_null() {
                return Err("this FFmpeg build has no concat demuxer".to_string());
            }
            let format = unsafe { ffmpeg::format::Input::wrap(concat as *mut _) };
            ffmpeg::format::open_with(&name, &ffmpeg::Format::Input(format), options).and_then(
                |context| match context {
                    ffmpeg::format::context::Context::Input(input) => Ok(input),
                    ffmpeg::format::context::Context::Output(_) => Err(ffmpeg::Error::Bug),
                },
            )
        }
    };
    opened.map_err(|e| format!("cannot open {}: {e}", shown(path)))
}

// the ffmpeg program's pick: the largest picture, a cover image last
fn chosen_video_stream(input: &ffmpeg::format::context::Input) -> Option<usize> {
    const DEFAULT_STREAM_WEIGHT: i64 = 5_000_000;
    const NEW_PACKETS_WEIGHT: i64 = 100_000_000;
    const COVER_IMAGE_SCORE: i64 = 1;
    let mut best: Option<(usize, i64)> = None;
    for stream in input.streams() {
        let raw = unsafe { &*stream.as_ptr() };
        let parameters = unsafe { &*raw.codecpar };
        if parameters.codec_type != ffi::AVMediaType::AVMEDIA_TYPE_VIDEO {
            continue;
        }
        let mut score = i64::from(parameters.width) * i64::from(parameters.height);
        if raw.event_flags & ffi::AVSTREAM_EVENT_FLAG_NEW_PACKETS != 0 {
            score += NEW_PACKETS_WEIGHT;
        }
        if raw.disposition & ffi::AV_DISPOSITION_DEFAULT != 0 {
            score += DEFAULT_STREAM_WEIGHT;
        }
        if raw.disposition & ffi::AV_DISPOSITION_ATTACHED_PIC != 0 {
            score = COVER_IMAGE_SCORE;
        }
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((stream.index(), score));
        }
    }
    best.map(|(index, _)| index)
}

fn stream_parameters<'a>(
    stream: &'a ffmpeg::format::stream::Stream<'_>,
) -> &'a ffi::AVCodecParameters {
    unsafe { &*(*stream.as_ptr()).codecpar }
}

fn coded_side_data(
    parameters: &ffi::AVCodecParameters,
    kind: ffi::AVPacketSideDataType,
) -> Option<&[u8]> {
    unsafe {
        let side_data = ffi::av_packet_side_data_get(
            parameters.coded_side_data,
            parameters.nb_coded_side_data,
            kind,
        );
        if side_data.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(
            (*side_data).data,
            (*side_data).size,
        ))
    }
}

// top, bottom, left and right, as the side data stores them
fn container_crop(parameters: &ffi::AVCodecParameters) -> Option<[u32; 4]> {
    let data = coded_side_data(
        parameters,
        ffi::AVPacketSideDataType::AV_PKT_DATA_FRAME_CROPPING,
    )?;
    if data.len() < 16 {
        return None;
    }
    let edge = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let crop = [edge(0), edge(4), edge(8), edge(12)];
    crop.iter().any(|edge| *edge != 0).then_some(crop)
}

fn display_rotation_degrees(parameters: &ffi::AVCodecParameters) -> i64 {
    let Some(data) = coded_side_data(
        parameters,
        ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
    ) else {
        return 0;
    };
    if data.len() < 36 {
        return 0;
    }
    let rotation = unsafe { ffi::av_display_rotation_get(data.as_ptr() as *const i32) };
    if rotation.is_nan() {
        0
    } else {
        rotation as i64
    }
}

fn tag_name(name: *const libc::c_char, unset: bool) -> String {
    if unset || name.is_null() {
        return PixelFormatInfo::default().pix_fmt;
    }
    unsafe { std::ffi::CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

fn pixel_format_info(parameters: &ffi::AVCodecParameters) -> PixelFormatInfo {
    unsafe {
        let format: ffi::AVPixelFormat = std::mem::transmute(parameters.format);
        PixelFormatInfo {
            pix_fmt: tag_name(ffi::av_get_pix_fmt_name(format), false),
            color_space: tag_name(
                ffi::av_color_space_name(parameters.color_space),
                parameters.color_space == ffi::AVColorSpace::AVCOL_SPC_UNSPECIFIED,
            ),
            color_range: tag_name(
                ffi::av_color_range_name(parameters.color_range),
                parameters.color_range == ffi::AVColorRange::AVCOL_RANGE_UNSPECIFIED,
            ),
            color_transfer: tag_name(
                ffi::av_color_transfer_name(parameters.color_trc),
                parameters.color_trc == ffi::AVColorTransferCharacteristic::AVCOL_TRC_UNSPECIFIED,
            ),
            color_primaries: tag_name(
                ffi::av_color_primaries_name(parameters.color_primaries),
                parameters.color_primaries == ffi::AVColorPrimaries::AVCOL_PRI_UNSPECIFIED,
            ),
        }
    }
}

// ─── probing ─────────────────────────────────────────────────────────────────

pub(crate) struct SourceProbe {
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    pub pixel_format: PixelFormatInfo,
}

fn packet_count(path: &Path, source: DecodeSource, stream_index: usize) -> Result<u64, String> {
    let mut input = open_input(path, source)?;
    let mut count = 0u64;
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) if packet.stream() == stream_index => count += 1,
            Ok(()) | Err(ffmpeg::Error::InvalidData) => {}
            Err(_) => return Ok(count),
        }
    }
}

// what the ffprobe probes report, read from the stream the decode opens
pub(crate) fn probe(path: &Path, source: DecodeSource) -> Result<SourceProbe, String> {
    ready()?;
    let input = open_input(path, source)?;
    let index = chosen_video_stream(&input)
        .ok_or_else(|| format!("{} holds no video stream", shown(path)))?;
    let stream = input.stream(index).expect("the chosen stream exists");
    let parameters = stream_parameters(&stream);
    let crop = container_crop(parameters).unwrap_or_default();
    let (width, height) = crate::probe::DisplaySideData {
        crop_top: crop[0],
        crop_bottom: crop[1],
        crop_left: crop[2],
        crop_right: crop[3],
        rotation_degrees: display_rotation_degrees(parameters),
    }
    .applied_to(parameters.width as u32, parameters.height as u32);
    let rate = stream.rate();
    let duration = stream.duration();
    let measured_by_duration = source == DecodeSource::Video
        && rate.numerator() > 0
        && rate.denominator() > 0
        && duration != ffi::AV_NOPTS_VALUE;
    let frame_count = if measured_by_duration {
        let seconds = duration as f64 * f64::from(stream.time_base());
        u64::from(crate::probe::frames_in(
            seconds,
            rate.numerator() as u32,
            rate.denominator() as u32,
        ))
    } else {
        packet_count(path, source, index)?
    };
    Ok(SourceProbe {
        width,
        height,
        frame_count,
        pixel_format: pixel_format_info(parameters),
    })
}

// ─── the decoder ─────────────────────────────────────────────────────────────

struct SharedDevice(*mut ffi::AVBufferRef);
// a device context is reference counted and thread safe in FFmpeg
unsafe impl Send for SharedDevice {}
unsafe impl Sync for SharedDevice {}

// one device for the whole process, so each encode skips the device start
fn shared_device() -> Result<&'static SharedDevice, String> {
    static DEVICE_CONTEXT: OnceLock<Result<SharedDevice, String>> = OnceLock::new();
    DEVICE_CONTEXT
        .get_or_init(|| {
            let mut device = std::ptr::null_mut();
            let created = unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut device,
                    DEVICE.0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if created < 0 {
                return Err(format!(
                    "cannot open the {} device for decoding: {}",
                    DEVICE.2,
                    ffmpeg::Error::from(created)
                ));
            }
            Ok(SharedDevice(device))
        })
        .as_ref()
        .map_err(Clone::clone)
}

// the ffmpeg program's choice: the device when the codec decodes there, else software
unsafe extern "C" fn device_or_software_format(
    context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        let mut cursor = formats;
        while *cursor != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            let descriptor = ffi::av_pix_fmt_desc_get(*cursor);
            if (*descriptor).flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 == 0 {
                break;
            }
            let mut config_index = 0;
            let mut matched = false;
            loop {
                let config = ffi::avcodec_get_hw_config((*context).codec, config_index);
                if config.is_null() {
                    break;
                }
                config_index += 1;
                if (*config).methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 == 0 {
                    continue;
                }
                if (*config).pix_fmt == *cursor {
                    matched = (*config).device_type == DEVICE.0;
                    break;
                }
            }
            if matched {
                break;
            }
            cursor = cursor.add(1);
        }
        *cursor
    }
}

fn open_decoder(
    stream: &ffmpeg::format::stream::Stream,
    hardware_decode: bool,
    path: &Path,
) -> Result<(ffmpeg::decoder::Video, bool), String> {
    let cannot = |e: ffmpeg::Error| format!("cannot decode {}: {e}", shown(path));
    let mut context =
        ffmpeg::codec::context::Context::from_parameters(stream.parameters()).map_err(cannot)?;
    let applies_cropping = unsafe {
        let raw = context.as_mut_ptr();
        (*raw).pkt_timebase = stream.time_base().into();
        let applies = (*raw).apply_cropping != 0;
        // the crop is applied to each frame afterwards, unaligned
        (*raw).apply_cropping = 0;
        if hardware_decode {
            let device = shared_device()?;
            (*raw).hw_device_ctx = ffi::av_buffer_ref(device.0);
            (*raw).get_format = Some(device_or_software_format);
            (*raw).extra_hw_frames = EXTRA_DEVICE_FRAMES;
        }
        applies
    };
    let codec = ffmpeg::decoder::find(context.id()).ok_or_else(|| {
        format!(
            "this FFmpeg build has no decoder for the {:?} picture in {}",
            context.id(),
            shown(path)
        )
    })?;
    let mut options = ffmpeg::Dictionary::new();
    options.set("threads", "auto");
    let decoder = context
        .decoder()
        .open_as_with(codec, options)
        .and_then(|opened| opened.video())
        .map_err(cannot)?;
    Ok((decoder, applies_cropping))
}

// ─── the source thread ───────────────────────────────────────────────────────

enum SourceItem {
    Frame(ffmpeg::frame::Video),
    // where the decode ended, in the time base it gives
    End(Option<(i64, ffi::AVRational)>),
}

struct FrameTiming {
    stream_time_base: ffi::AVRational,
    forced_rate: Option<ffi::AVRational>,
    average_rate: ffi::AVRational,
    timestamps_unreliable: bool,
    last_pts: i64,
    last_duration: i64,
    last_time_base: ffi::AVRational,
}

impl FrameTiming {
    fn duration_estimate(&self, frame: &ffi::AVFrame, codec_rate: ffi::AVRational) -> i64 {
        let ts_diff = if frame.pts != ffi::AV_NOPTS_VALUE && self.last_pts != ffi::AV_NOPTS_VALUE {
            frame.pts - self.last_pts
        } else {
            -1
        };
        let duration_unreliable = frame.duration == 1 && ts_diff > 2 * frame.duration;
        if self.forced_rate.is_some()
            || (frame.duration > 0 && !self.timestamps_unreliable && !duration_unreliable)
        {
            return frame.duration;
        }
        let mut codec_duration = 0;
        if codec_rate.den != 0 && codec_rate.num != 0 {
            let fields = i64::from(frame.repeat_pict + 2);
            let field_rate = ffi::AVRational {
                num: codec_rate.num * 2,
                den: codec_rate.den,
            };
            codec_duration = rescale(fields, inverse(field_rate), frame.time_base);
        }
        if codec_duration > 0 && self.timestamps_unreliable {
            return codec_duration;
        }
        if ts_diff > 0 {
            return ts_diff;
        }
        if frame.duration > 0 {
            return frame.duration;
        }
        if codec_duration > 0 {
            return codec_duration;
        }
        let rate = self.forced_rate.unwrap_or(self.average_rate);
        if valid(rate) {
            let duration = rescale(1, inverse(rate), frame.time_base);
            if duration > 0 {
                return duration;
            }
        }
        self.last_duration.max(1)
    }

    // the timestamps the ffmpeg program hands its filters
    fn stamp(&mut self, frame: &mut ffi::AVFrame, codec_rate: ffi::AVRational) {
        frame.time_base = self.stream_time_base;
        frame.pts = frame.best_effort_timestamp;
        if let Some(rate) = self.forced_rate {
            frame.pts = ffi::AV_NOPTS_VALUE;
            frame.duration = 1;
            frame.time_base = inverse(rate);
        }
        if frame.pts == ffi::AV_NOPTS_VALUE {
            frame.pts = if self.last_pts == ffi::AV_NOPTS_VALUE {
                0
            } else {
                self.last_pts + self.last_duration
            };
        }
        self.last_duration = self.duration_estimate(frame, codec_rate);
        self.last_pts = frame.pts;
        self.last_time_base = frame.time_base;
    }

    fn end(&self) -> Option<(i64, ffi::AVRational)> {
        (self.last_pts != ffi::AV_NOPTS_VALUE)
            .then_some((self.last_pts + self.last_duration, self.last_time_base))
    }
}

struct Source {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    applies_cropping: bool,
    stream_index: usize,
    // the ffmpeg program moves the file's start to zero
    timestamp_offset: i64,
    timing: FrameTiming,
    path: String,
    frames_decoded: u64,
    decode_errors: u64,
}

fn is_again(error: &ffmpeg::Error) -> bool {
    matches!(error, ffmpeg::Error::Other { errno } if *errno == libc::EAGAIN)
}

impl Source {
    fn open(request: &DecodeRequest) -> Result<Self, String> {
        let input = open_input(request.input, request.decode_source)?;
        let stream_index = chosen_video_stream(&input)
            .ok_or_else(|| format!("{} holds no video stream", shown(request.input)))?;
        let stream = input
            .stream(stream_index)
            .expect("the chosen stream exists");
        let (decoder, applies_cropping) =
            open_decoder(&stream, request.hardware_decode, request.input)?;
        let stream_time_base: ffi::AVRational = stream.time_base().into();
        let start_time = unsafe { (*input.as_ptr()).start_time };
        let timestamp_offset = if start_time == ffi::AV_NOPTS_VALUE {
            0
        } else {
            -rescale(start_time, ffi::AV_TIME_BASE_Q, stream_time_base)
        };
        let timestamps_unreliable =
            unsafe { (*(*input.as_ptr()).iformat).flags } & ffi::AVFMT_NOTIMESTAMPS != 0;
        let forced_rate = request.read_source_at.map(|rate| ffi::AVRational {
            num: rate.numerator as i32,
            den: rate.denominator as i32,
        });
        let average_rate = stream.avg_frame_rate().into();
        Ok(Self {
            input,
            decoder,
            applies_cropping,
            stream_index,
            timestamp_offset,
            timing: FrameTiming {
                stream_time_base,
                forced_rate,
                average_rate,
                timestamps_unreliable,
                last_pts: ffi::AV_NOPTS_VALUE,
                last_duration: 0,
                last_time_base: ffi::AVRational { num: 1, den: 1 },
            },
            path: request.input.display().to_string(),
            frames_decoded: 0,
            decode_errors: 0,
        })
    }

    fn decode_error(&mut self, error: ffmpeg::Error) -> Result<(), String> {
        if FAIL_ON_FIRST_DECODE_ERROR {
            return Err(format!(
                "cannot decode frame {} of {}: {error}",
                self.frames_decoded, self.path
            ));
        }
        self.decode_errors += 1;
        Ok(())
    }

    fn prepared(
        &mut self,
        mut frame: ffmpeg::frame::Video,
    ) -> Result<ffmpeg::frame::Video, String> {
        unsafe {
            if (*frame.as_ptr()).format == DEVICE.1 as i32 {
                let mut downloaded = ffmpeg::frame::Video::empty();
                let moved =
                    ffi::av_hwframe_transfer_data(downloaded.as_mut_ptr(), frame.as_ptr(), 0);
                if moved < 0 {
                    return Err(format!(
                        "cannot bring frame {} of {} back from the {} device: {}",
                        self.frames_decoded,
                        self.path,
                        DEVICE.2,
                        ffmpeg::Error::from(moved)
                    ));
                }
                ffi::av_frame_copy_props(downloaded.as_mut_ptr(), frame.as_ptr());
                frame = downloaded;
            }
            let raw = &mut *frame.as_mut_ptr();
            let codec_rate = (*self.decoder.as_ptr()).framerate;
            self.timing.stamp(raw, codec_rate);
            if self.applies_cropping {
                let cropped =
                    ffi::av_frame_apply_cropping(raw, ffi::AV_FRAME_CROP_UNALIGNED as i32);
                if cropped < 0 {
                    return Err(format!(
                        "cannot crop frame {} of {}: {}",
                        self.frames_decoded,
                        self.path,
                        ffmpeg::Error::from(cropped)
                    ));
                }
            }
        }
        Ok(frame)
    }

    fn corrupt(frame: &ffmpeg::frame::Video) -> bool {
        let raw = unsafe { &*frame.as_ptr() };
        raw.decode_error_flags != 0 || raw.flags & ffi::AV_FRAME_FLAG_CORRUPT != 0
    }

    // false once the graph thread has stopped taking frames
    fn drain_decoder(&mut self, frames: &SyncSender<SourceItem>) -> Result<bool, String> {
        loop {
            let mut frame = ffmpeg::frame::Video::empty();
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => return Ok(true),
                Err(error) if is_again(&error) => return Ok(true),
                Err(error) => {
                    self.decode_error(error)?;
                    continue;
                }
            }
            if FAIL_ON_FIRST_DECODE_ERROR && Self::corrupt(&frame) {
                return Err(format!(
                    "frame {} of {} decoded corrupt",
                    self.frames_decoded, self.path
                ));
            }
            let frame = self.prepared(frame)?;
            self.frames_decoded += 1;
            if frames.send(SourceItem::Frame(frame)).is_err() {
                return Ok(false);
            }
        }
    }

    fn run(&mut self, frames: &SyncSender<SourceItem>, stop: &AtomicBool) -> Result<(), String> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => break,
                Err(error) if FAIL_ON_FIRST_DECODE_ERROR => {
                    return Err(format!("cannot read {}: {error}", self.path));
                }
                Err(ffmpeg::Error::InvalidData) => continue,
                Err(_) => break,
            }
            if packet.stream() != self.stream_index {
                continue;
            }
            let offset = self.timestamp_offset;
            packet.set_pts(packet.pts().map(|pts| pts + offset));
            packet.set_dts(packet.dts().map(|dts| dts + offset));
            if let Err(error) = self.decoder.send_packet(&packet) {
                self.decode_error(error)?;
            }
            if !self.drain_decoder(frames)? {
                return Ok(());
            }
        }
        let _ = self.decoder.send_eof();
        if !self.drain_decoder(frames)? {
            return Ok(());
        }
        let attempts = self.frames_decoded + self.decode_errors;
        if attempts > 0 && self.decode_errors as f64 / attempts as f64 > MAX_DECODE_ERROR_RATE {
            return Err(format!(
                "{} of {} frames of {} could not be decoded",
                self.decode_errors, attempts, self.path
            ));
        }
        let _ = frames.send(SourceItem::End(self.timing.end()));
        Ok(())
    }
}

// ─── the filter graph ────────────────────────────────────────────────────────

// the parameters a graph is built for, a change of which rebuilds it
#[derive(PartialEq, Clone, Copy)]
struct GraphInput {
    format: i32,
    width: i32,
    height: i32,
    color_space: ffi::AVColorSpace,
    color_range: ffi::AVColorRange,
    display_matrix: Option<[i32; 9]>,
}

impl GraphInput {
    fn of(frame: &ffmpeg::frame::Video) -> Self {
        let raw = unsafe { &*frame.as_ptr() };
        Self {
            format: raw.format,
            width: raw.width,
            height: raw.height,
            color_space: raw.colorspace,
            color_range: raw.color_range,
            display_matrix: display_matrix(raw),
        }
    }
}

fn display_matrix(frame: &ffi::AVFrame) -> Option<[i32; 9]> {
    unsafe {
        let side_data = ffi::av_frame_get_side_data(
            frame,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_DISPLAYMATRIX,
        );
        if side_data.is_null() || (*side_data).size < 36 {
            return None;
        }
        let mut matrix = [0i32; 9];
        std::ptr::copy_nonoverlapping((*side_data).data as *const i32, matrix.as_mut_ptr(), 9);
        Some(matrix)
    }
}

// what the ffmpeg program puts before the caller's chain to turn the picture upright
fn upright_filters(matrix: &[i32; 9]) -> Vec<(&'static str, Option<String>)> {
    let mut theta = -unsafe { ffi::av_display_rotation_get(matrix.as_ptr()) }.round();
    theta -= 360.0 * (theta / 360.0 + 0.9 / 360.0).floor();
    let near = |angle: f64| (theta - angle).abs() < 1.0;
    let transpose = |direction: &str| ("transpose", Some(direction.to_string()));
    let mut filters = Vec::new();
    if near(90.0) {
        filters.push(transpose(if matrix[3] > 0 {
            "cclock_flip"
        } else {
            "clock"
        }));
    } else if near(180.0) {
        if matrix[0] < 0 {
            filters.push(("hflip", None));
        }
        if matrix[4] < 0 {
            filters.push(("vflip", None));
        }
    } else if near(270.0) {
        filters.push(transpose(if matrix[3] < 0 {
            "clock_flip"
        } else {
            "cclock"
        }));
    } else if theta.abs() > 1.0 {
        filters.push(("rotate", Some(format!("{theta:.6}*PI/180"))));
    } else if theta.abs() < 1.0 && matrix[4] < 0 {
        filters.push(("vflip", None));
    }
    filters
}

// each filter name in a chain, read the way libavfilter's parser reads it
fn filter_names(chain: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut characters = chain.chars().peekable();
    loop {
        while let Some(c) = characters.peek() {
            if c.is_whitespace() || *c == ',' || *c == ';' {
                characters.next();
            } else if *c == '[' {
                for c in characters.by_ref() {
                    if c == ']' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if characters.peek().is_none() {
            return names;
        }
        let mut name = String::new();
        while let Some(c) = characters.peek() {
            if matches!(c, '=' | ',' | ';' | '[') || c.is_whitespace() {
                break;
            }
            name.push(*c);
            characters.next();
        }
        names.push(name);
        if characters.peek() == Some(&'=') {
            let mut quoted = false;
            while let Some(c) = characters.peek().copied() {
                if !quoted && matches!(c, ',' | ';' | '[') {
                    break;
                }
                characters.next();
                match c {
                    '\\' => {
                        characters.next();
                    }
                    '\'' => quoted = !quoted,
                    _ => {}
                }
            }
        }
    }
}

fn check_filters_exist(chain: &str, path: &Path) -> Result<(), String> {
    for name in filter_names(chain) {
        let wanted = std::ffi::CString::new(name.clone()).map_err(|e| e.to_string())?;
        if unsafe { ffi::avfilter_get_by_name(wanted.as_ptr()) }.is_null() {
            return Err(format!(
                "the filter `{name}` is not in this FFmpeg build, so `{chain}` cannot run on {}",
                shown(path)
            ));
        }
    }
    Ok(())
}

fn with_library_denoiser(chain: &str) -> String {
    chain
        .split(',')
        .map(|item| {
            if item == crate::picture_processing::FFMPEG_PROGRAM_DENOISE_FILTER {
                LIBRARY_DENOISE_FILTER
            } else {
                item
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

struct Graph {
    graph: ffmpeg::filter::Graph,
    input: GraphInput,
    input_time_base: ffi::AVRational,
    // removed from each frame once the graph has turned it upright
    removes_display_matrix: bool,
}

struct GraphSettings {
    picture_filters: String,
    pixel_format: &'static str,
    detect_picture_findings: bool,
    container_crop: Option<[u32; 4]>,
    forced_rate: Option<ffi::AVRational>,
    guessed_rate: ffi::AVRational,
    path: String,
}

impl GraphSettings {
    // the order filters sit in decides where format negotiation puts conversions
    fn build(&self, frame: &ffmpeg::frame::Video) -> Result<Graph, String> {
        let raw = unsafe { &*frame.as_ptr() };
        let input = GraphInput::of(frame);
        let input_time_base = match self.forced_rate {
            Some(rate) => inverse(rate),
            None => raw.time_base,
        };
        let frame_rate = self.forced_rate.unwrap_or(self.guessed_rate);
        let aspect = if raw.sample_aspect_ratio.den > 0 {
            raw.sample_aspect_ratio
        } else {
            ffi::AVRational { num: 0, den: 1 }
        };
        let source_args = format!(
            "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}:frame_rate={}/{}:colorspace={}:range={}",
            raw.width,
            raw.height,
            raw.format,
            input_time_base.num,
            input_time_base.den,
            aspect.num,
            aspect.den,
            frame_rate.num,
            frame_rate.den,
            raw.colorspace as i32,
            raw.color_range as i32,
        );
        let mut front: Vec<(&str, Option<String>)> = Vec::new();
        if let Some([top, bottom, left, right]) = self.container_crop {
            front.push((
                "crop",
                Some(format!(
                    "w=iw-{left}-{right}:h=ih-{top}-{bottom}:x={left}:y={top}"
                )),
            ));
        }
        if let Some(matrix) = &input.display_matrix {
            front.extend(upright_filters(matrix));
        }
        let caller_graph = if self.detect_picture_findings {
            crate::picture_findings::with_detection_sink(&self.picture_filters, FINDINGS_SINK)
        } else {
            self.picture_filters.clone()
        };

        let failed = |error: i32| {
            format!(
                "the filters `{}` cannot run on {}: {}",
                self.picture_filters,
                self.path,
                ffmpeg::Error::from(error)
            )
        };
        let mut graph = ffmpeg::filter::Graph::new();
        let raw_graph = unsafe { graph.as_mut_ptr() };
        let spec = std::ffi::CString::new(caller_graph).map_err(|e| e.to_string())?;
        let mut open_inputs = std::ptr::null_mut();
        let mut open_outputs = std::ptr::null_mut();
        let parsed = unsafe {
            ffi::avfilter_graph_parse2(
                raw_graph,
                spec.as_ptr(),
                &mut open_inputs,
                &mut open_outputs,
            )
        };
        let linked = (|| {
            if parsed < 0 {
                return Err(failed(parsed));
            }
            unsafe {
                let mut last =
                    create_filter(raw_graph, "buffer", PICTURE_SOURCE, Some(&source_args))
                        .map_err(failed)?;
                for (name, args) in &front {
                    let filter =
                        create_filter(raw_graph, name, name, args.as_deref()).map_err(failed)?;
                    link(last, 0, filter, 0).map_err(failed)?;
                    last = filter;
                }
                if open_inputs.is_null() || !(*open_inputs).next.is_null() {
                    return Err(format!(
                        "the filters `{}` have to take exactly one picture",
                        self.picture_filters
                    ));
                }
                link(
                    last,
                    0,
                    (*open_inputs).filter_ctx,
                    (*open_inputs).pad_idx as u32,
                )
                .map_err(failed)?;

                let mut output = open_outputs;
                while !output.is_null() {
                    let label = (!(*output).name.is_null()).then(|| {
                        std::ffi::CStr::from_ptr((*output).name)
                            .to_string_lossy()
                            .into_owned()
                    });
                    if label.as_deref() == Some(FINDINGS_SINK) {
                        let sink = create_filter(raw_graph, "buffersink", FINDINGS_SINK, None)
                            .map_err(failed)?;
                        link((*output).filter_ctx, (*output).pad_idx as u32, sink, 0)
                            .map_err(failed)?;
                    } else {
                        let sink = create_filter(raw_graph, "buffersink", PICTURE_SINK, None)
                            .map_err(failed)?;
                        // the ffmpeg program's own spelling of its output format filter
                        let format_args = format!("pix_fmts={}:", self.pixel_format);
                        let format =
                            create_filter(raw_graph, "format", "format", Some(&format_args))
                                .map_err(failed)?;
                        link((*output).filter_ctx, (*output).pad_idx as u32, format, 0)
                            .map_err(failed)?;
                        link(format, 0, sink, 0).map_err(failed)?;
                    }
                    output = (*output).next;
                }
                let configured = ffi::avfilter_graph_config(raw_graph, std::ptr::null_mut());
                if configured < 0 {
                    return Err(failed(configured));
                }
            }
            Ok(())
        })();
        unsafe {
            ffi::avfilter_inout_free(&mut open_inputs);
            ffi::avfilter_inout_free(&mut open_outputs);
        }
        linked?;
        Ok(Graph {
            graph,
            input,
            input_time_base,
            removes_display_matrix: input.display_matrix.is_some(),
        })
    }
}

unsafe fn create_filter(
    graph: *mut ffi::AVFilterGraph,
    filter: &str,
    name: &str,
    args: Option<&str>,
) -> Result<*mut ffi::AVFilterContext, i32> {
    let filter = std::ffi::CString::new(filter).expect("filter names hold no null byte");
    let name = std::ffi::CString::new(name).expect("filter names hold no null byte");
    let args = args.map(|args| std::ffi::CString::new(args).expect("arguments hold no null byte"));
    let mut context = std::ptr::null_mut();
    let created = unsafe {
        ffi::avfilter_graph_create_filter(
            &mut context,
            ffi::avfilter_get_by_name(filter.as_ptr()),
            name.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |args| args.as_ptr()),
            std::ptr::null_mut(),
            graph,
        )
    };
    if created < 0 {
        return Err(created);
    }
    Ok(context)
}

unsafe fn link(
    source: *mut ffi::AVFilterContext,
    source_pad: u32,
    destination: *mut ffi::AVFilterContext,
    destination_pad: u32,
) -> Result<(), i32> {
    let linked = unsafe { ffi::avfilter_link(source, source_pad, destination, destination_pad) };
    if linked < 0 {
        return Err(linked);
    }
    Ok(())
}

// the ffmpeg program's constant rate step between the filters and rawvideo
struct ConstantRate {
    // the ffmpeg program's VSCFR, for an input file of one stream
    skips_initial_duplicates: bool,
    time_base: Option<ffi::AVRational>,
    next_pts: i64,
    frame_number: i64,
    duplicates_history: [i64; 3],
    last_dropped: bool,
    last_frame: Option<ffmpeg::frame::Video>,
    got_frame: bool,
}

fn median3(a: i64, b: i64, c: i64) -> i64 {
    let mut values = [a, b, c];
    values.sort_unstable();
    values[1]
}

impl ConstantRate {
    fn new(skips_initial_duplicates: bool) -> Self {
        Self {
            skips_initial_duplicates,
            time_base: None,
            next_pts: 0,
            frame_number: 0,
            duplicates_history: [0; 3],
            last_dropped: false,
            last_frame: None,
            got_frame: false,
        }
    }

    // the frame's pts in the output time base, finer than one tick
    fn exact_pts(frame: &mut ffi::AVFrame, output: ffi::AVRational) -> f64 {
        if frame.pts == ffi::AV_NOPTS_VALUE {
            return ffi::AV_NOPTS_VALUE as f64;
        }
        let log2_den = 31 - (output.den as u32).leading_zeros() as i32;
        let extra_bits = (29 - log2_den).clamp(0, 16);
        let finer = ffi::AVRational {
            num: output.num,
            den: output.den << extra_bits,
        };
        let mut exact = rescale(frame.pts, frame.time_base, finer) as f64;
        exact /= f64::from(1 << extra_bits);
        if exact != exact.round_ties_even() {
            exact += exact.signum() / f64::from(1 << 17);
        }
        frame.pts = rescale(frame.pts, frame.time_base, output);
        frame.time_base = output;
        exact
    }

    // how many copies of the previous frame and of this one the output gets
    fn counts(&mut self, frame: Option<&mut ffi::AVFrame>, output: ffi::AVRational) -> (i64, i64) {
        let Some(frame) = frame else {
            let history = self.duplicates_history;
            let copies = median3(history[0], history[1], history[2]);
            self.duplicates_history = [copies, history[0], history[1]];
            self.last_dropped = false;
            return (copies, copies);
        };
        let mut duration = frame.duration as f64 * q2d(frame.time_base) / q2d(output);
        let mut sync_pts = Self::exact_pts(frame, output);
        let mut delta0 = sync_pts - self.next_pts as f64;
        let mut delta = delta0 + duration;
        let mut previous_copies = 0;
        let mut copies: i64 = 1;
        if delta0 < 0.0 && delta > 0.0 {
            sync_pts = self.next_pts as f64;
            duration += delta0;
            delta0 = 0.0;
        }
        if self.skips_initial_duplicates && self.frame_number == 0 && delta0 >= 0.5 {
            delta = duration;
            delta0 = 0.0;
            self.next_pts = sync_pts.round_ties_even() as i64;
        }
        if delta < -1.1 {
            copies = 0;
        } else if delta > 1.1 {
            copies = (delta as f32).round_ties_even() as i64;
            if delta0 > 1.1 {
                previous_copies = ((delta0 - 0.6) as f32).round_ties_even() as i64;
            }
        }
        frame.duration = 1;
        let history = self.duplicates_history;
        self.duplicates_history = [previous_copies, history[0], history[1]];
        let repeats_previous = i64::from(previous_copies > 0 && self.last_dropped);
        if copies > repeats_previous + i64::from(copies > previous_copies)
            && copies as f64 > DUPLICATION_LIMIT_FRAMES
        {
            return (previous_copies, 0);
        }
        self.last_dropped = copies == previous_copies;
        (previous_copies, copies)
    }

    // the frames rawvideo would write for `frame`, None at the end of the stream
    fn output(
        &mut self,
        mut frame: Option<ffmpeg::frame::Video>,
        sink_rate: ffi::AVRational,
        emit: &mut dyn FnMut(ffmpeg::frame::Video) -> bool,
    ) -> bool {
        if frame.is_none() && !self.got_frame {
            return true;
        }
        let output = *self.time_base.get_or_insert_with(|| {
            let rate = if valid(sink_rate) {
                sink_rate
            } else {
                FALLBACK_FRAME_RATE
            };
            inverse(rate)
        });
        let raw = frame
            .as_mut()
            .map(|frame| unsafe { &mut *frame.as_mut_ptr() });
        let (previous_copies, copies) = self.counts(raw, output);
        for copy in 0..copies {
            let source = match (&self.last_frame, &frame) {
                (Some(previous), _) if copy < previous_copies => previous,
                (_, Some(current)) => current,
                (_, None) => break,
            };
            let mut out = reference(source);
            unsafe { (*out.as_mut_ptr()).pts = self.next_pts };
            if !emit(out) {
                return false;
            }
            self.frame_number += 1;
            self.next_pts += 1;
            self.got_frame = true;
        }
        if let Some(frame) = frame {
            self.last_frame = Some(frame);
        }
        true
    }
}

// the detections left on the frames, as the lines the ffmpeg program logs
struct Detections {
    lines: Vec<String>,
    black_start: Option<(i64, String)>,
    last_pts: Option<i64>,
    time_base: Option<ffi::AVRational>,
}

// the blackdetect threshold every pass uses
const BLACK_MINIMUM_DURATION_SECONDS: f64 = 2.0;

impl Detections {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            black_start: None,
            last_pts: None,
            time_base: None,
        }
    }

    fn reported(&self, start: i64, end: i64) -> bool {
        let time_base = self.time_base.expect("set with the first frame");
        let minimum = (BLACK_MINIMUM_DURATION_SECONDS / q2d(time_base)) as i64;
        end - start >= minimum
    }

    fn take(&mut self, frame: &ffmpeg::frame::Video, time_base: ffi::AVRational) {
        self.time_base.get_or_insert(time_base);
        let pts = frame.pts().unwrap_or(0);
        self.last_pts = Some(pts);
        let metadata = frame.metadata();
        if let Some(start) = metadata.get(BLACK_START_KEY) {
            self.black_start = Some((pts, start.to_string()));
        }
        if let Some(end) = metadata.get(BLACK_END_KEY)
            && let Some((start_pts, start)) = self.black_start.take()
            && self.reported(start_pts, pts)
        {
            self.lines
                .push(format!("black_start:{start} black_end:{end}"));
        }
        if let Some(start) = metadata.get(FREEZE_START_KEY) {
            self.lines.push(format!("{FREEZE_START_KEY}: {start}"));
        }
        if let Some(end) = metadata.get(FREEZE_END_KEY) {
            self.lines.push(format!("{FREEZE_END_KEY}: {end}"));
        }
    }

    // blackdetect reports a run still open at the end with the last frame's time
    fn finish(mut self) -> Vec<String> {
        if let (Some((start_pts, start)), Some(end_pts), Some(time_base)) =
            (self.black_start.take(), self.last_pts, self.time_base)
            && self.reported(start_pts, end_pts)
        {
            let end = time_string(end_pts, time_base);
            self.lines
                .push(format!("black_start:{start} black_end:{end}"));
        }
        self.lines
    }
}

// av_ts2timestr's spelling
fn time_string(ts: i64, time_base: ffi::AVRational) -> String {
    let value = q2d(time_base) * ts as f64;
    let magnitude = if value == 0.0 {
        f64::NEG_INFINITY
    } else {
        value.abs().log10().floor()
    };
    let precision = if magnitude.is_finite() && magnitude < 0.0 {
        (-magnitude + 5.0) as usize
    } else {
        6
    };
    let mut text = format!("{value:.precision$}");
    while text.len() > 1 && text.ends_with('0') {
        text.pop();
    }
    while text.len() > 1 && !text.ends_with(|c: char| c.is_ascii_digit()) {
        text.pop();
    }
    text
}

struct GraphStage {
    settings: GraphSettings,
    graph: Option<Graph>,
    constant_rate: ConstantRate,
    detections: Detections,
    frames_sent: u64,
    frame_limit: u64,
}

impl GraphStage {
    // false once nothing downstream wants more frames
    fn drain(
        &mut self,
        frames: &SyncSender<ffmpeg::frame::Video>,
        at_end: bool,
    ) -> Result<bool, String> {
        let wanted = self.drain_pictures(frames, at_end);
        // pulling pictures runs the detection branch too
        self.drain_findings();
        wanted
    }

    fn drain_findings(&mut self) {
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        if !self.settings.detect_picture_findings {
            return;
        }
        let mut findings_sink = graph
            .graph
            .get(FINDINGS_SINK)
            .expect("added with the graph");
        let time_base = unsafe { ffi::av_buffersink_get_time_base(findings_sink.as_ptr()) };
        loop {
            let mut detected = ffmpeg::frame::Video::empty();
            if findings_sink.sink().frame(&mut detected).is_err() {
                return;
            }
            self.detections.take(&detected, time_base);
        }
    }

    fn drain_pictures(
        &mut self,
        frames: &SyncSender<ffmpeg::frame::Video>,
        at_end: bool,
    ) -> Result<bool, String> {
        let Some(graph) = self.graph.as_mut() else {
            return Ok(true);
        };
        let mut picture_sink = graph.graph.get(PICTURE_SINK).expect("added with the graph");
        let sink_rate = unsafe { ffi::av_buffersink_get_frame_rate(picture_sink.as_ptr()) };
        let sink_time_base = unsafe { ffi::av_buffersink_get_time_base(picture_sink.as_ptr()) };
        let frame_limit = self.frame_limit;
        let frames_sent = &mut self.frames_sent;
        let mut emit = |frame: ffmpeg::frame::Video| {
            if *frames_sent >= frame_limit || frames.send(frame).is_err() {
                return false;
            }
            *frames_sent += 1;
            *frames_sent < frame_limit
        };
        loop {
            let mut picture = ffmpeg::frame::Video::empty();
            match picture_sink.sink().frame(&mut picture) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) if at_end => {
                    return Ok(self.constant_rate.output(None, sink_rate, &mut emit));
                }
                Err(ffmpeg::Error::Eof) => return Ok(true),
                Err(error) if is_again(&error) => return Ok(true),
                Err(error) => {
                    return Err(format!(
                        "the filters `{}` failed on {}: {error}",
                        self.settings.picture_filters, self.settings.path
                    ));
                }
            }
            unsafe {
                let raw = &mut *picture.as_mut_ptr();
                raw.time_base = sink_time_base;
                if raw.duration == 0 && valid(sink_rate) {
                    raw.duration = rescale(1, inverse(sink_rate), sink_time_base);
                }
            }
            if !self
                .constant_rate
                .output(Some(picture), sink_rate, &mut emit)
            {
                return Ok(false);
            }
        }
    }

    fn add(
        &mut self,
        mut frame: ffmpeg::frame::Video,
        frames: &SyncSender<ffmpeg::frame::Video>,
    ) -> Result<bool, String> {
        let input = GraphInput::of(&frame);
        if self
            .graph
            .as_ref()
            .is_some_and(|graph| graph.input != input)
        {
            // the ffmpeg program takes what the old graph has ready and drops the rest
            if !self.drain(frames, false)? {
                return Ok(false);
            }
            self.graph = None;
        }
        if self.graph.is_none() {
            self.graph = Some(self.settings.build(&frame)?);
        }
        let graph = self.graph.as_mut().expect("built above");
        unsafe {
            let raw = &mut *frame.as_mut_ptr();
            raw.pts = rescale(raw.pts, raw.time_base, graph.input_time_base);
            raw.duration = rescale(raw.duration, raw.time_base, graph.input_time_base);
            raw.time_base = graph.input_time_base;
            if graph.removes_display_matrix {
                ffi::av_frame_remove_side_data(
                    raw,
                    ffi::AVFrameSideDataType::AV_FRAME_DATA_DISPLAYMATRIX,
                );
            }
        }
        let mut source = graph
            .graph
            .get(PICTURE_SOURCE)
            .expect("added with the graph");
        source.source().add(&frame).map_err(|e| {
            format!(
                "the filters `{}` failed on {}: {e}",
                self.settings.picture_filters, self.settings.path
            )
        })?;
        self.drain(frames, false)
    }

    fn end(
        &mut self,
        end: Option<(i64, ffi::AVRational)>,
        frames: &SyncSender<ffmpeg::frame::Video>,
    ) -> Result<(), String> {
        let Some(graph) = self.graph.as_mut() else {
            return Ok(());
        };
        let pts = match end {
            Some((pts, time_base)) => unsafe {
                ffi::av_rescale_q_rnd(
                    pts,
                    time_base,
                    graph.input_time_base,
                    ffi::AVRounding::AV_ROUND_NEAR_INF,
                )
            },
            None => ffi::AV_NOPTS_VALUE,
        };
        let mut source = graph
            .graph
            .get(PICTURE_SOURCE)
            .expect("added with the graph");
        let closed = unsafe {
            ffi::av_buffersrc_close(source.as_mut_ptr(), pts, ffi::AV_BUFFERSRC_FLAG_PUSH as u32)
        };
        if closed < 0 {
            return Err(format!(
                "the filters `{}` failed on {}: {}",
                self.settings.picture_filters,
                self.settings.path,
                ffmpeg::Error::from(closed)
            ));
        }
        self.drain(frames, true).map(|_| ())
    }

    fn run(
        &mut self,
        decoded: Receiver<SourceItem>,
        frames: &SyncSender<ffmpeg::frame::Video>,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        for item in decoded {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match item {
                SourceItem::Frame(frame) => {
                    if !self.add(frame, frames)? {
                        return Ok(());
                    }
                }
                SourceItem::End(end) => return self.end(end, frames),
            }
        }
        Ok(())
    }
}

// ─── the public reader ───────────────────────────────────────────────────────

impl FrameDecoder {
    pub(crate) fn start(request: &DecodeRequest) -> Result<Self, String> {
        ready()?;
        let picture_filters = with_library_denoiser(request.picture_filters);
        check_filters_exist(&picture_filters, request.input)?;
        let mut source = Source::open(request)?;
        let (container_crop, guessed_rate, single_stream) = {
            let stream = source
                .input
                .stream(source.stream_index)
                .expect("the chosen stream exists");
            let guessed_rate = unsafe {
                ffi::av_guess_frame_rate(
                    source.input.as_ptr() as *mut _,
                    stream.as_ptr() as *mut _,
                    std::ptr::null_mut(),
                )
            };
            (
                container_crop(stream_parameters(&stream)),
                guessed_rate,
                source.input.nb_streams() == 1,
            )
        };
        let mut stage = GraphStage {
            settings: GraphSettings {
                picture_filters,
                pixel_format: request.input_format.ffmpeg_pixel_format(),
                detect_picture_findings: request.detect_picture_findings,
                container_crop,
                forced_rate: source.timing.forced_rate,
                guessed_rate,
                path: request.input.display().to_string(),
            },
            graph: None,
            constant_rate: ConstantRate::new(single_stream),
            detections: Detections::new(),
            frames_sent: 0,
            frame_limit: request.frame_limit.unwrap_or(u64::MAX),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let (decoded_tx, decoded_rx) = sync_channel(DECODED_FRAMES_QUEUED);
        let (filtered_tx, filtered_rx) = sync_channel(FILTERED_FRAMES_QUEUED);

        let source_thread = {
            let stop = stop.clone();
            let outcome = outcome.clone();
            std::thread::Builder::new()
                .name("postkit decode".to_string())
                .spawn(move || {
                    if let Err(e) = source.run(&decoded_tx, &stop) {
                        outcome.lock().unwrap().fail(e);
                    }
                })
                .map_err(|e| format!("cannot start the decode thread: {e}"))?
        };
        let graph_thread = {
            let stop = stop.clone();
            let outcome = outcome.clone();
            std::thread::Builder::new()
                .name("postkit filters".to_string())
                .spawn(move || {
                    let run = stage.run(decoded_rx, &filtered_tx, &stop);
                    let mut outcome = outcome.lock().unwrap();
                    if let Err(e) = run {
                        outcome.fail(e);
                    }
                    outcome.detection_lines =
                        std::mem::replace(&mut stage.detections, Detections::new()).finish();
                })
                .map_err(|e| format!("cannot start the filter thread: {e}"))?
        };

        Ok(Self {
            frames: Some(filtered_rx),
            stop,
            outcome,
            threads: vec![source_thread, graph_thread],
            input: request.input.display().to_string(),
            width: request.width,
            height: request.height,
            frame_bytes: request
                .input_format
                .frame_bytes(request.width, request.height),
            frames_read: 0,
        })
    }

    fn next(&mut self) -> Option<ffmpeg::frame::Video> {
        let frame = self.frames.as_ref()?.recv().ok()?;
        self.frames_read += 1;
        Some(frame)
    }

    // copies the next frame into `buffer` in the encoder input format's layout
    pub(crate) fn read_into(&mut self, buffer: &mut [u8]) -> NextFrame {
        let Some(frame) = self.next() else {
            return NextFrame::Ended;
        };
        let raw = unsafe { &*frame.as_ptr() };
        if (raw.width, raw.height) != (self.width as i32, self.height as i32) {
            self.outcome.lock().unwrap().fail(format!(
                "frame {} of {} decoded at {}x{}, where the encode expects {}x{}",
                self.frames_read - 1,
                self.input,
                raw.width,
                raw.height,
                self.width,
                self.height
            ));
            self.stop_threads();
            return NextFrame::Ended;
        }
        let format: ffi::AVPixelFormat = unsafe { std::mem::transmute(raw.format) };
        assert_eq!(
            buffer.len(),
            self.frame_bytes,
            "the pool sizes each buffer for one frame"
        );
        let copied = unsafe {
            ffi::av_image_copy_to_buffer(
                buffer.as_mut_ptr(),
                self.frame_bytes as i32,
                raw.data.as_ptr() as *const *const u8,
                raw.linesize.as_ptr(),
                format,
                raw.width,
                raw.height,
                1,
            )
        };
        if copied != self.frame_bytes as i32 {
            self.outcome.lock().unwrap().fail(format!(
                "frame {} of {} holds {copied} bytes, where the encode expects {}",
                self.frames_read - 1,
                self.input,
                self.frame_bytes
            ));
            self.stop_threads();
            return NextFrame::Ended;
        }
        NextFrame::Copied
    }

    // decodes and filters `frames` frames without copying them
    pub(crate) fn skip(&mut self, frames: u64) -> Result<(), String> {
        for skipped in 0..frames {
            if self.next().is_none() {
                return Err(self
                    .outcome
                    .lock()
                    .unwrap()
                    .failure
                    .clone()
                    .unwrap_or_else(|| {
                        format!(
                            "{} ended after {skipped} frames, before the {frames} to skip",
                            self.input
                        )
                    }));
            }
        }
        Ok(())
    }

    fn stop_threads(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    pub(crate) fn finish(mut self, fps: f64, frame_count: u64) -> FinishedDecode {
        self.stop_threads();
        let mut outcome = self.outcome.lock().unwrap();
        FinishedDecode {
            findings: crate::picture_findings::parse_ffmpeg_stderr(
                &outcome.detection_lines,
                fps,
                frame_count,
            ),
            failure: outcome.failure.take(),
        }
    }
}

impl Drop for FrameDecoder {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // POSTKIT_DECODE_MEASURE_CLIP names the clip, _FILTERS and _SIZE its chain and output raster
    #[test]
    #[ignore = "a measurement, not a check"]
    fn the_reader_decodes_this_many_frames_a_second() {
        let clip = std::env::var("POSTKIT_DECODE_MEASURE_CLIP").expect("a clip to measure");
        let filters = std::env::var("POSTKIT_DECODE_MEASURE_FILTERS").unwrap_or("fps=24".into());
        let probed = probe(Path::new(&clip), DecodeSource::Video).unwrap();
        let (width, height) = match std::env::var("POSTKIT_DECODE_MEASURE_SIZE") {
            Ok(size) => {
                let (width, height) = size.split_once('x').expect("WxH");
                (width.parse().unwrap(), height.parse().unwrap())
            }
            Err(_) => (probed.width, probed.height),
        };
        let pixel_format =
            crate::encode::PlanarYuvPixelFormat::from_ffmpeg_name(&probed.pixel_format.pix_fmt)
                .expect("a planar YUV clip");
        let input_format = EncoderInputFormat::PlanarYuv(crate::encode::YuvFrameFormat {
            pixel_format,
            matrix: crate::encode::YuvMatrix::Bt709,
            full_range: false,
        });
        let started = std::time::Instant::now();
        let mut decoder = FrameDecoder::start(&DecodeRequest {
            input: Path::new(&clip),
            decode_source: DecodeSource::Video,
            read_source_at: None,
            hardware_decode: false,
            picture_filters: &filters,
            input_format,
            width,
            height,
            frame_limit: None,
            detect_picture_findings: false,
        })
        .unwrap();
        let mut buffer = vec![0u8; input_format.frame_bytes(width, height)];
        let mut frames = 0u64;
        while let NextFrame::Copied = decoder.read_into(&mut buffer) {
            frames += 1;
        }
        let seconds = started.elapsed().as_secs_f64();
        let finished = decoder.finish(24.0, frames);
        assert!(finished.failure.is_none(), "{:?}", finished.failure);
        println!(
            "{frames} frames in {seconds:.2} s, {:.1} fps",
            frames as f64 / seconds
        );
    }

    #[test]
    fn filter_names_skip_labels_quotes_and_escapes() {
        assert_eq!(
            filter_names(
                "fps=24,lut3d=\\'/luts/a\\,b.cube\\',split=2[picture][detect];[detect]blackdetect=d=2,nullsink;[picture]null"
            ),
            vec!["fps", "lut3d", "split", "blackdetect", "nullsink", "null"]
        );
    }

    #[test]
    fn the_loaded_libraries_match_the_headers() {
        ready().unwrap();
    }

    #[test]
    fn a_filter_missing_from_the_build_is_named() {
        ready().unwrap();
        let error = check_filters_exist("fps=24,eq=contrast=2", Path::new("clip.mov")).unwrap_err();
        assert!(error.contains("`eq`"), "{error}");
    }

    #[test]
    fn the_library_chain_swaps_the_gpl_denoiser() {
        assert_eq!(
            with_library_denoiser("yadif,fps=24,hqdn3d,crop=8:8:0:0"),
            "yadif,fps=24,atadenoise,crop=8:8:0:0"
        );
    }

    #[test]
    fn times_are_spelled_the_way_ffmpeg_logs_them() {
        let tick = ffi::AVRational { num: 1, den: 24 };
        assert_eq!(time_string(72, tick), "3");
        assert_eq!(time_string(143, tick), "5.958333");
        assert_eq!(time_string(0, tick), "0");
        assert_eq!(
            time_string(1, ffi::AVRational { num: 1, den: 1000 }),
            "0.001"
        );
    }
}

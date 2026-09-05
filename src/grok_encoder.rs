//! In-process JPEG 2000 encoding via Grok FFI with a bounded work queue.
//!
//! Architecture (mirrors dcpomatic):
//!   [Producer] → bounded queue → [N encoder threads] → [Writer thread] → disk
//!
//! - Producer pushes raw XYZ frames into a bounded queue (backpressure when full)
//! - N encoder threads pop frames, compress via `grk_compress()` in-process
//! - Encoded data is sent to a writer channel for async disk I/O
//!
//! Enable with the `grok-ffi` cargo feature.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Byte order of the 16-bit samples in a packed rgb48 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOrder {
    Big,
    Little,
}

/// A raw frame ready for JPEG 2000 compression.
/// Can be either planar (from TIFF loader) or packed interleaved (from ffmpeg pipe).
pub enum RawFrame {
    /// Planar component buffers: [R, G, B], each with width*height i32 values
    Planar {
        components: [Vec<i32>; 3],
        width: u32,
        height: u32,
        precision: u8,
        index: u64,
    },
    /// Packed interleaved rgb48 bytes, 6 bytes per pixel, in `order`. Only
    /// grok's accelerator plugin takes the little-endian layout, as one
    /// interleaved buffer: everything else deinterleaves the frame on the host.
    Packed {
        data: Vec<u8>,
        order: SampleOrder,
        width: u32,
        height: u32,
        precision: u8,
        index: u64,
    },
    /// The source's own planar YUV exactly as ffmpeg's rawvideo muxer wrote it:
    /// the whole luma plane, then blue chroma, then red chroma, laid out by
    /// [`crate::encode::PlanarYuvPixelFormat::plane_layout`]. Only grok's
    /// accelerator plugin takes these: it upsamples the chroma and converts the
    /// colour on the device.
    PlanarYuv {
        data: Vec<u8>,
        format: crate::encode::YuvFrameFormat,
        width: u32,
        height: u32,
        index: u64,
    },
}

/// The depth grok's plugin writes samples at when it converts a planar YUV
/// frame, whether it ends in X'Y'Z' or in RGB. It is the depth both the cinema
/// profiles and IMF App 2E carry, so nothing is shifted after it.
pub const PLANAR_YUV_SAMPLE_PRECISION: u8 = 12;

impl RawFrame {
    pub fn index(&self) -> u64 {
        match self {
            RawFrame::Planar { index, .. }
            | RawFrame::Packed { index, .. }
            | RawFrame::PlanarYuv { index, .. } => *index,
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            RawFrame::Planar { width, .. }
            | RawFrame::Packed { width, .. }
            | RawFrame::PlanarYuv { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            RawFrame::Planar { height, .. }
            | RawFrame::Packed { height, .. }
            | RawFrame::PlanarYuv { height, .. } => *height,
        }
    }

    /// The depth of the samples the compressor sees, which for a planar YUV
    /// frame is what the plugin's conversion writes rather than what the pipe
    /// carried.
    pub fn precision(&self) -> u8 {
        match self {
            RawFrame::Planar { precision, .. } | RawFrame::Packed { precision, .. } => *precision,
            RawFrame::PlanarYuv { .. } => PLANAR_YUV_SAMPLE_PRECISION,
        }
    }
}

/// Encoded J2K frame output.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub index: u64,
}

/// Compression parameters for DCI JPEG 2000.
#[derive(Debug, Clone)]
pub struct CompressParams {
    /// Compression ratio (e.g. 10.0 for 10:1), ignored under `target_codestream_bytes`.
    pub compression_ratio: f64,
    pub target_codestream_bytes: Option<u64>,
    /// A PSNR target in dB that grok allocates layers by instead of the
    /// compression ratio. `codestream_byte_cap` still holds: grok is given it
    /// as `max_cs_size` and prefers it over the quality target.
    pub quality_psnr: Option<f64>,
    /// Per-codestream byte cap, the same one the writer checks, given to grok as `max_cs_size`.
    pub codestream_byte_cap: Option<u64>,
    /// Number of decomposition levels (default 6 for 2K)
    pub num_resolutions: u8,
    /// Codeblock size (DCI requires 32×32)
    pub codeblock_size: u32,
    /// Progression order: CPRL for DCI
    pub progression: ProgressionOrder,
    /// Number of quality layers
    pub num_layers: u16,
    /// RSIZ profile. A plain cinema profile (0x0003 or 0x0004) is written as
    /// 2K or 4K by each frame's raster, see [`crate::j2k::rsiz_for_raster`].
    pub profile: u16,
    /// Guard bits
    pub num_guard_bits: u8,
    /// The exact rate the frames are at, 24000/1001 and not 24, so a detection
    /// timestamp converts to the frame number the CPL will carry
    pub edit_rate: crate::encode::FrameRate,
    /// Enable irreversible (lossy) wavelet
    pub irreversible: bool,
    /// Enable MCT (multi-component transform)
    pub mct: bool,
    /// Apply XYZ colour transform (Rec.709 RGB → DCI X'Y'Z')
    pub apply_xyz_transform: bool,
    /// What the encoder threads do to each frame before compressing it.
    pub source_preparation: SourcePreparation,
    /// Threads per codec instance (set internally by pipeline)
    pub threads_per_codec: u32,
}

/// The work each decoded frame gets on the encoder threads before it reaches
/// the compressor.
///
/// Both steps go through [`SourcePreparation::apply`], which fixes their order:
/// subtitles are authored in display RGB, so the burn lands before any colour
/// conversion, this struct's own or the compressor's later one. There is no
/// knob for the other order.
#[derive(Debug, Clone, Default)]
pub struct SourcePreparation {
    /// Subtitles composited into the picture, in display RGB. Needs a packed
    /// 16-bit frame. The caller keeps it off for a source that is already
    /// X'Y'Z', where display-RGB text would land in the wrong space.
    pub subtitle_burn: Option<Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// Colour transform postkit runs over each frame, for a source space the
    /// compressor's own transform does not model (P3, Rec.2020), or for a
    /// source that has to reach Rec.709 RGB rather than X'Y'Z'. Setting it
    /// together with `apply_xyz_transform` converts the frame twice and is
    /// refused.
    pub colour_transform: Option<Arc<crate::colour::FrameColourTransform>>,
}

impl SourcePreparation {
    pub fn is_empty(&self) -> bool {
        self.subtitle_burn.is_none() && self.colour_transform.is_none()
    }

    /// Burn subtitles in, then convert the colour. Both steps need a packed
    /// 16-bit rgb48 frame, in either byte order.
    fn apply(&self, frame: &mut RawFrame, compressor_transform: bool) -> Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        if compressor_transform && self.colour_transform.is_some() {
            return Err(
                "the compressor's X'Y'Z' transform and a source transform are both set: \
                 the frame would be converted twice"
                    .to_string(),
            );
        }
        let index = frame.index();
        let (data, order, width, height) = match frame {
            RawFrame::Packed {
                data,
                order,
                width,
                height,
                precision: 16,
                ..
            } => (data, *order, *width, *height),
            _ => {
                return Err(
                    "a subtitle burn or a source colour transform needs a packed 16-bit \
                     RGB frame"
                        .to_string(),
                );
            }
        };
        if let Some(burn) = &self.subtitle_burn {
            burn.burn_rgb48(data, width, height, order, index)
                .map_err(|e| e.to_string())?;
        }
        if let Some(transform) = &self.colour_transform {
            transform.frame_rgb48_inplace(data, order);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProgressionOrder {
    Lrcp,
    Rlcp,
    Rpcl,
    Pcrl,
    Cprl,
}

impl CompressParams {
    /// grok sizes the per-frame byte budget from a whole rate, and DCI has only
    /// 24 and 48, so a 23.976 stream is a 24 fps one to the compressor.
    #[cfg(feature = "grok-ffi")]
    fn grok_frame_rate(&self) -> u16 {
        self.edit_rate.as_f64().round() as u16
    }
}

impl Default for CompressParams {
    fn default() -> Self {
        Self {
            compression_ratio: 10.0,
            target_codestream_bytes: None,
            quality_psnr: None,
            codestream_byte_cap: None,
            num_resolutions: 6,
            codeblock_size: 32,
            progression: ProgressionOrder::Cprl,
            num_layers: 1,
            profile: 0x0003, // GRK_PROFILE_CINEMA_2K
            num_guard_bits: 1,
            edit_rate: crate::encode::FrameRate::whole(24),
            irreversible: true,
            mct: true,
            apply_xyz_transform: false,
            source_preparation: SourcePreparation::default(),
            threads_per_codec: 1,
        }
    }
}

/// The stretches of work an encode splits into, timed apart so a slow encode
/// can be blamed on the right one.
#[derive(Debug, Clone, Copy)]
pub enum EncodePhase {
    /// Blocked reading the next decoded frame from ffmpeg.
    DecoderWait,
    /// Subtitle burn and source colour transform on the encoder threads.
    Preparation,
    /// The grok compression call.
    Jpeg2000,
    /// Writing the codestream to disk.
    CodestreamWrite,
}

const PHASE_COUNT: usize = 4;
const NANOS_PER_SECOND: f64 = 1_000_000_000.0;

/// Cumulative time spent in each phase, in nanoseconds.
///
/// Preparation and JPEG 2000 run on every encoder thread at once and are summed
/// over them, so they can add up to more than the wall clock.
#[derive(Debug, Default)]
pub struct PhaseClocks {
    nanos: [AtomicU64; PHASE_COUNT],
}

impl PhaseClocks {
    pub fn add(&self, phase: EncodePhase, elapsed: std::time::Duration) {
        self.nanos[phase as usize].fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn seconds(&self, phase: EncodePhase) -> f64 {
        self.nanos[phase as usize].load(Ordering::Relaxed) as f64 / NANOS_PER_SECOND
    }
}

/// Progress information from the encoder pipeline.
#[derive(Debug, Clone)]
pub struct EncodeProgress {
    pub frames_encoded: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
    /// Time blocked on the decoder's pipe.
    pub decode_wait_secs: f64,
    /// Time burning subtitles and converting colour, summed over the encoder
    /// threads, so it can exceed `elapsed_secs`.
    pub prepare_secs: f64,
    /// Time inside grok, summed over the encoder threads, so it can exceed
    /// `elapsed_secs`.
    pub encode_secs: f64,
    /// Time writing codestreams to disk.
    pub write_secs: f64,
}

impl EncodeProgress {
    fn new(
        frames_encoded: u64,
        total_frames: u64,
        elapsed_secs: f64,
        phase_clocks: &PhaseClocks,
    ) -> Self {
        Self {
            frames_encoded,
            total_frames,
            fps: if elapsed_secs > 0.0 {
                frames_encoded as f64 / elapsed_secs
            } else {
                0.0
            },
            elapsed_secs,
            decode_wait_secs: phase_clocks.seconds(EncodePhase::DecoderWait),
            prepare_secs: phase_clocks.seconds(EncodePhase::Preparation),
            encode_secs: phase_clocks.seconds(EncodePhase::Jpeg2000),
            write_secs: phase_clocks.seconds(EncodePhase::CodestreamWrite),
        }
    }
}

/// Result from the encoding pipeline.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    pub success: bool,
    pub error: String,
    pub frames_encoded: u64,
    pub output_dir: PathBuf,
    /// Black and frozen runs blackdetect and freezedetect saw during the
    /// decode. Only a pipeline that runs ffmpeg itself fills this in: one fed
    /// frames by its caller reports nothing, because the caller's own decode is
    /// where the filters run.
    pub picture_findings: crate::picture_findings::PictureFindings,
}

/// Bounded work queue with condition-variable backpressure (mirrors dcpomatic's design).
///
/// FIFO. A full queue refilled as fast as it is drained would never hand out its
/// oldest item under LIFO, and the overlapped wrap holds every later frame while
/// it waits for that one.
pub struct BoundedQueue<T> {
    items: Mutex<std::collections::VecDeque<T>>,
    not_full: Condvar,
    not_empty: Condvar,
    capacity: usize,
    closed: AtomicBool,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(std::collections::VecDeque::with_capacity(capacity)),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
            capacity,
            closed: AtomicBool::new(false),
        }
    }

    pub fn push(&self, item: T) -> bool {
        let mut queue = self.items.lock().unwrap();
        while queue.len() >= self.capacity {
            if self.closed.load(Ordering::Relaxed) {
                return false;
            }
            queue = self.not_full.wait(queue).unwrap();
        }
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        queue.push_back(item);
        self.not_empty.notify_one();
        true
    }

    pub fn pop(&self) -> Option<T> {
        let mut queue = self.items.lock().unwrap();
        loop {
            if let Some(item) = queue.pop_front() {
                self.not_full.notify_one();
                return Some(item);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            queue = self.not_empty.wait(queue).unwrap();
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.not_full.notify_all();
        self.not_empty.notify_all();
    }
}

/// Encode a sequence of raw XYZ frames using the in-process Grok FFI pipeline.
///
/// This is the high-performance path: N encoder threads share a bounded queue,
/// each calling `grk_compress()` directly (no subprocess overhead). Encoded
/// frames are written to disk by a dedicated writer thread.
///
/// `frame_producer` is called repeatedly to produce frames. Return `None` when done.
///
/// `phase_clocks` collects the preparation, compression and write time; the
/// producer is the only one that can time its own wait on the decoder, so it
/// adds [`EncodePhase::DecoderWait`] itself.
pub fn encode_pipeline<F, P>(
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    cancel: &Arc<AtomicBool>,
    phase_clocks: &Arc<PhaseClocks>,
    frame_producer: F,
    on_progress: P,
) -> PipelineResult
where
    F: FnMut() -> Option<RawFrame>,
    P: FnMut(EncodeProgress),
{
    encode_pipeline_with_mxf_feed(
        output_dir,
        params,
        total_frames,
        cancel,
        phase_clocks,
        None,
        None,
        frame_producer,
        on_progress,
    )
}

/// Like [`encode_pipeline`], but the writer thread also hands each codestream to
/// `mxf_feed` once it is on disk, so an MXF picture wrap can be written while the
/// encode runs instead of re-reading the whole J2K directory afterwards.
///
/// Frames reach the feed in the order they finish encoding, not in index order;
/// the wrap end reorders them. A feed that has stopped fails the encode, which is
/// how a wrap error gets out.
///
/// `codestream_byte_cap` is checked against each codestream as the writer puts it
/// on disk, and the first frame over it fails the run there rather than after the
/// whole sequence has been encoded.
#[allow(clippy::too_many_arguments)]
pub fn encode_pipeline_with_mxf_feed<F, P>(
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    cancel: &Arc<AtomicBool>,
    phase_clocks: &Arc<PhaseClocks>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    codestream_byte_cap: Option<u64>,
    mut frame_producer: F,
    mut on_progress: P,
) -> PipelineResult
where
    F: FnMut() -> Option<RawFrame>,
    P: FnMut(EncodeProgress),
{
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PipelineResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
            picture_findings: crate::picture_findings::PictureFindings::default(),
        };
    }

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    // grok >= 20.3.8 honors cparams.num_threads == 1 by giving each codec its
    // own inline executor, so n encoder threads => n independent single-thread
    // codecs with no global pool contention.
    let threads_per_codec = 1;
    let num_encoder_threads = num_threads;

    // Queue sized to keep all encoder threads fed without excessive memory use
    // (each 2K frame ≈ 21MB in planar i32)
    let queue_capacity = (num_encoder_threads * 2).clamp(4, 32);
    let input_queue: Arc<BoundedQueue<RawFrame>> = Arc::new(BoundedQueue::new(queue_capacity));

    // Writer channel, bounded like the input queue: a disk that cannot keep up
    // blocks whoever is sending, the encoder threads or the plugin's callback
    // threads, instead of piling finished codestreams up in memory
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(queue_capacity);

    let frames_encoded = Arc::new(AtomicU64::new(0));
    let error_flag = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(String::new()));

    let encode_start = std::time::Instant::now();

    // Writer thread — decoupled disk I/O
    let writer_output_dir = output_dir.to_path_buf();
    let writer_encoded_count = frames_encoded.clone();
    let writer_error_flag = error_flag.clone();
    let writer_first_error = first_error.clone();
    let writer_phase_clocks = phase_clocks.clone();
    let writer_input_queue = input_queue.clone();
    let writer_handle = std::thread::spawn(move || {
        for frame in writer_rx {
            let path = writer_output_dir.join(format!("frame_{:08}.j2c", frame.index));
            let write_start = std::time::Instant::now();
            let written = std::fs::write(&path, &frame.data);
            writer_phase_clocks.add(EncodePhase::CodestreamWrite, write_start.elapsed());
            if let Err(e) = written {
                fail_pipeline(
                    &writer_error_flag,
                    &writer_first_error,
                    &writer_input_queue,
                    format!("Write error frame {}: {e}", frame.index),
                );
                break;
            }
            if let Some(cap) = codestream_byte_cap
                && let Err(e) = crate::encode::check_codestream_size(&path, cap)
            {
                fail_pipeline(
                    &writer_error_flag,
                    &writer_first_error,
                    &writer_input_queue,
                    e,
                );
                break;
            }
            writer_encoded_count.fetch_add(1, Ordering::Relaxed);
            if let Some(feed) = &mxf_feed
                && let Err(e) = feed.send(frame.index, frame.data)
            {
                fail_pipeline(
                    &writer_error_flag,
                    &writer_first_error,
                    &writer_input_queue,
                    e,
                );
                break;
            }
        }
    });

    // Encoder threads
    let mut params = params.clone();
    params.threads_per_codec = threads_per_codec as u32;

    // The plugin's batch is fixed to one frame shape and needs it before the
    // first submit, so the first frame is pulled here and pushed ahead of the
    // rest. A PSNR target re-encodes by ratio when it overshoots the byte cap,
    // which the batch has no way to do, so it stays on the CPU.
    let mut pending_first_frame = None;
    let mut batch = None;
    if gpu_active()
        && params.quality_psnr.is_none()
        && let Some(frame) = frame_producer()
    {
        match Batch::begin(
            &frame,
            &params,
            &writer_tx,
            &error_flag,
            &first_error,
            &input_queue,
        ) {
            Ok(started) => batch = started,
            Err(e) => {
                drop(writer_tx);
                let _ = writer_handle.join();
                return PipelineResult {
                    success: false,
                    error: e,
                    frames_encoded: 0,
                    output_dir: output_dir.to_path_buf(),
                    picture_findings: crate::picture_findings::PictureFindings::default(),
                };
            }
        }
        pending_first_frame = Some(frame);
    }
    let batch_shape = batch.as_ref().map(Batch::shape);

    let frames_produced = std::thread::scope(|s| {
        let encoder_handles: Vec<_> = (0..num_encoder_threads)
            .map(|_| {
                let input_queue = input_queue.clone();
                let writer_tx = writer_tx.clone();
                let error_flag = error_flag.clone();
                let first_error = first_error.clone();
                let cancel = cancel.clone();
                let params = params.clone();
                let phase_clocks = phase_clocks.clone();

                s.spawn(move || {
                    encoder_thread_fn(
                        &input_queue,
                        &writer_tx,
                        &error_flag,
                        &first_error,
                        &cancel,
                        &params,
                        &phase_clocks,
                        batch_shape,
                    );
                })
            })
            .collect();

        // Drop our copy so writer sees hangup when all encoder threads finish
        drop(writer_tx);

        // Producer loop — feed frames into the bounded queue. Queue
        // backpressure paces this loop at encode speed, so it reports progress
        // too: the wait loop below only covers the final queue drain.
        let mut last_progress = std::time::Instant::now();
        let mut frames_produced = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) || error_flag.load(Ordering::Relaxed) {
                break;
            }

            match pending_first_frame.take().or_else(&mut frame_producer) {
                Some(frame) => {
                    if !input_queue.push(frame) {
                        break;
                    }
                    frames_produced += 1;
                }
                None => break,
            }

            if last_progress.elapsed().as_millis() >= 200 {
                last_progress = std::time::Instant::now();
                let done = frames_encoded.load(Ordering::Relaxed);
                let elapsed = encode_start.elapsed().as_secs_f64();
                on_progress(EncodeProgress::new(
                    done,
                    total_frames,
                    elapsed,
                    phase_clocks,
                ));
            }
        }

        // Signal encoder threads that no more frames are coming
        input_queue.close();

        // Scoped threads join here
        drop(encoder_handles);
        frames_produced
    });

    // The tail of the batch only reaches the callback in end(), so the drain
    // loop below has to wait for it.
    if let Some(batch) = batch
        && let Err(e) = batch.end()
    {
        fail_pipeline(&error_flag, &first_error, &input_queue, e);
    }

    // the producer may stop short of `total_frames`
    loop {
        let done = frames_encoded.load(Ordering::Relaxed);
        let elapsed = encode_start.elapsed().as_secs_f64();
        on_progress(EncodeProgress::new(
            done,
            total_frames,
            elapsed,
            phase_clocks,
        ));

        if done >= frames_produced
            || error_flag.load(Ordering::Relaxed)
            || cancel.load(Ordering::Relaxed)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Wait for writer to flush
    let _ = writer_handle.join();

    if cancel.load(Ordering::Relaxed) {
        return PipelineResult {
            success: false,
            error: "Cancelled".to_string(),
            frames_encoded: frames_encoded.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
            picture_findings: crate::picture_findings::PictureFindings::default(),
        };
    }

    let err = first_error.lock().unwrap();
    if !err.is_empty() {
        return PipelineResult {
            success: false,
            error: err.clone(),
            frames_encoded: frames_encoded.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
            picture_findings: crate::picture_findings::PictureFindings::default(),
        };
    }

    PipelineResult {
        success: true,
        error: String::new(),
        frames_encoded: frames_encoded.load(Ordering::Relaxed),
        output_dir: output_dir.to_path_buf(),
        picture_findings: crate::picture_findings::PictureFindings::default(),
    }
}

/// Keep the first error and stop the rest of the pipeline. Closing the input
/// queue is what releases a producer already blocked pushing into a full queue,
/// which nothing drains once the encoder threads have gone.
fn fail_pipeline(
    error_flag: &AtomicBool,
    first_error: &Mutex<String>,
    input_queue: &BoundedQueue<RawFrame>,
    message: String,
) {
    error_flag.store(true, Ordering::Relaxed);
    let mut err = first_error.lock().unwrap();
    if err.is_empty() {
        *err = message;
    }
    input_queue.close();
}

/// Per-thread encoder function. Pops frames from the queue, compresses them
/// in-process via Grok FFI, and sends encoded data to the writer channel.
///
/// With `batch_shape` set the thread submits each frame to the accelerator
/// plugin's running batch instead, and the batch's callback is what sends the
/// codestream to the writer channel.
#[allow(clippy::too_many_arguments)]
fn encoder_thread_fn(
    input_queue: &BoundedQueue<RawFrame>,
    writer_tx: &std::sync::mpsc::SyncSender<EncodedFrame>,
    error_flag: &AtomicBool,
    first_error: &Mutex<String>,
    cancel: &AtomicBool,
    params: &CompressParams,
    phase_clocks: &PhaseClocks,
    batch_shape: Option<BatchShape>,
) {
    // Pre-allocate output buffer once per thread and reuse across frames
    let buf_size = 2048 * 1080 * 3 * 2; // max 2K frame uncompressed size
    let mut output_buf = match batch_shape {
        Some(_) => Vec::new(),
        None => vec![0u8; buf_size],
    };

    while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
        let Some(mut frame) = input_queue.pop() else {
            break;
        };

        let prepare_start = std::time::Instant::now();
        let prepared = params
            .source_preparation
            .apply(&mut frame, params.apply_xyz_transform);
        phase_clocks.add(EncodePhase::Preparation, prepare_start.elapsed());

        if let Err(e) = prepared {
            fail_pipeline(
                error_flag,
                first_error,
                input_queue,
                format!("Encode failed frame {}: {e}", frame.index()),
            );
            break;
        }

        let encode_start = std::time::Instant::now();
        let compressed = match batch_shape {
            Some(shape) => submit_frame_to_batch(&frame, shape).map(|()| None),
            None => compress_frame_grok(&frame, params, &mut output_buf).map(Some),
        };
        phase_clocks.add(EncodePhase::Jpeg2000, encode_start.elapsed());

        match compressed {
            Ok(Some(data)) => {
                let encoded = EncodedFrame {
                    data,
                    index: frame.index(),
                };
                if writer_tx.send(encoded).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                fail_pipeline(
                    error_flag,
                    first_error,
                    input_queue,
                    format!("Encode failed frame {}: {e}", frame.index()),
                );
                break;
            }
        }
    }
    // a producer blocked on the full queue only wakes when the queue closes
    if cancel.load(Ordering::Relaxed) {
        input_queue.close();
    }
}

// ─── Grok FFI compression ──────────────────────────────────────────────────

/// How grok sizes one codestream.
#[cfg(feature = "grok-ffi")]
enum Allocation {
    /// grok's rate/distortion curve at this ratio, with a non-zero `max_bytes` a hard ceiling.
    Ratio { ratio: f64, max_bytes: u64 },
    /// grok's layer allocation at this PSNR target. `max_cs_size` does nothing
    /// here, so the target alone decides the size.
    Quality { psnr: f64 },
}

/// The bit depth the cinema profiles' picture is written at.
#[cfg(feature = "grok-ffi")]
const CINEMA_SAMPLE_PRECISION: u8 = 12;

#[cfg(feature = "grok-ffi")]
const GROK_IMAGE_COMPONENTS: u64 = 3;

#[cfg(feature = "grok-ffi")]
const BITS_PER_BYTE: u64 = 8;

/// The cinema profiles encode 3 components at 12 bits, and grok measures its
/// compression ratio against that rather than against the samples it was given.
#[cfg(feature = "grok-ffi")]
fn cinema_raw_frame_bytes(frame: &RawFrame) -> u64 {
    frame.width() as u64
        * frame.height() as u64
        * GROK_IMAGE_COMPONENTS
        * CINEMA_SAMPLE_PRECISION as u64
        / BITS_PER_BYTE
}

// grok budgets numcomps * prec * pixels / (ratio * 8), so a byte target becomes that ratio
#[cfg(feature = "grok-ffi")]
fn rate_allocation(frame: &RawFrame, params: &CompressParams) -> Result<Allocation, String> {
    let cap = params.codestream_byte_cap;
    let Some(target) = params.target_codestream_bytes else {
        return Ok(Allocation::Ratio {
            ratio: params.compression_ratio,
            max_bytes: cap.unwrap_or(0),
        });
    };
    let (rsiz, image_precision, _) = frame_encoding_shape(frame, params)?;
    let precision = grok_image_precision(rsiz, image_precision);
    let image_bits =
        GROK_IMAGE_COMPONENTS * precision as u64 * frame.width() as u64 * frame.height() as u64;
    Ok(Allocation::Ratio {
        ratio: image_bits as f64 / (BITS_PER_BYTE * target.max(1)) as f64,
        max_bytes: cap.map_or(target, |cap| cap.min(target)),
    })
}

/// Compress a single frame using Grok's in-process C API via FFI.
///
/// Safety: requires `grk_initialize()` to have been called once globally.
/// Each call creates a codec, compresses to a memory buffer, and cleans up.
/// The `output_buf` is a reusable scratch buffer to avoid per-frame allocation.
#[cfg(feature = "grok-ffi")]
fn compress_frame_grok(
    frame: &RawFrame,
    params: &CompressParams,
    output_buf: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    let Some(psnr) = params.quality_psnr else {
        let by_rate = rate_allocation(frame, params)?;
        return compress_frame_once(frame, params, by_rate, output_buf);
    };

    let compressed = compress_frame_once(frame, params, Allocation::Quality { psnr }, output_buf)?;
    let Some(cap) = params.codestream_byte_cap else {
        return Ok(compressed);
    };
    if compressed.len() as u64 <= cap {
        return Ok(compressed);
    }
    // grok holds to max_cs_size by rate but ignores it by quality, so a frame
    // the quality target cannot fit is compressed again by rate
    let by_ratio = Allocation::Ratio {
        ratio: cinema_raw_frame_bytes(frame) as f64 / cap as f64,
        max_bytes: cap,
    };
    compress_frame_once(frame, params, by_ratio, output_buf)
}

/// The bit depth IMF App 2E picture is written at.
#[cfg(feature = "grok-ffi")]
const IMF_SAMPLE_PRECISION: u8 = 12;

/// The rsiz the codestream carries, the precision the grok image is built at
/// and how far each source sample is shifted down to reach it.
#[cfg(feature = "grok-ffi")]
fn frame_encoding_shape(
    frame: &RawFrame,
    params: &CompressParams,
) -> Result<(u16, u8, u8), String> {
    let rsiz = crate::j2k::rsiz_for_raster(params.profile, frame.width(), frame.height())?;
    // grok reduces deeper samples to the 12 bits cinema profiles require,
    // fused with its X'Y'Z' transform, so frames pass through at full precision
    let mut precision = frame.precision();
    let mut bits_to_drop = 0u8;

    if crate::j2k::J2kProfile::from(params.profile) == crate::j2k::J2kProfile::Imf {
        if params.apply_xyz_transform {
            return Err(format!(
                "RSIZ {:#06x} is an IMF profile, whose picture is RGB, so the X'Y'Z' \
                 transform cannot be applied to it",
                params.profile
            ));
        }
        // grok writes an IMF codestream at whatever precision it is handed
        if precision < IMF_SAMPLE_PRECISION {
            return Err(format!(
                "IMF picture is written at {IMF_SAMPLE_PRECISION} bits and the frame is \
                 {precision}-bit"
            ));
        }
        bits_to_drop = precision - IMF_SAMPLE_PRECISION;
        precision = IMF_SAMPLE_PRECISION;
    }

    Ok((rsiz, precision, bits_to_drop))
}

/// A planar 32-bit grok image carrying the frame's samples shifted down by
/// `bits_to_drop`. The caller unrefs it.
#[cfg(feature = "grok-ffi")]
unsafe fn build_grok_image(
    frame: &RawFrame,
    precision: u8,
    bits_to_drop: u8,
) -> Result<*mut grokj2k_sys::grk_image, String> {
    use grokj2k_sys::*;
    use std::ptr;

    let width = frame.width();
    let height = frame.height();

    unsafe {
        // Set up image components
        let mut comps: [grk_image_comp; 3] = std::mem::zeroed();
        for comp in comps.iter_mut() {
            comp.w = width;
            comp.h = height;
            comp.dx = 1;
            comp.dy = 1;
            comp.prec = precision;
            comp.sgnd = false;
        }

        // labelled sycc, grok switches mct off for every untransformed frame
        let image = grk_image_new(
            3,
            comps.as_mut_ptr(),
            _GRK_COLOR_SPACE_GRK_CLRSPC_SRGB,
            true,
        );
        if image.is_null() {
            return Err("Failed to create Grok image".to_string());
        }

        // Fill component data based on frame format
        let w = width as usize;
        let h = height as usize;

        match frame {
            RawFrame::Planar { components, .. } => {
                for compno in 0..3u16 {
                    let comp = &*(*image).comps.add(compno as usize);
                    let comp_data = comp.data as *mut i32;
                    if comp_data.is_null() {
                        grk_object_unref(&mut (*image).obj);
                        return Err(format!("Null component data for component {compno}"));
                    }
                    let src = &components[compno as usize];
                    let stride = comp.stride as usize;
                    for y in 0..h {
                        let dst_row = comp_data.add(y * stride);
                        let src_row = &src[y * w..(y + 1) * w];
                        if bits_to_drop == 0 {
                            ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, w);
                            continue;
                        }
                        for (x, &sample) in src_row.iter().enumerate() {
                            *dst_row.add(x) = sample >> bits_to_drop;
                        }
                    }
                }
            }
            RawFrame::Packed { data, order, .. } => {
                // Deinterleave rgb48 directly into Grok component buffers
                // (avoids 21MB intermediate Vec<i32> allocation per frame)
                let comp0 = &*(*image).comps.add(0);
                let comp1 = &*(*image).comps.add(1);
                let comp2 = &*(*image).comps.add(2);
                let r_data = comp0.data as *mut i32;
                let g_data = comp1.data as *mut i32;
                let b_data = comp2.data as *mut i32;
                if r_data.is_null() || g_data.is_null() || b_data.is_null() {
                    grk_object_unref(&mut (*image).obj);
                    return Err("Null component data".to_string());
                }
                let stride = comp0.stride as usize;

                let sample = |bytes: [u8; 2]| match order {
                    SampleOrder::Big => u16::from_be_bytes(bytes),
                    SampleOrder::Little => u16::from_le_bytes(bytes),
                } as i32;

                for y in 0..h {
                    let row_offset = y * stride;
                    let src_row_offset = y * w * 6;
                    for x in 0..w {
                        let off = src_row_offset + x * 6;
                        let r = sample([data[off], data[off + 1]]);
                        let g = sample([data[off + 2], data[off + 3]]);
                        let b = sample([data[off + 4], data[off + 5]]);
                        *r_data.add(row_offset + x) = r >> bits_to_drop;
                        *g_data.add(row_offset + x) = g >> bits_to_drop;
                        *b_data.add(row_offset + x) = b >> bits_to_drop;
                    }
                }
            }
            RawFrame::PlanarYuv { format, .. } => {
                grk_object_unref(&mut (*image).obj);
                return Err(format!(
                    "a {} frame is grok's accelerator plugin's to convert and this \
                     compressor takes RGB",
                    format.pixel_format.ffmpeg_name()
                ));
            }
        }

        Ok(image)
    }
}

/// How many luma samples wide one chroma sample is in 4:2:2.
#[cfg(feature = "grok-ffi")]
pub const CHROMA_HORIZONTAL_SUBSAMPLING: u32 = 2;

/// One codestream from a 4:2:2 YCbCr frame: three planes at `precision` bits
/// with the chroma pair at half width, the shape ST 2067-21 allows for App 2E
/// CDCI picture.
///
/// Separate from [`encode_pipeline`], whose grok image is always 4:4:4, because
/// nothing in postkit converts a source to 4:2:2. It reads such essence back.
#[cfg(feature = "grok-ffi")]
pub fn compress_yuv422_frame(
    planes: [&[i32]; 3],
    width: u32,
    height: u32,
    precision: u8,
    params: &CompressParams,
) -> Result<Vec<u8>, String> {
    use grokj2k_sys::*;
    use std::ptr;

    let chroma_width = width.div_ceil(CHROMA_HORIZONTAL_SUBSAMPLING);
    let shape = [
        (width, height, 1u8, 1u8),
        (chroma_width, height, CHROMA_HORIZONTAL_SUBSAMPLING as u8, 1),
        (chroma_width, height, CHROMA_HORIZONTAL_SUBSAMPLING as u8, 1),
    ];
    for (index, (plane, (plane_width, plane_height, _, _))) in planes.iter().zip(shape).enumerate()
    {
        let samples = plane_width as usize * plane_height as usize;
        if plane.len() != samples {
            return Err(format!(
                "plane {index} holds {} samples, not the {samples} a {width}x{height} 4:2:2 \
                 frame is",
                plane.len()
            ));
        }
    }

    let rsiz = crate::j2k::rsiz_for_raster(params.profile, width, height)?;
    let allocation = Allocation::Ratio {
        ratio: params.compression_ratio,
        max_bytes: params.codestream_byte_cap.unwrap_or(0),
    };
    let mut output_buf = vec![0u8; width as usize * height as usize * 3 * 2];

    unsafe {
        let mut comps: [grk_image_comp; 3] = std::mem::zeroed();
        for (comp, (plane_width, plane_height, dx, dy)) in comps.iter_mut().zip(shape) {
            comp.w = plane_width;
            comp.h = plane_height;
            comp.dx = dx;
            comp.dy = dy;
            comp.prec = precision;
            comp.sgnd = false;
        }
        let image = grk_image_new(
            3,
            comps.as_mut_ptr(),
            _GRK_COLOR_SPACE_GRK_CLRSPC_SYCC,
            true,
        );
        if image.is_null() {
            return Err("Failed to create Grok image".to_string());
        }
        for (index, (plane, (plane_width, plane_height, _, _))) in
            planes.iter().zip(shape).enumerate()
        {
            let comp = &*(*image).comps.add(index);
            let data = comp.data as *mut i32;
            if data.is_null() {
                grk_object_unref(&mut (*image).obj);
                return Err(format!("Null component data for component {index}"));
            }
            let stride = comp.stride as usize;
            let plane_width = plane_width as usize;
            for row in 0..plane_height as usize {
                ptr::copy_nonoverlapping(
                    plane[row * plane_width..].as_ptr(),
                    data.add(row * stride),
                    plane_width,
                );
            }
        }

        let mut cparams = build_cparameters(params, rsiz, allocation);
        let mut stream_params: grk_stream_params = std::mem::zeroed();
        stream_params.buf = output_buf.as_mut_ptr();
        stream_params.buf_len = output_buf.len();

        let codec = grk_compress_init(&mut stream_params, &mut cparams, image);
        if codec.is_null() {
            grk_object_unref(&mut (*image).obj);
            return Err("Failed to initialize Grok compressor".to_string());
        }
        let compressed_len = grk_compress(codec, ptr::null_mut());
        grk_object_unref(codec);
        grk_object_unref(&mut (*image).obj);
        if compressed_len == 0 {
            return Err("Grok compression returned 0 bytes".to_string());
        }
        output_buf.truncate(compressed_len as usize);
        Ok(output_buf)
    }
}

/// The compression settings one codestream is written with.
#[cfg(feature = "grok-ffi")]
fn build_cparameters(
    params: &CompressParams,
    rsiz: u16,
    allocation: Allocation,
) -> grokj2k_sys::grk_cparameters {
    use grokj2k_sys::*;

    unsafe {
        let mut cparams: grk_cparameters = std::mem::zeroed();
        grk_compress_set_default_params(&mut cparams);

        cparams.cod_format = _GRK_SUPPORTED_FILE_FMT_GRK_FMT_J2K;
        cparams.numlayers = params.num_layers;
        match allocation {
            Allocation::Ratio { ratio, max_bytes } => {
                // at 1:1 the rate search still drops a pass, so ask for no allocation
                if ratio > 1.0 {
                    cparams.allocation_by_rate_distortion = true;
                    cparams.layer_rate[0] = ratio;
                }
                cparams.max_cs_size = max_bytes;
            }
            Allocation::Quality { psnr } => {
                cparams.allocation_by_quality = true;
                cparams.allocation_by_rate_distortion = false;
                cparams.layer_distortion[0] = psnr;
            }
        }
        cparams.numresolution = if rsiz == crate::j2k::CINEMA_4K_RSIZ {
            crate::j2k::CINEMA_4K_RESOLUTIONS
        } else {
            params.num_resolutions
        };
        cparams.cblockw_init = params.codeblock_size;
        cparams.cblockh_init = params.codeblock_size;
        cparams.irreversible = params.irreversible;
        cparams.mct = if params.mct { 1 } else { 0 };
        cparams.rsiz = rsiz;
        cparams.numgbits = params.num_guard_bits;
        // grok's default search tries every truncation point and halves the frame rate
        cparams.rate_control_algorithm = _GRK_RATE_CONTROL_ALGORITHM_GRK_RATE_CONTROL_PCRD_OPT;
        cparams.framerate = params.grok_frame_rate();
        cparams.num_threads = params.threads_per_codec;
        cparams.apply_xyz_transform = params.apply_xyz_transform;
        // Bv2.1 requires a TLM marker in every codestream (libdcp's
        // MISSING_JPEG2000_TLM_MARKER), and grok writes none unless asked
        cparams.write_tlm = true;

        cparams.prog_order = match params.progression {
            ProgressionOrder::Lrcp => _GRK_PROG_ORDER_GRK_LRCP,
            ProgressionOrder::Rlcp => _GRK_PROG_ORDER_GRK_RLCP,
            ProgressionOrder::Rpcl => _GRK_PROG_ORDER_GRK_RPCL,
            ProgressionOrder::Pcrl => _GRK_PROG_ORDER_GRK_PCRL,
            ProgressionOrder::Cprl => _GRK_PROG_ORDER_GRK_CPRL,
        };

        cparams
    }
}

#[cfg(feature = "grok-ffi")]
fn compress_frame_once(
    frame: &RawFrame,
    params: &CompressParams,
    allocation: Allocation,
    output_buf: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    use grokj2k_sys::*;
    use std::ptr;

    let (rsiz, precision, bits_to_drop) = frame_encoding_shape(frame, params)?;

    let needed = (frame.width() as usize) * (frame.height() as usize) * 3 * 2;
    if output_buf.len() < needed {
        output_buf.resize(needed, 0);
    }

    unsafe {
        let image = build_grok_image(frame, precision, bits_to_drop)?;
        let mut cparams = build_cparameters(params, rsiz, allocation);

        let mut stream_params: grk_stream_params = std::mem::zeroed();
        stream_params.buf = output_buf.as_mut_ptr();
        stream_params.buf_len = output_buf.len();

        let codec = grk_compress_init(&mut stream_params, &mut cparams, image);
        if codec.is_null() {
            grk_object_unref(&mut (*image).obj);
            return Err("Failed to initialize Grok compressor".to_string());
        }

        let compressed_len = grk_compress(codec, ptr::null_mut());
        grk_object_unref(codec);
        grk_object_unref(&mut (*image).obj);

        if compressed_len == 0 {
            return Err("Grok compression returned 0 bytes".to_string());
        }

        Ok(output_buf[..compressed_len as usize].to_vec())
    }
}

#[cfg(not(feature = "grok-ffi"))]
fn compress_frame_grok(
    _frame: &RawFrame,
    _params: &CompressParams,
    _output_buf: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    Err("grok-ffi feature not enabled — cannot use in-process encoder".to_string())
}

// ─── the accelerator plugin's in-memory batch ─────────────────────────────────

/// The frame shape one batch was begun with. Every frame submitted to it has to
/// match, and the encoder threads build their grok image from it.
#[cfg(feature = "grok-ffi")]
#[derive(Clone, Copy)]
struct BatchShape {
    width: u32,
    height: u32,
    /// what the grok image the encoder threads build carries
    image_precision: u8,
    /// how far a source sample is shifted down to reach `image_precision`
    bits_to_drop: u8,
}

#[cfg(not(feature = "grok-ffi"))]
#[derive(Clone, Copy)]
enum BatchShape {}

/// What the plugin's callback threads need to hand a finished codestream to the
/// writer thread. One of these is shared by every callback thread.
#[cfg(feature = "grok-ffi")]
struct BatchCollector {
    writer_tx: std::sync::mpsc::SyncSender<EncodedFrame>,
    error_flag: Arc<AtomicBool>,
    first_error: Arc<Mutex<String>>,
    input_queue: Arc<BoundedQueue<RawFrame>>,
}

/// How many frames the plugin's batch has handed back with a codestream.
/// `grk_plugin_accelerated_frames` counts only the per-call routed path.
#[cfg(feature = "grok-ffi")]
static BATCH_ACCELERATED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Runs on the plugin's threads, concurrently with itself and with the encoder
/// threads still submitting.
#[cfg(feature = "grok-ffi")]
unsafe extern "C" fn batch_frame_callback(
    user: *mut std::ffi::c_void,
    frame_user: *mut std::ffi::c_void,
    codestream: *const u8,
    length: usize,
) {
    let collector = unsafe { &*(user as *const BatchCollector) };
    let index = frame_user as u64;

    if length == 0 || codestream.is_null() {
        fail_pipeline(
            &collector.error_flag,
            &collector.first_error,
            &collector.input_queue,
            format!("Encode failed frame {index}: the batch returned no codestream"),
        );
        return;
    }

    let data = unsafe { std::slice::from_raw_parts(codestream, length) }.to_vec();
    BATCH_ACCELERATED_FRAMES.fetch_add(1, Ordering::Relaxed);
    let _ = collector.writer_tx.send(EncodedFrame { data, index });
}

/// One run of the accelerator plugin's in-memory batch. Dropping it after
/// [`Batch::end`] releases the parameters and the callback state the plugin
/// held for the whole batch.
#[cfg(feature = "grok-ffi")]
struct Batch {
    shape: BatchShape,
    /// the plugin reads both of these until `end` returns
    _parameters: Box<grokj2k_sys::grk_cparameters>,
    _collector: Box<BatchCollector>,
}

#[cfg(not(feature = "grok-ffi"))]
enum Batch {}

#[cfg(feature = "grok-ffi")]
impl Batch {
    /// Start a batch shaped by `frame`. `Ok(None)` means the plugin declined
    /// these parameters and the caller compresses on the CPU.
    fn begin(
        frame: &RawFrame,
        params: &CompressParams,
        writer_tx: &std::sync::mpsc::SyncSender<EncodedFrame>,
        error_flag: &Arc<AtomicBool>,
        first_error: &Arc<Mutex<String>>,
        input_queue: &Arc<BoundedQueue<RawFrame>>,
    ) -> Result<Option<Self>, String> {
        let (rsiz, image_precision, bits_to_drop) = frame_encoding_shape(frame, params)?;
        let by_rate = rate_allocation(frame, params)?;
        let mut parameters = Box::new(build_cparameters(params, rsiz, by_rate));
        let collector = Box::new(BatchCollector {
            writer_tx: writer_tx.clone(),
            error_flag: error_flag.clone(),
            first_error: first_error.clone(),
            input_queue: input_queue.clone(),
        });

        let mut info: grokj2k_sys::grk_plugin_batch_memory_info = unsafe { std::mem::zeroed() };
        info.compress_parameters = parameters.as_mut() as *mut grokj2k_sys::grk_cparameters;
        info.width = frame.width();
        info.height = frame.height();
        info.numcomps = 3;
        info.prec = grok_image_precision(rsiz, image_precision);
        // a zeroed info already describes the planar RGB frames postkit
        // deinterleaves itself, so only a YUV or interleaved source fills
        // these in
        info.source_prec = match frame {
            RawFrame::PlanarYuv { format, .. } => {
                info.source_format = if format.pixel_format.chroma_is_half_height() {
                    grokj2k_sys::_GRK_SOURCE_FORMAT_GRK_SOURCE_YUV420P
                } else {
                    grokj2k_sys::_GRK_SOURCE_FORMAT_GRK_SOURCE_YUV422P
                };
                info.yuv_matrix = match format.matrix {
                    crate::encode::YuvMatrix::Bt601 => grokj2k_sys::_GRK_YUV_MATRIX_GRK_YUV_BT601,
                    crate::encode::YuvMatrix::Bt709 => grokj2k_sys::_GRK_YUV_MATRIX_GRK_YUV_BT709,
                    crate::encode::YuvMatrix::Bt2020 => grokj2k_sys::_GRK_YUV_MATRIX_GRK_YUV_BT2020,
                };
                info.yuv_full_range = format.full_range;
                format.pixel_format.bit_depth()
            }
            RawFrame::Packed {
                order: SampleOrder::Little,
                precision,
                ..
            } => {
                info.source_format = grokj2k_sys::_GRK_SOURCE_FORMAT_GRK_SOURCE_RGB48LE;
                *precision
            }
            _ => image_precision,
        };
        let mut xyz_on_device = false;
        info.xyz_on_device = &mut xyz_on_device;
        info.callback = Some(batch_frame_callback);
        info.user = collector.as_ref() as *const BatchCollector as *mut std::ffi::c_void;

        match unsafe { grokj2k_sys::grk_plugin_batch_memory_begin(info) } {
            0 => {
                tracing::info!(
                    colour_transform_on_device = xyz_on_device,
                    "grok's accelerator plugin batch is running"
                );
                Ok(Some(Self {
                    shape: BatchShape {
                        width: frame.width(),
                        height: frame.height(),
                        image_precision,
                        bits_to_drop,
                    },
                    _parameters: parameters,
                    _collector: collector,
                }))
            }
            1 => {
                tracing::info!(
                    width = frame.width(),
                    height = frame.height(),
                    precision = grok_image_precision(rsiz, image_precision),
                    rsiz = format!("{rsiz:#06x}"),
                    "grok's accelerator plugin does not handle this frame shape or these \
                     compression parameters as a batch, compressing on the CPU"
                );
                Ok(None)
            }
            code => Err(format!(
                "grok's accelerator plugin failed to start a batch: \
                 grk_plugin_batch_memory_begin returned {code}"
            )),
        }
    }

    fn shape(&self) -> BatchShape {
        self.shape
    }

    /// Drain the batch. Every submitted frame has reached the callback when
    /// this returns.
    fn end(self) -> Result<(), String> {
        if unsafe { grokj2k_sys::grk_plugin_batch_memory_end() } {
            return Ok(());
        }
        Err("grok's accelerator plugin failed to drain the batch: \
             grk_plugin_batch_memory_end returned false"
            .to_string())
    }
}

#[cfg(not(feature = "grok-ffi"))]
impl Batch {
    fn begin(
        _frame: &RawFrame,
        _params: &CompressParams,
        _writer_tx: &std::sync::mpsc::SyncSender<EncodedFrame>,
        _error_flag: &Arc<AtomicBool>,
        _first_error: &Arc<Mutex<String>>,
        _input_queue: &Arc<BoundedQueue<RawFrame>>,
    ) -> Result<Option<Self>, String> {
        Ok(None)
    }

    fn shape(&self) -> BatchShape {
        match *self {}
    }

    fn end(self) -> Result<(), String> {
        match self {}
    }
}

/// Whether grok's accelerator plugin takes frames shaped like `shape_only`,
/// asked by beginning a batch with the run's own compression parameters and
/// ending it at once with nothing submitted. `shape_only` carries the frame's
/// size, format and precision and no bytes. The answer has to be had before the
/// decoder starts, because it decides what ffmpeg writes.
///
/// Every ask starts a batch, so a run asks at most once per candidate source.
/// `false` unless the plugin is switched on: nothing else takes these sources.
#[cfg(feature = "grok-ffi")]
pub fn plugin_takes_frame(shape_only: &RawFrame, params: &CompressParams) -> bool {
    if !gpu_active() {
        return false;
    }
    let (writer_tx, _writer_rx) = std::sync::mpsc::sync_channel(1);
    let error_flag = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(String::new()));
    let input_queue = Arc::new(BoundedQueue::new(1));
    match Batch::begin(
        shape_only,
        params,
        &writer_tx,
        &error_flag,
        &first_error,
        &input_queue,
    ) {
        Ok(Some(batch)) => batch.end().is_ok(),
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("grok's accelerator plugin could not be asked about the source: {e}");
            false
        }
    }
}

#[cfg(not(feature = "grok-ffi"))]
pub fn plugin_takes_frame(_shape_only: &RawFrame, _params: &CompressParams) -> bool {
    false
}

// grok budgets from the 12 bit image its cinema transform emits, not the frame's depth
#[cfg(feature = "grok-ffi")]
fn grok_image_precision(rsiz: u16, image_precision: u8) -> u8 {
    let cinema = matches!(
        crate::j2k::J2kProfile::from(rsiz),
        crate::j2k::J2kProfile::Cinema2k | crate::j2k::J2kProfile::Cinema4k
    );
    if cinema {
        CINEMA_SAMPLE_PRECISION
    } else {
        image_precision
    }
}

/// A grok image whose three components point straight at the frame's planes.
/// grok allocates nothing here and copies the planes inside submit, so the
/// frame can be dropped as soon as that call comes back. The caller unrefs the
/// image.
#[cfg(feature = "grok-ffi")]
unsafe fn build_yuv_grok_image(
    data: &[u8],
    format: crate::encode::YuvFrameFormat,
    width: u32,
    height: u32,
) -> Result<*mut grokj2k_sys::grk_image, String> {
    use grokj2k_sys::*;

    let layout = format.pixel_format.plane_layout(width, height);
    if data.len() < layout.frame_bytes {
        return Err(format!(
            "a {} frame at {width}x{height} is {} bytes and this one is {}",
            format.pixel_format.ffmpeg_name(),
            layout.frame_bytes,
            data.len()
        ));
    }
    let source_precision = format.pixel_format.bit_depth();
    let data_type = if format.pixel_format.bytes_per_sample() > 1 {
        _grk_data_type_GRK_INT_16
    } else {
        _grk_data_type_GRK_INT_8
    };
    let chroma_vertical_sampling = if format.pixel_format.chroma_is_half_height() {
        2
    } else {
        1
    };
    let planes = [
        (
            layout.luma_offset,
            layout.luma_width,
            layout.luma_height,
            1u8,
            1u8,
        ),
        (
            layout.blue_chroma_offset,
            layout.chroma_width,
            layout.chroma_height,
            2,
            chroma_vertical_sampling,
        ),
        (
            layout.red_chroma_offset,
            layout.chroma_width,
            layout.chroma_height,
            2,
            chroma_vertical_sampling,
        ),
    ];

    unsafe {
        let mut comps: [grk_image_comp; 3] = std::mem::zeroed();
        for (comp, (_, plane_width, plane_height, dx, dy)) in comps.iter_mut().zip(planes) {
            comp.w = plane_width;
            comp.h = plane_height;
            comp.stride = plane_width;
            comp.dx = dx;
            comp.dy = dy;
            comp.prec = source_precision;
            comp.sgnd = false;
            comp.data_type = data_type;
        }

        let image = grk_image_new(
            3,
            comps.as_mut_ptr(),
            _GRK_COLOR_SPACE_GRK_CLRSPC_SYCC,
            false,
        );
        if image.is_null() {
            return Err("Failed to create Grok image".to_string());
        }
        for (index, (offset, plane_width, plane_height, dx, dy)) in planes.into_iter().enumerate() {
            let comp = &mut *(*image).comps.add(index);
            comp.w = plane_width;
            comp.h = plane_height;
            comp.stride = plane_width;
            comp.dx = dx;
            comp.dy = dy;
            comp.prec = source_precision;
            comp.data_type = data_type;
            comp.owns_data = false;
            comp.data = data.as_ptr().add(offset) as *mut std::ffi::c_void;
        }
        Ok(image)
    }
}

/// A grok image over one interleaved rgb48le frame, the shape
/// `GRK_SOURCE_RGB48LE` takes: three components at the frame's own precision,
/// `comps[0]` pointing at the buffer with the row pitch in 16-bit samples and
/// the other two carrying no data. grok allocates nothing here and copies the
/// buffer inside submit. The caller unrefs the image.
#[cfg(feature = "grok-ffi")]
unsafe fn build_rgb48le_grok_image(
    data: &[u8],
    width: u32,
    height: u32,
    precision: u8,
) -> Result<*mut grokj2k_sys::grk_image, String> {
    use grokj2k_sys::*;

    const BYTES_PER_PIXEL: usize = 6;
    const SAMPLES_PER_PIXEL: u32 = 3;
    let frame_bytes = width as usize * height as usize * BYTES_PER_PIXEL;
    if data.len() < frame_bytes {
        return Err(format!(
            "an rgb48le frame at {width}x{height} is {frame_bytes} bytes and this one is {}",
            data.len()
        ));
    }

    unsafe {
        let mut comps: [grk_image_comp; 3] = std::mem::zeroed();
        for comp in comps.iter_mut() {
            comp.w = width;
            comp.h = height;
            comp.dx = 1;
            comp.dy = 1;
            comp.prec = precision;
            comp.sgnd = false;
            comp.data_type = _grk_data_type_GRK_INT_16;
        }

        let image = grk_image_new(
            3,
            comps.as_mut_ptr(),
            _GRK_COLOR_SPACE_GRK_CLRSPC_SRGB,
            false,
        );
        if image.is_null() {
            return Err("Failed to create Grok image".to_string());
        }
        let first = &mut *(*image).comps;
        first.stride = width * SAMPLES_PER_PIXEL;
        first.owns_data = false;
        first.data = data.as_ptr() as *mut std::ffi::c_void;
        Ok(image)
    }
}

/// Hand one frame to the running batch. Blocks while the plugin's pipeline is
/// full, which is what paces the encoder threads.
#[cfg(feature = "grok-ffi")]
fn submit_frame_to_batch(frame: &RawFrame, shape: BatchShape) -> Result<(), String> {
    if frame.width() != shape.width || frame.height() != shape.height {
        return Err(format!(
            "the batch was begun at {}x{} and this frame is {}x{}",
            shape.width,
            shape.height,
            frame.width(),
            frame.height()
        ));
    }

    unsafe {
        let image = match frame {
            RawFrame::PlanarYuv {
                data,
                format,
                width,
                height,
                ..
            } => build_yuv_grok_image(data, *format, *width, *height)?,
            RawFrame::Packed {
                data,
                order: SampleOrder::Little,
                width,
                height,
                precision,
                ..
            } => build_rgb48le_grok_image(data, *width, *height, *precision)?,
            _ => build_grok_image(frame, shape.image_precision, shape.bits_to_drop)?,
        };
        let frame_user = frame.index() as usize as *mut std::ffi::c_void;
        let submitted = grokj2k_sys::grk_plugin_batch_memory_submit(image, frame_user);
        grokj2k_sys::grk_object_unref(&mut (*image).obj);
        if !submitted {
            return Err("grk_plugin_batch_memory_submit refused the frame".to_string());
        }
    }
    Ok(())
}

#[cfg(not(feature = "grok-ffi"))]
fn submit_frame_to_batch(_frame: &RawFrame, shape: BatchShape) -> Result<(), String> {
    match shape {}
}

/// Initialize the Grok library. Must be called once before using the encoder.
/// Pass `num_threads = 0` to use all available CPUs for Grok's internal thread pool.
///
/// Safe to call multiple times (subsequent calls are no-ops).
#[cfg(feature = "grok-ffi")]
pub fn initialize(num_threads: u32) {
    unsafe {
        grokj2k_sys::grk_initialize(std::ptr::null(), num_threads, std::ptr::null_mut());
    }
}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn initialize(_num_threads: u32) {}

// ─── grok's accelerator plugin ────────────────────────────────────────────────

/// Whether the plugin is switched into grok's compress and decompress calls.
/// [`use_gpu`] and [`use_cpu`] are the only writers.
#[cfg(feature = "grok-ffi")]
static ACCELERATOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// The plugin's in-memory path runs on the first device it finds, so there is
/// no device left to choose.
#[cfg(feature = "grok-ffi")]
const ACCELERATOR_DEVICE_ID: i32 = 0;

/// Run every `grk_compress` and `grk_decompress` in this process on grok's
/// accelerator device. [`initialize`] has to have run first, because that is
/// the call which loads the plugin.
///
/// grok does the routing from here on, so no encode or decode call in postkit
/// changes. The device takes the wavelet and T1 while the host keeps the
/// header, the rate allocation and the packets. Frames the plugin does not
/// handle stay on the CPU inside grok, which covers a decode at a `reduce`
/// above 0, a tiled stream and a code block style. A device failure fails the
/// call rather than falling back.
#[cfg(feature = "grok-ffi")]
pub fn use_gpu() -> Result<(), String> {
    let init_info = grokj2k_sys::grk_plugin_init_info {
        device_id: ACCELERATOR_DEVICE_ID,
        verbose: false,
        license: std::ptr::null(),
        server: std::ptr::null(),
    };
    if !unsafe { grokj2k_sys::grk_plugin_init(init_info) } {
        return Err(
            "grok's accelerator plugin did not initialise. initialize() \
             looks for libgrokj2k_plugin under GRK_PLUGIN_PATH, then in the working \
             directory, then next to the executable, and searches nowhere at all when \
             GRK_NO_PLUGIN is set. A plugin that did load refuses here when the device \
             is unavailable."
                .to_string(),
        );
    }
    unsafe { grokj2k_sys::grk_plugin_set_enabled(true) };
    ACCELERATOR_ENABLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Send every frame back to the CPU. The plugin stays loaded and [`use_gpu`]
/// switches it in again. Safe to call when no plugin was ever loaded.
#[cfg(feature = "grok-ffi")]
pub fn use_cpu() {
    unsafe { grokj2k_sys::grk_plugin_set_enabled(false) };
    ACCELERATOR_ENABLED.store(false, Ordering::Relaxed);
}

/// Whether [`use_gpu`] has succeeded and [`use_cpu`] has not undone it.
#[cfg(feature = "grok-ffi")]
pub fn gpu_active() -> bool {
    ACCELERATOR_ENABLED.load(Ordering::Relaxed)
}

/// How many compress and decompress calls the device has run since the process
/// started. grok counts the calls it routed one at a time and postkit counts
/// the frames its batches got a codestream back for.
#[cfg(feature = "grok-ffi")]
pub fn accelerated_frames() -> u64 {
    let routed = unsafe { grokj2k_sys::grk_plugin_accelerated_frames() };
    routed + BATCH_ACCELERATED_FRAMES.load(Ordering::Relaxed)
}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn use_gpu() -> Result<(), String> {
    Err("postkit was built without the grok-ffi feature".to_string())
}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn use_cpu() {}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn gpu_active() -> bool {
    false
}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn accelerated_frames() -> u64 {
    0
}

// ─── Video-to-J2K in-process pipeline (ffmpeg pipe → Grok FFI) ─────────────────

/// High-performance video-to-J2K pipeline: decodes video with ffmpeg and encodes
/// each frame in-process via Grok FFI. No intermediate files on disk.
///
/// Pipeline: `ffmpeg -i input -pix_fmt rgb48be -f rawvideo pipe:1`
///           → read raw frame buffers from stdout
///           → bounded queue → N encoder threads (1-thread Grok per frame)
///           → writer thread → .j2c files on disk
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
    on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    encode_video_pipeline_resumable(
        input_video,
        output_dir,
        params,
        total_frames,
        width,
        height,
        &crate::probe::probe_pixel_format(input_video),
        cancel,
        false,
        None,
        None,
        on_progress,
    )
}

/// Number of `frame_{:08}.j2c` files forming a contiguous prefix (0,1,2,...) in
/// `dir`. This is the count of frames a resumable encode can safely skip.
pub fn contiguous_encoded_frames(dir: &Path) -> u64 {
    let mut n = 0u64;
    while dir.join(format!("frame_{n:08}.j2c")).is_file() {
        n += 1;
    }
    n
}

/// The `-vf` chain for a decode that may be windowed. The caller's filters run
/// at source timing and the window is cut out of what they produced, so a fade
/// lands where a full encode would have put it and frame N of the window is the
/// frame a full encode would have written at `first_frame + N`.
fn window_filter_chain(
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
) -> Option<String> {
    let Some(range) = frame_range else {
        return video_filter.map(str::to_string);
    };
    let mut chain: Vec<String> = video_filter.into_iter().map(str::to_string).collect();
    chain.extend(range.trim_filters());
    Some(chain.join(","))
}

/// Like [`encode_video_pipeline`], but when `resume` is true it skips frames
/// already encoded on disk (dom#344: an interrupted encode picks up where it
/// left off). The already-present contiguous prefix is decoded-and-discarded so
/// ffmpeg stays frame-aligned, then encoding continues from the next index. The
/// last existing frame is always re-encoded in case it was truncated by the
/// interruption.
///
/// `frame_range` encodes one window of the source instead of all of it. With a
/// window, `total_frames` is the window's length rather than the source's, the
/// codestreams are numbered from zero, and `resume` counts inside the window.
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_resumable<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    source: &crate::probe::PixelFormatInfo,
    cancel: &Arc<AtomicBool>,
    resume: bool,
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
    on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    encode_video_pipeline_resumable_with_mxf_feed(
        input_video,
        output_dir,
        params,
        total_frames,
        width,
        height,
        source,
        cancel,
        resume,
        video_filter,
        frame_range,
        None,
        on_progress,
    )
}

/// Like [`encode_video_pipeline_resumable`], but the writer thread also hands
/// each codestream to `mxf_feed` once it is on disk, so a picture MXF can be
/// written while the encode runs. A resumed run hands the feed only the frames
/// it encodes, so a caller that resumes has no complete wrap to feed.
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_resumable_with_mxf_feed<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    source: &crate::probe::PixelFormatInfo,
    cancel: &Arc<AtomicBool>,
    resume: bool,
    // ffmpeg -vf chain applied while decoding, for fades and the like. It must
    // not change the frame size or count: the reader slices stdout into fixed
    // width*height frames and the CPL already declares the count.
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    mut on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    use std::io::Read;
    use std::process::{Command, Stdio};

    if let Some(range) = frame_range {
        let (_, _, source_frames) = crate::encode::probe_video(input_video);
        if let Err(e) = range.check_against_probe(source_frames) {
            return PipelineResult {
                success: false,
                error: e,
                frames_encoded: 0,
                output_dir: output_dir.to_path_buf(),
                picture_findings: crate::picture_findings::PictureFindings::default(),
            };
        }
    }

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PipelineResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
            picture_findings: crate::picture_findings::PictureFindings::default(),
        };
    }

    // resume: re-encode from the last-but-one existing frame (the last is
    // suspect: fs::write is not atomic, so an interrupt can truncate it).
    let start_frame = if resume {
        contiguous_encoded_frames(output_dir).saturating_sub(1)
    } else {
        0
    };
    // everything already done: nothing to encode.
    if resume && total_frames > 0 && start_frame >= total_frames {
        return PipelineResult {
            success: true,
            error: String::new(),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
            picture_findings: crate::picture_findings::PictureFindings::default(),
        };
    }

    let picture_filters = window_filter_chain(video_filter, frame_range).unwrap_or_default();
    let accelerator_active = gpu_active();
    let chain = match crate::encode::decode_chain(
        &crate::encode::DecodeChainInputs {
            decode_source: crate::encode::DecodeSource::Video,
            read_source_at: None,
            picture: crate::encode::PictureFilters::Given(&picture_filters),
            // the colour this path can convert is the compressor's own
            // transform, and a caller's colour filter is caught by the chain
            source_colour: &crate::encode::SourceColour::DisplayRgb,
            source,
            accelerator_active,
            quality_psnr: params.quality_psnr,
            postkit_prepares_the_frame: !params.source_preparation.is_empty(),
        },
        width,
        height,
        params,
    ) {
        Ok(chain) => chain,
        Err(e) => {
            return PipelineResult {
                success: false,
                error: e,
                frames_encoded: 0,
                output_dir: output_dir.to_path_buf(),
                picture_findings: crate::picture_findings::PictureFindings::default(),
            };
        }
    };
    let pipe_format = chain.pipe_format;
    let filters = chain.filters;

    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        // the progress line carries no newline, so the reader would hold the
        // whole run in one string
        .arg("-nostats")
        .args(&chain.input_args);
    command
        .arg("-i")
        .arg(input_video)
        .arg("-vf")
        .arg(&filters)
        .arg("-pix_fmt")
        .arg(pipe_format.ffmpeg_pixel_format())
        .arg("-f")
        .arg("rawvideo");
    if let Some(range) = frame_range {
        command.args(range.frame_limit_args());
    }
    let mut child = match command
        .arg("pipe:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return PipelineResult {
                success: false,
                error: format!("Failed to spawn ffmpeg: {e}"),
                frames_encoded: 0,
                output_dir: output_dir.to_path_buf(),
                picture_findings: crate::picture_findings::PictureFindings::default(),
            };
        }
    };

    let detection_reader = child
        .stderr
        .take()
        .map(crate::picture_findings::read_detection_lines);

    let frame_size = pipe_format.frame_bytes(width, height);
    let mut stdout = child.stdout.take().unwrap();

    // discard the already-encoded prefix so ffmpeg stays frame-aligned.
    let mut frame_index: u64 = 0;
    let mut decode_read_to_end = false;
    if start_frame > 0 {
        let mut skip_buf = vec![0u8; frame_size];
        let mut aligned = true;
        while frame_index < start_frame {
            if stdout.read_exact(&mut skip_buf).is_err() {
                aligned = false;
                break;
            }
            frame_index += 1;
        }
        if !aligned {
            let _ = child.kill();
            let _ = child.wait();
            return PipelineResult {
                success: false,
                error: format!(
                    "resume: source has fewer than {start_frame} frames, cannot skip the encoded prefix"
                ),
                frames_encoded: 0,
                output_dir: output_dir.to_path_buf(),
                picture_findings: crate::picture_findings::PictureFindings::default(),
            };
        }
    }

    // progress total is the remaining frames so the pipeline's completion check
    // (done >= total) is reachable after a resume.
    let remaining_total = total_frames.saturating_sub(start_frame);
    let phase_clocks = Arc::new(PhaseClocks::default());
    let result = encode_pipeline_with_mxf_feed(
        output_dir,
        params,
        remaining_total,
        cancel,
        &phase_clocks,
        mxf_feed,
        None,
        || {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut buf = vec![0u8; frame_size];
            let read_start = std::time::Instant::now();
            let read = stdout.read_exact(&mut buf);
            phase_clocks.add(EncodePhase::DecoderWait, read_start.elapsed());
            match read {
                Ok(()) => {
                    let idx = frame_index;
                    frame_index += 1;
                    Some(match pipe_format {
                        crate::encode::PipeFormat::PackedRgb(order) => RawFrame::Packed {
                            data: buf,
                            order,
                            width,
                            height,
                            precision: crate::encode::PACKED_RGB_PRECISION,
                            index: idx,
                        },
                        crate::encode::PipeFormat::PlanarYuv(format) => RawFrame::PlanarYuv {
                            data: buf,
                            format,
                            width,
                            height,
                            index: idx,
                        },
                    })
                }
                Err(e) => {
                    decode_read_to_end = e.kind() == std::io::ErrorKind::UnexpectedEof;
                    None // EOF or error — no more frames
                }
            }
        },
        &mut on_progress,
    );

    // the detection timestamps are on ffmpeg's whole output, which after a
    // resume is longer than the frames this run encoded
    let picture_findings = crate::picture_findings::finish_detection(
        &mut child,
        detection_reader,
        decode_read_to_end,
        params.edit_rate.as_f64(),
        frame_index,
    );

    PipelineResult {
        picture_findings,
        ..result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A planar YUV frame is shifted nowhere after the plugin converts it, so
    /// the depth it converts to has to be the depth the code stream carries.
    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_yuv_conversion_writes_the_code_stream_depth() {
        assert_eq!(PLANAR_YUV_SAMPLE_PRECISION, CINEMA_SAMPLE_PRECISION);
        assert_eq!(PLANAR_YUV_SAMPLE_PRECISION, IMF_SAMPLE_PRECISION);
    }

    /// The window is cut after the caller's filters, so a fade keeps the timing
    /// a full encode would have given it.
    #[test]
    fn a_frame_range_trims_the_decode_after_the_callers_filters() {
        let window = crate::encode::FrameRange {
            first_frame: 10,
            frame_count: 5,
        };

        assert_eq!(window_filter_chain(None, None), None);
        assert_eq!(
            window_filter_chain(Some("crop=1920:804:0:138"), None).as_deref(),
            Some("crop=1920:804:0:138"),
            "no window has to leave the caller's chain as it was"
        );
        assert_eq!(
            window_filter_chain(None, Some(window)).as_deref(),
            Some("trim=start_frame=10:end_frame=15,setpts=PTS-STARTPTS")
        );
        assert_eq!(
            window_filter_chain(Some("crop=1920:804:0:138,fade=out:96:24"), Some(window))
                .as_deref(),
            Some(
                "crop=1920:804:0:138,fade=out:96:24,\
                 trim=start_frame=10:end_frame=15,setpts=PTS-STARTPTS"
            )
        );

        assert_eq!(window.frame_limit_args(), ["-frames:v", "5"]);
    }

    #[test]
    fn contiguous_frames_counts_prefix_and_stops_at_gap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(contiguous_encoded_frames(p), 0);
        for i in 0..3 {
            std::fs::write(p.join(format!("frame_{i:08}.j2c")), b"x").unwrap();
        }
        assert_eq!(contiguous_encoded_frames(p), 3);
        // a gap (missing frame 3) then frame 4 must not extend the prefix.
        std::fs::write(p.join(format!("frame_{:08}.j2c", 4)), b"x").unwrap();
        assert_eq!(contiguous_encoded_frames(p), 3);
    }

    #[test]
    fn test_bounded_queue_basic() {
        let queue: Arc<BoundedQueue<u32>> = Arc::new(BoundedQueue::new(3));
        assert!(queue.push(1));
        assert!(queue.push(2));
        assert!(queue.push(3));
        assert_eq!(queue.pop(), Some(1)); // fifo, oldest first
        assert!(queue.push(4));
        queue.close();
    }

    #[test]
    fn test_bounded_queue_close_unblocks() {
        let queue: Arc<BoundedQueue<u32>> = Arc::new(BoundedQueue::new(1));
        queue.push(1);
        let q2 = queue.clone();
        let handle = std::thread::spawn(move || q2.push(2));
        std::thread::sleep(std::time::Duration::from_millis(50));
        queue.close();
        let result = handle.join().unwrap();
        assert!(!result); // push should return false after close
    }

    fn one_red_packed_frame() -> RawFrame {
        RawFrame::Packed {
            data: [65535u16, 0, 0]
                .iter()
                .flat_map(|c| c.to_be_bytes())
                .collect(),
            order: SampleOrder::Big,
            width: 1,
            height: 1,
            precision: 16,
            index: 0,
        }
    }

    #[test]
    fn a_frame_transform_converts_packed_frames_and_refuses_the_rest() {
        let transform = crate::colour::DcdmTransform::to_xyz(crate::colour::ColourSpace::P3)
            .expect("P3 transform");
        let transform = Arc::new(crate::colour::FrameColourTransform::ToXyz(transform));
        let prep = SourcePreparation {
            subtitle_burn: None,
            colour_transform: Some(Arc::clone(&transform)),
        };
        let mut packed = one_red_packed_frame();
        prep.apply(&mut packed, false).unwrap();
        let RawFrame::Packed { data, .. } = &packed else {
            panic!("frame changed shape");
        };
        let want = transform.pixel([65535, 0, 0], u16::MAX);
        assert_eq!(u16::from_be_bytes([data[0], data[1]]), want[0]);

        // converting twice would be silently wrong colour, so it fails instead
        assert!(prep.apply(&mut packed, true).is_err());

        let mut planar = RawFrame::Planar {
            components: [vec![0i32], vec![0], vec![0]],
            width: 1,
            height: 1,
            precision: 16,
            index: 0,
        };
        assert!(prep.apply(&mut planar, false).is_err());
    }

    #[test]
    fn empty_source_preparation_leaves_the_frame_alone() {
        let prep = SourcePreparation::default();
        let mut packed = one_red_packed_frame();
        prep.apply(&mut packed, true).unwrap();
        let RawFrame::Packed { data, .. } = &packed else {
            panic!("frame changed shape");
        };
        assert_eq!(u16::from_be_bytes([data[0], data[1]]), 65535);
    }

    #[test]
    fn a_burn_lands_before_the_colour_transform() {
        use crate::subtitle_formats::{StyledCue, StyledRun, VAlign};
        use crate::subtitle_raster::{BurnStyle, SubtitleBurn};

        // A bitmap cue covering the whole 1x1 frame with opaque red, so the
        // burn result is known without a font.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("cue.png");
        write_red_png(&png);
        let mut cue = StyledCue::text(0, 1000, vec![StyledRun::plain("")]);
        cue.runs.clear();
        cue.image = Some(png);
        cue.valign = Some(VAlign::Top);
        cue.vposition = Some(0.0);
        let burn = SubtitleBurn::new(vec![cue], None, BurnStyle::default(), 24.0).unwrap();

        let transform = crate::colour::DcdmTransform::to_xyz(crate::colour::ColourSpace::P3)
            .expect("P3 transform");
        let transform = Arc::new(crate::colour::FrameColourTransform::ToXyz(transform));
        let prep = SourcePreparation {
            subtitle_burn: Some(Arc::new(burn)),
            colour_transform: Some(Arc::clone(&transform)),
        };
        let mut frame = RawFrame::Packed {
            data: vec![0u8; 6],
            order: SampleOrder::Big,
            width: 1,
            height: 1,
            precision: 16,
            index: 0,
        };
        prep.apply(&mut frame, false).unwrap();
        let RawFrame::Packed { data, .. } = &frame else {
            panic!("frame changed shape");
        };
        // Burnt red, then converted: the reverse order would give the red
        // straight through, unconverted.
        let want = transform.pixel([65535, 0, 0], u16::MAX);
        assert_eq!(u16::from_be_bytes([data[0], data[1]]), want[0]);
        assert_eq!(u16::from_be_bytes([data[2], data[3]]), want[1]);
        assert_eq!(u16::from_be_bytes([data[4], data[5]]), want[2]);
        assert_ne!(
            want,
            [65535, 0, 0],
            "P3 red must move, or the test proves nothing"
        );

        // A burn alone is fine alongside the compressor's own transform: the
        // text is composited in display RGB and grok converts it with the rest.
        let burn_only = SourcePreparation {
            subtitle_burn: prep.subtitle_burn.clone(),
            colour_transform: None,
        };
        let mut frame = RawFrame::Packed {
            data: vec![0u8; 6],
            order: SampleOrder::Big,
            width: 1,
            height: 1,
            precision: 16,
            index: 0,
        };
        burn_only.apply(&mut frame, true).unwrap();
        let RawFrame::Packed { data, .. } = &frame else {
            panic!("frame changed shape");
        };
        assert_eq!(u16::from_be_bytes([data[0], data[1]]), 65535);
    }

    fn write_red_png(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[255, 0, 0, 255])
            .unwrap();
    }

    #[test]
    fn test_compress_params_default() {
        let params = CompressParams::default();
        assert_eq!(params.compression_ratio, 10.0);
        assert_eq!(params.num_resolutions, 6);
        assert_eq!(params.codeblock_size, 32);
        assert_eq!(params.profile, 0x0003);
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn test_single_frame_compress_speed() {
        // Load a real TIFF frame and benchmark single-frame compress
        let frame_path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("dom_distribution/sintel_frames/frame_000001.tiff");
        if !frame_path.exists() {
            eprintln!("Skipping: no test frame at {}", frame_path.display());
            return;
        }

        let tf = crate::grok::load_tiff(&frame_path).expect("load_tiff");
        let frame = RawFrame::Planar {
            components: tf.components,
            width: tf.width,
            height: tf.height,
            precision: tf.precision,
            index: 0,
        };

        initialize(0);
        let params = CompressParams::default();
        let mut output_buf = vec![0u8; (frame.width() as usize) * (frame.height() as usize) * 6];

        let start = std::time::Instant::now();
        let n = 10;
        for _ in 0..n {
            let result = compress_frame_grok(&frame, &params, &mut output_buf);
            assert!(result.is_ok(), "compress failed: {:?}", result.err());
        }
        let elapsed = start.elapsed();
        let fps = n as f64 / elapsed.as_secs_f64();
        eprintln!(
            "Single-thread: {} frames in {:.2}s = {:.1} fps ({:.0}ms/frame)",
            n,
            elapsed.as_secs_f64(),
            fps,
            elapsed.as_secs_f64() / n as f64 * 1000.0
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn cinema_profile_survives_16_bit_input() {
        // cinema profiles require 12-bit samples, grok reduces deeper input
        // itself (needs grok >= 20.3.10)
        let (w, h) = (128u32, 128u32);
        let frame = RawFrame::Packed {
            data: vec![0x80u8; (w * h * 6) as usize],
            order: SampleOrder::Big,
            width: w,
            height: h,
            precision: 16,
            index: 0,
        };
        initialize(0);
        let mut buf = Vec::new();
        let bytes = compress_frame_grok(&frame, &CompressParams::default(), &mut buf).unwrap();
        // SOC ff4f, SIZ ff51, Lsiz u16, then Rsiz u16
        assert_eq!(&bytes[..4], &[0xff, 0x4f, 0xff, 0x51]);
        let rsiz = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert_eq!(rsiz, 0x0003, "cinema 2k profile was stripped");
    }

    #[cfg(feature = "grok-ffi")]
    fn grey_frame(width: u32, height: u32) -> RawFrame {
        RawFrame::Packed {
            data: vec![0x80u8; (width * height * 6) as usize],
            order: SampleOrder::Big,
            width,
            height,
            precision: 16,
            index: 0,
        }
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_4k_frame_is_written_under_the_cinema_4k_profile() {
        // grok refuses a 4K frame under the 2K profile
        initialize(0);
        let mut buf = Vec::new();
        let bytes = compress_frame_grok(
            &grey_frame(4096, 2160),
            &CompressParams::default(),
            &mut buf,
        )
        .expect("a 4K frame compresses under the default profile");
        let rsiz = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert_eq!(rsiz, crate::j2k::CINEMA_4K_RSIZ);
        // the decomposition level count is 9 bytes past the COD marker
        let cod = bytes
            .windows(2)
            .position(|w| w == [0xff, 0x52])
            .expect("COD marker");
        let decomposition_levels = bytes[cod + 9];
        assert_eq!(
            decomposition_levels,
            crate::j2k::CINEMA_4K_RESOLUTIONS - 1,
            "DCI 4K carries six decomposition levels"
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_4k_codestream_decodes_back_at_full_resolution() {
        initialize(0);
        let mut buf = Vec::new();
        let bytes = compress_frame_grok(
            &grey_frame(4096, 2160),
            &CompressParams::default(),
            &mut buf,
        )
        .expect("a 4K frame compresses");
        let decoded =
            crate::grok_decoder::decode(bytes.to_vec(), 0).expect("a 4K codestream decodes");
        assert_eq!((decoded.width, decoded.height), (4096, 2160));
        assert_eq!(decoded.components.len(), 3);
        assert_eq!(decoded.components[0].len(), 4096 * 2160);
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_frame_past_4k_is_refused_by_name() {
        initialize(0);
        let mut buf = Vec::new();
        let error = compress_frame_grok(
            &grey_frame(4097, 2160),
            &CompressParams::default(),
            &mut buf,
        )
        .expect_err("no cinema profile holds a frame wider than 4096");
        assert!(error.contains("4097x2160"), "{error}");
    }

    // 230 Mbit/s at 24 fps, the shipped default, in bytes per frame
    #[cfg(feature = "grok-ffi")]
    const DEFAULT_TARGET_BYTES: u64 = 1_197_917;

    #[cfg(feature = "grok-ffi")]
    const FEATURE_FPS: u32 = 24;

    // below this a rate allocation is spending fewer bits than it was asked for
    #[cfg(feature = "grok-ffi")]
    const TARGET_FLOOR: f64 = 0.97;

    #[cfg(feature = "grok-ffi")]
    fn compress_one_2k_frame_at_the_target(precision: u8, target: u64) -> u64 {
        let frame = noise_frame(0, 2048, 1080, precision);
        initialize(0);
        let params = CompressParams {
            target_codestream_bytes: Some(target),
            edit_rate: crate::encode::FrameRate::whole(FEATURE_FPS),
            ..CompressParams::default()
        };
        let mut buf = Vec::new();
        compress_frame_grok(&frame, &params, &mut buf)
            .unwrap()
            .len() as u64
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_default_bitrate_target_stays_under_the_dci_cap() {
        // noise is incompressible enough to hit the rate ceiling on every frame
        let bytes = compress_one_2k_frame_at_the_target(16, DEFAULT_TARGET_BYTES);
        let cap = crate::j2k::dci_codestream_byte_cap(FEATURE_FPS);
        assert!(bytes <= cap, "{bytes} bytes exceeds the {cap} byte DCI cap");
        assert!(
            bytes <= DEFAULT_TARGET_BYTES,
            "{bytes} bytes exceeds the {DEFAULT_TARGET_BYTES} byte target"
        );
        let reached = bytes as f64 / DEFAULT_TARGET_BYTES as f64;
        assert!(
            reached >= TARGET_FLOOR,
            "{bytes} bytes is only {reached} of the {DEFAULT_TARGET_BYTES} byte target"
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_12_bit_frame_hits_the_same_target_as_a_16_bit_one() {
        // grok cuts a deeper cinema frame to 12 bits itself, so the target has
        // to be measured against the image it ends up with either way
        let at_16 = compress_one_2k_frame_at_the_target(16, DEFAULT_TARGET_BYTES);
        let at_12 = compress_one_2k_frame_at_the_target(12, DEFAULT_TARGET_BYTES);
        let difference = (at_12 as f64 - at_16 as f64).abs() / at_16 as f64;
        assert!(
            difference < 0.02,
            "12-bit landed at {at_12} bytes and 16-bit at {at_16}"
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn overlapping_pipelines_share_the_inline_pool() {
        // pipelines resize grok's global pool; unguarded, one pipeline's exit
        // destroys the executor another's codecs are running on (segfault).
        let dir = tempfile::tempdir().unwrap();
        initialize(0);
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let out = dir.path().join(format!("p{i}"));
                std::thread::spawn(move || {
                    let (w, h) = (128u32, 128u32);
                    let cancel = Arc::new(AtomicBool::new(false));
                    let mut left = 3u64;
                    let phase_clocks = Arc::new(PhaseClocks::default());
                    let result = encode_pipeline(
                        &out,
                        &CompressParams::default(),
                        3,
                        &cancel,
                        &phase_clocks,
                        || {
                            if left == 0 {
                                return None;
                            }
                            left -= 1;
                            Some(RawFrame::Packed {
                                data: vec![0x80u8; (w * h * 6) as usize],
                                order: SampleOrder::Big,
                                width: w,
                                height: h,
                                precision: 16,
                                index: 3 - left - 1,
                            })
                        },
                        |_| {},
                    );
                    assert!(result.success, "pipeline {i} failed: {}", result.error);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// The producer blocks pushing into the full queue while the encoder
    /// threads compress, and a cancel makes those threads leave without
    /// draining it.
    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_cancel_releases_a_producer_blocked_on_the_full_queue() {
        // well past the queue capacity plus one frame per encoder thread
        const CANCEL_AT_FRAME: u64 = 200;
        const RETURN_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);
        const FRAME_BYTES: usize = 2048 * 1080 * 6;
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let produced = Arc::new(AtomicU64::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        initialize(0);
        let output_dir = dir.path().to_path_buf();
        let pipeline_cancel = cancel.clone();
        let pipeline_produced = produced.clone();
        std::thread::spawn(move || {
            let phase_clocks = Arc::new(PhaseClocks::default());
            let result = encode_pipeline(
                &output_dir,
                &CompressParams::default(),
                u64::MAX,
                &pipeline_cancel,
                &phase_clocks,
                || {
                    let index = pipeline_produced.fetch_add(1, Ordering::Relaxed);
                    if index == CANCEL_AT_FRAME {
                        pipeline_cancel.store(true, Ordering::Relaxed);
                    }
                    Some(RawFrame::Packed {
                        data: vec![0u8; FRAME_BYTES],
                        order: SampleOrder::Big,
                        width: 2048,
                        height: 1080,
                        precision: 16,
                        index,
                    })
                },
                |_| {},
            );
            let _ = done_tx.send(result);
        });
        let result = done_rx
            .recv_timeout(RETURN_WITHIN)
            .expect("the pipeline never returned after the cancel");
        assert_eq!(result.error, "Cancelled");
        assert!(
            produced.load(Ordering::Relaxed) > result.frames_encoded,
            "the queue held no frames when the cancel came, so nothing was blocked"
        );
    }

    /// One 128x128 frame of noise, incompressible enough that every frame lands
    /// at about the rate ceiling rather than a few header bytes.
    #[cfg(feature = "grok-ffi")]
    fn noise_frame(index: u64, width: u32, height: u32, precision: u8) -> RawFrame {
        let mut state = 7u32.wrapping_add(index as u32).wrapping_mul(2654435761);
        let mask = (1u32 << precision) - 1;
        let mut data = Vec::with_capacity((width * height * 6) as usize);
        for _ in 0..(width * height * 3) {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            data.extend_from_slice(&(((state >> 8) & mask) as u16).to_be_bytes());
        }
        RawFrame::Packed {
            data,
            order: SampleOrder::Big,
            width,
            height,
            precision,
            index,
        }
    }

    #[cfg(feature = "grok-ffi")]
    fn encode_noise_frames(output_dir: &Path, total: u64, cap: Option<u64>) -> PipelineResult {
        encode_noise_frames_at_the_target(output_dir, total, cap, None)
    }

    #[cfg(feature = "grok-ffi")]
    fn encode_noise_frames_at_the_target(
        output_dir: &Path,
        total: u64,
        cap: Option<u64>,
        target: Option<u64>,
    ) -> PipelineResult {
        let (width, height) = (128u32, 128u32);
        let cancel = Arc::new(AtomicBool::new(false));
        let phase_clocks = Arc::new(PhaseClocks::default());
        let mut next = 0u64;
        initialize(0);
        encode_pipeline_with_mxf_feed(
            output_dir,
            &CompressParams {
                codestream_byte_cap: cap,
                target_codestream_bytes: target,
                ..CompressParams::default()
            },
            total,
            &cancel,
            &phase_clocks,
            None,
            cap,
            || {
                if next == total {
                    return None;
                }
                let frame = noise_frame(next, width, height, 16);
                next += 1;
                Some(frame)
            },
            |_| {},
        )
    }

    #[cfg(feature = "grok-ffi")]
    fn largest_codestream(dir: &Path) -> u64 {
        written_codestreams(dir)
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .max()
            .unwrap()
    }

    #[cfg(feature = "grok-ffi")]
    fn written_codestreams(dir: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "j2c"))
            .collect();
        paths.sort();
        paths
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_cap_holds_every_codestream() {
        const TOTAL: u64 = 64;
        let dir = tempfile::tempdir().unwrap();

        // encode one frame first, so the cap is below what these frames
        // compress to by construction instead of by a guessed number
        let reference = dir.path().join("reference");
        let result = encode_noise_frames(&reference, 1, None);
        assert!(result.success, "reference encode failed: {}", result.error);
        let one_frame = largest_codestream(&reference);
        let cap = one_frame / 2;
        assert!(
            cap > 0,
            "a 128x128 noise frame compressed to {one_frame} bytes"
        );

        let capped = dir.path().join("capped");
        let result = encode_noise_frames(&capped, TOTAL, Some(cap));
        assert!(result.success, "capped encode failed: {}", result.error);
        assert_eq!(written_codestreams(&capped).len() as u64, TOTAL);
        let largest = largest_codestream(&capped);
        assert!(
            largest <= cap,
            "a codestream reached {largest} over the {cap} byte cap"
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_target_over_the_cap_still_holds_the_cap() {
        const TOTAL: u64 = 8;
        let dir = tempfile::tempdir().unwrap();

        let reference = dir.path().join("reference");
        let result = encode_noise_frames(&reference, 1, None);
        assert!(result.success, "reference encode failed: {}", result.error);
        let cap = largest_codestream(&reference) / 2;

        let capped = dir.path().join("capped");
        let result = encode_noise_frames_at_the_target(&capped, TOTAL, Some(cap), Some(cap * 4));
        assert!(result.success, "capped encode failed: {}", result.error);
        assert_eq!(written_codestreams(&capped).len() as u64, TOTAL);
        let largest = largest_codestream(&capped);
        assert!(
            largest <= cap,
            "a codestream reached {largest} over the {cap} byte cap, with a {} byte target",
            cap * 4
        );
    }

    // a codestream's own markers already run past this
    #[cfg(feature = "grok-ffi")]
    const UNMEETABLE_CAP: u64 = 100;

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_cap_grok_cannot_meet_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("capped");
        let result = encode_noise_frames(&output, 8, Some(UNMEETABLE_CAP));
        assert!(!result.success, "a 100 byte cap has to fail the encode");
        // grok emits over a cap it cannot reach, so the writer's own guard is
        // what stops the run
        assert!(
            result.error.contains(&format!(
                "over the {UNMEETABLE_CAP} byte per-frame cap: lower the bitrate"
            )),
            "wrong refusal: {}",
            result.error
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_generous_cap_encodes_the_same_frames_as_no_cap() {
        const TOTAL: u64 = 8;
        let dir = tempfile::tempdir().unwrap();

        let uncapped = dir.path().join("uncapped");
        let result = encode_noise_frames(&uncapped, TOTAL, None);
        assert!(result.success, "uncapped encode failed: {}", result.error);
        let uncapped_frames = written_codestreams(&uncapped);
        assert_eq!(uncapped_frames.len() as u64, TOTAL);

        let largest = uncapped_frames
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .max()
            .unwrap();

        let capped = dir.path().join("capped");
        let result = encode_noise_frames(&capped, TOTAL, Some(largest * 2));
        assert!(result.success, "capped encode failed: {}", result.error);
        assert_eq!(result.frames_encoded, TOTAL);
        let capped_frames = written_codestreams(&capped);
        assert_eq!(capped_frames.len() as u64, TOTAL);
        for (with_cap, without) in capped_frames.iter().zip(&uncapped_frames) {
            assert_eq!(
                std::fs::read(with_cap).unwrap(),
                std::fs::read(without).unwrap(),
                "{} differs from {}",
                with_cap.display(),
                without.display()
            );
        }
    }

    #[cfg(not(feature = "grok-ffi"))]
    #[test]
    fn test_compress_requires_grok_ffi() {
        let frame = RawFrame::Packed {
            data: vec![0; 6],
            order: SampleOrder::Big,
            width: 1,
            height: 1,
            precision: 16,
            index: 0,
        };
        let mut output = Vec::new();

        let error = compress_frame_grok(&frame, &CompressParams::default(), &mut output)
            .expect_err("compression should require grok-ffi");
        assert!(error.contains("grok-ffi feature not enabled"));
    }
}

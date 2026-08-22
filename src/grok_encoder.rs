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
    /// Packed interleaved rgb48be bytes (6 bytes per pixel, big-endian)
    Packed {
        data: Vec<u8>,
        width: u32,
        height: u32,
        precision: u8,
        index: u64,
    },
}

impl RawFrame {
    pub fn index(&self) -> u64 {
        match self {
            RawFrame::Planar { index, .. } | RawFrame::Packed { index, .. } => *index,
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            RawFrame::Planar { width, .. } | RawFrame::Packed { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            RawFrame::Planar { height, .. } | RawFrame::Packed { height, .. } => *height,
        }
    }

    pub fn precision(&self) -> u8 {
        match self {
            RawFrame::Planar { precision, .. } | RawFrame::Packed { precision, .. } => *precision,
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
    /// Compression ratio (e.g. 10.0 for 10:1)
    pub compression_ratio: f64,
    /// Number of decomposition levels (default 6 for 2K)
    pub num_resolutions: u8,
    /// Codeblock size (DCI requires 32×32)
    pub codeblock_size: u32,
    /// Progression order: CPRL for DCI
    pub progression: ProgressionOrder,
    /// Number of quality layers
    pub num_layers: u16,
    /// RSIZ profile (0x0003 = Cinema 2K, 0x0004 = Cinema 4K)
    pub profile: u16,
    /// Guard bits
    pub num_guard_bits: u8,
    /// Frame rate (used for rate control)
    pub frame_rate: u16,
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
    /// compressor's own transform does not model (P3, Rec.2020). Setting it
    /// together with `apply_xyz_transform` converts the frame twice and is
    /// refused.
    pub colour_transform: Option<Arc<crate::colour::DcdmTransform>>,
}

impl SourcePreparation {
    pub fn is_empty(&self) -> bool {
        self.subtitle_burn.is_none() && self.colour_transform.is_none()
    }

    /// Burn subtitles in, then convert the colour. Both steps need a packed
    /// 16-bit rgb48be frame.
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
        let (data, width, height) = match frame {
            RawFrame::Packed {
                data,
                width,
                height,
                precision: 16,
                ..
            } => (data, *width, *height),
            _ => {
                return Err(
                    "a subtitle burn or a source colour transform needs a packed 16-bit \
                     rgb48be frame"
                        .to_string(),
                );
            }
        };
        if let Some(burn) = &self.subtitle_burn {
            burn.burn_rgb48(
                data,
                width,
                height,
                crate::subtitle_raster::SampleOrder::Big,
                index,
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(transform) = &self.colour_transform {
            transform.frame_rgb48be_inplace(data);
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

impl Default for CompressParams {
    fn default() -> Self {
        Self {
            compression_ratio: 10.0,
            num_resolutions: 6,
            codeblock_size: 32,
            progression: ProgressionOrder::Cprl,
            num_layers: 1,
            profile: 0x0003, // GRK_PROFILE_CINEMA_2K
            num_guard_bits: 1,
            frame_rate: 24,
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

    // Writer channel (unbounded — disk I/O should keep up with encoding)
    let (writer_tx, writer_rx) = std::sync::mpsc::channel::<EncodedFrame>();

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
    std::thread::scope(|s| {
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
        loop {
            if cancel.load(Ordering::Relaxed) || error_flag.load(Ordering::Relaxed) {
                break;
            }

            match frame_producer() {
                Some(frame) => {
                    if !input_queue.push(frame) {
                        break;
                    }
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

        // Report progress while waiting for encoders to finish
        loop {
            let done = frames_encoded.load(Ordering::Relaxed);
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(EncodeProgress::new(
                done,
                total_frames,
                elapsed,
                phase_clocks,
            ));

            if done >= total_frames
                || error_flag.load(Ordering::Relaxed)
                || cancel.load(Ordering::Relaxed)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // Scoped threads join here
        drop(encoder_handles);
    });

    // Wait for writer to flush
    let _ = writer_handle.join();

    if cancel.load(Ordering::Relaxed) {
        return PipelineResult {
            success: false,
            error: "Cancelled".to_string(),
            frames_encoded: frames_encoded.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
        };
    }

    let err = first_error.lock().unwrap();
    if !err.is_empty() {
        return PipelineResult {
            success: false,
            error: err.clone(),
            frames_encoded: frames_encoded.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
        };
    }

    PipelineResult {
        success: true,
        error: String::new(),
        frames_encoded: frames_encoded.load(Ordering::Relaxed),
        output_dir: output_dir.to_path_buf(),
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
fn encoder_thread_fn(
    input_queue: &BoundedQueue<RawFrame>,
    writer_tx: &std::sync::mpsc::Sender<EncodedFrame>,
    error_flag: &AtomicBool,
    first_error: &Mutex<String>,
    cancel: &AtomicBool,
    params: &CompressParams,
    phase_clocks: &PhaseClocks,
) {
    // Pre-allocate output buffer once per thread and reuse across frames
    let buf_size = 2048 * 1080 * 3 * 2; // max 2K frame uncompressed size
    let mut output_buf = vec![0u8; buf_size];

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
        let compressed = compress_frame_grok(&frame, params, &mut output_buf);
        phase_clocks.add(EncodePhase::Jpeg2000, encode_start.elapsed());

        match compressed {
            Ok(data) => {
                let encoded = EncodedFrame {
                    data,
                    index: frame.index(),
                };
                if writer_tx.send(encoded).is_err() {
                    break;
                }
            }
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
}

// ─── Grok FFI compression ──────────────────────────────────────────────────

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
    use grokj2k_sys::*;
    use std::ptr;

    let width = frame.width();
    let height = frame.height();
    // grok reduces deeper samples to the 12 bits cinema profiles require,
    // fused with its X'Y'Z' transform, so frames pass through at full precision
    let precision = frame.precision();

    // Ensure buffer is large enough for this frame
    let needed = (width as usize) * (height as usize) * 3 * 2;
    if output_buf.len() < needed {
        output_buf.resize(needed, 0);
    }

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

        let image = grk_image_new(
            3,
            comps.as_mut_ptr(),
            _GRK_COLOR_SPACE_GRK_CLRSPC_SYCC,
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
                        ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, w);
                    }
                }
            }
            RawFrame::Packed { data, .. } => {
                // Deinterleave rgb48be directly into Grok component buffers
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

                for y in 0..h {
                    let row_offset = y * stride;
                    let src_row_offset = y * w * 6;
                    for x in 0..w {
                        let off = src_row_offset + x * 6;
                        let r = ((data[off] as i32) << 8) | (data[off + 1] as i32);
                        let g = ((data[off + 2] as i32) << 8) | (data[off + 3] as i32);
                        let b = ((data[off + 4] as i32) << 8) | (data[off + 5] as i32);
                        *r_data.add(row_offset + x) = r;
                        *g_data.add(row_offset + x) = g;
                        *b_data.add(row_offset + x) = b;
                    }
                }
            }
        }

        // Set up compression parameters
        let mut cparams: grk_cparameters = std::mem::zeroed();
        grk_compress_set_default_params(&mut cparams);

        cparams.cod_format = _GRK_SUPPORTED_FILE_FMT_GRK_FMT_J2K;
        cparams.numlayers = params.num_layers;
        cparams.allocation_by_rate_distortion = true;
        cparams.layer_rate[0] = params.compression_ratio;
        cparams.numresolution = params.num_resolutions;
        cparams.cblockw_init = params.codeblock_size;
        cparams.cblockh_init = params.codeblock_size;
        cparams.irreversible = params.irreversible;
        cparams.mct = if params.mct { 1 } else { 0 };
        cparams.rsiz = params.profile;
        cparams.numgbits = params.num_guard_bits;
        cparams.framerate = params.frame_rate;
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

/// Fallback: when grok-ffi feature is not enabled, use subprocess
#[cfg(not(feature = "grok-ffi"))]
fn compress_frame_grok(
    _frame: &RawFrame,
    _params: &CompressParams,
    _output_buf: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    Err("grok-ffi feature not enabled — cannot use in-process encoder".to_string())
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
        cancel,
        false,
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

/// Like [`encode_video_pipeline`], but when `resume` is true it skips frames
/// already encoded on disk (dom#344: an interrupted encode picks up where it
/// left off). The already-present contiguous prefix is decoded-and-discarded so
/// ffmpeg stays frame-aligned, then encoding continues from the next index. The
/// last existing frame is always re-encoded in case it was truncated by the
/// interruption.
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_resumable<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
    resume: bool,
    // ffmpeg -vf chain applied while decoding, for fades and the like. It must
    // not change the frame size or count: the reader slices stdout into fixed
    // width*height frames and the CPL already declares the count.
    video_filter: Option<&str>,
    mut on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    use std::io::Read;
    use std::process::{Command, Stdio};

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PipelineResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
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
        };
    }

    // Launch ffmpeg to decode video → raw rgb48be frames on stdout
    let mut command = Command::new("ffmpeg");
    command.arg("-y").arg("-i").arg(input_video);
    if let Some(filter) = video_filter {
        command.arg("-vf").arg(filter);
    }
    let mut child = match command
        .arg("-pix_fmt")
        .arg("rgb48be")
        .arg("-f")
        .arg("rawvideo")
        .arg("pipe:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return PipelineResult {
                success: false,
                error: format!("Failed to spawn ffmpeg: {e}"),
                frames_encoded: 0,
                output_dir: output_dir.to_path_buf(),
            };
        }
    };

    let frame_size = (width as usize) * (height as usize) * 6; // rgb48be = 6 bytes/pixel
    let mut stdout = child.stdout.take().unwrap();

    // discard the already-encoded prefix so ffmpeg stays frame-aligned.
    let mut frame_index: u64 = 0;
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
            };
        }
    }

    // progress total is the remaining frames so the pipeline's completion check
    // (done >= total) is reachable after a resume.
    let remaining_total = total_frames.saturating_sub(start_frame);
    let phase_clocks = Arc::new(PhaseClocks::default());
    let result = encode_pipeline(
        output_dir,
        params,
        remaining_total,
        cancel,
        &phase_clocks,
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
                    Some(RawFrame::Packed {
                        data: buf,
                        width,
                        height,
                        precision: 16,
                        index: idx,
                    })
                }
                Err(_) => None, // EOF or error — no more frames
            }
        },
        &mut on_progress,
    );

    // Clean up ffmpeg
    let _ = child.kill();
    let _ = child.wait();

    result
}

// ─── Subprocess-based encoder pipeline ─────────────────────────────────────────

/// Encode frames using parallel `grk_compress` subprocesses.
///
/// Each subprocess gets its own independent Grok thread pool, avoiding the
/// shared-pool bottleneck of the FFI approach. Frames are written as raw data
/// to a ramdisk (/dev/shm), compressed by grk_compress, and the resulting
/// .j2c files written to the output directory.
///
/// `input` provides raw frames (rgb48be) as a contiguous byte stream. Each frame
/// is `frame_size` bytes. The producer reads directly from the stream to /dev/shm,
/// avoiding intermediate buffer clones.
///
/// Of the phase clocks, only the decoder wait and the JPEG 2000 time are filled
/// in: this path prepares nothing, and grk_compress writes the codestream inside
/// the child, where nothing here can time it. The JPEG 2000 clock is the whole
/// grk_compress run, summed over the workers, so it also covers the child's own
/// read and write.
#[allow(clippy::too_many_arguments)]
pub fn encode_pipeline_subprocess<P>(
    output_dir: &Path,
    params: &CompressParams,
    grk_compress_bin: &Path,
    total_frames: u64,
    width: u32,
    height: u32,
    frame_size: usize,
    input: &mut dyn std::io::Read,
    cancel: &Arc<AtomicBool>,
    mut on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    use std::process::{Command, Stdio};

    if !params.source_preparation.is_empty() {
        return PipelineResult {
            success: false,
            error: "the subprocess encoder writes raw frames straight to grk_compress and \
                    cannot burn subtitles or run a source colour transform: use the \
                    in-process pipeline"
                .to_string(),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
        };
    }

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PipelineResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
        };
    }

    let total_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(16);
    // Use half as many workers as CPUs, with 2 threads each.
    // This keeps memory manageable while ensuring good parallelism.
    let num_workers = total_cpus / 2;
    let threads_per_worker = 2;

    // Use /dev/shm (ramdisk) for temporary frame I/O to avoid disk bottleneck
    let tmp_dir = PathBuf::from("/dev/shm/grok_encode_tmp");
    if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
        return PipelineResult {
            success: false,
            error: format!("Failed to create tmp dir: {e}"),
            frames_encoded: 0,
            output_dir: output_dir.to_path_buf(),
        };
    }

    let frames_encoded = Arc::new(AtomicU64::new(0));
    let error_flag = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(String::new()));
    let encode_start = std::time::Instant::now();
    let phase_clocks = Arc::new(PhaseClocks::default());

    // Bounded queue for work items: (frame_index, input_path)
    let work_queue: Arc<BoundedQueue<(u64, PathBuf, u32, u32)>> =
        Arc::new(BoundedQueue::new(num_workers * 2));

    let output_dir_owned = output_dir.to_path_buf();
    let grk_bin = grk_compress_bin.to_path_buf();

    // Build CLI args from params
    let mut cinema_flag: Vec<String> = if params.profile == 0x0003 {
        vec![
            "-w".to_string(),
            params.frame_rate.to_string(),
            "-H".to_string(),
            threads_per_worker.to_string(),
        ]
    } else if params.profile == 0x0004 {
        vec![
            "-x".to_string(),
            "-H".to_string(),
            threads_per_worker.to_string(),
        ]
    } else {
        vec![
            "-r".to_string(),
            format!("{}", params.compression_ratio),
            "-b".to_string(),
            format!("{},{}", params.codeblock_size, params.codeblock_size),
            "-p".to_string(),
            "CPRL".to_string(),
        ]
    };
    if params.apply_xyz_transform {
        cinema_flag.push("--xyz".to_string());
    }

    // Worker threads: each picks a frame from the queue, spawns grk_compress
    std::thread::scope(|s| {
        let worker_handles: Vec<_> = (0..num_workers)
            .map(|_| {
                let work_queue = work_queue.clone();
                let error_flag = error_flag.clone();
                let first_error = first_error.clone();
                let cancel = cancel.clone();
                let frames_encoded = frames_encoded.clone();
                let output_dir = output_dir_owned.clone();
                let grk_bin = grk_bin.clone();
                let cinema_flag = cinema_flag.clone();
                let phase_clocks = phase_clocks.clone();

                s.spawn(move || {
                    while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
                        let Some((frame_idx, input_path, w, h)) = work_queue.pop() else {
                            break;
                        };

                        let output_path = output_dir.join(format!("frame_{:08}.j2c", frame_idx));
                        let raw_spec = format!("{w},{h},3,16,u");

                        let compress_start = std::time::Instant::now();
                        let status = Command::new(&grk_bin)
                            .arg("-i")
                            .arg(&input_path)
                            .arg("-F")
                            .arg(&raw_spec)
                            .arg("-o")
                            .arg(&output_path)
                            .args(&cinema_flag)
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                        phase_clocks.add(EncodePhase::Jpeg2000, compress_start.elapsed());

                        match status {
                            Ok(s) if s.success() => {
                                frames_encoded.fetch_add(1, Ordering::Relaxed);
                                let _ = std::fs::remove_file(&input_path);
                            }
                            Ok(s) => {
                                error_flag.store(true, Ordering::Relaxed);
                                let mut err = first_error.lock().unwrap();
                                if err.is_empty() {
                                    *err = format!(
                                        "grk_compress failed frame {frame_idx}: exit {}",
                                        s.code().unwrap_or(-1)
                                    );
                                }
                                break;
                            }
                            Err(e) => {
                                error_flag.store(true, Ordering::Relaxed);
                                let mut err = first_error.lock().unwrap();
                                if err.is_empty() {
                                    *err = format!("Failed to spawn grk_compress: {e}");
                                }
                                break;
                            }
                        }
                    }
                })
            })
            .collect();

        // Producer: read frames from input stream directly to /dev/shm files
        let mut frame_index: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024]; // 64KB transfer buffer
        while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
            let input_path = tmp_dir.join(format!("frame_{:08}.raw", frame_index));

            // Read exactly frame_size bytes from input, writing directly to file
            let mut file = match std::fs::File::create(&input_path) {
                Ok(f) => f,
                Err(e) => {
                    error_flag.store(true, Ordering::Relaxed);
                    let mut err = first_error.lock().unwrap();
                    if err.is_empty() {
                        *err = format!("Failed to create frame file: {e}");
                    }
                    break;
                }
            };
            let mut remaining = frame_size;
            let mut hit_eof = false;
            while remaining > 0 {
                let chunk = remaining.min(buf.len());
                let read_start = std::time::Instant::now();
                let read = input.read_exact(&mut buf[..chunk]);
                phase_clocks.add(EncodePhase::DecoderWait, read_start.elapsed());
                match read {
                    Ok(()) => {
                        use std::io::Write;
                        if let Err(e) = file.write_all(&buf[..chunk]) {
                            error_flag.store(true, Ordering::Relaxed);
                            let mut err = first_error.lock().unwrap();
                            if err.is_empty() {
                                *err = format!("Failed to write frame {frame_index}: {e}");
                            }
                            hit_eof = true;
                            break;
                        }
                        remaining -= chunk;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        hit_eof = true;
                        break;
                    }
                    Err(e) => {
                        error_flag.store(true, Ordering::Relaxed);
                        let mut err = first_error.lock().unwrap();
                        if err.is_empty() {
                            *err = format!("Read error frame {frame_index}: {e}");
                        }
                        hit_eof = true;
                        break;
                    }
                }
            }
            if hit_eof {
                let _ = std::fs::remove_file(&input_path);
                break;
            }
            drop(file);

            if !work_queue.push((frame_index, input_path, width, height)) {
                break;
            }
            frame_index += 1;

            // Progress reporting
            let encoded = frames_encoded.load(Ordering::Relaxed);
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(EncodeProgress::new(
                encoded,
                total_frames,
                elapsed,
                &phase_clocks,
            ));
        }

        work_queue.close();
        for h in worker_handles {
            let _ = h.join();
        }
    });

    let elapsed = encode_start.elapsed().as_secs_f64();
    let final_count = frames_encoded.load(Ordering::Relaxed);
    on_progress(EncodeProgress::new(
        final_count,
        total_frames,
        elapsed,
        &phase_clocks,
    ));

    let _ = std::fs::remove_dir_all(&tmp_dir);

    let err = first_error.lock().unwrap().clone();
    PipelineResult {
        success: err.is_empty() && final_count == total_frames,
        error: err,
        frames_encoded: final_count,
        output_dir: output_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let transform = Arc::new(transform);
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
        let transform = Arc::new(transform);
        let prep = SourcePreparation {
            subtitle_burn: Some(Arc::new(burn)),
            colour_transform: Some(Arc::clone(&transform)),
        };
        let mut frame = RawFrame::Packed {
            data: vec![0u8; 6],
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
    #[test]
    fn the_default_bitrate_target_stays_under_the_dci_cap() {
        // rate allocation lands a frame either side of the target, so the
        // shipped target needs room under the cap. noise is incompressible
        // enough to hit the rate ceiling on every frame.
        let (w, h) = (2048u32, 1080u32);
        let mut state = 7u32;
        let data: Vec<u8> = (0..(w * h * 6))
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        let frame = RawFrame::Packed {
            data,
            width: w,
            height: h,
            precision: 16,
            index: 0,
        };
        initialize(0);

        const DEFAULT_TARGET_MBPS: f64 = 230.0;
        const FPS: u32 = 24;
        let raw_bits = w as f64 * h as f64 * 36.0;
        let params = CompressParams {
            compression_ratio: raw_bits / (DEFAULT_TARGET_MBPS * 1_000_000.0 / FPS as f64),
            apply_xyz_transform: true,
            frame_rate: FPS as u16,
            ..CompressParams::default()
        };

        let mut buf = Vec::new();
        let bytes = compress_frame_grok(&frame, &params, &mut buf).unwrap();
        let cap = crate::j2k::dci_codestream_byte_cap(FPS);
        assert!(
            (bytes.len() as u64) <= cap,
            "{} bytes exceeds the {cap} byte DCI cap",
            bytes.len()
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

    /// One 128x128 frame of noise, incompressible enough that every frame lands
    /// at about the rate ceiling rather than a few header bytes.
    #[cfg(feature = "grok-ffi")]
    fn noise_frame(index: u64, width: u32, height: u32) -> RawFrame {
        let mut state = 7u32.wrapping_add(index as u32).wrapping_mul(2654435761);
        let data: Vec<u8> = (0..(width * height * 6))
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        RawFrame::Packed {
            data,
            width,
            height,
            precision: 16,
            index,
        }
    }

    #[cfg(feature = "grok-ffi")]
    fn encode_noise_frames(output_dir: &Path, total: u64, cap: Option<u64>) -> PipelineResult {
        let (width, height) = (128u32, 128u32);
        let cancel = Arc::new(AtomicBool::new(false));
        let phase_clocks = Arc::new(PhaseClocks::default());
        let mut next = 0u64;
        initialize(0);
        encode_pipeline_with_mxf_feed(
            output_dir,
            &CompressParams::default(),
            total,
            &cancel,
            &phase_clocks,
            None,
            cap,
            || {
                if next == total {
                    return None;
                }
                let frame = noise_frame(next, width, height);
                next += 1;
                Some(frame)
            },
            |_| {},
        )
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
    fn a_frame_over_the_cap_stops_the_encode_where_it_lands() {
        const TOTAL: u64 = 64;
        let dir = tempfile::tempdir().unwrap();

        // encode one frame first, so the cap is below what these frames
        // compress to by construction instead of by a guessed number
        let reference = dir.path().join("reference");
        let result = encode_noise_frames(&reference, 1, None);
        assert!(result.success, "reference encode failed: {}", result.error);
        let one_frame = std::fs::metadata(&written_codestreams(&reference)[0])
            .unwrap()
            .len();
        let cap = one_frame / 2;
        assert!(
            cap > 0,
            "a 128x128 noise frame compressed to {one_frame} bytes"
        );

        let capped = dir.path().join("capped");
        let result = encode_noise_frames(&capped, TOTAL, Some(cap));
        assert!(!result.success, "an over-cap frame has to fail the encode");
        assert!(
            result.error.contains(&format!(
                "over the {cap} byte per-frame cap: lower the bitrate"
            )),
            "wrong refusal: {}",
            result.error
        );
        let written = written_codestreams(&capped).len() as u64;
        assert!(
            written < TOTAL,
            "the encode wrote {written} of {TOTAL} frames instead of stopping at the first \
             frame over the cap"
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

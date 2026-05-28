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
    /// Threads per codec instance (set internally by pipeline)
    pub threads_per_codec: u32,
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
            threads_per_codec: 1,
        }
    }
}

/// Progress information from the encoder pipeline.
#[derive(Debug, Clone)]
pub struct EncodeProgress {
    pub frames_encoded: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
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
pub struct BoundedQueue<T> {
    items: Mutex<Vec<T>>,
    not_full: Condvar,
    not_empty: Condvar,
    capacity: usize,
    closed: AtomicBool,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(Vec::with_capacity(capacity)),
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
        queue.push(item);
        self.not_empty.notify_one();
        true
    }

    pub fn pop(&self) -> Option<T> {
        let mut queue = self.items.lock().unwrap();
        loop {
            if let Some(item) = queue.pop() {
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
pub fn encode_pipeline<F, P>(
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    cancel: &Arc<AtomicBool>,
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

    // Grok's CompressScheduler always uses the full TFSingleton pool regardless
    // of per-codec cparams.num_threads. With N encoder threads, N taskflows
    // compete on the shared pool. Empirically: 1 encoder→7.1fps, 16→8.9fps.
    // Try a middle ground to balance pipeline depth vs contention.
    let threads_per_codec = 1;
    let num_encoder_threads = num_threads.min(4);

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
    let writer_handle = std::thread::spawn(move || {
        for frame in writer_rx {
            let path = writer_output_dir.join(format!("frame_{:08}.j2c", frame.index));
            if let Err(e) = std::fs::write(&path, &frame.data) {
                writer_error_flag.store(true, Ordering::Relaxed);
                let mut err = writer_first_error.lock().unwrap();
                if err.is_empty() {
                    *err = format!("Write error frame {}: {e}", frame.index);
                }
                break;
            }
            writer_encoded_count.fetch_add(1, Ordering::Relaxed);
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

                s.spawn(move || {
                    encoder_thread_fn(
                        &input_queue,
                        &writer_tx,
                        &error_flag,
                        &first_error,
                        &cancel,
                        &params,
                    );
                })
            })
            .collect();

        // Drop our copy so writer sees hangup when all encoder threads finish
        drop(writer_tx);

        // Producer loop — feed frames into the bounded queue
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
        }

        // Signal encoder threads that no more frames are coming
        input_queue.close();

        // Report progress while waiting for encoders to finish
        loop {
            let done = frames_encoded.load(Ordering::Relaxed);
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(EncodeProgress {
                frames_encoded: done,
                total_frames,
                fps: if elapsed > 0.0 {
                    done as f64 / elapsed
                } else {
                    0.0
                },
                elapsed_secs: elapsed,
            });

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

/// Per-thread encoder function. Pops frames from the queue, compresses them
/// in-process via Grok FFI, and sends encoded data to the writer channel.
fn encoder_thread_fn(
    input_queue: &BoundedQueue<RawFrame>,
    writer_tx: &std::sync::mpsc::Sender<EncodedFrame>,
    error_flag: &AtomicBool,
    first_error: &Mutex<String>,
    cancel: &AtomicBool,
    params: &CompressParams,
) {
    // Pre-allocate output buffer once per thread and reuse across frames
    let buf_size = 2048 * 1080 * 3 * 2; // max 2K frame uncompressed size
    let mut output_buf = vec![0u8; buf_size];

    while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
        let Some(frame) = input_queue.pop() else {
            break;
        };

        match compress_frame_grok(&frame, params, &mut output_buf) {
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
                error_flag.store(true, Ordering::Relaxed);
                let mut err = first_error.lock().unwrap();
                if err.is_empty() {
                    *err = format!("Encode failed frame {}: {e}", frame.index());
                }
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

/// Shut down the Grok library.
#[cfg(feature = "grok-ffi")]
pub fn deinitialize() {
    unsafe {
        grokj2k_sys::grk_deinitialize();
    }
}

/// Stub when grok-ffi is not enabled.
#[cfg(not(feature = "grok-ffi"))]
pub fn deinitialize() {}

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

    // Launch ffmpeg to decode video → raw rgb48be frames on stdout
    let mut child = match Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input_video)
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

    // Feed decoded frames into the generic encode_pipeline
    let mut frame_index: u64 = 0;
    let result = encode_pipeline(
        output_dir,
        params,
        total_frames,
        cancel,
        || {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut buf = vec![0u8; frame_size];
            match stdout.read_exact(&mut buf) {
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

    // Bounded queue for work items: (frame_index, input_path)
    let work_queue: Arc<BoundedQueue<(u64, PathBuf, u32, u32)>> =
        Arc::new(BoundedQueue::new(num_workers * 2));

    let output_dir_owned = output_dir.to_path_buf();
    let grk_bin = grk_compress_bin.to_path_buf();

    // Build CLI args from params
    let cinema_flag: Vec<String> = if params.profile == 0x0003 {
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

                s.spawn(move || {
                    while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
                        let Some((frame_idx, input_path, w, h)) = work_queue.pop() else {
                            break;
                        };

                        let output_path = output_dir.join(format!("frame_{:08}.j2c", frame_idx));
                        let raw_spec = format!("{w},{h},3,16,u");

                        let status = Command::new(&grk_bin)
                            .arg("-i")
                            .arg(&input_path)
                            .arg("-F")
                            .arg(&raw_spec)
                            .arg("-o")
                            .arg(&output_path)
                            .args(&cinema_flag)
                            .arg("-quiet")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();

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
                match input.read_exact(&mut buf[..chunk]) {
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
            let fps = if elapsed > 0.0 {
                encoded as f64 / elapsed
            } else {
                0.0
            };
            on_progress(EncodeProgress {
                frames_encoded: encoded,
                total_frames,
                fps,
                elapsed_secs: elapsed,
            });
        }

        work_queue.close();
        for h in worker_handles {
            let _ = h.join();
        }
    });

    let elapsed = encode_start.elapsed().as_secs_f64();
    let final_count = frames_encoded.load(Ordering::Relaxed);
    let fps = if elapsed > 0.0 {
        final_count as f64 / elapsed
    } else {
        0.0
    };
    on_progress(EncodeProgress {
        frames_encoded: final_count,
        total_frames,
        fps,
        elapsed_secs: elapsed,
    });

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
    fn test_bounded_queue_basic() {
        let queue: Arc<BoundedQueue<u32>> = Arc::new(BoundedQueue::new(3));
        assert!(queue.push(1));
        assert!(queue.push(2));
        assert!(queue.push(3));
        assert_eq!(queue.pop(), Some(3)); // Vec::pop returns last
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

    #[test]
    fn test_compress_params_default() {
        let params = CompressParams::default();
        assert_eq!(params.compression_ratio, 10.0);
        assert_eq!(params.num_resolutions, 6);
        assert_eq!(params.codeblock_size, 32);
        assert_eq!(params.profile, 0x0003);
    }

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
        deinitialize();
    }
}

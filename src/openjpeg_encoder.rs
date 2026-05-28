//! JPEG 2000 encoding via OpenJPEG (single-threaded per codec, scales with N threads).
//!
//! Unlike Grok's shared thread pool, each OpenJPEG codec instance is fully
//! independent. This allows linear scaling with N encoder threads — the same
//! architecture dcpomatic uses to achieve ~26fps on 16 cores.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::grok_encoder::{
    BoundedQueue, CompressParams, EncodeProgress, EncodedFrame, PipelineResult, RawFrame,
};

/// Encode a sequence of raw frames using OpenJPEG with N independent threads.
///
/// Each encoder thread runs its own single-threaded OpenJPEG codec instance.
/// No shared state between threads → linear scaling.
pub fn encode_pipeline_opj<F, P>(
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

    // Each thread gets its own independent OpenJPEG codec — no shared pool.
    // Tested: 1.5x oversubscription is slower due to context switching.
    // 1:1 ratio with CPU count is optimal for this workload.
    let num_encoder_threads = num_threads;

    let queue_capacity = (num_encoder_threads * 2).clamp(4, 32);
    let input_queue: Arc<BoundedQueue<RawFrame>> = Arc::new(BoundedQueue::new(queue_capacity));

    let (writer_tx, writer_rx) = std::sync::mpsc::channel::<EncodedFrame>();

    let frames_encoded = Arc::new(AtomicU64::new(0));
    let error_flag = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(String::new()));

    let encode_start = std::time::Instant::now();

    // Writer thread
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
    let params = params.clone();
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
                    opj_encoder_thread(
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

        drop(writer_tx);

        // Producer loop
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

        input_queue.close();

        // Progress reporting
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

        drop(encoder_handles);
    });

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

/// Video-to-J2K pipeline using OpenJPEG (ffmpeg decode → N independent encode threads).
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_opj<P>(
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

    let frame_size = (width as usize) * (height as usize) * 6;
    let mut stdout = child.stdout.take().unwrap();

    let mut frame_index: u64 = 0;
    let result = encode_pipeline_opj(
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
                        precision: 12,
                        index: idx,
                    })
                }
                Err(_) => None,
            }
        },
        &mut on_progress,
    );

    let _ = child.kill();
    let _ = child.wait();

    result
}

// ─── Per-thread encoder ────────────────────────────────────────────────────────

fn opj_encoder_thread(
    input_queue: &BoundedQueue<RawFrame>,
    writer_tx: &std::sync::mpsc::Sender<EncodedFrame>,
    error_flag: &AtomicBool,
    first_error: &Mutex<String>,
    cancel: &AtomicBool,
    params: &CompressParams,
) {
    while !cancel.load(Ordering::Relaxed) && !error_flag.load(Ordering::Relaxed) {
        let Some(frame) = input_queue.pop() else {
            break;
        };

        match compress_frame_opj(&frame, params) {
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
                    *err = format!("OpenJPEG encode failed frame {}: {e}", frame.index());
                }
                break;
            }
        }
    }
}

// ─── OpenJPEG FFI compression ──────────────────────────────────────────────────

/// Thread-local write buffer for OpenJPEG stream callbacks.
struct WriteBuffer {
    data: Vec<u8>,
    offset: usize,
}

impl WriteBuffer {
    fn new() -> Self {
        // Pre-allocate ~1.5MB (typical DCI frame is ~1.3MB)
        Self {
            data: Vec::with_capacity(1_500_000),
            offset: 0,
        }
    }
}

unsafe extern "C" fn stream_write(
    buffer: *mut std::ffi::c_void,
    nb_bytes: usize,
    user_data: *mut std::ffi::c_void,
) -> usize {
    unsafe {
        let wb = &mut *(user_data as *mut WriteBuffer);
        let new_end = wb.offset + nb_bytes;
        if new_end > wb.data.len() {
            wb.data.resize(new_end, 0);
        }
        std::ptr::copy_nonoverlapping(
            buffer as *const u8,
            wb.data.as_mut_ptr().add(wb.offset),
            nb_bytes,
        );
        wb.offset = new_end;
        nb_bytes
    }
}

unsafe extern "C" fn stream_seek(nb_bytes: i64, user_data: *mut std::ffi::c_void) -> i32 {
    unsafe {
        let wb = &mut *(user_data as *mut WriteBuffer);
        wb.offset = nb_bytes as usize;
        1 // OPJ_TRUE
    }
}

unsafe extern "C" fn error_callback(
    msg: *const std::ffi::c_char,
    _client_data: *mut std::ffi::c_void,
) {
    unsafe {
        let c_str = std::ffi::CStr::from_ptr(msg);
        tracing::error!("OpenJPEG: {}", c_str.to_string_lossy());
    }
}

/// Compress a single frame using OpenJPEG. Each call is fully independent — no shared state.
#[cfg(feature = "openjpeg")]
fn compress_frame_opj(frame: &RawFrame, params: &CompressParams) -> Result<Vec<u8>, String> {
    use openjpeg_sys::*;
    use std::ptr;

    let width = frame.width();
    let height = frame.height();
    let precision = frame.precision() as u32;

    unsafe {
        // Create image component parameters
        let mut cmptparms: [opj_image_cmptparm_t; 3] = std::mem::zeroed();
        for comp in cmptparms.iter_mut() {
            comp.dx = 1;
            comp.dy = 1;
            comp.w = width;
            comp.h = height;
            comp.prec = precision;
            comp.sgnd = 0;
        }

        let image = opj_image_create(3, cmptparms.as_mut_ptr(), OPJ_COLOR_SPACE::OPJ_CLRSPC_SYCC);
        if image.is_null() {
            return Err("Failed to create OpenJPEG image".to_string());
        }

        // Set image dimensions
        (*image).x0 = 0;
        (*image).y0 = 0;
        (*image).x1 = width;
        (*image).y1 = height;

        // Fill component data
        let w = width as usize;
        let h = height as usize;
        let npixels = w * h;

        match frame {
            RawFrame::Planar { components, .. } => {
                for (compno, src) in components.iter().enumerate() {
                    let comp = &mut *(*image).comps.add(compno);
                    if comp.data.is_null() {
                        opj_image_destroy(image);
                        return Err(format!("Null component data for component {compno}"));
                    }
                    ptr::copy_nonoverlapping(src.as_ptr(), comp.data, npixels);
                }
            }
            RawFrame::Packed { data, .. } => {
                // Deinterleave rgb48be into planar i32 components
                for compno in 0..3usize {
                    let comp = &mut *(*image).comps.add(compno);
                    if comp.data.is_null() {
                        opj_image_destroy(image);
                        return Err("Null component data".to_string());
                    }
                }
                let r_data = (*(*image).comps.add(0)).data;
                let g_data = (*(*image).comps.add(1)).data;
                let b_data = (*(*image).comps.add(2)).data;

                // Downshift from 16-bit to target precision if needed
                let shift = 16 - precision;

                for i in 0..npixels {
                    let off = i * 6;
                    let r = (((data[off] as u32) << 8) | (data[off + 1] as u32)) >> shift;
                    let g = (((data[off + 2] as u32) << 8) | (data[off + 3] as u32)) >> shift;
                    let b = (((data[off + 4] as u32) << 8) | (data[off + 5] as u32)) >> shift;
                    *r_data.add(i) = r as i32;
                    *g_data.add(i) = g as i32;
                    *b_data.add(i) = b as i32;
                }
            }
        }

        // Create encoder
        let encoder = opj_create_compress(OPJ_CODEC_FORMAT::OPJ_CODEC_J2K);
        if encoder.is_null() {
            opj_image_destroy(image);
            return Err("Failed to create OpenJPEG encoder".to_string());
        }

        opj_set_error_handler(encoder, Some(error_callback), ptr::null_mut());

        // Set encoding parameters
        let mut cparams: opj_cparameters_t = std::mem::zeroed();
        opj_set_default_encoder_parameters(&mut cparams);

        cparams.numresolution = params.num_resolutions as i32;
        cparams.rsiz = params.profile;
        cparams.tcp_numlayers = params.num_layers as i32;
        cparams.tcp_mct = if params.mct { 1 } else { 0 };
        cparams.irreversible = if params.irreversible { 1 } else { 0 };
        cparams.cblockw_init = params.codeblock_size as i32;
        cparams.cblockh_init = params.codeblock_size as i32;
        cparams.cod_format = 0; // J2K

        // DCI rate control: max_cs_size = bandwidth / 8 / fps
        // DCI 2K bandwidth = 250 Mbps
        let bandwidth: u64 = 250_000_000;
        cparams.max_cs_size = (bandwidth / 8 / params.frame_rate as u64) as i32;
        cparams.max_comp_size = (cparams.max_cs_size as f64 / 1.25) as i32;

        cparams.prog_order = match params.progression {
            crate::grok_encoder::ProgressionOrder::Lrcp => OPJ_PROG_ORDER::OPJ_LRCP,
            crate::grok_encoder::ProgressionOrder::Rlcp => OPJ_PROG_ORDER::OPJ_RLCP,
            crate::grok_encoder::ProgressionOrder::Rpcl => OPJ_PROG_ORDER::OPJ_RPCL,
            crate::grok_encoder::ProgressionOrder::Pcrl => OPJ_PROG_ORDER::OPJ_PCRL,
            crate::grok_encoder::ProgressionOrder::Cprl => OPJ_PROG_ORDER::OPJ_CPRL,
        };

        // Comment (asdcplib requires non-empty)
        let comment = b"dcpwizard\0";
        cparams.cp_comment = comment.as_ptr() as *mut i8;

        // Set numgbits via extra options
        let numgbits_str = format!("GUARD_BITS={}\0", params.num_guard_bits);
        let opt_ptr = numgbits_str.as_ptr() as *const i8;
        let extra_options: [*const i8; 2] = [opt_ptr, ptr::null()];

        if opj_setup_encoder(encoder, &mut cparams, image) == 0 {
            opj_destroy_codec(encoder);
            opj_image_destroy(image);
            return Err("opj_setup_encoder failed".to_string());
        }

        // Set GUARD_BITS via extra options (available in OpenJPEG 2.5+)
        opj_encoder_set_extra_options(encoder, extra_options.as_ptr() as *mut *const i8);

        // Create output stream writing to a memory buffer
        let mut write_buf = Box::new(WriteBuffer::new());
        let stream = opj_stream_default_create(0); // OPJ_STREAM_WRITE
        if stream.is_null() {
            opj_destroy_codec(encoder);
            opj_image_destroy(image);
            return Err("Failed to create OpenJPEG stream".to_string());
        }

        opj_stream_set_write_function(stream, Some(stream_write));
        opj_stream_set_seek_function(stream, Some(stream_seek));
        opj_stream_set_user_data(
            stream,
            &mut *write_buf as *mut WriteBuffer as *mut std::ffi::c_void,
            None, // We manage the WriteBuffer lifetime ourselves
        );

        // Encode
        if opj_start_compress(encoder, image, stream) == 0 {
            opj_stream_destroy(stream);
            opj_destroy_codec(encoder);
            opj_image_destroy(image);
            return Err("opj_start_compress failed".to_string());
        }

        if opj_encode(encoder, stream) == 0 {
            opj_stream_destroy(stream);
            opj_destroy_codec(encoder);
            opj_image_destroy(image);
            return Err("opj_encode failed".to_string());
        }

        if opj_end_compress(encoder, stream) == 0 {
            opj_stream_destroy(stream);
            opj_destroy_codec(encoder);
            opj_image_destroy(image);
            return Err("opj_end_compress failed".to_string());
        }

        // Extract encoded data
        let result = write_buf.data[..write_buf.offset].to_vec();

        opj_stream_destroy(stream);
        opj_destroy_codec(encoder);
        opj_image_destroy(image);

        if result.is_empty() {
            return Err("OpenJPEG produced 0 bytes".to_string());
        }

        Ok(result)
    }
}

#[cfg(not(feature = "openjpeg"))]
fn compress_frame_opj(_frame: &RawFrame, _params: &CompressParams) -> Result<Vec<u8>, String> {
    Err("openjpeg feature not enabled".to_string())
}

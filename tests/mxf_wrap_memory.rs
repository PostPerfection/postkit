//! What a wrap is allowed to hold in memory.
//!
//! A feature-length picture MXF is tens of gigabytes and its sound track is
//! several, so a wrap that keeps the essence, or the finished file, in one
//! allocation is killed before it returns. Each test here wraps essence far
//! larger than any single frame and holds the wrap to a fraction of that.

use postkit::encode::FrameRate;
use postkit::mxf_wrap::{
    EssenceType, IncrementalJ2kWrap, IncrementalWrapOptions, MxfStandard, MxfWrapOptions, mxf_wrap,
};
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// A 2K raster at the default compression ratio, so the clip comes to more than
/// [`MINIMUM_ESSENCE_BYTES`] while any one frame stays under half a megabyte.
const WIDTH: u32 = 1998;
const HEIGHT: u32 = 1080;
const FRAME_COUNT: u64 = 64;

const MINIMUM_ESSENCE_BYTES: u64 = 16 << 20;

/// The share of the essence a wrap may hold at once. A wrap that reads every
/// frame up front, or hashes the finished MXF in one allocation, is over this by
/// a factor of four.
const PEAK_SHARE_OF_ESSENCE: u64 = 4;

static CURRENTLY_ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

fn record_allocation(bytes: usize) {
    let current = CURRENTLY_ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_ALLOCATED_BYTES.fetch_max(current, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        CURRENTLY_ALLOCATED_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            CURRENTLY_ALLOCATED_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            record_allocation(new_size);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The counters are process-wide, so only one test may be measuring at a time.
static MEASURING: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASURING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Put the peak back to what is allocated right now and report that level.
fn reset_peak_to_current() -> usize {
    let current = CURRENTLY_ALLOCATED_BYTES.load(Ordering::Relaxed);
    PEAK_ALLOCATED_BYTES.store(current, Ordering::Relaxed);
    current
}

fn peak_above(level: usize) -> u64 {
    PEAK_ALLOCATED_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(level) as u64
}

fn make_clip(path: &Path, frames: u64) {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:r={frames}"),
            "-frames:v",
            &frames.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn encode_options(frames: u64) -> EncodeRunOptions {
    EncodeRunOptions {
        fps: FrameRate::whole(frames as u32),
        ..Default::default()
    }
}

fn codestreams(j2k_dir: &Path) -> Vec<PathBuf> {
    (0..postkit::grok_encoder::contiguous_encoded_frames(j2k_dir))
        .map(|index| j2k_dir.join(format!("frame_{index:08}.j2c")))
        .collect()
}

/// Encode a clip into a directory of codestreams, and report their total size.
fn encoded_codestreams(dir: &Path) -> (Vec<PathBuf>, u64) {
    let video = dir.join("clip.mp4");
    make_clip(&video, FRAME_COUNT);
    let encode = run_encode_with_options(
        &video,
        &dir.join("encode"),
        &encode_options(FRAME_COUNT),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("encode");
    let files = codestreams(&encode.j2k_dir);
    assert_eq!(files.len(), FRAME_COUNT as usize);
    let total: u64 = files
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum();
    assert!(
        total >= MINIMUM_ESSENCE_BYTES,
        "the clip has to be bigger than what the wrap may hold: {total} bytes"
    );
    (files, total)
}

fn assert_peak_under_share(peak: u64, total_essence_bytes: u64) {
    let allowed = total_essence_bytes / PEAK_SHARE_OF_ESSENCE;
    assert!(
        peak < allowed,
        "the wrap held {peak} bytes at once, over the {allowed} allowed for \
         {total_essence_bytes} bytes of essence"
    );
}

#[test]
fn wrapping_a_directory_does_not_hold_the_essence() {
    let _measuring = measuring();
    let dir = tempfile::tempdir().unwrap();
    let (files, total_essence_bytes) = encoded_codestreams(dir.path());
    let output = dir.path().join("picture.mxf");

    let level = reset_peak_to_current();
    let track = mxf_wrap(&MxfWrapOptions {
        input_files: files,
        output: output.clone(),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::AsDcp,
        fps_num: FRAME_COUNT as u32,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    let peak = peak_above(level);

    assert!(track.success, "wrap failed: {}", track.error);
    assert_eq!(track.duration, FRAME_COUNT);
    println!("directory wrap peak {peak} bytes for {total_essence_bytes} bytes of essence");
    assert_peak_under_share(peak, total_essence_bytes);
}

const WAV_CHANNELS: u16 = 6;
const WAV_SAMPLE_RATE: u32 = 48000;
const WAV_BITS: u16 = 24;
const WAV_SECONDS: u32 = 32;
const WAV_FRAMES_PER_SECOND: u32 = 24;
const WAV_WRITE_BLOCK_BYTES: usize = 1 << 20;

/// Write a silent PCM WAV block by block, so the test itself never holds it.
fn make_wav(path: &Path) -> u64 {
    use std::io::Write;
    let block_align = (WAV_BITS / 8) as u32 * WAV_CHANNELS as u32;
    let data_len = WAV_SAMPLE_RATE * WAV_SECONDS * block_align;
    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_len).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&WAV_CHANNELS.to_le_bytes());
    header.extend_from_slice(&WAV_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&(WAV_SAMPLE_RATE * block_align).to_le_bytes());
    header.extend_from_slice(&(block_align as u16).to_le_bytes());
    header.extend_from_slice(&WAV_BITS.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    file.write_all(&header).unwrap();
    let silence = vec![0u8; WAV_WRITE_BLOCK_BYTES];
    let mut remaining = data_len as usize;
    while remaining > 0 {
        let block = remaining.min(WAV_WRITE_BLOCK_BYTES);
        file.write_all(&silence[..block]).unwrap();
        remaining -= block;
    }
    file.flush().unwrap();
    data_len as u64
}

#[test]
fn wrapping_a_wav_does_not_hold_the_essence() {
    let _measuring = measuring();
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sound.wav");
    let total_essence_bytes = make_wav(&wav);
    assert!(total_essence_bytes >= MINIMUM_ESSENCE_BYTES);
    let output = dir.path().join("sound.mxf");

    let level = reset_peak_to_current();
    let track = mxf_wrap(&MxfWrapOptions {
        input_files: vec![wav],
        output: output.clone(),
        essence_type: EssenceType::Pcm,
        standard: MxfStandard::AsDcp,
        fps_num: WAV_FRAMES_PER_SECOND,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    let peak = peak_above(level);

    assert!(track.success, "wrap failed: {}", track.error);
    assert_eq!(track.duration, (WAV_SECONDS * WAV_FRAMES_PER_SECOND) as u64);
    println!("wav wrap peak {peak} bytes for {total_essence_bytes} bytes of essence");
    assert_peak_under_share(peak, total_essence_bytes);
}

#[test]
fn wrapping_frame_by_frame_does_not_hold_the_finished_mxf() {
    let _measuring = measuring();
    let dir = tempfile::tempdir().unwrap();
    let (files, total_essence_bytes) = encoded_codestreams(dir.path());
    let output = dir.path().join("picture.mxf");

    let level = reset_peak_to_current();
    let mut wrap = IncrementalJ2kWrap::new(IncrementalWrapOptions {
        output: output.clone(),
        standard: MxfStandard::AsDcp,
        fps_num: FRAME_COUNT as u32,
        fps_den: 1,
        encryption: None,
        hdr: None,
        asset_uuid: None,
    })
    .expect("incremental wrap");
    for path in &files {
        let frame = std::fs::read(path).unwrap();
        wrap.write_frame(&frame).expect("write_frame");
    }
    let track = wrap.finish().expect("finish");
    let peak = peak_above(level);

    assert!(track.success, "wrap failed: {}", track.error);
    assert_eq!(track.duration, FRAME_COUNT);
    println!("incremental wrap peak {peak} bytes for {total_essence_bytes} bytes of essence");
    assert_peak_under_share(peak, total_essence_bytes);
}

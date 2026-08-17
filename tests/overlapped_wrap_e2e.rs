//! The overlapped picture wrap, end to end.
//!
//! The point of overlapping the wrap with the encode is that the MXF is the same
//! file it would have been afterwards, so both tests here compare the overlapped
//! path against the encode-then-wrap path that produced DCPs before it: same
//! essence when it succeeds, nothing left behind when it is cancelled.

use postkit::encode::FrameRate;
use postkit::mxf_wrap::{
    EssenceType, IncrementalWrapOptions, MxfStandard, MxfWrapOptions, mxf_wrap,
};
use postkit::pipeline::{
    EncodeRunOptions, PipelineProgress, run_encode_and_wrap_picture, run_encode_with_options,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;
const FRAME_COUNT: u64 = 12;
/// Long enough that the encode is still running when the first progress update
/// arrives, so the cancel lands mid-encode.
const CANCEL_FRAME_COUNT: u64 = 600;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

fn wrap_options(output: PathBuf, frames: u64) -> IncrementalWrapOptions {
    IncrementalWrapOptions {
        output,
        standard: MxfStandard::AsDcp,
        fps_num: frames as u32,
        fps_den: 1,
        encryption: None,
        hdr: None,
        asset_uuid: None,
    }
}

fn codestreams(j2k_dir: &Path) -> Vec<PathBuf> {
    (0..postkit::grok_encoder::contiguous_encoded_frames(j2k_dir))
        .map(|index| j2k_dir.join(format!("frame_{index:08}.j2c")))
        .collect()
}

/// Every frame of a picture MXF, read back through asdcplib.
fn essence(path: &Path) -> Vec<Vec<u8>> {
    let mut reader = asdcplib::jp2k::MxfReader::new();
    reader
        .open_read(&path.to_string_lossy())
        .unwrap_or_else(|e| panic!("{} does not open as an MXF: {e}", path.display()));
    let frames = reader.picture_descriptor().unwrap().container_duration;
    (0..frames)
        .map(|index| {
            let mut buf = vec![0u8; 4 << 20];
            let read = reader.read_frame(index, &mut buf, None, None).unwrap();
            buf.truncate(read);
            buf
        })
        .collect()
}

/// The overlapped wrap sees frames as the encoder finishes them, which is not
/// index order, so a reordering mistake would show up here as essence that does
/// not match the sequential wrap's.
#[test]
fn wrapping_during_the_encode_writes_the_essence_wrapping_afterwards_does() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    make_clip(&video, FRAME_COUNT);
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));

    let overlapped_dir = dir.path().join("overlapped");
    let overlapped_mxf = overlapped_dir.join("picture.mxf");
    std::fs::create_dir_all(&overlapped_dir).unwrap();
    let (overlapped_encode, track) = run_encode_and_wrap_picture(
        &video,
        &overlapped_dir,
        &encode_options(FRAME_COUNT),
        wrap_options(overlapped_mxf.clone(), FRAME_COUNT),
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("overlapped encode and wrap");

    assert_eq!(overlapped_encode.frames_encoded, FRAME_COUNT);
    assert_eq!(
        track.duration, FRAME_COUNT,
        "the wrap should carry every encoded frame"
    );
    assert!(track.success && track.error.is_empty());
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&overlapped_encode.j2k_dir),
        FRAME_COUNT,
        "the J2K directory stays an artifact of the overlapped run"
    );

    let sequential_dir = dir.path().join("sequential");
    let sequential = run_encode_with_options(
        &video,
        &sequential_dir,
        &encode_options(FRAME_COUNT),
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("sequential encode");
    let sequential_mxf = sequential_dir.join("picture.mxf");
    let wrapped = mxf_wrap(&MxfWrapOptions {
        input_files: codestreams(&sequential.j2k_dir),
        output: sequential_mxf.clone(),
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
    assert!(wrapped.success, "sequential wrap failed: {}", wrapped.error);

    // ids and timestamps differ between any two wraps, so the essence is what has
    // to match, frame for frame
    let overlapped_essence = essence(&overlapped_mxf);
    let sequential_essence = essence(&sequential_mxf);
    assert_eq!(
        overlapped_essence.len(),
        FRAME_COUNT as usize,
        "the overlapped MXF should declare every frame"
    );
    assert_eq!(
        overlapped_essence.len(),
        sequential_essence.len(),
        "the two MXFs should hold the same number of frames"
    );
    for (index, (overlapped, sequential)) in overlapped_essence
        .iter()
        .zip(sequential_essence.iter())
        .enumerate()
    {
        assert_eq!(
            overlapped,
            sequential,
            "frame {index} differs: {} bytes overlapped, {} bytes sequential",
            overlapped.len(),
            sequential.len()
        );
    }

    // the codestreams on disk are the same essence, so a reader of either the
    // directory or the MXF sees one encode
    let first_codestream =
        std::fs::read(overlapped_encode.j2k_dir.join("frame_00000000.j2c")).unwrap();
    assert_eq!(
        overlapped_essence[0], first_codestream,
        "frame 0 of the MXF should be the codestream written for frame 0"
    );
}

/// A cancelled run must not leave an MXF: asdcplib writes the footer and the real
/// duration at finalize, so a part-written file cannot be read at all. The
/// codestreams that finished stay put, as they do for an encode on its own.
#[test]
fn a_cancelled_overlapped_wrap_leaves_the_codestreams_and_no_mxf() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    make_clip(&video, CANCEL_FRAME_COUNT);

    let output_dir = dir.path().join("out");
    let mxf = output_dir.join("picture.mxf");
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let cancel_from_progress = cancel.clone();

    let outcome = run_encode_and_wrap_picture(
        &video,
        &output_dir,
        &encode_options(CANCEL_FRAME_COUNT),
        wrap_options(mxf.clone(), CANCEL_FRAME_COUNT),
        &cancel,
        &pause,
        move |progress: &PipelineProgress| {
            if progress.frame > 0 {
                cancel_from_progress.store(true, Ordering::Relaxed);
            }
        },
        |_: &str| {},
    );

    let Err(error) = outcome else {
        panic!("a cancelled run cannot report a finished MXF");
    };
    assert_eq!(error, "Cancelled", "got: {error}");
    assert!(
        !mxf.exists(),
        "the part-written MXF was left behind at {}",
        mxf.display()
    );

    let j2k_dir = output_dir.join("j2k");
    let finished = postkit::grok_encoder::contiguous_encoded_frames(&j2k_dir);
    assert!(
        finished > 0,
        "the codestreams that finished should still be in {}",
        j2k_dir.display()
    );
    assert!(
        finished < CANCEL_FRAME_COUNT,
        "the cancel landed after the whole encode, so it proves nothing"
    );
}

/// A wrap that fails takes the encode down with it, and the encoder only knows
/// that the wrap stopped taking frames. The error that comes back has to be the
/// wrap's own, or nobody can tell what went wrong.
#[test]
fn a_wrap_that_cannot_open_its_mxf_reports_why_rather_than_that_it_stopped() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    make_clip(&video, FRAME_COUNT);

    let unwritable = dir.path().join("no").join("such").join("dir");
    let outcome = run_encode_and_wrap_picture(
        &video,
        &dir.path().join("out"),
        &encode_options(FRAME_COUNT),
        wrap_options(unwritable.join("picture.mxf"), FRAME_COUNT),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    );

    let Err(error) = outcome else {
        panic!("the MXF cannot be written into a directory that does not exist");
    };
    assert!(error.contains("open_write failed"), "{error}");
}

/// A J2K sequence is already codestreams: nothing hands postkit a frame to wrap
/// as it goes, so asking for an overlapped wrap has to say so rather than
/// silently wrap nothing.
#[test]
fn an_input_that_never_reaches_the_encoder_refuses_an_overlapped_wrap() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("codestreams");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("frame_00000000.j2c"),
        b"not really a codestream",
    )
    .unwrap();

    let outcome = run_encode_and_wrap_picture(
        &source,
        &dir.path().join("out"),
        &encode_options(24),
        wrap_options(dir.path().join("picture.mxf"), 24),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    );

    let Err(error) = outcome else {
        panic!("a J2K sequence cannot be wrapped while it encodes");
    };
    assert!(
        error.contains("cannot be wrapped while it encodes"),
        "{error}"
    );
}

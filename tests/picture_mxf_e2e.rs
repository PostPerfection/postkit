use postkit::encode::FrameRate;
use postkit::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};
use postkit::picture_processing::PictureProcessing;
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: usize = 4;
const EDIT_RATE: u32 = 24;
const READ_BUFFER_BYTES: usize = 1 << 20;

fn codestreams() -> Vec<Vec<u8>> {
    let fixture = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinema2k_64x64.j2c"),
    )
    .unwrap();
    (0..FRAME_COUNT)
        .map(|index| {
            let mut frame = fixture.clone();
            frame.extend_from_slice(format!("FRAME{index:04}").as_bytes());
            frame
        })
        .collect()
}

fn wrap(input_files: Vec<PathBuf>, output: PathBuf) -> PathBuf {
    let track = mxf_wrap(&MxfWrapOptions {
        input_files,
        output: output.clone(),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::AsDcp,
        fps_num: EDIT_RATE,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: Vec::new(),
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(track.success, "wrap failed: {}", track.error);
    output
}

fn picture_mxf(dir: &Path, frames: &[Vec<u8>]) -> PathBuf {
    let sources = dir.join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let paths = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let path = sources.join(format!("{index}.j2c"));
            std::fs::write(&path, frame).unwrap();
            path
        })
        .collect();
    wrap(paths, dir.join("source.mxf"))
}

fn read_essence(mxf: &Path) -> Vec<Vec<u8>> {
    let mut reader = asdcplib::jp2k::MxfReader::new();
    reader.open_read(&mxf.to_string_lossy()).unwrap();
    let frames = reader.picture_descriptor().unwrap().container_duration;
    (0..frames)
        .map(|index| {
            let mut buffer = vec![0u8; READ_BUFFER_BYTES];
            let size = reader.read_frame(index, &mut buffer, None, None).unwrap();
            buffer.truncate(size);
            buffer
        })
        .collect()
}

fn encode_options() -> EncodeRunOptions {
    EncodeRunOptions {
        fps: FrameRate::whole(EDIT_RATE),
        ..Default::default()
    }
}

#[test]
fn a_picture_mxf_rewraps_its_codestreams_without_re_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let frames = codestreams();
    let source = picture_mxf(dir.path(), &frames);
    let output_dir = dir.path().join("out");

    let result = run_encode_with_options(
        &source,
        &output_dir,
        &encode_options(),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .unwrap();
    assert_eq!(result.j2k_dir, output_dir.join("j2k"));
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&result.j2k_dir),
        FRAME_COUNT as u64
    );

    let unwrapped = (0..FRAME_COUNT)
        .map(|index| result.j2k_dir.join(format!("frame_{index:08}.j2c")))
        .collect();
    let rewrapped = wrap(unwrapped, dir.path().join("picture.mxf"));
    assert!(
        read_essence(&rewrapped) == frames,
        "the rewrapped MXF does not hold the source codestreams byte for byte"
    );
}

#[test]
fn a_picture_mxf_refuses_picture_processing_like_a_j2k_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let source = picture_mxf(dir.path(), &codestreams());
    let options = EncodeRunOptions {
        picture: PictureProcessing {
            flip_horizontal: true,
            ..PictureProcessing::default()
        },
        ..encode_options()
    };

    let outcome = run_encode_with_options(
        &source,
        &dir.path().join("out"),
        &options,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    );

    let Err(error) = outcome else {
        panic!("a picture MXF has no frames to process");
    };
    assert_eq!(
        error,
        "J2K input is already compressed, so there are no frames to crop, rotate or fit"
    );
}

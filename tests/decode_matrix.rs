#![cfg(feature = "grok-ffi")]

use postkit::encode::{
    FrameRate, SourceColour, StreamEncodeOptions, default_rsiz, stream_encode_inprocess,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: u32 = 24;
const COMPRESSION_RATIO: f64 = 4.0;
const DCI_PRECISION_BITS: u8 = 12;
const TWELVE_BIT_MAX_CODE: f64 = 4095.0;
// swscale rounds the studio range expansion, which costs a few codes of 4095
const CODE_TOLERANCE: i32 = 24;

// strong chroma, so the three matrices read the same samples far apart
const PATCH_RGB_SIGNAL: [f64; 3] = [0.90, 0.20, 0.05];

const BT2020_LUMINANCE_WEIGHTS: [f64; 3] = [0.2627, 0.6780, 0.0593];
const BT709_LUMINANCE_WEIGHTS: [f64; 3] = [0.2126, 0.7152, 0.0722];
const BT601_LUMINANCE_WEIGHTS: [f64; 3] = [0.299, 0.587, 0.114];

// 10-bit studio range: luma 64 to 940, chroma 512 either way by 448
const TEN_BIT_LUMA_OFFSET: f64 = 64.0;
const TEN_BIT_LUMA_SPAN: f64 = 876.0;
const TEN_BIT_CHROMA_OFFSET: f64 = 512.0;
const TEN_BIT_CHROMA_SPAN: f64 = 896.0;

struct ColourTags {
    matrix: &'static str,
    primaries: &'static str,
    transfer: &'static str,
}

const BT2020_TAGS: ColourTags = ColourTags {
    matrix: "bt2020nc",
    primaries: "bt2020",
    transfer: "bt2020-10",
};

const BT709_TAGS: ColourTags = ColourTags {
    matrix: "bt709",
    primaries: "bt709",
    transfer: "bt709",
};

fn studio_range_ycbcr(rgb_signal: [f64; 3], weights: [f64; 3]) -> [u16; 3] {
    let luma = weights[0] * rgb_signal[0] + weights[1] * rgb_signal[1] + weights[2] * rgb_signal[2];
    let blue_chroma = (rgb_signal[2] - luma) / (2.0 * (1.0 - weights[2]));
    let red_chroma = (rgb_signal[0] - luma) / (2.0 * (1.0 - weights[0]));
    [
        (TEN_BIT_LUMA_OFFSET + TEN_BIT_LUMA_SPAN * luma).round() as u16,
        (TEN_BIT_CHROMA_OFFSET + TEN_BIT_CHROMA_SPAN * blue_chroma).round() as u16,
        (TEN_BIT_CHROMA_OFFSET + TEN_BIT_CHROMA_SPAN * red_chroma).round() as u16,
    ]
}

fn rgb_signal_from_studio_range_ycbcr(ycbcr: [u16; 3], weights: [f64; 3]) -> [f64; 3] {
    let luma = (f64::from(ycbcr[0]) - TEN_BIT_LUMA_OFFSET) / TEN_BIT_LUMA_SPAN;
    let blue_chroma = (f64::from(ycbcr[1]) - TEN_BIT_CHROMA_OFFSET) / TEN_BIT_CHROMA_SPAN;
    let red_chroma = (f64::from(ycbcr[2]) - TEN_BIT_CHROMA_OFFSET) / TEN_BIT_CHROMA_SPAN;
    let red = luma + red_chroma * 2.0 * (1.0 - weights[0]);
    let blue = luma + blue_chroma * 2.0 * (1.0 - weights[2]);
    let green = (luma - weights[0] * red - weights[2] * blue) / weights[1];
    [red, green, blue]
}

fn twelve_bit_codes(rgb_signal: [f64; 3]) -> [i32; 3] {
    rgb_signal.map(|signal| (signal.clamp(0.0, 1.0) * TWELVE_BIT_MAX_CODE).round() as i32)
}

fn close_enough(got: [i32; 3], want: [i32; 3]) -> bool {
    got.iter()
        .zip(want)
        .all(|(have, expected)| (have - expected).abs() <= CODE_TOLERANCE)
}

// ffv1 is lossless, so the encode reads the code values written here
fn tagged_clip(dir: &Path, name: &str, ycbcr: [u16; 3], tags: &ColourTags) -> PathBuf {
    let raw = dir.join(format!("{name}.yuv"));
    let mut frame = Vec::new();
    for _ in 0..(WIDTH * HEIGHT) {
        frame.extend_from_slice(&ycbcr[0].to_le_bytes());
    }
    for chroma in ycbcr.iter().skip(1) {
        for _ in 0..(WIDTH / 2 * HEIGHT / 2) {
            frame.extend_from_slice(&chroma.to_le_bytes());
        }
    }
    std::fs::write(&raw, &frame).expect("the raw planes have to be written");

    // the tags go on the input as well: an untagged rawvideo frame makes ffmpeg
    // convert the range on the way into the encoder and the codes move
    let colour_arguments = [
        "-color_primaries",
        tags.primaries,
        "-color_trc",
        tags.transfer,
        "-colorspace",
        tags.matrix,
        "-color_range",
        "tv",
    ];
    let clip = dir.join(format!("{name}.mkv"));
    let run = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "rawvideo"])
        .args(["-pix_fmt", "yuv420p10le"])
        .args(["-s", &format!("{WIDTH}x{HEIGHT}")])
        .args(["-r", &FRAME_RATE.to_string()])
        .args(colour_arguments)
        .arg("-i")
        .arg(&raw)
        .args(["-c:v", "ffv1", "-frames:v", "1"])
        .args(colour_arguments)
        .arg(&clip)
        .output()
        .expect("ffmpeg has to run");
    assert!(
        run.status.success(),
        "ffmpeg could not write {}: {}",
        clip.display(),
        String::from_utf8_lossy(&run.stderr)
    );

    let probed = postkit::probe::probe_pixel_format(&clip);
    assert_eq!(probed.pix_fmt, "yuv420p10le");
    assert_eq!(
        probed.color_space, tags.matrix,
        "the fixture has to carry the tag the decode reads"
    );
    clip
}

/// The middle pixel of the first codestream a stream encode writes, as 12-bit
/// RGB. `KeepRgb` runs no transform of its own, so what the codestream holds is
/// what ffmpeg's yuv to rgb conversion produced.
fn encoded_middle_pixel(clip: &Path, output_dir: &Path) -> [i32; 3] {
    let result = stream_encode_inprocess(
        &StreamEncodeOptions {
            input: clip.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            compression_ratio: COMPRESSION_RATIO,
            fps: FrameRate::whole(FRAME_RATE),
            source_colour: SourceColour::KeepRgb,
            rsiz: default_rsiz(),
            ..StreamEncodeOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );
    assert!(result.success, "the encode failed: {}", result.error);
    assert_eq!(result.frames_encoded, 1);

    let mut codestreams: Vec<PathBuf> = std::fs::read_dir(output_dir)
        .expect("the output directory has to be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    codestreams.sort();
    let first = codestreams
        .first()
        .expect("the encode has to have written a codestream");
    let bytes = std::fs::read(first).expect("the codestream has to be readable");
    let frame = postkit::grok_decoder::decode(bytes, 0).expect("the codestream has to decode");
    let at = (frame.height / 2 * frame.width + frame.width / 2) as usize;
    let shift = i32::from(frame.precision) - i32::from(DCI_PRECISION_BITS);
    assert!(
        shift >= 0,
        "a {}-bit codestream is below the {DCI_PRECISION_BITS} bits a DCI picture carries",
        frame.precision
    );
    [0, 1, 2].map(|component| frame.components[component][at] >> shift)
}

#[test]
fn a_bt2020_tagged_source_decodes_through_the_bt2020_matrix() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let ycbcr = studio_range_ycbcr(PATCH_RGB_SIGNAL, BT2020_LUMINANCE_WEIGHTS);
    let clip = tagged_clip(dir.path(), "bt2020", ycbcr, &BT2020_TAGS);

    let pixel = encoded_middle_pixel(&clip, &dir.path().join("j2k"));
    let want = twelve_bit_codes(PATCH_RGB_SIGNAL);
    assert!(
        close_enough(pixel, want),
        "the patch came out {pixel:?}, not the {want:?} the BT.2020 matrix gives"
    );

    for (name, weights) in [
        ("BT.601", BT601_LUMINANCE_WEIGHTS),
        ("BT.709", BT709_LUMINANCE_WEIGHTS),
    ] {
        let as_if = twelve_bit_codes(rgb_signal_from_studio_range_ycbcr(ycbcr, weights));
        assert!(
            !close_enough(pixel, as_if),
            "the {name} reading of the same samples is {as_if:?}, too close to {want:?} for this \
             to prove the matrix"
        );
    }
}

#[test]
fn a_bt709_tagged_source_decodes_through_the_bt709_matrix() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // the same samples a BT.2020 source carries, tagged BT.709 instead
    let ycbcr = studio_range_ycbcr(PATCH_RGB_SIGNAL, BT2020_LUMINANCE_WEIGHTS);
    let clip = tagged_clip(dir.path(), "bt709", ycbcr, &BT709_TAGS);

    let pixel = encoded_middle_pixel(&clip, &dir.path().join("j2k"));
    let want = twelve_bit_codes(rgb_signal_from_studio_range_ycbcr(
        ycbcr,
        BT709_LUMINANCE_WEIGHTS,
    ));
    assert!(
        close_enough(pixel, want),
        "the patch came out {pixel:?}, not the {want:?} the BT.709 matrix gives"
    );
    let as_if_bt2020 = twelve_bit_codes(PATCH_RGB_SIGNAL);
    assert!(
        !close_enough(pixel, as_if_bt2020),
        "the BT.2020 reading of the same samples is {as_if_bt2020:?}, too close to {want:?} for \
         this to prove the matrix"
    );
}

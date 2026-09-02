//! grok's accelerator plugin taking the encode and the decode, then handing
//! them back to the CPU.
//!
//! A reversible encode is lossless either way round, so the samples that come
//! back have to be the samples that went in whichever ran them. The frame
//! counter is what separates the two: it grows once per compress and once per
//! decompress the device handled, and it stands still after `use_cpu`.
//!
//! This test needs a machine with the plugin, which is what the `grok-gpu`
//! feature is for. A missing plugin fails it rather than skipping it.

#![cfg(feature = "grok-gpu")]

use postkit::grok_decoder;
use postkit::grok_encoder::{
    CompressParams, PhaseClocks, ProgressionOrder, RawFrame, accelerated_frames, gpu_active,
    initialize, use_cpu, use_gpu,
};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const PRECISION: u8 = 12;
const MAX_SAMPLE: i32 = (1 << PRECISION) - 1;

/// A pattern where every component differs at every pixel, so a swapped
/// component or a row read at the wrong offset cannot pass.
fn pattern() -> [Vec<i32>; 3] {
    let mut components = [Vec::new(), Vec::new(), Vec::new()];
    for y in 0..HEIGHT as i32 {
        for x in 0..WIDTH as i32 {
            components[0].push((x * 37 + y * 11) % (MAX_SAMPLE + 1));
            components[1].push((x * 5 + y * 71) % (MAX_SAMPLE + 1));
            components[2].push((x * 13 + y * 29 + 7) % (MAX_SAMPLE + 1));
        }
    }
    components
}

/// Encode one frame losslessly and hand back its codestream.
fn encode_reversible(components: [Vec<i32>; 3]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let params = CompressParams {
        // reversible 5/3 wavelet at 1:1, so nothing is thrown away
        irreversible: false,
        compression_ratio: 1.0,
        mct: false,
        apply_xyz_transform: false,
        // a plain codestream: the cinema profiles constrain the raster
        profile: 0,
        num_resolutions: 3,
        progression: ProgressionOrder::Lrcp,
        ..CompressParams::default()
    };

    let mut frame = Some(RawFrame::Planar {
        components,
        width: WIDTH,
        height: HEIGHT,
        precision: PRECISION,
        index: 0,
    });
    let result = postkit::grok_encoder::encode_pipeline(
        dir.path(),
        &params,
        1,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(PhaseClocks::default()),
        || frame.take(),
        |_| {},
    );
    assert!(result.success, "encode failed: {}", result.error);

    let codestream = std::fs::read(dir.path().join("frame_00000000.j2c"))
        .expect("the encoder wrote a codestream");
    assert!(!codestream.is_empty());
    codestream
}

/// Encode the pattern and decode it back, asserting every sample survived.
fn round_trip_returns_the_source_samples(label: &str) {
    let sent = pattern();
    let codestream = encode_reversible(sent.clone());

    let frame = grok_decoder::decode(codestream, 0).unwrap_or_else(|e| panic!("{label}: {e}"));
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT), "{label}");
    assert_eq!(frame.precision, PRECISION, "{label}");
    assert_eq!(frame.components.len(), 3, "{label}");
    for (index, (got, want)) in frame.components.iter().zip(sent.iter()).enumerate() {
        assert_eq!(
            got.len(),
            want.len(),
            "{label}: component {index} came back the wrong length"
        );
        let first_difference = got.iter().zip(want.iter()).position(|(a, b)| a != b);
        assert!(
            first_difference.is_none(),
            "{label}: component {index} differs at sample {:?}: got {:?}, want {:?}",
            first_difference,
            first_difference.map(|i| got[i]),
            first_difference.map(|i| want[i]),
        );
    }
}

/// One test, because the plugin is a switch for the whole process and two
/// tests would race over it.
#[test]
fn the_device_takes_every_call_after_use_gpu_and_none_after_use_cpu() {
    initialize(0);
    if let Err(reason) = use_gpu() {
        panic!("{reason}");
    }
    assert!(gpu_active(), "use_gpu returned Ok without recording it");

    let before_device = accelerated_frames();
    round_trip_returns_the_source_samples("on the device");
    assert_eq!(
        accelerated_frames(),
        before_device + 2,
        "the device has to have run one compress and one decompress"
    );

    use_cpu();
    assert!(!gpu_active(), "use_cpu left the accelerator recorded as on");

    let before_cpu = accelerated_frames();
    round_trip_returns_the_source_samples("on the cpu");
    assert_eq!(
        accelerated_frames(),
        before_cpu,
        "use_cpu has to keep every frame off the device"
    );
}

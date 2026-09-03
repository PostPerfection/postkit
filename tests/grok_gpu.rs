//! grok's accelerator plugin taking the encode and the decode, then handing
//! them back to the CPU.
//!
//! A reversible encode is lossless either way round, so the samples that come
//! back have to be the samples that went in whichever ran them. The frame
//! counter is what separates the two: it grows once per compress and once per
//! decompress the device handled, and it stands still after `use_cpu`.
//!
//! With the plugin on, the encode runs as one batch through the plugin's
//! pipeline, so the run is several frames long: every frame differs from every
//! other and each code stream has to decode back to its own frame.
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
const FRAME_COUNT: u64 = 12;

/// A pattern where every component differs at every pixel and every frame
/// differs from its neighbours, so a swapped component, a row read at the wrong
/// offset or a code stream filed under the wrong frame cannot pass.
fn pattern(frame_index: u64) -> [Vec<i32>; 3] {
    let shift = frame_index as i32 * 137;
    let mut components = [Vec::new(), Vec::new(), Vec::new()];
    for y in 0..HEIGHT as i32 {
        for x in 0..WIDTH as i32 {
            components[0].push((x * 37 + y * 11 + shift) % (MAX_SAMPLE + 1));
            components[1].push((x * 5 + y * 71 + shift * 3) % (MAX_SAMPLE + 1));
            components[2].push((x * 13 + y * 29 + 7 + shift * 5) % (MAX_SAMPLE + 1));
        }
    }
    components
}

/// Encode `frames` pattern frames losslessly and hand back the directory the
/// code streams landed in.
fn encode_reversible_run(frames: u64) -> tempfile::TempDir {
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

    let mut next_index = 0u64;
    let result = postkit::grok_encoder::encode_pipeline(
        dir.path(),
        &params,
        frames,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(PhaseClocks::default()),
        || {
            if next_index >= frames {
                return None;
            }
            let frame = RawFrame::Planar {
                components: pattern(next_index),
                width: WIDTH,
                height: HEIGHT,
                precision: PRECISION,
                index: next_index,
            };
            next_index += 1;
            Some(frame)
        },
        |_| {},
    );
    assert!(result.success, "encode failed: {}", result.error);
    assert_eq!(result.frames_encoded, frames, "not every frame was written");
    dir
}

/// Encode a run and decode every frame back, asserting each code stream carries
/// its own frame's samples.
fn round_trip_returns_the_source_samples(label: &str) {
    let dir = encode_reversible_run(FRAME_COUNT);

    for index in 0..FRAME_COUNT {
        let path = dir.path().join(format!("frame_{index:08}.j2c"));
        let codestream = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{label}: frame {index} was not written: {e}"));
        assert!(!codestream.is_empty(), "{label}: frame {index} is empty");

        let frame = grok_decoder::decode(codestream, 0)
            .unwrap_or_else(|e| panic!("{label}: frame {index}: {e}"));
        assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT), "{label}");
        assert_eq!(frame.precision, PRECISION, "{label}");
        assert_eq!(frame.components.len(), 3, "{label}");

        let sent = pattern(index);
        for (component, (got, want)) in frame.components.iter().zip(sent.iter()).enumerate() {
            assert_eq!(
                got.len(),
                want.len(),
                "{label}: frame {index} component {component} came back the wrong length"
            );
            let first_difference = got.iter().zip(want.iter()).position(|(a, b)| a != b);
            assert!(
                first_difference.is_none(),
                "{label}: frame {index} component {component} differs at sample {:?}: \
                 got {:?}, want {:?}",
                first_difference,
                first_difference.map(|i| got[i]),
                first_difference.map(|i| want[i]),
            );
        }
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
        before_device + FRAME_COUNT * 2,
        "the device has to have run one compress and one decompress per frame"
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

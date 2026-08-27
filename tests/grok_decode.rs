//! The in-process grok decoder against the in-process grok encoder.
//!
//! A reversible encode is lossless, so the samples that come back have to be
//! the samples that went in, exactly. That is the whole correctness gate: it
//! needs no reference decoder and no external tool, and it catches the two
//! things a hand-written FFI reader gets wrong, row stride and component order.

#![cfg(feature = "grok-ffi")]

use postkit::grok_decoder;
use postkit::grok_encoder::{CompressParams, PhaseClocks, ProgressionOrder, RawFrame, initialize};
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

    initialize(0);
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

#[test]
fn a_reversible_round_trip_returns_the_samples_it_was_given() {
    let sent = pattern();
    let codestream = encode_reversible(sent.clone());

    let frame = grok_decoder::decode(codestream, 0).expect("decode");
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
    assert_eq!(frame.precision, PRECISION);
    assert_eq!(frame.components.len(), 3);
    for (index, (got, want)) in frame.components.iter().zip(sent.iter()).enumerate() {
        assert_eq!(
            got.len(),
            want.len(),
            "component {index} came back the wrong length"
        );
        let first_difference = got.iter().zip(want.iter()).position(|(a, b)| a != b);
        assert!(
            first_difference.is_none(),
            "component {index} differs at sample {:?}: got {:?}, want {:?}",
            first_difference,
            first_difference.map(|i| got[i]),
            first_difference.map(|i| want[i]),
        );
    }
}

#[test]
fn the_reduce_factor_halves_each_dimension_per_level() {
    let codestream = encode_reversible(pattern());

    let half = grok_decoder::decode(codestream.clone(), 1).expect("decode at half");
    assert_eq!((half.width, half.height), (WIDTH / 2, HEIGHT / 2));

    let quarter = grok_decoder::decode(codestream, 2).expect("decode at a quarter");
    assert_eq!((quarter.width, quarter.height), (WIDTH / 4, HEIGHT / 4));
    assert_eq!(
        quarter.components[0].len(),
        (WIDTH / 4) as usize * (HEIGHT / 4) as usize,
        "the samples have to match the reduced size, not the full one"
    );
}

#[test]
fn the_packed_frame_is_what_the_colour_transform_reads() {
    let codestream = encode_reversible(pattern());
    let frame = grok_decoder::decode(codestream, 0).expect("decode");
    let packed = frame.to_xyz12le().expect("pack");

    assert_eq!(
        packed.len(),
        WIDTH as usize * HEIGHT as usize * 3 * 2,
        "three 16-bit words per pixel"
    );
    // the first pixel's X sample, read back the way colour.rs reads it
    let x = u16::from_le_bytes([packed[0], packed[1]]) >> 4;
    assert_eq!(i32::from(x), frame.components[0][0]);
}

#[test]
fn a_codestream_that_is_not_one_is_refused() {
    let error = grok_decoder::decode(vec![0u8; 64], 0).unwrap_err();
    assert!(!error.is_empty(), "the reason has to be named");
}

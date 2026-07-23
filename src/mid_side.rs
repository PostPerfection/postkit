// Mid-side decode (dom#3020): turn an M/S pair back into L/R in place, leaving
// every other channel (HI/VI, surrounds, ...) untouched.
//
// convention: matches DoM's src/lib/mid_side_decoder.cc, which normalizes the
// mid as (L+R)/2 and the side as (L-R)/2 (no 1/sqrt(2) factor). The inverse is
// L = M + S, R = M - S. After decoding, the mid lane holds L and the side lane
// holds R.

use crate::wav_io::{read_interleaved, write_interleaved};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MidSideError {
    #[error("channels must be non-zero")]
    ZeroChannels,
    #[error("mid ({mid}) and side ({side}) must differ and be < channels ({channels})")]
    BadChannelIndex {
        mid: usize,
        side: usize,
        channels: usize,
    },
    #[error("sample count {len} is not a whole number of {channels}-channel frames")]
    RaggedBuffer { len: usize, channels: usize },
    #[error("wav i/o: {0}")]
    Wav(#[from] hound::Error),
}

/// Decode a mid-side pair in place within an interleaved multi-channel buffer.
///
/// `samples` is interleaved with `channels` lanes per frame; `mid` and `side`
/// are the lane indices carrying the mid and side signals. On success the `mid`
/// lane holds left and the `side` lane holds right; all other lanes are left
/// byte-for-byte unchanged.
pub fn decode_mid_side(
    samples: &mut [f32],
    channels: usize,
    mid: usize,
    side: usize,
) -> Result<(), MidSideError> {
    if channels == 0 {
        return Err(MidSideError::ZeroChannels);
    }
    if mid == side || mid >= channels || side >= channels {
        return Err(MidSideError::BadChannelIndex {
            mid,
            side,
            channels,
        });
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(MidSideError::RaggedBuffer {
            len: samples.len(),
            channels,
        });
    }
    for frame in samples.chunks_exact_mut(channels) {
        let m = frame[mid];
        let s = frame[side];
        frame[mid] = m + s; // left
        frame[side] = m - s; // right
    }
    Ok(())
}

/// Decode a mid-side pair in a WAV, writing the result to `output` with the same
/// format, bit depth, channel count and sample rate. The `mid` lane becomes left
/// and `side` becomes right; every other lane is preserved.
pub fn decode_mid_side_wav(
    input: &Path,
    output: &Path,
    mid: usize,
    side: usize,
) -> Result<(), MidSideError> {
    let (spec, mut samples) = read_interleaved(input)?;
    decode_mid_side(&mut samples, spec.channels as usize, mid, side)?;
    write_interleaved(output, spec, &samples)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pair_to_expected_left_right() {
        // 4 channels: mid at 0, side at 1, then two pass-through lanes.
        let mut buf = vec![
            0.5, 0.25, 0.9, -0.9, // frame 0: M=0.5 S=0.25
            -0.2, 0.1, 0.3, 0.4, // frame 1: M=-0.2 S=0.1
        ];
        decode_mid_side(&mut buf, 4, 0, 1).unwrap();
        // L = M+S, R = M-S
        assert_eq!(buf[0], 0.75);
        assert_eq!(buf[1], 0.25);
        assert_eq!(buf[4], -0.1);
        assert!((buf[5] - (-0.3)).abs() < 1e-6);
    }

    #[test]
    fn untouched_channels_are_byte_identical() {
        // 6 channels, M/S at 2 and 3; the rest (0,1,4,5) must not change a bit.
        let orig: Vec<f32> = vec![
            0.11, -0.22, 0.5, 0.25, 0.33, -0.44, //
            0.55, -0.66, -0.2, 0.1, 0.77, -0.88,
        ];
        let mut buf = orig.clone();
        decode_mid_side(&mut buf, 6, 2, 3).unwrap();
        for frame in 0..2 {
            for ch in [0usize, 1, 4, 5] {
                let i = frame * 6 + ch;
                assert_eq!(
                    buf[i].to_bits(),
                    orig[i].to_bits(),
                    "channel {ch} frame {frame} changed"
                );
            }
        }
        // and the pair was decoded
        assert_eq!(buf[2], 0.75);
        assert_eq!(buf[3], 0.25);
    }

    #[test]
    fn rejects_bad_arguments() {
        let mut buf = vec![0.0f32; 8];
        assert!(matches!(
            decode_mid_side(&mut buf, 0, 0, 1),
            Err(MidSideError::ZeroChannels)
        ));
        assert!(matches!(
            decode_mid_side(&mut buf, 4, 1, 1),
            Err(MidSideError::BadChannelIndex { .. })
        ));
        assert!(matches!(
            decode_mid_side(&mut buf, 4, 1, 4),
            Err(MidSideError::BadChannelIndex { .. })
        ));
        let mut ragged = vec![0.0f32; 7];
        assert!(matches!(
            decode_mid_side(&mut ragged, 4, 0, 1),
            Err(MidSideError::RaggedBuffer { .. })
        ));
    }

    #[test]
    fn wav_roundtrip_decodes_pair() {
        use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ms.wav");
        let dst = dir.path().join("lr.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        // M=0.5 S=0.25 full-scale-ish -> L=M+S=0.75, R=M-S=0.25.
        let fs = (1i64 << 23) as f32;
        let mut w = WavWriter::create(&src, spec).unwrap();
        for _ in 0..100 {
            w.write_sample((0.5 * fs) as i32).unwrap();
            w.write_sample((0.25 * fs) as i32).unwrap();
        }
        w.finalize().unwrap();

        decode_mid_side_wav(&src, &dst, 0, 1).unwrap();
        let mut r = WavReader::open(&dst).unwrap();
        assert_eq!(r.spec().channels, 2);
        assert_eq!(r.spec().bits_per_sample, 24);
        let s: Vec<i32> = r.samples::<i32>().map(|x| x.unwrap()).collect();
        assert!((s[0] as f32 / fs - 0.75).abs() < 1e-3, "L {}", s[0]);
        assert!((s[1] as f32 / fs - 0.25).abs() < 1e-3, "R {}", s[1]);
    }
}

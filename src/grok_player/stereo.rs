use std::sync::{Arc, Mutex};

use super::timeline::StereoscopicPhase;
use super::{RGBA_SAMPLES_PER_PIXEL, RgbaFrame, RgbaSample, RgbaSamples};

const EYES: usize = 2;
const LEFT_EYE_ONLY: [StereoscopicPhase; 1] = [StereoscopicPhase::Left];
const RIGHT_EYE_ONLY: [StereoscopicPhase; 1] = [StereoscopicPhase::Right];
const BOTH_EYES: [StereoscopicPhase; EYES] = [StereoscopicPhase::Left, StereoscopicPhase::Right];

// what a stereoscopic source shows, a mono source ignores it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StereoOutput {
    #[default]
    LeftEye,
    RightEye,
    // each eye squeezed to half the width, the left eye on the left
    SideBySide,
    // each eye squeezed to half the height, the left eye on top
    TopAndBottom,
}

impl StereoOutput {
    pub(super) fn eyes(self) -> &'static [StereoscopicPhase] {
        match self {
            StereoOutput::LeftEye => &LEFT_EYE_ONLY,
            StereoOutput::RightEye => &RIGHT_EYE_ONLY,
            StereoOutput::SideBySide | StereoOutput::TopAndBottom => &BOTH_EYES,
        }
    }

    pub(super) fn arrangement(self) -> EyeArrangement {
        match self {
            StereoOutput::LeftEye | StereoOutput::RightEye => EyeArrangement::Single,
            StereoOutput::SideBySide => EyeArrangement::SideBySide,
            StereoOutput::TopAndBottom => EyeArrangement::TopAndBottom,
        }
    }
}

// how the eyes share one frame
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum EyeArrangement {
    #[default]
    Single,
    SideBySide,
    TopAndBottom,
}

impl EyeArrangement {
    // eye cells across and down
    pub fn cells(self) -> (u32, u32) {
        match self {
            EyeArrangement::Single => (1, 1),
            EyeArrangement::SideBySide => (2, 1),
            EyeArrangement::TopAndBottom => (1, 2),
        }
    }
}

// the two eyes of one frame, decoded as two jobs
pub(super) struct StereoPair {
    arrangement: EyeArrangement,
    eyes: Mutex<[Option<Result<RgbaFrame, String>>; EYES]>,
}

pub(super) struct StereoHalf {
    pub pair: Arc<StereoPair>,
    pub eye: StereoscopicPhase,
}

impl StereoPair {
    pub fn new(arrangement: EyeArrangement) -> Arc<Self> {
        Arc::new(StereoPair {
            arrangement,
            eyes: Mutex::new([None, None]),
        })
    }

    // the packed frame once the other eye is in too, None until then
    pub fn complete(
        &self,
        eye: StereoscopicPhase,
        rendered: Result<RgbaFrame, String>,
    ) -> Option<Result<RgbaFrame, String>> {
        let mut eyes = self.eyes.lock().unwrap();
        eyes[eye as usize] = Some(rendered);
        if eyes.iter().any(Option::is_none) {
            return None;
        }
        let [Some(left), Some(right)] = std::mem::take(&mut *eyes) else {
            return None;
        };
        Some(left.and_then(|left| pack(left, right?, self.arrangement)))
    }
}

fn pixel<S>(samples: &[S], width: usize, x: usize, y: usize) -> &[S] {
    let at = (y * width + x) * RGBA_SAMPLES_PER_PIXEL;
    &samples[at..at + RGBA_SAMPLES_PER_PIXEL]
}

fn average<S: RgbaSample>(first: &[S], second: &[S]) -> [S; RGBA_SAMPLES_PER_PIXEL] {
    std::array::from_fn(|channel| {
        S::from_wide((first[channel].into() + second[channel].into()).div_ceil(2))
    })
}

fn squeeze_eyes<S: RgbaSample>(
    left: &[S],
    right: &[S],
    eye_width: usize,
    (packed_width, packed_height): (usize, usize),
    arrangement: EyeArrangement,
) -> Vec<S> {
    let mut samples = Vec::with_capacity(packed_width * packed_height * RGBA_SAMPLES_PER_PIXEL);
    for y in 0..packed_height {
        for x in 0..packed_width {
            let squeezed = match arrangement {
                EyeArrangement::SideBySide => {
                    let half = packed_width / EYES;
                    let eye = if x < half { left } else { right };
                    let column = (x % half) * EYES;
                    average(
                        pixel(eye, eye_width, column, y),
                        pixel(eye, eye_width, column + 1, y),
                    )
                }
                _ => {
                    let half = packed_height / EYES;
                    let eye = if y < half { left } else { right };
                    let row = (y % half) * EYES;
                    average(
                        pixel(eye, eye_width, x, row),
                        pixel(eye, eye_width, x, row + 1),
                    )
                }
            };
            samples.extend_from_slice(&squeezed);
        }
    }
    samples
}

// both eyes squeezed into a frame the size of one
pub(super) fn pack(
    left: RgbaFrame,
    right: RgbaFrame,
    arrangement: EyeArrangement,
) -> Result<RgbaFrame, String> {
    if (left.width, left.height) != (right.width, right.height) {
        return Err(format!(
            "the left eye is {}x{} and the right eye {}x{}",
            left.width, left.height, right.width, right.height
        ));
    }
    let (width, height) = (left.width as usize, left.height as usize);
    let (packed_width, packed_height) = match arrangement {
        EyeArrangement::Single => return Ok(left),
        EyeArrangement::SideBySide => (width / EYES * EYES, height),
        EyeArrangement::TopAndBottom => (width, height / EYES * EYES),
    };
    let packed_size = (packed_width, packed_height);
    let samples = match (left.samples, right.samples) {
        (RgbaSamples::Eight(left), RgbaSamples::Eight(right)) => {
            RgbaSamples::Eight(squeeze_eyes(&left, &right, width, packed_size, arrangement))
        }
        // one eye from the device at 8 bits and one from the cpu at 16
        (left, right) => RgbaSamples::Sixteen(squeeze_eyes(
            &left.widened(),
            &right.widened(),
            width,
            packed_size,
            arrangement,
        )),
    };
    Ok(RgbaFrame {
        width: packed_width as u32,
        height: packed_height as u32,
        samples,
        eyes: arrangement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok_player::OPAQUE_ALPHA;

    fn flat(width: u32, height: u32, colour: [u8; 3]) -> RgbaFrame {
        let [red, green, blue] = colour;
        RgbaFrame {
            width,
            height,
            samples: RgbaSamples::Eight(
                [red, green, blue, OPAQUE_ALPHA].repeat((width * height) as usize),
            ),
            eyes: EyeArrangement::Single,
        }
    }

    fn flat16(width: u32, height: u32, colour: [u16; 3]) -> RgbaFrame {
        let [red, green, blue] = colour;
        RgbaFrame {
            width,
            height,
            samples: RgbaSamples::Sixteen(
                [red, green, blue, u16::MAX].repeat((width * height) as usize),
            ),
            eyes: EyeArrangement::Single,
        }
    }

    fn colour_at(frame: &RgbaFrame, x: u32, y: u32) -> [u8; 3] {
        let RgbaSamples::Eight(samples) = &frame.samples else {
            panic!("the frame is not 8 bit");
        };
        let at = ((y * frame.width + x) as usize) * RGBA_SAMPLES_PER_PIXEL;
        [samples[at], samples[at + 1], samples[at + 2]]
    }

    fn colour16_at(frame: &RgbaFrame, x: u32, y: u32) -> [u16; 3] {
        let RgbaSamples::Sixteen(samples) = &frame.samples else {
            panic!("the frame is not 16 bit");
        };
        let at = ((y * frame.width + x) as usize) * RGBA_SAMPLES_PER_PIXEL;
        [samples[at], samples[at + 1], samples[at + 2]]
    }

    const LEFT: [u8; 3] = [200, 10, 10];
    const RIGHT: [u8; 3] = [10, 10, 200];

    #[test]
    fn side_by_side_puts_each_eye_in_its_half_at_the_eye_size() {
        let packed = pack(
            flat(8, 4, LEFT),
            flat(8, 4, RIGHT),
            EyeArrangement::SideBySide,
        )
        .unwrap();
        assert_eq!((packed.width, packed.height), (8, 4));
        assert_eq!(packed.eyes, EyeArrangement::SideBySide);
        assert_eq!(colour_at(&packed, 3, 3), LEFT);
        assert_eq!(colour_at(&packed, 4, 0), RIGHT);
    }

    #[test]
    fn top_and_bottom_puts_the_left_eye_on_top() {
        let packed = pack(
            flat(8, 4, LEFT),
            flat(8, 4, RIGHT),
            EyeArrangement::TopAndBottom,
        )
        .unwrap();
        assert_eq!((packed.width, packed.height), (8, 4));
        assert_eq!(colour_at(&packed, 7, 1), LEFT);
        assert_eq!(colour_at(&packed, 0, 2), RIGHT);
    }

    #[test]
    fn a_squeeze_averages_the_two_pixels_it_folds() {
        let mut left = flat(2, 1, [0, 0, 0]);
        let RgbaSamples::Eight(samples) = &mut left.samples else {
            unreachable!()
        };
        samples[RGBA_SAMPLES_PER_PIXEL..RGBA_SAMPLES_PER_PIXEL + 3].copy_from_slice(&[100, 50, 2]);
        let packed = pack(left, flat(2, 1, RIGHT), EyeArrangement::SideBySide).unwrap();
        assert_eq!(colour_at(&packed, 0, 0), [50, 25, 1]);
    }

    #[test]
    fn a_sixteen_bit_squeeze_keeps_the_low_bits() {
        let mut left = flat16(2, 1, [1000, 2001, u16::MAX]);
        let RgbaSamples::Sixteen(samples) = &mut left.samples else {
            unreachable!()
        };
        samples[RGBA_SAMPLES_PER_PIXEL..RGBA_SAMPLES_PER_PIXEL + 3]
            .copy_from_slice(&[1002, 2002, 1]);
        let packed = pack(left, flat16(2, 1, [7, 8, 9]), EyeArrangement::SideBySide).unwrap();
        assert_eq!(colour16_at(&packed, 0, 0), [1001, 2002, 32768]);
        assert_eq!(colour16_at(&packed, 1, 0), [7, 8, 9]);
    }

    #[test]
    fn an_eight_bit_eye_beside_a_sixteen_bit_one_packs_at_sixteen_bits() {
        let packed = pack(
            flat(4, 2, LEFT),
            flat16(4, 2, [1, 2, 3]),
            EyeArrangement::TopAndBottom,
        )
        .unwrap();
        assert_eq!(colour16_at(&packed, 0, 0), LEFT.map(u16::from_eight_bit));
        assert_eq!(colour16_at(&packed, 3, 1), [1, 2, 3]);
    }

    #[test]
    fn a_pair_packs_once_both_eyes_are_in_whichever_comes_first() {
        let pair = StereoPair::new(EyeArrangement::SideBySide);
        assert!(
            pair.complete(StereoscopicPhase::Right, Ok(flat(4, 2, RIGHT)))
                .is_none()
        );
        let packed = pair
            .complete(StereoscopicPhase::Left, Ok(flat(4, 2, LEFT)))
            .expect("both eyes are in")
            .unwrap();
        assert_eq!(colour_at(&packed, 0, 0), LEFT);
        assert_eq!(colour_at(&packed, 3, 1), RIGHT);
    }

    #[test]
    fn a_pair_with_a_failed_eye_fails_with_its_reason() {
        let pair = StereoPair::new(EyeArrangement::TopAndBottom);
        assert!(
            pair.complete(StereoscopicPhase::Left, Ok(flat(4, 2, LEFT)))
                .is_none()
        );
        let failed = pair.complete(
            StereoscopicPhase::Right,
            Err("the right eye did not decode".into()),
        );
        assert_eq!(
            failed.map(|result| result.err()),
            Some(Some("the right eye did not decode".to_string()))
        );
    }

    #[test]
    fn eyes_of_different_sizes_do_not_pack() {
        let error = pack(
            flat(4, 2, LEFT),
            flat(2, 2, RIGHT),
            EyeArrangement::SideBySide,
        )
        .err()
        .unwrap();
        assert!(error.contains("4x2"), "{error}");
    }
}

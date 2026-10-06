use std::sync::{Arc, Mutex};

use super::timeline::StereoscopicPhase;
use super::{RGBA_BYTES_PER_PIXEL, Rgba8Frame};

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
    eyes: Mutex<[Option<Result<Rgba8Frame, String>>; EYES]>,
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
        rendered: Result<Rgba8Frame, String>,
    ) -> Option<Result<Rgba8Frame, String>> {
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

fn pixel(frame: &Rgba8Frame, x: usize, y: usize) -> &[u8] {
    let at = (y * frame.width as usize + x) * RGBA_BYTES_PER_PIXEL;
    &frame.data[at..at + RGBA_BYTES_PER_PIXEL]
}

fn average(first: &[u8], second: &[u8]) -> [u8; RGBA_BYTES_PER_PIXEL] {
    std::array::from_fn(|channel| {
        ((u16::from(first[channel]) + u16::from(second[channel])).div_ceil(2)) as u8
    })
}

// both eyes squeezed into a frame the size of one
pub(super) fn pack(
    left: Rgba8Frame,
    right: Rgba8Frame,
    arrangement: EyeArrangement,
) -> Result<Rgba8Frame, String> {
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
    let mut data = Vec::with_capacity(packed_width * packed_height * RGBA_BYTES_PER_PIXEL);
    for y in 0..packed_height {
        for x in 0..packed_width {
            let squeezed = match arrangement {
                EyeArrangement::SideBySide => {
                    let half = packed_width / EYES;
                    let eye = if x < half { &left } else { &right };
                    let column = (x % half) * EYES;
                    average(pixel(eye, column, y), pixel(eye, column + 1, y))
                }
                _ => {
                    let half = packed_height / EYES;
                    let eye = if y < half { &left } else { &right };
                    let row = (y % half) * EYES;
                    average(pixel(eye, x, row), pixel(eye, x, row + 1))
                }
            };
            data.extend_from_slice(&squeezed);
        }
    }
    Ok(Rgba8Frame {
        width: packed_width as u32,
        height: packed_height as u32,
        data,
        eyes: arrangement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok_player::OPAQUE_ALPHA;

    fn flat(width: u32, height: u32, colour: [u8; 3]) -> Rgba8Frame {
        let [red, green, blue] = colour;
        Rgba8Frame {
            width,
            height,
            data: [red, green, blue, OPAQUE_ALPHA].repeat((width * height) as usize),
            eyes: EyeArrangement::Single,
        }
    }

    fn colour_at(frame: &Rgba8Frame, x: u32, y: u32) -> [u8; 3] {
        let at = ((y * frame.width + x) as usize) * RGBA_BYTES_PER_PIXEL;
        [frame.data[at], frame.data[at + 1], frame.data[at + 2]]
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
        left.data[RGBA_BYTES_PER_PIXEL..RGBA_BYTES_PER_PIXEL + 3].copy_from_slice(&[100, 50, 2]);
        let packed = pack(left, flat(2, 1, RIGHT), EyeArrangement::SideBySide).unwrap();
        assert_eq!(colour_at(&packed, 0, 0), [50, 25, 1]);
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

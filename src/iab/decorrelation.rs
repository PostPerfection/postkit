use std::f64::consts::PI;

use super::panner::Loudspeaker;

pub const FILTER_LENGTH: usize = 512;
// BS.2127 7.4 delays the direct path to line up with the filters
pub const DIRECT_PATH_DELAY: usize = (FILTER_LENGTH - 1) / 2;

// BS.2127 7.4, one random phase all-pass filter per output channel
pub(super) fn decorrelation_filters(loudspeakers: &[Loudspeaker]) -> Vec<Vec<f32>> {
    let mut sorted_labels: Vec<&str> = loudspeakers.iter().map(|speaker| speaker.label).collect();
    sorted_labels.sort_unstable();
    loudspeakers
        .iter()
        .map(|speaker| {
            let seed = sorted_labels
                .iter()
                .position(|label| *label == speaker.label)
                .expect("the sorted labels hold every loudspeaker");
            decorrelation_filter(seed as u32)
        })
        .collect()
}

fn decorrelation_filter(seed: u32) -> Vec<f32> {
    let mut generator = Mt19937::new(seed);
    let half = FILTER_LENGTH / 2;
    let phases: Vec<f64> = (1..half)
        .map(|_| 2.0 * PI * generator.next_unit_interval())
        .collect();
    // the inverse real FFT of unit magnitude bins, with bins 0 and N/2 at phase 0
    (0..FILTER_LENGTH)
        .map(|time| {
            let nyquist = if time % 2 == 0 { 1.0 } else { -1.0 };
            let middle: f64 = phases
                .iter()
                .enumerate()
                .map(|(index, phase)| {
                    let bin = (index + 1) as f64;
                    (phase + 2.0 * PI * bin * time as f64 / FILTER_LENGTH as f64).cos()
                })
                .sum();
            ((1.0 + nyquist + 2.0 * middle) / FILTER_LENGTH as f64) as f32
        })
        .collect()
}

// the MT19937 generator of Matsumoto and Nishimura, seeded with init_genrand
struct Mt19937 {
    state: [u32; 624],
    index: usize,
}

impl Mt19937 {
    fn new(seed: u32) -> Self {
        let mut state = [0u32; 624];
        state[0] = seed;
        for index in 1..624 {
            let previous = state[index - 1];
            state[index] = 1_812_433_253u32
                .wrapping_mul(previous ^ (previous >> 30))
                .wrapping_add(index as u32);
        }
        Mt19937 { state, index: 624 }
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for index in 0..624 {
                let upper = self.state[index] & 0x8000_0000;
                let lower = self.state[(index + 1) % 624] & 0x7fff_ffff;
                let combined = upper | lower;
                let mut next = self.state[(index + 397) % 624] ^ (combined >> 1);
                if combined & 1 != 0 {
                    next ^= 0x9908_b0df;
                }
                self.state[index] = next;
            }
            self.index = 0;
        }
        let mut value = self.state[self.index];
        self.index += 1;
        value ^= value >> 11;
        value ^= (value << 7) & 0x9d2c_5680;
        value ^= (value << 15) & 0xefc6_0000;
        value ^ (value >> 18)
    }

    fn next_unit_interval(&mut self) -> f64 {
        f64::from(self.next_u32()) / 4_294_967_296.0
    }
}

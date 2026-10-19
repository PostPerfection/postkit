use super::panner::{Cartesian, Loudspeaker, point_gains};

const HALF_POWER: f64 = std::f64::consts::FRAC_1_SQRT_2;

// the ST 2098-2 Table 19 channels SMPTE RDD 57 Table 1 allows, with the ITU-R
// BS.2051 label of the 0+7+0 loudspeaker the channel drives and the channel's ADM
// position. Lts and Rts have no BS.2051 loudspeaker: ST 2098-5 Annex B puts them
// on the ceiling between the centre and side screen loudspeakers, running front
// to back, so they sit at IAB (0.25, 0.5, 1) and (0.75, 0.5, 1).
struct BedChannel {
    channel_id: u32,
    label: Option<&'static str>,
    position: Cartesian,
}

const fn bed_channel(
    channel_id: u32,
    label: Option<&'static str>,
    x: f64,
    y: f64,
    z: f64,
) -> BedChannel {
    BedChannel {
        channel_id,
        label,
        position: Cartesian { x, y, z },
    }
}

const PROFILE_1_BED_CHANNELS: [BedChannel; 10] = [
    bed_channel(0x0, Some("M+030"), -1.0, 1.0, 0.0),
    bed_channel(0x2, Some("M+000"), 0.0, 1.0, 0.0),
    bed_channel(0x4, Some("M-030"), 1.0, 1.0, 0.0),
    bed_channel(0x5, Some("M+090"), -1.0, 0.0, 0.0),
    bed_channel(0x7, Some("M+135"), -1.0, -1.0, 0.0),
    bed_channel(0x8, Some("M-135"), 1.0, -1.0, 0.0),
    bed_channel(0x9, Some("M-090"), 1.0, 0.0, 0.0),
    bed_channel(0xB, None, -0.5, 0.0, 1.0),
    bed_channel(0xC, None, 0.5, 0.0, 1.0),
    bed_channel(0xD, Some("LFE1"), -1.0, 1.0, -1.0),
];

// the BS.2127 Table 16 rules for the labels above, in table order, leaving out
// the rows limited to a 9+10+3 input since the bed is taken as 0+7+0 input
const MAPPING_RULES: &[(&str, &[(&str, f64)])] = &[
    ("M+000", &[("M+000", 1.0)]),
    ("M+000", &[("M+030", HALF_POWER), ("M-030", HALF_POWER)]),
    ("M+090", &[("M+090", 1.0)]),
    ("M-090", &[("M-090", 1.0)]),
    ("M+090", &[("M+030", HALF_POWER), ("M+110", HALF_POWER)]),
    ("M-090", &[("M-030", HALF_POWER), ("M-110", HALF_POWER)]),
    ("M+090", &[("M+030", HALF_POWER)]),
    ("M-090", &[("M-030", HALF_POWER)]),
    ("M+135", &[("M+135", 1.0)]),
    ("M-135", &[("M-135", 1.0)]),
    ("M+135", &[("M+110", 1.0)]),
    ("M-135", &[("M-110", 1.0)]),
    ("M+135", &[("M+030", HALF_POWER)]),
    ("M-135", &[("M-030", HALF_POWER)]),
];

// BS.2127 clause 8, one gain per output channel; None for a channel Profile 1 does not allow
pub(super) fn bed_channel_gains(loudspeakers: &[Loudspeaker], channel_id: u32) -> Option<Vec<f64>> {
    let channel = PROFILE_1_BED_CHANNELS
        .iter()
        .find(|channel| channel.channel_id == channel_id)?;
    let mut gains = vec![0.0; loudspeakers.len()];
    let index_of = |label: &str| {
        loudspeakers
            .iter()
            .position(|speaker| speaker.label == label)
    };

    if let Some(label) = channel.label {
        let rule = MAPPING_RULES
            .iter()
            .filter(|(input, _)| *input == label)
            .find(|(_, outputs)| outputs.iter().all(|(output, _)| index_of(output).is_some()));
        if let Some((_, outputs)) = rule {
            for (output, gain) in *outputs {
                gains[index_of(output).expect("the rule matched every output")] = *gain;
            }
            return Some(gains);
        }
    }
    let is_lfe = channel.label == Some("LFE1");
    let candidates: Vec<usize> = (0..loudspeakers.len())
        .filter(|index| loudspeakers[*index].is_lfe == is_lfe)
        .collect();
    let label_match = candidates
        .iter()
        .find(|index| Some(loudspeakers[**index].label) == channel.label);
    if let Some(index) = label_match {
        gains[*index] = 1.0;
        return Some(gains);
    }
    // with no bounds coded, the bounds are the nominal position itself
    const BOUNDS_MARGIN: f64 = 1e-5;
    let at_nominal_position: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|index| {
            let position = loudspeakers[*index].position;
            (position.x - channel.position.x).abs() <= BOUNDS_MARGIN
                && (position.y - channel.position.y).abs() <= BOUNDS_MARGIN
                && (position.z - channel.position.z).abs() <= BOUNDS_MARGIN
        })
        .collect();
    if let [index] = at_nominal_position[..] {
        gains[index] = 1.0;
        return Some(gains);
    }
    // an LFE channel with no LFE loudspeaker is dropped, as BS.2127 8 step 6 does
    if is_lfe {
        return Some(gains);
    }
    let positions: Vec<Cartesian> = candidates
        .iter()
        .map(|index| loudspeakers[*index].position)
        .collect();
    for (index, gain) in candidates
        .iter()
        .zip(point_gains(&positions, channel.position))
    {
        gains[*index] = gain;
    }
    Some(gains)
}

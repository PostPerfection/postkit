use std::collections::{BTreeMap, HashMap, HashSet};

use super::decorrelation::{DIRECT_PATH_DELAY, FILTER_LENGTH, decorrelation_filters};
use super::direct_speakers::bed_channel_gains;
use super::panner::{
    Cartesian, FIVE_POINT_ONE_LOUDSPEAKERS, Loudspeaker, ObjectPanner, ObjectParameters,
    SEVEN_POINT_ONE_LOUDSPEAKERS, STEREO_LOUDSPEAKERS,
};
use super::{
    BedDefinition, Decorrelation, Frame, FrameRate, Gain, IabError, ObjectDefinition, ObjectPan,
    ObjectPosition, ObjectSnap, ObjectSpread, ZoneGain,
};

// output channel order: L R, L R C LFE Ls Rs, L R C LFE Lss Rss Lrs Rrs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLayout {
    Stereo,
    FivePointOne,
    SevenPointOne,
}

impl OutputLayout {
    pub fn channel_count(self) -> usize {
        self.loudspeakers().len()
    }

    pub(super) fn loudspeakers(self) -> &'static [Loudspeaker] {
        match self {
            OutputLayout::Stereo => STEREO_LOUDSPEAKERS,
            OutputLayout::FivePointOne => FIVE_POINT_ONE_LOUDSPEAKERS,
            OutputLayout::SevenPointOne => SEVEN_POINT_ONE_LOUDSPEAKERS,
        }
    }

    // ST 2098-2 Table 1, stereo is BS.2051 sound system A
    fn use_case(self) -> u8 {
        match self {
            OutputLayout::Stereo => 0x30,
            OutputLayout::FivePointOne => 0x01,
            OutputLayout::SevenPointOne => 0x02,
        }
    }
}

const ALWAYS_USE: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrenderedFeature {
    BedRemap,
    BedChannelDecorrelation,
    ChannelId(u32),
    ObjectZoneDefinition19,
    LowResolutionSpread,
    ThreeDimensionalSpread,
    ObjectSnapTolerance,
    ZoneGainCoefficient,
    ObjectDecorrelationCoefficient,
}

impl std::fmt::Display for UnrenderedFeature {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnrenderedFeature::BedRemap => write!(formatter, "a BedRemap"),
            UnrenderedFeature::BedChannelDecorrelation => {
                write!(formatter, "channel decorrelation")
            }
            UnrenderedFeature::ChannelId(channel_id) => {
                write!(formatter, "ChannelID {channel_id:#x}")
            }
            UnrenderedFeature::ObjectZoneDefinition19 => {
                write!(formatter, "an ObjectZoneDefinition19")
            }
            UnrenderedFeature::LowResolutionSpread => write!(formatter, "low resolution spread"),
            UnrenderedFeature::ThreeDimensionalSpread => write!(formatter, "3D spread"),
            UnrenderedFeature::ObjectSnapTolerance => write!(formatter, "a coded snap tolerance"),
            UnrenderedFeature::ZoneGainCoefficient => write!(formatter, "a coded zone gain"),
            UnrenderedFeature::ObjectDecorrelationCoefficient => {
                write!(formatter, "a coded decorrelation coefficient")
            }
        }
    }
}

pub struct Renderer {
    layout: OutputLayout,
    object_panner: ObjectPanner,
    // by object MetaID, the direct then the diffuse gains of its last sub-block
    last_object_gains: HashMap<u32, Vec<f32>>,
    // per channel, the last DIRECT_PATH_DELAY direct samples
    direct_history: Vec<Vec<f32>>,
    // per channel, the last FILTER_LENGTH - 1 diffuse samples
    diffuse_history: Vec<Vec<f32>>,
    decorrelation_filters: Vec<Vec<f32>>,
}

struct BedMix<'a> {
    samples: Option<&'a [i32]>,
    gains: Vec<f32>,
}

struct ObjectMix<'a> {
    meta_id: u32,
    samples: Option<&'a [i32]>,
    // per pan sub-block, the direct then the diffuse gains
    sub_block_gains: Vec<Vec<f32>>,
}

impl Renderer {
    pub fn new(layout: OutputLayout) -> Self {
        let loudspeakers = layout.loudspeakers();
        let mut renderer = Renderer {
            layout,
            object_panner: ObjectPanner::new(loudspeakers),
            last_object_gains: HashMap::new(),
            direct_history: Vec::new(),
            diffuse_history: Vec::new(),
            decorrelation_filters: decorrelation_filters(loudspeakers),
        };
        renderer.reset();
        renderer
    }

    pub fn reset(&mut self) {
        let channel_count = self.layout.channel_count();
        self.last_object_gains.clear();
        self.direct_history = vec![vec![0.0; DIRECT_PATH_DELAY]; channel_count];
        self.diffuse_history = vec![vec![0.0; FILTER_LENGTH - 1]; channel_count];
    }

    // the output lags the frame's audio by DIRECT_PATH_DELAY samples, as BS.2127 7.4 delays it
    pub fn render(
        &mut self,
        frame: &Frame,
        audio: &BTreeMap<u32, Vec<i32>>,
    ) -> Result<Vec<f32>, IabError> {
        let sample_count = frame.frame_rate.sample_count(frame.sample_rate);
        let sub_block_lengths = frame.frame_rate.pan_sub_block_lengths(frame.sample_rate);
        let target = self.layout.use_case();
        let loudspeakers = self.layout.loudspeakers();

        check_unique_meta_ids(&frame.beds, &bed_hierarchy())?;
        check_unique_meta_ids(&frame.objects, &object_hierarchy())?;
        let mut beds = Vec::new();
        for bed in rendered_elements(&frame.beds, target, &bed_hierarchy())? {
            beds.extend(bed_mixes(bed, target, loudspeakers, audio, sample_count)?);
        }
        let mut objects = Vec::new();
        for object in rendered_elements(&frame.objects, target, &object_hierarchy())? {
            objects.push(self.object_mix(object, frame.frame_rate, audio, sample_count)?);
        }

        let channel_count = loudspeakers.len();
        let mut direct: Vec<Vec<f32>> = self
            .direct_history
            .iter()
            .map(|history| [history.as_slice(), &vec![0.0; sample_count]].concat())
            .collect();
        let mut diffuse: Vec<Vec<f32>> = self
            .diffuse_history
            .iter()
            .map(|history| [history.as_slice(), &vec![0.0; sample_count]].concat())
            .collect();

        for bed in &beds {
            let Some(samples) = bed.samples else { continue };
            for (channel, gain) in bed.gains.iter().enumerate() {
                if *gain == 0.0 {
                    continue;
                }
                let bus = &mut direct[channel][DIRECT_PATH_DELAY..];
                for (output, sample) in bus.iter_mut().zip(samples) {
                    *output += gain * sample_value(*sample);
                }
            }
        }

        let mut last_object_gains = HashMap::new();
        for object in &objects {
            let mut previous = self
                .last_object_gains
                .get(&object.meta_id)
                .unwrap_or(&object.sub_block_gains[0]);
            let mut start = 0;
            for (gains, length) in object.sub_block_gains.iter().zip(&sub_block_lengths) {
                if let Some(samples) = object.samples {
                    let samples = &samples[start..start + length];
                    for channel in 0..channel_count {
                        mix_ramp(
                            &mut direct[channel][DIRECT_PATH_DELAY + start..],
                            samples,
                            previous[channel],
                            gains[channel],
                        );
                        mix_ramp(
                            &mut diffuse[channel][FILTER_LENGTH - 1 + start..],
                            samples,
                            previous[channel_count + channel],
                            gains[channel_count + channel],
                        );
                    }
                }
                previous = gains;
                start += length;
            }
            last_object_gains.insert(object.meta_id, previous.clone());
        }
        self.last_object_gains = last_object_gains;

        let mut output = vec![0.0; sample_count * channel_count];
        for channel in 0..channel_count {
            let filter = &self.decorrelation_filters[channel];
            let diffuse_is_silent = diffuse[channel].iter().all(|sample| *sample == 0.0);
            for index in 0..sample_count {
                let decorrelated: f32 = if diffuse_is_silent {
                    0.0
                } else {
                    diffuse[channel][index..index + FILTER_LENGTH]
                        .iter()
                        .rev()
                        .zip(filter)
                        .map(|(sample, tap)| sample * tap)
                        .sum()
                };
                output[index * channel_count + channel] = direct[channel][index] + decorrelated;
            }
            self.direct_history[channel] = direct[channel][sample_count..].to_vec();
            self.diffuse_history[channel] = diffuse[channel][sample_count..].to_vec();
        }
        Ok(output)
    }

    fn object_mix<'a>(
        &mut self,
        object: &ObjectDefinition,
        frame_rate: FrameRate,
        audio: &'a BTreeMap<u32, Vec<i32>>,
        sample_count: usize,
    ) -> Result<ObjectMix<'a>, IabError> {
        let unrendered = |feature| not_rendered("ObjectDefinition", object.meta_id, feature);
        if !object.zone_definitions_19.is_empty() {
            return Err(unrendered(UnrenderedFeature::ObjectZoneDefinition19));
        }
        if object.pan_sub_blocks.len() != frame_rate.pan_sub_block_count() {
            return Err(IabError::Invalid {
                element: "ObjectDefinition",
                field: "NumPanSubBlocks",
                value: object.pan_sub_blocks.len() as u64,
                rule: "ST 2098-2 Table 23 sets one pan sub-block count for each frame rate",
            });
        }
        let mut sub_block_gains: Vec<Vec<f32>> = Vec::new();
        let mut previous_pan: Option<&ObjectPan> = None;
        for pan in &object.pan_sub_blocks {
            if previous_pan == Some(pan) {
                let repeated = sub_block_gains
                    .last()
                    .expect("a sub-block came before")
                    .clone();
                sub_block_gains.push(repeated);
                continue;
            }
            let gain = gain_value(pan.gain, "ObjectDefinition", "ObjectGainPrefix")?;
            let parameters = object_parameters(pan, object.meta_id)?;
            let gains = self.object_panner.gains(&parameters);
            sub_block_gains.push(
                gains
                    .direct
                    .iter()
                    .chain(&gains.diffuse)
                    .map(|value| (value * gain) as f32)
                    .collect(),
            );
            previous_pan = Some(pan);
        }
        Ok(ObjectMix {
            meta_id: object.meta_id,
            samples: audio_samples(
                audio,
                "ObjectDefinition",
                object.audio_data_id,
                sample_count,
            )?,
            sub_block_gains,
        })
    }
}

fn mix_ramp(bus: &mut [f32], samples: &[i32], from: f32, to: f32) {
    if from == 0.0 && to == 0.0 {
        return;
    }
    let step = (to - from) / samples.len() as f32;
    for (index, (output, sample)) in bus.iter_mut().zip(samples).enumerate() {
        *output += (from + step * index as f32) * sample_value(*sample);
    }
}

// decode_audio leaves the coded bits at the top of 32
fn sample_value(sample: i32) -> f32 {
    sample as f32 / 2_147_483_648.0
}

fn not_rendered(element: &'static str, meta_id: u32, feature: UnrenderedFeature) -> IabError {
    IabError::NotRendered {
        element,
        meta_id,
        feature,
    }
}

fn audio_samples<'a>(
    audio: &'a BTreeMap<u32, Vec<i32>>,
    element: &'static str,
    audio_data_id: u32,
    sample_count: usize,
) -> Result<Option<&'a [i32]>, IabError> {
    // ST 2098-2 10.3.6: AudioDataID 0 is no audio asset
    if audio_data_id == 0 {
        return Ok(None);
    }
    let Some(samples) = audio.get(&audio_data_id) else {
        return Err(IabError::Invalid {
            element,
            field: "AudioDataID",
            value: u64::from(audio_data_id),
            rule: "the IAFrame carries no audio data with this AudioDataID",
        });
    };
    if samples.len() != sample_count {
        return Err(IabError::Invalid {
            element,
            field: "AudioDataID",
            value: u64::from(audio_data_id),
            rule: "the audio data holds a different sample count than the IAFrame",
        });
    }
    Ok(Some(samples))
}

// ST 2098-2 5.5
fn gain_value(gain: Gain, element: &'static str, field: &'static str) -> Result<f64, IabError> {
    const MUTE_CODE: u16 = 0x3FF;
    match gain {
        Gain::Unity => Ok(1.0),
        Gain::Silent | Gain::Coded(MUTE_CODE) => Ok(0.0),
        Gain::Coded(code) => Ok(2f64.powf(-f64::from(code) / 64.0)),
        Gain::Reserved(_) => Err(reserved_prefix(element, field)),
    }
}

// ST 2098-2 Tables 20, 21, 25 and 27 all reserve prefix code 3
fn reserved_prefix(element: &'static str, field: &'static str) -> IabError {
    IabError::Invalid {
        element,
        field,
        value: 3,
        rule: "prefix code 3 is reserved",
    }
}

// ST 2098-2 11.1 puts y = 0 at the front wall, ADM puts Y = 1 there
pub(super) fn adm_position(position: ObjectPosition) -> Result<Cartesian, IabError> {
    // ST 2098-2 5.4 DistanceXY for 16 bit codes, defined from 32767 up
    const DISTANCE_XY_OFFSET: u16 = 32_767;
    let distance_xy = |code: u16, field: &'static str| {
        if code < DISTANCE_XY_OFFSET {
            return Err(IabError::Invalid {
                element: "ObjectDefinition",
                field,
                value: u64::from(code),
                rule: "ST 2098-2 5.4 DistanceXY codes run from 32767 to 65535",
            });
        }
        Ok(f64::from(code - DISTANCE_XY_OFFSET) / 32_768.0)
    };
    Ok(Cartesian {
        x: 2.0 * distance_xy(position.x, "ObjectPosX")? - 1.0,
        y: 1.0 - 2.0 * distance_xy(position.y, "ObjectPosY")?,
        z: f64::from(position.z) / 65_535.0,
    })
}

pub(super) fn object_parameters(
    pan: &ObjectPan,
    meta_id: u32,
) -> Result<ObjectParameters, IabError> {
    let element = "ObjectDefinition";
    let unrendered = |feature| not_rendered(element, meta_id, feature);
    let reserved = |field| reserved_prefix(element, field);
    // ST 2098-2 10.5.16 spreads S / 2 either side in the cube, a BS.2127 half size of S in X and Y and S / 2 in Z
    let extent = match pan.spread {
        ObjectSpread::PointSource => Cartesian {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        ObjectSpread::OneDimension(code) => {
            let spread = f64::from(code) / 4_095.0;
            Cartesian {
                x: spread,
                y: spread,
                z: spread / 2.0,
            }
        }
        ObjectSpread::LowResolution(_) => {
            return Err(unrendered(UnrenderedFeature::LowResolutionSpread));
        }
        ObjectSpread::ThreeDimensions { .. } => {
            return Err(unrendered(UnrenderedFeature::ThreeDimensionalSpread));
        }
    };
    let channel_lock = match pan.snap {
        ObjectSnap::Off => false,
        // DEFAULT_OBJ_SNAP_TOL is 1.0, which snaps from anywhere, as a lock with no maxDistance does
        ObjectSnap::DefaultTolerance => true,
        ObjectSnap::Tolerance(_) => return Err(unrendered(UnrenderedFeature::ObjectSnapTolerance)),
    };
    let mut excluded_zones = [false; 9];
    if let Some(zone_gains) = &pan.zone_gains {
        for (excluded_zone, zone_gain) in excluded_zones.iter_mut().zip(zone_gains) {
            *excluded_zone = match zone_gain {
                ZoneGain::Silent => true,
                ZoneGain::Full => false,
                ZoneGain::Coded(_) => {
                    return Err(unrendered(UnrenderedFeature::ZoneGainCoefficient));
                }
                ZoneGain::Reserved(_) => return Err(reserved("ZoneGainPrefix")),
            };
        }
    }
    let diffuse = match pan.decorrelation {
        Decorrelation::Off => false,
        Decorrelation::Maximum => true,
        Decorrelation::Coded(_) => {
            return Err(unrendered(
                UnrenderedFeature::ObjectDecorrelationCoefficient,
            ));
        }
        Decorrelation::Reserved(_) => return Err(reserved("ObjectDecorCoefPrefix")),
    };
    Ok(ObjectParameters {
        position: adm_position(pan.position)?,
        extent,
        excluded_zones,
        channel_lock,
        diffuse,
    })
}

fn bed_mixes<'a>(
    bed: &BedDefinition,
    target: u8,
    loudspeakers: &[Loudspeaker],
    audio: &'a BTreeMap<u32, Vec<i32>>,
    sample_count: usize,
) -> Result<Vec<BedMix<'a>>, IabError> {
    let element = "BedDefinition";
    let unrendered = |feature| not_rendered(element, bed.meta_id, feature);
    if bed
        .remaps
        .iter()
        .any(|remap| is_activated(Some(remap.use_case), target))
    {
        return Err(unrendered(UnrenderedFeature::BedRemap));
    }
    let mut mixes = Vec::with_capacity(bed.channels.len());
    for channel in &bed.channels {
        match channel.decorrelation {
            None | Some(Decorrelation::Off) => {}
            Some(_) => return Err(unrendered(UnrenderedFeature::BedChannelDecorrelation)),
        }
        let gain = gain_value(channel.gain, element, "ChannelGainPrefix")?;
        let gains = bed_channel_gains(loudspeakers, channel.channel_id)
            .ok_or_else(|| unrendered(UnrenderedFeature::ChannelId(channel.channel_id)))?;
        mixes.push(BedMix {
            samples: audio_samples(audio, element, channel.audio_data_id, sample_count)?,
            gains: gains.iter().map(|value| (value * gain) as f32).collect(),
        });
    }
    Ok(mixes)
}

struct Hierarchy<T> {
    element: &'static str,
    meta_id: fn(&T) -> u32,
    use_case: fn(&T) -> Option<u8>,
    children: fn(&T) -> &[T],
}

fn bed_hierarchy() -> Hierarchy<BedDefinition> {
    Hierarchy {
        element: "BedDefinition",
        meta_id: |bed| bed.meta_id,
        use_case: |bed| bed.use_case,
        children: |bed| &bed.child_beds,
    }
}

fn object_hierarchy() -> Hierarchy<ObjectDefinition> {
    Hierarchy {
        element: "ObjectDefinition",
        meta_id: |object| object.meta_id,
        use_case: |object| object.use_case,
        children: |object| &object.child_objects,
    }
}

// ST 2098-2 10.3.1, children included
fn check_unique_meta_ids<T>(elements: &[T], hierarchy: &Hierarchy<T>) -> Result<(), IabError> {
    let mut seen = HashSet::new();
    let mut pending: Vec<&T> = elements.iter().collect();
    while let Some(element) = pending.pop() {
        let meta_id = (hierarchy.meta_id)(element);
        if !seen.insert(meta_id) {
            return Err(IabError::Invalid {
                element: hierarchy.element,
                field: "MetaID",
                value: u64::from(meta_id),
                rule: "no two elements of one ElementID share a MetaID within an IAFrame",
            });
        }
        pending.extend((hierarchy.children)(element));
    }
    Ok(())
}

fn is_activated(use_case: Option<u8>, target: u8) -> bool {
    match use_case {
        None => true,
        Some(code) => code == target || code == ALWAYS_USE,
    }
}

// ST 2098-2 10.3.2 and 10.5.1: the rendered element is the deepest activated one under activated ancestors
fn rendered_elements<'a, T>(
    elements: &'a [T],
    target: u8,
    hierarchy: &Hierarchy<T>,
) -> Result<Vec<&'a T>, IabError> {
    let activated = |element: &T| is_activated((hierarchy.use_case)(element), target);
    let mut rendered = Vec::new();
    for element in elements.iter().filter(|element| activated(element)) {
        let mut current = element;
        loop {
            let activated_children: Vec<&T> = (hierarchy.children)(current)
                .iter()
                .filter(|child| activated(child))
                .collect();
            match activated_children[..] {
                [] => break,
                [child] => current = child,
                _ => {
                    return Err(IabError::Invalid {
                        element: hierarchy.element,
                        field: "SubElementCount",
                        value: activated_children.len() as u64,
                        rule: "at most one child element is activated",
                    });
                }
            }
        }
        rendered.push(current);
    }
    Ok(rendered)
}

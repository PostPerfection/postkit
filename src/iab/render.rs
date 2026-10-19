use std::collections::{BTreeMap, HashMap};

use super::decorrelation::{DIRECT_PATH_DELAY, FILTER_LENGTH, decorrelation_filters};
use super::direct_speakers::bed_channel_gains;
use super::panner::{
    Cartesian, FIVE_POINT_ONE_LOUDSPEAKERS, Loudspeaker, ObjectPanner, ObjectParameters,
    SEVEN_POINT_ONE_LOUDSPEAKERS, STEREO_LOUDSPEAKERS,
};
use super::{
    BedDefinition, Decorrelation, Frame, Gain, IabError, ObjectDefinition, ObjectPan, ObjectSnap,
    ObjectSpread, ZoneGain,
};

// ITU-R BS.2051 layouts with the BS.2127 11.2 positions; the channel order is
// L R for stereo, L R C LFE Ls Rs for 5.1 and L R C LFE Lss Rss Lrs Rrs for 7.1
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

    // the ST 2098-2 Table 1 UseCase of the layout: stereo is BS.2051 sound
    // system A (0+2+0), 5.1 and 7.1 are the ST 428-12 5.1 and 7.1DS
    fn use_case(self) -> u8 {
        match self {
            OutputLayout::Stereo => 0x30,
            OutputLayout::FivePointOne => 0x01,
            OutputLayout::SevenPointOne => 0x02,
        }
    }
}

const ALWAYS_USE: u8 = 0xFF;

// what SMPTE RDD 57 IAB Application Profile 1 leaves out of a stream
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcludedFeature {
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

impl std::fmt::Display for ExcludedFeature {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExcludedFeature::BedRemap => write!(formatter, "a BedRemap"),
            ExcludedFeature::BedChannelDecorrelation => {
                write!(formatter, "channel decorrelation")
            }
            ExcludedFeature::ChannelId(channel_id) => {
                write!(formatter, "ChannelID {channel_id:#x}")
            }
            ExcludedFeature::ObjectZoneDefinition19 => {
                write!(formatter, "an ObjectZoneDefinition19")
            }
            ExcludedFeature::LowResolutionSpread => write!(formatter, "low resolution spread"),
            ExcludedFeature::ThreeDimensionalSpread => write!(formatter, "3D spread"),
            ExcludedFeature::ObjectSnapTolerance => write!(formatter, "a coded snap tolerance"),
            ExcludedFeature::ZoneGainCoefficient => write!(formatter, "a coded zone gain"),
            ExcludedFeature::ObjectDecorrelationCoefficient => {
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

    // interleaved samples at the frame's sample rate, DIRECT_PATH_DELAY samples
    // behind the frame's audio as BS.2127 7.4 delays every output
    pub fn render(
        &mut self,
        frame: &Frame,
        audio: &BTreeMap<u32, Vec<i32>>,
    ) -> Result<Vec<f32>, IabError> {
        let sample_count = frame.frame_rate.sample_count(frame.sample_rate);
        let sub_block_lengths = frame.frame_rate.pan_sub_block_lengths(frame.sample_rate);
        let target = self.layout.use_case();
        let loudspeakers = self.layout.loudspeakers();

        let mut beds = Vec::new();
        for bed in rendered_elements(&frame.beds, target, &bed_hierarchy())? {
            beds.extend(bed_mixes(bed, loudspeakers, audio, sample_count)?);
        }
        let mut objects = Vec::new();
        for object in rendered_elements(&frame.objects, target, &object_hierarchy())? {
            objects.push(self.object_mix(object, audio, sample_count)?);
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
        audio: &'a BTreeMap<u32, Vec<i32>>,
        sample_count: usize,
    ) -> Result<ObjectMix<'a>, IabError> {
        let excluded = |feature| outside_profile_1("ObjectDefinition", object.meta_id, feature);
        if !object.zone_definitions_19.is_empty() {
            return Err(excluded(ExcludedFeature::ObjectZoneDefinition19));
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
            let gain = gain_value(
                pan.gain,
                "ObjectDefinition",
                object.meta_id,
                "ObjectGainPrefix",
            )?;
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

fn outside_profile_1(element: &'static str, meta_id: u32, feature: ExcludedFeature) -> IabError {
    IabError::OutsideProfile1 {
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
fn gain_value(
    gain: Gain,
    element: &'static str,
    meta_id: u32,
    field: &'static str,
) -> Result<f64, IabError> {
    const MUTE_CODE: u16 = 0x3FF;
    match gain {
        Gain::Unity => Ok(1.0),
        Gain::Silent | Gain::Coded(MUTE_CODE) => Ok(0.0),
        Gain::Coded(code) => Ok(2f64.powf(-f64::from(code) / 64.0)),
        Gain::Reserved(_) => Err(IabError::Invalid {
            element,
            field,
            value: u64::from(meta_id),
            rule: "prefix code 3 is reserved",
        }),
    }
}

// SMPTE ST 2098-2 11.1 to ADM Cartesian: the cube's x runs left to right and y
// front to back from 0 to 1, ADM X and Y run from -1 to 1 with Y = 1 at the
// front, and both put z = 0 on the main loudspeaker plane
pub(super) fn adm_position(x_code: u16, y_code: u16, z_code: u16) -> Cartesian {
    // ST 2098-2 5.4 DistanceXY and DistanceZ for 16 bit codes
    let distance_xy = |code: u16| (f64::from(code) - 32_767.0) / 32_768.0;
    Cartesian {
        x: 2.0 * distance_xy(x_code) - 1.0,
        y: 1.0 - 2.0 * distance_xy(y_code),
        z: f64::from(z_code) / 65_535.0,
    }
}

pub(super) fn object_parameters(
    pan: &ObjectPan,
    meta_id: u32,
) -> Result<ObjectParameters, IabError> {
    let element = "ObjectDefinition";
    let excluded = |feature| outside_profile_1(element, meta_id, feature);
    let reserved = |field| IabError::Invalid {
        element,
        field,
        value: u64::from(meta_id),
        rule: "prefix code 3 is reserved",
    };
    // ObjectSpread S spans S / 2 either side in the unit cube, which is S in ADM
    // units, and ADM Cartesian extent is that half size (BS.2127 10.2.1 wd2xyz)
    let extent = match pan.spread {
        ObjectSpread::PointSource => 0.0,
        ObjectSpread::OneDimension(code) => f64::from(code) / 4_095.0,
        ObjectSpread::LowResolution(_) => {
            return Err(excluded(ExcludedFeature::LowResolutionSpread));
        }
        ObjectSpread::ThreeDimensions { .. } => {
            return Err(excluded(ExcludedFeature::ThreeDimensionalSpread));
        }
    };
    let channel_lock = match pan.snap {
        ObjectSnap::Off => false,
        // DEFAULT_OBJ_SNAP_TOL is 1.0, which snaps from anywhere, as a lock with no maxDistance does
        ObjectSnap::DefaultTolerance => true,
        ObjectSnap::Tolerance(_) => return Err(excluded(ExcludedFeature::ObjectSnapTolerance)),
    };
    let mut excluded_zones = [false; 9];
    if let Some(zone_gains) = &pan.zone_gains {
        for (excluded_zone, zone_gain) in excluded_zones.iter_mut().zip(zone_gains) {
            *excluded_zone = match zone_gain {
                ZoneGain::Silent => true,
                ZoneGain::Full => false,
                ZoneGain::Coded(_) => return Err(excluded(ExcludedFeature::ZoneGainCoefficient)),
                ZoneGain::Reserved(_) => return Err(reserved("ZoneGainPrefix")),
            };
        }
    }
    let diffuse = match pan.decorrelation {
        Decorrelation::Off => false,
        Decorrelation::Maximum => true,
        Decorrelation::Coded(_) => {
            return Err(excluded(ExcludedFeature::ObjectDecorrelationCoefficient));
        }
        Decorrelation::Reserved(_) => return Err(reserved("ObjectDecorCoefPrefix")),
    };
    Ok(ObjectParameters {
        position: adm_position(pan.position.x, pan.position.y, pan.position.z),
        extent,
        excluded_zones,
        channel_lock,
        diffuse,
    })
}

fn bed_mixes<'a>(
    bed: &BedDefinition,
    loudspeakers: &[Loudspeaker],
    audio: &'a BTreeMap<u32, Vec<i32>>,
    sample_count: usize,
) -> Result<Vec<BedMix<'a>>, IabError> {
    let element = "BedDefinition";
    let excluded = |feature| outside_profile_1(element, bed.meta_id, feature);
    if !bed.remaps.is_empty() {
        return Err(excluded(ExcludedFeature::BedRemap));
    }
    let mut mixes = Vec::with_capacity(bed.channels.len());
    for channel in &bed.channels {
        match channel.decorrelation {
            None | Some(Decorrelation::Off) => {}
            Some(_) => return Err(excluded(ExcludedFeature::BedChannelDecorrelation)),
        }
        let gain = gain_value(channel.gain, element, bed.meta_id, "ChannelGainPrefix")?;
        let gains = bed_channel_gains(loudspeakers, channel.channel_id)
            .ok_or_else(|| excluded(ExcludedFeature::ChannelId(channel.channel_id)))?;
        mixes.push(BedMix {
            samples: audio_samples(audio, element, channel.audio_data_id, sample_count)?,
            gains: gains.iter().map(|value| (value * gain) as f32).collect(),
        });
    }
    Ok(mixes)
}

struct Hierarchy<T> {
    element: &'static str,
    use_case: fn(&T) -> Option<u8>,
    children: fn(&T) -> &[T],
}

fn bed_hierarchy() -> Hierarchy<BedDefinition> {
    Hierarchy {
        element: "BedDefinition",
        use_case: |bed| bed.use_case,
        children: |bed| &bed.child_beds,
    }
}

fn object_hierarchy() -> Hierarchy<ObjectDefinition> {
    Hierarchy {
        element: "ObjectDefinition",
        use_case: |object| object.use_case,
        children: |object| &object.child_objects,
    }
}

// ST 2098-2 10.3.2 and 10.5.1: an element is activated when it is not conditional
// or its UseCase is the target's or Always Use, and the rendered one is the
// deepest activated element down a chain of activated ancestors
fn rendered_elements<'a, T>(
    elements: &'a [T],
    target: u8,
    hierarchy: &Hierarchy<T>,
) -> Result<Vec<&'a T>, IabError> {
    let activated = |element: &T| match (hierarchy.use_case)(element) {
        None => true,
        Some(code) => code == target || code == ALWAYS_USE,
    };
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

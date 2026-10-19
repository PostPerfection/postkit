mod bit_reader;
mod decorrelation;
mod direct_speakers;
mod dlc;
mod panner;
mod parse;
mod reader;
mod render;

use std::collections::BTreeMap;
use std::sync::Arc;

pub use decorrelation::{DIRECT_PATH_DELAY, FILTER_LENGTH};
pub use parse::parse_frame;
pub use reader::{AtmosReader, atmos_writer_info};
pub use render::{OutputLayout, Renderer, UnrenderedFeature};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IabError {
    #[error("IAB {element} {field} runs past the end of the element")]
    Truncated {
        element: &'static str,
        field: &'static str,
    },
    #[error("IAB {element} {field} is {value}: {rule}")]
    Invalid {
        element: &'static str,
        field: &'static str,
        value: u64,
        rule: &'static str,
    },
    #[error("IAB {element} {meta_id} uses {feature}, which the renderer does not render")]
    NotRendered {
        element: &'static str,
        meta_id: u32,
        feature: UnrenderedFeature,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRate {
    Hz48000,
    Hz96000,
}

impl SampleRate {
    pub fn hertz(self) -> u32 {
        match self {
            SampleRate::Hz48000 => 48_000,
            SampleRate::Hz96000 => 96_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    Bits16,
    Bits24,
}

impl BitDepth {
    pub fn bits(self) -> u32 {
        match self {
            BitDepth::Bits16 => 16,
            BitDepth::Bits24 => 24,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRate {
    Fps24,
    Fps25,
    Fps30,
    Fps48,
    Fps50,
    Fps60,
    Fps96,
    Fps100,
    Fps120,
    Fps24000Over1001,
}

impl FrameRate {
    // ST 2098-2 Table 18, the 96 kHz count is twice this one
    pub fn sample_count_48k(self) -> usize {
        match self {
            FrameRate::Fps24 => 2000,
            FrameRate::Fps25 => 1920,
            FrameRate::Fps30 => 1600,
            FrameRate::Fps48 => 1000,
            FrameRate::Fps50 => 960,
            FrameRate::Fps60 => 800,
            FrameRate::Fps96 => 500,
            FrameRate::Fps100 => 480,
            FrameRate::Fps120 => 400,
            FrameRate::Fps24000Over1001 => 2002,
        }
    }

    pub fn sample_count(self, sample_rate: SampleRate) -> usize {
        match sample_rate {
            SampleRate::Hz48000 => self.sample_count_48k(),
            SampleRate::Hz96000 => self.sample_count_48k() * 2,
        }
    }

    // ST 2098-2 Table 23
    pub fn pan_sub_block_count(self) -> usize {
        match self {
            FrameRate::Fps24
            | FrameRate::Fps25
            | FrameRate::Fps30
            | FrameRate::Fps24000Over1001 => 8,
            FrameRate::Fps48 | FrameRate::Fps50 | FrameRate::Fps60 => 4,
            FrameRate::Fps96 | FrameRate::Fps100 | FrameRate::Fps120 => 2,
        }
    }

    // ST 2098-2 Table 23, in bitstream order
    pub fn pan_sub_block_lengths(self, sample_rate: SampleRate) -> Vec<usize> {
        match (self, sample_rate) {
            (FrameRate::Fps24000Over1001, SampleRate::Hz48000) => {
                vec![251, 250, 250, 250, 251, 250, 250, 250]
            }
            (FrameRate::Fps24000Over1001, SampleRate::Hz96000) => {
                vec![501, 500, 501, 500, 501, 500, 501, 500]
            }
            _ => {
                let count = self.pan_sub_block_count();
                vec![self.sample_count(sample_rate) / count; count]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub sample_rate: SampleRate,
    pub bit_depth: BitDepth,
    pub frame_rate: FrameRate,
    pub max_rendered: u32,
    pub beds: Vec<BedDefinition>,
    pub objects: Vec<ObjectDefinition>,
    pub dlc_audio: Vec<AudioDataDlc>,
    pub pcm_audio: Vec<AudioDataPcm>,
    pub authoring_tool_uris: Vec<Vec<u8>>,
    pub user_data: Vec<UserData>,
}

// ST 2098-2 Table 20, the reserved prefix still carries a gain code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gain {
    Unity,
    Silent,
    Coded(u16),
    Reserved(u16),
}

// ST 2098-2 Table 25, zone prefixes 0 and 1 mean the opposite of Table 20
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneGain {
    Silent,
    Full,
    Coded(u16),
    Reserved(u16),
}

// ST 2098-2 Tables 21 and 27
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decorrelation {
    Off,
    Maximum,
    Coded(u8),
    Reserved(u8),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BedDefinition {
    pub meta_id: u32,
    pub use_case: Option<u8>,
    pub channels: Vec<BedChannel>,
    pub audio_description: u8,
    pub audio_description_text: Option<Vec<u8>>,
    pub child_beds: Vec<BedDefinition>,
    pub remaps: Vec<BedRemap>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BedChannel {
    pub channel_id: u32,
    pub audio_data_id: u32,
    pub gain: Gain,
    pub decorrelation: Option<Decorrelation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BedRemap {
    pub meta_id: u32,
    pub use_case: u8,
    pub source_channel_count: u32,
    pub destination_channel_count: u32,
    // one per pan sub block, a sub block without an update repeats the one before
    pub sub_blocks: Vec<Arc<[RemapDestination]>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemapDestination {
    pub channel_id: u32,
    pub source_gains: Vec<Gain>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDefinition {
    pub meta_id: u32,
    pub audio_data_id: u32,
    pub use_case: Option<u8>,
    // one per pan sub block, a sub block without an update repeats the one before
    pub pan_sub_blocks: Vec<ObjectPan>,
    pub audio_description: u8,
    pub audio_description_text: Option<Vec<u8>>,
    pub child_objects: Vec<ObjectDefinition>,
    pub zone_definitions_19: Vec<ObjectZoneDefinition19>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPan {
    pub gain: Gain,
    pub position: ObjectPosition,
    pub snap: ObjectSnap,
    pub zone_gains: Option<[ZoneGain; 9]>,
    pub spread: ObjectSpread,
    pub decorrelation: Decorrelation,
}

// the raw 16 bit codes, ST 2098-2 5.4 maps them into the unit cube
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectPosition {
    pub x: u16,
    pub y: u16,
    pub z: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSnap {
    Off,
    DefaultTolerance,
    Tolerance(u16),
}

// ST 2098-2 Table 26
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSpread {
    LowResolution(u8),
    PointSource,
    OneDimension(u16),
    ThreeDimensions { x: u16, y: u16, z: u16 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectZoneDefinition19 {
    // one per pan sub block, a sub block without an update repeats the one before
    pub sub_blocks: Vec<[ZoneGain; 19]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioDataDlc {
    pub audio_data_id: u32,
    // the DLCSize bytes after the DLCSize field
    pub coded: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioDataPcm {
    pub audio_data_id: u32,
    // SampleCount little-endian samples of the frame's bit depth
    pub sample_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserData {
    pub user_id: [u8; 16],
    pub bytes: Vec<u8>,
}

// samples are 32 bit with the coded bits at the top, as ST 2098-2 B.10 leaves DLC output
pub fn decode_audio(frame: &Frame) -> Result<BTreeMap<u32, Vec<i32>>, IabError> {
    let mut decoded = BTreeMap::new();
    for element in &frame.dlc_audio {
        let samples = dlc::decode(&element.coded, frame.sample_rate, frame.frame_rate)?;
        insert_unique(&mut decoded, "AudioDataDLC", element.audio_data_id, samples)?;
    }
    for element in &frame.pcm_audio {
        let samples = pcm_samples(&element.sample_bytes, frame.bit_depth);
        insert_unique(&mut decoded, "AudioDataPCM", element.audio_data_id, samples)?;
    }
    Ok(decoded)
}

fn insert_unique(
    decoded: &mut BTreeMap<u32, Vec<i32>>,
    element: &'static str,
    audio_data_id: u32,
    samples: Vec<i32>,
) -> Result<(), IabError> {
    if decoded.insert(audio_data_id, samples).is_some() {
        return Err(IabError::Invalid {
            element,
            field: "AudioDataID",
            value: u64::from(audio_data_id),
            rule: "an AudioDataID is unique within an IAFrame",
        });
    }
    Ok(())
}

fn pcm_samples(sample_bytes: &[u8], bit_depth: BitDepth) -> Vec<i32> {
    match bit_depth {
        BitDepth::Bits16 => sample_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| i32::from(i16::from_le_bytes(*sample)) << 16)
            .collect(),
        BitDepth::Bits24 => sample_bytes
            .as_chunks::<3>()
            .0
            .iter()
            .map(|[low, middle, high]| i32::from_le_bytes([0, *low, *middle, *high]))
            .collect(),
    }
}

#[cfg(test)]
mod render_tests;
#[cfg(test)]
pub(crate) mod tests;

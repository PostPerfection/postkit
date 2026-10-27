use super::bit_reader::BitReader;
use super::{
    AudioDataDlc, AudioDataPcm, BedChannel, BedDefinition, BedRemap, BitDepth, Decorrelation,
    Frame, FrameRate, Gain, IabError, ObjectDefinition, ObjectPan, ObjectPosition, ObjectSnap,
    ObjectSpread, ObjectZoneDefinition19, RemapDestination, SampleRate, UserData, ZoneGain,
};
use std::sync::Arc;

// ST 2098-2 Table 14
const IA_FRAME: u32 = 0x08;
const BED_DEFINITION: u32 = 0x10;
const BED_REMAP: u32 = 0x20;
const OBJECT_DEFINITION: u32 = 0x40;
const OBJECT_ZONE_DEFINITION_19: u32 = 0x80;
const AUTHORING_TOOL_INFO: u32 = 0x100;
const USER_DATA: u32 = 0x101;
const AUDIO_DATA_DLC: u32 = 0x200;
const AUDIO_DATA_PCM: u32 = 0x400;

// Annex C allows two levels, the margin lets a stream that breaks that rule still parse
const MAXIMUM_NESTING_DEPTH: u32 = 16;

pub fn parse_frame(bytes: &[u8]) -> Result<Frame, IabError> {
    let mut reader = BitReader::new(bytes, "IABitstream frame");
    let ia_frame = read_bitstream_frame(&mut reader)?;
    let trailing_bytes = reader.remaining_bytes();
    if trailing_bytes != 0 {
        return Err(reader.invalid(
            "IAFrameLength",
            trailing_bytes as u64,
            "no bytes follow the IAFrame in an IABitstream frame",
        ));
    }

    let mut ia_frame_reader = BitReader::new(ia_frame, "IAFrame");
    let (element_id, payload) = read_element(&mut ia_frame_reader)?;
    if element_id != IA_FRAME {
        return Err(ia_frame_reader.invalid(
            "ElementID",
            element_id.into(),
            "the IAFrame ElementID is 0x08",
        ));
    }
    ia_frame_reader.expect_end("IAFrameLength")?;
    parse_ia_frame(payload)
}

// a .iab file is IABitstream frames back to back
pub fn split_frames(stream: &[u8]) -> Result<Vec<&[u8]>, IabError> {
    let mut reader = BitReader::new(stream, "IABitstream frame");
    let mut frames = Vec::new();
    while reader.remaining_bytes() > 0 {
        let start = stream.len() - reader.remaining_bytes();
        read_bitstream_frame(&mut reader)?;
        frames.push(&stream[start..stream.len() - reader.remaining_bytes()]);
    }
    Ok(frames)
}

fn read_bitstream_frame<'a>(reader: &mut BitReader<'a>) -> Result<&'a [u8], IabError> {
    let preamble_tag = reader.read(8, "PreambleTag")?;
    if preamble_tag != 0x01 {
        return Err(reader.invalid(
            "PreambleTag",
            preamble_tag.into(),
            "the PreambleTag is 0x01",
        ));
    }
    let preamble_length = reader.read(32, "PreambleLength")?;
    reader.read_bytes(preamble_length as usize, "PreambleValue")?;
    let ia_frame_tag = reader.read(8, "IAFrameTag")?;
    if ia_frame_tag != 0x02 {
        return Err(reader.invalid("IAFrameTag", ia_frame_tag.into(), "the IAFrameTag is 0x02"));
    }
    let ia_frame_length = reader.read(32, "IAFrameLength")?;
    reader.read_bytes(ia_frame_length as usize, "IAFrame")
}

fn read_element<'a>(reader: &mut BitReader<'a>) -> Result<(u32, &'a [u8]), IabError> {
    let element_id = reader.read_plex(8, "ElementID")?;
    let element_size = reader.read_plex(8, "ElementSize")?;
    let payload = reader.read_bytes(element_size as usize, "ElementSize")?;
    Ok((element_id, payload))
}

fn parse_ia_frame(payload: &[u8]) -> Result<Frame, IabError> {
    let mut reader = BitReader::new(payload, "IAFrame");
    let version = reader.read(8, "Version")?;
    if version != 1 {
        return Err(reader.invalid(
            "Version",
            version.into(),
            "this edition defines Version 1 only",
        ));
    }
    let sample_rate = match reader.read(2, "SampleRate")? {
        0 => SampleRate::Hz48000,
        1 => SampleRate::Hz96000,
        code => {
            return Err(reader.invalid("SampleRate", code.into(), "codes 2 and 3 are reserved"));
        }
    };
    let bit_depth = match reader.read(2, "BitDepth")? {
        0 => BitDepth::Bits16,
        1 => BitDepth::Bits24,
        code => return Err(reader.invalid("BitDepth", code.into(), "codes 2 and 3 are reserved")),
    };
    let frame_rate = match reader.read(4, "FrameRate")? {
        0x0 => FrameRate::Fps24,
        0x1 => FrameRate::Fps25,
        0x2 => FrameRate::Fps30,
        0x3 => FrameRate::Fps48,
        0x4 => FrameRate::Fps50,
        0x5 => FrameRate::Fps60,
        0x6 => FrameRate::Fps96,
        0x7 => FrameRate::Fps100,
        0x8 => FrameRate::Fps120,
        0x9 => FrameRate::Fps24000Over1001,
        code => {
            return Err(reader.invalid("FrameRate", code.into(), "codes 0xA to 0xF are reserved"));
        }
    };
    let max_rendered = reader.read_plex(8, "MaxRendered")?;
    reader.align();

    let mut frame = Frame {
        sample_rate,
        bit_depth,
        frame_rate,
        max_rendered,
        beds: Vec::new(),
        objects: Vec::new(),
        dlc_audio: Vec::new(),
        pcm_audio: Vec::new(),
        authoring_tool_uris: Vec::new(),
        user_data: Vec::new(),
        reserved_sub_element_count: 0,
    };
    let sub_element_count = reader.read_plex(8, "SubElementCount")?;
    for _ in 0..sub_element_count {
        let (element_id, payload) = read_element(&mut reader)?;
        match element_id {
            BED_DEFINITION => frame
                .beds
                .push(parse_bed_definition(payload, frame_rate, 0)?),
            OBJECT_DEFINITION => frame
                .objects
                .push(parse_object_definition(payload, frame_rate, 0)?),
            AUDIO_DATA_DLC if frame_rate == FrameRate::Fps24000Over1001 => {
                return Err(reader.invalid(
                    "ElementID",
                    u64::from(AUDIO_DATA_DLC),
                    "AudioDataDLC is not carried at a non-integer frame rate",
                ));
            }
            AUDIO_DATA_DLC => frame.dlc_audio.push(parse_audio_data_dlc(payload)?),
            AUDIO_DATA_PCM => frame.pcm_audio.push(parse_audio_data_pcm(
                payload,
                frame_rate.sample_count(sample_rate),
                bit_depth,
            )?),
            AUTHORING_TOOL_INFO => {
                let mut element_reader = BitReader::new(payload, "AuthoringToolInfo");
                let uri = element_reader.read_null_terminated("AuthoringToolURI")?;
                element_reader.expect_end("ElementSize")?;
                frame.authoring_tool_uris.push(uri);
            }
            USER_DATA => frame.user_data.push(parse_user_data(payload)?),
            // ST 2098-2 9.1: any other child is ignored
            IA_FRAME | BED_REMAP | OBJECT_ZONE_DEFINITION_19 => {}
            _ => frame.reserved_sub_element_count += 1,
        }
    }
    reader.expect_end("ElementSize")?;
    Ok(frame)
}

fn check_nesting_depth(reader: &BitReader, depth: u32) -> Result<(), IabError> {
    if depth > MAXIMUM_NESTING_DEPTH {
        return Err(reader.invalid(
            "SubElementCount",
            depth.into(),
            "elements nest at most 16 levels deep",
        ));
    }
    Ok(())
}

fn parse_bed_definition(
    payload: &[u8],
    frame_rate: FrameRate,
    depth: u32,
) -> Result<BedDefinition, IabError> {
    let mut reader = BitReader::new(payload, "BedDefinition");
    check_nesting_depth(&reader, depth)?;
    let meta_id = reader.read_plex(8, "MetaID")?;
    let use_case = if reader.read_flag("ConditionalBed")? {
        Some(reader.read(8, "BedUseCase")? as u8)
    } else {
        None
    };
    let channel_count = reader.read_plex(4, "ChannelCount")?;
    let mut channels = Vec::new();
    for _ in 0..channel_count {
        let channel_id = reader.read_plex(4, "ChannelID")?;
        let audio_data_id = reader.read_plex(8, "AudioDataID")?;
        let gain = read_gain(&mut reader, "ChannelGainPrefix", "ChannelGain")?;
        let decorrelation = if reader.read_flag("ChannelDecorInfoExists")? {
            reader.read(4, "Reserved")?;
            Some(read_decorrelation(
                &mut reader,
                "ChannelDecorCoefPrefix",
                "ChannelDecorCoef",
            )?)
        } else {
            None
        };
        channels.push(BedChannel {
            channel_id,
            audio_data_id,
            gain,
            decorrelation,
        });
    }
    reader.read(10, "Reserved")?;
    reader.align();
    let (audio_description, audio_description_text) = read_audio_description(&mut reader)?;

    let mut child_beds = Vec::new();
    let mut remaps = Vec::new();
    let mut ignored_sub_element_count = 0;
    let sub_element_count = reader.read_plex(8, "SubElementCount")?;
    for _ in 0..sub_element_count {
        let (element_id, payload) = read_element(&mut reader)?;
        match element_id {
            BED_DEFINITION => {
                child_beds.push(parse_bed_definition(payload, frame_rate, depth + 1)?)
            }
            BED_REMAP => remaps.push(parse_bed_remap(payload, frame_rate)?),
            // ST 2098-2 9.2: any other child is ignored
            _ => ignored_sub_element_count += 1,
        }
    }
    reader.expect_end("ElementSize")?;
    Ok(BedDefinition {
        meta_id,
        use_case,
        channels,
        audio_description,
        audio_description_text,
        child_beds,
        remaps,
        ignored_sub_element_count,
    })
}

fn parse_bed_remap(payload: &[u8], frame_rate: FrameRate) -> Result<BedRemap, IabError> {
    let mut reader = BitReader::new(payload, "BedRemap");
    let meta_id = reader.read_plex(8, "MetaID")?;
    let use_case = reader.read(8, "RemapUseCase")? as u8;
    let source_channel_count = reader.read_plex(4, "SourceChannels")?;
    let destination_channel_count = reader.read_plex(4, "DestinationChannels")?;

    let read_sub_block = |reader: &mut BitReader| -> Result<Arc<[RemapDestination]>, IabError> {
        let mut destinations = Vec::new();
        for _ in 0..destination_channel_count {
            let channel_id = reader.read_plex(4, "DestinationChannelID")?;
            let mut source_gains = Vec::new();
            for _ in 0..source_channel_count {
                source_gains.push(read_gain(reader, "RemapGainPrefix", "RemapGain")?);
            }
            destinations.push(RemapDestination {
                channel_id,
                source_gains,
            });
        }
        Ok(destinations.into())
    };
    let mut current = read_sub_block(&mut reader)?;
    let mut sub_blocks = vec![Arc::clone(&current)];
    let mut remap_info_exists = vec![true];
    for _ in 1..frame_rate.pan_sub_block_count() {
        let info_exists = reader.read_flag("RemapInfoExists")?;
        if info_exists {
            current = read_sub_block(&mut reader)?;
        }
        sub_blocks.push(Arc::clone(&current));
        remap_info_exists.push(info_exists);
    }
    reader.align();
    reader.read_plex(8, "Reserved")?;
    reader.expect_end("ElementSize")?;
    Ok(BedRemap {
        meta_id,
        use_case,
        source_channel_count,
        destination_channel_count,
        sub_blocks,
        remap_info_exists,
    })
}

fn parse_object_definition(
    payload: &[u8],
    frame_rate: FrameRate,
    depth: u32,
) -> Result<ObjectDefinition, IabError> {
    let mut reader = BitReader::new(payload, "ObjectDefinition");
    check_nesting_depth(&reader, depth)?;
    let meta_id = reader.read_plex(8, "MetaID")?;
    let audio_data_id = reader.read_plex(8, "AudioDataID")?;
    let use_case = if reader.read_flag("ConditionalObject")? {
        reader.read(1, "Reserved")?;
        Some(reader.read(8, "ObjectUseCase")? as u8)
    } else {
        None
    };
    reader.read(1, "Reserved")?;

    let mut current = read_object_pan(&mut reader)?;
    let mut pan_sub_blocks = vec![current.clone()];
    let mut pan_info_exists = vec![true];
    for _ in 1..frame_rate.pan_sub_block_count() {
        let info_exists = reader.read_flag("PanInfoExists")?;
        if info_exists {
            current = read_object_pan(&mut reader)?;
        }
        pan_sub_blocks.push(current.clone());
        pan_info_exists.push(info_exists);
    }
    reader.align();
    let (audio_description, audio_description_text) = read_audio_description(&mut reader)?;

    let mut child_objects = Vec::new();
    let mut zone_definitions_19 = Vec::new();
    let mut ignored_sub_element_count = 0;
    let sub_element_count = reader.read_plex(8, "SubElementCount")?;
    for _ in 0..sub_element_count {
        let (element_id, payload) = read_element(&mut reader)?;
        match element_id {
            OBJECT_DEFINITION => {
                child_objects.push(parse_object_definition(payload, frame_rate, depth + 1)?)
            }
            OBJECT_ZONE_DEFINITION_19 => {
                zone_definitions_19.push(parse_object_zone_definition_19(payload, frame_rate)?)
            }
            // ST 2098-2 9.4: any other child is ignored
            _ => ignored_sub_element_count += 1,
        }
    }
    reader.expect_end("ElementSize")?;
    Ok(ObjectDefinition {
        meta_id,
        audio_data_id,
        use_case,
        pan_sub_blocks,
        pan_info_exists,
        audio_description,
        audio_description_text,
        child_objects,
        zone_definitions_19,
        ignored_sub_element_count,
    })
}

fn read_object_pan(reader: &mut BitReader) -> Result<ObjectPan, IabError> {
    let gain = read_gain(reader, "ObjectGainPrefix", "ObjectGain")?;
    reader.read(3, "Reserved")?;
    let position = ObjectPosition {
        x: reader.read(16, "ObjectPosX")? as u16,
        y: reader.read(16, "ObjectPosY")? as u16,
        z: reader.read(16, "ObjectPosZ")? as u16,
    };
    let snap = if reader.read_flag("ObjectSnap")? {
        let snap = if reader.read_flag("ObjectSnapTolExists")? {
            ObjectSnap::Tolerance(reader.read(12, "ObjectSnapTolerance")? as u16)
        } else {
            ObjectSnap::DefaultTolerance
        };
        reader.read(1, "Res2")?;
        snap
    } else {
        ObjectSnap::Off
    };
    let zone_gains = if reader.read_flag("ObjectZoneControl")? {
        let mut zone_gains = [ZoneGain::Silent; 9];
        for zone_gain in &mut zone_gains {
            *zone_gain = read_zone_gain(reader, "ZoneGain")?;
        }
        Some(zone_gains)
    } else {
        None
    };
    let spread = match reader.read(2, "ObjectSpreadMode")? {
        0 => ObjectSpread::LowResolution(reader.read(8, "ObjectSpread")? as u8),
        1 => ObjectSpread::PointSource,
        2 => ObjectSpread::OneDimension(reader.read(12, "ObjectSpread")? as u16),
        _ => ObjectSpread::ThreeDimensions {
            x: reader.read(12, "ObjectSpreadX")? as u16,
            y: reader.read(12, "ObjectSpreadY")? as u16,
            z: reader.read(12, "ObjectSpreadZ")? as u16,
        },
    };
    reader.read(4, "Reserved")?;
    let decorrelation = read_decorrelation(reader, "ObjectDecorCoefPrefix", "ObjectDecorCoef")?;
    Ok(ObjectPan {
        gain,
        position,
        snap,
        zone_gains,
        spread,
        decorrelation,
    })
}

fn parse_object_zone_definition_19(
    payload: &[u8],
    frame_rate: FrameRate,
) -> Result<ObjectZoneDefinition19, IabError> {
    let mut reader = BitReader::new(payload, "ObjectZoneDefinition19");
    let read_zone_gains = |reader: &mut BitReader| -> Result<[ZoneGain; 19], IabError> {
        let mut zone_gains = [ZoneGain::Silent; 19];
        for zone_gain in &mut zone_gains {
            *zone_gain = read_zone_gain(reader, "ZoneGain19")?;
        }
        Ok(zone_gains)
    };
    let mut current = read_zone_gains(&mut reader)?;
    let mut sub_blocks = vec![current];
    for _ in 1..frame_rate.pan_sub_block_count() {
        if reader.read_flag("ZoneInfoExists")? {
            current = read_zone_gains(&mut reader)?;
        }
        sub_blocks.push(current);
    }
    reader.expect_end("ElementSize")?;
    Ok(ObjectZoneDefinition19 { sub_blocks })
}

fn parse_audio_data_dlc(payload: &[u8]) -> Result<AudioDataDlc, IabError> {
    let mut reader = BitReader::new(payload, "AudioDataDLC");
    let audio_data_id = reader.read_plex(8, "AudioDataID")?;
    let dlc_size = reader.read(16, "DLCSize")?;
    let coded = reader.read_bytes(dlc_size as usize, "DLCSize")?.to_vec();
    reader.expect_end("DLCSize")?;
    Ok(AudioDataDlc {
        audio_data_id,
        coded,
    })
}

fn parse_audio_data_pcm(
    payload: &[u8],
    sample_count: usize,
    bit_depth: BitDepth,
) -> Result<AudioDataPcm, IabError> {
    let mut reader = BitReader::new(payload, "AudioDataPCM");
    let audio_data_id = reader.read_plex(8, "AudioDataID")?;
    let bytes_per_sample = bit_depth.bits() as usize / 8;
    let sample_bytes = reader
        .read_bytes(sample_count * bytes_per_sample, "PCMData")?
        .to_vec();
    reader.expect_end("ElementSize")?;
    Ok(AudioDataPcm {
        audio_data_id,
        sample_bytes,
    })
}

fn parse_user_data(payload: &[u8]) -> Result<UserData, IabError> {
    let mut reader = BitReader::new(payload, "UserData");
    let mut user_id = [0; 16];
    user_id.copy_from_slice(reader.read_bytes(16, "UserID")?);
    Ok(UserData {
        user_id,
        bytes: reader.read_rest().to_vec(),
    })
}

fn read_audio_description(reader: &mut BitReader) -> Result<(u8, Option<Vec<u8>>), IabError> {
    let audio_description = reader.read(8, "AudioDescription")? as u8;
    let text = if audio_description & 0x80 != 0 {
        Some(reader.read_null_terminated("AudioDescriptionText")?)
    } else {
        None
    };
    Ok((audio_description, text))
}

fn read_gain(
    reader: &mut BitReader,
    prefix_field: &'static str,
    gain_field: &'static str,
) -> Result<Gain, IabError> {
    Ok(match reader.read(2, prefix_field)? {
        0 => Gain::Unity,
        1 => Gain::Silent,
        2 => Gain::Coded(reader.read(10, gain_field)? as u16),
        _ => Gain::Reserved(reader.read(10, gain_field)? as u16),
    })
}

fn read_zone_gain(reader: &mut BitReader, gain_field: &'static str) -> Result<ZoneGain, IabError> {
    Ok(match reader.read(2, "ZoneGainPrefix")? {
        0 => ZoneGain::Silent,
        1 => ZoneGain::Full,
        2 => ZoneGain::Coded(reader.read(10, gain_field)? as u16),
        _ => ZoneGain::Reserved(reader.read(10, gain_field)? as u16),
    })
}

fn read_decorrelation(
    reader: &mut BitReader,
    prefix_field: &'static str,
    coefficient_field: &'static str,
) -> Result<Decorrelation, IabError> {
    Ok(match reader.read(2, prefix_field)? {
        0 => Decorrelation::Off,
        1 => Decorrelation::Maximum,
        2 => Decorrelation::Coded(reader.read(8, coefficient_field)? as u8),
        _ => Decorrelation::Reserved(reader.read(8, coefficient_field)? as u8),
    })
}

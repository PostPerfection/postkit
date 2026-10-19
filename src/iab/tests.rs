use super::parse::split_frames;
use super::*;
use crate::mxf_wrap::{
    EssenceType, McaConfig, MxfEncryption, MxfStandard, MxfWrapOptions, SoundfieldGroup, mxf_wrap,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

const VALIDATION_STREAM_COUNT: u32 = 148;

pub(crate) fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/iab")
}

fn stream_name(number: u32) -> String {
    format!("IABValidationStream_{number}")
}

pub(crate) fn read_zipped_stream(path: &Path) -> Vec<u8> {
    let file =
        std::fs::File::open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut entry = archive.by_index(0).unwrap();
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).unwrap();
    bytes
}

pub(crate) fn validation_stream_frames(number: u32) -> Vec<Vec<u8>> {
    let stream =
        read_zipped_stream(&fixture_directory().join(format!("{}.iab.zip", stream_name(number))));
    split_frames(&stream)
        .unwrap()
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

// one IAB frame a picture frame at 24 fps
pub(crate) fn wrap_atmos_track(
    frames: &[Vec<u8>],
    output: &Path,
    standard: MxfStandard,
    encryption: Option<MxfEncryption>,
) {
    let scratch = tempfile::tempdir().unwrap();
    let input_files: Vec<PathBuf> = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let path = scratch.path().join(format!("frame_{index:05}.iab"));
            std::fs::write(&path, frame).unwrap();
            path
        })
        .collect();
    let mca_config = (standard == MxfStandard::As02).then(|| McaConfig {
        labels: String::new(),
        spoken_language: Some("en-US".to_string()),
        soundfield_group: Some(SoundfieldGroup {
            title: "IAB".to_string(),
            title_version: "1".to_string(),
            audio_content_kind: "PRM".to_string(),
            audio_element_kind: "FCMP".to_string(),
        }),
    });
    let result = mxf_wrap(&MxfWrapOptions {
        input_files,
        output: output.to_path_buf(),
        essence_type: EssenceType::Atmos,
        standard,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption,
        mca_config,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(result.success, "{}", result.error);
}

fn read_verdict(path: &Path) -> serde_json::Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap()
}

// FNV-1a 64 over little-endian samples, the digest dump_decoded_digests.cpp prints
fn sample_digest(samples: &[i32]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in samples.iter().flat_map(|sample| sample.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

// the strings the validator's BitStreamSummary uses
fn summary_of(frame: &Frame, frame_count: usize) -> serde_json::Value {
    let frame_rate = match frame.frame_rate {
        FrameRate::Fps24 => "24",
        FrameRate::Fps25 => "25",
        FrameRate::Fps30 => "30",
        FrameRate::Fps48 => "48",
        FrameRate::Fps50 => "50",
        FrameRate::Fps60 => "60",
        FrameRate::Fps96 => "96",
        FrameRate::Fps100 => "100",
        FrameRate::Fps120 => "120",
        FrameRate::Fps24000Over1001 => "23.976",
    };
    serde_json::json!({
        "SampleRate": format!("{}kHz", frame.sample_rate.hertz() / 1000),
        "FrameRate": format!("{frame_rate} fps"),
        "BitDepth": format!("{}-bit", frame.bit_depth.bits()),
        "FrameCount": frame_count.to_string(),
        "MaxRendered": frame.max_rendered.to_string(),
    })
}

fn reference_hashes() -> BTreeMap<String, String> {
    std::fs::read_to_string(fixture_directory().join("iablib_decoded_digests_sha256.txt"))
        .unwrap()
        .lines()
        .map(|line| {
            let (name, hash) = line.split_once(' ').unwrap();
            (name.to_string(), hash.to_string())
        })
        .collect()
}

fn verdict_mismatch(
    name: &str,
    parsed: &[Result<Frame, IabError>],
    verdict: &serde_json::Value,
) -> Option<String> {
    let first_parse_error = parsed.iter().position(Result::is_err);
    match (verdict.get("BitStreamSummary"), first_parse_error) {
        (Some(_), Some(index)) => Some(format!(
            "{name}: frame {index} does not parse: {}",
            parsed[index].as_ref().unwrap_err()
        )),
        (Some(reference), None) => {
            let ours = summary_of(parsed[0].as_ref().unwrap(), parsed.len());
            (ours != *reference)
                .then(|| format!("{name}: summary {ours} against reference {reference}"))
        }
        // the validator counts frames from 1
        (None, index) => {
            let reference = verdict["ParserState"]["FrameIndex"].as_u64().unwrap();
            let ours = index.map(|index| index as u64 + 1);
            (ours != Some(reference)).then(|| {
                format!("{name}: the first frame that fails to parse is {ours:?}, the reference stops at {reference}")
            })
        }
    }
}

// the lines dump_decoded_digests.cpp prints, one per frame and AudioDataID
fn decoded_digest_lines(name: &str, parsed: &[Result<Frame, IabError>]) -> String {
    let mut lines = String::new();
    for (index, frame) in parsed.iter().enumerate() {
        let Some(decoded) = frame
            .as_ref()
            .ok()
            .and_then(|frame| decode_audio(frame).ok())
        else {
            writeln!(lines, "{name} {index} rejected").unwrap();
            break;
        };
        for (audio_data_id, samples) in decoded {
            let sample_count = samples.len();
            let digest = sample_digest(&samples);
            writeln!(
                lines,
                "{name} {index} {audio_data_id} {sample_count} {digest:016x}"
            )
            .unwrap();
        }
    }
    lines
}

fn stream_mismatches(
    directory: &Path,
    number: u32,
    reference_hashes: &BTreeMap<String, String>,
) -> Vec<String> {
    let name = stream_name(number);
    let stream = read_zipped_stream(&directory.join(format!("{name}.iab.zip")));
    let verdict = read_verdict(&directory.join(format!("Stream{number}_cA.json")));
    let frames = split_frames(&stream).unwrap_or_else(|error| panic!("{name}: {error}"));
    let parsed: Vec<Result<Frame, IabError>> =
        frames.iter().map(|bytes| parse_frame(bytes)).collect();

    let mut mismatches: Vec<String> = verdict_mismatch(&name, &parsed, &verdict)
        .into_iter()
        .collect();
    let hash = hex::encode(Sha256::digest(
        decoded_digest_lines(&name, &parsed).as_bytes(),
    ));
    if reference_hashes.get(&name) != Some(&hash) {
        mismatches.push(format!("{name}: decoded samples differ from IABLib's"));
    }
    mismatches
}

fn committed_stream_numbers() -> Vec<u32> {
    let mut numbers: Vec<u32> = std::fs::read_dir(fixture_directory())
        .unwrap()
        .filter_map(|entry| {
            let file_name = entry.unwrap().file_name().into_string().unwrap();
            let number = file_name
                .strip_prefix("IABValidationStream_")?
                .strip_suffix(".iab.zip")?;
            Some(number.parse().unwrap())
        })
        .collect();
    numbers.sort_unstable();
    numbers
}

#[test]
fn committed_streams_match_the_validator_summary_and_the_iablib_decode() {
    let directory = fixture_directory();
    let reference_hashes = reference_hashes();
    let numbers = committed_stream_numbers();
    assert!(!numbers.is_empty());
    let mismatches: Vec<String> = numbers
        .into_iter()
        .flat_map(|number| stream_mismatches(&directory, number, &reference_hashes))
        .collect();
    assert_eq!(mismatches, Vec::<String>::new());
}

#[test]
fn every_validation_stream_matches_the_validator_summary_and_the_iablib_decode() {
    let directory = std::env::var_os("POSTKIT_IAB_VALIDATION_STREAMS").expect(
        "POSTKIT_IAB_VALIDATION_STREAMS names the directory scripts/fetch-iab-validation-streams.sh fills",
    );
    let directory = PathBuf::from(directory);
    let reference_hashes = reference_hashes();
    assert_eq!(reference_hashes.len(), VALIDATION_STREAM_COUNT as usize);
    let mismatches: Vec<String> = (1..=VALIDATION_STREAM_COUNT)
        .flat_map(|number| stream_mismatches(&directory, number, &reference_hashes))
        .collect();
    assert_eq!(mismatches, Vec::<String>::new());
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit_count: usize,
}

impl BitWriter {
    fn bits(mut self, value: u64, width: u32) -> Self {
        for bit in (0..width).rev() {
            if self.bit_count.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let byte = self.bytes.last_mut().unwrap();
            *byte |= (((value >> bit) & 1) as u8) << (7 - self.bit_count % 8);
            self.bit_count += 1;
        }
        self
    }

    fn plex(mut self, value: u32, first_width: u32) -> Self {
        let mut width = first_width;
        while width < 32 && u64::from(value) >= (1 << width) - 1 {
            self = self.bits((1 << width) - 1, width);
            width *= 2;
        }
        self.bits(value.into(), width)
    }

    fn align(mut self) -> Self {
        self.bit_count = self.bit_count.div_ceil(8) * 8;
        self
    }

    fn bytes(mut self, bytes: &[u8]) -> Self {
        self = self.align();
        self.bytes.extend_from_slice(bytes);
        self.bit_count += bytes.len() * 8;
        self
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

fn element(element_id: u32, payload: &[u8]) -> Vec<u8> {
    BitWriter::default()
        .plex(element_id, 8)
        .plex(payload.len() as u32, 8)
        .bytes(payload)
        .finish()
}

fn bitstream_frame(ia_frame: &[u8]) -> Vec<u8> {
    BitWriter::default()
        .bits(0x01, 8)
        .bits(0, 32)
        .bits(0x02, 8)
        .bits(ia_frame.len() as u64, 32)
        .bytes(ia_frame)
        .finish()
}

// 48 kHz, 24 bit, 24 fps, MaxRendered 1
fn ia_frame_header() -> BitWriter {
    BitWriter::default()
        .bits(1, 8)
        .bits(0, 2)
        .bits(1, 2)
        .bits(0, 4)
        .plex(1, 8)
        .align()
}

fn frame_with_sub_elements(sub_elements: &[Vec<u8>]) -> Vec<u8> {
    let payload = ia_frame_header()
        .plex(sub_elements.len() as u32, 8)
        .bytes(&sub_elements.concat())
        .finish();
    bitstream_frame(&element(0x08, &payload))
}

fn bed_definition_without_channels(sub_elements: &[Vec<u8>]) -> Vec<u8> {
    let payload = BitWriter::default()
        .plex(1, 8)
        .bits(0, 1)
        .plex(0, 4)
        .bits(0x180, 10)
        .align()
        .bits(0, 8)
        .plex(sub_elements.len() as u32, 8)
        .bytes(&sub_elements.concat())
        .finish();
    element(0x10, &payload)
}

fn nested_beds(depth: u32) -> Vec<u8> {
    let children = if depth == 0 {
        Vec::new()
    } else {
        vec![nested_beds(depth - 1)]
    };
    bed_definition_without_channels(&children)
}

fn audio_data_dlc(dlc_size: u32, coded: &[u8]) -> Vec<u8> {
    let payload = BitWriter::default()
        .plex(1, 8)
        .bits(dlc_size.into(), 16)
        .bytes(coded)
        .finish();
    element(0x200, &payload)
}

fn truncated(element: &'static str, field: &'static str) -> IabError {
    IabError::Truncated { element, field }
}

fn first_frame(number: u32) -> Vec<u8> {
    validation_stream_frames(number).swap_remove(0)
}

#[test]
fn malformed_frames_return_errors_that_name_the_field() {
    let oversized_preamble = BitWriter::default()
        .bits(0x01, 8)
        .bits(0xFFFF_FFFF, 32)
        .finish();
    assert_eq!(
        parse_frame(&oversized_preamble).unwrap_err(),
        truncated("IABitstream frame", "PreambleValue")
    );

    let mut oversized_ia_frame = frame_with_sub_elements(&[]);
    oversized_ia_frame[6..10].copy_from_slice(&0x1000_0000u32.to_be_bytes());
    assert_eq!(
        parse_frame(&oversized_ia_frame).unwrap_err(),
        truncated("IABitstream frame", "IAFrame")
    );

    let oversized_element = bitstream_frame(
        &BitWriter::default()
            .plex(0x08, 8)
            .plex(0x10000, 8)
            .bits(1, 8)
            .finish(),
    );
    assert_eq!(
        parse_frame(&oversized_element).unwrap_err(),
        truncated("IAFrame", "ElementSize")
    );

    let escaped_past_32_bits = BitWriter::default()
        .bits(1, 8)
        .bits(0b0001_0000, 8)
        .bits(0xFF, 8)
        .bits(0xFFFF, 16)
        .bits(0xFFFF_FFFF, 32)
        .finish();
    assert!(matches!(
        parse_frame(&bitstream_frame(&element(0x08, &escaped_past_32_bits))).unwrap_err(),
        IabError::Invalid {
            element: "IAFrame",
            field: "MaxRendered",
            ..
        }
    ));

    let oversized_sub_element_count = ia_frame_header().plex(0xFFFF_FFFE, 8).finish();
    assert_eq!(
        parse_frame(&bitstream_frame(&element(
            0x08,
            &oversized_sub_element_count
        )))
        .unwrap_err(),
        truncated("IAFrame", "ElementID")
    );

    let oversized_channel_count = BitWriter::default()
        .plex(1, 8)
        .bits(0, 1)
        .plex(0xFFFF_FFFE, 4)
        .finish();
    let oversized_channel_count =
        frame_with_sub_elements(&[element(0x10, &oversized_channel_count)]);
    assert_eq!(
        parse_frame(&oversized_channel_count).unwrap_err(),
        truncated("BedDefinition", "ChannelID")
    );

    let oversized_remap = BitWriter::default()
        .plex(1, 8)
        .bits(0xFF, 8)
        .plex(0xFFFF_FFFE, 4)
        .plex(1, 4)
        .plex(0, 4)
        .finish();
    let oversized_remap = frame_with_sub_elements(&[bed_definition_without_channels(&[element(
        0x20,
        &oversized_remap,
    )])]);
    assert_eq!(
        parse_frame(&oversized_remap).unwrap_err(),
        truncated("BedRemap", "RemapGainPrefix")
    );

    assert!(matches!(
        parse_frame(&frame_with_sub_elements(&[nested_beds(1000)])).unwrap_err(),
        IabError::Invalid {
            element: "BedDefinition",
            field: "SubElementCount",
            ..
        }
    ));

    let oversized_dlc_size = frame_with_sub_elements(&[audio_data_dlc(0xFFFF, &[0; 8])]);
    assert_eq!(
        parse_frame(&oversized_dlc_size).unwrap_err(),
        truncated("AudioDataDLC", "DLCSize")
    );

    let short_pcm = element(
        0x400,
        &BitWriter::default().plex(1, 8).bytes(&[0; 10]).finish(),
    );
    assert_eq!(
        parse_frame(&frame_with_sub_elements(&[short_pcm])).unwrap_err(),
        truncated("AudioDataPCM", "PCMData")
    );

    // 48 kHz, ShiftBits 8, no predictor, a Rice sub block whose unary run reaches the end
    let endless_unary_run = BitWriter::default()
        .bits(0, 2)
        .bits(8, 5)
        .bits(0, 2)
        .bits(1, 1)
        .bits(0, 5)
        .bits(0x1_FFFF, 17)
        .finish();
    let frame = parse_frame(&frame_with_sub_elements(&[audio_data_dlc(
        4,
        &endless_unary_run,
    )]))
    .unwrap();
    assert_eq!(
        decode_audio(&frame).unwrap_err(),
        truncated("AudioDataDLC", "UnaryBit")
    );
}

#[test]
fn truncated_and_overwritten_validation_frames_return_errors() {
    for number in [1, 50] {
        let frame = first_frame(number);
        let cut_lengths = (0..1024).chain((1024..frame.len()).step_by(97));
        for length in cut_lengths {
            assert!(
                parse_frame(&frame[..length]).is_err(),
                "stream {number} cut to {length} bytes"
            );
        }

        let mut parsed = parse_frame(&frame).unwrap();
        for dlc in &mut parsed.dlc_audio {
            dlc.coded.truncate(dlc.coded.len() / 2);
        }
        assert!(
            decode_audio(&parsed).is_err(),
            "stream {number} with its DLC cut in half"
        );

        // only a panic fails here, a frame with one corrupt byte may still parse
        for position in 0..2048.min(frame.len()) {
            let mut overwritten = frame.clone();
            overwritten[position] = 0xFF;
            let _ = parse_frame(&overwritten);
        }
    }
}

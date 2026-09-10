use crate::preview::{PictureReader, read_j2c_frame};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// the PCM reader has to be told an edit rate before it can report its own
const PROBE_EDIT_RATE: asdcplib::Rational = asdcplib::Rational {
    numerator: 24,
    denominator: 1,
};

// six digits keeps a feature at 24 fps sorted by frame number
const FRAME_NUMBER_DIGITS: usize = 6;

const BITS_PER_BYTE: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSelection {
    All,
    PictureOnly,
    SoundOnly,
}

impl RestoreSelection {
    fn wants(self, kind: TrackKind) -> bool {
        match self {
            RestoreSelection::All => true,
            RestoreSelection::PictureOnly => kind == TrackKind::Picture,
            RestoreSelection::SoundOnly => kind == TrackKind::Sound,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Picture,
    Sound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredTrack {
    pub track_file: PathBuf,
    pub kind: TrackKind,
    pub output: PathBuf,
    // codestreams for a picture track, samples per channel for a sound track
    pub written: u64,
}

// named so a subtitle or ISXD track is not dropped silently
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTrack {
    pub track_file: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<RestoredTrack>,
    pub skipped: Vec<SkippedTrack>,
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("{0} holds no ASSETMAP")]
    NoAssetMap(PathBuf),
    #[error("{0} holds no CPL")]
    NoCpl(PathBuf),
    #[error("no CPL in {0} names a track file")]
    NoTrackFiles(PathBuf),
    #[error("a CPL names track file {id}, which the ASSETMAP of {package} does not list")]
    UnresolvedAsset { package: PathBuf, id: String },
    #[error("{path} is encrypted and no key was given")]
    MissingKey { path: PathBuf },
    #[error("{path} declares {bits}-bit samples, which no WAV can carry")]
    UnsupportedSoundDepth { path: PathBuf, bits: u32 },
    #[error("{path} declares an edit rate of {numerator}/{denominator}")]
    UnreadableEditRate {
        path: PathBuf,
        numerator: i32,
        denominator: i32,
    },
    #[error("{path}: {message}")]
    Mxf { path: PathBuf, message: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Wav { path: PathBuf, source: hound::Error },
}

pub fn restore_package(
    package: &Path,
    output: &Path,
    selection: RestoreSelection,
    key: Option<[u8; 16]>,
) -> Result<RestoreReport, RestoreError> {
    let track_files = track_files(package)?;
    create_dir(output)?;

    let mut report = RestoreReport::default();
    for track_file in track_files {
        let essence = asdcplib::essence_type(&track_file.to_string_lossy()).map_err(|error| {
            RestoreError::Mxf {
                path: track_file.clone(),
                message: format!("cannot read the essence type: {error}"),
            }
        })?;
        let Some(kind) = track_kind(essence) else {
            report.skipped.push(SkippedTrack {
                track_file,
                reason: format!("{essence:?} is not picture or sound essence"),
            });
            continue;
        };
        if !selection.wants(kind) {
            continue;
        }
        let restored = match kind {
            TrackKind::Picture => restore_picture(&track_file, essence, output, key)?,
            TrackKind::Sound => restore_sound(&track_file, output, key)?,
        };
        report.restored.push(restored);
    }
    Ok(report)
}

// anything but picture and sound is named and left alone
fn track_kind(essence: asdcplib::EssenceType) -> Option<TrackKind> {
    match essence {
        asdcplib::EssenceType::As02Jpeg2000 | asdcplib::EssenceType::Jpeg2000 => {
            Some(TrackKind::Picture)
        }
        asdcplib::EssenceType::As02Pcm24b48k | asdcplib::EssenceType::As02Pcm24b96k => {
            Some(TrackKind::Sound)
        }
        _ => None,
    }
}

fn restore_picture(
    track_file: &Path,
    essence: asdcplib::EssenceType,
    output: &Path,
    key: Option<[u8; 16]>,
) -> Result<RestoredTrack, RestoreError> {
    let as02 = essence == asdcplib::EssenceType::As02Jpeg2000;
    let mut reader = PictureReader::open(track_file, as02).map_err(|error| RestoreError::Mxf {
        path: track_file.to_path_buf(),
        message: error.to_string(),
    })?;
    let encrypted = reader
        .writer_info()
        .map_err(|error| mxf_error(track_file, error))?
        .encrypted_essence;
    let mut decrypt = decrypt_context(track_file, encrypted, key)?;
    let frames = reader
        .picture_descriptor()
        .map_err(|error| mxf_error(track_file, error))?
        .container_duration;

    let directory = track_directory(track_file, output)?;
    for frame in 0..frames {
        let codestream = read_j2c_frame(&mut reader, frame, decrypt.as_mut())
            .map_err(|error| mxf_error(track_file, error))?;
        let path = directory.join(format!("{frame:0FRAME_NUMBER_DIGITS$}.j2c"));
        std::fs::write(&path, codestream).map_err(|source| RestoreError::Io { path, source })?;
    }
    reader.close();

    Ok(RestoredTrack {
        track_file: track_file.to_path_buf(),
        kind: TrackKind::Picture,
        output: directory,
        written: u64::from(frames),
    })
}

fn restore_sound(
    track_file: &Path,
    output: &Path,
    key: Option<[u8; 16]>,
) -> Result<RestoredTrack, RestoreError> {
    let path = track_file.to_string_lossy().to_string();
    let mut reader = asdcplib::as02::pcm::MxfReader::new();
    reader
        .open_read(&path, PROBE_EDIT_RATE)
        .map_err(|error| mxf_error(track_file, error))?;
    let mut descriptor = reader
        .audio_descriptor()
        .map_err(|error| mxf_error(track_file, error))?;
    // reopened at the rate the file declares, the probe rate slices it wrongly
    if descriptor.edit_rate != PROBE_EDIT_RATE {
        let _ = reader.close();
        reader
            .open_read(&path, descriptor.edit_rate)
            .map_err(|error| mxf_error(track_file, error))?;
        descriptor = reader
            .audio_descriptor()
            .map_err(|error| mxf_error(track_file, error))?;
    }

    let encrypted = reader
        .writer_info()
        .map_err(|error| mxf_error(track_file, error))?
        .encrypted_essence;
    let mut decrypt = decrypt_context(track_file, encrypted, key)?;

    let layout = SoundLayout::read(track_file, &descriptor)?;
    let directory = track_directory(track_file, output)?;
    let wav = directory.join(format!("{}.wav", stem(track_file)));
    let mut writer = WavWriter::create(&wav, layout.spec).map_err(|source| RestoreError::Wav {
        path: wav.clone(),
        source,
    })?;

    let mut essence = vec![0u8; layout.bytes_per_edit_unit];
    let mut samples_per_channel = 0u64;
    for edit_unit in 0..descriptor.container_duration {
        let read = reader
            .read_frame(edit_unit, &mut essence, decrypt.as_mut(), None)
            .map_err(|error| mxf_error(track_file, error))?;
        let whole = read - read % layout.block_align;
        for sample in essence[..whole].chunks_exact(layout.sample_bytes) {
            writer
                .write_sample((layout.decode)(sample))
                .map_err(|source| RestoreError::Wav {
                    path: wav.clone(),
                    source,
                })?;
        }
        samples_per_channel += (whole / layout.block_align) as u64;
    }
    writer.finalize().map_err(|source| RestoreError::Wav {
        path: wav.clone(),
        source,
    })?;
    let _ = reader.close();

    Ok(RestoredTrack {
        track_file: track_file.to_path_buf(),
        kind: TrackKind::Sound,
        output: directory,
        written: samples_per_channel,
    })
}

type DecodeSample = fn(&[u8]) -> i32;

#[derive(Debug)]
struct SoundLayout {
    spec: WavSpec,
    sample_bytes: usize,
    block_align: usize,
    bytes_per_edit_unit: usize,
    decode: DecodeSample,
}

impl SoundLayout {
    fn read(
        track_file: &Path,
        descriptor: &asdcplib::pcm::AudioDescriptor,
    ) -> Result<Self, RestoreError> {
        let edit_rate = descriptor.edit_rate;
        if edit_rate.numerator <= 0 || edit_rate.denominator <= 0 {
            return Err(RestoreError::UnreadableEditRate {
                path: track_file.to_path_buf(),
                numerator: edit_rate.numerator,
                denominator: edit_rate.denominator,
            });
        }
        let decode = decode_sample(descriptor.quantization_bits).ok_or_else(|| {
            RestoreError::UnsupportedSoundDepth {
                path: track_file.to_path_buf(),
                bits: descriptor.quantization_bits,
            }
        })?;
        let sample_rate = descriptor.audio_sampling_rate.numerator.max(0) as u32;
        let sample_bytes = (descriptor.quantization_bits / BITS_PER_BYTE) as usize;
        let block_align = sample_bytes * descriptor.channel_count as usize;
        let samples_per_edit_unit = (u64::from(sample_rate) * edit_rate.denominator as u64)
            .div_ceil(edit_rate.numerator as u64);
        Ok(SoundLayout {
            spec: WavSpec {
                channels: descriptor.channel_count as u16,
                sample_rate,
                bits_per_sample: descriptor.quantization_bits as u16,
                sample_format: SampleFormat::Int,
            },
            sample_bytes,
            block_align,
            bytes_per_edit_unit: samples_per_edit_unit as usize * block_align,
            decode,
        })
    }
}

// little-endian signed PCM, the only thing an AS-02 sound track carries
fn decode_sample(bits: u32) -> Option<DecodeSample> {
    Some(match bits {
        16 => |bytes: &[u8]| i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        24 => |bytes: &[u8]| i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8,
        32 => |bytes: &[u8]| i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => return None,
    })
}

fn decrypt_context(
    track_file: &Path,
    encrypted: bool,
    key: Option<[u8; 16]>,
) -> Result<Option<asdcplib::crypto::AesDecContext>, RestoreError> {
    if !encrypted {
        return Ok(None);
    }
    let key = key.ok_or_else(|| RestoreError::MissingKey {
        path: track_file.to_path_buf(),
    })?;
    let mut context = asdcplib::crypto::AesDecContext::new();
    context.init_key(&key).map_err(|error| RestoreError::Mxf {
        path: track_file.to_path_buf(),
        message: format!("cannot use the key: {error}"),
    })?;
    Ok(Some(context))
}

pub fn track_files(package: &Path) -> Result<Vec<PathBuf>, RestoreError> {
    let asset_map = crate::assetmap::find(package)
        .ok_or_else(|| RestoreError::NoAssetMap(package.to_path_buf()))?;
    let assets = crate::assetmap::parse_ordered(&asset_map);
    let path_by_id: HashMap<&str, &str> = assets
        .iter()
        .map(|(id, relative)| (id.as_str(), relative.as_str()))
        .collect();

    let mut named = Vec::new();
    let mut found_a_cpl = false;
    for (_, relative) in &assets {
        let path = package.join(relative);
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !crate::cpl_xml::is_composition_playlist(&text) {
            continue;
        }
        found_a_cpl = true;
        named.extend(track_file_ids(&text));
    }
    if !found_a_cpl {
        return Err(RestoreError::NoCpl(package.to_path_buf()));
    }
    if named.is_empty() {
        return Err(RestoreError::NoTrackFiles(package.to_path_buf()));
    }
    if let Some(id) = named
        .iter()
        .find(|id| !path_by_id.contains_key(id.as_str()))
    {
        return Err(RestoreError::UnresolvedAsset {
            package: package.to_path_buf(),
            id: id.clone(),
        });
    }

    // ASSETMAP order, each track file once
    Ok(assets
        .iter()
        .filter(|(id, _)| named.contains(id))
        .map(|(_, relative)| package.join(relative))
        .collect())
}

fn track_file_ids(cpl: &str) -> Vec<String> {
    let Ok(pattern) = regex::Regex::new(
        r"<(?:\w+:)?TrackFileId>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})\s*</(?:\w+:)?TrackFileId>",
    ) else {
        return Vec::new();
    };
    pattern
        .captures_iter(cpl)
        .map(|captures| captures[1].to_ascii_lowercase())
        .collect()
}

fn stem(track_file: &Path) -> String {
    track_file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn track_directory(track_file: &Path, output: &Path) -> Result<PathBuf, RestoreError> {
    let directory = output.join(stem(track_file));
    create_dir(&directory)?;
    Ok(directory)
}

fn create_dir(path: &Path) -> Result<(), RestoreError> {
    std::fs::create_dir_all(path).map_err(|source| RestoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn mxf_error(track_file: &Path, error: impl std::fmt::Display) -> RestoreError {
    RestoreError::Mxf {
        path: track_file.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOUND_CHANNELS: u32 = 2;
    const SOUND_SAMPLE_RATE: u32 = 48_000;
    const SOUND_BITS: u32 = 24;
    const FRAMES_PER_SECOND: i32 = 24;

    fn descriptor(bits: u32, edit_rate: (i32, i32)) -> asdcplib::pcm::AudioDescriptor {
        asdcplib::pcm::AudioDescriptor {
            edit_rate: asdcplib::Rational::new(edit_rate.0, edit_rate.1),
            audio_sampling_rate: asdcplib::Rational::new(SOUND_SAMPLE_RATE as i32, 1),
            locked: true,
            channel_count: SOUND_CHANNELS,
            quantization_bits: bits,
            block_align: SOUND_CHANNELS * bits / BITS_PER_BYTE,
            avg_bps: 0,
            linked_track_id: 1,
            container_duration: 2,
            channel_format: asdcplib::pcm::ChannelFormat::Cfg6,
        }
    }

    #[test]
    fn an_edit_unit_holds_one_frames_worth_of_every_channel() {
        let layout =
            SoundLayout::read(Path::new("sound.mxf"), &descriptor(SOUND_BITS, (24, 1))).unwrap();
        assert_eq!(layout.spec.channels, SOUND_CHANNELS as u16);
        assert_eq!(layout.spec.sample_rate, SOUND_SAMPLE_RATE);
        assert_eq!(layout.spec.bits_per_sample, SOUND_BITS as u16);
        assert_eq!(layout.block_align, 6);
        assert_eq!(
            layout.bytes_per_edit_unit,
            (SOUND_SAMPLE_RATE / FRAMES_PER_SECOND as u32) as usize * 6
        );
    }

    #[test]
    fn a_fractional_edit_rate_rounds_the_edit_unit_up() {
        let layout = SoundLayout::read(
            Path::new("sound.mxf"),
            &descriptor(SOUND_BITS, (24000, 1001)),
        )
        .unwrap();
        // 48000 * 1001 / 24000 is 2002, so an edit unit is 2002 samples a channel
        assert_eq!(layout.bytes_per_edit_unit, 2002 * 6);
    }

    #[test]
    fn a_depth_no_wav_carries_is_refused_by_name() {
        let error =
            SoundLayout::read(Path::new("sound.mxf"), &descriptor(20, (24, 1))).unwrap_err();
        assert!(
            matches!(error, RestoreError::UnsupportedSoundDepth { bits: 20, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_zero_edit_rate_is_refused_rather_than_dividing_by_it() {
        let error =
            SoundLayout::read(Path::new("sound.mxf"), &descriptor(SOUND_BITS, (0, 1))).unwrap_err();
        assert!(
            matches!(error, RestoreError::UnreadableEditRate { .. }),
            "{error}"
        );
    }

    #[test]
    fn twenty_four_bit_samples_keep_their_sign() {
        let decode = decode_sample(24).unwrap();
        assert_eq!(decode(&[0x00, 0x00, 0x80]), -8_388_608);
        assert_eq!(decode(&[0xff, 0xff, 0x7f]), 8_388_607);
        assert_eq!(decode(&[0x01, 0x00, 0x00]), 1);
    }

    #[test]
    fn sixteen_bit_samples_keep_their_sign() {
        let decode = decode_sample(16).unwrap();
        assert_eq!(decode(&[0x00, 0x80]), -32_768);
        assert_eq!(decode(&[0xff, 0x7f]), 32_767);
    }

    #[test]
    fn every_track_file_id_a_cpl_names_is_read_in_document_order() {
        let cpl = r#"<CompositionPlaylist>
          <MainImageSequence><ResourceList>
            <Resource><TrackFileId>urn:uuid:AAAAAAAA-1111-2222-3333-444444444444</TrackFileId></Resource>
            <Resource><TrackFileId>urn:uuid:aaaaaaaa-1111-2222-3333-444444444444</TrackFileId></Resource>
          </ResourceList></MainImageSequence>
          <r0:MainAudioSequence><ResourceList>
            <Resource><r0:TrackFileId>urn:uuid:bbbbbbbb-1111-2222-3333-444444444444</r0:TrackFileId></Resource>
          </ResourceList></r0:MainAudioSequence>
        </CompositionPlaylist>"#;
        assert_eq!(
            track_file_ids(cpl),
            vec![
                "aaaaaaaa-1111-2222-3333-444444444444".to_string(),
                "aaaaaaaa-1111-2222-3333-444444444444".to_string(),
                "bbbbbbbb-1111-2222-3333-444444444444".to_string(),
            ]
        );
    }

    #[test]
    fn a_package_without_an_assetmap_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let error = track_files(directory.path()).unwrap_err();
        assert!(matches!(error, RestoreError::NoAssetMap(_)), "{error}");
    }

    #[test]
    fn a_package_whose_only_xml_is_a_pkl_is_refused_for_holding_no_cpl() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("ASSETMAP.xml"),
            r#"<AssetMap><AssetList>
              <Asset><Id>urn:uuid:11111111-2222-3333-4444-555555555555</Id>
                <ChunkList><Chunk><Path>PKL.xml</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        )
        .unwrap();
        std::fs::write(directory.path().join("PKL.xml"), "<PackingList/>").unwrap();
        let error = track_files(directory.path()).unwrap_err();
        assert!(matches!(error, RestoreError::NoCpl(_)), "{error}");
    }

    #[test]
    fn a_track_file_the_assetmap_does_not_list_is_refused_by_id() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("ASSETMAP.xml"),
            r#"<AssetMap><AssetList>
              <Asset><Id>urn:uuid:11111111-2222-3333-4444-555555555555</Id>
                <ChunkList><Chunk><Path>CPL.xml</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("CPL.xml"),
            "<CompositionPlaylist><TrackFileId>urn:uuid:99999999-2222-3333-4444-555555555555</TrackFileId></CompositionPlaylist>",
        )
        .unwrap();
        let error = track_files(directory.path()).unwrap_err();
        assert!(
            matches!(error, RestoreError::UnresolvedAsset { ref id, .. } if id == "99999999-2222-3333-4444-555555555555"),
            "{error}"
        );
    }

    #[test]
    fn a_selection_takes_only_the_kind_it_names() {
        assert!(RestoreSelection::All.wants(TrackKind::Picture));
        assert!(RestoreSelection::All.wants(TrackKind::Sound));
        assert!(RestoreSelection::PictureOnly.wants(TrackKind::Picture));
        assert!(!RestoreSelection::PictureOnly.wants(TrackKind::Sound));
        assert!(RestoreSelection::SoundOnly.wants(TrackKind::Sound));
        assert!(!RestoreSelection::SoundOnly.wants(TrackKind::Picture));
    }
}

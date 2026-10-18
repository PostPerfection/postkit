use std::path::{Path, PathBuf};

use asdcplib::crypto::AesDecContext;
use sha1::{Digest, Sha1};

use crate::composition_timeline::SubtitleSegment;
use crate::content_keys::ContentKeys;
use crate::subtitle_formats::dcp::{DcpSubtitleDocument, parse_dcp_subtitle};
use crate::subtitle_formats::imsc::{ImscDocument, parse_imsc};
use crate::subtitle_formats::interop::resolve_png;
use crate::subtitle_formats::{StyledCue, SubtitleError};

use super::MILLISECONDS_PER_SECOND;

const MXF_EXTENSION: &str = "mxf";
const URN_UUID_PREFIX: &str = "urn:uuid:";
const PNG_EXTENSION: &str = "png";
const FIRST_RESOURCE_BUFFER_BYTES: usize = 256 * 1024;

// one language's cues on the composition clock
#[derive(Debug, Default, Clone)]
pub(super) struct CompositionTrack {
    pub cues: Vec<StyledCue>,
    pub language: Option<String>,
}

pub(super) struct CompositionSubtitles {
    // one track a language, in the order the CPL first names them
    pub subtitles: Vec<CompositionTrack>,
    pub captions: Vec<CompositionTrack>,
    // one for each document naming images its track file does not carry
    pub warnings: Vec<String>,
    // the PNGs timed text track files carry, written out for the raster to read
    _images: Option<tempfile::TempDir>,
}

impl CompositionSubtitles {
    // one subtitle track and no captions
    pub(super) fn of_cues(cues: Vec<StyledCue>) -> Self {
        CompositionSubtitles {
            subtitles: vec![CompositionTrack {
                cues,
                language: None,
            }],
            captions: Vec::new(),
            warnings: Vec::new(),
            _images: None,
        }
    }
}

// in composition seconds
pub(super) struct Reels {
    pub starts_seconds: Vec<f64>,
    pub lengths_seconds: Vec<f64>,
}

pub(super) fn load(
    subtitles: &[SubtitleSegment],
    captions: &[SubtitleSegment],
    reels: &Reels,
    keys: Option<&ContentKeys>,
) -> Result<CompositionSubtitles, String> {
    let carries_mxf = subtitles
        .iter()
        .chain(captions)
        .any(|segment| is_mxf(&segment.path));
    let images = carries_mxf
        .then(tempfile::tempdir)
        .transpose()
        .map_err(|error| format!("cannot make a directory for subtitle images: {error}"))?;
    let image_directory = images.as_ref().map(|directory| directory.path());
    let mut warnings = Vec::new();
    let subtitles = load_track(subtitles, reels, keys, image_directory, &mut warnings)?;
    let captions = load_track(captions, reels, keys, image_directory, &mut warnings)?;
    Ok(CompositionSubtitles {
        subtitles,
        captions,
        warnings,
        _images: images,
    })
}

// a DCP document, or an IMSC one with the resolver's error for each image it lacks
struct TrackDocument {
    language: Option<String>,
    cues: Vec<StyledCue>,
    unresolved_images: Vec<String>,
}

impl From<DcpSubtitleDocument> for TrackDocument {
    fn from(document: DcpSubtitleDocument) -> Self {
        TrackDocument {
            language: document.language,
            cues: document.cues,
            unresolved_images: Vec::new(),
        }
    }
}

impl From<ImscDocument> for TrackDocument {
    fn from(document: ImscDocument) -> Self {
        TrackDocument {
            language: document.language,
            cues: document.cues,
            unresolved_images: document.unresolved_images,
        }
    }
}

fn load_track(
    segments: &[SubtitleSegment],
    reels: &Reels,
    keys: Option<&ContentKeys>,
    image_directory: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<Vec<CompositionTrack>, String> {
    let mut tracks: Vec<CompositionTrack> = Vec::new();
    for segment in segments {
        let (Some(&reel_start), Some(reel_lengths)) = (
            reels.starts_seconds.get(segment.reels.start),
            reels.lengths_seconds.get(segment.reels.clone()),
        ) else {
            continue;
        };
        let document = read_document(&segment.path, keys, image_directory)?;
        if !document.unresolved_images.is_empty() {
            warnings.push(format!(
                "{}: {}",
                segment.path.display(),
                document.unresolved_images.join(", ")
            ));
        }
        let span_seconds = reel_lengths.iter().sum::<f64>() - segment.offset_seconds;
        let entry_ms =
            to_milliseconds(segment.trim.as_ref().map_or(0.0, |trim| trim.start_seconds));
        let played_ms = to_milliseconds(
            segment
                .trim
                .as_ref()
                .and_then(|trim| trim.length_seconds)
                .unwrap_or(span_seconds)
                .min(span_seconds),
        );
        let reel_start_ms = to_milliseconds(reel_start + segment.offset_seconds);
        let language = segment.language.clone().or(document.language);
        let index = match tracks.iter().position(|track| track.language == language) {
            Some(index) => index,
            None => {
                tracks.push(CompositionTrack {
                    cues: Vec::new(),
                    language,
                });
                tracks.len() - 1
            }
        };
        let track = &mut tracks[index];
        for mut cue in document.cues {
            if cue.end_ms <= entry_ms || cue.start_ms >= entry_ms + played_ms {
                continue;
            }
            cue.start_ms = reel_start_ms + cue.start_ms.saturating_sub(entry_ms);
            cue.end_ms = reel_start_ms + (cue.end_ms - entry_ms).min(played_ms);
            track.cues.push(cue);
        }
    }
    Ok(tracks)
}

fn to_milliseconds(seconds: f64) -> u64 {
    (seconds * MILLISECONDS_PER_SECOND).round().max(0.0) as u64
}

fn is_mxf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(MXF_EXTENSION))
}

fn is_imsc_track_file(path: &Path) -> bool {
    matches!(
        asdcplib::essence_type(&path.to_string_lossy()),
        Ok(asdcplib::EssenceType::As02TimedText)
    )
}

fn read_document(
    path: &Path,
    keys: Option<&ContentKeys>,
    image_directory: Option<&Path>,
) -> Result<TrackDocument, String> {
    let named = |error: SubtitleError| format!("{}: {error}", path.display());
    if let (true, Some(image_directory)) = (is_mxf(path), image_directory) {
        let resource = |reference: &str| resource_png(image_directory, reference);
        if is_imsc_track_file(path) {
            let xml = read_imsc_track(path, keys, image_directory)?;
            return parse_imsc(&xml, resource)
                .map(TrackDocument::from)
                .map_err(named);
        }
        let xml = read_timed_text_track(path, keys, image_directory)?;
        return parse_dcp_subtitle(&xml, resource)
            .map(TrackDocument::from)
            .map_err(named);
    }
    let xml =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let beside = path.parent().unwrap_or(Path::new("."));
    parse_dcp_subtitle(&xml, |name| resolve_png(beside, name.trim()))
        .map(TrackDocument::from)
        .map_err(named)
}

// a urn:uuid reference, a bare uuid or a file name, to the PNG resource written out under its id
fn resource_png(image_directory: &Path, reference: &str) -> Result<PathBuf, SubtitleError> {
    let reference = reference.trim();
    let id = uuid::Uuid::parse_str(reference.strip_prefix(URN_UUID_PREFIX).unwrap_or(reference))
        .unwrap_or_else(|_| png_name_id(reference));
    let image = image_directory
        .join(id.to_string())
        .with_extension(PNG_EXTENSION);
    image
        .exists()
        .then_some(image)
        .ok_or_else(|| SubtitleError::MissingImage(PathBuf::from(reference)))
}

// ST 2067-2 5.4.5 ids a PNG named by file as the type 5 uuid of its base name in the URL namespace
fn png_name_id(reference: &str) -> uuid::Uuid {
    let base_name = reference.rsplit('/').next().unwrap_or(reference);
    let digest = Sha1::new()
        .chain_update(uuid::Uuid::NAMESPACE_URL.as_bytes())
        .chain_update(base_name.as_bytes())
        .finalize();
    let mut bytes = uuid::Bytes::default();
    let length = bytes.len();
    bytes.copy_from_slice(&digest[..length]);
    uuid::Builder::from_sha1_bytes(bytes).into_uuid()
}

fn decryption(
    path: &Path,
    info: &asdcplib::WriterInfo,
    keys: Option<&ContentKeys>,
) -> Result<Option<AesDecContext>, String> {
    match (info.encrypted_essence, keys) {
        (false, _) => Ok(None),
        (true, Some(keys)) => Ok(Some(keys.decrypt_context(info, "subtitle")?)),
        (true, None) => Err(format!(
            "{} is encrypted and the preview holds no content key for it",
            path.display()
        )),
    }
}

// an AS-02 track file carries one IMSC document
fn read_imsc_track(
    path: &Path,
    keys: Option<&ContentKeys>,
    image_directory: &Path,
) -> Result<String, String> {
    let named = |error: asdcplib::Error| format!("{}: {error}", path.display());
    let mut reader = asdcplib::as02::timed_text::MxfReader::new();
    reader.open_read(&path.to_string_lossy()).map_err(named)?;
    let info = reader.writer_info().map_err(named)?;
    let mut decrypt = decryption(path, &info, keys)?;
    let xml =
        read_resource(|buffer| reader.read_timed_text_resource(buffer, decrypt.as_mut(), None))
            .map_err(named)?;
    let resources = (0..reader.ancillary_resource_count().map_err(named)?)
        .map(|index| reader.ancillary_resource_info(index))
        .collect::<asdcplib::Result<Vec<_>>>()
        .map_err(named)?;
    write_png_resources(path, &resources, image_directory, |uuid, buffer| {
        reader.read_ancillary_resource(uuid, buffer, decrypt.as_mut(), None)
    })?;
    reader.close().map_err(named)?;
    String::from_utf8(xml).map_err(|error| format!("{}: {error}", path.display()))
}

// the XML, and every PNG ancillary resource written to `image_directory` as <uuid>.png, fonts left out
fn read_timed_text_track(
    path: &Path,
    keys: Option<&ContentKeys>,
    image_directory: &Path,
) -> Result<String, String> {
    let named = |error: asdcplib::Error| format!("{}: {error}", path.display());
    let mut reader = asdcplib::timed_text::MxfReader::new();
    reader.open_read(&path.to_string_lossy()).map_err(named)?;
    let info = reader.writer_info().map_err(named)?;
    let mut decrypt = decryption(path, &info, keys)?;
    let xml =
        read_resource(|buffer| reader.read_timed_text_resource(buffer, decrypt.as_mut(), None))
            .map_err(named)?;
    let resources = (0..reader.ancillary_resource_count().map_err(named)?)
        .map(|index| reader.ancillary_resource_info(index))
        .collect::<asdcplib::Result<Vec<_>>>()
        .map_err(named)?;
    write_png_resources(path, &resources, image_directory, |uuid, buffer| {
        reader.read_ancillary_resource(uuid, buffer, decrypt.as_mut(), None)
    })?;
    reader.close().map_err(named)?;
    String::from_utf8(xml).map_err(|error| format!("{}: {error}", path.display()))
}

// each PNG to `image_directory` as <uuid>.png, fonts left out
fn write_png_resources(
    path: &Path,
    resources: &[asdcplib::timed_text::AncillaryResourceInfo],
    image_directory: &Path,
    mut read: impl FnMut(&[u8; 16], &mut [u8]) -> asdcplib::Result<usize>,
) -> Result<(), String> {
    for resource in resources {
        if resource.mime_type != asdcplib::timed_text::MimeType::Png {
            continue;
        }
        let bytes = read_resource(|buffer| read(&resource.uuid, buffer))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let file: PathBuf = image_directory
            .join(uuid::Uuid::from_bytes(resource.uuid).to_string())
            .with_extension(PNG_EXTENSION);
        std::fs::write(&file, bytes).map_err(|error| format!("{}: {error}", file.display()))?;
    }
    Ok(())
}

fn read_resource(
    mut read: impl FnMut(&mut [u8]) -> asdcplib::Result<usize>,
) -> asdcplib::Result<Vec<u8>> {
    let mut buffer = vec![0u8; FIRST_RESOURCE_BUFFER_BYTES];
    let size = match read(&mut buffer) {
        Err(asdcplib::Error::BufferTooSmall { needed, .. }) => {
            buffer.resize(needed, 0);
            read(&mut buffer)?
        }
        result => result?,
    };
    buffer.truncate(size);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::super::timeline::Timeline;
    use super::super::timeline::tests::content_keys;
    use crate::composition_timeline::{SegmentTrim, SubtitleSegment};
    use crate::mxf_unwrap::tests::{FRAME_COUNT, wrap, write_frames};
    use crate::mxf_wrap::{EssenceType, MxfEncryption, MxfStandard, MxfWrapOptions, mxf_wrap};
    use crate::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};
    use std::path::{Path, PathBuf};

    const CPL_ID: &str = "5d000000-0000-4000-8000-000000000001";
    const PICTURE_IDS: [&str; 2] = [
        "5d000000-0000-4000-8000-000000000002",
        "5d000000-0000-4000-8000-000000000003",
    ];
    const SUBTITLE_ID: &str = "5d000000-0000-4000-8000-000000000004";
    const IMAGE_ID: &str = "5d000000-0000-4000-8000-000000000005";
    const SUBTITLE_KEY_ID: [u8; 16] = [0x7a; 16];
    const SUBTITLE_KEY: [u8; 16] = [0x8b; 16];
    const FRAMES_PER_SECOND: u32 = 24;
    const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

    fn milliseconds_of_frames(frames: u64) -> u64 {
        (frames as f64 * 1000.0 / f64::from(FRAMES_PER_SECOND)).round() as u64
    }

    // reel 2 starts FRAME_COUNT frames in, the cue at its second frame
    fn second_reel_cue_start_ms() -> u64 {
        milliseconds_of_frames(FRAME_COUNT as u64) + milliseconds_of_frames(1)
    }

    fn smpte_subtitle(image: bool) -> String {
        let body = if image {
            format!("<Image Valign=\"bottom\" Vposition=\"10\">urn:uuid:{IMAGE_ID}</Image>")
        } else {
            "<Text Valign=\"bottom\" Vposition=\"10\">second reel</Text>".to_string()
        };
        format!(
            "<?xml version=\"1.0\"?><SubtitleReel xmlns=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\">\
             <Id>urn:uuid:5d000000-0000-4000-8000-000000000030</Id><Language>en</Language>\
             <TimeCodeRate>24</TimeCodeRate><SubtitleList>\
             <Subtitle SpotNumber=\"1\" TimeIn=\"00:00:00:01\" TimeOut=\"00:00:00:03\">{body}</Subtitle>\
             </SubtitleList></SubtitleReel>"
        )
    }

    // the CPL names the subtitle in its second reel, played from `entry_point`
    fn write_package(directory: &Path, subtitle_file: &str, element: &str, entry_point: u64) {
        let scratch = tempfile::tempdir().unwrap();
        for (index, name) in ["reel1.mxf", "reel2.mxf"].iter().enumerate() {
            let (frames, _) = write_frames(scratch.path(), &format!("reel{index}"));
            wrap(frames, directory.join(name), None);
        }
        let reels = PICTURE_IDS
            .iter()
            .enumerate()
            .map(|(index, picture_id)| DcpCplReel {
                reel_id: format!("5d000000-0000-4000-8000-00000000001{index}"),
                picture_id: (*picture_id).into(),
                picture_edit_rate_num: FRAMES_PER_SECOND,
                picture_edit_rate_den: 1,
                picture_duration: FRAME_COUNT as u64,
                picture_width: 64,
                picture_height: 64,
                ..Default::default()
            })
            .collect();
        let cpl = DcpCpl {
            uuid: CPL_ID.into(),
            namespace: ns::CPL_SMPTE.into(),
            title: "Subtitled".into(),
            reels,
            ..Default::default()
        }
        .to_xml();
        let subtitle = format!(
            "<{element}><Id>urn:uuid:{SUBTITLE_ID}</Id><EditRate>24 1</EditRate>\
             <IntrinsicDuration>{FRAME_COUNT}</IntrinsicDuration><EntryPoint>{entry_point}</EntryPoint>\
             <Duration>{}</Duration></{element}>",
            FRAME_COUNT as u64 - entry_point
        );
        let at = cpl.match_indices("</AssetList>").nth(1).unwrap().0;
        let cpl = format!("{}{subtitle}{}", &cpl[..at], &cpl[at..]);
        std::fs::write(directory.join("CPL.xml"), cpl).unwrap();
        let asset = |id: &str, path: &str| AssetMapAsset {
            id: id.into(),
            path: path.into(),
            ..Default::default()
        };
        let assetmap = AssetMap {
            uuid: "5d000000-0000-4000-8000-000000000020".into(),
            namespace: ns::AM_SMPTE.into(),
            assets: vec![
                asset(CPL_ID, "CPL.xml"),
                asset(PICTURE_IDS[0], "reel1.mxf"),
                asset(PICTURE_IDS[1], "reel2.mxf"),
                asset(SUBTITLE_ID, subtitle_file),
            ],
            ..Default::default()
        };
        std::fs::write(directory.join("ASSETMAP.xml"), assetmap.to_xml()).unwrap();
    }

    fn write_png(path: &Path) {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        std::fs::write(path, bytes).unwrap();
    }

    fn timed_text_mxf(directory: &Path, image: bool, encryption: Option<MxfEncryption>) {
        let xml = directory.join("subtitle.xml");
        std::fs::write(&xml, smpte_subtitle(image)).unwrap();
        let mut input_files = vec![xml];
        let mut resource_ids = Vec::new();
        if image {
            let png = directory.join("image.png");
            write_png(&png);
            input_files.push(png);
            resource_ids.push(*uuid::Uuid::parse_str(IMAGE_ID).unwrap().as_bytes());
        }
        let track = mxf_wrap(&MxfWrapOptions {
            input_files,
            output: directory.join("subtitle.mxf"),
            essence_type: EssenceType::TimedText,
            standard: MxfStandard::AsDcp,
            fps_num: FRAMES_PER_SECOND,
            fps_den: 1,
            partition_size: 0,
            encryption,
            mca_config: None,
            resource_ids,
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: Some(FRAME_COUNT as u32),
        });
        assert!(track.success, "timed text wrap failed: {}", track.error);
    }

    fn subtitle_cues(
        package: &Path,
        keys: Option<&crate::content_keys::ContentKeys>,
    ) -> Vec<crate::subtitle_formats::StyledCue> {
        let mut timeline = Timeline::open(&package.join("CPL.xml"), keys, &[]).unwrap();
        let loaded = timeline
            .subtitles
            .as_mut()
            .expect("a composition loads its subtitles");
        std::mem::take(&mut loaded.subtitles[0].cues)
    }

    #[test]
    fn an_encrypted_timed_text_track_reads_with_its_key_and_is_refused_without() {
        let directory = tempfile::tempdir().unwrap();
        let encryption = MxfEncryption {
            content_key: SUBTITLE_KEY,
            key_id: SUBTITLE_KEY_ID,
        };
        timed_text_mxf(directory.path(), false, Some(encryption));
        write_package(directory.path(), "subtitle.mxf", "MainSubtitle", 0);
        let keys = content_keys(directory.path(), &[(SUBTITLE_KEY_ID, SUBTITLE_KEY)]);

        let cues = subtitle_cues(directory.path(), Some(&keys));

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].plain_text(), "second reel");
        assert_eq!(cues[0].start_ms, second_reel_cue_start_ms());
        let refusal = Timeline::open(&directory.path().join("CPL.xml"), None, &[])
            .err()
            .unwrap();
        assert!(refusal.contains("subtitle.mxf is encrypted"), "{refusal}");
    }

    #[test]
    fn a_timed_text_image_cue_reads_its_png_from_the_track_file() {
        let directory = tempfile::tempdir().unwrap();
        timed_text_mxf(directory.path(), true, None);
        write_package(directory.path(), "subtitle.mxf", "MainSubtitle", 0);

        // the timeline holds the directory the PNG is written to
        let timeline = Timeline::open(&directory.path().join("CPL.xml"), None, &[]).unwrap();

        let loaded = timeline.subtitles.as_ref().unwrap();
        let image = loaded.subtitles[0].cues[0]
            .image
            .as_ref()
            .expect("an image cue");
        assert_eq!(std::fs::read(image).unwrap()[..PNG_MAGIC.len()], PNG_MAGIC);
    }

    #[test]
    fn an_interop_png_cue_resolves_beside_its_xml_and_honours_the_entry_point() {
        let directory = tempfile::tempdir().unwrap();
        let subtitle_directory: PathBuf = directory.path().join(SUBTITLE_ID);
        std::fs::create_dir(&subtitle_directory).unwrap();
        write_png(&subtitle_directory.join("cue.png"));
        std::fs::write(
            subtitle_directory.join("reel.xml"),
            "<DCSubtitle Version=\"1.0\"><Language>English</Language>\
             <Subtitle SpotNumber=\"1\" TimeIn=\"00:00:00:012\" TimeOut=\"00:00:00:025\">\
             <Image VAlign=\"bottom\" VPosition=\"20\">cue.png</Image></Subtitle></DCSubtitle>",
        )
        .unwrap();
        write_package(
            directory.path(),
            &format!("{SUBTITLE_ID}/reel.xml"),
            "MainSubtitle",
            1,
        );

        let cues = subtitle_cues(directory.path(), None);

        let [cue] = cues.as_slice() else {
            panic!("one cue");
        };
        assert_eq!(
            cue.image.as_deref(),
            Some(subtitle_directory.join("cue.png").as_path())
        );
        // 48 ms into the track, which plays from its second frame in a reel starting FRAME_COUNT frames in
        let expected = milliseconds_of_frames(FRAME_COUNT as u64) + 48 - milliseconds_of_frames(1);
        assert_eq!(cue.start_ms, expected);
    }

    #[test]
    fn a_closed_caption_track_fills_the_caption_slot() {
        let directory = tempfile::tempdir().unwrap();
        timed_text_mxf(directory.path(), false, None);
        write_package(directory.path(), "subtitle.mxf", "tt:ClosedCaption", 0);

        let mut timeline = Timeline::open(&directory.path().join("CPL.xml"), None, &[]).unwrap();
        let loaded = timeline.subtitles.as_mut().unwrap();

        assert!(loaded.subtitles.is_empty());
        assert_eq!(loaded.captions[0].cues.len(), 1);
        assert_eq!(loaded.captions[0].language.as_deref(), Some("en"));
    }

    const IMSC_TEXT: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter"
        xml:lang="en" ttp:frameRate="24"><body><div>
        <p begin="12f" end="36f">entered mid cue</p>
        <p begin="2s" end="3s">Hello<br/>world</p>
        <p begin="7s" end="8s">after the trim</p>
        </div></body></tt>"#;

    const IMSC_IMAGE: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="en"
        xmlns:smpte="http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt"><body>
        <div begin="1s" end="2s" smpte:backgroundImage="Event1.png"/></body></tt>"#;

    fn imsc_track_file(directory: &Path, name: &str, document: &str) -> PathBuf {
        let xml = directory.join(name).with_extension("ttml");
        std::fs::write(&xml, document).unwrap();
        let output = directory.join(name).with_extension("mxf");
        let track = mxf_wrap(&MxfWrapOptions {
            input_files: vec![xml],
            output: output.clone(),
            essence_type: EssenceType::TimedText,
            standard: MxfStandard::As02,
            fps_num: FRAMES_PER_SECOND,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
            resource_ids: Vec::new(),
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: Some(10 * FRAMES_PER_SECOND),
        });
        assert!(
            track.success,
            "AS-02 timed text wrap failed: {}",
            track.error
        );
        output
    }

    // half a second into the second reel, playing 5 s of the file from 1 s in
    fn trimmed_segment(path: PathBuf) -> SubtitleSegment {
        SubtitleSegment {
            path,
            reels: 1..2,
            offset_seconds: 0.5,
            trim: Some(SegmentTrim {
                start_seconds: 1.0,
                length_seconds: Some(5.0),
            }),
            language: None,
        }
    }

    fn two_reels() -> super::Reels {
        super::Reels {
            starts_seconds: vec![0.0, 10.0],
            lengths_seconds: vec![10.0, 10.0],
        }
    }

    #[test]
    fn an_imsc_track_file_shows_its_text_on_the_composition_clock_and_warns_of_unresolved_images() {
        let directory = tempfile::tempdir().unwrap();
        let text = imsc_track_file(directory.path(), "text", IMSC_TEXT);
        let image = imsc_track_file(directory.path(), "image", IMSC_IMAGE);

        let loaded = super::load(
            &[trimmed_segment(text), trimmed_segment(image.clone())],
            &[],
            &two_reels(),
            None,
        )
        .unwrap();

        let [track] = loaded.subtitles.as_slice() else {
            panic!("both documents are English, so one track");
        };
        assert_eq!(track.language.as_deref(), Some("en"));
        let cues: Vec<(u64, u64, String)> = track
            .cues
            .iter()
            .map(|cue| (cue.start_ms, cue.end_ms, cue.plain_text()))
            .collect();
        assert_eq!(
            cues,
            vec![
                (10500, 11000, "entered mid cue".to_string()),
                (11500, 12500, "Hello\nworld".to_string()),
            ]
        );
        let [warning] = loaded.warnings.as_slice() else {
            panic!("one warning for the image document: {:?}", loaded.warnings);
        };
        assert!(
            warning.contains(&image.display().to_string()) && warning.contains("Event1.png"),
            "{warning}"
        );
    }

    // through asdcplib's writer, since mxf_wrap refuses AS-02 resources
    fn as02_image_track_file(
        directory: &Path,
        document: &str,
        image_id: uuid::Uuid,
        png_bytes: &[u8],
    ) -> PathBuf {
        let track_file = directory.join("image.mxf");
        let mut writer = asdcplib::as02::timed_text::MxfWriter::new();
        writer
            .open_write_with_resources(
                &track_file.to_string_lossy(),
                &asdcplib::WriterInfo::default(),
                &asdcplib::timed_text::TimedTextDescriptor {
                    edit_rate: asdcplib::Rational::new(FRAMES_PER_SECOND as i32, 1),
                    container_duration: 10 * FRAMES_PER_SECOND,
                    asset_id: *uuid::Uuid::new_v4().as_bytes(),
                    namespace_uri: "http://www.w3.org/ns/ttml/profile/imsc1/image".to_string(),
                    ucs_encoding: "UTF-8".to_string(),
                },
                &[asdcplib::timed_text::AncillaryResourceInfo {
                    uuid: *image_id.as_bytes(),
                    mime_type: asdcplib::timed_text::MimeType::Png,
                }],
                16_384,
            )
            .unwrap();
        writer
            .write_timed_text_resource(document, None, None)
            .unwrap();
        writer
            .write_ancillary_resource(png_bytes, image_id.as_bytes(), "image/png", None, None)
            .unwrap();
        writer.finalize().unwrap();
        track_file
    }

    #[test]
    fn an_imsc_image_track_file_shows_its_png_in_its_region_on_the_composition_clock() {
        let directory = tempfile::tempdir().unwrap();
        let image_id = uuid::Uuid::parse_str(IMAGE_ID).unwrap();
        let document = format!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="en"
            xmlns:tts="http://www.w3.org/ns/ttml#styling"
            xmlns:smpte="http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt" tts:extent="3840px 2160px">
            <head><layout><region xml:id="cropped" tts:origin="1152px 1728px" tts:extent="1536px 108px"/></layout></head>
            <body><div region="cropped" begin="2s" end="3s" smpte:backgroundImage="urn:uuid:{image_id}"/></body></tt>"#
        );
        let png = directory.path().join("cropped.png");
        write_png(&png);
        let png_bytes = std::fs::read(&png).unwrap();
        let track_file = as02_image_track_file(directory.path(), &document, image_id, &png_bytes);

        let loaded = super::load(&[trimmed_segment(track_file)], &[], &two_reels(), None).unwrap();

        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let [track] = loaded.subtitles.as_slice() else {
            panic!("one track");
        };
        let [cue] = track.cues.as_slice() else {
            panic!("one image cue: {:?}", track.cues);
        };
        assert_eq!((cue.start_ms, cue.end_ms), (11500, 12500));
        let image = cue.image.as_deref().expect("an image cue");
        assert_eq!(std::fs::read(image).unwrap(), png_bytes);
        assert_eq!(
            cue.image_area,
            Some(crate::subtitle_formats::FrameArea {
                left: 30.0,
                top: 80.0,
                width: 40.0,
                height: 5.0,
            })
        );
    }

    #[test]
    fn an_imsc_image_named_by_file_is_found_by_its_name_based_resource_id() {
        // the id asdcplib's as-02-wrap gives Event1.png, which the Plugfest image reel carries
        let event_1_id = uuid::Uuid::parse_str("3dab08a2-6aab-57c3-bd80-e07d58bece3f").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("Event1.png");
        write_png(&png);
        let png_bytes = std::fs::read(&png).unwrap();
        let track_file = as02_image_track_file(
            directory.path(),
            r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="en"
            xmlns:smpte="http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt"><body>
            <div begin="2s" end="3s" smpte:backgroundImage="images/Event1.png"/>
            <div begin="4s" end="5s" smpte:backgroundImage="Event2.png"/></body></tt>"#,
            event_1_id,
            &png_bytes,
        );

        let loaded = super::load(&[trimmed_segment(track_file)], &[], &two_reels(), None).unwrap();

        let [cue] = loaded.subtitles[0].cues.as_slice() else {
            panic!("one image cue: {:?}", loaded.subtitles[0].cues);
        };
        assert_eq!((cue.start_ms, cue.end_ms), (11500, 12500));
        assert_eq!(
            std::fs::read(cue.image.as_deref().unwrap()).unwrap(),
            png_bytes
        );
        let [warning] = loaded.warnings.as_slice() else {
            panic!("one warning for Event2.png: {:?}", loaded.warnings);
        };
        assert!(
            warning.contains("Event2.png") && !warning.contains("Event1.png"),
            "{warning}"
        );
    }
}

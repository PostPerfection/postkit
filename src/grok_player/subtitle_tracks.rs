use std::path::{Path, PathBuf};

use crate::composition_timeline::SubtitleSegment;
use crate::content_keys::ContentKeys;
use crate::subtitle_formats::dcp::{DcpSubtitleDocument, parse_dcp_subtitle};
use crate::subtitle_formats::interop::resolve_png;
use crate::subtitle_formats::{StyledCue, SubtitleError};

use super::MILLISECONDS_PER_SECOND;

const MXF_EXTENSION: &str = "mxf";
const URN_UUID_PREFIX: &str = "urn:uuid:";
const PNG_EXTENSION: &str = "png";
const FIRST_RESOURCE_BUFFER_BYTES: usize = 256 * 1024;
const UNREAD_IMSC_WARNING: &str =
    "this IMF composition's IMSC subtitles do not show, the player reads DCP subtitles only";

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
    pub warning: Option<String>,
    // the PNGs timed text track files carry, written out for the raster to read
    _images: Option<tempfile::TempDir>,
}

// in composition seconds
pub(super) struct Reels {
    pub starts_seconds: Vec<f64>,
    pub lengths_seconds: Vec<f64>,
}

pub(super) fn load(
    subtitles: &[SubtitleSegment],
    captions: &[SubtitleSegment],
    unread_subtitles: bool,
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
    Ok(CompositionSubtitles {
        subtitles: load_track(subtitles, reels, keys, image_directory)?,
        captions: load_track(captions, reels, keys, image_directory)?,
        warning: unread_subtitles.then(|| UNREAD_IMSC_WARNING.to_string()),
        _images: images,
    })
}

fn load_track(
    segments: &[SubtitleSegment],
    reels: &Reels,
    keys: Option<&ContentKeys>,
    image_directory: Option<&Path>,
) -> Result<Vec<CompositionTrack>, String> {
    let mut tracks: Vec<CompositionTrack> = Vec::new();
    for segment in segments {
        let (Some(&reel_start), Some(&reel_length)) = (
            reels.starts_seconds.get(segment.reel),
            reels.lengths_seconds.get(segment.reel),
        ) else {
            continue;
        };
        let document = read_document(&segment.path, keys, image_directory)?;
        let entry_ms =
            to_milliseconds(segment.trim.as_ref().map_or(0.0, |trim| trim.start_seconds));
        let played_ms = to_milliseconds(
            segment
                .trim
                .as_ref()
                .and_then(|trim| trim.length_seconds)
                .unwrap_or(reel_length)
                .min(reel_length),
        );
        let reel_start_ms = to_milliseconds(reel_start);
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

fn read_document(
    path: &Path,
    keys: Option<&ContentKeys>,
    image_directory: Option<&Path>,
) -> Result<DcpSubtitleDocument, String> {
    let named = |error: SubtitleError| format!("{}: {error}", path.display());
    if let (true, Some(image_directory)) = (is_mxf(path), image_directory) {
        let xml = read_timed_text_track(path, keys, image_directory)?;
        return parse_dcp_subtitle(&xml, |reference| {
            let id = reference
                .trim()
                .strip_prefix(URN_UUID_PREFIX)
                .unwrap_or(reference.trim())
                .to_ascii_lowercase();
            let image = image_directory.join(id).with_extension(PNG_EXTENSION);
            image
                .exists()
                .then_some(image.clone())
                .ok_or(SubtitleError::MissingImage(image))
        })
        .map_err(named);
    }
    let xml =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let beside = path.parent().unwrap_or(Path::new("."));
    parse_dcp_subtitle(&xml, |name| resolve_png(beside, name.trim())).map_err(named)
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
    let mut decrypt = match (info.encrypted_essence, keys) {
        (false, _) => None,
        (true, Some(keys)) => Some(keys.decrypt_context(&info, "subtitle")?),
        (true, None) => {
            return Err(format!(
                "{} is encrypted and the preview holds no content key for it",
                path.display()
            ));
        }
    };
    let xml =
        read_resource(|buffer| reader.read_timed_text_resource(buffer, decrypt.as_mut(), None))
            .map_err(named)?;
    let resource_count = reader.ancillary_resource_count().map_err(named)?;
    for index in 0..resource_count {
        let resource = reader.ancillary_resource_info(index).map_err(named)?;
        if resource.mime_type != asdcplib::timed_text::MimeType::Png {
            continue;
        }
        let bytes = read_resource(|buffer| {
            reader.read_ancillary_resource(&resource.uuid, buffer, decrypt.as_mut(), None)
        })
        .map_err(named)?;
        let file: PathBuf = image_directory
            .join(uuid::Uuid::from_bytes(resource.uuid).to_string())
            .with_extension(PNG_EXTENSION);
        std::fs::write(&file, bytes).map_err(|error| format!("{}: {error}", file.display()))?;
    }
    reader.close().map_err(named)?;
    String::from_utf8(xml).map_err(|error| format!("{}: {error}", path.display()))
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
}

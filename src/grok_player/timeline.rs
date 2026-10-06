use std::path::{Path, PathBuf};

use asdcplib::crypto::{AesDecContext, HmacContext};

use super::FrameRange;
use super::audio::{self, KeyedSoundSegment, PlayedFrames};
use crate::composition_timeline::{self, SoundSegment};
use crate::content_keys::ContentKeys;
use crate::preview::{self, PictureReader, ResolvedPicture};
use crate::preview_colour::PictureColour;

// a directory of bare codestreams states no frame rate
pub(super) const CODESTREAM_DIRECTORY_FPS: f64 = 24.0;

const CODESTREAM_EXTENSIONS: [&str; 3] = ["j2c", "j2k", "jp2"];
const MXF_EXTENSION: &str = "mxf";
const CPL_EXTENSION: &str = "xml";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DisplayRender {
    DcpXyz,
    Imf(PictureColour),
    PlainRgb,
}

struct Segment {
    reader: PictureReader,
    resolved: ResolvedPicture,
    first_frame: u32,
    frame_count: u32,
    render: DisplayRender,
    decrypt: Option<AesDecContext>,
}

enum Frames {
    Essence(Vec<Segment>),
    Codestreams {
        files: Vec<PathBuf>,
        render: DisplayRender,
    },
}

pub(super) struct Timeline {
    frames: Frames,
    segment_starts: Vec<u64>,
    // the composition frame shown at position 0, past it frame_count frames play
    pub first_frame: u64,
    pub frame_count: u64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub sound: Vec<KeyedSoundSegment>,
}

impl Timeline {
    pub fn open(
        source: &Path,
        keys: Option<&ContentKeys>,
        other_packages: &[PathBuf],
    ) -> Result<Self, String> {
        if source.is_dir() {
            if crate::assetmap::find(source).is_some() {
                return Self::from_resolved(source, other_packages, keys);
            }
            return Self::from_codestream_directory(source);
        }
        if !source.is_file() {
            return Err(format!("{} is not a file or a directory", source.display()));
        }
        match extension(source).as_deref() {
            Some(MXF_EXTENSION) => {
                Self::from_composition(source, Vec::new(), None, Vec::new(), keys)
            }
            Some(CPL_EXTENSION) => Self::from_resolved(source, other_packages, keys),
            _ => Err(format!(
                "{} is neither a JPEG 2000 MXF, a CPL, nor a directory of codestreams",
                source.display()
            )),
        }
    }

    fn from_resolved(
        source: &Path,
        other_packages: &[PathBuf],
        keys: Option<&ContentKeys>,
    ) -> Result<Self, String> {
        let composition = composition_timeline::resolve_composition(source, other_packages)?;
        Self::from_composition(
            source,
            composition.pictures,
            composition.title,
            composition.sound,
            keys,
        )
    }

    fn from_composition(
        source: &Path,
        segments: Vec<composition_timeline::PictureSegment>,
        title: Option<String>,
        sound: Vec<SoundSegment>,
        keys: Option<&ContentKeys>,
    ) -> Result<Self, String> {
        let listed: Vec<(PathBuf, Option<composition_timeline::SegmentTrim>)> =
            if segments.is_empty() {
                vec![(source.to_path_buf(), None)]
            } else {
                segments
                    .into_iter()
                    .map(|segment| (segment.path, segment.trim))
                    .collect()
            };

        let mut opened = Vec::new();
        let mut segment_starts = Vec::new();
        let mut frame_count = 0u64;
        for (path, trim) in listed {
            let segment = open_segment(&path, trim.as_ref(), keys)?;
            if segment.frame_count == 0 {
                continue;
            }
            segment_starts.push(frame_count);
            frame_count += u64::from(segment.frame_count);
            opened.push(segment);
        }
        let Some(first) = opened.first() else {
            return Err(format!("{} holds no picture frames", source.display()));
        };
        let sound = sound
            .into_iter()
            .map(|segment| {
                let key = audio::sound_content_key(&segment.path, keys)?;
                Ok(KeyedSoundSegment { segment, key })
            })
            .collect::<Result<Vec<_>, String>>()?;
        // a composition mixing frame rates plays at the first reel's
        let (fps, width, height) = (
            first.resolved.fps,
            first.resolved.width,
            first.resolved.height,
        );
        Ok(Timeline {
            segment_starts,
            first_frame: 0,
            frame_count,
            fps,
            width,
            height,
            title: title.unwrap_or_else(|| file_name(source)),
            frames: Frames::Essence(opened),
            sound,
        })
    }

    fn from_codestream_directory(directory: &Path) -> Result<Self, String> {
        let files = codestream_files(directory);
        if files.is_empty() {
            return Err(format!(
                "{} holds neither an ASSETMAP nor any JPEG 2000 codestream",
                directory.display()
            ));
        }
        let first = std::fs::read(&files[0]).map_err(|e| format!("{}: {e}", files[0].display()))?;
        let header = crate::j2k::parse_j2k_header(&first)
            .ok_or_else(|| format!("{} is not a JPEG 2000 codestream", files[0].display()))?;
        let render = if crate::j2k::J2kProfile::from(header.profile).is_dci_cinema() {
            DisplayRender::DcpXyz
        } else {
            DisplayRender::PlainRgb
        };
        let frame_count = files.len() as u64;
        Ok(Timeline {
            segment_starts: vec![0],
            first_frame: 0,
            frame_count,
            fps: CODESTREAM_DIRECTORY_FPS,
            width: header.width,
            height: header.height,
            title: file_name(directory),
            frames: Frames::Codestreams { files, render },
            sound: Vec::new(),
        })
    }

    pub fn play_range(&mut self, range: FrameRange, source: &Path) -> Result<(), String> {
        let composition_frames = self.frame_count;
        let out_frame = range.out_frame.unwrap_or(composition_frames);
        if range.in_frame >= out_frame || out_frame > composition_frames {
            return Err(format!(
                "frames {} to {out_frame} are not a range inside {}, which is {composition_frames} frames long",
                range.in_frame,
                source.display()
            ));
        }
        self.first_frame = range.in_frame;
        self.frame_count = out_frame - range.in_frame;
        Ok(())
    }

    pub fn played_frames(&self) -> PlayedFrames {
        PlayedFrames {
            fps: self.fps,
            first_frame: self.first_frame,
            frame_count: self.frame_count,
        }
    }

    // frame counts from first_frame
    pub fn codestream(&mut self, frame: u64) -> Result<(Vec<u8>, DisplayRender, PathBuf), String> {
        let frame = frame + self.first_frame;
        match &mut self.frames {
            Frames::Codestreams { files, render } => {
                let path = files
                    .get(frame as usize)
                    .ok_or_else(|| format!("frame {frame} is past the end of the sequence"))?;
                let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok((bytes, *render, path.clone()))
            }
            Frames::Essence(segments) => {
                let index = segment_of(&self.segment_starts, frame)
                    .ok_or_else(|| format!("frame {frame} is past the end of the composition"))?;
                let segment = &mut segments[index];
                let local = (frame - self.segment_starts[index]) as u32 + segment.first_frame;
                let codestream = preview::read_j2c_frame(
                    &mut segment.reader,
                    local,
                    segment.decrypt.as_mut(),
                    None,
                )
                .map_err(|e| e.to_string())?;
                Ok((codestream, segment.render, segment.resolved.mxf.clone()))
            }
        }
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        if let Frames::Essence(segments) = &mut self.frames {
            for segment in segments {
                segment.reader.close();
            }
        }
    }
}

fn segment_of(segment_starts: &[u64], frame: u64) -> Option<usize> {
    let after = segment_starts.partition_point(|&start| start <= frame);
    after.checked_sub(1)
}

fn open_segment(
    path: &Path,
    trim: Option<&composition_timeline::SegmentTrim>,
    keys: Option<&ContentKeys>,
) -> Result<Segment, String> {
    if matches!(
        asdcplib::essence_type(&path.to_string_lossy()),
        Ok(asdcplib::EssenceType::Jpeg2000Stereo)
    ) {
        return Err(format!(
            "{} is stereoscopic JPEG 2000 essence, which this player's mono reader cannot read",
            path.display()
        ));
    }
    let resolved = preview::resolve_picture(path).map_err(|e| e.to_string())?;
    let (first_frame, frame_count) = trimmed_range(&resolved, trim);
    let mut reader =
        PictureReader::open(&resolved.mxf, resolved.as02).map_err(|e| e.to_string())?;
    let (mut decrypt, mut hmac) = picture_contexts(&mut reader, &resolved, keys)?.unzip();
    // the hmac on this one read fails a tampered frame at load
    let render = resolve_render(
        &mut reader,
        &resolved,
        first_frame,
        decrypt.as_mut(),
        hmac.as_mut(),
    )?;
    Ok(Segment {
        reader,
        resolved,
        first_frame,
        frame_count,
        render,
        decrypt,
    })
}

fn picture_contexts(
    reader: &mut PictureReader,
    resolved: &ResolvedPicture,
    keys: Option<&ContentKeys>,
) -> Result<Option<(AesDecContext, HmacContext)>, String> {
    if !resolved.encrypted {
        return Ok(None);
    }
    let Some(keys) = keys else {
        return Err(format!(
            "{} is encrypted and the preview holds no content key for it",
            resolved.mxf.display()
        ));
    };
    let info = reader.writer_info().map_err(|e| e.to_string())?;
    keys.decrypt_and_hmac_contexts(&info, "picture").map(Some)
}

fn trimmed_range(
    resolved: &ResolvedPicture,
    trim: Option<&composition_timeline::SegmentTrim>,
) -> (u32, u32) {
    let Some(trim) = trim else {
        return (0, resolved.frame_count);
    };
    let first = (trim.start_seconds * resolved.fps).round().max(0.0) as u32;
    let first = first.min(resolved.frame_count);
    let available = resolved.frame_count - first;
    let length = match trim.length_seconds {
        Some(seconds) => ((seconds * resolved.fps).round().max(0.0) as u32).min(available),
        None => available,
    };
    (first, length)
}

// the profile check the still path routes a frame on
fn resolve_render(
    reader: &mut PictureReader,
    resolved: &ResolvedPicture,
    frame: u32,
    decrypt: Option<&mut AesDecContext>,
    hmac: Option<&mut HmacContext>,
) -> Result<DisplayRender, String> {
    let codestream =
        preview::read_j2c_frame(reader, frame, decrypt, hmac).map_err(|e| e.to_string())?;
    let header = crate::j2k::parse_j2k_header(&codestream).ok_or_else(|| {
        format!(
            "frame {frame} of {} is not a JPEG 2000 codestream",
            resolved.mxf.display()
        )
    })?;
    let profile = crate::j2k::J2kProfile::from(header.profile);
    if profile.is_dci_cinema() {
        return Ok(DisplayRender::DcpXyz);
    }
    if profile == crate::j2k::J2kProfile::Imf {
        let colour =
            crate::preview_colour::resolve_picture_colour(resolved).map_err(|e| e.to_string())?;
        return Ok(DisplayRender::Imf(colour));
    }
    Ok(DisplayRender::PlainRgb)
}

pub(super) fn codestream_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && extension(path).is_some_and(|ext| CODESTREAM_EXTENSIONS.contains(&ext.as_str()))
        })
        .collect();
    files.sort_by_key(|path| file_name(path));
    files
}

pub(super) fn accepts(source: &Path, other_packages: &[PathBuf]) -> bool {
    if source.is_dir() {
        if crate::assetmap::find(source).is_some() {
            return composition_is_readable(source, other_packages);
        }
        return !codestream_files(source).is_empty();
    }
    if !source.is_file() {
        return false;
    }
    match extension(source).as_deref() {
        Some(MXF_EXTENSION) => preview::is_jpeg2000_mxf(source),
        Some(CPL_EXTENSION) => composition_is_readable(source, other_packages),
        _ => false,
    }
}

fn composition_is_readable(source: &Path, other_packages: &[PathBuf]) -> bool {
    composition_timeline::resolve_composition(source, other_packages).is_ok_and(|composition| {
        composition
            .pictures
            .first()
            .is_some_and(|segment| preview::is_jpeg2000_mxf(&segment.path))
    })
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::mxf_unwrap::tests::{wrap, write_frames};
    use crate::mxf_wrap::MxfEncryption;

    pub(in crate::grok_player) const PICTURE_KEY: [u8; 16] = [0x11; 16];
    pub(in crate::grok_player) const PICTURE_KEY_ID: [u8; 16] = [0x22; 16];
    const WRONG_KEY: [u8; 16] = [0x99; 16];
    const UNRELATED_KEY_ID: [u8; 16] = [0x77; 16];

    pub(in crate::grok_player) fn content_keys(
        directory: &Path,
        pairs: &[([u8; 16], [u8; 16])],
    ) -> ContentKeys {
        let keys: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(key_id, key)| {
                serde_json::json!({
                    "key_type": "Mdik",
                    "key_id": uuid::Uuid::from_bytes(*key_id).to_string(),
                    "asset_uuid": "",
                    "content_key_hex": hex::encode(key),
                })
            })
            .collect();
        let path = directory.join("KEYS.json");
        std::fs::write(&path, serde_json::json!({ "keys": keys }).to_string()).unwrap();
        ContentKeys::from_keys_json(&path).unwrap()
    }

    fn encrypted_picture(directory: &Path) -> (PathBuf, Vec<Vec<u8>>) {
        let (paths, frames) = write_frames(directory, "secret");
        let encryption = MxfEncryption {
            content_key: PICTURE_KEY,
            key_id: PICTURE_KEY_ID,
        };
        let mxf = wrap(paths, directory.join("encrypted.mxf"), Some(encryption));
        (mxf, frames)
    }

    fn open_error(source: &Path, keys: Option<&ContentKeys>) -> String {
        Timeline::open(source, keys, &[])
            .err()
            .expect("the timeline must not open")
    }

    #[test]
    fn an_encrypted_picture_reads_back_with_its_content_key() {
        let directory = tempfile::tempdir().unwrap();
        let (mxf, frames) = encrypted_picture(directory.path());
        let keys = content_keys(directory.path(), &[(PICTURE_KEY_ID, PICTURE_KEY)]);

        let mut timeline = Timeline::open(&mxf, Some(&keys), &[]).unwrap();
        for (index, frame) in frames.iter().enumerate() {
            let (codestream, render, _) = timeline.codestream(index as u64).unwrap();
            assert!(
                codestream == *frame,
                "frame {index} differs from its source"
            );
            assert!(render == DisplayRender::DcpXyz);
        }
    }

    #[test]
    fn an_encrypted_picture_with_no_keys_keeps_the_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let (mxf, _) = encrypted_picture(directory.path());
        assert_eq!(
            open_error(&mxf, None),
            format!(
                "{} is encrypted and the preview holds no content key for it",
                mxf.display()
            )
        );
    }

    #[test]
    fn keys_that_do_not_cover_the_picture_name_its_key_id() {
        let directory = tempfile::tempdir().unwrap();
        let (mxf, _) = encrypted_picture(directory.path());
        let keys = content_keys(directory.path(), &[(UNRELATED_KEY_ID, PICTURE_KEY)]);
        assert_eq!(
            open_error(&mxf, Some(&keys)),
            format!(
                "KDM/keys do not cover picture KeyId {}",
                uuid::Uuid::from_bytes(PICTURE_KEY_ID)
            )
        );
    }

    // asdcplib's check value catches a wrong key before the MIC is read
    const CHECK_VALUE_FAILURE: &str = "asdcplib error (code -108)";
    const MIC_FAILURE: &str = "asdcplib error (code -109)";
    const FRAMES_PER_SECOND: i32 = 24;
    const WRITER_BUFFER_BYTES: u32 = 16_384;
    const INITIALISATION_VECTOR: [u8; 16] = [0x9c; 16];

    // a MIC keyed with another key over frames the content key decrypts
    fn picture_with_a_foreign_mic(directory: &Path) -> PathBuf {
        use asdcplib::crypto::AesEncContext;
        use asdcplib::jp2k::{CodestreamHeader, MxfWriter, PictureDescriptor};
        use asdcplib::{LabelSet, Rational, WriterInfo};

        let (_, frames) = write_frames(directory, "foreign");
        let first = &frames[0];
        let header = crate::j2k::parse_j2k_header(first).unwrap();
        let descriptor = PictureDescriptor {
            edit_rate: Rational::new(FRAMES_PER_SECOND, 1),
            sample_rate: Rational::new(FRAMES_PER_SECOND, 1),
            stored_width: header.width,
            stored_height: header.height,
            aspect_ratio: Rational::new(header.width as i32, header.height as i32),
            container_duration: frames.len() as u32,
            codestream: CodestreamHeader::parse(first).unwrap(),
        };
        let info = WriterInfo {
            cryptographic_key_id: PICTURE_KEY_ID,
            encrypted_essence: true,
            uses_hmac: true,
            label_set: LabelSet::Smpte,
            ..Default::default()
        };
        let mxf = directory.join("foreign_mic.mxf");
        let mut writer = MxfWriter::new();
        writer
            .open_write(
                &mxf.to_string_lossy(),
                &info,
                &descriptor,
                WRITER_BUFFER_BYTES,
            )
            .unwrap();
        let mut encryptor = AesEncContext::new();
        encryptor.init_key(&PICTURE_KEY).unwrap();
        encryptor.set_ivec(&INITIALISATION_VECTOR).unwrap();
        let mut hmac = HmacContext::new();
        hmac.init_key(&WRONG_KEY, LabelSet::Smpte).unwrap();
        for frame in &frames {
            writer
                .write_frame(frame, Some(&mut encryptor), Some(&mut hmac))
                .unwrap();
        }
        writer.finalize().unwrap();
        mxf
    }

    #[test]
    fn a_wrong_key_fails_at_load() {
        let directory = tempfile::tempdir().unwrap();
        let (mxf, _) = encrypted_picture(directory.path());
        let keys = content_keys(directory.path(), &[(PICTURE_KEY_ID, WRONG_KEY)]);
        let error = open_error(&mxf, Some(&keys));
        assert!(
            error.contains("read frame 0") && error.contains(CHECK_VALUE_FAILURE),
            "{error}"
        );
    }

    #[test]
    fn a_frame_that_fails_its_mic_fails_at_load() {
        let directory = tempfile::tempdir().unwrap();
        let mxf = picture_with_a_foreign_mic(directory.path());
        let keys = content_keys(directory.path(), &[(PICTURE_KEY_ID, PICTURE_KEY)]);
        let error = open_error(&mxf, Some(&keys));
        assert!(
            error.contains("read frame 0") && error.contains(MIC_FAILURE),
            "{error}"
        );
    }

    #[test]
    fn a_global_frame_lands_in_the_segment_that_holds_it() {
        let starts = [0u64, 48, 120];
        assert_eq!(segment_of(&starts, 0), Some(0));
        assert_eq!(segment_of(&starts, 47), Some(0));
        assert_eq!(segment_of(&starts, 48), Some(1));
        assert_eq!(segment_of(&starts, 119), Some(1));
        assert_eq!(segment_of(&starts, 120), Some(2));
        assert_eq!(segment_of(&starts, 9_999), Some(2));
    }

    #[test]
    fn a_trim_converts_to_frames_at_the_segments_own_rate() {
        let resolved = ResolvedPicture {
            mxf: PathBuf::from("reel.mxf"),
            asset_uuid: String::new(),
            encrypted: false,
            frame_count: 96,
            width: 2048,
            height: 1080,
            fps: 48.0,
            as02: false,
            color_primaries: None,
            transfer_characteristic: None,
            mastering_display_max_luminance: None,
            descriptor_says_ycbcr: false,
            coding_equations: None,
        };
        let trim = composition_timeline::SegmentTrim {
            start_seconds: 1.0,
            length_seconds: Some(0.5),
        };
        assert_eq!(trimmed_range(&resolved, Some(&trim)), (48, 24));

        // a length past the end of the file stops at the end of the file
        let past_end = composition_timeline::SegmentTrim {
            start_seconds: 1.5,
            length_seconds: Some(10.0),
        };
        assert_eq!(trimmed_range(&resolved, Some(&past_end)), (72, 24));

        // no trim at all plays every frame
        assert_eq!(trimmed_range(&resolved, None), (0, 96));
    }
}

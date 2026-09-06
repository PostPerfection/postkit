use std::path::{Path, PathBuf};

use crate::composition_timeline;
use crate::preview::{self, PictureReader, ResolvedPicture};
use crate::preview_colour::PictureColour;

// a directory of bare codestreams states no frame rate
pub(super) const CODESTREAM_DIRECTORY_FPS: f64 = 24.0;

const CODESTREAM_EXTENSIONS: [&str; 3] = ["j2c", "j2k", "jp2"];
const MXF_EXTENSION: &str = "mxf";
const CPL_EXTENSION: &str = "xml";

#[derive(Clone, Copy)]
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
    pub frame_count: u64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub title: String,
}

impl Timeline {
    pub fn open(source: &Path) -> Result<Self, String> {
        if source.is_dir() {
            if crate::assetmap::find(source).is_some() {
                let (segments, title) = composition_timeline::read_composition(source);
                return Self::from_composition(source, segments, title);
            }
            return Self::from_codestream_directory(source);
        }
        if !source.is_file() {
            return Err(format!("{} is not a file or a directory", source.display()));
        }
        match extension(source).as_deref() {
            Some(MXF_EXTENSION) => Self::from_composition(source, Vec::new(), None),
            Some(CPL_EXTENSION) => {
                let (segments, title) = composition_timeline::read_composition_from_cpl(source);
                if segments.is_empty() {
                    return Err(format!(
                        "{} names no picture this player can resolve",
                        source.display()
                    ));
                }
                Self::from_composition(source, segments, title)
            }
            _ => Err(format!(
                "{} is neither a JPEG 2000 MXF, a CPL, nor a directory of codestreams",
                source.display()
            )),
        }
    }

    fn from_composition(
        source: &Path,
        segments: Vec<composition_timeline::PictureSegment>,
        title: Option<String>,
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
            let segment = open_segment(&path, trim.as_ref())?;
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
        // a composition mixing frame rates plays at the first reel's
        let (fps, width, height) = (
            first.resolved.fps,
            first.resolved.width,
            first.resolved.height,
        );
        Ok(Timeline {
            segment_starts,
            frame_count,
            fps,
            width,
            height,
            title: title.unwrap_or_else(|| file_name(source)),
            frames: Frames::Essence(opened),
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
            frame_count,
            fps: CODESTREAM_DIRECTORY_FPS,
            width: header.width,
            height: header.height,
            title: file_name(directory),
            frames: Frames::Codestreams { files, render },
        })
    }

    pub fn codestream(&mut self, frame: u64) -> Result<(Vec<u8>, DisplayRender, PathBuf), String> {
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
                let codestream = preview::read_j2c_frame(&mut segment.reader, local, None)
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
    if resolved.encrypted {
        return Err(format!(
            "{} is encrypted and the preview holds no content key for it",
            resolved.mxf.display()
        ));
    }
    let (first_frame, frame_count) = trimmed_range(&resolved, trim);
    let mut reader =
        PictureReader::open(&resolved.mxf, resolved.as02).map_err(|e| e.to_string())?;
    let render = resolve_render(&mut reader, &resolved, first_frame)?;
    Ok(Segment {
        reader,
        resolved,
        first_frame,
        frame_count,
        render,
    })
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
) -> Result<DisplayRender, String> {
    let codestream = preview::read_j2c_frame(reader, frame, None).map_err(|e| e.to_string())?;
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

pub(super) fn accepts(source: &Path) -> bool {
    if source.is_dir() {
        if crate::assetmap::find(source).is_some() {
            return first_picture_is_readable(composition_timeline::read_composition(source).0);
        }
        return !codestream_files(source).is_empty();
    }
    if !source.is_file() {
        return false;
    }
    match extension(source).as_deref() {
        Some(MXF_EXTENSION) => preview::is_jpeg2000_mxf(source),
        Some(CPL_EXTENSION) => {
            first_picture_is_readable(composition_timeline::read_composition_from_cpl(source).0)
        }
        _ => false,
    }
}

fn first_picture_is_readable(segments: Vec<composition_timeline::PictureSegment>) -> bool {
    segments
        .first()
        .is_some_and(|segment| preview::is_jpeg2000_mxf(&segment.path))
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
mod tests {
    use super::*;

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

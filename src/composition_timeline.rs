//! A package directory to the one mpv source that plays its whole composition.
//!
//! Picking a picture MXF by filename or size plays a single reel of a
//! multi-reel composition. The CPL is the only document that says which track
//! files belong to the composition, in what order, how much of each one plays
//! and what the whole thing is called, so resolution goes ASSETMAP → CPL →
//! picture track files and mpv gets them as one EDL timeline.

use crate::cpl_xml::read_prefixed_tag;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The one mpv source that plays a composition, and the title to show for it.
#[derive(Debug, PartialEq)]
pub struct CompositionSource {
    pub uri: String,
    pub title: Option<String>,
}

/// A picture track file as the composition plays it: the whole file, or only
/// the span the reel enters and leaves at.
#[derive(Debug, PartialEq)]
pub struct PictureSegment {
    pub path: PathBuf,
    pub trim: Option<SegmentTrim>,
}

/// A sound track file as the composition plays it, same span rules as
/// picture so the two stay on one clock.
#[derive(Debug, PartialEq)]
pub struct SoundSegment {
    pub path: PathBuf,
    pub trim: Option<SegmentTrim>,
    // an AS-02 clip is read in edit units of this, a DCP reel's sound is wrapped a picture frame an edit unit
    pub edit_rate: Option<(i32, i32)>,
}

/// Seconds into the file and seconds of it. Length is None when the CPL states
/// an entry point without a duration.
#[derive(Debug, PartialEq)]
pub struct SegmentTrim {
    pub start_seconds: f64,
    pub length_seconds: Option<f64>,
}

// an ST 429-5 timed text MXF or an Interop subtitle XML
#[derive(Debug, PartialEq)]
pub struct SubtitleSegment {
    pub path: PathBuf,
    // the reel it plays in, which is the picture segment of the same index
    pub reel: usize,
    pub trim: Option<SegmentTrim>,
    pub language: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Composition {
    pub pictures: Vec<PictureSegment>,
    pub sound: Vec<SoundSegment>,
    pub subtitles: Vec<SubtitleSegment>,
    pub captions: Vec<SubtitleSegment>,
    // an IMF composition's IMSC subtitles, which the player has no reader for
    pub unread_subtitles: bool,
    pub title: Option<String>,
}

/// A track file the CPL names, as the id to look up and the span to play.
struct TrackFileReference {
    asset_id: String,
    trim: Option<SegmentTrim>,
    edit_rate: Option<(i32, i32)>,
}

/// The mpv source that plays every reel of the composition in `package_dir`.
///
/// None when the package has no ASSETMAP, no CPL, or a CPL naming no picture,
/// which leaves the caller on its own single-file fallback.
pub fn mpv_source(package_dir: &Path) -> Option<CompositionSource> {
    let (segments, title) = read_composition(package_dir);
    let uri = match segments.as_slice() {
        [] => return None,
        // one untrimmed reel is the file it always was: an EDL wrapper would
        // change the demuxer and add a chapter for no gain
        [only] if only.trim.is_none() => only.path.to_string_lossy().into_owned(),
        several => edl_uri(several),
    };
    Some(CompositionSource { uri, title })
}

/// Every picture track file the composition names, in composition order, with
/// the composition title.
pub fn read_composition(package_dir: &Path) -> (Vec<PictureSegment>, Option<String>) {
    let Some(assets) = package_assets(package_dir) else {
        return (Vec::new(), None);
    };
    let Some(cpl) = first_cpl(package_dir, &assets) else {
        return (Vec::new(), None);
    };
    (
        segments_of(package_dir, &assets, &cpl),
        composition_title(&cpl),
    )
}

fn package_assets(package_dir: &Path) -> Option<Vec<(String, String)>> {
    let assetmap = crate::assetmap::find(package_dir)?;
    Some(crate::assetmap::parse_ordered(&assetmap))
}

// the CPL's own package is searched first, then other_packages in order
pub fn resolve_composition(
    source: &Path,
    other_packages: &[PathBuf],
) -> Result<Composition, String> {
    let (package_dir, cpl) = composition_document(source)?;
    let searched: Vec<PathBuf> = std::iter::once(package_dir)
        .chain(other_packages.iter().cloned())
        .collect();
    let paths_by_id = asset_paths(&searched)?;
    let missing_asset_error = |asset_id: String| {
        let directories: Vec<String> = searched
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();
        format!(
            "asset {asset_id} is in none of the packages searched: {}",
            directories.join(", ")
        )
    };
    let pictures = located(picture_references(&cpl), &paths_by_id)
        .into_iter()
        .map(|found| {
            found.map(|(path, reference)| PictureSegment {
                path,
                trim: reference.trim,
            })
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(missing_asset_error)?;
    if pictures.is_empty() {
        return Err(format!("{} names no picture", source.display()));
    }
    let sound = located(sound_references(&cpl), &paths_by_id)
        .into_iter()
        .map(|found| {
            found.map(|(path, reference)| SoundSegment {
                path,
                trim: reference.trim,
                edit_rate: reference.edit_rate,
            })
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(missing_asset_error)?;
    let subtitle_segments = |element: &str| {
        reel_subtitle_references(&cpl, element)
            .into_iter()
            .map(|(reel, reference, language)| {
                let path = paths_by_id
                    .get(&reference.asset_id)
                    .ok_or_else(|| missing_asset_error(reference.asset_id.clone()))?;
                Ok(SubtitleSegment {
                    path: path.clone(),
                    reel,
                    trim: reference.trim,
                    language,
                })
            })
            .collect::<Result<Vec<_>, String>>()
    };
    Ok(Composition {
        pictures,
        sound,
        subtitles: subtitle_segments(MAIN_SUBTITLE_ELEMENT)?,
        captions: subtitle_segments(CLOSED_CAPTION_ELEMENT_PATTERN)?,
        unread_subtitles: !element_blocks(&cpl, IMF_SUBTITLES_SEQUENCE_ELEMENT).is_empty(),
        title: composition_title(&cpl),
    })
}

const MAIN_SUBTITLE_ELEMENT: &str = "MainSubtitle";
const IMF_SUBTITLES_SEQUENCE_ELEMENT: &str = "SubtitlesSequence";
// ST 429-12 names it ClosedCaption, the Interop CPL MainClosedCaption
const CLOSED_CAPTION_ELEMENT_PATTERN: &str = "(?:Main)?ClosedCaption";

// every track of the kind in each reel, a reel can carry one a language, with the reel's index
fn reel_subtitle_references(
    cpl: &str,
    element: &str,
) -> Vec<(usize, TrackFileReference, Option<String>)> {
    element_blocks(cpl, "Reel")
        .into_iter()
        .enumerate()
        .flat_map(|(reel, block)| {
            element_blocks(block, element)
                .into_iter()
                .filter_map(move |track| {
                    let reference = TrackFileReference {
                        asset_id: uuid_in(track, "Id")?,
                        trim: segment_trim(track, "Duration", None),
                        edit_rate: None,
                    };
                    Some((reel, reference, read_prefixed_tag(track, "Language")))
                })
        })
        .collect()
}

// the library packages holding the pictures and sound the CPL's own package lacks
pub fn find_original_version_packages(
    source: &Path,
    library: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let (package_dir, cpl) = composition_document(source)?;
    let own_assets = asset_paths(std::slice::from_ref(&package_dir))?;
    let mut missing: Vec<String> = picture_references(&cpl)
        .into_iter()
        .chain(sound_references(&cpl))
        .chain(
            reel_subtitle_references(&cpl, MAIN_SUBTITLE_ELEMENT)
                .into_iter()
                .map(|(_, reference, _)| reference),
        )
        .chain(
            reel_subtitle_references(&cpl, CLOSED_CAPTION_ELEMENT_PATTERN)
                .into_iter()
                .map(|(_, reference, _)| reference),
        )
        .map(|reference| reference.asset_id)
        .filter(|asset_id| !own_assets.contains_key(asset_id))
        .collect();
    let mut seen = HashSet::new();
    missing.retain(|asset_id| seen.insert(asset_id.clone()));
    let own_package = std::fs::canonicalize(&package_dir).ok();
    let mut found = Vec::new();
    for candidate in library {
        if missing.is_empty() {
            break;
        }
        if std::fs::canonicalize(candidate).ok() == own_package {
            continue;
        }
        let Some(assets) = package_assets(candidate) else {
            continue;
        };
        let held = |asset_id: &String| assets.iter().any(|(id, _)| id == asset_id);
        if !missing.iter().any(held) {
            continue;
        }
        missing.retain(|asset_id| !held(asset_id));
        found.push(candidate.clone());
    }
    if !missing.is_empty() {
        return Err(format!(
            "no package in the library holds {}",
            missing.join(", ")
        ));
    }
    Ok(found)
}

// the package directory and the text of its first CPL, or of the CPL file named
fn composition_document(source: &Path) -> Result<(PathBuf, String), String> {
    if source.is_dir() {
        let assets = package_assets(source)
            .ok_or_else(|| format!("{} holds no ASSETMAP", source.display()))?;
        let cpl = first_cpl(source, &assets)
            .ok_or_else(|| format!("{} holds no CPL", source.display()))?;
        return Ok((source.to_path_buf(), cpl));
    }
    let cpl = std::fs::read_to_string(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let package_dir = source.parent().unwrap_or(Path::new(".")).to_path_buf();
    Ok((package_dir, cpl))
}

// the first package that lists an id wins
fn asset_paths(packages: &[PathBuf]) -> Result<HashMap<String, PathBuf>, String> {
    let mut paths_by_id = HashMap::new();
    for package in packages {
        let assets = package_assets(package)
            .ok_or_else(|| format!("{} holds no ASSETMAP", package.display()))?;
        for (asset_id, relative) in assets {
            paths_by_id
                .entry(asset_id)
                .or_insert_with(|| package.join(relative));
        }
    }
    Ok(paths_by_id)
}

// each reference's file, or the id no package holds
fn located(
    references: Vec<TrackFileReference>,
    paths_by_id: &HashMap<String, PathBuf>,
) -> Vec<Result<(PathBuf, TrackFileReference), String>> {
    references
        .into_iter()
        .map(|reference| match paths_by_id.get(&reference.asset_id) {
            Some(path) => Ok((path.clone(), reference)),
            None => Err(reference.asset_id),
        })
        .collect()
}

fn segments_of(package_dir: &Path, assets: &[(String, String)], cpl: &str) -> Vec<PictureSegment> {
    resolve_segments(package_dir, assets, picture_references(cpl))
        .into_iter()
        .map(|(path, trim)| PictureSegment { path, trim })
        .collect()
}

fn resolve_segments(
    package_dir: &Path,
    assets: &[(String, String)],
    references: Vec<TrackFileReference>,
) -> Vec<(PathBuf, Option<SegmentTrim>)> {
    let paths_by_id: HashMap<String, PathBuf> = assets
        .iter()
        .map(|(id, relative)| (id.clone(), package_dir.join(relative)))
        .collect();
    located(references, &paths_by_id)
        .into_iter()
        .filter_map(Result::ok)
        .map(|(path, reference)| (path, reference.trim))
        .collect()
}

/// The text of the first CPL in ASSETMAP order. ASSETMAP order is the only
/// order a package states, so a package holding several CPLs resolves to the
/// same one on every run.
fn first_cpl(package_dir: &Path, assets: &[(String, String)]) -> Option<String> {
    assets
        .iter()
        .map(|(_, relative)| package_dir.join(relative))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
        })
        .find_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            crate::cpl_xml::is_composition_playlist(&text).then_some(text)
        })
}

/// Every picture the CPL names, in composition order: a DCP CPL's reel
/// MainPicture ids (ST 429-7), or an IMF CPL's MainImageSequence resource
/// TrackFileIds (ST 2067-3).
fn picture_references(cpl: &str) -> Vec<TrackFileReference> {
    let reel_pictures = main_picture_references(cpl);
    if reel_pictures.is_empty() {
        return sequence_resource_references(cpl, "MainImageSequence");
    }
    reel_pictures
}

// an IMF CPL's other audio tracks are other languages or mixes of the same picture
fn sound_references(cpl: &str) -> Vec<TrackFileReference> {
    let reel_sound = reel_asset_references(cpl, "MainSound");
    if reel_sound.is_empty() {
        return sequence_resource_references(cpl, "MainAudioSequence");
    }
    reel_sound
}

/// One MainPicture per reel, so a single forward scan gives reel order.
fn main_picture_references(cpl: &str) -> Vec<TrackFileReference> {
    reel_asset_references(cpl, crate::cpl_xml::MAIN_PICTURE_ELEMENT_PATTERN)
}

/// One named reel asset per reel (MainPicture, MainSound), in reel order.
fn reel_asset_references(cpl: &str, name: &str) -> Vec<TrackFileReference> {
    element_blocks(cpl, name)
        .into_iter()
        .filter_map(|block| {
            Some(TrackFileReference {
                asset_id: uuid_in(block, "Id")?,
                trim: segment_trim(block, "Duration", None),
                edit_rate: None,
            })
        })
        .collect()
}

/// A sequence may list several resources, so the sequence blocks come first
/// and the track files are read inside each one. Only the sequences on the
/// first sequence's TrackId are read, one track through every segment. A
/// resource without its own EditRate plays at the composition's.
fn sequence_resource_references(cpl: &str, sequence_name: &str) -> Vec<TrackFileReference> {
    let composition_edit_rate = read_prefixed_tag(cpl, "EditRate");
    let sequences = element_blocks(cpl, sequence_name);
    let first_track = sequences
        .first()
        .and_then(|sequence| uuid_in(sequence, "TrackId"));
    sequences
        .into_iter()
        .filter(|sequence| uuid_in(sequence, "TrackId") == first_track)
        .flat_map(|sequence| element_blocks(sequence, "Resource"))
        .filter_map(|resource| {
            let edit_rate =
                read_prefixed_tag(resource, "EditRate").or_else(|| composition_edit_rate.clone());
            let seconds_per_unit = edit_rate.as_deref().and_then(seconds_per_edit_unit);
            Some(TrackFileReference {
                asset_id: uuid_in(resource, "TrackFileId")?,
                trim: segment_trim(resource, "SourceDuration", seconds_per_unit),
                edit_rate: edit_rate.as_deref().and_then(edit_rate_numbers),
            })
        })
        .collect()
}

/// The span of the file this segment plays, None when it plays all of it. A
/// trimmed segment whose edit rate is missing or unparseable degrades to the
/// whole file rather than to a failed resolution.
fn segment_trim(
    block: &str,
    duration_element: &str,
    fallback_seconds_per_unit: Option<f64>,
) -> Option<SegmentTrim> {
    let entry_point = element_u64(block, "EntryPoint").unwrap_or(0);
    let stated_duration = element_u64(block, duration_element);
    let intrinsic_duration = element_u64(block, "IntrinsicDuration");
    let plays_part_of_the_file = match (stated_duration, intrinsic_duration) {
        (Some(stated), Some(intrinsic)) => stated != intrinsic,
        _ => false,
    };
    if entry_point == 0 && !plays_part_of_the_file {
        return None;
    }
    let seconds_per_unit = read_prefixed_tag(block, "EditRate")
        .as_deref()
        .and_then(seconds_per_edit_unit)
        .or(fallback_seconds_per_unit)?;
    Some(SegmentTrim {
        start_seconds: entry_point as f64 * seconds_per_unit,
        length_seconds: stated_duration.map(|units| units as f64 * seconds_per_unit),
    })
}

/// The title the CPL states: a DCP's ContentTitleText, else an IMF's
/// ContentTitle.
fn composition_title(cpl: &str) -> Option<String> {
    let stated = read_prefixed_tag(cpl, "ContentTitleText")
        .or_else(|| read_prefixed_tag(cpl, "ContentTitle"))?;
    let title = match quick_xml::escape::unescape(&stated) {
        Ok(unescaped) => unescaped.into_owned(),
        Err(_) => stated,
    };
    (!title.is_empty()).then_some(title)
}

/// Each `name` element of `xml`, bounded by its own close tag and never running
/// past the next one that opens, so a reel missing a close tag still resolves
/// instead of swallowing the reels after it.
pub(crate) fn element_blocks<'text>(xml: &'text str, name: &str) -> Vec<&'text str> {
    let prefix = crate::cpl_xml::ELEMENT_PREFIX_PATTERN;
    let Ok(open) = regex::Regex::new(&format!(r"<{prefix}{name}\b")) else {
        return Vec::new();
    };
    let Ok(close) = regex::Regex::new(&format!(r"</{prefix}{name}>")) else {
        return Vec::new();
    };
    let starts: Vec<usize> = open.find_iter(xml).map(|found| found.start()).collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let next_open = starts.get(index + 1).copied().unwrap_or(xml.len());
            let end = close
                .find_at(xml, start)
                .map(|found| found.end())
                .filter(|&end| end <= next_open)
                .unwrap_or(next_open);
            &xml[start..end]
        })
        .collect()
}

fn element_u64(block: &str, name: &str) -> Option<u64> {
    read_prefixed_tag(block, name)?.parse().ok()
}

/// The bare lowercased uuid in the first `name` element of `block`.
fn uuid_in(block: &str, name: &str) -> Option<String> {
    let prefix = crate::cpl_xml::ELEMENT_PREFIX_PATTERN;
    let pattern = format!(r"<{prefix}{name}>\s*(?:urn:uuid:)?([0-9a-fA-F-]{{36}})");
    let found = regex::Regex::new(&pattern).ok()?.captures(block)?;
    Some(found[1].to_ascii_lowercase())
}

fn edit_rate_numbers(edit_rate: &str) -> Option<(i32, i32)> {
    let mut parts = edit_rate.split_whitespace();
    let numerator = parts.next()?.parse().ok()?;
    let denominator = parts.next().unwrap_or("1").parse().ok()?;
    Some((numerator, denominator))
}

/// An EditRate of "num den" as the seconds one edit unit lasts.
fn seconds_per_edit_unit(edit_rate: &str) -> Option<f64> {
    let mut parts = edit_rate.split_whitespace();
    let numerator: f64 = parts.next()?.parse().ok()?;
    let denominator: f64 = parts.next().unwrap_or("1").parse().ok()?;
    (numerator > 0.0).then_some(denominator / numerator)
}

/// mpv's inline EDL URI (DOCS/edl-mpv.rst): one segment per file, separated by
/// `;`, played as a single virtual timeline. Every path is length-prefixed as
/// `%<bytes>%<path>` because a bare value may not hold `,`, `;`, newline or `!`.
/// A trimmed reel adds the positional `,start,length` in seconds.
fn edl_uri(segments: &[PictureSegment]) -> String {
    let segments: Vec<String> = segments.iter().map(edl_segment).collect();
    format!("edl://{}", segments.join(";"))
}

fn edl_segment(segment: &PictureSegment) -> String {
    let path = segment.path.to_string_lossy();
    let file = format!("%{}%{path}", path.len());
    let Some(trim) = &segment.trim else {
        return file;
    };
    match trim.length_seconds {
        Some(length) => format!("{file},{},{length}", trim.start_seconds),
        None => format!("{file},{}", trim.start_seconds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REEL_UUIDS: [&str; 3] = [
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
        "33333333-3333-3333-3333-333333333333",
    ];

    fn picture_files(package_dir: &Path) -> Vec<PathBuf> {
        read_composition(package_dir)
            .0
            .into_iter()
            .map(|segment| segment.path)
            .collect()
    }

    fn source_uri(package_dir: &Path) -> Option<String> {
        mpv_source(package_dir).map(|source| source.uri)
    }

    fn whole_files(paths: &[&str]) -> Vec<PictureSegment> {
        paths
            .iter()
            .map(|path| PictureSegment {
                path: PathBuf::from(path),
                trim: None,
            })
            .collect()
    }

    /// An ASSETMAP listing the CPL first, then each named file in order.
    fn write_assetmap(dir: &Path, cpl_name: &str, assets: &[(&str, &str)]) {
        let mut xml = String::from("<AssetMap><AssetList>");
        xml.push_str(&format!(
            "<Asset><Id>urn:uuid:cc10cc10-0000-0000-0000-000000000000</Id>\
             <ChunkList><Chunk><Path>{cpl_name}</Path></Chunk></ChunkList></Asset>"
        ));
        for (id, path) in assets {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.join("ASSETMAP.xml"), xml).unwrap();
    }

    fn dcp_cpl(picture_ids: &[&str]) -> String {
        let reels: String = picture_ids
            .iter()
            .map(|id| {
                format!(
                    "<Reel><AssetList><MainPicture><Id>urn:uuid:{id}</Id>\
                     <Duration>48</Duration></MainPicture></AssetList></Reel>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"x\">\
             <Id>urn:uuid:cc10cc10-0000-0000-0000-000000000000</Id>\
             <ReelList>{reels}</ReelList></CompositionPlaylist>"
        )
    }

    fn dcp_cpl_with_sound(reels: &[(&str, &str)]) -> String {
        let reels: String = reels
            .iter()
            .map(|(picture, sound)| {
                format!(
                    "<Reel><AssetList>\
                     <MainPicture><Id>urn:uuid:{picture}</Id><Duration>48</Duration></MainPicture>\
                     <MainSound><Id>urn:uuid:{sound}</Id><Duration>48</Duration></MainSound>\
                     </AssetList></Reel>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"x\">\
             <Id>urn:uuid:cc10cc10-0000-0000-0000-000000000000</Id>\
             <ReelList>{reels}</ReelList></CompositionPlaylist>"
        )
    }

    fn imf_cpl(track_file_ids: &[&str]) -> String {
        let resources: String = track_file_ids
            .iter()
            .map(|id| {
                format!(
                    "<Resource><Id>urn:uuid:{id}</Id>\
                     <TrackFileId>urn:uuid:{id}</TrackFileId></Resource>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"y\">\
             <SegmentList><Segment><SequenceList>\
             <cc:MainImageSequence xmlns:cc=\"z\"><ResourceList>{resources}</ResourceList>\
             </cc:MainImageSequence>\
             </SequenceList></Segment></SegmentList></CompositionPlaylist>"
        )
    }

    #[test]
    fn every_reel_plays_in_reel_order() {
        let dir = tempfile::tempdir().unwrap();
        // ASSETMAP order deliberately disagrees with reel order
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[
                (REEL_UUIDS[2], "tail.mxf"),
                (REEL_UUIDS[0], "head.mxf"),
                (REEL_UUIDS[1], "feature.mxf"),
            ],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS)).unwrap();

        assert_eq!(
            picture_files(dir.path()),
            vec![
                dir.path().join("head.mxf"),
                dir.path().join("feature.mxf"),
                dir.path().join("tail.mxf"),
            ]
        );
    }

    #[test]
    fn a_multi_reel_package_becomes_one_edl_timeline() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[(REEL_UUIDS[0], "head.mxf"), (REEL_UUIDS[1], "feature.mxf")],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS[..2])).unwrap();

        let head = dir.path().join("head.mxf").to_string_lossy().into_owned();
        let feature = dir
            .path()
            .join("feature.mxf")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            source_uri(dir.path()),
            Some(format!(
                "edl://%{}%{head};%{}%{feature}",
                head.len(),
                feature.len()
            ))
        );
    }

    #[test]
    fn a_single_reel_package_stays_a_plain_path() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "only.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS[..1])).unwrap();

        assert_eq!(
            source_uri(dir.path()),
            Some(dir.path().join("only.mxf").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn a_package_resolves_main_sound_beside_picture() {
        const SOUND: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[(REEL_UUIDS[0], "picture.mxf"), (SOUND, "sound.mxf")],
        );
        std::fs::write(
            dir.path().join("CPL_a.xml"),
            dcp_cpl_with_sound(&[(REEL_UUIDS[0], SOUND)]),
        )
        .unwrap();

        assert_eq!(
            picture_files(dir.path()),
            vec![dir.path().join("picture.mxf")]
        );
        assert_eq!(
            resolve_composition(dir.path(), &[])
                .unwrap()
                .sound
                .into_iter()
                .map(|segment| segment.path)
                .collect::<Vec<_>>(),
            vec![dir.path().join("sound.mxf")]
        );
    }

    #[test]
    fn a_picture_only_package_has_no_sound() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "only.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS[..1])).unwrap();
        assert!(
            resolve_composition(dir.path(), &[])
                .unwrap()
                .sound
                .is_empty()
        );
    }

    #[test]
    fn separators_in_a_path_survive_the_length_prefix() {
        assert_eq!(
            edl_uri(&whole_files(&["/dcp/reel,one;a.mxf", "/dcp/reel two!.mxf"])),
            "edl://%19%/dcp/reel,one;a.mxf;%18%/dcp/reel two!.mxf"
        );
    }

    #[test]
    fn the_prefix_counts_bytes_not_characters() {
        let uri = edl_uri(&whole_files(&["/dcp/café.mxf"]));
        assert_eq!(uri, "edl://%14%/dcp/café.mxf");
        assert_eq!(uri.strip_prefix("edl://%14%").unwrap().len(), 14);
    }

    #[test]
    fn a_3d_reel_resolves_its_stereoscopic_picture() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "stereo.mxf")]);
        std::fs::write(
            dir.path().join("CPL_a.xml"),
            format!(
                "<CompositionPlaylist xmlns=\"x\"><ReelList><Reel><AssetList>\
                 <msp-cpl:MainStereoscopicPicture xmlns:msp-cpl=\"y\">\
                 <Id>urn:uuid:{}</Id><Duration>48</Duration>\
                 </msp-cpl:MainStereoscopicPicture></AssetList></Reel></ReelList>\
                 </CompositionPlaylist>",
                REEL_UUIDS[0]
            ),
        )
        .unwrap();

        let composition = resolve_composition(dir.path(), &[]).unwrap();
        assert_eq!(composition.pictures.len(), 1);
        assert_eq!(composition.pictures[0].path, dir.path().join("stereo.mxf"));
    }

    #[test]
    fn an_imf_composition_resolves_its_image_track_files() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[
                (REEL_UUIDS[0], "VIDEO_one.mxf"),
                (REEL_UUIDS[1], "VIDEO_two.mxf"),
            ],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), imf_cpl(&REEL_UUIDS[..2])).unwrap();

        assert_eq!(
            picture_files(dir.path()),
            vec![
                dir.path().join("VIDEO_one.mxf"),
                dir.path().join("VIDEO_two.mxf"),
            ]
        );
    }

    const AUDIO_TRACK_UUIDS: [&str; 2] = [
        "a0d10000-0000-4000-8000-000000000001",
        "a0d10000-0000-4000-8000-000000000002",
    ];

    // two segments, each with a picture resource and the same two audio tracks, English first
    fn imf_cpl_with_two_audio_tracks(
        pictures: [&str; 2],
        english: [&str; 2],
        french: [&str; 2],
    ) -> String {
        let audio = |track: &str, file: &str| {
            format!(
                "<cc:MainAudioSequence xmlns:cc=\"z\"><TrackId>urn:uuid:{track}</TrackId><ResourceList>\
                 <Resource><EditRate>24000 1001</EditRate><TrackFileId>urn:uuid:{file}</TrackFileId>\
                 </Resource></ResourceList></cc:MainAudioSequence>"
            )
        };
        let segment = |index: usize| {
            format!(
                "<Segment><SequenceList><cc:MainImageSequence xmlns:cc=\"z\"><ResourceList>\
                 <Resource><TrackFileId>urn:uuid:{}</TrackFileId></Resource></ResourceList>\
                 </cc:MainImageSequence>{}{}</SequenceList></Segment>",
                pictures[index],
                audio(AUDIO_TRACK_UUIDS[0], english[index]),
                audio(AUDIO_TRACK_UUIDS[1], french[index]),
            )
        };
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"y\"><EditRate>24000 1001</EditRate>\
             <SegmentList>{}{}</SegmentList></CompositionPlaylist>",
            segment(0),
            segment(1)
        )
    }

    #[test]
    fn an_imf_composition_plays_its_first_audio_track_through_every_segment() {
        let dir = tempfile::tempdir().unwrap();
        let english = [
            "e0000000-0000-4000-8000-000000000001",
            "e0000000-0000-4000-8000-000000000002",
        ];
        let french = [
            "f0000000-0000-4000-8000-000000000001",
            "f0000000-0000-4000-8000-000000000002",
        ];
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[
                (REEL_UUIDS[0], "VIDEO_one.mxf"),
                (REEL_UUIDS[1], "VIDEO_two.mxf"),
                (english[0], "AUDIO_en_one.mxf"),
                (english[1], "AUDIO_en_two.mxf"),
                (french[0], "AUDIO_fr_one.mxf"),
                (french[1], "AUDIO_fr_two.mxf"),
            ],
        );
        std::fs::write(
            dir.path().join("CPL_a.xml"),
            imf_cpl_with_two_audio_tracks([REEL_UUIDS[0], REEL_UUIDS[1]], english, french),
        )
        .unwrap();

        let composition = resolve_composition(dir.path(), &[]).unwrap();

        let sound: Vec<(PathBuf, Option<(i32, i32)>)> = composition
            .sound
            .into_iter()
            .map(|segment| (segment.path, segment.edit_rate))
            .collect();
        assert_eq!(
            sound,
            vec![
                (dir.path().join("AUDIO_en_one.mxf"), Some((24000, 1001))),
                (dir.path().join("AUDIO_en_two.mxf"), Some((24000, 1001))),
            ]
        );
    }

    #[test]
    fn an_imf_subtitles_sequence_is_marked_unread() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "VIDEO_one.mxf")]);
        let with_subtitles = imf_cpl(&REEL_UUIDS[..1]).replace(
            "</SequenceList>",
            "<cc:SubtitlesSequence xmlns:cc=\"z\"><ResourceList/></cc:SubtitlesSequence></SequenceList>",
        );
        std::fs::write(dir.path().join("CPL_a.xml"), with_subtitles).unwrap();

        let composition = resolve_composition(dir.path(), &[]).unwrap();

        assert!(composition.unread_subtitles);
        assert!(composition.subtitles.is_empty());
    }

    #[test]
    fn a_supplemental_imp_finds_the_audio_its_original_holds() {
        let library = tempfile::tempdir().unwrap();
        let english = [
            "e0000000-0000-4000-8000-000000000001",
            "e0000000-0000-4000-8000-000000000002",
        ];
        let french = [
            "f0000000-0000-4000-8000-000000000001",
            "f0000000-0000-4000-8000-000000000002",
        ];
        let supplemental = package_directory(
            library.path(),
            "supplemental",
            &[(REEL_UUIDS[1], "VIDEO_new_ending.mxf")],
        );
        std::fs::write(
            supplemental.join("CPL_ov.xml"),
            imf_cpl_with_two_audio_tracks([REEL_UUIDS[0], REEL_UUIDS[1]], english, french),
        )
        .unwrap();
        let original = package_directory(
            library.path(),
            "original",
            &[
                (REEL_UUIDS[0], "VIDEO_one.mxf"),
                (english[0], "AUDIO_en_one.mxf"),
                (english[1], "AUDIO_en_two.mxf"),
            ],
        );

        let found = find_original_version_packages(&supplemental, std::slice::from_ref(&original));

        assert_eq!(found, Ok(vec![original.clone()]));
        let composition =
            resolve_composition(&supplemental, std::slice::from_ref(&original)).unwrap();
        assert_eq!(composition.sound[1].path, original.join("AUDIO_en_two.mxf"));
        assert_eq!(
            composition.pictures[1].path,
            supplemental.join("VIDEO_new_ending.mxf")
        );
    }

    #[test]
    fn several_cpls_resolve_to_the_first_in_assetmap_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut xml = String::from("<AssetMap><AssetList>");
        for (id, path) in [
            ("aaaa1111-0000-0000-0000-000000000000", "CPL_second.xml"),
            ("bbbb2222-0000-0000-0000-000000000000", "CPL_first.xml"),
            (REEL_UUIDS[0], "head.mxf"),
            (REEL_UUIDS[1], "feature.mxf"),
        ] {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.path().join("ASSETMAP.xml"), xml).unwrap();
        std::fs::write(dir.path().join("CPL_second.xml"), dcp_cpl(&[REEL_UUIDS[1]])).unwrap();
        std::fs::write(dir.path().join("CPL_first.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();

        // CPL_second.xml is listed first, so it wins whatever the names suggest
        assert_eq!(
            picture_files(dir.path()),
            vec![dir.path().join("feature.mxf")]
        );
    }

    #[test]
    fn an_opl_is_not_mistaken_for_a_cpl() {
        let dir = tempfile::tempdir().unwrap();
        let mut xml = String::from("<AssetMap><AssetList>");
        for (id, path) in [
            ("aaaa1111-0000-0000-0000-000000000000", "OPL_a.xml"),
            ("bbbb2222-0000-0000-0000-000000000000", "CPL_a.xml"),
            (REEL_UUIDS[0], "head.mxf"),
        ] {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.path().join("ASSETMAP.xml"), xml).unwrap();
        std::fs::write(
            dir.path().join("OPL_a.xml"),
            "<OutputProfileList><CompositionPlaylistId>urn:uuid:x</CompositionPlaylistId>\
             </OutputProfileList>",
        )
        .unwrap();
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();

        assert_eq!(picture_files(dir.path()), vec![dir.path().join("head.mxf")]);
    }

    #[test]
    fn no_assetmap_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("picture.mxf"), b"").unwrap();
        assert_eq!(source_uri(dir.path()), None);
    }

    #[test]
    fn no_cpl_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "missing.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        assert_eq!(source_uri(dir.path()), None);
    }

    #[test]
    fn unreadable_cpl_xml_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), "<CompositionPlaylist").unwrap();
        assert_eq!(source_uri(dir.path()), None);
    }

    #[test]
    fn a_cpl_naming_no_picture_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "sound.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[])).unwrap();
        assert_eq!(source_uri(dir.path()), None);
    }

    #[test]
    fn a_picture_missing_from_the_assetmap_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[("dead-beef", "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();
        assert_eq!(source_uri(dir.path()), None);
    }

    #[test]
    fn a_trimmed_reel_plays_only_its_entry_point_onward() {
        let block = "<MainPicture><Id>urn:uuid:x</Id><EditRate>24 1</EditRate>\
             <IntrinsicDuration>72</IntrinsicDuration><EntryPoint>24</EntryPoint>\
             <Duration>48</Duration></MainPicture>";
        assert_eq!(
            segment_trim(block, "Duration", None),
            Some(SegmentTrim {
                start_seconds: 1.0,
                length_seconds: Some(2.0),
            })
        );
    }

    #[test]
    fn a_trimmed_single_reel_becomes_a_one_segment_edl() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "only.mxf")]);
        std::fs::write(
            dir.path().join("CPL_a.xml"),
            format!(
                "<CompositionPlaylist xmlns=\"x\"><ReelList><Reel><AssetList><MainPicture>\
                 <Id>urn:uuid:{}</Id><EditRate>24 1</EditRate>\
                 <IntrinsicDuration>72</IntrinsicDuration><EntryPoint>24</EntryPoint>\
                 <Duration>48</Duration></MainPicture></AssetList></Reel></ReelList>\
                 </CompositionPlaylist>",
                REEL_UUIDS[0]
            ),
        )
        .unwrap();

        let only = dir.path().join("only.mxf").to_string_lossy().into_owned();
        assert_eq!(
            source_uri(dir.path()),
            Some(format!("edl://%{}%{only},1,2", only.len()))
        );
    }

    #[test]
    fn a_trimmed_reel_without_an_edit_rate_plays_the_whole_file() {
        let block = "<MainPicture><Id>urn:uuid:x</Id>\
             <IntrinsicDuration>72</IntrinsicDuration><EntryPoint>24</EntryPoint>\
             <Duration>48</Duration></MainPicture>";
        assert_eq!(segment_trim(block, "Duration", None), None);
    }

    #[test]
    fn an_untrimmed_reel_carries_no_span() {
        let block = "<MainPicture><Id>urn:uuid:x</Id><EditRate>24 1</EditRate>\
             <IntrinsicDuration>72</IntrinsicDuration><EntryPoint>0</EntryPoint>\
             <Duration>72</Duration></MainPicture>";
        assert_eq!(segment_trim(block, "Duration", None), None);
    }

    #[test]
    fn an_imf_resource_trims_at_its_own_edit_rate() {
        let cpl = format!(
            "<CompositionPlaylist xmlns=\"y\"><EditRate>25 1</EditRate><SegmentList><Segment>\
             <SequenceList><cc:MainImageSequence xmlns:cc=\"z\"><ResourceList><Resource>\
             <TrackFileId>urn:uuid:{}</TrackFileId><EditRate>48 1</EditRate>\
             <IntrinsicDuration>96</IntrinsicDuration><EntryPoint>48</EntryPoint>\
             <SourceDuration>24</SourceDuration></Resource></ResourceList>\
             </cc:MainImageSequence></SequenceList></Segment></SegmentList></CompositionPlaylist>",
            REEL_UUIDS[0]
        );
        let references = picture_references(&cpl);
        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].trim,
            Some(SegmentTrim {
                start_seconds: 1.0,
                length_seconds: Some(0.5),
            })
        );
    }

    #[test]
    fn an_imf_resource_without_an_edit_rate_uses_the_composition_rate() {
        let cpl = format!(
            "<CompositionPlaylist xmlns=\"y\"><EditRate>25 1</EditRate><SegmentList><Segment>\
             <SequenceList><cc:MainImageSequence xmlns:cc=\"z\"><ResourceList><Resource>\
             <TrackFileId>urn:uuid:{}</TrackFileId>\
             <IntrinsicDuration>100</IntrinsicDuration><EntryPoint>25</EntryPoint>\
             <SourceDuration>50</SourceDuration></Resource></ResourceList>\
             </cc:MainImageSequence></SequenceList></Segment></SegmentList></CompositionPlaylist>",
            REEL_UUIDS[0]
        );
        let references = picture_references(&cpl);
        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].trim,
            Some(SegmentTrim {
                start_seconds: 1.0,
                length_seconds: Some(2.0),
            })
        );
    }

    #[test]
    fn a_dcp_composition_is_titled_by_its_content_title_text() {
        let cpl = "<CompositionPlaylist xmlns=\"x\">\
             <ContentTitleText>Cle&amp;o &lt;2 &quot;cut&quot;</ContentTitleText>\
             </CompositionPlaylist>";
        assert_eq!(composition_title(cpl), Some("Cle&o <2 \"cut\"".to_string()));
    }

    #[test]
    fn an_imf_composition_is_titled_by_its_content_title() {
        let cpl = "<CompositionPlaylist xmlns=\"y\">\
             <ContentTitle>Feature OV</ContentTitle></CompositionPlaylist>";
        assert_eq!(composition_title(cpl), Some("Feature OV".to_string()));
    }

    #[test]
    fn a_cpl_stating_no_title_has_none() {
        assert_eq!(composition_title(&dcp_cpl(&REEL_UUIDS[..1])), None);
        let empty = "<CompositionPlaylist xmlns=\"x\">\
             <ContentTitleText></ContentTitleText></CompositionPlaylist>";
        assert_eq!(composition_title(empty), None);
    }

    #[test]
    fn the_composition_source_carries_the_title() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "only.mxf")]);
        std::fs::write(
            dir.path().join("CPL_a.xml"),
            format!(
                "<CompositionPlaylist xmlns=\"x\"><ContentTitleText>Feature</ContentTitleText>\
                 <ReelList><Reel><AssetList><MainPicture><Id>urn:uuid:{}</Id>\
                 </MainPicture></AssetList></Reel></ReelList></CompositionPlaylist>",
                REEL_UUIDS[0]
            ),
        )
        .unwrap();

        assert_eq!(
            mpv_source(dir.path()).unwrap().title,
            Some("Feature".to_string())
        );
    }

    const SOUND_UUID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    // a version file whose second reel picture and whose sound are in other packages
    fn version_file(dir: &Path) {
        write_assetmap(dir, "CPL_a.xml", &[(REEL_UUIDS[0], "vf_reel1.mxf")]);
        std::fs::write(
            dir.join("CPL_a.xml"),
            dcp_cpl_with_sound(&[(REEL_UUIDS[0], SOUND_UUID), (REEL_UUIDS[1], SOUND_UUID)]),
        )
        .unwrap();
    }

    fn package_directory(library: &Path, name: &str, assets: &[(&str, &str)]) -> PathBuf {
        let dir = library.join(name);
        std::fs::create_dir(&dir).unwrap();
        write_assetmap(&dir, "CPL_ov.xml", assets);
        dir
    }

    #[test]
    fn assets_resolve_through_the_other_packages_after_the_own_package() {
        let library = tempfile::tempdir().unwrap();
        let version_file_dir = package_directory(library.path(), "vf", &[]);
        version_file(&version_file_dir);
        let original_version = package_directory(
            library.path(),
            "ov",
            &[
                (REEL_UUIDS[0], "ov_reel1.mxf"),
                (REEL_UUIDS[1], "ov_reel2.mxf"),
                (SOUND_UUID, "ov_sound.mxf"),
            ],
        );

        let composition =
            resolve_composition(&version_file_dir, std::slice::from_ref(&original_version))
                .unwrap();
        let pictures: Vec<PathBuf> = composition
            .pictures
            .into_iter()
            .map(|segment| segment.path)
            .collect();
        assert_eq!(
            pictures,
            vec![
                version_file_dir.join("vf_reel1.mxf"),
                original_version.join("ov_reel2.mxf")
            ]
        );
        assert_eq!(
            composition.sound[0].path,
            original_version.join("ov_sound.mxf")
        );
    }

    #[test]
    fn an_asset_no_package_holds_fails_naming_it_and_the_packages_searched() {
        let library = tempfile::tempdir().unwrap();
        let version_file_dir = package_directory(library.path(), "vf", &[]);
        version_file(&version_file_dir);
        let original_version =
            package_directory(library.path(), "ov", &[(SOUND_UUID, "ov_sound.mxf")]);

        let error = resolve_composition(&version_file_dir, std::slice::from_ref(&original_version))
            .unwrap_err();
        assert!(error.contains(REEL_UUIDS[1]), "{error}");
        assert!(
            error.contains(&version_file_dir.display().to_string()),
            "{error}"
        );
        assert!(
            error.contains(&original_version.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn the_original_version_finder_keeps_only_packages_holding_missing_assets() {
        let library = tempfile::tempdir().unwrap();
        let version_file_dir = package_directory(library.path(), "vf", &[]);
        version_file(&version_file_dir);
        let unrelated = package_directory(library.path(), "unrelated", &[(REEL_UUIDS[2], "x.mxf")]);
        let pictures = package_directory(library.path(), "pictures", &[(REEL_UUIDS[1], "p.mxf")]);
        let sound = package_directory(library.path(), "sound", &[(SOUND_UUID, "s.mxf")]);
        let duplicate = package_directory(library.path(), "duplicate", &[(SOUND_UUID, "s.mxf")]);
        let not_a_package = library.path().join("not_a_package");
        std::fs::create_dir(&not_a_package).unwrap();

        let searched = [
            not_a_package,
            version_file_dir.clone(),
            unrelated,
            pictures.clone(),
            sound.clone(),
            duplicate,
        ];
        assert_eq!(
            find_original_version_packages(&version_file_dir, &searched),
            Ok(vec![pictures, sound])
        );
    }

    const SUBTITLE_UUID: &str = "5e000000-0000-4000-8000-000000000001";

    #[test]
    fn a_version_file_takes_its_subtitle_from_the_original_version() {
        let library = tempfile::tempdir().unwrap();
        let version_file_dir = package_directory(
            library.path(),
            "vf",
            &[
                (REEL_UUIDS[0], "vf_reel1.mxf"),
                (SOUND_UUID, "vf_sound.mxf"),
            ],
        );
        let subtitled = dcp_cpl_with_sound(&[(REEL_UUIDS[0], SOUND_UUID)]).replace(
            "</MainSound>",
            &format!(
                "</MainSound><MainSubtitle><Id>urn:uuid:{SUBTITLE_UUID}</Id><EntryPoint>0</EntryPoint>\
                 <Duration>48</Duration><Language>fr</Language></MainSubtitle>"
            ),
        );
        std::fs::write(version_file_dir.join("CPL_ov.xml"), subtitled).unwrap();
        let original_version =
            package_directory(library.path(), "ov", &[(SUBTITLE_UUID, "ov_subtitle.mxf")]);

        let found = find_original_version_packages(
            &version_file_dir,
            std::slice::from_ref(&original_version),
        );
        let composition =
            resolve_composition(&version_file_dir, std::slice::from_ref(&original_version))
                .unwrap();

        assert_eq!(found, Ok(vec![original_version.clone()]));
        assert_eq!(
            composition.subtitles,
            vec![SubtitleSegment {
                path: original_version.join("ov_subtitle.mxf"),
                reel: 0,
                trim: None,
                language: Some("fr".to_string()),
            }]
        );
        assert!(composition.captions.is_empty());
    }

    #[test]
    fn the_original_version_finder_names_the_assets_no_package_holds() {
        let library = tempfile::tempdir().unwrap();
        let version_file_dir = package_directory(library.path(), "vf", &[]);
        version_file(&version_file_dir);
        let pictures = package_directory(library.path(), "pictures", &[(REEL_UUIDS[1], "p.mxf")]);

        let error = find_original_version_packages(&version_file_dir, &[pictures]).unwrap_err();
        assert!(error.contains(SOUND_UUID), "{error}");
        assert!(!error.contains(REEL_UUIDS[1]), "{error}");
    }
}

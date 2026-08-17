//! A package directory to the one mpv source that plays its whole composition.
//!
//! Picking a picture MXF by filename or size plays a single reel of a
//! multi-reel composition. The CPL is the only document that says which track
//! files belong to the composition, in what order and how much of each one
//! plays, so resolution goes ASSETMAP → CPL → picture track files and mpv gets
//! them as one EDL timeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A picture track file as the composition plays it: the whole file, or only
/// the span the reel enters and leaves at.
#[derive(Debug, PartialEq)]
struct PictureSegment {
    path: PathBuf,
    trim: Option<SegmentTrim>,
}

/// Seconds into the file and seconds of it. Length is None when the CPL states
/// an entry point without a duration.
#[derive(Debug, PartialEq)]
struct SegmentTrim {
    start_seconds: f64,
    length_seconds: Option<f64>,
}

/// A picture the CPL names, as the id to look up and the span to play.
struct PictureReference {
    asset_id: String,
    trim: Option<SegmentTrim>,
}

/// The mpv source that plays every reel of the composition in `package_dir`.
///
/// None when the package has no ASSETMAP, no CPL, or a CPL naming no picture,
/// which leaves the caller on its own single-file fallback.
pub fn mpv_source(package_dir: &Path) -> Option<String> {
    match picture_segments(package_dir).as_slice() {
        [] => None,
        // one untrimmed reel is the file it always was: an EDL wrapper would
        // change the demuxer and add a chapter for no gain
        [only] if only.trim.is_none() => Some(only.path.to_string_lossy().into_owned()),
        several => Some(edl_uri(several)),
    }
}

/// Every picture track file the composition names, in composition order, with
/// the span of each one the composition plays.
fn picture_segments(package_dir: &Path) -> Vec<PictureSegment> {
    let Some(assetmap) = crate::assetmap::find(package_dir) else {
        return Vec::new();
    };
    let assets = crate::assetmap::parse_ordered(&assetmap);
    let Some(cpl) = first_cpl(package_dir, &assets) else {
        return Vec::new();
    };
    let path_by_id: HashMap<&str, &str> = assets
        .iter()
        .map(|(id, relative)| (id.as_str(), relative.as_str()))
        .collect();
    picture_references(&cpl)
        .into_iter()
        .filter_map(|picture| {
            let relative = path_by_id.get(picture.asset_id.as_str())?;
            Some(PictureSegment {
                path: package_dir.join(relative),
                trim: picture.trim,
            })
        })
        .collect()
}

/// The text of the first CPL in ASSETMAP order. ASSETMAP order is the only
/// order a package states, so a package holding several CPLs resolves to the
/// same one on every run.
fn first_cpl(package_dir: &Path, assets: &[(String, String)]) -> Option<String> {
    // an OPL carries a <CompositionPlaylistId>, so the root element name has to
    // end at the match
    let root = regex::Regex::new(r"<(?:\w+:)?CompositionPlaylist[\s>]").ok()?;
    assets
        .iter()
        .map(|(_, relative)| package_dir.join(relative))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
        })
        .find_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            root.is_match(&text).then_some(text)
        })
}

/// Every picture the CPL names, in composition order: a DCP CPL's reel
/// MainPicture ids (ST 429-7), or an IMF CPL's MainImageSequence resource
/// TrackFileIds (ST 2067-3).
fn picture_references(cpl: &str) -> Vec<PictureReference> {
    let reel_pictures = main_picture_references(cpl);
    if reel_pictures.is_empty() {
        return image_resource_references(cpl);
    }
    reel_pictures
}

/// One MainPicture per reel, so a single forward scan gives reel order.
fn main_picture_references(cpl: &str) -> Vec<PictureReference> {
    element_blocks(cpl, "MainPicture")
        .into_iter()
        .filter_map(|block| {
            Some(PictureReference {
                asset_id: uuid_in(block, "Id")?,
                trim: segment_trim(block, "Duration", None),
            })
        })
        .collect()
}

/// An image sequence may list several resources, so the sequence blocks come
/// first and the track files are read inside each one. A resource without its
/// own EditRate plays at the composition's.
fn image_resource_references(cpl: &str) -> Vec<PictureReference> {
    let composition_rate = element_text(cpl, "EditRate")
        .as_deref()
        .and_then(seconds_per_edit_unit);
    element_blocks(cpl, "MainImageSequence")
        .into_iter()
        .flat_map(|sequence| element_blocks(sequence, "Resource"))
        .filter_map(|resource| {
            Some(PictureReference {
                asset_id: uuid_in(resource, "TrackFileId")?,
                trim: segment_trim(resource, "SourceDuration", composition_rate),
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
    let seconds_per_unit = element_text(block, "EditRate")
        .as_deref()
        .and_then(seconds_per_edit_unit)
        .or(fallback_seconds_per_unit)?;
    Some(SegmentTrim {
        start_seconds: entry_point as f64 * seconds_per_unit,
        length_seconds: stated_duration.map(|units| units as f64 * seconds_per_unit),
    })
}

/// Each `name` element of `xml`, bounded by its own close tag and never running
/// past the next one that opens, so a reel missing a close tag still resolves
/// instead of swallowing the reels after it.
fn element_blocks<'text>(xml: &'text str, name: &str) -> Vec<&'text str> {
    let Ok(open) = regex::Regex::new(&format!(r"<(?:\w+:)?{name}\b")) else {
        return Vec::new();
    };
    let Ok(close) = regex::Regex::new(&format!(r"</(?:\w+:)?{name}>")) else {
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

/// The text of the first `name` element in `block`.
fn element_text(block: &str, name: &str) -> Option<String> {
    let pattern = format!(r"<(?:\w+:)?{name}>([^<]*)");
    let found = regex::Regex::new(&pattern).ok()?.captures(block)?;
    Some(found[1].trim().to_string())
}

fn element_u64(block: &str, name: &str) -> Option<u64> {
    element_text(block, name)?.parse().ok()
}

/// The bare lowercased uuid in the first `name` element of `block`.
fn uuid_in(block: &str, name: &str) -> Option<String> {
    let pattern = format!(r"<(?:\w+:)?{name}>\s*(?:urn:uuid:)?([0-9a-fA-F-]{{36}})");
    let found = regex::Regex::new(&pattern).ok()?.captures(block)?;
    Some(found[1].to_ascii_lowercase())
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
        picture_segments(package_dir)
            .into_iter()
            .map(|segment| segment.path)
            .collect()
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
            mpv_source(dir.path()),
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
            mpv_source(dir.path()),
            Some(dir.path().join("only.mxf").to_string_lossy().into_owned())
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
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn no_cpl_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "missing.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn unreadable_cpl_xml_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), "<CompositionPlaylist").unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn a_cpl_naming_no_picture_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "sound.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[])).unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn a_picture_missing_from_the_assetmap_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[("dead-beef", "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();
        assert_eq!(mpv_source(dir.path()), None);
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
            mpv_source(dir.path()),
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
}
